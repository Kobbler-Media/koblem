import { describe, expect, it } from 'vitest';
import {
  SCRIPT_PASS_MAX_UNITS,
  scriptPassBrief,
  scriptPassOutcome,
  scriptPassSeconds,
  type ScriptPassUnit
} from './script-pass';

/**
 * The one-pass writer's arithmetic and its brief.
 *
 * The property under test throughout is that the pass is SIZED BY THE TIMELINE and lands back on
 * it by position — the reply can never bring a render unit into existence, because on H3 the
 * generations are derived from the clips and are never stored as a packing.
 */

const units: ScriptPassUnit[] = [
  { id: 'gen:a', seconds: 15.083, parts: ['a woman at a bar', 'she puts the glass down', ''] },
  { id: 'gen:b', seconds: 8, parts: ['the door opens'] }
];

describe('scriptPassSeconds', () => {
  /**
   * 🔴 The SHORTEST, not the mean. Too short only makes the last beat play longer; too long writes
   * cut times past the end of a shorter generation, which is sent and renders wrong in silence.
   */
  it('is the shortest unit', () => {
    expect(scriptPassSeconds(units)).toBe(8);
  });

  it('is absent when nothing has a length, so the caller can omit the field', () => {
    expect(scriptPassSeconds([])).toBeUndefined();
    expect(scriptPassSeconds([{ id: 'x', seconds: 0, parts: [] }])).toBeUndefined();
  });
});

describe('scriptPassBrief', () => {
  const brief = scriptPassBrief('a night at the bar', units, { unit: 'generation', part: 'shot' });

  /** The author's own brief comes first — the listing is the constraint, not the idea. */
  it('opens with the brief', () => {
    expect(brief.startsWith('a night at the bar')).toBe(true);
  });

  /**
   * 🔴 The words are the MODEL's, not the screen's. `script_system_prompt` says "break the brief
   * into N GENERATIONS … may hold SEVERAL `[Shot N]` blocks"; the UI calls a generation a shot.
   */
  it('names the units the way the system prompt does', () => {
    expect(brief).toContain('Generation 1 — 15.08s, exactly 3 shots:');
    expect(brief).toContain('Generation 2 — 8.00s, exactly 1 shot:');
  });

  /** The count is a requirement because the reply is mapped back onto the units BY POSITION: a
   *  short reply does not leave a gap, it slides every later block onto the wrong unit. */
  it('lists every part, including one nobody has written yet', () => {
    expect(brief).toContain('[Shot 1] a woman at a bar');
    expect(brief).toContain('[Shot 2] she puts the glass down');
    expect(brief).toContain('[Shot 3] (you decide');
  });

  /** On LTX a render has no parts, so the unit's own one-liner is the whole line. */
  it('writes one line per unit when the engine has no sub-units', () => {
    const ltx = scriptPassBrief('', [{ id: 'c1', seconds: 5, parts: ['a wide of the harbour'] }], {
      unit: 'shot',
      part: null
    });
    expect(ltx).toContain('Shot 1 — 5.00s: a wide of the harbour');
    expect(ltx).not.toContain('exactly');
    // An empty author brief still produces a usable one: the listing alone is the brief, which is
    // what keeps the endpoint's "brief or image is required" satisfied without an image.
    expect(ltx.trim().length).toBeGreaterThan(0);
  });
});

describe('scriptPassOutcome', () => {
  it('applies the overlap and says nothing when the counts agree', () => {
    expect(scriptPassOutcome(3, 3, 'shots')).toEqual({ applied: 3, warnings: [] });
  });

  /** A short reply leaves the tail exactly as it was — a half-written batch is not a destructive one. */
  it('reports a short reply without clearing the rest', () => {
    const out = scriptPassOutcome(4, 2, 'shots');
    expect(out.applied).toBe(2);
    expect(out.warnings[0]).toContain('2 of 4');
  });

  /** 🔴 A surplus block has nowhere to land: generations are derived, so nothing can mint one. */
  it('reports a surplus reply as dropped', () => {
    const out = scriptPassOutcome(2, 5, 'shots');
    expect(out.applied).toBe(2);
    expect(out.warnings[0]).toContain('dropped');
  });
});

describe('SCRIPT_PASS_MAX_UNITS', () => {
  /** Mirrors the server's `req.n_segments.clamp(1, 24)` so the editor refuses instead of truncating. */
  it('is the endpoint ceiling', () => {
    expect(SCRIPT_PASS_MAX_UNITS).toBe(24);
  });
});
