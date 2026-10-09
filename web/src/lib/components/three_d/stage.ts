/*
 * Browser-side rig surgery for Pixal3D, and the shared types the viewer and the editor talk over.
 *
 * # What this is for, and what it is NOT
 *
 * The engine ALREADY runs a skin-weight cleanup on every rig, verifies it against the shipped pose
 * gate, and keeps the original skin byte-for-byte when it cannot prove an improvement. So this is
 * the manual override for the cases that pass declined — not a replacement for it, and not the
 * first thing to reach for.
 *
 * The defect it exists for is the visible one: a handful of vertices bound to a joint half a body
 * away, which get flung across the character when that joint rotates. Measured on one subject:
 * **97 vertices out of 247,727 (0.039%)**, cleanly separated from every other vertex by distance.
 * That separation is why a distance threshold works at all and why the count is shown live — if
 * pruning at your threshold takes thousands of vertices, the threshold is wrong, not the rig.
 *
 * The third repair — **moving a joint** — is further down, with the reason it is two edits rather
 * than one and why the engine's retarget actually sees it.
 *
 * ⚠️ NOT fixable here: hands passing through the body during a walk. That is self-intersection in
 * the retarget, which transfers motion joint-by-joint with no mesh awareness. Different problem,
 * genuinely harder, and no amount of weight cleanup touches it.
 */

import * as THREE from 'three';

import type { GltfAssociation } from './glb-patch';
import type { ClipSnapshot } from './motion-repair';

/** What the viewer hands the editor once a model is on screen. */
export interface StageHandle {
  root: THREE.Object3D;
  skinnedMeshes: THREE.SkinnedMesh[];
  bones: THREE.Bone[];
  clips: string[];
  /** 🔴 The GLB's ORIGINAL bytes, kept verbatim. Saving patches these rather than re-exporting the
   *  scene — see `glb-patch.ts` for the two reasons that is not optional. */
  source: ArrayBuffer;
  /** `parser.associations`: each three primitive → `{ meshes, primitives }` in the source file. */
  associations: Map<object, GltfAssociation>;
  /** The live renderer. Needed because the texture repair builds its UV coverage mask by rendering
   *  the mesh into a render target in UV space — the only ground truth for which texels an island
   *  actually covers. */
  renderer: THREE.WebGLRenderer;
  /** The baked tracks, and the mixer playing them. Empty on a rig tier; populated on a take. */
  animationClips: THREE.AnimationClip[];
  mixer: THREE.AnimationMixer | null;
  skeleton: THREE.Skeleton | null;
  /** 🔴 Pristine copies of every clip's key values, taken BEFORE the first `clipAction`.
   *  `snapshotClip` also de-aliases tracks that share one `Float32Array` — `GLTFLoader` caches
   *  accessors by index, so two samplers over the same output accessor yield two tracks over one
   *  buffer, and an in-place edit to one would move an unrelated bone. Once an action exists its
   *  interpolant holds that array by reference, so the de-aliasing has to happen first. */
  snapshots: ClipSnapshot[];
  /** Re-upload changed skin attributes to the GPU and re-fit the skeleton overlay. */
  refresh: () => void;
}

/**
 * The 22 core slots the engine's namer fills, verbatim from `rig_bone_names.hpp`.
 *
 * 🔴 THE PREFIX IS PART OF THE NAME **IN THE FILE**. The C++ retarget maps joints by exact string
 * (`map_kind: "names"`), so `LeftArm` matches nothing — it has to be `mixamorig:LeftArm`.
 * `named_core: 21/22` means 21 of these are present, and each missing one is a joint no humanoid
 * clip will drive.
 *
 * ⚠️ BUT THESE STRINGS NEVER APPEAR ON A `THREE.Bone`. See [`sanitizeName`] — always compare
 * through it, never against `bone.name` directly.
 */
export const MIXAMO_CORE = [
  'mixamorig:Hips',
  'mixamorig:Spine',
  'mixamorig:Spine1',
  'mixamorig:Spine2',
  'mixamorig:Neck',
  'mixamorig:Head',
  'mixamorig:LeftShoulder',
  'mixamorig:LeftArm',
  'mixamorig:LeftForeArm',
  'mixamorig:LeftHand',
  'mixamorig:RightShoulder',
  'mixamorig:RightArm',
  'mixamorig:RightForeArm',
  'mixamorig:RightHand',
  'mixamorig:LeftUpLeg',
  'mixamorig:LeftLeg',
  'mixamorig:LeftFoot',
  'mixamorig:LeftToeBase',
  'mixamorig:RightUpLeg',
  'mixamorig:RightLeg',
  'mixamorig:RightFoot',
  'mixamorig:RightToeBase'
] as const;

/**
 * A glTF node name as three.js will have rewritten it.
 *
 * 🔴 `GLTFLoader.createUniqueName` runs every node name through
 * `PropertyBinding.sanitizeNodeName`, which is
 * `name.replace(/\s/g,'_').replace(/[[\]./:]/g,'')` — it **deletes** `[ ] . : /` rather than
 * escaping them, because those characters are the track-path grammar. Verified against the r185
 * sources, 2026-08-15.
 *
 * So the file's `mixamorig:LeftArm` is `mixamorigLeftArm` on a `THREE.Bone`, and any comparison
 * against the canonical names silently reports every core slot missing.
 *
 * ⚠️ `bone.userData.name` MAY carry the original — two separate reads of the r185 loader disagreed
 * about whether it is assigned, and it was not worth a third. Treat it as a nicety: read it with a
 * `?? bone.name` fallback and never depend on it. The canonical name a save writes comes from the
 * edit record, not from the bone.
 *
 * ⚠️ The consequence on the way OUT is worse and is why this editor must not save through
 * `GLTFExporter`: `processNode` writes `nodeDef.name = String(object.name)` — the sanitized one,
 * never `userData.name` — so a round trip strips the colons out of the FILE and the engine's
 * name-based retarget stops matching. Canonical names reach the file only through the byte-patch
 * save path.
 */
export function sanitizeName(name: string): string {
  return name.replace(/\s/g, '_').replace(/[[\]./:]/g, '');
}

export interface SkinReport {
  /** The model's largest bounding-box dimension, in bind space. Every ratio below is against it,
   *  so a threshold means the same thing on a 2-metre character and a 2-unit one. */
  referenceLength: number;
  vertices: number;
  /** Non-zero influences across every skinned mesh. */
  influences: number;
  /** The single most distant influence, as a fraction of `referenceLength`. */
  maxRatio: number;
  /** How many influences sit beyond each ratio — the distribution the threshold slider reads.
   *  A healthy rig collapses to zero well before 0.3; a leak shows as a flat tail. */
  tail: { ratio: number; influences: number; vertices: number }[];
  /** Per-bone worst offenders, most distant first. Named bones first because they are the ones a
   *  clip will actually drive. */
  bones: { index: number; name: string; influences: number; maxRatio: number }[];
  /**
   * Per input mesh, per vertex: the worst influence ratio at that vertex (0 if unskinned).
   * Same order and length as the `meshes` argument to `analyseSkin`.
   *
   * This is the same number the tail is built from, kept per vertex so the viewport can PAINT the
   * leak instead of tabulating it. 97 offenders in 247,727 is a row in a table; it is a handful of
   * red specks on one arm when it is drawn on the model.
   *
   * ⚠️ Indexed by VERTEX. Any edit that renumbers vertices — [`compactVertices`] is the only one —
   * invalidates it, and there is no length check downstream that would notice on its own.
   */
  vertexRatios: Float32Array[];
}

export interface PruneResult {
  prunedInfluences: number;
  verticesTouched: number;
  /** Vertices whose ONLY influences were beyond the threshold, and which were therefore left
   *  exactly as they were. 🔴 Zeroing these would unskin them — the one failure the engine itself
   *  refuses to ship, because an unskinned vertex deforms nothing. */
  verticesSpared: number;
  byBone: { name: string; influences: number }[];
}

interface MeshSkin {
  mesh: THREE.SkinnedMesh;
  skinIndex: THREE.BufferAttribute;
  skinWeight: THREE.BufferAttribute;
  position: THREE.BufferAttribute;
  bonePositions: THREE.Vector3[];
}

/**
 * Bone rest positions and vertex rest positions, in ONE space.
 *
 * glTF skinning is `Σ wᵢ · (boneWorldᵢ · boneInverseᵢ) · bindMatrix · v`. At rest every bone's world
 * matrix equals its bind world matrix, so the bracket is the identity and the rest position of a
 * vertex is simply `bindMatrix · v`. `boneInverses[i]` inverted gives the same space for the bone.
 * Comparing anything else — `bone.position`, live `matrixWorld` mid-animation — measures the pose,
 * not the binding.
 */
function meshSkin(mesh: THREE.SkinnedMesh): MeshSkin | null {
  const skinIndex = mesh.geometry.getAttribute('skinIndex') as THREE.BufferAttribute | undefined;
  const skinWeight = mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute | undefined;
  const position = mesh.geometry.getAttribute('position') as THREE.BufferAttribute | undefined;
  if (!skinIndex || !skinWeight || !position || !mesh.skeleton) return null;
  const bonePositions = mesh.skeleton.boneInverses.map((inverse) =>
    new THREE.Vector3().setFromMatrixPosition(new THREE.Matrix4().copy(inverse).invert())
  );
  return { mesh, skinIndex, skinWeight, position, bonePositions };
}

function restPosition(skin: MeshSkin, i: number, out: THREE.Vector3): THREE.Vector3 {
  return out.fromBufferAttribute(skin.position, i).applyMatrix4(skin.mesh.bindMatrix);
}

/**
 * ⚠️ `BufferGeometry.getAttribute` is declared as always returning an attribute, and at runtime it
 * returns `undefined` for one the geometry does not have. `meshSkin` above already works around
 * that with a cast per attribute; this is the same cast, named once, and it keeps the union honest
 * about `InterleavedBufferAttribute` — `GLTFLoader` produces those for tightly packed accessors and
 * they carry the same `getX`/`getY`/`getZ`/`getW` accessors.
 */
function optionalAttribute(
  geometry: THREE.BufferGeometry,
  name: string
): THREE.BufferAttribute | THREE.InterleavedBufferAttribute | undefined {
  return geometry.getAttribute(name) as
    THREE.BufferAttribute | THREE.InterleavedBufferAttribute | undefined;
}

const COMPONENTS = ['x', 'y', 'z', 'w'] as const;

function influenceAt(
  skin: MeshSkin,
  vertex: number,
  slot: number
): { bone: number; weight: number } {
  const getIndex = `get${COMPONENTS[slot].toUpperCase()}` as 'getX' | 'getY' | 'getZ' | 'getW';
  return {
    bone: skin.skinIndex[getIndex](vertex),
    weight: skin.skinWeight[getIndex](vertex)
  };
}

function setWeight(skin: MeshSkin, vertex: number, slot: number, weight: number): void {
  const setter = `set${COMPONENTS[slot].toUpperCase()}` as 'setX' | 'setY' | 'setZ' | 'setW';
  skin.skinWeight[setter](vertex, weight);
}

/** The ratios the tail is sampled at. Coarse on purpose — it is a shape, not a measurement. */
const TAIL_RATIOS = [0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.4, 0.5, 0.75, 1.0];

/** Measure how far every skin influence reaches, without changing anything. */
export function analyseSkin(meshes: THREE.SkinnedMesh[]): SkinReport {
  // ⚠️ `perMesh` keeps the NULL holes; `skins` is the compacted list the measurement loops walk.
  // `vertexRatios` has to line up with the caller's `meshes` array — the overlay indexes it by the
  // same subscript it uses on its own proxy list — so a mesh with no skin attributes must still
  // occupy a slot rather than shifting every later mesh's ratios onto the wrong geometry.
  const perMesh = meshes.map(meshSkin);
  const skins = perMesh.filter((s): s is MeshSkin => s !== null);
  const vertexRatios = meshes.map((mesh, m) => {
    const position = perMesh[m]?.position ?? optionalAttribute(mesh.geometry, 'position');
    return new Float32Array(position?.count ?? 0);
  });
  const empty: SkinReport = {
    referenceLength: 0,
    vertices: 0,
    influences: 0,
    maxRatio: 0,
    tail: [],
    bones: [],
    vertexRatios
  };
  if (!skins.length) return empty;

  // The reference length comes off the REST positions, not the live scene graph: a model being
  // animated when the editor opens would otherwise change what a threshold means.
  const box = new THREE.Box3();
  const v = new THREE.Vector3();
  for (const skin of skins) {
    for (let i = 0; i < skin.position.count; i++) box.expandByPoint(restPosition(skin, i, v));
  }
  const size = box.getSize(new THREE.Vector3());
  const referenceLength = Math.max(size.x, size.y, size.z) || 1;

  const tailInfluences = new Array(TAIL_RATIOS.length).fill(0);
  const tailVertices = new Array(TAIL_RATIOS.length).fill(0);
  const perBone = new Map<number, { name: string; influences: number; maxRatio: number }>();
  let vertices = 0;
  let influences = 0;
  let maxRatio = 0;

  for (let m = 0; m < perMesh.length; m++) {
    const skin = perMesh[m];
    if (!skin) continue;
    const ratios = vertexRatios[m];
    const bones = skin.mesh.skeleton.bones;
    for (let i = 0; i < skin.position.count; i++) {
      vertices++;
      restPosition(skin, i, v);
      const crossed = new Array(TAIL_RATIOS.length).fill(false);
      for (let slot = 0; slot < 4; slot++) {
        const { bone, weight } = influenceAt(skin, i, slot);
        if (weight <= 0) continue;
        const bonePosition = skin.bonePositions[bone];
        if (!bonePosition) continue;
        influences++;
        const ratio = v.distanceTo(bonePosition) / referenceLength;
        if (ratio > maxRatio) maxRatio = ratio;
        // Recorded in the SAME pass, not a second traversal: the ratio is already in hand here and
        // walking 250k vertices again to recover a number we just threw away is the difference
        // between the heat map being free and it doubling the cost of measuring.
        if (ratio > ratios[i]) ratios[i] = ratio;
        const entry = perBone.get(bone) ?? {
          name: bones[bone]?.name ?? `bone ${bone}`,
          influences: 0,
          maxRatio: 0
        };
        entry.influences++;
        entry.maxRatio = Math.max(entry.maxRatio, ratio);
        perBone.set(bone, entry);
        TAIL_RATIOS.forEach((threshold, t) => {
          if (ratio > threshold) {
            tailInfluences[t]++;
            crossed[t] = true;
          }
        });
      }
      crossed.forEach((did, t) => {
        if (did) tailVertices[t]++;
      });
    }
  }

  return {
    referenceLength,
    vertices,
    influences,
    maxRatio,
    tail: TAIL_RATIOS.map((ratio, t) => ({
      ratio,
      influences: tailInfluences[t],
      vertices: tailVertices[t]
    })),
    bones: [...perBone.entries()]
      .map(([index, e]) => ({ index, ...e }))
      .sort((a, b) => b.maxRatio - a.maxRatio)
      .slice(0, 24),
    vertexRatios
  };
}

/**
 * Which vertices a bone actually drives, for "click a joint, see what it moves".
 *
 * One `Uint8Array` per mesh, same order and length as `meshes`, holding 1 where that bone's weight
 * at that vertex is at least `minWeight`. Feed it to the heat overlay's mask and the paint collapses
 * from the whole character to the one limb the joint owns — which is how you tell a joint that is
 * merely named wrongly from a joint that is bound wrongly.
 *
 * The default of 0.15 is a readability threshold, not a correctness one. Every vertex near a joint
 * carries a trace of its neighbours, so a mask at weight > 0 lights up half the torso for a wrist.
 *
 * ⚠️ Reads through `getX`/`getY`/`getZ`/`getW` on purpose. `WEIGHTS_0` arrives from a GLB as a
 * normalized `Uint8`/`Uint16` routinely, and the accessors are what DENORMALIZE it — so the
 * comparison here is against a real 0..1 weight either way. (The opposite rule governs writing; see
 * `mesh-repair.ts`'s `remapAttribute`.)
 */
export function boneVertexMask(
  meshes: THREE.SkinnedMesh[],
  boneIndex: number,
  minWeight = 0.15
): Uint8Array[] {
  return meshes.map((mesh) => {
    const skinIndex = optionalAttribute(mesh.geometry, 'skinIndex');
    const skinWeight = optionalAttribute(mesh.geometry, 'skinWeight');
    const position = optionalAttribute(mesh.geometry, 'position');
    const mask = new Uint8Array(position?.count ?? 0);
    if (!skinIndex || !skinWeight) return mask;

    for (let i = 0; i < mask.length; i++) {
      if (
        (skinIndex.getX(i) === boneIndex && skinWeight.getX(i) >= minWeight) ||
        (skinIndex.getY(i) === boneIndex && skinWeight.getY(i) >= minWeight) ||
        (skinIndex.getZ(i) === boneIndex && skinWeight.getZ(i) >= minWeight) ||
        (skinIndex.getW(i) === boneIndex && skinWeight.getW(i) >= minWeight)
      ) {
        mask[i] = 1;
      }
    }
    return mask;
  });
}

/** Count what a prune at `ratio` would take, without touching the buffers. */
export function previewPrune(meshes: THREE.SkinnedMesh[], ratio: number): PruneResult {
  return prune(meshes, ratio, false);
}

/**
 * Zero every influence reaching farther than `ratio × referenceLength` and renormalise what is
 * left.
 *
 * 🔴 A vertex whose influences would ALL be pruned is left untouched. Zeroing it would leave it
 * with no binding at all, and an unskinned vertex is worse than a badly skinned one — it is the
 * one failure mode the engine itself refuses to ship.
 */
export function applyPrune(meshes: THREE.SkinnedMesh[], ratio: number): PruneResult {
  return prune(meshes, ratio, true);
}

function prune(meshes: THREE.SkinnedMesh[], ratio: number, write: boolean): PruneResult {
  const skins = meshes.map(meshSkin).filter((s): s is MeshSkin => s !== null);
  const result: PruneResult = {
    prunedInfluences: 0,
    verticesTouched: 0,
    verticesSpared: 0,
    byBone: []
  };
  if (!skins.length || ratio <= 0) return result;

  const box = new THREE.Box3();
  const v = new THREE.Vector3();
  for (const skin of skins) {
    for (let i = 0; i < skin.position.count; i++) box.expandByPoint(restPosition(skin, i, v));
  }
  const size = box.getSize(new THREE.Vector3());
  const limit = (Math.max(size.x, size.y, size.z) || 1) * ratio;
  const byBone = new Map<string, number>();

  for (const skin of skins) {
    const bones = skin.mesh.skeleton.bones;
    for (let i = 0; i < skin.position.count; i++) {
      restPosition(skin, i, v);
      const weights: number[] = [];
      const doomed: number[] = [];
      let total = 0;
      let survivors = 0;
      for (let slot = 0; slot < 4; slot++) {
        const { bone, weight } = influenceAt(skin, i, slot);
        weights.push(weight);
        if (weight <= 0) continue;
        total += weight;
        const bonePosition = skin.bonePositions[bone];
        if (bonePosition && v.distanceTo(bonePosition) > limit) {
          doomed.push(slot);
          byBone.set(
            bones[bone]?.name ?? `bone ${bone}`,
            (byBone.get(bones[bone]?.name ?? `bone ${bone}`) ?? 0) + 1
          );
        } else {
          survivors += weight;
        }
      }
      if (!doomed.length) continue;
      if (survivors <= 0 || total <= 0) {
        // Every binding this vertex has is "too far". Leave it exactly as it was.
        result.verticesSpared++;
        // Un-count the bones we tentatively charged for a vertex we are not touching.
        for (const slot of doomed) {
          const { bone } = influenceAt(skin, i, slot);
          const name = bones[bone]?.name ?? `bone ${bone}`;
          const current = byBone.get(name) ?? 0;
          if (current > 0) byBone.set(name, current - 1);
        }
        continue;
      }
      result.verticesTouched++;
      result.prunedInfluences += doomed.length;
      if (!write) continue;
      // Rescale the survivors so the vertex keeps the total weight it ARRIVED with — a vertex whose
      // total changes shrinks or inflates under animation.
      //
      // ⚠️ That is "the total it arrived with", not "exactly 1", and the difference is real on a
      // non-conformant asset: `[0.4, 0.4]` with the second influence pruned becomes `[0.8, 0, 0, 0]`,
      // not `[1, 0, 0, 0]`. glTF requires the four weights to sum to 1, so on any conformant file
      // the two readings coincide — and preserving the incoming total is the right call anyway,
      // because silently normalising here would repair a fault this function was not asked to look
      // at and would mask it from whatever should be fixing it. (The comment used to claim exactly
      // 1; a test now pins the real behaviour.)
      const scale = total / survivors;
      for (let slot = 0; slot < 4; slot++) {
        if (doomed.includes(slot)) setWeight(skin, i, slot, 0);
        else setWeight(skin, i, slot, weights[slot] * scale);
      }
    }
    if (write) skin.skinWeight.needsUpdate = true;
  }

  result.byBone = [...byBone.entries()]
    .filter(([, influences]) => influences > 0)
    .map(([name, influences]) => ({ name, influences }))
    .sort((a, b) => b.influences - a.influences);
  return result;
}

/**
 * Which of the 22 core slots this skeleton fills, and which bones carry no core name.
 *
 * 🔴 Matched in THREE-SPACE and reported in CANONICAL space. The bones carry sanitized names and
 * the engine cares about the colon-bearing ones, so the comparison runs through
 * [`sanitizeName`] while the returned `present`/`missing` lists stay in the form a human reads and
 * the file stores. Comparing `MIXAMO_CORE` against `bone.name` directly reports 22/22 missing on
 * every rig ever produced by this pipeline.
 */
export function coreCoverage(bones: THREE.Bone[]): {
  present: string[];
  missing: string[];
  unnamed: THREE.Bone[];
} {
  const onRig = new Set(bones.map((b) => b.name));
  const present = MIXAMO_CORE.filter((n) => onRig.has(sanitizeName(n)));
  const missing = MIXAMO_CORE.filter((n) => !onRig.has(sanitizeName(n)));
  const core = new Set<string>(MIXAMO_CORE.map(sanitizeName));
  return { present, missing, unnamed: bones.filter((b) => !core.has(b.name)) };
}

/**
 * The anatomical parent of each core slot, in CANONICAL (colon-bearing) form.
 *
 * This is the skeleton the retarget expects, not a guess: Hips carries the spine and both legs,
 * Spine2 carries the neck and both shoulders. `mixamorig:Hips` is the root and has no entry.
 */
const CORE_PARENT_CANONICAL: Record<string, string> = {
  'mixamorig:Spine': 'mixamorig:Hips',
  'mixamorig:Spine1': 'mixamorig:Spine',
  'mixamorig:Spine2': 'mixamorig:Spine1',
  'mixamorig:Neck': 'mixamorig:Spine2',
  'mixamorig:Head': 'mixamorig:Neck',
  'mixamorig:LeftShoulder': 'mixamorig:Spine2',
  'mixamorig:LeftArm': 'mixamorig:LeftShoulder',
  'mixamorig:LeftForeArm': 'mixamorig:LeftArm',
  'mixamorig:LeftHand': 'mixamorig:LeftForeArm',
  'mixamorig:RightShoulder': 'mixamorig:Spine2',
  'mixamorig:RightArm': 'mixamorig:RightShoulder',
  'mixamorig:RightForeArm': 'mixamorig:RightArm',
  'mixamorig:RightHand': 'mixamorig:RightForeArm',
  'mixamorig:LeftUpLeg': 'mixamorig:Hips',
  'mixamorig:LeftLeg': 'mixamorig:LeftUpLeg',
  'mixamorig:LeftFoot': 'mixamorig:LeftLeg',
  'mixamorig:LeftToeBase': 'mixamorig:LeftFoot',
  'mixamorig:RightUpLeg': 'mixamorig:Hips',
  'mixamorig:RightLeg': 'mixamorig:RightUpLeg',
  'mixamorig:RightFoot': 'mixamorig:RightLeg',
  'mixamorig:RightToeBase': 'mixamorig:RightFoot'
};

/**
 * 🔴 The same table in THREE-SPACE, built once at load.
 *
 * `THREE.Bone.name` never carries the colons — `GLTFLoader` deletes them (see [`sanitizeName`]) —
 * so every lookup below has to go through the sanitized form. Comparing a `mixamorig:`-prefixed
 * constant against a bone name matches nothing, and the failure is not an error: it is a candidate
 * list that silently drops to "everything unnamed, in file order", which looks like a working
 * feature that just is not very good.
 */
const CORE_PARENT = new Map<string, string>(
  Object.entries(CORE_PARENT_CANONICAL).map(([child, parent]) => [
    sanitizeName(child),
    sanitizeName(parent)
  ])
);

/** How many hops up the parent chain `bone` sits below `anchor`, or 0 if it is not below it. */
function depthBelow(bone: THREE.Bone, anchor: THREE.Object3D): number {
  let walk: THREE.Object3D | null = bone.parent;
  let hops = 1;
  while (walk) {
    if (walk === anchor) return hops;
    walk = walk.parent;
    hops++;
  }
  return 0;
}

/**
 * Bones that are plausibly the missing core slot `coreName`, best first.
 *
 * Ranked by SKELETON TOPOLOGY rather than by name or by proximity, because a joint that has no core
 * name has, by definition, nothing to match on — but the joints AROUND it usually do. A skeleton
 * missing only `mixamorig:LeftForeArm` still has `mixamorig:LeftArm`, and the elbow is the thing
 * hanging directly off it. That is a far stronger signal than anything a distance heuristic gives,
 * and it is the difference between a dropdown of sixty anonymous joints and a dropdown whose first
 * entry is right.
 *
 *   1. a direct child of the bone carrying the canonical parent's name
 *   2. any deeper descendant of it, nearest first
 *   3. everything else, in skeleton order
 *
 * Bones that ALREADY hold a core name are never offered: they are spoken for, and assigning one to
 * a second slot would take a working joint away from the clip that drives it.
 *
 * ⚠️ Rank 1 is a suggestion, not a finding. Nothing here checks that the bone is anatomically where
 * the name would claim, and a wrong assignment retargets cleanly and animates wrongly — the same
 * warning the editor already prints next to the assign button.
 *
 * ⚠️ `mixamorig:Hips` has no anatomical parent, so every candidate for it lands in rank 3. That is
 * honest: with the root missing there is no topology left to reason from.
 */
export function coreCandidates(bones: THREE.Bone[], coreName: string): THREE.Bone[] {
  const core = new Set<string>(MIXAMO_CORE.map(sanitizeName));
  const parentName = CORE_PARENT.get(sanitizeName(coreName));
  const anchor = parentName ? bones.find((b) => sanitizeName(b.name) === parentName) : undefined;

  return bones
    .map((bone, order) => ({ bone, order, depth: anchor ? depthBelow(bone, anchor) : 0 }))
    .filter((c) => !core.has(sanitizeName(c.bone.name)))
    .sort((a, b) => {
      // depth 0 means "not below the anchor" and has to sort LAST, not first — hence the explicit
      // rank rather than sorting on depth directly.
      const rankA = a.depth === 0 ? 3 : a.depth === 1 ? 1 : 2;
      const rankB = b.depth === 0 ? 3 : b.depth === 1 ? 1 : 2;
      return rankA - rankB || a.depth - b.depth || a.order - b.order;
    })
    .map((c) => c.bone);
}

// ── moving a joint ───────────────────────────────────────────────────────────
//
// 🔴 WHY THIS IS WORTH DOING AT ALL, AND WHY IT IS TWO EDITS AND NOT ONE.
//
// The engine's retarget (`motion_retarget.hpp`) does NOT read `inverseBindMatrices`. It loads the
// glTF **node tree** — parents from `children`, local transforms from `matrix`/`translation` — and
// runs forward kinematics: `Gpos[i] = parentPos + parentRot · localTranslation[i]`. It then
// measures, per chain group, the angle between OUR rest bone direction and SMPL's, and its own
// comment calls that "the quantity delta silently adds to every frame". Those numbers come back on
// every animate result as `rest_arm_deg` / `rest_leg_deg` / `rest_spine_deg`.
//
// ⇒ Moving a joint changes what the retarget sees, and a 0.22 s take tells you whether it helped.
// In a pipeline with no automatic quality signal for motion, that is a real readout.
//
// But a node translation alone leaves `boneWorld ≠ bindWorld`, so the skinning bracket
// `boneWorld · inverseBind` stops being the identity and **the mesh visibly warps in its rest
// pose** — you would have posed the character, not repaired the rig. Re-binding
// (`Skeleton.calculateInverses()`) is what makes the joint move *inside* a stationary mesh, which
// is the whole point: the surface stays put and the joint's rotation pivot moves.
//
// ⚠️ Re-binding reads `bone.matrixWorld`, so the model must sit at its delivered transform while it
// happens. `ModelStage` aims the camera instead of moving the model precisely for this.

/**
 * A joint's delivered local position, kept so any edit can be undone exactly.
 *
 * 🔴 Keyed by the BONE, not by its name, and that is not a style choice. Naming a joint is one of
 * the edits this editor offers, so a name-keyed baseline goes stale the moment it is used — and
 * the failure is silent and asymmetric. A claimed anonymous joint drops out of the map and reads
 * as unmoved; a LEFT/RIGHT swap is worse, because both names still exist and every sided bone
 * then looks its rest position up under its PARTNER'S name. "Put every joint back" would answer
 * that by moving all sixteen to the other side of the body.
 */
export type JointRest = Map<THREE.Bone, THREE.Vector3>;

export function captureJointRest(bones: THREE.Bone[]): JointRest {
  return new Map(bones.map((b) => [b, b.position.clone()]));
}

/** Which joints currently differ from their delivered position, and by how far. */
export function movedJoints(
  bones: THREE.Bone[],
  rest: JointRest
): { name: string; distance: number }[] {
  return bones
    .map((b) => ({ name: b.name, distance: rest.get(b)?.distanceTo(b.position) ?? 0 }))
    .filter((j) => j.distance > 1e-6)
    .sort((a, b) => b.distance - a.distance);
}

/** Express a world-space delta in `target`'s parent space, where `Object3D.position` lives. */
function toParentSpace(target: THREE.Object3D, worldDelta: THREE.Vector3): THREE.Vector3 {
  const parent = target.parent;
  if (!parent) return worldDelta.clone();
  const inverse = new THREE.Matrix4().copy(parent.matrixWorld).invert();
  const from = target.getWorldPosition(new THREE.Vector3());
  const to = from.clone().add(worldDelta);
  return to.applyMatrix4(inverse).sub(from.applyMatrix4(inverse));
}

/**
 * Slide one joint by a world-space delta and re-bind, leaving the mesh exactly where it was.
 *
 * `keepChildren` is the difference between the two things a user might mean:
 *
 * - `true` (**move the joint only**) — direct child joints are counter-translated back to where
 *   they were, so dragging an elbow moves the elbow and leaves the wrist. This is what "the
 *   skeleton is a little off" means, and it is the default.
 * - `false` (**move the chain**) — forward kinematics carries every descendant along, which
 *   re-proportions the limb. Right for "the whole forearm is too short", wrong for everything else.
 */
export function moveJointWorld(
  root: THREE.Object3D,
  meshes: THREE.SkinnedMesh[],
  bone: THREE.Bone,
  worldDelta: THREE.Vector3,
  keepChildren: boolean
): void {
  if (!bone.parent) return;
  root.updateMatrixWorld(true);

  const children = bone.children.filter((c): c is THREE.Bone => (c as THREE.Bone).isBone === true);
  const held = children.map((c) => c.getWorldPosition(new THREE.Vector3()));

  bone.position.add(toParentSpace(bone, worldDelta));
  root.updateMatrixWorld(true);

  if (keepChildren) {
    children.forEach((child, i) => {
      const drift = held[i].clone().sub(child.getWorldPosition(new THREE.Vector3()));
      child.position.add(toParentSpace(child, drift));
    });
    root.updateMatrixWorld(true);
  }

  rebind(meshes);
}

/** Put every joint back where the engine delivered it. */
export function resetJoints(
  root: THREE.Object3D,
  meshes: THREE.SkinnedMesh[],
  bones: THREE.Bone[],
  rest: JointRest
): void {
  for (const bone of bones) {
    const home = rest.get(bone);
    if (home) bone.position.copy(home);
  }
  root.updateMatrixWorld(true);
  rebind(meshes);
}

/**
 * Re-bind every skeleton at the current rest pose.
 *
 * 🔴 This is the half that keeps the MESH still. `calculateInverses()` sets
 * `boneInverses[i] = bones[i].matrixWorld⁻¹`, so `boneWorld · inverseBind` is the identity again
 * and the skinned surface lands exactly where it was — with the moved joint now pivoting about its
 * new position. `glb-patch.ts` writes `boneInverses[i] · bindMatrix` back as the file's
 * `inverseBindMatrices`, so the repair survives the round trip.
 */
function rebind(meshes: THREE.SkinnedMesh[]): void {
  const done = new Set<THREE.Skeleton>();
  for (const mesh of meshes) {
    if (!mesh.skeleton || done.has(mesh.skeleton)) continue;
    done.add(mesh.skeleton);
    mesh.skeleton.calculateInverses();
    mesh.skeleton.update();
  }
}

/** The model's largest bounding-box dimension, so a nudge step means the same on any scale. */
export function modelSpan(root: THREE.Object3D): number {
  const size = new THREE.Box3().setFromObject(root).getSize(new THREE.Vector3());
  return Math.max(size.x, size.y, size.z) || 1;
}
