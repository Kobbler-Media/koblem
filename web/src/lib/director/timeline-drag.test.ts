import { describe, expect, it } from 'vitest';
import { reorderTarget, trimmedSeconds } from './timeline-drag';

describe('reorderTarget', () => {
  const three = [2, 2, 2];

  it('keeps the clip where it is while the grab point stays inside its own slot', () => {
    expect(reorderTarget(three, 1, 2.5)).toBe(1);
  });

  it('moves right only once the grab point clears the next neighbour midpoint', () => {
    // neighbours are laid out without the dragged clip: [0..2)=clip0, [2..4)=clip2
    expect(reorderTarget(three, 1, 2.9)).toBe(1);
    expect(reorderTarget(three, 1, 3.1)).toBe(2);
  });

  it('moves left once the grab point crosses back over the previous neighbour midpoint', () => {
    expect(reorderTarget(three, 1, 0.9)).toBe(0);
  });

  it('treats a sub-0.1s clip as 0.1s so a zero-length shot cannot swallow the ordering', () => {
    expect(reorderTarget([0, 2], 1, 0.02)).toBe(0);
    expect(reorderTarget([0, 2], 1, 0.08)).toBe(1);
  });

  it('clamps to the ends', () => {
    expect(reorderTarget(three, 0, -50)).toBe(0);
    expect(reorderTarget(three, 0, 500)).toBe(2);
  });
});

describe('trimmedSeconds', () => {
  it('rounds to a tenth of a second', () => {
    expect(trimmedSeconds(1, 3.44)).toBe(2.4);
    expect(trimmedSeconds(1, 3.46)).toBe(2.5);
  });

  it('floors at half a second so a trim can never produce an empty shot', () => {
    expect(trimmedSeconds(1, 1.05)).toBe(0.5);
    expect(trimmedSeconds(1, -4)).toBe(0.5);
  });

  it('is stable — re-trimming to the same pointer position yields the same value', () => {
    const once = trimmedSeconds(0, 2.37);
    expect(trimmedSeconds(0, once)).toBe(once);
  });
});
