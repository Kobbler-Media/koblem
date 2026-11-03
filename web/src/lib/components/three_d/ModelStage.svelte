<script lang="ts" module>
  import type { Vector3 } from 'three';

  import type { StageHandle } from './stage';

  /**
   * What the canvas draws, what a click does, and what a hover reports — one control, not four
   * independent checkboxes.
   *
   * 🔴 `pieces`, `weights` and `paint` are REST-POSE ONLY. The BVH behind picking and the ratios
   * behind the heat map are both built over the bind-space `position` attribute, so under a playing
   * clip they answer about a model that is not the one on screen. A GLB carrying animation tracks is
   * refused those modes here rather than trusted to the caller — and `paint` most of all, since it
   * WRITES to the skin through a point picked off that stale surface.
   */
  export type StageMode = 'view' | 'joints' | 'pieces' | 'weights' | 'paint';

  /**
   * Where an in-canvas popover should sit: CSS px inside the viewport element, origin top-left.
   *
   * `behind` is true when the anchored point is on the far side of the model from the camera, or
   * behind the camera entirely — where the projection is mirrored and `x`/`y` are clamped to the
   * viewport edge. FADE the popover on it; do not hide it, or a joint you orbited past vanishes
   * with no way to tell whether it is still selected.
   */
  export interface StageAnchor {
    x: number;
    y: number;
    behind: boolean;
  }

  /**
   * Where the CAMERA is put for a view preset, named by the world axis it sits on.
   *
   * 🔴 Deliberately NOT front/back/left/right. Front-facing yaw is not canonical in this pipeline —
   * some characters are delivered facing away — so a button labelled "Front" is a claim this
   * component cannot make, and would be wrong on a real fraction of the library. `+Z` is a fact
   * about the scene and stays true whichever way the subject looks. There is no `-y`: a view from
   * underneath has never been the thing anyone wanted, and the up-axis is the awkward one (see
   * `AXIS_HEADING`).
   */
  export type ViewAxis = '+x' | '-x' | '+y' | '+z' | '-z';

  /**
   * [`StageHandle`] plus the two things only the live viewport can do.
   *
   * ⚠️ Declared here rather than in `stage.ts` because the overlays whose lifetime these methods
   * manage live in this component; `stage.ts` is the vocabulary the viewer and the editor share.
   * Anything that only needs the model can keep taking a plain `StageHandle` — this extends it.
   */
  export interface EditorStageHandle extends StageHandle {
    /**
     * 🔴 CALL THIS AFTER EVERY INDEX OR VERTEX CHANGE — `applyIndex`, `removeComponents`,
     * `compactVertices`, `removeDegenerateTriangles`, a prune.
     *
     * The overlays and the picking BVH are all keyed by triangle number. Once the index shrinks, a
     * face id resolves to a DIFFERENT triangle and nothing bounds-checks its way to noticing: the
     * stale map is merely longer. The highlight then lights up one part of the character and the
     * delete removes another. `RigEditor.svelte:228-235` documents the same failure on the label
     * side; this is the viewport half of it.
     *
     * It also drops the WEIGHT-PAINT cache, which is the one that does not heal itself — see the
     * 🔴 on the call.
     *
     * Cheap — it throws caches away, it does not rebuild them. The next hover pays the BVH build.
     */
    invalidateGeometry(): void;
    /**
     * Take the GPU colour preview off the material, synchronously.
     *
     * ⚠️ Setting `grade` to `null` does the same thing, but only once Svelte flushes the effect —
     * which is after the caller's next statement. `adjustColour` repaints the texture IN that next
     * statement, so a commit relying on the prop would grade already-graded texels for a frame.
     * Call this first, then commit. Idempotent.
     */
    detachGrade(): void;
    /**
     * Fly the camera to a world point over ~320 ms and leave `controls.target` there.
     *
     * `radius` is the size of the thing being looked at, in world units — a component's
     * half-diagonal, a joint's reach. Omit it and the camera settles at a distance that reads a
     * joint without losing the body around it.
     *
     * 🔴 The MODEL IS NEVER MOVED. Every camera affordance here writes `camera.position` and
     * `controls.target`; a framing translation on `root` would be baked into every saved repair and
     * would break the re-bind maths. See the long note in `install`.
     */
    focusOn(point: Vector3, radius?: number): void;
    /**
     * Frame whatever is selected: the joint in `joints` mode, the component's centre in `pieces`
     * mode, the whole model when nothing is picked. What `F` does.
     */
    focusSelection(): void;
    /** Put the camera on a world axis and re-fit the model. ⚠️ Axes, not anatomy — see `ViewAxis`. */
    setView(axis: ViewAxis): void;
    /** Keyframe count of the clip on screen; 0 on a rig with no tracks. */
    frameCount(): number;
    /**
     * Jump the model to one frame of the clip on screen and HOLD it there. Clamped to the clip.
     *
     * ⚠️ Scrubbing pauses playback — that is what holding a frame means — so `playing` falls to
     * false and an owner bound to it sees its own transport follow.
     */
    scrubTo(frame: number): void;
  }

  /**
   * `1`–`4`, in rail order. Anything else falls through to the other bindings.
   *
   * ⚠️ `paint` deliberately has NO key. These four are the first four entries of the workbench's
   * mode rail and the numbers are only meaningful because they match it — but the rail is the
   * owner's, not this component's, and it already carries entries (`colour`, `motion`) that are not
   * `StageMode`s at all. Guessing a number for `paint` would put a key on a rail position it does
   * not occupy, which is worse than no shortcut: the user learns a mapping that is wrong.
   */
  const MODE_KEYS: Record<string, StageMode | undefined> = {
    '1': 'view',
    '2': 'joints',
    '3': 'pieces',
    '4': 'weights'
  };

  /** Arrow key → screen-plane direction, as `[right, up]`. See `nudgeSelectedJoint`. */
  const NUDGE_KEYS: Record<string, [number, number] | undefined> = {
    ArrowLeft: [-1, 0],
    ArrowRight: [1, 0],
    ArrowUp: [0, 1],
    ArrowDown: [0, -1]
  };

  /**
   * Where the camera sits for each preset, as a direction from the model's centre.
   *
   * ⚠️ `+y` is tilted a hair off vertical on purpose. Looking straight down the up-axis is the
   * singular case for `Object3D.lookAt`, and `OrbitControls` escapes it with `Spherical.makeSafe()`
   * — which picks a side for you. The tilt picks it deliberately, so the top view is the same top
   * view every time.
   */
  const AXIS_HEADING: Record<ViewAxis, [number, number, number]> = {
    '+x': [1, 0, 0],
    '-x': [-1, 0, 0],
    '+y': [0, 1, 0.02],
    '+z': [0, 0, 1],
    '-z': [0, 0, -1]
  };

  const VIEW_AXES: { id: ViewAxis; label: string; hint: string }[] = [
    { id: '+z', label: '+Z', hint: 'Camera on +Z, looking back along −Z' },
    { id: '-z', label: '−Z', hint: 'Camera on −Z, looking back along +Z' },
    { id: '+x', label: '+X', hint: 'Camera on +X, looking back along −X' },
    { id: '-x', label: '−X', hint: 'Camera on −X, looking back along +X' },
    { id: '+y', label: '+Y', hint: 'Camera above, looking down' }
  ];

  /**
   * ⚠️ Set on the viewport element rather than written in the template — see the note where the
   * keyboard is wired up. `aria-keyshortcuts` is a space-separated list of key names.
   */
  const VIEWPORT_LABEL =
    'Model viewport. 1 to 4 switch mode, F frames the selection, Escape clears it, ' +
    'arrow keys nudge the selected joint.';
  const VIEWPORT_SHORTCUTS = '1 2 3 4 F Escape ArrowUp ArrowDown ArrowLeft ArrowRight';
</script>

<script lang="ts">
  /*
   * The Pixal3D viewer: one GLB on screen, its skeleton optionally over it, its baked animation
   * tracks playable — and, since the editor rebuild, the editing surface itself.
   *
   * # The canvas IS the editor
   *
   * Everything a repair tool computes gets DRAWN ON THE MODEL rather than reported as a number: the
   * shell under the cursor, the triangles a delete is about to destroy, the vertices a joint really
   * drives, the colour grade under the slider. The tools' logic stays in the workbench; this
   * component owns the scene, the camera, the loop, the overlays, picking, and the projection that
   * lets the workbench render popovers at 3D points through the `jointPopover` / `piecePopover` /
   * `hud` snippets.
   *
   * 🔴 ANIMATIONS ARE INSIDE THE GLB, not sidecar files. A delivered take carries glTF animation
   * tracks named `<motion>__<variant>` plus a `skeleton-exercise` track that swings every
   * materially weighted joint in turn. That exercise clip is the honest "does this rig actually
   * deform" test and is why this component auto-selects it when nothing else is chosen.
   *
   * ⚠️ FRONT-FACING YAW IS NOT CANONICAL. Some characters face away by default. This auto-frames
   * and lets the user orbit; it never assumes a front.
   *
   * ⚠️ TEXTURES ARE RAW baseColor plus a baked normal map. If a model looks dark, that is the
   * source image — compare against it before "fixing" the lighting. The rig is lit neutrally here
   * on purpose so the comparison is possible.
   *
   * 🔴 OVERLAYS ARE SIBLINGS OF THE MODEL, NEVER CHILDREN OF IT. `overlays.ts` hands each proxy the
   * body's own attribute OBJECTS, and the teardown below runs
   * `root.traverse(o => o.geometry?.dispose())` — a proxy under the root would have the character's
   * GPU buffers freed out from under it. The save path also finds the body by traversing for
   * `isSkinnedMesh` and would be handed an overlay as if it were part of the character. Every
   * overlay is added to the SCENE.
   */
  import { onDestroy, type Snippet } from 'svelte';
  import * as THREE from 'three';
  import { GLTFLoader } from 'three/examples/jsm/loaders/GLTFLoader.js';
  import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js';
  import { fetchMediaBlob } from '#lib/api/client.ts';
  import { clipFrameCount, snapshotClip, type ClipSnapshot } from './motion-repair';
  import { componentAtFace, disposeBvh, raycastSurface, type ComponentLabels } from './mesh-repair';
  import {
    BrushCursor,
    HeatOverlay,
    HoverPicker,
    JointHandles,
    SurfaceOverlay,
    type JointState
  } from './overlays';
  import { attachLiveGrade, type LiveGrade } from './live-grade';
  import { disposeWeightPaintCache, type FalloffProfile } from './weight-paint';
  import type { ColourAdjust } from './texture-repair';

  let {
    src = '',
    showSkeleton = false,
    label = '',
    height = 'min(72vh, 820px)',
    mode,
    jointEdit,
    shellPick = false,
    selectedJoint = '',
    keepChildren = true,
    labels = null,
    selectedComponent = -1,
    previewFaces = null,
    jointStates = null,
    heat = null,
    grade = null,
    brush = null,
    ghostSrc = '',
    ghostOpacity = 0.35,
    playing = $bindable(true),
    scrubFrame = null,
    onReady,
    onError,
    onJointPicked,
    onJointDrag,
    onJointNudge,
    onFacePicked,
    onHoverComponent,
    onPickComponent,
    onHoverJoint,
    onModeRequested,
    onBrushBegin,
    onBrushDab,
    onBrushEnd,
    onBrushHover,
    onFrame,
    onClipChanged,
    jointPopover,
    piecePopover,
    hud
  }: {
    /** A bare koblem media path. Fetched as a blob with header auth — never a `?token=` URL. */
    src?: string;
    showSkeleton?: boolean;
    label?: string;
    /**
     * A CSS length, or a number of px. Defaults to a tall stage the parent can make sticky; the
     * floor is in the stylesheet, so a viewport-relative value cannot collapse the box to nothing.
     */
    height?: number | string;
    /**
     * What the canvas draws and what a click does. Defaults to `view`, or — when only the legacy
     * `jointEdit`/`shellPick` flags are passed — to whichever of those is on.
     *
     * 🔴 `mode` wins whenever it is passed. Those two flags used to be independent checkboxes and
     * the old pointer handler silently gave `jointEdit` priority when both were set.
     */
    mode?: StageMode;
    /**
     * Legacy show-the-handles flag, now the DRAG gesture switch: in `joints` mode a handle is
     * always selectable, and `jointEdit={false}` makes it selectable but not draggable.
     */
    jointEdit?: boolean;
    /** Legacy "clicking the model reports the triangle that was hit". Superseded by `mode`. */
    shellPick?: boolean;
    /** Sanitized `THREE.Bone.name` — the same string `onJointPicked` emits. */
    selectedJoint?: string;
    /** Only used to caption the joint-drag gesture; the caller is what actually applies it. */
    keepChildren?: boolean;
    /** Connected-component labels for the body. Enables piece hover and piece picking. */
    labels?: ComponentLabels | null;
    /** Component id drawn in the `select` tone while in `pieces` mode. -1 for none. */
    selectedComponent?: number;
    /**
     * Faces about to be destroyed, drawn in the `danger` tone — the "show me before I delete" path,
     * and it serves the bulk operations (prune dust, interior faces, degenerates) as much as one
     * picked piece. Drawn in EVERY mode, because what is about to be destroyed outranks whatever
     * else the canvas is doing.
     *
     * ⚠️ Compared by identity. Mutating a set in place will not redraw; pass a new one.
     */
    previewFaces?: ReadonlySet<number> | null;
    /** Parallel to `handle.bones`. A short array leaves the tail `unnamed`. */
    jointStates?: JointState[] | null;
    /**
     * The skin-weight leak, painted per vertex. ⚠️ `ratios` and `mask` are compared BY IDENTITY and
     * re-uploaded only when the reference changes: `threshold` is a live uniform, and dragging the
     * slider must not push 250k floats per event.
     */
    heat?: { ratios: Float32Array[]; threshold: number; mask: Uint8Array[] | null } | null;
    /**
     * GPU colour preview on the body's material; `null` detaches it.
     *
     * 🔴 The identity is `{ exposure: 0, contrast: 1, saturation: 1, temperature: 0 }` — contrast
     * and saturation are ONE at rest, not zero. Zeroes render a flat grey, correctly, because that
     * is what the commit would bake.
     */
    grade?: ColourAdjust | null;
    /**
     * The paint brush, in `paint` mode. `null` draws no cursor and reports no hover.
     *
     * `radius` is in WORLD units — the same space `WeightBrush.dab` takes, and the same space
     * `modelSpan()` measures — so the owner sizes it against the model rather than against the
     * screen. `profile` is carried only so the cursor and the owner's `brushField()` preview cannot
     * disagree about which falloff is on; this component does not evaluate it.
     *
     * ⚠️ The RING this draws claims only a centre and a size. What a dab actually reaches is
     * `brushField()` pushed through `heat` — see [`BrushCursor`]'s header for why there are
     * deliberately two halves and not one.
     */
    brush?: { radius: number; profile?: FalloffProfile } | null;
    /**
     * A second koblem media path — the DELIVERED model — drawn translucent and in place behind the
     * edited one, so "did that repair help" stops being a memory test.
     *
     * 🔴 A GHOST, NOT A MODEL. It is never picked, never enters `skinnedMeshes`, never reaches
     * `onReady` and so never reaches `analyseSkin` or the save path, and is never touched by the
     * teardown that disposes the real model's geometry. It owns its own GLB and frees it itself.
     */
    ghostSrc?: string;
    /**
     * How solid the ghost is. `0` hides it — which is the A/B flick, and is why it HIDES rather than
     * unloads: re-fetching and re-parsing a second hero tier on every toggle would make the
     * comparison unusable. Clear `ghostSrc` to actually free it.
     */
    ghostOpacity?: number;
    /**
     * The transport. Bindable, because scrubbing PAUSES and an owner driving a scrubber has to see
     * that happen — a transport that says "playing" over a held frame is lying about the model.
     */
    playing?: boolean;
    /**
     * Jump to this frame of the clip on screen and hold it. `null` requests nothing.
     *
     * ⚠️ EDGE-TRIGGERED, and deliberately not reset by a model load: assigning a NEW number scrubs,
     * re-assigning the same one does nothing. A frame request is a user gesture, and replaying the
     * last one every time a fresh GLB installs would pause a model nobody asked to pause.
     */
    scrubFrame?: number | null;
    onReady?: (handle: EditorStageHandle) => void;
    onError?: (message: string) => void;
    onJointPicked?: (name: string) => void;
    /** Incremental world-space delta while a joint is being dragged. Incremental rather than
     *  absolute so the caller can just keep applying them. */
    onJointDrag?: (name: string, worldDelta: THREE.Vector3) => void;
    /**
     * One arrow-key step on the selected joint, in the same incremental world-space form
     * `onJointDrag` uses — and falling back to `onJointDrag` when it is not supplied, so the
     * keyboard works on an owner that only wired the drag.
     *
     * Separate because the two are not the same edit to an undo stack: a drag is one gesture made
     * of a hundred deltas, a nudge is one delta.
     */
    onJointNudge?: (name: string, worldDelta: THREE.Vector3) => void;
    onFacePicked?: (faceIndex: number) => void;
    /** Component under the cursor, -1 for none. Fired only when the value CHANGES. */
    onHoverComponent?: (id: number) => void;
    /** A click (not an orbit drag) in `pieces` mode. `(-1, -1)` when it hit nothing. */
    onPickComponent?: (id: number, faceIndex: number) => void;
    /** Joint under the cursor, '' for none. Fired only when the value CHANGES. */
    onHoverJoint?: (name: string) => void;
    /**
     * `1`–`4` were pressed and this is the mode they asked for.
     *
     * 🔴 A REQUEST, not a change. The stage does not own `mode` — it is a prop — so the hotkey
     * emits and the owner decides, exactly as the mode rail's buttons do. A component that assigned
     * its own prop would flicker back on the next render and the rail would disagree with the
     * canvas. Already filtered by the rest-pose refusal, so a take never asks for `pieces`.
     */
    onModeRequested?: (mode: StageMode) => void;
    /**
     * A paint stroke started, i.e. the pointer went down on the model in `paint` mode.
     *
     * 🔴 Fired ONLY on a hit. A press on empty space is an orbit gesture and must stay one, or the
     * model becomes un-turnable in the mode that needs turning it most. An owner that opened a
     * `WeightBrush` stroke here is therefore guaranteed a matching `onBrushEnd`.
     */
    onBrushBegin?: () => void;
    /**
     * One dab, at a WORLD-space point on the rest surface, with that surface's WORLD-space normal.
     *
     * ⚠️ Throttled to one per ANIMATION FRAME, not one per `pointermove`. The move stream runs well
     * ahead of 60 Hz and a dab is real work (~2.8 ms at 250k verts), so a per-event brush spends
     * several milliseconds each frame recomputing the same visible result.
     *
     * `normal` is flipped to face the camera before it is handed over — see `raycastSurface` — so
     * it can go straight into `DabOptions.normal` with `frontFacing` on. Both vectors are fresh
     * objects per dab and are safe to keep.
     */
    onBrushDab?: (point: THREE.Vector3, normal: THREE.Vector3) => void;
    /** The stroke ended — pointer up, cancel, or the pointer leaving the viewport. */
    onBrushEnd?: () => void;
    /**
     * Where the brush is aimed while NO stroke is live. `null` when it is off the model.
     *
     * This is what drives a live `brushField()` preview through `heat`: the owner sees the falloff
     * on the surface before committing to it. Same once-per-frame throttle as `onBrushDab`.
     */
    onBrushHover?: (point: THREE.Vector3 | null) => void;
    /** The displayed frame changed. `frames` is the clip's keyframe count. Change-gated. */
    onFrame?: (frame: number, frames: number) => void;
    /** A different clip is now on the mixer, with its keyframe count. */
    onClipChanged?: (clip: string, frames: number) => void;
    /**
     * Rendered inside the viewport, in a wrapper THIS COMPONENT positions at the selected joint —
     * whenever `mode` is `joints` and a joint is selected. Do not position yourself absolutely
     * again; use `anchor` to decide which way to open and how far to fade, and set
     * `pointer-events: auto` on your own root, because the layer around you is `pointer-events:
     * none` so that it cannot swallow the orbit gesture.
     *
     * `name` is the sanitized `THREE.Bone.name`, matching `selectedJoint`. The canonical
     * colon-bearing name is `handle.bones[index].userData.name ?? name` — see `sanitizeName`.
     */
    jointPopover?: Snippet<[{ name: string; index: number; anchor: StageAnchor }]>;
    /** As `jointPopover`, at the selected component's bounding-box centre, in `pieces` mode. */
    piecePopover?: Snippet<[{ id: number; anchor: StageAnchor }]>;
    /** A free-floating layer over the canvas — mode rail, colour HUD, legends. */
    hud?: Snippet;
  } = $props();

  let host = $state<HTMLDivElement>();
  let canvasEl = $state<HTMLCanvasElement>();
  let loading = $state(false);
  let error = $state('');
  let clips = $state<string[]>([]);
  let selectedClip = $state('');
  let speed = $state(1);
  /** The frame on screen and the clip's keyframe count, for the scrubber in the bar. */
  let frameNow = $state(0);
  let frameTotal = $state(0);
  let stats = $state<{ bones: number; vertices: number; meshes: number } | null>(null);
  /**
   * Bumped whenever the scene is rebuilt or torn down.
   *
   * ⚠️ A DEPENDENCY, not a value. The overlays, the bones and the body are plain three.js objects
   * held in non-reactive `let`s — deliberately, because `$state` would wrap a 250k-vertex handle in
   * a deep proxy — so nothing else would ever tell an `$effect` to re-read them after a new model
   * installs. Same trick as `RigEditor`'s `renameTick`.
   */
  let modelVersion = $state(0);
  /** The anchored screen positions, republished from the render loop ONLY when they change. */
  let jointAnchor = $state.raw<StageAnchor | null>(null);
  let pieceAnchor = $state.raw<StageAnchor | null>(null);
  /**
   * The camera affordances, built in `install()` because every one of them closes over the camera,
   * the controls and the model's bounds. `null` until a model is on screen — which is also what the
   * in-canvas control gates on, so there are never buttons with nothing to aim at.
   *
   * ⚠️ `$state.raw`: it is a bag of closures, replaced wholesale per load and never mutated, and a
   * deep proxy over functions buys nothing.
   */
  let aim = $state.raw<{
    focusOn: (point: THREE.Vector3, radius?: number) => void;
    focusSelection: () => void;
    setView: (axis: ViewAxis) => void;
  } | null>(null);

  let dispose: (() => void) | null = null;
  let mixer: THREE.AnimationMixer | null = null;
  let action: THREE.AnimationAction | null = null;
  let allClips: THREE.AnimationClip[] = [];
  let snapshots: ClipSnapshot[] = [];
  let helper: THREE.SkeletonHelper | null = null;
  /** Bumped on every load so a slow fetch that lost the race cannot install itself over a newer one. */
  let generation = 0;
  /** The playing clip's keyframe count and duration, cached: `clipFrameCount` walks every track and
   *  the frame readout below runs in the render loop. */
  let clipFrames = 0;
  let clipDuration = 0;
  /** Last frame handed to `onFrame`, so a 60 Hz loop does not re-emit a 24 fps clip's frame. */
  let lastFrame = -1;
  /** Last `scrubFrame` acted on. See the prop's note on why this survives a model load. */
  let lastScrubRequest: number | null = null;

  // ── the drawn layer ──────────────────────────────────────────────────
  // All of these are created in `install()` and destroyed in its `dispose`, so an abandoned load
  // never creates one and a replaced model never leaks one.
  let sceneBones: THREE.Bone[] = [];
  /** The character. Overlays and picking are single-body: this pipeline delivers one skinned mesh,
   *  and `RigEditor` already refuses to tidy a model that has more than one. */
  let body: THREE.SkinnedMesh | null = null;
  let jointHandles: JointHandles | null = null;
  /** Selection and delete-preview. Separate from `hoverOverlay` so hovering B while A is selected
   *  shows both, which is most of the point of drawing the selection at all. */
  let markOverlay: SurfaceOverlay | null = null;
  let hoverOverlay: SurfaceOverlay | null = null;
  let heatOverlay: HeatOverlay | null = null;
  let hoverPicker: HoverPicker | null = null;
  let brushCursor: BrushCursor | null = null;
  let liveGrade: LiveGrade | null = null;

  // ── the A/B ghost ────────────────────────────────────────────────────
  // 🔴 Kept entirely OUTSIDE the model's own bookkeeping: not in `skinnedMeshes`, not under `root`,
  // not in anything `onReady` hands out. Everything it owns is freed by `disposeGhost` and by
  // nothing else. See the `ghostSrc` prop.
  /** The live scene, so the ghost can be added to it from outside `install`. */
  let sceneRoot: THREE.Scene | null = null;
  let ghostRoot: THREE.Group | null = null;
  /** The one material every ghost mesh wears; the GLB's own are disposed on install. */
  let ghostMaterial: THREE.MeshBasicMaterial | null = null;
  /** The path currently on screen, so re-running the effect does not re-fetch the same GLB. */
  let ghostPath = '';
  /** As `generation`, for the ghost's own fetch. See `loadGhost` for why BOTH are checked. */
  let ghostGeneration = 0;
  /** A DEPENDENCY, not a value — same trick as `modelVersion`, for the opacity effect. */
  let ghostVersion = $state(0);
  let ghostError = $state('');

  /** Last values pushed in either direction, so a 60 Hz loop does not re-emit or re-upload. */
  let lastRatios: Float32Array[] | null = null;
  let lastMask: Uint8Array[] | null = null;
  let lastHoverComponent = -1;
  let lastHoverJoint = '';
  /** Viewport size in CSS px, cached from `resize` — reading `clientWidth` per frame forces layout. */
  let viewW = 1;
  let viewH = 1;

  let dragging: { bone: THREE.Bone; plane: THREE.Plane; last: THREE.Vector3 } | null = null;
  /**
   * A paint stroke is live. Set on a `pointerdown` that HIT, cleared by up/cancel/leave.
   *
   * ⚠️ The same discipline the joint drag uses: `controls.enabled = false` for the duration, so
   * orbit and paint are not fighting over one pointer. It has to be re-enabled on every exit path,
   * which is why they all funnel through `endStroke`.
   */
  let painting = false;
  /** The captured pointer for the live stroke, so `endStroke` can release it from any path. */
  let strokePointer: number | null = null;
  /** Last brush hover reported, so `clearHover` only sends the `null` once. */
  let lastBrushHover = false;
  /** The live `OrbitControls`, so a stroke ended from outside `install` can hand orbit back. */
  let controlsRef: OrbitControls | null = null;
  const raycaster = new THREE.Raycaster();
  const pointer = new THREE.Vector2();
  /** Set by `pointermove`, consumed once per animation frame. Picking per EVENT is what makes hover
   *  expensive; picking per FRAME is what makes it affordable — and a dab is far dearer than a pick. */
  let hoverPending = false;
  const _anchor = new THREE.Vector3();
  const _direction = new THREE.Vector3();
  const _focus = new THREE.Vector3();
  const _heading = new THREE.Vector3();
  const _screenRight = new THREE.Vector3();
  const _screenUp = new THREE.Vector3();

  const cssHeight = $derived(typeof height === 'number' ? `${height}px` : height);
  /** 🔴 A GLB with baked tracks is an animated TAKE, not a rig, and its bones are mid-clip. */
  const posed = $derived(clips.length > 0);
  const requestedMode: StageMode = $derived(
    mode ?? (jointEdit ? 'joints' : shellPick ? 'pieces' : 'view')
  );
  const needsRestPose = $derived(
    requestedMode === 'pieces' || requestedMode === 'weights' || requestedMode === 'paint'
  );
  const activeMode: StageMode = $derived(posed && needsRestPose ? 'view' : requestedMode);
  /** 🔴 Refused on a take. Applying a drag re-binds the skeleton at the CURRENT pose, which freezes
   *  that pose into the bind and permanently deforms the character — the one unrecoverable mistake
   *  in this panel, and the same guard `RigEditor.svelte:139` states. */
  const canDragJoints = $derived(jointEdit !== false && !posed);
  const dragHint = $derived(keepChildren ? 'child joints stay put' : 'the chain follows');
  const modeNotice = $derived.by(() => {
    if (!posed) return '';
    if (needsRestPose) {
      return (
        'This model carries animation tracks, so its bones are mid-clip. The piece, weight and ' +
        'paint tools read the rest pose and are off here — open the rig tier to use them.'
      );
    }
    if (requestedMode === 'joints') {
      return (
        'This model carries animation tracks. Joints can be inspected but not moved: re-binding ' +
        'a posed skeleton would bake the pose into the rig.'
      );
    }
    return '';
  });
  const selectedJointIndex = $derived.by(() => {
    // `tick` is a DEPENDENCY, not a value: `sceneBones` holds three.js objects whose `name` a
    // rename mutates in place, so nothing else here would tell Svelte to re-read them.
    const tick = modelVersion;
    if (tick < 0 || !selectedJoint) return -1;
    return sceneBones.findIndex((b) => b.name === selectedJoint);
  });
  const selectedJointName = $derived(sceneBones[selectedJointIndex]?.name ?? '');

  $effect(() => {
    const path = src;
    if (!path) {
      teardown();
      return;
    }
    void load(path);
  });

  $effect(() => {
    const tick = modelVersion;
    if (tick >= 0 && helper) helper.visible = showSkeleton;
  });

  $effect(() => {
    if (!action) return;
    action.paused = !playing;
    action.timeScale = speed;
  });

  $effect(() => {
    const tick = modelVersion;
    const want = scrubFrame;
    // Edge-triggered — see the prop. `modelVersion` is read so a fresh model re-runs this, but the
    // equality below is what stops that re-run from replaying an old request at it.
    if (tick < 0 || want === null || want === undefined || want === lastScrubRequest) return;
    lastScrubRequest = want;
    scrubTo(want);
  });

  /**
   * The mode switch, enforced at the only place that can enforce it: the overlays' own visibility.
   *
   * 🔴 `JointHandles.pick()` checks `visible` itself, and everything here relies on that —
   * `Raycaster` tests `layers` but NOT `visible`, and the old code only got away with raycasting a
   * hidden InstancedMesh because it gated on the `jointEdit` checkbox first.
   */
  $effect(() => {
    const tick = modelVersion;
    const active = activeMode;
    if (tick < 0) return;
    if (jointHandles) jointHandles.visible = active === 'joints';
    // 🔴 The heat overlay is ON in `paint` as well, and that is not a convenience: it is the
    // brush's own footprint preview. `brushField()` returns per-mesh ratios shaped for
    // `setRatios`, the owner pushes them through `heat`, and this is the surface they land on.
    // Leaving it off in `paint` would silently blank the only thing that says what a dab reaches.
    if (heatOverlay) heatOverlay.visible = active === 'weights' || active === 'paint';
    if (active !== 'paint') endStroke();
    clearHover();
  });

  /**
   * The ring comes off the moment there is nothing to aim.
   *
   * ⚠️ Separate from the mode effect above, which calls `clearHover()` and therefore FIRES
   * callbacks. This one reads `brush`, whose identity changes on every radius tick, so it must stay
   * silent — an owner that rebuilt `brush` from a slider would otherwise get an `onBrushHover(null)`
   * per pixel of drag and lose the live `brushField()` preview it was dragging to adjust.
   */
  $effect(() => {
    const tick = modelVersion;
    const aiming = activeMode === 'paint' && brush !== null;
    if (tick < 0 || aiming) return;
    brushCursor?.hide();
  });

  /**
   * The ghost's own load, keyed on the path and on the SCENE being rebuilt.
   *
   * ⚠️ `ghostOpacity` is deliberately not read here. It is on a slider, and an effect that
   * re-fetched a second hero tier per `input` event would be a network request per pixel of drag.
   */
  $effect(() => {
    const tick = modelVersion;
    const path = ghostSrc;
    if (tick < 0) return;
    if (!path) {
      disposeGhost();
      return;
    }
    // `teardown` clears `ghostRoot`, so a fresh model re-runs this and reinstalls into the new
    // scene rather than leaving a ghost pointing at a scene that no longer exists.
    if (path === ghostPath && ghostRoot) return;
    void loadGhost(path);
  });

  $effect(() => {
    const tick = ghostVersion;
    const opacity = ghostOpacity;
    if (tick < 0 || !ghostMaterial || !ghostRoot) return;
    ghostMaterial.opacity = clamp(opacity, 0, 1);
    // Hidden, not unloaded — see the prop. The whole value of the comparison is that it toggles
    // instantly.
    ghostRoot.visible = opacity > 0;
  });

  $effect(() => {
    const tick = modelVersion;
    const states = jointStates;
    if (tick >= 0) jointHandles?.setStates(states ?? []);
  });

  $effect(() => {
    jointHandles?.setSelected(selectedJointIndex);
  });

  // What is about to be destroyed outranks the selection and is shown in every mode; the plain
  // selection belongs to `pieces` and would be noise anywhere else.
  $effect(() => {
    const tick = modelVersion;
    const preview = previewFaces;
    const id = selectedComponent;
    const map = labels;
    const active = activeMode;
    if (tick < 0 || !markOverlay) return;
    if (preview && preview.size > 0) markOverlay.showFaces(preview, 'danger');
    else if (map && id >= 0 && active === 'pieces') markOverlay.showComponent(map, id, 'select');
    else markOverlay.hide();
    // Clicking the piece under the cursor does not move the cursor, so the hover highlight would
    // still be drawing the same triangles — and it is added to the scene last, so its warm tone
    // wins over the selection blue at the same `renderOrder`. The selection has to be what you see.
    if (lastHoverComponent >= 0 && lastHoverComponent === id) hoverOverlay?.hide();
  });

  $effect(() => {
    const tick = modelVersion;
    const data = heat;
    if (tick < 0 || !heatOverlay || !data) return;
    // ⚠️ Identity checks, not deep ones. `setRatios` resyncs every proxy and refills a Float32Array
    // per mesh; running it on each `input` event of the threshold slider would make the control
    // stutter and teach the user that the tool is slow rather than that the rig is fine.
    if (data.ratios !== lastRatios) {
      lastRatios = data.ratios;
      heatOverlay.setRatios(data.ratios);
    }
    if (data.mask !== lastMask) {
      lastMask = data.mask;
      heatOverlay.setMask(data.mask);
    }
    heatOverlay.setThreshold(data.threshold);
  });

  /**
   * The live colour preview.
   *
   * ⚠️ No cleanup function, on purpose. This effect re-runs on every slider frame, and a
   * detach/re-attach pair per frame is `material.needsUpdate = true` per frame — a shader recompile
   * in the middle of a drag. The grade comes off when `grade` goes null, when `detachGrade()` is
   * called, or on teardown.
   */
  $effect(() => {
    const tick = modelVersion;
    const adjust = grade;
    if (tick < 0) return;
    if (!adjust) {
      detachGrade();
      return;
    }
    let live = liveGrade;
    if (!live) {
      // ⚠️ `Mesh.material` is `Material | Material[]`. A multi-material body would need a slot
      // picker and a per-slot patch on the save side, so the preview declines rather than grading
      // an arbitrary slot — the same call `RigEditor` makes.
      const material = body?.material;
      if (!material || Array.isArray(material)) return;
      live = attachLiveGrade(material as THREE.MeshStandardMaterial);
      liveGrade = live;
    }
    live.set(adjust);
  });

  function detachGrade() {
    liveGrade?.detach();
    liveGrade = null;
  }

  function clearHover() {
    hoverOverlay?.hide();
    jointHandles?.setHovered(-1);
    if (lastHoverComponent !== -1) {
      lastHoverComponent = -1;
      onHoverComponent?.(-1);
    }
    if (lastHoverJoint !== '') {
      lastHoverJoint = '';
      onHoverJoint?.('');
    }
    if (lastBrushHover) {
      lastBrushHover = false;
      onBrushHover?.(null);
    }
    brushCursor?.hide();
  }

  /**
   * End a paint stroke, from wherever it ended.
   *
   * 🔴 Every exit funnels through here because two things MUST come back: orbit, and the pointer
   * capture. Miss either on one path — a pointercancel from a palm rejection, a mode switch
   * mid-drag — and the viewport is left un-orbitable with no error and nothing on screen to explain
   * it. Idempotent, so calling it on a path where no stroke was live is free.
   */
  function endStroke() {
    if (!painting) return;
    painting = false;
    const id = strokePointer;
    strokePointer = null;
    if (controlsRef) controlsRef.enabled = true;
    // `renderer.domElement` IS this canvas — it is passed in to the renderer — so this is the same
    // element the capture was taken on.
    if (id !== null && canvasEl?.hasPointerCapture(id)) canvasEl.releasePointerCapture(id);
    brushCursor?.setTone('hover');
    onBrushEnd?.();
  }

  function teardown() {
    // 🔴 Before `dispose()`, which destroys the scene the ghost is parented to. A ghost left behind
    // would hold GPU buffers with nothing on screen to free them, and the effect above would see a
    // live `ghostRoot` for a scene that no longer exists and decline to reload it.
    disposeGhost();
    dispose?.();
    dispose = null;
    mixer = null;
    action = null;
    helper = null;
    allClips = [];
    snapshots = [];
    clips = [];
    stats = null;
    sceneBones = [];
    body = null;
    jointHandles = null;
    markOverlay = null;
    hoverOverlay = null;
    heatOverlay = null;
    hoverPicker = null;
    brushCursor = null;
    sceneRoot = null;
    controlsRef = null;
    painting = false;
    strokePointer = null;
    lastBrushHover = false;
    lastRatios = null;
    lastMask = null;
    lastHoverComponent = -1;
    lastHoverJoint = '';
    jointAnchor = null;
    pieceAnchor = null;
    aim = null;
    clipFrames = 0;
    clipDuration = 0;
    lastFrame = -1;
    frameNow = 0;
    frameTotal = 0;
    modelVersion++;
  }

  async function load(path: string) {
    const mine = ++generation;
    loading = true;
    error = '';
    try {
      const blob = await fetchMediaBlob(path);
      const bytes = await blob.arrayBuffer();
      if (mine !== generation) return;
      // 🔴 `parseAsync` does not consume `bytes`, and the editor needs them: saving PATCHES these
      // original bytes rather than re-exporting the scene, which is what keeps the `mixamorig:`
      // colons and the texture bytes intact. See `glb-patch.ts`.
      const gltf = await new GLTFLoader().parseAsync(bytes, '');
      if (mine !== generation) {
        // A newer load won while this one was parsing. Drop it rather than installing a model the
        // user has already navigated away from. Nothing leaks: every overlay, listener and loop is
        // created inside `install`, past this point.
        return;
      }
      teardown();
      install(gltf, bytes);
    } catch (e) {
      if (mine !== generation) return;
      const message = e instanceof Error ? e.message : String(e);
      error = message;
      onError?.(message);
    } finally {
      if (mine === generation) loading = false;
    }
  }

  /** A flat, cool tone. The ghost is a SILHOUETTE reference, not a second character to look at. */
  const GHOST_TONE = 0x7f9fd0;

  /**
   * Fetch and install the A/B ghost.
   *
   * 🔴 TWO generation checks, not one. `ghostGeneration` catches a second ghost request overtaking
   * this one, exactly as `generation` does for the model. But `generation` is checked as well
   * because the MODEL can change while a ghost is in flight — `teardown` then disposed the scene
   * this was going to be added to, and installing into it would attach a ghost to a dead scene and
   * leak every buffer it holds.
   */
  async function loadGhost(path: string) {
    const mineGhost = ++ghostGeneration;
    const mineModel = generation;
    ghostError = '';
    try {
      const blob = await fetchMediaBlob(path);
      const bytes = await blob.arrayBuffer();
      if (mineGhost !== ghostGeneration || mineModel !== generation) return;
      const gltf = await new GLTFLoader().parseAsync(bytes, '');
      if (mineGhost !== ghostGeneration || mineModel !== generation || !sceneRoot) return;
      disposeGhost();
      installGhost(gltf.scene, path);
    } catch (e) {
      if (mineGhost !== ghostGeneration) return;
      // Reported in the bar rather than thrown at `onError`: the ghost is a comparison aid, and a
      // failed one must not read as "the model failed to load".
      ghostError = e instanceof Error ? e.message : String(e);
    }
  }

  /**
   * Dress the delivered GLB as a ghost and hang it off the SCENE.
   *
   * 🔴 Never `root.add`. `install`'s teardown runs `root.traverse(o => o.geometry?.dispose())` and
   * the save path finds the character by traversing `root` for `isSkinnedMesh` — a ghost under
   * there would be freed by the first and offered to `patchGlb` as part of the character by the
   * second. Same rule, and the same reasoning, as every overlay in `overlays.ts`.
   */
  function installGhost(root: THREE.Group, path: string) {
    const material = new THREE.MeshBasicMaterial({
      color: GHOST_TONE,
      transparent: true,
      opacity: clamp(ghostOpacity, 0, 1),
      // 🔴 Depth test ON and depth write OFF, which is the whole comparison. Transparent objects
      // draw after the opaque body, so the ghost is HIDDEN wherever the edited model is in front of
      // it and VISIBLE exactly where the delivered geometry sticks out past the repair. Turning the
      // test off — as every diagnostic overlay here does — would paint the ghost over the character
      // and answer nothing.
      depthWrite: false,
      // ~200 independently generated shells have no trustworthy winding; a ghost that vanishes on
      // the inverted ones is a ghost that fails on precisely the junk being repaired.
      side: THREE.DoubleSide
    });

    root.traverse((o) => {
      const mesh = o as THREE.Mesh;
      if (!mesh.isMesh) return;
      // The delivered baseColor is up to 2048² and is never looked at here — the ghost is one flat
      // tone. Freeing them now keeps a second hero tier's textures out of memory entirely.
      const mats = Array.isArray(mesh.material) ? mesh.material : [mesh.material];
      mats.forEach((m) => {
        const standard = m as THREE.MeshStandardMaterial | undefined;
        standard?.map?.dispose();
        standard?.normalMap?.dispose();
        standard?.dispose();
      });
      mesh.material = material;
      mesh.frustumCulled = false;
      // 🔴 Unpickable, belt and braces. Nothing in this component raycasts the scene graph today —
      // handles are picked by object and the body through its own BVH — but a future hover that
      // did would otherwise report faces on a model the user cannot edit.
      mesh.raycast = () => {};
    });

    root.visible = ghostOpacity > 0;
    sceneRoot?.add(root);
    ghostRoot = root;
    ghostMaterial = material;
    ghostPath = path;
    ghostVersion++;
  }

  /**
   * Free everything the ghost owns — and nothing else.
   *
   * ⚠️ A plain traverse-and-dispose is safe HERE, where it is not safe for the overlay proxies: the
   * ghost is a separately parsed GLB and shares not one `BufferAttribute` with the character. The
   * single shared material is disposed once, after the traversal, rather than per mesh.
   */
  function disposeGhost() {
    // 🔴 Bumped FIRST and UNCONDITIONALLY. A fetch can be in flight with nothing yet installed —
    // `ghostSrc` cleared mid-download is exactly that — and a guard that returned early on "nothing
    // to free" would let it install a ghost the user has already turned off.
    ghostGeneration++;
    const root = ghostRoot;
    // Past that, there is genuinely nothing to do; bail before touching `ghostVersion` so the model
    // effect above does not re-run the opacity effect on every load for no reason.
    if (!root && !ghostMaterial && !ghostPath && !ghostError) return;
    ghostRoot = null;
    ghostPath = '';
    if (root) {
      root.removeFromParent();
      root.traverse((o) => (o as THREE.Mesh).geometry?.dispose());
    }
    ghostMaterial?.dispose();
    ghostMaterial = null;
    ghostError = '';
    ghostVersion++;
  }

  function install(
    gltf: {
      scene: THREE.Group;
      animations: THREE.AnimationClip[];
      parser?: { associations?: Map<object, { meshes?: number; primitives?: number }> };
    },
    source: ArrayBuffer
  ) {
    if (!host || !canvasEl) return;
    const hostEl = host;
    const scene = new THREE.Scene();
    scene.background = new THREE.Color(0x15171d);
    // Published so the ghost — which is loaded outside this function and outlives no scene — has
    // somewhere to attach. Cleared in `teardown`.
    sceneRoot = scene;

    const camera = new THREE.PerspectiveCamera(38, 1, 0.01, 1000);
    const renderer = new THREE.WebGLRenderer({ antialias: true, canvas: canvasEl });
    renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
    // Neutral, even light on purpose: the texture is raw baseColor and the point of looking at it
    // is comparing it to the source image, not flattering it.
    scene.add(new THREE.HemisphereLight(0xffffff, 0x273248, 2.4));
    const key = new THREE.DirectionalLight(0xffffff, 2.2);
    key.position.set(3, 5, 4);
    scene.add(key);
    const fill = new THREE.DirectionalLight(0xffffff, 0.9);
    fill.position.set(-4, 2, -3);
    scene.add(fill);

    const root = gltf.scene;
    scene.add(root);

    // 🔴 THE MODEL IS NEVER MOVED — THE CAMERA IS AIMED AT IT. Two separate things break if this
    // scene translates the model to frame it, and the second is the one that would have been hard
    // to diagnose:
    //
    //   1. `root` is the exact object the rig editor re-exports, so a framing offset would be baked
    //      into every saved repair — a character that no longer stands at its own origin.
    //   2. Joint adjustment re-binds the skeleton with `Skeleton.calculateInverses()`, which reads
    //      `bone.matrixWorld`. Under a translated parent those world matrices carry the offset, the
    //      fresh inverses cancel it, and the mesh comes back displaced by the inverse of the
    //      framing translation. Leaving the model at its delivered transform makes re-binding exact.
    //
    // ⚠️ So `frameCamera` below moves the CAMERA, including on every re-frame after a resize.
    const box = new THREE.Box3().setFromObject(root);
    const size = box.getSize(new THREE.Vector3());
    const span = size.length() || 1;
    const centre = box.getCenter(new THREE.Vector3());
    /** One arrow-key step. The largest single dimension, not the diagonal, so it matches the step
     *  the inspector's nudge buttons use and the two controls move a joint by the same amount. */
    const nudgeStep = Math.max(size.x, size.y, size.z) * 0.005;

    const controls = new OrbitControls(camera, renderer.domElement);
    controlsRef = controls;
    controls.enableDamping = true;
    camera.position.copy(centre).add(new THREE.Vector3(span * 0.55, span * 0.12, span * 0.95));
    controls.target.copy(centre);
    controls.update();

    helper = new THREE.SkeletonHelper(root);
    helper.material = new THREE.LineBasicMaterial({
      color: 0x58c7ff,
      depthTest: false,
      transparent: true
    });
    helper.visible = showSkeleton;
    helper.renderOrder = 999;
    scene.add(helper);

    const skinnedMeshes: THREE.SkinnedMesh[] = [];
    let vertices = 0;
    root.traverse((o) => {
      const mesh = o as THREE.SkinnedMesh;
      if (mesh.isSkinnedMesh) {
        skinnedMeshes.push(mesh);
        vertices += mesh.geometry.attributes.position?.count ?? 0;
      }
    });
    // 🔴 `skeleton.bones`, NOT a scene traversal. This array IS the correspondence the save path
    // relies on: `GLTFLoader.loadSkin` builds it by walking `skinDef.joints` in order, so
    // `bones[i]` is `skin.listJoints()[i]` in the file. A depth-first walk of the node tree only
    // COINCIDENTALLY matches that order, and additionally picks up bones that are not skin joints
    // at all — `glb-patch`'s length check would then pass while every joint translation and every
    // inverse-bind matrix went to the wrong joint. A traversal here was the original code and it
    // was wrong.
    const bones: THREE.Bone[] = skinnedMeshes[0]?.skeleton?.bones ?? [];
    stats = { bones: bones.length, vertices, meshes: skinnedMeshes.length };

    allClips = gltf.animations ?? [];
    clips = allClips.map((c) => c.name);
    // 🔴 BEFORE any `clipAction`. `snapshotClip` de-aliases tracks that share one `Float32Array`,
    // and an action's interpolant captures that array by reference the moment it is built — so
    // snapshotting afterwards would leave the aliasing in place and hand out a stale pristine copy.
    snapshots = allClips.map(snapshotClip);
    if (allClips.length) {
      mixer = new THREE.AnimationMixer(root);
      // Prefer the exercise track when nothing is chosen: it is the one that proves the skeleton
      // moves the mesh, which is the first question about any rig.
      const exercise = clips.find((c) => c.toLowerCase().includes('exercise'));
      selectedClip = clips.includes(selectedClip) ? selectedClip : (exercise ?? clips[0]);
      playClip(selectedClip);
    } else {
      selectedClip = '';
      snapshots = [];
    }

    // ── the drawn layer ─────────────────────────────────────────────────
    // 🔴 Every one of these is added to `scene`, never to `root`. See the file header.
    sceneBones = bones;
    body = skinnedMeshes[0] ?? null;
    // `span` is the bounding-box DIAGONAL, which is what the handles were always sized against.
    // `modelSpan()` is the largest single dimension and would shrink every grab target by ~40%.
    jointHandles = new JointHandles(bones, span, scene);
    if (body) {
      markOverlay = new SurfaceOverlay(body, scene);
      hoverOverlay = new SurfaceOverlay(body, scene);
      hoverPicker = new HoverPicker(body);
    }
    heatOverlay = new HeatOverlay(skinnedMeshes, scene);
    brushCursor = new BrushCursor(scene);

    const setPointer = (e: PointerEvent) => {
      const rect = renderer.domElement.getBoundingClientRect();
      pointer.set(
        ((e.clientX - rect.left) / rect.width) * 2 - 1,
        -((e.clientY - rect.top) / rect.height) * 2 + 1
      );
    };

    /** Where the gesture started, so an orbit drag is not mistaken for a click. */
    let downAt: { x: number; y: number; t: number } | null = null;

    /**
     * One dab, or one hover report, from wherever the pointer currently is.
     *
     * 🔴 Called ONCE PER ANIMATION FRAME, out of the render loop — never from `pointermove`. A dab
     * is ~2.8 ms at 250k vertices and the move stream runs well ahead of 60 Hz, so a per-event
     * brush burns several milliseconds a frame producing one visible result. Exactly the reason the
     * hover pick is throttled the same way, only more so.
     */
    const runBrush = () => {
      if (!body || !brush) return;
      raycaster.setFromCamera(pointer, camera);
      const hit = raycastSurface(body, raycaster);
      if (!hit) {
        // Nothing under the cursor. A live stroke keeps its capture — dragging off the silhouette
        // and back on is one stroke, not two — but there is nothing to paint out here.
        brushCursor?.hide();
        if (!painting && lastBrushHover) {
          lastBrushHover = false;
          onBrushHover?.(null);
        }
        return;
      }
      brushCursor?.set(hit.point, brush.radius);
      if (painting) {
        onBrushDab?.(hit.point, hit.normal);
        return;
      }
      lastBrushHover = true;
      onBrushHover?.(hit.point);
    };

    const onPointerDown = (e: PointerEvent) => {
      downAt = { x: e.clientX, y: e.clientY, t: performance.now() };
      if (activeMode === 'paint') {
        if (!body || !brush || e.button !== 0) return;
        setPointer(e);
        raycaster.setFromCamera(pointer, camera);
        const hit = raycastSurface(body, raycaster);
        // 🔴 A miss falls through to OrbitControls, exactly as a missed joint handle does. Painting
        // is the mode you orbit in most — you are chasing a seam round a limb — so a press on empty
        // space has to stay a turn.
        if (!hit) return;
        painting = true;
        strokePointer = e.pointerId;
        // The same discipline the joint drag uses: orbit and paint cannot share one pointer.
        controls.enabled = false;
        renderer.domElement.setPointerCapture(e.pointerId);
        brushCursor?.setTone('select');
        brushCursor?.set(hit.point, brush.radius);
        onBrushBegin?.();
        // The first dab lands NOW rather than on the next move, so a single click paints. A brush
        // that needs a wiggle to do anything reads as a broken tool.
        onBrushDab?.(hit.point, hit.normal);
        return;
      }
      if (activeMode !== 'joints') return;
      setPointer(e);
      raycaster.setFromCamera(pointer, camera);
      const index = jointHandles?.pick(raycaster) ?? -1;
      // A miss must fall through to OrbitControls, or the model is un-orbitable in this mode.
      if (index < 0) return;
      const bone = bones[index];
      onJointPicked?.(bone.name);
      if (!canDragJoints) return;
      // Drag on the plane facing the camera through the joint: the joint follows the cursor, which
      // is the whole interaction. No gizmo to learn, no axis to choose.
      //
      // 🔴 And specifically NOT `TransformControls`, which was evaluated: two of its three modes
      // have nowhere to go. `moveJointWorld` takes a world TRANSLATION delta and `glb-patch` saves
      // a joint by `joints[i].setTranslation(...)` plus the matching inverse-bind matrix — there is
      // no path through the save pipeline for a joint rotation or scale. A rotate gizmo would
      // therefore pose the character on screen and write nothing, which is the exact failure
      // `stage.ts`'s header warns about, dressed up as a working control. If axis-constrained
      // translation is wanted later, attach it to a DUMMY object and forward its `objectChange`
      // delta through `onJointDrag` — never to the `THREE.Bone`.
      const normal = camera.getWorldDirection(new THREE.Vector3()).negate();
      const at = bone.getWorldPosition(new THREE.Vector3());
      const plane = new THREE.Plane().setFromNormalAndCoplanarPoint(normal, at);
      const start = new THREE.Vector3();
      if (!raycaster.ray.intersectPlane(plane, start)) return;
      dragging = { bone, plane, last: start };
      // Orbiting while dragging a joint would fight the pointer for the same gesture.
      controls.enabled = false;
      renderer.domElement.setPointerCapture(e.pointerId);
    };

    const onPointerMove = (e: PointerEvent) => {
      if (!dragging) {
        // Nothing hovers in `view` or `weights`, and `setPointer` reads `getBoundingClientRect` —
        // a forced layout, on every move of an orbit gesture, for an answer nobody wants.
        if (activeMode !== 'joints' && activeMode !== 'pieces' && activeMode !== 'paint') return;
        setPointer(e);
        // ⚠️ A flag only. The pick happens once per animation frame in the loop below, because the
        // pointermove stream is faster than the frame rate and picking per event does the same
        // work several times over for one visible result.
        hoverPending = true;
        return;
      }
      setPointer(e);
      raycaster.setFromCamera(pointer, camera);
      const now = new THREE.Vector3();
      if (!raycaster.ray.intersectPlane(dragging.plane, now)) return;
      const delta = now.clone().sub(dragging.last);
      dragging.last.copy(now);
      onJointDrag?.(dragging.bone.name, delta);
    };

    const endDrag = (e: PointerEvent) => {
      if (!dragging) return;
      dragging = null;
      controls.enabled = true;
      if (renderer.domElement.hasPointerCapture(e.pointerId)) {
        renderer.domElement.releasePointerCapture(e.pointerId);
      }
    };

    const onPointerUp = (e: PointerEvent) => {
      const start = downAt;
      downAt = null;
      const wasDragging = dragging !== null;
      endDrag(e);
      endStroke();
      // 🔴 Picking on pointerDOWN — which is what the old shell pick did — fires on the first frame
      // of every orbit gesture, so simply grabbing the model to turn it selected a piece. Now that
      // a selection opens a popover over the model, that is not a nuisance, it is unusable.
      if (wasDragging || !start || activeMode !== 'pieces') return;
      if (Math.hypot(e.clientX - start.x, e.clientY - start.y) > 5) return;
      if (performance.now() - start.t > 500) return;
      setPointer(e);
      raycaster.setFromCamera(pointer, camera);
      const face = hoverPicker?.pick(raycaster) ?? -1;
      if (face < 0) {
        // A click on empty space is a deselect, which is how a popover gets closed by eye.
        onPickComponent?.(-1, -1);
        return;
      }
      onFacePicked?.(face);
      onPickComponent?.(labels ? componentAtFace(labels, face) : -1, face);
    };

    const onPointerCancel = (e: PointerEvent) => {
      downAt = null;
      endDrag(e);
      endStroke();
    };

    const onPointerLeave = () => {
      hoverPending = false;
      // ⚠️ Rarely reached mid-stroke: a captured pointer suppresses boundary events until the
      // capture is released, which is deliberate — dragging off the silhouette and back on is one
      // stroke. It is here for the paths where capture was never taken or was lost.
      endStroke();
      clearHover();
    };

    renderer.domElement.addEventListener('pointerdown', onPointerDown);
    renderer.domElement.addEventListener('pointermove', onPointerMove);
    renderer.domElement.addEventListener('pointerup', onPointerUp);
    renderer.domElement.addEventListener('pointercancel', onPointerCancel);
    renderer.domElement.addEventListener('pointerleave', onPointerLeave);

    /** The distance the model was last framed at, so `resize` can tell "the box changed shape" from
     *  "the user zoomed in". Re-framing over someone's own dolly is help nobody asks for twice. */
    let framedDistance = 0;

    /**
     * Fit a box to BOTH axes.
     *
     * ⚠️ `PerspectiveCamera(38, …)` takes a VERTICAL fov and the old code only ever recomputed
     * `camera.aspect`, so widening the box added empty space either side and nothing else. This
     * page is now full-width and wide is the common case. The horizontal half-angle is
     * `atan(tan(fovY/2) · aspect)`; whichever axis needs more distance wins.
     *
     * `halfWidth` is added at the end because the near face of the box is closer to the camera than
     * its centre.
     */
    const fitDistance = (halfWidth: number, halfHeight: number) => {
      const fovY = THREE.MathUtils.degToRad(camera.fov);
      const fovX = 2 * Math.atan(Math.tan(fovY / 2) * camera.aspect);
      return (
        Math.max(halfHeight / Math.tan(fovY / 2), halfWidth / Math.tan(fovX / 2)) * 1.08 + halfWidth
      );
    };

    /** The whole character. ⚠️ The on-screen horizontal extent depends on yaw, so `max(x, z)` is
     *  used rather than `x` — that keeps it framed all the way round the orbit instead of only from
     *  the front, which matters more here than anywhere because there is no canonical front. */
    const modelFitDistance = () => fitDistance(Math.max(size.x, size.z) / 2, size.y / 2);

    /** Re-fit the model along the heading the camera already has. */
    const frameCamera = () => {
      const distance = modelFitDistance();
      _direction.copy(camera.position).sub(controls.target);
      if (_direction.lengthSq() < 1e-12) _direction.set(0.55, 0.12, 0.95);
      camera.position.copy(controls.target).add(_direction.normalize().multiplyScalar(distance));
      framedDistance = distance;
      controls.update();
    };

    // ── camera affordances ──────────────────────────────────────────────
    //
    // 🔴 EVERY ONE OF THESE MOVES THE CAMERA AND `controls.target`, NEVER `root`. The long note
    // above says why: a framing translation would be baked into every saved repair, and
    // `Skeleton.calculateInverses()` reads `bone.matrixWorld`, so a re-bind under a shifted parent
    // brings the mesh back displaced by the inverse of the framing offset.

    let flight: {
      fromPos: THREE.Vector3;
      toPos: THREE.Vector3;
      fromTarget: THREE.Vector3;
      toTarget: THREE.Vector3;
      elapsed: number;
      duration: number;
    } | null = null;

    // A flight still running while the user orbits fights them for the same camera. `start` covers
    // the drag, the wheel and the touch gestures in one.
    controls.addEventListener('start', () => {
      flight = null;
    });

    /**
     * Aim the camera at `target` from `distance` away, over ~320 ms.
     *
     * ⚠️ Animated rather than cut. A teleport to a joint buried in the ribcage leaves no way to
     * tell which way the model turned to get there, and the whole reason to focus is to keep your
     * place. `smoothstep` starts and ends at rest, so it reads as travel rather than as two frames
     * swapped.
     *
     * `direction` is where the camera ends up RELATIVE to the target; omit it to keep the heading
     * the user has already orbited to, which is what focusing on something should preserve.
     */
    const flyTo = (target: THREE.Vector3, distance: number, direction?: THREE.Vector3) => {
      if (direction) _heading.copy(direction);
      else _heading.copy(camera.position).sub(controls.target);
      // A camera sitting exactly on its own target has no heading to keep — fall back to the same
      // three-quarter view `install` opens on.
      if (_heading.lengthSq() < 1e-12) _heading.set(0.55, 0.12, 0.95);
      _heading.normalize();
      flight = {
        fromPos: camera.position.clone(),
        fromTarget: controls.target.clone(),
        toTarget: target.clone(),
        toPos: target.clone().addScaledVector(_heading, distance),
        elapsed: 0,
        duration: 0.32
      };
    };

    const stepFlight = (dt: number) => {
      const move = flight;
      if (!move) return;
      move.elapsed += dt;
      const t = Math.min(move.elapsed / move.duration, 1);
      const eased = t * t * (3 - 2 * t);
      camera.position.lerpVectors(move.fromPos, move.toPos, eased);
      controls.target.lerpVectors(move.fromTarget, move.toTarget, eased);
      // `controls.update()` runs straight after this in the loop and re-derives its spherical from
      // exactly these two vectors, so it follows the flight rather than undoing it.
      if (t >= 1) flight = null;
    };

    /**
     * Get close enough to read something of this size without losing the body around it.
     *
     * ⚠️ `framedDistance` is deliberately left alone. The resize path stops re-framing once the
     * live distance stops matching it — that is how "the user has dollied, leave their camera
     * alone" is detected — and a focused camera is exactly that case. Updating it here would make
     * the next resize yank the view back off the joint you just flew to.
     *
     * The clamp is what keeps a focus on a knuckle from putting the near plane inside the mesh, and
     * a focus on a whole torso from being indistinguishable from no focus at all.
     */
    const focusOn = (point: THREE.Vector3, radius?: number) => {
      const reach = radius === undefined || radius <= 0 ? span * 0.12 : radius;
      flyTo(_focus.copy(point), clamp(fitDistance(reach, reach), span * 0.06, span * 3));
    };

    const focusSelection = () => {
      if (activeMode === 'joints') {
        const bone = sceneBones[selectedJointIndex];
        if (!bone) return;
        // Translation out of the world matrix, as `JointHandles.place` and `publishAnchors` do.
        const m = bone.matrixWorld.elements;
        focusOn(_focus.set(m[12], m[13], m[14]));
        return;
      }
      const piece = activeMode === 'pieces' && selectedComponent >= 0 ? selectedComponent : -1;
      const component = piece >= 0 ? labels?.components[piece] : undefined;
      if (component && body) {
        // 🔴 `restToWorld`, not `matrixWorld`. A component centre is a `position` attribute value
        // and lands in the wrong place through the node transform — same rule, and the same silent
        // failure, as `publishAnchors`. The diagonal is measured in that space too, so it takes the
        // matrix's scale with it.
        const toWorld = restToWorld(body);
        _focus
          .set(component.center[0], component.center[1], component.center[2])
          .applyMatrix4(toWorld);
        focusOn(_focus, (component.diagonal / 2) * toWorld.getMaxScaleOnAxis());
        return;
      }
      // Nothing is picked, so "focus" can only mean the model — which is the useful answer after
      // an orbit has wandered off it.
      const distance = modelFitDistance();
      framedDistance = distance;
      flyTo(centre, distance);
    };

    /**
     * ⚠️ NOT `three/addons/helpers/ViewHelper.js`, which is the same widget and was evaluated. Four
     * things ruled it out here, and the first is the one that would have cost a day:
     *
     *   1. It animates by writing `camera.position` AND `camera.quaternion` directly and never
     *      touches `controls.target`. `controls.update()` runs unconditionally in the loop below and
     *      recomputes both FROM the target — so the helper's animation is overwritten on the frame
     *      it is written, and adopting it means gating `controls.update()` on `helper.animating` and
     *      re-syncing the target afterwards. That is more moving parts than the lerp above, for the
     *      same picture.
     *   2. It needs a SECOND `renderer.render()` per frame behind a `setViewport` swap.
     *   3. It keeps the current dolly rather than re-fitting, so its presets do not frame anything.
     *   4. It can only sit in a corner, and the workbench's HUD already owns three corners and the
     *      bottom strip — and being a 3D widget it has nowhere to put the "axes, not anatomy" hint
     *      that these DOM buttons carry in their `title`.
     *
     * A preset re-fits the WHOLE model, so it is a re-frame and `framedDistance` follows it: a
     * resize after one should keep the model framed, not decide the user has dollied.
     */
    const setView = (axis: ViewAxis) => {
      const distance = modelFitDistance();
      framedDistance = distance;
      flyTo(centre, distance, new THREE.Vector3().fromArray(AXIS_HEADING[axis]));
    };

    aim = { focusOn, focusSelection, setView };

    // ── keyboard ────────────────────────────────────────────────────────

    /**
     * 🔴 Emitted, never applied. `moveJointWorld` re-binds the skeleton and belongs to the owner —
     * the same contract `onJointDrag` states, and the same reason the drag gesture reports a delta
     * instead of moving a bone.
     *
     * ⚠️ The step is taken in the CAMERA's screen plane, not along a world axis, so that "right"
     * means right on screen wherever the model has been orbited to — matching the drag gesture,
     * which already drags on the plane facing the camera. The view-depth axis is unreachable from
     * the keyboard for exactly the reason it is unreachable from a drag: orbit a quarter turn and
     * it becomes the horizontal one.
     */
    const nudgeSelectedJoint = (right: number, up: number, fast: boolean) => {
      if (activeMode !== 'joints' || !canDragJoints) return;
      const bone = sceneBones[selectedJointIndex];
      if (!bone) return;
      const step = nudgeStep * (fast ? 5 : 1);
      const delta = new THREE.Vector3();
      delta.addScaledVector(_screenRight.setFromMatrixColumn(camera.matrixWorld, 0), right * step);
      delta.addScaledVector(_screenUp.setFromMatrixColumn(camera.matrixWorld, 1), up * step);
      (onJointNudge ?? onJointDrag)?.(bone.name, delta);
    };

    /**
     * 🔴 Cleared through the SAME callbacks a click uses. Hiding the popover locally would leave
     * the owner still holding the selection, and its next render would put the popover straight
     * back — two sources of truth disagreeing, with the canvas losing.
     */
    const clearSelection = () => {
      if (activeMode === 'joints') onJointPicked?.('');
      else if (activeMode === 'pieces') onPickComponent?.(-1, -1);
      // Escape abandons a stroke in progress as well. The owner still gets `onBrushEnd`, so its
      // `commitStroke()` runs and the history entry covers what was already painted — a half
      // stroke that never lands in the stack is a half stroke that cannot be undone.
      endStroke();
      clearHover();
    };

    /**
     * The keyboard, scoped to the VIEWPORT and never to `window`.
     *
     * 🔴 A global handler would swallow `1`–`4` and the arrow keys from every field on the page.
     * The inspector beside this canvas has a name field, a prompt box and several number inputs,
     * and a shortcut that eats a keystroke in a text box is worse than no shortcut at all — it is a
     * bug the user cannot even describe.
     *
     * The guard is `e.target !== hostEl`, which is stricter than testing for inputs and is the
     * whole point: keystrokes aimed at anything INSIDE the viewport — a popover's rename field, its
     * Delete button — bubble up here carrying that element as their target, and are left alone.
     * Only the viewport itself holding focus counts, which is what tabbing to it or clicking the
     * canvas does.
     */
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.target !== hostEl) return;
      // Modified keys belong to the browser and the OS; `Shift` alone is ours (it scales a nudge).
      if (e.altKey || e.ctrlKey || e.metaKey) return;

      const wanted = MODE_KEYS[e.key];
      if (wanted) {
        // The same rest-pose refusal the rail and the click path apply — asking for a mode a take
        // cannot have would only be answered with the downgrade to `view`, which reads as the key
        // doing something random.
        if (!(posed && (wanted === 'pieces' || wanted === 'weights'))) onModeRequested?.(wanted);
        e.preventDefault();
        return;
      }

      const arrow = NUDGE_KEYS[e.key];
      if (arrow) {
        nudgeSelectedJoint(arrow[0], arrow[1], e.shiftKey);
        // Swallowed even when nothing is selected: the alternative is the page scrolling under a
        // canvas the user has explicitly focused.
        e.preventDefault();
        return;
      }

      if (e.key === 'Escape') {
        clearSelection();
        e.preventDefault();
      } else if (e.key === 'f' || e.key === 'F') {
        focusSelection();
        e.preventDefault();
      }
    };

    // ⚠️ `tabIndex` and the ARIA hints are set on the element here rather than written in the
    // template, for two reasons. An empty stage should not be a tab stop — there is nothing to
    // drive and no model to drive it on — and a `tabindex` on a non-interactive element in the
    // template is an a11y lint failure that only a `svelte-ignore` would quieten, which is itself
    // an error in this repo. Both come off in `dispose`.
    hostEl.tabIndex = 0;
    hostEl.setAttribute('role', 'application');
    hostEl.setAttribute('aria-label', VIEWPORT_LABEL);
    hostEl.setAttribute('aria-keyshortcuts', VIEWPORT_SHORTCUTS);
    hostEl.addEventListener('keydown', onKeyDown);

    const resize = () => {
      const w = hostEl.clientWidth;
      const h = hostEl.clientHeight;
      // ⚠️ The tab switcher hides the inactive panel with `hidden` (`display: none`), which zeroes
      // both — and the height is a CSS length now, so it can be measured mid-layout as well. Zero
      // divides into an infinite aspect and a broken projection. Skipping is safe: the observer
      // fires again when the box comes back.
      if (w < 1 || h < 1) return;
      viewW = w;
      viewH = h;
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
      renderer.setSize(w, h, false);
      // ⚠️ Never re-frame over a flight in progress. `frameCamera` jumps the camera, and the rest
      // of the lerp would then drag it back from wherever the jump left it — a visible fight
      // between two things both trying to help.
      if (flight) return;
      const current = camera.position.distanceTo(controls.target);
      if (framedDistance === 0 || Math.abs(current - framedDistance) < framedDistance * 0.01) {
        frameCamera();
      }
    };
    const observer = new ResizeObserver(resize);
    observer.observe(hostEl);
    resize();

    const clock = new THREE.Clock();
    renderer.setAnimationLoop(() => {
      const dt = clock.getDelta();
      mixer?.update(dt);
      // Before `controls.update()`, which re-derives its spherical from the camera and the target
      // this just wrote — so the flight leads and the controls follow.
      stepFlight(dt);
      controls.update();
      // Handles follow the bones every frame — they have to track both a playing animation and a
      // joint the user is dragging right now.
      jointHandles?.update();
      // The ring is a billboard, so it re-aims even when the pointer has not moved — an orbit past
      // a stationary cursor would otherwise turn the circle into a line.
      brushCursor?.update(camera);
      if (hoverPending) {
        hoverPending = false;
        if (activeMode === 'paint') runBrush();
        else runHover(camera);
      }
      publishAnchors(camera, centre);
      publishFrame();
      renderer.render(scene, camera);
    });

    dispose = () => {
      renderer.setAnimationLoop(null);
      observer.disconnect();
      renderer.domElement.removeEventListener('pointerdown', onPointerDown);
      renderer.domElement.removeEventListener('pointermove', onPointerMove);
      renderer.domElement.removeEventListener('pointerup', onPointerUp);
      renderer.domElement.removeEventListener('pointercancel', onPointerCancel);
      renderer.domElement.removeEventListener('pointerleave', onPointerLeave);
      hostEl.removeEventListener('keydown', onKeyDown);
      // The host outlives this scene — it is the component's own div — so everything put on it
      // comes back off, or an empty stage stays a tab stop that announces shortcuts it no longer
      // has.
      hostEl.removeAttribute('tabindex');
      hostEl.removeAttribute('role');
      hostEl.removeAttribute('aria-label');
      hostEl.removeAttribute('aria-keyshortcuts');
      flight = null;
      // 🔴 The overlays go FIRST, and the grade with them. Each proxy drops the body's borrowed
      // attributes before disposing its own geometry; running the traversal below first would free
      // the character's GPU buffers while the proxies still held those attribute objects.
      detachGrade();
      jointHandles?.dispose();
      markOverlay?.dispose();
      hoverOverlay?.dispose();
      heatOverlay?.dispose();
      hoverPicker?.dispose();
      brushCursor?.dispose();
      dragging = null;
      painting = false;
      strokePointer = null;
      hoverPending = false;
      controls.dispose();
      renderer.dispose();
      mixer?.stopAllAction();
      root.traverse((o) => {
        const mesh = o as THREE.Mesh;
        mesh.geometry?.dispose();
        const mats = Array.isArray(mesh.material) ? mesh.material : [mesh.material];
        mats.forEach((m) => {
          const material = m as THREE.MeshStandardMaterial | undefined;
          material?.map?.dispose();
          material?.normalMap?.dispose();
          material?.dispose();
        });
      });
    };

    // Last, so the effects that push the current props into these overlays run against a scene that
    // is fully built.
    modelVersion++;

    onReady?.({
      root,
      skinnedMeshes,
      bones,
      clips,
      source,
      associations: gltf.parser?.associations ?? new Map(),
      renderer,
      animationClips: allClips,
      mixer,
      skeleton: skinnedMeshes[0]?.skeleton ?? null,
      snapshots,
      refresh: () => {
        skinnedMeshes.forEach((mesh) => {
          const skinIndex = mesh.geometry.getAttribute('skinIndex');
          const skinWeight = mesh.geometry.getAttribute('skinWeight');
          if (skinIndex) skinIndex.needsUpdate = true;
          if (skinWeight) skinWeight.needsUpdate = true;
        });
        helper?.updateMatrixWorld(true);
      },
      invalidateGeometry: () => {
        markOverlay?.invalidate();
        hoverOverlay?.invalidate();
        hoverPicker?.invalidate();
        // Every mesh, not just the body: `HoverPicker` only knows the one it picks on, and the BVH
        // cache is keyed by geometry.
        skinnedMeshes.forEach((mesh) => {
          disposeBvh(mesh.geometry);
          // 🔴 The paint cache too, and this one is NOT self-healing on an index-only edit. It
          // re-validates on the position attribute's identity, count and version — all three
          // unchanged by `applyIndex` — while the `smooth` mode's class adjacency is built FROM
          // THE INDEX. Skip this and a smooth after a piece delete averages across edges of
          // triangles that no longer exist, silently, on the mesh's own numbering.
          disposeWeightPaintCache(mesh.geometry);
        });
        // 🔴 The heat proxies borrow the body's attributes AND its index, and `compactVertices`
        // replaces both; `setRatios` is the only thing that re-takes them. An empty push also makes
        // the map go cold, which is honest — those ratios were measured against a vertex numbering
        // that no longer exists. Push `heat` again after re-measuring.
        lastRatios = null;
        lastMask = null;
        heatOverlay?.setRatios([]);
        lastHoverComponent = -1;
      },
      detachGrade,
      focusOn,
      focusSelection,
      setView,
      frameCount: () => clipFrames,
      scrubTo
    });
  }

  /** One pick per animation frame, in whichever mode is asking for one. */
  function runHover(camera: THREE.PerspectiveCamera) {
    const active = activeMode;
    raycaster.setFromCamera(pointer, camera);
    if (active === 'joints') {
      const index = jointHandles?.pick(raycaster) ?? -1;
      jointHandles?.setHovered(index);
      const name = index >= 0 ? (sceneBones[index]?.name ?? '') : '';
      if (name !== lastHoverJoint) {
        lastHoverJoint = name;
        onHoverJoint?.(name);
      }
      return;
    }
    if (active !== 'pieces' || !hoverPicker) return;
    // ⚠️ The FIRST pick after a geometry change builds the BVH — hundreds of ms at 440k triangles,
    // as one frozen frame. Every pick after it is a few dozen triangle tests. There is no way to
    // avoid paying it; warming it early only moves the freeze somewhere else.
    const face = hoverPicker.pick(raycaster);
    const id = labels ? componentAtFace(labels, face) : -1;
    if (id === lastHoverComponent) return;
    lastHoverComponent = id;
    // 🔴 Redrawn only on CHANGE. `showComponent` scans the whole faceComponent array (440k entries)
    // to gather one component's triangles, and doing that per frame while the cursor sits still
    // would burn the frame budget to produce an identical picture.
    if (id >= 0 && labels && id !== selectedComponent) {
      hoverOverlay?.showComponent(labels, id, 'hover');
    } else {
      hoverOverlay?.hide();
    }
    onHoverComponent?.(id);
  }

  /**
   * ⚠️ Assigns to `$state` only when the ROUNDED position or `behind` actually changed. This runs
   * at 60 Hz, so an unconditional assignment is sixty component re-renders a second — and the
   * popovers hanging off it are not free to re-render.
   */
  function publishAnchors(camera: THREE.PerspectiveCamera, centre: THREE.Vector3) {
    const centreDistance = camera.position.distanceTo(centre);

    const jointIndex = jointPopover && activeMode === 'joints' ? selectedJointIndex : -1;
    const bone = jointIndex >= 0 ? sceneBones[jointIndex] : undefined;
    let next: StageAnchor | null = null;
    if (bone) {
      // Translation straight out of the world matrix, as `JointHandles.place` does —
      // `getWorldPosition` would re-walk the whole parent chain for the same three numbers.
      const m = bone.matrixWorld.elements;
      next = projectAnchor(_anchor.set(m[12], m[13], m[14]), camera, centreDistance);
    }
    if (!sameAnchor(jointAnchor, next)) jointAnchor = next;

    next = null;
    const piece = piecePopover && activeMode === 'pieces' ? selectedComponent : -1;
    const component = piece >= 0 ? labels?.components[piece] : undefined;
    if (component && body) {
      _anchor.set(component.center[0], component.center[1], component.center[2]);
      next = projectAnchor(_anchor.applyMatrix4(restToWorld(body)), camera, centreDistance);
    }
    if (!sameAnchor(pieceAnchor, next)) pieceAnchor = next;
  }

  /**
   * The matrix that carries a `position` attribute value to WORLD, at rest.
   *
   * 🔴 NOT `matrixWorld`, and the difference is silent. `GLTFLoader` binds with an IDENTITY
   * `bindMatrix`, and under the default `AttachedBindMode` `updateMatrixWorld` sets
   * `bindMatrixInverse = matrixWorld⁻¹` — the two cancel, and the rendered rest surface sits at
   * `bindMatrix · p`. Using `matrixWorld` here misses by exactly the mesh node's own transform,
   * which is invisible on every asset whose node happens to sit at the origin. Same rule as
   * `mesh-repair`'s `restToWorld` and `stage.ts`'s `restPosition`, and the reason a component
   * centre out of `labelComponents` cannot simply be fed to the camera.
   */
  function restToWorld(mesh: THREE.SkinnedMesh): THREE.Matrix4 {
    return mesh.bindMode === THREE.AttachedBindMode ? mesh.bindMatrix : mesh.matrixWorld;
  }

  /** ⚠️ Consumes `point`: it is projected in place. Pass the scratch vector, not a live position. */
  function projectAnchor(
    point: THREE.Vector3,
    camera: THREE.PerspectiveCamera,
    centreDistance: number
  ): StageAnchor {
    const distance = camera.position.distanceTo(point);
    point.project(camera);
    // Two ways to be "behind", and the caller treats them alike: past the model's centre, or past
    // the camera's own plane — where NDC is mirrored and the clamp below is the only thing keeping
    // the popover on screen at all.
    const behind = point.z > 1 || distance > centreDistance;
    return {
      x: clamp(Math.round((point.x * 0.5 + 0.5) * viewW), 0, viewW),
      y: clamp(Math.round((-point.y * 0.5 + 0.5) * viewH), 0, viewH),
      behind
    };
  }

  function clamp(value: number, low: number, high: number): number {
    return value < low ? low : value > high ? high : value;
  }

  function sameAnchor(a: StageAnchor | null, b: StageAnchor | null): boolean {
    if (a === null || b === null) return a === b;
    return a.x === b.x && a.y === b.y && a.behind === b.behind;
  }

  function playClip(name: string) {
    if (!mixer) return;
    const clip = allClips.find((c) => c.name === name);
    if (!clip) return;
    action?.stop();
    action = mixer.clipAction(clip);
    action.reset();
    action.setLoop(THREE.LoopRepeat, Infinity);
    action.timeScale = speed;
    action.paused = !playing;
    action.play();
    clipFrames = clipFrameCount(clip);
    clipDuration = clip.duration;
    frameTotal = clipFrames;
    frameNow = 0;
    lastFrame = 0;
    onClipChanged?.(name, clipFrames);
  }

  /**
   * Hold the model on one frame of the clip on screen.
   *
   * 🔴 NOT `mixer.setTime`, and `motion-repair`'s `detectIntersections` documents why in the same
   * words: `setTime` zeroes `mixer.time` AND every action's `time` before it re-runs `update`, so
   * anything else playing on this mixer is silently rewound underneath the caller. Worse, it does
   * not land where you asked — `update` scales its delta by `mixer.timeScale` and the action scales
   * again by its own, so with the speed slider at 0.5× a request for frame 90 would land on 45.
   *
   * This writes `action.time` directly and drives the mixer with a ZERO delta. At `deltaTime === 0`
   * `AnimationAction._updateTime` returns the action's time verbatim — no loop wrap, no time-scale
   * multiply — and every OTHER action on the mixer keeps the time it had, because nothing zeroed
   * it. Verified against the r185 sources, 2026-08-17.
   *
   * The action is PAUSED as well, which is what makes the hold survive the render loop: a paused
   * action's effective time scale is 0, so `mixer.update(dt)` cannot advance it, and it stays in the
   * mixer's active set so it keeps re-applying the held pose every frame rather than letting the
   * skeleton drift back.
   */
  function scrubTo(frame: number) {
    const current = action;
    if (!mixer || !current || clipFrames < 1) return;
    const at = clamp(Math.round(frame), 0, clipFrames - 1);
    playing = false;
    // Set here as well as through the `playing` effect: that effect flushes after this function
    // returns, and `mixer.update` below has to already see a paused action.
    current.paused = true;
    current.time = clipFrames > 1 ? (at / (clipFrames - 1)) * clipDuration : 0;
    mixer.update(0);
    publishFrame();
  }

  /**
   * ⚠️ Assigns to `$state` and calls back only when the INTEGER frame changes, for the same reason
   * `publishAnchors` does: this runs at 60 Hz and a 24 fps clip only turns over half as often.
   */
  function publishFrame() {
    const current = action;
    if (!current || clipFrames < 1) return;
    const at =
      clipFrames > 1 && clipDuration > 0
        ? clamp(Math.round((current.time / clipDuration) * (clipFrames - 1)), 0, clipFrames - 1)
        : 0;
    if (at === lastFrame) return;
    lastFrame = at;
    frameNow = at;
    onFrame?.(at, clipFrames);
  }

  function onClipChange() {
    playing = true;
    playClip(selectedClip);
  }

  onDestroy(() => teardown());
</script>

<div class="stage">
  <div bind:this={host} class="viewport" style={`height:${cssHeight}`}>
    <canvas bind:this={canvasEl}></canvas>

    <!-- Before `.layer`, so an anchored popover that happens to land on the right edge paints over
         these rather than under them. Gated on `aim`, which is null until a model is on screen —
         there is nothing to aim a camera at before then. -->
    {#if aim}
      <div class="tools">
        <div class="axes">
          {#each VIEW_AXES as axis (axis.id)}
            <button class="tool" title={axis.hint} onclick={() => aim?.setView(axis.id)}>
              {axis.label}
            </button>
          {/each}
        </div>
        <button
          class="tool wide"
          title="Frame the selection (F)"
          onclick={() => aim?.focusSelection()}
        >
          Focus
        </button>
        <!-- ⚠️ `aria-hidden`: the accessible copy of this is `aria-keyshortcuts` on the viewport
             itself, and reading the same list twice is worse than reading it once. `pointer-events`
             stays off so the legend cannot eat an orbit gesture. -->
        <div class="keys" aria-hidden="true">
          <span><kbd>1</kbd>–<kbd>4</kbd> mode</span>
          <span><kbd>F</kbd> frame</span>
          <span><kbd>Esc</kbd> clear</span>
          {#if activeMode === 'joints' && canDragJoints}
            <span><kbd>←↑↓→</kbd> nudge</span>
          {/if}
        </div>
      </div>
    {/if}

    <!-- ⚠️ `pointer-events: none`, or this layer swallows the orbit gesture over most of the
         model. Each popover re-enables it on its own root. -->
    <div class="layer">
      {#if hud}{@render hud()}{/if}
      {#if jointPopover && jointAnchor && selectedJointIndex >= 0}
        <div class="anchored" style={`left:${jointAnchor.x}px;top:${jointAnchor.y}px`}>
          {@render jointPopover({
            name: selectedJointName,
            index: selectedJointIndex,
            anchor: jointAnchor
          })}
        </div>
      {/if}
      {#if piecePopover && pieceAnchor && selectedComponent >= 0}
        <div class="anchored" style={`left:${pieceAnchor.x}px;top:${pieceAnchor.y}px`}>
          {@render piecePopover({ id: selectedComponent, anchor: pieceAnchor })}
        </div>
      {/if}
    </div>

    {#if modeNotice}
      <p class="notice">{modeNotice}</p>
    {/if}

    {#if loading}
      <div class="overlay">Loading model…</div>
    {:else if error}
      <div class="overlay err">{error}</div>
    {:else if !src}
      <div class="overlay">Nothing selected</div>
    {/if}
  </div>

  <div class="bar">
    {#if label}<span class="label">{label}</span>{/if}
    {#if stats}
      <span class="stat">{stats.bones} bones · {stats.vertices.toLocaleString()} verts</span>
    {/if}
    {#if activeMode === 'joints'}
      <span class="stat muted">
        {#if canDragJoints}
          Drag a handle to move a joint · {dragHint}
        {:else}
          Tap a handle to select a joint
        {/if}
      </span>
    {:else if activeMode === 'pieces'}
      <span class="stat muted">Hover to highlight a piece, click to select it</span>
    {:else if activeMode === 'paint'}
      <span class="stat muted">
        {#if brush}
          Drag on the model to paint · press on empty space to orbit
        {:else}
          Pick a brush to paint
        {/if}
      </span>
    {/if}
    {#if ghostError}
      <!-- The ghost is a comparison aid. Saying so here keeps a failed one from reading as a model
           that failed to load — the character on screen is fine. -->
      <span class="stat warn">Ghost model unavailable: {ghostError}</span>
    {/if}
    {#if clips.length}
      <label class="clip">
        Clip
        <select bind:value={selectedClip} onchange={onClipChange}>
          {#each clips as clip (clip)}
            <option value={clip}>{clip}</option>
          {/each}
        </select>
      </label>
      <button class="btn-sec small" onclick={() => (playing = !playing)}>
        {playing ? 'Pause' : 'Play'}
      </button>
      {#if frameTotal > 1}
        <!-- ⚠️ `value=`, not `bind:value`. The loop drives this slider while a clip plays, and a
             two-way binding would feed those frames straight back in as scrub requests — a
             transport that pauses itself the moment you press play. -->
        <label class="scrub">
          Frame
          <input
            type="range"
            min="0"
            max={frameTotal - 1}
            step="1"
            value={frameNow}
            oninput={(e) => scrubTo(e.currentTarget.valueAsNumber)}
          />
          <b>{frameNow + 1}/{frameTotal}</b>
        </label>
      {/if}
      <label class="speed">
        Speed
        <input type="range" min="0.1" max="2" step="0.1" bind:value={speed} />
        <b>{speed.toFixed(1)}×</b>
      </label>
    {:else if src && !loading && !error}
      <!-- A rig with no tracks is the normal state of a freshly delivered tier: animations are
           baked in by the retarget, which has not run for this GLB yet. -->
      <span class="stat muted">No baked animation tracks — this is the rig itself.</span>
    {/if}
  </div>
</div>

<style>
  .stage {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .viewport {
    position: relative;
    width: 100%;
    /* The height arrives as a CSS length. This floor is what stops a viewport-relative value from
       collapsing on a short window — a zero-height box makes the ResizeObserver fire a broken
       projection, and the camera has nothing to recover from. */
    min-height: 420px;
    border-radius: 8px;
    overflow: hidden;
    border: 1px solid var(--ring);
    background: #15171d;
  }
  /* The stage is focusable (see `install`), so it has to SHOW that it is — a keyboard user who
     cannot tell where focus went has no shortcuts, only a canvas that sometimes reacts. */
  .viewport:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .viewport canvas {
    display: block;
    width: 100%;
    height: 100%;
  }
  /* Right, vertically centred: the workbench's own HUD takes top-left (mode rail), top-right
     (hover notes) and bottom-left (colour/threshold floater), and `.notice` takes the bottom
     strip. This is the one edge left. */
  .tools {
    position: absolute;
    top: 50%;
    right: 10px;
    transform: translateY(-50%);
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 6px;
  }
  .axes {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 6px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 8px 24px rgb(0 0 0 / 35%);
  }
  .tool {
    padding: 4px 8px;
    border: 1px solid transparent;
    border-radius: 6px;
    background: transparent;
    color: var(--muted);
    font: inherit;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
    cursor: pointer;
  }
  .tool:hover {
    color: var(--ink);
    border-color: var(--ring);
  }
  .tool.wide {
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 8px 24px rgb(0 0 0 / 35%);
  }
  .keys {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 2px;
    font-size: 10px;
    color: var(--muted);
    opacity: 0.65;
    /* Never in the way of an orbit gesture. */
    pointer-events: none;
  }
  .keys kbd {
    font-family: inherit;
    font-size: 10px;
    color: var(--ink);
  }
  .layer {
    position: absolute;
    inset: 0;
    /* 🔴 The whole reason in-canvas UI is possible: the layer is transparent to the pointer, so
       orbiting still works everywhere a popover is not. */
    pointer-events: none;
  }
  .anchored {
    position: absolute;
    /* Position only. The snippet decides which way to open, whether to fade on `anchor.behind`,
       and re-enables pointer events on itself. */
    pointer-events: none;
  }
  .notice {
    position: absolute;
    left: 12px;
    right: 12px;
    bottom: 12px;
    margin: 0;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--warning);
    background: rgb(21 23 29 / 0.92);
    color: var(--warning);
    font-size: 12px;
    line-height: 1.4;
    pointer-events: none;
  }
  .overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--muted);
    font-size: 13px;
    pointer-events: none;
    padding: 16px;
    text-align: center;
  }
  .overlay.err {
    color: var(--danger);
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    font-size: 12px;
    color: var(--muted);
  }
  .label {
    color: var(--ink);
    font-weight: 600;
  }
  .stat {
    font-variant-numeric: tabular-nums;
  }
  .muted {
    color: var(--muted);
  }
  .warn {
    color: var(--warning);
  }
  .clip,
  .speed,
  .scrub {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    margin: 0;
  }
  /* Two class names deep so it beats the bare `input[type='range']` rule below whichever order
     they end up in — a frame scrubber needs more travel than a speed knob. */
  .scrub input[type='range'] {
    width: 160px;
  }
  .scrub b {
    font-variant-numeric: tabular-nums;
  }
  select {
    padding: 4px 8px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    color: var(--ink);
    font-family: inherit;
    font-size: 12px;
    max-width: 260px;
  }
  input[type='range'] {
    width: 90px;
  }
  .btn-sec.small {
    padding: 4px 10px;
    font-size: 12px;
  }
</style>
