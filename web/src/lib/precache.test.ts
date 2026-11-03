import { describe, expect, it } from 'vitest';
import { precachePathnames } from './precache';

describe('precachePathnames', () => {
  it('turns base-relative manifest paths into the pathnames fetch events carry', () => {
    const set = precachePathnames('https://koblem.test/', [
      [{ path: '_app/immutable/entry/start.js' }],
      [{ path: 'icon-192.png' }, { path: 'manifest.webmanifest' }]
    ]);
    expect([...set]).toEqual([
      '/_app/immutable/entry/start.js',
      '/icon-192.png',
      '/manifest.webmanifest'
    ]);
  });

  it('keeps a non-root scope as the prefix', () => {
    const set = precachePathnames('https://host.test/koblem/', [[{ path: 'icon-192.png' }]]);
    expect(set.has('/koblem/icon-192.png')).toBe(true);
  });
});
