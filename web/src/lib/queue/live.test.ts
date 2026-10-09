import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { JobEvent, JobState } from '$lib/api/client';

/// The SSE is the thing under test, so it is the thing faked: the handlers the store registers are
/// captured here and frames are pushed through them by hand.
let handlers: {
  onJob: (e: JobEvent) => void;
  onOpen?: () => void;
  onClose?: () => void;
} | null = null;
/// Whether a fresh connection succeeds. The store reconnects on its own, so "the stream is down"
/// has to mean the RECONNECT fails too — otherwise the backstop is correctly disarmed five seconds
/// later and the case never gets exercised.
let streamUp = true;

vi.mock('$lib/api/client', () => ({
  getQueueSnapshot: async () => ({ jobs: [], paused: false, pending: 0, running: 0, stuck: [] }),
  subscribeQueueEvents: (h: typeof handlers) => {
    handlers = h;
    if (streamUp) h?.onOpen?.();
    else h?.onClose?.();
    return () => {
      handlers = null;
    };
  }
}));

const { watchLiveJobs } = await import('./live');

function frame(kind: string, state: JobState): JobEvent {
  return { job_id: 'j', kind, state, warnings: [], at: '2026-08-17T00:00:00Z' } as JobEvent;
}

function watching(kinds: string[], downMs = 12000) {
  const counter = { checks: 0 };
  const w = watchLiveJobs({
    kinds: kinds as JobEvent['kind'][],
    check: () => void counter.checks++,
    downMs
  });
  return { w, counter };
}

describe('watchLiveJobs', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    handlers = null;
    streamUp = true;
  });

  /// A page that mounts onto a stream that has been up since the session started hears no `onOpen`
  /// — the title-bar indicator opened it — so without this it would never learn what is already
  /// running.
  it('reads once on mount even though the stream is already live', async () => {
    const { w, counter } = watching(['h3']);
    await vi.advanceTimersByTimeAsync(400);
    expect(counter.checks).toBe(1);
    w.stop();
  });

  /// 🔴 The whole point: the transition arrives, so nothing has to ask on a timer. Two frames for
  /// the same render — `queued` then `running`, moments apart — are one reason to look, not two.
  it('checks once per burst of frames, and never on a timer while the stream is up', async () => {
    const { w, counter } = watching(['h3', 'ltx25']);
    await vi.advanceTimersByTimeAsync(400);
    counter.checks = 0;

    handlers?.onJob(frame('h3', 'queued'));
    handlers?.onJob(frame('ltx25', 'running'));
    expect(counter.checks).toBe(0); // still coalescing
    await vi.advanceTimersByTimeAsync(400);
    expect(counter.checks).toBe(1);

    // A live stream must not also be polling. That is the regression that mattered: the Director
    // asked every twelve seconds all night and the answer was almost always no.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(counter.checks).toBe(1);

    w.stop();
  });

  it('ignores a finished render and anything of another kind', async () => {
    const { w, counter } = watching(['h3']);
    await vi.advanceTimersByTimeAsync(400);
    counter.checks = 0;

    handlers?.onJob(frame('h3', 'done'));
    handlers?.onJob(frame('h3', 'failed'));
    handlers?.onJob(frame('h3', 'cancelled'));
    handlers?.onJob(frame('music3', 'running'));
    await vi.advanceTimersByTimeAsync(1000);
    expect(counter.checks).toBe(0);

    w.stop();
  });

  /// The stream going down is the one case an event cannot cover, and the only case the interval
  /// exists for — it asks straight away rather than after a first full period, because the frames
  /// missed while it was dropping are exactly the ones worth catching up on.
  it('falls back to the interval only while the stream is down', async () => {
    const { w, counter } = watching(['h3']);
    await vi.advanceTimersByTimeAsync(30_000);
    expect(counter.checks).toBe(1); // the opening read, and nothing else
    counter.checks = 0;

    streamUp = false;
    handlers?.onClose?.();
    await vi.advanceTimersByTimeAsync(400);
    expect(counter.checks).toBe(1);
    await vi.advanceTimersByTimeAsync(12_000);
    expect(counter.checks).toBe(2);

    w.stop();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(counter.checks).toBe(2);
  });

  /// The store reconnects by itself, and a backstop that survived the reconnect would be the poll
  /// this change exists to delete, quietly restored by the first blip.
  it('puts the interval away again when the stream comes back', async () => {
    const { w, counter } = watching(['h3']);
    await vi.advanceTimersByTimeAsync(400);
    counter.checks = 0;

    streamUp = false;
    handlers?.onClose?.();
    await vi.advanceTimersByTimeAsync(400);
    expect(counter.checks).toBe(1); // the catch-up read

    streamUp = true;
    await vi.advanceTimersByTimeAsync(10_000); // the store's own reconnect
    counter.checks = 0;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(counter.checks).toBe(0);

    w.stop();
  });

  /// A stop that leaves the interval running is a page still polling after the user has navigated
  /// away from it — the exact cost this is meant to remove.
  it('stops asking, and stops listening, once torn down', async () => {
    const { w, counter } = watching(['h3']);
    await vi.advanceTimersByTimeAsync(400);
    counter.checks = 0;

    w.stop();
    handlers?.onJob(frame('h3', 'running'));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(counter.checks).toBe(0);
  });
});
