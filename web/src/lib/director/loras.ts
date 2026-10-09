/**
 * Runtime-LoRA encoding shared by every Director surface.
 *
 * A runtime LoRA is the TUNABLE counterpart to a folded `model` variant: the variant bakes its
 * adapter in at a fixed strength and costs nothing extra at render time, while these are chosen
 * per render (and per shot) at whatever strength you like. The engine switches them at the same
 * chain-segment boundary as a per-shot `model`, but re-applying adapters is ~1s against ~36s for a
 * whole folded-DiT swap — so varying an EFFECT'S STRENGTH across a timeline is what per-shot LoRAs
 * are actually for, and why the multiplier matters as much as the picker.
 *
 * This lives in one module because several surfaces (`SettingsSheet` at project level,
 * `ShotDetailsPane` and the mobile sheet per shot) all author the same field, and the per-shot
 * inherit/replace encoding below is exactly the kind of thing two copies get to disagree about.
 *
 * ## Inherit vs replace — TWO states, not three
 *
 * The engine's contract, mirrored by `LtxSegment.loras` in koblibs, is:
 *
 * | `clip.loras`      | on the wire         | engine behaviour                       |
 * |-------------------|---------------------|----------------------------------------|
 * | `undefined`       | key omitted         | inherit the request's top-level set    |
 * | `[]`              | key omitted*        | inherit the request's top-level set    |
 * | `[a, b]`          | `lora: [a, b]`      | REPLACES the top-level set, this shot  |
 *
 * \* koblem's emitter is gated on non-empty (`ltx_video.rs`, `if !seg_specs[i].loras.is_empty()`)
 * and the koblibs field is a plain `Vec` with `skip_serializing_if = "Vec::is_empty"`, so an empty
 * array is indistinguishable from an absent one all the way down.
 *
 * That means "this shot deliberately runs with NO LoRAs while the project has some" is **not
 * expressible**. A shot can drop an inherited adapter only by naming a different non-empty set.
 * So — unlike `character_ids` and `frame_drop`, which really do have three states —
 * [`normaliseLoraOverride`] collapses an emptied override back to `undefined`: one representation
 * of "inherit", and no UI state that lies about what the render will do. Do not "fix" this by
 * persisting `[]` and hoping; making the third state real means `Option<Vec<LtxRuntimeLora>>` in
 * koblibs plus emitting an explicit `"lora": []`, and an engine that honours it.
 */

import type { DirectorClip, LtxLoraInfo, LtxRuntimeLora } from '$lib/api/client';

/**
 * The neutral scale — what an adapter gets when NOBODY has said otherwise.
 *
 * Prefer {@link loraDefaultStrength}, which asks the server's `LtxVideoProps.loras` first. This is
 * the fallback for an adapter the engine's `loras.json` and koblibs' catalogue both say nothing
 * about, and for the callers that genuinely have no props in scope.
 */
export const LORA_STRENGTH_DEFAULT = 1.0;
/**
 * Multiplier bounds for the inputs. Negative multipliers invert an adapter (occasionally useful,
 * hence the floor below zero) and past ~2 they generally cook the sample — the clamp is here so a
 * typo in a number field can't send a nonsense strength to a render that then costs minutes.
 */
export const LORA_STRENGTH_MIN = -2;
export const LORA_STRENGTH_MAX = 2;
export const LORA_STRENGTH_STEP = 0.05;

/** Bring any authored/loaded value into range, mapping a non-finite one to the native scale. */
export function clampLoraStrength(v: number): number {
  if (!Number.isFinite(v)) return LORA_STRENGTH_DEFAULT;
  return Math.min(LORA_STRENGTH_MAX, Math.max(LORA_STRENGTH_MIN, v));
}

/** Is this adapter in the list? */
export function hasLora(list: LtxRuntimeLora[] | undefined, path: string): boolean {
  return (list ?? []).some((l) => l.path === path);
}

/** This adapter's strength in the list, or the native scale when it isn't in it. */
export function loraStrength(list: LtxRuntimeLora[] | undefined, path: string): number {
  return (list ?? []).find((l) => l.path === path)?.multiplier ?? LORA_STRENGTH_DEFAULT;
}

/**
 * The strength to ADD an adapter at, from the server's annotated offer list.
 *
 * 🔴 NOT ALWAYS 1.0, and the difference is invisible. These adapters carry no `.alpha` tensors, so
 * the engine multiplies their delta raw: `omninft` was trained at alpha/r = 2 and renders HALF at
 * 1.0, while the close-up face adapter wants ~0.2 and swamps the sheet at 1.0. Neither shows up as
 * an error — the render succeeds and the adapter merely under- or over-fires — which is exactly why
 * the number comes from the server (the engine's `loras.json`, then koblibs' catalogue) instead of
 * being a constant here.
 */
export function loraDefaultStrength(offered: LtxLoraInfo[] | undefined, path: string): number {
  const hit = (offered ?? []).find((l) => l.path === path);
  return clampLoraStrength(hit?.default_multiplier ?? LORA_STRENGTH_DEFAULT);
}

/**
 * Add/remove one adapter, returning a NEW list (the callers are `$state` holders, so nothing is
 * mutated in place). Order is append-on-enable, which is also the order the engine applies them.
 *
 * Pass `offered` (the props list) so switching an adapter on lands on ITS strength rather than a
 * blanket 1.0 — see {@link loraDefaultStrength}. Omitting it falls back to the neutral scale, which
 * is what every caller did before the offer list carried strengths.
 */
export function toggleLora(
  list: LtxRuntimeLora[] | undefined,
  path: string,
  offered: LtxLoraInfo[] | undefined = undefined
): LtxRuntimeLora[] {
  const cur = list ?? [];
  return hasLora(cur, path)
    ? cur.filter((l) => l.path !== path)
    : [...cur, { path, multiplier: loraDefaultStrength(offered, path) }];
}

/**
 * The trigger phrases that will be appended to this shot's prompt, for the editor to say out loud.
 *
 * DISPLAY ONLY — the backend does the appending (`ltx_apply_lora_triggers`, per shot, against the
 * shot's effective set) and it is the only place that can, because it is the only place that sees
 * the final prompt after identity clauses. Writing the phrase into the authored prompt here would
 * double it the moment the adapter is switched off.
 */
export function loraTriggers(offered: LtxLoraInfo[] | undefined, list: LtxRuntimeLora[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const l of list) {
    const t = (offered ?? []).find((o) => o.path === l.path)?.trigger;
    if (!t || seen.has(t.toLowerCase())) continue;
    seen.add(t.toLowerCase());
    out.push(t);
  }
  return out;
}

/** Set one adapter's strength (no-op when it isn't enabled), returning a NEW list. */
export function setLoraStrength(
  list: LtxRuntimeLora[] | undefined,
  path: string,
  multiplier: number
): LtxRuntimeLora[] {
  const v = clampLoraStrength(multiplier);
  return (list ?? []).map((l) => (l.path === path ? { ...l, multiplier: v } : l));
}

/**
 * The one representation of "inherit". An override the user emptied is NOT "deliberately none" —
 * see the module note: the wire cannot say that — so it goes back to `undefined` rather than
 * persisting a `[]` that would render identically while the UI implied otherwise.
 */
export function normaliseLoraOverride(
  list: LtxRuntimeLora[] | undefined
): LtxRuntimeLora[] | undefined {
  return list && list.length ? list : undefined;
}

/**
 * What this shot will actually render with: its own override when it has one, else the project's
 * set. Display-only — the compilers emit `clip.loras` as-is so the engine keeps doing the
 * inheriting, and a resolved copy on every segment would be a second source of truth.
 */
export function effectiveClipLoras(
  clip: Pick<DirectorClip, 'loras'> | null | undefined,
  projectLoras: LtxRuntimeLora[]
): LtxRuntimeLora[] {
  return clip?.loras?.length ? clip.loras : projectLoras;
}

/**
 * One-line "Audio-reactive ×1.50, Detailer ×0.60" for a summary row, or `""` for none.
 *
 * Pass `offered` to use the server's labels. Without it — or for an adapter the offer list does not
 * describe — this falls back to the file name with its extension trimmed, which is what this
 * function always did.
 *
 * The label is a SUMMARY-ROW convenience only. The pickers themselves still show the real file
 * name next to the label, because an operator debugging a render-time 400 needs the string the
 * engine rejected, not a prettified version of it.
 *
 * `short` trims the file extension, for width. Ignored when a label is available.
 */
export function loraSummary(
  list: LtxRuntimeLora[],
  short = true,
  offered: LtxLoraInfo[] | undefined = undefined
): string {
  return list
    .map((l) => {
      const label = (offered ?? []).find((o) => o.path === l.path)?.label;
      const name = label ?? (short ? shortLoraName(l.path) : l.path);
      return `${name} ×${l.multiplier.toFixed(2)}`;
    })
    .join(', ');
}

/** Filename with its extension dropped. Display sugar only — never sent, never stored. */
export function shortLoraName(path: string): string {
  return path.replace(/\.(gguf|safetensors|pt|bin)$/i, '');
}
