/*
 * 🔴 This app is a control panel for a live GPU queue, so a stale response is worse than a slow
 * one. Only the content-hashed build output and `static/` are cached; every navigation goes to the
 * network first and the shell is a fallback for when there is no network at all.
 */

import { version } from '$app/env';
import { assets, immutable } from '$app/manifest';
import { self as sw } from '$app/service-worker';
import { precachePathnames } from '#lib/precache.ts';

const CACHE = `koblem-${version}`;
const SHELL = '/index.html';
const ASSETS = precachePathnames(sw.registration.scope, [immutable, assets]);

sw.addEventListener('install', (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      await cache.addAll([...ASSETS]);
      // Separate from the bundle: the SPA fallback is served by nginx, not emitted as an asset, so
      // a failure here must not throw away the whole precache.
      await cache.add(SHELL).catch(() => {});
      await sw.skipWaiting();
    })()
  );
});

sw.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      for (const key of await caches.keys()) {
        if (key !== CACHE) await caches.delete(key);
      }
      await sw.clients.claim();
    })()
  );
});

sw.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;

  const url = new URL(request.url);
  if (url.origin !== sw.location.origin) return;
  // 🔴 The queue, the engine health and every render result live under these. A cached copy of any
  // of them is a lie about what the GPUs are doing.
  if (url.pathname.startsWith('/api/') || url.pathname.startsWith('/v1/')) return;
  if (url.pathname === '/health') return;

  if (ASSETS.has(url.pathname)) {
    event.respondWith(fromCache(request));
    return;
  }
  if (request.mode === 'navigate') {
    event.respondWith(networkThenShell(request));
  }
});

async function fromCache(request: Request): Promise<Response> {
  const cache = await caches.open(CACHE);
  const hit = await cache.match(request);
  return hit ?? fetch(request);
}

async function networkThenShell(request: Request): Promise<Response> {
  try {
    return await fetch(request);
  } catch {
    const cache = await caches.open(CACHE);
    const shell = await cache.match(SHELL);
    if (shell) return shell;
    throw new Error('offline');
  }
}

interface QueueNote {
  title: string;
  body: string;
  tag: string;
}

function readNote(data: PushMessageData | null): QueueNote | null {
  if (!data) return null;
  try {
    const parsed = data.json() as Partial<QueueNote>;
    if (typeof parsed?.title !== 'string' || typeof parsed?.tag !== 'string') return null;
    return { title: parsed.title, body: parsed.body ?? '', tag: parsed.tag };
  } catch {
    return null;
  }
}

sw.addEventListener('push', (event) => {
  const note = readNote(event.data);
  if (!note) return;
  event.waitUntil(
    sw.registration.showNotification(note.title, {
      body: note.body,
      // 🔴 The server reuses the page's own `queue:<job_id>:<state>` tags. A tab that is open but
      // hidden posts its own notification off the SSE frame; a matching tag makes the browser
      // REPLACE it rather than stack a second one for the same event.
      tag: note.tag,
      icon: '/icon-192.png',
      badge: '/icon-192-maskable.png',
      data: { url: '/queue' }
    })
  );
});

sw.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil(
    (async () => {
      const open = await sw.clients.matchAll({ type: 'window', includeUncontrolled: true });
      for (const client of open) {
        await client.focus();
        if (!new URL(client.url).pathname.startsWith('/queue')) await client.navigate('/queue');
        return;
      }
      await sw.clients.openWindow('/queue');
    })()
  );
});
