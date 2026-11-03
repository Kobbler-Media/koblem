import type { Job, JobEvent } from '#lib/api/client.ts';
import { blockedSummary, humanMsShort } from './format';

export interface NotifyMessage {
  title: string;
  body: string;
  /// Browsers collapse same-tag notifications, so a repeated frame for a
  /// transition already announced replaces rather than stacks.
  tag: string;
}

/**
 * 🔴 Terminal transitions only. An H3 job emits per-generation progress and a
 * notification per generation is sixteen interruptions for one render, so
 * anything short of done/failed/cancelled is deliberately silent.
 */
export function terminalMessage(event: JobEvent, label?: string): NotifyMessage | null {
  const what = label?.trim() || `${event.kind} · ${event.job_id}`;
  const tag = `queue:${event.job_id}:${event.state}`;

  switch (event.state) {
    case 'failed':
      return {
        title: `Render failed — ${what}`,
        // The error IS the notification. "A job failed" sends someone to a page
        // to read one sentence they could have had on the lock screen.
        body: event.error?.trim() || 'the engine gave no reason',
        tag
      };
    case 'cancelled':
      return { title: `Render cancelled — ${what}`, body: 'nothing was produced', tag };
    case 'done': {
      const warnings = event.warnings.filter((w) => w.trim());
      if (warnings.length === 0)
        return { title: `Render done — ${what}`, body: 'no warnings', tag };
      // 🔴 A success that silently dropped a generation looks exactly like never
      // having pressed render, so the warning travels with the good news.
      return {
        title: `Render done with ${warnings.length} warning${warnings.length === 1 ? '' : 's'} — ${what}`,
        body: warnings.join('\n'),
        tag
      };
    }
    default:
      return null;
  }
}

/** The server already applied the stuck threshold; this only says it out loud. */
export function stuckMessage(job: Job, nowMs: number): NotifyMessage {
  const waited = job.blocked_since ? Date.parse(job.blocked_since) : NaN;
  const forHow = Number.isFinite(waited) ? ` for ${humanMsShort(Math.max(0, nowMs - waited))}` : '';
  const why = job.blocked_reason ? blockedSummary(job.blocked_reason) : 'blocked';
  return {
    title: `Queue blocked — ${job.label?.trim() || job.kind}`,
    body: `${why}${forHow}`,
    tag: `queue:${job.id}:blocked`
  };
}
