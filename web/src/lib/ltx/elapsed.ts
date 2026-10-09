/**
 * How long a render has ALREADY been going when a client attaches to it.
 *
 * A render started on another device (or before this tab reloaded) is adopted mid-flight, so
 * counting from the attach reports a render that started ten minutes ago as one second old — the
 * progress bar and the ETA under-report for the rest of the run. The job's `created_at` is the
 * only fixed point both devices share, so elapsed is measured from it.
 */

/**
 * Seconds between `createdAt` and `nowMs`, as the baseline a progress timer counts up from.
 *
 * Returns 0 — "count from the attach", the old behaviour — for a render whose start time is
 * unknown (a koblem predating the field) or unparseable.
 *
 * A start time in the FUTURE also gives 0. The timestamp comes from the koblem host's clock and
 * `nowMs` from this browser's; the two are not synchronised, and a skewed clock would otherwise
 * produce a negative elapsed and an ETA that runs backwards. Clamping loses only the skew.
 */
export function adoptedElapsedSec(createdAt: string | null | undefined, nowMs: number): number {
  const startedMs = parseCreatedAt(createdAt);
  if (startedMs === null) return 0;
  return Math.max(0, (nowMs - startedMs) / 1000);
}

/** Epoch ms for a wire RFC3339 timestamp, or null if absent/unparseable. */
export function parseCreatedAt(createdAt: string | null | undefined): number | null {
  if (!createdAt) return null;
  const ms = Date.parse(createdAt);
  return Number.isFinite(ms) ? ms : null;
}
