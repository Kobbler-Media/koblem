/*
 * Replaying a joint repair from one LOD tier onto the others.
 *
 * # Why this exists
 *
 * A character ships as a 4-tier ladder — hero 220k / high 100k / medium 40k / game 15k — built over
 * ONE shared skeleton. The editor repairs whichever tier is on screen, which is almost always the
 * hero, and `game` is the tier a game actually ships. Without this module the user fixes the one
 * they can see and the one that matters stays broken.
 *
 * # What can and cannot cross a tier boundary
 *
 * Only **joint moves and joint renames**. Every tier's file carries the same skeleton, with the same
 * joints in the same order, so a node translation lifted from one tier is meaningful in the next.
 *
 * Weight prunes, shell deletes, degenerate filters and texture repairs are per-geometry: each tier
 * has its own vertices, its own index buffer and (in principle) its own maps. There is no vertex
 * correspondence across a decimation, so there is nothing here for them and there deliberately is
 * no hook to add one.
 *
 * # 🔴 Absolute local positions, not world deltas
 *
 * `moveJointWorld` is an *interaction*: a world-space drag, converted to parent space, optionally
 * counter-translating the direct children so the wrist stays put when the elbow moves. Replaying
 * that interaction on another tier would be wrong twice over — the counter-translated children are
 * themselves edited joints, so replaying the parent's delta would apply their correction a second
 * time, and every re-derivation of a parent-space delta costs float precision.
 *
 * What we replay instead is the FINAL STATE: each edited joint's absolute `bone.position`. That
 * already contains the child counter-translations (they were written to `child.position`), it is
 * idempotent, and it is exactly the number the file stores. Applying it is
 * [`resetJoints`] — "copy these local positions in by name, then re-bind" — whose name is about its
 * undo button in the UI, not about its mechanics. Reusing it is what guarantees the re-bind here is
 * bit-for-bit the same operation the interactive editor performs.
 *
 * # 🔴 A joint move is still two edits on the target too
 *
 * `bone.position` is what the engine's retarget reads: it runs FK over the glTF node tree and never
 * looks at `inverseBindMatrices`. `Skeleton.calculateInverses()` is what keeps the MESH still while
 * the pivot moves. Ship one without the other and the tier is broken in one direction or the other.
 * `resetJoints` does both; `patchGlb({ joints: true })` writes both halves back to the file.
 *
 * ⚠️ Re-binding reads `bone.matrixWorld`, so the tier is loaded and left at its delivered transform.
 * Nothing here translates, re-parents or frames the model — see the same warning in `ModelStage`.
 *
 * # What this module is not allowed to do
 *
 * No Svelte, no DOM, no fetch, no renderer. `GLTFLoader.parseAsync(bytes, '')` needs neither a WebGL
 * context nor a canvas, and `Skeleton.calculateInverses()`/`update()` are pure matrix maths, so a
 * tier can be loaded, repaired and patched entirely off-screen. Verified against the r185 sources on
 * 2026-08-15: `Skeleton.update()` only touches `boneTexture` if one exists, and one is only created
 * by the renderer.
 */

import * as THREE from 'three';
import { GLTFLoader } from 'three/examples/jsm/loaders/GLTFLoader.js';

import { patchGlb, type GltfAssociation } from './glb-patch';
import {
  captureJointRest,
  modelSpan,
  movedJoints,
  resetJoints,
  sanitizeName,
  type JointRest
} from './stage';

/**
 * One joint's repaired local position, and its canonical (colon-bearing) name if it was renamed.
 *
 * 🔴 Keyed by the SANITIZED three-side bone name, because that is what a loaded tier will report.
 * `GLTFLoader.createUniqueName` runs every node name through `PropertyBinding.sanitizeNodeName`,
 * which **deletes** `[ ] . : /`, so the file's `mixamorig:LeftArm` is `mixamorigLeftArm` on a
 * `THREE.Bone`. `patchGlb`'s `renames` map is keyed the same way, for the same reason.
 */
export interface JointEdit {
  /** The joint's repaired position in its PARENT's space — an absolute `bone.position`, not a
   *  delta. See the header for why a delta would double-apply. */
  position: [number, number, number];
  /** The colon-bearing name to write into the file. `undefined` means "leave the name alone". */
  canonicalName?: string;
  /**
   * The joint's position in the tier the edit came FROM, before the repair.
   *
   * 🔴 This is the shared-skeleton proof, and the only defence against the failure that would
   * otherwise be silent: absolute positions are meaningful across tiers *only* if the tiers really
   * do carry the same skeleton at the same scale. A ladder built at two different scales, or a tier
   * regenerated against a re-authored rig, would take these positions and quietly deform the
   * character with no error anywhere. [`propagateToTier`] refuses when the target's delivered
   * position does not match this one.
   */
  restPosition?: [number, number, number];
}

export type JointEdits = Map<string, JointEdit>;

/**
 * One rename, in CANONICAL (colon-bearing) form on both sides.
 *
 * Both halves are needed and neither is redundant: `to` is what gets written into the file, and
 * `from` is what the joint is still called on every tier that has not been repaired yet — which is
 * the only name propagation can find it by.
 */
export interface RenamedJoint {
  from: string;
  to: string;
}

export interface PropagateResult {
  /** Edited joints that were found in the target tier and written. */
  applied: number;
  /** Of those, the ones whose position actually changed here — i.e. the joints this tier was
   *  genuinely missing the repair for. Normally every moved joint; a short list means the tier had
   *  already been propagated to. */
  moved: string[];
  /** How many joints were given their canonical name. */
  renamed: number;
  /**
   * Edited joints this tier's skin does not contain.
   *
   * Should be empty for a shared skeleton, and is worth surfacing loudly if it is not: it means the
   * ladder is not what this module assumes, and the tier has been repaired only in part.
   */
  missing: string[];
  /** The subset of `missing` that DO exist as bones in the tier but are not joints of its skin.
   *  Split out because "the joint is right there, I can see it" is the confusing case — a bone
   *  outside the skin has no entry in `skin.joints` and therefore nowhere in the file to write. */
  unskinned: string[];
  /** The target tier's joint count, for the caller to compare across the ladder. */
  joints: number;
  glb: Blob;
}

export interface PropagateOptions {
  /**
   * The repaired tier's `skeleton.bones.length`.
   *
   * ⚠️ Optional but worth passing. `GLTFLoader.loadSkin` builds its bone list with
   * `if ( jointNode ) bones.push( … )` — a joint it could not resolve is SKIPPED and every index
   * after it shifts. `patchGlb` catches that within a single file (three's count vs the file's), but
   * only a cross-tier count can catch two tiers that disagree with each other.
   */
  expectedJoints?: number;
}

/**
 * How far a tier's delivered joint position may sit from the source tier's before we call the
 * skeletons different, as a fraction of the model's largest bounding-box dimension.
 *
 * Both files store the same float32s, so a genuine ladder is bit-identical here; 1e-4 of span
 * (≈0.2 mm on a 1.8 m character) is slack for a re-encode, not for a real difference.
 */
const REST_TOLERANCE = 1e-4;

/** Our texture-skipping plugin's key in `parser.plugins`. Namespaced so it can never collide with a
 *  real `KHR_`/`EXT_` extension name, which is what that map is otherwise keyed by. */
const SKIP_TEXTURES = 'KOBLEM_skip_textures_for_rig_patch';

/**
 * Lift the edits out of an already-repaired tier, so they can be replayed onto the others.
 *
 * `bones` and `rest` must be the same list `captureJointRest` was called on — `movedJoints` reports
 * a bone missing from `rest` as *unmoved*, so a mismatched pair would silently drop edits rather
 * than complain.
 *
 * ⚠️ `bones` here is the editor's list (`StageHandle.bones`, built by traversing the scene), and
 * that is fine: everything in this map is keyed by name. Bone ORDER only becomes load-bearing at
 * write time, and [`propagateToTier`] takes it from `skeleton.bones` where it is provably the
 * file's joint order.
 */
export function collectJointEdits(
  bones: THREE.Bone[],
  rest: JointRest,
  renames: Map<string, RenamedJoint>
): JointEdits {
  const byName = new Map(bones.map((b) => [b.name, b]));
  if (byName.size !== bones.length) {
    // `createUniqueName` suffixes duplicates (`_1`, `_2`), so this should be impossible on a
    // loaded GLB. If it ever happens, name-keyed edits would land on an arbitrary one of the pair.
    throw new Error(
      `this skeleton has ${bones.length} bones but only ${byName.size} distinct names, so an edit ` +
        `cannot be matched to a joint by name. Refusing to collect a repair that could be replayed ` +
        `onto the wrong bone.`
    );
  }

  for (const name of renames.keys()) {
    if (!byName.has(name)) {
      throw new Error(
        `"${name}" was renamed but is not a bone of this skeleton. A rename is keyed by the ` +
          `sanitized three-side name (the file's "mixamorig:LeftArm" is "mixamorigLeftArm" here), ` +
          `so this one would be silently dropped.`
      );
    }
  }

  /**
   * 🔴 A renamed joint is filed under the name it had BEFORE the rename, because that is the name
   * the other tiers still carry — they were not edited. Filing it under the new name is the bug
   * that made a left/right swap propagate as a silent no-op: both `mixamorigLeftArm` and
   * `mixamorigRightArm` exist in every tier, so each edit found a bone, matched the name it was
   * already going to write, and changed nothing. [`propagateToTier`]'s `renamedKey` fallback is
   * what re-finds the joint on a tier that HAS already been propagated to.
   */
  const filedAs = new Map<THREE.Bone, string>();
  for (const [current, rename] of renames) {
    const bone = byName.get(current);
    if (bone) filedAs.set(bone, sanitizeName(rename.from));
  }

  const edits: JointEdits = new Map();
  const put = (bone: THREE.Bone): JointEdit => {
    const key = filedAs.get(bone) ?? bone.name;
    const existing = edits.get(key);
    if (existing) return existing;
    const home = rest.get(bone);
    const edit: JointEdit = {
      position: [bone.position.x, bone.position.y, bone.position.z]
    };
    if (home) edit.restPosition = [home.x, home.y, home.z];
    edits.set(key, edit);
    return edit;
  };

  // The moved set comes from `movedJoints` rather than a fresh comparison so that "moved" means
  // exactly what the editor's own list means — same 1e-6 threshold, same answer.
  for (const { name } of movedJoints(bones, rest)) {
    const bone = byName.get(name);
    if (bone) put(bone);
  }

  for (const [name, rename] of renames) {
    const bone = byName.get(name);
    if (!bone) continue;
    // A rename-only joint still carries its position, which for a shared skeleton is the value the
    // target already holds — a no-op write. Carrying it costs nothing and keeps one code path.
    put(bone).canonicalName = rename.to;
  }

  return edits;
}

/**
 * Replay joint edits onto ANOTHER tier's GLB and return the patched bytes.
 *
 * Loads the tier off-screen, writes each edit as an absolute local position, re-binds, and patches
 * the ORIGINAL bytes — never `GLTFExporter`, which would strip the `mixamorig:` colons out of every
 * node name and re-encode every texture. See the header of `glb-patch.ts`.
 *
 * `tierBytes` is not consumed: `parseAsync` only takes views over it, and the same buffer is what
 * gets patched.
 */
export async function propagateToTier(
  tierBytes: ArrayBuffer,
  edits: JointEdits,
  options: PropagateOptions = {}
): Promise<PropagateResult> {
  if (edits.size === 0) {
    throw new Error('there are no joint edits to propagate');
  }

  const loader = new GLTFLoader();
  // ⚠️ Nothing here is ever rendered, and a hero tier's maps are routinely 4K — decoding two of
  // them per tier costs ~130 MB of ImageBitmap and a chunk of wall clock for pixels we then throw
  // away. `_invokeOne` takes the FIRST truthy result and consults plugins before the parser itself,
  // so returning a bare Texture short-circuits `loadTextureImage` entirely. `patchGlb` copies image
  // bytes straight from the source file, so the dummies never reach the output.
  //
  // Safe to lose: if this hook ever stops firing the real textures simply load, which is only
  // slower. (It already does not fire for EXT_texture_webp / KHR_texture_basisu maps, whose own
  // plugins are registered first and claim them.)
  loader.register(() => ({
    name: SKIP_TEXTURES,
    loadTexture: () => Promise.resolve(new THREE.Texture())
  }));

  const gltf = await loader.parseAsync(tierBytes, '');
  const root = gltf.scene;

  try {
    const meshes: THREE.SkinnedMesh[] = [];
    const reachable = new Set<THREE.Object3D>();
    const sceneBoneNames = new Set<string>();
    root.traverse((o) => {
      if ((o as THREE.SkinnedMesh).isSkinnedMesh) meshes.push(o as THREE.SkinnedMesh);
      if ((o as THREE.Bone).isBone) {
        reachable.add(o);
        sceneBoneNames.add(o.name);
      }
    });
    if (meshes.length === 0 || !meshes[0].skeleton) {
      throw new Error('this tier has no skinned mesh, so it carries no skeleton to repair');
    }

    // 🔴 The bone list comes from the SKELETON, not from a scene traversal. `GLTFLoader.loadSkin`
    // walks `skinDef.joints` in order, so `skeleton.bones[i]` is `skin.listJoints()[i]` — that
    // identity is the entire correspondence `patchGlb` writes through. A depth-first walk of the
    // node tree happens to produce the same order on most rigs and is not guaranteed to, and it
    // also picks up bones that are not joints of the skin at all.
    const bones = meshes[0].skeleton.bones;
    const byName = new Map(bones.map((b) => [b.name, b]));

    if (options.expectedJoints !== undefined && options.expectedJoints !== bones.length) {
      throw new Error(
        `the repaired tier has ${options.expectedJoints} joints but this tier has ${bones.length}, ` +
          `so they are not the same skeleton and the repair cannot be matched up by order. ` +
          `Refusing to patch rather than move the wrong joints.`
      );
    }

    // 🔴 Every skeleton joint must live under the scene root we are about to call
    // `updateMatrixWorld` on. A joint parented outside it (glTF allows nodes that belong to no
    // scene) would keep a stale `matrixWorld`, and `calculateInverses()` reads exactly that — the
    // file would get an inverse bind matrix for a pose the bone is not in, and the mesh would come
    // back deformed with nothing to point at.
    const orphans = bones.filter((b) => !reachable.has(b));
    if (orphans.length > 0) {
      throw new Error(
        `${orphans.length} of this tier's ${bones.length} joints are not under its scene root ` +
          `(first: "${orphans[0].name}"), so their world matrices cannot be refreshed and the ` +
          `re-bind would be computed from stale values`
      );
    }

    // 🔴 `patchGlb` writes ONE inverse-bind-matrix array, computed as
    // `boneInverses[i] · meshes[0].bindMatrix`. That is only valid for every mesh if they share a
    // bind matrix — which they do when the mesh nodes share a transform, and silently do not
    // otherwise. Check rather than ship an array that is right for one mesh and wrong for the rest.
    for (const mesh of meshes.slice(1)) {
      if (mesh.skeleton?.bones.length !== bones.length) {
        throw new Error(
          `"${mesh.name || 'a mesh'}" is bound to a skeleton of ${mesh.skeleton?.bones.length ?? 0} ` +
            `joints while the first mesh has ${bones.length}; this tier has more than one skeleton ` +
            `and the editor only knows how to repair a character with one`
        );
      }
      if (!mesh.bindMatrix.equals(meshes[0].bindMatrix)) {
        throw new Error(
          `"${mesh.name || 'a mesh'}" has a different bind matrix from the first mesh, so a single ` +
            `set of inverse bind matrices cannot be correct for both. Refusing to patch.`
        );
      }
    }

    // Delivered state, captured before anything is written: the baseline for "did this tier
    // actually need the repair", and the value the shared-skeleton check compares against.
    const delivered = captureJointRest(bones);
    const tolerance = modelSpan(root) * REST_TOLERANCE;

    const desired: JointRest = new Map();
    const renames = new Map<string, string>();
    const missing: string[] = [];
    const unskinned: string[] = [];

    for (const [name, edit] of edits) {
      // ⚠️ The fallback is what makes a rename survive a second pass. Once a tier has been
      // propagated to, its joint carries the canonical name, so on reload it reports
      // `sanitizeName(canonicalName)` — a different key from the one the edit was collected under.
      // Without this, re-running propagation on an already-patched tier would report every renamed
      // joint as missing and skip its position too.
      const renamedKey = edit.canonicalName ? sanitizeName(edit.canonicalName) : undefined;
      // 🔴 Resolve to the BONE, not to its name. `JointRest` is bone-keyed on purpose (see
      // `stage.ts`), and this is the one path that also renames joints — a name-keyed `desired`
      // misses every `rest.get(bone)` in `resetJoints`, so the propagation would move nothing and
      // then report the tier as already patched.
      const bone = byName.get(name) ?? (renamedKey ? byName.get(renamedKey) : undefined);
      if (!bone) {
        missing.push(name);
        if (sceneBoneNames.has(name)) unskinned.push(name);
        continue;
      }
      const key = bone.name;

      const home = delivered.get(bone);
      if (edit.restPosition && home) {
        const source = new THREE.Vector3(...edit.restPosition);
        if (source.distanceTo(home) > tolerance) {
          throw new Error(
            `"${key}" sits at ${format(home)} in this tier but at ${format(source)} in the tier the ` +
              `repair came from. These are not the same skeleton, so replaying joint positions ` +
              `across them would deform the character. Refusing to patch.`
          );
        }
      }

      desired.set(bone, new THREE.Vector3(...edit.position));
      if (edit.canonicalName) renames.set(key, edit.canonicalName);
    }

    if (desired.size === 0) {
      throw new Error(
        `none of the ${edits.size} edited joints exist in this tier, so there is nothing to ` +
          `propagate. The ladder does not share a skeleton.`
      );
    }

    // The apply and the re-bind, in the one function the interactive editor also goes through.
    resetJoints(root, meshes, bones, desired);

    const moved = movedJoints(bones, delivered).map((j) => j.name);
    if (moved.length === 0 && renames.size === 0) {
      // Every edit resolved and every one was already in place. Re-serialising the file anyway
      // would rewrite its buffers for no gain, and a gltf-transform round trip is not free of risk
      // (unknown extensions, buffer layout). Say so instead.
      throw new Error('this tier already carries every one of these joint edits; nothing to write');
    }

    const glb = await patchGlb(
      {
        source: tierBytes,
        meshes,
        bones,
        // `patchGlb` only reads this for geometry and texture patches, neither of which crosses a
        // tier boundary — but pass it rather than an empty map so a future patch kind is not
        // quietly broken here.
        associations: gltf.parser.associations as Map<object, GltfAssociation>
      },
      {
        // ⚠️ `joints: true` makes `patchGlb` rewrite EVERY joint translation and the WHOLE inverse
        // bind matrix array from the live skeleton — not just the ones we touched. That is only
        // safe because this scene was parsed fresh from the delivered bytes and nothing but
        // `resetJoints` has run on it. Never hand it a scene that has been posed by a mixer or run
        // through a prune.
        joints: moved.length > 0,
        renames: renames.size > 0 ? renames : undefined
      }
    );

    return {
      applied: desired.size,
      moved,
      renamed: renames.size,
      missing,
      unskinned,
      joints: bones.length,
      glb
    };
  } finally {
    // After the patch, never before: `patchGlb` reads `skeleton.boneInverses` and `bindMatrix` off
    // these meshes. (Disposal runs on the way out of the `try`, so the Blob is already built.)
    disposeScene(root);
  }
}

function format(v: THREE.Vector3): string {
  return `(${v.x.toFixed(4)}, ${v.y.toFixed(4)}, ${v.z.toFixed(4)})`;
}

/**
 * Release everything the throwaway tier allocated.
 *
 * Materials are scanned generically rather than by the two slots the viewer happens to light: a
 * delivered GLB may carry emissive, occlusion or ORM maps, and a map that is never disposed is a
 * decoded image held for the lifetime of the tab. Textures are collected into a set first because
 * one image is routinely shared by several materials.
 */
function disposeScene(root: THREE.Object3D): void {
  const textures = new Set<THREE.Texture>();
  root.traverse((o) => {
    const mesh = o as THREE.Mesh & { skeleton?: THREE.Skeleton };
    mesh.geometry?.dispose();
    mesh.skeleton?.dispose();
    const materials = Array.isArray(mesh.material)
      ? mesh.material
      : mesh.material
        ? [mesh.material]
        : [];
    for (const material of materials) {
      for (const value of Object.values(material as unknown as Record<string, unknown>)) {
        if (value && (value as THREE.Texture).isTexture) textures.add(value as THREE.Texture);
      }
      material.dispose();
    }
  });
  for (const texture of textures) texture.dispose();
}
