/**
 * Cutting one project song into the per-generation slices MiniMax-H3 conditions on.
 *
 * ## Why this exists
 *
 * H3 takes supplied audio — `audio_prompt_type="A"`/`"AB"` with `audio_guide`/`audio_guide2`
 * (`minimax_h3_handler.py`) — but it takes a FILE per generation, not a timeline. WanGP advertises
 * `audio_guide_window_slicing: True`, which sounds like it would do this for us, but Ref2VA sets
 * `sliding_window_size = video_length` (one window per generation) and validates duration on the
 * file, so a three-minute song is rejected on arrival. **The caller slices.** That is this module.
 *
 * Each slice is attached as a numbered `<Audio N>` omni reference with the `fully_copy` retention
 * marker, which is what makes performers sing the actual song rather than an invented bed.
 *
 * 🔴 **`fully_copy` does not copy.** It is a marker in `retention_analysis` declaring the
 * relationship; the output audio still comes out of the joint denoise, so what H3 returns is a
 * RECONSTRUCTION of the song. The real track is remuxed over the finished cut afterwards. Both
 * halves are needed and neither substitutes for the other — the conditioning is what syncs the
 * performance, the mux is what makes the deliverable listenable. This is exactly the split LTX
 * already runs (`ltx_video.rs:3016`: drive with the upload, then remux it, because "the model audio
 * is conditioning only").
 *
 * ## The arithmetic, and the two timelines it has to keep apart
 *
 * The easy mistake is to lay slice `i` at `sum(seconds[0..i])` into the song. That is wrong twice
 * over, and both errors accumulate along the cut in the same direction — which is the signature of
 * a soundtrack that starts in sync and is visibly late by the last chorus.
 *
 * 1. **Generations snap UP onto 17n+5.** A generation authored at 12.0 s renders at 12.125 s. The
 *    slice has to follow what RENDERS, never what was authored, or every boundary drifts by its own
 *    rounding.
 *
 * 2. **A seam eats a frame.** With the overlay anchor on, a continued generation reproduces the
 *    previous generation's last frame as its own frame 0, and the shim's `_assemble` trims that
 *    duplicate at delivery (`engine.py:1006`, `continuation.trim_join`, default 1). So the finished
 *    cut is one frame SHORTER per anchored seam than the sum of the generation lengths. Fifteen
 *    seams is 0.625 s — small per join, plainly audible by the end.
 *
 * So there are two clocks and a slice needs both:
 *
 * - **delivered time** — where a generation's first surviving frame lands in the finished cut.
 *   Advances by `frames - trim`. This is the clock the final remux follows.
 * - **song offset** — where the generation's frame 0 sits in the song. That frame is the one about
 *   to be trimmed, so it belongs one trim earlier than the generation's delivered start:
 *   `songOffset = deliveredAt - trim/fps`. The model is shown the whole generation including the
 *   frame that will not survive, so it has to hear that frame's audio too.
 */

import { H3_FPS, H3_AUDIO_REF, audioSliceSeconds } from './engine';
import type { PackedGeneration } from './h3-generations';
import type { DirectorH3Reference } from '$lib/api/client';

/**
 * The payload an unresolved soundtrack slice carries where an asset hash would go.
 *
 * Mirrors `H3_SOUNDTRACK_SENTINEL` in koblibs. koblem replaces it, per generation, with the
 * `asset:<hash>` of that generation's span of the song.
 */
export const H3_SOUNDTRACK_SENTINEL = 'soundtrack:';

/** The stable id the auto-managed soundtrack reference carries in the editor. */
export const H3_SOUNDTRACK_REF_ID = 'h3-project-soundtrack';

/**
 * The project's references WITH the soundtrack among them, when a song is attached.
 *
 * 🔴 **One definition, called by the composer AND by the compiler**, because they must agree about
 * the ordinal. `<Audio N>` numbers come from position in this list, so a soundtrack the compiler
 * appends but the composer never saw is one the prompt cannot address — and an unaddressed audio
 * reference conditions far more weakly than a declared one, silently.
 *
 * APPENDED, never prepended: a hand-attached reference's ordinal is what the author's prompt
 * already says, and inserting ahead of it would renumber every label they wrote.
 *
 * Idempotent — a list that already carries the soundtrack is returned unchanged, so calling it
 * twice (or on a document round-tripped through a save) cannot stack duplicates.
 */
export function withSoundtrackReference(
  refs: DirectorH3Reference[],
  hasSong: boolean
): DirectorH3Reference[] {
  if (!hasSong) return refs.filter((r) => r.id !== H3_SOUNDTRACK_REF_ID);
  if (refs.some((r) => r.id === H3_SOUNDTRACK_REF_ID)) return refs;
  return [
    ...refs,
    {
      id: H3_SOUNDTRACK_REF_ID,
      kind: 'audio',
      // The role that maps to the `fully_copy` retention marker — "the supplied signal IS the
      // target's final track", as opposed to a stylistic hint. `h3_prompting` emits its
      // `subject_definitions` entry and its retention line from this with no special-casing.
      role: 'audio_reuse',
      audio: H3_SOUNDTRACK_SENTINEL
    }
  ];
}

/**
 * Is the soundtrack in scope for this generation — i.e. will it get a slice?
 *
 * Ordinary reference scoping, deliberately: taking a shot out of the song is the same gesture as
 * taking it out of any other reference, and the Director already has that control. There is no
 * separate opt-out list to learn, to keep in sync, or to forget to migrate.
 */
export function generationWantsSlice(gen: PackedGeneration): boolean {
  return gen.references.some((r) => r.id === H3_SOUNDTRACK_REF_ID);
}

/**
 * The shim's `trim_join` default (`contract.py:260`).
 *
 * Mirrored rather than sent: `ResolvedH3Continuation` deliberately does not carry `trim_join`, so
 * koblem never transmits one and the shim applies its own default. That makes this a MIRROR of a
 * value we do not control — if the shim's default moves, every slice offset here is wrong by a
 * frame per seam and nothing will fail loudly. It is asserted against the plan echo in
 * `soundtrackDriftNote` instead.
 */
export const H3_TRIM_JOIN = 22;
// 🔴 1 -> 22 ON 2026-08-14, AND THIS FILE IS THE ONE THE UI DRAWS FROM. There are THREE
// copies of this number: comfy-api (the truth), koblibs `H3_TRIM_JOIN` (the render plan)
// and this one (`planSoundtrack` -> `timeline-layout.ts` -> the ruler, the waveform and
// the hover readout). Leaving this at 1 while the engine trims 22 does not fail — it
// silently draws every shot after the first in the wrong place, by 21 frames per seam,
// which is ~12 s of accumulated lie by the end of a 16-shot cut.
//
// Why it moved: comfy-api's continuation now defaults to `placement="on_target"`, which
// overlays the carry on the target's OWN first frames and trims them —
// `frames_for_latents(k)`, 22 at K=7 — instead of the single `frame0_anchor` frame.
//
// ⚠️ It is `frames_for_latents(7)` and nothing here enforces K=7. If K ever becomes
// author-settable this has to become a function of it, in all three places.

/**
 * How many head frames this generation renders and then loses at delivery.
 *
 * Mirrors `engine.py::_trim_for`. The test is "did THIS generation carry an ANCHORED continuation",
 * not "is it not first" — an unanchored seam reproduces nothing, so there is no duplicate head to
 * drop.
 */
export function trimJoinFor(gen: PackedGeneration): number {
  return gen.continuation?.anchor ? H3_TRIM_JOIN : 0;
}

/**
 * **The run-up**: seconds of picture a generation renders BEFORE its first delivered frame.
 *
 * 🔴 **It is an OVERLAP, not a gap, and that is the counter-intuitive half.** With
 * `placement="on_target"` the previous shot's tail is applied as keyframes onto this generation's
 * OWN first {@link H3_TRIM_JOIN} frames, so the render window is
 *
 *     [ deliveredAt − runUp , deliveredAt + deliveredLength )
 *
 * — it begins on top of the previous shot's last `runUp` seconds, re-rendering footage that shot
 * already delivered, and those frames are trimmed before the cut. **The delivered timeline still has
 * no holes in it**: nothing shifts, nothing is missing, and neither the ruler nor the waveform wants
 * a gap cut into it. What the run-up costs is that the generation's OWN clock — the one every
 * `[Shot N] At MM:SS.mmm` mark is measured on — starts `runUp` earlier than its block does.
 *
 * Zero on the first generation and on any unanchored seam, which is why every caller can apply it
 * unconditionally.
 */
export function runUpSecondsFor(gen: PackedGeneration): number {
  return trimJoinFor(gen) / H3_FPS;
}

/**
 * Is a `[Shot N] At …` time inside the trimmed head, i.e. discarded before anyone sees it?
 *
 * 🔴 **A real defect class, not a hypothetical.** One authored mark in the live project sat at
 * 0.592 s against a 0.917 s run-up: the model rendered it, the trim threw those frames away, and the
 * cue simply never appeared. Nothing failed — the render was fine, the beat was absent — so it had
 * to be rewritten as an opening state instead.
 *
 * Inclusive of the boundary. Strictly, frames `[0, runUp)` are discarded and the frame AT `runUp` is
 * the first delivered one — but a cue landing exactly on the first delivered frame has no lead-in at
 * all, and H3 places a marked action to roughly ±0.5 s, so half the time it lands in the trimmed
 * head anyway. The boundary is warned about rather than blessed.
 */
export function markIsTrimmed(localSeconds: number, runUpSeconds: number): boolean {
  return runUpSeconds > 0 && localSeconds <= runUpSeconds + 1e-9;
}

/** One generation's share of the song. */
export interface GenerationSlice {
  generationId: string;
  /** Index into the packed generation list, so a caller can attach without re-matching by id. */
  index: number;
  /**
   * Where this slice starts IN THE SONG, seconds. Feeds `ffmpeg -ss`.
   *
   * Accounts for the trimmed anchor frame — see this file's header. Never negative: the first
   * generation is never a continuation, so its trim is 0.
   */
  songOffsetSeconds: number;
  /**
   * How long the slice is. Clamped to `H3_AUDIO_REF.maxSeconds` (15.0), which is why a MAXIMUM
   * generation (362f = 15.083 s) carries a slice two frames shorter than its picture.
   */
  sliceSeconds: number;
  /** Where this generation's first surviving frame lands in the finished cut, seconds. */
  deliveredAtSeconds: number;
  /**
   * The {@link runUpSecondsFor run-up} — `deliveredAtSeconds - songOffsetSeconds`, always.
   *
   * Carried rather than re-derived so a reader cannot disagree with the offsets actually cut: the
   * offset is clamped at 0, so on a (pathological) first generation with an anchored continuation
   * `trim/fps` and `delivered - offset` would differ and only this one is true of the slice.
   */
  runUpSeconds: number;
  /**
   * Why this generation gets no slice, or `null` when it gets one.
   *
   * A generation under 2 s cannot carry a legal audio reference at all (`:348`), and one that runs
   * past the end of the song has nothing left to cut. Both render on the model's own generated bed,
   * which is a legitimate outcome and not a failure — but it is one the author must be TOLD about,
   * because a silent gap in conditioning looks identical to a slice that worked until you listen.
   */
  skipped: string | null;
}

export interface SoundtrackPlan {
  slices: GenerationSlice[];
  /** Total length of the finished cut, seconds — generation lengths minus the seam trims. */
  deliveredSeconds: number;
  /** Generations that will render on their own bed. Empty is the happy path. */
  skipped: GenerationSlice[];
  /** Song seconds past the end of the last generation, or 0. Drives the "song is longer" warning. */
  songTailSeconds: number;
}

/**
 * Cut `songSeconds` of audio into one slice per generation.
 *
 * Pure arithmetic over the pack — it does not touch files. The caller turns each
 * `{songOffsetSeconds, sliceSeconds}` into actual bytes (koblem does it with `ffmpeg -ss -t`) and
 * attaches the result as an `<Audio N>` reference.
 *
 * `songSeconds` of 0 or less yields every generation skipped, which is the correct answer for a
 * project with no track attached and keeps the caller free of a null branch.
 */
export function planSoundtrack(
  generations: PackedGeneration[],
  songSeconds: number
): SoundtrackPlan {
  const slices: GenerationSlice[] = [];
  let deliveredAt = 0;

  for (let i = 0; i < generations.length; i++) {
    const gen = generations[i];
    const trim = trimJoinFor(gen);
    // The run-up is about to be dropped from delivery, but the model still renders it and still
    // needs its audio — so the slice reaches back by exactly the trim.
    const songOffset = Math.max(0, deliveredAt - trim / H3_FPS);
    const wanted = audioSliceSeconds(gen.seconds);
    const remaining = songSeconds - songOffset;

    let sliceSeconds = 0;
    let skipped: string | null = null;
    if (songSeconds <= 0) {
      skipped = 'no project soundtrack attached';
    } else if (!generationWantsSlice(gen)) {
      skipped = 'the soundtrack is not in scope for this generation';
    } else if (wanted === null) {
      skipped =
        `this generation is ${gen.seconds.toFixed(2)}s, under the ` +
        `${H3_AUDIO_REF.minSeconds}s minimum WanGP accepts for an audio reference — it will ` +
        `render on the model's own bed`;
    } else if (remaining < H3_AUDIO_REF.minSeconds) {
      skipped =
        `the song ends ${Math.max(0, remaining).toFixed(2)}s into this generation, less than the ` +
        `${H3_AUDIO_REF.minSeconds}s minimum for a slice — it will render on the model's own bed`;
    } else {
      sliceSeconds = Math.min(wanted, remaining);
    }

    slices.push({
      generationId: gen.id,
      index: i,
      songOffsetSeconds: songOffset,
      sliceSeconds,
      deliveredAtSeconds: deliveredAt,
      runUpSeconds: deliveredAt - songOffset,
      skipped
    });
    deliveredAt += (gen.frames - trim) / H3_FPS;
  }

  return {
    slices,
    deliveredSeconds: deliveredAt,
    skipped: slices.filter((s) => s.skipped !== null),
    songTailSeconds: Math.max(0, songSeconds - deliveredAt)
  };
}

/**
 * What to tell the author about the fit between song and timeline, or `null` when it is clean.
 *
 * Deliberately a note rather than a clamp. The timeline is the author's; a song longer than the cut
 * is an ordinary state to be in halfway through building one, and a Director that silently
 * lengthened the timeline to match — or trimmed the song — would be editing the work to make its
 * own arithmetic tidy.
 */
export function soundtrackFitNote(plan: SoundtrackPlan, songSeconds: number): string | null {
  if (songSeconds <= 0) return null;
  const secs = (n: number) => `${n.toFixed(2)}s`;
  if (plan.songTailSeconds > 0.5) {
    return (
      `The song runs ${secs(songSeconds)} but the timeline delivers ${secs(plan.deliveredSeconds)} — ` +
      `the last ${secs(plan.songTailSeconds)} will not be reached. Add shots, or trim the track.`
    );
  }
  if (plan.skipped.length) {
    const which = plan.skipped.map((s) => s.index + 1).join(', ');
    return (
      `Generation${plan.skipped.length === 1 ? '' : 's'} ${which} will render on the model's own ` +
      `generated bed rather than the song. ${plan.skipped[0].skipped}.`
    );
  }
  return null;
}

/**
 * Where a point on the timeline sits on the clock a `[Shot N] At MM:SS.mmm` mark is measured on.
 *
 * 🔴 **The prompt's clock is not the timeline's clock, and the gap is silent.** Each generation is
 * conditioned on its own slice of the song, so the model's clock restarts at every render boundary
 * — `At 00:22.000` in the second generation of a 14 s-per-generation cut does not mean 22 s into
 * the song, it means 22 s into a 14 s render, which is outside the duration and therefore no cue at
 * all (`PROMPT-CONTRACT.md:176`). An author reading a beat off the waveform reads the SONG time;
 * this is the conversion that keeps them from writing it straight into a prompt.
 */
export interface ShotClock {
  /** Time in the song, which is also time in the finished cut. */
  songSeconds: number;
  /** 1-based generation number, or `null` when no generation has started by then. */
  shot: number | null;
  /**
   * Seconds from that generation's FRAME 0 — the number a `[Shot N] At …` mark carries.
   *
   * Measured from `songOffsetSeconds`, not from delivery: with an anchored seam the generation
   * renders a head that delivery then drops, and the model's clock starts on the frame it renders.
   * Equivalently, and this is the form the brief for this readout states it in:
   *
   *     localSeconds = (songSeconds − deliveredAt) + runUp
   */
  localSeconds: number;
  /** `localSeconds` is at or past the generation's rendered length, so a mark there is not a cue. */
  past: boolean;
  /** That generation's {@link runUpSecondsFor run-up} — 0 on the first shot and on a loose seam. */
  runUpSeconds: number;
  /**
   * `localSeconds` is inside the trimmed head, so a mark written there would be discarded.
   *
   * Only ever true at the exact first delivered frame when this comes from {@link shotClockAt},
   * which attributes a time to the generation whose DELIVERED block contains it — everything inside
   * that block is at or past the run-up by construction. It is on the record so the one readout that
   * can hit the boundary does not have to re-derive the rule. {@link runUpAt} is what answers "is
   * this instant inside somebody's trimmed head".
   */
  trimmed: boolean;
}

/**
 * The generation whose RUN-UP covers this delivered time, or `null`.
 *
 * 🔴 **A moment near a seam legitimately belongs to two shots**, and they are not in conflict: it is
 * inside shot N−1's delivered block (that is the picture the viewer sees there, and that is the
 * attribution {@link shotClockAt} gives it) and it is ALSO inside shot N's run-up, because shot N
 * re-renders that stretch to get itself moving into the join and then throws it away. This is the
 * second half, for a readout that wants to say so.
 *
 * The window is `[songOffsetSeconds, deliveredAtSeconds)` — half-open at the top, because the frame
 * AT `deliveredAtSeconds` is the generation's first surviving one and belongs to its delivered block
 * rather than to its run-up.
 */
export interface RunUpHit {
  /** 1-based generation number, matching {@link ShotClock.shot}. */
  shot: number;
  /** Seconds from that generation's frame 0 — under its run-up, therefore always trimmed. */
  localSeconds: number;
  runUpSeconds: number;
}

export function runUpAt(plan: SoundtrackPlan, songSeconds: number): RunUpHit | null {
  for (const s of plan.slices) {
    if (s.runUpSeconds <= 0) continue;
    if (songSeconds >= s.songOffsetSeconds && songSeconds < s.deliveredAtSeconds) {
      return {
        shot: s.index + 1,
        localSeconds: songSeconds - s.songOffsetSeconds,
        runUpSeconds: s.runUpSeconds
      };
    }
  }
  return null;
}

/**
 * Convert a song time to {@link ShotClock}.
 *
 * Takes the plan rather than recomputing, so it cannot disagree with the slices actually cut. The
 * owning generation is chosen on DELIVERED time because that is the clock the finished cut — and
 * therefore the muxed song — runs on.
 *
 * 🔴 **This module and koblem's planner DID disagree, silently, until 2026-08-10.** On
 * `11599681-…` (clips 14.4 / 14.0 / 14.0 s) koblem planned 42.375 s and offsets 0 / 14.375 /
 * 28.375 for a cut this module — and the engine — make 43.042 s at 0 / 14.3333 / 28.6667. Two
 * causes, both in `authored_generations` (`ltx_director.rs`), both now fixed and both regression
 * tests: it emitted `frames` UNSNAPPED (14.0 s → 336, which the engine renders as 345), and it
 * never emitted a `continuation` at all, so every seam planned as unanchored while the render
 * anchored it.
 *
 * ⚠️ The lesson, not the incident: this arithmetic exists twice, and the copies drifted without
 * anything failing. If it drifts again, {@link ShotClock.localSeconds} goes quietly wrong by the
 * same order as the error the readout exists to prevent. **koblem is authoritative** — a `[Shot N]`
 * mark is only as good as the offsets the render actually used, so re-measure (§ the project's
 * handoff) rather than trusting either implementation after a change to seams or shot lengths.
 */
export function shotClockAt(
  plan: SoundtrackPlan,
  generations: PackedGeneration[],
  songSeconds: number
): ShotClock {
  let slice: GenerationSlice | null = null;
  for (const s of plan.slices) {
    if (s.deliveredAtSeconds <= songSeconds + 1e-9) slice = s;
    else break;
  }
  if (!slice)
    return {
      songSeconds,
      shot: null,
      localSeconds: 0,
      past: false,
      runUpSeconds: 0,
      trimmed: false
    };
  const gen = generations[slice.index];
  const localSeconds = songSeconds - slice.songOffsetSeconds;
  return {
    songSeconds,
    shot: slice.index + 1,
    localSeconds,
    past: gen ? localSeconds >= gen.frames / H3_FPS : false,
    runUpSeconds: slice.runUpSeconds,
    trimmed: markIsTrimmed(localSeconds, slice.runUpSeconds)
  };
}

/**
 * `MM:SS.mmm` — the exact spelling a `[Shot N] At …` mark takes, so a readout is paste-ready.
 *
 * Milliseconds are not decoration: at 88 BPM a beat is 0.68 s and H3 places a marked action to
 * roughly ±0.5 s, so a mark rounded to the nearest second can miss by more than a beat before the
 * model's own tolerance is even counted.
 */
export function h3Timecode(seconds: number): string {
  const s = Math.max(0, seconds);
  const mm = Math.floor(s / 60);
  return `${String(mm).padStart(2, '0')}:${(s - mm * 60).toFixed(3).padStart(6, '0')}`;
}

/*
 * 🔴 A TS copy of the DRIFT GUARD used to live here and has been removed rather than left unwired.
 *
 * It compared the plan against the finished clip's real duration — but the browser never sees that
 * clip's duration, so it could never have been called, and the guard that matters runs in koblem
 * (`h3_soundtrack::drift_note`) where the file is. A second implementation with no caller is not a
 * safety net; it is a second definition of the seam-trim arithmetic waiting to disagree with the
 * one that actually runs.
 *
 * What remains here is deliberately only what the EDITOR can answer before a render: which
 * generations will be scored, and whether the song and the timeline are the same length. koblem
 * re-derives all of it authoritatively at submit; this exists so the author is not told after a
 * fifteen-minute queue.
 */
