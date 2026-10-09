import { describe, expect, it } from 'vitest';
import type { H3Ordinal } from './h3-prompt';
import { h3OrdinalRenames, h3Renumber, h3SubjectRenames, type H3Rename } from './h3-renumber';

const ord = (id: string, label: string, over: Partial<H3Ordinal> = {}): H3Ordinal => ({
  id,
  kind: label.startsWith('<Audio') ? 'audio' : label.startsWith('<Video') ? 'video' : 'image',
  label,
  ...over
});

const BLOCK = `subject_definitions:
<Subject 1> is the captain, appearing in <Picture 1>.
<Subject 2> is the dockhand, appearing in <Picture 2>.

summary:
[reference generation] <Subject 1> reads a telegram while <Subject 2> waits.

retention_analysis:
<Picture 1> appears in the video, identity is fully_preserved.
<Picture 2> appears in the video, wardrobe is fully_preserved.

detailed_description:
[Shot 1] <Subject 1> at the rail, matching <Picture 1>.
[Shot 2] At 00:04.200, the camera cuts to <Subject 2> and <Picture 2>.`;

describe('h3Renumber', () => {
  it('renames every occurrence, in every section', () => {
    const out = h3Renumber(BLOCK, [{ from: '<Picture 2>', to: '<Picture 9>' }]);
    expect(out).not.toContain('<Picture 2>');
    expect(out.match(/<Picture 9>/g)).toHaveLength(3);
    // Nothing else moved.
    expect(out.match(/<Picture 1>/g)).toHaveLength(3);
    expect(out.match(/<Subject 2>/g)).toHaveLength(3);
  });

  /**
   * 🔴 The reason this is two-phase. Applied one after another, `<Picture 2>` → `<Picture 1>`
   * followed by `<Picture 1>` → `<Picture 4>` would rename the SURVIVOR twice and land everything
   * on 4 — the shift and the vacancy pass through each other.
   */
  it('does not let one rename cascade into another', () => {
    const out = h3Renumber(BLOCK, [
      { from: '<Picture 2>', to: '<Picture 1>' },
      { from: '<Picture 1>', to: '<Picture 4>' }
    ]);
    expect(out).toContain('<Subject 1> is the captain, appearing in <Picture 4>.');
    expect(out).toContain('<Subject 2> is the dockhand, appearing in <Picture 1>.');
    expect(out.match(/<Picture 4>/g)).toHaveLength(3);
    expect(out.match(/<Picture 1>/g)).toHaveLength(3);
  });

  it('leaves the block alone when nothing moved', () => {
    expect(h3Renumber(BLOCK, [])).toBe(BLOCK);
  });

  /** The placeholder must be something no author can type, or the swap eats real prose. */
  it('survives a block that talks about the placeholder shape', () => {
    const odd = 'detailed_description:\n[Shot 1] she writes 0 and 1 on the board with <Picture 1>.';
    const out = h3Renumber(odd, [{ from: '<Picture 1>', to: '<Picture 2>' }]);
    expect(out).toBe(
      'detailed_description:\n[Shot 1] she writes 0 and 1 on the board with <Picture 2>.'
    );
  });
});

describe('h3SubjectRenames', () => {
  it('shifts every later subject down one', () => {
    expect(h3SubjectRenames(0, 3)).toEqual<H3Rename[]>([
      { from: '<Subject 2>', to: '<Subject 1>' },
      { from: '<Subject 3>', to: '<Subject 2>' }
    ]);
  });

  it('has nothing to do for the last one', () => {
    expect(h3SubjectRenames(2, 3)).toEqual([]);
    expect(h3SubjectRenames(0, 1)).toEqual([]);
  });

  it('renames the subject out of the whole block, not just its definition', () => {
    const rows = BLOCK.split('\n');
    // Removing <Subject 1> leaves the definition list to the caller; the renames cover the prose.
    const out = h3Renumber(rows.join('\n'), h3SubjectRenames(0, 2));
    expect(out).toContain('[Shot 2] At 00:04.200, the camera cuts to <Subject 1> and <Picture 2>.');
  });
});

describe('h3OrdinalRenames', () => {
  it('shifts the survivors when a reference leaves scope', () => {
    const before = [ord('a', '<Picture 1>'), ord('b', '<Picture 2>'), ord('c', '<Picture 3>')];
    const after = [ord('b', '<Picture 1>'), ord('c', '<Picture 2>')];
    const renames = h3OrdinalRenames(before, after);
    expect(renames).toContainEqual({ from: '<Picture 2>', to: '<Picture 1>' });
    expect(renames).toContainEqual({ from: '<Picture 3>', to: '<Picture 2>' });
  });

  /**
   * 🔴 The load-bearing rule. Without it the survivors shift down ONTO the vacated number, so a
   * retention line that claimed one asset silently becomes a claim about a different one. A stale
   * number the author can see is recoverable; a plausible number pointing at the wrong picture is
   * not — so the departing reference's own mentions go one past the end of its kind, where nothing
   * resolves and the editor paints them dead.
   */
  it('sends a departing reference to a number that resolves to nothing', () => {
    const before = [ord('a', '<Picture 1>'), ord('b', '<Picture 2>'), ord('c', '<Picture 3>')];
    const after = [ord('b', '<Picture 1>'), ord('c', '<Picture 2>')];
    const renames = h3OrdinalRenames(before, after);
    expect(renames).toContainEqual({ from: '<Picture 1>', to: '<Picture 3>' });

    const text =
      '<Picture 1> is the captain. <Picture 2> is the dockhand. <Picture 3> is the rail.';
    const out = h3Renumber(text, renames);
    // The captain's mentions no longer claim to be anybody in scope...
    expect(out).toBe(
      '<Picture 3> is the captain. <Picture 1> is the dockhand. <Picture 2> is the rail.'
    );
    // ...and the dead number is past everything `after` allocates.
    expect(after.some((o) => o.label === '<Picture 3>')).toBe(false);
  });

  it('counts the dead slot per kind, not across all of them', () => {
    const before = [ord('a', '<Picture 1>'), ord('s', '<Audio 1>'), ord('t', '<Audio 2>')];
    const after = [ord('a', '<Picture 1>'), ord('t', '<Audio 1>')];
    const renames = h3OrdinalRenames(before, after);
    expect(renames).toContainEqual({ from: '<Audio 1>', to: '<Audio 2>' });
    expect(renames).toContainEqual({ from: '<Audio 2>', to: '<Audio 1>' });
    expect(renames.some((r) => r.to.startsWith('<Picture'))).toBe(false);
  });

  it('moves a soundtracked video and its own audio label together', () => {
    const before = [
      ord('v', '<Video 1>', { kind: 'video', audioLabel: '<Audio 1>' }),
      ord('s', '<Audio 2>')
    ];
    const after = [
      ord('v', '<Video 1>', { kind: 'video', audioLabel: '<Audio 2>' }),
      ord('s', '<Audio 1>')
    ];
    const renames = h3OrdinalRenames(before, after);
    expect(renames).toContainEqual({ from: '<Audio 1>', to: '<Audio 2>' });
    expect(renames).toContainEqual({ from: '<Audio 2>', to: '<Audio 1>' });
  });

  it('has nothing to say when the numbering did not move', () => {
    const same = [ord('a', '<Picture 1>'), ord('b', '<Picture 2>')];
    expect(h3OrdinalRenames(same, same)).toEqual([]);
  });

  it('does not rename anything for a reference that was only just added', () => {
    const before = [ord('a', '<Picture 1>')];
    const after = [ord('a', '<Picture 1>'), ord('new', '<Picture 2>')];
    expect(h3OrdinalRenames(before, after)).toEqual([]);
  });
});
