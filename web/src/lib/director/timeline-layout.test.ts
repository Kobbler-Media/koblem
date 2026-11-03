import { describe, expect, it } from 'vitest';
import { waveformPeaks } from '@koblibs/ui';
import type { DirectorClip, DirectorH3Reference } from '#lib/api/client.ts';
import { packGenerations } from './h3-generations';
import {
  H3_SOUNDTRACK_REF_ID,
  H3_SOUNDTRACK_SENTINEL,
  H3_TRIM_JOIN,
  planSoundtrack,
  shotClockAt
} from './h3-soundtrack';
import { layoutClips, ltxDeliveredSeconds, toAuthoredSeconds } from './timeline-layout';

/** Derived, never spelled — see the note on the same constant in `h3-soundtrack.test.ts`. */
const TRIM = H3_TRIM_JOIN / 24;

const song: DirectorH3Reference = {
  id: H3_SOUNDTRACK_REF_ID,
  kind: 'audio',
  role: 'audio_reuse',
  audio: H3_SOUNDTRACK_SENTINEL
};

function clip(over: Partial<DirectorClip> = {}): DirectorClip {
  return { id: 'a', prompt: 'a shot', mode: 't2v', seconds: 4, keyframes: [], ...over };
}

/** N renders of one 14 s shot each — the shape that produced the report. */
const oneShotEach = (n: number) =>
  Array.from({ length: n }, (_, i) => clip({ id: `c${i}`, seconds: 14 }));

function laid(clips: DirectorClip[], songSeconds = 300) {
  const gens = packGenerations({ clips, references: [song], hasSoundtrack: true });
  const plan = planSoundtrack(gens, songSeconds);
  return { gens, plan, layout: layoutClips(clips, gens, plan) };
}

describe('layoutClips — the block and the waveform on one clock', () => {
  /**
   * 🔴 The regression, stated as the user reported it: hovering a moment plainly inside shot 8's
   * block read back "Shot 7". The blocks were drawn on authored seconds and everything else on song
   * seconds, and the two diverge by the frame-grid snap-up plus a trimmed seam frame per
   * generation — ~0.33 s each, always in the same direction, so by the eighth block the picture was
   * over two seconds early.
   */
  it('puts every song time inside the block the readout names for it', () => {
    const { gens, plan, layout } = laid(oneShotEach(9));
    for (let gi = 0; gi < gens.length; gi++) {
      const l = layout.clips[gi];
      // Sample inside the block, off both edges so a half-frame of rounding cannot decide it.
      for (const at of [l.start + 0.05, l.start + l.width / 2, l.start + l.width - 0.05]) {
        expect(shotClockAt(plan, gens, at).shot).toBe(gi + 1);
      }
    }
  });

  it('starts each block where the render delivers it, not where the authored sum lands', () => {
    const { plan, layout } = laid(oneShotEach(9));
    layout.clips.forEach((l, i) =>
      expect(l.start).toBeCloseTo(plan.slices[i].deliveredAtSeconds, 9)
    );
    // The authored layout would have said 14 s a block; the drift is what the report was about.
    // Derived from the constants, because the size AND the sign move with the trim: the +0.375 s
    // snap-up per generation used to outrun a 1-frame seam and now loses to a 22-frame one, so
    // delivered has gone from running ahead of authored to running behind it. Only the magnitude
    // is the subject here — a signed `> 2` silently became an assertion about the old trim.
    const drift = layout.clips[8].start - 8 * 14;
    expect(drift).toBeCloseTo(345 / 24 - 14 + 7 * (345 / 24 - TRIM - 14), 9);
    expect(Math.abs(drift)).toBeGreaterThan(2);
  });

  it('ends where the plan says the cut ends', () => {
    const { plan, layout } = laid(oneShotEach(4));
    expect(layout.end).toBeCloseTo(plan.deliveredSeconds, 9);
  });

  it('takes the seam trim off the head shot, so no block overlaps its neighbour', () => {
    const { layout } = laid(oneShotEach(5));
    for (let i = 1; i < layout.clips.length; i++) {
      const prev = layout.clips[i - 1];
      expect(layout.clips[i].start).toBeCloseTo(prev.start + prev.width, 9);
    }
    // A continued generation delivers one run-up less than it renders.
    expect(layout.clips[1].width).toBeCloseTo(345 / 24 - TRIM, 9);
    expect(layout.clips[0].width).toBeCloseTo(345 / 24, 9);
  });

  /**
   * A generation has no siblings any more — it is one shot — so each shot carries its OWN round-up
   * rather than a share of a pack's. That is more drift, not less: two 6 s and 4 s shots used to
   * snap once as 240 → 243 frames, and now snap twice, as 144 → 158 and 96 → 107.
   */
  it('gives every shot its own generation round-up, since a generation is one shot', () => {
    const { layout } = laid([clip({ id: 'a', seconds: 6 }), clip({ id: 'b', seconds: 4 })]);
    expect(layout.clips[0].width).toBeCloseTo(158 / 24, 9);
    // One run-up less than it renders: the anchored seam re-renders a head that delivery drops.
    expect(layout.clips[1].width).toBeCloseTo(107 / 24 - TRIM, 9);
    expect(layout.drifts).toBe(true);
  });

  it('is the authored layout when nothing snaps and no rule is supplied', () => {
    const clips = [clip({ id: 'a', seconds: 4 }), clip({ id: 'b', seconds: 2.5 })];
    const layout = layoutClips(clips, [], null);
    expect(layout.clips.map((l) => [l.start, l.width])).toEqual([
      [0, 4],
      [4, 2.5]
    ]);
    expect(layout.end).toBe(6.5);
    expect(layout.drifts).toBe(false);
  });

  /**
   * 🔴 LTX is not exempt. It rounds every shot to `8k+1` frames, so 8 s at 24 fps delivers 193
   * frames (8.042 s) and 7.9 s delivers 8.042 s too — the server calls this the produced length
   * outright. The error ROUNDS rather than ceils, so unlike H3's it wanders in both directions
   * instead of marching, which makes it harder to spot and no less wrong by the end.
   */
  it('lays LTX shots out at their 8k+1 delivered length, not the authored one', () => {
    const clips = [clip({ id: 'a', seconds: 8 }), clip({ id: 'b', seconds: 5 })];
    const layout = layoutClips(clips, [], null, ltxDeliveredSeconds(24));
    expect(layout.clips[0].width).toBeCloseTo(193 / 24, 9);
    // 5 s = 120 raw → k = round(119/8) = 15 → 121 frames, which is SHORTER than authored.
    expect(layout.clips[1].width).toBeCloseTo(121 / 24, 9);
    expect(layout.clips[1].start).toBeCloseTo(193 / 24, 9);
    expect(layout.drifts).toBe(true);
  });

  it('keeps an LTX split landing on the shot it was aimed at', () => {
    const clips = [clip({ id: 'a', seconds: 8 }), clip({ id: 'b', seconds: 8 })];
    const layout = layoutClips(clips, [], null, ltxDeliveredSeconds(24));
    // A point just inside the second block on the drawn axis must convert to a time inside the
    // second shot on the authored one — 8.0, not 8.042, is where `splitClipAt` counts from.
    expect(toAuthoredSeconds(layout, layout.clips[1].start + 0.01)).toBeGreaterThan(8);
    expect(toAuthoredSeconds(layout, layout.clips[1].start - 0.01)).toBeLessThan(8);
  });

  it('floors a zero-length shot rather than collapsing it out of the lane', () => {
    const layout = layoutClips([clip({ id: 'a', seconds: 0 })], [], null);
    expect(layout.clips[0].width).toBe(0.1);
  });

  /**
   * A plan the LLM drafted with an empty shot in the middle. It is its own render now, so what stops
   * the lane collapsing is the ENGINE's floor rather than the layout's: 0 s clamps up to the VAE's
   * 22 decodable frames.
   *
   * 🔴 **And at a 22-frame trim join those two numbers are the same number, so such a shot delivers
   * NOTHING.** It renders 22 frames, all 22 are the run-up, and delivery drops all 22 — the render
   * is spent and no picture reaches the cut. This is not a layout bug; it is the arithmetic the
   * engine will actually perform, and it became reachable on 2026-08-14 when `H3_TRIM_JOIN` moved
   * 1 → 22. It is asserted here so the state is on the record rather than discovered on a render.
   *
   * `MIN_CLIP_SECONDS` still floors the block, because a zero-width block cannot be clicked, and
   * that floor is the one place the lane is allowed to overlap: the plan fixes the NEXT shot's
   * start, so the floor has to come out of somewhere and taking it from a successor would move a
   * boundary the render already decided.
   *
   * ⚠️ Worth surfacing in the composer as "this shot delivers no frames". It is not, today.
   */
  it('renders a zero-length anchored shot into nothing at all, and floors the block anyway', () => {
    const { layout, plan } = laid([
      clip({ id: 'a', seconds: 4 }),
      clip({ id: 'b', seconds: 0 }),
      clip({ id: 'c', seconds: 4 })
    ]);
    // Its whole render is run-up: 22 frames in, 22 frames trimmed, nothing delivered.
    expect(plan.slices[1].runUpSeconds).toBeCloseTo(TRIM, 9);
    expect(plan.slices[2].deliveredAtSeconds).toBeCloseTo(plan.slices[1].deliveredAtSeconds, 9);
    expect(layout.clips[1].runUp).toBeCloseTo(TRIM, 9);
    expect(layout.clips[1].width).toBe(0.1);
    // Every other boundary is still exactly end-to-end.
    expect(layout.clips[1].start).toBeCloseTo(layout.clips[0].start + layout.clips[0].width, 9);
  });

  it('lays out an empty cut without inventing a span', () => {
    const layout = layoutClips([], [], null);
    expect(layout.clips).toEqual([]);
    expect(layout.end).toBe(0);
    expect(layout.drifts).toBe(false);
  });
});

/**
 * `LaidOutClip.runUp` — the seam the whole run-up UI hangs off.
 *
 * 🔴 The property that has to survive every future edit to this module: **adding the run-up changes
 * nothing about where anything is drawn.** `start` and `width` are byte-for-byte what they were; the
 * lane is still end-to-end; the ruler and the waveform still want no gap. It is a THIRD number,
 * carried so that a readout can name the clock a prompt mark is written on without re-deriving the
 * seam arithmetic a fourth time.
 */
describe('layoutClips — runUp', () => {
  it('matches the plan’s own run-up on every generation head', () => {
    const { plan, layout } = laid(oneShotEach(6));
    layout.clips.forEach((l, i) => expect(l.runUp).toBeCloseTo(plan.slices[i].runUpSeconds, 9));
    expect(layout.clips[0].runUp).toBe(0);
    expect(layout.clips[1].runUp).toBeCloseTo(TRIM, 9);
  });

  it('leaves the delivered lane exactly as it was — still end-to-end, still no gap', () => {
    const { layout } = laid(oneShotEach(6));
    for (let i = 1; i < layout.clips.length; i++) {
      const prev = layout.clips[i - 1];
      expect(layout.clips[i].start).toBeCloseTo(prev.start + prev.width, 9);
    }
  });

  /** The render window: `[start - runUp, start + width)`. It OVERLAPS its predecessor. */
  it('opens each render inside the previous block, not in front of it', () => {
    const { layout } = laid(oneShotEach(6));
    for (let i = 1; i < layout.clips.length; i++) {
      const prev = layout.clips[i - 1];
      const renderStart = layout.clips[i].start - layout.clips[i].runUp;
      expect(renderStart).toBeLessThan(layout.clips[i].start);
      expect(renderStart).toBeGreaterThan(prev.start);
      expect(renderStart).toBeLessThan(prev.start + prev.width);
    }
  });

  /**
   * `prompt_time(N, D) = (D − start) + runUp` — the arithmetic every readout on the timeline
   * implements, asserted against `shotClockAt`, which is the authority. If these two ever disagree
   * the picture and the number disagree, which is the exact class of bug the layout was moved onto
   * delivered time to end.
   */
  it('agrees with shotClockAt about the prompt clock, block by block', () => {
    const { gens, plan, layout } = laid(oneShotEach(6));
    layout.clips.forEach((l, i) => {
      for (const frac of [0, 0.3, 0.7, 0.99]) {
        const d = l.start + l.width * frac;
        const at = shotClockAt(plan, gens, d);
        expect(at.shot).toBe(i + 1);
        expect(at.localSeconds).toBeCloseTo(d - l.start + l.runUp, 9);
      }
    });
  });

  it('is zero everywhere nothing is anchored — LTX, and the unplaced path', () => {
    const ltx = layoutClips(
      [clip({ id: 'a', seconds: 8 }), clip({ id: 'b', seconds: 5 })],
      [],
      null,
      ltxDeliveredSeconds(24)
    );
    expect(ltx.clips.map((l) => l.runUp)).toEqual([0, 0]);
    expect(layoutClips([clip({ id: 'a' })], [], null).clips[0].runUp).toBe(0);
  });
});

describe('toAuthoredSeconds', () => {
  const cases = laid([clip({ id: 'a', seconds: 6 }), clip({ id: 'b', seconds: 4 })]);

  it('lands a delivered time in the same shot on the authored clock', () => {
    const { layout } = cases;
    // Mid-way through the tail shot, which is the one carrying the drift.
    const mid = layout.clips[1].start + layout.clips[1].width / 2;
    const authored = toAuthoredSeconds(layout, mid);
    expect(authored).toBeGreaterThan(6);
    expect(authored).toBeLessThan(10);
  });

  it('lands a boundary on the cut it was aimed at, so a split does not slice the wrong shot', () => {
    const { layout } = cases;
    for (const l of layout.clips) {
      expect(toAuthoredSeconds(layout, l.start)).toBeCloseTo(l.authoredStart, 9);
    }
  });

  it('clamps past either end instead of extrapolating off the cut', () => {
    const { layout } = cases;
    expect(toAuthoredSeconds(layout, -5)).toBe(0);
    expect(toAuthoredSeconds(layout, 1e6)).toBeCloseTo(10, 9);
  });

  it('is the identity when nothing drifts', () => {
    const layout = layoutClips([clip({ id: 'a', seconds: 4 })], [], null);
    expect(toAuthoredSeconds(layout, 2.5)).toBeCloseTo(2.5, 9);
  });
});

/**
 * The waveform envelope every audio strip in the Director draws, pinned here because it now has ONE
 * implementation (`@koblibs/ui`) where the timeline's two canvases and `WaveformTrim` each had their
 * own, and the phone strip added here would have been a fourth.
 *
 * 🔴 The stride is FRACTIONAL. An integer one drops up to `cols - 1` samples off the end of the bar,
 * so a track's final beat can be missing from the picture it was cut against — none of the merged
 * copies had that bug, and this is what stops one being written back in.
 */
describe('waveformPeaks', () => {
  it('covers every sample, including the tail an integer stride would drop', () => {
    // 5 samples into 2 columns: an integer stride of 2 would never look at index 4.
    const peaks = waveformPeaks(new Float32Array([0.1, 0.2, 0.3, 0.4, 0.9]), 2);
    expect(peaks[0]).toBeCloseTo(0.2, 6);
    expect(peaks[1]).toBeCloseTo(0.9, 6);
  });

  it('takes the absolute peak of each column and clamps it', () => {
    expect(Array.from(waveformPeaks(new Float32Array([0, -0.75, 0.25, 2]), 2))).toEqual([0.75, 1]);
    expect(Array.from(waveformPeaks(new Float32Array([0, -0.5, 0.25, 1]), 2))).toEqual([0.5, 1]);
  });

  it('answers an empty envelope rather than throwing on nothing to draw', () => {
    expect(waveformPeaks(null, 8)).toHaveLength(8);
    expect(waveformPeaks(new Float32Array(0), 8)).toHaveLength(8);
    expect(waveformPeaks(new Float32Array([1]), 0)).toHaveLength(0);
    expect(waveformPeaks(new Float32Array([1]), -3)).toHaveLength(0);
  });
});
