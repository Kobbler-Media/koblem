import { describe, expect, it } from 'vitest';
import type { DirectorH3Reference } from '#lib/api/client.ts';
import { parseH3Block } from './h3-block';
import {
  H3_CUT_VERBS,
  H3_PROMPT_MAX_CHARS,
  alignmentLine,
  composeH3Prompt,
  formatCutTime,
  h3BlocksRender,
  h3Ordinals,
  h3ShotIsCut,
  joinTaskTypes,
  validateH3Prompt,
  type H3ComposeInput
} from './h3-prompt';

function input(over: Partial<H3ComposeInput> = {}): H3ComposeInput {
  return {
    task: 't2va',
    durationSeconds: 10,
    shots: [{ index: 0, atSeconds: null, body: 'a harbour at dawn.' }],
    overallSoundscape: 'Gulls, and water against the hull.',
    nonDiegeticMusic: 'N/A',
    ...over
  };
}

const ref = (over: Partial<DirectorH3Reference>): DirectorH3Reference => ({
  id: over.id ?? 'r',
  kind: over.kind ?? 'image',
  ...over
});

/**
 * ## The beat construct, pinned
 *
 * 🔴 These are the tests that fail if somebody "fixes" the composer to always supply a default cut
 * verb — which is exactly what the old `<select value={… ?? H3_CUT_VERBS[0]}>` was one keystroke
 * away from committing to disk.
 *
 * A `[Shot N] At MM:SS.mmm` with NO transition verb is a **beat**: measured 2026-08-08, three such
 * marks in one 362-frame render produced zero scene changes (`scdet` max 0.71, where a hard cut
 * scores >10) and landed their actions within ~0.5 s of the stated times. An absent `cutVerb` is
 * therefore a MEANING, not an omission, and it has to survive a round trip intact.
 */
describe('a verbless [Shot N] is a beat, and stays one', () => {
  it('emits no verb when the shot carries none', () => {
    const block = composeH3Prompt(
      input({
        durationSeconds: 15,
        shots: [
          { index: 0, atSeconds: null, body: 'a woman at a bar, watching the television.' },
          { index: 1, atSeconds: 2, body: 'she places the tumbler down and licks her lips.' }
        ]
      })
    );
    expect(block).toContain('[Shot 2] At 00:02.000, she places the tumbler down');
    // The failure this guards is a DEFAULT sneaking in, so assert on every verb we can write.
    for (const v of H3_CUT_VERBS) expect(block).not.toContain(v);
  });

  it('round-trips a verbless shot back to cutVerb: undefined', () => {
    const block = composeH3Prompt(
      input({
        durationSeconds: 15,
        shots: [
          { index: 0, atSeconds: null, body: 'a woman at a bar.' },
          { index: 1, atSeconds: 2, body: 'she sets the glass down.' }
        ]
      })
    );
    const back = parseH3Block(block);
    expect(back.shots[1].cutVerb).toBeUndefined();
    expect(h3ShotIsCut(back.shots[1])).toBe(false);
    // …and the recompose is byte-identical, which is what makes the block the document.
    expect(composeH3Prompt(input({ durationSeconds: 15, shots: back.shots }))).toEqual(block);
  });

  it('still reads a verbed shot as a cut', () => {
    const block = composeH3Prompt(
      input({
        durationSeconds: 15,
        shots: [
          { index: 0, atSeconds: null, body: 'a woman at a bar.' },
          { index: 1, atSeconds: 2, body: 'the harbour at dawn.', cutVerb: 'the camera cuts to' }
        ]
      })
    );
    expect(block).toContain('[Shot 2] At 00:02.000, the camera cuts to the harbour at dawn.');
    expect(h3ShotIsCut(parseH3Block(block).shots[1])).toBe(true);
  });

  /**
   * 🔴 A dissolve is a cut even though the picker never offers one. Detecting against the five
   * offered verbs read it back as a BEAT, so the editor would have offered to "keep the camera
   * rolling" on a shot that dissolves — a misclassification that predates the distinction being
   * modelled at all.
   */
  it('reads a hand-written dissolve as a cut, not a beat', () => {
    const block = composeH3Prompt(
      input({
        durationSeconds: 15,
        shots: [
          { index: 0, atSeconds: null, body: 'a woman at a bar.' },
          { index: 1, atSeconds: 2, body: 'the shot cross-dissolves to the harbour at dawn.' }
        ]
      })
    );
    const shot = parseH3Block(block).shots[1];
    expect(h3ShotIsCut(shot)).toBe(true);
    expect(shot.cutVerb).toBe('the shot cross-dissolves to');
  });
});

describe('h3Ordinals — pipeline add-order', () => {
  /**
   * The rule that is silent when broken: the labels still look plausible, the render still
   * completes, and the prompt is simply talking about a different asset than the author meant.
   */
  it('numbers images, then videos, then standalone audio — regardless of array order', () => {
    const ords = h3Ordinals([
      ref({ id: 'a', kind: 'audio', audio: 'wav' }),
      ref({ id: 'v', kind: 'video', frames: ['f'] }),
      ref({ id: 'i', kind: 'image', image: 'png' })
    ]);
    // Emitted in INPUT order (the panel shows what the user arranged) with PIPELINE numbering.
    expect(ords.map((o) => [o.id, o.label])).toEqual([
      ['a', '<Audio 1>'],
      ['v', '<Video 1>'],
      ['i', '<Picture 1>']
    ]);
  });

  /**
   * The official request is the proof: its video carries a soundtrack, so the soundtrack is
   * `<Audio 1>`, the video `<Video 1>`, and the separate voice clip `<Audio 2>` — the video's own
   * audio row is packed BEFORE its video row, so it takes the lower ordinal.
   */
  it('gives a with_audio video its <Audio k> BEFORE any standalone audio', () => {
    const ords = h3Ordinals([
      ref({ id: 'v', kind: 'video', video: 'mp4', with_audio: true }),
      ref({ id: 'voice', kind: 'audio', audio: 'wav' })
    ]);
    expect(ords[0]).toMatchObject({ label: '<Video 1>', audioLabel: '<Audio 1>' });
    expect(ords[1].label).toBe('<Audio 2>');
  });

  /** A reference video does not create an `<Audio N>` just because the file could have sound. */
  it('allocates no audio ordinal to a video that did not ask for one', () => {
    const ords = h3Ordinals([
      ref({ id: 'v', kind: 'video', video: 'mp4' }),
      ref({ id: 'a', kind: 'audio', audio: 'wav' })
    ]);
    expect(ords[0].audioLabel).toBeUndefined();
    expect(ords[1].label).toBe('<Audio 1>');
  });

  /**
   * 🔴 The ordinal follows `with_audio` and NOTHING else. Keying it off an attached `audio` payload
   * was wrong twice over: the engine allocates the row for `with_audio` (`GUIDE-ref:112` — a
   * reference video does not earn an `<Audio N>` just because the file has sound), and a video
   * reference carrying a SEPARATE soundtrack is a shape koblem refuses by name, so the composer
   * was previewing labels for a request that 400s before it reaches a GPU.
   */
  it('ignores an attached audio payload on a video — that shape is a rejection, not an ordinal', () => {
    const ords = h3Ordinals([
      ref({ id: 'v', kind: 'video', video: 'mp4', audio: 'wav' }),
      ref({ id: 'voice', kind: 'audio', audio: 'wav' })
    ]);
    expect(ords[0].audioLabel).toBeUndefined();
    // The standalone clip keeps `<Audio 1>`: nothing was allocated ahead of it.
    expect(ords[1].label).toBe('<Audio 1>');
  });
});

describe('formatCutTime', () => {
  it.each([
    [4.5, '00:04.500'],
    [0, '00:00.000'],
    [63.25, '01:03.250'],
    [9.0005, '00:09.001']
  ])('%s → %s', (secs, want) => {
    expect(formatCutTime(secs)).toBe(want);
  });
});

describe('composeH3Prompt — the FL2VA family', () => {
  /** The header form is `name: ` inline, and the separator is a blank line. */
  it('emits three inline-header sections separated by a blank line', () => {
    const block = composeH3Prompt(input());
    expect(block).toBe(
      'integrated_multimodal_description: [Shot 1] a harbour at dawn.\n\n' +
        'overall_soundscape: Gulls, and water against the hull.\n\n' +
        'non_diegetic_music: N/A'
    );
    // None of the three literal prompts ends with a newline.
    expect(block.endsWith('\n')).toBe(false);
  });

  /** Style goes AFTER the [Shot 1] tag here — the opposite of Ref2VA. */
  it('folds the style opening inside [Shot 1]', () => {
    const block = composeH3Prompt(input({ styleOpening: 'Cinematic, warm anamorphic' }));
    expect(block).toContain(
      'integrated_multimodal_description: [Shot 1] Cinematic, warm anamorphic. a harbour at dawn.'
    );
  });

  /** Shots run INLINE, space-separated, on this schema (REQ-t2va:13, GUIDE-base:175). */
  it('runs later shots inline after a space', () => {
    const block = composeH3Prompt(
      input({
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
    expect(block).toContain(
      "[Shot 1] a harbour at dawn. [Shot 2] At 00:04.500, the camera cuts to a close-up of the captain's face."
    );
  });

  /** A hand-written body that already opens with a cut verb must not get a second one. */
  it('does not double the cut verb', () => {
    const block = composeH3Prompt(
      input({
        shots: [
          { index: 0, atSeconds: null, body: 'one.' },
          {
            index: 1,
            atSeconds: 2,
            body: 'the shot cuts to two.',
            cutVerb: 'the shot cuts to'
          }
        ]
      })
    );
    expect(block).toContain('[Shot 2] At 00:02.000, the shot cuts to two.');
    expect(block.match(/the shot cuts to/g)).toHaveLength(1);
  });

  /**
   * ⚠️ Placement is INFERRED — `<scenetrans>` has zero in-situ examples in the corpus. What the
   * test pins is that the marker appears on BOTH sides of the cut, which is the one thing the
   * guide does state.
   */
  it('marks both connecting points when dialogue crosses a cut', () => {
    const block = composeH3Prompt(
      input({
        shots: [
          { index: 0, atSeconds: null, body: 'she starts the line.' },
          { index: 1, atSeconds: 3, body: 'she finishes it.', scenetrans: true }
        ]
      })
    );
    expect(block).toContain('[Shot 1] she starts the line. <scenetrans>');
    expect(block).toContain('[Shot 2] At 00:03.000, <scenetrans> she finishes it.');
  });
});

describe('alignmentLine', () => {
  /** I2VA is character-for-character with no substitutions at all — the 0.00 is literal. */
  it('emits the I2VA line verbatim', () => {
    expect(alignmentLine('i2va', 8, 1)).toBe(
      'For the target video, at 0.00 seconds into the target video, <Picture 1> (from [Shot 1]) is fully referenced.'
    );
  });

  /**
   * ⚠️ The two-keyframe line is UNBRACKETED and that is deliberate: the guide writes it that way in
   * both its template and its worked example, while I2VA and L2VA are bracketed, and no script on
   * disk exercises the two-image form. Emitting each variant as its own template writes it beats a
   * normalisation we invent.
   */
  it('leaves the two-keyframe line unbracketed, with a two-decimal duration', () => {
    expect(alignmentLine('fl2va', 8, 1)).toContain(
      'Picture 2 (from Shot 1) aligns with the 8.00-second mark'
    );
    expect(alignmentLine('fl2va', 8, 1)).not.toContain('<Picture');
  });

  it('brackets the L2VA line and names the final shot', () => {
    expect(alignmentLine('l2va', 6, 3)).toBe(
      'How the reference pictures align with the target video — <Picture 1> (from [Shot 3]) aligns with the 6.00-second mark of the target video.'
    );
  });

  it('emits nothing for plain t2va', () => {
    expect(alignmentLine('none', 10, 1)).toBeNull();
  });

  /** The line is the FIRST line, followed by exactly one blank line before the core fields. */
  it('leads the block, separated by a blank line', () => {
    const block = composeH3Prompt(input({ alignment: 'i2va' }));
    expect(block.split('\n\n')[0]).toBe(alignmentLine('i2va', 10, 1));
    expect(block.split('\n\n')[1].startsWith('integrated_multimodal_description: ')).toBe(true);
  });
});

describe('composeH3Prompt — Ref2VA', () => {
  const ref2va = (over: Partial<H3ComposeInput> = {}) =>
    composeH3Prompt(
      input({
        task: 'ref2va',
        ordinals: h3Ordinals([ref({ id: 'i', kind: 'image', image: 'png' })]),
        taskTypes: ['reference generation'],
        subjectDefinitions: [
          { label: '<Subject 1>', text: '<Subject 1> is the harbour captain, grey beard.' }
        ],
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
      })
    );

  /** Six sections, fixed order, header on its own line — the opposite form to the FL2VA family. */
  it('emits six own-line headers in the fixed order', () => {
    const heads = ref2va()
      .split('\n\n')
      .map((s) => s.split('\n')[0]);
    expect(heads).toEqual([
      'subject_definitions:',
      'summary:',
      'retention_analysis:',
      'detailed_description:',
      'overall_soundscape:',
      'non_diegetic_music:'
    ]);
  });

  /** Style is its OWN line before [Shot 1], and shots each get their own line. */
  it('puts the style opening on its own line ahead of [Shot 1]', () => {
    expect(ref2va()).toContain(
      'detailed_description:\nThe target video is in realistic photographic style.\n[Shot 1] a harbour at dawn.'
    );
  });

  it('prefixes the summary with the bracketed task types', () => {
    expect(ref2va({ taskTypes: ['audio reference', 'video continuation'] })).toContain(
      'summary:\n[video continuation + audio reference] The target video'
    );
  });

  it('renders a retention line as label (note): marker - text', () => {
    expect(ref2va()).toContain(
      'retention_analysis:\n<Subject 1> (appears in [Shot 1]): fully_preserved - identity, face and wardrobe are retained'
    );
  });

  /** The editor shows the label beside the box, so an author who types it too gets it once. */
  it('does not repeat a definition label the author typed', () => {
    expect(ref2va()).toContain('subject_definitions:\n<Subject 1> is the harbour captain');
    expect(ref2va().match(/<Subject 1> <Subject 1>/)).toBeNull();
  });
});

describe('joinTaskTypes', () => {
  /** ⚠️ The order is ours — no rule is stated and the corpus contradicts itself. */
  it('joins with " + " in the canonical order and never repeats a type', () => {
    expect(joinTaskTypes(['audio reuse', 'video editing', 'video editing'])).toBe(
      'video editing + audio reuse'
    );
  });
});

describe('validateH3Prompt', () => {
  const fields = (i: H3ComposeInput, level: string) =>
    validateH3Prompt(i)
      .filter((x) => x.level === level)
      .map((x) => x.field);
  const errors = (i: H3ComposeInput) => fields(i, 'error');
  const unwritten = (i: H3ComposeInput) => fields(i, 'unwritten');

  it('passes a well-formed t2va generation', () => {
    expect(errors(input())).toEqual([]);
  });

  /**
   * 🔴 Re-pointed 2026-08-08, and the assertion is the STAGE: this used to demand an `error`.
   *
   * The fact it was defending has not changed — an empty soundscape does not mean silence on an
   * engine that generates its own audio, it means the model invents the bed unguided. What changed
   * is that "not written yet" is the normal state of a fresh shot and of every LLM draft, so
   * saying it in red made the editor open red every single time and taught everyone to ignore it.
   * It is still said, in the same words, at a tier that does not block a render.
   */
  it('files a missing soundscape or score as unwritten, not as an error', () => {
    expect(errors(input({ overallSoundscape: '' }))).toEqual([]);
    expect(unwritten(input({ overallSoundscape: '' }))).toContain('overall_soundscape');
    expect(unwritten(input({ nonDiegeticMusic: '' }))).toContain('non_diegetic_music');
    // The reason has to survive the demotion — it is the only argument for writing the section.
    const note = validateH3Prompt(input({ overallSoundscape: '' })).find(
      (i) => i.field === 'overall_soundscape'
    );
    expect(note?.message).toContain('invents the whole bed unguided');
  });

  /**
   * A fresh ref2va generation — a body, references in scope, and nothing else written. This is what
   * the editor opens on and what an LLM's first pass looks like, and it must not block a render.
   */
  it('meets an unwritten ref2va generation with placeholders and no errors at all', () => {
    const fresh = input({
      task: 'ref2va',
      ordinals: h3Ordinals([ref({ id: 'i', kind: 'image', image: 'p' })]),
      overallSoundscape: '',
      nonDiegeticMusic: ''
    });
    expect(errors(fresh)).toEqual([]);
    expect(h3BlocksRender(validateH3Prompt(fresh))).toBe(false);
    expect(unwritten(fresh).sort()).toEqual([
      'non_diegetic_music',
      'overall_soundscape',
      'retention_analysis',
      'subject_definitions',
      'summary',
      'summary'
    ]);
  });

  /**
   * 🔴 The one absence that stays red. Every other empty section still renders something; a block
   * with no shot body is an EMPTY prompt, and koblem drops an empty-prompt generation out of the
   * chain — so it does not fail, it silently disappears from the cut.
   */
  it('keeps an empty body as an error, because that generation vanishes rather than fails', () => {
    const empty = input({ shots: [{ index: 0, atSeconds: null, body: '   ' }] });
    expect(errors(empty)).toContain('integrated_multimodal_description');
    expect(h3BlocksRender(validateH3Prompt(empty))).toBe(true);
  });

  it('rejects a timestamped first shot', () => {
    expect(errors(input({ shots: [{ index: 0, atSeconds: 0, body: 'x' }] }))).toContain('shots');
  });

  it('requires strictly increasing cut times inside the duration', () => {
    const shots = (a: number, b: number) => [
      { index: 0, atSeconds: null, body: 'one' },
      { index: 1, atSeconds: a, body: 'two' },
      { index: 2, atSeconds: b, body: 'three' }
    ];
    expect(errors(input({ shots: shots(4, 4) }))).toContain('shots');
    expect(errors(input({ shots: shots(4, 8) }))).toEqual([]);
    // 10 s duration: a cut AT the end leaves the shot no time to play, and past it is worse.
    expect(errors(input({ shots: shots(4, 10) }))).toContain('shots');
    expect(errors(input({ shots: shots(4, 12) }))).toContain('shots');
  });

  /** `fl2va` and `ref2va` are different weight sets, so one generation cannot carry both. */
  it('refuses references on a keyframe generation and demands one on a ref2va generation', () => {
    expect(
      errors(input({ alignment: 'i2va', ordinals: h3Ordinals([ref({ id: 'i', kind: 'image' })]) }))
    ).toContain('references');
    expect(
      errors(input({ task: 'ref2va', ordinals: h3Ordinals([ref({ id: 'a', kind: 'audio' })]) }))
    ).toContain('references');
  });

  /** The two marker vocabularies are disjoint; picking from the wrong one is a schema error. */
  it('checks each retention marker against the table for its label kind', () => {
    const base: Partial<H3ComposeInput> = {
      task: 'ref2va',
      // `with_audio`, so the clip's own track is `<Audio 1>` and a retention line may name it.
      ordinals: h3Ordinals([ref({ id: 'v', kind: 'video', video: 'mp4', with_audio: true })]),
      taskTypes: ['video continuation'],
      subjectDefinitions: [{ label: '<Video 1>', text: '<Video 1> is the source clip.' }],
      summary: 's'
    };
    const withMarker = (label: string, marker: string) =>
      errors(
        input({
          ...base,
          retentionAnalysis: [{ label, marker, text: 'note' }]
        })
      );
    expect(withMarker('<Video 1>', 'fully_preserved')).not.toContain('retention_analysis');
    expect(withMarker('<Video 1>', 'fully_copy')).toContain('retention_analysis');
    expect(withMarker('<Audio 1>', 'fully_copy')).not.toContain('retention_analysis');
    expect(withMarker('<Audio 1>', 'fully_preserved')).toContain('retention_analysis');
  });

  /** A label the conditioner never packed is prose about nothing. */
  it('catches a label used but never defined', () => {
    const issues = validateH3Prompt(
      input({
        task: 'ref2va',
        ordinals: h3Ordinals([ref({ id: 'i', kind: 'image', image: 'p' })]),
        taskTypes: ['reference generation'],
        subjectDefinitions: [{ label: '<Subject 1>', text: '<Subject 1> is the captain.' }],
        summary: '<Picture 4> is the boat.',
        retentionAnalysis: [{ label: '<Subject 1>', marker: 'fully_preserved', text: 'kept' }]
      })
    );
    expect(issues.some((i) => i.message.includes('<Picture 4>'))).toBe(true);
  });

  /** 🔴 Rejected, never truncated: the last section to be cut is `non_diegetic_music`. */
  it('rejects a prompt past the 7000-character serving limit', () => {
    const long = input({ shots: [{ index: 0, atSeconds: null, body: 'x'.repeat(7100) }] });
    const issue = validateH3Prompt(long).find((i) => i.field === 'prompt');
    expect(issue?.level).toBe('error');
    expect(composeH3Prompt(long).length).toBeGreaterThan(H3_PROMPT_MAX_CHARS);
  });

  /** The 350–500 envelope is stated for `detailed_description` only — warning on the other schema
   *  would be inventing a rule the corpus does not state. */
  it('warns on body length for ref2va only', () => {
    const short = validateH3Prompt(
      input({
        task: 'ref2va',
        ordinals: h3Ordinals([ref({ id: 'i', kind: 'image', image: 'p' })]),
        taskTypes: ['reference generation'],
        subjectDefinitions: [{ label: '<Subject 1>', text: '<Subject 1> is the captain.' }],
        summary: 's',
        retentionAnalysis: [{ label: '<Subject 1>', marker: 'fully_preserved', text: 'kept' }]
      })
    );
    expect(short.some((i) => i.field === 'detailed_description' && i.level === 'warn')).toBe(true);
    expect(
      validateH3Prompt(input()).some((i) => i.field === 'integrated_multimodal_description')
    ).toBe(false);
    // Amber never blocks — that is the difference between the tiers, not a shade of the same thing.
    expect(h3BlocksRender(short.filter((i) => i.level === 'warn'))).toBe(false);
  });

  /** Speaker IDs are a guide violation the model renders through (GUIDE-ref:278) — amber, not red. */
  it('keeps speaker IDs in retention_analysis at warn', () => {
    const issues = validateH3Prompt(
      input({
        task: 'ref2va',
        ordinals: h3Ordinals([ref({ id: 'i', kind: 'image', image: 'p' })]),
        taskTypes: ['reference generation'],
        subjectDefinitions: [{ label: '<Subject 1>', text: '<Subject 1> is the captain.' }],
        summary: 's',
        retentionAnalysis: [
          { label: '<Subject 1>', marker: 'fully_preserved', text: 'kept, (S1) speaks' }
        ]
      })
    );
    const speaker = issues.find((i) => i.message.includes('Speaker IDs'));
    expect(speaker?.level).toBe('warn');
  });
});
