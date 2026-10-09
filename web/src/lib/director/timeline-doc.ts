/**
 * Reading a saved project — or a finished render's `request_meta` — back into editor values.
 *
 * This is where the version migrations live, and they are the reason it is worth isolating: v1 docs
 * stored width/height as the engine BASE pass while v2+ store the OUTPUT target, so a v1 project
 * read verbatim renders at half the resolution it was saved at, silently and only on old projects.
 * The same migration applies to legacy renders, which carry no `dims_are_target` flag at all.
 *
 * Nothing here touches editor state: it returns plain values for the caller to assign, in its own
 * order, because the snap that follows depends on several of them at once.
 */

import type {
  DirectorCharacter,
  DirectorClip,
  DirectorEngine,
  DirectorH3Generation,
  DirectorH3Reference,
  DirectorTimeline,
  LtxFrameDrop,
  LtxQualityProfile,
  LtxRuntimeLora,
  LtxUpscalerRatio,
  LyricWord,
  StoryContext
} from '$lib/api/client';
import { ltxRequestOf } from '$lib/api/client';
import { clampLoraStrength } from './loras';
import { readEngine } from './engine';

import { normaliseCharacters, pruneClipCharacters } from './characters';
import type { ChatMsg } from './chat';
import type { AudioKind } from './compile';
import {
  DETAIL_STRENGTH_DEFAULT,
  NAME_TO_RATIO,
  RATIO_MULT,
  RES_PRESETS,
  snap32,
  type CustomStage,
  type ResPreset
} from './resolution';

export const emptyStory = (): StoryContext => ({
  backstory: '',
  tone: '',
  rules: '',
  references: []
});

export interface AdoptedTimeline {
  /**
   * Which engine the project renders on. Absent in every document saved before H3 existed, and
   * `"ltx"` — retired LTX-2.3 — in one saved before 2026-08-14. Both read as `'ltx25'`: it is the
   * only LTX left, so it is the only reading under which those projects still open.
   */
  engine: DirectorEngine;
  /**
   * H3 omni references, project-level and IN ORDER. Kept SEPARATE from `characters` rather than
   * derived from it: an LTX character sheet is a rotary-tagged latent identity and an H3 reference
   * is text-encoder conditioning the prompt can address by ordinal, so translating one into the
   * other would look like it worked and would not.
   */
  h3References: DirectorH3Reference[];
  /**
   * H3 generations — the AUTHORED sections of each render unit.
   *
   * 🔴 Read as CONTENT, never as a packing. The packing is re-derived from the clips on every
   * render of the editor (`h3-generations.ts`), and these entries are re-attached to whichever
   * derived generation kept most of their shots. A saved packing would go stale the moment a shot
   * was trimmed, and a stale generation boundary is invisible until it renders as a seam.
   */
  h3Generations: DirectorH3Generation[];
  fps: number;
  shotSeconds: number;
  upscalerRatio: LtxUpscalerRatio;
  resPreset: ResPreset;
  width: number;
  height: number;
  proxyW: number;
  proxyH: number;
  proxySteps: number;
  proxyResumeJobId: string | null;
  finalResumeJobId: string | null;
  /**
   * H3 only: ask the engine for in-flight preview frames.
   *
   * 🔴 Defaults FALSE, and the default is the point. `generate_preview` decodes latents through the
   * RGB-factor path on every emission and that cost is UNMEASURED against a render whose steps are
   * already 43–50 s — so it is opt-in per project and must never silently tax a production render.
   * Absent in every doc saved before the toggle existed, which reads as off, which is what they had.
   */
  h3Preview: boolean;
  /**
   * H3 only: the LoRAs every generation renders with unless it names its own.
   *
   * 🔴 Bare filenames, not `LtxRuntimeLora` — H3's wire identifier for an adapter IS its filename
   * in the engine's `minimax_h3/` directory and there is no path, id or hash. The two engines'
   * LoRA fields therefore cannot share a type, and {@link AdoptedTimeline.loras} above is the LTX
   * one.
   *
   * 🔴 Persisted in the `editor` bag rather than in the compiled request, because there is no
   * top-level `loras` on `H3GenerateRequest` to round-trip through — the wire carries them PER
   * GENERATION (`contract.py: Generation.loras`). The project value is a DEFAULT that `compile.ts`
   * writes into each generation that has not overridden it, exactly like LTX's project LoRAs
   * inherit into a clip with an empty list.
   */
  h3Loras: string[];
  /** Positional multipliers for {@link AdoptedTimeline.h3Loras}, padded with 1.0. */
  h3LoraMultipliers: number[];
  /**
   * H3 only: the step count every generation renders at unless it names its own.
   *
   * The field the turbo LoRA exists to move — `h3-turbo-4step-ema-pruned.safetensors` buys nothing
   * on its own, because the adapter does not lower `steps` and costs ~5%/step to carry. 8 with the
   * LoRA against 20 without is −39% wall for the same VRAM, so the two knobs are set together and
   * belong at the same level.
   */
  h3Steps: number | undefined;
  model: string | undefined;
  loras: LtxRuntimeLora[];
  steps: number | undefined;
  cfgScale: number | undefined;
  negativePrompt: string;
  seed: number | undefined;
  detailStrength: number;
  qualityProfile: LtxQualityProfile;
  /** Absent when the doc carries no Custom recipe — the editor keeps whatever it had. */
  customBaseSampler?: string;
  customBaseSigmasText?: string;
  customStages?: CustomStage[];
  audioKind: AudioKind;
  /**
   * H3 only: what happens to the model's OWN generated audio once a song is supplied.
   *
   * Persisted because it changes what is DELIVERED, not merely how the editor looks — a project
   * reopened at the default would silently ship a different mix than the one its author chose.
   */
  h3SoundtrackMode: 'replace' | 'keep' | 'mix';
  /**
   * H3 only: **can the characters hear the supplied song?**
   *
   * 🔴 This exists because the prompt writer could not otherwise know, and its default answer was
   * always wrong for a music video. `GUIDE-ref:296` sends a reused soundtrack to
   * `non_diegetic_music`, which is *defined* as music the characters cannot hear — so every drafted
   * block declared the song audience-only, which contradicts asking the same character to sing it.
   *
   * Persisted rather than derived: a project can legitimately supply an audience-only score, and
   * nothing else in the timeline distinguishes that from a performance.
   */
  h3SoundtrackDiegetic: boolean;
  clips: DirectorClip[];
  characters: DirectorCharacter[];
  lyrics: LyricWord[];
  beatsEnabled: boolean;
  frameDrop: LtxFrameDrop;
  critique: { enabled: boolean; autoRetake: boolean; maxRetakes: number; threshold: number };
  pxPerSec: number;
  story: StoryContext;
  /** Absent on an auto-direct plan, which echoes no brief — the user's typed one must survive. */
  autoBrief?: string;
  summary: string | null;
  summaryStale: boolean;
  chatMessages: ChatMsg[];
}

export function readTimelineDoc(t: DirectorTimeline): AdoptedTimeline {
  const fps = t.fps ?? 24;
  // The LTX half of the compiled request, or `undefined` when this project compiled for H3. Every
  // read below goes through it: `request` is a union now, and an H3 document simply does not have
  // an upscaler ratio or a hires chain to recover.
  const req = ltxRequestOf(t.request);
  let upscalerRatio: LtxUpscalerRatio = req?.upscaler_ratio ?? 'x2';
  const resPreset = (t.editor?.resPreset as ResPreset) ?? '1080p';
  let width: number;
  let height: number;
  if (resPreset === 'custom') {
    // v1 docs stored width/height as the BASE pass; v2+ store the OUTPUT target. Migrate a v1
    // custom-resolution doc by scaling its base up by the saved upscaler ratio — the output the old
    // single-upscale pipeline actually produced. (Named presets self-migrate via RES_PRESETS.)
    const rawW = t.width ?? RES_PRESETS['1080p'].w;
    const rawH = t.height ?? RES_PRESETS['1080p'].h;
    const mig = (t.version ?? 1) < 2 ? RATIO_MULT[upscalerRatio] : 1;
    width = snap32(rawW * mig);
    height = snap32(rawH * mig);
  } else {
    const cfg = RES_PRESETS[resPreset];
    width = cfg.w;
    height = cfg.h;
    upscalerRatio = cfg.ratio;
  }

  const clips = (t.clips ?? []).map((c) => ({ ...c, keyframes: c.keyframes ?? [] }));
  // Cast first, then prune: a doc whose roster lost a character must not leave shots pointing at
  // it. `normaliseCharacters` also repairs a duplicated `source_id`, the one corruption that would
  // quietly re-label a face.
  const characters = normaliseCharacters(t.characters);
  for (const c of clips) pruneClipCharacters(c, characters);

  const savedStory = t.editor?.story as StoryContext | undefined;

  return {
    engine: readEngine(t.engine),
    // Order is PART OF THE REQUEST (it assigns each reference's `<Picture i>` ordinal), so this is
    // adopted verbatim — never sorted, grouped by kind, or de-duplicated.
    h3References: (t.h3_references ?? []).map((r) => ({ ...r })),
    // Absent on every project saved before generations existed — and on every H3 project whose
    // sections nobody has written yet. Both read as "derive a default packing and leave the
    // sections empty", which is what the packer does with an empty `stored` list.
    //
    // ⚠️ No migration runs here. A generation still carrying the retired `prompt_override` keeps
    // none of it — see the note in `h3-document.ts`. It has no `prompt` key, so it seeds a fresh
    // block from its stored sections on first open, like any other.
    h3Generations: (t.h3_generations ?? []).map((g) => ({ ...g })),
    fps,
    shotSeconds: (t.editor?.shotSeconds as number) ?? Math.round((t.segment_frames ?? 97) / fps),
    upscalerRatio,
    resPreset,
    width,
    height,
    proxyW: (t.editor?.proxyW as number) ?? 512,
    proxyH: (t.editor?.proxyH as number) ?? 288,
    proxySteps: (t.editor?.proxySteps as number) ?? 8,
    proxyResumeJobId: (t.editor?.proxyResumeJobId as string) ?? null,
    finalResumeJobId: (t.editor?.finalResumeJobId as string) ?? null,
    // `=== true` rather than a cast: an absent key, a null and a stray string all have to read as
    // OFF here, because on this flag "unknown" costs GPU time on every step of an 830 s generation.
    h3Preview: t.editor?.h3Preview === true,
    // Filtered to strings rather than cast: a hand-edited doc must not be able to put a number or a
    // null into a `loras[]` the shim validates by NAME — koblem refuses the whole request and the
    // author has no way to see which entry did it.
    h3Loras: Array.isArray(t.editor?.h3Loras)
      ? (t.editor.h3Loras as unknown[]).filter((x): x is string => typeof x === 'string' && !!x)
      : [],
    h3LoraMultipliers: Array.isArray(t.editor?.h3LoraMultipliers)
      ? (t.editor.h3LoraMultipliers as unknown[]).filter(
          (x): x is number => typeof x === 'number' && Number.isFinite(x)
        )
      : [],
    // `> 0` rather than merely present: 0 steps is not a render, and an absent value means "leave
    // it to the engine's own default", which is a different and legitimate state.
    h3Steps:
      typeof t.editor?.h3Steps === 'number' && t.editor.h3Steps > 0
        ? Math.trunc(t.editor.h3Steps)
        : undefined,
    model: t.model,
    // Project LoRAs live in the compiled request (the same place steps/cfg/seed round-trip
    // through), so save → load → compile is a closed loop with exactly one copy of the value.
    // Strengths are re-clamped on the way in: a doc hand-edited or written by an older, looser
    // caller must not smuggle a multiplier past the range the picker enforces.
    loras: (req?.loras ?? []).map((l) => ({
      path: l.path,
      multiplier: clampLoraStrength(l.multiplier)
    })),
    steps: req?.steps,
    cfgScale: req?.cfg_scale,
    negativePrompt: req?.negative_prompt ?? '',
    seed: req?.seed,
    detailStrength: req?.upscale_detail_strength ?? DETAIL_STRENGTH_DEFAULT,
    qualityProfile: req?.profile ?? 'standard',
    ...(req?.sampling_method ? { customBaseSampler: req.sampling_method } : {}),
    ...(req?.base_custom_sigmas?.length
      ? { customBaseSigmasText: req.base_custom_sigmas.join(', ') }
      : {}),
    ...(req?.hires_chain
      ? {
          customStages: req.hires_chain.map((s): CustomStage => ({
            upscaler: s.upscaler ? (NAME_TO_RATIO[s.upscaler] ?? 'x2') : 'x2',
            sigmasText: (s.custom_sigmas ?? []).join(', '),
            sample_method: s.sample_method ?? 'euler_cfg_pp',
            cfg: s.cfg ?? 1,
            steps: s.steps ?? 3
          }))
        }
      : {}),
    audioKind: t.audio_kind ?? 'none',
    // `replace` for any document written before the field existed, matching the live default: those
    // projects predate H3 taking a soundtrack at all, so there is no other answer they could mean.
    h3SoundtrackMode: t.h3_soundtrack_mode ?? 'replace',
    // Default TRUE, and deliberately not false. Every project this field exists for is one where a
    // song was supplied to a character who performs to it; a document written before the toggle
    // existed was drafted under the guide's audience-only default, which is the defect. Defaulting
    // false would preserve that defect on exactly the projects that have it.
    h3SoundtrackDiegetic: t.h3_soundtrack_diegetic ?? true,
    clips,
    characters,
    lyrics: t.lyrics ?? [],
    // Absent on every project saved before beats existed — default ON so they behave as designed.
    beatsEnabled: t.beats_enabled ?? true,
    // Absent on every project saved before the trim existed. Default AUTO rather than off: the
    // engine gates itself, so a doc that predates the field only changes if it is a t2v shot with
    // references on a checkpoint that leaks them — which is the bug this fixes.
    frameDrop: t.frame_drop ?? 'auto',
    critique: {
      enabled: t.critique?.enabled ?? false,
      autoRetake: t.critique?.auto_retake ?? false,
      maxRetakes: t.critique?.max_retakes ?? 2,
      threshold: t.critique?.threshold ?? 0.6
    },
    pxPerSec: (t.editor?.pxPerSec as number) ?? 80,
    story: savedStory
      ? {
          backstory: savedStory.backstory ?? '',
          tone: savedStory.tone ?? '',
          rules: savedStory.rules ?? '',
          references: savedStory.references ?? []
        }
      : emptyStory(),
    ...(typeof t.editor?.autoBrief === 'string' ? { autoBrief: t.editor.autoBrief } : {}),
    summary: typeof t.editor?.summary === 'string' ? t.editor.summary : null,
    summaryStale: t.editor?.summaryStale === true,
    chatMessages: Array.isArray(t.editor?.chatMessages) ? (t.editor.chatMessages as ChatMsg[]) : []
  };
}
