import { describe, expect, it } from 'vitest';
import {
  LTX_PATCH,
  PREVIEW_TIERS,
  baseStepsFromSigmas,
  newStage,
  parseSigmas,
  planResolution,
  profileFactors,
  sizeKeyFor,
  sizeTiersFor,
  snap32,
  snapDivisorFor,
  snapTargetFor,
  type CustomStage
} from './resolution';

const stage = (upscaler: string): CustomStage => ({ ...newStage(), upscaler });

describe('snap32', () => {
  it.each([
    [1920, 1920],
    [1080, 1088],
    [0, 32],
    [-40, 32],
    [17, 32]
  ])('snaps %s to %s', (input, want) => {
    expect(snap32(input)).toBe(want);
  });

  it('never returns a non-multiple for junk input', () => {
    expect(snap32(NaN)).toBe(32);
  });
});

describe('profileFactors', () => {
  it('standard is the single upscaler ratio, and nothing at all when it is none', () => {
    expect(profileFactors('standard', 'x2', [])).toEqual([2]);
    expect(profileFactors('standard', 'x1.5', [])).toEqual([1.5]);
    expect(profileFactors('standard', 'none', [])).toEqual([]);
  });

  it('high and 4x bake their own ladders regardless of the ratio dropdown', () => {
    expect(profileFactors('high', 'none', [])).toEqual([2]);
    expect(profileFactors('upscale_4x', 'x1.5', [])).toEqual([2, 2]);
  });

  /** The stale-ratio trap: an emptied Custom stage list is base-only, and the compiled request
   *  forces upscaler_ratio "none" to match. Falling back to the ratio here would make the readout
   *  and the snap divisor claim a ×2 the render does not do. */
  it('custom with no stages is base-only even while a ratio is still selected', () => {
    expect(profileFactors('custom', 'x2', [])).toEqual([]);
  });

  it('custom reads its stage list', () => {
    expect(profileFactors('custom', 'none', [stage('x2'), stage('x1.5')])).toEqual([2, 1.5]);
  });
});

describe('planResolution', () => {
  it('sizes the base pass down from the target and climbs back to it', () => {
    const plan = planResolution(1920, 1088, [2]);
    expect(plan).toMatchObject({ targetW: 1920, targetH: 1088, baseW: 960, baseH: 544, mult: 2 });
    expect(plan.ladder).toEqual([
      { w: 960, h: 544 },
      { w: 1920, h: 1088 }
    ]);
  });

  it('base equals target when nothing upscales', () => {
    const plan = planResolution(1280, 704, []);
    expect(plan.baseW).toBe(1280);
    expect(plan.baseH).toBe(704);
    expect(plan.ladder).toEqual([{ w: 1280, h: 704 }]);
  });

  it('walks every rung of a two-stage ladder', () => {
    const plan = planResolution(1920, 1152, [2, 2]);
    expect(plan.baseW).toBe(480);
    expect(plan.baseH).toBe(288);
    expect(plan.ladder).toEqual([
      { w: 480, h: 288 },
      { w: 960, h: 576 },
      { w: 1920, h: 1152 }
    ]);
  });

  /** An off-grid target is snapped before anything is divided, so the base is derived from the
   *  number the engine will actually be given rather than the one that was typed. */
  it('snaps the target before deriving the base', () => {
    expect(planResolution(1919, 1081, [2])).toMatchObject({ targetW: 1920, targetH: 1088 });
  });
});

describe('snapDivisorFor', () => {
  it.each([
    [1, 32],
    [1.5, 48],
    [2, 64],
    [4, 128]
  ])('total factor %s needs a ÷%s output', (mult, want) => {
    expect(snapDivisorFor(mult)).toBe(want);
  });

  /** The divisor is only ever the patch grid multiplied back UP by what the profile upscales, so a
   *  render that does not upscale is constrained by the patch grid and nothing else. */
  it('is the bare patch grid when nothing upscales', () => {
    expect(snapDivisorFor(1)).toBe(LTX_PATCH);
  });
});

describe('PREVIEW_TIERS', () => {
  /** The proxy is single-pass (`upscaler_ratio: none`, `dims_are_target: false`), so its dims ARE
   *  the base pass: the patch grid is the whole constraint and picking a tier never re-snaps. */
  it('offers proxy sizes already valid on the patch grid', () => {
    for (const t of PREVIEW_TIERS) {
      expect([snap32(t.w), snap32(t.h)]).toEqual([t.w, t.h]);
    }
  });

  /** …and the OUTPUT profile's divisor is not that constraint: half these presets are off it, so
   *  snapping — or labelling — the proxy fields by it promises a grid its own presets break. */
  it.each([2, 4])('is not on the divisor an output profile of ×%s would demand', (mult) => {
    const div = snapDivisorFor(mult);
    expect(PREVIEW_TIERS.every((t) => t.w % div === 0 && t.h % div === 0)).toBe(false);
  });
});

describe('sizeTiersFor', () => {
  /** The point of pre-snapping: the value the dropdown offers is already grid-valid, so choosing it
   *  never fires a surprise snap note. */
  it('offers tiers already valid for the profile divisor', () => {
    const tiers = sizeTiersFor(snapDivisorFor(4), 4);
    for (const t of tiers) {
      expect(t.w % 128).toBe(0);
      expect(t.h % 128).toBe(0);
    }
    expect(tiers.map((t) => t.key)).toEqual(['720p', '1080p', '1440p']);
  });

  it('reports the base dims the profile factor implies', () => {
    const [, hd] = sizeTiersFor(64, 2);
    expect(hd).toMatchObject({ key: '1080p', w: 1920, h: 1088, bw: 960, bh: 544 });
  });

  it('base equals output when nothing upscales', () => {
    const [, hd] = sizeTiersFor(32, 1);
    expect(hd).toMatchObject({ w: 1920, h: 1088, bw: 1920, bh: 1088 });
  });
});

describe('sizeKeyFor', () => {
  const tiers = sizeTiersFor(64, 2);

  it('names the tier the current dims land on', () => {
    expect(sizeKeyFor(tiers, 1920, 1088)).toBe('1080p');
  });

  it('falls back to custom for dims off every tier', () => {
    expect(sizeKeyFor(tiers, 1000, 1000)).toBe('custom');
  });
});

describe('snapTargetFor', () => {
  it('says exactly which side moved and why', () => {
    const out = snapTargetFor(1920, 1088, 128, 2);
    expect(out).toMatchObject({ width: 1920, height: 1152 });
    expect(out.note).toContain('height 1088→1152');
    expect(out.note).toContain('÷128');
    expect(out.note).toContain('2 stages');
  });

  it('says nothing when the target was already on the grid', () => {
    expect(snapTargetFor(1920, 1088, 64, 1)).toEqual({
      width: 1920,
      height: 1088,
      note: null
    });
  });

  it('singularises a one-stage ladder', () => {
    expect(snapTargetFor(1900, 1080, 64, 1).note).toContain('the 1 stage)');
  });
});

describe('sigma schedules', () => {
  it('parses a comma list and drops junk', () => {
    expect(parseSigmas('1.0, 0.5, , 0.0')).toEqual([1, 0.5, 0]);
  });

  /** N sigmas is N-1 denoise intervals; the Advanced "Steps" field does not reshape this. */
  it('counts steps as intervals, not values', () => {
    expect(baseStepsFromSigmas('1.0, 0.5, 0.0')).toBe(2);
    expect(baseStepsFromSigmas('')).toBe(0);
    expect(baseStepsFromSigmas('1.0')).toBe(0);
  });
});
