/*
 * Mirroring rig work across the character's own symmetry plane.
 *
 * # Why this exists
 *
 * The 22 core slots in [`MIXAMO_CORE`] are 6 mid-line joints and **8 Left/Right pairs**. Joint
 * repairs on a character out of the image-to-3D pipeline are almost always symmetric: if the left
 * elbow sits two centimetres inside the arm, the right one does too, by the same amount, because
 * the defect came out of the same fit. Doing that fix twice by eye produces two subtly different
 * elbows, and the asymmetry it introduces is the kind that only shows up once the character walks.
 *
 * # What this module does and does NOT do
 *
 * It computes **what** to apply. It never touches the scene graph — no `bone.position` writes, no
 * `updateMatrixWorld`, no re-bind. [`moveJointWorld`] in `stage.ts` is what applies a joint move
 * (and re-binds, which is the half that keeps the mesh still); the caller drives it twice, for the
 * two bones this module names, and pushes the pair as ONE undo entry.
 *
 * Even the measurement is non-mutating: world positions are composed here from each bone's local
 * transform up the parent chain rather than read through `getWorldPosition`, which would write the
 * cached matrices of an object this module was only asked to look at.
 *
 * ⚠️ Positions are read from the CURRENT scene graph unless a `THREE.Skeleton` is passed, in which
 * case they come from `boneInverses` — the bind pose, immune to a clip that happens to be playing.
 * The rig tier the workbench opens has no clips, so both agree there; a take does not, and the
 * skeleton argument is the honest reading for one.
 */

import * as THREE from 'three';

import { MIXAMO_CORE, sanitizeName } from './stage';

/**
 * The mirror plane's normal.
 *
 * Characters from this pipeline face ±Z with X across the body, so `x` is what
 * [`detectMirrorAxis`] returns for every rig seen so far — but nothing here assumes it. The axis is
 * derived from where the paired core joints actually sit, and a rig exported in another convention
 * gets the right answer instead of a confidently wrong one.
 */
export type MirrorAxis = 'x' | 'y' | 'z';

/** A mirror plane: the axis it is normal to, and where it sits along that axis in world units. */
export interface MirrorPlane {
  axis: MirrorAxis;
  offset: number;
}

export const MIRROR_AXES: readonly MirrorAxis[] = ['x', 'y', 'z'];

const AXIS_INDEX: Record<MirrorAxis, 0 | 1 | 2> = { x: 0, y: 1, z: 2 };

// ── the name table ───────────────────────────────────────────────────────────

/**
 * The 8 sided core pairs, in CANONICAL (colon-bearing) form, derived from [`MIXAMO_CORE`] itself
 * rather than retyped — so a slot added to or removed from that list cannot leave a stale pair here.
 *
 * The `:Left` → `:Right` rewrite is safe *only* because it runs over that closed 22-name table and
 * every result is checked back against it. It is not a general rule; see [`mirrorBoneName`] for what
 * happens off the table.
 */
export const CORE_PAIRS: readonly { left: string; right: string }[] = MIXAMO_CORE.filter((name) =>
  name.startsWith('mixamorig:Left')
)
  .map((left) => ({ left, right: left.replace('mixamorig:Left', 'mixamorig:Right') }))
  .filter((pair) => (MIXAMO_CORE as readonly string[]).includes(pair.right));

/**
 * 🔴 Keyed by SANITIZED name, valued in CANONICAL form.
 *
 * `GLTFLoader` deletes `:` from every node name (see [`sanitizeName`]), so `mixamorig:LeftArm` is
 * `mixamorigLeftArm` on a `THREE.Bone`. Keying this map on the canonical string would match no bone
 * on any rig this pipeline has ever produced — the exact bug `coreCoverage` shipped once, where
 * every core slot silently reported missing.
 */
const CORE_MIRROR = new Map<string, string>(
  CORE_PAIRS.flatMap((pair) => [
    [sanitizeName(pair.left), pair.right] as [string, string],
    [sanitizeName(pair.right), pair.left] as [string, string]
  ])
);

/**
 * The other side of a core slot, or `null` when the slot has no side (`Hips`, `Spine`, `Head`, …)
 * or is not a core slot at all.
 *
 * Forgiving on the way in — canonical or sanitized both work, because the input is run through
 * [`sanitizeName`] before the lookup. Always CANONICAL on the way out, which is the form the file
 * stores, the form the engine's name-based retarget matches, and the form a human reads.
 */
export function mirrorCoreName(core: string): string | null {
  return CORE_MIRROR.get(sanitizeName(core)) ?? null;
}

/**
 * Side tokens for rigs that are not Mixamo-named, and the boundary rules that keep them honest.
 *
 * 🔴 A plain `name.replace('Left', 'Right')` is a trap. `LeftoverBone` becomes `RightoverBone`,
 * `Bright_Eye` becomes `Bleft_Eye`, and either one pairs two bones that have nothing to do with each
 * other — which then get moved in opposite directions on the next mirrored edit. So a token only
 * counts when it is bounded on BOTH sides:
 *
 *   - before it: the start of the name, a separator, or a lowercase letter/digit (the camelCase
 *     hinge in `mixamorigLeftArm`). An uppercase letter before it means the token is a tail of a
 *     longer word — `Bright` — and is rejected.
 *   - after it: the end of the name, a separator, or an uppercase letter/digit. A lowercase letter
 *     after it means the token is a head of a longer word — `Leftover` — and is rejected.
 *
 * ⚠️ Known failure mode, accepted: an all-lowercase compound with a real camel hinge fools this the
 * way it fools a human — `copyright_bone` reads as `copy` + `Right`, and a rig carrying both it and
 * `copyleft_bone` would pair them. That needs two contrived names in one skeleton, and
 * [`mirrorBone`] still requires the partner to exist before it returns anything.
 */
const SIDE_TOKENS = new Map<string, string>([
  ['Left', 'Right'],
  ['Right', 'Left'],
  ['left', 'right'],
  ['right', 'left'],
  ['LEFT', 'RIGHT'],
  ['RIGHT', 'LEFT']
]);

const TOKEN_RE = /Left|Right|left|right|LEFT|RIGHT/g;

/**
 * `L`/`R` as a whole segment: `Bip01_L_Arm`, `arm_r`. Both sides must be a separator or a string
 * edge — deliberately stricter than the word tokens.
 *
 * ⚠️ `LArm` is NOT matched, and that is on purpose: a leading capital L is indistinguishable from a
 * bone whose name simply starts with L, and pairing `LArm` with `RArm` would also pair `Lamp` with
 * `Ramp`. Rigs using that convention get no partner rather than a wrong one.
 *
 * ⚠️ `sanitizeName` DELETES `. : [ ] /` rather than replacing them, so a file's `L.arm` arrives as
 * `Larm` with the boundary already destroyed. Only `_`, `-` and whitespace (rewritten to `_`)
 * survive as separators, which is why they are the whole separator class here.
 */
const LETTER_RE = /(^|[_-])([LRlr])(?=[_-]|$)/g;

const LETTER_MIRROR = new Map<string, string>([
  ['L', 'R'],
  ['R', 'L'],
  ['l', 'r'],
  ['r', 'l']
]);

function boundedBefore(name: string, start: number): boolean {
  if (start === 0) return true;
  return /[a-z0-9_-]/.test(name[start - 1]);
}

function boundedAfter(name: string, end: number): boolean {
  if (end === name.length) return true;
  return /[A-Z0-9_-]/.test(name[end]);
}

/**
 * The name of the bone on the other side, in THREE space (sanitized in, sanitized out).
 *
 * Two tiers, and the tier matters to the caller:
 *
 *   1. **the core table** — one of the 8 known pairs. Exact, closed, and the only tier that can be
 *      trusted on its own.
 *   2. **token patterns** — `Left`/`Right`, `L`/`R` as a bounded segment. A guess. It is what makes
 *      the feature work on the hand-named bones hanging off a core skeleton (`Left_Skirt_02`), and
 *      it is why [`mirrorBone`] refuses to return a partner that is not actually in the skeleton.
 *
 * Both tiers rewrite EVERY bounded token in the name in a single pass, so `Left_Arm_Left_Twist`
 * mirrors whole, and a name carrying both sides at once cannot be double-swapped back to itself.
 */
export function mirrorBoneName(name: string): string | null {
  const three = sanitizeName(name);
  const core = CORE_MIRROR.get(three);
  if (core) return sanitizeName(core);

  let hits = 0;
  const worded = three.replace(TOKEN_RE, (token, offset: number) => {
    if (!boundedBefore(three, offset) || !boundedAfter(three, offset + token.length)) return token;
    hits++;
    return SIDE_TOKENS.get(token) ?? token;
  });
  if (hits > 0) return worded === three ? null : worded;

  const lettered = three.replace(
    LETTER_RE,
    (_match, lead: string, letter: string) => `${lead}${LETTER_MIRROR.get(letter) ?? letter}`
  );
  return lettered === three ? null : lettered;
}

/** Whether a name pairs through the closed 22-slot table or through the pattern guess. */
export type MirrorSource = 'core' | 'pattern';

/**
 * The bone on the other side of `bone`, or `null`.
 *
 * 🔴 The name is only a candidate. This resolves it against the skeleton and returns a bone only
 * when EXACTLY ONE bone answers to it and it is not `bone` itself — which is what stops a pattern
 * guess like `RightoverBone` from ever surfacing, and what stops a rig with duplicate node names
 * (legal in glTF, and `createUniqueName` only de-duplicates within one file) from resolving to an
 * arbitrary one of them.
 */
export function mirrorBone(bones: THREE.Bone[], bone: THREE.Bone): THREE.Bone | null {
  const want = mirrorBoneName(bone.name);
  if (!want) return null;
  const matches = bones.filter((b) => b !== bone && sanitizeName(b.name) === want);
  return matches.length === 1 ? matches[0] : null;
}

/** Which tier paired a bone, or `null` when nothing did. Same resolution rules as [`mirrorBone`]. */
export function mirrorSource(bones: THREE.Bone[], bone: THREE.Bone): MirrorSource | null {
  const partner = mirrorBone(bones, bone);
  if (!partner) return null;
  return CORE_MIRROR.has(sanitizeName(bone.name)) ? 'core' : 'pattern';
}

/**
 * Every mirrored pair in the skeleton, listed once (not twice).
 *
 * Core pairs first and in `MIXAMO_CORE` order, pattern pairs after in skeleton order, so a UI that
 * truncates the list keeps the trustworthy half.
 *
 * ⚠️ `left`/`right` is anatomically meaningful only for the core pairs. For a pattern pair it is
 * just "whichever came first in the skeleton" — nothing here knows which side `Bip01_L_Arm` is on,
 * and nothing downstream needs to: a reflection is symmetric in its two arguments.
 */
export function mirrorPairs(
  bones: THREE.Bone[]
): { left: THREE.Bone; right: THREE.Bone; source: MirrorSource }[] {
  const seen = new Set<THREE.Bone>();
  const core: { left: THREE.Bone; right: THREE.Bone; source: MirrorSource }[] = [];
  const pattern: { left: THREE.Bone; right: THREE.Bone; source: MirrorSource }[] = [];

  const byName = new Map<string, THREE.Bone | null>();
  for (const bone of bones) {
    const key = sanitizeName(bone.name);
    // A duplicate name is unresolvable, so poison the slot rather than pick a winner — the same
    // rule `mirrorBone` enforces with its "exactly one match" test.
    byName.set(key, byName.has(key) ? null : bone);
  }

  for (const pair of CORE_PAIRS) {
    const left = byName.get(sanitizeName(pair.left));
    const right = byName.get(sanitizeName(pair.right));
    if (!left || !right) continue;
    seen.add(left);
    seen.add(right);
    core.push({ left, right, source: 'core' });
  }

  for (const bone of bones) {
    if (seen.has(bone)) continue;
    const partner = mirrorBone(bones, bone);
    if (!partner || seen.has(partner)) continue;
    seen.add(bone);
    seen.add(partner);
    pattern.push({ left: bone, right: partner, source: 'pattern' });
  }

  return [...core, ...pattern];
}

// ── geometry ─────────────────────────────────────────────────────────────────

/**
 * A bone's world position, composed here rather than read.
 *
 * `Object3D.getWorldPosition` calls `updateWorldMatrix`, which WRITES the cached matrices of the
 * bone and every ancestor. Harmless in isolation, but this module's whole contract is that it looks
 * and does not touch, and a diagnostic that quietly mutates the object it measures is the kind of
 * thing that only bites once the caller starts interleaving it with an edit.
 */
function composedWorld(object: THREE.Object3D, out: THREE.Vector3): THREE.Vector3 {
  const chain: THREE.Object3D[] = [];
  let walk: THREE.Object3D | null = object;
  while (walk) {
    chain.push(walk);
    walk = walk.parent;
  }
  const world = new THREE.Matrix4();
  const local = new THREE.Matrix4();
  for (let i = chain.length - 1; i >= 0; i--) {
    const node = chain[i];
    world.multiply(local.compose(node.position, node.quaternion, node.scale));
  }
  return out.setFromMatrixPosition(world);
}

/**
 * Where every bone sits, in one space.
 *
 * With a `skeleton`, positions come from `boneInverses[i]⁻¹` — the BIND pose, which is what the
 * skin was built against and what a joint repair is measured in. Without one they come from the
 * live local transforms, which equal the bind pose on a rig tier and are the animated pose on a
 * take. Bones absent from the skeleton fall back to the live path.
 */
export function jointPositions(
  bones: THREE.Bone[],
  skeleton?: THREE.Skeleton | null
): Map<THREE.Bone, THREE.Vector3> {
  const bind = new Map<THREE.Bone, THREE.Vector3>();
  if (skeleton) {
    skeleton.bones.forEach((bone, i) => {
      const inverse = skeleton.boneInverses[i] as THREE.Matrix4 | undefined;
      if (!inverse) return;
      bind.set(
        bone,
        new THREE.Vector3().setFromMatrixPosition(new THREE.Matrix4().copy(inverse).invert())
      );
    });
  }
  const out = new Map<THREE.Bone, THREE.Vector3>();
  for (const bone of bones) {
    out.set(bone, bind.get(bone)?.clone() ?? composedWorld(bone, new THREE.Vector3()));
  }
  return out;
}

/** The skeleton's largest bounding-box dimension, so a tolerance means the same at any scale. */
export function skeletonSpan(positions: Iterable<THREE.Vector3>): number {
  const box = new THREE.Box3();
  for (const p of positions) box.expandByPoint(p);
  const size = box.getSize(new THREE.Vector3());
  return Math.max(size.x, size.y, size.z);
}

/**
 * Mirror a world-space delta across the plane.
 *
 * 🔴 **What this promises:** the reflected DIRECTION. Move the left elbow 2 cm outward and this
 * gives the vector that moves the right elbow 2 cm outward — same distance, same anatomical
 * direction, opposite world direction across the plane. A pure reflection of a direction is
 * independent of where the plane sits, which is why this takes an axis and not a [`MirrorPlane`],
 * and why it cannot be thrown off by a character that is not centred on the origin.
 *
 * ⚠️ **Where it stops being enough.** It promises the same *repair*, not the same *result*. If the
 * two bones are not already exact reflections of each other — and on a rig out of this pipeline they
 * usually are not, to a millimetre or two — then applying this leaves that pre-existing asymmetry
 * exactly as it was. That is the right default: the asymmetry may be the character, and silently
 * flattening it is a bigger edit than the one that was asked for. When the intent really is "make
 * the two sides match", that is [`mirrorJointMove`]'s `'snap'` mode, which is a different and
 * larger claim and is priced as one — it reports the extra distance it is about to move.
 *
 * ⚠️ It is a reflection of a POSITION delta only. A rotation does not mirror by negating one
 * component (the handedness flips), and nothing here mirrors one — `moveJointWorld` translates, so
 * that gap is not currently reachable. Do not extend this to quaternions without revisiting it.
 */
export function mirrorDelta(delta: THREE.Vector3, axis: MirrorAxis): THREE.Vector3 {
  const out = delta.clone();
  const i = AXIS_INDEX[axis];
  out.setComponent(i, -out.getComponent(i));
  return out;
}

/** Reflect a world POINT through the plane. Unlike a delta, this does depend on the offset. */
export function reflectPoint(point: THREE.Vector3, plane: MirrorPlane): THREE.Vector3 {
  const out = point.clone();
  const i = AXIS_INDEX[plane.axis];
  out.setComponent(i, 2 * plane.offset - out.getComponent(i));
  return out;
}

// ── deriving the axis ────────────────────────────────────────────────────────

/**
 * A pair must be separated by at least this fraction of the skeleton's span along an axis before it
 * counts as full evidence for that axis. Two joints 0.1% of the body apart say nothing about which
 * way the body is mirrored, and without this floor a rig whose sides had collapsed together would
 * still score 1.0 on whichever axis its rounding error happened to favour.
 */
const SEPARATION_FLOOR = 0.02;

/**
 * The confidence below which a skeleton does not read as a mirror at all, and every feature built
 * on pairing sides has to refuse rather than guess.
 *
 * Exported because two callers gate on it — the workbench's mirrored editing and `chirality`'s
 * "is X really the across-the-body axis" check — and two copies of a threshold drift apart.
 */
export const MIRROR_MIN_CONFIDENCE = 0.6;

/** Below three pairs there is not enough of a skeleton to be sure, and the score is scaled down. */
const CONFIDENT_PAIRS = 3;

export interface AxisDetection extends MirrorPlane {
  /**
   * 0..1. Read it as "how much of a mirror this actually is", and refuse the whole feature below
   * ~0.6 rather than mirror an edit onto a bone that is not the other side of anything.
   */
  confidence: number;
  /** How many pairs the reading is built on. Zero means nothing paired — a non-humanoid rig. */
  pairs: number;
  /** Every axis's raw score, so a UI can say WHY it refused instead of just refusing. */
  scores: Record<MirrorAxis, number>;
}

/**
 * Derive the character's mirror axis from its own skeleton.
 *
 * Every mirrored pair gives one displacement `d = left − right`. The axis that best explains the
 * whole set wins, scored per axis as
 *
 *   `score = mean(alignment × separation) × signConsistency`
 *
 * - **alignment** `|d.a| / |d|` — how much of the pair's displacement lies along the candidate
 *   axis. 1.0 when the two joints differ in nothing else. This is the term that a character with
 *   one shoulder higher than the other erodes, gently and proportionally.
 * - **separation** `min(1, |d.a| / (0.02 × span))` — pairs that sit on top of each other along the
 *   candidate axis carry no evidence and must not be allowed to vote with a normalised direction.
 * - **signConsistency** `|Σ sign(d.a)| / n` — every left joint must be on the SAME side. This is
 *   what a mislabelled rig, or a set of pairs that are not really pairs, fails: hands on one side
 *   and feet on the other averages to zero however cleanly each pair lines up.
 *
 * The reported confidence is then the winner scaled by two doubts:
 *
 * - **margin** `(best − runnerUp) / best` — two axes explaining the data equally well means neither
 *   explains it. A clean humanoid scores ~1.0 on X and ~0.0 on the others, so this costs it
 *   nothing; a rig where the reading is a coin flip collapses to near zero.
 * - **sample** `min(1, pairs / 3)` — one pair is an anecdote.
 *
 * ⚠️ A skeleton with NO pairs returns `x` at confidence 0. The axis is a placeholder there, not an
 * answer, and the confidence is the part to read.
 */
export function detectMirrorAxis(
  bones: THREE.Bone[],
  skeleton?: THREE.Skeleton | null
): AxisDetection {
  const positions = jointPositions(bones, skeleton);
  const pairs = mirrorPairs(bones);
  const span = skeletonSpan(positions.values());
  const scores: Record<MirrorAxis, number> = { x: 0, y: 0, z: 0 };
  const midpoints: Record<MirrorAxis, number> = { x: 0, y: 0, z: 0 };

  const deltas = pairs
    .map((pair) => {
      const left = positions.get(pair.left);
      const right = positions.get(pair.right);
      return left && right
        ? { delta: left.clone().sub(right), mid: left.clone().add(right) }
        : null;
    })
    .filter((d): d is { delta: THREE.Vector3; mid: THREE.Vector3 } => d !== null);

  if (!deltas.length || span <= 0) {
    return { axis: 'x', offset: 0, confidence: 0, pairs: deltas.length, scores };
  }

  for (const axis of MIRROR_AXES) {
    const i = AXIS_INDEX[axis];
    let evidence = 0;
    let signs = 0;
    let mid = 0;
    for (const { delta, mid: sum } of deltas) {
      const along = delta.getComponent(i);
      const length = delta.length();
      const alignment = length > 0 ? Math.abs(along) / length : 0;
      const separation = Math.min(1, Math.abs(along) / (SEPARATION_FLOOR * span));
      evidence += alignment * separation;
      signs += Math.sign(along);
      mid += sum.getComponent(i) / 2;
    }
    const consistency = Math.abs(signs) / deltas.length;
    scores[axis] = (evidence / deltas.length) * consistency;
    midpoints[axis] = mid / deltas.length;
  }

  const ranked = [...MIRROR_AXES].sort((a, b) => scores[b] - scores[a]);
  const axis = ranked[0];
  const best = scores[axis];
  const margin = best > 0 ? (best - scores[ranked[1]]) / best : 0;
  const sample = Math.min(1, deltas.length / CONFIDENT_PAIRS);

  return {
    axis,
    offset: midpoints[axis],
    confidence: best * margin * sample,
    pairs: deltas.length,
    scores
  };
}

/**
 * Where the plane sits along `axis`: the mean midpoint of every mirrored pair.
 *
 * Falls back to the centre of the skeleton's bounding box when nothing pairs — which is a guess, but
 * only [`reflectPoint`] and `'snap'` mode depend on the offset at all, and both are already gated on
 * a confident detection.
 */
export function mirrorPlaneOffset(
  bones: THREE.Bone[],
  axis: MirrorAxis,
  skeleton?: THREE.Skeleton | null
): number {
  const positions = jointPositions(bones, skeleton);
  const pairs = mirrorPairs(bones);
  const i = AXIS_INDEX[axis];
  let total = 0;
  let counted = 0;
  for (const pair of pairs) {
    const left = positions.get(pair.left);
    const right = positions.get(pair.right);
    if (!left || !right) continue;
    total += (left.getComponent(i) + right.getComponent(i)) / 2;
    counted++;
  }
  if (counted > 0) return total / counted;

  const box = new THREE.Box3();
  for (const p of positions.values()) box.expandByPoint(p);
  if (box.isEmpty()) return 0;
  return box.getCenter(new THREE.Vector3()).getComponent(i);
}

function toPlane(
  bones: THREE.Bone[],
  plane: MirrorAxis | MirrorPlane,
  skeleton?: THREE.Skeleton | null
): MirrorPlane {
  if (typeof plane !== 'string') return plane;
  return { axis: plane, offset: mirrorPlaneOffset(bones, plane, skeleton) };
}

// ── the asymmetry that is already there ──────────────────────────────────────

export interface SymmetryPair {
  /** CANONICAL for core slots, sanitized for pattern pairs — whatever the bone answers to. */
  left: string;
  right: string;
  source: MirrorSource;
  /** How far the right bone sits from the exact reflection of the left one, in world units. */
  offset: number;
  /** The same, as a fraction of the skeleton's span. Scale-free, so a threshold travels. */
  ratio: number;
}

export interface SymmetryReport {
  plane: MirrorPlane;
  /** Worst first — the list is a diagnostic, and the top of it is the finding. */
  pairs: SymmetryPair[];
  /** The largest `offset`, in world units. 0 when nothing paired. */
  worst: number;
  /** The largest `ratio`. This is the number worth putting a threshold on. */
  worstRatio: number;
  span: number;
  /** Core slots present on only one side — no partner to mirror onto, and a rig defect in itself. */
  unpaired: string[];
}

/**
 * How far the rig departs from being symmetric, pair by pair.
 *
 * Worth reading before any mirrored edit and worth reading on its own: a rig that is 4% of its own
 * height out of symmetry at the shoulders did not come out of the fit that way by accident, and
 * `'snap'` mode would silently move that shoulder a long way. It is also the cheapest check that the
 * detected axis is the right one — a correct axis makes these numbers small, a wrong one makes every
 * pair enormous.
 */
export function symmetryReport(
  bones: THREE.Bone[],
  plane: MirrorAxis | MirrorPlane,
  skeleton?: THREE.Skeleton | null
): SymmetryReport {
  const resolved = toPlane(bones, plane, skeleton);
  const positions = jointPositions(bones, skeleton);
  const span = skeletonSpan(positions.values());
  const found = mirrorPairs(bones);

  const canonical = new Map<string, string>(
    CORE_PAIRS.flatMap((pair) => [
      [sanitizeName(pair.left), pair.left] as [string, string],
      [sanitizeName(pair.right), pair.right] as [string, string]
    ])
  );
  const label = (bone: THREE.Bone) => canonical.get(sanitizeName(bone.name)) ?? bone.name;

  const pairs: SymmetryPair[] = [];
  for (const pair of found) {
    const left = positions.get(pair.left);
    const right = positions.get(pair.right);
    if (!left || !right) continue;
    const offset = reflectPoint(left, resolved).distanceTo(right);
    pairs.push({
      left: label(pair.left),
      right: label(pair.right),
      source: pair.source,
      offset,
      ratio: span > 0 ? offset / span : 0
    });
  }
  pairs.sort((a, b) => b.offset - a.offset);

  const onRig = new Set(bones.map((b) => sanitizeName(b.name)));
  const unpaired = CORE_PAIRS.flatMap((pair) => {
    const hasLeft = onRig.has(sanitizeName(pair.left));
    const hasRight = onRig.has(sanitizeName(pair.right));
    if (hasLeft === hasRight) return [];
    return [hasLeft ? pair.left : pair.right];
  });

  return {
    plane: resolved,
    pairs,
    worst: pairs.length ? pairs[0].offset : 0,
    worstRatio: pairs.length ? pairs[0].ratio : 0,
    span,
    unpaired
  };
}

// ── what to apply ────────────────────────────────────────────────────────────

/**
 * - `'delta'` — the same repair on the other side. Reflects the direction and preserves whatever
 *   asymmetry the rig already had. The default, and the only one that is a *small* edit.
 * - `'snap'` — put the partner at the exact reflection of where the moved joint ends up. Makes the
 *   two sides identical, and in doing so absorbs the pre-existing asymmetry into this one move.
 *   `residual` is 0 afterwards by construction; `extra` is what it cost.
 */
export type MirrorMode = 'delta' | 'snap';

export interface MirroredMove {
  bone: THREE.Bone;
  /** The world-space delta to hand to `moveJointWorld` for `bone`. */
  delta: THREE.Vector3;
  source: MirrorSource;
  /** How far the partner will sit from the exact reflection of the moved joint AFTER both moves. */
  residual: number;
  /** Signed: how much farther (or, when negative, less far) this delta travels than the plain
   *  reflected one. Always 0 in `'delta'` mode, where it IS the plain reflected one. */
  extra: number;
}

/**
 * The second half of a symmetric joint repair: which bone to move, and by how much.
 *
 * Computes only. The caller applies both moves with `moveJointWorld` — `keepChildren` the same for
 * both, or the two sides end up with different children — and pushes the pair as ONE undo entry, so
 * that a single Ctrl+Z puts the character back rather than leaving it half-mirrored.
 *
 * Returns `null` when the bone has no partner: a mid-line joint (`Hips`, `Spine`, `Neck`, `Head`)
 * has nothing to mirror onto, and neither does a side-named bone whose opposite number is missing
 * from the skeleton. Both are normal, and both mean "just move the one joint".
 *
 * ⚠️ `residual` is the number to surface. In `'delta'` mode it is the asymmetry that was already
 * there and is being carried forward untouched; if it is large, the two sides were never
 * reflections and "the same repair" may not mean what the user thinks it does.
 */
export function mirrorJointMove(
  bones: THREE.Bone[],
  bone: THREE.Bone,
  worldDelta: THREE.Vector3,
  plane: MirrorAxis | MirrorPlane,
  mode: MirrorMode = 'delta',
  skeleton?: THREE.Skeleton | null
): MirroredMove | null {
  const partner = mirrorBone(bones, bone);
  const source = mirrorSource(bones, bone);
  if (!partner || !source) return null;

  const resolved = toPlane(bones, plane, skeleton);
  const positions = jointPositions(bones, skeleton);
  const here = positions.get(bone);
  const there = positions.get(partner);
  if (!here || !there) return null;

  const reflected = mirrorDelta(worldDelta, resolved.axis);
  const target = reflectPoint(here.clone().add(worldDelta), resolved);

  if (mode === 'snap') {
    const delta = target.clone().sub(there);
    return {
      bone: partner,
      delta,
      source,
      residual: 0,
      extra: delta.length() - reflected.length()
    };
  }
  return {
    bone: partner,
    delta: reflected,
    source,
    residual: target.distanceTo(there.clone().add(reflected)),
    extra: 0
  };
}
