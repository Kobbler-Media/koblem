import { describe, expect, it } from 'vitest';
import type { DirectorClip, DirectorH3Generation } from '$lib/api/client';
import { packGenerations } from './h3-generations';
import { h3ReviewGenerations } from './review';

/**
 * What the script supervisor is shown on H3.
 *
 * 🔴 The defect these pin: the review used to send one line per CLIP, and every schema check the
 * reviewer is asked for — cut times, reference labels, retention lines, the body field — lives
 * inside the assembled block and is invisible in such a listing. A model asked for findings it
 * cannot derive invents them. The reviewer now gets the blocks; the backend asks for the schema
 * checks only when they arrive.
 */

const clip = (over: Partial<DirectorClip> = {}): DirectorClip => ({
  id: over.id ?? 'a',
  prompt: 'a shot',
  mode: 't2v',
  seconds: 4,
  keyframes: [],
  ...over
});

describe('h3ReviewGenerations — the reviewer sees what renders', () => {
  it('sends the authored block verbatim, with the cut ids inside it', () => {
    // Two moments, ONE render: both `[Shot N]` lines live in a single clip's block, which is the
    // only place a cut time is written now.
    const block =
      'detailed_description:\n[Shot 1] A woman walks.\n\n[Shot 2] At 00:04.000, the wall.';
    const stored: DirectorH3Generation[] = [{ id: 'g1', clip_ids: ['a'], prompt: block }];
    const packed = packGenerations({ clips: [clip({ id: 'a' })], references: [], stored });
    const briefs = h3ReviewGenerations(packed);
    expect(briefs).toHaveLength(1);
    expect(briefs[0].prompt).toBe(block);
    expect(briefs[0].authored).toBe(true);
    // 🔴 The CLIP id, not the generation id: a finding becomes a one-click rework of one clip, so a
    // finding named after a generation would arrive with nothing to fix.
    expect(briefs[0].shots).toEqual(['a']);
  });

  /**
   * The SNAPPED duration, which is what renders — a cut at 5.1 s is legal in a generation that
   * snapped up to 5.17 s and illegal in the 5.0 s the author typed. Sending the authored sum would
   * have the reviewer report legal late cuts.
   */
  it('reports the duration the render actually gets, not the authored sum', () => {
    const packed = packGenerations({ clips: [clip({ id: 'a', seconds: 5 })], references: [] });
    expect(packed[0].seconds).toBeGreaterThan(5);
    expect(h3ReviewGenerations(packed)[0].seconds).toBe(packed[0].seconds);
  });

  /**
   * ⚠️ A generation nobody has opened still RENDERS: `compileH3Timeline` falls back to
   * `seedH3Prompt`. So the reviewer is shown those exact bytes rather than a blank — but flagged,
   * because unedited machine assembly of the cut bodies is not somebody's work and a reviewer that
   * thinks otherwise files a finding for every section not yet filled in.
   */
  it('falls back to the seeded block and says nobody wrote it', () => {
    const packed = packGenerations({
      clips: [clip({ id: 'c', prompt: 'she walks out', seconds: 5 })],
      references: []
    });
    const brief = h3ReviewGenerations(packed)[0];
    expect(brief.authored).toBe(false);
    expect(brief.prompt).toContain('she walks out');
    expect(brief.prompt).toContain('integrated_multimodal_description:');
  });

  /** A stored empty string is a cleared box, not a block — it must not read as authored. */
  it('treats a blank stored prompt as unwritten', () => {
    const stored: DirectorH3Generation[] = [{ id: 'g1', clip_ids: ['a'], prompt: '   \n ' }];
    const packed = packGenerations({ clips: [clip({ id: 'a' })], references: [], stored });
    expect(h3ReviewGenerations(packed)[0].authored).toBe(false);
  });
});
