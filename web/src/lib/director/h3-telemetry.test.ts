import { describe, expect, it } from 'vitest';
import type { LtxJobStatus } from '#lib/api/client.ts';
import type { H3Plan } from './h3-plan';
import {
  H3_CHECKPOINT_FLIP_SECONDS,
  formatDuration,
  h3Conditioning,
  h3PhaseLabel,
  h3ReloadCost,
  h3RenderWall,
  readH3Telemetry
} from './h3-telemetry';

const job = (over: Partial<LtxJobStatus> = {}): LtxJobStatus =>
  ({
    job_id: 'j1',
    status: 'rendering',
    resumable: false,
    segments: 0,
    scripts: [],
    gate_wait_ms: 0,
    render_ms: 0,
    ...over
  }) as LtxJobStatus;

describe('readH3Telemetry — absence is unknown, never zero', () => {
  /** 🔴 The failure this whole module exists to prevent: a job that has not started stepping must
   *  not render as "step 0 of 0", which a progress bar draws as FINISHED. */
  it('reports nothing at all for a job carrying no telemetry', () => {
    const t = readH3Telemetry(job());
    expect(t.known).toBe(false);
    expect(t.percent).toBeNull();
    expect(t.step).toBeNull();
    expect(t.totalSteps).toBeNull();
    expect(t.stepLabel).toBeNull();
    expect(t.generation).toBeNull();
    expect(t.generationLabel).toBeNull();
    expect(t.etaSeconds).toBeNull();
  });

  it('is safe on a null job — an LTX render never carries these keys', () => {
    expect(readH3Telemetry(null).known).toBe(false);
  });

  /** Both numbers or neither: a step with no total reads as "step 7 of ∞", which is the same
   *  failure wearing a different hat. */
  it('withholds the step label when only one half is known', () => {
    expect(readH3Telemetry(job({ current_step: 7 })).stepLabel).toBeNull();
    expect(readH3Telemetry(job({ total_steps: 20 })).stepLabel).toBeNull();
    expect(readH3Telemetry(job({ current_step: 7, total_steps: 20 })).stepLabel).toBe(
      'step 7 of 20'
    );
  });

  /**
   * ⚠️ The FIELDS keep the wire's word and the LABEL does not. `generation_index` is what the shim
   * reports and `generation`/`generationCount` mirror it; what the author is watching render is a
   * SHOT (`EngineVocab`), so that is what the sentence says. Pinned because the two drifting apart
   * — a label saying "generation" next to a timeline saying "Shot 2" — is the exact confusion the
   * display vocabulary was introduced to remove.
   */
  it('turns the 0-based generation index into a 1-based SHOT label against `segments`', () => {
    const t = readH3Telemetry(job({ generation_index: 1, generations_done: 1, segments: 4 }));
    expect(t.generation).toBe(2);
    expect(t.generationCount).toBe(4);
    expect(t.generationLabel).toBe('shot 2 of 4');
  });

  /** `segments` exists on every LTX job too. Reading it without a generation index would invent a
   *  generation count for an engine that has no generations. */
  it('does not read a generation count off a job with no generation index', () => {
    expect(readH3Telemetry(job({ segments: 6 })).generationCount).toBeNull();
  });

  it('scales progress to a percent and clamps it', () => {
    expect(readH3Telemetry(job({ progress: 0.25 })).percent).toBe(25);
    expect(readH3Telemetry(job({ progress: 1.4 })).percent).toBe(100);
  });

  /** 0% is a real answer and must survive as one — it is `null` that means "did not say". */
  it('keeps a reported 0% distinct from an unreported one', () => {
    expect(readH3Telemetry(job({ progress: 0 })).percent).toBe(0);
    expect(readH3Telemetry(job({ progress: 0 })).known).toBe(true);
    expect(readH3Telemetry(job()).percent).toBeNull();
  });
});

describe('h3PhaseLabel — a checkpoint swap is not a stall', () => {
  /** 🔴 The single most valuable readout on an H3 render: minutes of frozen step counter while
   *  ~20 GB of DiT and text encoder load, which otherwise looks exactly like a hang. */
  it('names `loading` as a checkpoint swap', () => {
    expect(h3PhaseLabel('loading')).toBe('Swapping checkpoint');
    expect(readH3Telemetry(job({ phase: 'loading' })).swapping).toBe(true);
    expect(readH3Telemetry(job({ phase: 'loading' })).phaseNote).toContain('has not stalled');
  });

  it('leaves every other phase without a defensive note', () => {
    expect(readH3Telemetry(job({ phase: 'denoising' })).phaseNote).toBeNull();
  });

  /** The shim owns this vocabulary and may grow it — printing what it said beats claiming the
   *  render is in an unknown state. */
  it('passes an unrecognised phase through verbatim', () => {
    expect(h3PhaseLabel('vae_tiling')).toBe('vae_tiling');
    expect(h3PhaseLabel(null)).toBeNull();
  });
});

describe('formatDuration', () => {
  it('reads an H3-scale ETA as minutes', () => {
    expect(formatDuration(48)).toBe('48s');
    expect(formatDuration(830)).toBe('13m 50s');
    expect(formatDuration(1350)).toBe('22m 30s');
    expect(formatDuration(3600)).toBe('1h 0m');
  });

  it('is null for an unknown duration rather than "0s"', () => {
    expect(formatDuration(null)).toBeNull();
    expect(formatDuration(undefined)).toBeNull();
  });
});

// ── The stamped plan echo: what a finished render actually cost ────────────────────────────────

const plan = (over: Partial<H3Plan> = {}): H3Plan => ({
  generation_plans: [],
  total_eta_seconds: null,
  canvas: null,
  warnings: [],
  ignored: [],
  dropped: [],
  frames: null,
  duration_seconds: null,
  gate_wait_ms: null,
  render_ms: null,
  at: null,
  ...over
});

describe('h3RenderWall — the queue is not the render', () => {
  /**
   * 🔴 The measurement the "~1 hr" report was missing. A 24.9-minute render was reported as an hour
   * because the editor timed its own session: a failed submit, a 54.2 s wait for the card, a retry,
   * and then the render. The two halves are drawn apart because their fixes are different.
   */
  it('separates the gate wait from the render', () => {
    const w = h3RenderWall(plan({ render_ms: 1_494_000, gate_wait_ms: 54_200 }));
    expect(w.render).toBe('24m 54s');
    expect(w.gateWait).toBe('54s');
  });

  /** A card that was free is a real zero, and not worth a line saying "0s of queue". */
  it('omits a zero gate wait rather than drawing it', () => {
    expect(h3RenderWall(plan({ render_ms: 60_000, gate_wait_ms: 0 })).gateWait).toBeNull();
  });

  /** A plan stamped by an older koblem carries no timing — and "0s" would read as instant. */
  it('says nothing at all when the render was never measured', () => {
    const w = h3RenderWall(plan());
    expect(w.render).toBeNull();
    expect(w.gateWait).toBeNull();
    expect(w.generations).toBeNull();
  });

  /**
   * 🔴 Summed only when EVERY generation reported. A partial sum drawn beside the whole-job
   * duration reads as "the rest was overhead", when the truth is the render stopped early.
   */
  it('sums the per-generation seconds only when the set is complete', () => {
    const full = plan({
      render_ms: 1_500_000,
      generation_plans: [{ render_seconds: 967.5 }, { render_seconds: 520.4 }]
    });
    expect(h3RenderWall(full).generations).toBe('24m 48s');
    const partial = plan({
      render_ms: 1_500_000,
      generation_plans: [{ render_seconds: 967.5 }, {}]
    });
    expect(h3RenderWall(partial).generations).toBeNull();
  });
});

describe('h3ReloadCost — a switch count nobody can price is not a warning', () => {
  /** 64 s is MEASURED (2026-08-07): 31.9 s reload + 32.5 s of slower first step. */
  it('prices each checkpoint switch at the measured flip cost', () => {
    const c = h3ReloadCost(
      plan({
        generation_plans: [
          { reload_required: false },
          { reload_required: true },
          { reload_required: true }
        ]
      })
    );
    expect(c.reloads).toBe(2);
    expect(c.seconds).toBe(2 * H3_CHECKPOINT_FLIP_SECONDS);
    expect(c.note).toContain('2m 8s');
  });

  it('says nothing on a chain that never switches', () => {
    expect(h3ReloadCost(plan({ generation_plans: [{ reload_required: false }] })).note).toBeNull();
    expect(h3ReloadCost(null).reloads).toBe(0);
  });
});

describe('h3Conditioning — the earliest signal that conditioning was dropped', () => {
  /** 🔴 A `with_audio` clip packs TWO labels and the prompt may address either. An echo naming only
   *  the video under-reports a handle the author had. */
  it('names both ordinals of a soundtracked clip, in packing order', () => {
    const c = h3Conditioning({
      task: 'ref2va',
      references: [
        { ordinal: '<Picture 1>', audio_ordinal: null, kind: 'image', label: 'Mia' },
        { ordinal: '<Video 1>', audio_ordinal: '<Audio 1>', kind: 'video', label: 'the song' }
      ]
    });
    expect(c.labels).toEqual(['<Picture 1> · Mia', '<Audio 1> + <Video 1> · the song']);
    expect(c.bare).toBe(false);
    expect(c.note).toBeNull();
  });

  /** `t2va` is the tell: the render succeeded and nothing conditioned it. */
  it('calls out a generation that fell back to text only', () => {
    const c = h3Conditioning({ task: 't2va', references: [] });
    expect(c.bare).toBe(true);
    expect(c.note).toContain('text only');
  });

  /** A reference task with no references echoed back is conditioning lost in transit. */
  it('calls out a reference task that arrived with nothing to reference', () => {
    expect(h3Conditioning({ task: 'ref2va', references: [] }).note).toContain('went missing');
  });

  /** An older shim omits the array entirely. Silence is not evidence of a dropped reference — but
   *  it is also not evidence of a good one, so the empty case is reported the same way. */
  it('draws no labels when the engine did not echo any', () => {
    expect(h3Conditioning({}).labels).toEqual([]);
    expect(h3Conditioning({}).note).toBeNull();
  });
});
