/**
 * Save-path policy for the Director editor, kept out of the component so the rules that decide
 * whether the user loses work are testable on their own.
 *
 * Two decisions live here:
 *  - where a project-name edit gets persisted from (task: a rename must not ship the timeline), and
 *  - what a rejected write does to the editor (task: a background autosave conflict must not throw
 *    a modal at somebody who did nothing).
 */

/** How a pending project-name edit reaches the server. */
export type RenameRoute =
  /** PATCH the name alone — no timeline in the request, no timeline in the reply. */
  | 'patch'
  /** The timeline is already going to be PUT and that PUT carries the name; don't write twice. */
  | 'with-timeline-save'
  /** Nothing saved yet — the name rides along on the row's first create. */
  | 'local-only';

export function renameRoute(state: {
  projectId: string | null;
  timelineDirty: boolean;
}): RenameRoute {
  if (!state.projectId) return 'local-only';
  return state.timelineDirty ? 'with-timeline-save' : 'patch';
}

/** The document changed under us. `prompt` = ask now; otherwise surface it passively. */
export interface StaleDoc {
  serverVersion: number;
  prompt: boolean;
}

export interface SaveState {
  /** Whether the in-memory doc still differs from what was last written. */
  dirty: boolean;
  /** Whether the debounced background save is still allowed to fire. */
  autosaveArmed: boolean;
  stale: StaleDoc | null;
}

/**
 * Fold a 409 into the editor state.
 *
 * Invariants, in order of how badly getting them wrong hurts:
 *  1. `dirty` stays set — the rejected write is still unsaved work, and nothing here discards it.
 *  2. autosave disarms — retrying a write that cannot succeed just burns requests, and each retry
 *     is another chance to surface noise.
 *  3. only a save the user actually asked for may interrupt them. A background autosave marks the
 *     document stale and waits; the modal belongs to the person who pressed Save.
 *
 * A conflict already being shown is never downgraded from a prompt to a passive notice — an
 * autosave landing after the user hit Save must not dismiss the question they were just asked.
 */
export function onWriteConflict(
  prev: SaveState,
  serverVersion: number,
  userInitiated: boolean
): SaveState {
  return {
    dirty: true,
    autosaveArmed: false,
    stale: { serverVersion, prompt: userInitiated || (prev.stale?.prompt ?? false) }
  };
}
