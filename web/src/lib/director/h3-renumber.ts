/**
 * Keeping a block's labels pointing at what they pointed at.
 *
 * ## Why this has to exist
 *
 * Nothing in the H3 document stores which reference a `<Picture 2>` means. The number IS the
 * binding, and it is allocated by POSITION — `h3Ordinals` walks the in-scope references in the
 * engine's own pack order and hands out `1, 2, 3…`. So taking one reference out of scope, adding
 * one, or reordering the roster renumbers everything after it, and every sentence in the prompt
 * that named a number now names a different asset.
 *
 * Before this module the prompt simply went stale and the author found out from a label that
 * resolved to nothing, if they looked.
 *
 * ## 🔴 The rule that makes it safe rather than merely automatic
 *
 * A rename that only shifts the survivors down is **actively wrong**. Take `<Picture 1>` out and
 * `<Picture 2>` becomes `<Picture 1>` — so a `retention_analysis` line that claimed the captain
 * silently becomes a claim about the dockhand, and it reads as perfectly correct. The same hazard is
 * already commented against on the retention label `<select>`: *"a select that silently re-pointed
 * the row at the first label in the list would rewrite a retention claim about one asset into a
 * claim about another."*
 *
 * So a departing reference's own mentions are sent to {@link deadSlot} — one past the end of its
 * kind, where nothing resolves and the editor paints them dead. A stale number the author can see is
 * recoverable. A plausible number pointing at the wrong picture is not.
 *
 * ⚠️ This module renames labels and nothing else. Removing a `subject_definitions` LINE, or deciding
 * that scope changed at all, belongs to the caller — the two are separate edits and only one of them
 * is reversible by re-running this.
 */

import type { H3Ordinal } from './h3-prompt';

/** One label becoming another, everywhere in the block. */
export interface H3Rename {
  from: string;
  to: string;
}

/**
 * A placeholder no author can type.
 *
 * 🔴 Load-bearing. The swap below writes every `from` to a placeholder and then every placeholder to
 * its `to`, and if a placeholder could occur in the source the second pass would rewrite the
 * author's prose. Unit separators are not reachable from a keyboard and are not legal in any label.
 */
const HOLD = (i: number) => `\u001F${i}\u001F`;

/**
 * Apply renames simultaneously.
 *
 * 🔴 **Two passes, because the renames are a permutation and not a sequence.** Applied one after
 * another, `<Picture 2>` → `<Picture 1>` followed by `<Picture 1>` → `<Picture 4>` renames the
 * survivor twice and lands both on 4: the shift and the vacancy pass through each other. Writing
 * every source to a placeholder first makes them independent.
 */
export function h3Renumber(text: string, renames: readonly H3Rename[]): string {
  if (!renames.length) return text;
  let out = text;
  renames.forEach((r, i) => {
    out = out.split(r.from).join(HOLD(i));
  });
  renames.forEach((r, i) => {
    out = out.split(HOLD(i)).join(r.to);
  });
  return out;
}

/**
 * The renames for removing one subject definition.
 *
 * Subjects are numbered by their position in `subject_definitions`, so dropping the i-th shifts
 * every later one down. Unlike references these are declared IN the block, which is why removing one
 * has no dead slot: the name is simply gone, and the caller drops its line in the same edit.
 */
export function h3SubjectRenames(removedIndex: number, count: number): H3Rename[] {
  const out: H3Rename[] = [];
  for (let n = removedIndex + 2; n <= count; n++) {
    out.push({ from: `<Subject ${n}>`, to: `<Subject ${n - 1}>` });
  }
  return out;
}

/** `<Picture 3>` → `Picture`. */
const kindOf = (label: string): string => /^<(\w+)/.exec(label)?.[1] ?? '';

/**
 * One past the last number this kind allocates — a number that resolves to nothing, by construction.
 *
 * Counting per KIND and not across all of them: pictures, videos and audio are three independent
 * sequences, so a departing `<Audio 1>` in a shot with four pictures must land on `<Audio 2>`, not
 * on `<Picture 5>`.
 */
function deadSlot(label: string, after: readonly H3Ordinal[]): string {
  const kind = kindOf(label);
  const taken = after.flatMap((o) => (o.audioLabel ? [o.label, o.audioLabel] : [o.label]));
  return `<${kind} ${taken.filter((l) => kindOf(l) === kind).length + 1}>`;
}

/** Every label one ordinal carries — a soundtracked video owns its own `<Audio k>` as well. */
const labelsOf = (o: H3Ordinal): string[] => (o.audioLabel ? [o.label, o.audioLabel] : [o.label]);

/**
 * What the prompt has to be rewritten to when the numbering moves.
 *
 * Compared by reference id rather than by position, because position is exactly what changed. A
 * reference in `before` and not in `after` has left the shot; one in `after` and not in `before` has
 * just arrived and owns no prose yet, so it contributes nothing.
 */
export function h3OrdinalRenames(
  before: readonly H3Ordinal[],
  after: readonly H3Ordinal[]
): H3Rename[] {
  const now = new Map(after.map((o) => [o.id, o]));
  const out: H3Rename[] = [];
  // Departures first, so their mentions are already claimed by a placeholder before a survivor is
  // renamed onto the number they vacated.
  for (const was of before) {
    if (now.has(was.id)) continue;
    for (const label of labelsOf(was)) out.push({ from: label, to: deadSlot(label, after) });
  }
  for (const was of before) {
    const is = now.get(was.id);
    if (!is) continue;
    if (was.label !== is.label) out.push({ from: was.label, to: is.label });
    if (was.audioLabel && is.audioLabel && was.audioLabel !== is.audioLabel) {
      out.push({ from: was.audioLabel, to: is.audioLabel });
    }
  }
  return out;
}
