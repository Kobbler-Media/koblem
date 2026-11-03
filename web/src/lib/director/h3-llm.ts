/**
 * Reading what the prompt LLM sends back for a MiniMax-H3 generation.
 *
 * koblem's `h3_prompting.rs` now hands the model MiniMax's own prompt-writing guide and asks for
 * **the finished block** — the six (or three) sections, the `[Shot N]` tags, the cut times, written
 * out. It stopped asking the model to fill in our editor's JSON shape, which is what
 * `PROMPT-CONTRACT.md` §2 was and why the writer kept producing schema-shaped non-prompts.
 *
 * 🔴 **Parse defensively, and never drop anything. FOUR carriers are live at once during a
 * rollout**, and {@link h3DraftFrom} tries them in this order:
 *
 * | # | carrier | who produces it |
 * | --- | --- | --- |
 * | 1 | the typed `h3_draft` / `h3_generations` field | a koblem still on the JSON contract |
 * | 2 | the same object as JSON text inside `prompt` / `segments` | an older koblem still |
 * | 3 | **a raw assembled BLOCK** | the current writer, and any hand-edit or chat turn |
 * | 4 | prose — `null` from here, so the caller drops it on the shot body | anything else |
 *
 * The four are mutually exclusive BY SHAPE, so the order is a cost ordering and not a tie-break:
 * {@link parseH3Reply} refuses anything that does not open with `{` or `[`, and `looksLikeH3Block`
 * demands one of the two main-field headers, which no JSON reply carries at top level.
 *
 * ⚠️ **None of the fallbacks is dead code, and none becomes dead when the block lands.** A saved
 * project, a koblem one deploy behind, and a model that answers in the retired shape all still put
 * something in carriers 1 and 2 — and the difference between keeping them and dropping them is the
 * difference between a stale koblem degrading and a stale koblem losing the author's work.
 *
 * ## Where prose goes, and why not `detailed_description`
 *
 * The document has no standalone `detailed_description` / `integrated_multimodal_description`
 * field: that section is the style opening plus one `[Shot N]` line per clip, so there is nothing to
 * drop a paragraph into. Prose therefore lands on `clip.prompt`, the shot body, which is both where
 * it used to live and where the composer picks it up. Nothing is lost and nothing is silently
 * re-labelled as a section it is not.
 */

import type {
  DirectorH3Generation,
  H3DraftShot,
  H3PromptDraft,
  H3RetentionEntry,
  H3SubjectDefinition,
  H3TaskType
} from '#lib/api/client.ts';
import { looksLikeH3Block, parseH3Block } from './h3-block';
import { H3_AUDIO_MARKERS, H3_TASK_TYPES, H3_VISIBLE_MARKERS } from './h3-prompt';

/**
 * ⚠️ The display vocabulary is no longer read here, and its absence is a decision.
 *
 * This module used to say "the writer returned 3 of this shot's 5 beats", which needed `vocabFor`.
 * Those warnings are gone with the positional mapping ({@link applyH3Reply}): under the
 * prompt-level model a reply with three `[Shot N]` lines is not an under-supplied reply, it is a
 * prompt with three beats. Nothing left in this file addresses the author in units.
 */

/** One `[Shot N]` body as the model returns it. */
export type H3LlmShot = H3DraftShot;

/**
 * The object `h3_prompting.rs` emits per generation.
 *
 * Aliased to the wire type rather than re-declared: two structural copies of the same contract is
 * how the parser and the typed field drift apart, and the drift would be silent — a field this
 * module forgot would simply never reach the composer.
 */
export type H3LlmReply = H3PromptDraft;

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === 'object' && v !== null && !Array.isArray(v);
const str = (v: unknown): string | undefined =>
  typeof v === 'string' && v.trim() ? v.trim() : undefined;

/**
 * Every section key the object can carry. Used as the "is this the structured shape" test, so that
 * a reply carrying only a soundscape — perfectly legal, every field but `shots` is optional — is
 * still recognised rather than falling through to the prose path and being pasted into a shot.
 */
const SECTION_KEYS = [
  'task_types',
  'style_opening',
  'subject_definitions',
  'summary',
  'retention_analysis',
  'shots',
  'overall_soundscape',
  'non_diegetic_music'
];

/**
 * Parse a reply into the structured object, or `null` if it is not one.
 *
 * Deliberately tolerant about everything except the SHAPE. Unknown keys are ignored (the contract
 * is expected to grow), a marker outside its vocabulary is kept as-is so the composer's validator
 * can report it next to the field rather than this function swallowing it, and a `shots` array of
 * bare strings is accepted because that is what a half-migrated backend emits.
 */
export function parseH3Reply(text: string | undefined | null): H3LlmReply | null {
  const raw = text?.trim();
  if (!raw || (raw[0] !== '{' && raw[0] !== '[')) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!isRecord(parsed)) return null;
  if (!SECTION_KEYS.some((k) => k in parsed)) return null;

  const shots: H3LlmShot[] = [];
  const rawShots = parsed.shots;
  if (Array.isArray(rawShots)) {
    rawShots.forEach((s, i) => {
      if (typeof s === 'string') {
        // A half-migrated backend sends the bodies as bare strings. Positional, so the index is
        // the array position and the first is untimed — the composer re-times them from the pack
        // anyway, so nothing is lost by not being told.
        shots.push({ index: i, at_seconds: null, body: s });
        return;
      }
      if (!isRecord(s)) return;
      const body = str(s.body);
      if (!body) return;
      shots.push({
        index: typeof s.index === 'number' ? s.index : i,
        at_seconds: typeof s.at_seconds === 'number' ? s.at_seconds : null,
        body
      });
    });
  }

  const defs: H3SubjectDefinition[] = [];
  if (Array.isArray(parsed.subject_definitions)) {
    for (const d of parsed.subject_definitions) {
      if (!isRecord(d)) continue;
      const label = str(d.label);
      const t = str(d.text);
      if (label && t) defs.push({ label, text: t });
    }
  }

  const retention: H3RetentionEntry[] = [];
  if (Array.isArray(parsed.retention_analysis)) {
    for (const e of parsed.retention_analysis) {
      if (!isRecord(e)) continue;
      const label = str(e.label);
      const t = str(e.text);
      const marker = str(e.marker);
      if (!label || !t || !marker) continue;
      retention.push({
        label,
        marker,
        text: t,
        ...(str(e.role_note) ? { role_note: str(e.role_note)! } : {})
      });
    }
  }

  const taskTypes = Array.isArray(parsed.task_types)
    ? (parsed.task_types.filter((t) => H3_TASK_TYPES.includes(t as H3TaskType)) as H3TaskType[])
    : undefined;

  return {
    shots,
    ...(taskTypes?.length ? { task_types: taskTypes } : {}),
    ...(str(parsed.style_opening) ? { style_opening: str(parsed.style_opening)! } : {}),
    ...(defs.length ? { subject_definitions: defs } : {}),
    ...(str(parsed.summary) ? { summary: str(parsed.summary)! } : {}),
    ...(retention.length ? { retention_analysis: retention } : {}),
    ...(str(parsed.overall_soundscape)
      ? { overall_soundscape: str(parsed.overall_soundscape)! }
      : {}),
    ...(str(parsed.non_diegetic_music)
      ? { non_diegetic_music: str(parsed.non_diegetic_music)! }
      : {})
  };
}

/**
 * Read an assembled BLOCK into the draft shape — **carrier 3**, and the one the current writer
 * produces.
 *
 * The parse itself is {@link parseH3Block}'s, which is the exact inverse of `h3-prompt.ts`'s
 * composer and is round-trip tested against it. This function only re-keys the result into the
 * `H3PromptDraft` shape the rest of this module and {@link applyH3Reply} already speak, so the block
 * arrives through the same door as every other carrier and nothing downstream has to branch.
 *
 * `null` when the text is not a block, which is what routes prose to the shot body. The test is
 * `looksLikeH3Block` — one of the two main-field headers must be present — because the section set
 * is what decides the schema, and text with neither header is not an H3 prompt no matter what else
 * it contains.
 *
 * ⚠️ **What this shape cannot carry, and why that is not a loss here.** `H3PromptDraft` has no place
 * for a shot's `cutVerb` or its `scenetrans` flag, and none for the block's `unparsed` tail. That is
 * survivable on THIS path because the caller keeps the block text as well: the verb stays inside the
 * body (the composer only prefixes one the body does not already open with), and the tail belongs to
 * whoever stores the block, not to a draft being folded into fields. 🔴 If you are tempted to make
 * this the only reader of a block, read `h3-block.ts`'s note on `replaceH3Section` first — a
 * round-trip through these fields is lossy in exactly the way a section-surgical edit is not.
 */
export function h3DraftFromBlock(text: string | undefined | null): H3LlmReply | null {
  const raw = text?.trim();
  if (!raw || !looksLikeH3Block(raw)) return null;
  const b = parseH3Block(raw);
  // `looksLikeH3Block` and a non-null `task` are the same test read two ways; belt and braces,
  // because a null task means the parser found no main field and every section below would be empty.
  if (!b.task) return null;
  return {
    shots: b.shots.map((s) => ({ index: s.index, at_seconds: s.atSeconds, body: s.body })),
    ...(b.taskTypes.length ? { task_types: b.taskTypes } : {}),
    ...(b.styleOpening ? { style_opening: b.styleOpening } : {}),
    ...(b.subjectDefinitions.length ? { subject_definitions: b.subjectDefinitions } : {}),
    ...(b.summary ? { summary: b.summary } : {}),
    ...(b.retentionAnalysis.length ? { retention_analysis: b.retentionAnalysis } : {}),
    ...(b.overallSoundscape ? { overall_soundscape: b.overallSoundscape } : {}),
    ...(b.nonDiegeticMusic ? { non_diegetic_music: b.nonDiegeticMusic } : {})
  };
}

/**
 * The draft for a generation, from whichever carrier this koblem uses.
 *
 * Typed field, then JSON-in-a-string, then the assembled block, then `null` — which routes the text
 * to the shot body as prose. See this module's header for the carrier table and why all four are
 * live at once; the short version is that the order is cheapest-test-first, not a tie-break, because
 * the four shapes cannot be confused for one another.
 *
 * 🔴 The block carrier is tried LAST among the three that succeed, and that is deliberate rather
 * than incidental: a koblem that still receives the typed field must keep using it, or a rollout
 * would silently downgrade every draft to a lossier path while both endpoints were healthy.
 */
export function h3DraftFrom(
  typed: H3PromptDraft | undefined,
  text: string | undefined | null
): H3LlmReply | null {
  if (typed && Array.isArray(typed.shots)) return typed;
  return parseH3Reply(text) ?? h3DraftFromBlock(text);
}

/** The section headers and the alignment line an ASSEMBLED block opens with. */
const BLOCK_HEADS =
  /^(subject_definitions:|integrated_multimodal_description:|detailed_description:|summary:|For the target video, at 0\.00 seconds|How the reference pictures align)/;

/**
 * Does this text look like a whole ASSEMBLED prompt block rather than one shot's body?
 *
 * ⚠️ **The question is narrower than it used to be.** It was written when a block appearing anywhere
 * was a defect, because the form was the only legal producer of one; now a block is the normal
 * artifact and {@link h3DraftFromBlock} reads it. What remains true — and what this still detects —
 * is a whole block sitting in the field that holds ONE shot's body, where the composer will
 * faithfully nest it inside `[Shot N]` and produce a prompt with `summary:` in the middle of a shot
 * description, invisible from the shot list.
 *
 * It does not repair that. It makes it visible, and the composer decides what to show. Silently
 * re-splitting a block that landed in a shot body would mean guessing which section each paragraph
 * belongs to; `parseH3Block` can only be trusted on text that is a block in its own right.
 *
 * `h3-block.ts::looksLikeH3Block` answers the other question — "is this text a block at all" — and
 * is stricter: it requires a main-field header, where this also fires on an alignment line or a
 * trailing audio section.
 */
export function isAssembledH3Block(text: string | undefined | null): boolean {
  const t = text?.trim();
  if (!t) return false;
  return BLOCK_HEADS.test(t) || /\n\n(overall_soundscape:|non_diegetic_music:)/.test(t);
}

/** What a reply changes. Applied by the caller, which owns the editor state. */
export interface H3ReplyPatch {
  /**
   * The generation's authored sections. Only the fields the reply actually carried.
   *
   * ⚠️ A SEED RECORD, not the render. `seedFields` is their only reader and it is consulted only
   * while `prompt` is absent — see {@link applyH3Reply}.
   */
  generation: Partial<DirectorH3Generation>;
  /**
   * Anything the reply carried that this build could not use, for the warning bar.
   *
   * 🔴 There is no `shots` field any more. It carried a positional clip mapping onto a store that
   * no longer exists — see {@link applyH3Reply} for all three reasons.
   */
  warnings: string[];
}

/**
 * Read a reply into the generation's authored sections.
 *
 * ## 🔴 What this used to do, and why it stopped
 *
 * It also mapped `reply.shots` POSITIONALLY onto a `clipIds` array and returned per-clip body
 * writes, plus a `focusIndex` anchor to stop an under-supplied reply landing its one body on
 * `[Shot 1]`, plus two warnings about the count not matching. All of that is retired, for three
 * separate reasons that happen to point the same way:
 *
 * 1. **Its writes were superseded moments later.** `absorbH3Reply` finishes by calling
 *    `setGenerationPrompt(block)`, and the block is what renders. The positional writes here landed
 *    on clip bodies the compiler had already stopped reading.
 * 2. **It was a DUPLICATE of a mapping the block already carries, not the source of it.** The
 *    `[Shot N]` number IS the position. Retiring this does not make the editor less positional; it
 *    makes the block the one place the position is recorded. ⚠️ Worth being precise about, because
 *    "we removed the positional mapping" would be false.
 * 3. **The unit changed.** The LLM edits a SHOT, and a shot is one prompt. "The writer returned 3 of
 *    this shot's 5 beats" is a warning about a reply shape that only exists if the reply is
 *    structured per beat; a block that writes three `[Shot N]` lines is not an under-supplied reply,
 *    it is a prompt with three beats, and the timeline catches up.
 *
 * ## What it still does, and why that is not dead either
 *
 * The section fields (`summary`, `subject_definitions`, …) still come back, and the caller still
 * stores them — but they are a SEED RECORD. `seedFields` in `h3-document.ts` is their only reader
 * and it stops being consulted the moment a `prompt` exists, which on the draft path is immediately
 * afterwards. They are kept because a project saved by an older koblem carries nothing else, and
 * dropping them would make such a generation open empty.
 *
 * The marker warning survives on its own merit: it is about CONTENT the composer will show as an
 * error beside the field, not about a mapping.
 */
export function applyH3Reply(reply: H3LlmReply): H3ReplyPatch {
  const warnings: string[] = [];
  const generation: Partial<DirectorH3Generation> = {};
  if (reply.task_types?.length) generation.task_types = reply.task_types;
  if (reply.style_opening) generation.style_opening = reply.style_opening;
  if (reply.subject_definitions?.length) generation.subject_definitions = reply.subject_definitions;
  if (reply.summary) generation.summary = reply.summary;
  if (reply.retention_analysis?.length) {
    generation.retention_analysis = reply.retention_analysis;
    for (const e of reply.retention_analysis) {
      const table: readonly string[] = e.label.startsWith('<Audio')
        ? H3_AUDIO_MARKERS
        : H3_VISIBLE_MARKERS;
      if (!table.includes(e.marker)) {
        // Kept, not dropped: the composer's validator shows it beside the field, where it can be
        // fixed. A marker silently replaced with a legal one would be a claim nobody made.
        warnings.push(
          `The writer used "${e.marker}" for ${e.label}, which is not in that label's marker vocabulary — fix it in Retention analysis.`
        );
      }
    }
  }
  if (reply.overall_soundscape) generation.overall_soundscape = reply.overall_soundscape;
  if (reply.non_diegetic_music) generation.non_diegetic_music = reply.non_diegetic_music;
  return { generation, warnings };
}
