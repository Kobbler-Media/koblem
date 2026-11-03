/**
 * What the time axis draws at a given zoom, and how a transcript is grouped to fit beside it.
 *
 * Both problems here are the same problem: the axis has a fixed number of pixels and the content
 * does not care. Solved by choosing a STEP that fits rather than by drawing everything and hoping.
 */

import type { LyricWord } from '#lib/api/client.ts';

/**
 * Candidate ruler steps as `[labelled, unlabelled]` pairs, seconds.
 *
 * Musical-ish rather than decimal — a cut is read against bars and phrases, so 15 and 30 earn their
 * places where 20 and 25 would not, and each minor is a division you would actually count in
 * (15 s in thirds, not fifths).
 */
const TICK_STEPS: [step: number, minor: number][] = [
  [0.1, 0.05],
  [0.25, 0.05],
  [0.5, 0.1],
  [1, 0.5],
  [2, 0.5],
  [5, 1],
  [10, 5],
  [15, 5],
  [30, 10],
  [60, 15],
  [120, 30],
  [300, 60]
];

/** Room a `MM:SS` label needs before its neighbour, in px. */
const LABEL_PX = 54;

export interface RulerTicks {
  /** Seconds between LABELLED ticks. */
  step: number;
  /** Seconds between unlabelled ticks. */
  minor: number;
  /** Labelled tick times, seconds. */
  major: number[];
  /** Unlabelled tick times — never a time that is already in {@link major}. */
  minors: number[];
}

/**
 * Choose a tick step that keeps labels from colliding, and lay the ticks out.
 *
 * 🔴 Replaces one labelled tick PER SECOND. That drew 209 nodes for a three-minute song and, below
 * ~54 px/s, drew them close enough that the labels overlapped into a smear — so the axis was least
 * readable at exactly the zoom where you are looking at the whole song and most need it.
 */
export function rulerTicks(totalSeconds: number, pxPerSec: number): RulerTicks {
  const span = Math.max(0, totalSeconds);
  // No zoom guard on the minor: every pair above divides its step by at most 5, and the step is
  // already at least LABEL_PX wide, so a minor is never closer than ~11 px. There is no reachable
  // zoom at which they would crowd, and a guard for one would be a branch that never runs.
  const [step, minor] =
    TICK_STEPS.find(([s]) => s * pxPerSec >= LABEL_PX) ?? TICK_STEPS[TICK_STEPS.length - 1];

  const major: number[] = [];
  for (let t = 0, i = 0; t <= span + 1e-9; i++, t = i * step) major.push(round(t));

  const minors: number[] = [];
  for (let t = 0, i = 0; t <= span + 1e-9; i++, t = i * minor) {
    // Modulo on floats lies; compare against the nearest major instead.
    if (Math.abs(t / step - Math.round(t / step)) > 1e-6) minors.push(round(t));
  }
  return { step, minor, major, minors };
}

const round = (n: number) => Math.round(n * 1e6) / 1e6;

/** `M:SS` for a ruler label — short, because it is repeated across the whole axis. */
export function tickLabel(seconds: number, step: number): string {
  const s = Math.max(0, seconds);
  const m = Math.floor(s / 60);
  const rest = s - m * 60;
  // Sub-second steps need a decimal or every label in a bar reads the same.
  if (step < 1) return `${m}:${rest.toFixed(1).padStart(4, '0')}`;
  return `${m}:${String(Math.round(rest)).padStart(2, '0')}`;
}

/**
 * A run of words sung without a break — a line, near enough.
 *
 * The unit that makes a transcript readable on an axis. Word-level labels are unplaceable: sung
 * words are ~0.3 s apart and their text is 6–10× wider than that at any usable zoom, so a
 * per-word view can only ever show a scattered minority and drop the rest — which is confetti, not
 * a transcript. A phrase spans its own words, so its label has the whole phrase's width to live in
 * and the text reads as language.
 */
export interface LyricPhrase {
  start: number;
  end: number;
  text: string;
  words: LyricWord[];
}

/** Silence longer than this between words ends the phrase. */
const PHRASE_GAP_SECONDS = 0.62;

/** Never let one phrase run longer than this, however continuously it is sung. */
const PHRASE_MAX_SECONDS = 9;

/**
 * Group a word-level transcript into phrases.
 *
 * Gap-based, not punctuation-based: STT output here has no punctuation to read, but its timings
 * are good (they are the same timings the beat picker snaps to). A held note does not split a
 * phrase — the gap is measured from the previous word's END, so `end`-to-`start` silence is what
 * counts and a two-second melisma stays inside its line.
 */
export function lyricPhrases(lyrics: LyricWord[]): LyricPhrase[] {
  const out: LyricPhrase[] = [];
  for (const w of lyrics) {
    const cur = out[out.length - 1];
    const broken =
      !cur || w.start - cur.end > PHRASE_GAP_SECONDS || w.end - cur.start > PHRASE_MAX_SECONDS;
    if (broken) {
      out.push({ start: w.start, end: w.end, text: w.text, words: [w] });
    } else {
      cur.words.push(w);
      cur.end = Math.max(cur.end, w.end);
      cur.text = `${cur.text} ${w.text}`;
    }
  }
  return out;
}

/**
 * Trim a phrase to what will actually fit its own span, with an ellipsis when it cannot.
 *
 * Cut on a WORD boundary. A phrase clipped mid-word reads as a different word — "the fire" cut to
 * "the fir" is not a shorter label, it is a wrong one — and the whole point of the phrase view is
 * that the text can be read as language.
 */
export function fitPhraseLabel(text: string, widthPx: number, charPx = 5.6): string {
  const budget = Math.floor((widthPx - 12) / charPx);
  if (budget <= 0) return '';
  if (text.length <= budget) return text;
  const words = text.split(' ');
  let out = '';
  for (const w of words) {
    const next = out ? `${out} ${w}` : w;
    if (next.length + 1 > budget) break;
    out = next;
  }
  // A single word wider than the whole span still has to say something, so it falls back to a hard
  // cut — but only where no word boundary was reachable at all.
  if (!out) return budget > 1 ? `${text.slice(0, budget - 1)}…` : '';
  return `${out}…`;
}
