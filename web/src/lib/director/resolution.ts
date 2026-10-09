/**
 * The Director's resolution ladder: what the OUTPUT target is, what base pass the selected quality
 * profile renders it from, and which sizes the editor is allowed to offer.
 *
 * Kept out of the component because getting it wrong is silent and expensive — an off-grid target
 * makes the engine round the base pass, so the render comes back at a resolution nobody asked for
 * and the OOM guard is computed against a number that isn't what hits VRAM. Mirrors
 * `resolve_quality_plan` in koblem/api `ltx_video.rs`.
 */

import type { LtxQualityProfile, LtxUpscalerRatio } from '$lib/api/client';

/**
 * A named output preset. Survives only to migrate OLD saved projects whose editor blob stored one;
 * the live UI drives width/height through the profile-aware size tiers and always saves `custom`.
 */
export type ResPreset = '720p' | '1080p' | '1080p-hi' | 'custom';

export const RES_PRESETS: Record<
  Exclude<ResPreset, 'custom'>,
  { w: number; h: number; ratio: LtxUpscalerRatio }
> = {
  '720p': { w: 1280, h: 704, ratio: 'none' },
  '1080p': { w: 1920, h: 1088, ratio: 'x2' },
  '1080p-hi': { w: 1920, h: 1056, ratio: 'x1.5' }
};

export const RATIO_MULT: Record<LtxUpscalerRatio, number> = {
  none: 1,
  'x1.5': 1.5,
  x2: 2
};

/** Ratio key ↔ engine spatial-upscaler name (mirrors `LtxUpscalerRatio::upscaler_name`). */
export const UPSCALER_NAME: Record<string, string> = {
  x2: 'ltx-2.3-spatial-upscaler-x2-1.1',
  'x1.5': 'ltx-2.3-spatial-upscaler-x1.5-1.0'
};
export const NAME_TO_RATIO: Record<string, string> = {
  'ltx-2.3-spatial-upscaler-x2-1.1': 'x2',
  'ltx-2.3-spatial-upscaler-x1.5-1.0': 'x1.5'
};

/**
 * One editable upscale stage under the Custom profile. `upscaler` holds a ratio key mapped to the
 * engine upscaler name on compile; `sigmasText` is a comma list parsed to `number[]`.
 */
export interface CustomStage {
  upscaler: string;
  sigmasText: string;
  sample_method: string;
  cfg: number;
  steps: number;
}

export const newStage = (): CustomStage => ({
  upscaler: 'x2',
  sigmasText: '0.85, 0.725, 0.421875, 0.0',
  sample_method: 'euler_cfg_pp',
  cfg: 1,
  steps: 3
});

export const DEFAULT_BASE_SIGMAS =
  '1.0, 0.99375, 0.9875, 0.98125, 0.975, 0.909375, 0.725, 0.421875, 0.0';
export const DEFAULT_BASE_SAMPLER = 'euler_a_cfg_pp';

/**
 * The final refine stage's renoise sigma0 for the baked High / 4× recipes. Only sent when moved off
 * this value, so an untouched render stays byte-identical.
 */
export const DETAIL_STRENGTH_DEFAULT = 0.85;

/**
 * LTX spatial patch requirement: each side a multiple of 32, min 32 (mirrors `round32` in
 * koblem/api `ltx_video.rs`). Every rung of every ladder lands here in the end, so it is also the
 * whole constraint for a render with no upscale — the proxy, whose dims ARE its base pass.
 */
export const LTX_PATCH = 32;

export const snap32 = (x: number) =>
  Math.max(LTX_PATCH, Math.round((Number(x) || 0) / LTX_PATCH) * LTX_PATCH);

/** Snap to an arbitrary divisor, floored at that divisor. */
const snapTo = (x: number, div: number) => Math.max(div, Math.round((Number(x) || 0) / div) * div);

export const parseSigmas = (t: string): number[] =>
  t
    .split(',')
    .map((s) => parseFloat(s.trim()))
    .filter((n) => !Number.isNaN(n));

/**
 * Base-pass step count = the number of sigma INTERVALS in the schedule (N sigmas → N-1 steps). This
 * is what actually governs base steps under Custom — the Advanced "Steps" field does not reshape the
 * schedule.
 */
export const baseStepsFromSigmas = (sigmasText: string) =>
  Math.max(0, parseSigmas(sigmasText).length - 1);

/**
 * The per-profile spatial-upscale factors applied on top of the base pass. Standard = the single
 * upscaler ratio; High = one ×2; 4× = two ×2; Custom = its stage list.
 *
 * Custom is driven ENTIRELY by its stage list — an empty list is base-only, and the compiled request
 * forces `upscaler_ratio: "none"` to match, so it must NOT fall back to the stale single-hires ratio
 * or the readout and the snap divisor would claim a ×2 the render no longer does.
 */
export function profileFactors(
  profile: LtxQualityProfile,
  upscalerRatio: LtxUpscalerRatio,
  customStages: readonly CustomStage[]
): number[] {
  switch (profile) {
    case 'high':
      return [2];
    case 'upscale_4x':
      return [2, 2];
    case 'custom':
      return customStages.map((s) => (s.upscaler === 'x1.5' ? 1.5 : 2));
    default:
      return RATIO_MULT[upscalerRatio] > 1 ? [RATIO_MULT[upscalerRatio]] : [];
  }
}

export interface ResolutionPlan {
  targetW: number;
  targetH: number;
  baseW: number;
  baseH: number;
  factors: number[];
  mult: number;
  ladder: { w: number; h: number }[];
}

/**
 * The resolution ladder for a profile: base pass → each ×N upscale → target output. Base = target ÷
 * (product of factors); each rung ×32-snapped so it is engine-valid.
 */
export function planResolution(width: number, height: number, factors: number[]): ResolutionPlan {
  const targetW = snap32(width);
  const targetH = snap32(height);
  const mult = factors.reduce((m, f) => m * f, 1);
  const baseW = mult <= 1 ? targetW : snap32(targetW / mult);
  const baseH = mult <= 1 ? targetH : snap32(targetH / mult);
  const ladder: { w: number; h: number }[] = [{ w: baseW, h: baseH }];
  let w = baseW;
  let h = baseH;
  for (const f of factors) {
    w = snap32(w * f);
    h = snap32(h * f);
    ladder.push({ w, h });
  }
  return { targetW, targetH, baseW, baseH, factors, mult, ladder };
}

/**
 * The multiple the OUTPUT target must be a multiple of so the base (target ÷ factor) stays on the
 * ×32 grid: Standard-none 32, ×1.5 48, ×2/High 64, 4× 128.
 */
export const snapDivisorFor = (mult: number) => Math.max(LTX_PATCH, Math.round(LTX_PATCH * mult));

/**
 * Canonical 16:9 output tiers, before the profile's divisor is applied.
 *
 * 🔴 **LTX only.** These are ladder TARGETS — each is divided back down through the selected
 * quality profile's upscale factor to get the base pass, which is why they are raw 16:9 numbers
 * and not on any grid until `sizeTiersFor` snaps them. Two of the three (1280×720, 1920×1080) are
 * not legal H3 canvases at all: 720/32 = 22.5 and 1080/32 = 33.75.
 *
 * H3's sizes live on the engine capability itself (`canvasPresets` in `director/engine.ts`) —
 * MiniMax's documented aspects, all already on the ×32 grid, with no ladder to divide out.
 */
export const SIZE_TIERS: { key: string; w: number; h: number; label: string }[] = [
  { key: '720p', w: 1280, h: 720, label: '720p' },
  { key: '1080p', w: 1920, h: 1080, label: '1080p' },
  { key: '1440p', w: 2560, h: 1440, label: '1440p (large)' }
];

export interface SizeTier {
  key: string;
  label: string;
  w: number;
  h: number;
  bw: number;
  bh: number;
}

/**
 * The tiers as the dropdown offers them: each snapped to the CURRENT profile's divisor, so picking
 * one never triggers a surprise re-snap, and carrying the base dims the profile's factor implies.
 */
export function sizeTiersFor(divisor: number, mult: number): SizeTier[] {
  return SIZE_TIERS.map((t) => {
    const w = snapTo(t.w, divisor);
    const h = snapTo(t.h, divisor);
    return {
      ...t,
      w,
      h,
      bw: mult <= 1 ? w : snap32(w / mult),
      bh: mult <= 1 ? h : snap32(h / mult)
    };
  });
}

/** The tier the current output matches, or `custom`. */
export function sizeKeyFor(tiers: SizeTier[], width: number, height: number): string {
  return tiers.find((t) => t.w === snap32(width) && t.h === snap32(height))?.key ?? 'custom';
}

/** Proxy-tier preview sizes. Independent of the output tiers — small and always single-pass. */
export const PREVIEW_TIERS: { key: string; w: number; h: number; label: string }[] = [
  { key: 'tiny', w: 384, h: 224, label: 'Tiny' },
  { key: 'small', w: 512, h: 288, label: 'Small' },
  { key: 'medium', w: 640, h: 384, label: 'Medium' },
  { key: 'large', w: 768, h: 448, label: 'Large' }
];

/**
 * Snapping the output target for a profile, and the human-readable note explaining what moved.
 * `note` is null when nothing changed — the fix for the "why did 1088 become 1152?" confusion.
 */
export function snapTargetFor(
  width: number,
  height: number,
  divisor: number,
  stages: number
): { width: number; height: number; note: string | null } {
  const nw = snapTo(width, divisor);
  const nh = snapTo(height, divisor);
  const changed: string[] = [];
  if (nw !== width) changed.push(`width ${width}→${nw}`);
  if (nh !== height) changed.push(`height ${height}→${nh}`);
  return {
    width: nw,
    height: nh,
    note: changed.length
      ? `${changed.join(', ')} — this profile upscales ${stages}×, so each output side must be ÷${divisor} ` +
        `(base ÷32 after the ${stages} stage${stages === 1 ? '' : 's'}).`
      : null
  };
}
