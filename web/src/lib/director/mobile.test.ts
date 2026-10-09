import { describe, expect, it } from 'vitest';
import type { DirectorH3Reference } from '$lib/api/client';
import {
  deadOrdinalNote,
  formatShotRanges,
  MOBILE_BEAT_GAP,
  mobileOrdinalBadge,
  mobileRefRows,
  nextBeatSeconds
} from './mobile';

describe('formatShotRanges', () => {
  it.each([
    [[], ''],
    [[3], '3'],
    [[2, 7, 9], '2, 7 and 9'],
    [[1, 2], '1–2'],
    [[1, 3, 4, 5, 6], '1 and 3–6'],
    [[1, 3, 4, 5, 6, 8, 10, 11, 12], '1, 3–6, 8 and 10–12'],
    // Unsorted and duplicated input is what a filter over an unordered status map produces.
    [[9, 2, 2, 7], '2, 7 and 9']
  ])('%j → %s', (input, expected) => {
    expect(formatShotRanges(input)).toBe(expected);
  });
});

describe('mobileOrdinalBadge', () => {
  it.each([
    ['<Picture 1>', 'P1'],
    ['<Video 12>', 'V12'],
    ['<Audio 2>', 'A2'],
    ['<Subject 3>', 'S3'],
    // Anything the pattern does not recognise comes back verbatim rather than mangled to one letter.
    ['<Mystery 1>', 'M1'],
    ['not a label', 'not a label']
  ])('%s → %s', (input, expected) => {
    expect(mobileOrdinalBadge(input)).toBe(expected);
  });
});

describe('mobileRefRows', () => {
  const src = (v: string | null | undefined) => (v ? `blob:${v}` : '');
  const ref = (r: Partial<DirectorH3Reference> & { id: string }): DirectorH3Reference => ({
    kind: 'image',
    ...r
  });

  it('numbers by position and carries the thumbnail through the injected resolver', () => {
    const rows = mobileRefRows(
      [
        ref({ id: 'a', label: 'Vex', image: 'asset:1' }),
        ref({ id: 'b', label: 'The crate', image: 'asset:2' })
      ],
      src
    );
    expect(rows.map((r) => [r.label, r.badge, r.name, r.image])).toEqual([
      ['<Picture 1>', 'P1', 'Vex', 'blob:asset:1'],
      ['<Picture 2>', 'P2', 'The crate', 'blob:asset:2']
    ]);
  });

  it("emits a soundtracked video's own audio ordinal as its own row, numbered first", () => {
    const rows = mobileRefRows([ref({ id: 'v', kind: 'video', with_audio: true })], src);
    expect(rows.map((r) => [r.label, r.ownAudio])).toEqual([
      ['<Audio 1>', true],
      ['<Video 1>', false]
    ]);
  });

  it('marks a reference confined to some units as scoped', () => {
    const rows = mobileRefRows([ref({ id: 'a' }), ref({ id: 'b', segments: [2] })], src);
    expect(rows.map((r) => r.scoped)).toEqual([false, true]);
  });
});

describe('nextBeatSeconds', () => {
  it('spreads beats over an empty unit rather than stacking them at the start', () => {
    expect(nextBeatSeconds([], 12)).toBe(6);
    expect(nextBeatSeconds([0], 12)).toBe(6);
    expect(nextBeatSeconds([0, 6], 12)).toBe(10);
  });

  it('never lands at or past the unit, which the render-time validator refuses', () => {
    // 12s unit: the ceiling is one gap short of the end, and repeated adds pile up against it.
    let at = nextBeatSeconds([0, 6], 12);
    expect(at).toBe(10);
    at = nextBeatSeconds([0, 6, at!], 12);
    expect(at).toBeLessThan(12);
    expect(at).toBe(11.95);
  });

  it('refuses once the ceiling is inside a gap of the last beat', () => {
    // The exact defect: 0, 6 → 10 → 11.5 → 11.5 again, equal to its predecessor.
    expect(nextBeatSeconds([0, 6, 10, 11.95], 12)).toBeNull();
    expect(nextBeatSeconds([0, 11.94], 12)).toBeNull();
    expect(nextBeatSeconds([], 0.05)).toBeNull();
  });

  it('is strictly monotonic over a full run of adds', () => {
    const beats = [0];
    for (let i = 0; i < 400; i++) {
      const at = nextBeatSeconds(beats, 12);
      if (at === null) break;
      expect(at).toBeGreaterThan(beats[beats.length - 1]);
      expect(at).toBeLessThan(12);
      beats.push(at);
    }
    // Millisecond resolution is what the block is written at, so no two may round together.
    const ms = beats.map((b) => Math.round(b * 1000));
    expect(new Set(ms).size).toBe(ms.length);
    expect(nextBeatSeconds(beats, 12)).toBeNull();
  });

  it('clears the MAX rather than the last, so a hand-typed time out of order still sorts last', () => {
    expect(nextBeatSeconds([0, 9, 3], 12)).toBeGreaterThan(9);
  });

  it('holds the gap when the even spread would be smaller than it', () => {
    const at = nextBeatSeconds([0, 1.98], 2, MOBILE_BEAT_GAP);
    expect(at).toBeNull();
    expect(nextBeatSeconds([0, 1.8], 2)).toBe(1.95);
  });
});

describe('deadOrdinalNote', () => {
  /** One sentence for a dead ordinal, because three surfaces print it and the fact is the same. */
  it('names what is in scope, and says what the model does with the number', () => {
    expect(deadOrdinalNote('<Picture 3>', ['P1', 'V2'])).toBe(
      '<Picture 3> names nothing — only P1, V2 exist here, so the model reads it as ordinary words.'
    );
    expect(deadOrdinalNote('<Audio 1>', [])).toBe(
      '<Audio 1> names nothing — no references are in scope, so the model reads it as ordinary words.'
    );
    // Singular, because "only P1 exist here" is the kind of thing that gets read as a bug.
    expect(deadOrdinalNote('<Picture 2>', ['P1'])).toContain('only P1 exists here');
  });
});
