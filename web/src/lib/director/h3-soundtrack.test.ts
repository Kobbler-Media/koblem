import { describe, expect, it } from 'vitest';
import {
  H3_SOUNDTRACK_SENTINEL,
  H3_TRIM_JOIN,
  h3Timecode,
  markIsTrimmed,
  planSoundtrack,
  runUpAt,
  runUpSecondsFor,
  shotClockAt,
  trimJoinFor,
  type SoundtrackPlan
} from './h3-soundtrack';
import type { PackedGeneration } from './h3-generations';

/**
 * 🔴 **Derived, never spelled.** `H3_TRIM_JOIN` moved 1 → 22 on 2026-08-14 when comfy-api's
 * continuation went to `placement="on_target"`, and every assertion in this file that had written
 * `1 / 24` by hand went stale in the same commit — silently, because they still described a real
 * arithmetic, just not this one. A test that hard-codes the constant it is testing cannot fail when
 * the constant moves, which is the one moment it exists for.
 */
const TRIM = H3_TRIM_JOIN / 24;

/**
 * The generation shape `planSoundtrack` and `shotClockAt` actually read.
 *
 * Only the fields those two touch — `frames`, `seconds`, `references`, `continuation` — so a test
 * does not have to invent a whole pack to assert on the arithmetic.
 */
function gen(frames: number, over: Partial<PackedGeneration> = {}): PackedGeneration {
  return {
    id: `g${frames}`,
    frames,
    seconds: frames / 24,
    references: [{ id: 'h3-project-soundtrack', kind: 'audio', audio: H3_SOUNDTRACK_SENTINEL }],
    continuation: null,
    ...over
  } as unknown as PackedGeneration;
}

describe('shotClockAt', () => {
  /**
   * 🔴 Pinned to a REAL project — "Gawdwin — Sealed by Fate": three 345-frame generations under a
   * 208 s track, gens 2 and 3 carrying anchored `latents` continuations. That shape is read off the
   * job stamp koblem persisted (`generations.request_meta.generation_plans`), and the same three
   * numbers are asserted server-side in `ltx_director.rs`'s
   * `the_soundtrack_plan_now_agrees_with_the_editor` — the two implementations drifted apart
   * silently once, so they are now pinned to one worked example from both ends.
   *
   * ⚠️ The naive answer here is 0 / 14.375 / 28.75 — generation length times index. It is wrong by
   * one RUN-UP per seam, and wrong in the direction that silently accumulates.
   *
   * This is the case that produced the bug the readout exists to prevent: the first lyric sits at
   * 22.80 s in the song, the author wrote `At 00:22.000` into the second generation, and 22 s is
   * outside a 14.375 s render — so the cue was not late, it was absent, and the mouth moved from
   * frame 0. The correct mark is 22.80 − (14.375 − run-up), which was 8.4667 while the trim was one
   * frame and is 9.3417 now that it is 22.
   */
  const anchored = { mode: 'latents', k: 7, anchor: true };
  const gens = [
    gen(345),
    gen(345, { continuation: anchored as never }),
    gen(345, { continuation: anchored as never })
  ];
  const plan: SoundtrackPlan = planSoundtrack(gens, 208.008);

  it('lays the slices where the render actually put them, one trim back per seam', () => {
    const offsets = plan.slices.map((s) => s.songOffsetSeconds);
    expect(offsets[0]).toBe(0);
    expect(offsets[1]).toBeCloseTo(14.375 - TRIM, 6);
    expect(offsets[2]).toBeCloseTo(14.375 + (345 - H3_TRIM_JOIN) / 24 - TRIM, 6);
  });

  it('reads the first generation on the song clock, unshifted', () => {
    const at = shotClockAt(plan, gens, 11.95);
    expect(at.shot).toBe(1);
    expect(at.localSeconds).toBeCloseTo(11.95, 6);
  });

  it('restarts the clock at every generation boundary', () => {
    const at = shotClockAt(plan, gens, 22.8);
    expect(at.shot).toBe(2);
    // 22.8 − (14.375 − runUp). The run-up is IN the answer, which is the whole point: the model's
    // clock is already running when the block appears.
    expect(at.localSeconds).toBeCloseTo(22.8 - 14.375 + TRIM, 6);
  });

  it('is the number the prompt takes, not the number the ruler shows', () => {
    // The whole defect in one assertion: same instant, two clocks, a whole generation apart.
    const at = shotClockAt(plan, gens, 22.8);
    expect(at.songSeconds).toBe(22.8);
    expect(at.songSeconds - at.localSeconds).toBeCloseTo(14.375 - TRIM, 6);
  });

  it('flags a time past the end of its generation, which is no cue at all', () => {
    expect(shotClockAt(plan, gens, plan.slices[1].songOffsetSeconds + 14.3).past).toBe(false);
    // 🔴 Reachable only on the LAST generation, and that is arithmetic rather than an oversight:
    // the local time available before handover is `(frames - trim)/fps + trim/fps`, which is
    // exactly `frames/fps`. So a middle generation reaches its own end at the precise instant the
    // next one's first delivered frame claims that song time, and the instant goes to the
    // successor (below). Past-the-end therefore means past the whole cut.
    const last = plan.slices[2].songOffsetSeconds;
    expect(shotClockAt(plan, gens, last + 14.3).past).toBe(false);
    expect(shotClockAt(plan, gens, last + 345 / 24).past).toBe(true);
  });

  it('hands a boundary instant to the generation that renders it, not the one that ended', () => {
    // 14.375 local into generation 2 IS generation 3's first delivered frame (28.708333 in the
    // song). The readout names shot 3, one trimmed frame in — reporting shot 2 as past its end
    // would point the author at a render that no longer has the picture.
    const at = shotClockAt(plan, gens, plan.slices[1].songOffsetSeconds + 345 / 24);
    expect(at.shot).toBe(3);
    expect(at.localSeconds).toBeCloseTo(TRIM, 6);
  });

  it('returns no shot before the first generation starts, rather than guessing one', () => {
    const at = shotClockAt(planSoundtrack([], 208), [], 5);
    expect(at.shot).toBeNull();
    expect(at.songSeconds).toBe(5);
  });

  it('measures from the rendered frame 0, not from delivery', () => {
    // Generation 2 is DELIVERED at 14.375, but the model renders the run-up that delivery then
    // drops and hears its audio, so its own clock is already a run-up in by then.
    expect(shotClockAt(plan, gens, 14.375).localSeconds).toBeCloseTo(TRIM, 6);
  });

  it('gives an unanchored seam no reach-back, because it duplicates no frame', () => {
    const loose = [gen(345), gen(345)];
    const p = planSoundtrack(loose, 208);
    expect(p.slices[1].songOffsetSeconds).toBe(14.375);
  });
});

/**
 * The run-up: the same arithmetic as above, named and asserted from the direction the UI reads it.
 *
 * 🔴 Pinned to the LIVE 16-shot project, whose numbers the owner measured off the render — 345-frame
 * generations, `latents` continuations with `anchor`, one 158-frame tail. Those five numbers below
 * are the ones a wrong implementation gets wrong first, and they are the ones he checks against.
 */
describe('the run-up', () => {
  const anchored = { mode: 'latents', k: 7, anchor: true };
  /** 16 shots: fifteen 345-frame renders, then a 158-frame tail. Shot 1 is never a continuation. */
  const live = Array.from({ length: 16 }, (_, i) =>
    gen(i === 15 ? 158 : 345, i === 0 ? {} : { continuation: anchored as never })
  );
  const plan = planSoundtrack(live, 208.008);

  it('is trimJoinFor over the frame rate, and zero where nothing is anchored', () => {
    expect(trimJoinFor(live[0])).toBe(0);
    expect(runUpSecondsFor(live[0])).toBe(0);
    expect(trimJoinFor(live[1])).toBe(H3_TRIM_JOIN);
    expect(runUpSecondsFor(live[1])).toBeCloseTo(H3_TRIM_JOIN / 24, 9);
    // The number the owner reads on screen. Derived, but pinned once so a change to H3_TRIM_JOIN
    // that nobody meant shows up as a failing expectation rather than as a quietly different UI.
    expect(runUpSecondsFor(live[1])).toBeCloseTo(0.91667, 5);
  });

  it('is exactly deliveredAt − songOffset on every slice', () => {
    for (const s of plan.slices) {
      expect(s.runUpSeconds).toBeCloseTo(s.deliveredAtSeconds - s.songOffsetSeconds, 12);
    }
  });

  /**
   * 🔴 The property the whole design rests on: the run-up is an OVERLAP, not a gap. Shot N's render
   * opens ON TOP of shot N−1's last `runUp` seconds — so the delivered blocks still abut exactly,
   * and neither the ruler nor the waveform wants a hole cut into it.
   */
  it('overlaps the previous shot rather than opening a gap in the delivered cut', () => {
    for (let i = 1; i < plan.slices.length; i++) {
      const prev = plan.slices[i - 1];
      const here = plan.slices[i];
      const renderStart = here.deliveredAtSeconds - here.runUpSeconds;
      // It begins BEFORE this shot delivers…
      expect(renderStart).toBeLessThan(here.deliveredAtSeconds);
      // …and after the previous shot began, i.e. inside its block, not in a gap before it.
      expect(renderStart).toBeGreaterThan(prev.deliveredAtSeconds);
      // And the delivered timeline is still continuous: nothing between the blocks. Each shot
      // advances by what it DELIVERS — its frames less its own run-up — and nothing is left over.
      const before = live[i - 1];
      expect(here.deliveredAtSeconds).toBeCloseTo(
        prev.deliveredAtSeconds + (before.frames - trimJoinFor(before)) / 24,
        6
      );
    }
  });

  /** The exact figures the owner quoted off the live render, to the millisecond he reads. */
  it('reproduces the live project’s shot 2 and shot 6 clocks', () => {
    // "shot 1: delivered_at = 14.375, runUp = 0.91667, prompt clock 0 = song 13.458"
    // (his shot 1 is the second render — this module numbers from 1, so index 1.)
    expect(plan.slices[1].deliveredAtSeconds).toBeCloseTo(14.375, 6);
    expect(plan.slices[1].runUpSeconds).toBeCloseTo(0.91667, 5);
    expect(plan.slices[1].songOffsetSeconds).toBeCloseTo(13.458, 3);
    // "hovering at song 23.717 → prompt time 10.259"
    expect(shotClockAt(plan, live, 23.717).shot).toBe(2);
    expect(shotClockAt(plan, live, 23.717).localSeconds).toBeCloseTo(10.259, 3);
    // "shot 5: prompt clock 0 = song 67.292; the rift beat authored at 11.508 lands at 78.800"
    expect(plan.slices[5].songOffsetSeconds).toBeCloseTo(67.292, 3);
    expect(plan.slices[5].songOffsetSeconds + 11.508).toBeCloseTo(78.8, 3);
  });

  it('leaves the opening shot on the song’s own clock, run-up and all', () => {
    expect(plan.slices[0].runUpSeconds).toBe(0);
    const at = shotClockAt(plan, live, 6.25);
    expect(at.shot).toBe(1);
    // No run-up ⇒ prompt time IS the delivered offset. The identity the first shot must keep.
    expect(at.localSeconds).toBeCloseTo(6.25 - plan.slices[0].deliveredAtSeconds, 9);
    expect(at.runUpSeconds).toBe(0);
    expect(at.trimmed).toBe(false);
  });

  /** `prompt_time(N, D) = (D − delivered_at(N)) + runUp(N)`, stated as the brief states it. */
  it('is (delivered offset + run-up) at every point of every block', () => {
    for (let i = 0; i < plan.slices.length; i++) {
      const s = plan.slices[i];
      const end = plan.slices[i + 1]?.deliveredAtSeconds ?? plan.deliveredSeconds;
      for (const frac of [0, 0.25, 0.5, 0.99]) {
        const d = s.deliveredAtSeconds + (end - s.deliveredAtSeconds) * frac;
        const at = shotClockAt(plan, live, d);
        expect(at.shot).toBe(i + 1);
        expect(at.localSeconds).toBeCloseTo(d - s.deliveredAtSeconds + s.runUpSeconds, 9);
      }
    }
  });
});

describe('markIsTrimmed — the unreachable boundary', () => {
  const runUp = H3_TRIM_JOIN / 24;

  it('discards everything at or under the run-up', () => {
    expect(markIsTrimmed(0, runUp)).toBe(true);
    // The mark that actually bit: 0.592 s against a 0.917 s head. It rendered and never appeared.
    expect(markIsTrimmed(0.592, runUp)).toBe(true);
    expect(markIsTrimmed(runUp - 1 / 24, runUp)).toBe(true);
  });

  /**
   * Inclusive at the boundary, on purpose. Strictly, frames `[0, runUp)` are discarded and the frame
   * AT `runUp` survives as delivered frame 0 — but a cue there has no lead-in whatsoever, and H3
   * places a marked action to roughly ±0.5 s, so it lands in the trimmed head about half the time.
   * Warned rather than blessed.
   */
  it('warns at the boundary itself rather than blessing it', () => {
    expect(markIsTrimmed(runUp, runUp)).toBe(true);
    expect(markIsTrimmed(runUp + 1 / 24, runUp)).toBe(false);
  });

  it('never fires where there is no run-up, so the opening shot can mark its own frame 0', () => {
    expect(markIsTrimmed(0, 0)).toBe(false);
    expect(markIsTrimmed(12, 0)).toBe(false);
  });
});

describe('runUpAt — the shot that renders an instant twice', () => {
  const anchored = { mode: 'latents', k: 7, anchor: true };
  const gens = [gen(345), gen(345, { continuation: anchored as never }), gen(345)];
  const plan = planSoundtrack(gens, 208.008);

  it('names the shot whose run-up covers the instant, on that shot’s own clock', () => {
    const s = plan.slices[1];
    const inside = s.songOffsetSeconds + 0.4;
    const hit = runUpAt(plan, inside);
    expect(hit).not.toBeNull();
    expect(hit!.shot).toBe(2);
    expect(hit!.localSeconds).toBeCloseTo(0.4, 9);
    // 🔴 And the DELIVERED attribution of the same instant is the shot BEFORE it — the picture
    // playing there is shot 1's. Both are true; they are two renders of one moment.
    expect(shotClockAt(plan, gens, inside).shot).toBe(1);
  });

  it('is half-open at the top, so a delivered frame 0 is never called a run-up', () => {
    const s = plan.slices[1];
    expect(runUpAt(plan, s.deliveredAtSeconds)).toBeNull();
    expect(runUpAt(plan, s.deliveredAtSeconds - 1e-6)).not.toBeNull();
  });

  it('finds nothing on an unanchored seam, which re-renders nothing', () => {
    // Generation 3 above carries no continuation, so it opens on its own first delivered frame.
    expect(runUpAt(plan, plan.slices[2].deliveredAtSeconds - 0.1)?.shot).not.toBe(3);
  });

  it('finds nothing anywhere in a cut with no anchored seam at all', () => {
    const loose = planSoundtrack([gen(345), gen(345)], 208);
    expect(runUpAt(loose, 0)).toBeNull();
    expect(runUpAt(loose, 14.3)).toBeNull();
    expect(runUpAt(loose, 20)).toBeNull();
  });
});

describe('h3Timecode', () => {
  it('writes the MM:SS.mmm spelling the prompt takes', () => {
    expect(h3Timecode(0)).toBe('00:00.000');
    expect(h3Timecode(8.425)).toBe('00:08.425');
    expect(h3Timecode(11.95)).toBe('00:11.950');
    expect(h3Timecode(63.5)).toBe('01:03.500');
  });

  it('keeps milliseconds, because a rounded mark misses by more than a beat', () => {
    expect(h3Timecode(1.03)).toBe('00:01.030');
    expect(h3Timecode(2.395)).toBe('00:02.395');
  });

  it('never emits a negative timecode', () => {
    expect(h3Timecode(-1)).toBe('00:00.000');
  });
});
