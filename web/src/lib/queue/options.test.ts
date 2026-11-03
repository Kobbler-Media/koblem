import { describe, expect, it } from 'vitest';
import type { Job, QueueSnapshot } from '#lib/api/client.ts';
import {
  effectiveBand,
  enqueueReceipt,
  hasQueueOverrides,
  queueHeaders,
  submitConsequence
} from './options';

function job(over: Partial<Job> = {}): Job {
  return {
    id: 'flux2_abc',
    kind: 'flux2',
    state: 'queued',
    band: 1,
    queue_rank: 1,
    force_pin: false,
    progress: {},
    created_by: 'web',
    warnings: [],
    units_done: 0,
    label: 'flux2 · 1 image',
    created_at: '2026-08-16T11:00:00Z',
    estimate_samples: 0,
    ...over
  };
}

function snap(over: Partial<QueueSnapshot> = {}): QueueSnapshot {
  return { jobs: [], paused: false, pending: 0, running: 0, stuck: [], ...over };
}

describe('queueHeaders', () => {
  it('sends nothing for an untouched form', () => {
    expect(queueHeaders({})).toEqual({});
    expect(queueHeaders(undefined)).toEqual({});
    expect(queueHeaders(null)).toEqual({});
  });

  it('never sends a mode the user did not choose', () => {
    expect(queueHeaders({ band: 2 })).toEqual({ 'X-Queue-Band': '2' });
  });

  it('sends band 0 rather than treating it as unset', () => {
    expect(queueHeaders({ band: 0 })).toEqual({ 'X-Queue-Band': '0' });
  });

  it('carries every override at once', () => {
    expect(
      queueHeaders({ mode: 'now', band: 0, gpu: 'GPU-00000000', force: true, after: 'music3_abc' })
    ).toEqual({
      'X-Queue-Mode': 'now',
      'X-Queue-Band': '0',
      'X-Queue-Gpu': 'GPU-00000000',
      'X-Queue-Force': '1',
      'X-Queue-After': 'music3_abc'
    });
  });

  it('drops force when no card is pinned', () => {
    expect(queueHeaders({ force: true })).toEqual({});
  });

  it('drops an empty pin and an empty dependency', () => {
    expect(queueHeaders({ gpu: '', after: '' })).toEqual({});
  });

  it('reports whether anything is overridden', () => {
    expect(hasQueueOverrides({})).toBe(false);
    expect(hasQueueOverrides({ force: true })).toBe(false);
    expect(hasQueueOverrides({ mode: 'now' })).toBe(true);
  });
});

describe('effectiveBand', () => {
  it('mirrors the server default per mode', () => {
    expect(effectiveBand({})).toBe(1);
    expect(effectiveBand({ mode: 'now' })).toBe(0);
    expect(effectiveBand({ mode: 'queue' })).toBe(1);
  });

  it('lets an explicit band win over the mode', () => {
    expect(effectiveBand({ mode: 'now', band: 2 })).toBe(2);
  });
});

describe('submitConsequence', () => {
  // 🔴 The regression this guards: on a box locked before this session opened
  // there is no blocked row of ANY kind to learn it from, so the same-kind probe
  // sees an empty queue and the button would have read "starts now".
  it('says Beefy on an EMPTY queue whose box is locked', () => {
    expect(submitConsequence(snap({ box_locked: 'llm' }), 'flux2', {})).toEqual({
      text: 'queues · the box is held by Beefy LLM (llm)',
      attention: true,
      short: 'beefy',
      immediate: false
    });
  });

  it('says it starts now on an idle queue', () => {
    expect(submitConsequence(snap(), 'flux2', {})).toEqual({
      text: 'starts now',
      attention: false,
      short: 'now',
      immediate: true
    });
  });

  it('counts only the rows that sort ahead of it', () => {
    const s = snap({ jobs: [job({ id: 'a', band: 1 }), job({ id: 'b', band: 2 })], pending: 2 });
    expect(submitConsequence(s, 'flux2', {}).text).toBe('queues at #2');
    expect(submitConsequence(s, 'flux2', { band: 2 }).text).toBe('queues at #3');
    expect(submitConsequence(s, 'flux2', { mode: 'now' }).text).toBe('queues at #1');
  });

  it('adds the batch ETA only when it lands at the tail', () => {
    const s = snap({
      jobs: [job({ id: 'a', band: 1, estimate_samples: 9 }), job({ id: 'b', band: 2 })],
      pending: 2,
      eta_finish: '2026-08-16T02:10:00Z'
    });
    expect(submitConsequence(s, 'flux2', {}).text).toBe('queues at #2');
    expect(submitConsequence(s, 'flux2', { band: 2 }).text).toMatch(
      /^queues at #3 · starts ~\d\d:/
    );
  });

  it('never invents an ETA the server refused to estimate', () => {
    const s = snap({ jobs: [job({ id: 'a' })], pending: 1, eta_finish: '2026-08-16T02:10:00Z' });
    expect(submitConsequence(s, 'flux2', {}).text).toBe('queues at #2');
  });

  it('surfaces a same-kind blocker over the position', () => {
    const s = snap({
      jobs: [job({ blocked_reason: { reason: 'service_disabled', service: 'flux2' } })],
      pending: 1
    });
    expect(submitConsequence(s, 'flux2', {})).toEqual({
      text: 'queues · flux2 is disabled',
      attention: true,
      short: 'blocked',
      immediate: false
    });
    expect(submitConsequence(s, 'music3', {}).text).toBe('queues at #2');
  });

  it('reports a paused queue', () => {
    expect(submitConsequence(snap({ paused: true }), 'flux2', {}).text).toBe(
      'queues · the queue is paused'
    );
  });

  it('names the job it waits on', () => {
    const s = snap({ jobs: [job({ id: 'music3_x', label: 'music3 · 2 takes' })], pending: 1 });
    expect(submitConsequence(s, 'flux2', { after: 'music3_x' }).text).toBe(
      'queues after music3 · 2 takes'
    );
    expect(submitConsequence(s, 'flux2', { after: 'gone' }).text).toBe('queues after gone');
  });

  it('does not claim it starts now while something is rendering', () => {
    const s = snap({ jobs: [job({ id: 'a', state: 'running' })], running: 1 });
    expect(submitConsequence(s, 'flux2', {}).text).toBe('queues at #1');
  });
});

describe('enqueueReceipt', () => {
  it('states the position and the start clock', () => {
    const view = enqueueReceipt({
      state: 'queued',
      position: 3,
      eta_start: '2026-08-16T02:10:00Z',
      deduplicated: false
    });
    expect(view.text).toMatch(/^queued at #3, starts ~\d\d:\d\d$/);
    expect(view.attention).toBe(false);
  });

  it('drops the clock the server omitted', () => {
    expect(enqueueReceipt({ state: 'queued', position: 3 }).text).toBe('queued at #3');
  });

  it('says when a retried click created no new work', () => {
    expect(enqueueReceipt({ state: 'queued', position: 3, deduplicated: true }).text).toBe(
      'already queued at #3'
    );
    expect(enqueueReceipt({ state: 'running', deduplicated: true }).text).toBe('already running');
  });

  it('reports a started job rather than a position', () => {
    expect(enqueueReceipt({ state: 'running' }).text).toBe('started');
    expect(enqueueReceipt({ state: 'collecting' }).text).toBe('started');
  });

  it('leads with the blocker', () => {
    expect(
      enqueueReceipt({
        state: 'queued',
        position: 1,
        blocked_reason: { reason: 'dependency_pending', parent: 'music3_x' }
      })
    ).toEqual({
      text: 'queued · waiting on music3_x',
      attention: true,
      short: 'blocked',
      immediate: false
    });
  });

  it('flags a held row', () => {
    expect(enqueueReceipt({ state: 'held', position: 2 })).toEqual({
      text: 'queued · held',
      attention: true,
      short: 'held',
      immediate: false
    });
  });

  it('copes with an accept that carries only a position', () => {
    expect(enqueueReceipt({ position: 2 }).text).toBe('queued at #2');
    expect(enqueueReceipt({ position: null }).text).toBe('queued');
  });
});

/// The chip beside the render button has room for a word. It must still say something DIFFERENT in
/// every state — a chip that reads "queued" whether the job is third in line, held, or waiting on a
/// disabled service is a decoration, and the sentence it abbreviates is only a hover away.
describe('the terse form', () => {
  it('separates the states a chip has to tell apart', () => {
    expect(submitConsequence(snap(), 'flux2', {}).short).toBe('now');
    expect(submitConsequence(snap(), 'flux2', {}).immediate).toBe(true);

    const busy = snap({ jobs: [job({ id: 'a', band: 1 }), job({ id: 'b', band: 2 })], pending: 2 });
    expect(submitConsequence(busy, 'flux2', {}).short).toBe('#2');
    expect(submitConsequence(busy, 'flux2', { mode: 'now' }).short).toBe('#1');
    expect(submitConsequence(busy, 'flux2', {}).immediate).toBe(false);

    expect(submitConsequence(snap({ paused: true }), 'flux2', {}).short).toBe('paused');

    const chained = snap({ jobs: [job({ id: 'music3_x' })], pending: 1 });
    expect(submitConsequence(chained, 'flux2', { after: 'music3_x' }).short).toBe('chained');
  });

  it('keeps the receipt terse without losing what happened', () => {
    expect(enqueueReceipt({ state: 'queued', position: 3 }).short).toBe('#3');
    expect(enqueueReceipt({ state: 'queued', position: null }).short).toBe('queued');
    expect(enqueueReceipt({ state: 'running' }).short).toBe('running');
    expect(enqueueReceipt({ state: 'running' }).immediate).toBe(true);
    expect(enqueueReceipt({ state: 'running', deduplicated: true }).short).toBe('running');
  });
});
