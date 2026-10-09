/**
 * Whether a shot starts from scratch, and whether anything continues out of its final frame.
 *
 * Mirrors `ShotIn::starts_fresh` / `closes_scene` in koblem/api `ltx_director.rs`. Both the compiler
 * and the prompt writer ask these, and they have to agree: the compiler decides whether the engine
 * carries motion across the cut, the writer decides whether to describe a standing scene or only
 * what changes in it.
 */

import type { DirectorClip, DirectorClipMode, LtxV2vSource } from '$lib/api/client';

export const isV2v = (m: DirectorClipMode) => m === 'v2v_edit' || m === 'v2v_restyle';

/**
 * The strength grid the sampler can actually land on: eight steps, so eighths.
 *
 * 🔴 Not a slider-feel choice. The engine resolves a strength to `round(steps * strength)` entered
 * steps of a fixed 8-step distilled schedule, so exactly nine outcomes exist (0–8) and every value
 * between two eighths renders identically to one of them. The old 0.05 step drew twenty-one
 * positions over those nine, so two thirds of the slider's travel changed the readout and not the
 * picture.
 *
 * ⚠️ The floor is one eighth rather than zero because `round` is BANKER'S rounding: `round(8 *
 * 0.0625) == 0`, i.e. no steps at all, and the plugin's `v2v_strength_floor` searches for the
 * lowest legal value rather than dividing to it.
 */
export const V2V_STRENGTH_STEP = 0.125;
export const V2V_STRENGTH_MIN = V2V_STRENGTH_STEP;
export const V2V_STRENGTH_MAX = 1;

/**
 * What an untouched v2v shot renders at.
 *
 * Shared by the compiler and the shot inspector on purpose: they used to disagree (0.45 shown,
 * 0.50 rendered), so the readout named a strength no render ever used. The engine's own default is
 * 1.0 — a full re-render — so this number is the product's, not a mirror of anything server-side.
 *
 * The scale is SDEdit: the value is the fraction of the schedule redone, so higher = further from
 * the source.
 *
 * 🔴 **Retuned 2026-08-14, and the old numbers were not merely conservative — they were both past
 * the top of the usable range.** LTX-2.5's distilled schedule is FRONT-LOADED: its first four steps
 * sit between sigma 1.0 and 0.975, so a strength of 0.5 enters at sigma 0.975 and is very nearly a
 * fresh generation. The 0.45 / 0.60 pair was tuned against LTX-2.3's schedule and on 2.5 means
 * "throw the source away" twice over. Useful editing lives below about 0.3; both defaults now sit
 * there or just above it, on the {@link V2V_STRENGTH_STEP} grid.
 */
export const v2vStrengthDefault = (m: DirectorClipMode) => (m === 'v2v_edit' ? 0.25 : 0.375);

/**
 * Is this shot's source a prior render THIS ENGINE CAN READ — or has it been stranded by one that
 * cannot?
 *
 * `priorRenderSource` is `EngineCapabilities.v2vPriorRenderSource`. A prior-render source is a
 * BANKED LATENT, and LTX-2.5 answers 501 to one: it becomes the sampler's `latent_image` and
 * `Ltx25LatentBankLoad` deliberately does not restore `downscale_ratio_spacial`.
 *
 * 🔴 A stranded clip is NOT quietly re-read as an upload. It can carry a `v2v_source_b64` as well —
 * the two fields are independent, and switching kinds in the inspector has never cleared the other
 * one — so falling back would render whatever clip the author attached before they moved off it,
 * with a 200 on the job and nothing on screen saying which footage was used. Stranded means "no
 * source", which is a state both the inspector and the pre-render warning already know how to say.
 */
export const v2vPriorStranded = (c: DirectorClip, priorRenderSource: boolean): boolean =>
  c.v2v_source_kind === 'prior_render' && !priorRenderSource;

/**
 * What this shot's picture is built FROM, or `null` when there is nothing usable to build it from.
 *
 * 🔴 ONE answer, read by the compiler, the shot inspector and the pre-render warning — a shot the
 * editor draws as sourced and the request compiles as bare is a shot that renders something
 * unrelated to the clip the author attached, with a 200 on it.
 */
export function v2vSourceOf(c: DirectorClip, priorRenderSource: boolean): LtxV2vSource | null {
  if (!isV2v(c.mode)) return null;
  if (v2vPriorStranded(c, priorRenderSource)) return null;
  if (c.v2v_source_kind === 'prior_render') {
    return c.v2v_prior_job_id
      ? { kind: 'prior_render', job_id: c.v2v_prior_job_id, segment: c.v2v_prior_segment ?? 0 }
      : null;
  }
  return c.v2v_source_b64 ? { kind: 'upload' } : null;
}

/** A shot that renders from scratch rather than continuing the previous shot's final frame. */
export const startsFresh = (c: DirectorClip) =>
  c.new_scene === true || c.mode === 'i2v' || isV2v(c.mode);

/**
 * Does anything continue out of this shot's final frame?
 *
 * A shot reliably CLOSES only when the NEXT shot starts fresh (a hard cut ignores this frame). "No
 * next shot yet" is NOT an ending while you are still appending — default to continuing so the shot
 * keeps the whole scene in its final frame, the only thing a later shot can see.
 */
export function closesScene(clips: DirectorClip[], idx: number): boolean {
  const next = clips[idx + 1];
  return next ? startsFresh(next) : false;
}
