import { describe, expect, it } from 'vitest';
import { planActiveJob, type VideoJobLike } from './activeJob';

const job = (job_id: string, status: string, created_at?: string): VideoJobLike => ({
  job_id,
  status,
  created_at
});

describe('planActiveJob', () => {
  it('adopts the newest unfinished render when this device remembers nothing', () => {
    const plan = planActiveJob(null, [job('new', 'rendering'), job('old', 'queued')]);
    expect(plan).toEqual({ adopt: 'new', running: true, forget: false, createdAt: null });
  });

  it('skips finished renders to reach the unfinished one', () => {
    const plan = planActiveJob(null, [job('done', 'completed'), job('live', 'rendering')]);
    expect(plan.adopt).toBe('live');
  });

  it('stays on its own render rather than jumping to a newer foreign one', () => {
    const plan = planActiveJob('mine', [job('theirs', 'queued'), job('mine', 'rendering')]);
    expect(plan.adopt).toBe('mine');
  });

  it('keeps a remembered render that has finished, so a reload still shows its clip', () => {
    const plan = planActiveJob('mine', [job('mine', 'completed')]);
    expect(plan).toEqual({ adopt: 'mine', running: false, forget: false, createdAt: null });
  });

  it('prefers a live render over the remembered one once that has finished', () => {
    const plan = planActiveJob('mine', [job('theirs', 'rendering'), job('mine', 'failed')]);
    expect(plan).toEqual({ adopt: 'theirs', running: true, forget: false, createdAt: null });
  });

  it('forgets a remembered id the server has never heard of', () => {
    const plan = planActiveJob('ghost', []);
    expect(plan).toEqual({ adopt: null, running: false, forget: true, createdAt: null });
  });

  it('drops a ghost id in favour of the render actually in flight', () => {
    const plan = planActiveJob('ghost', [job('live', 'rendering')]);
    expect(plan).toEqual({ adopt: 'live', running: true, forget: false, createdAt: null });
  });

  it('stays idle when every known render is finished and none is remembered', () => {
    const plan = planActiveJob(null, [job('a', 'completed'), job('b', 'cancelled')]);
    expect(plan).toEqual({ adopt: null, running: false, forget: false, createdAt: null });
  });

  it('falls back to the remembered id when the job list cannot be fetched', () => {
    expect(planActiveJob('mine', null)).toEqual({
      adopt: 'mine',
      running: true,
      forget: false,
      createdAt: null
    });
    expect(planActiveJob(null, null)).toEqual({
      adopt: null,
      running: false,
      forget: false,
      createdAt: null
    });
  });

  it('treats a blank stored key as nothing remembered', () => {
    expect(planActiveJob('  ', [])).toEqual({
      adopt: null,
      running: false,
      forget: false,
      createdAt: null
    });
  });

  it('hands back the adopted render start time so elapsed counts from the render', () => {
    const plan = planActiveJob(null, [job('live', 'rendering', '2026-07-31T11:58:00Z')]);
    expect(plan.createdAt).toBe('2026-07-31T11:58:00Z');
  });

  it('hands back the start time of a remembered render that has already finished', () => {
    const plan = planActiveJob('mine', [job('mine', 'completed', '2026-07-31T11:00:00Z')]);
    expect(plan).toEqual({
      adopt: 'mine',
      running: false,
      forget: false,
      createdAt: '2026-07-31T11:00:00Z'
    });
  });

  it('picks the newest by timestamp even when the list arrives out of order', () => {
    const plan = planActiveJob(null, [
      job('older', 'rendering', '2026-07-31T11:00:00Z'),
      job('newer', 'queued', '2026-07-31T11:30:00Z')
    ]);
    expect(plan.adopt).toBe('newer');
  });

  it('keeps the list order when a candidate carries no timestamp', () => {
    const plan = planActiveJob(null, [
      job('first', 'rendering'),
      job('second', 'queued', '2026-07-31T11:30:00Z')
    ]);
    expect(plan.adopt).toBe('first');
  });

  it('ignores an unparseable timestamp rather than letting it win or lose on ordering', () => {
    const plan = planActiveJob(null, [
      job('first', 'rendering', 'garbage'),
      job('second', 'queued', '2026-07-31T11:30:00Z')
    ]);
    expect(plan.adopt).toBe('first');
  });
});
