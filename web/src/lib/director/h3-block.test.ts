import { describe, expect, it } from 'vitest';
import type { DirectorH3Reference } from '$lib/api/client';
import { composeH3Prompt, h3Ordinals, type H3ComposeInput } from './h3-prompt';
import {
  h3MainSection,
  looksLikeH3Block,
  parseH3Block,
  replaceH3Section,
  type H3ParsedBlock
} from './h3-block';

/**
 * The parser's contract, in two halves that are deliberately different in strength:
 *
 * 1. **Byte-exact round-trip for anything the composer produced.** That is what
 *    {@link roundTrips} asserts, and it is the only guarantee this module makes about bytes.
 * 2. **Best-effort, never lossy, for everything else.** A hand-written or LLM-written block may
 *    contain things no field can hold; those land in `unparsed`, and `replaceH3Section` leaves
 *    their bytes alone so a field edit elsewhere cannot destroy them.
 *
 * 🔴 The second half is what makes the first half's narrowness safe. If a field edit recomposed the
 * whole block, everything the parser could not represent would be deleted the moment the author
 * touched an unrelated box — silently, and while they were doing something else.
 */

const ref = (over: Partial<DirectorH3Reference>): DirectorH3Reference => ({
  id: over.id ?? 'r',
  kind: over.kind ?? 'image',
  ...over
});

const base = (over: Partial<H3ComposeInput> = {}): H3ComposeInput => ({
  task: 't2va',
  durationSeconds: 10,
  shots: [{ index: 0, atSeconds: null, body: 'a harbour at dawn.' }],
  overallSoundscape: 'Gulls, and water against the hull.',
  nonDiegeticMusic: 'N/A',
  ...over
});

const ref2va = (over: Partial<H3ComposeInput> = {}): H3ComposeInput =>
  base({
    task: 'ref2va',
    ordinals: h3Ordinals([ref({ id: 'i', kind: 'image', image: 'png' })]),
    taskTypes: ['reference generation'],
    subjectDefinitions: [{ label: '<Subject 1>', text: 'is the harbour captain, grey beard.' }],
    summary: 'The target video shows the captain at the rail.',
    retentionAnalysis: [
      {
        label: '<Subject 1>',
        role_note: 'appears in [Shot 1]',
        marker: 'fully_preserved',
        text: 'identity, face and wardrobe are retained'
      }
    ],
    styleOpening: 'The target video is in realistic photographic style.',
    ...over
  });

const blank = (v: string | null): string | undefined => (v === null || v === '' ? undefined : v);

/**
 * A parsed block back into a compose input — the mapping Stage 2 performs in the editor.
 *
 * Duration and ordinals come from the pack rather than the text: neither is encoded in the block
 * (the alignment line carries a duration, but only on the FL2VA family and only to two decimals),
 * so they are carried through rather than re-derived.
 */
function reinput(parsed: H3ParsedBlock, original: H3ComposeInput): H3ComposeInput {
  return {
    task: parsed.task ?? original.task,
    durationSeconds: original.durationSeconds,
    shots: parsed.shots,
    ...(original.ordinals ? { ordinals: original.ordinals } : {}),
    ...(blank(parsed.styleOpening) ? { styleOpening: parsed.styleOpening! } : {}),
    ...(parsed.taskTypes.length ? { taskTypes: parsed.taskTypes } : {}),
    ...(parsed.subjectDefinitions.length ? { subjectDefinitions: parsed.subjectDefinitions } : {}),
    ...(blank(parsed.summary) ? { summary: parsed.summary! } : {}),
    ...(parsed.retentionAnalysis.length ? { retentionAnalysis: parsed.retentionAnalysis } : {}),
    ...(blank(parsed.overallSoundscape) ? { overallSoundscape: parsed.overallSoundscape! } : {}),
    ...(blank(parsed.nonDiegeticMusic) ? { nonDiegeticMusic: parsed.nonDiegeticMusic! } : {}),
    alignment: parsed.alignment
  };
}

/** compose → parse → compose, byte for byte. */
function roundTrips(input: H3ComposeInput) {
  const once = composeH3Prompt(input);
  const parsed = parseH3Block(once);
  expect(composeH3Prompt(reinput(parsed, input))).toBe(once);
  // A composed block is fully understood by definition — anything in `unparsed` is a parser gap.
  expect(parsed.unparsed).toEqual([]);
  expect(looksLikeH3Block(once)).toBe(true);
  return parsed;
}

describe('parseH3Block — byte-exact round-trip of composed blocks', () => {
  it('t2va, minimal', () => {
    expect(roundTrips(base()).task).toBe('t2va');
  });

  /** Style is folded INSIDE [Shot 1] on this schema, with no marker separating it from the body. */
  it('t2va with a style opening and a cut verb', () => {
    const parsed = roundTrips(
      base({
        styleOpening: 'Cinematic, warm anamorphic',
        shots: [
          { index: 0, atSeconds: null, body: 'a harbour at dawn.' },
          {
            index: 1,
            atSeconds: 4.5,
            body: "a close-up of the captain's face.",
            cutVerb: 'the camera cuts to'
          }
        ]
      })
    );
    // 🔴 The style is NOT recovered as a field here, and that is correct rather than a gap: nothing
    // separates it from the shot body on this schema, so guessing where it ended is what would lose
    // bytes. Recomposing with no style emits the identical string.
    expect(parsed.styleOpening).toBeNull();
    expect(parsed.shots[0].body).toBe('Cinematic, warm anamorphic. a harbour at dawn.');
    expect(parsed.shots[1].cutVerb).toBe('the camera cuts to');
  });

  it('t2va whose body already opens with its cut verb', () => {
    roundTrips(
      base({
        shots: [
          { index: 0, atSeconds: null, body: 'one.' },
          { index: 1, atSeconds: 2, body: 'the shot cuts to two.', cutVerb: 'the shot cuts to' }
        ]
      })
    );
  });

  /** The trailing marker is derived from the NEXT shot's flag, so only the incoming half is read. */
  it('t2va with a <scenetrans> pair', () => {
    const parsed = roundTrips(
      base({
        shots: [
          { index: 0, atSeconds: null, body: 'she starts the line.' },
          { index: 1, atSeconds: 3, body: 'she finishes it.', scenetrans: true }
        ]
      })
    );
    expect(parsed.shots[0].scenetrans).toBeUndefined();
    expect(parsed.shots[1].scenetrans).toBe(true);
    expect(parsed.shots[0].body).toBe('she starts the line.');
  });

  it.each([
    ['i2va', base({ alignment: 'i2va' })],
    ['fl2va', base({ alignment: 'fl2va', durationSeconds: 8 })],
    [
      'l2va',
      base({
        alignment: 'l2va',
        durationSeconds: 12,
        shots: [
          { index: 0, atSeconds: null, body: 'one.' },
          { index: 1, atSeconds: 4, body: 'two.', cutVerb: 'the shot changes to' },
          { index: 2, atSeconds: 8, body: 'three.', cutVerb: 'the shot switches to' }
        ]
      })
    ]
  ] as const)('%s alignment line', (kind, input) => {
    expect(roundTrips(input).alignment).toBe(kind);
  });

  it('ref2va, six sections', () => {
    const parsed = roundTrips(ref2va());
    expect(parsed.task).toBe('ref2va');
    // Ref2VA writes the style on its OWN line, so unlike the FL2VA family it comes back as a field.
    expect(parsed.styleOpening).toBe('The target video is in realistic photographic style.');
    expect(parsed.taskTypes).toEqual(['reference generation']);
    expect(parsed.retentionAnalysis[0].role_note).toBe('appears in [Shot 1]');
  });

  it('ref2va with several definitions and both marker vocabularies', () => {
    roundTrips(
      ref2va({
        ordinals: h3Ordinals([
          ref({ id: 'v', kind: 'video', video: 'mp4', with_audio: true }),
          ref({ id: 'a', kind: 'audio', audio: 'wav' })
        ]),
        taskTypes: ['video continuation', 'audio reference'],
        subjectDefinitions: [
          { label: '<Subject 1>', text: 'is the captain.' },
          { label: '<Video 1>', text: 'is the source clip the target continues from.' },
          { label: '<Audio 1>', text: 'is the synchronized audio track of <Video 1>.' },
          { label: '<Audio 2>', text: 'is the voice-timbre reference for <Subject 1> (S1).' }
        ],
        retentionAnalysis: [
          {
            label: '<Video 1>',
            role_note: 'continuation starting point',
            marker: 'fully_preserved',
            text: 'framing, grade and motion continue'
          },
          {
            label: '<Audio 1>',
            marker: 'reference',
            text: 'the bed continues without being copied'
          },
          {
            label: '<Audio 2>',
            marker: 'weak_reference',
            text: 'only the timbre - not the words - is referenced'
          }
        ]
      })
    );
  });

  /**
   * Production shape — the W2 arm from `RESULTS-musicvideo-audio-cost-2026-08-08.md`, which is the
   * prompt that measured best at 1152x640 / 345f. A known-good ref2va block with a prose keyframe
   * and a reused song, i.e. the exact thing Stage 6 emits.
   */
  it('ref2va at production shape — prose keyframe plus a reused song', () => {
    const parsed = roundTrips(
      base({
        task: 'ref2va',
        durationSeconds: 14.375,
        ordinals: h3Ordinals([
          ref({ id: 'p', kind: 'image', image: 'png', label: 'the robot' }),
          ref({ id: 'a', kind: 'audio', audio: 'mp3', label: 'the song' })
        ]),
        taskTypes: ['keyframe completion', 'audio reuse'],
        subjectDefinitions: [
          {
            label: '<Picture 1>',
            text: 'is the first frame of [Shot 1]: a weathered brass robot stands on wet paving under a sodium streetlight.'
          },
          { label: '<Audio 1>', text: 'is the complete final audio track of the target video.' }
        ],
        summary:
          'The robot walks the length of a rain-slick street and turns to face the camera as the chorus lands.',
        retentionAnalysis: [
          {
            label: '<Picture 1>',
            role_note: '[Shot 1] first frame',
            marker: 'fully_preserved',
            text: 'the robot, the paving and the light are the opening frame exactly'
          },
          {
            label: '<Audio 1>',
            marker: 'fully_copy',
            text: 'reused 1:1 at its own tempo for the whole target video'
          }
        ],
        styleOpening:
          'The target video is in realistic photographic style, night exterior, anamorphic.',
        shots: [
          {
            index: 0,
            atSeconds: null,
            body: 'The shot begins from <Picture 1>. The robot takes its first step and the camera tracks left with small amplitude at slow speed.'
          },
          {
            index: 1,
            atSeconds: 6.5,
            body: 'a low angle on the wet paving as the feet come down on the beat.',
            cutVerb: 'the camera cuts to'
          },
          {
            index: 2,
            atSeconds: 11.0,
            body: 'a wide of the street as the robot turns to face the lens.',
            cutVerb: 'the shot cuts to'
          }
        ],
        overallSoundscape:
          "The robot's metal feet tap the wet paving on the beat of <Audio 1>, and a faint room reflection of <Audio 1> comes off the brick walls. Nothing else is audible over the music.",
        nonDiegeticMusic:
          "<Audio 1> is the target video's complete final audio track, reused 1:1 at its own tempo. The drums, bass and lead vocal of <Audio 1> are what is heard throughout."
      })
    );
    expect(parsed.shots).toHaveLength(3);
    expect(parsed.shots[1].atSeconds).toBe(6.5);
  });
});

describe('parseH3Block — what must never split a section', () => {
  /**
   * 🔴 The parser scans for header LINES rather than splitting on `\n\n`, and this is why. An author
   * who presses Enter twice inside a paragraph writes a blank line into a section body; a `\n\n`
   * split would tear the section in half and re-label its remainder as whatever came next.
   */
  it('keeps a blank line inside a section', () => {
    const parsed = parseH3Block(
      'summary:\n[reference generation] First paragraph.\n\nSecond paragraph, same summary.\n\n' +
        'detailed_description:\n[Shot 1] a harbour.'
    );
    expect(parsed.summary).toBe('First paragraph.\n\nSecond paragraph, same summary.');
    expect(parsed.unparsed).toEqual([]);
  });

  /**
   * The section-name list is CLOSED for exactly this reason: a permissive `^(\w+):` would make any
   * sentence opening with a word and a colon a section boundary, cutting a shot body in half.
   */
  it('does not treat prose punctuation as a header', () => {
    const parsed = parseH3Block(
      'integrated_multimodal_description: [Shot 1] she reads it. Note: the paper is torn. ' +
        'Warning: nothing else moves.\n\noverall_soundscape: Paper.\n\nnon_diegetic_music: N/A'
    );
    expect(parsed.shots).toHaveLength(1);
    expect(parsed.shots[0].body).toContain('Warning: nothing else moves.');
  });

  /** Separator width carries no meaning — the engine runs `multi_prompts_gen_type = "FG"`. */
  it('reads one blank line between sections the same as five', () => {
    const one = parseH3Block('detailed_description:\n[Shot 1] x\n\noverall_soundscape:\ny');
    const five = parseH3Block(
      'detailed_description:\n[Shot 1] x\n\n\n\n\n\noverall_soundscape:\ny'
    );
    expect(five.shots).toEqual(one.shots);
    expect(five.overallSoundscape).toBe(one.overallSoundscape);
  });
});

describe('parseH3Block — nothing is dropped', () => {
  it('reports a retention line it could not read, and keeps the ones it could', () => {
    const parsed = parseH3Block(
      'retention_analysis:\n<Subject 1>: fully_preserved - kept\nthis is not a retention entry\n\n' +
        'detailed_description:\n[Shot 1] x'
    );
    expect(parsed.retentionAnalysis).toHaveLength(1);
    expect(parsed.unparsed[0]).toMatchObject({
      text: 'this is not a retention entry',
      section: 'retention_analysis'
    });
  });

  /** The explanation may contain hyphens, so the split is on the FIRST ` - ` and no other. */
  it('splits a retention line on the first hyphen only', () => {
    const parsed = parseH3Block(
      'retention_analysis:\n<Audio 2>: weak_reference - only the timbre - not the words - is referenced\n\n' +
        'detailed_description:\n[Shot 1] x'
    );
    expect(parsed.retentionAnalysis[0]).toMatchObject({
      label: '<Audio 2>',
      marker: 'weak_reference',
      text: 'only the timbre - not the words - is referenced'
    });
  });

  /** A missing pair of angle brackets must not cost the author their sentence. */
  it('keeps a definition line that carries no label', () => {
    const parsed = parseH3Block(
      'subject_definitions:\n<Subject 1> is the captain.\nthe mate is never shown.\n\n' +
        'detailed_description:\n[Shot 1] x'
    );
    expect(parsed.subjectDefinitions).toEqual([
      { label: '<Subject 1>', text: 'is the captain.' },
      { label: '', text: 'the mate is never shown.' }
    ]);
  });

  /**
   * 🔴 A mistyped alignment line must surface. It sits where the FL2VA family's alignment line
   * lives, so swallowing it would take the frame pin's meaning with it and say nothing.
   */
  it('surfaces a lead line that is not a recognisable alignment line', () => {
    const parsed = parseH3Block(
      'For the target vidoe, at 0.00 secnods, <Picture 1> is fully referenced.\n\n' +
        'integrated_multimodal_description: [Shot 1] x\n\noverall_soundscape: y\n\nnon_diegetic_music: N/A'
    );
    expect(parsed.alignment).toBe('none');
    expect(parsed.unparsed[0].text).toContain('For the target vidoe');
  });

  it('treats prose as prose rather than as an empty block', () => {
    const prose = 'A robot walks down a rainy street and looks at the camera.';
    expect(looksLikeH3Block(prose)).toBe(false);
    expect(parseH3Block(prose).task).toBeNull();
    expect(parseH3Block(prose).unparsed[0].text).toBe(prose);
    expect(parseH3Block('').unparsed).toEqual([]);
  });

  it('reads cut times back exactly, and leaves the verb in the body', () => {
    const parsed = parseH3Block(
      'integrated_multimodal_description: [Shot 1] a [Shot 2] At 01:03.250, the shot cuts to b ' +
        '[Shot 3] At 00:09.001, c'
    );
    expect(parsed.shots.map((s) => s.atSeconds)).toEqual([null, 63.25, 9.001]);
    // Left IN the body on purpose: `shotText` only prefixes a verb the body does not already open
    // with, so keeping it is what makes the recompose byte-identical.
    expect(parsed.shots[1]).toMatchObject({ cutVerb: 'the shot cuts to' });
    expect(parsed.shots[1].body).toBe('the shot cuts to b');
  });
});

describe('replaceH3Section — the surgical write', () => {
  const withUnknown =
    'subject_definitions:\n<Subject 1> is the captain.\n\n' +
    'speaker_notes:\nS1 is the captain, S2 is the mate.\n\n' +
    'detailed_description:\n[Shot 1] a harbour.\n\noverall_soundscape:\nGulls.';

  /**
   * 🔴 The guarantee the whole design rests on. A section this module has never heard of survives a
   * field edit **byte for byte** — which is what allows the parser to be best-effort at all.
   */
  it('changes one section and not one byte of anything else', () => {
    const edited = replaceH3Section(withUnknown, 'overall_soundscape', 'Rain on the deck.');
    expect(edited).toContain('overall_soundscape:\nRain on the deck.');
    expect(edited).toContain('speaker_notes:\nS1 is the captain, S2 is the mate.');
    expect(edited.replace('Rain on the deck.', 'Gulls.')).toBe(withUnknown);
  });

  /** An unknown section ends the previous one, so editing that one cannot reflow it either. */
  it('survives an edit to the section immediately before an unknown one', () => {
    expect(
      replaceH3Section(withUnknown, 'subject_definitions', '<Subject 1> is the captain.')
    ).toBe(withUnknown);
    expect(parseH3Block(withUnknown).unparsed[0].text).toBe(
      'speaker_notes:\nS1 is the captain, S2 is the mate.'
    );
  });

  it('inserts a missing section in schema order rather than at the end', () => {
    const missing =
      'subject_definitions:\n<Subject 1> is the captain.\n\n' +
      'detailed_description:\n[Shot 1] a harbour.\n\nnon_diegetic_music:\nN/A';
    const added = replaceH3Section(missing, 'overall_soundscape', 'Gulls.');
    expect(parseH3Block(added).sections.map((s) => s.name)).toEqual([
      'subject_definitions',
      'detailed_description',
      'overall_soundscape',
      'non_diegetic_music'
    ]);
  });

  /** The header form is the schema's, not ours: Ref2VA writes `name:\n`, the FL2VA family `name: `. */
  it('preserves the header form it found', () => {
    const inline =
      'integrated_multimodal_description: [Shot 1] x\n\noverall_soundscape: y\n\nnon_diegetic_music: N/A';
    expect(replaceH3Section(inline, 'overall_soundscape', 'z')).toContain('overall_soundscape: z');
    const ownLine = 'detailed_description:\n[Shot 1] x\n\noverall_soundscape:\ny';
    expect(replaceH3Section(ownLine, 'overall_soundscape', 'z')).toContain(
      'overall_soundscape:\nz'
    );
    expect(h3MainSection(inline)).toBe('integrated_multimodal_description');
    expect(h3MainSection(ownLine)).toBe('detailed_description');
    expect(h3MainSection('nothing here')).toBeNull();
  });
});

/**
 * A blank line is prose, not punctuation — and it must never move a section boundary.
 *
 * 🔴 The parser scans for header LINES and never splits on `\n\n`. `\n\n` is how `composeH3Prompt`
 * JOINS sections, but it is also what an author types for a paragraph break, and the two are
 * indistinguishable from the outside. A `\n\n` split therefore tears a section in half and re-labels
 * its remainder as whatever came next — the summary's second paragraph becomes part of
 * `retention_analysis`, silently, and it renders that way.
 *
 * 🔴 We PRESERVE rather than trim. The engine runs `multi_prompts_gen_type = "FG"`, so blank lines
 * carry no meaning on the wire and normalising them would be free — but they are a real paragraph
 * break to the AUTHOR, and trimming them would quietly rewrite their prose every time they touched
 * an unrelated box. The one blank line that IS absorbed is the one immediately following a header,
 * because there it was never content: it is the separator between `name:` and the body.
 */
describe('parseH3Block — a blank line is never a section boundary', () => {
  it('keeps a paragraph break inside summary, and calls nothing unparsed', () => {
    const p = parseH3Block(
      'summary:\n[reference generation] Para one.\n\nPara two.\n\ndetailed_description:\n[Shot 1] x'
    );
    expect(p.summary).toBe('Para one.\n\nPara two.');
    expect(p.taskTypes).toEqual(['reference generation']);
    expect(p.unparsed).toEqual([]);
  });

  it('keeps a paragraph break inside a shot body without splitting the shot', () => {
    const p = parseH3Block(
      'detailed_description:\nStyle.\n[Shot 1] she walks in.\n\nShe stops at the window.\n\n' +
        'overall_soundscape:\nRain.'
    );
    expect(p.styleOpening).toBe('Style.');
    expect(p.shots.length).toBe(1);
    expect(p.shots[0].body).toBe('she walks in.\n\nShe stops at the window.');
    expect(p.overallSoundscape).toBe('Rain.');
    expect(p.unparsed).toEqual([]);
  });

  /** The separator after a header was never content, however wide the author left it. */
  it('absorbs the blank line that follows a header, at any width', () => {
    const p = parseH3Block(
      'detailed_description:\n\n[Shot 1] x\n\noverall_soundscape:\n\n\nGulls.\n\nnon_diegetic_music:\n\nN/A'
    );
    expect(p.shots[0].body).toBe('x');
    expect(p.overallSoundscape).toBe('Gulls.');
    expect(p.nonDiegeticMusic).toBe('N/A');
  });

  it('keeps blank lines inside the two audio sections verbatim', () => {
    const p = parseH3Block(
      'detailed_description:\n[Shot 1] x\n\n' +
        'overall_soundscape:\nGulls overhead.\n\nWater against the hull.\n\n' +
        'non_diegetic_music:\nLow strings.\n\nA struck piano note every four bars.'
    );
    expect(p.overallSoundscape).toBe('Gulls overhead.\n\nWater against the hull.');
    expect(p.nonDiegeticMusic).toBe('Low strings.\n\nA struck piano note every four bars.');
    expect(p.unparsed).toEqual([]);
  });

  /** Both are line-per-entry sections, so a blank line between rows must be skipped, not counted. */
  it('drops nothing when definitions and retention rows are spaced out', () => {
    const p = parseH3Block(
      'subject_definitions:\n<Subject 1> is the captain.\n\n<Subject 2> is the mate.\n\n' +
        'retention_analysis:\n<Subject 1> (appears in [Shot 1]): fully_preserved - identity is retained\n\n' +
        '<Subject 2>: partially_preserved - the wardrobe changes\n\n' +
        'detailed_description:\n[Shot 1] x'
    );
    expect(p.subjectDefinitions.map((d) => d.label)).toEqual(['<Subject 1>', '<Subject 2>']);
    expect(p.retentionAnalysis.map((e) => e.marker)).toEqual([
      'fully_preserved',
      'partially_preserved'
    ]);
    expect(p.unparsed).toEqual([]);
  });

  /** The inline (FL2VA) form has the body starting on the header's own line — same rule. */
  it('keeps a paragraph break inside an inline section', () => {
    const p = parseH3Block(
      'integrated_multimodal_description: [Shot 1] a robot walks.\n\nIt turns to the camera.\n\n' +
        'overall_soundscape: Wet paving.\n\nnon_diegetic_music: N/A'
    );
    expect(p.shots.length).toBe(1);
    expect(p.shots[0].body).toBe('a robot walks.\n\nIt turns to the camera.');
    expect(p.overallSoundscape).toBe('Wet paving.');
    expect(p.nonDiegeticMusic).toBe('N/A');
    expect(p.unparsed).toEqual([]);
  });

  it('writes a body containing a blank line through replaceH3Section unchanged', () => {
    const src =
      'detailed_description:\n[Shot 1] she walks in.\n\nShe stops.\n\n' +
      'overall_soundscape:\nRain.\n\nThunder, far off.\n\nnon_diegetic_music:\nN/A';
    const out = replaceH3Section(src, 'overall_soundscape', 'Rain.\n\nA gull, once.');
    expect(out).toContain('overall_soundscape:\nRain.\n\nA gull, once.');
    // Every other section byte-identical — the shot body's own blank line included.
    expect(out.replace('Rain.\n\nA gull, once.', 'Rain.\n\nThunder, far off.')).toBe(src);
    expect(parseH3Block(out).shots[0].body).toBe('she walks in.\n\nShe stops.');
  });
});
