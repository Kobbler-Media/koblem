/**
 * Which take a rendered join was actually made against — and therefore which renders a take
 * selection has just invalidated.
 *
 * ## The gap this closes
 *
 * Selecting a take is already the only picker there is. `rebuild_cut` re-derives the stitched cut
 * from the SELECTED take of every shot, and `h3_single_generation_chain` resolves a retake's
 * `continuation.from: "prev"` to `selected_bank_key(previous)` — the previous unit's *selected*
 * take, never its newest row. So "the selected take is the golden one" is true in both places that
 * matter, and nothing here should try to add a second way of saying it.
 *
 * What was silent is the OTHER direction. Shot 4 was rendered continuing out of whichever take of
 * shot 3 was selected at the time. Select a different take of shot 3 and shot 4's opening frames
 * now join a version of shot 3 that no longer plays — and nothing said so.
 *
 * 🔴 **A rebuild cannot repair that; only a re-render can.** `rebuild_cut`'s own doc says why:
 * picking an older take cannot rewind the engine's banked latents, and neither engine is
 * bit-reproducible from a seed. Offering "rebuild the cut" as the fix would hand back a cut whose
 * seam is still wrong, with a green tick on it. The repair is to re-render the downstream units, in
 * order, through the existing single-unit retake path — each one pins to its predecessor's freshly
 * selected take, so the cascade resolves one render at a time.
 *
 * ## What is actually recorded, and what is not
 *
 * ⚠️ **A take does not carry the pin it rendered against.** `director_shot_renders` stores the take's
 * OWN `bank_key`, not the key it continued FROM, and `last_retake_continuation_from` in project
 * metadata holds only the most recent final-tier retake's pin — one value for the whole project,
 * overwritten by the next retake. So the pin has to be RECONSTRUCTED, and this module is honest
 * about the two cases where it can be and the one where it cannot:
 *
 *  1. **Same render** — a full run writes one take per unit, all sharing a `source_job_id`, and it
 *     wipes every other take of the tier as it lands (`delete_by_project_tier_except`). Within one
 *     run, unit N continued from unit N−1 of that same run, because that is what `"prev"` means
 *     inside a single submitted chain. Definitive, no extra recording needed. This is the case the
 *     owner's workflow actually hits: full run → retake one shot → pick between the two.
 *  2. **Only candidate** — a retake's pin is not recorded, but a take of the previous unit that did
 *     not EXIST yet cannot have been pinned. When exactly one take of the previous unit predates
 *     this one, that is the one. Definitive.
 *  3. **Unrecorded** — a retake made when the previous unit already had two or more takes to choose
 *     between. Genuinely unknown. This module says `unrecorded` and claims NOTHING, because a
 *     warning that fires on a guess is a warning that gets ignored.
 *
 * ## Engine-neutral on purpose
 *
 * LTX has takes and continuations too and the same staleness applies to it, so nothing here branches
 * on the engine. The caller supplies, per render unit, whether that unit's join CARRIES anything
 * (H3: `continuation.mode !== 'none'`; LTX: the shot does not start fresh) and its takes. That is
 * the whole engine-specific surface.
 */

/** The fields of one take this module reads. A superset of `DirectorTake` plus its provenance. */
export interface TakeLike {
  variant: number;
  selected: boolean;
  /** RFC3339. Orders takes against each other; a take cannot have pinned one that came later. */
  created_at: string;
  /** The koblem render that produced this take. Absent on rows written before takes existed. */
  source_job_id?: string | null;
  /** H3 only: this take's own bank entry, `"<engine job id>:<generation index>"`. Display only. */
  bank_key?: string | null;
}

/** What one render unit's join does, as the document describes it (not as it rendered). */
export interface JoinSpec {
  /**
   * Does this unit's opening carry anything over from the previous unit's tail?
   *
   * 🔴 `false` is the whole reason this is a parameter. The first unit has nothing before it, and a
   * hard cut (`continuation.mode === 'none'` on H3, `startsFresh` on LTX) carries nothing over — so
   * neither can be invalidated by a selection upstream, and flagging them would be pure noise. It
   * also STOPS the cascade: a re-render before a hard cut cannot disturb anything after it.
   */
  carries: boolean;
  /**
   * A bank key the author pinned by hand, instead of `"prev"`.
   *
   * Such a join does not follow the previous unit's selection at all — koblem forwards the key
   * verbatim ("not the shim's to resolve, and not ours to second-guess"), so changing what is
   * selected upstream changes nothing about it. Not stale, and not repairable by re-rendering.
   */
  explicitFrom?: string | null;
}

/** One render unit, in ENGINE index space — the space takes are keyed by. */
export interface UnitTakes {
  index: number;
  /** Every take of this unit, oldest first (the order `list_takes` returns). */
  takes: TakeLike[];
  join: JoinSpec;
}

/** Which take of the previous unit a take was rendered against, and how confidently we know. */
export type JoinPin =
  | {
      kind: 'exact';
      take: TakeLike;
      /** Display ordinal (1-based POSITION, matching the takes list on screen). */
      takeNo: number;
      /** Why we can be sure. See the module doc's three cases. */
      basis: 'same-render' | 'only-candidate';
    }
  /** A retake made when there was more than one predecessor take. koblem did not record which. */
  | { kind: 'unrecorded'; candidates: TakeLike[] }
  /** Nothing to pin to: the previous unit has no take that predates this one. */
  | { kind: 'none' };

const ms = (t: string): number => {
  const n = Date.parse(t);
  // A row with an unparseable timestamp must not silently sort to 1970 and turn "could not have
  // pinned this" into "definitely pinned this". Treat it as newer than everything instead, which
  // can only widen the candidate set and therefore only ever downgrades a claim to `unrecorded`.
  return Number.isNaN(n) ? Number.POSITIVE_INFINITY : n;
};

/**
 * Reconstruct which of `prevTakes` the given take was rendered continuing from.
 *
 * `prevTakes` must be the previous unit's takes oldest-first — the returned `takeNo` is a position
 * in that array, because that is how the takes list numbers them on screen (`variant` is a stable
 * id that a full run's replacement makes drift away from what is displayed).
 */
export function pinOf(take: TakeLike, prevTakes: TakeLike[]): JoinPin {
  if (prevTakes.length === 0) return { kind: 'none' };
  const job = (take.source_job_id ?? '').trim();
  if (job) {
    // Case 1. At most one match by construction — `(project, tier, shot_index, source_job_id)` is
    // unique — so finding one means this take and that one came out of the same submitted chain,
    // where `"prev"` could only ever have meant that take.
    const at = prevTakes.findIndex((t) => (t.source_job_id ?? '').trim() === job);
    if (at >= 0) {
      return { kind: 'exact', take: prevTakes[at], takeNo: at + 1, basis: 'same-render' };
    }
  }
  // Case 2/3. The pin was resolved server-side at dispatch and never written to the row. All that
  // survives is elimination: a take minted after this one cannot have been what it pinned.
  const born = ms(take.created_at);
  const earlier = prevTakes.filter((t) => ms(t.created_at) < born);
  if (earlier.length === 0) return { kind: 'none' };
  if (earlier.length === 1) {
    return {
      kind: 'exact',
      take: earlier[0],
      takeNo: prevTakes.indexOf(earlier[0]) + 1,
      basis: 'only-candidate'
    };
  }
  return { kind: 'unrecorded', candidates: earlier };
}

/**
 * The state of one render unit's join.
 *
 * - `first` / `cut` / `pinned` — cannot go stale. See {@link JoinSpec}.
 * - `unrendered` — no take, so no join has happened yet.
 * - `ok` — the take that plays was rendered against the take of the previous unit that also plays.
 * - `broken` — it was rendered against a take that no longer plays. **Direct evidence.**
 * - `downstream` — its own join is intact TODAY, but a broken join sits upstream of it, and
 *   repairing that one re-renders the unit this one continues from. So it must be re-rendered too,
 *   after that one. Kept distinct from `broken` because it is a consequence of the repair, not a
 *   defect on screen right now, and saying otherwise would overstate the evidence.
 * - `unknown` — a retake whose pin koblem did not record. No claim either way.
 */
export type JoinState =
  'first' | 'cut' | 'pinned' | 'unrendered' | 'ok' | 'broken' | 'downstream' | 'unknown';

export interface UnitContinuity {
  index: number;
  state: JoinState;
  /** The take that plays, as a 1-based position in this unit's takes list. 0 when none does. */
  playingTakeNo: number;
  /** The previous unit's take this one rendered against, when it is known. */
  pin: JoinPin | null;
  /** One sentence naming the problem, or `''` for the states that are not one. */
  reason: string;
}

/** How a unit is named on screen. Defaults to its engine index + 1, which is what H3 shows. */
export type LabelOf = (index: number) => string;

const defaultLabel: LabelOf = (i) => `shot ${i + 1}`;

export interface ContinuityOptions {
  labelOf?: LabelOf;
  /**
   * `EngineCapabilities.retakeSplicesTail` — does re-rendering one unit re-pin it to the footage
   * that already follows it?
   *
   * 🔴 **This is what decides whether the cascade runs at all.** On LTX it is `true`: the retake
   * reloads the banked prefix, re-renders shot `k` alone and splices the banked tail back
   * end-pinned to `k`'s new final frame — keeping the seam after `k` is the point of that path, so
   * demanding shots `k+1…` be re-rendered would spend hours of GPU on a seam the engine already
   * holds. On H3 it is `false`: one generation renders, nothing splices, and the generation after
   * it opened on a tail that no longer exists.
   *
   * ⚠️ It does not silence the report, only the cascade. The take that plays is still not the take
   * the next unit was rendered against, and a take from a second FULL run was never end-pinned to
   * anything — so the row is still shown, with the engine's own caveat attached.
   */
  retakeSplicesTail?: boolean;
}

/**
 * Read every unit's join state in one pass, then propagate the repair cascade forward.
 *
 * `units` must be index-ordered and contiguous from 0 — it is the whole timeline in engine space,
 * not a filtered subset, because a unit's state is defined against its immediate predecessor.
 */
export function continuityOf(
  units: UnitTakes[],
  { labelOf = defaultLabel, retakeSplicesTail = false }: ContinuityOptions = {}
): UnitContinuity[] {
  const rows: UnitContinuity[] = units.map((u, i) => {
    const playingIdx = u.takes.findIndex((t) => t.selected);
    const base = { index: u.index, playingTakeNo: playingIdx + 1, pin: null, reason: '' };
    if (u.takes.length === 0) return { ...base, state: 'unrendered' as JoinState };
    if (i === 0) return { ...base, state: 'first' as JoinState };
    if (!u.join.carries) return { ...base, state: 'cut' as JoinState };
    if (u.join.explicitFrom?.trim()) {
      return {
        ...base,
        state: 'pinned' as JoinState,
        reason:
          `${labelOf(u.index)} pins the bank key ${u.join.explicitFrom.trim()} by hand, so it ` +
          `does not follow what is selected on ${labelOf(units[i - 1].index)}.`
      };
    }
    // 🔴 The join belongs to the take that PLAYS. An unselected take's seam is not on screen and is
    // not what a rebuild would join, so judging it would flag shots nobody is looking at.
    const playing = playingIdx >= 0 ? u.takes[playingIdx] : u.takes[u.takes.length - 1];
    const prev = units[i - 1];
    const pin = pinOf(playing, prev.takes);
    if (pin.kind === 'none') {
      return {
        ...base,
        pin,
        state: 'unknown' as JoinState,
        reason: `${labelOf(u.index)} has no recorded predecessor take to have joined.`
      };
    }
    if (pin.kind === 'unrecorded') {
      return {
        ...base,
        pin,
        state: 'unknown' as JoinState,
        reason:
          `${labelOf(u.index)} was re-rendered when ${labelOf(prev.index)} already had ` +
          `${pin.candidates.length} takes, and koblem records the pin a retake used only for the ` +
          `most recent one — so which of them this joins is not knowable. Re-render ` +
          `${labelOf(u.index)} to make the join certain again.`
      };
    }
    if (pin.take.selected) return { ...base, pin, state: 'ok' as JoinState };
    const playingPrev = prev.takes.findIndex((t) => t.selected) + 1;
    return {
      ...base,
      pin,
      state: 'broken' as JoinState,
      reason:
        `${labelOf(u.index)} was rendered from take ${pin.takeNo} of ${labelOf(prev.index)}; ` +
        (playingPrev > 0 ? `take ${playingPrev} now plays.` : 'no take of it plays now.') +
        // The engine's own mitigation, stated where the claim is made rather than as a footnote —
        // "the seam is wrong" and "the seam was deliberately pinned to survive this" are different
        // things to be told, and only one of them is worth a render.
        (retakeSplicesTail
          ? ' A retake here end-pins its final frame to the footage that follows, so the seam may' +
            ' well hold — look at it before spending a render.'
          : '')
    };
  });

  // ── The cascade ───────────────────────────────────────────────────────────────────────────────
  //
  // Re-rendering a broken unit produces a NEW take with a different tail, which the unit after it
  // was not rendered against. So everything downstream of a break has to follow — even though its
  // own join is intact right now, which is exactly why it gets its own state rather than `broken`.
  //
  // 🔴 A hard cut stops it dead. It carries nothing over, so a fresh tail upstream cannot reach it,
  // and neither can anything past it. Running the cascade through a cut would demand re-renders
  // that change nothing.
  //
  // 🔴 And it does not run at all where a retake re-pins to the tail it already has. On LTX the
  // engine splices the banked tail back end-pinned to the re-rendered shot's new final frame, so
  // repairing shot k leaves k+1… exactly as valid as they were — a cascade there would be a demand
  // for hours of GPU time to fix nothing.
  if (retakeSplicesTail) return rows;
  let cascading = false;
  for (let i = 0; i < rows.length; i += 1) {
    const r = rows[i];
    // Everything that cannot inherit a break also cannot pass one on: `first` and `cut` carry
    // nothing over, `pinned` joins a key the author named, and `unrendered` has no join yet.
    if (
      r.state === 'cut' ||
      r.state === 'first' ||
      r.state === 'pinned' ||
      r.state === 'unrendered'
    ) {
      cascading = false;
      continue;
    }
    if (r.state === 'broken') {
      cascading = true;
      continue;
    }
    if (!cascading) continue;
    r.state = 'downstream';
    r.reason =
      `${labelOf(r.index)} joins ${labelOf(rows[i - 1].index)}, which has to be ` +
      `re-rendered — so its tail changes and this one has to follow it.`;
  }
  return rows;
}

/**
 * The units to re-render, in the order they must go: the first broken join, then everything the
 * cascade carries. Empty when nothing is broken.
 *
 * ⚠️ **In order, one at a time.** Each retake resolves its own `"prev"` against whatever take of its
 * predecessor is selected WHEN IT DISPATCHES, and the harvest makes a fresh take the selected one
 * as it lands. Fire two at once and the second pins to the take the first was about to replace.
 */
export const repairPlan = (rows: UnitContinuity[]): number[] =>
  rows.filter((r) => r.state === 'broken' || r.state === 'downstream').map((r) => r.index);

/** A short badge word for a join state, or `''` where there is nothing worth a badge. */
export const joinBadge = (s: JoinState): string =>
  s === 'broken'
    ? 'join broken'
    : s === 'downstream'
      ? 're-render after'
      : s === 'unknown'
        ? 'join unrecorded'
        : '';

/**
 * The one line the composer's join panel shows: which take of the previous unit this join is pinned
 * to. Forward-looking — this is what the NEXT render of this unit will join, which is always the
 * predecessor's currently selected take.
 */
export function pendingPinNote(prevTakes: TakeLike[], prevLabel: string): string {
  if (prevTakes.length === 0) {
    return `Nothing of ${prevLabel} is rendered yet, so there is no banked tail to join.`;
  }
  const at = prevTakes.findIndex((t) => t.selected);
  if (at < 0) return `No take of ${prevLabel} is selected, so this join has nothing to pin to.`;
  const key = prevTakes[at].bank_key?.trim();
  const of = prevTakes.length > 1 ? ` of ${prevTakes.length}` : '';
  return (
    `Joins take ${at + 1}${of} of ${prevLabel}${key ? ` · ${key}` : ''}. Selecting a different ` +
    `take there changes what this joins, but only from the next render of it onwards — the ` +
    `footage already on disk keeps the join it was rendered with.`
  );
}
