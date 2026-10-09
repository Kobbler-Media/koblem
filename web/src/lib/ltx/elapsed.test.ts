import { describe, expect, it } from 'vitest';
import { adoptedElapsedSec, parseCreatedAt } from './elapsed';

const NOW = Date.parse('2026-07-31T12:00:00Z');

describe('adoptedElapsedSec', () => {
  it('counts from the render start, not from the attach', () => {
    expect(adoptedElapsedSec('2026-07-31T11:58:00Z', NOW)).toBe(120);
  });

  it('accepts an offset timestamp as the same instant as its UTC form', () => {
    expect(adoptedElapsedSec('2026-07-31T12:58:00+01:00', NOW)).toBe(
      adoptedElapsedSec('2026-07-31T11:58:00Z', NOW)
    );
  });

  it('falls back to counting from the attach when the server sent no start time', () => {
    expect(adoptedElapsedSec(null, NOW)).toBe(0);
    expect(adoptedElapsedSec(undefined, NOW)).toBe(0);
    expect(adoptedElapsedSec('', NOW)).toBe(0);
    expect(adoptedElapsedSec('not a date', NOW)).toBe(0);
  });

  it('clamps a start time ahead of this browser to zero rather than counting backwards', () => {
    expect(adoptedElapsedSec('2026-07-31T12:00:30Z', NOW)).toBe(0);
  });

  it('reports sub-second precision for a render that just started', () => {
    expect(adoptedElapsedSec('2026-07-31T11:59:59.500Z', NOW)).toBe(0.5);
  });
});

describe('parseCreatedAt', () => {
  it('returns epoch ms for a wire timestamp and null for anything unusable', () => {
    expect(parseCreatedAt('2026-07-31T12:00:00Z')).toBe(NOW);
    expect(parseCreatedAt(null)).toBeNull();
    expect(parseCreatedAt('nope')).toBeNull();
  });
});
