import { describe, expect, it } from 'vitest';
import { onWriteConflict, renameRoute, type SaveState } from './save';

const clean: SaveState = { dirty: false, autosaveArmed: true, stale: null };

describe('renameRoute', () => {
  it('patches a saved project so the rename never ships the timeline', () => {
    expect(renameRoute({ projectId: 'p1', timelineDirty: false })).toBe('patch');
  });

  it('lets the pending timeline save carry the name when the timeline is dirty', () => {
    // Ordering: the PUT is happening regardless and its body already has `name`, so patching too
    // would be a second write — and a second version bump the pending PUT would then be stale for.
    expect(renameRoute({ projectId: 'p1', timelineDirty: true })).toBe('with-timeline-save');
  });

  it('keeps the name local until the project has a row to patch', () => {
    expect(renameRoute({ projectId: null, timelineDirty: false })).toBe('local-only');
    expect(renameRoute({ projectId: null, timelineDirty: true })).toBe('local-only');
  });
});

describe('onWriteConflict', () => {
  it('prompts only for a save the user asked for', () => {
    expect(onWriteConflict(clean, 9, true)).toEqual({
      dirty: true,
      autosaveArmed: false,
      stale: { serverVersion: 9, prompt: true }
    });
  });

  it('keeps a background autosave conflict passive', () => {
    const next = onWriteConflict({ ...clean, dirty: true }, 9, false);
    expect(next.stale).toEqual({ serverVersion: 9, prompt: false });
    expect(next.autosaveArmed).toBe(false);
  });

  it('never discards the unsaved work it just failed to write', () => {
    for (const userInitiated of [true, false]) {
      expect(onWriteConflict(clean, 2, userInitiated).dirty).toBe(true);
    }
  });

  it('does not let a later autosave dismiss the question the user was asked', () => {
    const prompted = onWriteConflict(clean, 9, true);
    expect(onWriteConflict(prompted, 11, false).stale).toEqual({
      serverVersion: 11,
      prompt: true
    });
  });
});
