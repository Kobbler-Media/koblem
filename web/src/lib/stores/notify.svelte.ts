import { getPushKey, subscribePush, unsubscribePush } from '$lib/api/client';
import type { NotifyMessage } from '$lib/queue/notify';
import {
  decodeServerKey,
  isIos,
  pushAvailability,
  subscriptionBody,
  type PushAvailability
} from '$lib/queue/push';

const key = 'koblem-queue-notify';

/// Absent in some embedded webviews. Every path below is a no-op without it
/// rather than a thrown ReferenceError halfway through an event handler.
const supported = typeof window !== 'undefined' && 'Notification' in window;

let enabled = $state(false);
let availability = $state<PushAvailability>({ state: 'unsupported', hint: '' });
/// Whether a push subscription reached the server. False means notifications only arrive while a
/// tab is open, which is a materially different promise and the UI says so.
let pushing = $state(false);
let busy = $state(false);

export function getNotifyState() {
  return {
    get enabled() {
      return enabled;
    },
    get availability() {
      return availability;
    },
    get pushing() {
      return pushing;
    },
    get busy() {
      return busy;
    }
  };
}

function standalone(): boolean {
  if (typeof window === 'undefined') return false;
  const legacy = (navigator as Navigator & { standalone?: boolean }).standalone === true;
  return legacy || window.matchMedia?.('(display-mode: standalone)').matches === true;
}

function readAvailability(): PushAvailability {
  if (typeof window === 'undefined') return { state: 'unsupported', hint: '' };
  return pushAvailability({
    hasServiceWorker: 'serviceWorker' in navigator,
    hasPushManager: 'PushManager' in window,
    hasNotification: supported,
    standalone: standalone(),
    ios: isIos(navigator.userAgent, navigator.platform ?? '', navigator.maxTouchPoints ?? 0),
    permission: supported ? Notification.permission : null
  });
}

async function registration(): Promise<ServiceWorkerRegistration | null> {
  if (typeof navigator === 'undefined' || !('serviceWorker' in navigator)) return null;
  try {
    const existing = await navigator.serviceWorker.getRegistration();
    if (existing) return await navigator.serviceWorker.ready;
    return await navigator.serviceWorker.register('/service-worker.js', { type: 'module' });
  } catch {
    return null;
  }
}

export function loadNotifyPreference() {
  availability = readAvailability();
  if (!supported) return;
  // A grant revoked in site settings must not leave the toggle claiming it is on.
  enabled = localStorage.getItem(key) === 'on' && Notification.permission === 'granted';
  if (enabled) void syncPush();
}

/// Re-reads the existing browser subscription without ever prompting, so a reload of an already
/// enabled install reports the truth rather than assuming it.
async function syncPush(): Promise<void> {
  if (availability.state !== 'ready') return;
  const reg = await registration();
  if (!reg) return;
  try {
    pushing = (await reg.pushManager.getSubscription()) !== null;
  } catch {
    pushing = false;
  }
}

/**
 * 🔴 Only ever reached from a click. `requestPermission()` on load is a dark
 * pattern, browsers reject the prompt outside a user gesture anyway, and on an
 * iOS tab it throws rather than resolving.
 */
export async function setNotifyEnabled(on: boolean): Promise<void> {
  availability = readAvailability();
  if (!supported) return;
  busy = true;
  try {
    if (!on) {
      enabled = false;
      localStorage.setItem(key, 'off');
      await dropPush();
      return;
    }
    if (availability.state !== 'ready') return;
    const granted =
      Notification.permission === 'granted' ? 'granted' : await Notification.requestPermission();
    enabled = granted === 'granted';
    localStorage.setItem(key, enabled ? 'on' : 'off');
    availability = readAvailability();
    if (enabled) await takePush();
  } catch {
    // A webview can expose the API and still throw on use; the tab-open path stays whatever it was.
  } finally {
    busy = false;
  }
}

async function takePush(): Promise<void> {
  const reg = await registration();
  if (!reg) return;
  try {
    const { public_key } = await getPushKey();
    if (!public_key) return;
    const existing = await reg.pushManager.getSubscription();
    const sub =
      existing ??
      (await reg.pushManager.subscribe({
        // Payloads are encrypted per subscription, so a silent push is not an option and Chrome
        // refuses the subscription without this anyway.
        userVisibleOnly: true,
        applicationServerKey: decodeServerKey(public_key) as BufferSource
      }));
    const body = subscriptionBody(sub);
    if (!body) return;
    await subscribePush(body);
    pushing = true;
  } catch {
    // No push is a degraded state, not a broken one: the tab-open notification still works.
    pushing = false;
  }
}

async function dropPush(): Promise<void> {
  const reg = await registration();
  if (!reg) return;
  try {
    const sub = await reg.pushManager.getSubscription();
    if (!sub) return;
    // Tell the server first: a browser-side unsubscribe that never reaches us leaves a row that
    // only gets cleaned up on the next 410.
    await unsubscribePush(sub.endpoint).catch(() => {});
    await sub.unsubscribe();
  } catch {
    // Nothing to do — the row is pruned on its first 404/410 either way.
  } finally {
    pushing = false;
  }
}

export function postNotification(message: NotifyMessage): void {
  if (!supported || !enabled || Notification.permission !== 'granted') return;
  // The row is already on screen; a toast for something being watched is noise.
  if (document.visibilityState === 'visible') return;
  try {
    const note = new Notification(message.title, { body: message.body, tag: message.tag });
    note.onclick = () => {
      window.focus();
      note.close();
      if (window.location.pathname !== '/queue') window.location.assign('/queue');
    };
  } catch {
    // A webview can expose the constructor and still throw on use.
  }
}
