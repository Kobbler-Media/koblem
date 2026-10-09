/*
 * Saving an edited rig by PATCHING the original GLB, never by re-exporting it.
 *
 * # 🔴 Why `GLTFExporter` cannot be the save path — two independent reasons, both verified against
 * the three r185 sources on 2026-08-15
 *
 * **1. It strips `mixamorig:` out of every bone name.** `GLTFLoader.createUniqueName` runs each
 * node name through `PropertyBinding.sanitizeNodeName`, which is
 * `name.replace(/\s/g,'_').replace(/[[\]./:]/g,'')` — it **deletes** `[ ] . : /` rather than
 * escaping them, because those characters are the animation track-path grammar. So the file's
 * `mixamorig:LeftArm` is `mixamorigLeftArm` on a `THREE.Bone`. On the way out,
 * `GLTFExporter.processNode` writes `nodeDef.name = String( object.name )` — the sanitized one,
 * never `userData.name`. A round trip therefore rewrites the FILE without its colons, and the
 * engine's `derive_map_named` matches exact strings. Every saved repair would quietly downgrade its
 * own character's animation mapping, with `named_core` collapsing and no error anywhere.
 *
 * **2. It re-encodes every texture.** `GLTFExporter.processImage` unconditionally allocates a
 * canvas, `drawImage`s the map into it and calls `toBlob`/`convertToBlob`. There is no branch that
 * reuses source bytes and no plugin hook that intercepts it (`writeTexture` fires afterwards).
 * `GLTFLoader` preserves `userData.mimeType`, so a JPEG map comes back JPEG at quality 0.92 —
 * generational loss on every save — and a PNG map is lossless but costs a 150–500 ms main-thread
 * encode and typically grows. Draco is decompressed on load and re-exported raw; unrecognised
 * extensions are dropped.
 *
 * # What this does instead
 *
 * Read the ORIGINAL bytes, change only the accessors and nodes the editor actually touched, write
 * them back. Node names keep their colons because nobody rewrote them. Textures keep their bytes
 * because nobody decoded them. Draco, extensions and every untouched buffer survive by default
 * rather than by effort.
 *
 * three.js stays the editing and preview model — every repair here is an attribute array, an index
 * buffer or a node transform, all of which lift out of the live scene wholesale at save time.
 *
 * # How the two models are matched up
 *
 * 🔴 **Bone order is load-bearing.** `GLTFLoader.loadSkin` builds `bones`/`boneInverses` by walking
 * `skinDef.joints` in order, so `skeleton.bones[i]` is `skin.listJoints()[i]`. That is the whole
 * correspondence for joint moves and renames — no name lookup, no association table.
 *
 * ⚠️ But that loop is `if ( jointNode ) bones.push( … )`: a joint the loader failed to resolve is
 * SKIPPED, which shifts every index after it. [`patchGlb`] refuses to write rather than patch the
 * wrong joints, because a silent off-by-one here would move the wrong bones and re-bind against
 * the wrong inverses.
 *
 * Meshes are matched through `parser.associations`, which maps each three primitive to
 * `{ meshes, primitives }`.
 */

import { WebIO, type Document, type TypedArray } from '@gltf-transform/core';
import * as THREE from 'three';

import { sanitizeName } from './stage';

/** `parser.associations` values, for the entries this module reads. */
export interface GltfAssociation {
  meshes?: number;
  primitives?: number;
  materials?: number;
  textures?: number;
}

/**
 * glTF attribute semantic → the name three gives the same buffer.
 *
 * ⚠️ `JOINTS_0` must stay an integer type (`UNSIGNED_BYTE`/`UNSIGNED_SHORT`) in the file. Copying
 * three's array as-is preserves that, because the loader kept the source type; building a
 * `Float32Array` of bone indices would be silently coerced on write and any fractional value
 * becomes a WRONG BONE.
 */
const ATTRIBUTE_NAMES: Record<string, string> = {
  POSITION: 'position',
  NORMAL: 'normal',
  TANGENT: 'tangent',
  TEXCOORD_0: 'uv',
  TEXCOORD_1: 'uv1',
  COLOR_0: 'color',
  JOINTS_0: 'skinIndex',
  WEIGHTS_0: 'skinWeight'
};

/** A replaced image, located by the material and slot rather than by the texture object. */
export interface TexturePatch {
  /** The three material the editor was working through. Cloned materials inherit the original's
   *  association, so a loader-cloned material still resolves. */
  material: THREE.Material;
  slot: 'baseColor' | 'normal';
  bytes: Uint8Array;
  mimeType: string;
}

export interface RigPatch {
  /** Rewrite these primitive attributes from the live three geometry. `['WEIGHTS_0']` after a
   *  weight prune; the full list after anything that removed vertices. */
  attributes?: string[];
  /** The index buffer changed — a shell delete, a degenerate filter, an interior-face pass or a
   *  decimation. Index-only edits are the safe ones: no vertex moved, so every other attribute is
   *  still valid untouched. */
  indices?: boolean;
  /** Joints were moved. Rewrites node translations AND `inverseBindMatrices` — both halves, or the
   *  mesh and its binding disagree. */
  joints?: boolean;
  /** Canonical, colon-bearing names to write, keyed by the bone's sanitized three-side name. */
  renames?: Map<string, string>;
  /**
   * Joints to delete from the skeleton outright.
   *
   * 🔴 Requires `joints` and both skin attributes in `attributes`, and refuses without them. The
   * live scene keeps the doomed bone (its index is what every `skinIndex` element is relative to),
   * so what makes the deletion safe is that nothing REFERENCES it any more — and that is only true
   * of the file once the rebound `JOINTS_0`/`WEIGHTS_0` have been written from the live mesh.
   */
  removeJoints?: THREE.Bone[];
  /** Replaced image bytes. Everything not listed here keeps its original bytes exactly. */
  textures?: TexturePatch[];
  /** Rewrite animation sampler outputs from the live `AnimationClip`s — a corrected take. */
  animations?: THREE.AnimationClip[];
}

export interface PatchSource {
  /** The bytes the viewer fetched, kept verbatim. */
  source: ArrayBuffer;
  meshes: THREE.SkinnedMesh[];
  bones: THREE.Bone[];
  associations: Map<object, GltfAssociation>;
}

/**
 * Apply an edit to the original GLB and return the new bytes.
 *
 * Throws with a message worth showing the user rather than writing something subtly wrong — a
 * corrupted rig is far more expensive than a refused save.
 */
export async function patchGlb(from: PatchSource, patch: RigPatch): Promise<Blob> {
  const io = new WebIO();
  const doc = await io.readBinary(new Uint8Array(from.source));

  if (patch.joints || patch.renames?.size) {
    applyJoints(doc, from, patch);
  }
  if (patch.attributes?.length || patch.indices) {
    applyGeometry(doc, from, patch);
  }
  // 🔴 AFTER the geometry, never before. Removing a joint renumbers every index above it, and the
  // array that has to be renumbered is the one `applyGeometry` just wrote.
  if (patch.removeJoints?.length) {
    applyJointRemoval(doc, from, patch);
  }
  if (patch.textures?.length) {
    applyTextures(doc, from, patch.textures);
  }
  if (patch.animations?.length) {
    applyAnimations(doc, patch.animations);
  }

  const bytes = await io.writeBinary(doc);
  // A fresh copy, because the writer's buffer may be a view over a larger arena.
  return new Blob([bytes.slice()], { type: 'model/gltf-binary' });
}

function applyJoints(doc: Document, from: PatchSource, patch: RigPatch): void {
  const skins = doc.getRoot().listSkins();
  if (skins.length !== 1) {
    throw new Error(
      `this GLB has ${skins.length} skins; the editor only knows how to patch a character with one`
    );
  }
  const joints = skins[0].listJoints();
  if (joints.length !== from.bones.length) {
    // 🔴 See the header: a joint the loader could not resolve is skipped, and every index after it
    // shifts. Writing anyway would move the wrong bones.
    throw new Error(
      `the skeleton has ${from.bones.length} bones but the file has ${joints.length} joints, so ` +
        `they cannot be matched up by order. Refusing to save rather than patch the wrong joints.`
    );
  }

  for (let i = 0; i < joints.length; i++) {
    const bone = from.bones[i];
    if (patch.joints) {
      joints[i].setTranslation([bone.position.x, bone.position.y, bone.position.z]);
    }
    const canonical = patch.renames?.get(bone.name);
    if (canonical) joints[i].setName(canonical);
  }

  if (!patch.joints) return;

  // 🔴 The other half of a joint move. glTF's joint matrix is
  // `inverse(meshNodeWorld) · jointWorld · inverseBindMatrix`, and three's is
  // `bindMatrixInverse · (boneWorld · boneInverse) · bindMatrix`. Equating them gives
  // `IBM = boneInverse · bindMatrix` — the same product `GLTFExporter.processSkin` writes. Ship the
  // node translations without this and the mesh deforms; ship this without them and the retarget
  // still reads the old skeleton.
  const ibm = skins[0].getInverseBindMatrices();
  if (!ibm) {
    throw new Error('this GLB has no inverseBindMatrices to update');
  }
  const skinned = from.meshes[0];
  if (!skinned) {
    throw new Error('no skinned mesh to take a bind matrix from');
  }
  const out = new Float32Array(joints.length * 16);
  const product = new THREE.Matrix4();
  for (let i = 0; i < joints.length; i++) {
    product.copy(skinned.skeleton.boneInverses[i]).multiply(skinned.bindMatrix);
    product.toArray(out, i * 16);
  }
  ibm.setArray(out);
}

/**
 * Delete joints from the skin, the node tree and every vertex binding that indexes past them.
 *
 * Four things have to move together, and leaving any one of them out writes a file that loads:
 *
 * 1. **The skin's joint list**, which is what `JOINTS_0` indexes into.
 * 2. **`inverseBindMatrices`**, one 16-float row per joint, in the same order.
 * 3. **Every `JOINTS_0` element above a removed index**, shifted down by how many were removed
 *    below it. Skip this and every vertex above the gap is bound to its NEIGHBOUR's bone — a
 *    character that animates, wrongly, with nothing to point at.
 * 4. **The node tree**, where the doomed node's children are re-hung on its parent so a whole limb
 *    does not leave with it.
 *
 * 🔴 It refuses rather than guesses, in every case where the answer is not forced: a joint still
 * referenced by a vertex, a root joint with nowhere to put its children, a joint carrying a rotation
 * or scale (the file only receives node TRANSLATIONS from the editor, so a rotation on the joint
 * being dissolved would silently change where its children sit).
 */
function applyJointRemoval(doc: Document, from: PatchSource, patch: RigPatch): void {
  const doomed = patch.removeJoints ?? [];
  if (!doomed.length) return;
  if (!patch.joints) {
    throw new Error(
      'removing a joint re-hangs its children on its parent, so every node translation has to be ' +
        'rewritten from the live skeleton. Refusing to remove without a joint patch.'
    );
  }
  const semantics = patch.attributes ?? [];
  if (!semantics.includes('JOINTS_0') || !semantics.includes('WEIGHTS_0')) {
    throw new Error(
      'removing a joint requires the rebound JOINTS_0 and WEIGHTS_0 to be written first, or the ' +
        'file would still carry vertices bound to the joint being deleted'
    );
  }

  const skins = doc.getRoot().listSkins();
  if (skins.length !== 1) {
    throw new Error(`this GLB has ${skins.length} skins; only a single-skin character can be cut`);
  }
  const skin = skins[0];
  const joints = skin.listJoints();
  if (joints.length !== from.bones.length) {
    throw new Error(
      `the skeleton has ${from.bones.length} bones but the file has ${joints.length} joints, so ` +
        `the joint to remove cannot be identified by order`
    );
  }

  // ── which indices, and what each survivor is renumbered to ─────────────────
  const removed = new Set<number>();
  for (const bone of doomed) {
    const index = from.bones.indexOf(bone);
    if (index < 0) {
      throw new Error(`"${bone.name}" is not a joint of this skeleton, so it cannot be removed`);
    }
    removed.add(index);
  }
  const renumber = new Int32Array(joints.length);
  let survivors = 0;
  for (let i = 0; i < joints.length; i++) renumber[i] = removed.has(i) ? -1 : survivors++;
  if (!survivors) {
    throw new Error('that would remove every joint in the skeleton');
  }

  // ── 3. the bindings, before anything is renumbered under them ──────────────
  const meshes = doc.getRoot().listMeshes();
  // ⚠️ By ACCESSOR, not by mesh. Two primitives sharing one `JOINTS_0` accessor is legal, and
  // renumbering it twice would shift every index a second time — a rig that loads and is wrong.
  const renumbered = new Set<object>();
  for (const mesh of from.meshes) {
    const where = from.associations.get(mesh);
    const prim =
      where?.meshes !== undefined && where.primitives !== undefined
        ? meshes[where.meshes]?.listPrimitives()[where.primitives]
        : undefined;
    const attr = prim?.getAttribute('JOINTS_0');
    if (!attr) {
      throw new Error(
        `"${mesh.name || 'a mesh'}" has no JOINTS_0 in the file, so its bindings cannot be ` +
          `renumbered around a removed joint`
      );
    }
    if (renumbered.has(attr)) continue;
    renumbered.add(attr);
    const array = attr.getArray();
    if (!array) throw new Error('a JOINTS_0 accessor has no array to renumber');
    for (let k = 0; k < array.length; k++) {
      const index = Math.round(array[k]);
      if (index < 0 || index >= renumber.length) {
        throw new Error(`a vertex is bound to joint ${index}, which this skeleton does not have`);
      }
      const next = renumber[index];
      if (next < 0) {
        // The weight transfer is what should have emptied this, and it runs before the save. Writing
        // anyway would rebind the vertex to whichever joint inherits the index.
        throw new Error(
          `a vertex is still bound to "${from.bones[index].name}", so it cannot be removed. Move ` +
            `its weights onto another joint first.`
        );
      }
      array[k] = next;
    }
    attr.setArray(array);
  }

  // ── 4. the node tree ───────────────────────────────────────────────────────
  const parentOf = new Map<THREE.Bone, number>();
  for (let i = 0; i < from.bones.length; i++) {
    const parent = from.bones[i].parent as THREE.Bone | null;
    const at = parent ? from.bones.indexOf(parent) : -1;
    if (at >= 0) parentOf.set(from.bones[i], at);
  }
  for (const bone of doomed) {
    const index = from.bones.indexOf(bone);
    const node = joints[index];
    const parentIndex = parentOf.get(bone);
    const children = node.listChildren();
    if (parentIndex === undefined && children.length) {
      throw new Error(
        `"${bone.name}" is a root joint carrying ${children.length} child joint(s), so removing it ` +
          `would orphan them. Remove them first, or leave this one alone.`
      );
    }
    if (parentIndex !== undefined && removed.has(parentIndex)) {
      throw new Error(
        `"${bone.name}" and its parent are both being removed, which this does not attempt: the ` +
          `re-hang would depend on the order they were cut in`
      );
    }
    const rotation = node.getRotation();
    const scale = node.getScale();
    const spun =
      Math.abs(rotation[3]) < 0.999999 || rotation.slice(0, 3).some((c) => Math.abs(c) > 1e-6);
    const scaled = scale.some((c) => Math.abs(c - 1) > 1e-6);
    if (children.length && (spun || scaled)) {
      throw new Error(
        `"${bone.name}" carries a rotation or scale and has children, so re-hanging them would ` +
          `move them. Refusing rather than deforming the character.`
      );
    }
    const parentNode = parentIndex === undefined ? null : joints[parentIndex];
    for (const child of children) {
      node.removeChild(child);
      parentNode?.addChild(child);
    }
    skin.removeJoint(node);
    // Severs its remaining graph edges — the parent link included — so nothing in the written file
    // references it and the writer has no reachable node to emit.
    node.dispose();
  }

  // ── 2. one inverse-bind row per surviving joint, in the new order ──────────
  const ibm = skin.getInverseBindMatrices();
  if (!ibm) throw new Error('this GLB has no inverseBindMatrices to shrink');
  const skinned = from.meshes[0];
  if (!skinned) throw new Error('no skinned mesh to take a bind matrix from');
  const out = new Float32Array(survivors * 16);
  const product = new THREE.Matrix4();
  for (let i = 0; i < from.bones.length; i++) {
    const next = renumber[i];
    if (next < 0) continue;
    product.copy(skinned.skeleton.boneInverses[i]).multiply(skinned.bindMatrix);
    product.toArray(out, next * 16);
  }
  ibm.setArray(out);

  const left = skin.listJoints().length;
  if (left !== survivors) {
    throw new Error(
      `the skin was left with ${left} joints where ${survivors} were expected, so the file and its ` +
        `inverse bind matrices would disagree`
    );
  }
}

function applyGeometry(doc: Document, from: PatchSource, patch: RigPatch): void {
  const meshes = doc.getRoot().listMeshes();
  for (const mesh of from.meshes) {
    const where = from.associations.get(mesh);
    if (where?.meshes === undefined || where.primitives === undefined) {
      throw new Error(
        `could not locate "${mesh.name || 'a mesh'}" in the source file, so its edits cannot be ` +
          `written back`
      );
    }
    const prim = meshes[where.meshes]?.listPrimitives()[where.primitives];
    if (!prim) {
      throw new Error(`the source file has no primitive for "${mesh.name || 'a mesh'}"`);
    }

    for (const semantic of patch.attributes ?? []) {
      const threeName = ATTRIBUTE_NAMES[semantic];
      const attr = prim.getAttribute(semantic);
      const live = threeName
        ? (mesh.geometry.getAttribute(threeName) as THREE.BufferAttribute | undefined)
        : undefined;
      if (!attr || !live) {
        throw new Error(
          `cannot write ${semantic} back: the file ${attr ? 'has it' : 'has none'} and the live ` +
            `mesh ${live ? 'has it' : 'has none'}`
        );
      }
      // ⚠️ Written with the array TYPE three is holding, never converted to float. `WEIGHTS_0` and
      // `JOINTS_0` are routinely normalized `Uint8`/`Uint16` in a GLB and three keeps them that
      // way (its `setX`/`setW` normalize on the way in). Converting would quantize the weights, or
      // turn a bone index into a fraction — which is a WRONG BONE, silently.
      attr.setArray(live.array as TypedArray);
      attr.setNormalized(live.normalized);
    }

    if (patch.indices) {
      const index = mesh.geometry.getIndex();
      if (!index) {
        throw new Error(`"${mesh.name || 'a mesh'}" has no index buffer to write back`);
      }
      const target = prim.getIndices();
      if (!target) {
        throw new Error(
          `the source file's primitive is non-indexed, so it has no indices to patch`
        );
      }
      // Mutating the EXISTING accessor rather than creating one: a new accessor would have to be
      // attached to a buffer and inserted, and every one of those steps is a way to write a GLB
      // that parses but is subtly wrong.
      target.setArray(index.array as TypedArray);
    }
  }
}

/**
 * Replace image bytes for the maps that were actually edited. Everything else keeps its original
 * bytes — which is the entire point of this module.
 *
 * 🔴 Located through the MATERIAL, not the texture. three's `associations` records a texture's
 * index into the glTF `textures` array, but a gltf-transform `Texture` corresponds to a glTF
 * **image** — and `textures[i].source` is the indirection between them. Going via
 * `material.getBaseColorTexture()` skips that entirely and cannot mismatch.
 */
/**
 * Write edited keyframe values back into the file's animation samplers.
 *
 * 🔴 Matched by SANITIZED node name plus target path, not by index. three's track name is
 * `sanitizedNodeName + '.quaternion'` while the file's node still carries `mixamorig:LeftArm`, so
 * the comparison has to run through the same sanitizer the loader used — see `sanitizeName` in
 * `stage.ts`. Matching channels positionally would work right up until a clip omits a channel.
 *
 * ⚠️ A CUBICSPLINE sampler stores THREE values per key (`[inTangent, vertex, outTangent]`), so its
 * output accessor is 3× longer. Copying three's array wholesale is still correct because the layout
 * is identical — but only because `motion-repair` rotates all three, which is the mathematically
 * right thing rather than a shortcut: left-multiplying by a unit quaternion is linear and
 * orthogonal, so it carries the tangents exactly.
 */
function applyAnimations(doc: Document, clips: THREE.AnimationClip[]): void {
  const animations = doc.getRoot().listAnimations();
  const byName = new Map(animations.map((a) => [a.getName(), a]));

  for (const clip of clips) {
    const animation = byName.get(clip.name) ?? animations[clips.indexOf(clip)];
    if (!animation) continue;
    for (const channel of animation.listChannels()) {
      const node = channel.getTargetNode();
      const path = channel.getTargetPath();
      const output = channel.getSampler()?.getOutput();
      if (!node || !path || !output) continue;
      const property =
        path === 'rotation' ? 'quaternion' : path === 'translation' ? 'position' : path;
      const trackName = `${sanitizeName(node.getName())}.${property}`;
      const track = clip.tracks.find((t) => t.name === trackName);
      if (!track) continue;
      if (track.values.length !== output.getArray()?.length) {
        throw new Error(
          `"${trackName}" has ${track.values.length} values but the file's sampler has ` +
            `${output.getArray()?.length ?? 0}; refusing to write a mismatched animation`
        );
      }
      output.setArray(Float32Array.from(track.values));
    }
  }
}

function applyTextures(doc: Document, from: PatchSource, patches: TexturePatch[]): void {
  const materials = doc.getRoot().listMaterials();
  for (const patch of patches) {
    const where = from.associations.get(patch.material);
    if (where?.materials === undefined) {
      throw new Error(
        `could not locate the material for the edited ${patch.slot} map in the source file`
      );
    }
    const material = materials[where.materials];
    const texture =
      patch.slot === 'baseColor' ? material?.getBaseColorTexture() : material?.getNormalTexture();
    if (!texture) {
      throw new Error(`the source material has no ${patch.slot} map to replace`);
    }
    texture.setImage(patch.bytes);
    texture.setMimeType(patch.mimeType);
  }
}
