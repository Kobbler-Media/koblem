/**
 * The H3 generation's **document rules** — what owns a shot's prompt.
 *
 * `h3-prompt.ts` composes a block and `h3-block.ts` reads one back. This module is the third
 * piece: it says which of the two is the ARTIFACT.
 *
 * ## The reversal, in one line
 *
 * `DirectorH3Generation.prompt` is the stored artifact. The panels are a lens on it. Nothing
 * recomposes the whole block from parsed fields — a field edit rewrites one section's bytes
 * (`replaceH3Section`) so anything the parser could not represent survives untouched.
 *
 * ## 🔴 One store, and the duplicate it replaced
 *
 * A shot's prose lives in the block and NOWHERE else. `DirectorClip.prompt` used to carry a copy of
 * each `[Shot N]` body, kept in step by a positional mirror — which stored every beat's text twice
 * and its cut time twice, and made "the timeline has not caught up with the prompt" a permanent
 * fixture of the editor. One clip is one shot is one render now (see `collapseH3Beats`), a beat is
 * a timed line inside that shot's prose, and there is no second copy to disagree with.
 */

import type {
  DirectorH3Generation,
  H3RetentionEntry,
  H3SubjectDefinition,
  H3Task,
  H3TaskType
} from '$lib/api/client';
import { parseH3Block, replaceH3Section, type H3ParsedBlock, type H3SectionName } from './h3-block';
import {
  H3_TRANSITION_VERBS,
  composeH3Prompt,
  h3Ordinals,
  joinTaskTypes,
  type H3Alignment,
  type H3ComposeInput
} from './h3-prompt';
import type { PackedGeneration } from './h3-generations';
import {
  H3_KEYFRAME_TASK_TYPE,
  h3ApplyKeyframes,
  h3KeyframeDefinitionText,
  h3KeyframeHasOpener,
  h3KeyframeOpener,
  h3KeyframeRetention,
  h3KeyframesFor,
  type H3KeyframeDeclaration
} from './h3-keyframes';

/**
 * ⚠️ There is deliberately NO migration from the retired `prompt_override`.
 *
 * The field existed for about a day and the owner confirmed 2026-08-08 that no stored generation
 * was worth keeping, so a document still carrying one simply seeds a fresh block from its stored
 * sections like any other — `needsH3PromptSeed` tests for the KEY, and an old document has no
 * `prompt` key. The override text is dropped with the rest of the unknown fields.
 *
 * Written down because "where did the migration go" is otherwise a reasonable thing to go looking
 * for: its absence is a decision, not an omission.
 */

/**
 * Does this generation still need its one-time seed?
 *
 * The test is on the KEY, not on the content: once the author has a block they are allowed to empty
 * it, and re-seeding an emptied box under their cursor is the behaviour the old override had and
 * the reason it needed a confirm dialog.
 */
export const needsH3PromptSeed = (gen: DirectorH3Generation): boolean =>
  typeof gen.prompt !== 'string';

/**
 * Which alignment instruction line the FL2VA family needs, from what the shots pin.
 *
 * 🔴 Lives here, and is used by BOTH the seed and `compileH3Timeline`, because the two used to
 * disagree: the editor's preview omitted the line and the compiler added it, so the block on screen
 * was not the block that rendered. That was survivable while the compiler composed at submit; now
 * the stored block IS the request, so an omitted line is an alignment instruction that never
 * reaches the model — and the pin it describes silently stops meaning anything.
 */
export function h3AlignmentFor(gen: PackedGeneration): H3Alignment {
  if (gen.task !== 'fl2va') return 'none';
  const first = gen.shots[0]?.clip;
  const last = gen.shots[gen.shots.length - 1]?.clip;
  const hasFirst = !!(first && first.mode === 'i2v' && first.image_b64);
  const hasLast = !!last?.h3_end_image;
  if (hasFirst && hasLast) return 'fl2va';
  if (hasFirst) return 'i2va';
  if (hasLast) return 'l2va';
  return 'none';
}

/**
 * The compose input for a generation that has no block yet — its stored fields and its clips.
 *
 * ⚠️ The per-section fields it reads (`doc.summary`, `doc.subject_definitions`, …) stop being
 * written the moment `prompt` exists. They are a **seed record**: what a project saved before
 * 2026-08-08 carries, and what makes it open showing the block it always rendered rather than an
 * empty one. Nothing else reads them.
 *
 * One definition, two callers — the composer's seed effect and `compileH3Timeline`'s fallback — so
 * that a generation nobody has opened compiles to exactly what opening it would have stored.
 */
export function h3SeedInput(gen: PackedGeneration): H3ComposeInput {
  // 🔴 The keyframe declarations are folded in HERE and not inside `composeH3Prompt`. The composer
  // is a pure emitter of the fields it is handed, and it has to stay one: the round-trip gate is
  // `compose(parse(compose(x))) === compose(x)`, so a composer that injected these four lines from
  // a keyframe list would inject them a second time on the recompose and double every one of them.
  // See `h3-keyframes.ts`. This is also why the fold is idempotent against its own input — a seed
  // that already carries the declarations (an old document's stored fields) is left alone.
  return h3ApplyKeyframes(seedFields(gen), h3KeyframesFor(gen));
}

/**
 * The stored fields of a generation, before the keyframe declarations are folded in.
 *
 * 🔴 **DO NOT TIDY `s.clip.h3_cut_verb` OUT OF HERE.** It looks dead — the editor's cut/beat toggle
 * reads the BLOCK now (`h3ShotIsCut`), nothing writes `h3_cut_verb` any more, and the obvious
 * conclusion is that a stale one on an old document is noise to be dropped. It is not noise, and the
 * reason is narrow enough to be worth writing down:
 *
 * This function is the ONLY reader of the per-section seed fields, and it runs in exactly two
 * places — the composer's one-time seed effect, and `compile.ts:420`, the fallback that composes a
 * block for a generation **nobody has ever opened**. A generation WITH a `prompt` never reaches
 * either. So the only document whose `h3_cut_verb` can still reach the wire is one that predates the
 * reversal and has never been opened, and for that document the verb is the ONLY record of whether
 * its author asked for a cut. Clearing it would silently turn every such cut into a beat, in a
 * project the author has not touched, on the render.
 *
 * ⚠️ Once a block exists the verb lives in the block's text, where `readShot` finds it and
 * `h3StripCutVerb` swaps it. The two never disagree because they are never both consulted.
 */
function seedFields(gen: PackedGeneration): H3ComposeInput {
  const doc = gen.doc;
  return {
    task: gen.task,
    durationSeconds: gen.seconds,
    shots: gen.shots.map((s) => ({
      index: s.shotIndex,
      atSeconds: s.cutAtSeconds,
      body: s.clip.prompt.trim(),
      ...(s.clip.h3_cut_verb ? { cutVerb: s.clip.h3_cut_verb } : {}),
      ...(s.clip.h3_scenetrans ? { scenetrans: true } : {})
    })),
    ordinals: h3Ordinals(gen.references),
    ...(doc.style_opening ? { styleOpening: doc.style_opening } : {}),
    ...(doc.task_types?.length ? { taskTypes: doc.task_types } : {}),
    ...(doc.subject_definitions?.length ? { subjectDefinitions: doc.subject_definitions } : {}),
    ...(doc.summary ? { summary: doc.summary } : {}),
    ...(doc.retention_analysis?.length ? { retentionAnalysis: doc.retention_analysis } : {}),
    ...(doc.overall_soundscape ? { overallSoundscape: doc.overall_soundscape } : {}),
    ...(doc.non_diegetic_music ? { nonDiegeticMusic: doc.non_diegetic_music } : {}),
    alignment: h3AlignmentFor(gen)
  };
}

/**
 * Compose the block a generation starts life with.
 *
 * A thin alias for `composeH3Prompt` so the seeding call has a name that says it is the ONE place
 * composition is still allowed to produce a whole block. Everything after this point is
 * `replaceH3Section` on the text this returned.
 */
export const seedH3Prompt = (gen: PackedGeneration): string => composeH3Prompt(h3SeedInput(gen));

/** Which section carries the `[Shot N]` bodies on this checkpoint. */
export const h3MainSectionFor = (task: H3Task): H3SectionName =>
  task === 'ref2va' ? 'detailed_description' : 'integrated_multimodal_description';

// ── clip → block: the explicit push ──────────────────────────────────────────

/** One packed shot, as the realign needs to see it. */
export interface H3ShotPlacement {
  /** 0-based within the generation — `index + 1` is the `[Shot N]` number. */
  index: number;
  /** Where the cut falls inside the generation. `null` on index 0, which is untimed. */
  atSeconds: number | null;
  cutVerb?: string;
  scenetrans?: boolean;
  /** The body to write. Callers pass `clip.prompt` to push, or the parsed body to merely retime. */
  body: string;
}

/**
 * Rewrite the main section with an explicit style opening — what the Style panel edits.
 *
 * ⚠️ `styleOpening` is meaningful on **Ref2VA only**, where it sits on its own line before
 * `[Shot 1]`. The FL2VA family folds it INSIDE `[Shot 1]` with no marker separating it from the
 * shot's prose, so `parseH3Block` deliberately leaves it in the body rather than guessing where it
 * ends — and passing one here on that checkpoint would emit it a second time. Callers pass `null`
 * there and the style is edited as the front of `[Shot 1]`, which is where the schema puts it.
 */
export function writeH3MainSection(
  block: string,
  task: H3Task,
  shots: H3ShotPlacement[],
  styleOpening: string | null
): string {
  const style = task === 'ref2va' ? styleOpening?.trim() || undefined : undefined;
  const composed = composeH3Prompt({
    task,
    // Only reachable through the FL2VA alignment line, which is composed separately and is not in
    // this section — so any value produces the same main body.
    durationSeconds: 0,
    shots: shots.map((s) => ({
      index: s.index,
      atSeconds: s.atSeconds,
      body: s.body.trim(),
      ...(s.cutVerb ? { cutVerb: s.cutVerb } : {}),
      ...(s.scenetrans ? { scenetrans: true } : {})
    })),
    ...(style ? { styleOpening: style } : {})
  });
  // Read the body straight back out of a freshly composed block rather than exporting the emitter's
  // private body builders. The round-trip is the pinned guarantee (`h3-block.test.ts`), so this is
  // the emitter's own output by construction and cannot drift from it.
  const main = h3MainSectionFor(task);
  const body = parseH3Block(composed).sections.find((s) => s.name === main)?.body ?? '';
  return replaceH3Section(block, main, body);
}

/**
 * Write the four keyframe-completion declarations into an EXISTING block.
 *
 * 🔴 Up to four `replaceH3Section` writes, never a recompose of the whole block. Everything the
 * parser could not represent — a section MiniMax adds next month, a malformed retention line, a
 * paragraph left between two headers — keeps its bytes, exactly as for every other panel edit.
 *
 * What each one writes (`GUIDE-ref:132-139`, §2.2 and §4.1):
 *
 * | section | line |
 * |---|---|
 * | `subject_definitions` | `<Picture 1> is the first frame of [Shot 1]:` |
 * | `summary` | `keyframe completion` joined into the `[…]` bracket |
 * | `retention_analysis` | `<Picture 1> ([Shot 1] first frame): fully_preserved - …` |
 * | `detailed_description` | `The shot begins from <Picture 1>.` at the FRONT of that shot's body |
 *
 * **Idempotent per piece.** Run it twice and nothing moves. Run it on a block where the author
 * already wrote the definition in their own words and only the three pieces they did not write are
 * added — a fleshed-out definition is never reverted to the template, because the check is on the
 * LABEL, not on the sentence.
 *
 * ⚠️ The main section IS recomposed from the parsed shots (via {@link writeH3MainSection}), which is
 * the smallest blast radius available there: the shot bodies are all that is in it, and splicing
 * individual `[Shot N]` runs would have to re-derive the `<scenetrans>` pairing by hand — the one
 * piece of the emitter written from a NEIGHBOUR's flag.
 */
export function h3WriteKeyframes(
  block: string,
  task: H3Task,
  parsed: H3ParsedBlock,
  keyframes: readonly H3KeyframeDeclaration[]
): string {
  if (!keyframes.length) return block;
  let out = block;

  const defs = [...parsed.subjectDefinitions];
  const ret = [...parsed.retentionAnalysis];
  let defsChanged = false;
  let retChanged = false;
  for (const k of keyframes) {
    if (!defs.some((d) => d.label === k.label)) {
      defs.push({ label: k.label, text: h3KeyframeDefinitionText(k) });
      defsChanged = true;
    }
    if (!ret.some((e) => e.label === k.label)) {
      ret.push(h3KeyframeRetention(k));
      retChanged = true;
    }
  }
  if (defsChanged) out = replaceH3Section(out, 'subject_definitions', h3DefinitionsBody(defs));
  if (retChanged) out = replaceH3Section(out, 'retention_analysis', h3RetentionBody(ret));

  if (!parsed.taskTypes.includes(H3_KEYFRAME_TASK_TYPE)) {
    out = replaceH3Section(
      out,
      'summary',
      h3SummaryBody([...parsed.taskTypes, H3_KEYFRAME_TASK_TYPE], parsed.summary ?? '')
    );
  }

  const missingOpeners = keyframes.filter((k) => !h3KeyframeHasOpener(parsed, k));
  if (missingOpeners.length) {
    const shots: H3ShotPlacement[] = parsed.shots.map((s) => ({
      index: s.index,
      atSeconds: s.atSeconds,
      body: s.body,
      ...(s.cutVerb ? { cutVerb: s.cutVerb } : {}),
      ...(s.scenetrans ? { scenetrans: true } : {})
    }));
    for (const k of missingOpeners) {
      const shot = shots[k.shotNumber - 1];
      // 🔴 The FRONT of the body, not the end. `shotText` writes
      // `[Shot N] At MM:SS.mmm, <verb> <body>`, so an opener appended after the prose would tell
      // the model the shot begins from a picture in the sentence after it has already described the
      // whole shot. The position is the instruction.
      if (shot) shot.body = `${h3KeyframeOpener(k)} ${shot.body}`.trim();
    }
    // ⚠️ The style opening comes from the BLOCK, not the document — on the FL2VA family it is folded
    // inside `[Shot 1]` and the parser deliberately leaves it there, so passing one would emit it
    // twice. Ref2VA is the only checkpoint with a separable style opening, and the only one
    // keyframe completion applies to at all.
    out = writeH3MainSection(out, task, shots, task === 'ref2va' ? parsed.styleOpening : null);
  }

  return out;
}

/**
 * Strip a leading cut verb so a new one can be chosen without stacking.
 *
 * 🔴 `readShot` deliberately leaves the verb IN the body — that is what makes the round-trip
 * byte-exact, because `shotText` prefixes `cutVerb` only when the body does not already open with
 * it. The cost is exactly here: swapping the verb without stripping the old one gives
 * *"the shot switches to the shot cuts to the harbour"*, since the body opens with a verb that is
 * no longer the chosen one. Matching is case-insensitive and longest-first, so a verb that is a
 * prefix of another cannot claim it.
 */
export function h3StripCutVerb(body: string): string {
  const t = body.trimStart();
  // Widened with `readShot` — the two must agree about what a verb IS, or a swap strips nothing
  // and the body ends up opening "the shot cuts to the shot cross-dissolves to …".
  const hit = [...H3_TRANSITION_VERBS]
    .sort((a, b) => b.length - a.length)
    .find((v) => t.toLowerCase().startsWith(v.toLowerCase()));
  return hit ? t.slice(hit.length).trimStart() : t;
}

// ── one-section emitters, for the panels that edit a LIST ────────────────────

/**
 * A `subject_definitions` body from the editor's rows.
 *
 * ⚠️ Deliberately does **not** drop a row whose text is empty, which is the one place it diverges
 * from `composeH3Prompt`'s own filter. Under the lens the block IS the document, so a row the
 * author has just added and not yet typed into has nowhere else to live — filtering it would make
 * the row disappear from the panel the instant it was created, because the panel is redrawn from
 * the block. A bare label is honest: it is in the block, it will be sent, and `validateH3Prompt`
 * already calls an empty definition an error.
 *
 * `h3-document.test.ts` pins this against `composeH3Prompt` for populated rows, so the two cannot
 * drift on the case where both apply.
 */
export const h3DefinitionsBody = (defs: H3SubjectDefinition[]): string =>
  defs
    .map((d) => {
      const t = d.text.trim();
      // The corpus writes the label into the sentence (`<Subject 1> is the young man…`) and the
      // editor shows it separately, so an author who types it again would get it twice.
      const text = t.startsWith(d.label) ? t.slice(d.label.length).trim() : t;
      return `${d.label} ${text}`.trim();
    })
    .join('\n');

/** A `retention_analysis` body. Empty rows are kept for the same reason as above. */
export const h3RetentionBody = (entries: H3RetentionEntry[]): string =>
  entries
    .map((e) => {
      const note = e.role_note?.trim();
      const head = note ? `${e.label} (${note})` : e.label;
      return `${head}: ${e.marker} - ${e.text.trim()}`;
    })
    .join('\n');

/** A `summary` body: the `[task types]` bracket, then the paragraph. */
export const h3SummaryBody = (taskTypes: H3TaskType[], summary: string): string => {
  const prefix = joinTaskTypes(taskTypes);
  return `${prefix ? `[${prefix}] ` : ''}${summary}`.trimEnd();
};
