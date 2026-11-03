import {
  getQueueSnapshot,
  subscribeQueueEvents,
  type JobEvent,
  type QueueSnapshot
} from '#lib/api/client.ts';
import { SvelteSet } from 'svelte/reactivity';
import { isTerminal } from '#lib/queue/format.ts';
import { stuckMessage, terminalMessage, type NotifyMessage } from '#lib/queue/notify.ts';
import { postNotification } from '#lib/stores/notify.svelte.ts';

/// Backstop only. The SSE stream is what makes the page live; this catches the
/// fields an event does not carry (unit counts, ranks, ETAs) and the window
/// where the stream is down.
const SNAPSHOT_MS = 10000;
const COALESCE_MS = 250;
const RECONNECT_MS = 5000;
/// A koblem whose dispatcher runs in another process answers 409 here forever.
/// Back off rather than reconnecting every five seconds all night.
const RECONNECT_MAX_MS = 60000;

const empty = (): QueueSnapshot => ({
  jobs: [],
  paused: false,
  pending: 0,
  running: 0,
  stuck: []
});

let snapshot = $state<QueueSnapshot>(empty());
let error = $state('');
let streaming = $state(false);
let loaded = $state(false);

let refs = 0;
let stopStream: (() => void) | null = null;
let poll: ReturnType<typeof setInterval> | null = null;
let coalesce: ReturnType<typeof setTimeout> | null = null;
let reconnect: ReturnType<typeof setTimeout> | null = null;
let reconnectMs = RECONNECT_MS;
let inFlight = false;
/// 🔴 The coalescing set. One entry per announced transition, so the repeat
/// frames a reconnect replays — and the ten progress events between them — can
/// never turn one finished render into ten notifications.
const announced = new SvelteSet<string>();
/// Everything that wants the raw frames rather than the snapshot they patch.
const listeners = new SvelteSet<(event: JobEvent) => void>();

/**
 * Watch the job frames themselves.
 *
 * 🔴 This is what a page uses INSTEAD of a timer. The snapshot tells you what the queue looks like;
 * a frame tells you the moment something changed, which is the only thing a "has a render started
 * for me?" poll was ever asking. The Director asked it every twelve seconds, and the answer was no
 * every time except the handful when it was not.
 *
 * The stream can be down — see {@link getQueueState}'s `streaming` — so a caller that must not miss
 * a transition keeps a backstop for exactly that case, and only that case.
 */
export function onJobEvent(fn: (event: JobEvent) => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/// Everything that needs to know when the frames stop arriving.
const streamListeners = new SvelteSet<(live: boolean) => void>();

/**
 * Watch the stream itself go up and down.
 *
 * 🔴 A callback rather than a reactive read of {@link getQueueState}'s `streaming`. Anything that
 * keeps a backstop for the stream being down is doing so from `onMount`, outside any effect scope,
 * and an effect rooted by hand out there is a scheduler dependency for something that is not a
 * render — it is a subscription, so it is spelled as one.
 */
export function onStreamChange(fn: (live: boolean) => void): () => void {
  streamListeners.add(fn);
  return () => streamListeners.delete(fn);
}

function setStreaming(live: boolean) {
  streaming = live;
  for (const fn of streamListeners) {
    try {
      fn(live);
    } catch {
      // One page's handler must not take the stream down for every other page.
    }
  }
}

export function getQueueState() {
  return {
    get snapshot() {
      return snapshot;
    },
    get error() {
      return error;
    },
    get streaming() {
      return streaming;
    },
    get loaded() {
      return loaded;
    }
  };
}

function announce(message: NotifyMessage) {
  if (announced.has(message.tag)) return;
  announced.add(message.tag);
  postNotification(message);
}

/// The server has already applied the stuck threshold, so this is the same grain
/// as its webhook: a job that has crossed it, once.
function announceStuck(snap: QueueSnapshot) {
  const now = Date.now();
  for (const id of snap.stuck) {
    const job = snap.jobs.find((j) => j.id === id);
    if (job) announce(stuckMessage(job, now));
  }
}

export async function refreshQueue(): Promise<void> {
  if (inFlight) return;
  inFlight = true;
  try {
    snapshot = await getQueueSnapshot();
    announceStuck(snapshot);
    error = '';
    loaded = true;
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
  } finally {
    inFlight = false;
  }
}

function scheduleRefresh() {
  if (coalesce) return;
  coalesce = setTimeout(() => {
    coalesce = null;
    void refreshQueue();
  }, COALESCE_MS);
}

function applyEvent(event: JobEvent) {
  // Before the patch below, not after: a listener's job is to react to the TRANSITION, and none of
  // them read the snapshot to do it. Notifying first also means a listener still sees the frame
  // when the row is one this client has never held.
  for (const fn of listeners) {
    try {
      fn(event);
    } catch {
      // A page that throws in a handler must not take the stream down for every other page.
    }
  }
  const job = snapshot.jobs.find((j) => j.id === event.job_id);
  if (isTerminal(event.state)) {
    const message = terminalMessage(event, job?.label);
    if (message) announce(message);
  }
  // A row we have never seen, or one changing state, changes queue MEMBERSHIP and
  // the counts an event does not carry. Patch what we can, then re-read.
  if (!job || job.state !== event.state) scheduleRefresh();
  if (!job) return;
  job.state = event.state;
  if (event.progress) job.progress = event.progress;
  job.blocked_reason = event.blocked_reason;
  job.error = event.error;
  job.warnings = event.warnings ?? [];
}

function connect() {
  stopStream = subscribeQueueEvents({
    onJob: applyEvent,
    onLagged: () => void refreshQueue(),
    onOpen: () => {
      setStreaming(true);
      reconnectMs = RECONNECT_MS;
    },
    onClose: () => {
      setStreaming(false);
      stopStream = null;
      if (refs > 0 && !reconnect) {
        const wait = reconnectMs;
        reconnectMs = Math.min(RECONNECT_MAX_MS, reconnectMs * 2);
        reconnect = setTimeout(() => {
          reconnect = null;
          if (refs > 0) connect();
        }, wait);
      }
    }
  });
}

/// Ref-counted: the title-bar indicator holds it open for the whole session and
/// the queue page joins the same stream rather than opening a second one.
export function startQueueFeed(): () => void {
  refs += 1;
  if (refs === 1) {
    void refreshQueue();
    poll = setInterval(() => void refreshQueue(), SNAPSHOT_MS);
    connect();
  }
  return () => {
    refs = Math.max(0, refs - 1);
    if (refs > 0) return;
    stopStream?.();
    stopStream = null;
    if (poll) clearInterval(poll);
    poll = null;
    if (coalesce) clearTimeout(coalesce);
    coalesce = null;
    if (reconnect) clearTimeout(reconnect);
    reconnect = null;
    reconnectMs = RECONNECT_MS;
    setStreaming(false);
    announced.clear();
  };
}
