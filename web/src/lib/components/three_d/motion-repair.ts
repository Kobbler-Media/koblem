/*
 * Post-hoc correction of a delivered animation clip: the hands-through-the-body defect.
 *
 * # Why this exists at all
 *
 * The engine retargets motion joint-by-joint from a source skeleton. It has no idea a body is in
 * the way, so on a subject whose proportions differ from the source's, forearms and hands sweep
 * through the torso. That is not fixable upstream without giving the retarget a collision model, so
 * this module corrects the clip AFTER it is delivered — a constant rotation added to a limb's
 * existing keys, which pushes the limb out without touching its timing or its character.
 *
 * The product shape this is built for is **one slider the user drags and immediately sees**, so
 * every function here is cheap enough to run on every frame of a drag (a 120-key clip costs a
 * memcpy and ~120 quaternion multiplies per bone) and every adjustment recomputes from a pristine
 * snapshot rather than from the current state.
 *
 * # The three properties everything else hangs off
 *
 * 1. **Recompute from the snapshot, never from the current values.** `offsetBoneTrack` copies the
 *    pristine values in before it applies anything. Compounding onto the live values makes the
 *    slider one-way: drag right, drag back, and the arm does not return.
 * 2. **Mutate `track.values` in place.** `KeyframeTrack.InterpolantFactoryMethodLinear` passes
 *    `this.times, this.values` to the interpolant BY REFERENCE, and `AnimationAction` builds its
 *    interpolants once in its constructor and keeps them. Writing floats into the existing
 *    `Float32Array` is therefore live on the next `mixer.update(0)`. 🔴 Assigning `track.values =`
 *    a new array — or anything that reallocates: `optimize()`, `trim()`, resampling,
 *    `clip.clone()` — leaves the interpolant pointing at the OLD buffer and the edit silently does
 *    nothing until `mixer.uncacheClip(clip)` and a fresh `mixer.clipAction(clip)`.
 * 3. **Premultiply, never post-multiply.** `q' = qOff * q` rotates about an axis fixed in the
 *    PARENT's frame, so "+8° of abduction" means the same anatomical thing on every frame.
 *    `q' = q * qOff` rotates about an axis fixed in the bone's own animated frame — that is a
 *    twist along the bone, and its axis swings around with the animation. The difference is
 *    invisible on frame 0 and obvious by frame 40.
 *
 * # What was ruled out, so nobody re-treads it
 *
 * - `three/addons/math/Capsule.js` offers `intersectsBox` and nothing else — no capsule-capsule.
 *   The segment-segment closest-point test at the bottom of this file is the missing 40 lines.
 * - `Octree.capsuleIntersect` and `three-mesh-bvh` are both built over STATIC triangles. A skinned
 *   body deforms every frame; rebuilding either per frame costs more than the whole render.
 * - `CCDIKSolver` is refused outright. Its `target`/`effector` are indices into `skeleton.bones`,
 *   so an IK target has to be appended as a real bone — and `GLTFExporter.processSkin` then writes
 *   that bone into the skin's `joints` array, contaminating a rig whose downstream consumer counts
 *   named joints.
 *
 * # Before ANY export
 *
 * 🔴 `mixer.stopAllAction()`, then `skeleton.pose()`, then `root.updateMatrixWorld(true)`. Node TRS
 * is written from the currently-posed bones, so exporting mid-animation bakes an animation frame in
 * as the rest pose while `inverseBindMatrices` still describes the real bind — and the two then
 * disagree, which reads as a character permanently stuck in a half-crouch.
 *
 * Every API used here was checked against the shipped `three@0.185.1` sources, 2026-08-15.
 */

import * as THREE from 'three';

import { sanitizeName } from './stage';

// ---------------------------------------------------------------------------------------------
// Canonical names
// ---------------------------------------------------------------------------------------------

/*
 * 🔴 The names below are the CANONICAL, in-the-file names — the ones the engine's retarget matches
 * on. They never appear on a `THREE.Bone`: `GLTFLoader.createUniqueName` runs every node name
 * through `PropertyBinding.sanitizeNodeName`, which DELETES `[ ] . : /` because those characters
 * are the track-path grammar. So `mixamorig:LeftArm` in the file is `mixamorigLeftArm` on the bone
 * and `mixamorigLeftArm.quaternion` on the track.
 *
 * Every lookup in this file therefore goes through `sanitizeName` (imported from `./stage`, which
 * owns that regex — do not reimplement it here, the two must never drift). The canonical form is
 * what gets REPORTED, because that is the form a human recognises and the form the engine logs.
 */

const HIPS = 'mixamorig:Hips';
const HEAD = 'mixamorig:Head';
const NECK = 'mixamorig:Neck';

/** The torso chain — what a limb collides WITH. Deliberately excludes the shoulders: a shoulder is
 *  the arm's own parent, and a bone always overlaps its parent's capsule. */
export const BODY_BONES = [
  HIPS,
  'mixamorig:Spine',
  'mixamorig:Spine1',
  'mixamorig:Spine2',
  NECK,
  HEAD
] as const;

/** What passes through the body. Upper arms are included because on a wide subject the humerus
 *  buries itself in the ribcage before the forearm ever gets near. */
export const LIMB_BONES = [
  'mixamorig:LeftArm',
  'mixamorig:LeftForeArm',
  'mixamorig:LeftHand',
  'mixamorig:RightArm',
  'mixamorig:RightForeArm',
  'mixamorig:RightHand'
] as const;

/** Left/right pairs the clearance control drives. Order is [left, right] and that order is load
 *  bearing — the sign convention in `applyClearance` depends on it. */
const ARM_PAIR = ['mixamorig:LeftArm', 'mixamorig:RightArm'] as const;
const LEG_PAIR = ['mixamorig:LeftUpLeg', 'mixamorig:RightUpLeg'] as const;

// ---------------------------------------------------------------------------------------------
// Tunables — the honest knobs. Each one is a judgement call, not a derived constant.
// ---------------------------------------------------------------------------------------------

/**
 * Which percentile of a bone's skin distances becomes its capsule radius.
 *
 * 🔴 NOT the maximum. `stage.ts` documents the exact reason: a rig routinely carries a handful of
 * vertices bound to a joint half a body away (97 out of 247,727 on one measured subject). A max
 * radius lets those strays inflate the torso capsule until it swallows the whole silhouette and
 * every frame reports a collision. A percentile is immune to them.
 */
const RADIUS_PERCENTILE = 0.75;

/** Histogram resolution for that percentile. 64 bins over [0, max] is ~1.5% of a bone's reach —
 *  finer than the radius estimate deserves, and it keeps the second pass allocation-free. */
const RADIUS_BINS = 64;

/**
 * How deep a limb must be before it counts as a collision, as a fraction of the two radii summed.
 *
 * Capsules are a crude proxy: an arm hanging naturally at an A-pose is already touching the torso
 * capsule. Reporting that is technically true and completely useless. This is the false-positive
 * knob — raise it if detection lights up on a clip that looks fine.
 */
const CONTACT_SLACK = 0.1;

/** Envelope half-width, in frames — ±4 at 30 fps is a little over a tenth of a second, which is
 *  about the shortest correction the eye reads as deliberate. See `envelopeFrom` for why it
 *  dilates before it smooths. */
const ENVELOPE_RADIUS = 4;

const EPSILON = 1e-10;

// ---------------------------------------------------------------------------------------------
// Snapshot / restore
// ---------------------------------------------------------------------------------------------

/** Pristine copies of every track's values, taken once. 🔴 Every adjustment recomputes FROM this,
 *  never from the current state, or a slider is one-way. */
export interface ClipSnapshot {
  clipName: string;
  values: Map<string, Float32Array>;
}

/**
 * Copy every track's values aside, and de-alias any that share a buffer.
 *
 * 🔴 CALL THIS BEFORE THE FIRST `mixer.clipAction(clip)`. The de-aliasing below REPLACES
 * `track.values`, and an interpolant that already exists holds the old array. Snapshot at load,
 * next to the `GLTFLoader` callback, not lazily when the user first opens the panel.
 *
 * ⚠️ The de-aliasing is not paranoia. `GLTFLoader` passes `accessor.array` straight into the track
 * constructor and caches accessors by index, so two animation samplers that reference the SAME
 * output accessor — which a baker emitting identical constant tracks will happily produce — end up
 * as two tracks sharing one `Float32Array`. Mutating one would then move a completely unrelated
 * bone, once, mysteriously, on some assets only. Object identity is the right test here: distinct
 * accessors are distinct views even when they sit in one ArrayBuffer, so they cannot collide.
 *
 * (`track.times` is shared across tracks on nearly every glTF and that is fine — nothing here ever
 * writes to it.)
 */
export function snapshotClip(clip: THREE.AnimationClip): ClipSnapshot {
  const values = new Map<string, Float32Array>();
  const owner = new Map<Float32Array, string>();

  for (const track of clip.tracks) {
    if (values.has(track.name)) {
      // Two tracks with one name is unfixable here and breaks the mixer's binding cache too, so
      // fail loudly rather than silently correcting only one of them.
      throw new Error(
        `Animation "${clip.name}" has two tracks named "${track.name}". The clip is malformed; ` +
          `re-export it before trying to repair it.`
      );
    }
    if (owner.has(track.values)) {
      track.values = track.values.slice();
    }
    owner.set(track.values, track.name);
    values.set(track.name, track.values.slice());
  }

  return { clipName: clip.name, values };
}

/** Put a clip back exactly as delivered. */
export function restoreClip(clip: THREE.AnimationClip, snapshot: ClipSnapshot): void {
  for (const track of clip.tracks) {
    const base = snapshot.values.get(track.name);
    if (!base) continue;
    if (base.length !== track.values.length) {
      throw new Error(
        `Track "${track.name}" has been resampled since the snapshot was taken, so it can no ` +
          `longer be restored. Reload the model.`
      );
    }
    // In place — see the header. `track.values = base` would restore nothing visible.
    track.values.set(base);
  }
}

// ---------------------------------------------------------------------------------------------
// Track shape
// ---------------------------------------------------------------------------------------------

/** The one property of `createInterpolant` we care about. `@types/three` 0.185.4 does not declare
 *  `createInterpolant` on `KeyframeTrack` at all, hence the cast at the single point of use. */
interface CubicSplineFactory {
  isInterpolantFactoryMethodGLTFCubicSpline?: boolean;
}

/**
 * Whether a track carries glTF CUBICSPLINE data.
 *
 * 🔴 This changes the memory layout, not just the interpolation. A CUBICSPLINE key stores THREE
 * quaternions — `[inTangent, splineVertex, outTangent]` — so `values.length / times.length` is 12,
 * not 4. Striding by 4 walks the tangents as if they were orientations and turns a clean clip into
 * a seizure.
 *
 * `GLTFLoader._createCubicSplineTrackInterpolant` marks the factory rather than the track, because
 * `track.getInterpolation()` cannot express a custom interpolant — so the flag is the primary test.
 * The stride check is a belt-and-braces corroboration for a track that reached us some other way.
 * (`KeyframeTrack.clone()` copies `createInterpolant` across, so the flag survives cloning.)
 */
export function isCubicSpline(track: THREE.KeyframeTrack): boolean {
  const factory = (track as unknown as { createInterpolant?: CubicSplineFactory })
    .createInterpolant;
  if (factory?.isInterpolantFactoryMethodGLTFCubicSpline === true) return true;
  return track.ValueTypeName === 'quaternion' && track.getValueSize() === 12;
}

/**
 * The clip's frame count: the key count of its densest track.
 *
 * Every `Float32Array` of weights in this module is indexed on this grid, uniformly spaced across
 * `clip.duration`. ⚠️ glTF permits each node its own sampler input, so a clip is NOT guaranteed to
 * have one key count — which is exactly why `offsetBoneTrack` maps weights by TIME rather than
 * trusting index alignment.
 */
export function clipFrameCount(clip: THREE.AnimationClip): number {
  let frames = 0;
  for (const track of clip.tracks) frames = Math.max(frames, track.times.length);
  return frames;
}

// ---------------------------------------------------------------------------------------------
// Bind-pose geometry
// ---------------------------------------------------------------------------------------------

const _bindMatrix = /*@__PURE__*/ new THREE.Matrix4();
const _decomposePos = /*@__PURE__*/ new THREE.Vector3();
const _decomposeScale = /*@__PURE__*/ new THREE.Vector3();

/**
 * Where a bone sits at the bind pose, in the space `boneInverses` are expressed in.
 *
 * 🔴 Read from `boneInverses[i]` inverted, never from `bone.matrixWorld` or `bone.position`.
 * `boneInverses` is animation-independent; the live scene graph is whatever frame the viewer
 * happens to be showing, so measuring off it makes every result depend on when the user opened the
 * panel. (Same reasoning as `stage.ts`'s rest-position handling — glTF skinning is
 * `Σ wᵢ · (boneWorldᵢ · boneInverseᵢ) · bindMatrix · v`, and at rest the bracket is the identity.)
 */
function bindPosition(skeleton: THREE.Skeleton, index: number, out: THREE.Vector3): THREE.Vector3 {
  _bindMatrix.copy(skeleton.boneInverses[index]).invert();
  return out.setFromMatrixPosition(_bindMatrix);
}

/** Bind-pose world ROTATION of a bone, by the same route and for the same reason. */
function bindRotation(
  skeleton: THREE.Skeleton,
  index: number,
  out: THREE.Quaternion
): THREE.Quaternion {
  _bindMatrix.copy(skeleton.boneInverses[index]).invert();
  // decompose rather than extractRotation: a bind matrix with non-uniform scale would otherwise
  // hand back a rotation with the scale still baked into its columns.
  _bindMatrix.decompose(_decomposePos, out, _decomposeScale);
  return out;
}

/** Sanitized-name → index into `skeleton.bones`. Built once per call that needs it; a rig is 40–60
 *  bones so this is cheaper than repeated `getBoneByName` scans inside a loop. */
function boneIndexMap(skeleton: THREE.Skeleton): Map<string, number> {
  const map = new Map<string, number>();
  skeleton.bones.forEach((bone, i) => map.set(bone.name, i));
  return map;
}

function indexOfCanonical(
  index: Map<string, number>,
  canonical: string | undefined
): number | undefined {
  return canonical === undefined ? undefined : index.get(sanitizeName(canonical));
}

// ---------------------------------------------------------------------------------------------
// Axis selection
// ---------------------------------------------------------------------------------------------

/**
 * A world-space axis at the bind pose, converted into a bone's parent frame — the axis a
 * premultiplied offset should rotate about.
 *
 * 🔴 THE RETURN IS NOT A ROTATION. It is the unit axis packed into a quaternion as
 * `(x, y, z, 0)`, because that is the return type this module's contract specifies while
 * `offsetBoneTrack` wants the axis itself. Feed it straight to `offsetBoneTrack` (which accepts
 * either form) or unpack it with `axisOf`. Do NOT `premultiply` it onto anything — a quaternion
 * with `w = 0` is a 180° flip, which is at least loudly wrong rather than quietly wrong.
 *
 * ⚠️ Why the conversion is needed at all: Mixamo rest orientations are not world-aligned and differ
 * per rig, so a hardcoded local axis such as `(0, 0, 1)` tilts a different way on every subject.
 * Choosing the axis in world space at the BIND pose and converting it once into the parent's frame
 * makes "abduct the arm" mean the same anatomical thing everywhere — and because the offset is
 * premultiplied, the parent's frame is the frame the offset actually acts in.
 *
 * Returns `null` for a missing bone, a root bone with no parent, or a degenerate axis.
 */
export function boneParentAxis(
  skeleton: THREE.Skeleton,
  boneName: string,
  worldAxis: THREE.Vector3
): THREE.Quaternion | null {
  if (worldAxis.lengthSq() < EPSILON) return null;

  const index = boneIndexMap(skeleton);
  const self = index.get(boneName);
  if (self === undefined) return null;

  const parent = skeleton.bones[self].parent;
  if (!parent) return null;

  const frame = new THREE.Quaternion();
  const parentIndex = index.get(parent.name);
  if (parentIndex !== undefined && skeleton.bones[parentIndex] === parent) {
    bindRotation(skeleton, parentIndex, frame);
  } else {
    // The parent is not itself a skinning joint — typically the Armature/Scene node above Hips, for
    // which no `boneInverses` entry exists. ⚠️ This branch reads the LIVE scene graph, so it is
    // only correct with the model at rest. It never fires for arms or legs, whose parents are
    // always joints; if you find yourself here for a limb, the skeleton is not the one you think.
    parent.getWorldQuaternion(frame);
  }

  const axis = worldAxis.clone().normalize().applyQuaternion(frame.invert());
  if (axis.lengthSq() < EPSILON) return null;
  axis.normalize();
  return new THREE.Quaternion(axis.x, axis.y, axis.z, 0);
}

/** Unpack what `boneParentAxis` returns. */
export function axisOf(packed: THREE.Quaternion): THREE.Vector3 {
  return new THREE.Vector3(packed.x, packed.y, packed.z);
}

// ---------------------------------------------------------------------------------------------
// The edit
// ---------------------------------------------------------------------------------------------

const _axis = /*@__PURE__*/ new THREE.Vector3();
const _full = /*@__PURE__*/ new THREE.Quaternion();
const _stepped = /*@__PURE__*/ new THREE.Quaternion();
const _key = /*@__PURE__*/ new THREE.Quaternion();

/**
 * Add a constant rotation to every key of one bone's quaternion track, optionally weighted per key.
 *
 * `weights` is the whole extension point: `w ≡ 1` (pass `null`) is the global slider, a
 * `rampWeights` output limits the edit to a range without a pop, and an `envelopeFrom` output
 * limits it to the frames that actually collide. All three go down this identical path — there is
 * no separate "ranged" or "collision" code to keep in sync.
 *
 * ⚠️ `weights` is interpreted as a curve over the clip's DURATION sampled on a uniform grid, not as
 * a per-key array — unless its length happens to equal the track's key count, in which case it is
 * indexed directly (same answer when the clip is uniformly sampled, but exact). glTF lets each node
 * carry its own sampler input, so index alignment across tracks is an assumption, not a guarantee.
 *
 * `degrees` is signed and rotates about `axis` by the right-hand rule.
 *
 * Returns `false` when the bone has no rotation track — an ordinary "not present", not an error.
 *
 * 🔴 The caller must re-drive the mixer afterwards for the change to appear: `mixer.update(0)` to
 * re-evaluate where it is, `mixer.setTime(t)` to scrub.
 */
export function offsetBoneTrack(
  clip: THREE.AnimationClip,
  snapshot: ClipSnapshot,
  boneName: string,
  axis: THREE.Vector3 | THREE.Quaternion,
  degrees: number,
  weights?: Float32Array | null
): boolean {
  const trackName = `${boneName}.quaternion`;
  const track = clip.tracks.find((t) => t.name === trackName);
  if (!track) return false;

  const base = snapshot.values.get(trackName);
  if (!base) {
    throw new Error(
      `No pristine copy of "${trackName}". The snapshot was taken from a different clip ` +
        `("${snapshot.clipName}") or before this track existed.`
    );
  }
  if (base.length !== track.values.length) {
    throw new Error(
      `Track "${trackName}" has been resampled since the snapshot was taken. Reload the model.`
    );
  }

  const keys = track.times.length;
  const stride = track.getValueSize();
  const cubic = isCubicSpline(track);
  const expected = cubic ? 12 : 4;
  if (stride !== expected) {
    throw new Error(
      `Track "${trackName}" stores ${stride} floats per key; a ` +
        `${cubic ? 'CUBICSPLINE' : 'linear'} quaternion track must store ${expected}. ` +
        `This clip is not something this repair understands.`
    );
  }

  // 🔴 Recompute from pristine. Everything below writes ON TOP of the delivered values, so a
  // weight of 0 — or a degrees of 0 — leaves exactly what was delivered, and the slider is
  // two-way. This memcpy is also what makes the CUBICSPLINE tangent handling safe: untouched
  // keys keep their delivered tangents byte for byte.
  track.values.set(base);

  if (axis instanceof THREE.Quaternion) {
    _axis.set(axis.x, axis.y, axis.z);
  } else {
    _axis.copy(axis);
  }
  if (_axis.lengthSq() < EPSILON) return true;
  _axis.normalize();

  const radians = THREE.MathUtils.degToRad(degrees);
  if (radians === 0) return true;
  // ⚠️ `setFromAxisAngle` assumes a unit axis and does not check; the normalize above is not
  // optional. And keep |degrees| well under 180 — the slerp below always takes the short way, so a
  // larger angle silently comes back as its complement.
  _full.setFromAxisAngle(_axis, radians);

  const duration = clip.duration > 0 ? clip.duration : track.times[keys - 1] || 0;
  const direct = weights != null && weights.length === keys;
  const vertexOffset = cubic ? 4 : 0;

  for (let i = 0; i < keys; i++) {
    const w = weightAt(weights, direct, i, track.times[i], duration);
    if (w <= 0) continue;

    // Scaling the offset by slerping out of the identity keeps every intermediate a valid unit
    // rotation about the same axis. Scaling `degrees` and rebuilding would be equivalent here but
    // costs a trig pair per key.
    const offset = w >= 1 ? _full : _stepped.identity().slerp(_full, w);

    const at = i * stride + vertexOffset;
    _key.fromArray(track.values, at).premultiply(offset).normalize().toArray(track.values, at);

    if (cubic) {
      // 🔴 The tangents must move with the vertex. glTF CUBICSPLINE is a Hermite blend of the raw
      // 4-vectors followed by a normalize (see `GLTFCubicSplineQuaternionInterpolant`), and
      // left-multiplication by a unit quaternion is a LINEAR, orthogonal map on that 4-vector — so
      // it carries derivatives correctly and commutes with the normalize. Rotating the vertex and
      // leaving its tangents in the old frame is what actually produces wobble; rotating both is
      // exact for a constant offset.
      //
      // ⚠️ Do NOT normalize a tangent. It is a derivative, not an orientation; `premultiply` is a
      // bare Hamilton product and leaves its magnitude alone, which is what we want.
      const inAt = i * stride;
      _key.fromArray(track.values, inAt).premultiply(offset).toArray(track.values, inAt);
      const outAt = i * stride + 8;
      _key.fromArray(track.values, outAt).premultiply(offset).toArray(track.values, outAt);
    }
  }

  return true;
}

/**
 * The weight for one key.
 *
 * ⚠️ Sign structure is preserved by all of this: a baked quaternion track routinely flips sign
 * between adjacent keys (q and −q are the same rotation) and the interpolant relies on that to
 * pick the short arc. Since `qOff · (−q) = −(qOff · q)`, premultiplying — with a per-key weight or
 * not — cannot introduce a new flip. Post-multiplying has the same algebraic property but rotates
 * about the wrong axis, which is why it is still refused.
 */
function weightAt(
  weights: Float32Array | null | undefined,
  direct: boolean,
  key: number,
  time: number,
  duration: number
): number {
  if (weights == null || weights.length === 0) return 1;
  if (direct) return weights[key];
  if (weights.length === 1) return weights[0];
  const u = duration > 0 ? THREE.MathUtils.clamp(time / duration, 0, 1) : 0;
  const x = u * (weights.length - 1);
  const lo = Math.floor(x);
  const hi = Math.min(lo + 1, weights.length - 1);
  return weights[lo] + (weights[hi] - weights[lo]) * (x - lo);
}

// ---------------------------------------------------------------------------------------------
// Weight curves
// ---------------------------------------------------------------------------------------------

/**
 * 0 outside [start,end], 1 in the core, smoothstep over `shoulder` frames at each end.
 *
 * The ramp IS the answer to "how do I avoid a pop". A hard-edged range edit leaves a
 * first-derivative discontinuity at each boundary — the pose is continuous but its velocity is not,
 * and the eye reads that as a snap even at 1° of correction. Smoothstep is C¹, so the velocity is
 * continuous too.
 *
 * ⚠️ When the two shoulders overlap (`2·shoulder > end − start`) the `min` below produces a corner
 * at the crossover — the one case this function does not fully protect you from. Shrink the
 * shoulder or widen the range.
 */
export function rampWeights(
  frames: number,
  start: number,
  end: number,
  shoulder: number
): Float32Array {
  const count = Math.max(0, Math.floor(frames));
  const out = new Float32Array(count);
  if (count === 0) return out;

  const lo = Math.min(start, end);
  const hi = Math.max(start, end);
  const width = Math.max(0, shoulder);

  for (let i = 0; i < count; i++) {
    if (i < lo || i > hi) continue;
    const rise = width > 0 ? THREE.MathUtils.clamp((i - lo) / width, 0, 1) : 1;
    const fall = width > 0 ? THREE.MathUtils.clamp((hi - i) / width, 0, 1) : 1;
    const t = Math.min(rise, fall);
    out[i] = t * t * (3 - 2 * t);
  }
  return out;
}

/**
 * Turn detections into a per-key weight envelope, smoothed.
 *
 * 🔴 Dilate, THEN smooth. Feeding the raw per-frame depth straight in produces visible chatter —
 * the correction switching on and off across a contact. But a mean filter over an isolated depth
 * spike guts exactly the frame that needed the most help, so a plain smooth is worse than useless.
 * A max filter first widens the event to cover its own approach and recovery and keeps the peak at
 * 1; the triangular smooth after that removes the corners the max filter created.
 *
 * Depth is normalised against this limb's own worst frame, so the envelope is a SHAPE. How many
 * degrees it is worth is the caller's slider, not this function's business.
 *
 * ⚠️ `limb` is matched against `Intersection.limb`, which carries the SANITIZED name — pass
 * `sanitizeName('mixamorig:LeftForeArm')`, not the canonical string, or you get an all-zero
 * envelope and a correction that does nothing.
 */
export function envelopeFrom(
  intersections: Intersection[],
  frames: number,
  limb: string
): Float32Array {
  const count = Math.max(0, Math.floor(frames));
  const out = new Float32Array(count);
  if (count === 0) return out;

  const raw = new Float32Array(count);
  let peak = 0;
  for (const hit of intersections) {
    if (hit.limb !== limb) continue;
    if (hit.frame < 0 || hit.frame >= count) continue;
    if (hit.depth > raw[hit.frame]) raw[hit.frame] = hit.depth;
    if (hit.depth > peak) peak = hit.depth;
  }
  if (peak <= 0) return out;

  const dilated = new Float32Array(count);
  for (let i = 0; i < count; i++) {
    let best = 0;
    const from = Math.max(0, i - ENVELOPE_RADIUS);
    const to = Math.min(count - 1, i + ENVELOPE_RADIUS);
    for (let j = from; j <= to; j++) if (raw[j] > best) best = raw[j];
    dilated[i] = best;
  }

  for (let i = 0; i < count; i++) {
    let sum = 0;
    let total = 0;
    for (let k = -ENVELOPE_RADIUS; k <= ENVELOPE_RADIUS; k++) {
      const j = i + k;
      if (j < 0 || j >= count) continue;
      // Triangular kernel. Renormalising by the weights actually used means the ends of the clip
      // are averaged over a shorter window rather than being faded towards zero by the edge.
      const kw = 1 - Math.abs(k) / (ENVELOPE_RADIUS + 1);
      sum += dilated[j] * kw;
      total += kw;
    }
    out[i] = total > 0 ? THREE.MathUtils.clamp(sum / total / peak, 0, 1) : 0;
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// The product control
// ---------------------------------------------------------------------------------------------

/** The product control: one "clearance" value driving both arms (and optionally both upper legs),
 *  mirrored. */
export interface ClearanceOptions {
  /** Degrees of abduction. Positive opens the arms away from the torso. */
  arms?: number;
  legs?: number;
  weights?: Float32Array | null;
}

interface RigFrame {
  /** Bind-pose world direction from the character's right side to its left. */
  lateral: THREE.Vector3;
  /** Bind-pose world direction from hips to head. */
  up: THREE.Vector3;
  /** `lateral × up` — the character's forward, and the axis abduction rotates about. */
  forward: THREE.Vector3;
}

/**
 * Recover the character's own axes from its bind pose.
 *
 * ⚠️ Do not hardcode world +Z as "forward". Which way a delivered character faces depends on the
 * exporter and on whatever the acquisition step did to it; measuring instead of assuming costs six
 * bone lookups and removes an entire class of "the fix moves the arms backwards" bug.
 *
 * With `lateral` pointing at the character's LEFT and `up` at its head, `lateral × up` is forward,
 * and a POSITIVE rotation about forward swings the character's left side upward and outward. That
 * single fact is the whole mirroring rule: left limbs take `+degrees`, right limbs take
 * `−degrees`, and both move outward in world space. Applying the same sign to both would abduct one
 * arm and adduct the other — which looks, from the front, exactly like the rig is broken.
 */
function rigFrame(skeleton: THREE.Skeleton): RigFrame | null {
  const index = boneIndexMap(skeleton);
  const hips = indexOfCanonical(index, HIPS);
  const top =
    indexOfCanonical(index, HEAD) ??
    indexOfCanonical(index, NECK) ??
    indexOfCanonical(index, 'mixamorig:Spine2');
  const left = indexOfCanonical(index, ARM_PAIR[0]) ?? indexOfCanonical(index, LEG_PAIR[0]);
  const right = indexOfCanonical(index, ARM_PAIR[1]) ?? indexOfCanonical(index, LEG_PAIR[1]);
  if (hips === undefined || top === undefined || left === undefined || right === undefined) {
    return null;
  }

  const hipsAt = bindPosition(skeleton, hips, new THREE.Vector3());
  const topAt = bindPosition(skeleton, top, new THREE.Vector3());
  const leftAt = bindPosition(skeleton, left, new THREE.Vector3());
  const rightAt = bindPosition(skeleton, right, new THREE.Vector3());

  const up = topAt.sub(hipsAt);
  const lateral = leftAt.sub(rightAt);
  if (up.lengthSq() < EPSILON || lateral.lengthSq() < EPSILON) return null;
  up.normalize();
  lateral.normalize();

  const forward = new THREE.Vector3().crossVectors(lateral, up);
  // Degenerate only if the shoulders sit on the spine axis — a rig, not a pose, problem.
  if (forward.lengthSq() < 1e-8) return null;
  forward.normalize();

  return { lateral, up, forward };
}

/**
 * Drive both arms (and optionally both upper legs) apart by one signed angle.
 *
 * 🔴 Arms AND legs are rewritten on every call even when their angle is 0, and that is deliberate:
 * `offsetBoneTrack` restores from the snapshot before it applies anything, so touching a bone with
 * 0° is what puts it BACK. Skipping the zero case would strand whatever the previous drag left
 * behind, and the slider would only ever open the arms.
 *
 * `missing` therefore lists bones this rig does not have — including legs on an arms-only call —
 * which is honest rather than tidy.
 */
export function applyClearance(
  clip: THREE.AnimationClip,
  snapshot: ClipSnapshot,
  skeleton: THREE.Skeleton,
  options: ClearanceOptions
): { applied: string[]; missing: string[] } {
  const frame = rigFrame(skeleton);
  if (!frame) {
    throw new Error(
      'Cannot work out which way this character faces: it needs mixamorig:Hips, a head or neck, ' +
        'and a left/right pair of arms or legs. The rig is missing core joints.'
    );
  }

  const applied: string[] = [];
  const missing: string[] = [];
  const weights = options.weights ?? null;

  const groups: { pair: readonly [string, string]; degrees: number }[] = [
    { pair: ARM_PAIR, degrees: options.arms ?? 0 },
    { pair: LEG_PAIR, degrees: options.legs ?? 0 }
  ];

  for (const group of groups) {
    // [left, right] → [+1, −1]. See `rigFrame` for why the sign flips rather than the axis.
    group.pair.forEach((canonical, side) => {
      const sign = side === 0 ? 1 : -1;
      const boneName = sanitizeName(canonical);
      const axis = boneParentAxis(skeleton, boneName, frame.forward);
      if (!axis) {
        missing.push(canonical);
        return;
      }
      const ok = offsetBoneTrack(clip, snapshot, boneName, axis, sign * group.degrees, weights);
      (ok ? applied : missing).push(canonical);
    });
  }

  return { applied, missing };
}

// ---------------------------------------------------------------------------------------------
// Body proxy
// ---------------------------------------------------------------------------------------------

/** Body proxy for detection: one capsule per bone segment, radius from the skin. */
export interface BoneCapsule {
  /** Sanitized (three-side) bone name — what `skeleton.getBoneByName` answers to. */
  bone: string;
  /** 🔴 BIND-POSE space, not the current pose. `detectIntersections` animates these by
   *  `boneMatrixWorld · boneInverse`, exactly as the skinning shader does. Reading them as if they
   *  were live world positions gives you the T-pose on every frame and no collisions ever. */
  start: THREE.Vector3;
  end: THREE.Vector3;
  radius: number;
}

const _restPosition = /*@__PURE__*/ new THREE.Vector3();
const _skinIndex = /*@__PURE__*/ new THREE.Vector4();
const _skinWeight = /*@__PURE__*/ new THREE.Vector4();

interface Segment {
  start: THREE.Vector3;
  end: THREE.Vector3;
}

/**
 * One capsule per bone: the bone's own segment, fattened to the flesh it carries.
 *
 * The segment runs from the bone to its FURTHEST child joint. Averaging the children instead
 * collapses Hips — whose children go up the spine and down both legs — to a point. Choosing the
 * furthest keeps every chain bone honest and leaves Hips short, which is fine: the pelvis radius is
 * large, so a short capsule there is effectively the sphere it should be.
 *
 * Radius comes from the skin: every vertex is assigned to its single strongest bone and its
 * distance to that bone's segment goes into a histogram, from which `RADIUS_PERCENTILE` is read.
 * Two passes over the vertex buffer (one for the range, one for the bins) so the inner loop never
 * allocates and never grows an array — ~250k vertices × 2 is a few tens of milliseconds, once, at
 * load. Bones no vertex claims are dropped rather than given a guessed radius.
 */
export function buildCapsules(
  meshes: THREE.SkinnedMesh[],
  skeleton: THREE.Skeleton
): BoneCapsule[] {
  const bones = skeleton.bones;
  const count = bones.length;
  if (count === 0 || skeleton.boneInverses.length < count) return [];

  const bindAt: THREE.Vector3[] = [];
  for (let i = 0; i < count; i++) bindAt.push(bindPosition(skeleton, i, new THREE.Vector3()));

  const index = boneIndexMap(skeleton);
  const segments: Segment[] = [];
  for (let i = 0; i < count; i++) {
    segments.push(boneSegment(bones, bindAt, index, i));
  }

  // Pass 1 — how far the furthest vertex of each bone reaches.
  const reach = new Float32Array(count);
  forEachDominantInfluence(meshes, count, (bone, rest) => {
    const d = pointSegmentDistance(rest, segments[bone].start, segments[bone].end);
    if (d > reach[bone]) reach[bone] = d;
  });

  // Pass 2 — the distribution, binned.
  const bins = new Int32Array(count * RADIUS_BINS);
  const totals = new Int32Array(count);
  forEachDominantInfluence(meshes, count, (bone, rest) => {
    const span = reach[bone];
    if (span <= 0) return;
    const d = pointSegmentDistance(rest, segments[bone].start, segments[bone].end);
    const bin = Math.min(RADIUS_BINS - 1, Math.floor((d / span) * RADIUS_BINS));
    bins[bone * RADIUS_BINS + bin]++;
    totals[bone]++;
  });

  const capsules: BoneCapsule[] = [];
  for (let i = 0; i < count; i++) {
    if (totals[i] === 0 || reach[i] <= 0) continue;
    const target = totals[i] * RADIUS_PERCENTILE;
    let seen = 0;
    let bin = RADIUS_BINS - 1;
    for (let b = 0; b < RADIUS_BINS; b++) {
      seen += bins[i * RADIUS_BINS + b];
      if (seen >= target) {
        bin = b;
        break;
      }
    }
    // Upper edge of the chosen bin: a radius that excludes the percentile's own vertices would be
    // consistently a bin narrow.
    const radius = ((bin + 1) / RADIUS_BINS) * reach[i];
    if (radius <= 0) continue;
    capsules.push({
      bone: bones[i].name,
      start: segments[i].start.clone(),
      end: segments[i].end.clone(),
      radius
    });
  }
  return capsules;
}

function boneSegment(
  bones: THREE.Bone[],
  bindAt: THREE.Vector3[],
  index: Map<string, number>,
  i: number
): Segment {
  const start = bindAt[i].clone();
  let best = -1;
  let bestDistance = 0;
  for (const child of bones[i].children) {
    const childIndex = index.get(child.name);
    if (childIndex === undefined || bones[childIndex] !== child) continue;
    const d = bindAt[childIndex].distanceTo(bindAt[i]);
    if (d > bestDistance) {
      bestDistance = d;
      best = childIndex;
    }
  }
  if (best >= 0 && bestDistance > EPSILON) {
    return { start, end: bindAt[best].clone() };
  }

  // A leaf — a hand on a rig with no fingers, a toe tip. Stub it a quarter of its own bone length
  // further along the direction it came from, so the capsule has an orientation rather than
  // degenerating to a sphere at the wrist.
  const parent = bones[i].parent;
  const parentIndex = parent ? index.get(parent.name) : undefined;
  if (parentIndex !== undefined && bones[parentIndex] === parent) {
    const direction = bindAt[i].clone().sub(bindAt[parentIndex]);
    if (direction.lengthSq() > EPSILON) {
      return { start, end: start.clone().add(direction.multiplyScalar(0.25)) };
    }
  }
  return { start, end: start.clone() };
}

/**
 * Visit every vertex once, as (dominant bone, rest position).
 *
 * ⚠️ Dominant influence only — one bone per vertex. Counting a vertex against all four of its
 * influences double-counts the seams, where a vertex sits far from three of its bones by
 * construction, and drags every radius upward.
 *
 * Rest position is `bindMatrix · v`, the same space `boneInverses` inverted lives in. This is the
 * one space where bone and vertex are directly comparable; `stage.ts` has the full derivation.
 */
function forEachDominantInfluence(
  meshes: THREE.SkinnedMesh[],
  boneCount: number,
  visit: (bone: number, rest: THREE.Vector3) => void
): void {
  for (const mesh of meshes) {
    const geometry = mesh.geometry;
    const position = geometry.getAttribute('position') as THREE.BufferAttribute | undefined;
    const skinIndex = geometry.getAttribute('skinIndex') as THREE.BufferAttribute | undefined;
    const skinWeight = geometry.getAttribute('skinWeight') as THREE.BufferAttribute | undefined;
    if (!position || !skinIndex || !skinWeight) continue;

    for (let v = 0, n = position.count; v < n; v++) {
      _skinWeight.fromBufferAttribute(skinWeight, v);
      _skinIndex.fromBufferAttribute(skinIndex, v);

      let bone = -1;
      let best = 0;
      if (_skinWeight.x > best) {
        best = _skinWeight.x;
        bone = _skinIndex.x;
      }
      if (_skinWeight.y > best) {
        best = _skinWeight.y;
        bone = _skinIndex.y;
      }
      if (_skinWeight.z > best) {
        best = _skinWeight.z;
        bone = _skinIndex.z;
      }
      // The fourth slot deliberately does not update `best`: nothing reads it afterwards, and
      // eslint's `no-useless-assignment` is right to say so.
      if (_skinWeight.w > best) bone = _skinIndex.w;
      // An unskinned vertex (every weight zero) belongs to nothing and must not be attributed to
      // bone 0 — `stage.ts` documents that these exist and that they deform nothing.
      if (bone < 0 || bone >= boneCount || !Number.isInteger(bone)) continue;

      _restPosition.fromBufferAttribute(position, v).applyMatrix4(mesh.bindMatrix);
      visit(bone, _restPosition);
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

/** Which frames a limb is inside the body, and how deep. */
export interface Intersection {
  frame: number;
  /** Sanitized bone name of the offending limb. */
  limb: string;
  /** Sanitized bone name of the body part it is inside. */
  body: string;
  /** Penetration in world units: `radiusLimb + radiusBody − closestApproach`. Always > 0. */
  depth: number;
}

const _skinTransform = /*@__PURE__*/ new THREE.Matrix4();

interface PosedCapsule {
  capsule: BoneCapsule;
  boneIndex: number;
  start: THREE.Vector3;
  end: THREE.Vector3;
}

/**
 * Sweep the clip and report every frame where a limb capsule is inside a body capsule.
 *
 * ~6 limbs × 6 body parts × 120 frames ≈ 4k segment-segment tests, each a couple of dozen flops.
 * Microseconds. Detection on its own is a shippable feature — "your clip has 14 bad frames,
 * here they are" is worth more than a silent fix — and it produces the envelope `offsetBoneTrack`
 * wants.
 *
 * 🔴 This DRIVES the mixer. `mixer.setTime` zeroes the mixer's time and EVERY action's time before
 * it updates, so any other clip playing on this mixer gets rewound underneath you. Run detection on
 * a mixer that has only this clip. The mixer's own time and this action's state are saved and put
 * back; other actions are not, because the mixer does not expose them.
 *
 * ⚠️ Capsules are a proxy. A frame where the arm brushes the ribs is not a defect, which is what
 * `CONTACT_SLACK` is for, and a hand passing through a skirt is not modelled at all. This finds the
 * gross failures — the forearm buried in the ribcage — and is not trying to do more.
 */
export function detectIntersections(
  clip: THREE.AnimationClip,
  mixer: THREE.AnimationMixer,
  root: THREE.Object3D,
  skeleton: THREE.Skeleton,
  capsules: BoneCapsule[]
): Intersection[] {
  const frames = clipFrameCount(clip);
  if (frames < 1 || capsules.length === 0) return [];

  const byName = new Map<string, BoneCapsule>();
  for (const capsule of capsules) byName.set(capsule.bone, capsule);
  const index = boneIndexMap(skeleton);

  const limbs = posedSet(LIMB_BONES, byName, index);
  const bodies = posedSet(BODY_BONES, byName, index);
  if (limbs.length === 0 || bodies.length === 0) return [];

  const action = mixer.existingAction(clip, root) ?? mixer.clipAction(clip, root);
  if (!action) return [];

  const savedTime = mixer.time;
  const wasRunning = action.isRunning();
  const savedEnabled = action.enabled;
  const savedWeight = action.weight;
  const savedPaused = action.paused;
  const savedMixerScale = mixer.timeScale;
  const savedActionScale = action.timeScale;

  // 🔴 `setTime(t)` does NOT land at t. It zeroes every action's time and then calls `update(t)`,
  // and `update` multiplies its delta by `mixer.timeScale` — and the action multiplies again by
  // its own `timeScale`. A viewer showing the clip at half speed would therefore have every
  // sample land at t/2, and the sweep would silently only cover the first half of the clip.
  mixer.timeScale = 1;
  action.timeScale = 1;
  // `reset()` rather than bare `play()`: it clears any fade or warp interpolant left over from a
  // crossfade, which would otherwise override the weight we set below and evaluate the clip at a
  // weight the caller cannot see. ⚠️ A fade in progress is therefore lost, not restored — one more
  // reason to run detection on a mixer that is doing nothing else.
  action.reset();
  action.weight = 1;
  action.play();

  const hits: Intersection[] = [];
  const duration = clip.duration > 0 ? clip.duration : 0;
  // 🔴 Never sample at exactly `duration`. `AnimationAction._updateTime` treats `time >= duration`
  // under the default LoopRepeat as a wrap and hands back time 0, so the last frame of the sweep
  // would silently be a duplicate of the FIRST frame — and a clip whose defect is in its final
  // frames would come back clean.
  const lastSample = duration > 0 ? duration * (1 - 1e-6) : 0;

  try {
    for (let f = 0; f < frames; f++) {
      const t = frames > 1 ? Math.min((f / (frames - 1)) * duration, lastSample) : 0;
      mixer.setTime(t);
      // The mixer writes into `bone.quaternion`; nothing has recomputed the world matrices yet.
      // Skip this and every frame reports the previous frame's geometry.
      root.updateMatrixWorld(true);

      poseAll(limbs, skeleton);
      poseAll(bodies, skeleton);

      for (const limb of limbs) {
        for (const body of bodies) {
          // A bone always overlaps its own parent's capsule — that is anatomy, not a defect.
          // Only DIRECT adjacency is excluded: an upper arm is a descendant of the spine, and
          // excluding all ancestors would throw away precisely the pairs this exists to find.
          if (adjacent(skeleton, limb.boneIndex, body.boneIndex)) continue;

          const reach = limb.capsule.radius + body.capsule.radius;
          const gap = segmentDistance(limb.start, limb.end, body.start, body.end);
          const depth = reach - gap;
          if (depth > reach * CONTACT_SLACK) {
            hits.push({ frame: f, limb: limb.capsule.bone, body: body.capsule.bone, depth });
          }
        }
      }
    }
  } finally {
    // Order matters: put the clock back while the time scales are still 1 and the action is still
    // playing at weight 1, so the model lands on the frame the caller was looking at. Restoring the
    // scales first would make this last `setTime` land somewhere else entirely.
    mixer.setTime(savedTime);
    action.timeScale = savedActionScale;
    action.weight = savedWeight;
    action.paused = savedPaused;
    action.enabled = savedEnabled;
    // ⚠️ If nothing was playing when we arrived, we stop again — which leaves the model wherever
    // the last sampled frame put it. Call `skeleton.pose()` if the caller needs the bind pose back.
    if (!wasRunning) action.stop();
    mixer.timeScale = savedMixerScale;
    root.updateMatrixWorld(true);
  }

  return hits;
}

function posedSet(
  names: readonly string[],
  byName: Map<string, BoneCapsule>,
  index: Map<string, number>
): PosedCapsule[] {
  const out: PosedCapsule[] = [];
  for (const canonical of names) {
    const sanitized = sanitizeName(canonical);
    const capsule = byName.get(sanitized);
    const boneIndex = index.get(sanitized);
    if (!capsule || boneIndex === undefined) continue;
    out.push({ capsule, boneIndex, start: new THREE.Vector3(), end: new THREE.Vector3() });
  }
  return out;
}

/**
 * Move a capsule from bind space into the current pose.
 *
 * `boneMatrixWorld · boneInverse` is exactly the matrix the skinning shader applies to a
 * bind-space vertex, so a capsule built at the bind pose follows the flesh rather than sliding off
 * it. 🔴 `bindMatrix` does NOT appear here — the capsule endpoints were already built in
 * post-`bindMatrix` space in `buildCapsules`, and applying it twice shrinks the whole body toward
 * the origin on any model whose mesh node is not at identity.
 */
function poseAll(set: PosedCapsule[], skeleton: THREE.Skeleton): void {
  for (const posed of set) {
    _skinTransform.multiplyMatrices(
      skeleton.bones[posed.boneIndex].matrixWorld,
      skeleton.boneInverses[posed.boneIndex]
    );
    posed.start.copy(posed.capsule.start).applyMatrix4(_skinTransform);
    posed.end.copy(posed.capsule.end).applyMatrix4(_skinTransform);
  }
}

function adjacent(skeleton: THREE.Skeleton, a: number, b: number): boolean {
  if (a === b) return true;
  const boneA = skeleton.bones[a];
  const boneB = skeleton.bones[b];
  return boneA.parent === boneB || boneB.parent === boneA;
}

// ---------------------------------------------------------------------------------------------
// Geometry primitives
//
// `three/addons/math/Capsule.js` has `intersectsBox` and nothing else, so these are ours. Ericson,
// Real-Time Collision Detection §5.1.9 — the clamped variant, which is the one that stays correct
// when the segments are parallel or when either degenerates to a point (both of which happen here:
// a leaf-bone stub can be very short and two spine bones can be exactly colinear).
// ---------------------------------------------------------------------------------------------

const _d1 = /*@__PURE__*/ new THREE.Vector3();
const _d2 = /*@__PURE__*/ new THREE.Vector3();
const _r = /*@__PURE__*/ new THREE.Vector3();
const _c1 = /*@__PURE__*/ new THREE.Vector3();
const _c2 = /*@__PURE__*/ new THREE.Vector3();

/** Closest approach between segment `p1→q1` and segment `p2→q2`. */
function segmentDistance(
  p1: THREE.Vector3,
  q1: THREE.Vector3,
  p2: THREE.Vector3,
  q2: THREE.Vector3
): number {
  _d1.subVectors(q1, p1);
  _d2.subVectors(q2, p2);
  _r.subVectors(p1, p2);

  const a = _d1.dot(_d1);
  const e = _d2.dot(_d2);
  const f = _d2.dot(_r);

  let s = 0;
  let t = 0;

  if (a <= EPSILON && e <= EPSILON) return _r.length();

  if (a <= EPSILON) {
    t = THREE.MathUtils.clamp(f / e, 0, 1);
  } else {
    const c = _d1.dot(_r);
    if (e <= EPSILON) {
      s = THREE.MathUtils.clamp(-c / a, 0, 1);
    } else {
      const b = _d1.dot(_d2);
      const denom = a * e - b * b;
      // denom === 0 means parallel: any s works, so pick the start and let t follow.
      s = denom !== 0 ? THREE.MathUtils.clamp((b * f - c * e) / denom, 0, 1) : 0;
      t = (b * s + f) / e;
      // t fell outside its segment, so re-solve s against the clamped endpoint rather than
      // clamping t alone — clamping both independently gives a point pair that is not the
      // closest one, and the error shows up as missed collisions at limb extremes.
      if (t < 0) {
        t = 0;
        s = THREE.MathUtils.clamp(-c / a, 0, 1);
      } else if (t > 1) {
        t = 1;
        s = THREE.MathUtils.clamp((b - c) / a, 0, 1);
      }
    }
  }

  _c1.copy(p1).addScaledVector(_d1, s);
  _c2.copy(p2).addScaledVector(_d2, t);
  return _c1.distanceTo(_c2);
}

const _pd = /*@__PURE__*/ new THREE.Vector3();
const _pv = /*@__PURE__*/ new THREE.Vector3();

/** Distance from a point to segment `a→b`. */
function pointSegmentDistance(point: THREE.Vector3, a: THREE.Vector3, b: THREE.Vector3): number {
  _pd.subVectors(b, a);
  const lengthSq = _pd.lengthSq();
  if (lengthSq <= EPSILON) return point.distanceTo(a);
  _pv.subVectors(point, a);
  const t = THREE.MathUtils.clamp(_pv.dot(_pd) / lengthSq, 0, 1);
  return _pv.sub(_pd.multiplyScalar(t)).length();
}
