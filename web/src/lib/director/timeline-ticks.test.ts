import { describe, expect, it } from 'vitest';
import type { LyricWord } from '$lib/api/client';
import { fitPhraseLabel, lyricPhrases, rulerTicks, tickLabel } from './timeline-ticks';

describe('rulerTicks', () => {
  /**
   * 🔴 The ruler drew one labelled tick PER SECOND regardless of zoom. On a three-minute song that
   * is 209 nodes, and below ~54 px/s the labels overlap into a smear — so the axis was least
   * readable at exactly the zoom where you are looking at the whole song.
   */
  it('never labels two ticks closer than a label is wide', () => {
    for (const pxPerSec of [20, 32, 56, 80, 120, 200, 320]) {
      const { step } = rulerTicks(208, pxPerSec);
      expect(step * pxPerSec).toBeGreaterThanOrEqual(54);
    }
  });

  it('opens the step out as it zooms out, and closes it in as it zooms in', () => {
    expect(rulerTicks(208, 320).step).toBeLessThan(rulerTicks(208, 20).step);
    // The whole song at once: a label every 5 s or wider, not every second.
    expect(rulerTicks(208, 20).step).toBeGreaterThanOrEqual(5);
  });

  it('covers the whole span and starts at zero', () => {
    const { major } = rulerTicks(30, 80);
    expect(major[0]).toBe(0);
    expect(major[major.length - 1]).toBeGreaterThanOrEqual(30);
  });

  it('keeps minor ticks readable at every zoom, so the guard for it would never run', () => {
    for (const pxPerSec of [1, 4, 20, 80, 320]) {
      const { minor } = rulerTicks(6000, pxPerSec);
      expect(minor * pxPerSec).toBeGreaterThanOrEqual(10);
    }
  });

  it('never puts a minor tick on top of a labelled one', () => {
    const { major, minors } = rulerTicks(60, 120);
    const majorSet = new Set(major);
    expect(minors.some((m) => majorSet.has(m))).toBe(false);
    expect(minors.length).toBeGreaterThan(0);
  });

  /**
   * The node count is not the win — minor ticks still land roughly per second at low zoom, which is
   * what a ruler should look like. The win is that only a fraction of them carry TEXT, which is the
   * thing that was colliding.
   */
  it('labels a small fraction of the ticks on a long song', () => {
    const { major, minors } = rulerTicks(208, 20);
    expect(major.length).toBeLessThan(50);
    expect(major.length).toBeLessThan(minors.length / 2);
  });

  it('lays out an empty axis without looping forever', () => {
    expect(rulerTicks(0, 80).major).toEqual([0]);
  });
});

describe('tickLabel', () => {
  it('is short, because it repeats across the whole axis', () => {
    expect(tickLabel(0, 5)).toBe('0:00');
    expect(tickLabel(65, 5)).toBe('1:05');
    expect(tickLabel(125, 30)).toBe('2:05');
  });

  it('keeps a decimal where the step is under a second, or every label in a bar reads alike', () => {
    expect(tickLabel(1.5, 0.5)).toBe('0:01.5');
    expect(tickLabel(61.2, 0.25)).toBe('1:01.2');
  });
});

describe('lyricPhrases', () => {
  const words = (...spec: [string, number, number][]): LyricWord[] =>
    spec.map(([text, start, end]) => ({ text, start, end }));

  it('groups words sung together and breaks on silence', () => {
    const p = lyricPhrases(
      words(
        ['she', 1, 1.3],
        ['walks', 1.35, 1.7],
        ['in', 1.75, 1.9],
        // A second of nothing — new line.
        ['fire', 3.2, 3.8]
      )
    );
    expect(p.map((x) => x.text)).toEqual(['she walks in', 'fire']);
    expect(p[0].start).toBe(1);
    expect(p[0].end).toBe(1.9);
  });

  /** The gap is measured END to START, so a held note is not a line break. */
  it('keeps a held note inside its line', () => {
    const p = lyricPhrases(words(['hooooold', 1, 3], ['on', 3.1, 3.4]));
    expect(p).toHaveLength(1);
    expect(p[0].text).toBe('hooooold on');
  });

  it('breaks a phrase that never stops, so one label cannot own the whole song', () => {
    const long = Array.from(
      { length: 60 },
      (_, i) => ['la', i * 0.3, i * 0.3 + 0.25] as [string, number, number]
    );
    const p = lyricPhrases(words(...long));
    expect(p.length).toBeGreaterThan(1);
    expect(Math.max(...p.map((x) => x.end - x.start))).toBeLessThanOrEqual(9);
  });

  it('has nothing to say about an empty transcript', () => {
    expect(lyricPhrases([])).toEqual([]);
  });
});

describe('fitPhraseLabel', () => {
  it('leaves a phrase that fits alone', () => {
    expect(fitPhraseLabel('she walks in', 200)).toBe('she walks in');
  });

  /** A phrase cut mid-word reads as a DIFFERENT word, which is worse than a shorter label. */
  it('cuts on a word boundary', () => {
    const out = fitPhraseLabel('she walks into the fire tonight', 70);
    expect(out.endsWith('…')).toBe(true);
    expect(
      out
        .replace('…', '')
        .trim()
        .split(' ')
        .every((w) => 'she walks into the fire tonight'.split(' ').includes(w))
    ).toBe(true);
  });

  it('says nothing at all where nothing legible fits', () => {
    expect(fitPhraseLabel('anything', 6)).toBe('');
  });

  it('falls back to a hard cut only for a single word wider than its whole span', () => {
    // Total visual length stays inside the budget: seven characters plus the ellipsis.
    expect(fitPhraseLabel('supercalifragilistic', 60)).toBe('superca…');
  });
});
