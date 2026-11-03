import { describe, expect, it } from 'vitest';
import type { DirectorClip, DirectorH3Reference } from '#lib/api/client.ts';
import { packGenerations, h3ScopeAfterToggle, pinsAFrame } from './h3-generations';
import { h3TaskTypesForRefs, h3UnjustifiedTaskTypes } from './h3-roles';
import {
  h3ApplyKeyframes,
  h3KeyframesFor,
  h3ReferenceIssues,
  h3UndeclaredKeyframes
} from './h3-keyframes';
import { h3SeedInput, h3WriteKeyframes } from './h3-document';
import { parseH3Block, replaceH3Section } from './h3-block';
import { composeH3Prompt } from './h3-prompt';

/**
 * Stage 5 (references, named and per-shot) and Stage 6 (i2v / keyframes on ref2va).
 *
 * The assertions here are the specification for two things that are silent when wrong:
 *
 * 1. **Scope granularity.** `reference_ids` is per GENERATION because the conditioner builds one
 *    reference presentation per call. The old per-rendered-shot `segments` list described something
 *    the engine cannot do; it is still READ and never written, and both halves are pinned below.
 * 2. **The two frame paths.** An attached image either wire-pins on `fl2va` — throwing away every
 *    reference and the song for a 64 s checkpoint flip — or is declared as a `ref2va` keyframe. The
 *    difference reaches the render through `pinsAFrame`, and everything downstream keys off it.
 */

function clip(over: Partial<DirectorClip> = {}): DirectorClip {
  return { id: over.id ?? 'a', prompt: 'a shot', mode: 't2v', seconds: 4, keyframes: [], ...over };
}
const pack = (clips: DirectorClip[], references: DirectorH3Reference[] = [], stored?: never) =>
  packGenerations({ clips, references, stored });

const mia: DirectorH3Reference = { id: 'mia', kind: 'image', image: 'a', label: 'Mia' };
const wall: DirectorH3Reference = { id: 'wall', kind: 'image', image: 'b', label: 'the wall' };

// ── Stage 5: scope ───────────────────────────────────────────────────────────

describe('h3ScopeAfterToggle — the scope that had no writer', () => {
  /**
   * 🔴 The first toggle MATERIALISES. Until then the scope is `undefined`, meaning "whatever the
   * reference list says" — and taking one reference out has to leave the others exactly where they
   * were rather than resetting them to the project default.
   */
  it('materialises the effective scope on the first toggle', () => {
    expect(h3ScopeAfterToggle([mia, wall], ['mia', 'wall'], 'wall')).toEqual(['mia']);
  });

  /**
   * 🔴 The result is re-derived by filtering the PROJECT list, so it comes back in project order
   * whatever order the author clicked in. Position assigns the ordinal and advances a shared rotary
   * clock, so a scope in a different order is a different render.
   */
  it('keeps project order regardless of click order', () => {
    expect(h3ScopeAfterToggle([mia, wall], ['wall'], 'mia')).toEqual(['mia', 'wall']);
  });

  /** A synthesised keyframe picture belongs to one generation and must never enter a stored scope. */
  it('drops ids that are not project references', () => {
    expect(h3ScopeAfterToggle([mia], ['mia', 'kf:x:first'], 'mia')).toEqual([]);
  });

  /**
   * ⚠️ Empty is a real, reachable state and is NOT absent. `[]` means no references at all — the
   * generation renders as t2va — while `undefined` means every reference the list allows. Collapsing
   * them turns "this is for nobody" into "this is for everybody".
   */
  it('produces an empty list, never undefined, when the last one is turned off', () => {
    const next = h3ScopeAfterToggle([mia], ['mia'], 'mia');
    expect(next).toEqual([]);
    expect(next).not.toBe(undefined);
  });
});

describe('referencesForGeneration — reference_ids wins, segments still resolves', () => {
  it('honours a generation scope written by the new control', () => {
    const gens = packGenerations({
      clips: [clip({ id: 'x' })],
      references: [mia, wall],
      stored: [{ id: 'g', clip_ids: ['x'], reference_ids: ['wall'] }]
    });
    expect(gens[0].references.map((r) => r.id)).toEqual(['wall']);
  });

  /** The old documents. `segments` is read forever and written nowhere. */
  it('falls back to a legacy per-shot segments list when no generation scope is set', () => {
    const gens = pack(
      [clip({ id: 'x' }), clip({ id: 'y' })],
      [
        { ...mia, segments: [1] },
        { ...wall, segments: [] }
      ]
    );
    // `mia` is scoped to rendered shot 1 — the SECOND render, since a render is one shot now.
    // `wall` is scoped to nothing, which is not the same as being scoped to everything.
    expect(gens[1].references.map((r) => r.id)).toEqual(['mia']);
    expect(gens[0].references).toEqual([]);
  });

  /** An explicit empty generation scope beats a permissive legacy one — opposite claims, and the
   *  newer, narrower one is the one somebody actually made. */
  it('lets an empty generation scope override an absent segments scope', () => {
    const gens = packGenerations({
      clips: [clip({ id: 'x' })],
      references: [mia],
      stored: [{ id: 'g', clip_ids: ['x'], reference_ids: [] }]
    });
    expect(gens[0].references).toEqual([]);
    expect(gens[0].task).toBe('t2va');
  });
});

// ── Stage 6: the two frame paths ─────────────────────────────────────────────

describe('pinsAFrame — an attached image is no longer automatically a wire pin', () => {
  it('wire-pins by default, which is what every pre-2026-08-08 document meant', () => {
    expect(pinsAFrame(clip({ mode: 'i2v', image_b64: 'img' }))).toBe(true);
    expect(pinsAFrame(clip({ h3_end_image: 'img' }))).toBe(true);
  });

  it('does not wire-pin when the shot is on the keyframe path', () => {
    expect(pinsAFrame(clip({ mode: 'i2v', image_b64: 'img', h3_frame_mode: 'keyframe' }))).toBe(
      false
    );
  });
});

describe('packGenerations — what each frame path costs', () => {
  /**
   * The expensive path still costs a checkpoint: `fl2va` is a different weight set, so the pinned
   * render drops every reference and the song with them.
   *
   * 🔴 What it no longer costs is a BOUNDARY. It used to force one on both sides and label it
   * `checkpoint`; every clip is its own render now, so the pin decides the task and nothing else.
   * Its neighbours keep the references it had to throw away.
   */
  it('drops every reference from a wire-pinned shot without disturbing its neighbours', () => {
    const gens = pack(
      [clip({ id: 'a' }), clip({ id: 'b', mode: 'i2v', image_b64: 'img' }), clip({ id: 'c' })],
      [mia]
    );
    expect(gens).toHaveLength(3);
    expect(gens[1].breakReason).toBe('forced');
    expect(gens[1].task).toBe('fl2va');
    expect(gens[1].references).toEqual([]);
    expect(gens.map((g) => g.task)).toEqual(['ref2va', 'fl2va', 'ref2va']);
    expect(gens[0].references.map((r) => r.id)).toEqual(['mia']);
  });

  /**
   * 🔴 The whole point of Stage 6: same image, same shot, and the references survive where the wire
   * pin above throws them away and flips the checkpoint.
   */
  it('keeps a keyframe shot on ref2va with the references, where a wire pin would drop them', () => {
    const gens = pack(
      [
        clip({ id: 'a', seconds: 3 }),
        clip({ id: 'b', seconds: 3, mode: 'i2v', image_b64: 'img', h3_frame_mode: 'keyframe' })
      ],
      [mia]
    );
    expect(gens).toHaveLength(2);
    expect(gens.map((g) => g.task)).toEqual(['ref2va', 'ref2va']);
    expect(gens[1].references.map((r) => r.id)).toEqual(['mia', 'kf:b:first']);
    expect(gens[0].references.map((r) => r.id)).toEqual(['mia']);
  });

  /**
   * 🔴 APPENDED, never prepended. A reference's position assigns its ordinal, so a synthesised
   * picture at the front would renumber every picture the prompt already names — silently, on the
   * render after a checkbox was ticked.
   */
  it('gives the keyframe the next free ordinal and leaves every existing one alone', () => {
    const gens = pack(
      [clip({ id: 'b', mode: 'i2v', image_b64: 'img', h3_frame_mode: 'keyframe' })],
      [mia, wall]
    );
    const ks = h3KeyframesFor(gens[0]);
    expect(ks).toHaveLength(1);
    expect(ks[0].label).toBe('<Picture 3>');
    expect(ks[0].shotNumber).toBe(1);
    expect(ks[0].edge).toBe('first');
  });

  /** A keyframe-only generation is still `ref2va`: the synthesised picture IS a reference. */
  it('turns a keyframe shot with no other references into ref2va rather than t2va', () => {
    const gens = pack([clip({ id: 'b', h3_end_image: 'img', h3_frame_mode: 'keyframe' })]);
    expect(gens[0].task).toBe('ref2va');
    expect(h3KeyframesFor(gens[0])[0].edge).toBe('last');
  });
});

// ── Stage 6: what the emitter writes ─────────────────────────────────────────

const keyframeGen = () =>
  pack([clip({ id: 'b', mode: 'i2v', image_b64: 'img', h3_frame_mode: 'keyframe' })], [mia])[0];

describe('the four keyframe declarations', () => {
  /** `GUIDE-ref:132-139`, §2.2 and §4.1 — one line per section, all four or none of it works. */
  it('seeds a block carrying all four', () => {
    const block = composeH3Prompt(h3SeedInput(keyframeGen()));
    expect(block).toContain('<Picture 2> is the first frame of [Shot 1]:');
    expect(block).toContain('[keyframe completion]');
    expect(block).toContain('<Picture 2> ([Shot 1] first frame): fully_preserved -');
    expect(block).toContain('[Shot 1] The shot begins from <Picture 2>.');
  });

  /**
   * 🔴 The reason none of this lives in `composeH3Prompt`: the round-trip gate is
   * `compose(parse(compose(x))) === compose(x)`, and a composer that injected the declarations from
   * a keyframe list would inject them a SECOND time on the recompose.
   */
  it('round-trips byte-exact once seeded', () => {
    const once = composeH3Prompt(h3SeedInput(keyframeGen()));
    const parsed = parseH3Block(once);
    const twice = composeH3Prompt({
      task: 'ref2va',
      durationSeconds: 10,
      shots: parsed.shots,
      styleOpening: parsed.styleOpening ?? undefined,
      taskTypes: parsed.taskTypes,
      subjectDefinitions: parsed.subjectDefinitions,
      summary: parsed.summary ?? '',
      retentionAnalysis: parsed.retentionAnalysis,
      overallSoundscape: parsed.overallSoundscape ?? '',
      nonDiegeticMusic: parsed.nonDiegeticMusic ?? ''
    });
    expect(twice).toBe(once);
  });

  it('is idempotent as a fold: applying twice writes nothing extra', () => {
    const gen = keyframeGen();
    const ks = h3KeyframesFor(gen);
    const once = h3ApplyKeyframes(h3SeedInput(gen), ks);
    expect(composeH3Prompt(h3ApplyKeyframes(once, ks))).toBe(composeH3Prompt(once));
  });
});

describe('h3WriteKeyframes — the surgical repair', () => {
  const bare = [
    'subject_definitions:\n<Picture 1> is Mia, in a red coat.',
    'summary:\n[reference generation] Mia walks.',
    'retention_analysis:\n<Picture 1> (appears in [Shot 1]): fully_preserved - her face is kept.',
    'detailed_description:\nRealistic photographic style.\n[Shot 1] Mia walks past the wall.',
    'overall_soundscape:\nFootsteps.',
    'non_diegetic_music:\nN/A'
  ].join('\n\n');

  const write = (block: string) => {
    const gen = keyframeGen();
    return h3WriteKeyframes(block, 'ref2va', parseH3Block(block), h3KeyframesFor(gen));
  };

  it('writes all four into an existing block', () => {
    const out = write(bare);
    expect(out).toContain('<Picture 2> is the first frame of [Shot 1]:');
    // ⚠️ The bracket comes back in `H3_TASK_TYPES` canonical order, not the order it was written
    // in — `joinTaskTypes` normalises, which is documented and stable rather than incidental.
    expect(out).toContain('[keyframe completion + reference generation]');
    expect(out).toContain('<Picture 2> ([Shot 1] first frame): fully_preserved -');
    expect(out).toContain('[Shot 1] The shot begins from <Picture 2>. Mia walks past the wall.');
  });

  /** 🔴 Never a recompose. An unknown section is bytes nothing touches. */
  it('leaves a section it has never heard of byte-identical', () => {
    const withForeign = `${bare}\n\nfuture_section:\nwhatever MiniMax adds next.`;
    expect(write(withForeign)).toContain('future_section:\nwhatever MiniMax adds next.');
  });

  it('is idempotent — a second pass changes nothing', () => {
    const once = write(bare);
    const twice = h3WriteKeyframes(
      once,
      'ref2va',
      parseH3Block(once),
      h3KeyframesFor(keyframeGen())
    );
    expect(twice).toBe(once);
  });

  /**
   * The check is on the LABEL, never on the sentence. An author who fleshed the definition out must
   * not have it reverted to the template underneath them.
   */
  it('does not overwrite a definition the author already wrote', () => {
    const authored = replaceH3Section(
      bare,
      'subject_definitions',
      '<Picture 1> is Mia, in a red coat.\n<Picture 2> is the first frame of [Shot 1]: Mia stands under the awning, seen from the street.'
    );
    const out = write(authored);
    expect(out).toContain('Mia stands under the awning, seen from the street.');
    // …and it still supplies the three pieces the author did not write.
    expect(out).toContain('<Picture 2> ([Shot 1] first frame): fully_preserved -');
  });

  /** Partial is undeclared: a definition with no retention row still leaves the model guessing how
   *  hard to hold the frame, and the offer has to survive the author writing half of it. */
  it('still reports a half-declared keyframe as undeclared', () => {
    const half = replaceH3Section(
      bare,
      'subject_definitions',
      '<Picture 1> is Mia.\n<Picture 2> is the first frame of [Shot 1]:'
    );
    expect(h3UndeclaredKeyframes(parseH3Block(half), h3KeyframesFor(keyframeGen()))).toHaveLength(
      1
    );
    expect(h3UndeclaredKeyframes(parseH3Block(write(bare)), h3KeyframesFor(keyframeGen()))).toEqual(
      []
    );
  });
});

// ── Per-ASSET exclusivity ────────────────────────────────────────────────────

describe('reference generation vs keyframe completion — per ASSET, not per generation', () => {
  /**
   * 🔴 `GUIDE-ref:135` defines `reference generation` as guidance *"without serving as a concrete
   * frame"*. One asset has one role, one role maps to one task type, so a single asset cannot earn
   * both — the illegal state is unreachable rather than merely validated against.
   */
  it('gives one asset exactly one task type', () => {
    expect(h3TaskTypesForRefs([{ ...mia, role: 'first_frame' }])).toEqual(['keyframe completion']);
    expect(h3TaskTypesForRefs([{ ...mia, role: 'identity_source' }])).toEqual([
      'reference generation'
    ]);
  });

  /** They coexist happily when DIFFERENT assets play the two roles. That is the case the rule
   *  explicitly allows, so it must not be reported. */
  it('allows both in one summary when two assets play the two roles', () => {
    const refs: DirectorH3Reference[] = [
      { ...mia, role: 'identity_source' },
      { ...wall, role: 'first_frame' }
    ];
    expect(h3UnjustifiedTaskTypes(refs, ['reference generation', 'keyframe completion'])).toEqual(
      []
    );
  });

  /** …and reports the claim nothing in scope plays the role for. */
  it('reports a claim no asset justifies', () => {
    const refs: DirectorH3Reference[] = [{ ...mia, role: 'first_frame' }];
    expect(h3UnjustifiedTaskTypes(refs, ['reference generation', 'keyframe completion'])).toEqual([
      'reference generation'
    ]);
  });

  /** 🔴 Silent when nothing carries a role — that is an old document, not a mis-claim, and an amber
   *  note on every generation of every old project is how a tier stops being read. */
  it('says nothing when no reference carries a role at all', () => {
    expect(h3UnjustifiedTaskTypes([mia, wall], ['video editing'])).toEqual([]);
  });
});

// ── The slot a keyframe spends, and nobody was counting ──────────────────────

/**
 * 🔴 A keyframe picture is a REAL `<Picture N>` on the wire, appended to the render's reference
 * list by the packer — so `validate_generative_settings` counts it exactly like any other picture.
 * `ReferencesPanel` has only ever counted the project ROSTER, which a keyframe is not in. A project
 * on the cap plus one keyframe cut was therefore a 400 at submit, after the queue wait, naming a
 * limit the editor had just said it was inside.
 */
describe('engine reference caps count the keyframe pictures too', () => {
  const caps = { maxImages: 2, maxVideos: 2, maxAudios: 2, maxTotal: 12 };
  /** Two roster pictures plus one keyframe = three, over a cap of two. */
  const overGen = () =>
    pack(
      [clip({ id: 'b', mode: 'i2v', image_b64: 'img', h3_frame_mode: 'keyframe' })],
      [mia, wall]
    )[0];

  it('refuses a render whose keyframe pushes it over the picture cap, and says why', () => {
    const gen = overGen();
    const issues = h3ReferenceIssues(
      parseH3Block(composeH3Prompt(h3SeedInput(gen))),
      gen.references,
      h3KeyframesFor(gen),
      caps
    );
    const cap = issues.find((i) => i.message.includes('3 pictures'));
    expect(cap?.level).toBe('error');
    expect(cap?.field).toBe('references');
    // The half a roster count can never explain.
    expect(cap?.message).toContain('a cut opens or closes on');
  });

  /** 🔴 Absent caps accuse nobody: an unfetched limit is unknown, not infinite, and refusing a
   *  legal request because the engine is down is a worse failure than the one being prevented. */
  it('says nothing at all when the caps are unknown', () => {
    const gen = overGen();
    const parsed = parseH3Block(composeH3Prompt(h3SeedInput(gen)));
    const kf = h3KeyframesFor(gen);
    expect(h3ReferenceIssues(parsed, gen.references, kf).some((i) => i.level === 'error')).toBe(
      false
    );
    expect(
      h3ReferenceIssues(parsed, gen.references, kf, null).some((i) => i.level === 'error')
    ).toBe(false);
  });

  it('is silent while the render is inside the caps', () => {
    const gen = keyframeGen();
    const issues = h3ReferenceIssues(
      parseH3Block(composeH3Prompt(h3SeedInput(gen))),
      gen.references,
      h3KeyframesFor(gen),
      caps
    );
    expect(issues.filter((i) => i.field === 'references' && i.level === 'error')).toEqual([]);
  });
});
