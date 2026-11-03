import { describe, expect, it } from 'vitest';
import type { DirectorClip } from '#lib/api/client.ts';
import {
  clipAtPromptedIndex,
  promptedIndexOf,
  shotNumberAtPromptedIndex,
  shotStatusIn
} from './shot-status';
import { closesScene, isV2v, startsFresh } from './continuity';

function clip(id: string, prompt: string, over: Partial<DirectorClip> = {}): DirectorClip {
  return { id, prompt, mode: 't2v', seconds: 4, keyframes: [], ...over };
}

/** A blank shot sitting between two prompted ones — the whole reason two index spaces exist. */
const withGap = () => [clip('a', 'one'), clip('gap', '  '), clip('b', 'two')];

describe('prompt-filtered index space', () => {
  it('numbers only the shots the engine will see', () => {
    const clips = withGap();
    expect(promptedIndexOf(clips, 'a')).toBe(0);
    expect(promptedIndexOf(clips, 'b')).toBe(1);
    expect(promptedIndexOf(clips, 'gap')).toBe(-1);
  });

  it('resolves an engine index back to its clip', () => {
    expect(clipAtPromptedIndex(withGap(), 1)?.id).toBe('b');
    expect(clipAtPromptedIndex(withGap(), 5)).toBeNull();
  });

  /** The label has to match the stage slate and the timeline that was clicked, which count RAW
   *  positions — so engine shot 1 is editor shot 3 when a blank clip sits before it. */
  it('labels an engine index with its raw editor number', () => {
    expect(shotNumberAtPromptedIndex(withGap(), 1)).toBe(3);
  });

  it('falls back to the engine number when the clip is gone', () => {
    expect(shotNumberAtPromptedIndex(withGap(), 9)).toBe(10);
  });
});

describe('shotStatusIn', () => {
  const clips = [clip('a', 'one'), clip('b', 'two')];
  const idle = { banked: [] as string[], live: null };

  it('a prompt-less shot is never rendered', () => {
    expect(shotStatusIn(withGap(), withGap()[1], idle)).toBe('none');
  });

  it('nothing banked means nothing rendered', () => {
    expect(shotStatusIn(clips, clips[0], idle)).toBe('none');
  });

  it('matching the banked prompt is rendered; differing is edited-since', () => {
    expect(shotStatusIn(clips, clips[0], { banked: ['one', 'two'], live: null })).toBe('ok');
    expect(shotStatusIn(clips, clips[0], { banked: ['other'], live: null })).toBe('stale');
  });

  it('a shot past the banked range is not rendered', () => {
    expect(shotStatusIn(clips, clips[1], { banked: ['one'], live: null })).toBe('none');
  });

  /** Mid-render the banked prefix is the PREVIOUS render's; a shot whose partial has landed is done
   *  and everything else is in the queue. */
  it('reads a live render instead of the banked prefix', () => {
    const live = { banked: ['one', 'two'], live: [{ index: 0 }] };
    expect(shotStatusIn(clips, clips[0], live)).toBe('ok');
    expect(shotStatusIn(clips, clips[1], live)).toBe('busy');
  });

  it('reports busy even before any partial has landed', () => {
    expect(shotStatusIn(clips, clips[0], { banked: [], live: [] })).toBe('busy');
  });
});

describe('continuity', () => {
  it.each([
    ['t2v', false],
    ['i2v', true],
    ['v2v_edit', true],
    ['v2v_restyle', true]
  ] as const)('%s starts fresh: %s', (mode, want) => {
    expect(startsFresh(clip('a', 'p', { mode }))).toBe(want);
    expect(isV2v(mode)).toBe(mode.startsWith('v2v'));
  });

  it('a hard cut starts fresh whatever its mode', () => {
    expect(startsFresh(clip('a', 'p', { new_scene: true }))).toBe(true);
  });

  /** "No next shot yet" is not an ending while you are still appending: keeping the scene in the
   *  final frame is the only thing a later shot can see. */
  it('a shot only closes when the next one starts fresh', () => {
    const clips = [clip('a', 'one'), clip('b', 'two'), clip('c', 'three', { mode: 'i2v' })];
    expect(closesScene(clips, 0)).toBe(false);
    expect(closesScene(clips, 1)).toBe(true);
    expect(closesScene(clips, 2)).toBe(false);
  });
});
