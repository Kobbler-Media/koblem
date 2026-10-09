import { describe, expect, it } from 'vitest';
import type { Job, QueueSnapshot } from '$lib/api/client';
import {
  batchEta,
  blockedSummary,
  clockOf,
  elapsedMs,
  humanMs,
  humanMsShort,
  indicatorState,
  jobEta,
  reorderAfterId,
  unitsLabel
} from './format';

const NOW = Date.parse('2026-08-16T12:00:00Z');

function job(over: Partial<Job> = {}): Job {
  return {
    id: 'h3_abc',
    kind: 'h3',
    state: 'queued',
    band: 1,
    queue_rank: 1,
    force_pin: false,
    progress: {},
    created_by: 'web',
    warnings: [],
    units_done: 0,
    label: 'h3 · 16 generations',
    created_at: '2026-08-16T11:00:00Z',
    estimate_samples: 0,
    ...over
  };
}

function snap(over: Partial<QueueSnapshot> = {}): QueueSnapshot {
  return { jobs: [], paused: false, pending: 0, running: 0, stuck: [], ...over };
}

describe('humanMs', () => {
  it('matches the CLI ladder', () => {
    expect(humanMs(45_000)).toBe('45s');
    expect(humanMs(125_000)).toBe('2m 5s');
    expect(humanMs(15_120_000)).toBe('4h 12m');
  });

  it('clamps a clock skew to zero rather than counting backwards', () => {
    expect(humanMs(-5000)).toBe('0s');
  });
});

describe('humanMsShort', () => {
  it('drops the finer unit for the title bar', () => {
    expect(humanMsShort(45_000)).toBe('45s');
    expect(humanMsShort(125_000)).toBe('2m');
    expect(humanMsShort(15_120_000)).toBe('4h');
  });
});

describe('elapsedMs', () => {
  it('is null for an absent or unparseable stamp', () => {
    expect(elapsedMs(null, NOW)).toBeNull();
    expect(elapsedMs(undefined, NOW)).toBeNull();
    expect(elapsedMs('not a date', NOW)).toBeNull();
  });

  it('measures from the stamp', () => {
    expect(elapsedMs('2026-08-16T11:00:00Z', NOW)).toBe(3_600_000);
  });
});

describe('blockedSummary', () => {
  it('says the same sentence as the server for every variant', () => {
    expect(blockedSummary({ reason: 'service_disabled', service: 'flux2' })).toBe(
      'flux2 is disabled'
    );
    expect(blockedSummary({ reason: 'no_card_fits', detail: '24G needed, 11G free' })).toBe(
      '24G needed, 11G free'
    );
    expect(blockedSummary({ reason: 'lane_busy', lane: 'comfy' })).toBe('comfy slot busy');
    expect(blockedSummary({ reason: 'eviction_cooldown' })).toBe('an eviction is settling');
    expect(blockedSummary({ reason: 'dependency_pending', parent: 'h3_1' })).toBe(
      'waiting on h3_1'
    );
    expect(blockedSummary({ reason: 'engine_unreachable', detail: 'timeout' })).toBe(
      'engine unreachable: timeout'
    );
    expect(blockedSummary({ reason: 'head_reserved', head: 'ltx_9' })).toBe(
      'holding resources for ltx_9'
    );
    expect(blockedSummary({ reason: 'paused' })).toBe('the queue is paused');
    expect(blockedSummary({ reason: 'box_locked', service: 'llm' })).toBe(
      'the box is held by Beefy LLM (llm)'
    );
  });
});

describe('jobEta', () => {
  it('refuses to render a time off a zero-sample estimate', () => {
    const j = job({ estimate_samples: 0, eta_start: '2026-08-16T12:30:00Z' });
    expect(jobEta(j)).toBeNull();
  });

  it('carries the sample count with the times', () => {
    const j = job({
      estimate_samples: 3,
      eta_start: '2026-08-16T12:30:00Z',
      eta_finish: '2026-08-16T13:00:00Z'
    });
    expect(jobEta(j)).toEqual({
      starts: clockOf('2026-08-16T12:30:00Z'),
      finishes: clockOf('2026-08-16T13:00:00Z'),
      samples: 3
    });
  });

  it('is null when the server sent no times at all', () => {
    expect(jobEta(job({ estimate_samples: 9 }))).toBeNull();
  });
});

describe('batchEta', () => {
  it('is null when nothing in the batch has ever been measured', () => {
    const s = snap({ eta_finish: '2026-08-16T13:00:00Z', jobs: [job()] });
    expect(batchEta(s)).toBeNull();
  });

  it('counts the pending rows the estimate excludes', () => {
    const s = snap({
      eta_finish: '2026-08-16T13:00:00Z',
      jobs: [
        job({ id: 'a', estimate_samples: 12 }),
        job({ id: 'b', estimate_samples: 0 }),
        job({ id: 'c', state: 'done', estimate_samples: 0 })
      ]
    });
    expect(batchEta(s)).toEqual({ at: clockOf('2026-08-16T13:00:00Z'), unestimated: 1 });
  });
});

describe('unitsLabel', () => {
  it('prefers the job unit count', () => {
    expect(unitsLabel(job({ units_done: 2, units_total: 16 }))).toBe('2/16');
  });

  it('falls back to engine steps', () => {
    expect(unitsLabel(job({ progress: { current_step: 4, total_steps: 30 } }))).toBe('4/30');
  });

  it('is null when nothing is published — a 0/0 bar reads as finished', () => {
    expect(unitsLabel(job())).toBeNull();
    expect(unitsLabel(job({ units_total: 0 }))).toBeNull();
  });
});

describe('reorderAfterId', () => {
  const order = ['a', 'b', 'c'];

  it('is null at the head of the band', () => {
    expect(reorderAfterId(order, 0)).toBeNull();
  });

  it('names the row the job now follows', () => {
    expect(reorderAfterId(order, 1)).toBe('a');
    expect(reorderAfterId(order, 2)).toBe('b');
  });

  it('is null off the end rather than undefined', () => {
    expect(reorderAfterId(order, 9)).toBeNull();
  });
});

describe('indicatorState', () => {
  it('is a quiet nav entry when the queue is empty', () => {
    const view = indicatorState(snap(), NOW);
    expect(view.mode).toBe('idle');
    expect(view.text).toBe('⛁ Queue');
    expect(view.title).toBe('GPU queue — nothing running');
  });

  it('reports the running job and the batch finish', () => {
    const s = snap({
      pending: 3,
      running: 1,
      eta_finish: '2026-08-16T16:20:00Z',
      jobs: [
        job({
          id: 'live',
          state: 'running',
          units_done: 2,
          units_total: 16,
          estimate_samples: 8
        })
      ]
    });
    const view = indicatorState(s, NOW);
    expect(view.mode).toBe('active');
    expect(view.text).toBe(`⛁ 4 · rendering h3 (2/16) · done ~${clockOf('2026-08-16T16:20:00Z')}`);
  });

  it('omits the batch finish when nothing has been measured', () => {
    const s = snap({ pending: 1, running: 0, eta_finish: '2026-08-16T16:20:00Z', jobs: [job()] });
    expect(indicatorState(s, NOW).text).toBe('⛁ 1 · 1 queued');
  });

  it('lets the blocked state win over a running job', () => {
    const s = snap({
      pending: 3,
      running: 1,
      stuck: ['stuck1'],
      jobs: [
        job({ id: 'live', state: 'running', units_done: 2, units_total: 16 }),
        job({
          id: 'stuck1',
          blocked_reason: { reason: 'service_disabled', service: 'flux2' },
          blocked_since: '2026-08-16T08:00:00Z'
        })
      ]
    });
    const view = indicatorState(s, NOW);
    expect(view.mode).toBe('blocked');
    expect(view.text).toBe('⛁ 4 · 1 BLOCKED 4h · flux2 is disabled');
  });

  it('reports the longest-blocked of the stuck rows', () => {
    const s = snap({
      pending: 2,
      running: 0,
      stuck: ['a', 'b'],
      jobs: [
        job({
          id: 'a',
          blocked_reason: { reason: 'paused' },
          blocked_since: '2026-08-16T11:30:00Z'
        }),
        job({
          id: 'b',
          blocked_reason: { reason: 'service_disabled', service: 'avatar' },
          blocked_since: '2026-08-16T06:00:00Z'
        })
      ]
    });
    expect(indicatorState(s, NOW).text).toBe('⛁ 2 · 2 BLOCKED 6h · avatar is disabled');
  });

  // 🔴 Beefy on an EMPTY queue is the case worth a test of its own: every other
  // reading of "nothing running" is idle, and this one means nothing CAN run.
  it('says Beefy over idle when the box is locked and nothing is queued', () => {
    const view = indicatorState(snap({ box_locked: 'llm' }), NOW);
    expect(view.mode).toBe('beefy');
    expect(view.text).toBe('⚡ BEEFY LLM');
  });

  it('says Beefy over a blocked pile, and counts what is waiting on it', () => {
    const s = snap({
      pending: 5,
      running: 0,
      box_locked: 'llm',
      stuck: ['a'],
      jobs: [
        job({
          id: 'a',
          blocked_reason: { reason: 'box_locked', service: 'llm' },
          blocked_since: '2026-08-16T06:00:00Z'
        })
      ]
    });
    const view = indicatorState(s, NOW);
    expect(view.mode).toBe('beefy');
    expect(view.text).toBe('⚡ BEEFY LLM · 5 waiting');
  });
});
