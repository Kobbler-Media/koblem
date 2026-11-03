/**
 * Reading a MiniMax-H3 prompt block back into fields — the inverse of `h3-prompt.ts`.
 *
 * 🔴 **This file reverses the direction the H3 Director was built in.** `h3-prompt.ts:32` recorded
 * the old design as a property: *"Round-tripping is explicitly not required. The assembled block is
 * the artifact."* That made the composer's form the only legal way to produce a prompt, so every
 * other producer — the prompt LLM, the chat surface, an author who simply wants to type — had to be
 * squeezed through it or rejected. The LLM was made to fill our editor's JSON shape rather than
 * write a prompt; a hand-written block became a dead-end escape hatch that dimmed every panel; a
 * chat edit that produced a real block was flagged as a defect.
 *
 * The block is now the document and the panels are a lens on it. Both directions are live.
 *
 * ## The round-trip guarantee, stated narrowly
 *
 * **Byte-exact for any block `composeH3Prompt` produced.** That is what the tests pin:
 * `compose(parse(compose(x))) === compose(x)`.
 *
 * For anything else — a hand-written block, an LLM's, one carrying a section we have never heard of
 * — the parser is BEST EFFORT and everything it could not place lands in {@link H3ParsedBlock.unparsed}
 * verbatim, with the offset it was found at, so the editor can show it rather than swallow it.
 *
 * ## 🔴 Why that weaker guarantee is still safe: `replaceH3Section`
 *
 * This is the load-bearing part of the design, and it is the opposite of what it looks like.
 *
 * If editing a field recomposed the WHOLE block from parsed fields, then anything the parser could
 * not represent would be destroyed the first time the author touched an unrelated text box — a
 * malformed retention line, a section we do not know, a stray paragraph between two headers. The
 * damage would be silent and would happen while editing something else entirely.
 *
 * So a field edit does not recompose. It calls {@link replaceH3Section}, which rewrites exactly the
 * bytes of one section's body and leaves every other byte in the block alone — understood or not.
 * {@link composeH3Prompt} is demoted to SEEDING a block (a fresh shot, or a fallback when the LLM
 * returns fields rather than text) and never rewrites one.
 *
 * ## What decides the schema
 *
 * The section set present, and nothing else. `detailed_description` ⇒ Ref2VA's six sections;
 * `integrated_multimodal_description` ⇒ the FL2VA family's three. This mirrors
 * `composeH3Prompt`, where the field name is a literal token sequence the model was trained to see
 * at that position rather than a style choice.
 *
 * Authority for the block's shape is MiniMax's own guides —
 * `VIDEO_PROMPT_WRITING_GUIDE_{base,ref}_en.md`. Where this file and a guide disagree, the guide
 * wins; where the guide is silent (task-type ORDER, `\n\n` section separators, `<scenetrans>`
 * placement) the ruling is ours and is recorded in `SPEC-h3-prompt-emitter.md` §K.
 */

import type { H3RetentionEntry, H3SubjectDefinition, H3Task, H3TaskType } from '#lib/api/client.ts';
import { H3_TASK_TYPES, H3_TRANSITION_VERBS, type H3Alignment, type H3ShotBody } from './h3-prompt';

/**
 * Every section header the parser recognises, in the composer's emission order.
 *
 * 🔴 The list is CLOSED, and that is a safety property rather than an omission. The parser finds
 * sections by scanning for header LINES, so a permissive `^(\w+):` would split a block at any
 * sentence an author happened to open with a word and a colon — *"Note: she turns away"* would
 * silently become a section boundary and cut a shot body in half. Only these names split.
 */
export const H3_SECTION_NAMES = [
  'subject_definitions',
  'summary',
  'retention_analysis',
  'detailed_description',
  'integrated_multimodal_description',
  'overall_soundscape',
  'non_diegetic_music'
] as const;

export type H3SectionName = (typeof H3_SECTION_NAMES)[number];

const isSectionName = (s: string): s is H3SectionName =>
  (H3_SECTION_NAMES as readonly string[]).includes(s);

/**
 * How a header was written. **Not cosmetic, and not ours to normalise.**
 *
 * Ref2VA writes `name:` then a newline; the FL2VA family writes `name: ` inline with the content on
 * the same line. Both are unambiguous in their own literal scripts (`h3-prompt.ts:411`), so
 * {@link replaceH3Section} preserves whichever form it found rather than imposing one.
 */
export type H3SectionForm = 'own-line' | 'inline';

/** One section as it sits in the text, with the offsets that make a surgical rewrite possible. */
export interface H3ParsedSection {
  name: H3SectionName;
  form: H3SectionForm;
  /** The body, trimmed of its surrounding blank lines. */
  body: string;
  /** Offset of the first character of the header line. */
  headerStart: number;
  /** Offset just past the body — where the next section's separator begins. */
  bodyEnd: number;
}

/** A run of text the parser could not place, kept verbatim so the editor can show it. */
export interface H3UnparsedRun {
  text: string;
  /** Offset in the source block. */
  at: number;
  /**
   * The section it was found inside, or `null` when it sat between sections (or before the first).
   * Lets the editor put it next to the field it belongs near instead of in one bin at the bottom.
   */
  section: H3SectionName | null;
}

/** Everything a block says, as the lens reads it. */
export interface H3ParsedBlock {
  /**
   * Which checkpoint this block is written for, or `null` when neither main field is present —
   * which means the text is not an H3 block at all, and the caller should treat it as prose.
   */
  task: H3Task | null;
  /** Derived from the leading alignment line. `none` when there is not one. */
  alignment: H3Alignment;
  /** The alignment line verbatim, or `null`. Re-emitted from `alignment`, never from this. */
  alignmentLine: string | null;
  taskTypes: H3TaskType[];
  styleOpening: string | null;
  subjectDefinitions: H3SubjectDefinition[];
  summary: string | null;
  retentionAnalysis: H3RetentionEntry[];
  shots: H3ShotBody[];
  overallSoundscape: string | null;
  nonDiegeticMusic: string | null;
  /** The raw sections, in the order they appear. For {@link replaceH3Section} and for diagnostics. */
  sections: H3ParsedSection[];
  unparsed: H3UnparsedRun[];
}

// ── Finding the sections ─────────────────────────────────────────────────────

/**
 * A header line: optional leading spaces, a known name, a colon, then either nothing (own-line) or
 * content (inline). Anchored per line via the `m` flag.
 *
 * ⚠️ Deliberately NOT split on `\n\n`, which is how the composer joins sections. An author who
 * presses Enter twice inside `summary` writes a blank line into a section body, and a `\n\n` split
 * would tear the section in half and re-label its remainder. Scanning for headers is indifferent to
 * how much whitespace sits between them — which also makes the parser agnostic to the separator
 * width the engine stopped caring about when it moved to `multi_prompts_gen_type = "FG"`.
 */
const HEADER_RE = new RegExp(`^[ \\t]*(${H3_SECTION_NAMES.join('|')})[ \\t]*:([ \\t]*)(.*)$`, 'gm');

/**
 * A header we do not recognise — a schema field MiniMax adds later, or one an LLM invented.
 *
 * 🔴 Used **only to end the previous section's body**, never to claim content. That distinction is
 * the whole reason it can exist at all: {@link H3_SECTION_NAMES} is closed precisely so a sentence
 * opening with a word and a colon cannot split a shot body, and widening it would reintroduce
 * exactly that. So this is deliberately narrow — own-line only (nothing after the colon), lowercase
 * `snake_case`, three characters or more. `[Shot 1]` cannot match it, and neither can *"Note: the
 * paper is torn"*, which is capitalised and carries its content on the same line.
 *
 * Without this boundary an unknown section is absorbed into whichever section precedes it, and then
 * a field edit on THAT section reflows it — the text survives, but its blank line and its identity
 * as a section do not. With it, an unknown section is bytes nothing touches.
 */
const FOREIGN_HEADER_RE = /^[ \t]*([a-z][a-z0-9_]{2,})[ \t]*:[ \t]*$/gm;

/** Split a block into its sections, whatever sat before the first one, and any foreign headers. */
function findSections(text: string): {
  lead: string;
  sections: H3ParsedSection[];
  foreign: { name: string; headerStart: number; end: number }[];
} {
  type HitAt = { form: H3SectionForm; headerStart: number; bodyStart: number };
  // `known` is a literal discriminant, not a boolean: the sort below interleaves known and foreign
  // hits, and only a discriminated union narrows `name` back to H3SectionName after the guard.
  type Hit =
    (HitAt & { known: true; name: H3SectionName }) | (HitAt & { known: false; name: string });
  const hits: Hit[] = [];

  HEADER_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = HEADER_RE.exec(text)) !== null) {
    const name = m[1];
    if (!isSectionName(name)) continue;
    // Inline when there is content on the header's own line. The gap between the colon and that
    // content is part of the header, not of the body.
    const inline = m[3].trim().length > 0;
    hits.push({
      name,
      known: true,
      form: inline ? 'inline' : 'own-line',
      headerStart: m.index,
      bodyStart: m.index + m[0].length - m[3].length
    });
  }

  FOREIGN_HEADER_RE.lastIndex = 0;
  while ((m = FOREIGN_HEADER_RE.exec(text)) !== null) {
    if (isSectionName(m[1])) continue; // already taken, in its own-line form
    hits.push({
      name: m[1],
      known: false,
      form: 'own-line',
      headerStart: m.index,
      bodyStart: m.index + m[0].length
    });
  }

  hits.sort((a, b) => a.headerStart - b.headerStart);

  const sections: H3ParsedSection[] = [];
  const foreign: { name: string; headerStart: number; end: number }[] = [];
  hits.forEach((h, i) => {
    const rawEnd = i + 1 < hits.length ? hits[i + 1].headerStart : text.length;
    if (!h.known) {
      foreign.push({ name: h.name, headerStart: h.headerStart, end: rawEnd });
      return;
    }
    const raw = text.slice(h.bodyStart, rawEnd);
    // Trim only the surrounding blank space: the body's own internal newlines are content.
    const body = raw.replace(/^[ \t]*\n?/, '').replace(/\s+$/, '');
    sections.push({
      name: h.name,
      form: h.form,
      body,
      headerStart: h.headerStart,
      bodyEnd: h.bodyStart + raw.replace(/\s+$/, '').length
    });
  });

  const leadEnd = hits.length ? hits[0].headerStart : text.length;
  return { lead: text.slice(0, leadEnd).trim(), sections, foreign };
}

// ── The alignment line ───────────────────────────────────────────────────────

/**
 * Which alignment variant a leading line is, if any.
 *
 * ⚠️ The three variants are bracketed inconsistently in the guide and `alignmentLine` emits each one
 * exactly as its own template writes it — I2VA and L2VA with `<Picture 1>` / `[Shot N]`, the
 * two-keyframe FL2VA line unbracketed in both its template and its worked example. So the detector
 * has to accept both bracketings rather than assume one, and it keys off the shape that separates
 * them: an FL2VA line names TWO pictures.
 */
function readAlignment(line: string): H3Alignment {
  const t = line.trim();
  if (!t) return 'none';
  if (/^For the target video, at 0\.00 seconds into the target video/.test(t)) return 'i2va';
  if (/^How the reference pictures align with the target video/.test(t)) {
    // Two pictures named ⇒ the first/last pair; one ⇒ the last-frame-only variant.
    return /<?Picture 2>?/.test(t) ? 'fl2va' : 'l2va';
  }
  return 'none';
}

// ── Section readers ──────────────────────────────────────────────────────────

/** A leading `<Subject 1>` / `<Picture 2>` / `<Video 1>` / `<Audio 3>` label. */
const LABEL_RE = /^(<(?:Subject|Picture|Video|Audio) \d+>)\s*/;

/**
 * `subject_definitions` — one definition per line.
 *
 * The label is stored SEPARATELY from the text, because that is how the editor shows it: the label
 * sits beside the box and the box holds the remainder. `composeH3Prompt` writes
 * `${label} ${stripLeadingLabel(text, label)}`, so either convention round-trips — splitting them is
 * what stops the author seeing `<Subject 1>` twice.
 *
 * 🔴 A line with no recognisable label is kept with an EMPTY label rather than discarded. The
 * composer emits `${''} ${text}`.trim(), which is the line unchanged, so the text survives a
 * recompose and the editor shows a blank label the author can fill in. Dropping it would lose an
 * author's sentence to a missing pair of angle brackets.
 */
function readDefinitions(body: string): H3SubjectDefinition[] {
  return body
    .split('\n')
    .map((l) => l.trim())
    .filter(Boolean)
    .map((line) => {
      const m = LABEL_RE.exec(line);
      return m ? { label: m[1], text: line.slice(m[0].length).trim() } : { label: '', text: line };
    });
}

/**
 * `retention_analysis` — `label (optional note): marker - text`.
 *
 * The marker is a single fixed English token and the hyphen after it is the separator, so the split
 * is on the FIRST ` - ` and everything after it is the explanation. Splitting on the last, or on
 * every occurrence, would break the moment an author wrote a hyphenated aside in the explanation —
 * which they will.
 *
 * A marker outside its label's vocabulary is kept AS WRITTEN. Validation belongs next to the field,
 * where it can be fixed; a parser that silently substituted a legal marker would be making a claim
 * nobody made — the same rule `applyH3Reply` already follows for LLM replies.
 *
 * ⚠️ The explanation may be EMPTY (`\s*` after the hyphen, not `\s+`). `composeH3Prompt` filters an
 * entry with no text so it never emits one — but the editor is a lens on the block now, and a row
 * the author has just added has nowhere else to live. Requiring text would drop that row into
 * `unparsed`, i.e. it would vanish from the panel the instant it was created. Whitespace before the
 * hyphen is still required, so ordinary prose containing a dash cannot be read as an entry.
 */
const RETENTION_RE = /^(<[^>]+>)(?:\s*\(([^)]*)\))?\s*:\s*(\S+)\s+-\s*([\s\S]*)$/;

function readRetention(
  body: string,
  push: (text: string, at: number) => void,
  bodyAt: number
): H3RetentionEntry[] {
  const out: H3RetentionEntry[] = [];
  let at = bodyAt;
  for (const line of body.split('\n')) {
    const trimmed = line.trim();
    if (trimmed) {
      const m = RETENTION_RE.exec(trimmed);
      if (m) {
        out.push({
          label: m[1],
          ...(m[2]?.trim() ? { role_note: m[2].trim() } : {}),
          marker: m[3],
          text: m[4].trim()
        });
      } else {
        // Kept visible rather than coerced into an entry: an entry with an invented marker would
        // recompose into a line the author never wrote.
        push(trimmed, at);
      }
    }
    at += line.length + 1;
  }
  return out;
}

/**
 * `summary` — an optional `[task types]` bracket, then the paragraph.
 *
 * 🔴 The bracket is lifted ONLY when every ` + `-joined part is a known task type. A summary that
 * legitimately opens with a bracket of anything else — `[Shot 1]`, an author's note — keeps it as
 * prose. Guessing wrong here would delete text from the summary and put a task type on the render
 * that the author never chose.
 *
 * ⚠️ Order is normalised on recompose. `joinTaskTypes` re-emits in {@link H3_TASK_TYPES} order,
 * which is OUR canonical order (no rule is stated anywhere and the corpus contradicts itself), so a
 * hand-written block listing them differently comes back reordered. Lossless in meaning, not in
 * bytes — and stable, because a composed block is already in canonical order.
 */
function readSummary(body: string): { taskTypes: H3TaskType[]; summary: string } {
  const m = /^\[([^\]]*)\]\s*([\s\S]*)$/.exec(body.trim());
  if (!m) return { taskTypes: [], summary: body.trim() };
  const parts = m[1]
    .split('+')
    .map((s) => s.trim())
    .filter(Boolean);
  const known = parts.every((p) => (H3_TASK_TYPES as string[]).includes(p));
  if (!parts.length || !known) return { taskTypes: [], summary: body.trim() };
  return { taskTypes: parts as H3TaskType[], summary: m[2].trim() };
}

/** `[Shot 3]` — the tag that opens every shot in both schemas. */
const SHOT_TAG_RE = /\[Shot (\d+)\]/g;
/** `At 00:04.500,` — the cut time, present on every shot but the first. */
const CUT_TIME_RE = /^At (\d{2}):(\d{2})\.(\d{3}),\s*/;

/**
 * The main body — the style opening (Ref2VA only) and the `[Shot N]` bodies.
 *
 * The two schemas lay this out differently and the parser has to read both:
 *
 * - **Ref2VA** puts the style on its OWN LINE before `[Shot 1]` and one shot per line, so the style
 *   comes back exactly as it was written.
 * - **The FL2VA family** folds the style INSIDE `[Shot 1]` right after the tag, with the shots
 *   running inline separated by a space. There is no marker separating style from shot body there,
 *   so the parser does not try to split them: the whole thing stays in the shot body and
 *   `styleOpening` is left `null`.
 *
 *   🔴 That is round-trip-SAFE rather than lossy. `fl2vaBody` emits
 *   `[Shot 1] ${style}. ${body}`; parsing it back as one body and recomposing with no style emits
 *   the identical string. Trying to guess where the style ended is what would lose bytes.
 */
function readBody(
  body: string,
  isRef: boolean
): { styleOpening: string | null; shots: H3ShotBody[] } {
  SHOT_TAG_RE.lastIndex = 0;
  const tags: { n: number; at: number; end: number }[] = [];
  let m: RegExpExecArray | null;
  while ((m = SHOT_TAG_RE.exec(body)) !== null) {
    tags.push({ n: Number(m[1]), at: m.index, end: m.index + m[0].length });
  }
  if (!tags.length) {
    // No tags at all — prose, not a schema body. Keep every byte as the first shot so nothing is
    // lost, and let the validator say the tag is missing.
    const t = body.trim();
    return { styleOpening: null, shots: t ? [{ index: 0, atSeconds: null, body: t }] : [] };
  }

  const lead = body.slice(0, tags[0].at).trim();
  const shots: H3ShotBody[] = tags.map((tag, i) => {
    const raw = body.slice(tag.end, i + 1 < tags.length ? tags[i + 1].at : body.length).trim();
    return readShot(tag.n - 1, raw);
  });

  // 🔴 The trailing `<scenetrans>` on a shot is the OUTGOING half of a pair and is DERIVED by the
  // composer from the NEXT shot's flag — it is never stored on the shot that carries it. So it is
  // stripped on the way in and re-derived on the way out, which is what keeps the two halves from
  // being written independently and drifting apart.
  for (let i = 0; i < shots.length; i++) {
    shots[i].body = shots[i].body.replace(/\s*<scenetrans>\s*$/, '').trim();
  }

  // On the FL2VA family the lead is empty (style lives inside [Shot 1]); on Ref2VA it is the style.
  return { styleOpening: isRef && lead ? lead : null, shots };
}

/** One shot's text, after its `[Shot N]` tag. */
function readShot(index: number, raw: string): H3ShotBody {
  let rest = raw;
  let atSeconds: number | null = null;
  const t = CUT_TIME_RE.exec(rest);
  if (t) {
    atSeconds = Number(t[1]) * 60 + Number(t[2]) + Number(t[3]) / 1000;
    rest = rest.slice(t[0].length);
  }
  // The INCOMING half of a `<scenetrans>` pair sits immediately after the cut time.
  let scenetrans = false;
  const st = /^<scenetrans>\s*/.exec(rest);
  if (st) {
    scenetrans = true;
    rest = rest.slice(st[0].length);
  }
  // 🔴 The verb is detected but LEFT IN the body. `shotText` only prefixes a verb the body does not
  // already open with, so keeping it makes the recompose byte-identical — while stripping it would
  // be a coin flip on whether the author's own wording came back.
  //
  // 🔴 Detected against the WIDER `H3_TRANSITION_VERBS`, not the five the picker offers. A
  // hand-written or LLM-written cross-dissolve IS a cut to the model; matching only the offered set
  // read it back as a beat, so the editor would have offered to "keep the camera rolling" on a shot
  // that dissolves. Longest-first so `the shot cuts to` cannot shadow a longer verb sharing a prefix.
  const verb = [...H3_TRANSITION_VERBS]
    .sort((a, b) => b.length - a.length)
    .find((v) => rest.toLowerCase().startsWith(v.toLowerCase()));
  return {
    index,
    atSeconds,
    body: rest.trim(),
    ...(verb ? { cutVerb: verb } : {}),
    ...(scenetrans ? { scenetrans: true } : {})
  };
}

/**
 * `overall_soundscape` → `overall soundscape`. The block's field names, as prose.
 *
 * The wire name is the schema the model was trained on and never changes; this is only ever what a
 * label, a picker option or a sentence shows. It was a local in `H3BlockEditor` until the phone
 * shell added three components that render the same names; promoted here so the two shells cannot
 * come to disagree about what a section is called.
 */
export const sectionLabel = (name: string): string => name.replace(/_/g, ' ');

// ── The parser ───────────────────────────────────────────────────────────────

/**
 * Read a block into fields. Never throws; never drops.
 *
 * `task` is `null` when neither main field is present, which is the caller's signal that the text is
 * not an H3 block at all — prose, or a paste of something else — and should be treated as such
 * rather than shown as a block with five empty sections.
 */
export function parseH3Block(text: string): H3ParsedBlock {
  const unparsed: H3UnparsedRun[] = [];
  const src = text ?? '';
  const { lead, sections, foreign } = findSections(src);
  const at = (name: H3SectionName) => sections.find((s) => s.name === name);

  // A section we do not know is REPORTED rather than absorbed. It is not editable through any panel
  // — there is no field for it — but it is in the block, it will be sent, and the author has to be
  // able to see that. `replaceH3Section` already leaves its bytes alone.
  for (const f of foreign) {
    unparsed.push({
      text: src.slice(f.headerStart, f.end).trim(),
      at: f.headerStart,
      section: null
    });
  }

  const detailed = at('detailed_description');
  const integrated = at('integrated_multimodal_description');
  const task: H3Task | null = detailed ? 'ref2va' : integrated ? 't2va' : null;
  const isRef = task === 'ref2va';

  // The lead is the alignment line when it is one, and unparsed otherwise. 🔴 An unrecognised lead
  // must NOT be silently swallowed: on the FL2VA family it is the position an alignment line lives
  // in, so a typo'd one would otherwise vanish and take the frame pin's meaning with it.
  const alignment = readAlignment(lead);
  if (lead && alignment === 'none') unparsed.push({ text: lead, at: 0, section: null });

  const summarySec = at('summary');
  const { taskTypes, summary } = summarySec
    ? readSummary(summarySec.body)
    : { taskTypes: [] as H3TaskType[], summary: '' };

  const retentionSec = at('retention_analysis');
  const retentionAnalysis = retentionSec
    ? readRetention(
        retentionSec.body,
        (t, offset) => unparsed.push({ text: t, at: offset, section: 'retention_analysis' }),
        retentionSec.bodyEnd - retentionSec.body.length
      )
    : [];

  const main = detailed ?? integrated;
  const { styleOpening, shots } = main
    ? readBody(main.body, isRef)
    : { styleOpening: null, shots: [] as H3ShotBody[] };

  const defsSec = at('subject_definitions');
  const sound = at('overall_soundscape');
  const music = at('non_diegetic_music');

  return {
    task,
    // 🔴 An alignment line and the Ref2VA schema cannot coexist: the alignment line is the FL2VA
    // family's, and Ref2VA carries no such line. A block with both is malformed, and the SECTION SET
    // is the authority on the checkpoint, so the line is the part that is wrong.
    alignment: isRef ? 'none' : alignment,
    alignmentLine: lead && alignment !== 'none' ? lead : null,
    taskTypes,
    styleOpening,
    subjectDefinitions: defsSec ? readDefinitions(defsSec.body) : [],
    summary: summary || null,
    retentionAnalysis,
    shots,
    overallSoundscape: sound?.body.trim() || null,
    nonDiegeticMusic: music?.body.trim() || null,
    sections,
    unparsed
  };
}

// ── The surgical write ───────────────────────────────────────────────────────

/**
 * Replace ONE section's body, leaving every other byte of the block untouched.
 *
 * 🔴 **This is what makes the field lens lossless**, and it is the reason the parser is allowed to
 * be best-effort. A field edit that recomposed the whole block from parsed fields would destroy
 * anything the parser could not represent — a malformed retention line, a section we have never
 * heard of, a paragraph an author left between two headers — and it would do it while they were
 * editing something else entirely. Rewriting one section's bytes cannot.
 *
 * The header's FORM is preserved, not normalised: Ref2VA writes `name:` then a newline and the
 * FL2VA family writes `name: ` inline, and both are correct for their own schema.
 *
 * A section that is not present is APPENDED, in {@link H3_SECTION_NAMES} order relative to the
 * sections that are — so writing a soundscape into a block that has none puts it where the schema
 * says it goes rather than at the end. The form for a new section follows the block's existing
 * sections; a block with none falls back to the form the schema uses.
 */
export function replaceH3Section(block: string, name: H3SectionName, body: string): string {
  const text = block ?? '';
  const { sections } = findSections(text);
  const hit = sections.find((s) => s.name === name);
  const next = body.trim();

  if (hit) {
    const head = hit.form === 'inline' ? `${name}: ` : `${name}:\n`;
    return text.slice(0, hit.headerStart) + head + next + text.slice(hit.bodyEnd);
  }

  // Not present. Insert before the first section that sorts AFTER it, or append.
  const order = (n: H3SectionName) => H3_SECTION_NAMES.indexOf(n);
  const after = sections.find((s) => order(s.name) > order(name));
  // Follow the block's own convention. `integrated_multimodal_description` implies the inline form
  // even when it is the section being added, because that is the schema it belongs to.
  const inline = sections.length
    ? sections[0].form === 'inline'
    : name === 'integrated_multimodal_description';
  const head = inline ? `${name}: ` : `${name}:\n`;
  const chunk = head + next;
  if (after)
    return text.slice(0, after.headerStart) + chunk + '\n\n' + text.slice(after.headerStart);
  return text.trim() ? `${text.replace(/\s+$/, '')}\n\n${chunk}` : chunk;
}

/** Which main-body section this block uses. `null` when it has neither — i.e. it is not a block. */
export function h3MainSection(block: string): H3SectionName | null {
  const { sections } = findSections(block ?? '');
  if (sections.some((s) => s.name === 'detailed_description')) return 'detailed_description';
  if (sections.some((s) => s.name === 'integrated_multimodal_description')) {
    return 'integrated_multimodal_description';
  }
  return null;
}

/**
 * Does this text look like an H3 block at all?
 *
 * The test the editor uses to decide between the lens and a plain prose box. Deliberately stricter
 * than `isAssembledH3Block` in `h3-llm.ts`, which answers a different question ("is this a whole
 * block that has been pasted where one shot's body belongs") and may stay as it is.
 */
export const looksLikeH3Block = (text: string | null | undefined): boolean =>
  h3MainSection(text ?? '') !== null;
