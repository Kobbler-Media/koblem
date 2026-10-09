/**
 * Pure geometry for the timeline's pointer drags. Extracted from Timeline.svelte so the rules a
 * touch pan can trip (a reorder that shouldn't move, a trim that shouldn't shrink to nothing) are
 * testable — the component itself only decides whether the result differs from what was there.
 */

/** Index the dragged clip lands on, given the grab point in seconds along the other clips. */
export function reorderTarget(durations: number[], from: number, grabSec: number): number {
  let target = 0;
  let acc = 0;
  for (let k = 0; k < durations.length; k++) {
    if (k === from) continue;
    const mid = acc + Math.max(0.1, durations[k]) / 2;
    if (grabSec > mid) target = k < from ? k + 1 : k;
    acc += Math.max(0.1, durations[k]);
  }
  return target;
}

/** New shot length for a trim handle dragged to `pointerSec`, in tenths and never empty. */
export function trimmedSeconds(clipStartSec: number, pointerSec: number): number {
  return Math.max(0.5, Math.round((pointerSec - clipStartSec) * 10) / 10);
}
