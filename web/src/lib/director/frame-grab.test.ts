import { beforeEach, describe, expect, it } from 'vitest';
import {
  cachedFrame,
  clampFrameTime,
  clearFrameCache,
  frameCacheKey,
  frameSize,
  FRAME_CACHE_LIMIT,
  grabFrames,
  planFrameGrabs,
  rememberFrame,
  roundFrameTime,
  seekLanded,
  type FrameGrabStep
} from './frame-grab';

const URL_A = 'blob:http://localhost/take-a';
const URL_B = 'blob:http://localhost/take-b';

/** The one step shape the cache writes through, so a test can seed without a browser. */
const step = (seekTo: number, url = URL_A, times = [seekTo]): FrameGrabStep => ({
  seekTo,
  times,
  keys: times.map((t) => frameCacheKey(url, t))
});

beforeEach(() => clearFrameCache());

describe('roundFrameTime', () => {
  /**
   * 🔴 The whole reason keys are rounded. Beat times are a running sum of float durations, so the
   * same beat comes out fractionally different on every recompute; keyed raw, one beat is a new
   * cache entry and a new seek after every edit.
   */
  it('collapses float drift to the millisecond', () => {
    expect(roundFrameTime(4.2000001)).toBe(4.2);
    expect(roundFrameTime(4.1999999)).toBe(4.2);
    expect(roundFrameTime(0.30000000000000004)).toBe(0.3);
  });

  it('keeps millisecond differences apart', () => {
    expect(roundFrameTime(4.2)).not.toBe(roundFrameTime(4.201));
  });

  it('rounds rather than truncates', () => {
    expect(roundFrameTime(1.2345)).toBe(1.235);
    expect(roundFrameTime(1.2344)).toBe(1.234);
  });
});

describe('frameCacheKey', () => {
  it('gives float-drifted times one key', () => {
    expect(frameCacheKey(URL_A, 4.2)).toBe(frameCacheKey(URL_A, 4.2000001));
    expect(frameCacheKey(URL_A, 4.2)).toBe(frameCacheKey(URL_A, 4.2004));
  });

  it('separates times a millisecond apart', () => {
    expect(frameCacheKey(URL_A, 4.2)).not.toBe(frameCacheKey(URL_A, 4.201));
  });

  it('separates takes', () => {
    expect(frameCacheKey(URL_A, 1)).not.toBe(frameCacheKey(URL_B, 1));
  });

  /** A collision across takes would paint one shot's frame into another shot's ribbon. */
  it('cannot be collided by a url that looks like a key', () => {
    expect(frameCacheKey('blob:x', 1.5)).not.toBe(frameCacheKey('blob:x 1.500', 0));
    expect(frameCacheKey('blob:x1', 0.5)).not.toBe(frameCacheKey('blob:x', 10.5));
  });

  it('writes a fixed 3-decimal time, so 4.2 and 4.2000 are one string', () => {
    expect(frameCacheKey('u', 4.2).endsWith('4.200')).toBe(true);
    expect(frameCacheKey('u', 12).endsWith('12.000')).toBe(true);
  });
});

describe('clampFrameTime', () => {
  /**
   * 🔴 Not 0. A fresh element is already at 0, and setting `currentTime` to the position it holds
   * aborts the seek without firing `seeked` — so a beat at t=0 would hang out its whole timeout.
   */
  it('never returns a time the element is already sitting at', () => {
    expect(clampFrameTime(0, 10)).toBeGreaterThan(0);
    expect(clampFrameTime(-3, 10)).toBeGreaterThan(0);
    expect(clampFrameTime(0, 10)).toBeLessThan(0.01);
  });

  it('leaves a time inside the take alone', () => {
    expect(clampFrameTime(4.2, 10)).toBe(4.2);
    expect(clampFrameTime(4.2000001, 10)).toBe(4.2);
  });

  /** An edited timeline outruns the take it was rendered from; that beat wants the last frame. */
  it('pulls a time past the end back to just inside it', () => {
    const t = clampFrameTime(99, 6);
    expect(t).toBeLessThan(6);
    expect(t).toBeGreaterThan(5.9);
  });

  it('treats a time exactly at the duration as past the end', () => {
    expect(clampFrameTime(6, 6)).toBeLessThan(6);
  });

  it('still returns something seekable for a take shorter than the end guard', () => {
    const t = clampFrameTime(5, 0.02);
    expect(t).toBeGreaterThan(0);
    expect(t).toBeLessThanOrEqual(0.02);
  });

  it('clamps only the floor when the duration is unknown or nonsense', () => {
    expect(clampFrameTime(4.2)).toBe(4.2);
    expect(clampFrameTime(4.2, Number.NaN)).toBe(4.2);
    expect(clampFrameTime(4.2, Number.POSITIVE_INFINITY)).toBe(4.2);
    expect(clampFrameTime(4.2, 0)).toBe(4.2);
    expect(clampFrameTime(-1)).toBeGreaterThan(0);
  });

  it('rounds its result like every other time key', () => {
    expect(clampFrameTime(4.2000001, 10)).toBe(clampFrameTime(4.2, 10));
  });
});

describe('planFrameGrabs', () => {
  /** One decoder walked forwards: a backward seek restarts it at the previous keyframe. */
  it('orders the seeks ascending whatever order the beats arrive in', () => {
    const plan = planFrameGrabs(URL_A, [4.2, 0.5, 9, 2], 12);
    expect(plan.map((s) => s.seekTo)).toEqual([0.5, 2, 4.2, 9]);
  });

  it('answers every requested time, keyed by the number the caller passed', () => {
    const plan = planFrameGrabs(URL_A, [1, 2.5], 12);
    expect(plan.flatMap((s) => s.times)).toEqual([1, 2.5]);
  });

  /** Float drift must not become two decodes of the same frame. */
  it('collapses times that round together into one seek', () => {
    const plan = planFrameGrabs(URL_A, [4.2, 4.2000001], 12);
    expect(plan).toHaveLength(1);
    expect(plan[0].times).toEqual([4.2, 4.2000001]);
    expect(plan[0].keys).toEqual([frameCacheKey(URL_A, 4.2)]);
  });

  /** Several beats past the end of a short take are the same frame, so they are one seek. */
  it('collapses everything past the end into one seek but keeps each beat its own key', () => {
    const plan = planFrameGrabs(URL_A, [7, 8, 20], 6);
    expect(plan).toHaveLength(1);
    expect(plan[0].seekTo).toBeLessThan(6);
    expect(plan[0].times).toEqual([7, 8, 20]);
    expect(plan[0].keys).toEqual([
      frameCacheKey(URL_A, 7),
      frameCacheKey(URL_A, 8),
      frameCacheKey(URL_A, 20)
    ]);
  });

  /**
   * 🔴 Keys are the REQUESTED time, never the clamped one. `cachedFrame` is a synchronous peek with
   * no duration in hand, so a frame filed under the clamped time is a frame the ribbon can never
   * find again — it would re-extract the whole out-of-range tail on every repaint.
   */
  it('keys an out-of-range beat under the time the caller will ask for', () => {
    const plan = planFrameGrabs(URL_A, [99], 6);
    expect(plan[0].keys).toEqual([frameCacheKey(URL_A, 99)]);
    expect(plan[0].keys).not.toContain(frameCacheKey(URL_A, plan[0].seekTo));
  });

  it('has nothing to do for no times', () => {
    expect(planFrameGrabs(URL_A, [])).toEqual([]);
  });

  /** A NaN beat means the arithmetic upstream broke; a frame from the head would hide that. */
  it('drops a non-finite time instead of flooring it', () => {
    const plan = planFrameGrabs(URL_A, [Number.NaN, 2, Number.POSITIVE_INFINITY], 12);
    expect(plan.map((s) => s.seekTo)).toEqual([2]);
  });

  it('folds a beat at zero into a real seek', () => {
    const plan = planFrameGrabs(URL_A, [0], 12);
    expect(plan[0].seekTo).toBeGreaterThan(0);
    expect(plan[0].keys).toEqual([frameCacheKey(URL_A, 0)]);
  });

  it('plans against the take even when the duration is not known yet', () => {
    const plan = planFrameGrabs(URL_A, [3, 1]);
    expect(plan.map((s) => s.seekTo)).toEqual([1, 3]);
  });
});

describe('frameSize', () => {
  it('preserves the aspect ratio', () => {
    expect(frameSize(1920, 1080, 256)).toEqual({ width: 256, height: 144 });
    expect(frameSize(1080, 1920, 256)).toEqual({ width: 256, height: 455 });
  });

  it('does not upscale a source narrower than the target', () => {
    expect(frameSize(128, 72, 256)).toEqual({ width: 128, height: 72 });
  });

  /** videoWidth is 0 until metadata lands, and a 0-wide canvas throws on draw. */
  it('falls back to 16:9 when the element reports no size', () => {
    expect(frameSize(0, 0, 256)).toEqual({ width: 16, height: 9 });
  });

  it('never produces a zero dimension', () => {
    const tiny = frameSize(1920, 1080, 1);
    expect(tiny.width).toBeGreaterThan(0);
    expect(tiny.height).toBeGreaterThan(0);
  });

  it('ignores a nonsense width', () => {
    expect(frameSize(1920, 1080, 0)).toEqual({ width: 256, height: 144 });
    expect(frameSize(1920, 1080, Number.NaN)).toEqual({ width: 256, height: 144 });
  });
});

describe('seekLanded', () => {
  it('accepts the position a completed seek reports', () => {
    expect(seekLanded(4.2, 4.2)).toBe(true);
    expect(seekLanded(4.21, 4.2)).toBe(true);
  });

  /** A `seeked` from a target we already timed out on carries the wrong frame. */
  it('rejects a position from some other seek', () => {
    expect(seekLanded(1, 4.2)).toBe(false);
  });
});

describe('the frame cache', () => {
  it('reads back what a step wrote, for every time the step answered', () => {
    rememberFrame(step(4.2, URL_A, [4.2, 4.2000001]), 'data:image/jpeg;base64,AAA');
    expect(cachedFrame(URL_A, 4.2)).toBe('data:image/jpeg;base64,AAA');
    expect(cachedFrame(URL_A, 4.2000001)).toBe('data:image/jpeg;base64,AAA');
  });

  it('misses on a time nothing captured, and on another take', () => {
    rememberFrame(step(1), 'data:1');
    expect(cachedFrame(URL_A, 2)).toBeUndefined();
    expect(cachedFrame(URL_B, 1)).toBeUndefined();
  });

  it('clears one take without touching the others', () => {
    rememberFrame(step(1, URL_A), 'data:a');
    rememberFrame(step(1, URL_B), 'data:b');
    clearFrameCache(URL_A);
    expect(cachedFrame(URL_A, 1)).toBeUndefined();
    expect(cachedFrame(URL_B, 1)).toBe('data:b');
  });

  it('clears everything when given no take', () => {
    rememberFrame(step(1, URL_A), 'data:a');
    rememberFrame(step(1, URL_B), 'data:b');
    clearFrameCache();
    expect(cachedFrame(URL_A, 1)).toBeUndefined();
    expect(cachedFrame(URL_B, 1)).toBeUndefined();
  });

  /** A prefix clear must not take a take whose url merely starts the same way. */
  it('does not clear a take whose url is a prefix of another', () => {
    rememberFrame(step(1, 'blob:x'), 'data:x');
    rememberFrame(step(1, 'blob:x-2'), 'data:x2');
    clearFrameCache('blob:x');
    expect(cachedFrame('blob:x', 1)).toBeUndefined();
    expect(cachedFrame('blob:x-2', 1)).toBe('data:x2');
  });

  it('stays bounded, dropping the oldest entries first', () => {
    for (let i = 0; i < FRAME_CACHE_LIMIT + 10; i++) rememberFrame(step(i + 1), `data:${i}`);
    expect(cachedFrame(URL_A, 1)).toBeUndefined();
    expect(cachedFrame(URL_A, 10)).toBeUndefined();
    expect(cachedFrame(URL_A, 11)).toBe('data:10');
    expect(cachedFrame(URL_A, FRAME_CACHE_LIMIT + 10)).toBe(`data:${FRAME_CACHE_LIMIT + 9}`);
  });
});

/**
 * The canvas/video path itself is not exercised here — this suite runs in plain node, and a fake
 * `document` would only assert that the mock behaves like the mock. What IS asserted is the
 * contract every caller depends on: it resolves, it never throws, and it answers from cache without
 * needing a decoder at all.
 */
describe('grabFrames', () => {
  it('answers entirely from cache, keyed by the caller’s own numbers', async () => {
    rememberFrame(step(4.2, URL_A, [4.2]), 'data:cached');
    const out = await grabFrames(URL_A, [4.2000001]);
    expect(out.get(4.2000001)).toBe('data:cached');
  });

  it('resolves with what it has rather than throwing when there is no DOM', async () => {
    rememberFrame(step(1), 'data:one');
    const out = await grabFrames(URL_A, [1, 2, 3]);
    expect([...out]).toEqual([[1, 'data:one']]);
  });

  it('resolves empty for an already-aborted signal', async () => {
    const ctl = new AbortController();
    ctl.abort();
    await expect(grabFrames(URL_A, [1, 2], { signal: ctl.signal })).resolves.toEqual(new Map());
  });

  it('still returns the cache hits when aborted', async () => {
    rememberFrame(step(1), 'data:one');
    const ctl = new AbortController();
    ctl.abort();
    const out = await grabFrames(URL_A, [1, 2], { signal: ctl.signal });
    expect(out.get(1)).toBe('data:one');
    expect(out.has(2)).toBe(false);
  });

  it('resolves empty for no times', async () => {
    await expect(grabFrames(URL_A, [])).resolves.toEqual(new Map());
  });
});
