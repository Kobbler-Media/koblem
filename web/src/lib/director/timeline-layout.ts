/**
 * Where each shot sits on the time axis the timeline actually draws.
 *
 * 🔴 **The picture and the waveform were on two different clocks.** Clip geometry accumulated
 * AUTHORED seconds (`sum(clip.seconds)`) while the waveform, the lyric chips, the `mark` line and
 * the hover readout were all drawn on SONG seconds — which is delivered time. Those clocks diverge
 * at every render boundary: a generation authored at 14.0 s renders at 345 frames (14.375 s) and
 * delivers 14.333 s after the anchored seam eats its duplicate head frame. That is +0.33 s per
 * generation, in one direction, forever. Seven generations in, a beat plainly sitting under shot 8's
 * block reported "Shot 7", because the block was drawn ~2.3 s early and the readout was not.
 *
 * So the layout is on DELIVERED time — the clock the finished cut, the muxed song and every
 * `[Shot N] At MM:SS.mmm` mark run on. The generation offsets come from {@link SoundtrackPlan}
 * rather than being re-derived, for the same reason `shotClockAt` takes the plan: this arithmetic
 * already exists twice (here and in koblem) and the copies have drifted silently before.
 *
 * The clip array itself is untouched — `clip.seconds` stays authored, because that is what the
 * author typed and what compiles. {@link toAuthoredSeconds} is the way back for the edit gestures.
 *
 * ## The third clock: {@link LaidOutClip.runUp}
 *
 * There is now one more, and it does NOT move anything on this axis. An anchored H3 continuation
 * re-renders the previous shot's last 22 frames as its own head and trims them, so a shot's RENDER
 * begins 0.917 s before its delivered block — on top of its predecessor, not in a gap. `start` and
 * `width` are untouched by it and the lane still has no holes. What it changes is the clock a
 * `[Shot N] At MM:SS.mmm` mark is measured on, which is the number a human types into a prompt: it
 * runs from `start - runUp`, not from `start`. Carrying it here is what lets every readout on the
 * timeline show that number without re-deriving the seam arithmetic a fourth time.
 */

import type { DirectorClip } from '$lib/api/client';
import { actualDuration } from '$lib/ltx/frames';
import type { PackedGeneration } from './h3-generations';
import type { SoundtrackPlan } from './h3-soundtrack';

/** The shortest span the timeline will draw. Mirrors the floor the drag math has always applied. */
export const MIN_CLIP_SECONDS = 0.1;

export interface LaidOutClip {
  id: string;
  /** Delivered seconds — the same axis as the waveform. */
  start: number;
  /** Delivered length. Differs from {@link authored} on the first and last shot of a generation. */
  width: number;
  /** Where this clip starts on the authored clock, for the inverse. */
  authoredStart: number;
  /** `clip.seconds`, floored. What compiles, and what a trim writes back. */
  authored: number;
  /**
   * The **run-up**: seconds this shot's render begins BEFORE its block, and never delivers.
   *
   * 🔴 An OVERLAP, not a gap — see {@link runUpSecondsFor}. The render window is
   * `[start - runUp, start + width)`, so it opens on top of the previous shot's last `runUp`
   * seconds, re-renders them, and trims them. **Nothing on the delivered axis moves**: `start` and
   * `width` are exactly what they were, the lane still has no holes in it, and the ruler and the
   * waveform want no gap cut into them. What it changes is only the CLOCK a `[Shot N] At …` mark is
   * read on, which starts `runUp` before this block does.
   *
   * ⚠️ Carried on the generation's FIRST shot only, and 0 on every other clip. It is a property of
   * the RENDER, not of a shot: a shot in the middle of a generation is already mid-render, so
   * nothing precedes it that delivery is going to drop. 0 on LTX throughout, which is why every
   * caller can add it unconditionally.
   */
  runUp: number;
}

export interface TimelineLayout {
  clips: LaidOutClip[];
  byId: Map<string, LaidOutClip>;
  /** Delivered length of the whole cut. */
  end: number;
  /** Whether delivered and authored time disagree anywhere — i.e. whether any of this matters. */
  drifts: boolean;
}

/**
 * The delivered length of a clip that no generation packing covers — which is every clip on LTX.
 *
 * 🔴 **LTX drifts too, and for its own reason.** It rounds each shot to a VAE-valid `8k+1` frame
 * count (`framesFromSeconds`), so an 8 s shot at 24 fps delivers 8.042 s and a 7.9 s one delivers
 * 7.875 s. The server is explicit that this IS the produced length — a continuation shot's seam
 * overlap is padded and trimmed by the same number, so it contributes nothing
 * (`ltx_video.rs`: "a shot's produced length is EXACTLY `shot_visible`").
 *
 * Unlike H3's the error is a ROUND, not a ceil, so it does not march in one direction — it wanders,
 * which is worse to diagnose and no less wrong by the last chorus.
 */
export const ltxDeliveredSeconds =
  (fps: number) =>
  (clip: DirectorClip): number =>
    actualDuration(clip.seconds, fps);

/**
 * Lay the clips out on delivered time.
 *
 * Clips a generation covers take their length from the pack; the rest take it from
 * `deliveredSeconds`, which is how the LTX rule gets in without this module knowing an engine.
 * Omitting it falls back to the authored length, which is only right where nothing snaps.
 */
export function layoutClips(
  clips: DirectorClip[],
  generations: PackedGeneration[],
  plan: SoundtrackPlan | null,
  deliveredSeconds?: (clip: DirectorClip) => number
): TimelineLayout {
  const placed = new Map<string, { start: number; width: number; runUp: number }>();

  if (plan && generations.length && plan.slices.length === generations.length) {
    generations.forEach((gen, gi) => {
      const from = plan.slices[gi].deliveredAtSeconds;
      const to = plan.slices[gi + 1]?.deliveredAtSeconds ?? plan.deliveredSeconds;
      // The head an anchored seam re-renders and delivery then drops (`planSoundtrack`) — the
      // RUN-UP. It comes off the FIRST shot because that is the stretch that does not survive —
      // taking it off the last would put the generation's shots back out of step with its own
      // delivered start.
      //
      // Read out of the plan's own boundaries rather than from `trimJoinFor`, for the same reason
      // the offsets are: this module must not hold a second copy of the trim arithmetic. It is
      // `gen.seconds` (the RENDERED length) minus the delivered span, which is exactly `trim/fps`.
      const headTrim = Math.max(0, gen.seconds - (to - from));
      let t = from;
      gen.shots.forEach((s, i) => {
        const runUp = i === 0 ? headTrim : 0;
        // Floored, like the authored path below: a 0-second shot (an LLM plan, a hand-edited doc)
        // would otherwise take no width, and every block after it inside this generation would draw
        // on top of the one before. The floor is repaid out of the generation's own span rather
        // than pushing its successors, so the boundaries the plan fixed stay put.
        const width = Math.max(MIN_CLIP_SECONDS, s.renderedSeconds - runUp);
        placed.set(s.clip.id, { start: t, width, runUp });
        t += width;
      });
    });
  }

  const out: LaidOutClip[] = [];
  let delivered = 0;
  let authoredAt = 0;
  for (const c of clips) {
    const authored = Math.max(MIN_CLIP_SECONDS, c.seconds);
    // Unplaced means either LTX (nothing is packed there) or — on H3, where packing covers every
    // clip — a bug. Both want the same answer: draw the clip at the length it will really deliver,
    // laid end to end, so the lane can never contain a hole.
    const own = Math.max(MIN_CLIP_SECONDS, deliveredSeconds?.(c) ?? authored);
    const p = placed.get(c.id) ?? { start: delivered, width: own, runUp: 0 };
    out.push({
      id: c.id,
      start: p.start,
      width: p.width,
      authoredStart: authoredAt,
      authored,
      runUp: p.runUp
    });
    delivered = p.start + p.width;
    authoredAt += authored;
  }

  return {
    clips: out,
    byId: new Map(out.map((l) => [l.id, l])),
    end: out.length ? out[out.length - 1].start + out[out.length - 1].width : 0,
    drifts: out.some((l) => Math.abs(l.width - l.authored) > 1e-6)
  };
}

/**
 * Delivered seconds → the authored clock the clip array is stored on.
 *
 * Every edit gesture reads a time off the axis (which is delivered) and writes it into a clip
 * (which is authored). Interpolating inside the clip that owns the point rather than scaling the
 * whole axis matters: the drift is not uniform — it is concentrated on the last shot of each
 * generation — so a global ratio would put a split in the wrong shot.
 */
export function toAuthoredSeconds(layout: TimelineLayout, delivered: number): number {
  if (!layout.clips.length) return Math.max(0, delivered);
  const first = layout.clips[0];
  if (delivered <= first.start) return 0;
  for (const l of layout.clips) {
    if (delivered < l.start + l.width) {
      const frac = l.width > 0 ? (delivered - l.start) / l.width : 0;
      return l.authoredStart + frac * l.authored;
    }
  }
  const last = layout.clips[layout.clips.length - 1];
  return last.authoredStart + last.authored;
}
