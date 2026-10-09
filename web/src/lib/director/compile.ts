/**
 * The timeline → `LtxVideoGenerateRequest` compiler.
 *
 * Every rule in here is an "only when it differs" rule, and the reason is always the same: a project
 * that never touched a control must compile to the byte-identical request it always did, or an
 * untouched render stops reproducing. That contract is invisible in the markup and impossible to
 * eyeball in a 7,000-line component, so it lives here with tests.
 *
 * Two omissions are deliberate and load-bearing:
 *  - `undefined` is INHERIT and is never conflated with `"off"` (per-shot `frame_drop`) or with an
 *    empty override (per-shot `loras`), because both compile to the same wire bytes as absent;
 *  - the `ref_t2v:` prompt prefix is added by the BACKEND, so the prompt this emits stays exactly
 *    the prompt the editor shows and stores.
 */

import type {
  DirectorCharacter,
  DirectorClip,
  DirectorEngine,
  DirectorH3Generation,
  DirectorH3Reference,
  DirectorRequest,
  H3GenerateRequest,
  H3RequestGeneration,
  H3RequestReference,
  LtxAudioSource,
  LtxFrameDrop,
  LtxHiresStage,
  LtxQualityProfile,
  LtxRuntimeLora,
  LtxSegment,
  LtxUpscalerRatio,
  LtxVideoGenerateRequest
} from '$lib/api/client';
import { ltxRequestOf } from '$lib/api/client';
import { carriedCharacterIds, characterRefsForClip, resolveClipVariants } from './characters';
import { v2vSourceOf, v2vStrengthDefault } from './continuity';
import { capabilities } from './engine';
import {
  DETAIL_STRENGTH_DEFAULT,
  UPSCALER_NAME,
  parseSigmas,
  snap32,
  type CustomStage
} from './resolution';
import { framesFromSeconds } from '$lib/ltx/frames';
// H3's own frame arithmetic lives in `h3-generations.ts` because it applies to a GENERATION, not
// to a shot — `framesForSeconds` from `./engine` is the LTX-shaped per-shot answer and using it
// here is what made shots look like renders.
import { packGenerations, pinsAFrame } from './h3-generations';
import { composeH3Prompt } from './h3-prompt';
import { h3SeedInput } from './h3-document';

export type AudioKind = 'none' | 'voice' | 'bgm' | 'mixed';

export function audioSource(kind: AudioKind): LtxAudioSource {
  switch (kind) {
    case 'voice':
      return { kind: 'voice_upload' };
    case 'bgm':
      return { kind: 'bgm_upload' };
    case 'mixed':
      return { kind: 'mixed_upload' };
    default:
      return { kind: 'none' };
  }
}

/** Everything the compiled request reads. One bag, so a new knob cannot be forgotten at a caller. */
export interface CompileInput {
  /**
   * Which engine this project renders on. Everything below this line that H3 has no concept of is
   * simply not emitted for it — see `compileH3Timeline`, and `lib/director/engine.ts` for why each
   * field is or is not on that list.
   */
  engine: DirectorEngine;
  clips: DirectorClip[];
  characters: DirectorCharacter[];
  /**
   * H3 omni references, project-level and IN ORDER — position assigns each one's `<Picture i>`
   * ordinal, so this array is copied through verbatim. Empty (and ignored entirely) on LTX.
   */
  h3References: DirectorH3Reference[];
  /**
   * H3 generations — the AUTHORED sections of each render unit. The packing itself is re-derived
   * here from the clips, so this array only supplies content; see `h3-generations.ts`. Empty (and
   * ignored entirely) on LTX.
   */
  h3Generations: DirectorH3Generation[];
  /** H3 only: ask the engine for in-flight preview frames. Off by default — the cost is unmeasured. */
  h3Preview?: boolean;
  /**
   * H3 only: the project's LoRAs (bare engine filenames) and their positional multipliers, and the
   * project's step count.
   *
   * 🔴 DEFAULTS, not overrides. Each is written into a generation only when that generation names
   * none of its own, mirroring how LTX's project LoRAs inherit into a clip with an empty list
   * (`effectiveLoras` in `./loras`). A generation that set its own keeps it — the project value must
   * never overwrite something an author typed on the card.
   *
   * 🔴 Separate from {@link CompileInput.loras} / {@link CompileInput.steps} because they are not
   * the same field in a different dress: an H3 LoRA is a bare filename with no path (the shim
   * resolves it inside `minimax_h3/`), and the H3 request has no top-level `steps` at all — the
   * wire carries one per generation.
   */
  h3Loras?: string[];
  h3LoraMultipliers?: number[];
  h3Steps?: number | undefined;
  model: string | undefined;
  loras: LtxRuntimeLora[];
  width: number;
  height: number;
  fps: number;
  shotSeconds: number;
  upscalerRatio: LtxUpscalerRatio;
  qualityProfile: LtxQualityProfile;
  audioKind: AudioKind;
  /**
   * The project song, `asset:<hash>` or base64.
   *
   * On LTX it becomes `audio_source`; on H3 it becomes the per-generation `<Audio N>` slices AND the
   * delivery mux. Both engines take it — see the audio block in `./engine`.
   */
  audioB64?: string | null;
  /**
   * What happens to H3's OWN generated audio once a song is supplied. Defaults to `replace`.
   *
   * There is always something to displace — H3 denoises a synchronised track in the same pass as
   * the picture — so this is a real choice rather than a formality. `keep` still CONDITIONS the
   * render (performers sing to the song) and ships the model's audio; `mix` lays the song over a
   * ducked version of it.
   */
  h3SoundtrackMode?: 'replace' | 'keep' | 'mix';
  steps: number | undefined;
  cfgScale: number | undefined;
  negativePrompt: string;
  seed: number | undefined;
  frameDrop: LtxFrameDrop;
  detailStrength: number;
  customBaseSampler: string;
  customBaseSigmasText: string;
  customStages: CustomStage[];
  /** The ENGINE dir id to continue from, or null for a full fresh prefix. LTX only — H3 keeps no
   *  bank, so there is nothing to continue from. */
  resumeJobId: string | null;
}

export interface CompiledTimeline {
  /** Shape follows the engine — see `isH3Request` / `ltxRequestOf` for narrowing it. */
  request: DirectorRequest;
  /** Per RAW clip: the asset ref to ship as `seg_image_<i>`, or null. */
  segImageRefs: (string | null)[];
  /** Per RAW clip: the asset ref to ship as `seg_v2v_<i>`, or null. */
  segV2vRefs: (string | null)[];
}

function compileSegment(
  c: DirectorClip,
  ci: number,
  input: CompileInput,
  routedModel: string | undefined,
  /**
   * Character ids this shot's CARRY already supplies — see `carriedCharacterIds`. Decides, with
   * `clip.reference_mode`, whether the sheet is re-sent here. Passing an empty set means "nothing
   * carries in", i.e. every enabled character sends a reference, which is what every shot did
   * before this existed.
   */
  carried: Set<string>
): LtxSegment {
  const seg: LtxSegment = { prompt: c.prompt.trim(), seconds: c.seconds };
  // Only when it DIFFERS from the project model, so an untouched project stays byte-identical.
  if (routedModel && routedModel !== (input.model || undefined)) seg.model = routedModel;
  // AUTHORED sets only. Prop routing yields advice, never an adapter set, so nothing here can opt a
  // shot into an adapter silently. Empty and absent are the same "inherit" to koblem and the engine.
  if (c.loras?.length) seg.loras = c.loras;
  if (c.frame_drop !== undefined) seg.frame_drop = c.frame_drop;
  const v2vSource = v2vSourceOf(c, capabilities(input.engine).v2vPriorRenderSource);
  if (v2vSource) {
    // A v2v shot owns its content, so it carries no keyframes / scene image. A v2v clip with no
    // source falls through to t2v/i2v below rather than emitting a source-less v2v.
    seg.mode = c.mode;
    seg.v2v_strength = c.v2v_strength ?? v2vStrengthDefault(c.mode);
    seg.v2v_source = v2vSource;
    // 🔴 A v2v shot CANNOT ALSO BE A CONTINUATION, and this branch is the only place that can say
    // so — `startsFresh` has always agreed, but the chain below is an `else if` and never reaches
    // the line that emits the marker. The source is VAE-encoded over the init latent, which is
    // exactly what the carry writes into; the engine refuses the pair outright rather than
    // obliterating one of them the way the retired C++ engine did. And unlike an i2v or keyframed
    // shot — whose frame-0 guide clears the continuation inside the engine — a v2v source is not a
    // guide, so nothing downstream clears it and an unmarked v2v shot past the first 400s every
    // time. Shot 0 is fresh by definition, so the marker there would be noise on the wire.
    if (ci >= 1) seg.scene_cut = true;
  } else if (c.keyframes.length) {
    const pins = c.keyframes.map((k) => ({
      image_b64: k.image_b64,
      seconds: Math.min(k.seconds, c.seconds)
    }));
    // An i2v scene image + pins means "start on this image, then interpolate to the pinned
    // frame(s)". Inject the scene image as the frame-0 pin (unless one already sits at frame 0) or
    // keyframes win and the scene image is silently dropped, leaving a model-invented opening.
    if (c.mode === 'i2v' && c.image_b64 && !pins.some((p) => p.seconds <= 0.001)) {
      pins.unshift({ image_b64: c.image_b64, seconds: 0 });
    }
    seg.keyframes = pins;
    if (c.pin_strength !== undefined && Math.abs(c.pin_strength - 1) > 1e-6) {
      seg.pin_strength = c.pin_strength;
    }
  } else if (c.mode === 'i2v' && c.image_b64) {
    seg.image = { kind: 'upload' };
    if (c.pin_strength !== undefined && Math.abs(c.pin_strength - 1) > 1e-6) {
      seg.pin_strength = c.pin_strength;
    }
  } else if (ci >= 1 && c.new_scene === true) {
    // Shot 0 is always fresh, so never set there; omitted otherwise so the request stays
    // byte-identical for ordinary continuation shots.
    seg.scene_cut = true;
  }
  // This shot's own audio, whatever its visual mode. The engine composites it over the project
  // track for exactly this shot's span, so no offset is computed here.
  if (c.audio_b64) {
    seg.audio_b64 = c.audio_b64;
    seg.audio_kind = c.audio_kind ?? 'voice';
  }
  // Prompt Relay beats. `label` is editor-only and deliberately not sent. Clamped into the clip so
  // a beat left behind by a shortened shot cannot land past its end.
  const beats = (c.beats ?? []).filter((b) => b.text.trim().length > 0);
  if (beats.length) {
    seg.beats = beats.map((b) => ({
      seconds: Math.max(0, Math.min(b.seconds, c.seconds)),
      text: b.text.trim()
    }));
  }
  if (c.seed !== undefined && c.seed >= 0) seg.seed = c.seed;
  if (c.steps !== undefined && c.steps > 0) seg.steps = c.steps;
  if (c.cfg !== undefined && c.cfg >= 0) seg.cfg = c.cfg;
  if (c.negative_prompt?.trim()) seg.negative_prompt = c.negative_prompt.trim();
  seg.character_refs = characterRefsForClip(c, input.characters, carried);
  return seg;
}

/**
 * The compiled request as the ENGINE must receive it — which is not always the document the author
 * wrote, and is why this is a separate step rather than part of the compile.
 *
 * 🔴 An engine with no upscale chain renders ONE PASS AT THE TARGET, and has to say so.
 * `dims_are_target` makes koblem divide the request dims by the profile's total upscale factor to
 * size the base pass, so a 1080p project on `standard` + `x2` submits 960×544 and relies on the
 * refine to put it back. LTX-2.5 declines that refine for any shot carrying an opening frame,
 * keyframes or character references — nearly every Director shot — and the base pass becomes the
 * delivery: the user picks 1080p and is handed 544p, with the picker hidden (`caps.upscaleChain`)
 * so the value is not even theirs to change.
 *
 * 🔴 APPLIED ON DISPATCH, NEVER ON SAVE. It used to live inside `compileTimeline`, which also
 * builds the request embedded in the saved document — and `readTimelineDoc` reads the ladder back
 * out of that request, because it is where those two settings are STORED. So an LTX-2.3 project
 * put on 2.5 for one autosave came back reading `standard` / `none`, and switching to 2.5 and back
 * silently deleted a High ×2 ladder the author had chosen. What the engine may honour is a fact
 * about this render; what the author asked for is the document.
 */
export function compileForDelivery(
  engine: DirectorEngine,
  request: DirectorRequest
): DirectorRequest {
  const ltx = ltxRequestOf(request);
  if (!ltx || capabilities(engine).upscaleChain) return request;
  // 🔴 THREE fields, matching `apply_engine_delivery_limits` (`api/src/routes/ltx_director.rs`)
  // field for field — the two are one rule and the server path is the one the critic's unattended
  // auto-retakes go through. `hires_chain` is only reachable via Custom, whose ladder `standard`
  // does not read today; stating it here means the two sides cannot answer differently if it ever
  // does, rather than agreeing by way of a default.
  return { ...ltx, profile: 'standard', upscaler_ratio: 'none', hires_chain: undefined };
}

/**
 * Compile the timeline into the request the project's ENGINE renders.
 *
 * The LTX branch below is unchanged and must stay byte-identical for a project that never touched
 * a control — see the contract at the top of this file. H3 gets its own builder rather than a
 * filtered LTX request: the engine hard-400s on the LTX-only fields, so an omission that is merely
 * *usually* right would fail the render, and a subtractive filter is only correct until the next
 * LTX field is added.
 */
export function compileTimeline(input: CompileInput): CompiledTimeline {
  if (input.engine === 'h3') return compileH3Timeline(input);
  const { clips, model, loras, qualityProfile, upscalerRatio } = input;
  // Per-shot routing: an explicit `clip.model` pin is resolved here and nowhere else.
  const clipVariants = resolveClipVariants(clips, input.characters, model, loras);
  // 🔴 Computed over the WHOLE clip list, once, because whether a shot needs a reference depends on
  // the shot BEFORE it — which a per-segment compile cannot see.
  const carried = carriedCharacterIds(clips, input.characters);
  const segments = clips.map((c, ci) =>
    compileSegment(c, ci, input, clipVariants.get(c.id)?.model, carried[ci] ?? new Set<string>())
  );

  const request: LtxVideoGenerateRequest = {
    segments,
    model: model || undefined,
    // width/height are the OUTPUT target; the backend sizes the profile's base pass down from here
    // (dims_are_target). snap32 keeps them on-grid even for a hand-typed custom value.
    width: snap32(input.width),
    height: snap32(input.height),
    dims_are_target: true,
    fps: input.fps,
    segment_frames: framesFromSeconds(input.shotSeconds, input.fps),
    // Custom fully specifies its upscale via `hires_chain` (empty stage list = base-only). Force the
    // single-hires ratio off so an empty chain cannot fall back to a phantom ×2 in the backend's
    // single-hires path; a non-empty chain takes precedence and ignores this anyway. The upscaler
    // dropdown only renders for Standard, so upscalerRatio is stale under Custom.
    upscaler_ratio: qualityProfile === 'custom' ? 'none' : upscalerRatio,
    audio_source: audioSource(input.audioKind),
    save_clip: true,
    steps: input.steps,
    cfg_scale: input.cfgScale,
    negative_prompt: input.negativePrompt.trim() || undefined,
    seed: input.seed,
    profile: qualityProfile,
    // Sent always (including "auto", the default) so the compiled request states the contract rather
    // than relying on koblem's own default — the same reason `profile` is always sent.
    frame_drop: input.frameDrop
  };
  // Omitted entirely when there are none, so a project that never opened the picker compiles to the
  // byte-identical request it always did. This is also what every shot without its own override
  // inherits — the engine does the inheriting, so no resolved copy is written onto the segments.
  if (loras.length) request.loras = loras;
  // Only the baked High/4× recipes honour it (Custom edits sigmas directly; Standard has no upscale
  // refine). Sent only when moved off the baked default so an untouched render is byte-identical.
  if (
    (qualityProfile === 'high' || qualityProfile === 'upscale_4x') &&
    Math.abs(input.detailStrength - DETAIL_STRENGTH_DEFAULT) > 1e-6
  ) {
    request.upscale_detail_strength = input.detailStrength;
  }
  if (qualityProfile === 'custom') {
    request.sampling_method = input.customBaseSampler || undefined;
    const baseSigmas = parseSigmas(input.customBaseSigmasText);
    request.base_custom_sigmas = baseSigmas.length ? baseSigmas : undefined;
    request.hires_chain = input.customStages.map((s): LtxHiresStage => ({
      upscaler: UPSCALER_NAME[s.upscaler] ?? s.upscaler,
      custom_sigmas: parseSigmas(s.sigmasText),
      sample_method: s.sample_method || undefined,
      cfg: s.cfg,
      steps: s.steps
    }));
  }
  // Continue: reuse the prior render's banked shots, render only the newly-added tail shots.
  // The ENGINE dir id, NOT koblem's job id — the engine resolves its own dir by it.
  if (input.resumeJobId) request.resume_job_id = input.resumeJobId;

  return {
    request,
    // Only i2v clips WITHOUT keyframes supply one (a shot with keyframes carries the scene image
    // inline as its frame-0 pin instead).
    segImageRefs: clips.map((c) =>
      c.mode === 'i2v' && c.image_b64 && c.keyframes.length === 0 ? c.image_b64 : null
    ),
    // Only uploaded-source v2v clips supply one (a prior_render source resolves server-side).
    // Through the same resolver the segment was built from, so bytes cannot be shipped for a shot
    // whose source the request never claimed — or withheld from one it did.
    segV2vRefs: clips.map((c) =>
      v2vSourceOf(c, capabilities(input.engine).v2vPriorRenderSource)?.kind === 'upload'
        ? (c.v2v_source_b64 ?? null)
        : null
    )
  };
}

/**
 * One reference on the wire: the bytes, and the author's role verbatim.
 *
 * Two things this deliberately does NOT do:
 *
 * 1. 🔴 **It does not narrow the role.** koblem maps the eleven authoring roles down to the
 *    engine's own vocabulary, because it is the side that owns both the prompt and the engine call.
 *    Narrowing here made `audio_reuse` unreachable from the editor entirely and collapsed the
 *    distinctions the `[task type + task type]` prefix is derived from.
 * 2. 🔴 **It does not send `frames`.** A video reference is a CONTAINER — `_add_video_reference`
 *    opens the file and does its own 2 fps sampling and block timestamping — and the shim's request
 *    model is `extra="forbid"`, so a `frames` array is a 400, not a slow path. Legacy documents
 *    carrying one are left carrying it; the compiler simply never emits it, and koblem refuses a
 *    frames-only reference with a message that names the change.
 *
 * `with_audio` and `tail_frames` are emitted for `kind: 'video'` and for nothing else. Both are
 * refused by name on another kind, and the image case is the one that would be silent if it were
 * not: honouring `with_audio` on a picture allocates an `<Audio n>` for an asset that has none and
 * shifts every later audio ordinal by one, so the prompt's `<Audio 2>` would land on `<Audio 1>`'s
 * asset with the render still returning 200.
 */
function wireReference(r: DirectorH3Reference): H3RequestReference {
  const video = r.kind === 'video';
  return {
    kind: r.kind,
    ...(r.role ? { role: r.role } : {}),
    ...(r.image ? { image: r.image } : {}),
    ...(r.video ? { video: r.video } : {}),
    ...(r.audio ? { audio: r.audio } : {}),
    // `true` only: `with_audio: false` is the default the shim already applies, and stating it
    // would change the bytes of every reference that never opted in.
    ...(video && r.with_audio ? { with_audio: true } : {}),
    ...(video && r.tail_frames !== undefined ? { tail_frames: r.tail_frames } : {})
  };
}

/**
 * The MiniMax-H3 request.
 *
 * Built additively from what H3 HAS, never by removing what it lacks. The fields it lacks are not
 * fields the engine 400s on: they do not exist, and anything unrecognised comes back in `ignored[]`
 * having changed nothing — which is worse, because the default renders and nobody finds out. So
 * the safe construction is the one that cannot name them, and {@link H3GenerateRequest} is a real
 * type precisely so a stray one is a compile error rather than a silent no-op.
 *
 * Four things are worth reading twice:
 *
 * 1. 🔴 **The unit is a GENERATION, not a shot.** Consecutive shots are packed into ≤15.083 s
 *    generations by `h3-generations.ts`, and the cuts between them are `[Shot N] At MM:SS.mmm`
 *    lines inside ONE prompt that the model renders itself. Rendering shots separately is the
 *    degradation you accept past the budget — chaining to obtain a cut is strictly worse, because
 *    it loses `<scenetrans>`, loses the prose identity carry, and adds a seam.
 * 2. **The prompt is the STORED ARTIFACT, not something composed here.** `DirectorH3Generation.prompt`
 *    is the block; it is sent verbatim. `composeH3Prompt` runs only as the fallback for a
 *    generation that has never been opened — see the `prompt` doc below for why an empty one takes
 *    that fallback rather than shipping.
 * 3. **`references` is copied through in ORDER.** Position assigns each one's `<Picture i>` /
 *    `<Video k>` / `<Audio j>` ordinal AND advances a shared rotary clock, so filtering, sorting
 *    or grouping this array silently renumbers what the prompt refers to. Scoping NARROWS the
 *    list — it never reorders it — and per-generation is the granularity the conditioner has.
 * 4. **A generation has EITHER frame pins or references, never both** — `fl2va` and `ref2va` are
 *    different weight sets. The packer already isolates a pinned shot into its own generation, so
 *    this function cannot construct the illegal combination.
 */
function compileH3Timeline(input: CompileInput): CompiledTimeline {
  const { clips, h3References, h3Generations } = input;
  // The soundtrack reference is already IN `h3References` — the editor keeps it there whenever a
  // song is attached, so it is persisted, scopeable and numbered like any other. Augmenting the
  // list here instead would have given the compiler an ordinal the composer never saw, and a
  // `<Audio N>` the prompt cannot address conditions far more weakly and silently.
  const hasSong = !!input.audioB64 && input.audioKind !== 'none';
  const packed = packGenerations({
    clips,
    references: h3References,
    stored: h3Generations,
    // Tightens the generation ceiling from 362 to 345 frames so no generation is longer than the
    // 15.0s audio reference that has to score it. See `H3_MAX_FRAMES_WITH_AUDIO`.
    hasSoundtrack: hasSong
  });

  const generations: H3RequestGeneration[] = packed.map((gen) => {
    const doc = gen.doc;
    // 🔴 **The stored block, sent VERBATIM.** `doc.prompt` is the artifact — the panels in
    // `H3PromptComposer` are a lens over these exact bytes, and a field edit rewrote one section of
    // them rather than recomposing the whole thing. So there is nothing left to assemble here.
    //
    // ⚠️ The test is the CONTENT, not the presence of the key, and that rule is load-bearing rather
    // than defensive: **koblem drops an empty-prompt generation from the chain**, so shipping `""`
    // would make this generation silently VANISH from the render instead of failing. A stored `""`
    // is also far more likely to be a box somebody cleared than a request to render unconditioned.
    // Falling back to the composed block is the recoverable answer to both.
    //
    // The fallback is also what a generation nobody has opened compiles to: the composer seeds
    // `prompt` on first open, so an untouched project renders the same bytes it always did.
    //
    // 🔴 A stored block does NOT follow a re-pack. Trim a shot and its `[Shot N] At MM:SS.mmm`
    // times are stale, and they are what renders — the composer's own retime is the repair, and
    // re-deriving the block here would throw away everything the author wrote.
    //
    // The fallback is `h3SeedInput`, the SAME input the composer seeds a fresh block with, so a
    // generation nobody has opened compiles to exactly what opening it would have stored. Building
    // it here separately is what let the editor's preview and the compiled request drift on the
    // FL2VA alignment line.
    const prompt = doc.prompt?.trim() || composeH3Prompt(h3SeedInput(gen));

    // 🔴 **Project defaults, applied here and nowhere else.** The wire has no project level — every
    // LoRA and every step count is a per-generation field (`contract.py: Generation`) — so "the
    // whole project renders at 8 steps with the turbo LoRA" has to become that value on each
    // generation at compile time. Doing it here rather than by writing the value into every stored
    // generation keeps ONE copy of the setting: change the project number and every generation that
    // never overrode it follows, instead of drifting apart the moment one was edited.
    //
    // 🔴 The override test is the GENERATION'S OWN list being non-empty, matching `effectiveLoras`
    // on the LTX side. An empty list therefore means "inherit" and not "no LoRAs" — see the note in
    // `./loras` about why explicitly clearing the set is not currently representable on either
    // engine, and why that is a wire limitation rather than a UI choice.
    const genLoras = doc.loras?.length ? doc.loras : (input.h3Loras ?? []);
    // The multipliers follow whichever list won, so a generation that overrides the LoRAs can never
    // inherit the project's multipliers and silently apply them positionally to different adapters.
    const genLoraMults = doc.loras?.length
      ? (doc.lora_multipliers ?? [])
      : (input.h3LoraMultipliers ?? []).slice(0, genLoras.length);
    const genSteps =
      doc.steps !== undefined && doc.steps > 0
        ? doc.steps
        : input.h3Steps !== undefined && input.h3Steps > 0
          ? input.h3Steps
          : undefined;

    const pinned = gen.shots.filter((s) => pinsAFrame(s.clip));
    const firstFrame = pinned[0]?.clip;
    const lastFrame = pinned[pinned.length - 1]?.clip;
    return {
      task: gen.task,
      frames: gen.frames,
      prompt,
      // ADVISORY. The authority on where the cuts are is the `[Shot N]` text inside `prompt` — this
      // is what makes a packing bug visible in the plan echo instead of merely looking like a bad
      // render.
      shots: gen.shots.map((s) => ({ index: s.shotIndex, at_seconds: s.cutAtSeconds })),
      ...(gen.references.length ? { references: gen.references.map(wireReference) } : {}),
      ...(firstFrame?.mode === 'i2v' && firstFrame.image_b64
        ? { first_frame: firstFrame.image_b64 }
        : {}),
      ...(lastFrame?.h3_end_image ? { last_frame: lastFrame.h3_end_image } : {}),
      // Absent on the first generation: the wire omits the field rather than sending
      // `mode: "none"`, which would claim a decision was made about a join that does not exist.
      //
      // The RESOLVED continuation is what is spread, not the stored one — `resolveContinuation`
      // fills the three numbers every consumer needs, and drops the WanGP leftovers an older
      // document may still hold rather than spreading them onto the wire.
      // 🔴 `from` DEFAULTS to `'prev'`, it is no longer forced to it. The spread used to end with
      // `from: 'prev'`, which silently overwrote an explicit latent-bank key on its way out — so a
      // field koblem resolves carefully (`h3_single_generation_chain`: an explicit key is
      // "forwarded verbatim … not ours to second-guess") could never carry one, and "continue from
      // take 2" was unreachable by construction rather than unimplemented.
      //
      // `'prev'` stays the default because it is what a linear cut means and what the shim resolves
      // at run time — it survives a retake that re-renders an earlier generation, which a baked key
      // would not.
      ...(gen.continuation
        ? { continuation: { ...gen.continuation, from: gen.continuation.from?.trim() || 'prev' } }
        : {}),
      // 🔴 **No per-generation canvas, deliberately.** The field exists on the document and the
      // editor used to offer it, which meant one shot could render 1280×720 and the next 1920×1080
      // — a cut that cannot be concatenated — and, because the noise is drawn at the latent SHAPE,
      // it silently changed what a PINNED SEED produced. The request-level `width`/`height` govern
      // every generation. Any value left on an old document is dead data and is not read.
      ...(doc.seed !== undefined && doc.seed >= 0 ? { seed: doc.seed } : {}),
      ...(genSteps !== undefined ? { steps: genSteps } : {}),
      // 🔴 The turbo LoRA is the point: `h3-turbo-4step-ema-pruned.safetensors` at 8 steps is
      // 465.4 s against the 20-step control's 762.8 s, for the same 6528 MiB. It does NOT set
      // `steps` — that is the field above, and the two have to move together or the win is not
      // collected. Emitted only when non-empty, so a project that never opened the picker compiles
      // to the byte-identical request it always did.
      ...(genLoras.length ? { loras: genLoras } : {}),
      // Positional against `loras` and padded with 1.0, so a SHORTER list is normal. Emitted
      // whenever it is non-empty rather than only alongside a non-empty `loras`: a multiplier list
      // with nothing to multiply is an authoring mistake, and koblem refuses it by name — dropping
      // it here would turn that into a render that quietly ignored what the author set.
      ...(genLoraMults.length ? { lora_multipliers: genLoraMults } : {})
    };
  });

  const request: H3GenerateRequest = {
    generations,
    // 🔴 SENT AS AUTHORED. This used to be `snap32(input.width)`, which quietly rewrote the
    // canvas: a user who typed 1280×720 (illegal on H3 — 720/32 = 22.5) got 1280×704 with nothing
    // said anywhere, which is the same defect as the frame floor silently lengthening a shot.
    //
    // The 32-grid is model law and koblem REFUSES an off-grid canvas by name, saying which side
    // is wrong and what the legal neighbours are; `checkCanvas` in `./engine` puts the same
    // message in the settings sheet while the number is still being typed. Correcting it here
    // would hide both.
    width: Math.round(input.width),
    height: Math.round(input.height),
    // Stated rather than omitted: it is the ONE legal value, so a request that somehow carried
    // another rate fails at submit rather than rendering against a timeline whose arithmetic
    // assumed 24.
    fps: 24,
    ...(input.h3Preview ? { preview: true } : {}),
    // The DELIVERY half. The slices above are what the engine hears; this is what koblem remuxes
    // over the finished cut, because `fully_copy` is a prompt marker and the engine's audio is
    // therefore a reconstruction of the song rather than the song.
    ...(hasSong
      ? {
          soundtrack: { audio: input.audioB64 as string, mode: input.h3SoundtrackMode ?? 'replace' }
        }
      : {})
  };

  // Canvas rules are enforced by koblem, which has the generation index; the settings sheet shows
  // the same verdict live via `checkCanvas`. Neither of them snaps: the ×32 grid is REPORTED and
  // the area budget is a VRAM warning, not a refusal.
  return {
    request,
    // H3 carries its keyframes INLINE on the generation (there is no `assets.seg_images_b64`
    // sidecar to keep aligned), and has no v2v path at all.
    segImageRefs: clips.map(() => null),
    segV2vRefs: clips.map(() => null)
  };
}
