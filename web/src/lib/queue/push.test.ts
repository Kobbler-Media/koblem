import { describe, expect, it } from 'vitest';
import {
  decodeServerKey,
  encodeKey,
  isIos,
  pushAvailability,
  subscriptionBody,
  type PushEnvironment
} from './push';

function env(over: Partial<PushEnvironment> = {}): PushEnvironment {
  return {
    hasServiceWorker: true,
    hasPushManager: true,
    hasNotification: true,
    standalone: false,
    ios: false,
    permission: 'default',
    ...over
  };
}

describe('pushAvailability', () => {
  it('is ready on a supporting browser that has not been asked yet', () => {
    expect(pushAvailability(env())).toEqual({ state: 'ready', hint: '' });
  });

  it('names Add to Home Screen on an iOS tab rather than calling it unsupported', () => {
    const it_ = pushAvailability(env({ ios: true, hasPushManager: false, hasNotification: false }));
    expect(it_.state).toBe('needs-install');
    expect(it_.hint).toContain('Add to Home Screen');
  });

  it('is ready once the same iPhone is running from the Home Screen', () => {
    expect(pushAvailability(env({ ios: true, standalone: true })).state).toBe('ready');
  });

  it('calls a browser with no service worker or push manager unsupported', () => {
    expect(pushAvailability(env({ hasServiceWorker: false })).state).toBe('unsupported');
    expect(pushAvailability(env({ hasPushManager: false })).state).toBe('unsupported');
    expect(pushAvailability(env({ hasNotification: false })).state).toBe('unsupported');
  });

  it('says a denied grant is a site setting, not a broken toggle', () => {
    const denied = pushAvailability(env({ permission: 'denied' }));
    expect(denied.state).toBe('blocked');
    expect(denied.hint).toContain('site settings');
  });
});

describe('isIos', () => {
  it('reads an iPhone off the user agent', () => {
    expect(isIos('Mozilla/5.0 (iPhone; CPU iPhone OS 18_2 like Mac OS X)', 'iPhone', 5)).toBe(true);
  });

  it('catches an iPad claiming to be a Mac', () => {
    expect(isIos('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', 'MacIntel', 5)).toBe(true);
  });

  it('leaves a real Mac alone', () => {
    expect(isIos('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', 'MacIntel', 0)).toBe(false);
    expect(isIos('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64', 0)).toBe(false);
  });
});

describe('key encoding', () => {
  it('round-trips the bytes the browser and the server exchange', () => {
    const bytes = new Uint8Array([0, 1, 2, 250, 251, 252, 253, 254, 255]);
    const encoded = encodeKey(bytes.buffer as ArrayBuffer);
    expect(encoded).not.toContain('=');
    expect(encoded).not.toContain('+');
    expect(encoded).not.toContain('/');
    expect(Array.from(decodeServerKey(encoded))).toEqual(Array.from(bytes));
  });

  it('decodes an unpadded key of every remainder length', () => {
    for (const len of [1, 2, 3, 4, 5, 65]) {
      const bytes = new Uint8Array(len).map((_, i) => (i * 37) % 256);
      expect(Array.from(decodeServerKey(encodeKey(bytes.buffer as ArrayBuffer)))).toEqual(
        Array.from(bytes)
      );
    }
  });

  it('treats an absent key as empty rather than throwing', () => {
    expect(encodeKey(null)).toBe('');
  });
});

describe('subscriptionBody', () => {
  const sub = (endpoint: string, keys: Record<string, Uint8Array | null>) =>
    ({
      endpoint,
      getKey: (name: string) => keys[name]?.buffer ?? null
    }) as unknown as PushSubscription;

  it('carries the endpoint and both keys', () => {
    const body = subscriptionBody(
      sub('https://push.example/x', {
        p256dh: new Uint8Array([1, 2, 3]),
        auth: new Uint8Array([4, 5, 6])
      })
    );
    expect(body).toEqual({
      endpoint: 'https://push.example/x',
      keys: { p256dh: 'AQID', auth: 'BAUG' }
    });
  });

  it('refuses a keyless subscription, which could never carry a payload', () => {
    expect(
      subscriptionBody(sub('https://push.example/x', { p256dh: new Uint8Array([1]), auth: null }))
    ).toBeNull();
    expect(
      subscriptionBody(sub('', { p256dh: new Uint8Array([1]), auth: new Uint8Array([2]) }))
    ).toBeNull();
  });
});
