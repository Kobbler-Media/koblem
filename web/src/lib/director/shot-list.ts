/**
 * Pure rules for a shot AS A LIST ROW: its render state, the gist that labels it, and the ↑/↓
 * reorder the phone shell offers where the desktop drags on a time axis.
 *
 * Shared by both shells — `MobileShots` and the desktop's `Timeline` / `TimelineBar` / `RailDock` /
 * `StageViewer` all read `ShotRenderStatus` from here — which is the point of it being pure.
 */

export type ShotRenderStatus = 'ok' | 'stale' | 'none' | 'busy';

/**
 * One ↑/↓ step, or `null` when the move would change nothing.
 *
 * The null is the point: the caller marks the project dirty only on a non-null result, so a tap on
 * an edge button never schedules an autosave — and, under optimistic concurrency, never turns a
 * wasted write into a 409 conflict banner.
 */
export function moveShot<T>(items: T[], from: number, delta: number): T[] | null {
  const to = from + delta;
  if (delta === 0) return null;
  if (from < 0 || from >= items.length) return null;
  if (to < 0 || to >= items.length) return null;
  const next = items.slice();
  const [moved] = next.splice(from, 1);
  next.splice(to, 0, moved);
  return next;
}

/** The one line a shot shows when there is no room for its prompt. */
export function shotGist(c: {
  gist?: string | null;
  idea?: string | null;
  prompt: string;
}): string {
  return c.gist?.trim() || c.idea?.trim() || c.prompt.trim() || '—';
}

/** Render state in words. A phone row has space for the wording and no legend to look a dot up in. */
export function shotStateLabel(status: ShotRenderStatus): string {
  switch (status) {
    case 'ok':
      return 'rendered';
    case 'stale':
      return 'edited since';
    case 'busy':
      return 'rendering';
    default:
      return 'not rendered';
  }
}
