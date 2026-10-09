/**
 * Which render a plain (non-Director) video panel attaches to on load.
 *
 * These panels have no project identity — one global render per engine — so the question is only
 * "is anything rendering on this engine?", and the answer is the newest render that has not
 * finished. That is what makes a render started on another device visible here: the server's job
 * list is the authority, and `localStorage` is a fast path for this device's own render, not the
 * source of truth.
 */

import { parseCreatedAt } from './elapsed';

/** The fields of a `…/jobs` list entry this decision needs. */
export interface VideoJobLike {
  job_id: string;
  status: string;
  /** RFC3339 submit time. Absent from a koblem predating the field. */
  created_at?: string | null;
}

export interface ActiveJobPlan {
  /** Job to attach the poll loop to, or null for "show an idle panel". */
  adopt: string | null;
  /** Whether `adopt` is still rendering — drives the busy UI before the first poll lands. */
  running: boolean;
  /** The remembered id names no job the server holds: drop the key. */
  forget: boolean;
  /**
   * When `adopt` was submitted, so the panel's progress timer starts at the render's true age
   * instead of at this attach. Null when the server did not say (or nothing was adopted).
   */
  createdAt: string | null;
}

const TERMINAL = new Set(['completed', 'failed', 'cancelled']);

/**
 * Newest of the unfinished renders. `created_at` decides it wherever both candidates carry one;
 * otherwise the list order does, which the koblem endpoints already serve newest-first
 * (`LtxJobStore::list` and `WanJobStore::list` sort by `Reverse(created)`). Keeping the order as
 * the fallback is what lets a koblem predating `created_at` still answer this correctly.
 */
function newestLive(jobs: VideoJobLike[]): VideoJobLike | null {
  let best: VideoJobLike | null = null;
  let bestMs: number | null = null;
  for (const j of jobs) {
    if (TERMINAL.has(j.status)) continue;
    const ms = parseCreatedAt(j.created_at);
    if (!best) {
      best = j;
      bestMs = ms;
      continue;
    }
    if (ms !== null && bestMs !== null && ms > bestMs) {
      best = j;
      bestMs = ms;
    }
  }
  return best;
}

/**
 * Pass `jobs = null` when the list could not be fetched: the remembered id is then the only thing
 * left to go on, and the poll loop corrects (or gives up on) it from there.
 *
 * Relies on non-terminal jobs never being swept from the store (both stores' TTL and MAX_JOBS
 * branches filter on `is_terminal()`), so an unfinished render is always still there to be found.
 */
export function planActiveJob(
  remembered: string | null,
  jobs: VideoJobLike[] | null
): ActiveJobPlan {
  const id = remembered?.trim() || null;
  if (!jobs) return { adopt: id, running: !!id, forget: false, createdAt: null };
  const mine = jobs.find((j) => j.job_id === id) ?? null;
  const attach = (j: VideoJobLike, running: boolean): ActiveJobPlan => ({
    adopt: j.job_id,
    running,
    forget: false,
    createdAt: j.created_at ?? null
  });
  if (mine && !TERMINAL.has(mine.status)) return attach(mine, true);
  const live = newestLive(jobs);
  if (live) return attach(live, true);
  // Nothing is rendering. A remembered render that finished is still worth attaching to — that is
  // how a reload right after a render re-shows its clip. Someone else's finished render is not.
  if (mine) return attach(mine, false);
  return { adopt: null, running: false, forget: !!id, createdAt: null };
}
