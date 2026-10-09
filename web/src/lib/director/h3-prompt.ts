/**
 * Assembling a MiniMax-H3 prompt — the single authority on what the model is actually sent.
 *
 * 🔴 **An H3 prompt is a SCHEMA, not prose.** The model was trained on named sections in a fixed
 * order, and bare prose is out-of-distribution conditioning. The part that costs picture quality is
 * subtler than that: prose carries no `overall_soundscape` and no `non_diegetic_music` at all, so
 * on the engine whose headline feature is joint video+audio, the model invents the entire audio bed
 * unguided. That is the single largest capability the Director was leaving on the table.
 *
 * 🔴 **The authority is MiniMax's own two guides**, vendored verbatim at
 * `api/assets/minimax-h3/VIDEO_PROMPT_WRITING_GUIDE_{base,ref}_en.md` and sent to the prompt LLM
 * unedited. `~/handoffs/docker/wan2gp-dev/SPEC-h3-prompt-emitter.md` reconstructed them and is now
 * authoritative only where the guides are SILENT — task-type ORDER, the `\n\n` section separator,
 * `<scenetrans>` placement (§K). Silence is not disagreement.
 *
 * ## The three corrections that bite
 *
 * 1. **The angle brackets are part of the prompt text.** Official prompts write `<Picture 1>`,
 *    `<Subject 1>`, `<Video 1>`, `<Audio 2>` verbatim, and it is the same string the conditioner
 *    emits (`text_encoder.py:137`). The old `h3_prompting.rs:89` claimed the brackets were "the
 *    engine's framing, not the prompt's" and stripped them — which made every label we emitted
 *    unresolvable.
 * 2. **The section set is decided by the CHECKPOINT.** The FL2VA family keys on
 *    `integrated_multimodal_description` with 3 sections; Ref2VA keys on `detailed_description`
 *    with 6. The field name is a literal token sequence the model was trained to see at that
 *    position — using the wrong one is a schema error, not a style choice, and it loses the four
 *    label sections that carry all the reference semantics.
 * 3. **The style opening flips sides.** Ref2VA writes 1–2 sentences of style on their own line
 *    BEFORE `[Shot 1]`; the FL2VA family writes style AFTER the `[Shot 1]` tag, inside the shot.
 *
 * ## 🔴 This file no longer decides what is sent — read `h3-block.ts` first
 *
 * It used to. The header above said *"round-tripping is explicitly not required; the assembled block
 * is the artifact"*, and that one design property is what made the whole H3 surface a one-way
 * emitter with a form bolted onto the front: the LLM was made to fill the editor's JSON shape
 * instead of writing a prompt, a hand-written block was a dead end, and a half-written draft was
 * indistinguishable from an invalid one. Reversed 2026-08-08 —
 * `~/handoffs/koblem/h3-director/PLAN-h3-director-reversal-2026-08-08.md`.
 *
 * **The stored block is the document now** (`DirectorH3Generation.prompt`), and the editor's panels
 * are a lens over it. So this module has exactly two remaining jobs:
 *
 * - `composeH3Prompt` **SEEDS** a block — a fresh shot, or a fallback when the LLM returned fields
 *   rather than text. ⚠️ It must never REWRITE one: a field edit calls `replaceH3Section`, which
 *   rewrites one section's bytes and leaves every other byte alone. That is what lets the parser be
 *   best-effort without an author's text ever being destroyed by an edit made somewhere else.
 * - `validateH3Prompt` reports, at three tiers — `error` (the engine or model refuses it), `warn`
 *   (outside the guide's envelope), `unwritten` (nothing there yet, so a placeholder rather than a
 *   verdict).
 *
 * 🔴 Anything folded into a block by a FEATURE (keyframe declarations, for instance) belongs in
 * `h3SeedInput`, never here — a composer that injected them would inject twice on recompose, and
 * the round-trip test would then break rather than catch it.
 */

import type {
  DirectorH3Reference,
  H3ReferenceKind,
  H3RetentionEntry,
  H3SubjectDefinition,
  H3Task,
  H3TaskType
} from '$lib/api/client';
import { vocabFor } from './engine';

/**
 * What one render, and one piece of one, are CALLED in the messages below — `"shot"` and `"cut"`.
 *
 * ⚠️ Display only, and only in the {@link H3PromptIssue} text. The BLOCK this module composes still
 * writes `[Shot N]` for a cut, because that is the literal tag MiniMax trained on; every message
 * that names the tag keeps writing it that way and calls the thing a cut in the same sentence. See
 * `EngineVocab`.
 */
const UNIT = vocabFor('h3').renderUnit;
const SUB = vocabFor('h3').subUnit ?? 'cut';

/** MiniMax's serving limit. Rejected, never truncated — see {@link validateH3Prompt}. */
export const H3_PROMPT_MAX_CHARS = 7000;

/** `detailed_description` length envelope for a generation task (`GUIDE-ref:242`). */
export const H3_BODY_MIN_WORDS = 350;
export const H3_BODY_MAX_WORDS = 500;

/**
 * Retention markers for VISIBLE content — `<Subject N>`, `<Picture N>`, `<Video N>`.
 * Fixed English values in the output format (`GUIDE-ref:161`), not free text.
 */
export const H3_VISIBLE_MARKERS = [
  'fully_preserved',
  'partially_preserved',
  'attribute_transfer',
  'weak_reference'
] as const;

/** Retention markers for `<Audio N>`. `weak_reference` is the only one both tables share. */
export const H3_AUDIO_MARKERS = [
  'fully_copy',
  'partially_copy',
  'reference',
  'weak_reference'
] as const;

/**
 * The five legal ordinary cut verbs (`GUIDE-base:92`) — the set the picker OFFERS.
 *
 * Cross-dissolve, fade and wipe exist but are user-requested only, so they are deliberately absent
 * from the picker: offering them as peers would have the editor emit them by default. For
 * DETECTING whether existing text is a cut, use {@link H3_TRANSITION_VERBS} instead — a
 * hand-written dissolve is a cut even though we never offer one.
 */
export const H3_CUT_VERBS = [
  'the camera cuts to',
  'the shot cuts to',
  'the shot transitions to',
  'the shot changes to',
  'the shot switches to'
] as const;

/**
 * 🔴 **A PARSER, not an authoring rule.** `[Shot N]` is a CUT — koblem never authors one as a
 * timing device, and timed moments are `At MM:SS.mmm,` clauses inside a shot (see
 * `shot_and_beat_facts` in `api/src/routes/h3_prompting.rs`).
 *
 * ⚠️ This classifier must still read blocks that already exist: every legacy project holds tags
 * written without a transition verb, and the UI has to label them. Verb ⇒ cut is sound; verbless
 * means "authored before the rule changed, and it cuts anyway".
 *
 * ⚠️ **Wider than {@link H3_CUT_VERBS} on purpose.** The guide restricts dissolve/fade/wipe to
 * explicit user request, so the picker must not offer them — but a human or an LLM can write one,
 * and the model reads it as a transition. Detecting against the narrow set misclassified those:
 * the UI would have offered to "keep the camera rolling" on a shot that dissolves.
 */
export const H3_TRANSITION_VERBS = [
  ...H3_CUT_VERBS,
  'the shot cross-dissolves to',
  'the shot dissolves to',
  'the shot fades to',
  'the shot wipes to',
  'cross-dissolve to',
  'dissolve to',
  'fade to',
  'wipe to'
] as const;

/**
 * Camera motion vocabulary — type, then amplitude, then speed, written as natural English inside
 * the shot rather than stacked as labels at the end of a sentence (`GUIDE-base:117`).
 *
 * ⚠️ The served T2VA script breaks that rule in the opening clause of `[Shot 1]`
 * (`"Cinematic, medium wide shot, pushing in slowly."`) and conforms everywhere else. The spec's
 * ruling is: natural English in the body, compact labels allowed only in `[Shot 1]`'s opening
 * style/framing clause, which is where the corpus actually does it.
 */
export const H3_CAMERA_MOTION = [
  'Zoom In',
  'Zoom Out',
  'Push In',
  'Pull Out',
  'Pan Left',
  'Pan Right',
  'Truck Left',
  'Truck Right',
  'Tilt Up',
  'Tilt Down',
  'Pedestal Up',
  'Pedestal Down',
  'Arc Shot',
  'Tracking Shot',
  'Static Shot',
  'Shake Slightly',
  'Shake Strongly',
  'POV',
  'Roll Clockwise',
  'Roll Counterclockwise'
] as const;
export const H3_CAMERA_AMPLITUDE = ['with small amplitude', 'with large amplitude'] as const;
export const H3_CAMERA_SPEED = ['at slow speed', 'at fast speed'] as const;

/** The six task types, in this module's canonical join order — see {@link joinTaskTypes}. */
export const H3_TASK_TYPES: H3TaskType[] = [
  'video editing',
  'video continuation',
  'keyframe completion',
  'reference generation',
  'audio reuse',
  'audio reference'
];

// ── Ordinals ─────────────────────────────────────────────────────────────────

/** One reference's labels, as the prompt must write them. */
export interface H3Ordinal {
  /** `DirectorH3Reference.id`. */
  id: string;
  kind: H3ReferenceKind;
  /** `<Picture 1>` / `<Video 2>` / `<Audio 1>` — brackets included. */
  label: string;
  /**
   * A soundtracked VIDEO's own `<Audio k>`, which is numbered BEFORE its `<Video k>`.
   *
   * Set for `kind: 'video'` references whose request says `with_audio` — see {@link h3Ordinals}
   * for why that field and not the presence of a soundtrack. A reference video does not
   * automatically earn a retention entry for it — `GUIDE-ref:112` — but the label exists and the
   * prompt may address it, which is exactly what the official request does
   * (*"`<Audio 1>` is the synchronized audio track of `<Video 1>`"*).
   */
  audioLabel?: string;
  /** The author's note, for the editor only. The model addresses a reference by ordinal alone. */
  labelText?: string;
}

/**
 * Assign every in-scope reference its ordinals.
 *
 * 🔴 The order is WanGP's pipeline add-order, not the order of the array as displayed, and the two
 * differ whenever a video carries a soundtrack. Images first (`pipeline.py:311-312`), then videos —
 * and `_add_video_reference` appends a soundtracked video's `{"type":"audio"}` entry BEFORE its
 * `{"type":"video"}` entry (`pipeline.py:249-254`), so that video's `<Audio k>` is allocated first —
 * then standalone audio (`pipeline.py:334-337`).
 *
 * The official request confirms all of it: its video carries a soundtrack, so the soundtrack is
 * `<Audio 1>`, the video is `<Video 1>`, and the separate voice clip is `<Audio 2>`.
 *
 * 🔴 **A video's `<Audio k>` is keyed off `with_audio`, not off the reference carrying audio
 * bytes**, and getting that backwards was wrong in both directions:
 *
 * - the ordinal the engine allocates is the one `with_audio: true` asks for. A reference video
 *   does NOT earn an `<Audio N>` just because the file has sound (`GUIDE-ref:112`) — it is a
 *   deliberate request field — so keying off anything else previews an ordinal the conditioner
 *   never allocates and renumbers every later `<Audio n>` in the preview but not in the render;
 * - the shape it used to key off is a REJECTION. koblem refuses a video reference with a separate
 *   soundtrack attached by name (*"The container's own audio is what the engine reads — set
 *   `with_audio` to give it an `<Audio n>` ordinal"*), so the composer was previewing labels for a
 *   request that 400s before it reaches the GPU.
 *
 * Getting this wrong is silent. The labels still look plausible, the render still completes, and
 * the prompt is simply talking about the wrong asset.
 */
export function h3Ordinals(refs: DirectorH3Reference[]): H3Ordinal[] {
  const out = new Map<string, H3Ordinal>();
  let picture = 0;
  let video = 0;
  let audio = 0;
  for (const r of refs) {
    if (r.kind !== 'image') continue;
    out.set(r.id, { id: r.id, kind: 'image', label: `<Picture ${++picture}>`, labelText: r.label });
  }
  for (const r of refs) {
    if (r.kind !== 'video') continue;
    // Audio first, then the video — mirroring the order the rows are packed in.
    const audioLabel = r.with_audio ? `<Audio ${++audio}>` : undefined;
    out.set(r.id, {
      id: r.id,
      kind: 'video',
      label: `<Video ${++video}>`,
      ...(audioLabel ? { audioLabel } : {}),
      labelText: r.label
    });
  }
  for (const r of refs) {
    if (r.kind !== 'audio') continue;
    out.set(r.id, { id: r.id, kind: 'audio', label: `<Audio ${++audio}>`, labelText: r.label });
  }
  // Emitted in the INPUT order, so the panel that displays these rows stays in the order the user
  // arranged — only the numbering follows the pipeline.
  return refs.map((r) => out.get(r.id)!).filter(Boolean);
}

/** Every label a prompt is allowed to name, given the references in scope. */
export function h3DefinedLabels(
  ordinals: H3Ordinal[],
  subjects: H3SubjectDefinition[]
): Set<string> {
  const s = new Set<string>();
  for (const o of ordinals) {
    s.add(o.label);
    if (o.audioLabel) s.add(o.audioLabel);
  }
  for (const d of subjects) if (d.label.trim()) s.add(d.label.trim());
  return s;
}

// ── Assembly ─────────────────────────────────────────────────────────────────

/**
 * Is this `[Shot N]` a CUT, or a beat inside the shot that is already running?
 *
 * The one definition, so twelve components stop re-deriving it — and so that nothing re-derives it
 * from the CLIP, which is where the old cut-verb picker got its lie from: `clip.h3_cut_verb` had
 * exactly one writer (that picker) and a `?? H3_CUT_VERBS[0]` fallback, so every untouched shot
 * displayed "the camera cuts to" while the block held no verb and the render was a beat.
 *
 * 🔴 Read from the BLOCK. The block is the document and the block is what renders; a clip-side copy
 * is a mirror that can be stale, absent, or never written.
 */
export const h3ShotIsCut = (s: Pick<H3ShotBody, 'cutVerb'>): boolean => s.cutVerb !== undefined;

/** One `[Shot N]` body, as the composer receives it. */
export interface H3ShotBody {
  /** 0-based within the generation. Index 0 becomes `[Shot 1]` and is untimed. */
  index: number;
  /** Absolute cut time inside the generation, in seconds. `null` on index 0 only. */
  atSeconds: number | null;
  /** The prose. On a cut shot the composer prefixes `cutVerb` unless the text already opens with it. */
  body: string;
  /**
   * The transition verb this shot opens with, or **absent — which means it is a BEAT**, not that a
   * verb was forgotten. Ignored on index 0, which opens the render and has nothing to cut from.
   *
   * Written from {@link H3_CUT_VERBS} (what the picker offers); detected against
   * {@link H3_TRANSITION_VERBS} (the wider set a human may have typed). See {@link h3ShotIsCut}.
   */
  cutVerb?: string;
  /**
   * A line of dialogue runs across the cut INTO this shot: emit `<scenetrans>` on both sides.
   *
   * ⚠️ `<scenetrans>` has zero in-situ examples in the corpus (`SPEC` §K, row 4). It is described
   * in prose only, so the placement below — immediately before the cut time on the incoming shot,
   * and at the end of the outgoing one — is INFERRED and should be revisited if an example
   * appears. It is plain BPE text either way: nothing parses it.
   */
  scenetrans?: boolean;
}

/** Which alignment instruction line the FL2VA family needs, if any. */
export type H3Alignment = 'none' | 'i2va' | 'fl2va' | 'l2va';

export interface H3ComposeInput {
  task: H3Task;
  /** The generation's rendered duration, in seconds — the SNAPPED value, not the authored one. */
  durationSeconds: number;
  shots: H3ShotBody[];
  /** In-scope references, already ordinal-assigned. Empty on the FL2VA family. */
  ordinals?: H3Ordinal[];
  styleOpening?: string;
  taskTypes?: H3TaskType[];
  subjectDefinitions?: H3SubjectDefinition[];
  summary?: string;
  retentionAnalysis?: H3RetentionEntry[];
  overallSoundscape?: string;
  nonDiegeticMusic?: string;
  /** FL2VA family only. Derived from which frames are pinned; `none` for plain t2va. */
  alignment?: H3Alignment;
}

/**
 * `MM:SS.mmm` — 2-digit minutes, 2-digit seconds, exactly 3 digits of milliseconds, a colon
 * between MM and SS and a **period** before mmm (`GUIDE-base:86`, and every observed value).
 */
export function formatCutTime(seconds: number): string {
  const total = Math.max(0, Math.round(seconds * 1000));
  const ms = total % 1000;
  const allSec = Math.floor(total / 1000);
  const ss = allSec % 60;
  const mm = Math.floor(allSec / 60);
  const p2 = (n: number) => String(n).padStart(2, '0');
  return `${p2(mm)}:${p2(ss)}.${String(ms).padStart(3, '0')}`;
}

/** The effective duration as the alignment line writes it — exactly two decimals. */
const formatDuration = (seconds: number): string => seconds.toFixed(2);

/**
 * Join the task types into the `summary:` bracket prefix.
 *
 * ⚠️ Order is OUR choice: no rule is stated anywhere and the corpus is not self-consistent
 * (`GUIDE-ref:127` puts `audio reuse` last; `REQ-ref2va:13` puts `audio reference` before
 * `audio reuse`). {@link H3_TASK_TYPES} is visual-structural first, then audio, most-specific
 * first. Duplicates are dropped — the guide forbids repeating a type.
 */
export function joinTaskTypes(types: H3TaskType[]): string {
  const seen = new Set(types);
  return H3_TASK_TYPES.filter((t) => seen.has(t)).join(' + ');
}

/**
 * The alignment instruction line, verbatim per variant.
 *
 * ⚠️ The brackets are inconsistent in the guide and this is deliberate rather than a bug here: I2VA
 * and L2VA are written `<Picture 1>` / `[Shot N]`, while the two-keyframe FL2VA line is written
 * unbracketed in BOTH its template and its worked example, and no script on disk exercises it
 * (`SPEC` §A.4.1, §K row 2). The ruling is to emit each variant exactly as its own guide template
 * writes it, because the guide is self-consistent per variant and that is more likely to match
 * what the IR generator does than a normalisation we invent.
 *
 * `lastShotNumber` is the 1-based index of the FINAL shot in the body — for a single-shot
 * generation that is 1, which is what the guide's own 8-second example writes.
 */
export function alignmentLine(
  kind: H3Alignment,
  durationSeconds: number,
  lastShotNumber: number
): string | null {
  const s = formatDuration(durationSeconds);
  switch (kind) {
    case 'i2va':
      // Character-for-character, no substitutions at all — the `0.00` is literal.
      return 'For the target video, at 0.00 seconds into the target video, <Picture 1> (from [Shot 1]) is fully referenced.';
    case 'fl2va':
      return `How the reference pictures align with the target video — Picture 1 (from Shot 1) aligns with the 0.00-second mark of the target video; Picture 2 (from Shot ${lastShotNumber}) aligns with the ${s}-second mark of the target video.`;
    case 'l2va':
      return `How the reference pictures align with the target video — <Picture 1> (from [Shot ${lastShotNumber}]) aligns with the ${s}-second mark of the target video.`;
    default:
      return null;
  }
}

/**
 * One `[Shot N] …` line/clause, shared by both schemas.
 *
 * `closesWithTrans` is the OUTGOING half of a `<scenetrans>` pair: the marker goes at **both**
 * connecting points, so the shot a line of dialogue runs out of has to carry one too. Deciding it
 * here rather than on the shot itself keeps the two halves from ever being written independently
 * and drifting apart.
 */
function shotText(s: H3ShotBody, isFirst: boolean, closesWithTrans = false): string {
  const n = s.index + 1;
  const body = s.body.trim();
  const tail = closesWithTrans ? ' <scenetrans>' : '';
  if (isFirst) return `[Shot ${n}] ${body}${tail}`.trimEnd();
  const verb = s.cutVerb?.trim();
  // The verb is prefixed only when the author has not already written one, so a hand-edited body
  // that opens "the shot cuts to a close-up" does not become "the shot cuts to the shot cuts to".
  const lead =
    verb && !body.toLowerCase().startsWith(verb.toLowerCase()) ? `${verb} ${body}` : body;
  const trans = s.scenetrans ? '<scenetrans> ' : '';
  return `[Shot ${n}] At ${formatCutTime(s.atSeconds ?? 0)}, ${trans}${lead}${tail}`.trimEnd();
}

/**
 * Ref2VA's `detailed_description` body: the style sentence on its own line, then one line per shot.
 *
 * Assembled rather than authored, because the two halves have different owners — style is a
 * property of the generation and the shot bodies are properties of the clips.
 */
function ref2vaBody(input: H3ComposeInput): string {
  const lines: string[] = [];
  const style = input.styleOpening?.trim();
  if (style) lines.push(style);
  input.shots.forEach((s, i) =>
    lines.push(shotText(s, s.index === 0, !!input.shots[i + 1]?.scenetrans))
  );
  return lines.join('\n');
}

/**
 * The FL2VA family's `integrated_multimodal_description` body.
 *
 * Two divergences from Ref2VA, both verified against the literal scripts: the style statement goes
 * INSIDE `[Shot 1]` right after the tag, and the shots run INLINE separated by a space rather than
 * each on its own line (`REQ-t2va:13`, `GUIDE-base:175`).
 */
function fl2vaBody(input: H3ComposeInput): string {
  const style = input.styleOpening?.trim();
  return input.shots
    .map((s, i) => {
      const closes = !!input.shots[i + 1]?.scenetrans;
      if (i !== 0 || !style) return shotText(s, s.index === 0, closes);
      const withStyle: H3ShotBody = {
        ...s,
        body: `${style.replace(/[.\s]+$/, '')}. ${s.body.trim()}`.trim()
      };
      return shotText(withStyle, true, closes);
    })
    .join(' ');
}

const retentionLine = (e: H3RetentionEntry): string => {
  const note = e.role_note?.trim();
  const head = note ? `${e.label} (${note})` : e.label;
  return `${head}: ${e.marker} - ${e.text.trim()}`;
};

/**
 * Compose the final schema block. **This is what gets sent.**
 *
 * Section separator is `\n\n` throughout. ⚠️ The two FL2VA-family scripts disagree here — `REQ-t2va`
 * uses a single `\n` between sections while `REQ-fl2va` uses `\n\n` — and both were served
 * successfully, so the model tolerates either. The spec rules `\n\n`: it is what the guide
 * specifies, what the harder case (the one with an alignment line) does, and what Ref2VA does.
 * ⚠️ Whether the difference measurably changes output is UNVERIFIED (`SPEC` §K, row 1). It is not a
 * tuning knob.
 *
 * The header FORM differs by schema and is not cosmetic: the FL2VA family writes `name: ` inline
 * with the content on the same line, Ref2VA writes `name:` then a newline. Both are unambiguous in
 * their literal scripts. No trailing newline — none of the three literal prompts has one.
 */
export function composeH3Prompt(input: H3ComposeInput): string {
  const sections: string[] = [];
  if (input.task === 'ref2va') {
    const defs = (input.subjectDefinitions ?? []).filter((d) => d.text.trim());
    const ret = (input.retentionAnalysis ?? []).filter((e) => e.text.trim());
    const prefix = joinTaskTypes(input.taskTypes ?? []);
    const summary = input.summary?.trim() ?? '';
    sections.push(
      `subject_definitions:\n${defs.map((d) => `${d.label} ${stripLeadingLabel(d.text, d.label)}`.trim()).join('\n')}`,
      `summary:\n${prefix ? `[${prefix}] ` : ''}${summary}`.trimEnd(),
      `retention_analysis:\n${ret.map(retentionLine).join('\n')}`,
      `detailed_description:\n${ref2vaBody(input)}`,
      `overall_soundscape:\n${(input.overallSoundscape ?? '').trim()}`,
      `non_diegetic_music:\n${(input.nonDiegeticMusic ?? '').trim()}`
    );
  } else {
    const line = alignmentLine(
      input.alignment ?? 'none',
      input.durationSeconds,
      input.shots.length || 1
    );
    if (line) sections.push(line);
    sections.push(
      `integrated_multimodal_description: ${fl2vaBody(input)}`,
      `overall_soundscape: ${(input.overallSoundscape ?? '').trim()}`,
      `non_diegetic_music: ${(input.nonDiegeticMusic ?? '').trim()}`
    );
  }
  return sections.join('\n\n');
}

/**
 * A definition's text already opens with its own label in every corpus example
 * (`<Subject 1> is the young man…`), and the editor shows the label separately — so an author who
 * types it again would get it twice. Strip one leading copy, keep everything else verbatim.
 */
function stripLeadingLabel(text: string, label: string): string {
  const t = text.trim();
  return t.startsWith(label) ? t.slice(label.length).trim() : t;
}

// ── Validation ───────────────────────────────────────────────────────────────

/**
 * Three tiers, and the split is the whole point of this module's 2026-08-08 rewrite.
 *
 * 🔴 **Until then there were two, and "not yet written" was filed under `error`.** Six hard errors
 * fired at any ref2va generation that was not fully filled in — which is the normal state of a
 * fresh shot AND of every LLM draft, since the model writes the prose sections first. The editor
 * therefore opened red, every time, on documents that were merely young. Red that is always on is
 * red nobody reads, and it buried the errors that meant something: an undefined `<Picture 4>`, a
 * cut time past the end.
 *
 * - `error` — **the engine, koblem or the model actually refuses it.** Red, and it blocks the
 *   render. Every one of these is about text that IS there and is wrong, never about text that is
 *   missing, which is what makes it safe to show the moment it appears rather than gating it.
 * - `warn` — **outside the guide's envelope; it renders anyway.** Amber, inline. Gated on the
 *   author having touched the field (or hit render), because "48 words, the guide asks for 350" is
 *   true and useless about a paragraph nobody has started.
 * - `unwritten` — **the section simply has no content yet.** A ghost placeholder INSIDE the
 *   section, never red, never blocking. ⚠️ The message still carries the full mechanical reason —
 *   an empty `overall_soundscape` genuinely does mean the model invents the whole audio bed
 *   unguided — because that fact is the only thing that gets the section written. What changed is
 *   the voice, not the content: it states what the field is for instead of accusing the author of
 *   having omitted it.
 */
export type H3IssueLevel = 'error' | 'warn' | 'unwritten';

export interface H3PromptIssue {
  level: H3IssueLevel;
  /** The section-per-panel field this belongs under, so the editor can put it next to the input. */
  field: string;
  message: string;
}

/**
 * Does this set of issues stop a render?
 *
 * 🔴 Only `error` does. `unwritten` must never block: a t2va generation with nothing but a shot
 * body is a legal render and always was, and a gate that waited for six sections would make the
 * fast path unreachable. `warn` must never block by definition.
 */
export function h3BlocksRender(issues: readonly H3PromptIssue[]): boolean {
  return issues.some((i) => i.level === 'error');
}

const words = (s: string): number => s.trim().split(/\s+/).filter(Boolean).length;

/**
 * Everything that makes a composed block wrong — or not written yet — checked in one place.
 *
 * Ordered roughly by how expensive the mistake is: a section-set gap means the model never sees the
 * reference semantics at all, a cut-time error means the picture cuts somewhere the audio does not,
 * a length warning is just prose that drifted out of the envelope. Nothing here truncates or
 * repairs — 🔴 truncating to fit the 7000-character cap would silently drop `non_diegetic_music`,
 * the LAST section, and the render would come back with an invented score and no error attached.
 *
 * See {@link H3IssueLevel} for what each tier means and why the split exists. The one thing to hold
 * on to when adding a check: **ask whether the text is WRONG or whether it is ABSENT.** Wrong is an
 * error, absent is `unwritten`, and the difference is what stopped this function shouting at every
 * new shot.
 */
export function validateH3Prompt(input: H3ComposeInput, composed?: string): H3PromptIssue[] {
  const out: H3PromptIssue[] = [];
  const push = (level: H3IssueLevel, field: string, message: string) =>
    out.push({ level, field, message });
  const block = composed ?? composeH3Prompt(input);
  const isRef = input.task === 'ref2va';

  // ── Section set: absent, not wrong ─────────────────────────────────────────
  //
  // 🔴 Every check in this block is `unwritten`, and every message is phrased as what the section
  // DOES rather than as a complaint that it is missing. The mechanics are unchanged and deliberately
  // so — the reason an empty section costs something is the only argument for filling it in, and
  // dropping it to make the UI calmer would have made the UI calm and useless.
  if (isRef) {
    if (!(input.subjectDefinitions ?? []).some((d) => d.text.trim())) {
      push(
        'unwritten',
        'subject_definitions',
        'No definitions yet. Every label the prompt names — <Subject 1>, <Picture 1> — has to be defined here before it is used, or it resolves against nothing and the sentence that names it is prose about nobody.'
      );
    }
    if (!input.summary?.trim()) {
      push(
        'unwritten',
        'summary',
        `Not written yet. The summary is where the whole ${UNIT} is stated in one paragraph, and it is one of the six sections Ref2VA was trained to see.`
      );
    }
    if (!(input.taskTypes ?? []).length) {
      push(
        'unwritten',
        'summary',
        'No task type picked. It is the bracket the summary opens with, and it is how the model is told what relationship the references have to the target — without it the references are present but unexplained.'
      );
    }
    if (!(input.retentionAnalysis ?? []).some((e) => e.text.trim())) {
      push(
        'unwritten',
        'retention_analysis',
        'Nothing written yet. A retention line per referenced label is what says how much of each reference survives into the target; with none, the model decides for itself how much of a face or a track to keep.'
      );
    }
    if (!(input.ordinals ?? []).some((o) => o.kind === 'image' || o.kind === 'video')) {
      push(
        'error',
        'references',
        `A ref2va ${UNIT} needs at least one picture or clip in scope. Audio references supplement visual conditioning and cannot drive a render alone.`
      );
    }
  } else if (input.ordinals?.length) {
    push(
      'error',
      'references',
      `This ${UNIT} pins a frame, so it renders on the fl2va checkpoint, which takes no omni references. Clear the pin or take the references out of scope — they are different weight sets, not a flag.`
    );
  }

  // ── The main body ──────────────────────────────────────────────────────────
  const bodyField = isRef ? 'detailed_description' : 'integrated_multimodal_description';
  const bodyText = isRef ? ref2vaBody(input) : fl2vaBody(input);
  if (!input.shots.length || !input.shots.some((s) => s.body.trim())) {
    // 🔴 `error`, and the one place this function departs from the "absent ⇒ unwritten" rule — with
    // a reason. Every other empty section leaves a legal prompt that renders something; a block
    // with no shot body at all is an EMPTY prompt, and koblem drops an empty-prompt generation out
    // of the chain entirely (`compileH3Timeline`). So it does not render badly, it does not fail —
    // this generation silently disappears from the cut and the seam closes over it. That is a
    // refusal wearing a disappearance's clothes, and it has to be red.
    push(
      'error',
      bodyField,
      `No ${SUB} has any text. This is not an unfinished prompt, it is an empty one — koblem drops an empty-prompt ${UNIT} from the chain, so this ${UNIT} would vanish from the render rather than fail.`
    );
  } else if (isRef) {
    // The 350–500 envelope is stated for `detailed_description` only (`GUIDE-ref:242`). The
    // FL2VA family has no published envelope at all, so warning about one there would be inventing
    // a rule (`SPEC` §K, row 9).
    const n = words(bodyText);
    if (n < H3_BODY_MIN_WORDS) {
      push(
        'warn',
        bodyField,
        `${n} words. The guide asks for ${H3_BODY_MIN_WORDS}–${H3_BODY_MAX_WORDS} for a generation task — a thin description is where the model starts inventing.`
      );
    } else if (n > H3_BODY_MAX_WORDS) {
      push(
        'warn',
        bodyField,
        `${n} words, past the ${H3_BODY_MAX_WORDS}-word envelope. Long prompts also shift the media rotary origin, so trimming keeps A/Bs comparable.`
      );
    }
  }

  // ── Shot tags and cut times ────────────────────────────────────────────────
  input.shots.forEach((s, i) => {
    if (s.index !== i) {
      push(
        'error',
        'shots',
        `Cut numbers must run 1, 2, 3… with no gaps — ${SUB} ${i + 1} is written as [Shot ${s.index + 1}].`
      );
    }
  });
  if (input.shots[0]?.atSeconds != null) {
    push(
      'error',
      'shots',
      `[Shot 1] carries no timestamp. The first ${SUB} of a ${UNIT} starts at the start.`
    );
  }
  let prev = -1;
  for (const s of input.shots.slice(1)) {
    if (s.atSeconds == null) {
      push(
        'error',
        'shots',
        `[Shot ${s.index + 1}] has no cut time. Every ${SUB} after the first needs one.`
      );
      continue;
    }
    if (s.atSeconds <= prev) {
      push(
        'error',
        'shots',
        `Cut times must strictly increase — [Shot ${s.index + 1}] cuts at ${formatCutTime(s.atSeconds)}, at or before the ${SUB} before it.`
      );
    }
    if (s.atSeconds >= input.durationSeconds) {
      push(
        'error',
        'shots',
        `[Shot ${s.index + 1}] cuts at ${formatCutTime(s.atSeconds)}, at or past this ${UNIT}'s ${input.durationSeconds.toFixed(2)}s — it would have no time to play.`
      );
    }
    prev = s.atSeconds;
  }

  // ── Audio: the reason the schema is worth the trouble ──────────────────────
  //
  // 🔴 `unwritten`, not `error` — but the SENTENCE is unchanged in substance, because it is the
  // only thing that gets these two written. The render succeeds either way; what an empty
  // soundscape costs is not a refusal, it is a soundtrack nobody chose, on the one engine whose
  // headline feature is that it makes the audio at the same time as the picture.
  if (!input.overallSoundscape?.trim()) {
    push(
      'unwritten',
      'overall_soundscape',
      'Not written yet — and empty does not mean silent. H3 generates the soundtrack in the same pass as the picture, so with nothing here the model invents the whole bed unguided. Write it, or put N/A for deliberate silence.'
    );
  }
  if (!input.nonDiegeticMusic?.trim()) {
    push(
      'unwritten',
      'non_diegetic_music',
      'Not written yet. Describe the score — instrumentation, tempo, rhythm, dynamics — or write N/A if there is none. Left blank it is invented along with the rest of the bed, not omitted.'
    );
  }

  // ── Labels ─────────────────────────────────────────────────────────────────
  if (isRef) {
    const defined = h3DefinedLabels(input.ordinals ?? [], input.subjectDefinitions ?? []);
    const used = new Set(block.match(/<(?:Subject|Picture|Video|Audio) \d+>/g) ?? []);
    for (const label of used) {
      if (!defined.has(label)) {
        push(
          'error',
          'subject_definitions',
          `${label} is named in the prompt but never defined. The model resolves a label only against what the conditioner actually packed — an undefined one is prose about nothing.`
        );
      }
    }
    for (const e of input.retentionAnalysis ?? []) {
      const audio = e.label.startsWith('<Audio');
      const table: readonly string[] = audio ? H3_AUDIO_MARKERS : H3_VISIBLE_MARKERS;
      if (e.text.trim() && !table.includes(e.marker)) {
        push(
          'error',
          'retention_analysis',
          `${e.label} has marker "${e.marker}", which is not in the ${audio ? 'audio' : 'visible-content'} vocabulary. The two are disjoint and the markers are fixed English values, not descriptions.`
        );
      }
    }
    if ((input.retentionAnalysis ?? []).some((e) => /\(S\d/.test(e.text))) {
      push(
        'warn',
        'retention_analysis',
        'Speaker IDs do not belong in retention_analysis (GUIDE-ref:278).'
      );
    }
  }

  // ── Hard limits ────────────────────────────────────────────────────────────
  if (block.length > H3_PROMPT_MAX_CHARS) {
    push(
      'error',
      'prompt',
      `${block.length} characters, past MiniMax's ${H3_PROMPT_MAX_CHARS} serving limit. This is refused rather than trimmed: truncating would silently drop non_diegetic_music, the last section, and the render would come back with an invented score.`
    );
  }
  return out;
}
