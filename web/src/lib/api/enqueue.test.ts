import { describe, it, expect } from 'vitest';
import {
  isEnqueued,
  imageJobResult,
  krea2JobResult,
  type EnqueueResponse,
  type ImageGenerateResponse,
  type ImageJob,
  type Krea2Job
} from './client';

const accepted: EnqueueResponse = {
  job_id: 'job-1',
  state: 'queued',
  position: 3,
  deduplicated: false
};

const finished: ImageGenerateResponse = {
  images: [{ url: '/g/1.png', seed: 7 }],
  model_used: 'base',
  width: 1024,
  height: 576,
  steps: 50,
  cfg: 4,
  render_ms: 1200,
  gate_wait_ms: 30,
  generation_id: 'gen-1'
};

const job: ImageJob = {
  id: 'job-1',
  state: 'done',
  progress: {},
  images: [{ url: '/g/1.png', seed: 7 }],
  generation_id: 'gen-1',
  model_used: 'edit',
  width: 512,
  height: 512,
  steps: 4,
  cfg: 1,
  warnings: [],
  admit_ms: 90,
  duration_ms: 4500
};

describe('sync-if-fast', () => {
  it('tells an accepted job from a finished render', () => {
    expect(isEnqueued(accepted)).toBe(true);
    expect(isEnqueued(finished)).toBe(false);
  });

  // 🔴 The job row names these `admit_ms` / `duration_ms`; the inline body names them
  // `gate_wait_ms` / `render_ms`. A miss here shows as a render that took 0 s.
  it('maps a finished row onto the inline body', () => {
    const mapped = imageJobResult(job);
    expect(mapped.gate_wait_ms).toBe(90);
    expect(mapped.render_ms).toBe(4500);
    expect(mapped.model_used).toBe('edit');
    expect(mapped.images).toEqual(job.images);
  });

  // Absent means UNKNOWN on the wire, and the inline body's fields are not optional.
  it('reports unknown timings as zero rather than NaN', () => {
    const mapped = imageJobResult({ ...job, admit_ms: null, duration_ms: undefined });
    expect(mapped.gate_wait_ms).toBe(0);
    expect(mapped.render_ms).toBe(0);
  });

  it('carries krea2 mode_used through the same mapping', () => {
    const { model_used: _flux, ...shared } = job;
    void _flux;
    const k: Krea2Job = { ...shared, mode_used: 'restage' };
    expect(krea2JobResult(k).mode_used).toBe('restage');
  });
});
