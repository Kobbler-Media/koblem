<script lang="ts">
  /*
   * Repair a delivered asset in the browser, IN the browser's viewport, then hand the bytes to
   * whoever asked for them.
   *
   * This is `RigEditor` turned inside out. Every tool it had answered in a list: you selected a
   * stray shell and the viewport showed nothing, you measured the skin and got a table saying 97
   * influences reach past 35% of the body with no way to find out WHERE, and you named a joint by
   * picking a core slot and then choosing from a dropdown of sanitized bone names that mean nothing
   * spatially. The whole reason a browser tool exists here is that a human looking at the model is
   * the missing check — so the canvas is the editor, and this component is the state behind it.
   *
   * The repairs, and why these:
   *
   *   1. **Prune a skin-weight leak.** The "spears off the arms" defect: a handful of vertices bound
   *      to a joint half a body away, flung across the character when it rotates. On one measured
   *      subject that was 97 vertices of 247,727 — 0.039% — cleanly separated by distance, which is
   *      why a distance threshold works and why the live paint is the check on it.
   *   2. **Name a missing core joint.** The retarget maps by exact string, so a skeleton with 20 of
   *      22 core names has two joints no humanoid clip will ever drive.
   *   3. **Delete stray shells.** These characters come out as ~200 interpenetrating shells rather
   *      than one solid, and some of them are floating junk.
   *   4. **Move a joint**, **grade the texture**, **weaken the normal map**.
   *   5. **Push a limb out of the body on a delivered take** — see the Motion mode note below.
   *
   * ⚠️ THE ENGINE ALREADY RAN A CLEANUP. It verifies itself against the shipped pose gate and keeps
   * the original skin byte-for-byte when it cannot prove an improvement — so anything left here is
   * something that pass declined, and pruning aggressively is a way to make a rig worse. Start from
   * the distribution, not from the slider.
   *
   * ⚠️ Renaming a joint that is not anatomically where its new name claims will retarget CLEANLY and
   * animate WRONGLY — nothing validates the claim. "Show what it drives" is the only check there is.
   * A retarget is 0.22 s, so bake a take and look.
   *
   * # Motion is a MODE, not a side panel
   *
   * A GLB that carries animation tracks is a take, and every rig mode is refused on it — which used
   * to leave the mode rail empty and the take's own repair bolted on beside the canvas as text. It
   * is a mode now, with the same in-canvas treatment: the findings are a list you click, and
   * clicking one scrubs the model to the offending frame and lights the two bones up on the
   * character — the limb in danger tone, the body part it is buried inside in select tone.
   *
   * 🔴 THE ROOT CAUSE IS NOT A BUG TO FIX UPSTREAM. Motion is transferred joint-by-joint from a
   * source skeleton with no mesh awareness at all, so self-intersection is normal for this class of
   * retarget. This is a post-hoc correction on the clip, and NOTHING SCORES WHETHER THE RESULT IS
   * RIGHT — the count falling is a signal, not a verdict. Your eye is the only judge in the loop.
   *
   * # Where the saved bytes go is the HOST's business
   *
   * This component builds a patched GLB and hands it to `onSave`. It does not know about characters,
   * tiers or edit records — `savePixal3dEdit` wants a character id and a base tier, and an engine
   * like SPAR3D has neither. With no handler the GLB downloads, which is the honest answer for a
   * host with nowhere to put it. Tier propagation stays behind the props that make it possible.
   */
  import { onDestroy, untrack } from 'svelte';
  import * as THREE from 'three';

  import { fetchMediaBlob, pixal3dModelPath, savePixal3dEdit } from '$lib/api/client';
  import ColourHud from './ColourHud.svelte';
  import JointPopover from './JointPopover.svelte';
  import ModelStage, { type EditorStageHandle, type StageMode } from './ModelStage.svelte';
  import NormalHud from './NormalHud.svelte';
  import PiecePopover from './PiecePopover.svelte';
  import UvPanel from './UvPanel.svelte';
  import { chirality, sideSwapPlan, type Chirality, type SideSwap } from './chirality';
  import { patchGlb, type TexturePatch } from './glb-patch';
  import { EditHistory, type HistoryView } from './history';
  import {
    applyIndex,
    compactVertices,
    decimate,
    interiorFaces,
    labelComponents,
    pruneIsolated,
    refreshBounds,
    removeComponents,
    removeDegenerateTriangles,
    type ComponentLabels,
    type DecimateReport,
    type IndexArray,
    type MeshComponent
  } from './mesh-repair';
  import {
    applyClearance,
    buildCapsules,
    clipFrameCount,
    detectIntersections,
    envelopeFrom,
    restoreClip,
    type Intersection
  } from './motion-repair';
  import { BoneSpotlight, type JointState, type SpotlitBone } from './overlays';
  import {
    MIXAMO_CORE,
    analyseSkin,
    applyPrune,
    boneVertexMask,
    captureJointRest,
    coreCandidates,
    coreCoverage,
    modelSpan,
    moveJointWorld,
    movedJoints,
    previewPrune,
    resetJoints,
    sanitizeName,
    type JointRest,
    type PruneResult,
    type SkinReport
  } from './stage';
  import {
    IDENTITY_ADJUST,
    adjustColour,
    applyToTexture,
    encodeTexture,
    setNormalStrength,
    textureToImageData,
    type ColourAdjust
  } from './texture-repair';
  import {
    MIRROR_MIN_CONFIDENCE,
    detectMirrorAxis,
    mirrorBone,
    mirrorJointMove,
    symmetryReport,
    type AxisDetection,
    type MirrorMode,
    type SymmetryReport
  } from './symmetry';
  import {
    collectJointEdits,
    propagateToTier,
    type JointEdits,
    type RenamedJoint
  } from './tier-propagate';
  import {
    WeightBrush,
    brushField,
    captureLike,
    disposeWeightPaintCache,
    readInfluences,
    restoreStroke,
    strokeByteLength,
    transferInfluence,
    verticesInBrush,
    type FalloffProfile,
    type PaintMode
  } from './weight-paint';

  let {
    src = '',
    label: viewLabel = '',
    characterId = '',
    baseTier = '',
    tiers = [],
    stagingReady = true,
    height = 'min(72vh, 820px)',
    onSave,
    onSaved,
    onReloadRequested,
    onError,
    onReady
  }: {
    /** A bare koblem media path for the viewer. Changing it resets the whole editing session. */
    src?: string;
    /** What the user picked, for the viewer caption and for naming a saved repair. */
    label?: string;
    /** Only needed for tier propagation, which is Pixal3D-specific. Empty on any other host. */
    characterId?: string;
    /** The tier a repair is based on. Empty when the current view is not an editable rig — a take,
     *  or an edit whose base tier is itself a take — in which case every RIG editing mode is
     *  refused. Motion mode does not need one. */
    baseTier?: string;
    /** Every tier this character has, so a skeleton repair can be carried to the rest of the ladder. */
    tiers?: string[];
    /** Pixal3D's shared staging mount. Defaults to true so a host with no such concept says nothing. */
    stagingReady?: boolean;
    /** CSS length for the viewport. */
    height?: number | string;
    /**
     * Called with the patched GLB; the HOST persists it however it can.
     *
     * 🔴 With no handler the workbench DOWNLOADS the file instead. That is deliberate: the endpoint
     * this used to call directly is Pixal3D-specific — it wants a character id and a `base_tier` and
     * stores an edit against a character record — and a host with no such record (SPAR3D returns
     * URLs and nothing else) can still produce bytes. A silent no-op would be the alternative.
     *
     * `animatable: false` means the host produced bytes but nothing downstream can animate them;
     * `note` is what the user is told about it.
     */
    onSave?: (
      glb: Blob,
      meta: { label: string; changes: Record<string, unknown> }
    ) => Promise<{ note?: string; animatable?: boolean }>;
    /** The host persisted something and its own listings are now stale. */
    onSaved?: () => void;
    /** Ask the parent to re-fetch the pristine GLB — the only honest "undo" for an in-place edit.
     *  The parent bumps a cache-busting nonce, which arrives here as a new `src`. */
    onReloadRequested?: () => void;
    onError?: (message: string) => void;
    /** Notification only. This component owns the handle; nothing upstream needs to. */
    onReady?: (handle: EditorStageHandle) => void;
  } = $props();

  type WorkMode = 'view' | 'joints' | 'pieces' | 'weights' | 'paint' | 'uv' | 'colour' | 'motion';

  /**
   * `rig` needs a rest pose and a tier to save against; `take` needs baked animation tracks. They
   * are mutually exclusive by construction — a GLB either carries clips or it does not — which is
   * why the rail is never empty and never offers both halves at once.
   */
  const MODES: { id: WorkMode; label: string; needs: 'any' | 'rig' | 'take' }[] = [
    { id: 'view', label: 'View', needs: 'any' },
    { id: 'joints', label: 'Joints', needs: 'rig' },
    { id: 'pieces', label: 'Pieces', needs: 'rig' },
    { id: 'weights', label: 'Weights', needs: 'rig' },
    { id: 'paint', label: 'Paint', needs: 'rig' },
    { id: 'uv', label: 'Atlas', needs: 'rig' },
    { id: 'colour', label: 'Colour', needs: 'rig' },
    { id: 'motion', label: 'Motion', needs: 'take' }
  ];

  let mode = $state<WorkMode>('view');
  let showSkeleton = $state(true);
  /** The workbench root, so the undo shortcut can be scoped to it. See `onWorkbenchKey`. */
  let rootEl = $state<HTMLDivElement>();

  //: 🔴 `$state.raw`, not `$state`. A plain `$state` DEEP-PROXIES objects and arrays, and this
  //: handle carries three.js arrays read in tight loops over ~250k vertices. The proxy buys nothing
  //: — the handle is replaced wholesale on every load, never mutated.
  //: `EditorStageHandle` is `StageHandle` plus the things only the live viewport can do:
  //: `invalidateGeometry()`, `detachGrade()`, the camera affordances and `scrubTo()`.
  let stage = $state.raw<EditorStageHandle | null>(null);

  let busy = $state('');
  let meshBusy = $state('');
  let error = $state('');
  let saved = $state('');
  let repairLabel = $state('');

  function failMessage(message: string): void {
    error = message;
    onError?.(message);
  }

  function fail(e: unknown): void {
    failMessage(e instanceof Error ? e.message : String(e));
  }

  /**
   * 🔴 CALL THIS AFTER EVERY INDEX CHANGE, ALONGSIDE THE RE-LABEL. `SurfaceOverlay` borrows the
   * body's attributes and `HoverPicker` rides a BVH cached against the geometry; both answer from
   * the numbering they were built over. A stale one does not throw — it lights up, and then deletes,
   * a different part of the character.
   */
  function invalidateStageGeometry(): void {
    stage?.invalidateGeometry();
  }

  // ── undo/redo ───────────────────────────────────────────────────────
  /**
   * 🔴 `EditHistory` enforces ORDERING, NOT NARROWNESS, and that is the whole trap in wiring it up.
   * The idiom this panel already had is `restoreWeights ??= <copy of every skin weight>` — capture
   * once, restore EVERYTHING. Wrapping that per step gives a stack where undoing entry N silently
   * reverts N−1…0 while the UI still lists them as applied. So every entry below captures the state
   * IT changed, immediately before it changed it, and puts back exactly that.
   *
   * The session-wide `restoreWeights` / `restoreImage` / `restoreNormal` / `originalIndex` buffers
   * survive, but only as the backing store for the deliberately COARSE "put it all back" buttons —
   * and each of those is itself pushed as an entry, so the stack never disagrees with the model.
   *
   * ⚠️ No entry declares `dispose`. Nothing here uniquely owns a `THREE.Texture` or a render target
   * — the captures are typed arrays and `ImageData`, which go when the closure does — and `dispose`
   * is not refcounted, so two entries capturing one texture would both free it.
   */
  const history = new EditHistory();
  /** A DEPENDENCY, not a value: `EditHistory` deliberately owns no reactive state. */
  let historyTick = $state(0);
  const historyView = $derived.by((): HistoryView => {
    const tick = historyTick;
    return tick >= 0 ? history.list() : { entries: [], cursor: 0 };
  });
  const canUndo = $derived(historyTick >= 0 && history.canUndo);
  const canRedo = $derived(historyTick >= 0 && history.canRedo);
  const undoLabel = $derived(historyTick >= 0 ? history.undoLabel : null);
  const redoLabel = $derived(historyTick >= 0 ? history.redoLabel : null);

  /**
   * How long a run of edits may pause before the next one starts a NEW undo step.
   *
   * ⚠️ The coalesce key has to name the GESTURE, not the tool: `move:LeftArm` shared by a drag and a
   * later nudge merges two deliberate actions into one entry that reverses both. `ModelStage`
   * reports drag deltas with no start/end event, so the gesture boundary is recovered from the gap
   * between deltas — a drag fires every few ms, and nobody presses a nudge button twice inside 400.
   */
  const GESTURE_GAP_MS = 400;
  let gestureSeq = 0;
  let gestureId = '';
  let gestureAt = 0;

  function gestureKey(kind: string, name: string): string {
    const now = performance.now();
    const id = `${kind}:${name}`;
    if (id !== gestureId || now - gestureAt > GESTURE_GAP_MS) {
      gestureSeq++;
      gestureId = id;
    }
    gestureAt = now;
    return `${id}:${gestureSeq}`;
  }

  function undoStep(): void {
    error = '';
    try {
      history.undo();
    } catch (e) {
      fail(e);
    } finally {
      historyTick++;
    }
  }

  function redoStep(): void {
    error = '';
    try {
      history.redo();
    } catch (e) {
      fail(e);
    } finally {
      historyTick++;
    }
  }

  /**
   * 🔴 Scoped to the workbench, NEVER to `window`. A global `Ctrl+Z` eats the browser's own undo in
   * the repair-name field, in the search box at the top of the page and in every other input on the
   * document — a bug the user cannot even describe. This listens on the workbench element, so it
   * only sees keystrokes that bubbled out of it, and text fields keep their own undo.
   *
   * ⚠️ Added imperatively rather than as `onkeydown=` in the template: a keyboard handler on a
   * static `<div>` is an a11y lint failure that only a `svelte-ignore` would quieten, and an
   * unnecessary `svelte-ignore` is itself an error here. `ModelStage` sets its `tabindex` and ARIA
   * the same way, for the same reason.
   */
  const EDITABLE = new Set(['INPUT', 'TEXTAREA', 'SELECT']);

  function onWorkbenchKey(e: KeyboardEvent): void {
    if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
    const key = e.key.toLowerCase();
    if (key !== 'z' && key !== 'y') return;
    const target = e.target as HTMLElement | null;
    if (target && (target.isContentEditable || EDITABLE.has(target.tagName))) return;
    e.preventDefault();
    if (key === 'y' || e.shiftKey) redoStep();
    else undoStep();
  }

  $effect(() => {
    const el = rootEl;
    if (!el) return;
    el.addEventListener('keydown', onWorkbenchKey);
    return () => el.removeEventListener('keydown', onWorkbenchKey);
  });

  // ── skin weights ────────────────────────────────────────────────────
  /** What `BufferAttribute.array.slice()` hands back: the same concrete typed array, which is all a
   *  restore and the byte accounting need. */
  type WeightArray = ArrayLike<number> & { byteLength: number };
  /**
   * 🔴 BOTH skin attributes, not just `skinWeight`.
   *
   * A prune only zeroes weights, so this used to copy one attribute. The brush REBINDS — it writes
   * `skinIndex` as well — and a restore that put the delivered weights back beside painted joint
   * indices would produce a skin that never existed on any model, silently and with no error.
   */
  interface SkinCopy {
    index: WeightArray | null;
    weight: WeightArray;
  }
  type WeightCopy = Map<THREE.SkinnedMesh, SkinCopy>;

  let report = $state.raw<SkinReport | null>(null);
  let threshold = $state(0.35);
  let preview = $state.raw<PruneResult | null>(null);
  let applied = $state.raw<PruneResult[]>([]);
  /** The DELIVERED skin weights, copied before the first prune — the backing store for the coarse
   *  "put the weights back" button only. Per-step undo captures its own copy; see the history note. */
  let restoreWeights = $state.raw<WeightCopy | null>(null);

  function copyWeights(meshes: THREE.SkinnedMesh[]): WeightCopy {
    return new Map(
      meshes.map((mesh): [THREE.SkinnedMesh, SkinCopy] => {
        const index = mesh.geometry.getAttribute('skinIndex') as THREE.BufferAttribute | undefined;
        const weight = mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
        return [mesh, { index: index?.array.slice() ?? null, weight: weight.array.slice() }];
      })
    );
  }

  /**
   * ⚠️ A length mismatch means `compactVertices` renumbered the vertices after the copy was taken.
   * Writing it back would scramble the skin, so that mesh is skipped — and compaction pushes a
   * history BARRIER, so no entry can reach this state anyway.
   *
   * ⚠️ The cast on the DESTINATION is `BufferAttribute.array` being a `TypedArray` union with no
   * callable `.set` signature. The source needs none: `WeightArray` is already an `ArrayLike<number>`,
   * which is exactly what `.set` takes. Both sides are the same concrete array at runtime — the copy
   * came from `.slice()` of this very attribute — and the length guard above is the real check.
   */
  function writeWeights(meshes: THREE.SkinnedMesh[], copy: WeightCopy): void {
    for (const mesh of meshes) {
      const original = copy.get(mesh);
      if (!original) continue;
      const live = mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
      if (original.weight.length !== live.array.length) continue;
      (live.array as Float32Array).set(original.weight);
      live.needsUpdate = true;
      const liveIndex = mesh.geometry.getAttribute('skinIndex') as
        THREE.BufferAttribute | undefined;
      if (!original.index || !liveIndex || original.index.length !== liveIndex.array.length)
        continue;
      (liveIndex.array as Float32Array).set(original.index);
      liveIndex.needsUpdate = true;
    }
  }

  function weightBytes(copy: WeightCopy): number {
    let bytes = 0;
    for (const entry of copy.values()) {
      bytes += entry.weight.byteLength + (entry.index?.byteLength ?? 0);
    }
    return bytes;
  }

  // ── joints ──────────────────────────────────────────────────────────
  let renames = $state<{ from: string; to: string }[]>([]);
  let selectedJoint = $state('');
  let hoveredJoint = $state('');
  let keepChildren = $state(true);
  let jointRest = $state.raw<JointRest | null>(null);
  /**
   * Every joint's name as the engine delivered it, in CANONICAL (colon-bearing) form.
   *
   * 🔴 The `renames` array above is a LOG — it grows, it can name the same bone twice, and after a
   * left/right swap its `from` fields are intermediate names no other tier has ever carried. What a
   * save needs is STATE: for each bone, what it was called on arrival and what it is called now.
   * That is this map, and it is immune to the order the edits happened in.
   */
  let deliveredNames = $state.raw<Map<THREE.Bone, string> | null>(null);
  /**
   * Joints staged for deletion: their weights have been moved onto their parent and the save will
   * cut them out of the skin, the node tree and every binding index above them.
   *
   * 🔴 They stay in the LIVE skeleton until then, and that is not laziness. `skinIndex` holds an
   * index into `skeleton.bones`, and `glb-patch` matches the file's joints to that same list BY
   * ORDER — pulling a bone out of it here would silently renumber every joint above it under the
   * live mesh. The deletion is a fact about the FILE, applied once, at save.
   */
  let dissolved = $state.raw<THREE.Bone[]>([]);
  /** Bumped when a model loads, so the moved-joint list re-reads a fresh skeleton's bones. Every
   *  edit bumps `jointNudges`; the two are summed wherever a dependency on "a joint moved" is
   *  wanted, because bones are mutated in place and are not reactive. */
  let jointVersion = $state(0);
  let jointNudges = $state(0);
  /** Bumped on every rename. `stage.bones` is a plain array of three.js objects, not reactive state
   *  — renaming mutates `bone.name` in place, so something has to tell Svelte to re-read it. */
  let renameTick = $state(0);
  /** The bone whose driven vertices are painted, or '' for none. Sanitized (three-side) name. */
  let drivingJoint = $state('');
  let drivingMask = $state.raw<Uint8Array[] | null>(null);
  let drivingRatios = $state.raw<Float32Array[] | null>(null);
  /** The face the stage last reported a click on, which is what the atlas panel points at. */
  let pickedFace = $state(-1);

  // ── symmetry ────────────────────────────────────────────────────────
  /**
   * The character's own mirror plane, derived from where its paired core joints actually sit.
   *
   * 🔴 Gated on `confidence`, not merely consulted. `detectMirrorAxis` returns `x` at confidence 0
   * for a skeleton where nothing paired — the axis is a placeholder there, not an answer — so
   * mirroring an edit on that reading would move a joint that is not the other side of anything.
   * `scores` is carried so the refusal can say WHICH axis it liked and by how little, rather than
   * being a dead end.
   */
  let mirror = $state.raw<AxisDetection | null>(null);
  /** ⚠️ Below `MIRROR_MIN_CONFIDENCE` the feature refuses. `detectMirrorAxis` scales its winner by
   *  the margin over the runner-up and by the sample size, so a clean humanoid lands near 1.0 and a
   *  coin flip near 0. The floor lives in `symmetry.ts` because `chirality` gates on it too. */
  const mirrorReady = $derived(!!mirror && mirror.confidence >= MIRROR_MIN_CONFIDENCE);
  let symmetric = $state(false);
  let mirrorMode = $state<MirrorMode>('delta');
  /**
   * The asymmetry already in the rig.
   *
   * ⚠️ A `$state.raw` refreshed on demand rather than a `$derived`, deliberately: `symmetryReport`
   * re-pairs every bone against every other one, and a derived reading `jointVersion` would run
   * that on every `pointermove` of a drag. It is a measurement, so it is stamped stale and
   * re-measured on a press.
   */
  let symmetryDiag = $state.raw<SymmetryReport | null>(null);
  let symmetryStale = $state(false);

  // ── weight painting ─────────────────────────────────────────────────
  /** Rebuilt per model; it holds one `seen` mask per mesh and the open stroke's scratch. */
  let brush: WeightBrush | null = null;
  /**
   * `pick` is not a paint mode — it writes nothing. It answers "which joint owns THIS bit of the
   * character", which is the question you have to answer before any of the other three mean
   * anything, and which used to be answerable only by leaving for Joints mode and coming back.
   */
  type BrushTool = PaintMode | 'pick';
  const PAINT_TOOLS: { id: BrushTool; hint: string }[] = [
    { id: 'pick', hint: 'Click the model to select the joint that drives it — one click, no edit' },
    { id: 'assign', hint: 'Drive these vertices with the selected joint' },
    { id: 'smooth', hint: 'Average each vertex with its neighbours — the fix for a torn seam' },
    { id: 'erase', hint: 'Take the selected joint off these vertices, sharing its weight out' }
  ];
  let paintTool = $state<BrushTool>('pick');
  /** The tool `pick` interrupted, so one click can hand the brush straight back to it. */
  let toolBeforePick = $state<PaintMode>('assign');
  /** What the last pick found: the joint, and its share of the weight under the brush. Cleared when
   *  the brush moves on, and the reason a pick reads as an ANSWER rather than a silent selection. */
  let picked = $state.raw<{ name: string; share: number; vertices: number } | null>(null);
  let paintProfile = $state<FalloffProfile>('smooth');
  let paintStrength = $state(0.5);
  /** Radius as a FRACTION of the model's longest dimension, so the default means the same on a
   *  2-metre character and a 2-unit one. `brushRadius` below is the world-space value the brush and
   *  the cursor ring both take. */
  let brushScale = $state(0.04);
  /** Where the cursor is on the surface, in world space. Null when it is off the model. */
  let brushAt = $state.raw<THREE.Vector3 | null>(null);
  let painting = $state(false);
  /** What the last committed stroke did, for the readout. `DabResult`'s counters, summed. */
  let strokeTally = $state.raw<{
    dabs: number;
    vertices: number;
    evictions: number;
    declined: number;
    spared: number;
    meshesSkipped: number;
  } | null>(null);
  let strokes = $state(0);

  // ── mesh tidying ────────────────────────────────────────────────────
  //: Everything here is an INDEX-ONLY edit: a new index buffer over the same vertices. No attribute
  //: is read or rewritten, so skin weights and UVs cannot be damaged by construction. That is why
  //: this is safe to hand a user with a delete button. `compactVertices` is the one exception and is
  //: a separate, explicit press.
  let labels = $state.raw<ComponentLabels | null>(null);
  let originalIndex = $state.raw<IndexArray | null>(null);
  let selectedComponent = $state(-1);
  let hoveredComponent = $state(-1);
  /** The component the delete button is hovering over — drawn in the danger tone before the press. */
  let dangerComponent = $state(-1);
  let removedPieces = $state(0);
  let removedFaces = $state(0);
  let degenerateRemoved = $state(0);
  let compacted = $state(false);
  /** Triangles the mesh was decimated DOWN TO, or 0 for "never decimated". The achieved number, not
   *  the requested one — see `decimateReport`. */
  let decimatedTo = $state(0);
  /** A computed decimation, held so its report can be read BEFORE anything is applied. */
  let decimateReport = $state.raw<DecimateReport | null>(null);
  let decimateRatio = $state(0.5);
  /** Live progress of the interior-face pass, and the handle that cancels it. */
  let interiorProgress = $state.raw<{ done: number; total: number } | null>(null);
  let interiorAbort: AbortController | null = null;

  /**
   * A destructive bulk operation that has been COMPUTED but not run, so its victims can be drawn
   * first. "Remove hidden interior faces" used to delete blind, and `mesh-repair.ts`'s
   * `InteriorOptions` header warns that too few viewpoints punches visible holes in the model — so
   * the holes-to-be are shown and confirmed.
   *
   * ⚠️ `faces` and any captured index are only valid against the CURRENT index buffer, so every
   * other mesh operation is disabled while one is staged.
   */
  interface PendingOp {
    what: string;
    detail: string;
    /** Null when the exact faces could not be recovered; the confirm then shows a count only. */
    faces: Set<number> | null;
    run: () => void;
  }
  let pending = $state.raw<PendingOp | null>(null);

  // ── the UV atlas panel ──────────────────────────────────────────────
  /**
   * Faces the atlas asked to have drawn on the body — an island, or its degenerate triangles.
   *
   * ⚠️ `previewFaces` is compared BY IDENTITY in the stage and the mesh-repair previews already own
   * it. The arbitration is that this only reaches the prop while the atlas mode is the active one,
   * and a staged destructive operation still outranks it: what is about to be deleted must never be
   * hidden by a diagnostic highlight.
   */
  let uvFaces = $state.raw<ReadonlySet<number> | null>(null);
  let uvHoverFace = $state(-1);
  /**
   * Bumped whenever the atlas's findings stop describing the model, which REMOUNTS the panel.
   *
   * 🔴 Every face id changes on an index edit, so an island's face set, the hit grid and the
   * highlight are all dead — not merely stale. Coverage is measured against the texture's texels, so
   * a colour bake invalidates the drawn atlas too, though not the geometry findings. Remounting
   * throws the analysis away rather than showing numbers about a mesh that no longer exists.
   */
  let uvVersion = $state(0);

  // ── the A/B ghost ───────────────────────────────────────────────────
  /**
   * The DELIVERED model, drawn translucent in place behind the edited one. 0 is off.
   *
   * The workbench already knows the pristine path: `src` is what "Discard and reload" re-fetches,
   * and every edit here happens to the model in memory rather than to that file. So the same path
   * loaded a second time IS the before — which turns "did that repair actually help" from a memory
   * test into a look.
   */
  let ghostOpacity = $state(0);
  /**
   * ⚠️ A latch, because `ghostSrc` and `ghostOpacity` are two halves on purpose: the stage HIDES at
   * zero opacity and only frees the second GLB when the path is cleared. Passing the path
   * unconditionally would fetch and parse a second hero tier for every session that never asks for
   * the comparison; clearing it at zero would re-fetch on every flick of the A/B. So it is loaded
   * the first time the slider leaves zero and kept for the rest of the session.
   */
  let ghostArmed = $state(false);

  // ── texture colour and normal strength ──────────────────────────────
  //: 🔴 Deliberately NOT a "clean up texture" button. The engine's baker already dilates the gutter
  //: and inpaints interior holes, and re-dilating in the browser reintroduces a chart-bleed seam it
  //: caps its own fill to avoid. See the header of `texture-repair.ts`. Colour is the one thing the
  //: baker will never do, because it is faithful to the source photo on purpose.
  /**
   * 🔴 IDENTITY IS `IDENTITY_ADJUST`, NOT ALL-ZEROES. `ColourAdjust`'s contrast and saturation are
   * multipliers whose rest value is 1; the panel this replaces initialised all four to 0, which asks
   * `adjustColour` for zero contrast and zero saturation — a flat grey — and gets exactly that, from
   * the commit and from the GPU preview alike. The state below holds a real `ColourAdjust` in
   * `adjustColour`'s own units, so there is no second scale and therefore no conversion to get wrong
   * between the slider, the preview and the bake.
   */
  let colour = $state<Required<ColourAdjust>>({ ...IDENTITY_ADJUST });
  let colourApplied = $state(false);
  /**
   * ⚠️ True only while the CPU pass is being staged. `grade` is derived from it, so setting it takes
   * the GPU preview off BEFORE `adjustColour` repaints the texture underneath it — otherwise the
   * commit bakes texels the shader is still grading and the result is the grade applied twice.
   */
  let committing = $state(false);
  /** The delivered baseColor pixels, copied before the first adjust — same reasoning as the skin
   *  weights: an in-place edit with no cheap way back is an edit people are afraid to try. */
  let restoreImage = $state.raw<ImageData | null>(null);

  /** Normal detail multiplier. 1 is the map as the baker delivered it. */
  let normalStrength = $state(1);
  let normalApplied = $state(false);
  let restoreNormal = $state.raw<ImageData | null>(null);
  /**
   * 🔴 The material's OWN `normalScale`, captured before the slider ever writes to it. glTF's
   * `normalTexture.scale` is a real field and `GLTFLoader` copies it onto the material, so writing
   * the slider's value in absolutely would silently discard whatever the asset declared. The slider
   * is a multiplier ON this, and a bake puts exactly this back.
   */
  let baseNormalScale = 1;
  /** Which material `baseNormalScale` was read from, so a new model re-reads rather than inheriting
   *  the last one's. */
  let scaledMaterial: THREE.MeshStandardMaterial | null = null;

  // ── motion (takes) ──────────────────────────────────────────────────
  /** The clip `ModelStage` is actually showing, reported by `onClipChanged`. Repairs act on THAT
   *  clip, not on `animationClips[0]` — the stage auto-selects the skeleton-exercise track when one
   *  exists, and correcting a clip nobody is looking at is worse than useless. */
  let stageClip = $state('');
  let playing = $state(true);
  let frameNow = $state(0);
  let frameTotal = $state(0);
  let motionArms = $state(0);
  let motionLegs = $state(0);
  let motionScoped = $state(false);
  let motionBusy = $state('');
  let motionFound = $state.raw<Intersection[] | null>(null);
  /** The count from the PREVIOUS sweep, so a re-check after a correction can show it fall. That
   *  before/after is the only quality signal this path has, and it is not a verdict. */
  let motionPrevCount = $state<number | null>(null);
  /** Which contact row is selected, indexing `contacts`. -1 for none. */
  let selectedContact = $state(-1);
  /** Clips whose tracks currently carry a correction. A clip the sliders were dragged over but
   *  never applied to is NOT in here — nothing changed in it. */
  let motionApplied = $state<string[]>([]);

  /**
   * The two bones a selected contact lights up.
   *
   * ⚠️ Built HERE, from `handle.root.parent`, rather than inside `ModelStage` — which owns every
   * other overlay but does not expose this one. `overlays.ts`'s rule is that a proxy is added to the
   * SCENE and never parented to the model root (the stage's teardown disposes the character's
   * geometry through the root, and the save path finds the body by traversing it), and the scene is
   * exactly what `root.parent` is. It is rebuilt per model and disposed with it. The proper home is
   * a stage prop; see the note in the report.
   */
  let spotlight: BoneSpotlight | null = null;

  function disposeSpotlight(): void {
    spotlight?.dispose();
    spotlight = null;
  }

  onDestroy(() => {
    disposeSpotlight();
    brush?.dispose();
    brush = null;
    interiorAbort?.abort();
    history.clear();
  });

  // ── derived model facts ─────────────────────────────────────────────

  /** The mesh being tidied. Characters from this pipeline are one skinned mesh; if there are more,
   *  the panel says so rather than silently working on an arbitrary one. */
  const body = $derived(stage?.skinnedMeshes[0] ?? null);
  const extraMeshes = $derived(Math.max(0, (stage?.skinnedMeshes.length ?? 0) - 1));

  /** three's attribute names → the glTF semantics `glb-patch` writes back. Only the ones actually
   *  present are patched: naming an absent semantic makes the save throw. */
  const GLTF_SEMANTICS: Record<string, string> = {
    position: 'POSITION',
    normal: 'NORMAL',
    tangent: 'TANGENT',
    uv: 'TEXCOORD_0',
    uv1: 'TEXCOORD_1',
    color: 'COLOR_0',
    skinIndex: 'JOINTS_0',
    skinWeight: 'WEIGHTS_0'
  };
  const presentSemantics = $derived(
    Object.keys(body?.geometry.attributes ?? {})
      .map((k) => GLTF_SEMANTICS[k])
      .filter(Boolean)
  );
  /** ⚠️ `compactVertices` remaps morph targets too, but `glb-patch` has no morph-target path — so a
   *  character carrying them would save a mesh whose morphs point at the old vertex numbering. No
   *  asset from this pipeline has any; the guard is here because the failure would be silent. */
  const hasMorphs = $derived(Object.keys(body?.geometry.morphAttributes ?? {}).length > 0);

  /** ⚠️ `Mesh.material` is `Material | Material[]`. A multi-material body would need a slot picker
   *  and a per-slot patch, so the texture controls simply do not offer themselves there. */
  const baseMaterial = $derived.by(() => {
    const material = body?.material;
    if (!material || Array.isArray(material)) return null;
    return material as THREE.MeshStandardMaterial;
  });
  const hasBaseMap = $derived(!!baseMaterial?.map);
  const hasNormalMap = $derived(!!baseMaterial?.normalMap);

  /**
   * 🔴 Joint editing re-binds the skeleton at the CURRENT pose, so it is only ever correct at rest.
   * A GLB with baked tracks is an animated take, not a rig, and its bones are mid-clip — re-binding
   * one would freeze that pose into the bind and permanently deform the character. It is the one
   * unrecoverable mistake in this panel. The same condition governs `HoverPicker` and the BVH, which
   * are built over bind-space positions and answer about a model that is not the one on screen while
   * a clip plays.
   *
   * 🔴 And it is decided by WHAT THE LOADED MODEL IS, not by which chip was clicked: a saved
   * take-repair is reached through the edits row but still carries animation tracks.
   */
  const posed = $derived((stage?.animationClips.length ?? 0) > 0);

  const editRefusal = $derived.by(() => {
    if (!stage) return 'Load a model first.';
    if (posed)
      return (
        '🔴 This model carries baked animation tracks, so its bones are mid-clip rather than at ' +
        'rest. Every rig tool here re-binds or ray-casts against the rest pose, and re-binding a ' +
        'posed skeleton would freeze that pose into the character permanently. Motion mode is the ' +
        'one that belongs to a take; open a rig tier for the rest.'
      );
    // 🔴 `baseTier` is NOT "is this editable" — it is "which tier does a save get FILED against",
    // and those two came apart the moment the save became pluggable. A host with no persistence
    // (SPAR3D: `POST /api/v1/3d/generate` returns URLs, there is no edit endpoint and no asset
    // record) takes the download fallback, and a download needs no tier. Refusing on `!baseTier`
    // alone closed every rig tool on exactly the assets these tools exist for, and forced that panel
    // to pass a fake tier string to get its own editor back.
    if (onSave && !baseTier)
      return 'This view has no rig tier behind it, so a repair would have nothing to be saved against.';
    return '';
  });
  const canEdit = $derived(editRefusal === '');

  /** The clip on screen, and the pristine copy of its key values. `snapshots` is parallel to
   *  `animationClips`, so one index answers both. */
  const motionClipIndex = $derived.by(() => {
    const handle = stage;
    const name = stageClip;
    if (!handle || !name) return -1;
    return handle.animationClips.findIndex((c) => c.name === name);
  });
  const motionClip = $derived(
    motionClipIndex >= 0 ? (stage?.animationClips[motionClipIndex] ?? null) : null
  );
  const motionSnapshot = $derived(
    motionClipIndex >= 0 ? (stage?.snapshots[motionClipIndex] ?? null) : null
  );
  const motionReady = $derived(
    !!motionClip && !!motionSnapshot && !!stage?.skeleton && !!stage?.mixer
  );
  const motionTouched = $derived(motionArms !== 0 || motionLegs !== 0);
  const motionDirty = $derived(motionApplied.length > 0);
  const clipCorrected = $derived(motionApplied.includes(stageClip));

  function modeReady(needs: 'any' | 'rig' | 'take'): boolean {
    if (needs === 'any') return true;
    return needs === 'rig' ? canEdit : motionReady;
  }

  /**
   * ⚠️ `WorkMode` is a superset of `StageMode`, so the extra members have to name the stage
   * behaviour they want. Colour grades the live material and Motion drives the transport; neither
   * wants a picking mode, so both ride `view`.
   *
   * 🔴 The atlas rides `pieces`, which is not cosmetic: `onFacePicked` fires from ONE place in the
   * stage — the `pieces` click path — and a face id is exactly what the atlas needs to point at.
   * `labels` is withheld below in that mode, so the click reports the face and no piece is selected
   * or deleted by it.
   */
  const stageMode = $derived.by((): StageMode => {
    if (mode === 'colour' || mode === 'motion') return 'view';
    if (mode === 'uv') return 'pieces';
    return mode;
  });

  // Bones are mutated in place and are not reactive; `renameTick` is the dependency that makes every
  // read below re-run after an assignment. It is a DEPENDENCY, not a value.
  const coverage = $derived.by(() => {
    const tick = renameTick;
    return stage && tick >= 0 ? coreCoverage(stage.bones) : null;
  });

  /** The sanitized form of the prefix the engine's namer puts on every joint it could name. Derived
   *  through `sanitizeName` rather than typed out, because the colon is exactly what three deletes
   *  and a hand-typed `'mixamorig'` here would be a second copy of that rule to get out of step. */
  const NAMED_PREFIX = sanitizeName('mixamorig:');

  /** `mixamorigLeftForeArm` → `LeftForeArm`, and `mixamorig:LeftForeArm` → `LeftForeArm` too. The
   *  prefix is on every joint the engine named, so it carries no information in a list where every
   *  row has it — and both forms of the name reach this, since the rename log holds the canonical
   *  one and the bones hold the sanitized one. */
  function shortBone(name: string): string {
    const short = name.startsWith(NAMED_PREFIX) ? name.slice(NAMED_PREFIX.length) : name;
    return short.startsWith(':') ? short.slice(1) : short;
  }

  /**
   * 🔴 Three states, because one colour is why naming a joint was impossible to do by eye. `core`
   * fills one of the 22 slots the retarget matches on; `named` carries the engine's prefix but is
   * not a core slot (fingers, twist joints); `unnamed` is anonymous and is what a missing core slot
   * has to be claimed from.
   *
   * ⚠️ The `named` test is pipeline-specific on purpose: every joint this engine names carries the
   * `mixamorig:` prefix, and anything without it came out of the decoder with a generated label.
   */
  const jointStates = $derived.by(() => {
    const tick = renameTick;
    const bones = stage?.bones;
    if (!bones || tick < 0) return null;
    const core = new Set<string>(MIXAMO_CORE.map(sanitizeName));
    const gone = new Set(dissolved);
    return bones.map((b): JointState =>
      gone.has(b)
        ? 'dissolved'
        : core.has(b.name)
          ? 'core'
          : b.name.startsWith(NAMED_PREFIX)
            ? 'named'
            : 'unnamed'
    );
  });

  const selectedIndex = $derived.by(() => {
    const tick = renameTick;
    const bones = stage?.bones;
    if (!bones || !selectedJoint || tick < 0) return -1;
    return bones.findIndex((b) => b.name === selectedJoint);
  });
  const selectedBone = $derived(selectedIndex >= 0 ? (stage?.bones[selectedIndex] ?? null) : null);

  /**
   * The missing core slots the SELECTED joint could fill, best first.
   *
   * This is `coreCandidates` read backwards: for each slot it ranks every free bone by skeleton
   * topology, so this joint's position in that ranking is how good a claim it has. Rank 0 means it
   * hangs directly off the bone carrying the slot's anatomical parent — a far stronger signal than
   * any name or distance heuristic, and the reason the list is usually one entry long and right.
   */
  const jointSuggestions = $derived.by(() => {
    const tick = renameTick;
    const bones = stage?.bones;
    const bone = selectedBone;
    if (!bones || !bone || tick < 0) return [] as { core: string; rank: number }[];
    return coreCoverage(bones)
      .missing.map((core) => ({ core, rank: coreCandidates(bones, core).indexOf(bone) }))
      .filter((s) => s.rank >= 0)
      .sort((a, b) => a.rank - b.rank);
  });

  /**
   * The bone the selected joint's edits would mirror onto, or null.
   *
   * ⚠️ Null is ORDINARY: `Hips`, `Spine`, `Neck` and `Head` are mid-line and have nothing to mirror
   * onto, and a side-named bone whose opposite number is missing from the skeleton has none either.
   * Both mean "this one moves alone", which is worth saying before the drag rather than after.
   */
  const mirrorPartner = $derived.by(() => {
    const tick = renameTick;
    const bones = stage?.bones;
    const bone = selectedBone;
    if (!bones || !bone || tick < 0) return null;
    return mirrorBone(bones, bone);
  });

  /**
   * What every renamed joint is called now, keyed by its CURRENT sanitized name.
   *
   * Read off the bones rather than replayed from the `renames` log, so a joint renamed twice — an
   * anonymous one claimed as `LeftHand` and then swapped to `RightHand` — reports one rename from
   * the name it arrived with, which is the only one the file and the other tiers can act on.
   */
  const renameState = $derived.by((): Map<string, RenamedJoint> => {
    const tick = renameTick;
    const delivered = deliveredNames;
    const bones = stage?.bones;
    const out = new Map<string, RenamedJoint>();
    if (!bones || !delivered || tick < 0) return out;
    for (const bone of bones) {
      const from = delivered.get(bone);
      const to = (bone.userData?.name as string | undefined) ?? bone.name;
      if (from && from !== to) out.set(bone.name, { from, to });
    }
    return out;
  });

  /** Which way the character faces, and whether its Left/Right names agree with that.
   *
   * ⚠️ Depends on `renameTick`, not on `jointVersion`: renaming is what changes the answer, and a
   * dependency on joint moves would re-measure this on every pointermove of a drag. */
  const sides = $derived.by((): Chirality | null => {
    const tick = renameTick;
    const handle = stage;
    return handle && tick >= 0 ? chirality(handle.bones, handle.skeleton) : null;
  });

  /** Staged deletions, by the name they carry now — for the panel's summary lines. */
  const dissolvedNames = $derived.by((): string[] => {
    const tick = renameTick;
    return tick >= 0 ? dissolved.map((bone) => shortBone(bone.name)) : [];
  });

  /** The renames a sides swap would make. Counted for the button, applied by `swapSides`. */
  const swapPlan = $derived.by((): SideSwap[] => {
    const tick = renameTick;
    const bones = stage?.bones;
    return bones && tick >= 0 ? sideSwapPlan(bones) : [];
  });

  /** Joints currently off their delivered position. Re-reads whenever the stage reports a drag or a
   *  nudge lands, because bones are mutated in place and are not reactive. */
  const moved = $derived.by(() => {
    const version = jointVersion + jointNudges;
    return stage && jointRest && version >= 0 ? movedJoints(stage.bones, jointRest) : [];
  });
  const movedNames = $derived(new Set(moved.map((j) => j.name)));

  /** The model's longest dimension. Everything scale-relative below is a fraction of it, so a step
   *  or a penetration depth means the same on a 2-metre character and a 2-unit one. */
  const span = $derived(stage ? modelSpan(stage.root) : 0);
  /** One nudge = 0.5% of that, matching the arrow-key step `ModelStage` takes. */
  const nudgeStep = $derived(span * 0.005);
  /** The brush in world units, which is what both the brush and the stage's cursor ring take. */
  const brushRadius = $derived(span * brushScale);
  /** `smooth` needs no target; the other two rebind vertices ONTO a joint and cannot guess which. */
  const paintNeedsBone = $derived(paintTool === 'assign' || paintTool === 'erase');
  const paintRefusal = $derived.by(() => {
    if (!canEdit) return editRefusal;
    if (paintNeedsBone && !selectedBone) {
      return 'Pick a joint first — in Joints mode, or from the list below. Assign and erase rebind vertices onto a named joint, so there is nothing to paint without one.';
    }
    return '';
  });

  /** Components small enough to be plausible junk, smallest first. The largest component is the
   *  character and is never offered. */
  const strays = $derived.by(() => {
    const current = labels;
    if (!current) return [] as MeshComponent[];
    const largest = current.components[0]?.id;
    return current.components
      .filter((c) => c.id !== largest)
      .slice()
      .sort((a, b) => a.triangles - b.triangles);
  });
  const selectedPiece = $derived(
    labels?.components.find((c) => c.id === selectedComponent) ?? null
  );
  const hoveredPiece = $derived(labels?.components.find((c) => c.id === hoveredComponent) ?? null);

  const meshDirty = $derived(
    removedPieces > 0 || degenerateRemoved > 0 || removedFaces > 0 || compacted || decimatedTo > 0
  );
  const colourTouched = $derived(
    colour.exposure !== IDENTITY_ADJUST.exposure ||
      colour.contrast !== IDENTITY_ADJUST.contrast ||
      colour.saturation !== IDENTITY_ADJUST.saturation ||
      colour.temperature !== IDENTITY_ADJUST.temperature
  );
  /** Painting rewrites `skinIndex` as well as `skinWeight` — it REBINDS vertices, where a prune only
   *  zeroes weights — so both semantics have to be in the patch or the save keeps the old joints
   *  next to the new weights, which is a different rig from the one on screen. */
  //: A dissolve is a skin edit too — `transferInfluence` rewrites `skinIndex`, not just weights.
  const skinDirty = $derived(applied.length > 0 || strokes > 0 || dissolved.length > 0);
  const dirty = $derived(
    skinDirty ||
      renames.length > 0 ||
      moved.length > 0 ||
      meshDirty ||
      colourApplied ||
      normalApplied ||
      motionDirty
  );

  // ── what the stage draws ────────────────────────────────────────────

  /**
   * The brush footprint, as a per-vertex falloff field.
   *
   * `brushField` hands back one `Float32Array` per mesh shaped exactly for `HeatOverlay.setRatios`,
   * so the preview is drawn ON the surface — the only cursor that tells the truth about what a dab
   * reaches, since a ring on the screen says nothing about the far side of a limb.
   *
   * ⚠️ Each recompute allocates and re-uploads a float per vertex (1 MB at 250k), so it is NOT run
   * per pointer event: `onBrushHover` only moves `brushAt` once the cursor has travelled a fraction
   * of the radius, and this suppresses itself entirely mid-stroke, where the paint landing on the
   * model is the feedback and a second overlay would only fight it.
   */
  const brushFieldRatios = $derived.by(() => {
    const at = brushAt;
    const meshes = stage?.skinnedMeshes;
    if (mode !== 'paint' || painting || !at || !meshes?.length) return null;
    return brushField(meshes, at, brushRadius, paintProfile);
  });

  /**
   * ⚠️ Two different questions through one overlay. In `weights` mode it paints the measured
   * per-vertex leak against the prune threshold. In "show what it drives" it paints a flat mask: the
   * ratios ARE the mask, so every driven vertex reads as 1 against a threshold of 0.5 and burns, and
   * everything else is masked to nothing. Same shader, no second overlay class, and the two are
   * mutually exclusive because they answer about the same pixels.
   *
   * ⚠️ This OBJECT is rebuilt on every threshold change, because the threshold is on a slider.
   * `ratios` keeps its identity across those rebuilds — the stage must compare it by reference
   * before calling `setRatios`, or dragging the slider re-uploads a 250k-float channel per pointer
   * event. The threshold itself is a uniform and is the only thing meant to change per frame.
   */
  const heat = $derived.by(() => {
    // The brush footprint outranks both, and only while the cursor is on the surface: it is the
    // answer to "what am I about to change", which nothing else on screen can give.
    if (mode === 'paint' && brushFieldRatios) {
      // Threshold 1 makes the ramp the falloff itself — cool at the rim where a dab barely moves a
      // weight, burning at the centre where it moves it fully.
      return { ratios: brushFieldRatios, threshold: 1, mask: null };
    }
    if (drivingMask && drivingRatios) {
      return { ratios: drivingRatios, threshold: 0.5, mask: drivingMask };
    }
    if (mode === 'weights' && report) {
      return { ratios: report.vertexRatios, threshold, mask: null };
    }
    return null;
  });

  /**
   * 🔴 Reads the four fields, and returns a FRESH object. Returning `colour` itself would make this
   * derived depend on the state object's identity alone — a slider move mutates a property signal
   * this would never have read, so the derived would not invalidate and the GPU preview would sit
   * frozen at whatever the first drag produced.
   */
  const grade = $derived.by((): ColourAdjust | null => {
    if (committing || colourApplied) return null;
    const next = {
      exposure: colour.exposure,
      contrast: colour.contrast,
      saturation: colour.saturation,
      temperature: colour.temperature
    };
    return colourTouched ? next : null;
  });

  /** ⚠️ The hover half is gated on the mode as well as on the id: the popover unmounts when the mode
   *  changes without ever firing its own pointerleave, so without this a red "about to be deleted"
   *  highlight survives into a mode that has no delete button. */
  const previewFaces = $derived.by(() => {
    if (pending?.faces) return pending.faces;
    if (mode === 'pieces' && dangerComponent >= 0 && labels) {
      return facesOfComponents(new Set([dangerComponent]));
    }
    // The atlas only reaches this prop in its own mode, and never over a staged delete. See
    // `uvFaces`: the set is compared by identity and the mesh-repair previews own it everywhere else.
    if (mode === 'uv') return uvFaces;
    return null;
  });

  // ── session lifecycle ───────────────────────────────────────────────

  /**
   * 🔴 Drop everything the moment the viewer is pointed somewhere else. Loading is async, so without
   * this the editor would spend the gap measuring the PREVIOUS model and saving the result against
   * the new one. `lastSrc` is a plain `let`: it is a change detector, not state anything renders.
   *
   * 🔴 The history goes too. Every entry closes over three.js objects from the old model — a bone, a
   * geometry, a material — so after a reload its undo would write into a mesh that is no longer on
   * screen, silently and with no error.
   */
  let lastSrc = '';
  $effect(() => {
    const next = src;
    if (next === lastSrc) return;
    lastSrc = next;
    // ⚠️ `untrack`, because `resetSession` bumps `renameTick` — a read-modify-write, which would
    // otherwise make this effect depend on a counter it also changes and schedule a second pass for
    // nothing. `src` is the only thing this effect is watching.
    untrack(() => {
      disposeSpotlight();
      stage = null;
      jointRest = null;
      resetSession();
    });
  });

  /** A mode collapses to View the moment the model stops being the kind it needs. */
  $effect(() => {
    const m = mode;
    const entry = MODES.find((x) => x.id === m);
    if (entry && !modeReady(entry.needs)) mode = 'view';
  });

  /**
   * The heavy pass is what the mode is FOR, so entering it runs it once rather than making the user
   * find a button that is the only thing the panel can do. Latched on plain `let`s, not state: these
   * are one-shot guards, and making them reactive would feed the effect its own writes.
   */
  let autoAnalysed = false;
  let autoLabelled = false;
  $effect(() => {
    const m = mode;
    if (m === 'weights' && !autoAnalysed && stage) {
      autoAnalysed = true;
      analyse();
    }
    if (m === 'pieces' && !autoLabelled && body) {
      autoLabelled = true;
      analyseMesh();
    }
  });

  /**
   * In Paint mode, keep "what it drives" lit for the joint being painted.
   *
   * 🔴 This is the answer to "it painted something and I cannot see what changed". A dab moves
   * numbers in two attributes; the only visible consequence is which vertices the joint now owns,
   * and `refreshDrives` already re-reads that mask after every write. Lighting it automatically
   * turns each stroke into a territory that visibly grows or shrinks, instead of a tally in a panel
   * on the other side of the screen.
   *
   * ⚠️ `untrack` around the write: `showDrives` sets `drivingJoint`, which this effect reads to
   * decide whether it needs to run at all. Without it the effect feeds on its own output.
   */
  $effect(() => {
    const m = mode;
    const name = selectedJoint;
    if (m !== 'paint' || !name || !stage) return;
    untrack(() => {
      if (drivingJoint !== name) showDrives(name);
    });
  });

  /**
   * The spotlight belongs to Motion mode and to a selected contact, and to nothing else.
   *
   * ⚠️ `BoneSpotlight.set` is O(bones × vertices) — it re-scans every mesh per bone — so it is only
   * ever called on a selection or a mode change, never from a slider or the render loop.
   *
   * ⚠️ `stage` is read for its DEPENDENCY, not its value: `spotlight` is a plain `let` holding a
   * three.js object, so nothing else here would re-run this against the instance built for a freshly
   * loaded model.
   */
  $effect(() => {
    const m = mode;
    const contact = selectedContact;
    const handle = stage;
    if (!handle || !spotlight) return;
    if (m !== 'motion') spotlight.set(null);
    spotlight.visible = m === 'motion' && contact >= 0;
  });

  /**
   * The live normal strength.
   *
   * 🔴 Once the weakened texels are IN the image, the multiplier goes back to 1 or the model renders
   * `strength²` — the shader scales what the bake already scaled. `NormalHud`'s header states the
   * contract and this is the owner's half of it. `applyNormal` also writes it synchronously, because
   * this effect does not flush until after the caller's next statement.
   */
  $effect(() => {
    const material = baseMaterial;
    const value = normalApplied ? 1 : normalStrength;
    if (!material?.normalMap) return;
    if (material !== scaledMaterial) {
      scaledMaterial = material;
      baseNormalScale = material.normalScale.x;
    }
    const scale = baseNormalScale * value;
    material.normalScale.set(scale, scale);
  });

  function resetSession(): void {
    mode = 'view';
    report = null;
    preview = null;
    applied = [];
    restoreWeights = null;
    renames = [];
    renameTick++;
    deliveredNames = null;
    dissolved = [];
    selectedJoint = '';
    hoveredJoint = '';
    clearDrives();
    pickedFace = -1;
    mirror = null;
    symmetric = false;
    mirrorMode = 'delta';
    symmetryDiag = null;
    symmetryStale = false;
    proposals = null;
    brush?.dispose();
    brush = null;
    brushAt = null;
    painting = false;
    strokeTally = null;
    // The sampled joint belongs to the model that just went away, and so does the tool state that
    // one-shot pick was on its way back to.
    picked = null;
    paintTool = 'pick';
    toolBeforePick = 'assign';
    strokes = 0;
    uvFaces = null;
    uvHoverFace = -1;
    uvVersion++;
    ghostOpacity = 0;
    ghostArmed = false;
    labels = null;
    originalIndex = null;
    selectedComponent = -1;
    hoveredComponent = -1;
    dangerComponent = -1;
    pending = null;
    removedPieces = 0;
    removedFaces = 0;
    degenerateRemoved = 0;
    compacted = false;
    decimatedTo = 0;
    decimateReport = null;
    interiorAbort?.abort();
    interiorAbort = null;
    interiorProgress = null;
    colour = { ...IDENTITY_ADJUST };
    colourApplied = false;
    committing = false;
    restoreImage = null;
    normalStrength = 1;
    normalApplied = false;
    restoreNormal = null;
    scaledMaterial = null;
    stageClip = '';
    playing = true;
    frameNow = 0;
    frameTotal = 0;
    motionArms = 0;
    motionLegs = 0;
    motionScoped = false;
    motionBusy = '';
    motionFound = null;
    motionPrevCount = null;
    selectedContact = -1;
    motionApplied = [];
    pendingEdits = null;
    propagated = [];
    saved = '';
    error = '';
    busy = '';
    meshBusy = '';
    autoAnalysed = false;
    autoLabelled = false;
    history.clear();
    historyTick++;
  }

  function onStageReady(handle: EditorStageHandle): void {
    stage = handle;
    // The delivered local positions, captured before anything can move — this is what "put every
    // joint back" restores to, and it must be taken from the freshly loaded model.
    jointRest = captureJointRest(handle.bones);
    deliveredNames = new Map(
      handle.bones.map((bone) => [bone, (bone.userData?.name as string | undefined) ?? bone.name])
    );
    selectedJoint = '';
    selectedComponent = -1;
    hoveredComponent = -1;
    jointVersion++;
    renameTick++;
    disposeSpotlight();
    // See the declaration: the scene is `root.parent`, and a proxy must be a sibling of the model
    // rather than a child of it.
    const scene = handle.root.parent;
    if (scene) spotlight = new BoneSpotlight(handle.skinnedMeshes, scene);
    brush?.dispose();
    brush = new WeightBrush(handle.skinnedMeshes);
    // The mirror plane is a fact about this skeleton, so it is read once, here — and read from the
    // BIND pose via the skeleton rather than the live transforms, which is the honest reading on a
    // take and identical on a rig.
    mirror = detectMirrorAxis(handle.bones, handle.skeleton);
    symmetryDiag = symmetryReport(handle.bones, mirror, handle.skeleton);
    symmetryStale = false;
    // A take arrives with exactly one useful mode, so open it rather than making the user find it.
    mode = handle.animationClips.length > 0 ? 'motion' : 'view';
    onReady?.(handle);
  }

  // ── skin weights ────────────────────────────────────────────────────

  function analyse(): void {
    const handle = stage;
    if (!handle) return;
    busy = 'Measuring every skin influence…';
    error = '';
    // Deferred a frame so the button visibly changes before a pass over ~250k vertices blocks the
    // main thread. This is a few hundred ms, not a job — a worker would cost a structured copy of
    // the whole geometry to save less than that.
    setTimeout(() => {
      try {
        report = analyseSkin(handle.skinnedMeshes);
        refreshPreview();
      } catch (e) {
        fail(e);
      } finally {
        busy = '';
      }
    }, 16);
  }

  function refreshPreview(): void {
    if (!stage || !report) return;
    preview = previewPrune(stage.skinnedMeshes, threshold);
  }

  /** Re-measure after any write to the weight buffers, so the distribution and the preview describe
   *  what is actually in the model. Shared by the forward pass and by undo. */
  function remeasureWeights(handle: EditorStageHandle): void {
    handle.refresh();
    report = analyseSkin(handle.skinnedMeshes);
    refreshPreview();
    // The drives mask is a fact about the weights, and the weights just changed under it.
    refreshDrives();
  }

  function applyWeightPrune(): void {
    const handle = stage;
    if (!handle) return;
    busy = 'Pruning…';
    error = '';
    setTimeout(() => {
      try {
        // The session-wide DELIVERED copy, for the coarse button. Captured once, exactly as before.
        restoreWeights ??= copyWeights(handle.skinnedMeshes);
        // 🔴 And a second copy, of the state THIS prune is about to change. Pushing an entry that
        // restored `restoreWeights` instead would reverse every earlier prune too, while the list
        // still showed them applied.
        const before = copyWeights(handle.skinnedMeshes);
        const beforeApplied = applied;
        const ratio = threshold;
        const result = applyPrune(handle.skinnedMeshes, ratio);
        if (result.prunedInfluences === 0) {
          refreshPreview();
          return;
        }
        applied = [...applied, result];
        const afterApplied = applied;
        remeasureWeights(handle);
        history.push({
          label: `Pruned ${result.prunedInfluences.toLocaleString()} influences past ${(
            ratio * 100
          ).toFixed(0)}%`,
          weight: weightBytes(before),
          undo: () => {
            writeWeights(handle.skinnedMeshes, before);
            applied = beforeApplied;
            remeasureWeights(handle);
          },
          // Deterministic from the state `undo` produced — which is exactly the state this prune
          // ran against — so the redo re-derives rather than holding a second 3.5 MB copy.
          redo: () => {
            applyPrune(handle.skinnedMeshes, ratio);
            applied = afterApplied;
            remeasureWeights(handle);
          }
        });
        historyTick++;
      } catch (e) {
        fail(e);
      } finally {
        busy = '';
      }
    }, 16);
  }

  /**
   * Put the DELIVERED skin back — deliberately coarser than the stack, and pushed onto it so the two
   * never disagree about what the model holds.
   *
   * ⚠️ It reverses BRUSH STROKES as well as prunes, because it restores both skin attributes
   * wholesale. That is what the button has to do to be correct — half a restore is a skin that never
   * existed — and it is why the label and the confirm copy name the strokes.
   */
  function undoPrune(): void {
    const handle = stage;
    const delivered = restoreWeights;
    if (!handle || !delivered) return;
    const before = copyWeights(handle.skinnedMeshes);
    const beforeApplied = applied;
    const beforeStrokes = strokes;
    writeWeights(handle.skinnedMeshes, delivered);
    applied = [];
    strokes = 0;
    remeasureWeights(handle);
    refreshDrives();
    history.push({
      label: 'Put the delivered skin back',
      weight: weightBytes(before),
      undo: () => {
        writeWeights(handle.skinnedMeshes, before);
        applied = beforeApplied;
        strokes = beforeStrokes;
        remeasureWeights(handle);
        refreshDrives();
      },
      redo: () => {
        writeWeights(handle.skinnedMeshes, delivered);
        applied = [];
        strokes = 0;
        remeasureWeights(handle);
        refreshDrives();
      }
    });
    historyTick++;
  }

  // ── the brush ───────────────────────────────────────────────────────

  /**
   * A stroke begins.
   *
   * 🔴 The DELIVERED skin is copied here too, once per session, for the same reason the prune does
   * it: an in-place edit with no cheap way back is an edit people are afraid to try. The per-stroke
   * undo below is three to four orders of magnitude smaller and is what the history actually holds.
   */
  function onBrushBegin(): void {
    const handle = stage;
    if (!handle || !brush || paintRefusal) return;
    // `pick` writes nothing, so it takes none of this: no delivered copy, no stroke, no tally. It
    // acts on the first dab and hands the brush back.
    if (paintTool === 'pick') return;
    restoreWeights ??= copyWeights(handle.skinnedMeshes);
    brush.beginStroke();
    painting = true;
    strokeTally = { dabs: 0, vertices: 0, evictions: 0, declined: 0, spared: 0, meshesSkipped: 0 };
  }

  /**
   * One dab, at a world-space point on the surface. The stage throttles these to one per frame.
   *
   * ⚠️ The surface normal goes in too, and it matters: a brush is a SPHERE, and without a normal to
   * cull against, painting the front of a thigh paints the back of it as well. The stage flips the
   * normal toward the camera before handing it over, which is exactly what `frontFacing` wants.
   */
  function onBrushDab(point: THREE.Vector3, normal: THREE.Vector3): void {
    // Read once: `pick` reassigns `paintTool` on its way out, and the dab below must be the tool
    // this event arrived for. It also gives the brush a `PaintMode`, which is what `dab` takes.
    const tool = paintTool;
    if (tool === 'pick') {
      pickJointUnder(point);
      return;
    }
    if (!brush || !painting) return;
    const result = brush.dab({
      point,
      normal,
      frontFacing: true,
      radius: brushRadius,
      mode: tool,
      bone: selectedBone,
      strength: paintStrength,
      profile: paintProfile
    });
    const tally = strokeTally;
    if (!tally) return;
    strokeTally = {
      dabs: tally.dabs + 1,
      vertices: tally.vertices + result.vertices,
      evictions: tally.evictions + result.evictions,
      declined: tally.declined + result.declined,
      spared: tally.spared + result.spared,
      meshesSkipped: tally.meshesSkipped + result.meshesSkipped
    };
  }

  /**
   * Select the joint that actually drives the surface under the brush.
   *
   * 🔴 The gesture this mode was missing. Painting needs a target joint, the target was only
   * settable in Joints mode, and a joint handle floating inside the mesh is a poor way to say
   * "whatever moves THIS". So the brush answers it from the skin itself: sum every influence over
   * the vertices under the cursor and take the joint carrying the most weight.
   *
   * ⚠️ Summed over the whole footprint rather than read off the nearest vertex, because one vertex
   * at a seam can be bound to something its neighbours are not — which is exactly the defect the
   * brush exists to repair, and precisely the wrong thing to steer by.
   *
   * The share comes back with it: `LeftHand · 62% of the weight here` says both which joint and how
   * contested the patch is, and a low number over a limb is itself the finding.
   */
  function pickJointUnder(point: THREE.Vector3): void {
    const handle = stage;
    if (!handle) return;
    const perMesh = verticesInBrush(handle.skinnedMeshes, point, brushRadius);
    const mass = new Map<number, number>();
    let total = 0;
    let counted = 0;
    perMesh.forEach((ids, m) => {
      const mesh = handle.skinnedMeshes[m];
      if (!mesh) return;
      counted += ids.length;
      for (const v of ids) {
        const influences = readInfluences(mesh, v);
        for (let k = 0; k < influences.bones.length; k++) {
          const weight = influences.weights[k];
          mass.set(influences.bones[k], (mass.get(influences.bones[k]) ?? 0) + weight);
          total += weight;
        }
      }
    });
    let best = -1;
    let bestMass = 0;
    for (const [bone, weight] of mass) {
      if (weight > bestMass) {
        bestMass = weight;
        best = bone;
      }
    }
    const bone = best >= 0 ? handle.bones[best] : null;
    if (!bone || total <= 0) {
      picked = null;
      error =
        'Nothing under the brush is bound to a joint — widen the radius, or aim at the character.';
      return;
    }
    error = '';
    selectedJoint = bone.name;
    picked = { name: bone.name, share: bestMass / total, vertices: counted };
    // One click, then out of the way: `pick` is a question, and leaving the brush unable to paint
    // after it has been answered is the mode dance this was meant to end.
    paintTool = toolBeforePick;
  }

  /**
   * The stroke ends: ONE history entry for the whole gesture, holding only the vertices it touched.
   *
   * A 5,000-vertex stroke on the common `Uint8`/`Uint8` layout is ~60 KB per side against 3.5 MB for
   * a full-attribute copy, so `strokeByteLength` is the honest weight and the byte budget can do its
   * job. `captureLike` re-reads exactly those vertices AFTER the stroke, which is the redo half.
   */
  function onBrushEnd(): void {
    const handle = stage;
    if (paintTool === 'pick') return;
    if (!brush || !painting) return;
    painting = false;
    const before = brush.commitStroke();
    if (!handle || !before) return;
    const after = captureLike(before);
    afterSkinWrite(handle);
    strokes++;
    const target = paintTool === 'smooth' ? 'the seam' : shortBone(selectedJoint);
    history.push({
      label: `Painted ${paintTool} over ${before.vertices.toLocaleString()} vertices of ${target}`,
      weight: strokeByteLength(before) + strokeByteLength(after),
      undo: () => {
        restoreStroke(before);
        strokes--;
        afterSkinWrite(handle);
      },
      redo: () => {
        restoreStroke(after);
        strokes++;
        afterSkinWrite(handle);
      }
    });
    historyTick++;
  }

  /**
   * ⚠️ The cursor moves constantly and the field costs a float per vertex, so this only moves the
   * anchor once the pointer has travelled far enough for the footprint to look different. The ring
   * the stage draws follows the real pointer; this is the SURFACE preview behind it.
   */
  function onBrushHover(point: THREE.Vector3 | null): void {
    if (!point) {
      brushAt = null;
      return;
    }
    const at = brushAt;
    if (at && at.distanceTo(point) < brushRadius * 0.15) return;
    // Cloned: the stage owns the vector it reports and is free to reuse it next frame.
    brushAt = point.clone();
  }

  /**
   * 🔴 The skin attributes just changed, so everything measured FROM them is stale.
   *
   * `refresh()` re-uploads them; the report and its preview described the skin as it was, and a
   * stale distribution on screen is worse than none — the same call `compact()` makes, for the same
   * reason, so the mode re-measures when next opened. The drives mask is recomputed rather than
   * dropped: watching a joint's territory grow under the brush is the point of painting.
   */
  function afterSkinWrite(handle: EditorStageHandle): void {
    handle.refresh();
    report = null;
    preview = null;
    autoAnalysed = false;
    refreshDrives();
  }

  // ── joints ──────────────────────────────────────────────────────────

  /** The popover names the joint it is anchored to; the review list names one it picked out. */
  function assign(coreName: string): void {
    const bone = selectedBone;
    if (bone) assignBone(bone, coreName);
  }

  function assignBone(bone: THREE.Bone, coreName: string): void {
    if (!canEdit) return;
    const fromCanonical = (bone.userData?.name as string | undefined) ?? bone.name;
    const fromSanitized = bone.name;
    const beforeRenames = renames;
    const beforeDriving = drivingJoint;
    renames = [...renames, { from: fromCanonical, to: coreName }];
    // 🔴 Two names, deliberately. The BONE gets the sanitized form, because three's own track
    // binding parses `.` and `:` as path syntax and a colon in `bone.name` would make every
    // animation channel targeting it unresolvable. The CANONICAL form is what the engine's
    // name-based retarget matches on, and it reaches the file through `renames` and the byte-patch
    // save — never through `bone.name`, which `GLTFExporter` would strip again on the way out.
    const toSanitized = sanitizeName(coreName);
    renameBone(bone, toSanitized, coreName);
    const afterRenames = renames;
    history.push({
      label: `Named ${shortBone(fromSanitized)} as ${coreName}`,
      undo: () => {
        renameBone(bone, fromSanitized, fromCanonical);
        renames = beforeRenames;
        drivingJoint = beforeDriving;
      },
      redo: () => {
        renameBone(bone, toSanitized, coreName);
        renames = afterRenames;
      }
    });
    historyTick++;
  }

  /** Why this joint cannot be dissolved, in a sentence, or '' if it can. Drawn in the popover in
   *  place of the button, because a disabled button that never says why is a dead end. */
  function dissolveRefusalFor(bone: THREE.Bone): string {
    const parent = bone.parent as THREE.Bone | null;
    if (!parent?.isBone) {
      return 'This is the root joint — there is no parent to take its weights or its children, so it cannot be dissolved.';
    }
    if (jointStates?.[stage?.bones.indexOf(bone) ?? -1] === 'core') {
      return `This joint fills the ${shortBone(bone.name)} core slot, and deleting it would leave that slot empty — no humanoid clip would drive this part of the body again. Rename it onto another slot first if it is genuinely in the wrong place.`;
    }
    return '';
  }

  /**
   * Delete a joint: move everything it drives onto its parent, then stage it for removal.
   *
   * # Why it is two halves
   *
   * A joint with weights cannot simply be cut. Every vertex it drives would have to go somewhere,
   * and "somewhere" is a decision, not a detail — a vertex left with no influence at all is the one
   * failure mode the engine itself refuses to ship. So the weights move FIRST, onto the parent,
   * which is the joint whose motion the doomed one was riding anyway. Only then is there nothing
   * left pointing at it.
   *
   * ⚠️ Its child joints are re-hung on the same parent with `attach`, which preserves their world
   * position — so a stray joint in the middle of a chain can go without the chain below it moving.
   *
   * 🔴 The mesh does not move. The weight transfer is a rebinding, not a deformation: a vertex that
   * was 100% on this joint is now 100% on the parent, and it sits exactly where it did until
   * something animates. What changes is what it will do THEN.
   */
  function dissolveJoint(bone: THREE.Bone): void {
    const handle = stage;
    if (!handle || !canEdit) return;
    const parent = bone.parent as THREE.Bone | null;
    if (!parent?.isBone) {
      fail(
        new Error(
          `"${shortBone(bone.name)}" is a root joint — it has no parent to take its weights or its ` +
            `children, so there is nothing to dissolve it into.`
        )
      );
      return;
    }
    const index = handle.bones.indexOf(bone);
    const parentIndex = handle.bones.indexOf(parent);
    if (index < 0 || parentIndex < 0) {
      fail(new Error('that joint is not part of this skeleton'));
      return;
    }
    if (dissolved.includes(bone)) return;

    const children = bone.children.filter(
      (c): c is THREE.Bone => (c as THREE.Bone).isBone === true
    );
    // 🔴 The file learns a re-hung child's new place from its node TRANSLATION alone — `applyJoints`
    // writes `setTranslation` and nothing else. A rotation or scale on the joint being dissolved
    // folds into its children's LOCAL transform when they move up, and that half would never reach
    // the file. `glb-patch` refuses such a save; refusing here means it is refused before the edit
    // rather than after, with the reason at the joint the user is looking at.
    const spun =
      Math.abs(bone.quaternion.w) < 0.999999 ||
      Math.abs(bone.quaternion.x) > 1e-6 ||
      Math.abs(bone.quaternion.y) > 1e-6 ||
      Math.abs(bone.quaternion.z) > 1e-6;
    const scaled =
      Math.abs(bone.scale.x - 1) > 1e-6 ||
      Math.abs(bone.scale.y - 1) > 1e-6 ||
      Math.abs(bone.scale.z - 1) > 1e-6;
    if (children.length && (spun || scaled)) {
      fail(
        new Error(
          `"${shortBone(bone.name)}" carries a rotation or a scale and has ${children.length} joint(s) ` +
            `below it. Re-hanging them on its parent would move them, and only their positions ` +
            `reach the file. Refusing rather than deforming the character.`
        )
      );
      return;
    }

    const transfer = transferInfluence(handle.skinnedMeshes, index, parentIndex);
    // ⚠️ `attach` preserves a child's WORLD transform, and reads `matrixWorld` to do it — so the
    // matrices have to be current going in, not merely on the way out.
    handle.root.updateMatrixWorld(true);
    for (const child of children) parent.attach(child);
    handle.root.updateMatrixWorld(true);

    // 🔴 A re-hung child has a new LOCAL position and the same world position — it did not move, its
    // parent went away. So its delivered baseline moves with it, or `movedJoints` reports the whole
    // limb as edited and "put every joint back" writes local positions that belonged to a parent
    // this joint no longer has, which would fling it across the character.
    const beforeRest = jointRest;
    if (beforeRest && children.length) {
      const next = new Map(beforeRest);
      for (const child of children) next.set(child, child.position.clone());
      jointRest = next;
    }
    const afterRest = jointRest;

    const beforeDissolved = dissolved;
    dissolved = [...dissolved, bone];
    const afterDissolved = dissolved;
    if (selectedJoint === bone.name) selectedJoint = '';
    if (drivingJoint === bone.name) clearDrives();
    renameTick++;
    remeasureWeights(handle);

    history.push({
      label: `Dissolved ${shortBone(bone.name)} into ${shortBone(parent.name)}${
        transfer.vertices ? ` (${transfer.vertices.toLocaleString()} vertices moved)` : ''
      }`,
      weight: transfer.before.bytes,
      undo: () => {
        restoreStroke(transfer.before);
        handle.root.updateMatrixWorld(true);
        for (const child of children) bone.attach(child);
        handle.root.updateMatrixWorld(true);
        jointRest = beforeRest;
        dissolved = beforeDissolved;
        renameTick++;
        remeasureWeights(handle);
      },
      redo: () => {
        transferInfluence(handle.skinnedMeshes, index, parentIndex);
        handle.root.updateMatrixWorld(true);
        for (const child of children) parent.attach(child);
        handle.root.updateMatrixWorld(true);
        jointRest = afterRest;
        dissolved = afterDissolved;
        renameTick++;
        remeasureWeights(handle);
      }
    });
    historyTick++;
  }

  /**
   * Rename all sixteen sided core joints onto their opposite number, in ONE step.
   *
   * 🔴 This is a PERMUTATION and has to be applied as one. `renameBone` moves the selection and the
   * drives paint along with the bone they name, which is right for a single rename and wrong here:
   * halfway through the loop the selection would have been carried from `LeftArm` to `RightArm` and
   * then carried a second time, by the bone that was `RightArm`, back to `LeftArm`. So the whole
   * mapping is computed first and every read of it is against the names as they were.
   *
   * ⚠️ The undo is the SAME operation with the plan reversed, because swapping sides is its own
   * inverse — not a stack of sixteen individual renames, which a Ctrl+Z could unwind halfway and
   * leave a character with two left arms.
   */
  function applySideSwap(plan: SideSwap[]): void {
    const moves = new Map(plan.map((swap) => [swap.bone.name, sanitizeName(swap.to)]));
    for (const swap of plan) {
      swap.bone.userData = { ...swap.bone.userData, name: swap.to };
      swap.bone.name = sanitizeName(swap.to);
    }
    selectedJoint = moves.get(selectedJoint) ?? selectedJoint;
    hoveredJoint = moves.get(hoveredJoint) ?? hoveredJoint;
    drivingJoint = moves.get(drivingJoint) ?? drivingJoint;
    // The pairs the report holds are named, and those names just moved. The geometry did not, so
    // this is stale labelling rather than a stale measurement — but showing it would be a lie.
    symmetryStale = true;
    renameTick++;
  }

  function swapSides(): void {
    const handle = stage;
    if (!handle || !canEdit) return;
    const plan = swapPlan;
    if (!plan.length) return;
    const back = plan.map((swap) => ({ bone: swap.bone, from: swap.to, to: swap.from }));
    const beforeRenames = renames;
    const afterRenames = [...renames, ...plan.map((swap) => ({ from: swap.from, to: swap.to }))];
    renames = afterRenames;
    applySideSwap(plan);
    history.push({
      label: `Swapped left and right on ${plan.length} joints`,
      undo: () => {
        applySideSwap(back);
        renames = beforeRenames;
      },
      redo: () => {
        applySideSwap(plan);
        renames = afterRenames;
      }
    });
    historyTick++;
  }

  /** ⚠️ The selection and the drives paint are both held BY NAME, and the name just changed under
   *  them — so they move with it or they silently point at a bone that no longer answers. */
  function renameBone(bone: THREE.Bone, sanitized: string, canonical: string): void {
    if (drivingJoint === bone.name) drivingJoint = sanitized;
    if (selectedJoint === bone.name) selectedJoint = sanitized;
    bone.name = sanitized;
    bone.userData = { ...bone.userData, name: canonical };
    renameTick++;
  }

  /** The bones `moveJointWorld` touches: the joint, and the direct children it counter-translates. */
  function jointFamily(bone: THREE.Bone): THREE.Bone[] {
    return [
      bone,
      ...bone.children.filter((c): c is THREE.Bone => (c as THREE.Bone).isBone === true)
    ];
  }

  /**
   * Record a joint edit as a pair of positions, which is all one is.
   *
   * `captureJointRest` is a map of `bone → Vector3` and `resetJoints` writes it back and
   * re-binds — the same two calls the reset button makes, over three bones instead of sixty. A
   * negated delta would ALMOST invert `moveJointWorld`, and "almost" is not what an undo is for.
   */
  function pushJointEdit(
    label: string,
    touched: THREE.Bone[],
    before: JointRest,
    coalesce: string | null
  ): void {
    const handle = stage;
    if (!handle) return;
    const after = captureJointRest(touched);
    history.push({
      label,
      coalesce,
      // Three floats per bone. Declaring anything here would evict real history for arithmetic.
      undo: () => {
        resetJoints(handle.root, handle.skinnedMeshes, touched, before);
        jointNudges++;
      },
      redo: () => {
        resetJoints(handle.root, handle.skinnedMeshes, touched, after);
        jointNudges++;
      }
    });
    historyTick++;
  }

  /**
   * One joint move — and its mirror, when symmetric editing is on.
   *
   * 🔴 BOTH SIDES, ONE HISTORY ENTRY. `captureJointRest` is taken over the union of the two
   * families before anything moves, so a single Ctrl+Z puts the whole edit back; two entries would
   * leave the character half-mirrored, which is worse than the defect being repaired.
   *
   * 🔴 And both sides get the SAME `keepChildren`. `moveJointWorld(keepChildren: true)`
   * counter-translates direct children to hold them still, so mirroring with a different setting
   * would move one elbow and re-proportion the other forearm.
   *
   * ⚠️ The mirrored move is COMPUTED FIRST. `mirrorJointMove` reads both joints' current positions,
   * and applying this side first would have it measure against a skeleton that had already moved.
   */
  function applyJointMove(bone: THREE.Bone, delta: THREE.Vector3, coalesce: string | null): void {
    const handle = stage;
    if (!handle || !canEdit) return;
    const plane = mirror;
    const other =
      symmetric && plane && mirrorReady
        ? mirrorJointMove(handle.bones, bone, delta, plane, mirrorMode, handle.skeleton)
        : null;

    const touched = jointFamily(bone);
    if (other) {
      for (const b of jointFamily(other.bone)) if (!touched.includes(b)) touched.push(b);
    }
    const before = captureJointRest(touched);

    moveJointWorld(handle.root, handle.skinnedMeshes, bone, delta, keepChildren);
    if (other) {
      moveJointWorld(handle.root, handle.skinnedMeshes, other.bone, other.delta, keepChildren);
    }
    jointNudges++;
    symmetryStale = true;
    const label = other
      ? `Moved ${shortBone(bone.name)} and ${shortBone(other.bone.name)}`
      : `Moved ${shortBone(bone.name)}`;
    pushJointEdit(label, touched, before, coalesce);
  }

  function nudge(axis: 0 | 1 | 2, sign: 1 | -1): void {
    const bone = selectedBone;
    if (!bone) return;
    const delta = new THREE.Vector3();
    delta.setComponent(axis, sign * nudgeStep);
    applyJointMove(bone, delta, gestureKey('nudge', bone.name));
  }

  /**
   * Put ONE joint back where the engine delivered it.
   *
   * ⚠️ Its direct children go with it. `moveJointWorld(keepChildren: true)` counter-translates them
   * to hold them still, so resetting the parent alone would carry those counter-offsets forward and
   * leave the children displaced — the reset would visibly break what it was meant to repair. This
   * is exactly the set `moveJointWorld` touches.
   */
  function resetJoint(): void {
    const handle = stage;
    const bone = selectedBone;
    const rest = jointRest;
    if (!handle || !bone || !rest) return;
    const touched = jointFamily(bone);
    const before = captureJointRest(touched);
    resetJoints(handle.root, handle.skinnedMeshes, touched, rest);
    jointNudges++;
    symmetryStale = true;
    pushJointEdit(`Put ${shortBone(bone.name)} back`, touched, before, null);
  }

  function undoJoints(): void {
    const handle = stage;
    const rest = jointRest;
    if (!handle || !rest) return;
    const touched = handle.bones;
    const before = captureJointRest(touched);
    resetJoints(handle.root, handle.skinnedMeshes, touched, rest);
    jointNudges++;
    symmetryStale = true;
    pushJointEdit('Put every joint back', touched, before, null);
  }

  /**
   * 🔴 THE CALLER APPLIES THE DRAG. `ModelStage` reports an incremental world-space delta and moves
   * nothing itself — its `keepChildren` prop only captions the gesture. In the panel this replaces
   * NOBODY applied it: the parent's handler found the bone and then only bumped a counter, so the
   * drag gesture was silently inert and the nudge buttons were the one working path. This is that
   * bug fixed.
   *
   * 🔴 What must NOT go here is `stage.refresh()`. That flags skinIndex/skinWeight for re-upload —
   * ~3.5 MB of attribute traffic on a hero tier — and a drag fires this on every pointermove.
   * Nothing about a joint move touches those buffers: `moveJointWorld` re-binds and runs
   * `skeleton.update()` itself, and the skeleton overlay re-reads bone matrices every frame.
   *
   * ⚠️ One gesture is hundreds of these, so the entry COALESCES: the stack keeps the first push's
   * `undo` and the last push's `redo`, which is the state either side of the drag.
   */
  function onJointDrag(name: string, delta: THREE.Vector3): void {
    const bone = stage?.bones.find((b) => b.name === name);
    if (bone) applyJointMove(bone, delta, gestureKey('drag', name));
  }

  /** One arrow-key step. Separate from the drag so it is one entry rather than one of hundreds. */
  function onJointNudge(name: string, delta: THREE.Vector3): void {
    const bone = stage?.bones.find((b) => b.name === name);
    if (bone) applyJointMove(bone, delta, gestureKey('nudge', name));
  }

  /** Re-measure the asymmetry. Explicit, because it re-pairs every bone against every other one. */
  function measureSymmetry(): void {
    const handle = stage;
    const plane = mirror;
    if (!handle || !plane) return;
    symmetryDiag = symmetryReport(handle.bones, plane, handle.skeleton);
    symmetryStale = false;
  }

  /** Switch brush tool. `pick` is a one-shot detour, so it remembers what it interrupted. */
  function selectTool(tool: BrushTool): void {
    if (tool === 'pick' && paintTool !== 'pick') toolBeforePick = paintTool;
    // ^ `paintTool` is narrowed to `PaintMode` by the guard: it cannot be 'pick' in that branch.
    if (tool !== 'pick') picked = null;
    paintTool = tool;
  }

  /** Paint the vertices the selected joint actually moves. This is the check on a name being right:
   *  a joint claiming to be the left hand had better light up the left hand. */
  function toggleDrives(): void {
    if (drivingJoint === selectedJoint) {
      clearDrives();
      return;
    }
    showDrives(selectedJoint);
  }

  function showDrives(name: string): void {
    const handle = stage;
    if (!handle || !name) return;
    const index = handle.bones.findIndex((b) => b.name === name);
    if (index < 0) return;
    const mask = boneVertexMask(handle.skinnedMeshes, index);
    drivingJoint = name;
    drivingMask = mask;
    // The heat shader ramps a per-vertex ratio; feeding it the mask as the ratio too turns it into a
    // flat "these vertices" paint against a threshold of 0.5.
    drivingRatios = mask.map((m) => Float32Array.from(m));
  }

  /** Re-read the mask after anything wrote to the skin. A mask taken before a stroke describes the
   *  territory the joint USED to have, which is precisely the thing the stroke changed. */
  function refreshDrives(): void {
    if (drivingJoint) showDrives(drivingJoint);
  }

  function clearDrives(): void {
    drivingJoint = '';
    drivingMask = null;
    drivingRatios = null;
  }

  /** Jump to the best-ranked candidate for a missing slot and open its options. The inverse of the
   *  popover's list, for when you know which slot is missing but not which joint it should be. */
  function focusCandidate(coreName: string): void {
    const bones = stage?.bones;
    if (!bones) return;
    const best = coreCandidates(bones, coreName)[0];
    if (!best) return;
    mode = 'joints';
    selectedJoint = best.name;
    clearDrives();
  }

  // ── naming every missing slot at once ───────────────────────────────

  /** One proposed name, waiting to be looked at. NOT applied — see `proposeNames`. */
  interface NameProposal {
    core: string;
    bone: THREE.Bone;
    /** Where this bone came in `coreCandidates`' ranking for the slot. 0 is "hangs directly off the
     *  bone carrying the slot's anatomical parent", which is the only strong signal there is. */
    rank: number;
  }
  let proposals = $state.raw<NameProposal[] | null>(null);

  /**
   * Rank a best guess for every missing core slot.
   *
   * 🔴 A REVIEW LIST, NEVER AN APPLICATION. Naming a joint that is not anatomically where its name
   * claims retargets CLEANLY and animates WRONGLY, and nothing in this pipeline validates the claim
   * — the engine matches on the exact string and asks no further questions. Doing that once by hand
   * is a risk the user took deliberately; doing it eight times in one press is the same risk
   * multiplied and taken by the tool. So every row has to be looked at, and "Show what it drives" is
   * the look: click it and watch the left hand light up, or watch a shin light up and reject it.
   *
   * ⚠️ A bone is proposed for at most ONE slot. `coreCandidates` ranks each slot independently, so
   * the same anonymous joint is routinely the best answer for two of them — accepting both would
   * rename it twice and leave a slot silently unfilled.
   */
  function proposeNames(): void {
    const bones = stage?.bones;
    if (!bones) return;
    const taken = new Set<THREE.Bone>();
    const out: NameProposal[] = [];
    for (const core of coreCoverage(bones).missing) {
      const ranked = coreCandidates(bones, core);
      const rank = ranked.findIndex((b) => !taken.has(b));
      if (rank < 0) continue;
      taken.add(ranked[rank]);
      out.push({ core, bone: ranked[rank], rank });
    }
    proposals = out;
  }

  /** Look at one proposal on the model: select it, light up what it drives, fly the camera to it. */
  function inspectProposal(proposal: NameProposal): void {
    const handle = stage;
    mode = 'joints';
    selectedJoint = proposal.bone.name;
    showDrives(proposal.bone.name);
    if (handle) handle.focusOn(proposal.bone.getWorldPosition(new THREE.Vector3()), span * 0.12);
  }

  /** Accept ONE row. One name, one history entry, one deliberate press. */
  function acceptProposal(proposal: NameProposal): void {
    assignBone(proposal.bone, proposal.core);
    dropProposal(proposal);
  }

  function dropProposal(proposal: NameProposal): void {
    const list = proposals;
    if (!list) return;
    const next = list.filter((p) => p !== proposal);
    proposals = next.length ? next : null;
  }

  // ── mesh tidying ────────────────────────────────────────────────────

  function analyseMesh(): void {
    const mesh = body;
    if (!mesh) return;
    meshBusy = 'Finding separate pieces…';
    setTimeout(() => {
      try {
        originalIndex ??= (mesh.geometry.getIndex()?.array.slice() ?? null) as IndexArray | null;
        labels = labelComponents(mesh.geometry);
        selectedComponent = -1;
        hoveredComponent = -1;
      } catch (e) {
        fail(e);
      } finally {
        meshBusy = '';
      }
    }, 16);
  }

  /** Every triangle belonging to any of these components. */
  function facesOfComponents(ids: ReadonlySet<number>): Set<number> {
    const out = new Set<number>();
    const current = labels;
    if (!current) return out;
    for (let f = 0; f < current.faceComponent.length; f++) {
      if (ids.has(current.faceComponent[f])) out.add(f);
    }
    return out;
  }

  /**
   * Which triangles a survivor index dropped, so a bulk operation can be DRAWN before it is run.
   *
   * Both producers here — `removeDegenerateTriangles` and meshopt's `simplifyPrune` — emit the
   * survivors in their original order, so one greedy forward walk recovers the removed set exactly.
   * 🔴 If that ever stops being true the walk fails to consume every survivor; returning null then
   * makes the caller confirm on a COUNT rather than light up triangles that are not the ones about
   * to go. (Two bit-identical adjacent triangles of which one is dropped resolve to the first — the
   * same geometry either way, a different face number in the highlight. Nothing acts on it.)
   */
  function droppedFaces(
    current: ArrayLike<number>,
    survivors: ArrayLike<number>
  ): Set<number> | null {
    const removed = new Set<number>();
    const faces = current.length / 3;
    let s = 0;
    for (let f = 0; f < faces; f++) {
      const a = current[f * 3];
      const b = current[f * 3 + 1];
      const c = current[f * 3 + 2];
      if (
        s < survivors.length &&
        survivors[s] === a &&
        survivors[s + 1] === b &&
        survivors[s + 2] === c
      ) {
        s += 3;
      } else {
        removed.add(f);
      }
    }
    return s === survivors.length ? removed : null;
  }

  /** The counters the dirty summary and the save patch read, captured as a unit so an undo can put
   *  the tally back alongside the index it describes. */
  interface MeshTally {
    pieces: number;
    faces: number;
    degenerates: number;
    decimatedTo: number;
  }

  function meshTally(): MeshTally {
    return {
      pieces: removedPieces,
      faces: removedFaces,
      degenerates: degenerateRemoved,
      decimatedTo
    };
  }

  function setMeshTally(tally: MeshTally): void {
    removedPieces = tally.pieces;
    removedFaces = tally.faces;
    degenerateRemoved = tally.degenerates;
    decimatedTo = tally.decimatedTo;
  }

  /**
   * Install a new index buffer and put every derived answer back in step.
   *
   * 🔴 RE-LABELS AFTERWARDS, AND THAT IS NOT OPTIONAL. `removeComponents` preserves the order of the
   * survivors but renumbers nothing — so once the index shrinks, `faceComponent` is indexed by a
   * triangle numbering that no longer exists. A raycast then resolves to the WRONG component
   * silently (the stale array is merely longer, so no bounds check fires), and a second delete
   * throws on the length guard. Re-labelling is ~50–150 ms at 220k vertices; correctness is worth
   * far more than that here, because the failure mode is "deleted the wrong part of the character".
   *
   * 🔴 And it tells the stage, for the same reason: `SurfaceOverlay` borrows the body's attributes
   * and the `HoverPicker`'s BVH was built over the old numbering too.
   */
  function installIndex(mesh: THREE.SkinnedMesh, index: IndexArray): void {
    applyIndex(mesh, index);
    selectedComponent = -1;
    hoveredComponent = -1;
    dangerComponent = -1;
    pending = null;
    labels = labelComponents(mesh.geometry);
    invalidateStageGeometry();
    // 🔴 EVERY FACE ID IS DEAD, not merely stale — decimation does not even preserve triangle
    // ORDER. A picked face, the atlas's island sets and its hit grid all name triangles that no
    // longer exist, and the atlas would go on reporting coverage about a mesh that is gone. Cleared
    // and remounted rather than reused; `labels` above is the same rule for the component map.
    pickedFace = -1;
    uvFaces = null;
    uvHoverFace = -1;
    uvVersion++;
    // 🔴 And the brush's cached class-level ADJACENCY was built from the old index buffer. Its grid
    // is keyed on the position attribute, which an index-only edit does not touch, so nothing else
    // would notice — and a `smooth` stroke would then average each vertex with the neighbours it
    // had before the edit.
    disposeWeightPaintCache(mesh.geometry);
  }

  /**
   * `build` is a thunk rather than an array so that the index construction — which throws on a stale
   * label set, by design — is inside the same try as the install.
   *
   * ⚠️ Every install gets its OWN copy of the array. `applyIndex` wraps what it is handed in a live
   * `BufferAttribute`, so an entry that kept the same array would be holding a buffer the geometry
   * owns, and a later edit to it would rewrite history.
   */
  function commitIndex(label: string, build: () => IndexArray, tally: () => void): void {
    const mesh = body;
    if (!mesh) return;
    try {
      originalIndex ??= (mesh.geometry.getIndex()?.array.slice() ?? null) as IndexArray | null;
      const before = mesh.geometry.getIndex()?.array.slice() as IndexArray | undefined;
      const beforeTally = meshTally();
      const after = build();
      installIndex(mesh, after.slice() as IndexArray);
      tally();
      const afterTally = meshTally();
      if (!before) return;
      history.push({
        label,
        // Both arrays are held for the life of the entry: the "before" one always, and the "after"
        // one on its own once the step has been undone and the geometry no longer carries it.
        weight: before.byteLength + after.byteLength,
        undo: () => {
          installIndex(mesh, before.slice() as IndexArray);
          setMeshTally(beforeTally);
        },
        redo: () => {
          installIndex(mesh, after.slice() as IndexArray);
          setMeshTally(afterTally);
        }
      });
      historyTick++;
    } catch (e) {
      fail(e);
    }
  }

  function dropSelected(): void {
    const mesh = body;
    const current = labels;
    if (!mesh || !current || selectedComponent < 0) return;
    const piece = current.components.find((c) => c.id === selectedComponent);
    const ids = new Set([selectedComponent]);
    commitIndex(
      `Deleted a ${(piece?.triangles ?? 0).toLocaleString()}-triangle piece`,
      () => removeComponents(mesh.geometry, current, ids),
      () => {
        removedPieces += 1;
      }
    );
  }

  function stageDropUnder(ratio: number): void {
    const mesh = body;
    const current = labels;
    if (!mesh || !current) return;
    const doomed = strays.filter((c) => c.sizeRatio < ratio);
    if (!doomed.length) {
      failMessage(`No piece is smaller than ${(ratio * 100).toFixed(0)}% of the model.`);
      return;
    }
    const ids = new Set(doomed.map((c) => c.id));
    const tris = doomed.reduce((n, c) => n + c.triangles, 0);
    pending = {
      what: `Delete ${ids.size} piece${ids.size === 1 ? '' : 's'} under ${(ratio * 100).toFixed(0)}%`,
      detail: `${tris.toLocaleString()} triangles across ${ids.size} connected pieces.`,
      faces: facesOfComponents(ids),
      run: () =>
        commitIndex(
          `Deleted ${ids.size} piece${ids.size === 1 ? '' : 's'} under ${(ratio * 100).toFixed(0)}%`,
          () => removeComponents(mesh.geometry, current, ids),
          () => {
            removedPieces += ids.size;
          }
        )
    };
  }

  /** meshoptimizer's own isolated-component prune. Different affordance from picking a piece: it is
   *  threshold-driven, so it clears dust in one press without anyone having to find each speck. */
  function stageDropDust(): void {
    const mesh = body;
    if (!mesh) return;
    meshBusy = 'Pruning small parts…';
    setTimeout(() => {
      void (async () => {
        try {
          const before = mesh.geometry.getIndex()?.array as ArrayLike<number> | undefined;
          if (!before) throw new Error('this mesh has no index buffer');
          const next = await pruneIsolated(mesh.geometry, 0.02);
          const gone = (before.length - next.length) / 3;
          if (gone <= 0) {
            failMessage('meshoptimizer found no isolated parts to prune at 2%.');
            return;
          }
          pending = {
            what: 'Prune isolated dust',
            detail: `${gone.toLocaleString()} triangles in parts smaller than 2% of the model.`,
            faces: droppedFaces(before, next),
            run: () =>
              commitIndex(
                `Pruned ${gone.toLocaleString()} triangles of isolated dust`,
                () => next,
                () => {
                  removedFaces += gone;
                }
              )
          };
        } catch (e) {
          fail(e);
        } finally {
          meshBusy = '';
        }
      })();
    }, 16);
  }

  function stageDegenerates(): void {
    const mesh = body;
    if (!mesh) return;
    meshBusy = 'Checking every triangle…';
    setTimeout(() => {
      try {
        const before = mesh.geometry.getIndex()?.array as ArrayLike<number> | undefined;
        if (!before) throw new Error('this mesh has no index buffer');
        const next = removeDegenerateTriangles(mesh.geometry);
        const gone = (before.length - next.length) / 3;
        if (gone <= 0) {
          failMessage('No zero-area or collapsed triangles in this mesh.');
          return;
        }
        pending = {
          what: 'Strip degenerate triangles',
          detail:
            `${gone.toLocaleString()} zero-area or collapsed triangles. They render as nothing ` +
            'and break tangent generation with a divide by zero.',
          faces: droppedFaces(before, next),
          run: () =>
            commitIndex(
              `Stripped ${gone.toLocaleString()} degenerate triangles`,
              () => next,
              () => {
                degenerateRemoved += gone;
              }
            )
        };
      } catch (e) {
        fail(e);
      } finally {
        meshBusy = '';
      }
    }, 16);
  }

  /**
   * Faces buried inside another shell — invisible, but paying full file size and z-fighting where
   * two surfaces are near-coincident. Index-only, so the rig is untouched.
   *
   * 🔴 No `setTimeout` wrapper. That existed only so the busy label could paint before a
   * multi-second block; the pass is chunked now and hands the event loop back after ~10 ms, which is
   * what makes the progress bar move and the Cancel button reachable at all.
   *
   * 🔴 On cancel it REJECTS rather than returning what it had, deliberately: a half-finished pass
   * yields a strict subset of the interior faces that is indistinguishable from a complete answer,
   * and the UI would then offer "N faces are buried" about a meaningless number. The signal is what
   * tells a cancel from a real failure — the reason is whatever `abort()` was handed, so sniffing
   * the error would be guessing. A plain `Error` means the geometry changed underneath it.
   */
  async function stageInterior(): Promise<void> {
    const mesh = body;
    if (!mesh || interiorAbort) return;
    const controller = new AbortController();
    interiorAbort = controller;
    meshBusy = 'Casting rays from every side…';
    interiorProgress = { done: 0, total: 0 };
    error = '';
    try {
      const hidden = await interiorFaces(mesh, {
        signal: controller.signal,
        onProgress: (done, total) => {
          interiorProgress = { done, total };
        }
      });
      if (hidden.size === 0) {
        failMessage('Nothing is fully buried — every face is reachable from outside.');
        return;
      }
      pending = {
        what: 'Remove hidden interior faces',
        detail:
          `${hidden.size.toLocaleString()} faces no ray from any of 64 viewpoints could reach. ` +
          'Look at the highlight first: deep concavities — armpits, under the chin, between the ' +
          'fingers, inside the ear — are what get miscalled, and applying that punches visible ' +
          'holes in the model.',
        faces: hidden,
        run: () =>
          commitIndex(
            `Removed ${hidden.size.toLocaleString()} hidden interior faces`,
            () => indexWithout(mesh.geometry, hidden),
            () => {
              removedFaces += hidden.size;
            }
          )
      };
    } catch (e) {
      if (!controller.signal.aborted) fail(e);
    } finally {
      interiorAbort = null;
      interiorProgress = null;
      meshBusy = '';
    }
  }

  /** A new index over the same vertices, minus a set of triangles. Keeps the source array's type: it
   *  already addressed every vertex, and dropping triangles cannot need a wider one. */
  function indexWithout(geometry: THREE.BufferGeometry, faces: ReadonlySet<number>): IndexArray {
    const index = geometry.getIndex();
    if (!index) throw new Error('this mesh has no index buffer');
    const src = index.array as ArrayLike<number>;
    const triangles = index.count / 3;
    const out = new (src.constructor as Uint32ArrayConstructor)(
      (triangles - faces.size) * 3
    ) as IndexArray;
    let w = 0;
    for (let f = 0; f < triangles; f++) {
      if (faces.has(f)) continue;
      out[w++] = src[f * 3];
      out[w++] = src[f * 3 + 1];
      out[w++] = src[f * 3 + 2];
    }
    return out;
  }

  /**
   * Compute a decimation and STOP, so its report can be read before anything is applied.
   *
   * 🔴 `metTarget: false` is ROUTINE here and is not an error. meshoptimizer stops the moment the
   * next collapse would break topology, and these characters are ~200 interpenetrating shells —
   * every shell a closed border it will not collapse across. So the panel below leads with the
   * ACHIEVED triangle count; reporting "decimated to 25%" over a result that came back at 91% is
   * exactly what the report exists to prevent.
   *
   * ⚠️ Index-only, like every other mesh tool here: a new index buffer over the same vertices, so
   * skin weights and UVs cannot be damaged by it. What it CAN do is leave a joint driving nothing,
   * which is why `report.rig` is surfaced.
   */
  async function stageDecimate(): Promise<void> {
    const mesh = body;
    if (!mesh || decimateReport || !!meshBusy) return;
    meshBusy = 'Simplifying…';
    error = '';
    try {
      decimateReport = await decimate(mesh.geometry, { ratio: decimateRatio });
    } catch (e) {
      fail(e);
    } finally {
      meshBusy = '';
    }
  }

  function applyDecimation(): void {
    const mesh = body;
    const report = decimateReport;
    if (!mesh || !report) return;
    decimateReport = null;
    if (report.unchanged) return;
    const kept = report.outputTriangles;
    commitIndex(
      `Decimated to ${kept.toLocaleString()} triangles (${(report.achievedRatio * 100).toFixed(0)}% of what was there)`,
      () => report.index,
      () => {
        decimatedTo = kept;
      }
    );
  }

  /** Joint ids from `starvedJoints` are indices into the BODY's skeleton, which is where the names
   *  come from. A starved joint is a "look at this", not an error — leaf and helper bones drive
   *  nothing legitimately. */
  function starvedNames(ids: number[]): string[] {
    const bones = body?.skeleton?.bones ?? [];
    return ids.map((id) => shortBone(bones[id]?.name ?? `joint ${id}`));
  }

  /**
   * Drop vertices no surviving triangle references. 🔴 The one mesh operation that rewrites every
   * attribute rather than just the index — which is why it is a separate, explicit button and not
   * part of deleting a piece, and why it goes on the history as a BARRIER rather than an entry.
   *
   * 🔴 It RENUMBERS EVERY VERTEX. `originalIndex`, `SkinReport.vertexRatios`, the drives mask, the
   * component labels and every captured skin-weight copy are all indexed BY vertex, so each earlier
   * entry now addresses a numbering that no longer exists. Undoing one would not throw — it would
   * quietly draw triangles between unrelated vertices, or write someone else's weights. A barrier
   * releases them and stays in the list carrying the reason, which is the difference between an undo
   * stack and a corruption engine.
   */
  function compact(): void {
    const mesh = body;
    if (!mesh || hasMorphs) return;
    meshBusy = 'Compacting…';
    setTimeout(() => {
      try {
        compactVertices(mesh.geometry);
        refreshBounds(mesh);
        compacted = true;
        labels = labelComponents(mesh.geometry);
        selectedComponent = -1;
        hoveredComponent = -1;
        pending = null;
        invalidateStageGeometry();
        // The atlas's findings are per FACE and per vertex, and both numberings just changed.
        pickedFace = -1;
        uvFaces = null;
        uvHoverFace = -1;
        uvVersion++;
        // The stale measurements go with the numbering they were taken against. The overlay's own
        // length check would leave them cold rather than paint someone else's numbers, but a stale
        // measurement on screen is worse than none — so the mode re-measures when next opened.
        report = null;
        preview = null;
        clearDrives();
        restoreWeights = null;
        autoAnalysed = false;
        history.pushBarrier(
          'Dropped orphaned vertices',
          'Compacting renumbers every vertex, and the steps before it were recorded against the ' +
            'old numbering — putting one back would draw triangles between unrelated vertices. ' +
            'Discard and reload is the way back past this point.'
        );
        historyTick++;
      } catch (e) {
        fail(e);
      } finally {
        meshBusy = '';
      }
    }, 16);
  }

  /**
   * Put the delivered index back.
   *
   * 🔴 Refused once the vertices have been compacted. `originalIndex` addresses the DELIVERED vertex
   * numbering, and `compactVertices` renumbered every one of them — installing it afterwards would
   * not throw, it would silently draw triangles between unrelated vertices. Discard-and-reload is
   * the way back from a compaction.
   */
  function restoreMesh(): void {
    const delivered = originalIndex;
    if (!delivered || compacted) return;
    commitIndex(
      'Put the delivered mesh back',
      () => delivered.slice() as IndexArray,
      () => setMeshTally({ pieces: 0, faces: 0, degenerates: 0, decimatedTo: 0 })
    );
  }

  /** ⚠️ `-1` is a real value here, not a miss to ignore: the stage reports a click on empty space as
   *  `(-1, -1)`, and letting it through is how the popover gets closed by eye. */
  function onPickComponent(id: number): void {
    selectedComponent = id;
    dangerComponent = -1;
  }

  // ── the UV atlas ────────────────────────────────────────────────────

  /**
   * Take the camera to a face the atlas pointed at.
   *
   * 🔴 The centroid is `bindMatrix · p`, NOT `matrixWorld · p`. `GLTFLoader` binds with the identity
   * and three's `AttachedBindMode` makes `bindMatrixInverse` cancel `matrixWorld`, so the rest
   * surface sits at `bindMatrix · p` — the same rule `mesh-repair`'s `restToWorld` documents and
   * `raycastFace` applies internally. Using `matrixWorld` misses by exactly the node transform,
   * invisibly on every asset whose mesh node happens to sit on the origin.
   */
  function focusFace(faceIndex: number): void {
    const mesh = body;
    const handle = stage;
    if (!mesh || !handle || faceIndex < 0) return;
    const index = mesh.geometry.getIndex();
    const position = mesh.geometry.getAttribute('position');
    if (!index || !position || faceIndex * 3 + 2 >= index.count) return;
    const centroid = new THREE.Vector3();
    const corner = new THREE.Vector3();
    for (let k = 0; k < 3; k++) {
      const v = index.getX(faceIndex * 3 + k);
      centroid.add(corner.set(position.getX(v), position.getY(v), position.getZ(v)));
    }
    centroid.multiplyScalar(1 / 3);
    const toWorld =
      mesh.isSkinnedMesh && mesh.bindMode === THREE.AttachedBindMode
        ? mesh.bindMatrix
        : mesh.matrixWorld;
    handle.focusOn(centroid.applyMatrix4(toWorld), span * 0.06);
  }

  // ── texture colour ──────────────────────────────────────────────────

  /**
   * A slider moved. If a bake is already in the texture, put the delivered pixels back first: the
   * GPU preview grades whatever is in the map, so previewing over a committed bake would show — and
   * then, on the next Apply, actually produce — the grade applied twice.
   */
  function onColourInput(): void {
    if (!colourApplied) return;
    const map = baseMaterial?.map;
    if (map && restoreImage) applyToTexture(map, restoreImage);
    colourApplied = false;
  }

  function applyColour(): void {
    const map = baseMaterial?.map;
    if (!map) return;
    meshBusy = 'Adjusting…';
    // 🔴 The GPU preview comes off SYNCHRONOUSLY and BEFORE the CPU pass, or the commit bakes texels
    // the shader is still grading and the result is the grade applied twice. Setting `grade` to null
    // — which `committing` does — has the same effect, but only once Svelte flushes the effect, and
    // that is after this statement. `detachGrade()` is the version with no ordering to get wrong;
    // `committing` is what stops the prop re-attaching it on the next flush.
    committing = true;
    stage?.detachGrade();
    // The 16 ms defer is so the button visibly changes before 4.19 M texels block the main thread.
    setTimeout(() => {
      try {
        // 🔴 Always adjusted from the DELIVERED pixels, never from the current ones — otherwise the
        // sliders compound and dragging one back does not undo it.
        const delivered = (restoreImage ??= textureToImageData(map));
        const graded = { ...colour };
        applyToTexture(map, adjustColour(delivered, graded));
        colourApplied = true;
        // The atlas draws these texels under its layout, so its picture is now the old one. Its
        // geometry findings are untouched — coverage is about the UV fill rule, not the colour.
        uvVersion++;
        history.push({
          label: 'Baked the colour grade',
          // The entry recomputes from `restoreImage`, which the session holds for its whole life —
          // so it pins no `ImageData` of its own and declaring 16 MB here would evict real history
          // to make room for a number.
          undo: () => {
            applyToTexture(map, delivered);
            colourApplied = false;
          },
          redo: () => {
            applyToTexture(map, adjustColour(delivered, graded));
            colour = { ...graded };
            colourApplied = true;
          }
        });
        historyTick++;
      } catch (e) {
        fail(e);
      } finally {
        committing = false;
        meshBusy = '';
      }
    }, 16);
  }

  function undoColour(): void {
    const map = baseMaterial?.map;
    const delivered = restoreImage;
    if (!map || !delivered) return;
    const wasApplied = colourApplied;
    const wasColour = { ...colour };
    applyToTexture(map, delivered);
    colour = { ...IDENTITY_ADJUST };
    colourApplied = false;
    if (!wasApplied) return;
    history.push({
      label: 'Put the delivered colour back',
      undo: () => {
        applyToTexture(map, adjustColour(delivered, wasColour));
        colour = { ...wasColour };
        colourApplied = true;
      },
      redo: () => {
        applyToTexture(map, delivered);
        colour = { ...IDENTITY_ADJUST };
        colourApplied = false;
      }
    });
    historyTick++;
  }

  // ── normal strength ─────────────────────────────────────────────────

  /** Same rule as the colour input: a live multiplier must never scale texels a bake already
   *  weakened, so a committed bake drops back to the delivered pixels first. */
  function onNormalInput(): void {
    if (!normalApplied) return;
    const map = baseMaterial?.normalMap;
    if (map && restoreNormal) applyToTexture(map, restoreNormal);
    normalApplied = false;
  }

  function applyNormal(): void {
    const map = baseMaterial?.normalMap;
    const material = baseMaterial;
    if (!map || !material) return;
    meshBusy = 'Weakening the normal map…';
    // 🔴 The multiplier goes back to the DELIVERED value SYNCHRONOUSLY, before the bake writes the
    // weakened texels. The effect that mirrors `normalApplied` does the same thing, but only after
    // Svelte flushes — which is after this function returns — and until then the shader would be
    // scaling texels the bake has already scaled, showing the strength SQUARED. Exactly the
    // ordering `applyColour` solves with `detachGrade()`.
    material.normalScale.set(baseNormalScale, baseNormalScale);
    setTimeout(() => {
      try {
        const delivered = (restoreNormal ??= textureToImageData(map));
        const strength = normalStrength;
        applyToTexture(map, setNormalStrength(delivered, strength));
        normalApplied = true;
        history.push({
          label: `Baked normal strength ${strength.toFixed(2)}`,
          undo: () => {
            applyToTexture(map, delivered);
            normalApplied = false;
          },
          redo: () => {
            applyToTexture(map, setNormalStrength(delivered, strength));
            normalStrength = strength;
            normalApplied = true;
          }
        });
        historyTick++;
      } catch (e) {
        fail(e);
      } finally {
        meshBusy = '';
      }
    }, 16);
  }

  function undoNormal(): void {
    const map = baseMaterial?.normalMap;
    const delivered = restoreNormal;
    if (!map || !delivered) return;
    const wasApplied = normalApplied;
    const wasStrength = normalStrength;
    applyToTexture(map, delivered);
    normalStrength = 1;
    normalApplied = false;
    if (!wasApplied) return;
    history.push({
      label: 'Put the delivered normal detail back',
      undo: () => {
        applyToTexture(map, setNormalStrength(delivered, wasStrength));
        normalStrength = wasStrength;
        normalApplied = true;
      },
      redo: () => {
        applyToTexture(map, delivered);
        normalStrength = 1;
        normalApplied = false;
      }
    });
    historyTick++;
  }

  // ── motion ──────────────────────────────────────────────────────────

  /**
   * One run of frames where a limb is inside a body part, which is the unit a human thinks in.
   *
   * `detectIntersections` reports per frame and per pair, so a two-second contact is ~50 rows of the
   * same finding. Collapsing them keeps the list clickable and puts the frame worth looking at —
   * the deepest one — on each row.
   */
  interface Contact {
    limb: string;
    body: string;
    from: number;
    to: number;
    frames: number;
    peakFrame: number;
    peakDepth: number;
  }

  /** ⚠️ A run survives a gap of up to this many frames. Contacts flicker across the slack threshold
   *  at their edges, and splitting on a single clean frame would report one event as three. */
  const CONTACT_GAP = 2;

  const contacts = $derived.by((): Contact[] => {
    const found = motionFound;
    if (!found?.length) return [];
    const byPair = new Map<string, Intersection[]>();
    for (const hit of found) {
      const key = `${hit.limb}|${hit.body}`;
      const list = byPair.get(key);
      if (list) list.push(hit);
      else byPair.set(key, [hit]);
    }
    const out: Contact[] = [];
    for (const list of byPair.values()) {
      list.sort((a, b) => a.frame - b.frame);
      let run: Intersection[] = [];
      const flush = () => {
        if (!run.length) return;
        let peak = run[0];
        for (const hit of run) if (hit.depth > peak.depth) peak = hit;
        out.push({
          limb: peak.limb,
          body: peak.body,
          from: run[0].frame,
          to: run[run.length - 1].frame,
          frames: new Set(run.map((h) => h.frame)).size,
          peakFrame: peak.frame,
          peakDepth: peak.depth
        });
        run = [];
      };
      for (const hit of list) {
        if (run.length && hit.frame - run[run.length - 1].frame > CONTACT_GAP) flush();
        run.push(hit);
      }
      flush();
    }
    return out.sort((a, b) => b.peakDepth - a.peakDepth);
  });

  /**
   * Show a contact: hold the model on its worst frame, and light both bones on the character.
   *
   * This is the whole feature. "The left forearm is inside the ribcage on frames 41–58" is a
   * sentence nobody can check; the same thing in danger red inside select blue is one look.
   *
   * ⚠️ The limb goes LAST in the list. `BoneSpotlight` lets later entries win where two bones share
   * a vertex, and the vertices in dispute are exactly the ones the offending limb owns.
   */
  function showContact(index: number): void {
    const contact = contacts[index];
    const handle = stage;
    if (!contact || !handle) return;
    selectedContact = index;
    handle.scrubTo(contact.peakFrame);
    const bones = handle.bones;
    const bodyIndex = bones.findIndex((b) => b.name === contact.body);
    const limbIndex = bones.findIndex((b) => b.name === contact.limb);
    const lit: SpotlitBone[] = [];
    if (bodyIndex >= 0) lit.push({ boneIndex: bodyIndex, tone: 'select' });
    if (limbIndex >= 0) lit.push({ boneIndex: limbIndex, tone: 'danger' });
    spotlight?.set(lit.length ? lit : null);
  }

  function clearContact(): void {
    selectedContact = -1;
    spotlight?.set(null);
  }

  /** Let the clip run again from wherever it was held, with nothing lit. */
  function resumePlay(): void {
    clearContact();
    playing = true;
  }

  /**
   * 🔴 Put the clip back on the mixer after something drove it.
   *
   * `detectIntersections` saves and restores the mixer's clock, but it ends with `action.stop()`
   * whenever the action was not RUNNING when it arrived — and a scrubbed model is paused, which
   * `isRunning()` reports as false. A stopped action is deactivated, so nothing re-applies a pose
   * and the character is left frozen wherever the last sample put it, with a transport that does
   * nothing. Verified against the r185 sources.
   *
   * `clipAction` returns the action the stage is already driving — the mixer caches by clip and root
   * — so this revives that one rather than adding a second.
   */
  function restoreTransport(frame: number | null): void {
    const handle = stage;
    const clip = motionClip;
    const mixer = handle?.mixer;
    if (!handle || !mixer || !clip) return;
    const action = mixer.clipAction(clip);
    action.reset();
    action.play();
    if (frame === null) {
      playing = true;
      return;
    }
    // `scrubTo` pauses, which is what holding a frame means, and `playing` is bound so the
    // transport in the bar follows rather than claiming to be running over a held frame.
    handle.scrubTo(frame);
  }

  function detectMotion(): void {
    const handle = stage;
    const clip = motionClip;
    const skeleton = handle?.skeleton;
    const mixer = handle?.mixer;
    if (!handle || !clip || !skeleton || !mixer) return;
    motionBusy = 'Checking every frame…';
    error = '';
    const held = selectedContact >= 0 ? (contacts[selectedContact]?.peakFrame ?? null) : null;
    setTimeout(() => {
      try {
        const capsules = buildCapsules(handle.skinnedMeshes, skeleton);
        const previous = motionFound?.length ?? null;
        motionFound = detectIntersections(clip, mixer, handle.root, skeleton, capsules);
        motionPrevCount = previous;
        // The old row indices name findings that no longer exist, so the paint goes with them. The
        // held frame stays: yanking the camera's subject away is not what "re-check" means.
        clearContact();
      } catch (e) {
        fail(e);
      } finally {
        motionBusy = '';
        // The sweep left the mixer in whatever state its own restore produced; put the clip back
        // and re-hold the frame the user was looking at.
        restoreTransport(held);
      }
    }, 16);
  }

  function applyMotion(): void {
    const handle = stage;
    const clip = motionClip;
    const snapshot = motionSnapshot;
    const skeleton = handle?.skeleton;
    if (!handle || !clip || !snapshot || !skeleton) return;
    error = '';
    // 🔴 Recomputed FROM the pristine snapshot every time, never from the current state — that is
    // what makes the slider two-way. And zero is NOT a skip: `applyClearance` restoring from the
    // snapshot is the only thing that puts a bone back.
    //
    // The scoped envelope is built around ONE limb, so it follows the selection: the contact you are
    // looking at, or the deepest one when nothing is selected.
    const focus = contacts[selectedContact >= 0 ? selectedContact : 0];
    const weights =
      motionScoped && motionFound?.length && focus
        ? envelopeFrom(motionFound, clipFrameCount(clip), focus.limb)
        : null;
    try {
      applyClearance(clip, snapshot, skeleton, {
        arms: motionArms,
        legs: motionLegs,
        weights
      });
      // Values are mutated in place, so the interpolants re-read them — no action rebuild needed.
      // A zero delta is enough even on a held frame: a paused action stays in the mixer's active set
      // and re-applies its pose.
      handle.mixer?.update(0);
    } catch (e) {
      fail(e);
      return;
    }
    markMotionApplied(clip.name, motionTouched);
    // Re-check, so the count falling is visible. ⚠️ It is a signal, not a verdict — nothing in this
    // pipeline scores whether a corrected take actually looks right.
    if (motionFound) detectMotion();
  }

  function resetMotion(): void {
    const clip = motionClip;
    const snapshot = motionSnapshot;
    if (!clip || !snapshot) return;
    try {
      restoreClip(clip, snapshot);
    } catch (e) {
      fail(e);
      return;
    }
    stage?.mixer?.update(0);
    motionArms = 0;
    motionLegs = 0;
    markMotionApplied(clip.name, false);
  }

  function markMotionApplied(name: string, on: boolean): void {
    if (on) {
      if (!motionApplied.includes(name)) motionApplied = [...motionApplied, name];
    } else {
      motionApplied = motionApplied.filter((c) => c !== name);
    }
  }

  /** A different clip is on screen, so the sweep that described the last one no longer applies. The
   *  clearance sliders stay put: they are a value the user chose, and `motionApplied` is what says
   *  which clips actually carry it. */
  function onClipChanged(name: string, frames: number): void {
    stageClip = name;
    frameTotal = frames;
    motionFound = null;
    motionPrevCount = null;
    clearContact();
  }

  function depthOf(units: number): string {
    if (span <= 0) return `${units.toFixed(3)} units`;
    return `${((units / span) * 100).toFixed(1)}% of the model`;
  }

  // ── save ────────────────────────────────────────────────────────────

  /**
   * ⚠️ The browser cannot be told to reuse a name, so this is the one place a label becomes a
   * filename. Anything glTF-hostile is flattened rather than escaped — a `/` in a download name is a
   * silent failure on some platforms and a path on others.
   */
  function fileName(label: string): string {
    const clean = label.replace(/[\\/:*?"<>|]+/g, '-').trim();
    return `${clean || 'model'}.glb`;
  }

  function downloadGlb(blob: Blob, name: string): void {
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = name;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
    // ⚠️ Revoked late, not in the same tick. The browser reads the blob asynchronously after the
    // click, and revoking immediately cancels the download on Firefox with no error anywhere.
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }

  async function buildTakeGlb(handle: EditorStageHandle): Promise<Blob> {
    // ⚠️ No stop-and-pose dance before this, and that is checked rather than assumed: with only
    // `animations` set, `patchGlb` calls `applyAnimations` and nothing else — it rewrites sampler
    // outputs from the live tracks and never reads a bone's TRS or touches `inverseBindMatrices`.
    // (`applyJoints` is what would, and it only runs for `joints`/`renames`, neither of which a take
    // can have: every rig mode is refused on a posed model.) Stopping the mixer to force the bind
    // pose would leave the take frozen with no action driving it, for no benefit.
    return patchGlb(
      {
        source: handle.source,
        meshes: handle.skinnedMeshes,
        bones: handle.bones,
        associations: handle.associations
      },
      { animations: handle.animationClips }
    );
  }

  async function buildRigGlb(handle: EditorStageHandle): Promise<Blob> {
    // 🔴 PATCHED, not re-exported. `GLTFExporter` would strip the `mixamorig:` colons out of every
    // bone name (breaking the engine's name-based retarget) and re-encode both textures. See
    // `glb-patch.ts`.
    const textures: TexturePatch[] = [];
    if (colourApplied && baseMaterial?.map) {
      const encoded = await encodeTexture(
        textureToImageData(baseMaterial.map),
        (baseMaterial.map.userData?.mimeType as string | undefined) ?? 'image/png'
      );
      textures.push({
        material: baseMaterial,
        slot: 'baseColor',
        bytes: encoded.bytes,
        mimeType: encoded.mimeType
      });
    }
    if (normalApplied && baseMaterial?.normalMap) {
      const encoded = await encodeTexture(
        textureToImageData(baseMaterial.normalMap),
        (baseMaterial.normalMap.userData?.mimeType as string | undefined) ?? 'image/png'
      );
      textures.push({
        material: baseMaterial,
        slot: 'normal',
        bytes: encoded.bytes,
        mimeType: encoded.mimeType
      });
    }
    return patchGlb(
      {
        source: handle.source,
        meshes: handle.skinnedMeshes,
        bones: handle.bones,
        associations: handle.associations
      },
      {
        // Compacting renumbers every vertex, so every attribute has to go — a weight prune alone
        // only moves `WEIGHTS_0`.
        //
        // 🔴 A brush stroke moves `JOINTS_0` too. It REBINDS vertices, where a prune only zeroes
        // weights, so patching `WEIGHTS_0` alone would file new weights against the delivered joint
        // indices — a rig that never existed, and one that looks right in this viewer because the
        // viewer is reading the memory that WAS updated.
        attributes: compacted
          ? presentSemantics
          : strokes > 0 || dissolved.length > 0
            ? presentSemantics.filter((s) => s === 'JOINTS_0' || s === 'WEIGHTS_0')
            : applied.length
              ? ['WEIGHTS_0']
              : undefined,
        indices: meshDirty,
        // 🔴 A dissolve forces the joint patch on. Removing a joint re-hangs its children on its
        // parent, and the file learns their new local positions only from the node translations
        // `applyJoints` writes — see `applyJointRemoval`, which refuses without them.
        joints: moved.length > 0 || dissolved.length > 0,
        removeJoints: dissolved.length ? [...dissolved] : undefined,
        renames: new Map([...renameState].map(([current, rename]) => [current, rename.to])),
        textures: textures.length ? textures : undefined
      }
    );
  }

  async function save(): Promise<void> {
    const handle = stage;
    if (!handle || !dirty) return;
    busy = 'Writing the GLB…';
    error = '';
    saved = '';
    const takeSave = motionDirty;
    try {
      const label = repairLabel.trim() || `${viewLabel || baseTier || 'model'} · repaired`;
      const blob = takeSave ? await buildTakeGlb(handle) : await buildRigGlb(handle);
      // ⚠️ Every reactive value is COPIED out on the way in. `changes` crosses a public seam into a
      // host callback, and a `$state` array or object handed over there is a live proxy the caller
      // would be reading long after this function cleared it.
      const changes: Record<string, unknown> = takeSave
        ? {
            motion_clip: stageClip,
            corrected_clips: [...motionApplied],
            motion_arms_deg: motionArms,
            motion_legs_deg: motionLegs,
            scoped_to_intersections: motionScoped,
            intersections_before: motionPrevCount,
            intersections_after: motionFound?.length ?? null
          }
        : {
            pruned_influences: applied.reduce((n, a) => n + a.prunedInfluences, 0),
            vertices_touched: applied.reduce((n, a) => n + a.verticesTouched, 0),
            vertices_spared: applied.reduce((n, a) => n + a.verticesSpared, 0),
            prune_threshold_ratio: applied.length ? threshold : null,
            painted_strokes: strokes,
            renamed_joints: renames.map((r) => ({ from: r.from, to: r.to })),
            dissolved_joints: dissolved.map((b) => b.name),
            mirrored_edits: symmetric ? mirrorMode : null,
            moved_joints: moved.map((j) => ({
              name: j.name,
              distance: Number(j.distance.toFixed(5))
            })),
            removed_pieces: removedPieces,
            removed_interior_faces: removedFaces,
            decimated_to_triangles: decimatedTo || null,
            compacted,
            removed_degenerate_triangles: degenerateRemoved,
            colour: colourApplied ? { ...colour } : null,
            normal_strength: normalApplied ? normalStrength : null
          };

      let outcome: { note?: string; animatable?: boolean };
      if (onSave) {
        outcome = await onSave(blob, { label, changes });
      } else {
        // The honest answer for a host with nowhere to put an edit. The alternative is a Save button
        // that silently does nothing.
        downloadGlb(blob, fileName(label));
        outcome = {
          animatable: false,
          note:
            'Downloaded. This viewer has no place to store an edit, so the patched GLB went to ' +
            'your browser — nothing on the server changed.'
        };
      }
      const animatable = outcome.animatable ?? true;
      saved = animatable
        ? 'Saved. It can be animated like any tier.'
        : (outcome.note ??
          'Saved, but the shared staging directory is missing — this edit can be previewed and downloaded, not animated.');

      // 🔴 Captured BEFORE the state is cleared. `renames` is emptied a few lines down, and a flag
      // computed after that would always read zero — and `propagate()` would then carry the joint
      // moves to the other tiers while silently dropping every rename.
      pendingEdits =
        !takeSave && jointRest && (moved.length > 0 || renames.length > 0)
          ? collectJointEdits(handle.bones, jointRest, renameState)
          : null;
      propagated = [];
      applied = [];
      strokes = 0;
      renames = [];
      dissolved = [];
      removedPieces = 0;
      removedFaces = 0;
      degenerateRemoved = 0;
      decimatedTo = 0;
      compacted = false;
      colourApplied = false;
      normalApplied = false;
      motionApplied = [];
      restoreWeights = null;
      restoreImage = null;
      restoreNormal = null;
      // 🔴 A barrier, not a clear. The pending-change list the save just emptied is what every entry
      // above would put back a piece of, so undoing into it would show a model that claims to have
      // nothing pending while carrying half a repair. The reason stays on screen so the history does
      // not simply evaporate; new edits stack on top and undo normally.
      history.pushBarrier(
        'Saved as a new asset',
        'Saving cleared the pending-change list, so the steps that fed it can no longer be ' +
          'reversed against it. The model on screen still carries every repair.'
      );
      historyTick++;
      onSaved?.();
    } catch (e) {
      fail(e);
    } finally {
      busy = '';
    }
  }

  // ── carrying a skeleton repair across the ladder ────────────────────
  //: 🔴 Only JOINT MOVES AND RENAMES propagate. All four tiers are built over one skeleton with the
  //: same joints in the same order, so the same edit lands identically on each. A weight prune, a
  //: deleted shell and a colour adjust are per-geometry — each tier has its own — and silently
  //: pretending otherwise would ship a "repaired" ladder that is repaired in one place.
  //:
  //: 🔴 And it is PIXAL3D-ONLY, gated on the props that make it possible rather than on a service
  //: name: it fetches sibling tiers by `pixal3dModelPath` and posts each one back to the edits
  //: endpoint. A host with no character id and no tier ladder simply never sees this block. (SPAR3D's
  //: LOD GLBs might share a skeleton and be propagable, but nothing verifies that — do not assume it.)
  /** The joint edits captured at save time, replayed onto the rest of the ladder. Held rather than
   *  recomputed because the rename list is cleared once the save lands. */
  let pendingEdits = $state.raw<JointEdits | null>(null);
  const skeletonEdited = $derived((pendingEdits?.size ?? 0) > 0);
  let propagating = $state('');
  let propagated = $state<string[]>([]);
  const otherTiers = $derived(tiers.filter((t) => t !== baseTier));
  const canPropagate = $derived(!!characterId && !!baseTier && otherTiers.length > 0);

  async function propagate(): Promise<void> {
    const edits = pendingEdits;
    if (!edits || edits.size === 0 || !canPropagate) return;
    error = '';
    for (const tier of otherTiers) {
      if (propagated.includes(tier)) continue;
      propagating = tier;
      try {
        const bytes = await (
          await fetchMediaBlob(pixal3dModelPath(characterId, tier))
        ).arrayBuffer();
        const result = await propagateToTier(bytes, edits);
        if (result.missing.length) {
          // Not fatal — the ladder shares a skeleton, so this should be empty. If it is not, the
          // assumption this whole feature rests on is wrong and the user needs to hear it.
          failMessage(
            `the ${tier} tier is missing ${result.missing.length} of the edited joints ` +
              `(${result.missing.slice(0, 3).join(', ')}) — its skeleton is not the one that was repaired`
          );
        }
        await savePixal3dEdit(characterId, result.glb, {
          label: `${repairLabel.trim() || baseTier} · ${tier}`,
          base_tier: tier,
          changes: {
            propagated_from: baseTier,
            moved_joints: result.moved,
            renamed_joints: result.renamed,
            joints: result.joints
          }
        });
        propagated = [...propagated, tier];
      } catch (e) {
        failMessage(`${tier}: ${e instanceof Error ? e.message : String(e)}`);
        break;
      } finally {
        propagating = '';
      }
    }
    onSaved?.();
  }

  function revert(): void {
    resetSession();
    // The parent re-fetches the pristine GLB, which replaces the bones wholesale — so the joint
    // edits go with it and there is nothing to undo by hand.
    onReloadRequested?.();
  }

  function pct(n: number, of: number): string {
    if (!of) return '0%';
    const p = (n / of) * 100;
    return p < 0.01 ? '<0.01%' : `${p.toFixed(p < 1 ? 3 : 1)}%`;
  }

  const dirtySummary = $derived.by(() => {
    const parts: string[] = [];
    const influences = applied.reduce((n, a) => n + a.prunedInfluences, 0);
    if (influences) parts.push(`${influences.toLocaleString()} influences pruned`);
    if (strokes) parts.push(`${strokes} brush stroke${strokes === 1 ? '' : 's'}`);
    if (renames.length) {
      parts.push(`${renames.length} joint${renames.length === 1 ? '' : 's'} named`);
    }
    if (moved.length) parts.push(`${moved.length} joint${moved.length === 1 ? '' : 's'} moved`);
    if (removedPieces)
      parts.push(`${removedPieces} piece${removedPieces === 1 ? '' : 's'} deleted`);
    if (removedFaces) parts.push(`${removedFaces.toLocaleString()} faces removed`);
    if (degenerateRemoved) parts.push(`${degenerateRemoved.toLocaleString()} degenerates stripped`);
    if (decimatedTo) parts.push(`decimated to ${decimatedTo.toLocaleString()} triangles`);
    if (compacted) parts.push('vertices compacted');
    if (colourApplied) parts.push('colour baked');
    if (normalApplied) parts.push(`normal strength ${normalStrength.toFixed(2)} baked`);
    if (motionApplied.length) parts.push(`${motionApplied.length} clip corrected`);
    return parts;
  });
</script>

<div class="workbench" bind:this={rootEl}>
  <div class="stage-col">
    <div class="sticky">
      <ModelStage
        {src}
        label={viewLabel}
        {height}
        {showSkeleton}
        mode={stageMode}
        labels={mode === 'pieces' ? labels : null}
        {selectedComponent}
        {previewFaces}
        {jointStates}
        {selectedJoint}
        {heat}
        {grade}
        {keepChildren}
        jointEdit={mode === 'joints'}
        brush={mode === 'paint' && !paintRefusal
          ? { radius: brushRadius, profile: paintProfile }
          : null}
        ghostSrc={ghostArmed ? src : ''}
        {ghostOpacity}
        bind:playing
        onReady={onStageReady}
        onError={failMessage}
        onJointPicked={(n: string) => (selectedJoint = n)}
        onHoverJoint={(n: string) => (hoveredJoint = n)}
        onHoverComponent={(id: number) => (hoveredComponent = id)}
        {onPickComponent}
        onFacePicked={(f: number) => (pickedFace = f)}
        {onJointDrag}
        {onJointNudge}
        {onBrushBegin}
        {onBrushDab}
        {onBrushEnd}
        {onBrushHover}
        {onClipChanged}
        onFrame={(f: number) => (frameNow = f)}
        onModeRequested={(m: StageMode) => (mode = m)}
      >
        {#snippet hud()}
          <div class="rail">
            {#each MODES as m (m.id)}
              <button
                class="mode"
                class:on={mode === m.id}
                disabled={!modeReady(m.needs)}
                aria-pressed={mode === m.id}
                onclick={() => (mode = m.id)}
              >
                {m.label}
              </button>
            {/each}
            <label class="skel">
              <input type="checkbox" bind:checked={showSkeleton} /> Skeleton
            </label>
            <!-- The delivered model, translucent and in place. It is a GHOST: never picked, never
                 saved, never part of the character — see `ghostOpacity`. -->
            <label class="skel">
              <input
                type="range"
                min="0"
                max="0.8"
                step="0.05"
                bind:value={ghostOpacity}
                oninput={() => (ghostArmed = ghostArmed || ghostOpacity > 0)}
                aria-label="Ghost of the delivered model"
              />
              Before
            </label>
          </div>

          <!-- Stacked in one column rather than pinned to opposite corners: `ModelStage` puts its
               own mode notice across the bottom of the viewport, and the colour/threshold floater
               takes the bottom-left. -->
          <div class="notes">
            {#if mode === 'motion' && selectedContact >= 0 && contacts[selectedContact]}
              {@const contact = contacts[selectedContact]}
              <div class="note contact-note">
                <b class="danger-ink">{shortBone(contact.limb)}</b>
                inside
                <b class="select-ink">{shortBone(contact.body)}</b>
                · frame {contact.peakFrame}
              </div>
            {:else if !canEdit && !posed}
              <div class="note refusal">{editRefusal}</div>
            {:else if mode === 'pieces' && hoveredPiece && hoveredComponent !== selectedComponent}
              <div class="note">
                {hoveredPiece.triangles.toLocaleString()} tris · {(
                  hoveredPiece.sizeRatio * 100
                ).toFixed(1)}% of the model
              </div>
            {:else if mode === 'joints' && hoveredJoint && hoveredJoint !== selectedJoint}
              <div class="note"><code>{hoveredJoint}</code></div>
            {/if}
            {#if busy || meshBusy || motionBusy}
              <div class="note work">{busy || meshBusy || motionBusy}</div>
            {/if}
          </div>

          {#if mode === 'weights' && report}
            <div class="floater panel-mini">
              <label class="knob">
                <span class="k">Leak past</span>
                <input
                  type="range"
                  min="0.05"
                  max="1"
                  step="0.01"
                  bind:value={threshold}
                  oninput={refreshPreview}
                />
                <b>{(threshold * 100).toFixed(0)}%</b>
              </label>
              <p class="hint">
                Red is at or past the threshold. Drag until only the specks are lit — if the whole
                torso burns, the threshold is wrong, not the rig.
              </p>
            </div>
          {:else if mode === 'paint'}
            <div class="floater panel-mini">
              <!-- The target is the headline, because assign and erase are meaningless without one
                   and the whole gesture is "put THIS joint here". -->
              <p class="target">
                {#if paintTool === 'pick'}
                  Click the model to pick the joint that drives it
                {:else if paintTool === 'smooth'}
                  Smoothing the seam under the brush
                {:else if selectedJoint}
                  Painting <b>{shortBone(selectedJoint)}</b>
                {:else}
                  <span class="warn-ink">No joint picked — use <b>pick</b> and click the model</span
                  >
                {/if}
              </p>
              <div class="row">
                {#each PAINT_TOOLS as tool (tool.id)}
                  <button
                    class="chip"
                    class:on={paintTool === tool.id}
                    aria-pressed={paintTool === tool.id}
                    title={tool.hint}
                    onclick={() => selectTool(tool.id)}
                  >
                    {tool.id}
                  </button>
                {/each}
              </div>
              {#if picked}
                <p class="picked">
                  Picked <b>{shortBone(picked.name)}</b> — it carries
                  <b>{(picked.share * 100).toFixed(0)}%</b>
                  of the weight over {picked.vertices.toLocaleString()} vertices there, and what it drives
                  is lit on the model now.
                </p>
              {:else if selectedJoint && paintTool !== 'smooth' && drivingJoint === selectedJoint}
                <p class="picked">
                  The lit patch away from the cursor is everything
                  <b>{shortBone(selectedJoint)}</b> drives. Watch it change as you paint — that is the
                  edit, on the model.
                </p>
              {/if}
              {#if strokeTally && strokeTally.dabs > 0 && !painting}
                <p class="picked">
                  Last stroke: <b>{strokeTally.vertices.toLocaleString()}</b> vertex writes{strokeTally.evictions
                    ? `, ${strokeTally.evictions.toLocaleString()} evicted`
                    : ''}{strokeTally.spared
                    ? `, ${strokeTally.spared.toLocaleString()} spared`
                    : ''}{strokeTally.declined
                    ? `, ${strokeTally.declined.toLocaleString()} declined`
                    : ''}. Ctrl+Z puts it back.
                </p>
              {/if}
              <label class="knob">
                <span class="k">Radius</span>
                <input type="range" min="0.005" max="0.2" step="0.005" bind:value={brushScale} />
                <b>{(brushScale * 100).toFixed(1)}%</b>
              </label>
              <label class="knob">
                <span class="k">Strength</span>
                <input type="range" min="0.05" max="1" step="0.05" bind:value={paintStrength} />
                <b>{paintStrength.toFixed(2)}</b>
              </label>
              <label class="knob">
                <span class="k">Falloff</span>
                <select bind:value={paintProfile}>
                  <option value="smooth">smooth</option>
                  <option value="linear">linear</option>
                  <option value="sharp">sharp</option>
                  <option value="constant">constant</option>
                </select>
              </label>
              <p class="hint">
                The lit patch is the falloff, on the surface — hot where a dab moves a weight fully,
                cool at the rim where it barely does. Take the cursor off the model and "what it
                drives" comes back, if it is on.
              </p>
            </div>
          {:else if mode === 'colour'}
            <div class="floater stack">
              <ColourHud
                bind:adjust={colour}
                applied={colourApplied}
                busy={meshBusy}
                hasMap={hasBaseMap}
                onInput={onColourInput}
                onApply={applyColour}
                onRevert={undoColour}
              />
              <NormalHud
                bind:strength={normalStrength}
                applied={normalApplied}
                busy={meshBusy}
                hasMap={hasNormalMap}
                onInput={onNormalInput}
                onApply={applyNormal}
                onRevert={undoNormal}
              />
            </div>
          {:else if mode === 'motion' && motionReady}
            <div class="floater panel-mini">
              <label class="knob">
                <span class="k">Arms</span>
                <input type="range" min="-20" max="20" step="0.5" bind:value={motionArms} />
                <b>{motionArms.toFixed(1)}°</b>
              </label>
              <label class="knob">
                <span class="k">Legs</span>
                <input type="range" min="-20" max="20" step="0.5" bind:value={motionLegs} />
                <b>{motionLegs.toFixed(1)}°</b>
              </label>
              <div class="row">
                <button
                  class="btn-sec small"
                  onclick={applyMotion}
                  disabled={!!motionBusy || (!motionTouched && !clipCorrected)}
                >
                  Apply
                </button>
                <button class="btn-sec small" onclick={detectMotion} disabled={!!motionBusy}>
                  {motionFound ? 'Re-check' : 'Find contacts'}
                </button>
                <button
                  class="btn-sec small"
                  onclick={resetMotion}
                  disabled={!!motionBusy || (!motionTouched && !clipCorrected)}
                >
                  Reset
                </button>
              </div>
              <p class="hint">
                Both sides move symmetrically, away from the body. Positive opens; negative closes,
                which is occasionally what a too-wide idle needs.
                {#if !clipCorrected && motionTouched}
                  <strong>Nothing is in the clip until you press Apply.</strong>
                {/if}
              </p>
            </div>
          {/if}
        {/snippet}

        {#snippet jointPopover({ index, anchor })}
          {#if mode === 'joints' && stage && stage.bones[index]}
            {@const bone = stage.bones[index]}
            <!-- ⚠️ NOT positioned here. `ModelStage` already renders this snippet inside a wrapper it
                 places at the joint; a second `position: absolute` would re-anchor it to the layer.
                 All this does is carry the `behind` fade — a joint you orbited past stays reachable
                 rather than vanishing. `pointer-events: auto` is on the popover's own root. -->
            <div class="anchored" class:behind={anchor.behind}>
              <JointPopover
                name={(bone.userData?.name as string | undefined) ?? bone.name}
                sanitized={bone.name}
                state={jointStates?.[index] ?? 'unnamed'}
                suggestions={jointSuggestions}
                driving={drivingJoint === bone.name}
                moved={movedNames.has(bone.name)}
                {canEdit}
                reason={editRefusal}
                bind:keepChildren
                dissolveRefusal={dissolveRefusalFor(bone)}
                dissolvedAlready={dissolved.includes(bone)}
                onAssign={assign}
                onNudge={nudge}
                onToggleDrives={toggleDrives}
                onReset={resetJoint}
                onDissolve={() => dissolveJoint(bone)}
                onClose={() => (selectedJoint = '')}
              />
            </div>
          {/if}
        {/snippet}

        {#snippet piecePopover({ id, anchor })}
          {#if mode === 'pieces' && selectedPiece && selectedPiece.id === id}
            <div class="anchored" class:behind={anchor.behind}>
              <PiecePopover
                triangles={selectedPiece.triangles}
                vertices={selectedPiece.vertices}
                sizeRatio={selectedPiece.sizeRatio}
                largest={labels?.components[0]?.id === id}
                busy={!!meshBusy || !!pending}
                onDelete={dropSelected}
                onPreview={(on: boolean) => (dangerComponent = on ? id : -1)}
                onClose={() => (selectedComponent = -1)}
              />
            </div>
          {/if}
        {/snippet}
      </ModelStage>
    </div>
  </div>

  <aside class="inspector">
    {#if error}<div class="error">{error}</div>{/if}
    {#if saved}<div class="ok-box">{saved}</div>{/if}

    {#if pending}
      <section class="card panel danger-panel">
        <h3>{pending.what}</h3>
        <p class="hint">{pending.detail}</p>
        {#if pending.faces}
          <p class="hint">
            Everything about to go is lit in red on the model. Orbit it before you confirm.
          </p>
        {:else}
          <p class="warn">
            ⚠️ The exact triangles could not be resolved for the highlight, so this is confirming on
            a count alone. Nothing is drawn.
          </p>
        {/if}
        <div class="actions">
          <button class="btn-danger" onclick={() => pending?.run()}>Delete them</button>
          <button class="btn-sec" onclick={() => (pending = null)}>Cancel</button>
        </div>
      </section>
    {/if}

    {#if decimateReport}
      {@const r = decimateReport}
      <section class="card panel danger-panel">
        <h3>Simplification — computed, not applied</h3>
        <!-- 🔴 The ACHIEVED number leads. `metTarget: false` is routine on a character made of ~200
             interpenetrating shells, because every shell is a closed border the simplifier will not
             collapse across — and a panel that answered "decimated to 25%" over a 91% result is
             exactly the failure the report exists to prevent. -->
        <p class="stat">
          {r.inputTriangles.toLocaleString()} → <b>{r.outputTriangles.toLocaleString()}</b>
          triangles · <b>{(r.achievedRatio * 100).toFixed(1)}%</b> of what is there now
        </p>
        {#if r.unchanged}
          <p class="warn">
            ⚠️ Nothing could be removed. Every legal collapse was already refused — on this class of
            asset that means the mesh is nearly all border, and simplification has nothing to work
            with.
          </p>
        {:else if !r.metTarget}
          <p class="warn">
            ⚠️ It asked for {r.requestedTriangles.toLocaleString()} and stopped at
            {r.outputTriangles.toLocaleString()}. That is <strong>not an error</strong>: the
            simplifier stops the moment the next collapse would break the mesh's topology, and these
            characters are hundreds of separate shells, each one a border it will not cross. The
            number above is what you would actually get.
          </p>
        {/if}
        <p class="hint">
          Deformation introduced: <b>{(r.error * 100).toFixed(3)}%</b> of the model's size ({r.errorAbsolute.toFixed(
            4
          )} in its own units).
          {#if r.attributesUsed.length}
            Measured against {r.attributesUsed.join(' and ')} as well as position — on a projected texture,
            sliding a vertex across a flat face costs nothing in silhouette and drags the image with it.
          {/if}
        </p>
        {#if r.rig.checked && r.rig.starved.length > 0}
          <p class="warn">
            ⚠️ {r.rig.starved.length} joint{r.rig.starved.length === 1 ? '' : 's'} would be left driving
            nothing: {starvedNames(r.rig.starved).slice(0, 6).join(', ')}{r.rig.starved.length > 6
              ? ` +${r.rig.starved.length - 6} more`
              : ''}. They stay in the skeleton and every clip goes on animating them, and that part
            of the character simply stops moving. It is a "look at this", not an error — leaf and
            helper bones legitimately drive nothing.
          </p>
        {:else if r.rig.checked}
          <p class="hint ok">Every joint that drove geometry still drives some.</p>
        {:else}
          <p class="hint">
            The rig check could not run on this mesh — no readable joint/weight pair — so nothing is
            known about starved joints here.
          </p>
        {/if}
        <p class="hint">
          ⚠️ There is no highlight for this one, deliberately: decimation does not remove a set of
          triangles, it rebuilds the index over the same vertices, so
          <strong>every face number changes</strong>. Applying it clears the picked face, the piece
          map and the atlas's findings rather than reusing them.
        </p>
        <p class="hint">
          Only the index shrinks — the vertices all stay in the file. <strong
            >Drop orphaned vertices</strong
          > afterwards is where the file-size win actually lands.
        </p>
        <div class="actions">
          <button class="btn-danger" onclick={applyDecimation} disabled={r.unchanged}>
            Apply — keep {r.outputTriangles.toLocaleString()} triangles
          </button>
          <button class="btn-sec" onclick={() => (decimateReport = null)}>Cancel</button>
        </div>
      </section>
    {/if}

    <section class="card panel">
      {#if mode === 'view'}
        <h3>Workbench</h3>
        <p class="hint">
          Pick a mode in the canvas.
          {#if posed}
            <strong>Motion</strong> is the one a take gets: it finds the frames where a limb is inside
            the body and pushes it out.
          {:else}
            <strong>Joints</strong> names and moves them, <strong>Pieces</strong>
            deletes stray shells and simplifies, <strong>Weights</strong> paints the skin leak,
            <strong>Paint</strong>
            rebinds it by hand, <strong>Atlas</strong> shows the UV layout, and
            <strong>Colour</strong> grades the texture and the normal map live.
          {/if}
        </p>
        <p class="hint">
          The <strong>Before</strong> slider in the canvas fades the delivered model in behind the edited
          one, in place. "Did that repair actually help" is a look rather than a memory test.
        </p>
        <p class="hint">
          Edits are saved as a <strong>new asset</strong> — the delivered
          {#if baseTier}<code>{baseTier}</code>{:else}source{/if} model is never overwritten, so a repair
          that turns out worse is discarded by deleting it.
        </p>
        {#if extraMeshes > 0}
          <p class="warn">
            ⚠️ This character has {extraMeshes + 1} meshes. Only the first is tidied by the mesh tools.
          </p>
        {/if}
        {#if !canEdit}
          <p class="warn">{editRefusal}</p>
        {/if}
      {:else if mode === 'joints'}
        <h3>Joints</h3>
        <p class="hint">
          Green fills a core slot, blue is named but not core, violet is anonymous. Click one for
          its options. The retarget maps joints by <strong>exact name</strong>, so every core slot
          with no bone is a joint no humanoid clip will drive — that is what a low
          <code>named_core</code> costs. Non-humanoids get anonymous joints correctly and should be left
          alone.
        </p>
        {#if coverage}
          <p class="stat">{coverage.present.length} of 22 core slots named</p>
          {#if coverage.missing.length === 0}
            <p class="hint ok">All 22 core slots are named. Nothing to do here.</p>
          {:else}
            <details>
              <summary>{coverage.missing.length} missing slots</summary>
              <ul class="missing">
                {#each coverage.missing as name, i (i)}
                  <li>
                    <code>{name}</code>
                    <button class="btn-sec small" onclick={() => focusCandidate(name)}>
                      Show the best candidate
                    </button>
                  </li>
                {/each}
              </ul>
              <p class="hint">
                ⚠️ "Best" is skeleton topology — the joint hanging off the slot's anatomical parent.
                It is a suggestion, not a finding: nothing checks that the joint is where the name
                claims, and a wrong assignment retargets cleanly and animates wrongly.
              </p>
            </details>
            <div class="actions">
              <button class="btn-sec small" onclick={proposeNames} disabled={!stage}>
                Guess a name for all {coverage.missing.length}…
              </button>
              {#if proposals}
                <button class="btn-sec small" onclick={() => (proposals = null)}>
                  Close the list
                </button>
              {/if}
            </div>
          {/if}
        {/if}

        {#if sides}
          <div class="review sides">
            <h4>Left and right</h4>
            {#if sides.verdict === 'mirrored'}
              <p class="warn">
                🔴 This rig's <code>Left</code>/<code>Right</code> names are
                <strong>swapped</strong>. Every humanoid convention names a joint from the
                CHARACTER's point of view, so <code>RightArm</code> is the arm on its own right —
                the one you see on your left when it faces you. Measured from its own feet, this
                character faces <b>{sides.facing > 0 ? '+Z' : '−Z'}</b>, which puts its right at
                <b>{sides.rightSign > 0 ? '+X' : '−X'}</b> — and the joints named
                <code>Right</code> are all on the other side.
              </p>
              <p class="hint">
                It comes from the rig namer, which derives facing correctly and then turns it into a
                side with <code>up × forward</code> where the right vector is
                <code>forward × up</code>. Motion is unaffected — the retarget measures which side
                the joints really sit on and swaps for itself — so this is wrong exactly where a
                person or another engine reads the names: here, and in a Unity, VRM or Mixamo
                import.
              </p>
              <div class="actions">
                <button class="btn-sec small" onclick={swapSides} disabled={!canEdit}>
                  Swap left ↔ right on all {swapPlan.length} sided joints
                </button>
              </div>
              <p class="hint">
                One undo step, and it renames only the sided core slots — fingers, twist joints and
                anonymous bones keep their names. Save writes the new names into the file; use
                <strong>Carry to the other tiers</strong> afterwards, or the rest of the ladder stays
                mirrored.
              </p>
            {:else if sides.verdict === 'ok'}
              <p class="hint ok">
                Names agree with the body: it faces <b>{sides.facing > 0 ? '+Z' : '−Z'}</b>, its own
                right is <b>{sides.rightSign > 0 ? '+X' : '−X'}</b>, and that is where the
                <code>Right</code> joints are. Checked over {sides.pairs} sided pairs.
              </p>
            {:else}
              <p class="hint">{sides.reason}</p>
            {/if}
            {#if sides.verdict !== 'unknown'}
              <details>
                <summary>What this was measured from</summary>
                <ul class="tail">
                  <li>
                    <span class="count">toes forward of ankles</span>
                    <span class="share"
                      >{sides.feet
                        ? `${sides.feet.sign > 0 ? '+Z' : '−Z'} · ${(sides.feet.margin * 100).toFixed(1)}% of the skeleton`
                        : 'no cue — flat feet, or no toe joints'}</span
                    >
                  </li>
                  <li>
                    <span class="count">head forward of neck</span>
                    <span class="share"
                      >{sides.head
                        ? `${sides.head.sign > 0 ? '+Z' : '−Z'} · ${(sides.head.margin * 100).toFixed(1)}% of the skeleton`
                        : 'no cue'}</span
                    >
                  </li>
                </ul>
                {#if sides.cuesDisagree}
                  <p class="warn">
                    ⚠️ The two cues DISAGREE, so the answer rests on the feet alone. On a character
                    whose head sits behind its neck, or whose toes point backwards, look before you
                    swap.
                  </p>
                {/if}
                <p class="hint">
                  Both cues are read within one leg and along the mid-line, so neither can be fooled
                  by the very naming being checked. The rule they feed is glTF's own: right-handed,
                  +Y up, +Z forward, −X right.
                </p>
              </details>
            {/if}
          </div>
        {/if}

        {#if proposals}
          <div class="review">
            <h4>{proposals.length} proposed names — none of them applied</h4>
            <p class="warn">
              🔴 Nothing here is a finding. The retarget maps by <strong>exact name</strong> and
              validates nothing else, so a joint named as a hand that is actually a shin will
              retarget cleanly and animate wrongly — and a batch guess multiplies that. Press
              <strong>Show</strong> on a row and watch which part of the character lights up. That is
              the only check that exists.
            </p>
            <ul class="proposals">
              {#each proposals as proposal (proposal.core)}
                <li>
                  <span class="pair">
                    <code>{proposal.core}</code>
                    ← <b>{shortBone(proposal.bone.name)}</b>
                    <span class="stat">
                      {proposal.rank === 0 ? 'best match' : `candidate #${proposal.rank + 1}`}
                    </span>
                  </span>
                  <span class="btns">
                    <button class="btn-sec small" onclick={() => inspectProposal(proposal)}>
                      Show what it drives
                    </button>
                    <button
                      class="btn-sec small"
                      onclick={() => acceptProposal(proposal)}
                      disabled={!canEdit}
                    >
                      Use this name
                    </button>
                    <button class="btn-sec small" onclick={() => dropProposal(proposal)}>
                      No
                    </button>
                  </span>
                </li>
              {/each}
            </ul>
            <p class="hint">
              Ranks were computed against each other, so no joint is proposed for two slots. Taking
              one name does not re-rank the rest.
            </p>
          </div>
        {/if}

        <!-- ── symmetry ─────────────────────────────────────────────── -->
        {#if mirror}
          <div class="sym">
            <h4>Symmetry</h4>
            {#if mirrorReady}
              <p class="stat">
                Mirrors across <b>{mirror.axis.toUpperCase()}</b> · {mirror.pairs} paired joints · confidence
                {(mirror.confidence * 100).toFixed(0)}%
              </p>
              <label class="toggle">
                <input type="checkbox" bind:checked={symmetric} /> Mirror every joint move
              </label>
              {#if selectedJoint}
                <p class="hint">
                  {#if mirrorPartner}
                    <code>{shortBone(selectedJoint)}</code> mirrors onto
                    <code>{shortBone(mirrorPartner.name)}</code>.
                  {:else}
                    <code>{shortBone(selectedJoint)}</code> has no partner — a mid-line joint, or one
                    whose opposite number is not in this skeleton. It moves alone whatever this is set
                    to.
                  {/if}
                </p>
              {/if}
              {#if symmetric}
                <div class="row">
                  <button
                    class="chip"
                    class:on={mirrorMode === 'delta'}
                    aria-pressed={mirrorMode === 'delta'}
                    onclick={() => (mirrorMode = 'delta')}
                  >
                    same repair
                  </button>
                  <button
                    class="chip"
                    class:on={mirrorMode === 'snap'}
                    aria-pressed={mirrorMode === 'snap'}
                    onclick={() => (mirrorMode = 'snap')}
                  >
                    make the sides match
                  </button>
                </div>
                <p class="hint">
                  <strong>Same repair</strong> reflects the direction and leaves whatever asymmetry
                  the rig already had — the small edit, and the right default.
                  <strong>Make the sides match</strong> puts the partner at the exact reflection, which
                  absorbs that asymmetry into this one move.
                </p>
                {#if symmetryDiag && mirrorMode === 'snap'}
                  <p class="warn">
                    ⚠️ The two sides are already <b>{(symmetryDiag.worstRatio * 100).toFixed(2)}%</b
                    >
                    of the model's size apart at their worst pair ({symmetryDiag.pairs[0]?.left ??
                      '—'}). Matching them moves the partner by that much on top of your edit, and
                    that asymmetry may be the character.
                  </p>
                {/if}
                <p class="hint">
                  Both sides move with the same
                  <em>{keepChildren ? 'move the joint only' : 'move the chain'}</em>
                  setting, and land on the undo stack as
                  <strong>one step</strong> — a Ctrl+Z that left the character half-mirrored would be
                  worse than no undo at all.
                </p>
              {/if}
              <div class="actions">
                <button class="btn-sec small" onclick={measureSymmetry}>
                  {symmetryStale ? 'Re-measure symmetry' : 'Measure symmetry'}
                </button>
                {#if symmetryStale}
                  <span class="stat">joints moved since this was measured</span>
                {/if}
              </div>
              {#if symmetryDiag}
                <details>
                  <summary>
                    How symmetric it already is (worst {(symmetryDiag.worstRatio * 100).toFixed(
                      2
                    )}%)
                  </summary>
                  <ul class="tail">
                    {#each symmetryDiag.pairs.slice(0, 8) as pair (pair.left)}
                      <li>
                        <span class="ratio">{(pair.ratio * 100).toFixed(2)}%</span>
                        <span class="count">{shortBone(pair.left)} ↔ {shortBone(pair.right)}</span>
                        <span class="share">{pair.source}</span>
                      </li>
                    {/each}
                  </ul>
                  {#if symmetryDiag.unpaired.length}
                    <p class="warn">
                      ⚠️ {symmetryDiag.unpaired.length} core slot{symmetryDiag.unpaired.length === 1
                        ? ' is'
                        : 's are'} present on one side only ({symmetryDiag.unpaired
                        .slice(0, 3)
                        .join(', ')}) — nothing to mirror onto, and a rig defect in itself.
                    </p>
                  {/if}
                  <p class="hint">
                    <code>pattern</code> pairs matched on a name token (<code>Left</code>/<code
                      >Right</code
                    >, <code>_L_</code>/<code>_R_</code>) rather than the closed 22-slot table, so
                    they are a guess that the skeleton happened to confirm.
                  </p>
                </details>
              {/if}
            {:else}
              <p class="warn">
                ⚠️ Mirrored editing is off: this skeleton does not read as a mirror
                {#if mirror.pairs === 0}
                  — <strong>nothing paired at all</strong>, which is normal for a non-humanoid rig.
                {:else}
                  — {mirror.pairs} pairs, confidence {(mirror.confidence * 100).toFixed(0)}% against
                  the {(MIRROR_MIN_CONFIDENCE * 100).toFixed(0)}% this needs.
                {/if}
              </p>
              <p class="hint">
                Per-axis score: X {(mirror.scores.x * 100).toFixed(0)}% · Y {(
                  mirror.scores.y * 100
                ).toFixed(0)}% · Z {(mirror.scores.z * 100).toFixed(0)}%. The score is how much of
                each pair's separation lies along that axis, times how consistently every left joint
                is on the same side of it. Two axes explaining the rig equally well means neither
                does, and mirroring an edit then moves a joint that is not the other side of
                anything.
              </p>
            {/if}
          </div>
        {/if}
        {#if renames.length}
          <p class="hint">
            Renamed: {renames
              .slice(0, 6)
              .map((r) => `${shortBone(r.from)} → ${shortBone(r.to)}`)
              .join(', ')}{renames.length > 6 ? ` +${renames.length - 6} more` : ''}
          </p>
        {/if}
        {#if dissolvedNames.length}
          <p class="hint">
            Dissolved, and deleted from the file on save: {dissolvedNames.join(', ')}. Their weights
            are on their parents and nothing in the mesh points at them any more.
          </p>
        {/if}
        {#if moved.length}
          <p class="hint">
            Moved: {moved
              .slice(0, 6)
              .map((j) => j.name)
              .join(', ')}{moved.length > 6 ? ` +${moved.length - 6} more` : ''}
          </p>
          <div class="actions">
            <button class="btn-sec small" onclick={undoJoints}>Put every joint back</button>
          </div>
        {/if}
        <details>
          <summary>Why moving a joint is worth doing</summary>
          <p class="hint">
            🔴 It is not cosmetic. The retarget measures your rest bone directions against the
            motion model's and adds the mismatch to every frame. A take is 0.22 s, and it reports
            <code>rest_arm_deg</code> / <code>rest_leg_deg</code> / <code>rest_spine_deg</code> — bake
            one and watch those numbers. The mesh does not move: the joint moves inside it, and its rotation
            pivot moves with it.
          </p>
        </details>
      {:else if mode === 'pieces'}
        <h3>Stray pieces</h3>
        <p class="hint">
          These characters come out as many separate shells rather than one solid, and some are
          floating junk. Hover the model to find them; click to select, and what you delete is what
          is lit. Deleting rewrites only the index buffer — no vertex moves, so skin weights and UVs
          cannot be damaged by it.
        </p>
        {#if labels}
          <p class="stat">
            {labels.components.length.toLocaleString()} connected pieces · largest is the character
          </p>
          {#if strays.length === 0}
            <p class="hint ok">One piece. Nothing to tidy here.</p>
          {:else}
            <ul class="pieces">
              {#each strays.slice(0, 12) as piece (piece.id)}
                <li class:on={piece.id === selectedComponent}>
                  <button
                    class="piece"
                    onclick={() => (selectedComponent = piece.id)}
                    onpointerenter={() => (hoveredComponent = piece.id)}
                    onpointerleave={() => (hoveredComponent = -1)}
                  >
                    <span>{piece.triangles.toLocaleString()} tris</span>
                    <span class="stat">{(piece.sizeRatio * 100).toFixed(1)}% of the model</span>
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        {/if}
        {#if interiorProgress}
          <!-- The whole point of the pass being chunked: a bar that moves and a button that stops
               it. ⚠️ `done` counts faces CLASSIFIED, not work done — an exterior face costs one ray
               and a buried one costs all 64 — so it crawls through the guts and sprints over open
               surface. -->
          <div class="progress">
            <progress max={interiorProgress.total || 1} value={interiorProgress.done}></progress>
            <span class="stat">
              {interiorProgress.done.toLocaleString()} / {interiorProgress.total.toLocaleString()} faces
            </span>
            <button class="btn-sec small" onclick={() => interiorAbort?.abort()}>Cancel</button>
          </div>
        {/if}
        <div class="actions">
          <button class="btn-sec small" onclick={analyseMesh} disabled={!!meshBusy || !body}>
            {labels ? 'Re-scan' : 'Find pieces'}
          </button>
          <button
            class="btn-sec small"
            onclick={() => stageDropUnder(0.02)}
            disabled={!labels || !!pending}
          >
            Everything under 2%…
          </button>
          <button
            class="btn-sec small"
            onclick={stageDropDust}
            disabled={!!meshBusy || !body || !!pending}
          >
            Prune dust…
          </button>
          <button
            class="btn-sec small"
            onclick={stageInterior}
            disabled={!!meshBusy || !body || !!pending}
          >
            Hidden interior faces…
          </button>
          <button
            class="btn-sec small"
            onclick={stageDegenerates}
            disabled={!!meshBusy || !body || !!pending}
          >
            Degenerate triangles…
          </button>
        </div>
        <p class="hint">
          The four with an ellipsis compute first and light up what they would take. Nothing is
          removed until you confirm it.
        </p>
        <div class="knobs">
          <label class="knob">
            <span class="k">Simplify to</span>
            <input type="range" min="0.05" max="0.95" step="0.05" bind:value={decimateRatio} />
            <b>{(decimateRatio * 100).toFixed(0)}%</b>
          </label>
        </div>
        <div class="actions">
          <button
            class="btn-sec small"
            onclick={stageDecimate}
            disabled={!!meshBusy || !body || !!pending || !!decimateReport}
          >
            Simplify the mesh…
          </button>
        </div>
        <p class="hint">
          Fewer triangles over the <strong>same vertices</strong> — index-only, like everything else here,
          so the skin and the UVs are untouched by construction. It computes first and reports what it
          actually achieved, which is routinely not what you asked for.
        </p>
        {#if meshDirty}
          <div class="actions">
            {#if !compacted}
              <button class="btn-sec small" onclick={compact} disabled={!!meshBusy || hasMorphs}>
                Drop orphaned vertices
              </button>
              <button class="btn-sec small" onclick={restoreMesh} disabled={!originalIndex}>
                Put the mesh back
              </button>
            {/if}
          </div>
          {#if compacted}
            <p class="hint">
              Orphaned vertices dropped. Saving will rewrite every attribute.
              <strong>Put the mesh back is gone:</strong> the delivered index addresses the delivered
              vertex numbering, which no longer exists. Discard and reload is the way back, and the session
              history carries a barrier saying the same thing.
            </p>
          {:else}
            <p class="hint">
              Dropping orphans shrinks the file. It is the one mesh step that rewrites every
              attribute rather than just the index, so it is a deliberate press — and it cannot be
              undone in place.
            </p>
          {/if}
          {#if hasMorphs && !compacted}
            <p class="warn">
              ⚠️ This mesh has morph targets and the save path has no morph-target step, so
              compacting would leave them pointing at the old vertex numbering. Disabled.
            </p>
          {/if}
        {/if}
        {#if removedPieces > 0}
          <p class="hint">
            {removedPieces} piece{removedPieces === 1 ? '' : 's'} deleted — the scan is re-run after every
            delete, so what you see is what is left. Their vertices stay in the file, costing size and
            nothing else.
          </p>
        {/if}
        {#if removedFaces > 0}
          <p class="hint">
            Removed {removedFaces.toLocaleString()} faces — invisible ones were paying full file size
            and z-fighting wherever two surfaces nearly touched.
          </p>
        {/if}
        {#if degenerateRemoved > 0}
          <p class="hint">Removed {degenerateRemoved.toLocaleString()} zero-area triangles.</p>
        {/if}
        {#if decimatedTo > 0}
          <p class="hint">
            Simplified to {decimatedTo.toLocaleString()} triangles. The vertices are all still in the
            file until you drop the orphans.
          </p>
        {/if}
      {:else if mode === 'weights'}
        <h3>Skin-weight leak</h3>
        <p class="hint">
          Vertices bound to a joint far away get flung across the character when it rotates. The
          engine already ran its own cleanup pass and kept the original skin wherever it could not
          prove an improvement, so what is left here is what that pass declined.
        </p>
        <div class="actions">
          <button class="btn-sec small" onclick={analyse} disabled={!!busy || !stage}>
            {report ? 'Re-measure' : 'Measure'}
          </button>
        </div>
        {#if report}
          <p class="stat">
            {report.vertices.toLocaleString()} vertices · {report.influences.toLocaleString()} influences
            · farthest reaches <b>{(report.maxRatio * 100).toFixed(0)}%</b> of the model's size
          </p>
          {#if preview}
            <p class="hint" class:warn={preview.prunedInfluences > report.influences * 0.01}>
              Would drop <b>{preview.prunedInfluences.toLocaleString()}</b>
              influences across
              <b>{preview.verticesTouched.toLocaleString()}</b>
              vertices ({pct(preview.verticesTouched, report.vertices)}).
              {#if preview.verticesSpared > 0}
                {preview.verticesSpared.toLocaleString()} vertices whose only bindings are beyond the
                threshold are left untouched — zeroing those would unskin them.
              {/if}
              {#if preview.prunedInfluences > report.influences * 0.01}
                🔴 That is over 1% of every influence in the model. A real leak is a fraction of a
                percent; at this threshold you are re-rigging by hand, not repairing.
              {/if}
            </p>
            {#if preview.byBone.length}
              <p class="hint">
                Mostly from: {preview.byBone
                  .slice(0, 4)
                  .map((b) => `${b.name} (${b.influences})`)
                  .join(', ')}
              </p>
            {/if}
          {/if}
          <div class="actions">
            <button
              class="btn-sec small"
              onclick={applyWeightPrune}
              disabled={!!busy || !preview?.prunedInfluences}
            >
              Prune {preview?.prunedInfluences ?? 0} influences
            </button>
            {#if applied.length}
              <button class="btn-sec small" onclick={undoPrune} disabled={!!busy}>
                Put the weights back
              </button>
            {/if}
          </div>
          <details>
            <summary>The distribution</summary>
            <!-- What says whether there IS a leak. A healthy rig runs out of influences well
                 before 0.3; a leak shows as a flat tail that does not. -->
            <ul class="tail">
              {#each report.tail as row (row.ratio)}
                <li class:zero={row.influences === 0}>
                  <span class="ratio">&gt; {(row.ratio * 100).toFixed(0)}%</span>
                  <span class="count">{row.influences.toLocaleString()}</span>
                  <span class="share">{pct(row.influences, report.influences)}</span>
                </li>
              {/each}
            </ul>
          </details>
        {/if}
      {:else if mode === 'paint'}
        <h3>Paint the skin</h3>
        <p class="hint">
          The prune tool can only <strong>delete</strong> a binding. This rebinds: a vertex on a plausible
          but wrong joint, or a seam where two adjacent vertices disagree about which joint owns them,
          needs a brush and nothing else will do it.
        </p>
        <p class="hint">
          The order is <strong>pick, then paint</strong>, and both happen here:
          <code>pick</code> turns the brush into a joint sampler — click the character and whichever joint
          carries the most weight under the cursor becomes the target, then the brush hands itself back
          to the tool you were using. There is no need to visit Joints mode for this.
        </p>
        <p class="hint">
          Every dab is a blend between two bindings that each sum to one, so the weights cannot
          drift — and split vertices are painted as a <strong>group</strong>: one point on the
          surface is often three vertices, and rebinding only the one the ray hit tears the surface
          open the moment the joint moves.
        </p>
        {#if paintRefusal}
          <p class="warn">{paintRefusal}</p>
          {#if canEdit && paintNeedsBone}
            <div class="actions">
              <button class="btn-sec small" onclick={() => selectTool('pick')}>
                Pick one off the model
              </button>
              <button class="btn-sec small" onclick={() => selectTool('smooth')}>
                Smooth instead — it needs no target
              </button>
              <button class="btn-sec small" onclick={() => (mode = 'joints')}>
                Or choose from the joint list
              </button>
            </div>
          {/if}
        {:else}
          <p class="stat">
            {#if paintTool === 'pick'}
              Picking · click the character and the joint driving that patch becomes the target
            {:else if paintTool === 'smooth'}
              Smoothing · radius {(brushScale * 100).toFixed(1)}% of the model
            {:else}
              {paintTool === 'erase' ? 'Erasing' : 'Assigning'}
              <code>{shortBone(selectedJoint)}</code> · radius {(brushScale * 100).toFixed(1)}% of
              the model
            {/if}
          </p>
          <div class="actions">
            <button class="btn-sec small" onclick={() => selectTool('pick')} disabled={!stage}>
              Pick a joint off the model
            </button>
            <button class="btn-sec small" onclick={toggleDrives} disabled={selectedIndex < 0}>
              {drivingJoint === selectedJoint
                ? 'Stop showing what it drives'
                : 'Show what it drives'}
            </button>
          </div>
          <p class="hint">
            🔴 <strong>What it drives</strong> is lit automatically in this mode, and it is the whole
            readout: a dab moves numbers in two attributes, and the only thing you can SEE is which vertices
            the joint owns afterwards. It is re-measured after every stroke, so the patch growing or shrinking
            under the brush is the edit itself.
          </p>
        {/if}
        {#if strokeTally && strokeTally.dabs > 0}
          <p class="stat">
            Last stroke: {strokeTally.dabs} dabs over {strokeTally.vertices.toLocaleString()} vertex writes
          </p>
          {#if strokeTally.evictions > 0}
            <p class="warn">
              ⚠️ {strokeTally.evictions.toLocaleString()} influences were evicted. A vertex holds
              <strong>four</strong> joints and no more, so painting a fifth throws the smallest one out
              — which moves the vertex slightly, by roughly the dropped weight times the distance to the
              joint it was dropped from.
            </p>
          {/if}
          {#if strokeTally.spared > 0}
            <p class="hint">
              {strokeTally.spared.toLocaleString()} vertices were left exactly as they were: erasing their
              only binding would have <strong>unskinned</strong> them, and an unskinned vertex deforms
              with nothing.
            </p>
          {/if}
          {#if strokeTally.declined > 0}
            <p class="hint">
              {strokeTally.declined.toLocaleString()} vertices at the rim declined the paint — the incoming
              weight was fainter than one step of the file's own quantisation, and it would have cost
              a real binding to store it.
            </p>
          {/if}
          {#if strokeTally.meshesSkipped > 0}
            <p class="warn">
              ⚠️ {strokeTally.meshesSkipped}
              {strokeTally.meshesSkipped === 1 ? 'mesh pass was' : 'mesh passes were'} skipped: that joint
              is not in their skeleton, or their joint attribute cannot hold its index.
            </p>
          {/if}
        {/if}
        {#if strokes > 0}
          <p class="stat">{strokes} stroke{strokes === 1 ? '' : 's'} this session</p>
          <div class="actions">
            <button class="btn-sec small" onclick={undoPrune} disabled={!restoreWeights || !!busy}>
              Put the delivered skin back
            </button>
          </div>
          <p class="hint">
            That button is the coarse one: it reverses <strong>every</strong> stroke and every prune at
            once. One stroke at a time is Ctrl+Z.
          </p>
        {/if}
        <details>
          <summary>What a stroke costs, and what it cannot fix</summary>
          <p class="hint">
            A stroke's undo holds only the vertices it touched — tens of kilobytes, against 3.5 MB
            for a copy of the whole skin — so the history keeps a hundred of them for what two
            coarse restores would cost.
          </p>
          <p class="hint">
            🔴 It paints where the model is <strong>at rest</strong>, which is why every rig tool
            here is refused on a take: the spatial index is built over the bind-pose positions, so
            under a playing clip it would paint the vertices that happen to be under the cursor on a
            model standing still.
          </p>
        </details>
      {:else if mode === 'uv'}
        <h3>The atlas</h3>
        <p class="hint">
          The texture is the source photo <strong>projected</strong> onto the reconstruction and packed
          automatically. When something looks wrong on the model — a seam, a smear, a patch of the wrong
          colour — the explanation is in here, and this is read-only: it changes no pixel and no vertex.
        </p>
        {#if !body}
          <p class="warn">⚠️ No mesh loaded.</p>
        {:else}
          <!-- 🔴 Keyed, so an index edit or a colour bake REMOUNTS it. Its islands, hit grid and
               coverage are indexed by face and measured against the texels that were there; after
               either it would be reporting confidently about a mesh or an image that is gone. -->
          {#key uvVersion}
            <UvPanel
              renderer={stage?.renderer ?? null}
              mesh={body}
              texture={baseMaterial?.map ?? null}
              highlightFace={pickedFace}
              onHighlightFaces={(faces: ReadonlySet<number> | null) => (uvFaces = faces)}
              onHoverFace={(face: number) => (uvHoverFace = face)}
              onFocusFace={focusFace}
            />
          {/key}
          <p class="hint">
            Click the model to point the atlas at a face{#if pickedFace >= 0}
              — currently face {pickedFace.toLocaleString()}{/if}. An island you select in the atlas
            is drawn on the character; double-click one to fly the camera to it.
          </p>
          {#if uvHoverFace >= 0}
            <p class="stat">Under the cursor: face {uvHoverFace.toLocaleString()}</p>
          {/if}
        {/if}
      {:else if mode === 'colour'}
        <h3>Surface</h3>
        <p class="hint">
          The sliders are in the canvas, over the model they change. The texture is the source
          photo, projected — so if it came out dark, that is the photo, and the engine will never
          "correct" it, which is why this is here.
        </p>
        <p class="hint">
          <strong>Seams and gaps are not offered:</strong> the baker already seals the gutter and fills
          interior holes, and re-doing it in the browser would bleed neighbouring UV islands together.
        </p>
        <p class="hint">
          Normal strength is the shader's own <code>normalScale</code> while you drag it — free, full
          precision and exactly flat at 0. Baking it into the image is a deliberate press, for when the
          weakening has to survive into a viewer that reads the file and ignores the material.
        </p>
        {#if !hasBaseMap}
          <p class="warn">
            ⚠️ This body has no single baseColor map, so there is nothing to grade.
          </p>
        {/if}
        {#if !hasNormalMap}
          <p class="warn">⚠️ This body has no single normal map to weaken.</p>
        {/if}
      {:else if mode === 'motion'}
        <h3>Limbs through the body</h3>
        <p class="hint">
          🔴 This is <strong>not a bug to fix upstream</strong>. Motion is transferred joint by
          joint from a source skeleton with no idea where the mesh is, so self-intersection is
          normal for this class of retarget. What follows is a post-hoc correction on the clip, and
          <strong>nothing scores whether the result is right</strong> — the count falling is a signal,
          not a verdict. Look at the model.
        </p>
        {#if stageClip}
          <p class="stat">
            Clip <code>{stageClip}</code>
            {#if frameTotal > 1}· frame {frameNow + 1}/{frameTotal}{/if}
            {#if !playing}· held{/if}
          </p>
        {/if}
        <div class="actions">
          <button class="btn-sec small" onclick={detectMotion} disabled={!!motionBusy}>
            {motionFound ? 'Re-check every frame' : 'Find contacts'}
          </button>
          {#if !playing}
            <button class="btn-sec small" onclick={resumePlay}>Play</button>
          {/if}
        </div>

        {#if motionFound}
          {#if motionFound.length === 0}
            <p class="hint ok">
              No limb capsule is inside a body capsule on any frame
              {#if motionPrevCount !== null}(was {motionPrevCount.toLocaleString()}){/if}.
            </p>
          {:else}
            <p class="warn">
              {motionFound.length.toLocaleString()} intersecting frame-pairs in
              {contacts.length} contact{contacts.length === 1 ? '' : 's'}
              {#if motionPrevCount !== null && motionPrevCount !== motionFound.length}
                — was {motionPrevCount.toLocaleString()}
              {/if}
            </p>
            <ul class="contacts">
              {#each contacts as contact, i (`${contact.limb}|${contact.body}|${contact.from}`)}
                <li class:on={i === selectedContact}>
                  <button class="contact" onclick={() => showContact(i)}>
                    <span class="pair">
                      <b class="danger-ink">{shortBone(contact.limb)}</b>
                      inside
                      <b class="select-ink">{shortBone(contact.body)}</b>
                    </span>
                    <span class="stat">
                      {#if contact.from === contact.to}
                        frame {contact.from}
                      {:else}
                        frames {contact.from}–{contact.to} ({contact.frames})
                      {/if}
                      · deepest {depthOf(contact.peakDepth)} at {contact.peakFrame}
                    </span>
                  </button>
                </li>
              {/each}
            </ul>
            <p class="hint">
              Click one to hold the model on its worst frame with both bones lit — the limb in red,
              the part it is buried in in blue.
            </p>
            <label class="toggle">
              <input type="checkbox" bind:checked={motionScoped} /> Only correct those frames
            </label>
            <p class="hint">
              Scoped corrections ramp in and out over a few frames — a hard-edged range edit leaves
              a first-derivative discontinuity, which reads as a snap. Leave it off and the whole
              clip moves, which is usually what you want and never pops. The envelope follows the
              selected contact's limb, or the deepest one when nothing is selected.
            </p>
          {/if}
        {/if}

        <details>
          <summary>What the check can and cannot see</summary>
          <p class="hint">
            ⚠️ Each bone is a capsule fattened to the skin it carries, which is a proxy and not the
            mesh. An arm brushing the ribs is not a defect — there is a contact-slack threshold for
            exactly that — and a hand passing through a skirt is <strong>not modelled at all</strong
            >, because a skirt is not a bone. This finds the gross failures: a forearm buried in the
            ribcage. A clean count is not a clean take.
          </p>
        </details>
      {/if}
    </section>

    <section class="card panel">
      <div class="undo-bar">
        <button
          class="btn-sec small"
          onclick={undoStep}
          disabled={!canUndo}
          title={undoLabel ?? 'Nothing to undo'}
        >
          Undo
        </button>
        <button
          class="btn-sec small"
          onclick={redoStep}
          disabled={!canRedo}
          title={redoLabel ?? 'Nothing to redo'}
        >
          Redo
        </button>
        {#if undoLabel}<span class="stat">next: {undoLabel}</span>{/if}
      </div>
      {#if historyView.entries.length === 0}
        <p class="hint">
          Nothing to undo yet. <kbd>Ctrl</kbd>/<kbd>⌘</kbd>+<kbd>Z</kbd> works while the canvas or anything
          else in this panel has focus — never inside a text field, which keeps its own.
        </p>
      {:else}
        <details open>
          <summary>Session history ({historyView.cursor}/{historyView.entries.length})</summary>
          <ol class="history">
            {#each historyView.entries as row (row.id)}
              <li class:undone={!row.applied} class:barrier={row.kind === 'barrier'}>
                <span class="what">{row.label}</span>
                {#if row.reason}
                  <span class="why">{row.reason}</span>
                {/if}
              </li>
            {/each}
          </ol>
          <p class="hint">
            Oldest steps are dropped first once the stack outgrows its memory budget — the big
            captures are a skin-weight copy (~3.5 MB) and an index buffer. A dropped step is gone,
            and the list shrinks with it rather than offering an undo that would do nothing.
          </p>
        </details>
      {/if}
    </section>

    <section class="card panel">
      <div class="knobs">
        <!-- ⚠️ Same precedence as the fallback in `save()`. These disagreed — placeholder
             `baseTier || viewLabel`, actual `viewLabel || baseTier` — so a host carrying both showed
             the user one name and then wrote a different one into the file. -->
        <label>
          Name this repair
          <input
            type="text"
            bind:value={repairLabel}
            placeholder={`${viewLabel || baseTier || 'model'} · repaired`}
          />
        </label>
      </div>
      {#if dirtySummary.length}
        <p class="stat">Pending: {dirtySummary.join(' · ')}</p>
      {:else}
        <p class="hint">Nothing changed yet.</p>
      {/if}
      {#if colourTouched && !colourApplied}
        <p class="warn">
          ⚠️ The colour on screen is a preview only. Press Apply in the canvas to bake it, or it
          will not be in the saved file.
        </p>
      {/if}
      {#if normalStrength !== 1 && !normalApplied}
        <p class="warn">
          ⚠️ The normal strength on screen is the material's multiplier, not the file. Bake it in
          the canvas if the weakening has to be in the saved GLB.
        </p>
      {/if}
      {#if onSave && !stagingReady}
        <p class="warn">
          ⚠️ The shared staging directory is not mounted, so a saved edit can be previewed and
          downloaded but <strong>not animated</strong> — the engine container cannot open a file it does
          not mount.
        </p>
      {/if}
      {#if !onSave}
        <p class="hint">
          This viewer has nowhere to store an edit, so Save writes the patched GLB straight to your
          browser's downloads. Nothing on the server changes.
        </p>
      {/if}
      <div class="actions">
        <button class="btn-primary" onclick={save} disabled={!dirty || !!busy}>
          {busy || (onSave ? 'Save as a new asset' : 'Save a copy to my downloads')}
        </button>
        <button class="btn-sec" onclick={revert} disabled={!dirty || !!busy}>
          Discard and reload
        </button>
      </div>
      <p class="hint">
        Saving patches the original file rather than rebuilding it, so the textures keep their exact
        bytes and every joint keeps its <code>mixamorig:</code> name — which is what the retarget matches
        on. Only the parts you edited are rewritten.
      </p>

      {#if skeletonEdited && canPropagate}
        <div class="ladder">
          <h4>The rest of the ladder</h4>
          <p class="hint">
            This repair moved joints, and all {tiers.length} tiers are built over
            <strong>one skeleton</strong> — so the same fix lands identically on every one of them.
            Without this you would ship a repaired
            <code>{baseTier}</code>
            next to an unrepaired <code>{otherTiers[otherTiers.length - 1]}</code>, which is usually
            the tier a game actually loads.
          </p>
          <p class="hint">
            ⚠️ Only joint moves and renames carry. A weight prune, a deleted piece and a colour
            change are per-geometry — each tier has its own — so they stay on
            <code>{baseTier}</code> alone.
          </p>
          {#if dissolvedNames.length}
            <p class="warn">
              🔴 A dissolved joint does NOT carry either, and it is the one that leaves the ladder
              uneven: <code>{baseTier}</code> will have {dissolvedNames.length} joint{dissolvedNames.length ===
              1
                ? ''
                : 's'} fewer than the others, and the weights that moved onto the parent are per-geometry
              as well. Dissolve the same joint on each tier, or accept that only this one is cut.
            </p>
          {/if}
          <div class="actions">
            <button
              class="btn-sec small"
              onclick={propagate}
              disabled={!!propagating || propagated.length === otherTiers.length}
            >
              {#if propagating}
                Applying to {propagating}…
              {:else if propagated.length === otherTiers.length}
                Applied to every tier
              {:else}
                Apply to the other {otherTiers.length - propagated.length} tier{otherTiers.length -
                  propagated.length ===
                1
                  ? ''
                  : 's'}
              {/if}
            </button>
            {#if propagated.length}
              <span class="stat">done: {propagated.join(', ')}</span>
            {/if}
          </div>
        </div>
      {/if}
    </section>
  </aside>
</div>

<style>
  .workbench {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 360px;
    gap: 16px;
    align-items: start;
    margin-bottom: 16px;
  }
  .stage-col {
    min-width: 0;
  }
  .sticky {
    position: sticky;
    top: 12px;
  }
  .inspector {
    display: flex;
    flex-direction: column;
    gap: 12px;
    min-width: 0;
  }
  /* Content ≈ 860px: the inspector has nowhere to sit beside a usable canvas. */
  @media (max-width: 1080px) {
    .workbench {
      grid-template-columns: minmax(0, 1fr);
    }
    .sticky {
      position: static;
    }
  }
  .panel {
    padding: 16px;
  }
  .panel h3 {
    margin: 0 0 10px;
    font-size: 15px;
  }
  .danger-panel {
    border-color: var(--danger, #ef4444);
  }
  h4 {
    margin: 0 0 6px;
    font-size: 13px;
    color: var(--ink);
  }
  .ladder {
    margin-top: 16px;
    padding-top: 14px;
    border-top: 1px solid var(--ring);
  }

  /* ── in-canvas layer ─────────────────────────────────────────────── */
  .rail {
    position: absolute;
    top: 10px;
    left: 10px;
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 6px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 8px 24px rgb(0 0 0 / 35%);
    pointer-events: auto;
  }
  .mode {
    padding: 5px 12px;
    background: none;
    border: 1px solid transparent;
    border-radius: 6px;
    color: var(--muted);
    font-family: inherit;
    font-size: 12px;
    text-align: left;
    cursor: pointer;
  }
  .mode:hover:not(:disabled) {
    color: var(--ink);
    border-color: var(--ring);
  }
  .mode.on {
    color: var(--accent);
    border-color: var(--accent);
  }
  .mode:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .skel {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 4px;
    padding: 6px 8px 2px;
    border-top: 1px solid var(--ring);
    color: var(--muted);
    font-size: 11px;
  }
  .notes {
    position: absolute;
    top: 10px;
    right: 10px;
    max-width: 300px;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 6px;
  }
  .note {
    padding: 6px 10px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--card);
    color: var(--ink);
    font-size: 11px;
    line-height: 1.4;
  }
  .note.refusal {
    color: var(--warning, #d9952d);
  }
  .note.work {
    color: var(--muted);
  }
  .contact-note {
    font-variant-numeric: tabular-nums;
  }
  .floater {
    position: absolute;
    left: 10px;
    bottom: 10px;
    pointer-events: auto;
  }
  /* Two HUDs on a short window would run off the top of the canvas, so the column scrolls rather
     than growing. */
  .floater.stack {
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-height: calc(100% - 60px);
    overflow-y: auto;
  }
  /* ColourHud and NormalHud bring their own card; the slider floaters need one. */
  .panel-mini {
    width: 268px;
    box-sizing: border-box;
    padding: 10px 12px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 10px 30px rgb(0 0 0 / 45%);
  }
  .knob {
    display: grid;
    grid-template-columns: 68px 1fr 38px;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: var(--muted);
  }
  .knob .k {
    white-space: nowrap;
  }
  .knob b {
    color: var(--ink);
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  .knob input[type='range'] {
    width: 100%;
    min-width: 0;
  }
  /* The falloff picker has no numeric readout, so it takes the value column as well. */
  .knob select {
    grid-column: 2 / span 2;
    padding: 3px 6px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    color: var(--ink);
    font-family: inherit;
    font-size: 11px;
  }
  .target {
    margin: 0 0 8px;
    color: var(--muted);
    font-size: 11px;
  }
  .target b {
    color: var(--accent);
  }
  .warn-ink {
    color: var(--warning, #d9952d);
  }
  .picked {
    margin: 6px 0 0;
    font-size: 11px;
    line-height: 1.45;
    color: var(--muted);
  }
  .picked b {
    color: var(--ink);
  }
  .chip {
    padding: 3px 9px;
    background: none;
    border: 1px solid var(--ring);
    border-radius: 999px;
    color: var(--muted);
    font-family: inherit;
    font-size: 11px;
    cursor: pointer;
  }
  .chip:hover {
    color: var(--ink);
  }
  .chip.on {
    color: var(--accent);
    border-color: var(--accent);
  }
  .row {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  /* Faded rather than hidden: a joint you orbited past should still be reachable. Positioning is
     `ModelStage`'s wrapper's job — see the note in the snippet. */
  .anchored.behind {
    opacity: 0.45;
  }

  /* ── inspector bits ──────────────────────────────────────────────── */
  .hint {
    color: var(--muted);
    font-size: 11px;
    line-height: 1.45;
    margin: 8px 0 0;
  }
  .hint.ok {
    color: var(--ok, #4ade80);
  }
  .warn {
    color: var(--warning, #d9952d);
    font-size: 11px;
    line-height: 1.45;
    margin: 8px 0 0;
  }
  .stat {
    color: var(--muted);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }
  .ok-box {
    padding: 8px 12px;
    border: 1px solid var(--ok, #4ade80);
    border-radius: 8px;
    color: var(--ok, #4ade80);
    font-size: 12px;
  }
  details {
    margin-top: 12px;
  }
  summary {
    color: var(--muted);
    font-size: 11px;
    cursor: pointer;
  }
  kbd {
    padding: 0 4px;
    border: 1px solid var(--ring);
    border-radius: 4px;
    font-family: inherit;
    font-size: 10px;
  }
  .tail {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 2px 12px;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }
  .tail li {
    display: flex;
    gap: 8px;
    color: var(--ink);
  }
  .tail li.zero {
    color: var(--muted);
    opacity: 0.55;
  }
  .tail .ratio {
    width: 62px;
    color: var(--muted);
  }
  .tail .count {
    flex: 1;
  }
  .review,
  .sym {
    margin-top: 16px;
    padding-top: 14px;
    border-top: 1px solid var(--ring);
  }
  .review {
    border-color: var(--warning, #d9952d);
  }
  .proposals {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-height: 300px;
    overflow-y: auto;
  }
  .proposals li {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
  }
  .proposals .pair {
    display: flex;
    align-items: baseline;
    gap: 6px;
    flex-wrap: wrap;
    color: var(--muted);
  }
  .proposals b {
    color: var(--ink);
  }
  .btns {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .missing {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .missing li {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 12px;
  }
  .missing code {
    min-width: 175px;
  }
  .pieces {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 220px;
    overflow-y: auto;
  }
  .pieces li {
    display: flex;
    align-items: center;
    gap: 8px;
    border-radius: 6px;
  }
  .pieces li.on {
    background: var(--bg-2);
  }
  .piece {
    flex: 1;
    display: flex;
    gap: 10px;
    align-items: baseline;
    background: none;
    border: none;
    color: var(--ink);
    font-family: inherit;
    font-size: 12px;
    cursor: pointer;
    padding: 5px 6px;
    text-align: left;
  }
  .contacts {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 260px;
    overflow-y: auto;
  }
  .contacts li {
    border-radius: 6px;
  }
  .contacts li.on {
    background: var(--bg-2);
  }
  .contact {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 2px;
    background: none;
    border: none;
    color: var(--ink);
    font-family: inherit;
    font-size: 12px;
    cursor: pointer;
    padding: 5px 6px;
    text-align: left;
  }
  .contact .pair {
    color: var(--muted);
  }
  /* The two tones the spotlight paints on the model, so the row and the character agree. */
  .danger-ink {
    color: var(--danger, #ef4444);
  }
  .select-ink {
    color: var(--accent);
  }
  .toggle {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    color: var(--ink);
    font-size: 13px;
    margin-top: 10px;
  }
  .progress {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 12px;
  }
  .progress progress {
    flex: 1;
    min-width: 0;
    height: 6px;
  }
  .undo-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .history {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
    max-height: 220px;
    overflow-y: auto;
    font-size: 11px;
  }
  .history li {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 3px 6px;
    border-radius: 6px;
    color: var(--ink);
  }
  .history li.undone {
    color: var(--muted);
    opacity: 0.55;
  }
  .history li.barrier {
    border: 1px dashed var(--warning, #d9952d);
    color: var(--warning, #d9952d);
  }
  .history .why {
    color: var(--muted);
    line-height: 1.4;
  }
  .actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 12px;
  }
  .btn-sec.small {
    padding: 4px 10px;
    font-size: 12px;
  }
  .knobs label {
    display: block;
    font-size: 12px;
    color: var(--muted);
  }
  input[type='text'] {
    box-sizing: border-box;
    width: 100%;
    padding: 6px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font-family: inherit;
    font-size: 12px;
  }
</style>
