import { describe, expect, it } from 'vitest';
import type { DirectorClip, DirectorH3Generation, DirectorH3Reference } from '$lib/api/client';
import {
  H3_DOCUMENTED_MIN_FRAMES,
  H3_MAX_FRAMES,
  H3_MAX_SECONDS,
  H3_MIN_FRAMES,
  canonicalH3Store,
  clipRenders,
  collapseH3Beats,
  dedupeH3GenerationIds,
  generationFrames,
  h3BlockedClipIds,
  mergeBlockedReason,
  packGenerations
} from './h3-generations';

function clip(over: Partial<DirectorClip> = {}): DirectorClip {
  return {
    id: over.id ?? 'a',
    prompt: 'a shot',
    mode: 't2v',
    seconds: 4,
    keyframes: [],
    ...over
  };
}

const pack = (
  clips: DirectorClip[],
  references: DirectorH3Reference[] = [],
  stored?: DirectorH3Generation[]
) => packGenerations({ clips, references, stored });

describe('generationFrames — the grid, the decodable floor and the cap', () => {
  it('snaps UP onto 17n+5', () => {
    // 4 s = 96 frames, which is not on the grid: it rounds UP to 107. That is arithmetic, not a
    // clamp — 107 stopped being a floor on 2026-08-07.
    expect(generationFrames(4)).toBe(H3_DOCUMENTED_MIN_FRAMES);
    expect(generationFrames(10)).toBe(243);
  });

  /**
   * 🔴 The floor is 22 frames — the video VAE's own decodable minimum — NOT 107. While it was 107
   * this function lengthened every shorter generation in silence: a 2-second authored beat
   * rendered as 4.458 s with no error anywhere.
   */
  it('keeps a short generation short, and clamps only at the decodable floor and the cap', () => {
    expect(H3_MIN_FRAMES).toBe(22);
    expect(generationFrames(2)).toBe(56);
    expect(generationFrames(1)).toBe(39);
    // Below what the VAE can decode there genuinely is no render to offer.
    expect(generationFrames(0.05)).toBe(H3_MIN_FRAMES);
    expect(generationFrames(60)).toBe(H3_MAX_FRAMES);
  });
});

describe('packGenerations — the packing rule', () => {
  /**
   * 🔴 **One clip is one generation is one render.** A generation used to hold 1–N clips, which made
   * a clip a BEAT — and that inverted the vocabulary the rest of the product uses and stored every
   * beat's text and cut time twice. Two moments in one render is now two `[Shot N]` lines inside ONE
   * clip's block, so a boundary is never a decision the packer makes: `first` opens, `forced` is
   * every other, because every one of them is a clip the author created.
   */
  it('gives every clip its own generation, however well they would fit together', () => {
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' }), clip({ id: 'c' })]);
    expect(gens.map((g) => g.shots.map((s) => s.clip.id))).toEqual([['a'], ['b'], ['c']]);
    expect(gens.map((g) => g.breakReason)).toEqual(['first', 'forced', 'forced']);
  });

  /**
   * The budget decides nothing any more. These three used to pack into two generations with the
   * second labelled `budget`; that break reason is unreachable now, and the 15.083 s ceiling only
   * survives as the `overBudget` flag on a single over-long shot.
   */
  it('breaks on every clip regardless of the budget', () => {
    const gens = pack([
      clip({ id: 'a', seconds: 8 }),
      clip({ id: 'b', seconds: 8 }),
      clip({ id: 'c', seconds: 4 })
    ]);
    expect(gens.map((g) => g.shots.map((s) => s.clip.id))).toEqual([['a'], ['b'], ['c']]);
    expect(gens.map((g) => g.breakReason)).toEqual(['first', 'forced', 'forced']);
  });

  /**
   * A frame pin used to force a boundary on BOTH sides, because `fl2va` and `ref2va` are different
   * weight sets. It needs no boundary of its own now — it already has one — so the only thing the
   * pin still decides is which checkpoint its own generation lands on.
   */
  it('gives a frame-pinned shot the same forced boundary as any other, and only flips its task', () => {
    const gens = pack([
      clip({ id: 'a' }),
      clip({ id: 'b', mode: 'i2v', image_b64: 'img' }),
      clip({ id: 'c' })
    ]);
    expect(gens.map((g) => g.shots.map((s) => s.clip.id))).toEqual([['a'], ['b'], ['c']]);
    expect(gens.map((g) => g.breakReason)).toEqual(['first', 'forced', 'forced']);
    expect(gens.map((g) => g.task)).toEqual(['t2va', 'fl2va', 't2va']);
  });

  /** One clip is one render, so an over-budget render is simply a clip that is too long. */
  it('flags a render longer than a single call can hold', () => {
    const gens = pack([clip({ id: 'long', seconds: 20 })]);
    expect(gens[0].overBudget).toBe(true);
    // Still clamped to what will actually render, so the timeline arithmetic stays honest.
    expect(gens[0].frames).toBe(H3_MAX_FRAMES);
  });

  /**
   * 🔴 FLAGGED, NOT LENGTHENED. This assertion used to read `seconds ≈ 107/24` — i.e. it asserted
   * that a 2-second beat rendered as 4.458 s. That was the invented 107-frame floor, and the
   * silent correction it caused is exactly what this now guards against.
   */
  it('flags a generation under the DOCUMENTED floor and renders it at the authored length', () => {
    const gens = pack([clip({ seconds: 2 })]);
    expect(gens[0].belowDocumentedFloor).toBe(true);
    // 48 frames snaps UP one grid step to 56 (2.33 s) — it is NOT pushed to 107 (4.46 s).
    expect(gens[0].frames).toBe(56);
    expect(gens[0].shots[0].renderedSeconds).toBeCloseTo(56 / 24, 5);
  });

  it('says nothing about a generation inside the documented range', () => {
    const gens = pack([clip({ seconds: 6 })]);
    expect(gens[0].belowDocumentedFloor).toBe(false);
  });
});

describe('packGenerations — cut times and where the snap-up lands', () => {
  /**
   * 🔴 A generation holds exactly one shot, so its only `[Shot N]` is the untimed opener and
   * `cutAtSeconds` is always null. The cuts INSIDE a render are `[Shot N] At MM:SS.mmm` lines in the
   * block's own text — the packer neither derives them from `clip.seconds` nor stores a second copy
   * of them, which is the duplication the collapse removed.
   */
  it('leaves every shot untimed, because a render opens on its only shot', () => {
    const gens = pack([
      clip({ id: 'a', seconds: 4 }),
      clip({ id: 'b', seconds: 4 }),
      clip({ id: 'c', seconds: 4 })
    ]);
    expect(gens.map((g) => g.shots.map((s) => s.cutAtSeconds))).toEqual([[null], [null], [null]]);
    expect(gens.map((g) => g.shots[0].shotIndex)).toEqual([0, 0, 0]);
  });

  /**
   * The generation rounds UP, and the extra time has to land somewhere. It lands at the END,
   * because that is where a longer render puts it — and the end is now the render's only shot, so
   * the timeline draws the whole shot longer than it was authored.
   */
  it('gives the whole snap-up to the last shot', () => {
    // 3 s = 72 frames, off the grid, so it rounds up to 73.
    const gens = pack([clip({ id: 'a', seconds: 3 })]);
    expect(gens[0].frames).toBe(73);
    expect(gens[0].shots[0].renderedSeconds).toBeCloseTo(gens[0].seconds, 9);
    expect(gens[0].shots[0].renderedSeconds).toBeGreaterThan(3);
  });
});

describe('packGenerations — reference scope and the index space', () => {
  /** Scopes are expressed in RENDERED-shot index space, so a prompt-less clip must not consume one. */
  it('indexes scopes past blank clips', () => {
    const gens = pack([clip({ id: 'blank', prompt: '   ' }), clip({ id: 'real' })]);
    expect(gens.map((g) => g.shots[0].renderedIndex)).toEqual([-1, 0]);
  });

  /**
   * 🔴 On H3 the prompt lives on the GENERATION, so `clip.prompt` is empty for every shot whose
   * block was written in the composer. Deciding `renderedIndex` off `clip.prompt` alone would give
   * every such shot -1 and shift the index space every scope is expressed in.
   */
  it('counts a clip whose prose lives in a stored block as rendering', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'g', clip_ids: ['blocked'], prompt: 'detailed_description:\n[Shot 1] a harbour.' }
    ];
    const gens = pack([clip({ id: 'blocked', prompt: '' }), clip({ id: 'real' })], [], stored);
    expect(gens.map((g) => g.shots[0].renderedIndex)).toEqual([0, 1]);
  });

  it('brings a reference into scope for the render its segment names, and no other', () => {
    const refs: DirectorH3Reference[] = [
      { id: 'r1', kind: 'image', image: 'p', segments: [1] },
      { id: 'r2', kind: 'image', image: 'p', segments: [] }
    ];
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' })], refs);
    // Rendered shot 1 is the SECOND render now, and `r2` is scoped to nothing at all.
    expect(gens[1].references.map((r) => r.id)).toEqual(['r1']);
    expect(gens[1].task).toBe('ref2va');
    expect(gens[0].references).toEqual([]);
    expect(gens[0].task).toBe('t2va');
  });

  /** `undefined` scope = every shot; `[]` = no shot. Collapsing them inverts the meaning. */
  it('keeps an absent scope distinct from an empty one', () => {
    const gens = pack(
      [clip()],
      [
        { id: 'all', kind: 'image', image: 'p' },
        { id: 'none', kind: 'image', image: 'p', segments: [] }
      ]
    );
    expect(gens[0].references.map((r) => r.id)).toEqual(['all']);
  });

  /**
   * A project-wide reference is in scope for every shot — but `fl2va` takes none at all, so a
   * pinned generation must drop them rather than flip to `ref2va` and quietly discard the frame
   * the author placed by hand.
   */
  it('drops every reference from a frame-pinned generation', () => {
    const gens = pack(
      [clip({ id: 'p', mode: 'i2v', image_b64: 'still' })],
      [{ id: 'all', kind: 'image', image: 'p' }]
    );
    expect(gens[0].references).toEqual([]);
    expect(gens[0].task).toBe('fl2va');
  });

  /** A generation-level scope wins outright: per-generation is the granularity the conditioner
   *  has, since it builds ONE reference presentation per call. */
  it('lets a generation-level scope override the per-shot one', () => {
    const refs: DirectorH3Reference[] = [
      { id: 'a', kind: 'image', image: 'p' },
      { id: 'b', kind: 'image', image: 'p', segments: [] }
    ];
    const gens = pack([clip({ id: 'x' })], refs, [
      { id: 'g', clip_ids: ['x'], reference_ids: ['b'] }
    ]);
    expect(gens[0].references.map((r) => r.id)).toEqual(['b']);
  });
});

describe('packGenerations — re-attaching authored sections', () => {
  /**
   * The reason the packing is derived and only the CONTENT is stored: trimming a shot must not
   * orphan a 400-word description, and the stored packing cannot be trusted to still be right.
   *
   * Overlap is also what carries a section through the collapse — `collapseH3Beats` folds a stored
   * run onto its FIRST member, so `g1`'s `['a', 'b']` still overlaps the surviving `a` by one.
   */
  it('follows the sections to whichever generation kept most of their shots', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'g1', clip_ids: ['a', 'b'], summary: 'the first half' },
      { id: 'g2', clip_ids: ['c'], summary: 'the second half' }
    ];
    const gens = pack([clip({ id: 'a' }), clip({ id: 'c' })], [], stored);
    expect(gens.map((g) => g.doc.summary)).toEqual(['the first half', 'the second half']);
  });

  /**
   * 🔴 Adoption carries the WRITING, never the id. The id says which clip a generation opens on, and
   * an adopted entry was written for a different packing — inheriting its id is what divorced ids
   * from clips, and a divorced id is what let two generations end up under one. `g1`/`g2` above are
   * ids no packer would mint, and they do not survive the pack.
   */
  it('mints the id even when it adopts an entry, so the id always names the opening clip', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'g1', clip_ids: ['a', 'b'], summary: 'the first half' },
      { id: 'g2', clip_ids: ['c'], summary: 'the second half' }
    ];
    const gens = pack([clip({ id: 'a' }), clip({ id: 'c' })], [], stored);
    expect(gens.map((g) => g.id)).toEqual(['gen:a', 'gen:c']);
    // …and the id is the doc's own, so a store built from these docs resolves by it.
    expect(gens.map((g) => g.doc.id)).toEqual(['gen:a', 'gen:c']);
  });

  it('mints a stable id for a generation nothing was written for', () => {
    expect(pack([clip({ id: 'a' })])[0].id).toBe('gen:a');
    // Re-packing an unchanged timeline is idempotent.
    expect(pack([clip({ id: 'a' })])[0].id).toBe(pack([clip({ id: 'a' })])[0].id);
  });

  /**
   * 🔴 The production shape, from a 16-shot project on 2026-08-19: two stored entries under one id,
   * each holding its own 5 KB block. Each is still adopted by the right generation (one claim each),
   * but the id that comes back must not be the same twice — every consumer resolves a generation by
   * it, so a duplicate writes one shot's edit into the other shot's document.
   */
  it('never returns two generations under one id', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'gen:a', clip_ids: ['a'], summary: 'first' },
      { id: 'gen:a', clip_ids: ['b'], summary: 'second' }
    ];
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' })], [], stored);
    // Both blocks survive and stay with their own shot…
    expect(gens.map((g) => g.doc.summary)).toEqual(['first', 'second']);
    // …and the second falls back to its own minted id rather than repeating the first's.
    expect(gens.map((g) => g.id)).toEqual(['gen:a', 'gen:b']);
    expect(new Set(gens.map((g) => g.id)).size).toBe(2);
  });

  /**
   * The same store that crashed the Prompts list, packed. Nothing is inherited, so the ids are the
   * two clips' own — the duplicate cannot survive the pack even before the store is repaired.
   */
  it('cannot carry a stored duplicate through, whichever clip it names', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'gen:b', clip_ids: ['a'], summary: 'first' },
      { id: 'gen:b', clip_ids: ['b'], summary: 'second' }
    ];
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' })], [], stored);
    expect(gens.map((g) => g.id)).toEqual(['gen:a', 'gen:b']);
    expect(gens.map((g) => g.doc.summary)).toEqual(['first', 'second']);
  });

  it('rewrites clip_ids to the live packing so a stale list cannot be re-adopted', () => {
    const stored: DirectorH3Generation[] = [{ id: 'g1', clip_ids: ['a', 'gone'], summary: 's' }];
    expect(pack([clip({ id: 'a' })], [], stored)[0].doc.clip_ids).toEqual(['a']);
  });
});

describe('canonicalH3Store — the invariant', () => {
  const three = [clip({ id: 'a' }), clip({ id: 'b' }), clip({ id: 'c' })];

  it('is what the packing produced, keyed by the clip each generation opens on', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'whatever', clip_ids: ['b'], prompt: 'b block' },
      { id: 'whatever', clip_ids: ['c'], prompt: 'c block' }
    ];
    const store = canonicalH3Store(pack(three, [], stored));
    expect(store.map((g) => g.id)).toEqual(['gen:b', 'gen:c']);
    expect(store.map((g) => g.prompt)).toEqual(['b block', 'c block']);
    expect(store.map((g) => g.clip_ids)).toEqual([['b'], ['c']]);
  });

  /**
   * 🔴 The property the whole fix exists for: every id the packer reports resolves to exactly ONE
   * entry in the store. `updateGeneration` looks a generation up with `findIndex` on this array, so
   * anything less than exactly-one is either a write into somebody else's document (more) or a write
   * that appends an entry adoption never picks (fewer).
   */
  it('resolves every generation to exactly one entry', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'gen:a', clip_ids: ['a'], prompt: 'one' },
      { id: 'gen:a', clip_ids: ['b'], prompt: 'two' },
      { id: 'stale', clip_ids: ['c'], prompt: 'three' }
    ];
    const gens = pack(three, [], stored);
    const store = canonicalH3Store(gens);
    for (const g of gens) expect(store.filter((e) => e.id === g.id)).toHaveLength(1);
    expect(store.map((g) => g.prompt)).toEqual(['one', 'two', 'three']);
  });

  it('is a fixed point — canonicalising a canonical store changes nothing', () => {
    const once = canonicalH3Store(pack(three, [], [{ id: 'x', clip_ids: ['a'], prompt: 'p' }]));
    const twice = canonicalH3Store(pack(three, [], once));
    expect(twice).toEqual(once);
    // And again, because a load → save → load cycle is exactly this composition.
    expect(canonicalH3Store(pack(three, [], twice))).toEqual(once);
  });

  it('drops a generation nobody has written for, and one whose clips are gone', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'gen:a', clip_ids: ['a'] }, // id + clip_ids only: nothing authored
      { id: 'gen:gone', clip_ids: ['gone'], prompt: 'orphaned' }
    ];
    expect(canonicalH3Store(pack([clip({ id: 'a' })], [], stored))).toEqual([]);
  });

  it('survives a reorder: the ids follow their clips rather than their positions', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'gen:a', clip_ids: ['a'], prompt: 'A' },
      { id: 'gen:b', clip_ids: ['b'], prompt: 'B' }
    ];
    const flipped = canonicalH3Store(pack([clip({ id: 'b' }), clip({ id: 'a' })], [], stored));
    expect(flipped.map((g) => [g.id, g.prompt])).toEqual([
      ['gen:b', 'B'],
      ['gen:a', 'A']
    ]);
  });
});

describe('dedupeH3GenerationIds — repairing the STORE', () => {
  it('leaves a healthy list alone, and says so', () => {
    expect(
      dedupeH3GenerationIds([
        { id: 'gen:a', clip_ids: ['a'] },
        { id: 'gen:b', clip_ids: ['b'] }
      ])
    ).toBeNull();
    expect(dedupeH3GenerationIds([])).toBeNull();
  });

  it('renames the duplicate to the minted id of its OWN first clip', () => {
    // The production case: the second entry carries the first's id but holds a different clip.
    const out = dedupeH3GenerationIds([
      { id: 'gen:a', clip_ids: ['a'], prompt: 'first block' },
      { id: 'gen:a', clip_ids: ['b'], prompt: 'second block' }
    ]);
    expect(out).toEqual([
      { id: 'gen:a', clip_ids: ['a'], prompt: 'first block' },
      { id: 'gen:b', clip_ids: ['b'], prompt: 'second block' }
    ]);
  });

  it('renames rather than drops — both entries hold real writing', () => {
    const out = dedupeH3GenerationIds([
      { id: 'g', clip_ids: ['a'], prompt: 'one' },
      { id: 'g', clip_ids: ['b'], prompt: 'two' },
      { id: 'g', clip_ids: ['c'], prompt: 'three' }
    ]);
    expect(out?.length).toBe(3);
    expect(out?.map((g) => g.prompt)).toEqual(['one', 'two', 'three']);
    expect(new Set(out?.map((g) => g.id)).size).toBe(3);
  });

  it('is deterministic when even the minted ids collide', () => {
    const twice = () =>
      dedupeH3GenerationIds([
        { id: 'gen:a', clip_ids: ['x'] },
        { id: 'gen:a', clip_ids: [] },
        { id: 'gen:a', clip_ids: [] }
      ]);
    expect(twice()?.map((g) => g.id)).toEqual(['gen:a', 'gen:empty', 'gen:empty#2']);
    expect(twice()).toEqual(twice());
  });

  /** The repair has to agree with the packer, which mints rather than inherits. */
  it('produces a store the packer adopts one-for-one', () => {
    const repaired = dedupeH3GenerationIds([
      { id: 'gen:a', clip_ids: ['a'], summary: 'first' },
      { id: 'gen:a', clip_ids: ['b'], summary: 'second' }
    ]);
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' })], [], repaired ?? []);
    expect(gens.map((g) => g.id)).toEqual(['gen:a', 'gen:b']);
    expect(gens.map((g) => g.doc.summary)).toEqual(['first', 'second']);
  });
});

describe('clipRenders / h3BlockedClipIds — where the prompt actually lives', () => {
  /** The bare form is the LTX test, and it is what every non-H3 caller still passes. */
  it('renders a clip with its own prose, and nothing for a blank one', () => {
    expect(clipRenders(clip({ prompt: 'a shot' }))).toBe(true);
    expect(clipRenders(clip({ prompt: '   ' }))).toBe(false);
  });

  /**
   * 🔴 On H3 the prose is the generation's BLOCK, not `clip.prompt` — a shot written in the composer
   * carries an empty one and renders perfectly well. Without the `written` set every such shot reads
   * as a placeholder and drops out of the rendered-index space the reference scopes are stated in.
   */
  it('renders a blank clip that a stored block covers', () => {
    const written = new Set(['a']);
    expect(clipRenders(clip({ id: 'a', prompt: '' }), written)).toBe(true);
    expect(clipRenders(clip({ id: 'b', prompt: '' }), written)).toBe(false);
  });

  it('collects every clip id a stored generation holds a block for', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'g1', clip_ids: ['a', 'b'], prompt: '[Shot 1] a harbour.' },
      { id: 'g2', clip_ids: ['c'] },
      { id: 'g3', clip_ids: ['d'], prompt: '  \n ' }
    ];
    expect(h3BlockedClipIds(stored)).toEqual(new Set(['a', 'b']));
  });

  /** A cleared box is not a block: it must not keep a clip in the index space it has left. */
  it('ignores a stored generation with no block at all', () => {
    expect(h3BlockedClipIds([{ id: 'g', clip_ids: ['a'] }])).toEqual(new Set());
  });
});

/**
 * The migration off beat-clips.
 *
 * Each stored generation's `clip_ids` is a RECORD of the old packing — the packer emits one group
 * per clip now and can no longer describe that shape — so the run it names is folded back into the
 * one clip it always should have been.
 */
describe('collapseH3Beats', () => {
  const collapse = (clips: DirectorClip[], stored: DirectorH3Generation[]) =>
    collapseH3Beats({ clips, references: [], stored });

  it('folds a stored run into one clip, keeping the first member id and summing the seconds', () => {
    const out = collapse(
      [
        clip({ id: 'a', seconds: 3, prompt: 'the harbour' }),
        clip({ id: 'b', seconds: 4, prompt: 'the captain' }),
        clip({ id: 'c', seconds: 5 })
      ],
      [{ id: 'g1', clip_ids: ['a', 'b'] }]
    )!;
    expect(out.map((c) => c.id)).toEqual(['a', 'c']);
    expect(out[0].seconds).toBe(7);
    // 🔴 The beat bodies live in the block and are read from it. Keeping the head's copy would leave
    // one beat's prose labelling the whole shot everywhere the timeline draws it.
    expect(out[0].prompt).toBe('');
  });

  /**
   * 🔴 The only two per-clip fields `compileH3Timeline` ever reached for: the FIRST member's
   * `image_b64` is the generation's `first_frame` and the LAST member's `h3_end_image` is its
   * `last_frame`. Everything else on a beat-clip was LTX-only.
   */
  it('keeps the first member first frame and the last member end frame', () => {
    const out = collapse(
      [
        clip({ id: 'a', mode: 'i2v', image_b64: 'head', h3_end_image: 'mid' }),
        clip({ id: 'b', image_b64: 'ignored', h3_end_image: 'tail' })
      ],
      [{ id: 'g1', clip_ids: ['a', 'b'] }]
    )!;
    expect(out).toHaveLength(1);
    expect(out[0].image_b64).toBe('head');
    expect(out[0].h3_end_image).toBe('tail');
  });

  /** Opening a project that is already one clip per shot must not mark it dirty. */
  it('returns null when nothing needs collapsing', () => {
    expect(collapse([clip({ id: 'a' })], [{ id: 'g1', clip_ids: ['a'] }])).toBeNull();
    expect(collapse([clip({ id: 'a' })], [])).toBeNull();
  });

  it('leaves a clip that no stored generation covers exactly where it is', () => {
    const out = collapse(
      [clip({ id: 'a' }), clip({ id: 'b' }), clip({ id: 'loose', prompt: 'never opened' })],
      [{ id: 'g1', clip_ids: ['a', 'b'] }]
    )!;
    expect(out.map((c) => c.id)).toEqual(['a', 'loose']);
    expect(out[1].prompt).toBe('never opened');
  });

  /**
   * ⚠️ Runs of ADJACENT members only. A `clip_ids` that no longer describes a contiguous run has
   * been reordered since it was written, and merging across the gap would swallow the shot that was
   * moved between them.
   */
  it('does not merge across a run the timeline has since broken up', () => {
    const out = collapse(
      [clip({ id: 'a' }), clip({ id: 'moved' }), clip({ id: 'b' })],
      [{ id: 'g1', clip_ids: ['a', 'b'] }]
    );
    expect(out).toBeNull();
  });
});

describe('continuation', () => {
  /** The first generation has nothing before it, and the wire omits the field rather than
   *  sending `mode: "none"`. */
  it('gives every generation but the first a resolved continuation', () => {
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' })]);
    expect(gens[0].continuation).toBeNull();
    expect(gens[1].continuation).toEqual({
      mode: 'latents',
      k: 7,
      anchor: true
    });
  });

  it('fills a partially-authored continuation out to the defaults', () => {
    const stored: DirectorH3Generation[] = [
      { id: 'g0', clip_ids: ['a'] },
      { id: 'g1', clip_ids: ['b'], continuation: { mode: 'frame' } }
    ];
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' })], [], stored);
    expect(gens[1].continuation).toMatchObject({ mode: 'frame', k: 7, anchor: true });
  });
});

describe('mergeBlockedReason', () => {
  it('lets an author take back their own break', () => {
    const gens = pack([clip({ id: 'a' }), clip({ id: 'b' })]);
    expect(mergeBlockedReason(gens[0], gens[1])).toBeNull();
  });

  it('never merges past the budget', () => {
    const gens = pack([clip({ id: 'a', seconds: 10 }), clip({ id: 'b', seconds: 10 })]);
    expect(mergeBlockedReason(gens[0], gens[1])).toContain(H3_MAX_SECONDS.toFixed(2));
  });
});
