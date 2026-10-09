import { describe, expect, it } from 'vitest';
import type { Job, JobEvent } from '$lib/api/client';
import { stuckMessage, terminalMessage } from './notify';

const NOW = Date.parse('2026-08-16T12:00:00Z');

function event(over: Partial<JobEvent> = {}): JobEvent {
  return {
    job_id: 'h3_abc',
    kind: 'h3',
    state: 'done',
    warnings: [],
    at: '2026-08-16T12:00:00Z',
    ...over
  };
}

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

describe('terminalMessage', () => {
  it('is silent for everything short of a terminal state', () => {
    for (const state of ['queued', 'running', 'collecting', 'held'] as const) {
      expect(terminalMessage(event({ state }))).toBeNull();
    }
  });

  it('carries the error on a failure', () => {
    const msg = terminalMessage(event({ state: 'failed', error: 'CUDA OOM on card 1' }), 'h3 · 16');
    expect(msg?.title).toBe('Render failed — h3 · 16');
    expect(msg?.body).toBe('CUDA OOM on card 1');
  });

  it('says something rather than nothing when the engine gave no reason', () => {
    expect(terminalMessage(event({ state: 'failed' }))?.body).toBe('the engine gave no reason');
  });

  it('says a success carried warnings, and quotes them', () => {
    const msg = terminalMessage(
      event({ state: 'done', warnings: ['generation 7 dropped: no audio'] }),
      'h3 · 16'
    );
    expect(msg?.title).toBe('Render done with 1 warning — h3 · 16');
    expect(msg?.body).toBe('generation 7 dropped: no audio');
  });

  it('is plain for a clean success', () => {
    expect(terminalMessage(event(), 'h3 · 16')).toEqual({
      title: 'Render done — h3 · 16',
      body: 'no warnings',
      tag: 'queue:h3_abc:done'
    });
  });

  it('names the job by kind and id when there is no label', () => {
    expect(terminalMessage(event({ state: 'cancelled' }))?.title).toBe(
      'Render cancelled — h3 · h3_abc'
    );
  });

  it('tags per job and state so a replayed frame replaces rather than stacks', () => {
    expect(terminalMessage(event({ state: 'failed' }))?.tag).toBe('queue:h3_abc:failed');
    expect(terminalMessage(event({ state: 'done' }))?.tag).toBe('queue:h3_abc:done');
  });
});

describe('stuckMessage', () => {
  it('says what is blocking and for how long', () => {
    const msg = stuckMessage(
      job({
        blocked_reason: { reason: 'service_disabled', service: 'flux2' },
        blocked_since: '2026-08-16T08:00:00Z'
      }),
      NOW
    );
    expect(msg.title).toBe('Queue blocked — h3 · 16 generations');
    expect(msg.body).toBe('flux2 is disabled for 4h');
    expect(msg.tag).toBe('queue:h3_abc:blocked');
  });

  it('drops the duration rather than inventing one', () => {
    const msg = stuckMessage(job({ blocked_reason: { reason: 'eviction_cooldown' } }), NOW);
    expect(msg.body).toBe('an eviction is settling');
  });
});
