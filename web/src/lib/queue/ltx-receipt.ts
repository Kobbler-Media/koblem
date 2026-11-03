import type { LtxJobState } from '#lib/api/client.ts';
import type { EnqueueLike } from './options';

/**
 * 🔴 The LTX + Director submits answer with `{ job_id, status }` and nothing else — no `position`,
 * no `eta_start`, no `blocked_reason` — so their receipt is narrower than the eleven routes that
 * return the full `EnqueueResponse`. It still states what HAPPENED rather than what will, which is
 * the property the receipt exists for; the position is missing from the wire, not dropped here.
 * `api/src/routes/ltx_video.rs` (`VideoJobSubmitResponse`) and `ltx_director.rs`'s `/run` are where
 * it would have to be added.
 */
export function ltxReceipt(status: LtxJobState | 'up_to_date' | undefined): EnqueueLike {
  return { state: status === 'rendering' ? 'running' : 'queued' };
}
