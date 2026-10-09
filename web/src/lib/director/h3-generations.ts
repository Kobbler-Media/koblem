/**
 * Packing the timeline's shots into MiniMax-H3 **generations** — and the arithmetic that decides
 * where a seam is allowed to appear.
 *
 * ## Why this module exists at all
 *
 * A render is one clip on both engines. What H3 adds is that ONE render can contain several timed
 * moments — `[Shot N] At MM:SS.mmm` lines inside a single prompt, up to 15.083 s — with continuity
 * free between them and `<scenetrans>` carrying audio across a cut. Across a clip boundary you get
 * a seam and, at best, an approximate continuation.
 *
 * ⇒ **Splitting a clip is the degradation you accept when you exceed the budget, not the way to get
 * a cut.** A cut inside one render costs nothing; a cut across two loses `<scenetrans>`, loses the
 * prose identity carry ("the captain from Shot 1"), and adds a seam.
 *
 * koblem owns the packing rather than the engine, because koblem owns the timeline and therefore
 * the budget arithmetic; the shim receives an explicit `generations[]` list and does not re-pack.
 *
 * ## The rule, in one place
 *
 * 🔴 **One clip is one generation is one render.** A generation used to hold 1–N clips, which made a
 * clip a BEAT — and that inverted the vocabulary everywhere else in the product, stored each beat's
 * text and cut time twice, and produced the whole "the timeline has not caught up with the prompt"
 * apparatus. Two moments in one render is now what it always should have been: two `[Shot N]` lines
 * in ONE clip's prompt, rendered by the model itself with continuity free and `<scenetrans>`
 * carrying audio across the cut. {@link collapseH3Beats} migrates older projects onto this.
 *
 * A boundary is therefore never a decision: `breakReason` is `first` for the opening render and
 * `forced` for every other, because every clip boundary is one the author placed by making a clip.
 *
 * ## What is stored, and what is re-derived
 *
 * 🔴 The packing is **always re-derived**. Only the authored sections and the forced breaks are
 * persisted. A stored packing would drift the moment a shot was trimmed or reordered, and a stale
 * boundary is invisible until it renders — the exact failure mode the LTX bank taught us
 * (`project_ltx_retake_destroyed_takes`: state that was only *believed* to be on disk).
 * {@link packGenerations} re-associates each derived generation with the sections a human already
 * wrote for it by clip-id overlap.
 */

import type {
  DirectorClip,
  DirectorH3Continuation,
  DirectorH3Generation,
  DirectorH3Reference,
  H3Task
} from '$lib/api/client';
import { H3_FPS, capabilities, h3MaxFrames, snapFrames, vocabFor } from './engine';
import { parseH3Block } from './h3-block';

/**
 * What one render is CALLED on screen — `"shot"`.
 *
 * ⚠️ Display only, and read off the capability record so the word has one home. Everything in this
 * module keeps its name: a `PackedGeneration` is still a generation in the code and on disk. Only
 * the sentences {@link mergeBlockedReason} hands the UI use it. See `EngineVocab`.
 */
const UNIT = vocabFor('h3').renderUnit;

const H3 = capabilities('h3');

/**
 * 22 frames = 0.917 s. The video VAE's own DECODABLE minimum, and the only real floor per RENDER.
 *
 * Below it a 2-latent block decodes 0 chunks and the decoder raises `decoded 0 frames, expected 5`
 * — so 5 frames genuinely fails, and 22 (= 5 + 17·1) is the first length that does not.
 */
export const H3_MIN_FRAMES = H3.minFrames ?? 22;
/**
 * 107 frames = 4.458 s. **Advisory, and not a floor** — the grid-snap of MiniMax's documented
 * 4-second *duration* guideline ("Output duration | 4-15 seconds").
 *
 * 🔴 This was enforced as a hard clamp until 2026-08-07, in five places, and the effect was
 * silent: a 2-second authored beat became a 4.458-second render, `generationFrames` lengthened it
 * and the last shot absorbed the difference, with no error anywhere. WanGP hardened it into a
 * `raise` at `pipeline.py:296`; nothing else agrees — ComfyUI's floor is 5, diffusers raises only
 * below 1, and our own `h3_short_clips.py` renders 22-frame clips.
 *
 * It is now what it always was: the edge of the range MiniMax validated. A generation under it
 * renders, and says so once (`belowDocumentedFloor`).
 */
export const H3_DOCUMENTED_MIN_FRAMES = 107;
/** 362 frames = 15.083 s = 5 + 17·21. On-grid, and equal to WanGP's own `window_default`. */
export const H3_MAX_FRAMES = H3.maxFrames ?? 362;
/** The budget, in the unit the timeline is drawn in. */
export const H3_MAX_SECONDS = H3_MAX_FRAMES / H3_FPS;
export const H3_MIN_SECONDS = H3_MIN_FRAMES / H3_FPS;
/** MiniMax's documented shortest output, in the unit the timeline is drawn in. */
export const H3_DOCUMENTED_MIN_SECONDS = H3_DOCUMENTED_MIN_FRAMES / H3_FPS;

/**
 * The only reference-tail lengths the engine handles honestly: `5 + 17k`, inside **56..345**.
 *
 * Two separate rules meet here and both bite:
 *
 * - **On the grid.** Off-grid is not rejected, it is silently truncated — `_add_video_reference`
 *   keeps the **first** frames and discards the rest, so an off-grid tail is shortened and the
 *   model is then told a lie about what moment it is looking at. A wrong render with no error.
 * - **2–15 seconds.** `validate_generative_settings:274-275` rejects any reference video outside
 *   that range outright. This is what rules 39 out: it is perfectly on-grid and it is 1.63 s, so
 *   the editor used to offer a tail koblem refuses. The ceiling is 345 (14.375 s), not 107 —
 *   an earlier draft of API.md had both wrong and has since been corrected.
 *
 * Authority: `docker/h3-wangp/app/config.py: H3_LEGAL_TAILS`, mirrored by koblibs'
 * `h3_legal_tails()` / `h3_is_legal_tail()`. Generated rather than listed so the two cannot drift
 * by a typo.
 */
export const H3_LEGAL_TAIL_FRAMES: number[] = (() => {
  const out: number[] = [];
  for (let f = 56; f <= 345; f += 17) out.push(f);
  return out;
})();

/** Is this a tail the engine will take at face value? */
export const isLegalTail = (frames: number): boolean => H3_LEGAL_TAIL_FRAMES.includes(frames);

/**
 * A continuation with every number filled in. `from` is absent by construction: it is added at
 * compile time (always `"prev"`) and never stored on the document, where position already says it.
 */
export type ResolvedH3Continuation = Required<Omit<DirectorH3Continuation, 'from'>> &
  Pick<DirectorH3Continuation, 'from'>;

/**
 * What a boundary does when the author has not chosen.
 *
 * `latents` because it is the best seam measured so far (93% of a normal frame step) and it is what
 * the plan specifies as the default; `k=7` likewise. None of it is settled — the contract
 * deliberately does not hard-code a winner, and the owner is still converging the mechanism.
 *
 * 🔴 THREE FIELDS SHORTER THAN IT WAS. `tail_frames: 56`, and koblem's `drop_guide` /
 * `posterior_noise`, were WanGP continuation fields. The ComfyUI backend sets the carry's depth
 * from `k` alone, has no video guide, and takes a mix factor rather than an additive sigma — so
 * every one of them reached the engine only to be answered with a warning, which the Director
 * showed the author on a render that was fine. A saved project that still holds them loads and
 * compiles; they just stop going out.
 */
export const H3_CONTINUATION_DEFAULT: ResolvedH3Continuation = {
  mode: 'latents',
  k: 7,
  anchor: true
};

/**
 * Why a render starts where it does.
 *
 * Only two answers survive one-clip-per-render: it is the opening shot, or the author made a clip
 * boundary. `checkpoint` and `budget` described a packer that decided this for you and no longer
 * exists.
 */
export type H3BreakReason = 'first' | 'forced';

/** One shot as it sits INSIDE its generation. */
export interface PackedShot {
  clip: DirectorClip;
  /** Position in the whole timeline — what the inspector and the drag handlers index by. */
  rawIndex: number;
  /**
   * Position among the shots that actually render (a prompt-less clip renders nothing).
   *
   * The index space every reference `segments` scope is expressed in, so scope lookups must use
   * this and never `rawIndex` — see `h3RefsForShot`.
   */
  renderedIndex: number;
  /** Position inside this generation. 0 is `[Shot 1]`, which is untimed. */
  shotIndex: number;
  /**
   * The cut time this shot's `[Shot N]` tag carries, relative to the START OF ITS GENERATION —
   * `null` for the first shot in it. Derived from the pack, never stored: it moves whenever a
   * preceding shot is trimmed, and a stored copy would go stale silently.
   *
   * 🔴 NOT snapped to the frame grid, and deliberately. A cut is a sentence in the prompt, not a
   * VAE boundary — the only rules on it are strictly-increasing, inside the duration, and
   * `MM:SS.mmm`. Rounding cut times onto 17n+5 (the mistake the LTX arithmetic invites) would move
   * every cut by up to 0.7 s for no reason the model cares about.
   */
  cutAtSeconds: number | null;
  /**
   * How long this shot actually plays.
   *
   * Identical to the authored length for every shot but the last, which absorbs the generation's
   * snap-up: a generation's frames round UP onto 17n+5, and that extra time has to land
   * somewhere. It lands at the end, because that is where a longer render puts it. This is the
   * number the timeline shows per shot (G8) — showing the authored one leaves the last shot of
   * every generation quietly wrong.
   *
   * 🔴 It used to absorb the 107-frame floor as well, which is how a 2-second authored beat
   * became a 4.458-second render with the whole 2.4 s dumped on the last shot. The floor is now
   * the VAE's decodable minimum, so at ordinary lengths this only ever absorbs the grid step
   * (at most 16 frames, 0.7 s).
   */
  renderedSeconds: number;
}

/** One render unit, ready to draw and ready to compile. */
export interface PackedGeneration {
  id: string;
  /** The authored sections. Empty (all fields absent) until somebody writes them. */
  doc: DirectorH3Generation;
  shots: PackedShot[];
  /** Why this generation starts here — the label the timeline boundary shows. */
  breakReason: H3BreakReason;
  /** The sum of the authored shot lengths, before the frame grid gets involved. */
  authoredSeconds: number;
  /** What it will ACTUALLY render: snapped UP onto 17n+5, then clamped into 22..362. */
  frames: number;
  seconds: number;
  /** The checkpoint this lands on, derived from what is in scope. */
  task: H3Task;
  /** In scope for this generation, IN PROJECT ORDER — position assigns each ordinal. */
  references: DirectorH3Reference[];
  /** This render is longer than the budget a single call can hold. */
  overBudget: boolean;
  /**
   * Shorter than MiniMax's documented 4-second minimum output.
   *
   * 🔴 **This no longer means the render is lengthened.** It used to: the field was `belowFloor`,
   * and anything under 107 frames was silently pushed up to 4.458 s. It now means only that the
   * generation is outside the duration range MiniMax document — it renders at the authored
   * length, and the editor says so once instead of correcting it.
   *
   * ⚠️ The ENGINE still has to be built for it: WanGP's `pipeline.py:296` raises under 4.0 s
   * unless the container runs `h3_short_clips`. Where it does not, the shim raises the length to
   * 107 and reports that in the plan warnings, naming the switch — never in silence.
   */
  belowDocumentedFloor: boolean;
  /** How this generation joins the previous one. `null` on the first — nothing precedes it. */
  continuation: ResolvedH3Continuation | null;
}

/**
 * What the timeline draws for a generation when nobody has written its gist.
 *
 * 🔴 The FIRST beat's, and only the first beat's. A generation is up to ~15 s holding several
 * beats, and concatenating them produces a paragraph the band has no room for — while the opening
 * beat is what the render opens on and is the closest thing to a title the document already holds.
 * `clip.gist` before `clip.prompt` because the gist is already a summary; the prompt is schema-ish
 * prose and reads badly clipped to one line.
 *
 * ⚠️ Empty string, not a dash — every caller has its own idea of what "nothing" should look like.
 */
export function h3GenerationGistFallback(gen: PackedGeneration): string {
  const clip = gen.shots[0]?.clip;
  // ⚠️ The block's opening beat, not `clip.prompt` — that field is empty on H3 now that the prose
  // lives only in the block, so reading it would show nothing for every shot written here.
  return (
    clip?.gist?.trim() ||
    parseH3Block(gen.doc.prompt ?? '').shots[0]?.body.trim() ||
    clip?.idea?.trim() ||
    ''
  );
}

/**
 * The label for one RENDER — what the timeline band and the shot list row show.
 *
 * 🔴 `DirectorH3Generation.gist` is **display only and never compiled into the request**; see its
 * doc in `client.ts`. It is the gist of the ENTIRE prompt, i.e. of the whole shot, which is the
 * unit that renders, delivers and retakes — not of any one beat inside it.
 *
 * The fallback exists so a project written before the field did shows something the moment it is
 * opened, rather than a row of blank bands until somebody presses Summarise on each one.
 */
export function h3GenerationGist(gen: PackedGeneration): string {
  return gen.doc.gist?.trim() || h3GenerationGistFallback(gen);
}

/**
 * A checkpoint flip costs this, measured: 31.9 s reload + 32.5 s of extra first step.
 *
 * Named because it is the number the frame-pin control has to say out loud. A 13-generation music
 * video that alternates pinned and referenced generations buys thirteen of these — around thirteen
 * minutes of pure reload on a 25-minute render — and nothing else in the editor is in a position
 * to notice.
 */
export const H3_CHECKPOINT_FLIP_SECONDS = 64;

/** Has this shot got a first/last frame image attached at all, whichever way it is used? */
export function h3HasFrameImage(c: DirectorClip): boolean {
  return !!((c.mode === 'i2v' && c.image_b64) || c.h3_end_image);
}

/**
 * How this shot's attached frames reach the model. See `DirectorClip.h3_frame_mode`.
 *
 * 🔴 Absent means `pin`, and it is resolved HERE rather than from the project's soundtrack. The
 * song-aware default belongs at ATTACH time, in the editor, where it is written into the document:
 * a default that read the soundtrack would change what an existing project renders the moment a
 * track was attached, with nothing on screen moving.
 */
export function h3FrameMode(c: DirectorClip): 'pin' | 'keyframe' {
  return c.h3_frame_mode === 'keyframe' ? 'keyframe' : 'pin';
}

/**
 * Does this shot **wire-pin** a frame — i.e. is it on `fl2va` rather than `ref2va`/`t2va`?
 *
 * 🔴 An attached image is no longer enough. Since 2026-08-08 the same image can instead be declared
 * as a `ref2va` **keyframe completion** (`h3_frame_mode: 'keyframe'`), which reaches the model as a
 * `<Picture N>` and prose rather than as `first_frame`/`last_frame` on the other checkpoint. That
 * path measures 0.776 frame-0 NCC against the wire pin's 0.968 — but it keeps every reference AND
 * the song slice, and it costs no {@link H3_CHECKPOINT_FLIP_SECONDS} flip.
 *
 * Everything downstream keys off THIS function: the `checkpoint` break reason, `taskFor`,
 * `referencesForGeneration`'s refusal, the compiler's `first_frame`/`last_frame` fields, and the
 * capability guard in `engine.ts`. So a keyframe-mode shot is not isolated, not stripped of
 * references, and not emitted as a pin — one edit, five behaviours, which is the point of it being
 * one function.
 */
export function pinsAFrame(c: DirectorClip): boolean {
  return h3HasFrameImage(c) && h3FrameMode(c) === 'pin';
}

/**
 * The images this shot hands to `ref2va` as standalone `<Picture N>` keyframes, in emission order.
 *
 * Empty unless the shot is in `keyframe` mode. `first` before `last` because that is the order the
 * ordinals should read in and the order the guide's own examples list them.
 */
export function h3KeyframeImages(c: DirectorClip): { edge: 'first' | 'last'; image: string }[] {
  if (!h3HasFrameImage(c) || h3FrameMode(c) !== 'keyframe') return [];
  const out: { edge: 'first' | 'last'; image: string }[] = [];
  if (c.mode === 'i2v' && c.image_b64) out.push({ edge: 'first', image: c.image_b64 });
  if (c.h3_end_image) out.push({ edge: 'last', image: c.h3_end_image });
  return out;
}

/** The reference id a keyframe image is synthesised under. Stable, so scope and ordinals are too. */
export const h3KeyframeRefId = (clipId: string, edge: 'first' | 'last'): string =>
  `kf:${clipId}:${edge}`;

/**
 * Turn a generation's keyframe-mode images into references the ordinal assigner can number.
 *
 * 🔴 **Appended, never prepended.** A reference's position assigns its ordinal AND advances a shared
 * rotary clock, so inserting a synthesised `<Picture 1>` at the front would renumber every picture
 * the prompt already names — silently, on the render after the author ticked a checkbox. Appending
 * gives the keyframe the next free number and leaves every existing sentence pointing where it did.
 *
 * ⚠️ These are **not** in `h3References`, so `reference_ids` cannot scope them and does not need to:
 * a keyframe belongs to exactly one shot in exactly one generation by construction.
 */
export function h3KeyframeReferences(shots: PackedShot[]): DirectorH3Reference[] {
  return shots.flatMap((s) =>
    h3KeyframeImages(s.clip).map((k): DirectorH3Reference => ({
      id: h3KeyframeRefId(s.clip.id, k.edge),
      kind: 'image',
      image: k.image,
      // 🔴 `frame_anchor`, NOT `first_frame`/`last_frame`. Those two are the FL2VA wire-pin roles
      // — `engine_role()` sends them as `first_frame`, `is_keyframe()` is true for them, and
      // `contract.py:649` REFUSES a generation carrying a keyframe alongside any omni reference
      // ("different CHECKPOINTS, not flags"). This function only ever runs for a `keyframe`-mode
      // shot, which is the path that deliberately stays on ref2va, so labelling its picture with
      // the pin role made the cheap path behave as the expensive one and put it in direct
      // conflict with the project's own cast references.
      //
      // `frame_anchor` maps to `reference` on the wire — an ordinary omni `<Picture N>`, which is
      // what the comment on this function has always said it appends. The first/last distinction
      // is not lost: it was never carried by the role (all three map to the same
      // `keyframe completion` task type), it is carried by the PROSE declaration and the label.
      role: 'frame_anchor',
      // Read by the composer's pills and by nothing on the wire — the model addresses a reference
      // by its ordinal alone.
      label: `[Shot ${s.shotIndex + 1}] ${k.edge} frame`
    }))
  );
}

/**
 * A clip renders only if it has a prompt. Blank clips are placeholders, not shots.
 *
 * ⚠️ On H3 the prompt is the generation's BLOCK, not `clip.prompt` — a shot whose block was written
 * in the composer has an empty `clip.prompt` and renders perfectly well. Callers there pass
 * {@link h3BlockedClipIds} as `written`; the bare form is the LTX test.
 */
export const clipRenders = (c: DirectorClip, written?: ReadonlySet<string>): boolean =>
  c.prompt.trim().length > 0 || !!written?.has(c.id);

/**
 * Every clip id covered by a stored generation that actually holds a block.
 *
 * Computed from `stored` rather than from the packed result because {@link clipRenders} is what
 * DECIDES `renderedIndex`, so it runs before a generation's document has been adopted.
 */
export const h3BlockedClipIds = (stored: readonly DirectorH3Generation[]): Set<string> => {
  const out = new Set<string>();
  for (const g of stored) {
    if (!g.prompt?.trim()) continue;
    for (const id of g.clip_ids ?? []) out.add(id);
  }
  return out;
};

/**
 * Frames for a whole generation: snap UP onto the grid, then clamp into the engine's bounds.
 *
 * 🔴 The clamp belongs HERE and nowhere else. These bounds are per RENDER, and a render is a
 * generation — a 2.5 s shot is perfectly legal as one cut inside a longer generation. Applying
 * them per shot (which is what a naive port of the LTX arithmetic does) rejects exactly the
 * multi-shot packing that makes this engine worth using.
 *
 * 🔴 The low bound is 22 frames (the VAE's decodable minimum), not 107. While it was 107 this
 * function LENGTHENED every generation under 4.458 s without saying a word.
 */
export function generationFrames(seconds: number, maxFrames: number = H3_MAX_FRAMES): number {
  // `snapFrames` carries the clamp itself, matching koblibs' `h3_snap_frames` — one definition of
  // "what will this render at", shared by the editor and the server.
  const snapped = snapFrames('h3', Math.round(Math.max(0, seconds) * H3_FPS));
  // 🔴 The second clamp is the SOUNDTRACK cap, and it can only ever be tighter than the spec one
  // `snapFrames` already applied — so it narrows and never widens. A project carrying a song packs
  // to 345 frames rather than 362 so a generation is never longer than the audio slice it is
  // allowed to carry; see `H3_MAX_FRAMES_WITH_AUDIO`. `Math.min` rather than a re-snap because
  // `maxFrames` is always itself on the grid.
  return Math.min(snapped, maxFrames);
}

/** The snap WITHOUT the clamp, for asking "would this fit?" rather than "what renders?". */
function rawSnap(seconds: number): number {
  const frames = Math.round(Math.max(0, seconds) * H3_FPS);
  return frames <= 5 ? 5 : 5 + Math.ceil((frames - 5) / 17) * 17;
}

/** The duration a generation will actually render at. */
export const generationSeconds = (seconds: number): number => generationFrames(seconds) / H3_FPS;

/**
 * The references in scope for a whole generation, in project order. Narrows; never reorders.
 *
 * A generation-level `reference_ids` wins outright when present, because per-generation is the
 * granularity the engine actually has: `pipeline.py:311-340` builds ONE reference presentation per
 * call, so a scope that varies shot-to-shot inside a generation describes something the
 * conditioner cannot do. Absent, it falls back to the union of the per-shot `segments` scopes,
 * which is what every document written before generations existed carries.
 *
 * 🔴 `segments` is READ here and is no longer WRITTEN anywhere. The shipped control until
 * 2026-08-08 was `ReferencesPanel`'s numbered chip row, which scoped by rendered-SHOT index — a
 * granularity the conditioner does not have — and meanwhile `reference_ids`, which is the
 * granularity it does have, had no writer at all. The scope control now lives on the generation
 * card and writes `reference_ids`; this fallback exists only so an old document keeps rendering
 * what it always did.
 *
 * ⚠️ Absent scope and empty scope are different states and mean opposite things — `undefined` is
 * every shot, `[]` is no shot. Collapsing them turns "this reference is for nobody" into "this
 * reference is for everybody".
 */
export function referencesForGeneration(
  refs: DirectorH3Reference[],
  shots: PackedShot[],
  scoped: string[] | undefined
): DirectorH3Reference[] {
  // 🔴 A pinned generation renders on `fl2va`, which takes no omni references at all — different
  // weight set, not a flag. A project-wide reference (absent scope = every shot) would otherwise
  // drift into it and flip the whole generation to `ref2va`, quietly throwing the frame pin away.
  // The pin wins, because it is the thing the author placed by hand.
  if (shots.some((s) => pinsAFrame(s.clip))) return [];
  if (scoped !== undefined) {
    const want = new Set(scoped);
    return refs.filter((r) => want.has(r.id));
  }
  return refs.filter(
    (r) => r.segments === undefined || shots.some((s) => r.segments!.includes(s.renderedIndex))
  );
}

/**
 * The `reference_ids` to store after the author toggles one reference on the generation card.
 *
 * 🔴 The first toggle **materialises** the scope. Until then it is `undefined`, meaning "whatever
 * the reference list says" — and turning one reference off has to leave the other nine exactly
 * where they were, not reset them to the project default. So the caller passes the EFFECTIVE set
 * (what `referencesForGeneration` currently returns) and gets an explicit list back.
 *
 * 🔴 The result is re-derived by filtering `project`, which does two load-bearing jobs at once: it
 * keeps the list in PROJECT ORDER (position assigns the ordinal and advances a shared rotary clock,
 * so a scope in a different order is a different render), and it drops any id that is not a real
 * project reference — synthesised keyframe pictures live only inside one generation and must never
 * be written into a stored scope.
 *
 * ⚠️ An empty result is `[]` and means NO references. That is a legal, reachable state and it is
 * NOT the same as `undefined`; the caller must store it as-is rather than collapsing it.
 */
export function h3ScopeAfterToggle(
  project: DirectorH3Reference[],
  effective: readonly string[],
  id: string
): string[] {
  const want = new Set(effective);
  if (want.has(id)) want.delete(id);
  else want.add(id);
  return project.filter((r) => want.has(r.id)).map((r) => r.id);
}

/**
 * Which checkpoint a generation lands on.
 *
 * Follows the emitter spec's selection rule exactly: references win, then frame pins, then plain
 * text. It is a derivation and not a choice, because the two checkpoints are disjoint weight sets
 * — there is no request that is "a bit of both", and offering the user a picker would be offering
 * a state that cannot render.
 */
export function taskFor(shots: PackedShot[], references: DirectorH3Reference[]): H3Task {
  if (references.length > 0) return 'ref2va';
  return shots.some((s) => pinsAFrame(s.clip)) ? 'fl2va' : 't2va';
}

export interface PackInput {
  clips: DirectorClip[];
  /** The project's references, in order. */
  references: DirectorH3Reference[];
  /** Sections a human already wrote, from the saved document. */
  stored?: DirectorH3Generation[];
  /**
   * Does this project carry a song?
   *
   * Tightens the generation ceiling from 362 frames to 345 so a generation is never longer than the
   * 15.0 s audio reference it has to be scored by — see {@link H3_MAX_FRAMES_WITH_AUDIO}. Packing
   * is where it has to happen: a cap applied later could only report the overflow, and by then the
   * author has already been shown boundaries that cannot carry their audio.
   */
  hasSoundtrack?: boolean;
}

/**
 * Pack the timeline into generations.
 *
 * One group per clip. Every boundary is one the author placed by making a clip, so there is nothing
 * here to optimise and nothing that can move a seam underneath them.
 */
export function packGenerations(input: PackInput): PackedGeneration[] {
  const { clips, references, stored = [], hasSoundtrack = false } = input;
  // The ceiling in force for THIS project. Every budget test below reads it rather than the
  // module constant, so a song-bearing project packs to 345 and one without keeps 362.
  const maxFrames = h3MaxFrames(hasSoundtrack);
  const groups: { shots: PackedShot[]; reason: H3BreakReason }[] = [];
  let renderedIndex = 0;
  const written = h3BlockedClipIds(stored);

  for (let raw = 0; raw < clips.length; raw++) {
    const clip = clips[raw];
    const shot: PackedShot = {
      clip,
      rawIndex: raw,
      renderedIndex: clipRenders(clip, written) ? renderedIndex++ : -1,
      shotIndex: 0,
      cutAtSeconds: null,
      renderedSeconds: Math.max(0, clip.seconds)
    };
    groups.push({ shots: [shot], reason: groups.length === 0 ? 'first' : 'forced' });
  }

  const claimed = new Set<number>();
  // 🔴 Uniqueness of the generation ID is enforced HERE because nothing upstream can promise it —
  // see {@link dedupeH3GenerationIds} for the three ways a store comes to hold two entries under one
  // id, and for the migration that repairs the store itself. This is the last line of defence: every
  // consumer keys on this id (`updateGeneration`, `dropGeneration`, the takes, the bank key, a keyed
  // `{#each}`), so a duplicate reaching a `PackedGeneration` routes one shot's write into another
  // shot's document.
  const usedIds = new Set<string>();
  return groups
    .map((g) => {
      const authoredSeconds = g.shots.reduce((t, s) => t + Math.max(0, s.clip.seconds), 0);
      const frames = generationFrames(authoredSeconds, maxFrames);
      let at = 0;
      g.shots.forEach((s, i) => {
        s.shotIndex = i;
        // `[Shot 1]` is untimed; later shots carry the offset of the cut inside this generation,
        // rounded to the millisecond the `MM:SS.mmm` tag can express and no further.
        s.cutAtSeconds = i === 0 ? null : Math.round(at * 1000) / 1000;
        at += Math.max(0, s.clip.seconds);
        // The last shot absorbs the whole snap-up (and the floor), because a longer render puts the
        // extra time at the end. Clamped at 0 so an over-budget generation reports a truncated last
        // shot rather than a negative one.
        s.renderedSeconds =
          i === g.shots.length - 1
            ? Math.max(0, Math.max(0, s.clip.seconds) + (frames / H3_FPS - authoredSeconds))
            : Math.max(0, s.clip.seconds);
      });
      const doc = adoptStored(g.shots, stored, claimed, usedIds);
      // 🔴 The keyframes go on the END, after scoping. Two reasons, both silent when wrong:
      // position assigns the ordinal (so prepending renumbers every picture the prompt already
      // names), and `referencesForGeneration` returns `[]` for a WIRE-PINNED generation — a
      // keyframe-mode shot is not wire-pinned, so its picture must survive that refusal.
      const refs = [
        ...referencesForGeneration(references, g.shots, doc.reference_ids),
        ...h3KeyframeReferences(g.shots)
      ];
      const over = rawSnap(authoredSeconds) > maxFrames;
      return {
        id: doc.id,
        doc,
        shots: g.shots,
        breakReason: g.reason,
        authoredSeconds,
        frames,
        seconds: frames / H3_FPS,
        task: taskFor(g.shots, refs),
        references: refs,
        overBudget: over,
        belowDocumentedFloor: rawSnap(authoredSeconds) < H3_DOCUMENTED_MIN_FRAMES,
        continuation: null
      };
    })
    .map((gen, i) => ({
      ...gen,
      // Only a boundary can carry one: the first generation has nothing before it to continue from,
      // and the wire contract omits the field there rather than sending `mode: "none"`.
      continuation: i === 0 ? null : resolveContinuation(gen.doc.continuation)
    }));
}

/** Fill a partial stored continuation out to the defaults, so every consumer reads real numbers. */
export function resolveContinuation(c: DirectorH3Continuation | undefined): ResolvedH3Continuation {
  return {
    mode: c?.mode ?? H3_CONTINUATION_DEFAULT.mode,
    k: c?.k ?? H3_CONTINUATION_DEFAULT.k,
    anchor: c?.anchor ?? H3_CONTINUATION_DEFAULT.anchor,
    // 🔴 `tail_frames`, `drop_guide` and `posterior_noise` are NOT carried through, even when an
    // old document still holds them. The compiler spreads this object onto the wire, so passing
    // them through here is exactly what used to put them in front of the engine — which answered
    // with a warning per field, per continued shot, on a render that was correct.
    // 🔴 Carried through, not overwritten. The compiler used to force `from: 'prev'` onto every
    // generation, which made an explicit latent-bank key UNREACHABLE from the editor even though
    // both koblem and the shim have always honoured one: `h3_single_generation_chain` forwards a
    // non-`prev` key verbatim ("not the shim's to resolve, and not ours to second-guess"), and
    // `Continuation.from` takes a bank key precisely so a generation can pin to something other
    // than its immediate predecessor. Absent still compiles to `'prev'` — see `compileH3Timeline` —
    // so a document that never set one is byte-identical.
    ...(c?.from?.trim() ? { from: c.from.trim() } : {})
  };
}

/**
 * Re-attach the sections a human wrote to the generation they were written for.
 *
 * Matched by the largest overlap of clip ids, each stored entry claimable once. Trimming a shot
 * must not orphan a 400-word `detailed_description`, and adding a shot in the middle must not hand
 * the second half of the timeline somebody else's soundscape — overlap gets both right without
 * needing the old packing to be trustworthy.
 *
 * See {@link h3MintedId} for the id a generation with nothing stored gets, and the `usedIds` note in
 * {@link packGenerations} for why the id that comes back is not always the one that was adopted.
 */
function adoptStored(
  shots: PackedShot[],
  stored: DirectorH3Generation[],
  claimed: Set<number>,
  usedIds: Set<string>
): DirectorH3Generation {
  const ids = new Set(shots.map((s) => s.clip.id));
  let bestAt = -1;
  let bestScore = 0;
  stored.forEach((s, i) => {
    if (claimed.has(i)) return;
    const score = (s.clip_ids ?? []).filter((id) => ids.has(id)).length;
    if (score > bestScore) {
      bestScore = score;
      bestAt = i;
    }
  });
  const mint = h3MintedId(shots[0]?.clip.id);
  const fresh: DirectorH3Generation = { id: mint, clip_ids: shots.map((s) => s.clip.id) };
  let doc = fresh;
  if (bestAt >= 0) {
    claimed.add(bestAt);
    doc = { ...stored[bestAt], clip_ids: fresh.clip_ids };
  }
  // 🔴 **The id is MINTED, never inherited — even when an entry was adopted.** Adoption carries the
  // WRITING (`prompt`, the sections, the LoRAs); it must not carry the entry's id, because an id
  // says which clip a generation opens on and the adopted entry was written for a different packing.
  // Inheriting it is what divorced the id from the clips, and a divorced id is what let two
  // generations end up under one: see {@link canonicalH3Store}.
  //
  // `uniqueId` is therefore a belt on a fastened seatbelt: with one clip per generation, `gen:<clip>`
  // is unique because clip ids are. It stays because the grouping rule has changed once already, and
  // a future multi-shot group would put two clips behind one mint again.
  return { ...doc, id: uniqueId(mint, mint, usedIds) };
}

/**
 * The id a generation gets when nothing was written for it: derived from its first clip rather than
 * random, so re-packing an unchanged timeline is idempotent and the tests can assert on it.
 */
export const h3MintedId = (firstClipId: string | undefined): string =>
  `gen:${firstClipId ?? 'empty'}`;

/** `want` if it is free, else `fallback`, else `fallback#2`, `#3`… Deterministic; records the winner. */
function uniqueId(want: string, fallback: string, used: Set<string>): string {
  let id = used.has(want) ? fallback : want;
  for (let n = 2; used.has(id); n++) id = `${fallback}#${n}`;
  used.add(id);
  return id;
}

/**
 * **The generation store, as it must always look: the packing's own documents.**
 *
 * 🔴 This is THE invariant, and it is the airtight form of a bug that was only guarded before. Every
 * generation's id is `gen:<the clip it opens on>` ({@link h3MintedId}), minted by the packer and never
 * inherited from whatever was on disk — so an id can no longer come apart from the clips it names,
 * and two generations cannot end up under one id unless two CLIPS share an id, which the clip layer
 * does not allow. The guards ({@link dedupeH3GenerationIds}, the `usedIds` pass) are now belts on a
 * fastened seatbelt rather than the thing holding it together.
 *
 * The rule it enforces: **what is held in memory is what would be saved.** The save path has always
 * written the derived docs (`packedGenerations.map(g => g.doc)`, pruned) rather than the array it was
 * given, which is why the store's `clip_ids` track the live packing. The load path did NOT, so a
 * document could sit in memory with ids that the packer no longer mints — and then the first write to
 * such a generation appends a SECOND entry for the same clips (`updateGeneration` appends when it
 * cannot resolve an id), whose content the adoption tie-break never picks. The write appears to save
 * and comes back missing.
 *
 * One definition, called from both ends, so load and save cannot produce different bytes.
 *
 * ⚠️ Entries nothing adopted are dropped — a generation whose clips are all gone. That is not new
 * behaviour, it is the `filter` the save has always applied; doing it at load too is what makes the
 * two agree.
 */
export const canonicalH3Store = (gens: readonly PackedGeneration[]): DirectorH3Generation[] =>
  gens
    .map((g) => g.doc)
    // id + clip_ids and nothing else: a generation nobody has written for. It is re-derived on the
    // next pack, so storing it says nothing and it would only accumulate.
    .filter((g) => Object.keys(g).length > 2);

/**
 * Repair a stored list that holds two entries under one id — **the store, not the packing.**
 *
 * 🔴 Found in production 2026-08-19: a 16-shot project whose last two generations both claimed
 * `gen:03d5ee27…`, each with its own 5 KB block. Nothing showed it. Both blocks were adopted
 * correctly (each stored entry is claimable once, so the two shots displayed the right text), but
 * `updateGeneration` resolves an id with `findIndex` — so every subsequent edit to the SECOND shot
 * was going to be written into the FIRST one's entry, and the loser's block would come back on the
 * next load.
 *
 * The store can arrive in that state three ways and none of them is guarded upstream:
 * `adoptStored` returns the stored id while attaching a DIFFERENT clip set (so an entry minted from
 * clip B can end up holding clip A, and B's own generation then mints `gen:B` afresh), `putGeneration`
 * writes whatever document the chat layer hands it, and an `aiRestore` blob is JSON from another
 * session entirely.
 *
 * Renaming rather than dropping, because both entries hold real writing. The new id is the MINTED
 * form of the entry's own first clip — the same rule {@link packGenerations} uses — so the renamed
 * entry is the one its own generation adopts and writes back to.
 *
 * ⚠️ Returns `null` when the list is already unique, so a caller can tell "repaired" from "fine" and
 * opening a healthy project does not mark it dirty.
 *
 * 🔴 Since {@link canonicalH3Store}, this is a GUARD rather than the fix: the packer no longer
 * inherits ids, so nothing new can be written duplicated. It stays for the two cases the canonical
 * form cannot reach — a row written by an older build, and a restore point that is JSON in
 * `localStorage` from whichever build captured it — because both are installed into the store before
 * anything has re-packed, and `updateGeneration` resolves an id by `findIndex`.
 */
export function dedupeH3GenerationIds(
  stored: readonly DirectorH3Generation[]
): DirectorH3Generation[] | null {
  const used = new Set<string>();
  let repaired = false;
  const out = stored.map((g) => {
    if (!used.has(g.id)) {
      used.add(g.id);
      return g;
    }
    repaired = true;
    return { ...g, id: uniqueId(g.id, h3MintedId(g.clip_ids?.[0]), used) };
  });
  return repaired ? out : null;
}

/**
 * May the boundary before `gen` be dissolved — i.e. can these two generations become one?
 *
 * Returns the reason it cannot, or `null` when it can. A wire-pinned shot can never be merged with
 * a neighbour (different weight sets), and neither can a pair whose combined length passes what one
 * render can hold.
 */
export function mergeBlockedReason(
  prev: PackedGeneration | undefined,
  gen: PackedGeneration,
  hasSoundtrack = false
): string | null {
  if (!prev) return `This is the first ${UNIT} — there is nothing before it to merge into.`;
  // 🔴 Read off the CLIPS, not off `breakReason`. The packer no longer decides boundaries, so a
  // checkpoint clash is a property of what the two shots pin rather than of why they are apart.
  if (pinsAFrame(gen.shots[0]?.clip) || pinsAFrame(prev.shots[0]?.clip)) {
    return `These render on different checkpoints. A ${UNIT} that pins a first or last frame runs on fl2va; references run on ref2va. They cannot share one render.`;
  }
  const total = prev.authoredSeconds + gen.authoredSeconds;
  // 🔴 The same ceiling `packGenerations` used, or this offers a merge the packer would immediately
  // split again — the author clicks Merge, the boundary comes straight back, and nothing says why.
  const maxFrames = h3MaxFrames(hasSoundtrack);
  if (rawSnap(total) > maxFrames) {
    const cap = (maxFrames / H3_FPS).toFixed(2);
    return (
      `Together these run ${total.toFixed(1)}s, past the ${cap}s a single ${UNIT} can render` +
      (hasSoundtrack
        ? ` while the project carries a soundtrack (an audio reference caps at 15.0s, so ${UNIT}s pack to 345 frames rather than 362).`
        : '.') +
      ' Shorten one first.'
    );
  }
  return null;
}

/**
 * Fold every pre-collapse generation's beat-clips into ONE clip.
 *
 * ## Why this exists
 *
 * H3 stored a beat as its own `DirectorClip` and made the render unit a *pack* of them, which
 * inverted the vocabulary used everywhere else in the product: a shot is a separately rendered video
 * with its own prompt, and a beat is a timed event inside one. It also stored a beat's TEXT twice —
 * as the block's `[Shot N]` body and as `clip.prompt` — and its TIME twice — as `At MM:SS.mmm` and
 * as accumulated `clip.seconds`. Those two duplicates were the whole of "the timeline has not caught
 * up with the prompt".
 *
 * Nothing is lost, because almost nothing on a beat-clip ever reached the wire: `compileH3Timeline`
 * reads the block, the references, the frames, and exactly two images — the FIRST clip's
 * `image_b64` and the LAST clip's `h3_end_image`, which are the generation's `first_frame` and
 * `last_frame`. Both are preserved below. Every other per-clip field it consumes is LTX-only.
 *
 * 🔴 **Grouped by `stored[].clip_ids`, not by re-running the packer.** The packer now emits one
 * group per clip by construction, so it can no longer describe the old shape at all — but each
 * stored generation still lists the clips it was packed from, which is a RECORD of that shape
 * rather than a guess at it. A clip in no stored generation was never opened and has no block to
 * preserve, so it is left exactly as it is.
 *
 * 🔴 **The merged clip keeps the FIRST member's id.** `adoptStored` claims a stored generation by
 * clip-id OVERLAP, so the surviving id is what carries the authored block — the sections, the
 * soundscape, the prompt — across the collapse.
 *
 * Returns `null` when there is nothing to do, so a project already one-clip-per-shot is not marked
 * dirty merely by being opened.
 */
export function collapseH3Beats(input: PackInput): DirectorClip[] | null {
  const { clips, stored = [] } = input;
  const groupOf = new Map<string, number>();
  stored.forEach((g, i) => {
    if ((g.clip_ids ?? []).length < 2) return;
    for (const id of g.clip_ids ?? []) groupOf.set(id, i);
  });
  if (!groupOf.size) return null;

  const out: DirectorClip[] = [];
  let at = 0;
  let changed = false;
  while (at < clips.length) {
    const g = groupOf.get(clips[at].id);
    // ⚠️ Runs of ADJACENT members only. A stored `clip_ids` that no longer describes a contiguous
    // run has been reordered since it was written, and merging across the gap would swallow a shot
    // that was moved between them.
    let end = at + 1;
    if (g !== undefined) while (end < clips.length && groupOf.get(clips[end].id) === g) end++;
    const members = clips.slice(at, end);
    if (members.length === 1) {
      out.push(members[0]);
    } else {
      changed = true;
      const head = members[0];
      const last = members[members.length - 1];
      out.push({
        ...head,
        seconds: members.reduce((t, c) => t + Math.max(0, c.seconds), 0),
        h3_end_image: last.h3_end_image ?? null,
        // Every member's planned sound, not just the head's. These are the planner's intents and
        // the soundscape has to reconcile them, so dropping the tail would lose authoring intent
        // that nothing else records.
        h3_audio:
          members
            .map((c) => c.h3_audio?.trim())
            .filter(Boolean)
            .join(' ') || undefined,
        // The beat bodies live in the block and are read from it. Carrying the head's copy forward
        // would leave one beat's prose labelling the whole shot on the timeline.
        prompt: '',
        gist: head.gist ?? '',
        references: members.flatMap((c) => c.references ?? [])
      });
    }
    at = end;
  }
  return changed ? out : null;
}
