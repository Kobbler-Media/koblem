import { describe, expect, it } from 'vitest';
import { moveShot, shotGist, shotStateLabel } from './shot-list';

describe('moveShot', () => {
  const three = ['a', 'b', 'c'];

  it('swaps with the neighbour in the requested direction', () => {
    expect(moveShot(three, 1, -1)).toEqual(['b', 'a', 'c']);
    expect(moveShot(three, 1, 1)).toEqual(['a', 'c', 'b']);
  });

  it('leaves the input alone — the caller decides whether to adopt the result', () => {
    moveShot(three, 1, 1);
    expect(three).toEqual(['a', 'b', 'c']);
  });

  it('reports a no-op at each end rather than returning an identical array', () => {
    expect(moveShot(three, 0, -1)).toBeNull();
    expect(moveShot(three, 2, 1)).toBeNull();
  });

  it('reports a no-op for a zero step and for an index off the list', () => {
    expect(moveShot(three, 1, 0)).toBeNull();
    expect(moveShot(three, -1, 1)).toBeNull();
    expect(moveShot(three, 9, -1)).toBeNull();
    expect(moveShot([], 0, 1)).toBeNull();
  });

  it('has no no-op case that still moves something (every null really is unchanged)', () => {
    for (let from = -1; from <= three.length; from++) {
      for (const delta of [-1, 0, 1]) {
        const next = moveShot(three, from, delta);
        if (next === null) continue;
        expect(next).not.toEqual(three);
      }
    }
  });
});

describe('shotGist', () => {
  it('prefers the summary, then the idea, then the raw prompt', () => {
    expect(shotGist({ gist: 'g', idea: 'i', prompt: 'p' })).toBe('g');
    expect(shotGist({ idea: 'i', prompt: 'p' })).toBe('i');
    expect(shotGist({ prompt: 'p' })).toBe('p');
  });

  it('treats whitespace-only fields as absent', () => {
    expect(shotGist({ gist: '   ', idea: 'i', prompt: 'p' })).toBe('i');
    expect(shotGist({ gist: null, idea: '\n', prompt: 'p' })).toBe('p');
  });

  it('falls back to a dash so a blank shot still has a row to tap', () => {
    expect(shotGist({ prompt: '' })).toBe('—');
    expect(shotGist({ gist: null, idea: null, prompt: '  ' })).toBe('—');
  });
});

describe('shotStateLabel', () => {
  it('spells each render state out in words', () => {
    expect(shotStateLabel('ok')).toBe('rendered');
    expect(shotStateLabel('stale')).toBe('edited since');
    expect(shotStateLabel('busy')).toBe('rendering');
    expect(shotStateLabel('none')).toBe('not rendered');
  });
});
