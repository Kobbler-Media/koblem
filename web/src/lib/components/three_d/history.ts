/*
 * Session undo/redo for the rig editor: a stack of REVERSIBLE COMMANDS over edits that happen
 * mostly IN PLACE.
 *
 * # Why commands, and not snapshots
 *
 * Every repair in the workbench mutates a live buffer — a prune zeroes influences inside the
 * skinWeight attribute, a colour adjust repaints the baseColor texels, a piece delete swaps the
 * index. The tools that offer a way back each keep one pristine copy of the thing they touch
 * (`restoreWeights`, `restoreImage`, `originalIndex`, `jointRest`) and put ALL of it back. That is
 * one step of undo per tool, captured before the first edit, with no ordering between tools, no
 * redo, and no way to take back the sixth prune without taking back the other five.
 *
 * Snapshotting the model per step instead is exactly what the next section rules out. So an edit
 * hands over the smallest closure that reverses ITSELF, and declares what that closure holds alive.
 *
 * # 🔴 Memory is the whole design
 *
 * A hero tier's skin weights are ~3.5 MB, its delivered index another ~2.6 MB, and its baseColor is
 * a 2048² `ImageData` — 16 MB. One copy per step reaches a gigabyte inside a dozen edits and the
 * tab dies. So the stack is capped twice — by DEPTH, and by the BYTES its entries declare in
 * `weight` — and over either cap the OLDEST entries are evicted.
 *
 * Oldest-first is not a preference, it is the only safe direction. Undo is strictly LIFO, so an
 * evicted prefix is unreachable by construction: no undo can ever run past the hole. Dropping from
 * the middle or the top would instead leave entries whose closures capture a state their surviving
 * neighbours have already moved past — the failure the workbench guards by hand today when it
 * refuses to install a pre-compaction index.
 *
 * ⚠️ An evicted step is GONE, and the stack says so — `canUndo`, `size` and the list shrink
 * together, so the UI is never offered an undo that would silently do nothing. The one deliberate
 * exception: eviction stops at the last surviving edit, so a `maxBytes` smaller than a single 16 MB
 * texture copy holds one over-budget entry rather than leaving the user's most recent edit
 * unundoable. The overrun is bounded by the largest single entry, never by the session's length.
 *
 * # 🔴 The barrier is the reason this file exists
 *
 * `compactVertices` renumbers every vertex, and `originalIndex`, `SkinReport.vertexRatios`, the
 * drives mask and the component labels are all indexed BY vertex. After it runs, every entry
 * captured before it addresses a numbering that no longer exists. Undoing one would not throw — it
 * would quietly draw triangles between unrelated vertices, which is the difference between an undo
 * stack and a corruption engine.
 *
 * An edit like that must not be pushed as an entry. It is pushed as a BARRIER ({@link
 * EditHistory.pushBarrier}): everything below is released, and the barrier stays in the list
 * carrying the reason, so the history the user had does not just silently evaporate. Entries pushed
 * afterwards undo normally and stop at it.
 *
 * # No Svelte, no three.js, no DOM
 *
 * Deliberately. This is one of the few things in this directory that is trivially unit-testable,
 * and the one whose bugs are silent. Mutations bump {@link EditHistory.revision}; a Svelte caller
 * bumps its own `$state` tick after calling in, rather than this module reaching for a rune.
 */

/** A step the caller knows how to reverse. */
export interface HistoryEntry {
  /** Shown in the history list. Written for a human: "Deleted a 312-triangle piece". */
  label: string;
  /**
   * Put the world back as it was before this edit.
   *
   * 🔴 Only ever called with every LATER entry already undone — the stack guarantees that ordering,
   * and it is what lets an undo be a narrow in-place write instead of a whole-model restore.
   */
  undo: () => void;
  /** Re-apply the edit. Called only against the same state `undo` produced, for the same reason. */
  redo: () => void;
  /**
   * Entries pushed consecutively with the same non-null key collapse into one.
   *
   * Not optional in practice: a joint drag fires on every `pointermove`, so one gesture is 200
   * pushes. Make the key identify the GESTURE, not the tool — `move:LeftArm` and not `move`, or
   * dragging one joint and then another merges into a single step that undoes both.
   */
  coalesce?: string | null;
  /**
   * Roughly how many bytes this entry keeps alive, for the byte budget. 0 (or omitted) for an entry
   * that captured nothing — a joint move holds three floats, and pretending otherwise evicts real
   * history to make room for arithmetic.
   */
  weight?: number;
  /**
   * Run when the entry is dropped (evicted, discarded as a redo tail, or cleared).
   *
   * ⚠️ Only for handles the GC will not reclaim on its own — a `THREE.Texture`, a render target, an
   * `ImageBitmap`. Typed arrays and `ImageData` need nothing here: dropping the entry drops the
   * closure that held them.
   */
  dispose?: () => void;
}

/** One row of {@link EditHistory.list}. Plain data — it holds no closure, so a component may keep
 *  it in state without pinning the buffers the entry captured. */
export interface HistoryItem {
  /** Stable across the entry's life; the list order is not, since eviction shifts it. */
  id: number;
  kind: 'edit' | 'barrier';
  label: string;
  /** Why the history below a barrier was discarded. Null on an ordinary edit. */
  reason: string | null;
  /** What this row's `weight` contributed to the budget. */
  bytes: number;
  /** True while the step is in effect — false once it has been undone and is waiting for a redo. */
  applied: boolean;
}

export interface HistoryView {
  entries: HistoryItem[];
  /** How many leading entries are applied, i.e. where the cursor sits. `entries.length` when
   *  everything is in effect, 0 when everything has been undone. */
  cursor: number;
}

export interface HistoryOptions {
  /** Hard cap on retained steps, regardless of how cheap they are. */
  maxDepth?: number;
  /** Cap on the summed `weight` of retained steps. */
  maxBytes?: number;
}

/** Deep enough that a session of ordinary edits never hits it before the byte budget does. */
export const DEFAULT_MAX_DEPTH = 50;

/** ~4 baseColor copies, or ~18 skin-weight copies. Chosen to sit far under the point where the tab
 *  is at risk while still covering the run of edits anyone actually walks back. */
export const DEFAULT_MAX_BYTES = 64 * 1024 * 1024;

const NOOP = (): void => {};

/** A dropped entry's cleanup must not abort the eviction that is dropping it — the slot is gone
 *  either way, and throwing here would leave the budget accounting mid-update. */
function safeDispose(fn: (() => void) | null): void {
  if (!fn) return;
  try {
    fn();
  } catch (e) {
    console.warn('history: dispose failed', e);
  }
}

function weightOf(entry: HistoryEntry): number {
  const w = entry.weight ?? 0;
  return Number.isFinite(w) && w > 0 ? w : 0;
}

/**
 * A retained step.
 *
 * `head*` and `tail*` exist for coalescing: a merged run keeps the FIRST push's `undo` (the state
 * before the drag started) and the LAST push's `redo`, so it is holding two entries' captures and
 * has to account for — and release — both. An unmerged entry has an empty tail, so its own weight
 * is counted exactly once.
 */
interface Slot {
  readonly id: number;
  readonly kind: 'edit' | 'barrier';
  label: string;
  reason: string | null;
  undo: () => void;
  redo: () => void;
  headWeight: number;
  tailWeight: number;
  headDispose: (() => void) | null;
  tailDispose: (() => void) | null;
}

export class EditHistory {
  readonly maxDepth: number;
  readonly maxBytes: number;

  private slots: Slot[] = [];
  /** Number of APPLIED slots, so it doubles as the index of the next slot to redo. Eviction from
   *  the front subtracts from it, which is what keeps `canUndo` honest about a shortened stack. */
  private cursor = 0;
  /** The coalesce key still open for merging. Cleared by anything that breaks the run of
   *  consecutive pushes — an undo, a redo, a barrier, a clear — so "consecutive" means what it
   *  says and a redone step can never be extended by a later drag. */
  private openKey: string | null = null;
  private nextId = 1;
  private bytes = 0;
  private rev = 0;
  /** True while a caller's `undo`/`redo` closure is running. See {@link push}. */
  private applying = false;

  constructor(options: HistoryOptions = {}) {
    this.maxDepth = Math.max(1, options.maxDepth ?? DEFAULT_MAX_DEPTH);
    this.maxBytes = Math.max(0, options.maxBytes ?? DEFAULT_MAX_BYTES);
  }

  /** Bumped on every mutation. The hook for a `$state` tick in a component, since this module
   *  deliberately owns no reactive state of its own. */
  get revision(): number {
    return this.rev;
  }

  /** Retained rows, barrier included — what the memory guard in the UI is reading. */
  get size(): number {
    return this.slots.length;
  }

  /** Summed declared weight of everything retained. */
  get heldBytes(): number {
    return this.bytes;
  }

  get canUndo(): boolean {
    return this.undoable !== null;
  }

  get canRedo(): boolean {
    return this.redoable !== null;
  }

  /** For labelling the button: "Undo delete piece". Null when there is nothing to undo. */
  get undoLabel(): string | null {
    return this.undoable?.label ?? null;
  }

  get redoLabel(): string | null {
    return this.redoable?.label ?? null;
  }

  /**
   * Record a step that has ALREADY been applied to the world.
   *
   * 🔴 Pushing while the cursor is not at the top discards the redo tail — the entries above the
   * cursor describe a future that this edit has just made unreachable, and offering a redo into it
   * would replay a change against a state it was never captured over. They are released here, not
   * left dangling, because those are the entries most likely to be holding a megabyte each.
   */
  push(entry: HistoryEntry): void {
    // ⚠️ A push from inside an undo/redo callback is dropped, not honoured. It would discard the
    // redo tail — that is, the very step being undone — and the state it wants to describe is
    // already modelled by the cursor move that is in flight. Almost always a reactive effect
    // firing on state the callback touched, so it is worth a word in the console.
    if (this.applying) {
      console.warn('history: push during undo/redo ignored:', entry.label);
      return;
    }

    this.dropRedoTail();

    const key = entry.coalesce ?? null;
    const top = this.slots.length > 0 ? this.slots[this.slots.length - 1] : null;
    if (key !== null && key === this.openKey && top && top.kind === 'edit') {
      // The superseded tail's capture is unreachable the moment its `redo` is replaced. Identity
      // check because a caller may hand the same `dispose` closure to every push in a run, and the
      // head's copy of it is still live.
      if (top.tailDispose && top.tailDispose !== top.headDispose) safeDispose(top.tailDispose);
      this.bytes -= top.tailWeight;
      top.tailWeight = weightOf(entry);
      this.bytes += top.tailWeight;
      top.redo = entry.redo;
      top.tailDispose = entry.dispose ?? null;
      // The newest label, because it describes the run so far ("Moved Hips 4.1 cm") where the first
      // push's describes a single pointermove.
      top.label = entry.label;
      this.enforceBudget();
      this.rev++;
      return;
    }

    const weight = weightOf(entry);
    this.slots.push({
      id: this.nextId++,
      kind: 'edit',
      label: entry.label,
      reason: null,
      undo: entry.undo,
      redo: entry.redo,
      headWeight: weight,
      tailWeight: 0,
      headDispose: entry.dispose ?? null,
      tailDispose: null
    });
    this.bytes += weight;
    this.cursor = this.slots.length;
    this.openKey = key;
    this.enforceBudget();
    this.rev++;
  }

  /**
   * Record an edit that CANNOT be reversed in place, and discard everything it invalidated.
   *
   * The case this is for is `compactVertices`: it renumbers every vertex, so each earlier entry now
   * addresses a numbering that no longer exists and would undo into garbage rather than throwing.
   * Anything that renumbers, resizes or replaces the buffers earlier entries captured belongs here
   * — a barrier costs the user their history, an entry costs them their model.
   *
   * The barrier is not undoable and stays at the base of the list carrying `reason`, so the UI can
   * say why the steps went. A second barrier replaces the first: the older explanation describes
   * history that is already gone, and two of them is noise.
   */
  pushBarrier(label: string, reason: string): void {
    if (this.applying) {
      console.warn('history: barrier during undo/redo ignored:', label);
      return;
    }
    this.releaseAll();
    this.slots.push({
      id: this.nextId++,
      kind: 'barrier',
      label,
      reason,
      undo: NOOP,
      redo: NOOP,
      headWeight: 0,
      tailWeight: 0,
      headDispose: null,
      tailDispose: null
    });
    this.cursor = 1;
    this.rev++;
  }

  /**
   * Reverse the top step. Returns its label, or null when there is nothing to undo.
   *
   * 🔴 If the caller's closure throws, the world is in an unknown half-reversed state and every
   * entry below it was captured against a state that no longer exists. The stack is therefore
   * discarded and replaced by a barrier explaining it, and the error is re-thrown. Losing the
   * history is the cheap outcome; the expensive one is offering more undos over a model nobody can
   * describe any more.
   */
  undo(): string | null {
    const slot = this.undoable;
    if (!slot) return null;
    this.openKey = null;
    this.run(slot.undo, 'An undo failed part-way');
    this.cursor--;
    this.rev++;
    return slot.label;
  }

  /** Re-apply the step above the cursor. Same failure rule as {@link undo}. */
  redo(): string | null {
    const slot = this.redoable;
    if (!slot) return null;
    this.openKey = null;
    this.run(slot.redo, 'A redo failed part-way');
    this.cursor++;
    this.rev++;
    return slot.label;
  }

  /** Drop everything, barrier included. For a new model, or a discard-and-reload. */
  clear(): void {
    if (this.slots.length === 0 && this.cursor === 0) return;
    this.releaseAll();
    this.rev++;
  }

  /** The session history as the UI shows it: rows in the order they happened, plus the cursor. */
  list(): HistoryView {
    return {
      cursor: this.cursor,
      entries: this.slots.map((slot, i) => ({
        id: slot.id,
        kind: slot.kind,
        label: slot.label,
        reason: slot.reason,
        bytes: slot.headWeight + slot.tailWeight,
        applied: i < this.cursor
      }))
    };
  }

  // ── internals ───────────────────────────────────────────────────────

  /** A barrier is never undoable, which is how undo stops at it rather than walking through. */
  private get undoable(): Slot | null {
    if (this.cursor <= 0) return null;
    const slot = this.slots[this.cursor - 1];
    return slot && slot.kind === 'edit' ? slot : null;
  }

  private get redoable(): Slot | null {
    if (this.cursor >= this.slots.length) return null;
    const slot = this.slots[this.cursor];
    return slot && slot.kind === 'edit' ? slot : null;
  }

  private run(fn: () => void, failure: string): void {
    this.applying = true;
    try {
      fn();
    } catch (e) {
      this.applying = false;
      this.pushBarrier(
        failure,
        'The model no longer matches any recorded step, so the steps went.'
      );
      throw e;
    } finally {
      this.applying = false;
    }
  }

  private dropRedoTail(): void {
    if (this.cursor >= this.slots.length) return;
    for (const slot of this.slots.splice(this.cursor)) this.retire(slot);
  }

  private releaseAll(): void {
    for (const slot of this.slots) this.retire(slot);
    this.slots.length = 0;
    this.cursor = 0;
    this.openKey = null;
    // Belt and braces: `retire` has already subtracted every slot's contribution.
    this.bytes = 0;
  }

  /** Give back what a slot was holding and make it inert, so a stray reference is a loud no-op
   *  rather than a restore from a buffer the model has moved past. */
  private retire(slot: Slot): void {
    this.bytes -= slot.headWeight + slot.tailWeight;
    if (this.bytes < 0) this.bytes = 0;
    const head = slot.headDispose;
    const tail = slot.tailDispose;
    slot.headDispose = null;
    slot.tailDispose = null;
    slot.headWeight = 0;
    slot.tailWeight = 0;
    slot.undo = NOOP;
    slot.redo = NOOP;
    safeDispose(head);
    if (tail !== head) safeDispose(tail);
  }

  private enforceBudget(): void {
    while (this.slots.length > this.maxDepth || this.bytes > this.maxBytes) {
      if (!this.evictOldest()) return;
    }
  }

  /**
   * Drop the oldest surviving EDIT. False when there is nothing left to give.
   *
   * ⚠️ The barrier is skipped even though it sits at the front: it is the only record of why the
   * history beneath it is missing, and evicting it would leave a stack that looks like an ordinary
   * clean one. It costs a row and no bytes.
   *
   * ⚠️ The last edit is kept even when it is over budget — see the header. A user whose most recent
   * action has no undo is a worse outcome than a single bounded overrun.
   */
  private evictOldest(): boolean {
    let index = -1;
    let edits = 0;
    for (let i = 0; i < this.slots.length; i++) {
      if (this.slots[i].kind !== 'edit') continue;
      edits++;
      if (index < 0) index = i;
    }
    if (index < 0 || edits <= 1) return false;
    const [slot] = this.slots.splice(index, 1);
    this.retire(slot);
    // Guarded rather than unconditional: eviction only ever runs from `push`, which has already
    // discarded the redo tail, so today the cursor is always above the evicted row.
    if (index < this.cursor) this.cursor--;
    return true;
  }
}
