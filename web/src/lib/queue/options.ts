import type { BlockedReason, JobKind, JobState, QueueSnapshot } from '#lib/api/client.ts';
import { batchEta, blockedSummary, clockOf, isPending } from './format';

export type SubmitMode = 'queue' | 'now';

export interface QueueSubmitOptions {
  mode?: SubmitMode;
  band?: number;
  gpu?: string;
  force?: boolean;
  after?: string;
}

/**
 * 🔴 An unset field sends NO header. Absence means "the default for this caller",
 * which the server computes from the principal — sending `X-Queue-Mode: queue`
 * because that happens to be today's web default would pin the form to a value the
 * server is supposed to own.
 */
export function queueHeaders(opts?: QueueSubmitOptions | null): Record<string, string> {
  const headers: Record<string, string> = {};
  if (!opts) return headers;
  if (opts.mode) headers['X-Queue-Mode'] = opts.mode;
  if (opts.band != null) headers['X-Queue-Band'] = String(opts.band);
  if (opts.gpu) {
    headers['X-Queue-Gpu'] = opts.gpu;
    // Force has nothing to force without a pin, and the server reads it as a
    // pin modifier — sending it alone would be noise in the access log.
    if (opts.force) headers['X-Queue-Force'] = '1';
  }
  if (opts.after) headers['X-Queue-After'] = opts.after;
  return headers;
}

export function hasQueueOverrides(opts: QueueSubmitOptions): boolean {
  return Object.keys(queueHeaders(opts)).length > 0;
}

/**
 * 🔴 Mirrors `enqueue()`'s `band.unwrap_or(match mode …)` in api/src/routes/queue.rs.
 * A web session defaults to `queue`, so an unset band is 1 unless run-now moved it
 * to 0. Only used for the pre-submit guess; the 202 is the authority.
 */
export function effectiveBand(opts: QueueSubmitOptions): number {
  if (opts.band != null) return opts.band;
  return opts.mode === 'now' ? 0 : 1;
}

export interface ConsequenceView {
  text: string;
  attention: boolean;
  /**
   * The same answer in one or two words, for the chip that sits beside the render button.
   *
   * 🔴 Derived HERE rather than by trimming {@link text} at the call site. The sentence is the
   * thing that has to stay readable — "queues at #3 · starts ~14:20" is worth its width in a
   * tooltip and worthless in a toolbar — and two components abbreviating it their own way is how
   * the same queue state ends up spelled three ways on one screen.
   */
  short: string;
  /** True when this job would go straight onto the GPU, which is the one state worth an icon. */
  immediate: boolean;
}

/**
 * 🔴 The line that sits NEXT TO the render button, never on it. Whether a job runs
 * now is a prediction, and a verb that flips "Render" → "Queue" under the cursor
 * races the snapshot it was computed from.
 */
export function submitConsequence(
  snapshot: QueueSnapshot,
  kind: JobKind,
  opts: QueueSubmitOptions
): ConsequenceView {
  // 🔴 Before the same-kind probe, because that probe cannot see this. Beefy
  // refuses EVERY kind, so on a box that has been locked since before this
  // session there is no blocked row of this kind to learn it from — the first
  // render of the evening would read "starts now" and then sit there.
  if (snapshot.box_locked) {
    return {
      text: `queues · ${blockedSummary({ reason: 'box_locked', service: snapshot.box_locked })}`,
      attention: true,
      short: 'beefy',
      immediate: false
    };
  }
  const sameKindBlocked = snapshot.jobs.find(
    (j) => j.kind === kind && isPending(j.state) && j.blocked_reason
  );
  if (sameKindBlocked?.blocked_reason) {
    return {
      text: `queues · ${blockedSummary(sameKindBlocked.blocked_reason)}`,
      attention: true,
      short: 'blocked',
      immediate: false
    };
  }
  if (snapshot.paused) {
    return {
      text: 'queues · the queue is paused',
      attention: true,
      short: 'paused',
      immediate: false
    };
  }

  if (opts.after) {
    const parent = snapshot.jobs.find((j) => j.id === opts.after);
    return {
      text: `queues after ${parent?.label ?? opts.after}`,
      attention: false,
      short: 'chained',
      immediate: false
    };
  }

  const band = effectiveBand(opts);
  const pending = snapshot.jobs.filter((j) => isPending(j.state));
  const ahead = pending.filter((j) => j.band <= band).length;
  // 🔴 "Starts now" needs an EMPTY queue, not merely an empty band. Rows waiting
  // behind a lower band are waiting on something, and a band-0 submit that jumps
  // them still has to get past whatever is holding them.
  if (pending.length === 0 && snapshot.running === 0) {
    return { text: 'starts now', attention: false, short: 'now', immediate: true };
  }

  // The batch ETA is this job's start only when nothing pending sorts after it;
  // anywhere else it is a clock read off the wrong tail.
  const isTail = pending.every((j) => j.band <= band);
  const at = isTail ? (batchEta(snapshot)?.at ?? null) : null;
  return {
    text: `queues at #${ahead + 1}${at ? ` · starts ~${at}` : ''}`,
    attention: false,
    short: `#${ahead + 1}`,
    immediate: false
  };
}

/// `EnqueueResponse` plus the narrower accepts (pixal3d's rig/motion) that carry
/// the position without the rest of the envelope.
export interface EnqueueLike {
  state?: JobState;
  position?: number | null;
  eta_start?: string | null;
  blocked_reason?: BlockedReason | null;
  deduplicated?: boolean;
}

/// What actually happened, off the 202 — the only non-racy signal there is.
export function enqueueReceipt(res: EnqueueLike): ConsequenceView {
  const verb = res.deduplicated ? 'already queued' : 'queued';
  if (res.blocked_reason) {
    return {
      text: `${verb} · ${blockedSummary(res.blocked_reason)}`,
      attention: true,
      short: 'blocked',
      immediate: false
    };
  }
  if (res.state === 'running' || res.state === 'collecting') {
    return {
      text: res.deduplicated ? 'already running' : 'started',
      attention: false,
      short: 'running',
      immediate: true
    };
  }
  if (res.state === 'held') {
    return { text: `${verb} · held`, attention: true, short: 'held', immediate: false };
  }
  const where = res.position != null ? `${verb} at #${res.position}` : verb;
  const at = clockOf(res.eta_start);
  return {
    text: at ? `${where}, starts ~${at}` : where,
    attention: false,
    short: res.position != null ? `#${res.position}` : 'queued',
    immediate: false
  };
}
