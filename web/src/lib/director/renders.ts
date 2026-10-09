/**
 * Which persisted clips a Director client actually downloads, kept out of the component so the
 * rules that decide how many megabytes a phone pulls are testable on their own.
 *
 * Two rules live here:
 *  - the TIER governs downloads, not just what is displayed (see `tier.ts`: a `final` shot is
 *    1.4–19.5 MB and the proxy tier exists to not fetch it), and
 *  - a clip already held is never fetched again. A client that WATCHED a render holds the exact
 *    bytes the harvest then persisted, so the re-read that follows every render must cost rows, not
 *    the whole render set a second time.
 */

import type { RenderTier } from './tier';

/** The identity of one persisted clip: the render that produced it, plus which shot it is. */
export interface RenderRowLike {
  tier: string;
  shot_index: number;
  url: string;
  created_at: string;
  source_job_id?: string | null;
}

/** A clip this client has already downloaded, tagged with the identity it was fetched for. */
export interface HeldClip {
  index: number;
  url: string;
  /** [`clipKey`] of the row it was downloaded from, or [`jobClipKey`] of the render it streamed off. */
  key: string;
}

/**
 * Identity of a persisted clip: the producing render, the shot, and WHEN it was written.
 *
 * The timestamp is not redundant. A rebuilt cut is deliberately one take that replaces itself — same
 * producing id, same file name, new bytes — so without it, picking a different take twice would keep
 * showing the first rebuild.
 */
export function clipKey(row: RenderRowLike): string {
  return `row:${row.source_job_id || '?'}:${row.shot_index}:${row.created_at}`;
}

/** Identity of a clip streamed live off a render, before any row exists for it. */
export function jobClipKey(jobId: string, index: number): string {
  return `job:${jobId}:${index}`;
}

/**
 * Whether a held clip already IS this row's bytes.
 *
 * A live-streamed clip matches by render + shot rather than by exact key: it is the same file the
 * harvest then copies out, which is the whole reason a completed render does not have to download
 * everything it just streamed. It deliberately cannot match a rebuilt cut or an unattributable row,
 * neither of which names a render this client watched.
 */
function holds(key: string, row: RenderRowLike): boolean {
  if (key.startsWith('row:')) return key === clipKey(row);
  const cut = key.lastIndexOf(':');
  const jobId = key.slice('job:'.length, cut);
  return (
    !!row.source_job_id &&
    row.source_job_id === jobId &&
    key.slice(cut + 1) === String(row.shot_index)
  );
}

export interface HydrationPlan<T> {
  /** Rows to download. */
  fetch: T[];
  /** Held clips that are still current, in shot order. */
  keep: HeldClip[];
  /** Object URLs to revoke — held clips no longer backed by a row. */
  drop: string[];
}

/** Per-shot plan for one tier. Rows of other tiers are ignored: they are not this client's to pull. */
export function planHydration<T extends RenderRowLike>(
  rows: T[],
  tier: RenderTier,
  held: HeldClip[]
): HydrationPlan<T> {
  const mine = rows
    .filter((r) => r.tier === tier && r.shot_index >= 0)
    .sort((a, b) => a.shot_index - b.shot_index);
  const matched = mine.map((r) => ({ row: r, clip: held.find((h) => holds(h.key, r)) ?? null }));
  const kept = new Set(matched.map((m) => m.clip).filter((c): c is HeldClip => !!c));
  return {
    fetch: matched.filter((m) => !m.clip).map((m) => m.row),
    keep: matched.map((m) => m.clip).filter((c): c is HeldClip => !!c),
    drop: held.filter((h) => !kept.has(h)).map((h) => h.url)
  };
}

/** The same for the stitched cut of one tier (`shot_index === -1`). */
export function planCutHydration<T extends RenderRowLike>(
  rows: T[],
  tier: RenderTier,
  held: { url: string; key: string } | null
): { fetch: T | null; keep: boolean; drop: string | null } {
  const row = rows.find((r) => r.tier === tier && r.shot_index === -1) ?? null;
  if (row && held && holds(held.key, row)) return { fetch: null, keep: true, drop: null };
  return { fetch: row, keep: false, drop: held?.url ?? null };
}

/** A persisted row, reduced to what names the bank its take came out of. */
export interface BankedRowLike {
  tier: string;
  shot_index: number;
  latent_job_id?: string | null;
  /** Which engine's store `latent_job_id` addresses. Absent on a row migration `0039` could not place. */
  engine?: string | null;
}

/**
 * The tier's `resume_job_id`: the chain the first shot was banked out of ON THIS ENGINE.
 *
 * 🔴 NOT the newest chain. The engine resolves `resume_job_id` by composing `"<job>:<segment>"` and
 * walking forward from segment 0, so the only id it can accept is one whose bank holds segment 0 —
 * and a continue banks ONLY the shots it rendered under a fresh job id. Render five shots as job A,
 * continue two more as job B, adopt B, and the next continue asks B for segment 0 and is answered
 * with a hard 400. LTX-2.3 masked this by aliasing each fresh id onto the bank it continued, so
 * every id resolved every segment; LTX-2.5's bank is a flat store with no aliases.
 *
 * 🔴 NOT another engine's chain either. The two LTX deployments keep separate job stores, and the
 * engine picker sits above a project that has already rendered — so after a switch every row here
 * names a job the current engine has never heard of. `engine` is the row's own record of where its
 * chain lives (see migration `0039`), and a row that cannot state one names nothing.
 *
 * WHICH TAKE: shot 0's SELECTED take, and this reads it without choosing, because `GET …/renders`
 * emits exactly one row per (tier, shot) — the selected one. `first_shot_latent_job_id` says the
 * same thing as a `WHERE … AND engine = $3 AND selected`, and skipping a row with no
 * `latent_job_id` is its `IS NOT NULL`: one rule, stated twice because the client cannot ask the
 * server mid-render. Where shot 0's selected take belongs to another engine BOTH answer nothing,
 * and the server then falls back to that engine's newest chain — a fallback the client has no rows
 * to compute, which is why it keeps the id it holds instead of inventing one.
 *
 * `null` when that row predates the `latent_job_id` column, or belongs to another engine — the
 * caller must then KEEP the id it already holds rather than read the null as "nothing to resume
 * from" and re-render the whole prefix. It is the engine SWITCH, not this function, that clears a
 * stale id.
 */
export function bankedResumeTarget<T extends BankedRowLike>(
  rows: T[],
  tier: RenderTier,
  engine: string
): string | null {
  const row = rows.find(
    (r) => r.tier === tier && r.shot_index === 0 && r.engine === engine && !!r.latent_job_id
  );
  return row?.latent_job_id ?? null;
}

/** What a completed render is allowed to write back into the editor. */
export interface BankAdoption {
  /** The tier's resume target after this completion. */
  resumeJobId: string | null;
  /**
   * May the render's own provenance be adopted — the banked prompt prefix, the v2v prior-render
   * source it offers, and the autosave that makes both durable?
   */
  adopt: boolean;
}

/**
 * Does a render still speak for the project — i.e. is the engine it was DISPATCHED to the one the
 * project is on now?
 *
 * The one question every write-back off a render has to answer, asked in one place because the two
 * sides of a render answer it identically: a completion may not bank an id the project's engine has
 * never held, and a failure may not CLEAR a target belonging to an engine it did not run on. The
 * clear is the cheaper error of the two (a re-rendered prefix rather than a 404 chain), but it is
 * the same error, and it is reachable without touching the picker at all — a doc reload or a
 * harvest can move the project's engine underneath a render that is still polling.
 *
 * `false` for an unknown dispatch engine (a render adopted from another device or from a session
 * that did not record one): unknown is not a match, for the same reason a row with no `engine`
 * names nothing.
 */
export function dispatchIsProjectEngine(
  dispatchEngine: string | null,
  projectEngine: string
): boolean {
  return dispatchEngine !== null && dispatchEngine === projectEngine;
}

/**
 * What a finished render may hand back to the editor, given the engine the project is on NOW.
 *
 * 🔴 A render carries no statement of which engine ran it, and the project's engine is a picker the
 * author may move WHILE it runs. So the completion has to be told: `dispatchEngine` is what the
 * editor sent the job to, and where it no longer matches the project, every id this job can offer
 * names a store the project has stopped using. Adopting one wrote a 2.3 chain id onto a project on
 * 2.5 and persisted it — every shot of the next render a 404 — and nothing downstream could undo it,
 * because `bankedResumeTarget` correctly answers `null` against those rows and `null` never clears.
 *
 * Withholding is the safe half of the trade: the id is recovered from the durable rows by
 * `bankedResumeTarget` on the very next read whenever it IS this engine's, and where it is not, the
 * next render simply re-renders the whole prefix.
 *
 * `null` for an unknown dispatch engine (a render adopted from a device or a session that did not
 * record one) — unknown is not a match, for the same reason a row with no `engine` names nothing.
 */
export function adoptCompletedBank(opts: {
  dispatchEngine: string | null;
  projectEngine: string;
  /** The target this render RESUMED from, kept stable across the chain. */
  pending: string | null;
  /** The engine dir this job banked into (`server_job_id`). */
  banked?: string | null;
  /** What the editor holds now — kept verbatim when nothing may be adopted. */
  held: string | null;
}): BankAdoption {
  if (!dispatchIsProjectEngine(opts.dispatchEngine, opts.projectEngine)) {
    return { resumeJobId: opts.held, adopt: false };
  }
  return { resumeJobId: opts.pending ?? opts.banked ?? opts.held, adopt: true };
}

/** One in-flight (or still-persisting) render, as `/active` reports it. */
export interface ActiveJobLike {
  project_id: string;
  /** True once the render is over and only its harvest into the durable rows is outstanding. */
  harvesting?: boolean;
}

/**
 * Whether a project's durable renders are safe to read.
 *
 * A render that is still RUNNING does not block them — its output is not in the table yet and
 * nothing is being rewritten. What blocks them is the harvest, which the server reports until it
 * lands. The client used to sleep a flat 2s after every completion and hope, which is both slower
 * than needed and not actually a guarantee: a full run's harvest replaces the tier's whole set, and
 * reading it mid-swap showed a half-populated project.
 */
export function harvestSettled(items: ActiveJobLike[], projectId: string): boolean {
  return !items.some((j) => j.project_id === projectId && j.harvesting);
}

/**
 * Does this render's `/media` hold only PART of the timeline — i.e. must it never be adopted as the
 * stitched cut?
 *
 * 🔴 Two unrelated situations, and neither source can answer for both:
 *
 *  - a resume or a retake, whose media is the rendered window. `predicted` covers this one, from
 *    `retakeSplicesTail` at DISPATCH — a poll earlier than any job body, which matters because by
 *    the time the flag lands the fragment would already have replaced a good four-minute cut.
 *  - a full render whose ffmpeg concatenation failed, whose media is segment 0 alone. A RUNTIME
 *    failure, so no capability can predict it: `reported` is the only account of it.
 *
 * Hence the OR, and hence `reported === false` never clears `predicted`. The engines do not all
 * publish the flag — the C++ `ltx-video` engine publishes neither it nor a render window — so a
 * missing or false flag is "nothing to add", never "the dispatcher was wrong".
 */
export function mediaIsFragment(predicted: boolean, reported: boolean | null | undefined): boolean {
  return predicted || reported === true;
}
