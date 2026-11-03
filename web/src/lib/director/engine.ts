/**
 * One Director, two engines — and the single place that says what each one can do.
 *
 * Every `{#if}` that hides an LTX-only control on an H3 project reads a flag from here rather than
 * comparing `engine === 'ltx'` inline. The reason is not tidiness: the two engines differ in about
 * twenty places spread across five components, and a comparison written inline records only WHICH
 * engine, never WHY. When a third engine appears, or when H3 gains one of these, the change has to
 * be findable — a grep for `engine === 'ltx'` across 12,000 lines of Svelte is not.
 *
 * The flags are named after the CAPABILITY, not the engine, for the same reason.
 *
 * ## What actually differs
 *
 * H3 renders video AND its synchronised stereo soundtrack in one denoise over one packed sequence,
 * is guidance-distilled (exactly one forward per step), ships one DiT, and keeps NO latent state
 * between shots. Everything below follows from those four facts:
 *
 * - guidance-distilled ⇒ no negative prompt, no CFG
 * - one DiT ⇒ no model variants, no upscale/hires chain
 * - joint audio, generated ⇒ there is no audio INPUT at all; the LTX audio sheet is meaningless
 * - no latent state ⇒ no resume, no retake splice, no continuity/seam overlap, no v2v
 *
 * 🔴 **And one difference that reframes the rest: the RENDER UNIT is not the shot.**
 *
 * LTX's unit is a segment = one render. H3's unit is a `generation` = up to ~15 s containing 1–N
 * clips, where a cut between two of them is `[Shot N] At MM:SS.mmm` written INSIDE one prompt and
 * rendered by the model itself. Inside a generation continuity is free and `<scenetrans>` carries
 * audio across the cut; across a generation boundary you get a seam and only an approximate
 * continuation. So the generation — not the clip — is what renders, what is delivered, and what a
 * retake re-renders. `lib/director/h3-generations.ts` owns the packing arithmetic; the flags below
 * only say that the concept exists.
 *
 * ⚠️ **Which is why the UI calls a generation a SHOT.** The thing that renders, delivers and
 * retakes is what a director means by "shot", on either engine — so `vocab` (below) names it that
 * on both, and names the clips packed inside an H3 generation `cut`s. The TYPES are untouched:
 * `DirectorClip` is still a clip, `DirectorH3Generation` is still a generation, and the block still
 * writes `[Shot N]` because that is the model's own trained vocabulary. Read {@link EngineVocab}
 * before renaming anything on the strength of a label you saw on screen.
 *
 * The engine REFUSES the LTX-only fields with a hard 400 rather than dropping them silently — so a
 * UI that leaves the control visible is a UI that builds requests which fail.
 */

import type { DirectorEngine, H3Props } from '#lib/api/client.ts';

export type { DirectorEngine };

/**
 * Absent, `ltx` and anything unrecognised all read as `ltx25` — the only LTX that still exists.
 *
 * ⚠️ A document written by a newer koblem must open on an older one as SOMETHING. It renders on the
 * wrong engine, which is visible; the alternative — refusing to open the project — loses the work.
 *
 * 🔴 This maps a stored `"ltx"` to a LIVE engine, and that is safe ONLY for the timeline's own
 * engine field, where the value says which controls to draw. It is NOT safe for the ids in
 * `director_shot_renders`: a row stamped `'ltx'` names a job in the retired C++ engine's store, and
 * handing one to 2.5 is a 404 after the full queue wait. Those rows are compared against the
 * project's engine as raw strings (`renders.ts`), never through this function, so a project reading
 * as `ltx25` simply does not match them — which is the intended outcome and the reason this
 * function is not applied there.
 */
export function readEngine(value: unknown): DirectorEngine {
  return value === 'h3' ? 'h3' : 'ltx25';
}

/**
 * H3's fixed frame rate. Not a default — the model is only defined at it.
 *
 * Declared here rather than beside the other H3 constants because the capability record below
 * publishes it as {@link EngineCapabilities.fixedFps}, and a `const` cannot be read above its own
 * declaration at module init.
 */
export const H3_FPS = 24;

/**
 * The canvas a NEW H3 project starts on, and the one a project moved onto H3 is reframed to:
 * MiniMax's own 768p output size.
 *
 * 🔴 A new project used to start at LTX's 1920×1088. That is a legal ×32 pair, but it is **2.02×
 * the area every H3 timing and VRAM number was measured at** — so with the area rule relaxed from
 * a 400 to a warning it would become "allowed, and will OOM the 16 GB card". An H3 project gets an
 * H3 default.
 *
 * Declared up here for the same reason as {@link H3_FPS}: the capability record below publishes it
 * as {@link EngineCapabilities.homeCanvas}.
 */
export const H3_DEFAULT_CANVAS = { w: 1344, h: 768 };

/**
 * What this engine's units are CALLED on screen. Display only — nothing here is on the wire.
 *
 * 🔴 **The types keep their names and so does the block.** `DirectorClip`, `DirectorH3Generation`,
 * `PackedGeneration`, `reference_ids`, `h3_generations` and every field on them are unchanged, and
 * the composed prompt still writes `[Shot N]` because that is the literal vocabulary MiniMax
 * trained on. A find-replace over the ~1,900 `shot` hits in the Director would have broken the wire
 * types AND the model's own tags; what changes is what the UI *says*, and only that.
 *
 * The reason it lives on the capability record rather than in a `engine === 'h3' ? … : …` at each
 * label: the two engines do not disagree about the word for a RENDER — both call it a shot, which
 * is the whole point of the decision. They disagree about whether a render has parts inside it. One
 * component asking "does a render have sub-units here" (`subUnit !== null`) is asking a capability
 * question; the same component asking "am I on H3" is asking a question that goes stale the moment
 * a third engine appears.
 *
 * ⚠️ Every string here is lowercase. Sentence-leading and heading uses go through {@link capitalise}
 * so there is one spelling of each word rather than two that can drift apart.
 */
export interface EngineVocab {
  /**
   * What ONE RENDER is called. `'shot'` on both engines — that IS the point.
   *
   * On LTX a shot is a segment is a render. On H3 a `generation` is what renders, what is delivered
   * and what a retake re-renders, so it is the thing that deserves the word. Calling it a
   * "generation" on screen was the terminology collision that made "shot audio: per generation, not
   * per shot" read as a contradiction.
   */
  renderUnit: string;
  renderUnitPlural: string;
  /**
   * What the things INSIDE one render are called, or `null` where a render has no parts.
   *
   * `null` on LTX and that is load-bearing: a component that draws sub-unit markers must draw
   * nothing there rather than draw one marker per render.
   *
   * 🔴 `'beat'` on H3 — it was `'cut'` until 2026-08-08, and the rename is a correction rather than
   * a preference. A `[Shot N] At MM:SS.mmm` written without a transition verb produces **no cut**
   * (measured: `scdet` max 0.71 over 362 frames, where a hard cut scores >10) and still lands its
   * action roughly on the timestamp. So most of these are not cuts, and calling them cuts was
   * misleading in the expensive direction: an author who believes every `[Shot N]` is a cut writes
   * cut-shaped prose for moments they wanted continuous, and gets a chopped-up render.
   */
  subUnit: string | null;
  subUnitPlural: string | null;
  /**
   * What a sub-unit that ALSO changes the camera is called, or `null` where a render's parts cannot
   * differ that way.
   *
   * 🔴 **A FLAVOUR, not a second type.** On the wire both are `[Shot N] At MM:SS.mmm, …` inside one
   * generation, occupying identical space and identical time; the only difference is whether the
   * body opens with one of {@link H3_TRANSITION_VERBS}. The block already models it — `cutVerb` is
   * present or absent — so nothing new is stored and nothing downstream of the packer may branch on
   * it. **Every cut is a beat; not every beat is a cut.**
   *
   * ⚠️ `null` on LTX, where `subUnit` is already `null` — a render there has no parts to flavour.
   */
  subUnitCut: string | null;
  subUnitCutPlural: string | null;
}

/** `'cut'` → `'Cut'`. One spelling of each word, capitalised where a sentence or heading needs it. */
export const capitalise = (s: string): string => s.charAt(0).toUpperCase() + s.slice(1);

export interface EngineCapabilities {
  /** Human label for the picker and for "why is this hidden" copy. */
  label: string;
  /** One line the settings sheet shows under the picker. */
  blurb: string;
  /** What this engine's units are CALLED on screen. See {@link EngineVocab}. */
  vocab: EngineVocab;

  // ── Sampling ──────────────────────────────────────────────────────────────
  /** A negative prompt does anything. H3 is guidance-distilled: there is no unconditional branch. */
  negativePrompt: boolean;
  /** CFG is adjustable. H3's only legal value is 1.0, and it is genuinely honoured there. */
  cfg: boolean;
  /** The base sampler / sigma recipe is editable. */
  customRecipe: boolean;
  /**
   * A CLIP carries its own `seed` / `steps` / `cfg` / `negative_prompt`, overriding the project's.
   *
   * 🔴 About the STORAGE, not about whether a shot can be seeded — every engine seeds a shot.
   * `compileSegment` is the only reader of those four fields and it never runs on H3, where the
   * same numbers live on the `DirectorH3Generation` the shot packs into (`doc.seed`, `doc.steps`,
   * read in `compileH3Timeline`) and are edited in `H3PromptComposer` / `H3RenderKnobs`.
   * `packGenerations` does not copy them across. So a per-shot seed box drawn on H3 is a live
   * control writing a field that compiles to nothing.
   *
   * ⚠️ It is the OUTER gate only. {@link cfg}, {@link negativePrompt} and {@link customRecipe}
   * still decide which of the four the engine honours — all three are false on LTX-2.5, where the
   * distilled checkpoint accepts them and ignores them.
   */
  perShotSampling: boolean;

  // ── Model ─────────────────────────────────────────────────────────────────
  /** More than one base DiT to choose between. */
  modelVariants: boolean;
  /**
   * Runtime LoRAs can be attached, per project and per render unit.
   *
   * 🔴 **True on H3 since 2026-08-07, and it was `false` for a while after that was no longer so.**
   * The flag was written when H3's only DiT meant no adapter path existed; `Generation.loras` +
   * `lora_multipliers` then went live (`contract.py`, `/h3/v1/loras`) and the compiler started
   * emitting both, while this stayed `false` — so the settings sheet offered no project-level
   * picker and the only way to attach the turbo LoRA was to type it on every generation card. A
   * capability flag that disagrees with the wire hides a shipped feature, which is the same defect
   * `audioInput: false` was.
   */
  loras: boolean;
  /**
   * WHAT NAMES a LoRA on this engine, because the two answers are not the same field in a different
   * dress and no picker can serve both.
   *
   * - `path` — LTX. A path plus catalogue metadata the engine serves from `loras.json`: a label, a
   *   blurb, a trigger word and a trained default strength. The state is `LtxRuntimeLora[]`
   *   (`{path, multiplier}`), and the picker is a checklist because the set is known and described.
   * - `filename` — H3. A **bare filename** in the shim's `minimax_h3/` LoRA directory and nothing
   *   else: no id, no hash, no metadata, and a name containing a separator is refused outright
   *   because it could resolve outside the mounted root. The state is `string[]` plus a positional
   *   `number[]`, and the picker is a text box backed by a datalist — a `<select>` would make a file
   *   copied in a second ago unreachable, and the listing legitimately lags.
   * - `null` where {@link EngineCapabilities.loras} is false.
   *
   * 🔴 It exists because `caps.loras` alone is not enough to choose a control. A component that
   * asked only "does this engine take LoRAs" would render the LTX checklist — driven by
   * `LtxVideoProps.loras`, which is fetched on an H3 project too — and write `{path, multiplier}`
   * objects into a field the H3 shim validates as a list of strings.
   */
  loraIdentity: 'path' | 'filename' | null;
  /** A latent-upscale refine chain exists (the quality profile + upscaler ratio controls). */
  upscaleChain: boolean;

  // ── Timing ────────────────────────────────────────────────────────────────
  /** fps is a choice. H3 is DEFINED at 24 and refuses anything else. */
  variableFps: boolean;
  /**
   * The rate this engine renders at when it does not take one — `null` ⇔ {@link variableFps}.
   *
   * 🔴 The value, not just the fact. A stored `fps` carried over from another engine is NOT what a
   * fixed-rate engine renders at, and the packer computes on this constant regardless — so any
   * surface that turns frames into seconds, or seconds into a stepper grid, has to read this rather
   * than the document's own number, or it walks a grid the packer never snaps to.
   */
  fixedFps: number | null;
  /** Frames must land on this grid: `base + period * n`. LTX is `8k+1`, H3 is `17n+5`. */
  framePeriod: number;
  frameBase: number;
  /**
   * Frames per RENDER — the floor and the ceiling the engine itself enforces.
   *
   * 🔴 On H3 these bound a GENERATION, not a shot. A 2.5 s shot is perfectly legal as one cut
   * inside a longer generation. Clamping a shot against these numbers is the mistake to avoid —
   * see `generationFrames` in `h3-generations.ts`, which is the only place they are applied.
   *
   * 🔴 `minFrames` on H3 is 22, the video VAE's DECODABLE minimum, not the 107 it used to be.
   * See `H3_DOCUMENTED_MIN_FRAMES` in `h3-generations.ts` for what 107 actually is.
   *
   * `null` on LTX: its ceiling is the temporal window, which `lib/ltx/frames.ts` already owns and
   * expresses as a function of fps and whether the shot continues.
   */
  minFrames: number | null;
  maxFrames: number | null;
  /**
   * Width/height must both be a multiple of this. **Hard, and model law** — H3's VAE downsamples
   * 16x spatially and the DiT patchifies 2x on top of it, so an off-grid side has no whole-number
   * latent width. Refused, never snapped behind the user's back.
   *
   * 🔴 The two sizes everyone types are BOTH illegal: 720 / 32 = 22.5 and 1080 / 32 = 33.75. The
   * legal neighbours are 1280×704 (or 1280×736) and 1920×1088.
   */
  dimMultiple: number;
  /**
   * ADVISORY area budget on `width * height`, or `null` where there is none.
   *
   * 🔴 **Not a cap.** On H3 this was enforced as a 400 until 2026-08-07 and the cost was concrete:
   * true 16:9 at a 768 short edge is 1376×768 = 1,056,768 px, 2.4% over, so it could not be
   * rendered at all. `diffusers` calls that exact number a *"soft area cap"*, ComfyUI does not
   * enforce it, and no config in the MiniMax weights tree carries a max resolution.
   *
   * What is real is VRAM: 1344×768 is the shape every timing and memory number was measured at,
   * and a much larger canvas may not fit the 16 GB card. So over budget WARNS about fit — it does
   * not claim the size is illegal.
   */
  advisoryMaxPixels: number | null;
  /**
   * Canvas sizes worth offering in the picker, or `null` where the engine drives its own ladder.
   *
   * H3 has no upscale chain, so its sizes are just sizes: the aspects MiniMax document, all on the
   * ×32 grid. LTX's come from `resolution.ts`, where the target has to divide back out through the
   * profile's upscale factor.
   */
  canvasPresets: { w: number; h: number; label: string }[] | null;
  /**
   * The size a project ARRIVING on this engine is reframed to when the canvas it brings is one this
   * engine has no business rendering — or `null` where the engine has no single home size.
   *
   * 🔴 The reason {@link reframeForEngine} asks a capability rather than the engine's name: LTX's
   * canvas is a target that has to divide back out through its quality profile (`resolution.ts`),
   * so there is nothing to fall back TO there and a project keeps whatever it had. H3 has exactly
   * one measured shape, so a 1920×1088 LTX canvas carried across is 2.02× the area every timing
   * and VRAM number came from.
   */
  homeCanvas: { w: number; h: number } | null;

  // ── Between shots ─────────────────────────────────────────────────────────
  /**
   * Shots share latent state, so a chain has continuity, a seam, and a resumable bank — and a
   * retake has to splice. False for H3, where shots are wholly independent.
   */
  chainedShots: boolean;
  /**
   * Re-rendering ONE unit also re-pins it to the footage that ALREADY FOLLOWS it.
   *
   * 🔴 The difference decides how far a take selection reaches, which is why it is a flag and not a
   * comment. `true` on LTX: the engine reloads the banked prefix `[0, k)`, re-renders only shot `k`,
   * and splices the banked tail `(k+1, N)` back **end-pinned to k's new final frame** — the seam
   * after `k` is a design goal of the retake, so the shots downstream do not have to follow it.
   * `false` on H3: `retake_segment_h3` renders one generation and nothing else, and the generation
   * after it opened on a tail that has now changed — so the repair cascades forward until a hard
   * cut stops it.
   *
   * ⚠️ It does NOT make a stale join harmless on LTX. The end pin holds the final FRAME; the take
   * that plays is still not the take the next shot was rendered against, and a take from a second
   * full run was never end-pinned to anything. `take-continuity.ts` still reports the join — it just
   * does not demand the whole tail be re-rendered behind it.
   */
  retakeSplicesTail: boolean;
  /** A failed/partial render can be continued from its banked latents. */
  resume: boolean;
  /**
   * A "render everything" can be narrowed to the units whose takes have gone stale, INSTEAD of
   * re-rendering (and destroying) the tier.
   *
   * 🔴 A fact about the SERVER, not a preference: `GET …/render-plan` answers
   * `{"supported": false}` and `POST …/run?scope=stale` answers a 400 on both LTX engines, because
   * the plan describes the packing of clips into generations and neither of them packs. So a
   * surface that offers the choice — the scope dialog, or a sentence promising "re-renders shots 2
   * and 5, the rest keep their takes" — is describing something that will not happen there: the
   * button wipes the tier and redraws every shot.
   */
  scopedFullRender: boolean;
  /**
   * {@link resume} is a CEILING on this engine, not the answer — the deployment has the last word.
   *
   * 🔴 The distinction the whole gate turns on. `resume` says the MODEL and its plugin implement a
   * bank; this says the bank is a directory an operator configures, so the same build answers 501
   * to a resume and to a single-unit retake when it is unset. The live fact is
   * `LtxVideoProps.bank_enabled`, and {@link resumeAvailable} is the one place the two are put
   * together — a control drawn off `resume` alone dispatches a render that fails after the queue
   * wait, which is the worst place to learn it.
   *
   * `false` where the bank is part of the build (the C++ `ltx-video` engine) or absent entirely
   * (H3) — an engine that reports nothing must not have its own capability record overruled by the
   * silence.
   */
  resumeNeedsLiveBank: boolean;
  /**
   * A frame can be pinned at an ARBITRARY TIME inside a shot, rather than only at its edges.
   *
   * ⚠️ Not "H3 cannot go from one keyframe to another" — it can, and that is the FL2VA case the
   * guide devotes a section to: `<Picture 1>` at the 0.00-second mark, `<Picture 2>` at the
   * duration, and the body supplies *"the motion path that connects them"* (`GUIDE-base:198-201`).
   * That is {@link EngineCapabilities.endFrame}, and it is `true` here.
   *
   * What H3 has no expression for is a pin at 3.2 s of an 8 s shot: the alignment instruction can
   * only name 0.00 and the duration, and a `keyframe completion` picture declared any other way is
   * a SEMANTIC anchor with no time attached (measured at 0.776 frame-0 NCC against a wire pin's
   * 0.968). A third keyframe is therefore a new cut or nothing.
   */
  midShotKeyframes: boolean;
  /** `pin_strength` — how hard a pinned frame is held. H3 anchors an `fl2va` frame outright. */
  pinStrength: boolean;
  /** A shot may close the frame as well as open it (H3's `fl2va` last-frame anchor). */
  endFrame: boolean;
  /**
   * A CLIP can carry its own source video and be restyled or edited from it.
   *
   * 🔴 **Not "this engine can edit video".** H3 can: `video editing` is one of its six task types —
   * *"An existing source video is directly modified"* (`GUIDE-ref:136`) — and it combines, e.g.
   * `[video editing + audio reuse]`. What H3 does not have is a PER-CLIP source-video field. The
   * source arrives as a numbered `<Video N>` omni reference on the generation, with the role
   * `edit_source`, and the prompt says what changes about it.
   *
   * So this flag still gates the LTX clip modes correctly — a `v2v_source_b64` on a clip has
   * nowhere to go on H3 — but it must not be read as an absence of the capability. The message it
   * prints ({@link UNAVAILABLE.v2v}) used to say "MiniMax-H3 has no video-to-video path", which was
   * simply false and left a shipped feature undiscoverable.
   *
   * ⚠️ It says nothing about WHERE the source may come from. That is
   * {@link v2vPriorRenderSource}, and the split is the whole reason this one could go `true` on
   * LTX-2.5 — the half that is refused there is narrow, and turning the whole mode off to express
   * it took the half that works away with it.
   */
  v2v: boolean;
  /**
   * A v2v source may be A SHOT THIS PROJECT ALREADY RENDERED, named by job id, as well as a clip
   * the author uploads.
   *
   * 🔴 **Narrower than {@link v2v} on purpose, and the narrowness is the point.** LTX-2.5 renders
   * v2v from uploaded frames perfectly well and answers 501 to this alone: a banked-latent source
   * becomes the sampler's `latent_image`, and `Ltx25LatentBankLoad` deliberately does not restore
   * `downscale_ratio_spacial` because its outputs otherwise only feed `LTXVSelectLatents` /
   * `LTXVAdainLatent`. So this gates ONE button inside the source card — the picker's
   * "From a prior render" half — and nothing else.
   *
   * ⚠️ False on H3 too, and not for that reason: H3 has no per-clip source field at all
   * ({@link v2v} is false there), so there is no narrower question to ask. A capability that is
   * meaningless on an engine still has to answer `false` rather than be absent, or every reader
   * has to know which engines the field applies to.
   *
   * `v2vSourceOf` in `lib/director/continuity.ts` is the one place it is applied: a shot pointed at
   * a prior render on an engine that refuses one has NO source, so it compiles as plain t2v and
   * shows up in the same warning strip as a v2v shot nobody attached anything to.
   */
  v2vPriorRenderSource: boolean;
  /**
   * This engine has a dedicated `clip.beats` ARRAY — the LTX Prompt Relay conditioning layout.
   *
   * Prompt Relay is ONE text encode plus an additive Gaussian bias on TEXT CROSS-ATTENTION, each
   * clause penalised away from its own moment. It is emphatically NOT a mid-sample re-encode of
   * the prompt register — that claim was written down twice here and is what kept this `false` on
   * LTX-2.5, on the grounds that no ComfyUI node could do it. A block-level attention patch can,
   * and `custom_nodes/ltx25_prompt_relay` is one. See the LTX record's doc block for the three
   * behavioural differences that come with it.
   *
   * 🔴 **Read the name narrowly. `false` on H3 does NOT mean "H3 has no beats" — H3 has beats.**
   * It writes them as `[Shot N] At MM:SS.mmm` with no transition verb, inside the generation's
   * prompt, and they work (see {@link UNAVAILABLE} for the measurement). What H3 does not have is
   * *this storage*: a `beats[]` array hanging off a clip, conditioned by the engine.
   *
   * ⚠️ So do not "fix" this to `true` on the strength of the beat finding. `ShotPromptPane` renders
   * the Prompt Relay array editor behind this flag, and on H3 that array reaches `compileH3Timeline`
   * and is dropped on the floor — a live control writing a field that compiles to nothing, which is
   * the exact class of defect the upscaler row and the resume tick-box already were. H3's beats are
   * authored in the prompt, and the control for them belongs with the prompt.
   */
  beats: boolean;
  /**
   * `beats` is a ceiling here, and this deployment's Prompt Relay node has the last word.
   *
   * 🔴 The same split as {@link resumeNeedsLiveBank}, and for the same reason: the relay is served
   * by `custom_nodes/ltx25_prompt_relay`, which appears in ComfyUI's `/object_info` only after the
   * comfyui container restarts. Drawn off `beats` alone the editor offers a beats array that is
   * refused at graph build — after the GPU-gate wait. {@link beatsAvailable} is what a control
   * reads; this flag only says whose answer to require.
   */
  beatsNeedsLiveNode: boolean;
  /**
   * One render can hold several TIMED MOMENTS, expressed as `[Shot N]` lines inside its one prompt.
   *
   * ⚠️ It does NOT mean several clips pack into a call — a clip is a render on both engines. It
   * means the model renders the transitions between the beats inside one prompt itself, so a cut
   * within a render is free while a cut between two renders is a seam. False on LTX, where a
   * prompt describes one continuous moment.
   */
  multiShotPerGeneration: boolean;
  /**
   * The prompt is a fixed SCHEMA of named sections, not prose.
   *
   * The model was trained on the schema; bare prose is out-of-distribution and — the part that
   * costs picture quality — carries no `overall_soundscape` and no `non_diegetic_music`, so the
   * model invents the whole audio bed unguided. Gates the section-per-panel composer against the
   * plain prompt box.
   */
  structuredPrompt: boolean;
  /**
   * A retake re-renders the whole render unit, not one moment inside it.
   *
   * ⚠️ Now equivalent to "one clip is one render" — it distinguished a generation from the clips
   * packed into it, and there are no longer several. Kept because the retake and banking paths read
   * it in a dozen places and collapsing them is a separate change.
   */
  generationIsRetakeUnit: boolean;
  /** A render unit can be joined to the previous one — the only place a seam can appear. */
  crossGenerationContinuation: boolean;
  /** The engine can emit in-flight preview frames while it denoises. */
  preview: boolean;
  /**
   * The PREVIEW tier renders the full canvas at fewer steps, rather than a smaller canvas.
   *
   * 🔴 True on H3, and it is a correctness flag rather than a preference. The initial noise is
   * `randn((1, 24, latent_t, latent_h, latent_w), manual_seed(seed))`, so a smaller canvas is a
   * different latent SHAPE and therefore a different DRAW — not a downsample of the same one. At
   * the same seed a 512x288 pass gives fresh composition, camera path and subject placement, and
   * because the audio noise comes off the same generator immediately afterwards, a different music
   * bed as well. Nothing about the render it precedes survives except the prompt.
   *
   * Holding the canvas and cutting the steps keeps the noise byte-identical: same framing, same
   * placement, same bed, under-converged. ~2x cheaper rather than ~20x, and peak VRAM is unchanged
   * (~11 GB) — which is the binding constraint anyway, so the small canvas was not buying headroom.
   *
   * ⚠️ Consequence: on such an engine the proxy tier no longer means "fewer bytes". The preview
   * clip is the same size as the final one; what it saves is TIME.
   */
  previewIsFullRes: boolean;

  // ── Identity ──────────────────────────────────────────────────────────────
  /** LTX character sheets: rotary-tagged latent identity you inject but cannot name. */
  characterRefs: boolean;
  /**
   * H3 omni references: image/video/audio routed through the text encoder, ordinal-labelled, so
   * the prompt can address them.
   */
  omniReferences: boolean;
  /**
   * A shot may carry BOTH a frame pin and an identity reference.
   *
   * True on LTX, where `character_refs` and `keyframes` compose freely. FALSE on H3, and not as a
   * policy choice: `fl2va` and `ref2va` are served by DIFFERENT TRANSFORMERS (`transformer/` vs
   * `transformer_ref/`), a checkpoint boundary rather than a flag, so the engine refuses a shot
   * that carries both. A UI that lets the user set both is offering something that cannot render.
   */
  keyframesAndRefsCompose: boolean;

  // ── Audio ─────────────────────────────────────────────────────────────────
  //
  // 🔴 These were ONE boolean, `audioInput`, and collapsing them was a real defect rather than a
  // tidiness problem. It gated five controls at once, and only the first of the three jobs a
  // supplied track does is actually engine-specific:
  //
  //   1. CONDITIONING — the engine hears it. LTX slices a 16 kHz voice track to drive lip-sync;
  //      H3 takes it as a numbered `<Audio N>` omni reference. Different mechanisms, both real.
  //   2. AUTHORING SPINE — waveform, STT word onsets, shot cuts snapping to lyrics. The engine
  //      never sees any of this; it is timeline metadata.
  //   3. DELIVERY — the original file remuxed over the finished cut.
  //
  // `audioInput: false` on H3 asserted that all three were impossible because the first one is
  // done differently. It is not: `minimax_h3_handler.validate_generative_settings` accepts
  // `audio_prompt_type="A"`/`"AB"` with `audio_guide`/`audio_guide2`, and the whole reference path
  // (kind `audio`, role `audio_reuse`, marker `fully_copy`) was already built end to end. The flag
  // was the only thing claiming otherwise, and it hid a shipped feature.

  /**
   * A song can be attached to the PROJECT: waveform, STT lyric lane, and shot cuts that snap to
   * word onsets. True on both engines — none of it reaches the engine, so nothing about the model
   * can make it impossible.
   */
  projectSoundtrack: boolean;
  /**
   * The supplied track reaches the engine as CONDITIONING, so performers move and sing to it.
   *
   * True on both, by different routes — see {@link EngineCapabilities.soundtrackRoute}.
   */
  audioConditioning: boolean;
  /**
   * HOW a supplied track reaches the engine, because the two are not interchangeable and the
   * compiler has to pick one.
   *
   * - `audio_source` — LTX: a track on the request, sliced per segment to drive lip-sync.
   * - `audio_reference` — H3: a per-generation slice attached as a numbered `<Audio N>` omni
   *   reference. Capped at 2 refs, each **2–15 s**, `sum <= 15 s` — a budget SEPARATE from the
   *   reference-video one (`minimax_h3_handler.py:322` vs `:355`; the shim's `config.py:184`
   *   comment calls them one shared pool and is wrong).
   */
  soundtrackRoute: 'audio_source' | 'audio_reference' | null;
  /**
   * A single SHOT can carry its own audio file.
   *
   * False on H3 and not from a missing feature: H3's render unit is the generation, and audio
   * arrives through the ordered reference list where position assigns the `<Audio N>` ordinal. A
   * per-shot uploader would have nowhere to put the file that the prompt could then address.
   */
  perShotAudioUpload: boolean;
  /**
   * The original file is remuxed over the finished cut.
   *
   * 🔴 True on H3 **because `fully_copy` is a prompt marker, not a copy instruction.** It declares
   * the relationship in `retention_analysis`; the output audio still comes out of the joint
   * denoise, so the deliverable is a RECONSTRUCTION of the song rather than the song. This is
   * exactly why LTX remuxes too (`ltx_video.rs:3016` — "the model audio is conditioning only").
   */
  soundtrackMux: boolean;
  /**
   * The engine GENERATES the soundtrack alongside the picture. True on H3 and on LTX-2.5.
   *
   * ⚠️ A FACT, and not the gate for the soundtrack-mode dial it used to draw. That dial asks what
   * happens to the model's own track and whether the characters can hear yours, and both answers
   * are written into H3-only document fields — so it is gated on
   * {@link soundtrackRoute} `=== 'audio_reference'`, which is the property that actually makes the
   * question askable: the supplied track reaches the model as an `<Audio N>` REFERENCE and the
   * delivered audio still comes out of the joint denoise. On LTX-2.5 the track is an
   * `audio_source`, koblem remuxes it over the finished cut, and there is nothing to choose.
   */
  generatesAudio: boolean;
}

/**
 * LTX-2.5 under ComfyUI — the `ltx25` plugin on the comfy-api shim, and the only LTX there is.
 *
 * 🔴🔴 **A FLAG HERE IS A SWITCH ON A CONTROL. IT IS NOT A DESCRIPTION OF THE ENGINE.** Several of
 * these gate a control that writes an H3-ONLY document field, and `compile.ts` reads those fields
 * inside `compileH3Timeline` and nowhere else. Setting one `true` because 2.5 genuinely has the
 * underlying ability does not deliver the ability — it draws the H3 control on an LTX project,
 * where what it writes compiles to nothing, and it takes away the LTX control that worked. Three
 * were flipped that way on 2026-08-13 and reverted the same day. **An engine fact with no control
 * to gate belongs in this block, in prose — not in a flag.** The three facts, so nobody re-applies
 * the correction:
 *
 *  - **A LoRA is named by BARE FILENAME here** (a separator is a 400). But `loraIdentity` picks the
 *    PICKER, and `path` is the one that works: it writes `[{path, multiplier}]`, exactly what
 *    `parse_loras` wants, and `resolve_offered_loras` offers this engine's own listing verbatim, so
 *    the values already ARE bare filenames. `filename` swaps in `H3RenderKnobs`, which writes
 *    `h3Loras` / `h3LoraMultipliers` / `h3Steps` — read only by `compileH3Timeline`, and one of
 *    them an H3 step-count knob on an engine whose `customRecipe` is false — and `ShotDetailsPane`
 *    has no `filename` picker at all, so per-shot LoRAs vanish outright.
 *  - **A last frame CAN be pinned here**, as a keyframe at the final index. But `endFrame` draws
 *    H3's `fl2va` Last-frame dropzone, bound to `clip.h3_end_image` and read at `compile.ts` inside
 *    `compileH3Timeline` — so a closing frame attached through it renders nothing at all.
 *  - **2.5 writes its own soundtrack.** That one IS what `generatesAudio` means, so it stays `true`
 *    below. The H3 soundtrack dial it used to gate moved to `soundtrackRoute === 'audio_reference'`,
 *    which is what that dial is really about — see {@link EngineCapabilities.generatesAudio}.
 *
 * ⚠️ Until 2026-08-14 this was written as LTX-2.3's record plus overrides, because 2.5 was being
 * brought up alongside the C++ `ltx-video` engine rather than in place of it. 2.3 is gone — service,
 * container and all — so the two are one record again. What the overrides used to say about the
 * ComfyUI runtime is still true and is kept below, each one measured against `plugins/ltx25/
 * contract.py`, which answers a 501 naming itself rather than silently downgrading:
 *
 *  - **`modelVariants: false`** — 2.5 ships ONE distilled transformer. `echo-e50` / `echo-full`
 *    were 2.3 checkpoints and naming one is a 501.
 *  - **`negativePrompt` / `cfg` / `customRecipe: false`** — the checkpoint is DISTILLED, so
 *    `negative_prompt`, `cfg_scale`, `sampling_method`, `sample_params`, `scheduler` and `steps`
 *    are accepted and NOT honoured (they ride on every LTX request, so refusing them would
 *    hard-fail every render). Live controls writing them would be exactly the "field that compiles
 *    to nothing" defect.
 *  - **`retakeSplicesTail: false`** — a retake re-renders its own segment against the banked prefix;
 *    the segments after it are restored from the bank as they were, NOT re-pinned to the new take's
 *    final frame. The engine's own capabilities document says so (`retake.splices_tail: false`), so
 *    the join after a retaken shot can drift until the next hard cut — exactly H3's behaviour.
 *  - **`upscaleChain: false`** — the two-stage template exists, but its stage list, sigmas,
 *    sampler, cfg and steps are the checkpoint's, and the upscaler is DECLINED outright for any
 *    request carrying an opening frame, keyframes or character references (guides must be cropped
 *    before `LTXVLatentUpsampler`; with it on, stage 2 refines with nothing holding the subject —
 *    measured, a character lost an eye). Which is nearly every Director shot. koblem never sends
 *    the explicit `upscale` that would override the decline — it sends `hires` / `hires_chain`,
 *    which `_resolve_upscale` treats as a quality-profile artifact. The single-stage render is at
 *    the FINAL geometry and is a complete denoise, not a truncation.
 *    🔴 It is also load-bearing for the CANVAS: `true` re-enables `snapTargetToProfile`, which
 *    rewrites the project's width/height onto a `32 × profile factor` ladder 2.5 does not have and
 *    marks the doc dirty doing it, and it suppresses `checkCanvas`'s ×32 error — which is the one
 *    rule `check_canvas` in the plugin hard-400s on.
 *
 * ⚠️ `resume` is true and gated a second time. There is a real latent bank, keyed per segment and
 * reported back as `segments[].bank_key`; a resume restores the banked prefix and renders only the
 * tail. But the bank is a DIRECTORY the deployment configures, so `resumeNeedsLiveBank` is true and
 * {@link resumeAvailable} — not this flag — is what a control reads.
 *
 * ## `v2v` and `beats` went TRUE on 2026-08-14, and both had been false on a wrong reason
 *
 * Kept in prose here because in each case the FLAG moved for a reason, and the reason it used to
 * carry is the sort that gets re-derived and re-applied.
 *
 * - **`v2v`** was false on "ComfyUI core has no LTX node that takes a per-frame control track".
 *   True, and irrelevant: the C++ engine's v2v was never a control stack either. It VAE-encodes the
 *   source over `init_latent`, leaves the denoise mask at 1.0 and truncates the schedule by
 *   `t_enc = steps * strength` — plain SDEdit, which `SplitSigmasDenoise` (core) does. `v2v_mode` 1
 *   and 2 are ONE code path there, differing only in which field supplies the strength, and the
 *   plugin implements them the same way, so **Edit** and **Restyle** keep meaning what they meant.
 *   ⚠️ Lightricks' own 2.5 IC-LoRA V2V is a different and probably better mechanism; if it is ever
 *   ported it gets its own name rather than quietly changing what these two buttons do.
 * - **`v2vPriorRenderSource` stays false**, which is the narrow half that is genuinely still 501 —
 *   see the field doc. koblem's way to do it today is `control_frames`, which it already builds for
 *   a foreign clip; that is exactly the upload path.
 * - **`strength` does not mean here what it means anywhere else.** The distilled schedule is
 *   FRONT-LOADED — its first four steps sit between sigma 1.0 and 0.975 — so 0.5 enters at 0.975
 *   and is very nearly a fresh generation. Only nine outcomes are reachable (0–8 entered steps) and
 *   the useful editing range is below about 0.3. `v2vStrengthDefault` and `V2V_STRENGTH_STEP` in
 *   `lib/director/continuity.ts` are where that lives; the defaults were retuned off 2.3's
 *   schedule in the same change.
 * - **`beats`** was false on "Prompt Relay re-encodes the prompt register mid-sample, and no
 *   ComfyUI node does that". **The mechanism description was simply wrong**, and the comfy side
 *   corrected it: there is ONE text encode, and relay is an additive Gaussian bias on text
 *   cross-attention — `C(i,j) = strength * ReLU(|t(i) - m| - w)^2 / (2 s^2)` — which a block-level
 *   attention patch can apply. `custom_nodes/ltx25_prompt_relay` is that patch, first-party, and it
 *   owns the positive text encode because only the tokeniser knows where each clause lands in the
 *   token stream. Three differences the beats UI has to state, because they are behavioural:
 *   `relay_steps_frac` is accepted and NOT honoured (retiring the mask partway needs a per-step
 *   hook a block-level patch does not get), so beats read HARDER here than on the C++ engine; beats
 *   are REFUSED on the upscaled arm, because stage 2 has four times the tokens per frame and one
 *   beat map cannot be right for both samplers; and the whole thing is **UNRENDERED** — the
 *   arithmetic is unit-tested, the tensor layer has never been through a sampler, and the node
 *   disables itself with a loud log rather than misaligning, so the failure mode is a shot with no
 *   timing rather than a shot with wrong timing.
 *
 * 🔴 **`beats` is a CEILING, exactly like `resume`, and {@link beatsAvailable} is what a control
 * reads.** The relay node needs a comfyui container restart to appear in `/object_info`; until it
 * does, a `beats` request is refused with a 501 naming the mount — after the GPU-gate wait, which is
 * the expensive place to learn it. So this flag says the engine implements Prompt Relay, and
 * `props.beats_enabled` says this deployment has the node mounted. The pair is drawn together, the
 * same shape as `resume` + `bank_enabled`.
 */
const LTX: EngineCapabilities = {
  label: 'LTX-2.5',
  blurb:
    'LTX-2.5 on ComfyUI: chained shots with banked latents, runtime LoRAs, keyframes and character references, and it writes its own soundtrack. Where the deployment configures a bank directory, banked latents give resume and single-shot retake — but a retake is not re-pinned to the shots after it. Edit or restyle an uploaded clip, and pin timed beats inside a shot. One model, one pass at the final size.',
  // A shot IS a render here, so there is nothing inside one to name. `null` rather than `'shot'`:
  // it is what tells a component to draw no sub-unit markers at all.
  vocab: {
    renderUnit: 'shot',
    renderUnitPlural: 'shots',
    subUnit: null,
    subUnitPlural: null,
    subUnitCut: null,
    subUnitCutPlural: null
  },
  // Distilled: accepted on the wire and NOT honoured. Left visible they would be live controls
  // writing fields that compile to nothing.
  negativePrompt: false,
  cfg: false,
  customRecipe: false,
  // The shots compile through `compileSegment`, so a per-shot seed lands on the segment. The three
  // DISTILLED flags above are what hide the knobs the checkpoint ignores.
  perShotSampling: true,
  modelVariants: false,
  // Its own adapter directory, listed at `/sdapi/v1/loras`. `path` despite the bare-filename wire
  // format — see the flag-vs-fact block.
  loras: true,
  loraIdentity: 'path',
  // Declined for any shot carrying conditioning, which is nearly all of them — and it owns the
  // canvas snap and the ×32 guard as well. See the doc block.
  upscaleChain: false,
  variableFps: true,
  fixedFps: null,
  // 8k+1.
  framePeriod: 8,
  frameBase: 1,
  minFrames: null,
  maxFrames: null,
  dimMultiple: 32,
  advisoryMaxPixels: null,
  // LTX's sizes come from `resolution.ts`: the target has to divide back out through the selected
  // quality profile's upscale factor, so they cannot be a flat list here.
  canvasPresets: null,
  // No single home size, for the same reason: the ladder decides it. A project arriving here keeps
  // the canvas it brought.
  homeCanvas: null,
  // A continuation carry is a live latent wire inside one ComfyUI prompt, which is why a run's
  // segments travel in one request.
  chainedShots: true,
  // The bank restores the tail as it was rather than re-pinning it. See the doc block.
  retakeSplicesTail: false,
  resume: true,
  // `COMFY_API_LTX_BANK_DIR` decides whether the bank exists at all on this deployment.
  resumeNeedsLiveBank: true,
  // `render-plan` answers `supported: false` here — a shot IS a render, so there is no packing to
  // describe and the server has no staleness answer to give.
  scopedFullRender: false,
  midShotKeyframes: true,
  pinStrength: true,
  endFrame: false,
  // SDEdit over the init latent, both modes through one path. `compileSegment` writes `mode` /
  // `v2v_strength` / `v2v_source`, and it is the only compiler an `ltx25` project reaches.
  v2v: true,
  // The narrow half that is still 501: a banked latent becomes the sampler's `latent_image` and
  // `Ltx25LatentBankLoad` does not restore `downscale_ratio_spacial`. Uploads are unaffected.
  v2vPriorRenderSource: false,
  // A first-party attention-bias node, NOT the mid-sample re-encode this record used to claim.
  // ⚠️ The only capability here that cannot be pre-gated — see the doc block.
  beats: true,
  // The relay is a mounted custom node, not part of the image. See the field doc.
  beatsNeedsLiveNode: true,
  multiShotPerGeneration: false,
  structuredPrompt: false,
  generationIsRetakeUnit: false,
  crossGenerationContinuation: false,
  preview: false,
  previewIsFullRes: false,
  characterRefs: true,
  omniReferences: false,
  keyframesAndRefsCompose: true,
  projectSoundtrack: true,
  audioConditioning: true,
  soundtrackRoute: 'audio_source',
  perShotAudioUpload: true,
  soundtrackMux: true,
  // It denoises a soundtrack of its own. Gates nothing H3-shaped — see the field doc.
  generatesAudio: true
};

const H3: EngineCapabilities = {
  label: 'MiniMax-H3',
  blurb:
    'Video and its synchronised soundtrack from one pass. A shot is up to ~15s and holds several beats — timed moments the model renders itself, so continuity through one is free. A beat that changes the camera is a cut. Supply a song and performers sing to it. Runtime LoRAs are named by filename; the turbo one at 8 steps is -39% wall. No CFG.',
  // 🔴 `renderUnit` is 'shot' and NOT 'generation', deliberately: a `DirectorH3Generation` is what
  // renders, delivers and retakes, which is exactly what "shot" means on LTX. The clips packed into
  // it are 'cut's. The stored types and the `[Shot N]` tags in the block keep their names — see
  // `EngineVocab`.
  vocab: {
    renderUnit: 'shot',
    renderUnitPlural: 'shots',
    // 🔴 'beat', not 'cut' — see the field doc. A cut is the flavour below.
    subUnit: 'beat',
    subUnitPlural: 'beats',
    subUnitCut: 'cut',
    subUnitCutPlural: 'cuts'
  },
  negativePrompt: false,
  cfg: false,
  // The sampler and scheduler ARE honoured (the shared sampler walks them) — but the LTX "custom
  // recipe" panel is a base-sigmas + hires-stage editor and the stages half does not exist here.
  // The flow shift is FIXED at 12.0 on the WanGP contract and is not a field at all.
  customRecipe: false,
  // The four per-clip override fields are read by `compileSegment` alone, which never runs here —
  // H3's live on the generation the shot packs into. See the field doc.
  perShotSampling: false,
  modelVariants: false,
  // 🔴 TRUE. `Generation.loras` + `lora_multipliers` are live on the shim contract and
  // `/h3/v1/loras` lists what is on disk — see the field doc for why this said `false` for a day
  // after that stopped being true. The headline case is the turbo LoRA at 8 steps: -39% wall for
  // zero extra VRAM, and it is the single biggest lever on this engine.
  loras: true,
  // Bare filenames, resolved inside the engine's `minimax_h3/` directory. Not paths.
  loraIdentity: 'filename',
  upscaleChain: false,
  variableFps: false,
  fixedFps: H3_FPS,
  // 17n+5.
  framePeriod: 17,
  frameBase: 5,
  // 🔴 22 = 0.917 s, the video VAE's own DECODABLE floor: a 2-latent block decodes 0 chunks and
  // the decoder raises, so 5 frames really is refused and 22 (= 5 + 17) is the first that is not.
  // It was 107 until 2026-08-07 — that is the grid-snap of MiniMax's documented 4-second DURATION
  // guideline, and enforcing it silently lengthened every shorter beat. See
  // `H3_DOCUMENTED_MIN_FRAMES`.
  // 362 = 15.083 s, on-grid and equal to WanGP's own `window_default`.
  // Both bound a GENERATION — see the field doc above.
  minFrames: 22,
  maxFrames: 362,
  dimMultiple: 32,
  // ADVISORY (1344 * 768) — the area every VRAM and timing number was measured at, NOT a limit.
  advisoryMaxPixels: 1032192,
  // MiniMax's own documented aspect list, every one on the ×32 grid.
  // 🔴 1344×768 is **7:4 (1.75)**, not 16:9 (1.7778) — it is what an area cap does to a true 16:9
  // canvas at a 768 short edge. Both are offered, labelled honestly.
  canvasPresets: [
    { w: 1344, h: 768, label: '1344×768 · 7:4 · MiniMax 768p (default)' },
    { w: 1376, h: 768, label: '1376×768 · true 16:9 · 768p' },
    { w: 768, h: 1344, label: '768×1344 · 9:16 portrait · same cost' },
    { w: 1024, h: 768, label: '1024×768 · 4:3' },
    { w: 768, h: 768, label: '768×768 · 1:1' },
    { w: 1536, h: 672, label: '1536×672 · 21:9' },
    { w: 1152, h: 640, label: '1152×640 · 16:9 · cheap rung (~0.5× attention)' }
  ],
  homeCanvas: H3_DEFAULT_CANVAS,
  chainedShots: false,
  retakeSplicesTail: false,
  resume: false,
  resumeNeedsLiveBank: false,
  scopedFullRender: true,
  midShotKeyframes: false,
  pinStrength: false,
  endFrame: true,
  v2v: false,
  // No per-clip source field at all, so there is no narrower question to ask — see the field doc.
  v2vPriorRenderSource: false,
  beats: false,
  beatsNeedsLiveNode: false,
  multiShotPerGeneration: true,
  structuredPrompt: true,
  generationIsRetakeUnit: true,
  crossGenerationContinuation: true,
  preview: true,
  previewIsFullRes: true,
  characterRefs: false,
  omniReferences: true,
  keyframesAndRefsCompose: false,
  projectSoundtrack: true,
  audioConditioning: true,
  soundtrackRoute: 'audio_reference',
  // The generation is the render unit and the reference LIST is where audio lives — see the field
  // doc. Not a gap.
  perShotAudioUpload: false,
  soundtrackMux: true,
  generatesAudio: true
};

export function capabilities(engine: DirectorEngine): EngineCapabilities {
  return engine === 'h3' ? H3 : LTX;
}

/**
 * Can this deployment resume a render and retake one unit RIGHT NOW — capability AND bank together.
 *
 * 🔴 `capabilities().resume` answers a question about the model; `bank_enabled` answers one about
 * the box it is running on, and only the pair is enough to draw a control. LTX-2.5 implements both
 * and answers 501 to each with `COMFY_API_LTX_BANK_DIR` unset, so a retake offered off the
 * capability alone dispatches, waits in the queue, and lands as an error on the job.
 *
 * ⚠️ `undefined` is "the engine did not say", and on a {@link EngineCapabilities.resumeNeedsLiveBank}
 * engine that is a NO — the props call is unconditional there, so silence means the service is
 * unreachable and nothing can be resumed from it anyway. It must not read as a no anywhere else:
 * koblem sends no flag at all for the C++ engine, whose bank is part of the build.
 */
export function resumeAvailable(
  engine: DirectorEngine,
  bankEnabled: boolean | null | undefined
): boolean {
  const caps = capabilities(engine);
  if (!caps.resume) return false;
  return caps.resumeNeedsLiveBank ? bankEnabled === true : true;
}

/**
 * Can this deployment author timed beats RIGHT NOW — the capability AND the mounted relay node.
 *
 * 🔴 The exact shape of {@link resumeAvailable}, and it exists for the same failure. LTX-2.5
 * implements Prompt Relay, but the relay is `custom_nodes/ltx25_prompt_relay` mounted into the
 * comfyui container, and ComfyUI only lists it in `/object_info` after that container restarts —
 * separately from the comfy-api container that answers `/props`. Until then a shot carrying `beats`
 * is refused at graph build with a 501 naming the mount, and the refusal arrives AFTER the GPU gate
 * has been acquired and waited on. A control drawn off `caps.beats` alone buys that wait every time.
 *
 * ⚠️ `undefined` is "the engine did not say", and on a {@link EngineCapabilities.beatsNeedsLiveNode}
 * engine that is a NO: the props call is unconditional there, so silence means the service is
 * unreachable, and an engine that cannot be asked cannot render beats either. It must not read as a
 * no anywhere else — an engine whose beats need no mounted node sends no flag at all.
 */
export function beatsAvailable(
  engine: DirectorEngine,
  beatsEnabled: boolean | null | undefined
): boolean {
  const caps = capabilities(engine);
  if (!caps.beats) return false;
  return caps.beatsNeedsLiveNode ? beatsEnabled === true : true;
}

/**
 * What a retake does to the units AFTER the one it re-renders, in one sentence.
 *
 * 🔴 One phrasing for every retake affordance on both shells, because the answer is a fact about
 * the engine and two hand-written versions of it would be two chances to promise a splice that
 * does not happen. It is the same claim `take-continuity.ts` makes when it reports a stale join —
 * `retakeSplicesTail` decides both, so the seed box and the Joins panel cannot disagree.
 */
export function retakeTailNote(caps: EngineCapabilities): string {
  const { renderUnit, renderUnitPlural } = caps.vocab;
  return caps.retakeSplicesTail
    ? `splices only this ${renderUnit} back into the banked chain — the ${renderUnitPlural} after it are re-pinned to its new final frame.`
    : `re-renders this ${renderUnit} alone — the ${renderUnitPlural} after it are left as they were rather than re-pinned to its new final frame, so the join can drift until the next hard cut.`;
}

/**
 * What is different about beats HERE, in one sentence, for every surface that offers them.
 *
 * 🔴 One string rather than two hand-written ones for the same reason {@link retakeTailNote} is a
 * function: the beats array editor and the "let the LLM place beats" tick-box both promise
 * something about how beats behave, and two copies of that promise are two chances for one of them
 * to go stale. Every clause is a fact the author can see in the output:
 *
 *  - **They read harder than they did.** `relay_steps_frac` is accepted and NOT honoured — retiring
 *    the mask partway through the sample needs a per-step hook a block-level attention patch does
 *    not receive — so the bias is on for the whole denoise instead of the opening fraction of it.
 *  - **UNRENDERED.** The node's arithmetic is unit-tested; the tensor layer and the attention patch
 *    have never been through a sampler. It verifies the token geometry it was told against the
 *    tensor it gets and turns itself OFF with a loud log rather than misaligning, so the failure
 *    mode to expect is a shot that ignores its timing, not a shot timed wrongly.
 *
 * ⚠️ There is a third limit — beats are refused on the two-stage upscaled arm, because stage 2 has
 * four times the tokens per frame and one beat map cannot be right for both samplers — which is NOT
 * in this string. It cannot be reached: {@link EngineCapabilities.upscaleChain} is false on the only
 * engine with beats, and koblem never sends the explicit `upscale` that would turn stage 2 on. It
 * belongs in the LTX record's doc block, where it is, until something can actually hit it.
 *
 * 🔴 And one that CANNOT be said here at all: the relay node needs a comfyui container restart to
 * appear in `/object_info`, and until it has, a shot carrying beats is refused with a 501 — after
 * the GPU-gate wait. The engine publishes that fact (`capabilities.beats`), but koblem's props body
 * carries only `bank_enabled`, so nothing in the browser can read it. See the LTX doc block.
 */
export const BEATS_RELAY_NOTE =
  'Beats are conditioned by a bias on the prompt’s attention that stays on for the whole render, ' +
  'so they bite harder than a timing hint — keep a beat to the one thing that changes. ' +
  'This path is new and has not been through a render yet: if the engine cannot line the beats up ' +
  'it drops them and renders the shot untimed rather than timing it wrongly.';

/**
 * What this engine's units are called on screen. See {@link EngineVocab}.
 *
 * Exported separately from `capabilities` because most components that need it need ONLY it —
 * `Timeline`, `TakesPanel` and friends take a `vocab` prop rather than an `engine` one,
 * so they never gain the ability to branch on the engine at all.
 */
export const vocabFor = (engine: DirectorEngine): EngineVocab => capabilities(engine).vocab;

/**
 * Which `generations` service this engine's finished renders are filed under.
 *
 * 🔴 The two engines do NOT share a bucket. `run_h3_job` persists under `minimax-h3` and
 * `run_ltx_job` under `ltx25` (`LtxVideoClient::service_tag()`), so a history panel asking for the
 * wrong one is not merely incomplete — it is empty, with no error to notice. That is exactly what
 * happened: the Director's render history was hardcoded to one bucket, so an H3 project showed
 * nothing while its rows piled up under the other.
 *
 * ⚠️ Renders made before LTX-2.3 was retired are filed under a third name, `ltx-video`, which
 * nothing returns any more. Those rows are still in `generations`; they are simply no longer
 * reachable from the Director's history panel.
 *
 * ⚠️ These are SERVER service names, not `DirectorEngine` values, and they are not interchangeable
 * with the `minimax_h3` checkpoint prefix or the `h3` engine id. Change one only with
 * `tier2_helpers::persist_generation_files`'s caller in hand.
 */
export const historyServiceFor = (engine: DirectorEngine): string =>
  engine === 'h3' ? 'minimax-h3' : 'ltx25';

/**
 * The default a component falls back to when nobody has told it. LTX's, where a shot IS a render.
 *
 * ⚠️ Not a neutral placeholder — it is the correct answer for every LTX project, which is why it is
 * LTX's rather than an invented third vocabulary.
 */
export const DEFAULT_VOCAB: EngineVocab = LTX.vocab;

/** The engine picker's contents, in the order it offers them. */
export const ENGINE_OPTIONS: { value: DirectorEngine; label: string }[] = [
  { value: 'ltx25', label: LTX.label },
  { value: 'h3', label: H3.label }
];

/**
 * What WanGP will accept as an `<Audio N>` reference, verbatim from
 * `models/minimax_h3/minimax_h3_handler.py::validate_generative_settings`.
 *
 * Mirrored here (rather than only shim-side) because the editor has to STOP the author before a
 * render: a song slice that violates one of these comes back as a 400 after the queue wait, and
 * the author has no way to see why from the timeline.
 */
export const H3_AUDIO_REF = {
  /** `:353` — `if audio_count > 2`. Two slots: `audio_guide` and `audio_guide2`. */
  maxRefs: 2,
  /** `:348` — `if not 2 <= duration <= 15`. Per FILE, checked with librosa. */
  minSeconds: 2.0,
  maxSeconds: 15.0,
  /**
   * `:355` — `sum(audio_durations) > 15`.
   *
   * 🔴 A budget SEPARATE from the reference-video one at `:322`. 15 s of audio AND 15 s of video is
   * legal. The shim's `app/config.py:184` comment ("each reference video/audio 2-15 s with a 15 s
   * total") reads as one shared pool and is wrong — do not re-derive this from it.
   */
  totalSeconds: 15.0
} as const;

/**
 * The generation ceiling for a project that carries a soundtrack: **345 frames = 14.3750 s**.
 *
 * 🔴 **Lower than `maxFrames` (362) on purpose, and the grid is what forces the choice.**
 *
 * Typing "15 s" snaps UP to 362 frames = 15.0833 s, because the grid is `17n+5` and 360 is not on
 * it. But an audio reference caps at 15.0 s exactly (`H3_AUDIO_REF.maxSeconds`), so a maximum-length
 * generation cannot carry a slice as long as its own picture — it is over by 2 frames. The grid has
 * nothing between: the next rung down is 345f, and 345 → 362 is the whole ladder near the top.
 *
 * Two ways out, and this is the one that was chosen:
 *
 * - **Cap at 345 (this).** The slice equals the generation exactly, so "how long is this
 *   generation" and "how long is its audio" are one number and cannot drift apart. Costs 0.71 s of
 *   length per generation — roughly one extra seam across a three-minute song.
 * - Keep 362 and trim the slice to 15.0. Cheaper in seams, but it permanently separates the two
 *   clocks, and any code that advances the song by `sliceSeconds` instead of by the generation's
 *   real length compounds 0.083 s per generation — a second of drift by the end of a song.
 *
 * The second failure is silent and grows along the cut, which is the kind this codebase keeps
 * getting bitten by. The cap makes it unrepresentable.
 *
 * ⚠️ Applies ONLY when a soundtrack is attached. A project with no song keeps the full 362.
 */
export const H3_MAX_FRAMES_WITH_AUDIO = 345;

/**
 * The frame ceiling in force for this project, which depends on whether it carries a song.
 *
 * The one definition — the packer, the timeline's boundary labels and the slice planner all read
 * it, so a generation can never be packed longer than its audio is allowed to be.
 */
export function h3MaxFrames(hasSoundtrack: boolean): number {
  return hasSoundtrack ? H3_MAX_FRAMES_WITH_AUDIO : (H3.maxFrames ?? H3_MAX_FRAMES_WITH_AUDIO);
}

/**
 * How long a slice for a generation of this length should be, or `null` when it cannot have one.
 *
 * With {@link H3_MAX_FRAMES_WITH_AUDIO} in force the `min` never actually binds — the generation is
 * already inside the cap. It stays as a backstop for a saved document packed before the cap existed
 * (or one hand-edited past it), where the alternative is a 400 from `librosa` after the queue wait.
 *
 * `null` under the 2 s floor: there is no legal slice, and the generation renders on the model's own
 * generated bed. A legitimate outcome, but one the author has to be told about — see
 * `h3-soundtrack.ts`.
 */
export function audioSliceSeconds(generationSeconds: number): number | null {
  if (generationSeconds < H3_AUDIO_REF.minSeconds) return null;
  return Math.min(generationSeconds, H3_AUDIO_REF.maxSeconds);
}

/** What is wrong with a canvas, if anything. See {@link checkCanvas}. */
export interface CanvasCheck {
  /** Off the engine's dimension grid. **Model law** — the render is refused, not corrected. */
  error: string | null;
  /** Legal, but outside the area the engine's numbers were measured at. Renders anyway. */
  warning: string | null;
  /** The nearest legal pair, offered as a one-click fix. `null` when the size is already legal. */
  suggest: { w: number; h: number } | null;
}

/**
 * The one place the canvas rules are applied, so the editor, the compiler and koblem cannot
 * disagree about which of them is a refusal.
 *
 * 🔴 Two rules, deliberately different in kind:
 *
 * - **Multiple of `dimMultiple` is model law** and is REPORTED, never silently snapped. Snapping
 *   is what `compile.ts` used to do — a user who typed 1280×720 got 1280×704 with nothing said,
 *   which is the same defect as the frame floor quietly lengthening a shot. Telling them *"720 is
 *   not a multiple of 32; the legal neighbours are 704 and 736"* is the whole point: 720 and 1080
 *   are the numbers people know, and both are illegal on H3.
 * - **The area figure is guidance about VRAM.** `diffusers` calls H3's exact number a *"soft area
 *   cap"*, ComfyUI does not enforce it, and no config in the weights tree carries a max
 *   resolution — so it warns about fit rather than claiming the size is illegal.
 */
export function checkCanvas(engine: DirectorEngine, width: number, height: number): CanvasCheck {
  const caps = capabilities(engine);
  const m = caps.dimMultiple;
  const w = Math.max(0, Math.round(width));
  const h = Math.max(0, Math.round(height));
  const badW = w % m !== 0;
  const badH = h % m !== 0;

  // 🔴 Only on an engine WITHOUT an upscale chain. On LTX the binding grid is the quality
  // profile's `32 × factor`, not this 32, and `snapTargetToProfile` already owns it and explains
  // itself through `snapNote` — reporting a second, laxer rule here would contradict it.
  if ((badW || badH) && !caps.upscaleChain) {
    // Round to the NEAREST legal pair rather than down: 720 → 704 loses less than 720 → 736 gains,
    // but 1080 → 1088 is the size everyone actually means, and nearest gets both right.
    const near = (n: number) => Math.max(m, Math.round(n / m) * m);
    const which = badW && badH ? `neither ${w} nor ${h} is` : badW ? `${w} is not` : `${h} is not`;
    return {
      error:
        `${w}×${h} is not a legal ${caps.label} canvas — ${which} a multiple of ${m}. ` +
        `The VAE downsamples 16× and the model patchifies 2× on top of it, so both sides have to ` +
        `divide by ${m} exactly. Try ${near(w)}×${near(h)}.`,
      warning: null,
      suggest: { w: near(w), h: near(h) }
    };
  }

  if (caps.advisoryMaxPixels && w * h > caps.advisoryMaxPixels) {
    return {
      error: null,
      warning:
        `${w}×${h} is ${(w * h).toLocaleString()} pixels, over the ` +
        `${caps.advisoryMaxPixels.toLocaleString()} that 1344×768 was measured at. This is ` +
        `advisory, not a limit — the model has no maximum resolution — but a much larger canvas ` +
        `may not fit the 16 GB card, and every speed and memory figure we have is for 1344×768.`,
      suggest: null
    };
  }

  return { error: null, warning: null, suggest: null };
}

/**
 * The canvas a project must be moved to as it ARRIVES on `engine`, or `null` to keep the one it has.
 *
 * 🔴 Beside the engine change, not inside one control's `onchange`. The reframe used to live in the
 * picker, so it fired on a switch and not on an undo that crossed one — and an undo back onto H3
 * left a 1920×1088 canvas that only `checkCanvas`'s warning ever mentioned. Anything that moves the
 * engine calls this; that is the whole reason it is a function rather than four lines in a handler.
 *
 * Off-grid and over-budget both count, because both are what an LTX canvas looks like from H3:
 * ×32 is model law, and the area figure is the shape every VRAM number was measured at. Engines
 * with no {@link EngineCapabilities.homeCanvas} answer `null` — there is nothing to move to.
 */
export function reframeForEngine(
  engine: DirectorEngine,
  width: number,
  height: number
): { w: number; h: number } | null {
  const caps = capabilities(engine);
  if (!caps.homeCanvas) return null;
  const offGrid = width % caps.dimMultiple !== 0 || height % caps.dimMultiple !== 0;
  const overBudget = caps.advisoryMaxPixels !== null && width * height > caps.advisoryMaxPixels;
  return offGrid || overBudget ? caps.homeCanvas : null;
}

/**
 * The ceiling this BOX can currently reach, as opposed to the one the spec allows.
 *
 * Two different numbers and both are worth showing. 362 is the contract's cap and never changes;
 * what a given container will actually run is a function of VRAM, the loaded checkpoint and
 * whatever reachability work has landed, and the shim answers it at `/h3/v1/capabilities`. Showing
 * only the spec cap invites a render that fails at 15 s on a box that tops out at 12; showing only
 * the reachable one hides that the timeline is being constrained by this machine rather than by
 * the model.
 *
 * `null` when the engine is down or has not been asked — the caller then has only the spec cap,
 * which is the honest thing to display.
 *
 * 🔴 **It lives at `capabilities.frames.reachable_max`.** The shim's document nests its frame facts
 * under `frames` (`main.py`'s `/h3/v1/capabilities`), and koblem forwards that document VERBATIM as
 * `capabilities` — koblem's own flat `max_frames` sits one level up, on the props body, and is the
 * SPEC cap that never moves. Reading a top-level `capabilities.max_frames` therefore found nothing
 * on every real engine and this returned `null` unconditionally, which reads as "the box did not
 * say" and is exactly the silence this function exists to break. The flat key is still accepted as
 * a fallback so a shim that ever flattens the document keeps working.
 */
export function reachableMaxFrames(props: H3Props | null | undefined): number | null {
  const caps = props?.capabilities as Record<string, unknown> | null | undefined;
  if (!caps) return null;
  const frames = caps.frames as Record<string, unknown> | undefined;
  const raw = frames?.reachable_max ?? caps.reachable_max ?? caps.max_frames;
  return typeof raw === 'number' && Number.isFinite(raw) && raw > 0 ? Math.floor(raw) : null;
}

/**
 * The same ceiling in seconds. Read from the engine's own `duration_seconds.reachable_max` when it
 * is there rather than divided out of the frame count: the two are the same number on today's shim,
 * but the engine is the authority on its own arithmetic and a divide here would silently become a
 * second definition of it.
 */
export function reachableMaxSeconds(props: H3Props | null | undefined): number | null {
  const caps = props?.capabilities as Record<string, unknown> | null | undefined;
  const raw = (caps?.duration_seconds as Record<string, unknown> | undefined)?.reachable_max;
  if (typeof raw === 'number' && Number.isFinite(raw) && raw > 0) return raw;
  const frames = reachableMaxFrames(props);
  return frames === null ? null : frames / H3_FPS;
}

/**
 * Is this generation longer than what the box can currently render? The message, or `null`.
 *
 * 🔴 **A WARNING, and deliberately not a cap.** `reachable_max` is a RUNTIME property of a busy
 * box — it moves with VRAM, the loaded checkpoint and whatever reachability work is in flight — so
 * it must never be fed into the packer or into `H3_MAX_SECONDS`. A saved timeline that silently
 * re-packed itself because the GPU happened to be warm would change what renders without the author
 * touching anything, and change back an hour later. Authoring limits stay static; this says, next
 * to the generation it applies to, that the machine will refuse it today.
 *
 * Silent in three cases, all of which mean "no reduction has been reported":
 *  - the engine reports a ceiling at or above the spec cap;
 *  - the engine is down, or props were never fetched (`capabilities` is null);
 *  - the field is absent — an older shim does not send it, and inventing a limit from that would be
 *    worse than saying nothing.
 */
export function reachableCeilingNote(
  props: H3Props | null | undefined,
  frames: number
): string | null {
  const reachable = reachableMaxFrames(props);
  const spec = H3.maxFrames;
  if (reachable === null || spec === null) return null;
  if (reachable >= spec) return null;
  if (frames <= reachable) return null;
  const secs = (n: number) => (n / H3_FPS).toFixed(2);
  return (
    `This shot renders ${frames} frames (${secs(frames)}s), but the engine currently ` +
    `reaches only ${reachable} (${secs(reachable)}s) on this box — it will be refused. The model's ` +
    `own ceiling is ${spec} frames (${secs(spec)}s); what has dropped is what this machine can run ` +
    `right now, so the timeline has not been changed.`
  );
}

/**
 * Round a shot length UP onto an engine's frame grid.
 *
 * The DIRECTION matters and is the easy thing to get wrong: both engines snap up, so a shot
 * authored at 5.0 s (120 frames) renders as 124 on H3, not 120. A UI that rounds the other way
 * disagrees with the engine about the duration of every shot, and the error accumulates along the
 * cut until the audio no longer lines up with anything.
 */
export function snapFrames(engine: DirectorEngine, frames: number): number {
  const { framePeriod, frameBase, minFrames, maxFrames } = capabilities(engine);
  const snapped =
    frames <= frameBase
      ? frameBase
      : frameBase + Math.ceil((frames - frameBase) / framePeriod) * framePeriod;
  // 🔴 The clamp is part of the snap, matching koblibs' `h3_snap_frames`. Two implementations of
  // "what will this render at" that disagree is the whole failure this exists to prevent.
  //
  // On H3 the low end is now 22 frames — the VAE's decodable minimum — not the 107 it was until
  // 2026-08-07. That clamp is what made a 3 s request render as 4.46 s; the drift was invisible
  // until the audio no longer lined up. Being under MiniMax's documented 4 s is now REPORTED
  // (`belowDocumentedFloor` in `h3-generations.ts`) instead of corrected.
  //
  // ⚠️ It bounds a RENDER, which on H3 is a GENERATION rather than a shot. Nothing may call this
  // with one shot's length on H3 and treat the answer as that shot's duration — a 2.5 s shot is
  // legal as one cut inside a generation that clears the floor. `h3-generations.ts` is the only
  // caller that passes an H3 duration in, and it passes the generation's.
  const lo = minFrames ?? snapped;
  const hi = maxFrames ?? snapped;
  return Math.min(hi, Math.max(lo, snapped));
}

/** What this engine actually renders at: its own fixed rate, or the document's choice. */
const effectiveFps = (engine: DirectorEngine, fps: number): number =>
  capabilities(engine).fixedFps ?? fps;

/** Frames for a duration, on the engine's grid. */
export function framesForSeconds(engine: DirectorEngine, seconds: number, fps: number): number {
  return snapFrames(engine, Math.round(Math.max(0, seconds) * effectiveFps(engine, fps)));
}

/** The duration a shot will ACTUALLY render at, once its frames are snapped. */
export function snappedSeconds(engine: DirectorEngine, seconds: number, fps: number): number {
  return framesForSeconds(engine, seconds, fps) / effectiveFps(engine, fps);
}

/**
 * Does pinning a frame on this shot take it out of every reference's scope?
 *
 * The pin-vs-reference exclusivity rule, in ONE place. It was hand-coded at three call sites
 * (`ReferencesPanel`, `ShotDetailsPane`, `prompt-context`), each spelling out `mode === 'i2v' &&
 * image_b64 || h3_end_image` inline and each free to disagree with the others about what counts as
 * a pin. It is not a policy choice either: `fl2va` and `ref2va` are served by DIFFERENT weight
 * sets, so a shot carrying both is a request neither checkpoint can satisfy.
 *
 * Takes the boolean rather than the clip so the capability seam stays free of document types —
 * `pinsAFrame` in `h3-generations.ts` is the one definition of "pins a frame".
 */
export function pinBlocksReferences(engine: DirectorEngine, clipPinsAFrame: boolean): boolean {
  return clipPinsAFrame && !capabilities(engine).keyframesAndRefsCompose;
}

/**
 * Why a control is unavailable on this engine, as a sentence a user can act on.
 *
 * Deliberately explains the MODEL rather than saying "not supported": a greyed-out control with no
 * reason reads as a bug, and the cases users hit most — no negative prompt, references vs
 * keyframes, "where did my audio track go" — are all surprising unless you know why.
 *
 * 🔴 **Every key here must have a render site.** This map previously carried fourteen strings of
 * which twelve were never printed anywhere, so switching engine silently deleted a dozen controls
 * with no explanation and the reasons rotted unread. Six keys were deleted rather than given a
 * home: they explained controls that vanish inside a collapsed "advanced" fold, where a paragraph
 * of engine trivia is noise rather than an answer. If you add a key, render it in the same commit.
 *
 * | key | rendered at |
 * |---|---|
 * | `negativePrompt` | `SettingsSheet.svelte`, in place of the field |
 * | `variableFps` | `SettingsSheet.svelte`, under the disabled FPS box |
 * | `characterRefs` | `ReferencesPanel.svelte` header |
 * | `v2v` | `ShotDetailsPane.svelte`, under the mode picker |
 * | `v2vPriorRenderSource` | `ShotDetailsPane.svelte`, in place of the source-kind picker |
 * | `resume` | `RendersDrawer.svelte`, in place of "continue from last render"; `SettingsSheet.svelte` under the engine blurb; the disabled Retake buttons' titles |
 * | `keyframesAndRefs` | `ReferencesPanel.svelte` + `ShotDetailsPane.svelte` scope guards |
 *
 * `upscaleChain` is rendered the same way (`SettingsSheet.svelte`, in place of the quality-profile
 * picker) but lives in {@link UNAVAILABLE_LTX25} alone — H3 has no key for it, because the profile
 * picker's absence there is explained by the engine having no ladder rather than by a note.
 *
 * 🔴 **Two keys were deleted 2026-08-08 because they were apologising for features that exist.**
 * Both greyed out a panel and printed a paragraph, which reads as "missing" however carefully the
 * paragraph is written:
 *
 * - `beats` — **a beat is a timed clause inside a shot, not a `[Shot N]`.**
 *
 *   🔴 This entry asserted the opposite until 2026-08-28, on a 2026-08-08 measurement that a
 *   verbless `[Shot N] At MM:SS.mmm` renders as continuous picture. It held on WanGP and for weeks
 *   on ComfyUI, then reversed around ComfyUI v0.34.0 / torch 2.13 — the prompt text tokenizes
 *   byte-identically across both, so only the sample changed. A bit-identical A/B then showed the
 *   verbless tags cutting at exactly their marks. `[Shot N]` is a CUT; a moment that merely happens
 *   at a time is written `At MM:SS.mmm,` in the running prose.
 *
 *   A beat and a cut remain one wire construct with one optional property, which is why
 *   {@link EngineVocab} names the sub-unit `beat` and carries `subUnitCut` as a FLAVOUR rather than
 *   a second type — every legacy project still holds verbless tags and the UI has to label them.
 * - `perShotAudioUpload` — with the §3 vocabulary a render unit IS a shot, so audio genuinely is
 *   per shot. The panel became the shot's audio-reference row.
 */
export const UNAVAILABLE: Record<string, string> = {
  negativePrompt:
    'MiniMax-H3 is guidance-distilled — one forward per step, with no unconditional branch for a negative prompt to steer.',
  characterRefs:
    'MiniMax-H3 conditions on references through its text encoder rather than on tagged latents — so a reference is numbered and the prompt can name it.',
  v2v:
    'MiniMax-H3 edits video through a reference, not a mode. Add the clip in the References panel, ' +
    'set its role to “Edit this clip”, and scope it to this shot — the prompt then says what changes ' +
    'about it and the summary claims [video editing]. There is no per-shot source-video field here ' +
    'because the source is addressed by ordinal (<Video 1>) from inside the prompt.',
  // 🔴 The old wording — "MiniMax-H3 keeps no latent bank — there is nothing to resume from" — was
  // false. There IS a bank: `Reference.bank` takes a key, `Continuation.from` takes one instead of
  // `"prev"`, and `chain_anchor` is per-chain bank state the shim carries forward itself
  // (`BankEntry.chain_anchor_png`). What is absent is a spliced PREFIX to resume INTO, and that
  // absence costs nothing here — a shot is one render, so a failed run is repaired by re-rendering
  // the one that is missing rather than everything up to it.
  resume:
    'MiniMax-H3 renders each shot on its own, so there is no partial chain to resume — a run that ' +
    'stopped short is finished by rendering the shots it missed, not by replaying the ones it got. ' +
    '(The engine does keep a latent bank; it is what a shot continues from, not something to resume.)',
  keyframesAndRefs:
    'MiniMax-H3 renders an EXACT frame pin (fl2va) and omni references (ref2va) on different models, so a shot carrying a pin uses one or the other. Switch that cut’s frame to “Open on this image” and it stays on ref2va — the image becomes a numbered <Picture N> keyframe and every reference and the song slice survive.',
  variableFps:
    'MiniMax-H3’s frame grid, temporal VAE and audio latent rate are all defined at 24 fps.'
};

/**
 * The same explanations for LTX-2.5, which hides some of the same controls for entirely different
 * reasons.
 *
 * 🔴 This exists because {@link UNAVAILABLE} is H3's copy, not generic copy, and printing it on an
 * LTX-2.5 project would explain the absence by naming a model the project is not rendering on.
 * Every hidden control here traces to one of three facts: the checkpoint is DISTILLED (so the
 * sampling knobs are accepted and not honoured — refusing them would hard-fail every render, since
 * the LTX wire carries them on every request), the deployment has not configured the directory the
 * mechanism reads, or one graph node cannot be wired the way the request asks.
 *
 * ⚠️ **"The ComfyUI runtime has no node for the mechanism" used to be the middle one, and it was
 * the reason under both `v2v` and `beats`.** Both were wrong about the mechanism rather than about
 * the runtime, both capabilities landed on 2026-08-14, and both keys went with them — a control
 * that renders is not a control needing a paragraph. Reach for that reason again only with the node
 * list in hand.
 *
 * Keys are the capability names, matching `UNAVAILABLE`.
 */
export const UNAVAILABLE_LTX25: Record<string, string> = {
  negativePrompt:
    'LTX-2.5 is distilled — one forward per step, with no unconditional branch for a negative prompt to steer. The field is still accepted on the wire (every LTX request carries it) and has no effect.',
  cfg: 'LTX-2.5 is distilled: guidance is baked into the checkpoint, so a CFG scale is accepted and not honoured.',
  customRecipe:
    'LTX-2.5’s sampler, scheduler, sigma schedule and step count are the distilled checkpoint’s own, verbatim. They are accepted on the wire and not honoured.',
  modelVariants:
    'LTX-2.5 ships one transformer. The Echo variants were LTX-2.3 checkpoints and that engine has been retired — serving them off this base would be a different model reported as the one you asked for.',
  upscaleChain:
    'LTX-2.5 renders in one pass at the final size. Its two-stage upscaler is declined for any shot carrying an opening frame, keyframes or character references — the guides would condition stage 1 only and stage 2 would refine with nothing holding the subject.',
  // 🔴 A DEPLOYMENT absence, and the only one in either map — every other key here explains
  // something about the checkpoint. LTX-2.5 implements resume and single-shot retake fully; this
  // box has no bank directory for them to read, and the engine's own capabilities document says so.
  // Named as a setting rather than as a limitation because it is one somebody can go and fix.
  resume:
    'LTX-2.5 banks a latent per shot, but this deployment has no bank directory (COMFY_API_LTX_BANK_DIR) — with it unset the engine answers 501 to a resume and to a single-shot retake, so both are hidden here rather than offered and failed after the queue wait. The same message appears when the service is unreachable, since an engine that cannot be asked cannot be resumed from either.',
  // 🔴 What is LEFT of the old `v2v` key, which went when the mode landed. The mode itself is not
  // absent any more and the whole card renders; this explains the one button inside it that does
  // not, and it names the thing to do instead rather than the thing that is missing.
  // 🔴 A DEPLOYMENT absence, like `resume` above and unlike everything else here. LTX-2.5
  // implements Prompt Relay; this box has not restarted its comfyui container since the node was
  // mounted, so ComfyUI does not list it and the render would be refused at graph build.
  beats:
    'LTX-2.5 places timed beats with Prompt Relay, but that is served by a custom node (custom_nodes/ltx25_prompt_relay) and this deployment\u2019s ComfyUI does not list it yet \u2014 it appears once the comfyui container restarts. Until then a shot carrying beats is refused after the GPU wait, so the editor hides them rather than spending it. The same message appears when the service is unreachable, since an engine that cannot be asked cannot render beats either.',
  v2vPriorRenderSource:
    'LTX-2.5 edits an uploaded clip, but it cannot take a shot it rendered earlier as the source. That source would be a BANKED LATENT, and a v2v source becomes the sampler’s own starting latent — which the bank loader does not restore the spatial downscale ratio for, so the shot would be the wrong shape before it sampled a step. Export the render and drop it in as a clip: that is the same footage, sent as frames rather than as latents, which is the path the engine actually reads.'
};

/**
 * What to tell the user about a control this engine does not have.
 *
 * ⚠️ Falls back to the H3 wording, which is where all of these started and where most of them still
 * only apply. A missing key returns an empty string rather than `undefined`, so a template can
 * print it unguarded.
 */
export function unavailableFor(engine: DirectorEngine, key: string): string {
  if (engine === 'ltx25') return UNAVAILABLE_LTX25[key] ?? UNAVAILABLE[key] ?? '';
  return UNAVAILABLE[key] ?? '';
}
