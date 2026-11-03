import { describe, expect, it } from 'vitest';
import type { DirectorH3Generation } from '#lib/api/client.ts';
import { parseH3Block } from './h3-block';
import { composeH3Prompt } from './h3-prompt';
import {
  h3DefinitionsBody,
  h3RetentionBody,
  h3StripCutVerb,
  h3SummaryBody,
  needsH3PromptSeed,
  writeH3MainSection
} from './h3-document';

/**
 * The document rules, and the one that settled the vocabulary: **the block owns a shot's body**.
 *
 * It used to live in two places — `DirectorClip.prompt` and the block's `[Shot N]` — with a mirror
 * copying one onto the other on every keystroke, and the plan's own risk table named getting that
 * wrong as a silent desynchronisation of the timeline from what renders. One clip is one shot now
 * and the block is the only store of its prose and its cut times, so there is nothing left to
 * desynchronise. The assertions below are the specification of what is left.
 */

describe('needsH3PromptSeed', () => {
  /** An emptied box is a decision. Re-seeding it would be the old override's confirm dialog again. */
  it('does not want a seed once the author has a prompt, even an empty one', () => {
    expect(needsH3PromptSeed({ id: 'g', clip_ids: [], prompt: '' } as DirectorH3Generation)).toBe(
      false
    );
  });
});

/**
 * The panels that edit a LIST cannot go through `replaceH3Section` with a body somebody typed —
 * they have to emit one. These three emitters therefore duplicate formatting that lives inside
 * `composeH3Prompt`, and the only thing stopping them drifting from it is this block.
 */
describe('one-section emitters agree with composeH3Prompt', () => {
  const defs = [
    { label: '<Subject 1>', text: 'is the harbour captain, grey beard.' },
    // The corpus writes the label into the sentence; the editor shows it separately. Typing it
    // again must not emit it twice.
    { label: '<Subject 2>', text: '<Subject 2> is the mate.' }
  ];
  const retention = [
    {
      label: '<Subject 1>',
      role_note: 'appears in [Shot 1]',
      marker: 'fully_preserved',
      text: 'identity and wardrobe are retained'
    },
    { label: '<Audio 1>', marker: 'reference', text: 'the rhythm is followed, not copied' }
  ];
  const composed = composeH3Prompt({
    task: 'ref2va',
    durationSeconds: 10,
    shots: [{ index: 0, atSeconds: null, body: 'a harbour.' }],
    taskTypes: ['reference generation', 'audio reference'],
    summary: 'The target video shows the captain at the rail.',
    subjectDefinitions: defs,
    retentionAnalysis: retention
  });
  const bodyOf = (name: string) =>
    parseH3Block(composed).sections.find((s) => s.name === name)?.body ?? '';

  it('emits the same subject_definitions body', () => {
    expect(h3DefinitionsBody(defs)).toBe(bodyOf('subject_definitions'));
    expect(h3DefinitionsBody(defs)).toContain('<Subject 2> is the mate.');
    expect(h3DefinitionsBody(defs)).not.toContain('<Subject 2> <Subject 2>');
  });

  it('emits the same retention_analysis body', () => {
    expect(h3RetentionBody(retention)).toBe(bodyOf('retention_analysis'));
  });

  it('emits the same summary body, task-type bracket included', () => {
    expect(
      h3SummaryBody(
        ['reference generation', 'audio reference'],
        'The target video shows the captain at the rail.'
      )
    ).toBe(bodyOf('summary'));
  });

  /**
   * 🔴 The one deliberate divergence. `composeH3Prompt` FILTERS a row with no text; the lens must
   * not, because the block is the document and a row the author just added has nowhere else to
   * live — it would vanish from the panel the instant it was created.
   */
  it('keeps a row the author has not typed into yet', () => {
    expect(h3DefinitionsBody([...defs, { label: '<Subject 3>', text: '' }])).toBe(
      `${bodyOf('subject_definitions')}\n<Subject 3>`
    );
    const withBlank = h3RetentionBody([
      ...retention,
      { label: '<Video 1>', marker: 'fully_preserved', text: '' }
    ]);
    expect(withBlank).toBe(`${bodyOf('retention_analysis')}\n<Video 1>: fully_preserved - `);
    // 🔴 And it has to survive the trip back, or the row disappears from the panel that created it.
    const reparsed = parseH3Block(
      `retention_analysis:\n${withBlank}\n\ndetailed_description:\n[Shot 1] x`
    );
    expect(reparsed.retentionAnalysis.length).toBe(3);
    expect(reparsed.retentionAnalysis[2]).toEqual({
      label: '<Video 1>',
      marker: 'fully_preserved',
      text: ''
    });
    expect(reparsed.unparsed).toEqual([]);
  });
});

describe('h3StripCutVerb — swapping a verb the parser left in the body', () => {
  /**
   * 🔴 Without the strip, choosing a new verb stacks it on the old one: the body already opens with
   * a verb, so `shotText` sees no match for the NEW one and prefixes it anyway.
   */
  it('removes a leading verb so the new one does not stack', () => {
    expect(h3StripCutVerb('the shot cuts to the harbour mouth.')).toBe('the harbour mouth.');
    const rebuilt = writeH3MainSection(
      'detailed_description:\n[Shot 1] a.\n[Shot 2] At 00:04.500, the shot cuts to the harbour mouth.',
      'ref2va',
      [
        { index: 0, atSeconds: null, body: 'a.' },
        {
          index: 1,
          atSeconds: 4.5,
          cutVerb: 'the shot switches to',
          body: h3StripCutVerb('the shot cuts to the harbour mouth.')
        }
      ],
      null
    );
    expect(rebuilt).toContain('[Shot 2] At 00:04.500, the shot switches to the harbour mouth.');
    expect(rebuilt).not.toContain('the shot cuts to');
  });

  it('leaves a body that opens with no known verb alone', () => {
    expect(h3StripCutVerb('the harbour mouth, seen from the deck.')).toBe(
      'the harbour mouth, seen from the deck.'
    );
  });
});
