import type { JobKind } from '$lib/api/client';
import {
  getQueueState,
  onJobEvent,
  onStreamChange,
  startQueueFeed
} from '$lib/stores/queue.svelte';
import { isTerminal } from './format';

/**
 * "Tell me when a job of these kinds may have appeared or started" — the SSE-driven replacement for
 * a page that asked the server on a timer.
 *
 * 🔴 The timer is not gone, it is DEMOTED. The stream is the normal path; the interval runs only
 * while the stream is down, which is the one case an event cannot cover. Keeping both would be
 * paying for a poll nothing reads; keeping neither would leave the page deaf the first time nginx
 * dropped the connection.
 *
 * The check is COALESCED, because two things fire it and they overlap: a render enqueued on another
 * device publishes its `queued` frame and its `running` frame moments apart, and both mean the same
 * "go and look".
 */
export interface LiveWatch {
  stop(): void;
}

/// Long enough that a burst of frames costs one request, short enough to feel immediate.
const COALESCE_MS = 350;

export function watchLiveJobs(opts: {
  /** Which kinds are worth a look. A frame for anything else is somebody else's render. */
  kinds: JobKind[];
  /** Go and ask the server what is live. Never called concurrently with itself. */
  check: () => Promise<void> | void;
  /** How often to fall back to asking, while the stream is down. */
  downMs: number;
}): LiveWatch {
  const kinds = new Set(opts.kinds);
  const stopFeed = startQueueFeed();

  let coalesce: ReturnType<typeof setTimeout> | null = null;
  let backstop: ReturnType<typeof setInterval> | null = null;
  let running = false;
  let stopped = false;

  async function run() {
    // Serialised rather than dropped: a frame that arrives mid-check is the one most likely to be
    // the transition worth seeing, so the answer is re-read after it rather than skipped.
    if (running) {
      schedule();
      return;
    }
    running = true;
    try {
      await opts.check();
    } finally {
      running = false;
    }
  }

  function schedule() {
    if (stopped || coalesce) return;
    coalesce = setTimeout(() => {
      coalesce = null;
      void run();
    }, COALESCE_MS);
  }

  const stopEvents = onJobEvent((e) => {
    // A finished render is not a reason to look for something to attach to: whatever ended is
    // either ours, and already being watched, or somebody else's.
    if (kinds.has(e.kind) && !isTerminal(e.state)) schedule();
  });

  function arm(live: boolean) {
    if (backstop) {
      clearInterval(backstop);
      backstop = null;
    }
    if (live || stopped) return;
    // Ask straight away rather than after a first full period: the frames missed while the stream
    // was dropping are exactly the ones worth catching up on.
    schedule();
    backstop = setInterval(() => void run(), opts.downMs);
  }

  const stopStreamWatch = onStreamChange(arm);
  // 🔴 Read the CURRENT state rather than assuming the stream is down and waiting to be told
  // otherwise. The feed is ref-counted and the title-bar indicator holds it open for the whole
  // session, so by the time any page mounts the stream is normally already up — and `onOpen` fired
  // long ago. A watcher that waited for it would poll forever beside a perfectly live stream.
  arm(getQueueState().streaming);
  // One opening read regardless, so a page that mounts onto an already-live stream still learns
  // what is running right now. Coalesced with whatever `arm` may just have asked for.
  schedule();

  return {
    stop() {
      stopped = true;
      stopEvents();
      stopStreamWatch();
      stopFeed();
      if (coalesce) clearTimeout(coalesce);
      if (backstop) clearInterval(backstop);
      coalesce = null;
      backstop = null;
    }
  };
}
