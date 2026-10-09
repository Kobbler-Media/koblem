import type { BlockedReason, Job, JobState, QueueSnapshot } from '$lib/api/client';

/** Same shape the CLI prints, so a screenshot and a terminal never disagree. */
export function humanMs(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

/** Coarser form for the title bar, where "4h 12m" is longer than the space. */
export function humanMsShort(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  return `${Math.floor(s / 3600)}h`;
}

export function elapsedMs(since: string | null | undefined, nowMs: number): number | null {
  if (!since) return null;
  const at = Date.parse(since);
  if (!Number.isFinite(at)) return null;
  return Math.max(0, nowMs - at);
}

/** Mirrors `BlockedReason::summary()`. */
export function blockedSummary(reason: BlockedReason): string {
  switch (reason.reason) {
    case 'service_disabled':
      return `${reason.service} is disabled`;
    case 'no_card_fits':
      return reason.detail;
    case 'lane_busy':
      return `${reason.lane} slot busy`;
    case 'eviction_cooldown':
      return 'an eviction is settling';
    case 'dependency_pending':
      return `waiting on ${reason.parent}`;
    case 'engine_unreachable':
      return `engine unreachable: ${reason.detail}`;
    case 'head_reserved':
      return `holding resources for ${reason.head}`;
    case 'paused':
      return 'the queue is paused';
    case 'box_locked':
      return `the box is held by Beefy LLM (${reason.service})`;
  }
}

/** Local wall-clock `HH:MM` for a wire timestamp, or null if absent/unparseable. */
export function clockOf(iso: string | null | undefined): string | null {
  if (!iso) return null;
  const at = Date.parse(iso);
  if (!Number.isFinite(at)) return null;
  return new Date(at).toLocaleTimeString(undefined, {
    hour: '2-digit',
    minute: '2-digit',
    hour12: false
  });
}

export interface EtaView {
  starts: string | null;
  finishes: string | null;
  samples: number;
}

/**
 * 🔴 A zero-sample estimate is not an ETA. The server omits both times in that
 * case; this refuses to render one anyway, and carries the sample count so the
 * page can never show a confident-looking clock built from three runs.
 */
export function jobEta(job: Job): EtaView | null {
  if (job.estimate_samples <= 0) return null;
  const starts = clockOf(job.eta_start);
  const finishes = clockOf(job.eta_finish);
  if (!starts && !finishes) return null;
  return { starts, finishes, samples: job.estimate_samples };
}

export interface BatchEta {
  at: string;
  /** Pending rows the estimator has never measured. They are NOT in the number. */
  unestimated: number;
}

export function batchEta(snapshot: QueueSnapshot): BatchEta | null {
  const measured = snapshot.jobs.some((j) => j.estimate_samples > 0);
  const at = clockOf(snapshot.eta_finish);
  if (!measured || !at) return null;
  return {
    at,
    unestimated: snapshot.jobs.filter((j) => isPending(j.state) && j.estimate_samples <= 0).length
  };
}

/** "2/16", or null when the engine publishes no unit count. */
export function unitsLabel(job: Job): string | null {
  if (job.units_total != null && job.units_total > 0) return `${job.units_done}/${job.units_total}`;
  const { current_step, total_steps } = job.progress;
  if (current_step != null && total_steps != null && total_steps > 0) {
    return `${current_step}/${total_steps}`;
  }
  return null;
}

export function isPending(state: JobState): boolean {
  return state === 'queued' || state === 'held';
}

export function isLive(state: JobState): boolean {
  return state === 'running' || state === 'collecting';
}

export function isTerminal(state: JobState): boolean {
  return state === 'done' || state === 'failed' || state === 'cancelled';
}

const BAND_NAMES: Record<number, string> = { 0: 'interactive', 1: 'normal', 2: 'background' };

export function bandLabel(band: number): string {
  const name = BAND_NAMES[band];
  return name ? `${band} · ${name}` : `band ${band}`;
}

export const BANDS = [0, 1, 2];

/**
 * The `after` id for a job dropped at `index` of `order` — the row it should now
 * follow, or null for the head of the band. `order` is the post-drop arrangement.
 */
export function reorderAfterId(order: string[], index: number): string | null {
  if (index <= 0) return null;
  return order[index - 1] ?? null;
}

export interface IndicatorView {
  /**
   * `blocked` is the overnight failure mode: stalled on something clearable.
   * `beefy` is the deliberate operator state that CAUSES it — the box is handed
   * to the 27B and nothing else can be admitted at all.
   */
  mode: 'idle' | 'active' | 'blocked' | 'beefy';
  text: string;
  title: string;
}

/**
 * The title-bar line. Always resolves to something clickable — with nothing in
 * flight it is a nav entry and nothing louder. The blocked shape wins outright
 * over the active one: a running job is reassuring noise next to five rows that
 * never started.
 *
 * 🔴 And Beefy wins over everything, including idle. It is the only state where
 * an EMPTY queue is the alarming reading: nothing is running because nothing
 * CAN, and the sidebar banner that says so is one collapse away from invisible.
 */
export function indicatorState(snapshot: QueueSnapshot, nowMs: number): IndicatorView {
  const active = snapshot.pending + snapshot.running;
  if (snapshot.box_locked) {
    const waiting = active > 0 ? ` · ${active} waiting` : '';
    return {
      mode: 'beefy',
      text: `⚡ BEEFY LLM${waiting}`,
      title:
        `The box is handed to the big LLM (${snapshot.box_locked}) — no queued GPU work can ` +
        `start until it is turned off. Queued jobs are waiting, not cancelled.`
    };
  }
  if (snapshot.stuck.length === 0 && active === 0) {
    return { mode: 'idle', text: '⛁ Queue', title: 'GPU queue — nothing running' };
  }
  const count = `⛁ ${active}`;

  if (snapshot.stuck.length > 0) {
    const stuck = snapshot.jobs.filter((j) => snapshot.stuck.includes(j.id));
    const worst = stuck.reduce<{ job: Job; ms: number } | null>((acc, job) => {
      const ms = elapsedMs(job.blocked_since, nowMs) ?? 0;
      return !acc || ms > acc.ms ? { job, ms } : acc;
    }, null);
    const why = worst?.job.blocked_reason ? blockedSummary(worst.job.blocked_reason) : 'blocked';
    const forHow = worst ? ` ${humanMsShort(worst.ms)}` : '';
    return {
      mode: 'blocked',
      text: `${count} · ${snapshot.stuck.length} BLOCKED${forHow} · ${why}`,
      title: `${snapshot.stuck.length} job(s) blocked on something you can clear — open the queue`
    };
  }

  const live = snapshot.jobs.find((j) => isLive(j.state));
  const parts = [count];
  if (live) {
    const units = unitsLabel(live);
    const verb = live.state === 'collecting' ? 'collecting' : 'rendering';
    parts.push(`${verb} ${live.kind}${units ? ` (${units})` : ''}`);
  } else if (snapshot.paused) {
    parts.push('paused');
  } else {
    parts.push(`${snapshot.pending} queued`);
  }
  const eta = batchEta(snapshot);
  if (eta) parts.push(`done ~${eta.at}`);
  return {
    mode: 'active',
    text: parts.join(' · '),
    title: `${snapshot.running} running, ${snapshot.pending} queued — open the queue`
  };
}
