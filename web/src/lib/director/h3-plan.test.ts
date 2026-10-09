import { describe, expect, it } from 'vitest';
import { planIsEmpty, planNoteCount, planReloads, readH3Plan } from './h3-plan';

/** The shape koblem stamps: the shim's 202 body plus koblem's own `dropped` and `at`. */
const stamped = {
  h3_plan: {
    generation_plans: [
      {
        index: 0,
        task: 't2va',
        frames: 362,
        seconds: 15.083,
        checkpoint: 'minimax_h3_fl2va_pruned_local',
        reload_required: false,
        eta_seconds: 830.0
      },
      {
        index: 1,
        task: 'ref2va',
        frames: 209,
        seconds: 8.708,
        checkpoint: 'minimax_h3_ref2va_pruned_local',
        reload_required: true,
        eta_seconds: 520.0
      }
    ],
    total_eta_seconds: 1350.0,
    canvas: { width: 1344, height: 768, fps: 24 },
    warnings: ['generations[0].frames 350 snapped UP to 362'],
    ignored: ['sigma_shift_video', 'pin_strength'],
    dropped: ['loras', 'beats'],
    frames: 571,
    duration_seconds: 23.791,
    at: '2026-08-07T10:00:00Z'
  }
};

describe('readH3Plan', () => {
  it('reads the three lists apart from each other', () => {
    const plan = readH3Plan(stamped)!;
    expect(plan.warnings).toEqual(['generations[0].frames 350 snapped UP to 362']);
    expect(plan.ignored).toEqual(['sigma_shift_video', 'pin_strength']);
    expect(plan.dropped).toEqual(['loras', 'beats']);
    expect(planNoteCount(plan)).toBe(5);
  });

  /** The reload is the reason a render appears to pause for minutes between two generations. */
  it('counts the checkpoint reloads the chain forces', () => {
    expect(planReloads(readH3Plan(stamped))).toBe(1);
  });

  it('is null for a project that has never rendered on H3', () => {
    expect(readH3Plan({})).toBeNull();
    expect(readH3Plan(null)).toBeNull();
    expect(readH3Plan({ h3_plan: 'nope' })).toBeNull();
  });

  /** Every field is optional: the shim may stop sending a diagnostic and an accepted render must
   *  not become an error over one. */
  it('survives a plan carrying nothing but an id-less shell', () => {
    const plan = readH3Plan({ h3_plan: {} })!;
    expect(plan.generation_plans).toEqual([]);
    expect(plan.total_eta_seconds).toBeNull();
    expect(plan.canvas).toBeNull();
    expect(planIsEmpty(plan)).toBe(true);
  });

  it('drops non-string entries rather than rendering a blank row', () => {
    const plan = readH3Plan({ h3_plan: { ignored: ['ok', null, 3, '  '] } })!;
    expect(plan.ignored).toEqual(['ok']);
  });

  /** A run with notes but no per-generation plans still has something to say. */
  it('is not empty when only a note came back', () => {
    expect(planIsEmpty(readH3Plan({ h3_plan: { ignored: ['pin_strength'] } }))).toBe(false);
  });
});
