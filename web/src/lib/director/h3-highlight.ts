/**
 * Colouring an H3 block — the tokens the editor paints, and the section the caret is in.
 *
 * ## Why this is a separate pass from `parseH3Block`
 *
 * The parser answers *what does this block say*, and to do it it throws away exactly what a
 * highlighter needs: it hands back a `H3ShotBody[]` with the `[Shot 3]` tag consumed and the cut
 * time turned into a number, and it never records where a `<Picture 2>` sat in the prose. A
 * highlighter needs the opposite — every byte of the source, in order, with a label on the runs
 * that mean something. So this walks the SOURCE and yields a complete, non-overlapping cover of it:
 * `text.slice(t.start, t.end)` concatenated over the tokens is the input, byte for byte.
 *
 * The editor turns these runs into CodeMirror mark decorations over that same document
 * ({@link h3Decorations}), so the offsets are read back against the very text they were measured on.
 * The complete-cover property is what makes that safe to do incrementally, and it is asserted in the
 * tests rather than left as an intention.
 *
 * ## What is deliberately NOT highlighted
 *
 * Prose. There is no keyword list, no camera-motion vocabulary, no verb colouring — a body is the
 * author's English and painting parts of it would make the schema harder to see, not easier. What
 * gets a colour is what the MODEL resolves as structure: the section headers, the `[Shot N]` tags
 * and their cut times, the `<Label N>` ordinals, `<scenetrans>`, and the fixed retention markers.
 */

import { parseH3Block, type H3ParsedBlock, type H3SectionName } from './h3-block';
import { H3_AUDIO_MARKERS, H3_VISIBLE_MARKERS } from './h3-prompt';

export type H3TokenKind =
  /** A header the schema knows, colon included: `summary:`. */
  | 'header'
  /** A header we do not know but the block carries — shown as structure, flagged as foreign. */
  | 'foreign'
  /** `[Shot 3]`. */
  | 'shot'
  /** `At 00:04.500,` — the cut time, with its trailing comma. */
  | 'time'
  /** `<Subject 1>` / `<Picture 2>` / `<Video 1>` / `<Audio 3>`. */
  | 'label'
  /** `<scenetrans>`. */
  | 'scenetrans'
  /** A `retention_analysis` marker — `fully_preserved`, `fully_copy`, … */
  | 'marker'
  /** The `[reference generation + audio reuse]` bracket the summary opens with. */
  | 'tasktypes'
  | 'text';

export interface H3Token {
  kind: H3TokenKind;
  start: number;
  end: number;
  text: string;
  /**
   * The `[Shot N]` number, the label string, or the cut time in seconds — whatever the editor needs
   * to look this token up without re-parsing it out of `text`.
   */
  shot?: number;
  label?: string;
  seconds?: number;
}

/** Where one section sits in the source, for the "you are here" band. */
export interface H3SectionSpan {
  name: H3SectionName;
  /** First byte of the header line. */
  start: number;
  /** First byte of the body — where a jump-to-section puts the caret. */
  bodyStart: number;
  /** Just past the body. */
  end: number;
}

/**
 * One hover card the editor draws over a token.
 *
 * Lives here rather than in the component because the COMPOSER fills it in: which reference an
 * ordinal resolves to, whether the timeline has a beat for `[Shot 7]`, what a cut time is in song
 * time. All of that is the composer's derived state, and a component that had to know it would be a
 * second place that knows what a keyframe reference is.
 */
export interface H3TokenCard {
  title: string;
  meta?: string;
  body?: string;
  /** A resolved image URL — the caller has already been through `assetSrc`. */
  image?: string | null;
  /** Amber line: the label names nothing, the beat has no clip yet. */
  warn?: string;
}

/**
 * The section spans, from the parse rather than from a second scan.
 *
 * ⚠️ They do NOT cover the whole block: the alignment line before the first header belongs to no
 * section, and neither does the whitespace between two of them. A caret there has no active section,
 * which is the honest answer — `replaceH3Section` would not touch it either.
 */
export function h3SectionSpans(parsed: H3ParsedBlock): H3SectionSpan[] {
  return parsed.sections.map((s) => ({
    name: s.name,
    start: s.headerStart,
    // Both header forms are covered by measuring back from the end: the own-line form has a newline
    // in the gap and the inline form a space, and re-matching the header to find out would be a
    // third place that has to agree with the parser about what a header looks like.
    bodyStart: s.bodyEnd - s.body.length,
    end: s.bodyEnd
  }));
}

/** Which section a caret offset is in, or `null` between sections. */
export function h3SectionAt(spans: H3SectionSpan[], offset: number): H3SectionSpan | null {
  return spans.find((s) => offset >= s.start && offset <= s.end) ?? null;
}

/** What a section's header line says about itself, for the badge the editor hangs off it. */
export interface H3SectionBadge {
  name: H3SectionName;
  /**
   * Just past the header's colon.
   *
   * 🔴 An offset past the end of the document throws inside CodeMirror on the keystroke that
   * produced it, so this is derived from the parser's own header start rather than re-matched, and
   * pinned in the tests against both header forms.
   */
  at: number;
  /** Body length in characters — the section's share of the 7,000. */
  chars: number;
  /** Nothing written yet. The normal look of a fresh shot, so the badge can say so in words. */
  empty: boolean;
}

/**
 * The badges, one per section the block carries.
 *
 * 🔴 **Facts, not wording.** The gloss beside a section name is the composer's vocabulary and moves
 * with the engine's; putting the string here would be a second place that has to agree with it. So
 * this measures and the caller writes.
 */
export function h3SectionBadges(
  text: string,
  parsed: H3ParsedBlock = parseH3Block(text)
): H3SectionBadge[] {
  return h3SectionSpans(parsed).map((s) => ({
    name: s.name,
    // `name:` is the header in both forms, so the colon is always this far in.
    at: Math.min(s.start + s.name.length + 1, text.length),
    chars: s.end - s.bodyStart,
    empty: text.slice(s.bodyStart, s.end).trim().length === 0
  }));
}

const SHOT_RE = /\[Shot (\d+)\]/g;
const TIME_RE = /At (\d{2}):(\d{2})\.(\d{3}),/g;
const LABEL_RE = /<(?:Subject|Picture|Video|Audio) \d+>/g;
const SCENETRANS_RE = /<scenetrans>/g;
/** The summary's leading bracket. Only claimed when every part is a task type — see below. */
const TASKTYPES_RE = /\[[^\]\n]*\]/g;
const MARKERS = [...new Set([...H3_VISIBLE_MARKERS, ...H3_AUDIO_MARKERS])];
const MARKER_RE = new RegExp(`(?<![\\w-])(${MARKERS.join('|')})(?![\\w-])`, 'g');

type Hit = Omit<H3Token, 'text'>;

function scan(
  re: RegExp,
  text: string,
  from: number,
  to: number,
  make: (m: RegExpExecArray) => Hit | null
): Hit[] {
  const out: Hit[] = [];
  re.lastIndex = from;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index >= to) break;
    const hit = make(m);
    if (hit && hit.end <= to) out.push(hit);
  }
  return out;
}

/**
 * Tokenize a block into a complete, non-overlapping cover of its bytes.
 *
 * 🔴 **Section-scoped rather than global, for the two token kinds where a global scan would lie.** A
 * retention marker is a fixed English word — `reference` is one of them — and highlighting every
 * occurrence of "reference" in a 400-word body as schema would be noise pretending to be meaning. A
 * `[…]` bracket is a task-type list at the head of `summary:` and a `[Shot N]` tag everywhere else.
 * Both are therefore matched only inside the section where the schema gives them that meaning.
 *
 * `parsed` is passed in wherever the caller already has one — the composer parses on every keystroke
 * as it is, and parsing twice per frame for the colours is a cost with nothing to show for it.
 */
export function h3Tokens(text: string, parsed: H3ParsedBlock = parseH3Block(text)): H3Token[] {
  const src = text ?? '';
  const hits: Hit[] = [];

  for (const s of parsed.sections) {
    // The header runs from its own first byte to where the body starts. `bodyEnd - body.length` is
    // that offset for both header forms, which is why it is computed rather than re-matched: the
    // own-line form puts a newline in the gap and the inline form puts a space.
    const bodyStart = s.bodyEnd - s.body.length;
    hits.push({ kind: 'header', start: s.headerStart, end: bodyStart });

    if (s.name === 'retention_analysis') {
      hits.push(
        ...scan(MARKER_RE, src, bodyStart, s.bodyEnd, (m) => ({
          kind: 'marker',
          start: m.index,
          end: m.index + m[0].length
        }))
      );
    }
    if (s.name === 'summary') {
      // Only the FIRST bracket, and only when `readSummary` would have lifted it — the parser
      // refuses to claim a bracket of anything else, so painting one as a task-type list would
      // colour a claim it did not make.
      const bracket = scan(TASKTYPES_RE, src, bodyStart, s.bodyEnd, (m) => ({
        kind: 'tasktypes',
        start: m.index,
        end: m.index + m[0].length
      }))[0];
      if (bracket && parsed.taskTypes.length && src.slice(bodyStart, bracket.start).trim() === '') {
        hits.push(bracket);
      }
    }
  }

  // The foreign headers the parser reported. It records them as unparsed runs at their own offset,
  // so the header line is re-read from the SOURCE at that offset — `u.text` is trimmed, and a span
  // measured off it would fall short by whatever indentation the author used.
  for (const u of parsed.unparsed) {
    if (u.section !== null) continue;
    const eol = src.indexOf('\n', u.at);
    const line = src.slice(u.at, eol < 0 ? src.length : eol);
    if (/^[ \t]*[a-z][a-z0-9_]{2,}[ \t]*:[ \t]*$/.test(line)) {
      hits.push({ kind: 'foreign', start: u.at, end: u.at + line.length });
    }
  }

  hits.push(
    ...scan(SHOT_RE, src, 0, src.length, (m) => ({
      kind: 'shot',
      start: m.index,
      end: m.index + m[0].length,
      shot: Number(m[1])
    }))
  );
  hits.push(
    ...scan(TIME_RE, src, 0, src.length, (m) => ({
      kind: 'time',
      start: m.index,
      end: m.index + m[0].length,
      seconds: Number(m[1]) * 60 + Number(m[2]) + Number(m[3]) / 1000
    }))
  );
  hits.push(
    ...scan(LABEL_RE, src, 0, src.length, (m) => ({
      kind: 'label',
      start: m.index,
      end: m.index + m[0].length,
      label: m[0]
    }))
  );
  hits.push(
    ...scan(SCENETRANS_RE, src, 0, src.length, (m) => ({
      kind: 'scenetrans',
      start: m.index,
      end: m.index + m[0].length
    }))
  );

  // Earlier start wins; on a tie the longer run wins, so a header never loses to something that
  // starts at the same byte. Anything overlapping a run already taken is dropped rather than
  // clipped — a half-painted `[Shot 12]` is worse than an unpainted one.
  hits.sort((a, b) => a.start - b.start || b.end - a.end);

  const out: H3Token[] = [];
  let at = 0;
  for (const h of hits) {
    if (h.start < at || h.end <= h.start) continue;
    if (h.start > at)
      out.push({ kind: 'text', start: at, end: h.start, text: src.slice(at, h.start) });
    out.push({ ...h, text: src.slice(h.start, h.end) });
    at = h.end;
  }
  if (at < src.length) out.push({ kind: 'text', start: at, end: src.length, text: src.slice(at) });
  return out;
}

/** The stretch of text the caret is "in", for the purpose of dimming everything else. */
interface H3LiveRegion {
  start: number;
  end: number;
}

/**
 * Narrow a section down to the part the caret is actually working in.
 *
 * 🔴 **Inside the main body that is ONE `[Shot N]`, not the section.** `detailed_description` holds
 * the entire film — every shot in the render — so lighting the whole of it answers a question nobody
 * asked: the caret is always in `detailed_description`, and saying so locates nothing. The unit that
 * means something there is the shot being written, which is what the tag delimits.
 *
 * Everywhere else the section IS the unit, because there is nothing finer inside it: a soundscape is
 * one paragraph and a summary is one sentence.
 *
 * The style opening on Ref2VA — the text between the header and `[Shot 1]` — is its own region for
 * the same reason it is its own field: it belongs to no shot.
 */
function liveRegion(
  section: H3SectionSpan | null,
  tokens: H3Token[],
  caret: number
): H3LiveRegion[] {
  if (!section) return [];
  const isBody =
    section.name === 'detailed_description' || section.name === 'integrated_multimodal_description';
  if (!isBody) return [section];
  const tags = tokens.filter(
    (t) => t.kind === 'shot' && t.start >= section.start && t.end <= section.end
  );
  if (!tags.length) return [section];
  // Before the first tag: the style opening, which belongs to no shot. It is the head of the
  // section, so it reads as one unit with the header above it.
  if (caret < tags[0].start) return [{ start: section.start, end: tags[0].start }];
  for (let i = tags.length - 1; i >= 0; i--) {
    if (caret >= tags[i].start) {
      // 🔴 Two ranges, and the header is the second one deliberately: it is the landmark that says
      // WHICH section this shot is in, and dimming the header of the section being typed in reads
      // as a rendering fault rather than as focus.
      return [
        { start: section.start, end: section.bodyStart },
        { start: tags[i].start, end: i + 1 < tags.length ? tags[i + 1].start : section.end }
      ];
    }
  }
  return [section];
}

/** One painted range, in the shape a CodeMirror mark decoration wants. */
export interface H3Decoration {
  from: number;
  to: number;
  /** Space-separated class list — the token's kind, plus `h3-dim` when it is outside the caret's section. */
  cls: string;
}

/**
 * The ranges to paint over a block, given where the caret is.
 *
 * 🔴 **Sorted by `from`, non-empty, non-overlapping and inside the text — all four are load-bearing
 * rather than tidy.** These become mark decorations in a `RangeSetBuilder`, which requires ranges in
 * ascending order, rejects an empty one, and will read past the document for a range that runs off
 * the end. Any of the four going wrong is a thrown exception inside the editor on a keystroke, not a
 * mis-coloured word — so they are asserted in the tests instead of being left to the tokenizer's
 * good behaviour.
 *
 * `caret` of `null` means "not focused": nothing dims. On a freshly opened shot the caret sits at 0,
 * and dimming everything but the first section would read as a rendering fault rather than a cursor.
 *
 * `resolved` is the set of `<Label N>` the composer can actually answer for — in-scope references,
 * the packer's keyframes, and the block's own subject definitions. A label outside it is marked
 * `h3-dead`. Passing `undefined` marks nothing, because "the caller has not told us" and "nothing
 * resolves" are opposite claims and only one of them should be able to paint the block red.
 */
export function h3Decorations(
  text: string,
  caret: number | null,
  parsed: H3ParsedBlock = parseH3Block(text),
  resolved?: ReadonlySet<string>
): H3Decoration[] {
  const tokens = h3Tokens(text, parsed);
  const live =
    caret === null ? [] : liveRegion(h3SectionAt(h3SectionSpans(parsed), caret), tokens, caret);
  const out: H3Decoration[] = [];
  for (const t of tokens) {
    if (t.end <= t.start) continue;
    const dim = live.length > 0 && !live.some((r) => t.start < r.end && t.end > r.start);
    // Prose inside the live section is left undecorated — that is what makes it look like prose.
    if (t.kind === 'text' && !dim) continue;
    const dead = t.kind === 'label' && resolved !== undefined && !resolved.has(t.text);
    out.push({
      from: t.start,
      to: t.end,
      cls: `h3-${t.kind}${dead ? ' h3-dead' : ''}${dim ? ' h3-dim' : ''}`
    });
  }
  return out;
}
