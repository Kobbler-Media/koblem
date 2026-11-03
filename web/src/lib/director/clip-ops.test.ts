import { describe, expect, it } from 'vitest';
import type { DirectorClip } from '#lib/api/client.ts';
import {
  deleteClips,
  duplicateClips,
  insertClip,
  mergeClips,
  splitClipAt,
  splitClipForCut
} from './clip-ops';

function clip(id: string, over: Partial<DirectorClip> = {}): DirectorClip {
  return { id, prompt: id, mode: 't2v', seconds: 4, keyframes: [], ...over };
}

const three = () => [clip('a'), clip('b'), clip('c')];

describe('insertClip', () => {
  it('lands the new shot right after the primary selection and selects it', () => {
    const out = insertClip(three(), 'a', 4);
    expect(out.clips.map((c) => c.id).slice(0, 2)).toEqual(['a', out.clips[1].id]);
    expect(out.clips).toHaveLength(4);
    expect(out.selectedIds).toEqual([out.clips[1].id]);
  });

  it('appends when nothing is selected', () => {
    const out = insertClip(three(), null, 6);
    expect(out.clips[3].seconds).toBe(6);
    expect(out.selectedIds).toEqual([out.clips[3].id]);
  });

  /** A stale primary id is not a position, so it has to mean "no selection" and append — a raw
   *  findIndex would resolve it to -1 and put the new shot at the HEAD. Unreachable in the editor
   *  (the selection is pruned on every structural edit) but pinned so a change to the lookup cannot
   *  move shots silently. */
  it('appends when the primary id no longer exists', () => {
    const out = insertClip(three(), 'gone', 4);
    expect(out.clips.map((c) => c.id).slice(0, 3)).toEqual(['a', 'b', 'c']);
    expect(out.clips).toHaveLength(4);
    expect(out.clips[3].id).toBe(out.selectedIds[0]);
  });
});

describe('deleteClips', () => {
  it('selects whatever slid into the first deleted slot', () => {
    expect(deleteClips(three(), ['a'])).toMatchObject({ selectedIds: ['b'] });
    expect(deleteClips(three(), ['b'])).toMatchObject({ selectedIds: ['c'] });
  });

  /** Deleting the tail has nothing after it, so the selection has to walk backwards or it points at
   *  a shot that is not there. */
  it('falls back to the new last shot when the tail goes', () => {
    expect(deleteClips(three(), ['c'])).toMatchObject({ selectedIds: ['b'] });
  });

  it('leaves an empty selection when everything goes', () => {
    expect(deleteClips(three(), ['a', 'b', 'c'])).toEqual({ clips: [], selectedIds: [] });
  });
});

describe('duplicateClips', () => {
  it('copies each selected shot after itself and keeps order', () => {
    const out = duplicateClips(three(), ['a', 'c']);
    expect(out.clips.map((c) => c.prompt)).toEqual(['a', 'a', 'b', 'c', 'c']);
    expect(out.selectedIds).toEqual([out.clips[1].id, out.clips[4].id]);
  });

  it('gives the copy a fresh id and does not share nested objects', () => {
    const src = [clip('a', { keyframes: [{ image_b64: 'x', seconds: 1 }] })];
    const out = duplicateClips(src, ['a']);
    expect(out.clips[1].id).not.toBe('a');
    out.clips[1].keyframes[0].seconds = 3;
    expect(src[0].keyframes[0].seconds).toBe(1);
  });
});

describe('mergeClips', () => {
  it('refuses a broken run', () => {
    expect(mergeClips(three(), ['a', 'c'])).toBeNull();
  });

  it('refuses a single shot', () => {
    expect(mergeClips(three(), ['a'])).toBeNull();
  });

  it('joins prompts and sums durations', () => {
    const out = mergeClips(three(), ['a', 'b'])!;
    expect(out.clips.map((c) => c.prompt)).toEqual(['a — b', 'c']);
    expect(out.clips[0].seconds).toBe(8);
    expect(out.selectedIds).toEqual([out.clips[0].id]);
  });

  /**
   * 🔴 The FIRST member's id, not a fresh one. `packGenerations` claims a stored H3 block by clip-id
   * OVERLAP, so minting an id here orphans the authored prompt of the shot being merged into —
   * which is the shot the author kept. Dissolving a boundary in the composer is this operation.
   */
  it('keeps the first member id, so the authored block follows the merge', () => {
    const out = mergeClips(three(), ['a', 'b'])!;
    expect(out.clips.map((c) => c.id)).toEqual(['a', 'c']);
    expect(out.selectedIds).toEqual(['a']);
  });

  it('skips empty prompts rather than joining onto an empty string', () => {
    const out = mergeClips([clip('a'), clip('b', { prompt: '  ' })], ['a', 'b'])!;
    expect(out.clips[0].prompt).toBe('a');
  });

  /** The merged shot is one span, so a later shot's keyframe has to move to where that moment now
   *  falls — otherwise every pin after the first stacks up at the head of the joined shot. */
  it('re-times keyframes onto the joined span', () => {
    const a = clip('a', { keyframes: [{ image_b64: 'k0', seconds: 1 }] });
    const b = clip('b', { keyframes: [{ image_b64: 'k1', seconds: 2 }] });
    const out = mergeClips([a, b], ['a', 'b'])!;
    expect(out.clips[0].keyframes).toEqual([
      { image_b64: 'k0', seconds: 1 },
      { image_b64: 'k1', seconds: 6 }
    ]);
  });

  /** One span can hold only one clip: the second shot's audio would have nowhere to start. */
  it('keeps only the media belonging to the first shot', () => {
    const a = clip('a', { image_b64: 'img-a', audio_b64: 'aud-a', audio_kind: 'voice' });
    const b = clip('b', { image_b64: 'img-b', audio_b64: 'aud-b' });
    const out = mergeClips([a, b], ['a', 'b'])!;
    expect(out.clips[0]).toMatchObject({
      image_b64: 'img-a',
      audio_b64: 'aud-a',
      audio_kind: 'voice'
    });
  });
});

describe('splitClipAt', () => {
  it('cuts the shot the playhead is inside and selects the first half', () => {
    const out = splitClipAt(three(), 6)!;
    expect(out.clips.map((c) => c.seconds)).toEqual([4, 2, 2, 4]);
    expect(out.selectedIds).toEqual([out.clips[1].id]);
  });

  it('does nothing at or near a shot boundary', () => {
    expect(splitClipAt(three(), 0)).toBeNull();
    expect(splitClipAt(three(), 4)).toBeNull();
    expect(splitClipAt(three(), 4.05)).toBeNull();
    expect(splitClipAt(three(), 99)).toBeNull();
  });

  it('sends each keyframe to the half it falls in, re-based', () => {
    const src = [
      clip('a', {
        seconds: 8,
        keyframes: [
          { image_b64: 'k0', seconds: 1 },
          { image_b64: 'k1', seconds: 6 }
        ]
      })
    ];
    const out = splitClipAt(src, 4)!;
    expect(out.clips[0].keyframes).toEqual([{ image_b64: 'k0', seconds: 1 }]);
    expect(out.clips[1].keyframes).toEqual([{ image_b64: 'k1', seconds: 2 }]);
  });

  /** All three are anchored to the shot's own t=0: cloning them would open the second half on the
   *  first half's frame and replay its audio over it. */
  it('strips the start frame, v2v source and audio from the second half', () => {
    const src = [
      clip('a', {
        seconds: 8,
        image_b64: 'img',
        v2v_source_b64: 'vid',
        audio_b64: 'aud',
        audio_kind: 'voice'
      })
    ];
    const out = splitClipAt(src, 4)!;
    expect(out.clips[0]).toMatchObject({ image_b64: 'img', v2v_source_b64: 'vid' });
    expect(out.clips[1]).toMatchObject({
      image_b64: null,
      v2v_source_b64: null,
      audio_b64: null,
      audio_kind: undefined
    });
  });

  /**
   * 🔴 `packGenerations` re-attaches a generation's authored block by clip-id OVERLAP, so a split
   * that minted two fresh ids orphaned the block of a single-clip generation — sections, soundscape
   * and prompt silently became an unclaimed stored entry and the generation reseeded itself empty.
   */
  it('keeps the original id on the first half', () => {
    const out = splitClipAt([clip('a', { seconds: 8 })], 4)!;
    expect(out.clips[0].id).toBe('a');
    expect(out.clips[1].id).not.toBe('a');
  });

  /**
   * A forced boundary is placed once, by hand. Cloning it onto the new half splits a generation
   * where nothing asked for a split — and `h3_end_image` anchors the END of the span, which after a
   * cut is the second half, not a moment now buried in the middle of the first.
   */
  it('moves the closing anchor to the second half', () => {
    const src = [clip('a', { seconds: 8, h3_end_image: 'end' })];
    const out = splitClipAt(src, 4)!;
    expect(out.clips[0]).toMatchObject({ h3_end_image: null });
    expect(out.clips[1]).toMatchObject({ h3_end_image: 'end' });
  });
});

/**
 * The "something changes here → make it a cut" split. Every assertion here is a defect that would
 * otherwise be SILENT — the timeline looks right in all four cases and the render does not.
 */
describe('splitClipForCut', () => {
  it("keeps the FIRST half's id, so a generation's stored block is not orphaned", () => {
    const out = splitClipForCut([clip('a', { seconds: 8 })], 'a', 3, 'she looks up')!;
    // 🔴 `packGenerations` re-attaches an authored block by clip-id OVERLAP. If both halves were
    // fresh ids the overlap would be zero and the whole prompt — sections, soundscape, the lot —
    // would be silently orphaned and the generation would reseed itself empty.
    expect(out.clips[0].id).toBe('a');
    expect(out.clips[1].id).not.toBe('a');
    expect(out.clips.map((c) => c.seconds)).toEqual([3, 5]);
  });

  it('writes the change into the SECOND half and selects it', () => {
    const out = splitClipForCut([clip('a', { seconds: 8 })], 'a', 4, 'she sets the glass down')!;
    expect(out.clips[1].prompt).toBe('she sets the glass down');
    expect(out.clips[1].gist).toBe('she sets the glass down');
    expect(out.clips[0].prompt).toBe('a');
    expect(out.selectedIds).toEqual([out.clips[1].id]);
  });

  /** The end of the span moved, so the closing anchor has to move with it — on BOTH sides, or the
   *  frame is pinned twice, in the middle and at the end. */
  it('moves the last-frame image to the second half and strips the t=0 anchors', () => {
    const out = splitClipForCut(
      [clip('a', { seconds: 8, image_b64: 'img', audio_b64: 'aud', h3_end_image: 'end' })],
      'a',
      4,
      'x'
    )!;
    expect(out.clips[0]).toMatchObject({ image_b64: 'img', h3_end_image: null });
    expect(out.clips[1]).toMatchObject({ image_b64: null, audio_b64: null, h3_end_image: 'end' });
  });

  it('declines a cut at the very edge, and an unknown clip', () => {
    expect(splitClipForCut([clip('a', { seconds: 8 })], 'a', 0.05, 'x')).toBeNull();
    expect(splitClipForCut([clip('a', { seconds: 8 })], 'a', 7.97, 'x')).toBeNull();
    expect(splitClipForCut([clip('a', { seconds: 8 })], 'gone', 4, 'x')).toBeNull();
  });
});
