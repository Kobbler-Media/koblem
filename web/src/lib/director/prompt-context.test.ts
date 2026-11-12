import { describe, expect, it } from 'vitest';
import type {
  DirectorClip,
  DirectorH3Reference,
  LyricWord,
  StoryContext
} from '#lib/api/client.ts';
import { packGenerations } from './h3-generations';
import { H3_SOUNDTRACK_REF_ID, H3_SOUNDTRACK_SENTINEL, H3_TRIM_JOIN } from './h3-soundtrack';
import { layoutClips } from './timeline-layout';
import {
  aspectPhrase,
  bareAssetHash,
  beatWordsFor,
  clipHasContent,
  editorShots,
  shotContext,
  shotHasSpeech,
  shotHeading,
  shotOneLiner,
  shotWantsText,
  promptLayout,
  storyIsEmpty,
  vocalLanes,
  type PromptContext
} from './prompt-context';

function clip(id: string, over: Partial<DirectorClip> = {}): DirectorClip {
  return { id, prompt: `prompt ${id}`, mode: 't2v', seconds: 4, keyframes: [], ...over };
}

const emptyStory = (): StoryContext => ({
  backstory: '',
  tone: '',
  rules: '',
  references: []
});

function ctx(over: Partial<PromptContext> = {}): PromptContext {
  return {
    clips: [clip('a'), clip('b'), clip('c')],
    characters: [],
    story: emptyStory(),
    lyrics: [],
    fps: 24,
    audioKind: 'none',
    ...over
  };
}

describe('storyIsEmpty', () => {
  it('whitespace is still empty', () => {
    expect(storyIsEmpty({ ...emptyStory(), tone: '   ' })).toBe(true);
  });

  it('a reference with only an image still counts as canon', () => {
    expect(
      storyIsEmpty({
        ...emptyStory(),
        references: [{ label: '', description: '', image_b64: 'img' }]
      })
    ).toBe(false);
  });

  it('a wholly blank reference does not', () => {
    expect(
      storyIsEmpty({
        ...emptyStory(),
        references: [{ label: ' ', description: '', image_b64: null }]
      })
    ).toBe(true);
  });
});

describe('editorShots', () => {
  /** A shot sent without an id can never be edited by the result that comes back keyed to one. */
  it('always carries the stable id', () => {
    expect(editorShots([clip('a')])[0].id).toBe('a');
  });

  it('drops shots with nothing written in them', () => {
    const clips = [
      clip('a'),
      clip('blank', { prompt: '  ' }),
      clip('idea', { prompt: '', idea: 'x' })
    ];
    expect(editorShots(clips).map((s) => s.id)).toEqual(['a', 'idea']);
    expect(clipHasContent(clips[1])).toBe(false);
  });

  it('trims and omits empty text fields rather than sending blanks', () => {
    const s = editorShots([clip('a', { prompt: ' p ', idea: '  ', gist: ' g ' })])[0];
    expect(s).toMatchObject({ prompt: 'p', gist: 'g' });
    expect(s.idea).toBeUndefined();
  });
});

describe('shot summaries', () => {
  it('heads a shot with its most human field', () => {
    expect(shotHeading(clip('a', { gist: 'g', idea: 'i' }))).toBe('g');
    expect(shotHeading(clip('a', { idea: 'i' }))).toBe('i');
    expect(shotHeading(clip('a', { prompt: '', idea: '' }))).toBe('Untitled shot');
  });

  it('truncates a long prompt down to a one-liner', () => {
    const long = Array.from({ length: 20 }, (_, i) => `w${i}`).join(' ');
    expect(shotOneLiner(clip('a', { prompt: long }))).toBe(
      Array.from({ length: 16 }, (_, i) => `w${i}`).join(' ') + '…'
    );
  });

  it('leaves a short prompt alone and says nothing for an empty shot', () => {
    expect(shotOneLiner(clip('a', { prompt: 'short one' }))).toBe('short one');
    expect(shotOneLiner(clip('a', { prompt: '' }))).toBe('');
  });
});

describe('speech and beat words', () => {
  const lyrics: LyricWord[] = [
    { text: 'one', start: 1, end: 1.4 },
    { text: 'two', start: 5, end: 5.4 },
    { text: 'three', start: 9, end: 9.4 }
  ];
  const clips = [clip('a'), clip('b'), clip('c')];
  // An UNSNAPPED layout, so these cases test the window rules on round numbers. No LTX shot length
  // is ever integral — `8k+1` is odd and 24 is even, so 4 s renders as 97 frames (4.0417 s) and
  // there is no length for which it does not. The grid gets its own cases below.
  const flat = layoutClips(clips, [], null);

  it('answers whether anyone speaks inside a shot span', () => {
    expect(shotHasSpeech(flat, lyrics, 'a')).toBe(true);
    expect(shotHasSpeech(flat, lyrics, 'b')).toBe(true);
    expect(shotHasSpeech(flat, [], 'a')).toBe(false);
  });

  it('reads a shot nobody is talking over as silent', () => {
    expect(shotHasSpeech(flat, [{ text: 'one', start: 1, end: 1.4 }], 'b')).toBe(false);
  });

  /** A word held across the cut is mouth-movement in BOTH shots. Testing only its start called the
   *  second one silent, and the writer was then told to write for a closed mouth over a lip-sync. */
  it('counts a word that runs across the cut as speech in both shots', () => {
    const spanning: LyricWord[] = [{ text: 'heeeld', start: 3.6, end: 4.4 }];
    expect(shotHasSpeech(flat, spanning, 'a')).toBe(true);
    expect(shotHasSpeech(flat, spanning, 'b')).toBe(true);
  });

  /** Half-open, matching beatWordsFor: a word that lands exactly ON the cut belongs to one side. */
  it.each([
    ['ends on the cut', { text: 'edge', start: 3.6, end: 4 }, true, false],
    ['starts on the cut', { text: 'go', start: 4, end: 4.5 }, false, true]
  ])('does not double-count a word that %s', (_name, word, first, second) => {
    expect(shotHasSpeech(flat, [word as LyricWord], 'a')).toBe(first);
    expect(shotHasSpeech(flat, [word as LyricWord], 'b')).toBe(second);
  });

  it('re-bases the words inside a shot onto that shot own clock', () => {
    expect(beatWordsFor(flat, lyrics, 'b').map((w) => [w.text, w.offset])).toEqual([['two', 1]]);
  });

  it('has nothing to snap to without a transcript', () => {
    expect(beatWordsFor(flat, [], 'a')).toEqual([]);
    expect(beatWordsFor(flat, lyrics, 'nope')).toEqual([]);
  });

  /**
   * 🔴 The window has to be the DELIVERED one. Both of these used to sum `clip.seconds`, which is
   * the authored clock, and compare it against a transcript that is on the SONG clock — so late in
   * a cut the writer was told the wrong shot was singing and "snap a beat to the word" offered the
   * wrong words. Same two-clocks defect as the timeline's, in the data rather than the picture.
   *
   * Ten 5 s shots at 24 fps: LTX snaps each to 121 frames (5.0417 s), so shot 11 really starts at
   * 50.417 s while the authored sum says 50.0. A word at 50.2 s is in shot 10, not shot 11.
   */
  it('windows a shot on delivered time, not on the authored sum', () => {
    const many = Array.from({ length: 12 }, (_, i) => clip(`s${i}`, { seconds: 5 }));
    const layout = promptLayout({ clips: many, fps: 24 });
    // Wholly inside shot 10 and clear of the real cut — `shotHasSpeech` is overlap-based, so a word
    // straddling the boundary would legitimately be speech in both and prove nothing.
    const word: LyricWord[] = [{ text: 'late', start: 50.2, end: 50.35 }];
    expect(shotHasSpeech(layout, word, 's9')).toBe(true);
    expect(shotHasSpeech(layout, word, 's10')).toBe(false);
    expect(beatWordsFor(layout, word, 's9').map((w) => w.text)).toEqual(['late']);
    expect(beatWordsFor(layout, word, 's10')).toEqual([]);
  });

  /**
   * 🔴 The offset is UNSCALED — the raw distance from where the block is DRAWN, not a fraction of
   * `clip.seconds`. A generation's round-up lands on the tail of its shot, so every moment before
   * that tail maps 1:1 between the drawn axis and the clock the render (and therefore a
   * `[Shot N] At …` mark) runs on. Rescaling would move every beat in the shot to buy nothing.
   *
   * 🔴 …and the RUN-UP is added, which is a SHIFT rather than a scale. b carries an anchored
   * continuation, so its render opens 22 frames before its block and delivery trims them: the word
   * is 1.5 s into the picture and 2.417 s into the render, and 2.417 is the number that has to go
   * into the prompt for it. Before 2026-08-14 the trim was one frame and this read 1.542; the shape
   * of the answer has not changed, only how much it now matters.
   */
  it('measures the offset from the render start, unscaled, run-up included', () => {
    const two = [clip('a', { seconds: 6 }), clip('b', { seconds: 4 })];
    const packed = packGenerations({ clips: two, references: [] });
    const layout = promptLayout({ clips: two, fps: 24, h3Packing: packed });
    // Each shot is its own render: 6 s = 144 frames → 158, so b is DRAWN at 6.583 rather than at
    // the 6.0 the authored sum claims. The ratio of b's own snap-up (107/96) never enters into it.
    const bStart = 158 / 24;
    expect(layout.byId.get('b')?.start).toBeCloseTo(bStart, 9);
    const runUp = layout.byId.get('b')!.runUp;
    expect(runUp).toBeCloseTo(H3_TRIM_JOIN / 24, 9);
    const w: LyricWord[] = [{ text: 'x', start: bStart + 1.5, end: bStart + 1.7 }];
    expect(beatWordsFor(layout, w, 'b')[0].offset).toBeCloseTo(1.5 + runUp, 9);
  });

  /** The opening shot has no run-up, so its words are still measured straight off its block. */
  it('leaves the opening shot’s offsets as plain distances into the block', () => {
    const two = [clip('a', { seconds: 6 }), clip('b', { seconds: 4 })];
    const packed = packGenerations({ clips: two, references: [] });
    const layout = promptLayout({ clips: two, fps: 24, h3Packing: packed });
    expect(layout.byId.get('a')!.runUp).toBe(0);
    const w: LyricWord[] = [{ text: 'x', start: 2.25, end: 2.4 }];
    expect(beatWordsFor(layout, w, 'a')[0].offset).toBeCloseTo(2.25, 9);
  });
});

describe('shotContext', () => {
  it('feeds every earlier shot in as the story so far, and the immediate neighbours verbatim', () => {
    const c = ctx();
    const out = shotContext(c, c.clips[1], 1);
    expect(out.prior_shots).toEqual(['prompt a']);
    expect(out.prev_prompt).toBe('prompt a');
    expect(out.next_prompt).toBe('prompt c');
  });

  /** A continuation shot renders from the previous shot's final frame, so it must be described as a
   *  change to a standing scene rather than as a fresh one. */
  it('marks a plain shot past the first as a continuation', () => {
    const c = ctx();
    expect(shotContext(c, c.clips[0], 0).continuation).toBe(false);
    expect(shotContext(c, c.clips[1], 1).continuation).toBe(true);
  });

  it('a hard cut is not a continuation', () => {
    const clips = [clip('a'), clip('b', { new_scene: true })];
    expect(shotContext(ctx({ clips }), clips[1], 1).continuation).toBe(false);
  });

  it('says nothing about speech when there is no transcript at all', () => {
    const c = ctx();
    expect('speech_in_shot' in shotContext(c, c.clips[0], 0)).toBe(false);
    const withLyrics = ctx({ lyrics: [{ text: 'x', start: 0.5, end: 1 }] });
    expect(shotContext(withLyrics, withLyrics.clips[0], 0).speech_in_shot).toBe(true);
  });

  /** The path that actually reaches the writer: a word carried over the cut has to arrive as
   *  speech in the shot it is still being said in. */
  it('tells the writer a word held across the cut is speech in the shot it lands in', () => {
    const c = ctx({ lyrics: [{ text: 'heeeld', start: 3.6, end: 4.4 }] });
    expect(shotContext(c, c.clips[1], 1).speech_in_shot).toBe(true);
    expect(shotContext(c, c.clips[2], 2).speech_in_shot).toBe(false);
  });

  /** A shot with its own clip has audio even when the project track is empty. */
  it('reports audio from either the project track or the shot own clip', () => {
    const c = ctx();
    expect(shotContext(c, c.clips[0], 0).audio_supplied).toBe(false);
    expect(shotContext(ctx({ audioKind: 'voice' }), c.clips[0], 0).audio_supplied).toBe(true);
    expect(shotContext(c, clip('a', { audio_b64: 'ref' }), 0).audio_supplied).toBe(true);
  });

  it('labels the opening image differently from a later scene image', () => {
    const c = ctx();
    const i2v = clip('a', { mode: 'i2v', image_b64: 'img' });
    expect(shotContext(c, i2v, 0).images?.[0].role).toBe('opening');
    expect(shotContext(c, i2v, 2).images?.[0].role).toBe('scene');
  });

  it('drops every image when the caller asks for text only', () => {
    const c = ctx();
    const rich = clip('a', {
      mode: 'i2v',
      image_b64: 'img',
      keyframes: [{ image_b64: 'k', seconds: 1 }],
      references: [{ image_b64: 'r', role: 'reference' }]
    });
    const out = shotContext(c, rich, 0, false);
    expect(out.images).toBeUndefined();
    expect(out.references).toBeUndefined();
  });

  /** The beats are conditioned separately, so the writer is told about them to keep them OUT of the
   *  prompt. `label` is editor-only. */
  it('passes beats with their text trimmed and no editor label', () => {
    const c = ctx();
    const beaty = clip('a', {
      beats: [
        { seconds: 1, text: ' lands ', label: 'chorus' },
        { seconds: 2, text: '  ' }
      ]
    });
    expect(shotContext(c, beaty, 0).beats).toEqual([{ seconds: 1, text: 'lands' }]);
  });

  it('omits an empty story rather than sending blank canon', () => {
    const c = ctx();
    expect('story' in shotContext(c, c.clips[0], 0)).toBe(false);
    const told = ctx({ story: { ...emptyStory(), tone: 'neon noir' } });
    expect(shotContext(told, told.clips[0], 0).story?.tone).toBe('neon noir');
  });
});

describe('aspectPhrase', () => {
  it('names the ratios the corpus actually uses', () => {
    expect(aspectPhrase(1344, 768)).toBe('16:9 landscape');
    expect(aspectPhrase(768, 1344)).toBe('9:16 vertical');
    expect(aspectPhrase(1024, 1024)).toBe('1:1 square');
  });

  /** A made-up "1.87:1" in a prompt is a number the model has no reason to have seen. */
  it('falls back to orientation rather than inventing an odd ratio', () => {
    expect(aspectPhrase(1600, 900)).toBe('16:9 landscape');
    expect(aspectPhrase(1500, 800)).toBe('landscape');
  });

  it('says nothing without a geometry', () => {
    expect(aspectPhrase(undefined, 768)).toBeUndefined();
  });
});

describe('shotWantsText', () => {
  it('spots a shot that puts something on screen', () => {
    expect(shotWantsText(clip('a', { prompt: 'the TITLE resolves out of the dark' }))).toBe(true);
    expect(shotWantsText(clip('a', { idea: 'game menu slides in' }))).toBe(true);
  });

  it('leaves an ordinary shot alone', () => {
    expect(shotWantsText(clip('a', { prompt: 'she walks to the window' }))).toBe(false);
  });
});

describe('shotContext on H3', () => {
  /** LTX's chain grounding is not merely irrelevant here, it is wrong: there is no carried frame
   *  for `continuation`/`closes_scene` to describe, and no supplied track for `audio_supplied`. */
  it('drops the chain fields LTX sends', () => {
    const c = ctx({ engine: 'h3', width: 1344, height: 768 });
    const out = shotContext(c, c.clips[1], 1);
    expect(out.engine).toBe('h3');
    expect('continuation' in out).toBe(false);
    expect('closes_scene' in out).toBe(false);
    expect('audio_supplied' in out).toBe(false);
    expect('beats' in out).toBe(false);
    expect(out.aspect).toBe('16:9 landscape');
  });

  /**
   * 🔴 A SHOT is not a render on H3. Several shots pack into one ~15 s generation and only the
   * GENERATION snaps onto the 17n+5 grid, absorbing the rounding into its last shot. This used to
   * report the snapped number, which told the writer every 4 s shot was 4.46 s long and had it
   * place its cues accordingly.
   */
  it('reports the shot length it was authored at, unsnapped', () => {
    const c = ctx({ engine: 'h3' });
    expect(shotContext(c, c.clips[0], 0).seconds).toBe(4);
  });

  /**
   * 🔴 **Scope lives on the RENDER UNIT and `segments` has had no writer since Stage 5.**
   *
   * `reference_ids` on the generation is the only granularity the conditioner has —
   * `pipeline.py:311-340` builds one reference presentation per CALL. Reading `segments` instead
   * made a generation-level narrowing invisible to the drafting model: every project reference
   * looked in scope, so the writer over-included and named a `<Picture N>` the render was never
   * going to carry. The prose resolves against nothing and the render comes back describing the
   * wrong asset, with no error anywhere.
   */
  it('takes the roster from the PACKING when there is one, so `reference_ids` is honoured', () => {
    const clips = [clip('a'), clip('b')];
    const references: DirectorH3Reference[] = [
      { id: '1', kind: 'image', label: 'mood' },
      { id: '2', kind: 'image', label: 'hero' }
    ];
    // The generation narrows itself to the second reference alone. `segments` says nothing, so the
    // legacy path would report BOTH.
    const h3Packing = packGenerations({
      clips,
      references,
      stored: [{ id: 'gen:a', clip_ids: ['a', 'b'], reference_ids: ['2'] }]
    });
    const c = ctx({ engine: 'h3', clips, h3References: references, h3Packing });
    expect(shotContext(c, clips[0], 0).h3_references).toEqual([
      { kind: 'image', has_audio: false, label: 'hero' }
    ]);
    // Without the packing the same document reads as unscoped — the behaviour a caller that has no
    // packing still gets, rather than an empty roster, which would be a different wrong answer.
    expect(
      shotContext(ctx({ engine: 'h3', clips, h3References: references }), clips[0], 0).h3_references
    ).toHaveLength(2);
  });

  /** A keyframe picture is appended to the render's reference list by the packer and TAKES AN
   *  ORDINAL, so the writer has to know about it or it will number every later picture wrong. */
  it('includes the keyframe picture the packer synthesised', () => {
    const clips = [clip('a', { mode: 'i2v', image_b64: 'img', h3_frame_mode: 'keyframe' })];
    const references: DirectorH3Reference[] = [{ id: '1', kind: 'image', label: 'mood' }];
    const h3Packing = packGenerations({ clips, references });
    const out = shotContext(
      ctx({ engine: 'h3', clips, h3References: references, h3Packing }),
      clips[0],
      0
    );
    expect(out.h3_references).toHaveLength(2);
    expect(out.h3_references?.[1].role).toBe('frame_anchor');
  });

  /** Ordinals restart at 1 per shot over the in-scope list, so the roster must be narrowed here or
   *  the prompt addresses the wrong reference. */
  it('scopes the references to this shot, in order', () => {
    const c = ctx({
      engine: 'h3',
      h3References: [
        { id: '1', kind: 'image', segments: [0], label: 'mood' },
        { id: '2', kind: 'image', label: 'hero' },
        { id: '3', kind: 'audio', segments: [] }
      ]
    });
    expect(shotContext(c, c.clips[1], 1).h3_references).toEqual([
      { kind: 'image', has_audio: false, label: 'hero' }
    ]);
    expect(shotContext(c, c.clips[0], 0).h3_references).toEqual([
      { kind: 'image', has_audio: false, label: 'mood' },
      { kind: 'image', has_audio: false, label: 'hero' }
    ]);
  });

  /**
   * The writers were told each reference's label and never shown the picture, so every character
   * was described from a caption and the reference image then fought the prose. The picture rides
   * with the brief now; a sound or video reference has no picture to send.
   */
  it('sends an image reference its picture, and nothing else gets one', () => {
    const c = ctx({
      engine: 'h3',
      h3References: [
        { id: '1', kind: 'image', label: 'hero', image: 'asset:abc' },
        { id: '2', kind: 'audio', audio: 'asset:def' }
      ]
    });
    const refs = shotContext(c, c.clips[0], 0).h3_references ?? [];
    expect(refs[0]).toEqual({ kind: 'image', has_audio: false, label: 'hero', image: 'asset:abc' });
    expect(refs[1]).not.toHaveProperty('image');
  });

  /**
   * `fl2va` and `ref2va` are different weight sets, so a pinned shot carries no references at all —
   * and the PIN wins, because it is the thing the author placed by hand. Telling the writer about
   * references the render will not carry gets prose addressing `<Picture 1>` on a checkpoint that
   * was never given one.
   */
  it('hides every reference from a frame-pinned shot and keeps its anchor', () => {
    const withRef = ctx({
      engine: 'h3',
      h3References: [{ id: '1', kind: 'image' }]
    });
    const pinned = clip('a', { h3_end_image: 'asset:abc' });
    expect('h3_references' in shotContext(withRef, pinned, 0)).toBe(false);
    expect(shotContext(withRef, pinned, 0).h3_end_image).toBe(true);
    expect(shotContext(ctx({ engine: 'h3' }), pinned, 0).h3_end_image).toBe(true);
  });

  it('carries the planner audio intent through', () => {
    const c = ctx({ engine: 'h3' });
    const noisy = clip('a', { h3_audio: '  rain on the awning, no music ' });
    expect(shotContext(c, noisy, 0).h3_audio).toBe('rain on the awning, no music');
  });
});

// ── The vocal lane ───────────────────────────────────────────────────────────

/**
 * The real project, reduced to what the lane reads: "Gawdwin — Sealed by Fate", fifteen 345-frame
 * generations under a 208.008 s track, every seam after the first an anchored `latents` join.
 */
const SONG_SECONDS = 208.008;
const SOUNDTRACK_REF: DirectorH3Reference = {
  id: H3_SOUNDTRACK_REF_ID,
  kind: 'audio',
  role: 'audio_reuse',
  audio: H3_SOUNDTRACK_SENTINEL
};

/** `n` generations of exactly 345 frames, each holding one clip, all in scope for the song. */
function sealedByFate(n: number, references: DirectorH3Reference[] = [SOUNDTRACK_REF]) {
  const clips = Array.from({ length: n }, (_, i) => clip(`g${i}`, { seconds: 345 / 24 }));
  return {
    clips,
    packed: packGenerations({ clips, references, hasSoundtrack: true })
  };
}

const words = (...spec: [string, number, number][]): LyricWord[] =>
  spec.map(([text, start, end]) => ({ text, start, end }));

describe('vocalLaneFor / vocalLanes', () => {
  it('packs the worked example into 345-frame generations', () => {
    const { packed } = sealedByFate(3);
    expect(packed.map((g) => g.frames)).toEqual([345, 345, 345]);
    expect(packed[0].continuation).toBeNull();
    expect(packed[1].continuation?.anchor).toBe(true);
  });

  /**
   * 🔴 THE CLOCK, which is the one thing that has to be right or every number in the brief is
   * wrong. Generation 1's slice starts at song 13.458 s — `songOffsetSeconds`, one RUN-UP before
   * its delivered start of 14.375 — so the song's first lyric at 22.80 s lands at LOCAL 9.342 s.
   * That is the number a `[Shot N] At 00:09.342` mark carries.
   *
   * ⚠️ The naive answer is 22.80 − 14.375 = 8.425, and it is wrong by a run-up here and by a run-up
   * per seam thereafter. koblem's own `offsets=[…]` log prints the DELIVERED clock, so reading it as
   * the slice offsets reproduces exactly this error.
   *
   * 🔴 These numbers moved on 2026-08-14 with `H3_TRIM_JOIN` 1 → 22 (comfy-api's continuation went
   * to `placement="on_target"`). They used to be 14.3333 and 8.467. Derived from the
   * constant here so this file cannot go stale the same way twice.
   */
  it('converts the song clock onto each generation own local clock', () => {
    const { packed } = sealedByFate(15);
    const lyrics = words(['sealed', 22.8, 23.2], ['by', 23.25, 23.5], ['fate', 23.55, 24.0]);
    const lanes = vocalLanes({
      lyrics,
      h3Packing: packed,
      engine: 'h3',
      songSeconds: SONG_SECONDS
    });
    expect(lanes).toHaveLength(15);
    // Generation 0 covers song 0–14.375: nothing sung there.
    expect(lanes[0].spans).toEqual([]);
    // Generation 1's slice runs from song 13.458 (its render start, run-up included) and holds the
    // whole phrase.
    const gen1Offset = 14.375 - H3_TRIM_JOIN / 24;
    expect(lanes[1].spans).toHaveLength(1);
    expect(lanes[1].spans[0].start).toBeCloseTo(22.8 - gen1Offset, 3);
    expect(lanes[1].spans[0].end).toBeCloseTo(24.0 - gen1Offset, 3);
    // The three words are one phrase (no gap over 0.45 s), quoted at the entry.
    expect(lanes[1].spans[0].words).toBe('sealed by fate');
    // …and every later generation is instrumental, which is the arm that was silently wrong.
    for (let i = 2; i < 15; i++) expect(lanes[i].spans).toEqual([]);
  });

  /**
   * 🔴 THE DEFECT. Three generations of the reviewed project have no vocal at all — the song's
   * voice stops 37 s before the video does — and each was written with *"its mouth moves in perfect
   * synchronization with the male vocals"* as a standing fact.
   *
   * An empty `spans` is a MEASUREMENT ("we looked; there is none"), which is why the lane is an
   * object: `[]` on its own could not be told apart from "nobody measured", and only the second one
   * should leave the server on its old boolean.
   */
  it('reports an instrumental generation as a lane with no spans, never as no lane', () => {
    const { packed } = sealedByFate(15);
    const lanes = vocalLanes({
      lyrics: words(['hey', 22.8, 23.2]),
      h3Packing: packed,
      engine: 'h3',
      songSeconds: SONG_SECONDS
    });
    // Present, numbered, and sized — a lane that says "nothing here", not an absence. The run-up
    // rides every anchored generation, instrumental or not, and is derived rather than spelled:
    // a written-out 0.917 is what went stale when the trim moved 1 → 22.
    expect(lanes[13]).toEqual({
      generation: 14,
      seconds: 345 / 24,
      run_up_seconds: H3_TRIM_JOIN / 24,
      spans: []
    });
  });

  /** A phrase straddling a boundary is half in each generation, and each half is local to its own. */
  it('clips a phrase that runs across a generation boundary', () => {
    const { packed } = sealedByFate(2);
    // 13.0 → 15.5 in the song. Gen 1's slice reaches BACK by its run-up, but the LANE is clipped
    // to that floor: those frames are trimmed and, from the viewer's side, they are gen 0's
    // picture. So gen 1's half opens at the run-up, not at local 0 — and `h3_prompting`'s
    // "earliest markable time" clause is written assuming exactly this clip.
    const lanes = vocalLanes({
      lyrics: words(['heeeeey', 13.0, 15.5]),
      h3Packing: packed,
      engine: 'h3',
      songSeconds: SONG_SECONDS
    });
    expect(lanes[0].spans[0].start).toBeCloseTo(13.0, 6);
    expect(lanes[0].spans[0].end).toBeCloseTo(14.375, 6);
    expect(lanes[1].spans[0].start).toBeCloseTo(H3_TRIM_JOIN / 24, 6);
    expect(lanes[1].spans[0].end).toBeCloseTo(15.5 - (14.375 - H3_TRIM_JOIN / 24), 6);
  });

  /** 0.45 s is the split, matching the threshold the measured table was produced with. */
  it('splits a lane on a real breath and not on the gap between two words of one phrase', () => {
    const { packed } = sealedByFate(1);
    const lanes = vocalLanes({
      lyrics: words(['one', 1.0, 1.3], ['two', 1.6, 1.9], ['three', 3.0, 3.4]),
      h3Packing: packed,
      engine: 'h3',
      songSeconds: SONG_SECONDS
    });
    // 1.3 → 1.6 is 0.30 s and joins; 1.9 → 3.0 is 1.10 s and splits.
    expect(lanes[0].spans.map((s) => [s.start, s.end])).toEqual([
      [1.0, 1.9],
      [3.0, 3.4]
    ]);
    expect(lanes[0].spans[0].words).toBe('one two');
  });

  /**
   * Taking a generation out of the song is ordinary reference scoping, and a generation with no
   * slice renders on the model's own bed — so there is no song vocal in it to perform, whatever the
   * transcript says about that stretch of the track.
   */
  it('gives a generation scoped out of the soundtrack an empty lane', () => {
    const { packed } = sealedByFate(2, []);
    const lanes = vocalLanes({
      lyrics: words(['sung', 1.0, 2.0], ['later', 20.0, 21.0]),
      h3Packing: packed,
      engine: 'h3',
      songSeconds: SONG_SECONDS
    });
    expect(lanes.every((l) => l.spans.length === 0)).toBe(true);
  });

  /** ⚠️ No transcript, no packing or not H3 ⇒ nothing sent ⇒ the server keeps the prompt it had. */
  it('sends nothing when there is nothing measured', () => {
    const { packed } = sealedByFate(2);
    expect(vocalLanes({ lyrics: [], h3Packing: packed, engine: 'h3', songSeconds: 100 })).toEqual(
      []
    );
    expect(
      vocalLanes({
        lyrics: words(['x', 1, 2]),
        h3Packing: [],
        engine: 'h3',
        songSeconds: 100
      })
    ).toEqual([]);
    expect(
      vocalLanes({
        lyrics: words(['x', 1, 2]),
        h3Packing: packed,
        engine: 'ltx25',
        songSeconds: 100
      })
    ).toEqual([]);
  });
});

describe('shotContext vocal lane', () => {
  function h3Ctx(over: Partial<PromptContext> = {}): { c: PromptContext; clips: DirectorClip[] } {
    const { clips, packed } = sealedByFate(15);
    return {
      clips,
      c: ctx({
        engine: 'h3',
        clips,
        h3Packing: packed,
        h3References: [SOUNDTRACK_REF],
        lyrics: words(['sealed', 22.8, 23.2], ['by', 23.25, 23.5]),
        songSeconds: SONG_SECONDS,
        ...over
      })
    };
  }

  /**
   * One lane, for THIS generation, and deliberately unnumbered: the single-shot path describes one
   * render, and numbering it would invite the writer to address a neighbour it has not been told
   * about.
   */
  it('sends exactly one unnumbered lane, on this generation own clock', () => {
    const { c, clips } = h3Ctx();
    const lanes = shotContext(c, clips[1], 1).h3_vocal_lanes;
    expect(lanes).toHaveLength(1);
    expect(lanes?.[0].generation).toBeUndefined();
    // Generation 1's clock starts at its RENDER, one run-up before its delivered block.
    expect(lanes?.[0].spans[0].start).toBeCloseTo(22.8 - (14.375 - H3_TRIM_JOIN / 24), 3);
  });

  /** The generation whose slice is instrumental gets the lane that says so. */
  it('says so when this shot generation has no vocal in it', () => {
    const { c, clips } = h3Ctx();
    expect(shotContext(c, clips[13], 13).h3_vocal_lanes?.[0].spans).toEqual([]);
  });

  /**
   * `speech_in_shot` still rides along. The server IGNORES it whenever a lane is present — they are
   * read off the same transcript, so they can only agree — but a koblem that predates the lane must
   * keep the behaviour it was written against.
   */
  it('keeps sending the old boolean beside it', () => {
    const { c, clips } = h3Ctx();
    expect('speech_in_shot' in shotContext(c, clips[1], 1)).toBe(true);
  });

  /** ⚠️ Absent lane ⇒ the request is exactly what it was: no transcript, nothing new on the wire. */
  it('sends no lane at all when the project has no transcript', () => {
    const { c, clips } = h3Ctx({ lyrics: [] });
    const out = shotContext(c, clips[1], 1);
    expect('h3_vocal_lanes' in out).toBe(false);
    expect('speech_in_shot' in out).toBe(false);
  });

  /**
   * 🔴 The song rides as a BARE hash. koblem's request edge rewrites every `asset:<hash>` string in
   * a body into the base64 of its bytes, so a prefixed field would inline the whole song into a
   * request that wants fourteen seconds of it.
   */
  it('sends the song as a bare hash with this generation offset and slice length', () => {
    const hash = 'a'.repeat(64);
    const { c, clips } = h3Ctx({ songAsset: `asset:${hash}` });
    const out = shotContext(c, clips[1], 1);
    expect(out.h3_song_asset).toBe(hash);
    expect(out.h3_song_offset_seconds).toBeCloseTo(14.375 - H3_TRIM_JOIN / 24, 6);
    expect(out.h3_song_slice_seconds).toBeCloseTo(345 / 24, 6);
  });

  it('sends no song fields for a legacy inline track or a project with none', () => {
    const inline = h3Ctx({ songAsset: 'SUQzBAAAAAA' });
    expect('h3_song_asset' in shotContext(inline.c, inline.clips[1], 1)).toBe(false);
    const none = h3Ctx();
    expect('h3_song_asset' in shotContext(none.c, none.clips[1], 1)).toBe(false);
  });
});

describe('bareAssetHash', () => {
  it('strips the prefix, accepts a bare hash, and refuses everything else', () => {
    const hash = 'b'.repeat(64);
    expect(bareAssetHash(`asset:${hash}`)).toBe(hash);
    expect(bareAssetHash(hash)).toBe(hash);
    expect(bareAssetHash(`  asset:${hash}  `)).toBe(hash);
    // Uppercase is not the store's spelling; short/long is not a hash; base64 is a legacy payload.
    expect(bareAssetHash(`asset:${'B'.repeat(64)}`)).toBeUndefined();
    expect(bareAssetHash('asset:abc')).toBeUndefined();
    expect(bareAssetHash('SUQzBAAAAAA')).toBeUndefined();
    expect(bareAssetHash(null)).toBeUndefined();
    expect(bareAssetHash(undefined)).toBeUndefined();
  });
});
