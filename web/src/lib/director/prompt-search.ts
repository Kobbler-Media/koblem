/**
 * Find and replace **across every H3 prompt block in a project** — the arithmetic behind the
 * Prompts screen, kept out of the component so the part that rewrites the author's text is
 * testable on its own.
 *
 * ## Why this is not CodeMirror's own search extension
 *
 * `@codemirror/search` searches ONE document. The thing an author actually asks for here spans all
 * of them: *"the character is called Mia in eleven shots and I renamed her"*. So the unit of work is
 * the LIST — a match carries the id of the block it was found in, the cursor walks from the last
 * match of one block into the first of the next, and Replace all is one pass over the set the
 * filter left on screen.
 *
 * ## 🔴 Literal, never a regular expression
 *
 * A block is a schema of angle-bracket labels, square-bracket tags and timecodes — `<Subject 1>`,
 * `[Shot 2]`, `At 00:04.500,` — so almost every string an author would type as a needle is also a
 * regex with a different meaning, and the failure is silent: `[Shot 1]` as a pattern is a character
 * class that matches one of `Sho1`, so Replace all would rewrite a letter in the middle of a
 * hundred words instead of finding nothing. Case folding is offered because a rename genuinely needs
 * it; pattern syntax is not, and {@link findMatches} escapes nothing because it never compiles
 * anything.
 *
 * ## 🔴 A WINDOW is not a filter, and an absent window is not "everywhere"
 *
 * Scoping the search to one section of a block (*"only the soundscape lines"*) is expressed as a
 * `window` on the doc: absolute offsets, so every replacement stays offset arithmetic on the whole
 * text and nothing here has to know what a section is — that stays in the panel, with the parser.
 *
 * `window: null` means the whole document. A section the block does not HAVE must therefore be
 * `{ from: 0, to: 0 }` — an empty window, which matches nothing — and never `null`, which would
 * silently widen the search back to the whole block and let a scoped Replace all rewrite the prose
 * the author had just scoped away.
 *
 * ## What "one match" means
 *
 * Non-overlapping, left to right, and — the part that stops Replace all from looping — a
 * replacement is never re-searched. `cat` → `the cat` scans past the text it just wrote rather than
 * finding the needle inside its own output.
 */

/** A half-open range of absolute offsets in a document's text. */
export interface TextWindow {
  from: number;
  to: number;
}

/** One block, as this module sees it: an id to report against and the text to search. */
export interface PromptDoc {
  id: string;
  text: string;
  /**
   * Search only inside this range. `null`/absent is the whole text; an EMPTY range matches nothing.
   * See the window note at the top of the file for why those two are not the same thing.
   */
  window?: TextWindow | null;
}

export interface PromptSearchOptions {
  /** Off by default — a rename is normally meant to catch `Mia` and `mia` alike. */
  caseSensitive?: boolean;
  /**
   * Require a word boundary at each edge of the needle **that is itself a word character**.
   *
   * 🔴 The "that is itself" half is what makes the toggle usable on an H3 block. A block is full of
   * needles that begin and end in punctuation — `<Subject 1>`, `[Shot 2]`, `00:04.500,` — and a rule
   * that demanded a boundary on both edges unconditionally would make every one of them unfindable
   * the moment the toggle was on, for no gain: `>` is already a boundary.
   */
  wholeWord?: boolean;
}

/** Where one hit sits: which block, and the range inside it. */
export interface PromptMatch {
  docId: string;
  from: number;
  to: number;
}

/**
 * Every hit inside one string, in order.
 *
 * ⚠️ An empty needle yields NOTHING rather than a hit at every offset. The empty match is the one
 * input that makes both callers dangerous — highlighting would paint the whole document and
 * {@link replaceAllIn} would insert the replacement between every pair of characters — and "no
 * query typed yet" is the state the screen spends most of its life in.
 */
export function findMatches(
  text: string,
  query: string,
  opts: PromptSearchOptions = {}
): { from: number; to: number }[] {
  if (!query) return [];
  const hits = allHits(text, query, opts);
  return opts.wholeWord ? hits.filter((h) => isWholeWord(text, query, h)) : hits;
}

function allHits(
  text: string,
  query: string,
  opts: PromptSearchOptions
): { from: number; to: number }[] {
  if (!opts.caseSensitive) {
    const hay = sameLengthLower(text);
    const needle = sameLengthLower(query);
    // The fast path, and the only one that runs on text made of the characters a prompt is made of.
    if (hay !== null && needle !== null) return indexOfAll(hay, needle);
    return foldScan(text, query);
  }
  return indexOfAll(text, query);
}

/**
 * What counts as inside a word: letters, digits and `_`, Unicode-wide.
 *
 * `\p{L}` rather than `\w` because the cast is written in whatever alphabet the author writes in,
 * and `\w` would call every accented letter a boundary — so "Mia" would match inside "Miá".
 */
const WORD_CHAR = /[\p{L}\p{N}_]/u;
const isWordChar = (ch: string | undefined): boolean => ch !== undefined && WORD_CHAR.test(ch);

/** See {@link PromptSearchOptions.wholeWord} for why each edge is tested independently. */
function isWholeWord(text: string, query: string, hit: { from: number; to: number }): boolean {
  if (isWordChar(query[0]) && isWordChar(text[hit.from - 1])) return false;
  if (isWordChar(query[query.length - 1]) && isWordChar(text[hit.to])) return false;
  return true;
}

/** Every occurrence of `needle` in `hay`, resuming past each one so hits cannot overlap. */
function indexOfAll(hay: string, needle: string): { from: number; to: number }[] {
  const out: { from: number; to: number }[] = [];
  let at = hay.indexOf(needle);
  while (at !== -1) {
    out.push({ from: at, to: at + needle.length });
    at = hay.indexOf(needle, at + needle.length);
  }
  return out;
}

/**
 * `s.toLowerCase()` when it is the same LENGTH as `s`, else `null`.
 *
 * 🔴 The guard exists because a case-folded search reports offsets that are then used to REWRITE the
 * original string, and `toLowerCase` is not length-preserving in Unicode: `İ` (U+0130) folds to two
 * code units, `ẛ̣` folds to three. One such character anywhere earlier in a block shifts every offset
 * after it, so a Replace would eat the wrong bytes — silently, in the middle of a sentence, in the
 * one class of text (a name with a Turkish dotted capital I) where nobody would think to look.
 */
function sameLengthLower(s: string): string | null {
  const lower = s.toLowerCase();
  return lower.length === s.length ? lower : null;
}

/**
 * The exact fallback: a character-by-character scan of the ORIGINAL text, folding one character at a
 * time. Quadratic and only reached for text {@link sameLengthLower} refuses, where being right
 * matters more than the constant factor — a hit is `query.length` code units long by construction, so
 * the offsets are exact whatever the folding does to individual characters.
 */
function foldScan(text: string, query: string): { from: number; to: number }[] {
  const out: { from: number; to: number }[] = [];
  const n = query.length;
  outer: for (let i = 0; i + n <= text.length;) {
    for (let j = 0; j < n; j++) {
      const a = text[i + j];
      const b = query[j];
      if (a !== b && a.toLowerCase() !== b.toLowerCase()) {
        i++;
        continue outer;
      }
    }
    out.push({ from: i, to: i + n });
    i += n;
  }
  return out;
}

/** How many hits one string holds. */
export const countMatches = (text: string, query: string, opts: PromptSearchOptions = {}): number =>
  findMatches(text, query, opts).length;

/**
 * One doc's hits, **inside its window**.
 *
 * ⚠️ The whole text is scanned and the hits are then filtered to the window, rather than the window's
 * SLICE being searched and the offsets shifted back. Same answer, and it keeps one property that is
 * worth more than the saved characters: every offset this module ever produces was measured on the
 * text it will be used to rewrite. A shift is one arithmetic slip away from a replacement landing a
 * few characters off, in a section nobody was looking at.
 */
export function docMatches(
  doc: PromptDoc,
  query: string,
  opts: PromptSearchOptions = {}
): { from: number; to: number }[] {
  const hits = findMatches(doc.text, query, opts);
  const w = doc.window;
  if (!w) return hits;
  return hits.filter((h) => h.from >= w.from && h.to <= w.to);
}

/**
 * Every hit across the whole list, **in the order the docs were handed over** — which is the order
 * they are drawn in, so the next/previous buttons walk down the screen rather than around a set.
 */
export function searchMatches(
  docs: readonly PromptDoc[],
  query: string,
  opts: PromptSearchOptions = {}
): PromptMatch[] {
  const out: PromptMatch[] = [];
  for (const d of docs) for (const m of docMatches(d, query, opts)) out.push({ docId: d.id, ...m });
  return out;
}

/**
 * Does this block survive the filter box? An empty filter keeps everything.
 *
 * ⚠️ Window-aware and toggle-aware, so "the shots shown" means the same thing as "where the hits
 * are". A filter that matched anywhere while the search was scoped to one section would list blocks
 * that Replace all then declined to touch, with nothing on screen explaining the discrepancy.
 */
export const docMatchesFilter = (
  doc: PromptDoc,
  filter: string,
  opts: PromptSearchOptions = {}
): boolean => !filter || docMatches(doc, filter, opts).length > 0;

/** The blocks the filter leaves on screen — and therefore the blocks a Replace all may touch. */
export const filterDocs = <T extends PromptDoc>(
  docs: readonly T[],
  filter: string,
  opts: PromptSearchOptions = {}
): T[] => docs.filter((d) => docMatchesFilter(d, filter, opts));

/** Index of `active` in `matches`, or `-1`. Identity is the triple, not object identity. */
export const indexOfMatch = (
  matches: readonly PromptMatch[],
  active: PromptMatch | null
): number =>
  active === null
    ? -1
    : matches.findIndex(
        (m) => m.docId === active.docId && m.from === active.from && m.to === active.to
      );

/**
 * Step the cursor, wrapping at both ends.
 *
 * A cursor that has fallen off the list — the author typed in the block under it, or the filter
 * moved — restarts from the appropriate end rather than refusing, because "Next found nothing"
 * while eleven hits are visible on screen reads as a broken button.
 */
export function stepMatch(
  matches: readonly PromptMatch[],
  active: PromptMatch | null,
  dir: 1 | -1
): PromptMatch | null {
  if (matches.length === 0) return null;
  const at = indexOfMatch(matches, active);
  if (at === -1) return dir === 1 ? matches[0] : matches[matches.length - 1];
  return matches[(at + dir + matches.length) % matches.length];
}

/**
 * The first hit at or after `pos` in `docId`, else the first hit in a LATER doc, else the first hit
 * anywhere. Where the cursor lands after a replacement.
 */
export function matchAtOrAfter(
  docs: readonly PromptDoc[],
  matches: readonly PromptMatch[],
  docId: string,
  pos: number
): PromptMatch | null {
  if (matches.length === 0) return null;
  const order = new Map(docs.map((d, i) => [d.id, i]));
  const home = order.get(docId) ?? 0;
  return (
    matches.find((m) => m.docId === docId && m.from >= pos) ??
    matches.find((m) => (order.get(m.docId) ?? 0) > home) ??
    matches[0]
  );
}

/** Replace one range. Split out so the offsets are arithmetic in one place rather than at each call. */
export const replaceRange = (text: string, from: number, to: number, replacement: string): string =>
  text.slice(0, from) + replacement + text.slice(to);

/**
 * Rewrite one hit and say where the cursor goes.
 *
 * 🔴 **Returns `null` when the range no longer holds the needle.** The screen keeps its cursor as a
 * pair of offsets while every block underneath is a live editor the author can type into, so by the
 * time Replace is pressed the range may name different bytes than it did when it was painted — and
 * rewriting them anyway would silently damage a sentence nobody was looking at. A refusal makes the
 * caller recompute, which costs one search.
 *
 * The cursor lands on the first hit at or after the END of what was just written, so a replacement
 * containing the needle (`Mia` → `Mia Chen`) is stepped over instead of found again.
 */
export function replaceMatch(
  docs: readonly PromptDoc[],
  active: PromptMatch,
  query: string,
  replacement: string,
  opts: PromptSearchOptions = {}
): { docs: PromptDoc[]; text: string; active: PromptMatch | null } | null {
  if (!query) return null;
  const doc = docs.find((d) => d.id === active.docId);
  if (!doc) return null;
  // 🔴 Re-derived rather than trusted: the bytes must still BE the needle, and the hit must still be
  // one this search would produce — a cursor left over from before the window or the whole-word
  // toggle moved names a range that is no longer in scope, and rewriting it would edit text the
  // author has scoped away.
  if (!docMatches(doc, query, opts).some((h) => h.from === active.from && h.to === active.to))
    return null;
  const text = replaceRange(doc.text, active.from, active.to, replacement);
  // 🔴 The edited doc's window has to grow by what the replacement added, or the cursor's next stop is
  // chosen against a window that ends one replacement too early — and the last hit in a scoped section
  // would be skipped every time the replacement was longer than the needle. The hit is inside the
  // window by construction (the guard above), so only `to` moves.
  const delta = replacement.length - (active.to - active.from);
  const grown = doc.window ? { from: doc.window.from, to: doc.window.to + delta } : doc.window;
  const next = docs.map((d) => (d.id === doc.id ? { ...d, text, window: grown } : d));
  return {
    docs: next,
    text,
    active: matchAtOrAfter(
      next,
      searchMatches(next, query, opts),
      doc.id,
      active.from + replacement.length
    )
  };
}

/**
 * Write `replacement` over the given hits, left to right, in one pass.
 *
 * Takes the hits rather than finding them, so the rules that DECIDE a hit — the case fold, whole
 * word, the window — live in exactly one place and cannot be applied to the search and forgotten on
 * the rewrite. The scan is manual rather than `String.replaceAll` because a case-insensitive pass has
 * to preserve the bytes AROUND each hit while replacing bytes that differ from the needle, and
 * because `replaceAll` with a string needle would treat `$&` in the replacement as a back-reference.
 *
 * ⚠️ Hits must be ascending and non-overlapping, which is what {@link findMatches} produces.
 */
export function applyReplacements(
  text: string,
  hits: readonly { from: number; to: number }[],
  replacement: string
): { text: string; count: number } {
  if (hits.length === 0) return { text, count: 0 };
  let out = '';
  let at = 0;
  for (const h of hits) {
    out += text.slice(at, h.from) + replacement;
    at = h.to;
  }
  return { text: out + text.slice(at), count: hits.length };
}

/** Replace every hit in one string. The window-free form — {@link replaceAllAcross} is the real one. */
export const replaceAllIn = (
  text: string,
  query: string,
  replacement: string,
  opts: PromptSearchOptions = {}
): { text: string; count: number } =>
  applyReplacements(text, findMatches(text, query, opts), replacement);

/**
 * Replace all, over exactly the docs handed in — i.e. the ones the filter left on screen — and
 * inside each one's window.
 */
export function replaceAllAcross(
  docs: readonly PromptDoc[],
  query: string,
  replacement: string,
  opts: PromptSearchOptions = {}
): { changed: { id: string; text: string }[]; count: number } {
  const changed: { id: string; text: string }[] = [];
  let count = 0;
  for (const d of docs) {
    const r = applyReplacements(d.text, docMatches(d, query, opts), replacement);
    if (r.count === 0) continue;
    changed.push({ id: d.id, text: r.text });
    count += r.count;
  }
  return { changed, count };
}
