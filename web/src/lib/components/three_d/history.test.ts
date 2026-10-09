import { describe, expect, it, vi } from 'vitest';
import { DEFAULT_MAX_BYTES, DEFAULT_MAX_DEPTH, EditHistory, type HistoryEntry } from './history';

/**
 * A step that writes its own name into a shared log whenever it is applied or reversed, so a test
 * asserts on the ORDER the closures actually ran in rather than on the stack's bookkeeping.
 */
function step(log: string[], name: string, extra: Partial<HistoryEntry> = {}): HistoryEntry {
  return {
    label: name,
    undo: () => log.push(`-${name}`),
    redo: () => log.push(`+${name}`),
    ...extra
  };
}

const noop = (): void => {};

describe('EditHistory push / undo / redo', () => {
  it('reverses steps newest first and re-applies them oldest first', () => {
    const log: string[] = [];
    const h = new EditHistory();
    h.push(step(log, 'a'));
    h.push(step(log, 'b'));
    h.push(step(log, 'c'));

    expect(h.size).toBe(3);
    expect(h.undoLabel).toBe('c');
    expect(h.redoLabel).toBeNull();

    expect(h.undo()).toBe('c');
    expect(h.undo()).toBe('b');
    expect(log).toEqual(['-c', '-b']);
    expect(h.undoLabel).toBe('a');
    expect(h.redoLabel).toBe('b');

    expect(h.redo()).toBe('b');
    expect(h.redo()).toBe('c');
    expect(log).toEqual(['-c', '-b', '+b', '+c']);
    expect(h.canRedo).toBe(false);
  });

  it('offers nothing on an empty stack rather than throwing', () => {
    const h = new EditHistory();
    expect(h.canUndo).toBe(false);
    expect(h.canRedo).toBe(false);
    expect(h.undo()).toBeNull();
    expect(h.redo()).toBeNull();
    expect(h.undoLabel).toBeNull();
    expect(h.list()).toEqual({ cursor: 0, entries: [] });
  });

  it('stops at the bottom instead of walking off it', () => {
    const log: string[] = [];
    const h = new EditHistory();
    h.push(step(log, 'a'));
    expect(h.undo()).toBe('a');
    expect(h.undo()).toBeNull();
    expect(log).toEqual(['-a']);
  });

  it('bumps the revision on every mutation, since that is the only reactivity hook', () => {
    const h = new EditHistory();
    const seen: number[] = [];
    h.push(step([], 'a'));
    seen.push(h.revision);
    h.undo();
    seen.push(h.revision);
    h.redo();
    seen.push(h.revision);
    h.clear();
    seen.push(h.revision);
    expect(seen).toEqual([...seen].sort((x, y) => x - y));
    expect(new Set(seen).size).toBe(4);
  });

  it('reports the cursor and which rows are still in effect', () => {
    const h = new EditHistory();
    h.push(step([], 'a', { weight: 10 }));
    h.push(step([], 'b'));
    h.undo();

    const view = h.list();
    expect(view.cursor).toBe(1);
    expect(view.entries.map((e) => [e.label, e.applied, e.kind, e.bytes])).toEqual([
      ['a', true, 'edit', 10],
      ['b', false, 'edit', 0]
    ]);
    // Ids are stable across the entry's life even though the row order is not.
    expect(new Set(view.entries.map((e) => e.id)).size).toBe(2);
  });
});

describe('EditHistory redo tail', () => {
  /**
   * 🔴 A redo tail describes a future the new edit has just made unreachable. Replaying it would
   * apply a closure captured over a state that no longer exists — silent model corruption, not an
   * error.
   */
  it('discards the redo tail on a new push', () => {
    const log: string[] = [];
    const h = new EditHistory();
    h.push(step(log, 'a'));
    h.push(step(log, 'b'));
    h.undo();
    h.push(step(log, 'c'));

    expect(h.canRedo).toBe(false);
    expect(h.size).toBe(2);
    expect(h.list().entries.map((e) => e.label)).toEqual(['a', 'c']);
    expect(h.undo()).toBe('c');
    expect(h.undo()).toBe('a');
    expect(log).toEqual(['-b', '-c', '-a']);
  });

  /** The discarded entries are the ones most likely to be holding a megabyte each. */
  it('releases the discarded tail: dispose runs and the bytes come back', () => {
    const dispose = vi.fn();
    const h = new EditHistory();
    h.push(step([], 'a', { weight: 100 }));
    h.push(step([], 'b', { weight: 1_000_000, dispose }));
    expect(h.heldBytes).toBe(1_000_100);

    h.undo();
    h.push(step([], 'c', { weight: 5 }));
    expect(dispose).toHaveBeenCalledTimes(1);
    expect(h.heldBytes).toBe(105);
  });

  /** A throwing dispose must not abort the drop — the slot is gone either way. */
  it('survives a dispose that throws', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(noop);
    const h = new EditHistory();
    h.push(
      step([], 'a', {
        weight: 40,
        dispose: () => {
          throw new Error('texture already gone');
        }
      })
    );
    expect(() => h.clear()).not.toThrow();
    expect(h.size).toBe(0);
    expect(h.heldBytes).toBe(0);
    warn.mockRestore();
  });
});

describe('EditHistory coalescing', () => {
  /**
   * A joint drag fires on every pointermove, so one gesture is ~200 pushes. The merged entry must
   * reverse the WHOLE run: the first push's undo (the state before the drag started) against the
   * last push's redo (where the drag ended).
   */
  it('collapses a 200-event drag into one entry that reverses the whole run', () => {
    const log: string[] = [];
    const h = new EditHistory();
    for (let i = 0; i < 200; i++) {
      h.push({
        label: `Moved Hips ${i}`,
        undo: () => log.push(`undo@${i}`),
        redo: () => log.push(`redo@${i}`),
        coalesce: 'move:Hips'
      });
    }

    expect(h.size).toBe(1);
    // The newest label, because it describes the run so far, not a single pointermove.
    expect(h.undoLabel).toBe('Moved Hips 199');

    h.undo();
    h.redo();
    expect(log).toEqual(['undo@0', 'redo@199']);
  });

  it('keys the run to the gesture, so dragging a second joint is a second step', () => {
    const log: string[] = [];
    const h = new EditHistory();
    h.push(step(log, 'hips1', { coalesce: 'move:Hips' }));
    h.push(step(log, 'hips2', { coalesce: 'move:Hips' }));
    h.push(step(log, 'arm1', { coalesce: 'move:LeftArm' }));
    h.push(step(log, 'arm2', { coalesce: 'move:LeftArm' }));

    expect(h.size).toBe(2);
    expect(h.list().entries.map((e) => e.label)).toEqual(['hips2', 'arm2']);
    h.undo();
    h.undo();
    expect(log).toEqual(['-arm1', '-hips1']);
  });

  it('never merges entries that declare no key', () => {
    const h = new EditHistory();
    h.push(step([], 'a'));
    h.push(step([], 'b'));
    h.push(step([], 'c', { coalesce: null }));
    expect(h.size).toBe(3);
  });

  /**
   * 🔴 "Consecutive" means consecutive PUSHES. An undo/redo in between moved the world through a
   * state the open run never saw, so extending it would give one entry an undo and a redo captured
   * either side of an unrelated cursor move.
   */
  it('breaks the run on an undo/redo in between, even for the same key', () => {
    const log: string[] = [];
    const h = new EditHistory();
    h.push(step(log, 'a', { coalesce: 'move:Hips' }));
    h.push(step(log, 'b', { coalesce: 'move:Hips' }));
    expect(h.size).toBe(1);

    h.undo();
    h.redo();
    h.push(step(log, 'c', { coalesce: 'move:Hips' }));

    expect(h.size).toBe(2);
    expect(h.undo()).toBe('c');
    expect(h.undo()).toBe('b');
    expect(log).toEqual(['-a', '+b', '-c', '-a']);
  });

  it('breaks the run on a barrier, so a barrier can never be extended into', () => {
    const h = new EditHistory();
    h.push(step([], 'a', { coalesce: 'k' }));
    h.pushBarrier('Compacted vertices', 'renumbered');
    h.push(step([], 'b', { coalesce: 'k' }));
    h.push(step([], 'c', { coalesce: 'k' }));
    // b opened a fresh run (a is gone), c merged into b.
    expect(h.list().entries.map((e) => e.kind)).toEqual(['barrier', 'edit']);
  });

  it('breaks the run on a clear', () => {
    const h = new EditHistory();
    h.push(step([], 'a', { coalesce: 'k' }));
    h.clear();
    h.push(step([], 'b', { coalesce: 'k' }));
    h.push(step([], 'c', { coalesce: 'k' }));
    expect(h.size).toBe(1);
    expect(h.undoLabel).toBe('c');
  });

  /** A merged run holds two captures — the head's and the tail's — and must account for both. */
  it('charges the budget for the head and the current tail only', () => {
    const h = new EditHistory();
    h.push(step([], 'a', { coalesce: 'k', weight: 100 }));
    expect(h.heldBytes).toBe(100);
    h.push(step([], 'b', { coalesce: 'k', weight: 50 }));
    expect(h.heldBytes).toBe(150);
    h.push(step([], 'c', { coalesce: 'k', weight: 7 }));
    expect(h.heldBytes).toBe(107);
    expect(h.list().entries[0].bytes).toBe(107);
  });

  it('releases the superseded tail as the run advances, and keeps the head alive', () => {
    const d1 = vi.fn();
    const d2 = vi.fn();
    const d3 = vi.fn();
    const h = new EditHistory();
    h.push(step([], 'a', { coalesce: 'k', dispose: d1 }));
    h.push(step([], 'b', { coalesce: 'k', dispose: d2 }));
    expect(d2).not.toHaveBeenCalled();
    h.push(step([], 'c', { coalesce: 'k', dispose: d3 }));

    // b's capture is unreachable the moment its redo was replaced; a's is still the run's undo.
    expect(d2).toHaveBeenCalledTimes(1);
    expect(d1).not.toHaveBeenCalled();
    expect(d3).not.toHaveBeenCalled();

    h.clear();
    expect(d1).toHaveBeenCalledTimes(1);
    expect(d3).toHaveBeenCalledTimes(1);
  });

  /** ⚠️ A caller may hand the SAME dispose closure to every push in a run; the head's copy is live. */
  it('does not dispose a shared closure while the head still holds it', () => {
    const dispose = vi.fn();
    const h = new EditHistory();
    for (let i = 0; i < 5; i++) h.push(step([], `p${i}`, { coalesce: 'k', dispose }));
    expect(dispose).not.toHaveBeenCalled();
    h.clear();
    expect(dispose).toHaveBeenCalledTimes(1);
  });
});

describe('EditHistory eviction', () => {
  it('evicts the oldest step over the depth cap', () => {
    const log: string[] = [];
    const h = new EditHistory({ maxDepth: 3 });
    for (const name of ['a', 'b', 'c', 'd', 'e']) h.push(step(log, name));

    expect(h.size).toBe(3);
    expect(h.list().entries.map((e) => e.label)).toEqual(['c', 'd', 'e']);
    expect(h.list().cursor).toBe(3);
  });

  it('evicts the oldest step over the byte cap', () => {
    const h = new EditHistory({ maxBytes: 250 });
    h.push(step([], 'a', { weight: 100 }));
    h.push(step([], 'b', { weight: 100 }));
    h.push(step([], 'c', { weight: 100 }));
    expect(h.size).toBe(2);
    expect(h.heldBytes).toBe(200);
    expect(h.list().entries.map((e) => e.label)).toEqual(['b', 'c']);
  });

  it('releases what it evicts', () => {
    const dispose = vi.fn();
    const h = new EditHistory({ maxDepth: 1 });
    h.push(step([], 'a', { weight: 4096, dispose }));
    h.push(step([], 'b'));
    expect(dispose).toHaveBeenCalledTimes(1);
    expect(h.heldBytes).toBe(0);
  });

  /**
   * 🔴 An evicted step is GONE and the stack has to say so. `canUndo`, `size` and the list shrink
   * together, so the UI is never offered an undo that would silently do nothing — or worse, run a
   * closure over a state the surviving neighbours have already moved past.
   */
  it('never offers an undo for a step it evicted', () => {
    const log: string[] = [];
    const h = new EditHistory({ maxDepth: 2 });
    for (const name of ['a', 'b', 'c', 'd']) h.push(step(log, name));

    const reversed: string[] = [];
    for (;;) {
      const label = h.undo();
      if (label === null) break;
      reversed.push(label);
    }
    expect(reversed).toEqual(['d', 'c']);
    expect(log).toEqual(['-d', '-c']);
    expect(h.canUndo).toBe(false);
  });

  /**
   * ⚠️ The one deliberate overrun: a `maxBytes` smaller than a single 16 MB texture copy holds one
   * over-budget entry rather than leaving the user's most recent edit unundoable. Bounded by the
   * largest single entry, never by the session's length.
   */
  it('keeps the last surviving edit even when it alone blows the budget', () => {
    const h = new EditHistory({ maxBytes: 1 });
    h.push(step([], 'huge', { weight: 16 * 1024 * 1024 }));
    expect(h.size).toBe(1);
    expect(h.canUndo).toBe(true);

    h.push(step([], 'huger', { weight: 32 * 1024 * 1024 }));
    expect(h.size).toBe(1);
    expect(h.undoLabel).toBe('huger');
    expect(h.heldBytes).toBe(32 * 1024 * 1024);
  });

  /**
   * ⚠️ The barrier costs a row and no bytes, and it is the only record of why the history beneath
   * it is missing — evicting it would leave a stack that looks like an ordinary clean one.
   */
  it('never evicts the barrier to make room', () => {
    const h = new EditHistory({ maxDepth: 2 });
    h.pushBarrier('Compacted vertices', 'Vertex numbering changed.');
    h.push(step([], 'a'));
    h.push(step([], 'b'));
    h.push(step([], 'c'));

    const view = h.list();
    expect(view.entries.map((e) => e.kind)).toEqual(['barrier', 'edit']);
    expect(view.entries[0].reason).toBe('Vertex numbering changed.');
    expect(view.entries[1].label).toBe('c');
    expect(view.cursor).toBe(2);
    expect(h.undoLabel).toBe('c');
  });

  it('ignores a weight that is negative, NaN or absent rather than evicting for arithmetic', () => {
    const h = new EditHistory({ maxBytes: 10 });
    h.push(step([], 'a', { weight: Number.NaN }));
    h.push(step([], 'b', { weight: -1000 }));
    h.push(step([], 'c'));
    expect(h.size).toBe(3);
    expect(h.heldBytes).toBe(0);
  });

  it('clamps absurd options rather than trusting them', () => {
    expect(new EditHistory({ maxDepth: 0 }).maxDepth).toBe(1);
    expect(new EditHistory({ maxDepth: -5 }).maxDepth).toBe(1);
    expect(new EditHistory({ maxBytes: -1 }).maxBytes).toBe(0);
    const d = new EditHistory();
    expect(d.maxDepth).toBe(DEFAULT_MAX_DEPTH);
    expect(d.maxBytes).toBe(DEFAULT_MAX_BYTES);
  });
});

describe('EditHistory barrier', () => {
  /**
   * 🔴 `compactVertices` renumbers every vertex, so every entry captured before it addresses a
   * numbering that no longer exists. Undoing one would not throw — it would draw triangles between
   * unrelated vertices. A barrier costs the user their history; an entry would cost them their
   * model.
   */
  it('releases everything below it and stops canUndo', () => {
    const log: string[] = [];
    const dispose = vi.fn();
    const h = new EditHistory();
    h.push(step(log, 'a', { weight: 1000, dispose }));
    h.push(step(log, 'b', { weight: 1000 }));

    h.pushBarrier('Compacted vertices', 'Vertex numbering changed, so earlier steps went.');

    expect(dispose).toHaveBeenCalledTimes(1);
    expect(h.heldBytes).toBe(0);
    expect(h.canUndo).toBe(false);
    expect(h.canRedo).toBe(false);
    expect(h.undo()).toBeNull();
    expect(log).toEqual([]);
  });

  it('stays in the list carrying the reason, so the history does not silently evaporate', () => {
    const h = new EditHistory();
    h.push(step([], 'a'));
    h.pushBarrier('Compacted vertices', 'Vertex numbering changed.');

    const view = h.list();
    expect(view.entries).toHaveLength(1);
    expect(view.entries[0]).toMatchObject({
      kind: 'barrier',
      label: 'Compacted vertices',
      reason: 'Vertex numbering changed.',
      bytes: 0,
      applied: true
    });
    expect(view.cursor).toBe(1);
  });

  it('lets later edits undo normally and stop at it', () => {
    const log: string[] = [];
    const h = new EditHistory();
    h.pushBarrier('Compacted vertices', 'renumbered');
    h.push(step(log, 'a'));
    h.push(step(log, 'b'));

    expect(h.undo()).toBe('b');
    expect(h.undo()).toBe('a');
    expect(h.undo()).toBeNull();
    expect(log).toEqual(['-b', '-a']);
    expect(h.redo()).toBe('a');
    expect(h.redo()).toBe('b');
    expect(h.redo()).toBeNull();
  });

  /** Two of them is noise: the older explanation describes history that is already gone. */
  it('replaces an earlier barrier rather than stacking', () => {
    const h = new EditHistory();
    h.pushBarrier('First', 'one');
    h.push(step([], 'a'));
    h.pushBarrier('Second', 'two');
    expect(h.list().entries).toHaveLength(1);
    expect(h.list().entries[0].label).toBe('Second');
  });

  it('clears everything, barrier included', () => {
    const h = new EditHistory();
    h.pushBarrier('Compacted vertices', 'renumbered');
    h.push(step([], 'a'));
    h.clear();
    expect(h.size).toBe(0);
    expect(h.list()).toEqual({ cursor: 0, entries: [] });
  });
});

describe('EditHistory failure handling', () => {
  /**
   * 🔴 A half-reversed world is not describable by any entry below it. Losing the history is the
   * cheap outcome; offering more undos over a model nobody can describe is the expensive one.
   */
  it('poisons the stack when an undo throws, and re-throws', () => {
    const log: string[] = [];
    const dispose = vi.fn();
    const h = new EditHistory();
    h.push(step(log, 'a', { weight: 2048, dispose }));
    h.push({
      label: 'Pruned 97 influences',
      undo: () => {
        throw new Error('skinWeight attribute is the wrong length');
      },
      redo: noop
    });

    expect(() => h.undo()).toThrow('skinWeight attribute is the wrong length');
    expect(h.canUndo).toBe(false);
    expect(h.canRedo).toBe(false);
    expect(h.size).toBe(1);
    expect(h.list().entries[0]).toMatchObject({
      kind: 'barrier',
      label: 'An undo failed part-way'
    });
    expect(h.list().entries[0].reason).toContain('no longer matches');
    expect(dispose).toHaveBeenCalledTimes(1);
    expect(h.heldBytes).toBe(0);
    expect(log).toEqual([]);
  });

  it('poisons the stack when a redo throws, and re-throws', () => {
    const h = new EditHistory();
    h.push({
      label: 'Deleted a 312-triangle piece',
      undo: noop,
      redo: () => {
        throw new Error('index buffer is gone');
      }
    });
    h.undo();

    expect(() => h.redo()).toThrow('index buffer is gone');
    expect(h.size).toBe(1);
    expect(h.list().entries[0]).toMatchObject({ kind: 'barrier', label: 'A redo failed part-way' });
    expect(h.canUndo).toBe(false);
  });

  it('is usable again after a poisoning, with the barrier as its floor', () => {
    const log: string[] = [];
    const h = new EditHistory();
    h.push({
      label: 'boom',
      undo: () => {
        throw new Error('boom');
      },
      redo: noop
    });
    expect(() => h.undo()).toThrow();

    h.push(step(log, 'later'));
    expect(h.undo()).toBe('later');
    expect(h.undo()).toBeNull();
    expect(log).toEqual(['-later']);
  });

  /**
   * ⚠️ Almost always a reactive effect firing on state the callback touched. Honouring it would
   * discard the redo tail — that is, the very step being undone.
   */
  it('drops a push made from inside an undo callback', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(noop);
    const log: string[] = [];
    const h = new EditHistory();
    h.push(step(log, 'a'));
    h.push({
      label: 'reentrant',
      undo: () => {
        log.push('-reentrant');
        h.push(step(log, 'ghost'));
      },
      redo: () => log.push('+reentrant')
    });

    h.undo();
    expect(h.size).toBe(2);
    expect(h.list().entries.map((e) => e.label)).toEqual(['a', 'reentrant']);
    expect(warn).toHaveBeenCalled();

    // The step that was being undone is still redoable — the ghost push did not eat it.
    expect(h.redo()).toBe('reentrant');
    expect(log).toEqual(['-reentrant', '+reentrant']);
    warn.mockRestore();
  });

  it('drops a barrier raised from inside an undo callback', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(noop);
    const h = new EditHistory();
    h.push(step([], 'a'));
    h.push({
      label: 'reentrant',
      undo: () => h.pushBarrier('ghost barrier', 'nope'),
      redo: noop
    });

    h.undo();
    expect(h.list().entries.map((e) => e.kind)).toEqual(['edit', 'edit']);
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});
