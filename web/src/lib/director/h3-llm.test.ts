import { describe, expect, it } from 'vitest';
import {
  applyH3Reply,
  h3DraftFrom,
  h3DraftFromBlock,
  isAssembledH3Block,
  parseH3Reply
} from './h3-llm';

const OBJECT = {
  task_types: ['video continuation', 'audio reference'],
  style_opening: 'The target video is in a warm anamorphic style, matching <Video 1>.',
  subject_definitions: [{ label: '<Subject 1>', text: '<Subject 1> is the harbour captain.' }],
  summary: 'The target video continues directly from the end of <Video 1>.',
  retention_analysis: [
    {
      label: '<Video 1>',
      role_note: 'continuation starting point',
      marker: 'fully_preserved',
      text: 'the framing and lighting are carried forward'
    }
  ],
  shots: [
    { index: 0, at_seconds: null, body: 'The shot resumes from the final state of <Video 1>.' },
    { index: 1, at_seconds: 4.5, body: "a close-up of the captain's face." }
  ],
  overall_soundscape: 'Hull timbers groan under the swell.',
  non_diegetic_music: 'N/A'
};

describe('parseH3Reply', () => {
  it('reads the contract object out of the JSON the prompt endpoints return', () => {
    const r = parseH3Reply(JSON.stringify(OBJECT))!;
    expect(r.task_types).toEqual(['video continuation', 'audio reference']);
    expect(r.subject_definitions).toHaveLength(1);
    expect(r.shots.map((s) => s.at_seconds)).toEqual([null, 4.5]);
    expect(r.non_diegetic_music).toBe('N/A');
  });

  /**
   * The prose path is gone from the backend but not from the world: older projects, older koblem
   * builds and the chat surface all still return strings. Returning `null` is what routes them to
   * the shot body they were always drafted for, so nothing is lost when a stale koblem answers.
   */
  it.each([
    ['prose', 'A harbour at dawn, the captain waits at the rail.'],
    ['an assembled block', 'integrated_multimodal_description: [Shot 1] a harbour at dawn.'],
    ['broken JSON', '{"shots": ['],
    ['unrelated JSON', '{"ok": true}'],
    ['nothing', '']
  ])('returns null for %s', (_label, text) => {
    expect(parseH3Reply(text)).toBeNull();
  });

  /** Every field but `shots` is optional, so a reply carrying only audio is still structured. */
  it('recognises a partial reply', () => {
    const r = parseH3Reply('{"overall_soundscape":"Rain on the awning."}')!;
    expect(r.overall_soundscape).toBe('Rain on the awning.');
    expect(r.shots).toEqual([]);
  });

  /** A half-migrated backend sends the bodies as bare strings; the composer re-times them anyway. */
  it('accepts shot bodies as bare strings', () => {
    const r = parseH3Reply('{"shots":["one","two"]}')!;
    expect(r.shots).toEqual([
      { index: 0, at_seconds: null, body: 'one' },
      { index: 1, at_seconds: null, body: 'two' }
    ]);
  });

  /** The contract is expected to grow, and an unknown key must not invalidate the whole reply. */
  it('ignores keys it does not know', () => {
    expect(parseH3Reply('{"summary":"s","invented_later":42}')?.summary).toBe('s');
  });
});

describe('applyH3Reply', () => {
  /**
   * 🔴 **The per-shot mapping and the `focusIndex` anchor are RETIRED**, and the tests that pinned
   * them went with them. They were not wrong about anything; they described a problem that only
   * exists if the reply is structured per beat.
   *
   * What replaced them: `absorbH3Reply` stores the reply's BLOCK verbatim, and that is the end of
   * it. One clip is one shot, so a reply that writes three `[Shot N]` lines is not an
   * under-supplied mapping onto three clips — it is one shot's prompt with three beats in it, and
   * nothing is derived back down.
   */
  it('reads the reply into the generation patch', () => {
    const patch = applyH3Reply(parseH3Reply(JSON.stringify(OBJECT))!);
    expect(patch.generation.summary).toBe(OBJECT.summary);
    expect(patch.generation.style_opening).toBe(OBJECT.style_opening);
    expect(patch.warnings).toEqual([]);
  });

  /** Kept and reported, never silently corrected — a marker replaced with a legal one would be a
   *  claim about the render that nobody made. */
  it('keeps an illegal retention marker and warns about it', () => {
    const bad = {
      ...OBJECT,
      retention_analysis: [{ label: '<Audio 1>', marker: 'fully_preserved', text: 'kept' }]
    };
    const patch = applyH3Reply(parseH3Reply(JSON.stringify(bad))!);
    expect(patch.generation.retention_analysis?.[0].marker).toBe('fully_preserved');
    expect(patch.warnings[0]).toContain('<Audio 1>');
  });

  /** An absent field means "leave what the human wrote alone", so the patch must not carry it. */
  it('patches only the fields the reply actually carried', () => {
    const patch = applyH3Reply(parseH3Reply('{"summary":"s"}')!);
    expect(Object.keys(patch.generation)).toEqual(['summary']);
  });

  /**
   * The seed record still has to arrive intact, because it is all an older project carries — a
   * generation with no `prompt` opens from exactly these fields (`seedFields`).
   */
  it('carries every section the seed record is composed from', () => {
    const patch = applyH3Reply(parseH3Reply(JSON.stringify(OBJECT))!);
    expect(patch.generation.subject_definitions?.length).toBeGreaterThan(0);
    expect(patch.generation.overall_soundscape).toBe(OBJECT.overall_soundscape);
  });
});

const REF_BLOCK = [
  'subject_definitions:',
  '<Subject 1> is the harbour captain, weathered, in a navy pea coat.',
  '',
  'summary:',
  '[reference generation + audio reference] The target video follows <Subject 1> along the quay.',
  '',
  'retention_analysis:',
  '<Subject 1> (appears in [Shot 1]): fully_preserved - the coat and the weathered face are kept.',
  '',
  'detailed_description:',
  'The target video is in a warm anamorphic style.',
  '[Shot 1] <Subject 1> walks the quay as the light drops.',
  '[Shot 2] At 00:04.500, the camera cuts to a close-up of his face.',
  '',
  'overall_soundscape:',
  'Hull timbers groan under the swell.',
  '',
  'non_diegetic_music:',
  'N/A'
].join('\n');

describe('h3DraftFromBlock — carrier 3', () => {
  /**
   * The writer now returns the BLOCK, so this is the carrier that does the work. Every section has
   * to survive the re-key into the draft shape, because anything that does not arrives as "the
   * author left it empty" and the composer will show a ghost placeholder over text that exists.
   */
  it('reads a whole ref2va block into the draft shape', () => {
    const d = h3DraftFromBlock(REF_BLOCK)!;
    expect(d.task_types).toEqual(['reference generation', 'audio reference']);
    expect(d.style_opening).toBe('The target video is in a warm anamorphic style.');
    expect(d.subject_definitions).toEqual([
      { label: '<Subject 1>', text: 'is the harbour captain, weathered, in a navy pea coat.' }
    ]);
    expect(d.summary).toBe('The target video follows <Subject 1> along the quay.');
    expect(d.retention_analysis?.[0]).toMatchObject({
      label: '<Subject 1>',
      role_note: 'appears in [Shot 1]',
      marker: 'fully_preserved'
    });
    expect(d.overall_soundscape).toBe('Hull timbers groan under the swell.');
    expect(d.non_diegetic_music).toBe('N/A');
    // The cut time comes back in seconds, and the tag/timestamp are stripped off the bodies.
    expect(d.shots.map((s) => s.at_seconds)).toEqual([null, 4.5]);
    expect(d.shots[1].body).toBe('the camera cuts to a close-up of his face.');
  });

  /** The FL2VA family folds its style INSIDE `[Shot 1]`, so there is no separate style opening. */
  it('reads a three-section block without inventing a style opening', () => {
    const d = h3DraftFromBlock(
      'integrated_multimodal_description: [Shot 1] Live-action, a harbour at dawn.\n\noverall_soundscape: Gulls.\n\nnon_diegetic_music: N/A'
    )!;
    expect(d.style_opening).toBeUndefined();
    expect(d.shots).toHaveLength(1);
    expect(d.overall_soundscape).toBe('Gulls.');
  });

  /** Prose is not a block. `null` is what routes it to the shot body, which is where it belongs. */
  it.each(['a harbour at dawn, the captain waits', '[Shot 1] the captain turns', '', undefined])(
    'refuses text that is not a block',
    (text) => {
      expect(h3DraftFromBlock(text)).toBeNull();
    }
  );
});

describe('h3DraftFrom — four carriers, one door', () => {
  /**
   * 🔴 A rollout has all four live at once. The typed field must keep winning while it is being
   * sent, or a healthy deploy would silently downgrade every draft to a lossier path; and the block
   * must be read when it is not, or the current writer's reply lands on a shot body as prose.
   */
  it('prefers the typed field over anything in the text', () => {
    const typed = { shots: [{ index: 0, at_seconds: null, body: 'typed' }] };
    expect(h3DraftFrom(typed, REF_BLOCK)).toBe(typed);
  });

  it('falls back to JSON in the string', () => {
    expect(h3DraftFrom(undefined, JSON.stringify(OBJECT))?.summary).toBe(OBJECT.summary);
  });

  it('falls back to the raw block', () => {
    expect(h3DraftFrom(undefined, REF_BLOCK)?.overall_soundscape).toBe(
      'Hull timbers groan under the swell.'
    );
  });

  it('gives up on prose, so the caller can drop it on the shot body', () => {
    expect(h3DraftFrom(undefined, 'the captain turns to camera')).toBeNull();
  });
});

describe('isAssembledH3Block', () => {
  /**
   * The chat surface still emits assembled blocks, and a block pasted into one shot gets nested
   * inside `[Shot N]` — producing a prompt with `summary:` in the middle of a shot description and
   * nothing in the shot list to say so. Detected so the composer can show it, not repaired: guessing
   * which of six sections each paragraph belongs to and getting it wrong is worse done quietly.
   */
  it.each([
    'subject_definitions:\n<Subject 1> is the captain.',
    'integrated_multimodal_description: [Shot 1] a harbour.',
    'For the target video, at 0.00 seconds into the target video, <Picture 1> (from [Shot 1]) is fully referenced.',
    '[Shot 1] a harbour.\n\noverall_soundscape: gulls'
  ])('spots an assembled block', (text) => {
    expect(isAssembledH3Block(text)).toBe(true);
  });

  it.each(['a harbour at dawn, the captain waits', '[Shot 1] the captain turns to camera', ''])(
    'leaves an ordinary shot body alone',
    (text) => {
      expect(isAssembledH3Block(text)).toBe(false);
    }
  );
});
