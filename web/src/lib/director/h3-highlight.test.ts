import { describe, expect, it } from 'vitest';
import { parseH3Block } from './h3-block';
import {
  h3Decorations,
  h3SectionAt,
  h3SectionBadges,
  h3SectionSpans,
  h3Tokens,
  type H3TokenKind
} from './h3-highlight';

/**
 * The one invariant everything else rests on: the tokens are a COMPLETE, non-overlapping cover of
 * the source. The editor paints them into a `<pre>` sitting exactly under a transparent textarea,
 * so a token that dropped or duplicated a single character would shift every glyph after it and
 * slide the highlight off the text — visibly, and only on blocks with that one construct in them.
 */
const covers = (text: string) => {
  const tokens = h3Tokens(text);
  expect(tokens.map((t) => t.text).join('')).toBe(text);
  for (let i = 1; i < tokens.length; i++) expect(tokens[i].start).toBe(tokens[i - 1].end);
  for (const t of tokens) expect(text.slice(t.start, t.end)).toBe(t.text);
  return tokens;
};

const kinds = (text: string, kind: H3TokenKind) =>
  h3Tokens(text)
    .filter((t) => t.kind === kind)
    .map((t) => t.text);

const REF_BLOCK = `subject_definitions:
<Subject 1> Mia, dark hair, red coat.
<Picture 1> the harbour at dawn.

summary:
[reference generation + audio reuse] <Subject 1> walks the quay while <Audio 1> plays.

retention_analysis:
<Subject 1> (appears in the video): fully_preserved - identity and wardrobe are retained
<Audio 1>: fully_copy - the supplied track is the final one

detailed_description:
The target video is in realistic photographic style.
[Shot 1] <Subject 1> stands at the rail looking out at <Picture 1>. <scenetrans>
[Shot 2] At 00:04.500, <scenetrans>the camera cuts to her hands on the rope.
[Shot 3] At 00:09.250, she turns toward the door.

overall_soundscape:
Gulls, water against the hull.

non_diegetic_music:
N/A`;

describe('h3Tokens', () => {
  it('covers the source exactly', () => {
    covers(REF_BLOCK);
    covers('');
    covers('just some prose with no schema in it at all');
    covers('detailed_description:\n[Shot 1] a room.');
  });

  it('finds every header, tag, time and label', () => {
    expect(kinds(REF_BLOCK, 'header')).toEqual([
      'subject_definitions:\n',
      'summary:\n',
      'retention_analysis:\n',
      'detailed_description:\n',
      'overall_soundscape:\n',
      'non_diegetic_music:\n'
    ]);
    expect(kinds(REF_BLOCK, 'shot')).toEqual(['[Shot 1]', '[Shot 2]', '[Shot 3]']);
    expect(kinds(REF_BLOCK, 'time')).toEqual(['At 00:04.500,', 'At 00:09.250,']);
    expect(kinds(REF_BLOCK, 'scenetrans')).toEqual(['<scenetrans>', '<scenetrans>']);
    expect(kinds(REF_BLOCK, 'label')).toEqual([
      '<Subject 1>',
      '<Picture 1>',
      '<Subject 1>',
      '<Audio 1>',
      '<Subject 1>',
      '<Audio 1>',
      '<Subject 1>',
      '<Picture 1>'
    ]);
  });

  it('carries the number, the label and the time so the editor need not re-parse', () => {
    const tokens = h3Tokens(REF_BLOCK);
    expect(tokens.filter((t) => t.kind === 'shot').map((t) => t.shot)).toEqual([1, 2, 3]);
    expect(tokens.filter((t) => t.kind === 'time').map((t) => t.seconds)).toEqual([4.5, 9.25]);
    expect(tokens.find((t) => t.kind === 'label')?.label).toBe('<Subject 1>');
  });

  it('reads the inline header form the FL2VA family writes', () => {
    const b = 'integrated_multimodal_description: [Shot 1] a room, slowly lit.';
    covers(b);
    expect(kinds(b, 'header')).toEqual(['integrated_multimodal_description: ']);
    expect(kinds(b, 'shot')).toEqual(['[Shot 1]']);
  });

  /**
   * 🔴 The reason markers are scanned per section rather than globally. `reference` is a legal audio
   * marker AND an ordinary English word; painting every occurrence in a 400-word body as schema is
   * noise wearing the costume of meaning.
   */
  it('only paints retention markers inside retention_analysis', () => {
    expect(kinds(REF_BLOCK, 'marker')).toEqual(['fully_preserved', 'fully_copy']);
    const prose =
      'retention_analysis:\n<Audio 1>: reference - the bed\n\n' +
      'detailed_description:\n[Shot 1] she keeps a reference photo, fully_preserved in a drawer.';
    covers(prose);
    expect(kinds(prose, 'marker')).toEqual(['reference']);
  });

  /** Same rule for the bracket: a task-type list at the head of `summary:`, a shot tag anywhere else. */
  it('paints the summary bracket only when the parser lifted it as task types', () => {
    expect(kinds(REF_BLOCK, 'tasktypes')).toEqual(['[reference generation + audio reuse]']);
    const notTypes =
      'summary:\n[Shot 1] is where she appears.\n\ndetailed_description:\n[Shot 1] a room.';
    covers(notTypes);
    expect(kinds(notTypes, 'tasktypes')).toEqual([]);
    // …and it is still a shot tag, because that is what it is.
    expect(kinds(notTypes, 'shot')).toEqual(['[Shot 1]', '[Shot 1]']);
  });

  it('shows a section MiniMax added that we have never heard of', () => {
    const b = 'detailed_description:\n[Shot 1] a room.\n\ncamera_metadata:\n35mm, f1.8';
    covers(b);
    expect(kinds(b, 'foreign')).toEqual(['camera_metadata:']);
  });

  it('keeps an indented foreign header aligned with the source', () => {
    const b = 'detailed_description:\n[Shot 1] a room.\n\n  camera_metadata:\n35mm';
    const tokens = covers(b);
    const foreign = tokens.find((t) => t.kind === 'foreign');
    expect(foreign?.text).toBe('  camera_metadata:');
  });
});

describe('h3SectionSpans', () => {
  it('puts a caret in the section whose bytes it is in', () => {
    const spans = h3SectionSpans(parseH3Block(REF_BLOCK));
    expect(spans.map((s) => s.name)).toEqual([
      'subject_definitions',
      'summary',
      'retention_analysis',
      'detailed_description',
      'overall_soundscape',
      'non_diegetic_music'
    ]);
    const at = (needle: string) => REF_BLOCK.indexOf(needle);
    expect(h3SectionAt(spans, at('[Shot 2]'))?.name).toBe('detailed_description');
    expect(h3SectionAt(spans, at('Gulls'))?.name).toBe('overall_soundscape');
    expect(h3SectionAt(spans, at('Mia'))?.name).toBe('subject_definitions');
  });

  /**
   * The gaps are real and are answered honestly rather than by rounding to a neighbour: the FL2VA
   * alignment line sits before the first header and belongs to no section, and `replaceH3Section`
   * would not touch it either.
   */
  it('has no section for the lead line', () => {
    const b =
      'How the reference pictures align with the target video: <Picture 1> is the first frame.\n\n' +
      'integrated_multimodal_description: [Shot 1] a room.';
    const spans = h3SectionSpans(parseH3Block(b));
    expect(h3SectionAt(spans, 5)).toBeNull();
    expect(h3SectionAt(spans, b.indexOf('[Shot 1]'))?.name).toBe(
      'integrated_multimodal_description'
    );
  });
});

/**
 * The four properties CodeMirror's `RangeSetBuilder` will throw on, rather than merely draw wrongly:
 * ranges must arrive sorted, non-empty, non-overlapping, and inside the document. A violation is an
 * exception on a keystroke, so they are pinned here rather than trusted to the tokenizer.
 */
describe('h3Decorations', () => {
  const legal = (text: string, caret: number | null) => {
    const decos = h3Decorations(text, caret);
    let last = 0;
    for (const d of decos) {
      expect(d.to).toBeGreaterThan(d.from);
      expect(d.from).toBeGreaterThanOrEqual(last);
      expect(d.to).toBeLessThanOrEqual(text.length);
      last = d.to;
    }
    return decos;
  };

  it('emits legal ranges for a whole block, focused anywhere in it', () => {
    for (let caret = 0; caret <= REF_BLOCK.length; caret += 7) legal(REF_BLOCK, caret);
    legal(REF_BLOCK, null);
  });

  it('emits legal ranges for the degenerate documents', () => {
    for (const text of ['', ' ', '\n', 'prose', 'detailed_description:', '[Shot 1]']) {
      legal(text, 0);
      legal(text, text.length);
    }
  });

  /** Every character of a half-typed block is reachable while the author is mid-keystroke. */
  it('survives a block being typed one character at a time', () => {
    for (let i = 0; i <= REF_BLOCK.length; i++) {
      const partial = REF_BLOCK.slice(0, i);
      legal(partial, partial.length);
    }
  });

  it('dims what the caret is not in, and nothing when unfocused', () => {
    const inSound = REF_BLOCK.indexOf('Gulls');
    const dimmed = h3Decorations(REF_BLOCK, inSound).filter((d) => d.cls.includes('h3-dim'));
    expect(dimmed.length).toBeGreaterThan(0);
    // The soundscape's own header is the one header that must NOT be dimmed.
    const header = h3Decorations(REF_BLOCK, inSound).find(
      (d) => REF_BLOCK.slice(d.from, d.to) === 'overall_soundscape:\n'
    );
    expect(header?.cls).toBe('h3-header');
    expect(h3Decorations(REF_BLOCK, null).some((d) => d.cls.includes('h3-dim'))).toBe(false);
  });
});

/**
 * 🔴 Inside the main body the live region is ONE `[Shot N]`, not the section.
 *
 * Reported by the owner: `detailed_description` holds the whole film, so lighting all of it says
 * nothing about where you are — every shot is "in detailed_description". The useful unit there is
 * the shot the caret is inside, which is the thing being written.
 */
describe('h3Decorations — the live region inside the body', () => {
  const at = (needle: string) => REF_BLOCK.indexOf(needle);
  const dimmedAround = (caret: number, needle: string) => {
    const from = REF_BLOCK.indexOf(needle);
    const d = h3Decorations(REF_BLOCK, caret).find((x) => x.from === from);
    return d?.cls.includes('h3-dim') ?? null;
  };

  it('lights only the shot the caret is in, and dims its neighbours', () => {
    const caret = at('her hands on the rope');
    expect(dimmedAround(caret, '[Shot 2]')).toBe(false);
    expect(dimmedAround(caret, '[Shot 1]')).toBe(true);
    expect(dimmedAround(caret, '[Shot 3]')).toBe(true);
    // Its own cut time rides with it.
    expect(dimmedAround(caret, 'At 00:04.500,')).toBe(false);
    expect(dimmedAround(caret, 'At 00:09.250,')).toBe(true);
  });

  it('still dims the other sections while a shot is live', () => {
    const caret = at('her hands on the rope');
    expect(dimmedAround(caret, 'overall_soundscape:')).toBe(true);
    expect(dimmedAround(caret, 'subject_definitions:')).toBe(true);
  });

  /** The style opening sits before [Shot 1] and is its own region — it is not part of any shot. */
  it('treats the style opening before [Shot 1] as its own region', () => {
    const caret = at('realistic photographic style');
    expect(dimmedAround(caret, '[Shot 1]')).toBe(true);
    expect(dimmedAround(caret, 'detailed_description:')).toBe(false);
  });

  /** Outside the body, the whole section is still the unit — there is nothing finer to point at. */
  it('keeps whole-section lighting outside the body', () => {
    const caret = at('Gulls');
    expect(dimmedAround(caret, 'overall_soundscape:')).toBe(false);
    expect(dimmedAround(caret, 'non_diegetic_music:')).toBe(true);
  });
});

/**
 * The badge that hangs off each header line — its gloss and its size, drawn as a widget rather than
 * written into the block.
 *
 * 🔴 The offset is the whole risk. A widget decoration placed past the end of the document throws
 * inside CodeMirror on the keystroke that produced it, so `at` is pinned here against both header
 * forms rather than trusted: Ref2VA writes `name:` then a newline, the FL2VA family writes `name: `
 * with the body inline on the same line, and the badge has to land just past the colon in both.
 */
describe('h3SectionBadges', () => {
  it('gives every section one badge, just past its colon', () => {
    const badges = h3SectionBadges(REF_BLOCK);
    expect(badges.map((b) => b.name)).toEqual([
      'subject_definitions',
      'summary',
      'retention_analysis',
      'detailed_description',
      'overall_soundscape',
      'non_diegetic_music'
    ]);
    for (const b of badges) {
      expect(REF_BLOCK.slice(b.at - 1, b.at)).toBe(':');
      expect(b.at).toBeLessThanOrEqual(REF_BLOCK.length);
    }
  });

  it('never places a badge past the start of the body it labels', () => {
    const spans = h3SectionSpans(parseH3Block(REF_BLOCK));
    const badges = h3SectionBadges(REF_BLOCK);
    for (const s of spans) {
      const b = badges.find((x) => x.name === s.name)!;
      expect(b.at).toBeGreaterThan(s.start);
      expect(b.at).toBeLessThanOrEqual(s.bodyStart);
    }
  });

  it('lands just past the colon on the inline header form too', () => {
    const b =
      'integrated_multimodal_description: [Shot 1] a room.\n\n' +
      'overall_soundscape: Rain on glass.\n\n' +
      'non_diegetic_music: N/A';
    const badges = h3SectionBadges(b);
    expect(badges.map((x) => x.name)).toEqual([
      'integrated_multimodal_description',
      'overall_soundscape',
      'non_diegetic_music'
    ]);
    for (const x of badges) expect(b.slice(x.at - 1, x.at)).toBe(':');
    // The body starts one space later, so the badge sits between the colon and the prose.
    expect(b.slice(badges[0].at, badges[0].at + 2)).toBe(' [');
  });

  it('measures each body against the character budget', () => {
    const badges = h3SectionBadges(REF_BLOCK);
    const by = (n: string) => badges.find((b) => b.name === n)!;
    expect(by('overall_soundscape').chars).toBe('Gulls, water against the hull.'.length);
    expect(by('non_diegetic_music').chars).toBe('N/A'.length);
    expect(by('subject_definitions').chars).toBeGreaterThan(0);
  });

  /**
   * An unwritten section is the normal look of a fresh shot, so the badge has to be able to say so
   * — it reads as instruction rather than as fault, and that is a state the editor must not confuse
   * with a one-character body.
   */
  it('reports an unwritten section as empty', () => {
    const b = 'summary:\n\ndetailed_description:\n[Shot 1] a room.\n\noverall_soundscape:\n';
    const badges = h3SectionBadges(b);
    const by = (n: string) => badges.find((x) => x.name === n)!;
    expect(by('summary').empty).toBe(true);
    expect(by('overall_soundscape').empty).toBe(true);
    expect(by('detailed_description').empty).toBe(false);
  });
});

/**
 * A label that resolves to nothing reads as completely normal in a wall of monospace, and after a
 * reference is taken out of scope it is the DESIGNED outcome of the renumber rather than a rare
 * mistake — so it has to be visible in the text, not only in a hover card nobody opened.
 */
describe('h3Decorations — labels that resolve to nothing', () => {
  const classesFor = (text: string, label: string, resolved?: Set<string>) =>
    h3Decorations(text, null, parseH3Block(text), resolved)
      .filter((d) => text.slice(d.from, d.to) === label)
      .map((d) => d.cls);

  it('marks a label the caller cannot answer for', () => {
    const live = new Set(['<Subject 1>', '<Picture 1>', '<Audio 1>']);
    expect(classesFor(REF_BLOCK, '<Picture 1>', live).every((c) => !c.includes('h3-dead'))).toBe(
      true
    );
    const narrowed = new Set(['<Subject 1>', '<Audio 1>']);
    expect(classesFor(REF_BLOCK, '<Picture 1>', narrowed).every((c) => c.includes('h3-dead'))).toBe(
      true
    );
  });

  /**
   * 🔴 "The caller has not told us" and "nothing resolves" are opposite claims. Only the second one
   * is allowed to paint the block red, or every consumer that has not opted in gets a wall of it.
   */
  it('marks nothing when it was given no set', () => {
    const decos = h3Decorations(REF_BLOCK, null);
    expect(decos.some((d) => d.cls.includes('h3-dead'))).toBe(false);
  });

  it('keeps the dim class and the dead class independent', () => {
    const caret = REF_BLOCK.indexOf('Gulls');
    const decos = h3Decorations(REF_BLOCK, caret, parseH3Block(REF_BLOCK), new Set());
    const subject = decos.find((d) => REF_BLOCK.slice(d.from, d.to) === '<Subject 1>')!;
    expect(subject.cls).toContain('h3-dead');
    expect(subject.cls).toContain('h3-dim');
  });
});
