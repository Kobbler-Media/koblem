/*
 * Web Push, minus the browser objects — so the parts that decide what the toggle says can be
 * tested without one.
 */

export type PushState = 'ready' | 'unsupported' | 'needs-install' | 'blocked';

export interface PushAvailability {
  state: PushState;
  /** What to tell the user. Empty when there is nothing to say. */
  hint: string;
}

export interface PushEnvironment {
  hasServiceWorker: boolean;
  hasPushManager: boolean;
  hasNotification: boolean;
  /** Launched from the Home Screen / app list rather than a browser tab. */
  standalone: boolean;
  ios: boolean;
  permission: NotificationPermission | null;
}

/**
 * 🔴 The iOS arm is checked FIRST and deliberately. Safari in a normal tab exposes no
 * `PushManager` at all and throws from `Notification.requestPermission()`, so every generic
 * "unsupported" branch below would fire for a phone that is one Add to Home Screen away from
 * working — and would say the wrong thing about it.
 */
export function pushAvailability(env: PushEnvironment): PushAvailability {
  if (env.ios && !env.standalone) {
    return {
      state: 'needs-install',
      hint: 'iOS only delivers notifications to an installed app. Share → Add to Home Screen, open Koblem from there, then turn this on.'
    };
  }
  if (!env.hasServiceWorker || !env.hasPushManager || !env.hasNotification) {
    return {
      state: 'unsupported',
      hint: 'This browser cannot receive push notifications.'
    };
  }
  if (env.permission === 'denied') {
    return {
      state: 'blocked',
      hint: 'The browser has blocked notifications for this site — clear it in site settings.'
    };
  }
  return { state: 'ready', hint: '' };
}

/** iPadOS reports itself as a Mac; the touch points are the only thing that gives it away. */
export function isIos(userAgent: string, platform: string, maxTouchPoints: number): boolean {
  if (/iphone|ipad|ipod/i.test(userAgent)) return true;
  return platform === 'MacIntel' && maxTouchPoints > 1;
}

/** `applicationServerKey` wants raw bytes, and the server publishes the key as base64url. */
export function decodeServerKey(base64url: string): Uint8Array {
  const padded = base64url.replace(/-/g, '+').replace(/_/g, '/');
  const raw = atob(padded.padEnd(padded.length + ((4 - (padded.length % 4)) % 4), '='));
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i += 1) bytes[i] = raw.charCodeAt(i);
  return bytes;
}

export function encodeKey(buffer: ArrayBuffer | null): string {
  if (!buffer) return '';
  const bytes = new Uint8Array(buffer);
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export interface SubscriptionBody {
  endpoint: string;
  keys: { p256dh: string; auth: string };
}

/** `null` when the browser handed back a subscription with no keys — it cannot receive a payload. */
export function subscriptionBody(sub: PushSubscription): SubscriptionBody | null {
  const p256dh = encodeKey(sub.getKey('p256dh'));
  const auth = encodeKey(sub.getKey('auth'));
  if (!sub.endpoint || !p256dh || !auth) return null;
  return { endpoint: sub.endpoint, keys: { p256dh, auth } };
}
