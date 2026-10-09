/*
 * Cheap, targeted repairs for a GLB character out of an image-to-3D pipeline.
 *
 * The asset is not a clean mesh and never will be: ~200 interpenetrating shells rather than one
 * solid, stray floating islands, and (on one DC remesh) ~13,000 disconnected components. The goal
 * here is emphatically NOT a modelling tool — it is "delete the obvious garbage without a whole
 * adventure", on a SkinnedMesh whose skin weights must survive bit-identical.
 *
 * # The single organising idea: index-only edits
 *
 * Every operation in this file except [`compactVertices`] returns a NEW INDEX BUFFER OVER THE SAME
 * VERTEX ARRAY. No vertex moves, no attribute is rewritten, so `skinIndex`/`skinWeight`/`uv` cannot
 * be damaged — not "are unlikely to be", *cannot be*, because nothing touches them. That is why
 * this is the default shape of the API and why [`compactVertices`] is opt-in and quarantined at
 * the bottom.
 *
 * ## 🔴 Why not the obvious three.js tools
 *
 * - `SimplifyModifier` `deleteAttribute`s anything outside position/uv/normal/tangent/color. Point
 *   it at a SkinnedMesh and it silently destroys `skinIndex`/`skinWeight`; the mesh then renders
 *   collapsed at the origin with no error anywhere.
 * - `BufferGeometryUtils` has no connected-component pass and no vertex-removal pass. Its complete
 *   export list is `computeMikkTSpaceTangents, mergeGeometries, mergeAttributes,
 *   deepCloneAttribute, interleaveAttributes, deinterleaveAttribute, deinterleaveGeometry,
 *   estimateBytesUsed, mergeVertices, toTrianglesDrawMode, computeMorphedAttributes, mergeGroups,
 *   toCreasedNormals`. Nothing there does what this file does.
 * - `mergeVertices` is NOT a shell merger. It hashes *every* attribute — uv, normal, skinIndex,
 *   skinWeight — so it only collapses vertices that are identical in all of them. It will not merge
 *   the 200 shells, and on a seam-split character it barely merges anything at all.
 *
 * ## 🔴 Bind space vs posed space
 *
 * `geometry.attributes.position` is in BIND space. `SkinnedMesh.raycast` reports POSED positions,
 * because `SkinnedMesh.getVertexPosition` runs `applyBoneTransform` on the way out. At rest pose
 * the two coincide; away from rest pose they do not, and a face id picked from a posed raycast
 * still maps correctly (face ids are topological) but any *distance* or *bounds* number in this
 * file is a bind-space number. Everything here assumes the mesh is at rest pose. Put the rig in
 * rest pose before running [`interiorFaces`] or the viewpoint sphere is wrapped around the wrong
 * shape.
 *
 * ## ⚠️ Two bounding volumes, not one
 *
 * In r185 `SkinnedMesh` carries its OWN `boundingBox`/`boundingSphere`, separate from the
 * geometry's, and `SkinnedMesh.raycast` tests `this.boundingSphere` first and then
 * `geometry.boundingBox`. `GLTFLoader` seeds `geometry.boundingBox`/`boundingSphere` straight from
 * the glTF accessor min/max, so they are stale the instant anything changes. Every mutation here
 * routes through [`refreshBounds`], which refreshes BOTH. Miss one and picking goes dead in a
 * region of the model with no error and no visual clue.
 *
 * @see glb-patch.ts for how an edit made here gets back into the file without a re-export.
 */

import * as THREE from 'three';
import { MeshBVH } from 'three-mesh-bvh';

/** What an index buffer is allowed to be on the way out. See [`allocIndex`] for the Uint16 rule. */
export type IndexArray = Uint16Array | Uint32Array;

/**
 * The typed arrays a `BufferAttribute` can actually hold in a GLB. Declared locally rather than
 * imported because @types/three's `TypedArray` alias has moved between releases and this file has
 * to compile off-server on the first try.
 */
type AttributeArray =
  | Int8Array
  | Uint8Array
  | Uint8ClampedArray
  | Int16Array
  | Uint16Array
  | Int32Array
  | Uint32Array
  | Float32Array
  | Float64Array;

export interface MeshComponent {
  id: number;
  triangles: number;
  vertices: number;
  /** Bounding-box diagonal, and as a fraction of the whole mesh's diagonal. */
  diagonal: number;
  sizeRatio: number;
  center: [number, number, number];
}

export interface ComponentLabels {
  components: MeshComponent[];
  /** Component id per triangle, indexed by faceIndex — what a raycast hit maps through. */
  faceComponent: Uint32Array;
}

export interface LabelOptions {
  /**
   * Union vertices that share a position before walking the index buffer. Default `true`.
   *
   * 🔴 Leave this on. A GLB out of an image-to-3D pipeline is seam-split: every UV seam, every hard
   * normal crease and every skin-influence boundary duplicates its vertices, so two triangles that
   * are physically welded can share NO index value. Labelling on the raw index buffer therefore
   * reports a shredded mesh — that is very likely where "13,000 disconnected components" came from,
   * and turning welding off will reproduce it. meshoptimizer agrees: `meshopt_simplifyPrune` runs
   * `buildPositionRemap` before `buildComponents` for exactly this reason, so welding here is also
   * what makes [`labelComponents`] and [`pruneIsolated`] talk about the same components.
   */
  weld?: boolean;
  /**
   * World-space cell size for the positional weld. Default `diagonal * 1e-6`.
   *
   * ⚠️ This is a quantisation grid, not a true radius: two points a hair apart can still land in
   * different cells. That is deliberate and fine, because the duplicates we care about are
   * bit-identical floats written by the exporter. Do NOT raise this hoping to merge the 200 shells
   * — a loose grid welds the two faces of a thin sleeve into one component and the UI then offers
   * you "delete the shirt".
   */
  tolerance?: number;
}

export interface InteriorOptions {
  /**
   * Viewpoints on the surrounding sphere. Default 64.
   *
   * ⚠️ This is the accuracy/runtime knob and the failure is asymmetric. Too few viewpoints and
   * deeply concave regions — armpits, under the chin, between the fingers, inside the ear — are
   * never reached by any ray, get classified interior, and applying the result punches visible
   * holes in the model. Too many and you only pay time. 64 is deliberately conservative; below ~24
   * expect holes.
   */
  viewpoints?: number;
  /**
   * Ray `near` distance, to skip the face the ray starts on. Default `diagonal * 1e-5`.
   *
   * Without it every face occludes itself at t≈0 and the whole mesh reads as interior. Raising it
   * lets rays escape through genuinely thin geometry and mislabels real surface as interior.
   */
  epsilon?: number;
  /**
   * Cancellation, checked once per chunk — so a click on Cancel lands within [`CHUNK_BUDGET_MS`],
   * not at the end of some face's 64 viewpoints.
   *
   * ⚠️ On abort the promise REJECTS with the signal's reason (a `DOMException` named `AbortError`
   * unless the caller passed its own to `abort()`); it does not resolve with what it had. That is
   * deliberately NOT `frame-grab.ts`'s "return the partial result" idiom: a half-finished pass
   * yields a strict subset of the interior faces that is indistinguishable from a complete answer,
   * and the UI would then offer "N faces are buried" about a number that means nothing. A catching
   * caller should test `signal.aborted` rather than sniff the error — the reason is whatever the
   * caller handed `abort()`.
   */
  signal?: AbortSignal;
  /**
   * Faces classified / faces total. Fired once at 0, once per chunk, and once at completion —
   * enough to drive a determinate bar.
   *
   * ⚠️ `done` counts faces CLASSIFIED, not work done, and the two are only loosely related: an
   * exterior face costs one or two rays and an interior one costs all [`viewpoints`]. The bar
   * therefore crawls through a buried region and sprints across open surface. That is the honest
   * reading of the same asymmetry [`viewpoints`] documents, and still far better than a spinner.
   */
  onProgress?: (done: number, total: number) => void;
  /**
   * Main-thread budget per chunk in ms. Default [`CHUNK_BUDGET_MS`]; clamped to 1–100, and a
   * non-finite value falls back to the default rather than producing a `NaN` deadline that never
   * expires — which would be exactly the freeze this whole mechanism exists to remove.
   */
  budgetMs?: number;
}

/* ------------------------------------------------------------------------------------------- */
/* Shared plumbing                                                                              */
/* ------------------------------------------------------------------------------------------- */

/**
 * Type guard rather than a cast, so the narrowed branches give up `data`/`offset` and `array`
 * without any `as`. `isInterleavedBufferAttribute` is the flag three itself brands instances with;
 * `instanceof` would fail across the two copies of three that a bundler occasionally ends up with.
 */
function isInterleaved(
  attr: THREE.BufferAttribute | THREE.InterleavedBufferAttribute
): attr is THREE.InterleavedBufferAttribute {
  return (attr as THREE.InterleavedBufferAttribute).isInterleavedBufferAttribute === true;
}

function requireIndex(geometry: THREE.BufferGeometry): THREE.BufferAttribute {
  const index = geometry.getIndex();
  if (index === null) {
    throw new Error(
      'This mesh has no index buffer, so it has no triangle identity to repair. Re-export it as ' +
        'an indexed mesh.'
    );
  }
  // ⚠️ @types/three declares `getIndex(): BufferAttribute | null`, but nothing at runtime enforces
  // that. three-mesh-bvh throws outright on an interleaved index and every loop below reads
  // `index.array` directly, so check rather than trust the declaration.
  const branded = index as unknown as { isInterleavedBufferAttribute?: boolean };
  if (branded.isInterleavedBufferAttribute) {
    throw new Error('Interleaved index buffers are not supported.');
  }
  if (index.count % 3 !== 0) {
    throw new Error(
      `Index buffer holds ${index.count} entries, which is not a whole number of triangles. This ` +
        'is not a triangle mesh.'
    );
  }
  return index;
}

function requirePositionAttribute(
  geometry: THREE.BufferGeometry
): THREE.BufferAttribute | THREE.InterleavedBufferAttribute {
  const position = geometry.getAttribute('position');
  if (!position) throw new Error('This mesh has no position attribute.');
  if (position.itemSize !== 3) {
    throw new Error(`Expected 3-component positions, found ${position.itemSize} components.`);
  }
  return position;
}

/**
 * Positions as a tight `Float32Array` of `count * 3`.
 *
 * ⚠️ This is the ONE place `getX/getY/getZ` are the right tool, and it is the exact opposite of the
 * rule that governs [`compactVertices`]. There we must never round-trip through the accessors,
 * because they denormalize on read and renormalize on write and that quantises skin weights. Here
 * we *want* the denormalized value: under `KHR_mesh_quantization` a position attribute is a
 * normalized Int16, and the decoded float is what three actually renders and what meshoptimizer
 * needs. Copying its raw bits would give us integers in the wrong space.
 *
 * The common case — a plain non-normalized `Float32Array` — is aliased with `subarray`, not copied,
 * to avoid a pointless 2.6 MB allocation at 220k verts. Nothing in this file writes through it.
 */
function readPositions(geometry: THREE.BufferGeometry): Float32Array {
  const attr = requirePositionAttribute(geometry);
  const count = attr.count;

  if (!isInterleaved(attr) && attr.array instanceof Float32Array && !attr.normalized) {
    return attr.array.subarray(0, count * 3);
  }

  const out = new Float32Array(count * 3);
  for (let i = 0; i < count; i++) {
    out[i * 3] = attr.getX(i);
    out[i * 3 + 1] = attr.getY(i);
    out[i * 3 + 2] = attr.getZ(i);
  }
  return out;
}

interface Bounds {
  min: [number, number, number];
  max: [number, number, number];
  diagonal: number;
}

function boundsOf(positions: Float32Array): Bounds {
  let minX = Infinity;
  let minY = Infinity;
  let minZ = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  let maxZ = -Infinity;
  for (let i = 0; i < positions.length; i += 3) {
    const x = positions[i];
    const y = positions[i + 1];
    const z = positions[i + 2];
    if (x < minX) minX = x;
    if (y < minY) minY = y;
    if (z < minZ) minZ = z;
    if (x > maxX) maxX = x;
    if (y > maxY) maxY = y;
    if (z > maxZ) maxZ = z;
  }
  if (!Number.isFinite(minX)) {
    throw new Error('The mesh has no finite vertex positions; it is empty or corrupt.');
  }
  const dx = maxX - minX;
  const dy = maxY - minY;
  const dz = maxZ - minZ;
  return {
    min: [minX, minY, minZ],
    max: [maxX, maxY, maxZ],
    diagonal: Math.sqrt(dx * dx + dy * dy + dz * dz)
  };
}

/**
 * Allocate an index buffer of `length`, downcast to `Uint16Array` when the vertex count allows.
 *
 * This is a file-size decision, not a correctness one: halving the index accessor halves the
 * largest non-texture chunk in the GLB, and at the 15k/40k LOD tiers that is most of the file. It
 * costs nothing at render time — WebGL2 supports 32-bit indices unconditionally.
 *
 * ⚠️ The cutoff is 65535 vertices, not 65536. three's own `arrayNeedsUint32` reserves 65535 for
 * `PRIMITIVE_RESTART_FIXED_INDEX` (mrdoob/three.js#24565), and matching it means an index we build
 * here and an index three builds from the same data agree on type.
 *
 * ⚠️ Because the index-only operations do not renumber vertices, a 220k-vertex mesh stays Uint32
 * however much you delete. The downcast only pays after [`compactVertices`].
 */
function allocIndex(vertexCount: number, length: number): IndexArray {
  return vertexCount <= 65535 ? new Uint16Array(length) : new Uint32Array(length);
}

/* ------------------------------------------------------------------------------------------- */
/* Union-find                                                                                   */
/* ------------------------------------------------------------------------------------------- */

// Iterative, path-compressing, union-by-rank, over typed arrays. Recursion would blow the stack on
// a 13,000-component mesh where chains get long before compression kicks in.
function findRoot(parent: Uint32Array, start: number): number {
  let root = start;
  while (parent[root] !== root) root = parent[root];
  let walk = start;
  while (parent[walk] !== root) {
    const next = parent[walk];
    parent[walk] = root;
    walk = next;
  }
  return root;
}

function unite(parent: Uint32Array, rank: Uint8Array, a: number, b: number): void {
  const ra = findRoot(parent, a);
  const rb = findRoot(parent, b);
  if (ra === rb) return;
  if (rank[ra] < rank[rb]) {
    parent[ra] = rb;
  } else if (rank[ra] > rank[rb]) {
    parent[rb] = ra;
  } else {
    parent[rb] = ra;
    rank[ra]++;
  }
}

/**
 * Map each vertex to the lowest-numbered vertex sharing its quantised position.
 *
 * Open-addressed typed-array hash rather than a `Map<string, number>`: at 220k vertices the string
 * version allocates 220k keys and the GC pause is longer than the whole rest of the pass.
 *
 * ⚠️ A NaN coordinate quantises to 0 and welds every NaN vertex into one blob. That is a degenerate
 * asset either way, and the alternative — throwing — would block repairing a mesh whose only fault
 * is a handful of NaNs in a region you are about to delete.
 */
function buildPositionWeld(positions: Float32Array, count: number, cell: number): Uint32Array {
  const q = new Int32Array(count * 3);
  const inv = 1 / cell;
  for (let i = 0; i < count * 3; i++) q[i] = Math.round(positions[i] * inv) | 0;

  let capacity = 8;
  while (capacity < count * 2) capacity *= 2;
  const mask = capacity - 1;
  const table = new Int32Array(capacity).fill(-1);
  const rep = new Uint32Array(count);

  for (let v = 0; v < count; v++) {
    const x = q[v * 3];
    const y = q[v * 3 + 1];
    const z = q[v * 3 + 2];
    // `^` yields a signed int32; `& mask` with a positive mask forces it non-negative.
    let slot = (Math.imul(x, 73856093) ^ Math.imul(y, 19349663) ^ Math.imul(z, 83492791)) & mask;
    for (;;) {
      const occupant = table[slot];
      if (occupant === -1) {
        table[slot] = v;
        rep[v] = v;
        break;
      }
      if (q[occupant * 3] === x && q[occupant * 3 + 1] === y && q[occupant * 3 + 2] === z) {
        rep[v] = occupant;
        break;
      }
      slot = (slot + 1) & mask;
    }
  }
  return rep;
}

/* ------------------------------------------------------------------------------------------- */
/* labelComponents                                                                              */
/* ------------------------------------------------------------------------------------------- */

/**
 * Union-find over the index buffer (plus a positional weld — see [`LabelOptions.weld`]).
 *
 * Budget: ~50–150 ms at 220k verts / 440k tris on a desktop browser. It is synchronous and it will
 * block the frame; call it on an explicit user action, not on hover, and do not call it per-frame.
 *
 * Components come back sorted by triangle count DESCENDING, so `components[0]` is the character
 * body and the tail is the junk. That ordering is the whole point — the UI can offer "keep the
 * largest N" without a heuristic. `faceComponent` is remapped to match, so ids are stable against
 * this array and only this array.
 *
 * ⚠️ The returned labels are indexed by faceIndex and are invalidated by ANY change to the index
 * buffer — including a `MeshBVH` built without `indirect: true`, which permutes the index in place.
 * See [`getBvh`].
 */
export function labelComponents(
  geometry: THREE.BufferGeometry,
  options: LabelOptions = {}
): ComponentLabels {
  const index = requireIndex(geometry);
  const idx = index.array as ArrayLike<number>;
  const triCount = index.count / 3;
  const positions = readPositions(geometry);
  const vertexCount = positions.length / 3;
  const bounds = boundsOf(positions);

  // Out-of-range indices are the one failure that would silently corrupt everything downstream:
  // typed-array reads past the end return `undefined`, which becomes NaN, which becomes vertex 0.
  // One cheap pass buys a real error message instead of a mesh that quietly loses its head.
  for (let i = 0; i < index.count; i++) {
    if (idx[i] >= vertexCount) {
      throw new Error(
        `Index buffer references vertex ${idx[i]} but the mesh only has ${vertexCount}. The file ` +
          'is corrupt.'
      );
    }
  }

  const parent = new Uint32Array(vertexCount);
  for (let v = 0; v < vertexCount; v++) parent[v] = v;
  const rank = new Uint8Array(vertexCount);

  if (options.weld !== false) {
    const cell = Math.max(options.tolerance ?? bounds.diagonal * 1e-6, Number.MIN_VALUE);
    const weld = buildPositionWeld(positions, vertexCount, cell);
    for (let v = 0; v < vertexCount; v++) {
      if (weld[v] !== v) unite(parent, rank, v, weld[v]);
    }
  }

  for (let f = 0; f < triCount; f++) {
    const a = idx[f * 3];
    const b = idx[f * 3 + 1];
    const c = idx[f * 3 + 2];
    unite(parent, rank, a, b);
    unite(parent, rank, b, c);
  }

  // Only roots that actually carry a triangle become components. Vertices referenced by nothing are
  // dead weight in the buffer, not "components" — surfacing them in the UI as deletable objects
  // that do not render would be pure confusion. [`compactVertices`] is what removes them.
  const idOfRoot = new Int32Array(vertexCount).fill(-1);
  const faceComponentRaw = new Uint32Array(triCount);
  const referenced = new Uint8Array(vertexCount);
  const triCounts: number[] = [];
  let componentCount = 0;

  for (let f = 0; f < triCount; f++) {
    const a = idx[f * 3];
    const b = idx[f * 3 + 1];
    const c = idx[f * 3 + 2];
    referenced[a] = 1;
    referenced[b] = 1;
    referenced[c] = 1;
    const root = findRoot(parent, a);
    let id = idOfRoot[root];
    if (id < 0) {
      id = componentCount++;
      idOfRoot[root] = id;
      triCounts.push(0);
    }
    faceComponentRaw[f] = id;
    triCounts[id]++;
  }

  const vertCounts = new Uint32Array(componentCount);
  const boxes = new Float32Array(componentCount * 6);
  for (let c = 0; c < componentCount; c++) {
    boxes[c * 6] = Infinity;
    boxes[c * 6 + 1] = Infinity;
    boxes[c * 6 + 2] = Infinity;
    boxes[c * 6 + 3] = -Infinity;
    boxes[c * 6 + 4] = -Infinity;
    boxes[c * 6 + 5] = -Infinity;
  }
  for (let v = 0; v < vertexCount; v++) {
    if (referenced[v] === 0) continue;
    const id = idOfRoot[findRoot(parent, v)];
    if (id < 0) continue;
    vertCounts[id]++;
    const x = positions[v * 3];
    const y = positions[v * 3 + 1];
    const z = positions[v * 3 + 2];
    const o = id * 6;
    if (x < boxes[o]) boxes[o] = x;
    if (y < boxes[o + 1]) boxes[o + 1] = y;
    if (z < boxes[o + 2]) boxes[o + 2] = z;
    if (x > boxes[o + 3]) boxes[o + 3] = x;
    if (y > boxes[o + 4]) boxes[o + 4] = y;
    if (z > boxes[o + 5]) boxes[o + 5] = z;
  }

  // Sort by triangle count descending, then renumber. Ties break on the original id so the ordering
  // is deterministic across runs — the UI stores component ids in its selection state and a
  // reshuffle between two identical labelling passes would move the user's checkboxes.
  const order = new Uint32Array(componentCount);
  for (let c = 0; c < componentCount; c++) order[c] = c;
  const sorted = Array.from(order).sort((a, b) => triCounts[b] - triCounts[a] || a - b);
  const newIdOf = new Uint32Array(componentCount);
  for (let newId = 0; newId < componentCount; newId++) newIdOf[sorted[newId]] = newId;

  const faceComponent = new Uint32Array(triCount);
  for (let f = 0; f < triCount; f++) faceComponent[f] = newIdOf[faceComponentRaw[f]];

  const whole = bounds.diagonal || 1;
  const components: MeshComponent[] = sorted.map((oldId, newId) => {
    const o = oldId * 6;
    const dx = boxes[o + 3] - boxes[o];
    const dy = boxes[o + 4] - boxes[o + 1];
    const dz = boxes[o + 5] - boxes[o + 2];
    const diagonal = Math.sqrt(dx * dx + dy * dy + dz * dz);
    return {
      id: newId,
      triangles: triCounts[oldId],
      vertices: vertCounts[oldId],
      diagonal,
      sizeRatio: diagonal / whole,
      center: [
        (boxes[o] + boxes[o + 3]) / 2,
        (boxes[o + 1] + boxes[o + 4]) / 2,
        (boxes[o + 2] + boxes[o + 5]) / 2
      ]
    };
  });

  return { components, faceComponent };
}

/**
 * `intersection.faceIndex` → component id, or `-1` when there is no hit.
 *
 * ⚠️ three types `Intersection.faceIndex` as `number | undefined` — it is undefined for points,
 * lines and sprites — so this takes the optional directly rather than making every call site do
 * the narrowing and get it wrong once.
 */
export function componentAtFace(
  labels: ComponentLabels,
  faceIndex: number | undefined | null
): number {
  if (faceIndex === undefined || faceIndex === null) return -1;
  if (faceIndex < 0 || faceIndex >= labels.faceComponent.length) return -1;
  return labels.faceComponent[faceIndex];
}

/* ------------------------------------------------------------------------------------------- */
/* Index-only edits                                                                             */
/* ------------------------------------------------------------------------------------------- */

/**
 * Drop whole components. Returns a NEW index array over the SAME vertices.
 *
 * This is the safe operation and should be the default in the UI: no attribute is read, written or
 * reallocated, so skin weights and UVs are untouched by construction. The orphaned vertices stay in
 * the buffer costing file size and nothing else — run [`compactVertices`] later if that matters.
 *
 * Triangle order is preserved for the survivors, which is what lets the caller re-label cheaply and
 * keeps diffs against the original accessor small.
 */
export function removeComponents(
  geometry: THREE.BufferGeometry,
  labels: ComponentLabels,
  drop: ReadonlySet<number>
): IndexArray {
  const index = requireIndex(geometry);
  const idx = index.array as ArrayLike<number>;
  const triCount = index.count / 3;
  if (labels.faceComponent.length !== triCount) {
    throw new Error(
      'These component labels were computed for a different index buffer. Re-run the component ' +
        'scan before deleting.'
    );
  }

  const vertexCount = requirePositionAttribute(geometry).count;
  const out = allocIndex(vertexCount, index.count);
  let n = 0;
  for (let f = 0; f < triCount; f++) {
    if (drop.has(labels.faceComponent[f])) continue;
    out[n++] = idx[f * 3];
    out[n++] = idx[f * 3 + 1];
    out[n++] = idx[f * 3 + 2];
  }
  if (n === 0) {
    throw new Error('That selection would delete every triangle in the mesh.');
  }
  return out.slice(0, n) as IndexArray;
}

/**
 * Drop zero-area and repeated-index triangles. Index-only.
 *
 * Two distinct defects with one cause. A repeated index (`a === b`) is a triangle the exporter
 * emitted with a collapsed edge; a zero-area triangle has three distinct indices at three identical
 * (or collinear) positions. Both render as nothing, both break tangent generation with a divide by
 * zero, and both survive every "simplify" pass because they cost no error.
 *
 * ⚠️ The area threshold is RELATIVE to the mesh diagonal, not absolute. An absolute epsilon is a
 * bug waiting for the first asset authored in centimetres: the same threshold that catches slivers
 * on a 2-unit character eats real triangles on a 200-unit one.
 *
 * ⚠️ The effective bar is an edge of `√2 · diagonal · 1e-6`, not `diagonal * 1e-6` as this comment
 * used to say. `minCross2` compares `|e1 × e2|²` against `4·minEdge⁴`, i.e. `|cross| > 2·minEdge²`,
 * while a right triangle with both legs exactly `minEdge` has `|cross| = minEdge²` — so that
 * triangle is dropped. The factor is real and now pinned by a test; it is still orders of magnitude
 * below anything a 220k-vertex character legitimately contains, which is why this is a documentation
 * fix rather than a behaviour change.
 */
export function removeDegenerateTriangles(geometry: THREE.BufferGeometry): IndexArray {
  const index = requireIndex(geometry);
  const idx = index.array as ArrayLike<number>;
  const triCount = index.count / 3;
  const positions = readPositions(geometry);
  const vertexCount = positions.length / 3;
  const minEdge = boundsOf(positions).diagonal * 1e-6;
  // |edge1 × edge2| is twice the area, in squared-length units; compare squared to skip the sqrt.
  const minCross2 = (2 * minEdge * minEdge) ** 2;

  const out = allocIndex(vertexCount, index.count);
  let n = 0;
  for (let f = 0; f < triCount; f++) {
    const a = idx[f * 3];
    const b = idx[f * 3 + 1];
    const c = idx[f * 3 + 2];
    if (a === b || b === c || a === c) continue;

    const ax = positions[a * 3];
    const ay = positions[a * 3 + 1];
    const az = positions[a * 3 + 2];
    const b1 = positions[b * 3] - ax;
    const b2 = positions[b * 3 + 1] - ay;
    const b3 = positions[b * 3 + 2] - az;
    const c1 = positions[c * 3] - ax;
    const c2 = positions[c * 3 + 1] - ay;
    const c3 = positions[c * 3 + 2] - az;
    const nx = b2 * c3 - b3 * c2;
    const ny = b3 * c1 - b1 * c3;
    const nz = b1 * c2 - b2 * c1;
    const cross2 = nx * nx + ny * ny + nz * nz;
    if (!(cross2 > minCross2)) continue; // `!(x > y)` also rejects NaN, which `<=` would keep.

    out[n++] = a;
    out[n++] = b;
    out[n++] = c;
  }
  if (n === 0) {
    throw new Error('Every triangle in this mesh is degenerate; there is nothing to keep.');
  }
  return out.slice(0, n) as IndexArray;
}

/* ------------------------------------------------------------------------------------------- */
/* Rig damage — the one thing an index-only edit CAN still break                                */
/* ------------------------------------------------------------------------------------------- */

export interface JointStarvation {
  /**
   * Joint ids — indices into `SkinnedMesh.skeleton.bones` — that drove at least one drawn vertex
   * before the edit and drive none after it.
   */
  starved: number[];
  /** How many joint slots the skin attribute actually uses. 0 when the mesh is not skinned. */
  joints: number;
  /** False when there is no `skinIndex`/`skinWeight` pair to read, or it is unreadable. */
  checked: boolean;
}

/**
 * Which joints the edit left driving nothing.
 *
 * ## What this is for
 *
 * The index-only discipline guarantees no skin weight is CORRUPTED. It guarantees nothing about a
 * joint still having geometry to move: drop enough triangles and a bone is still in the skeleton,
 * still animated by every clip in the file, and attached to no drawn vertex at all. Nothing throws,
 * nothing warns, and the model simply stops moving in that region. That is the one damage mode
 * [`decimate`] and [`removeComponents`] genuinely share, so it is worth the one extra pass.
 *
 * Applies to any index-only edit, not just decimation — deleting a shell starves joints the same
 * way, which is why this takes two index buffers rather than living inside [`decimate`].
 *
 * ## ⚠️ What it CANNOT tell you, and these matter
 *
 * - **It is binary.** A joint that kept one vertex out of four thousand is not reported, and its
 *   deformation is ruined. Skin quality degrades continuously; this only catches the cliff.
 * - **A starved joint is not necessarily a fault.** Leaf/helper bones legitimately drive nothing,
 *   and a joint whose only geometry was the floating island you just deleted *should* go empty.
 *   This is a "look at this", not an error, and the UI must present it that way.
 * - **No names.** A `BufferGeometry` does not know its skeleton, so these are joint ids; the caller
 *   maps them through `skeleton.bones[id].name`.
 * - **One influence set only.** three reads `JOINTS_0`/`WEIGHTS_0` and nothing else, so on an
 *   8-influence asset the second set is already gone before this function sees anything.
 * - **Nothing about animation.** A mesh whose every joint kept a vertex can still deform visibly
 *   worse after decimation. Measuring that needs the clips and a rendered comparison.
 *
 * 🔴 A zero weight is not an influence. A GLB pads unused slots with joint 0 and weight 0, so
 * counting joint ids without testing the weight reports the root bone as driving the entire mesh
 * and every other joint as starved the moment anything is deleted — i.e. exactly backwards.
 */
export function starvedJoints(
  geometry: THREE.BufferGeometry,
  before: ArrayLike<number>,
  after: ArrayLike<number>
): JointStarvation {
  const jointAttr = geometry.getAttribute('skinIndex');
  const weightAttr = geometry.getAttribute('skinWeight');
  if (!jointAttr || !weightAttr) return { starved: [], joints: 0, checked: false };
  if (jointAttr.itemSize !== 4 || weightAttr.itemSize !== 4) {
    return { starved: [], joints: 0, checked: false };
  }
  // ⚠️ glTF forbids a normalized JOINTS_0, and `BufferAttribute.getComponent` denormalizes when the
  // flag is set — so a file that sets it anyway would hand back joint 7 as 0.027 and every id would
  // collapse to 0. Refuse to guess rather than report a confident wrong answer.
  if (jointAttr.normalized) return { starved: [], joints: 0, checked: false };

  const vertexCount = jointAttr.count;
  let maxJoint = -1;
  for (let v = 0; v < vertexCount; v++) {
    for (let k = 0; k < 4; k++) {
      if (weightAttr.getComponent(v, k) <= 0) continue;
      const j = jointAttr.getComponent(v, k);
      if (j > maxJoint) maxJoint = j;
    }
  }
  if (maxJoint < 0) return { starved: [], joints: 0, checked: false };

  const joints = maxJoint + 1;
  const drivenBefore = new Uint8Array(joints);
  const drivenAfter = new Uint8Array(joints);

  const mark = (indices: ArrayLike<number>, driven: Uint8Array): void => {
    const seen = new Uint8Array(vertexCount);
    for (let i = 0; i < indices.length; i++) {
      const v = indices[i];
      // Out of range means a corrupt buffer, and this is a diagnostic — skip rather than throw and
      // take down an edit that the rest of the pipeline is about to reject with a real message.
      if (v >= vertexCount || seen[v] === 1) continue;
      seen[v] = 1;
      for (let k = 0; k < 4; k++) {
        if (weightAttr.getComponent(v, k) <= 0) continue;
        const j = jointAttr.getComponent(v, k);
        if (j < joints) driven[j] = 1;
      }
    }
  };
  mark(before, drivenBefore);
  mark(after, drivenAfter);

  const starved: number[] = [];
  for (let j = 0; j < joints; j++) {
    if (drivenBefore[j] === 1 && drivenAfter[j] === 0) starved.push(j);
  }
  return { starved, joints, checked: true };
}

/* ------------------------------------------------------------------------------------------- */
/* meshoptimizer                                                                                */
/* ------------------------------------------------------------------------------------------- */

/** The slice of `MeshoptSimplifier` this file uses, declared locally — see [`loadSimplifier`]. */
interface SimplifierApi {
  supported: boolean;
  ready: Promise<void>;
  simplifyPrune(
    indices: Uint32Array,
    vertexPositions: Float32Array,
    vertexPositionsStride: number,
    targetError: number
  ): Uint32Array;
  /** `[newIndex, achievedError]`. The error is RELATIVE — see [`SimplifierApi.getScale`]. */
  simplify(
    indices: Uint32Array,
    vertexPositions: Float32Array,
    vertexPositionsStride: number,
    targetIndexCount: number,
    targetError: number
  ): [Uint32Array, number];
  simplifyWithAttributes(
    indices: Uint32Array,
    vertexPositions: Float32Array,
    vertexPositionsStride: number,
    vertexAttributes: Float32Array,
    vertexAttributesStride: number,
    attributeWeights: number[],
    vertexLock: Uint8Array | null,
    targetIndexCount: number,
    targetError: number
  ): [Uint32Array, number];
  /** Multiply a relative error by this to get one in the mesh's own units. */
  getScale(vertexPositions: Float32Array, vertexPositionsStride: number): number;
}

/**
 * 🔴 `simplifyWithUpdate` is deliberately NOT declared here, and adding it would break the one
 * property this whole file rests on. It rewrites `vertex_positions` and `vertex_attributes` IN
 * PLACE — which for us means writing through the `subarray` alias [`readPositions`] hands out,
 * straight into the live `position` attribute — and it only updates the attributes you passed it.
 * `skinIndex`/`skinWeight`, which you cannot pass (they are not a smoothly interpolatable
 * quantity), would then be describing vertices that have moved somewhere else. Use [`decimate`].
 */

/**
 * Load and initialise the meshopt WASM simplifier.
 *
 * The import is dynamic so a browser that cannot instantiate the module fails at the moment the
 * user asks for pruning, with a message, rather than taking the whole 3D panel down at import time.
 *
 * ⚠️ Under a strict CSP this needs `script-src 'wasm-unsafe-eval'`. Without it
 * `WebAssembly.instantiate` throws a `CompileError` and you will be looking at the wrong layer for
 * an hour. `MeshoptSimplifier.supported` is a feature probe, not a CSP probe — it can be `true` and
 * instantiation can still be blocked, which is why the `await ready` is inside the try.
 *
 * ⚠️ The shape is asserted through `unknown` rather than trusted from the package's own types. Its
 * `exports` map does publish `./simplifier` types, but this project cannot compile to check, and a
 * local interface that is wrong fails loudly at the call site instead of subtly at runtime.
 */
async function loadSimplifier(): Promise<SimplifierApi> {
  let simplifier: SimplifierApi;
  try {
    const mod = await import('meshoptimizer/simplifier');
    simplifier = (mod as unknown as { MeshoptSimplifier: SimplifierApi }).MeshoptSimplifier;
    if (!simplifier) throw new Error('module did not export MeshoptSimplifier');
    if (!simplifier.supported) throw new Error('WebAssembly SIMD is not supported here');
    await simplifier.ready;
  } catch (err) {
    throw new Error(
      'Could not start the mesh simplifier (WebAssembly). If this page is served under a strict ' +
        "Content-Security-Policy it needs script-src 'wasm-unsafe-eval'. Underlying error: " +
        (err instanceof Error ? err.message : String(err)),
      { cause: err }
    );
  }
  return simplifier;
}

/**
 * meshoptimizer `simplifyPrune`: delete small isolated parts, keep everything else bit-identical.
 *
 * This is the one-click version of [`labelComponents`] + [`removeComponents`], and worth preferring
 * when the user does not want to inspect anything. It returns only a new index buffer over the
 * original vertex array — meshopt moves no vertex and rewrites no attribute — so, exactly as with
 * the hand-rolled path above, skin weights and UVs cannot be damaged.
 *
 * `targetError` is the size of a component that may be discarded, RELATIVE TO MESH EXTENTS, in
 * `[0..1]` — meshoptimizer's own wording is "0.01 = 1% deformation". So 0.01–0.05 removes dust and
 * floating islands; 0.2 starts removing hands.
 *
 * 🔴 `simplifyPrune` decides connectivity by POSITION (`buildPositionRemap` runs before
 * `buildComponents` in `simplifier.cpp`), not by index value. So it is immune to seam splits, and
 * it will disagree with `labelComponents(geometry, { weld: false })` — that is the welded labelling
 * being right and the unwelded labelling being wrong, not a bug here.
 *
 * ⚠️ meshopt's internal `assert()` throws a bare `Error('Assertion failed')`. Everything it checks
 * is pre-validated here so the user never sees that string.
 */
export async function pruneIsolated(
  geometry: THREE.BufferGeometry,
  targetError: number
): Promise<Uint32Array> {
  if (!(targetError >= 0) || targetError > 1) {
    throw new Error(`Prune threshold must be between 0 and 1, got ${targetError}.`);
  }
  const index = requireIndex(geometry);
  const positions = readPositions(geometry);

  // 🔴 Feed it a Uint32Array specifically. The wrapper does
  // `result = indices instanceof Uint32Array ? result : new indices.constructor(result)`, so a
  // Uint16Array input hands back a Uint16Array — contradicting the package's own `.d.ts`, which
  // declares the return as `Uint32Array` unconditionally. Normalising the input keeps our declared
  // return type honest.
  const source = index.array as ArrayLike<number>;
  const indices = source instanceof Uint32Array ? source : new Uint32Array(source);

  const simplifier = await loadSimplifier();
  // Stride is in FLOATS, not bytes — the wrapper multiplies by 4 on the way into WASM. `positions`
  // is tightly packed by `readPositions`, hence 3.
  return simplifier.simplifyPrune(indices, positions, 3, targetError);
}

/* ------------------------------------------------------------------------------------------- */
/* decimate                                                                                     */
/* ------------------------------------------------------------------------------------------- */

export interface DecimateOptions {
  /**
   * Target triangle count as a fraction of the current one, in `[0..1]`. Default `0.5`.
   *
   * `0` means "no triangle target at all — go as far as [`targetError`] allows", which is the
   * error-driven mode. Anything above 0 is the ratio-driven mode and [`targetError`] becomes the
   * safety rail rather than the objective.
   */
  ratio?: number;
  /** Absolute triangle target; overrides [`ratio`] when set. Rounded down, floored at 0. */
  targetTriangles?: number;
  /**
   * The most deformation the simplifier may introduce to reach the target, RELATIVE to mesh
   * extents — meshoptimizer's own units, where "0.01 = 1% deformation". Default `1`, i.e. no cap.
   *
   * ⚠️ Default `1` is deliberate and is why the report matters. With a cap the simplifier stops the
   * moment the next collapse would exceed it, and the ratio you asked for is simply not delivered;
   * measured on a 40×40 grid asked for 10%, a cap of `1e-5` returned 91% of the triangles. Without
   * a cap it hits the ratio and tells you what that cost.
   */
  targetError?: number;
  /**
   * Weight the error by vertex normals and UVs as well as position. Default: `true` whenever the
   * geometry has a usable `normal` or `uv`. See [`decimate`] for why that is the default.
   */
  attributeAware?: boolean;
  /**
   * Per-attribute weights. Roughly "a change of `1/weight` in the attribute costs as much as a
   * change of `1` in position", so HIGHER preserves the attribute at the expense of the silhouette.
   *
   * ⚠️ Defaults are 1.0 for both, which is meshoptimizer's own recommendation for normalized
   * attributes (unit normals, UVs in `[0..1]`). They are NOT tuned on our characters, and a
   * synthetic flat grid cannot tell you anything about the right value — the sweep 0.25→8 on one
   * moved the achieved triangle count by 1. Tune against a real asset and an eye test, not a
   * number, and raise `uv` first: on a projected texture, swim is what you notice.
   */
  attributeWeights?: { normal?: number; uv?: number };
}

export interface DecimateReport {
  /** The new index buffer, over the SAME vertex array. Feed straight to [`applyIndex`]. */
  index: IndexArray;
  inputTriangles: number;
  outputTriangles: number;
  /** What was actually asked of meshoptimizer, after ratio/target resolution. */
  requestedTriangles: number;
  /** `outputTriangles / inputTriangles`. */
  achievedRatio: number;
  /**
   * True when the simplifier delivered the triangle count that was asked for.
   *
   * ⚠️ False is the interesting case and it is NOT an error: meshoptimizer stops early whenever the
   * next collapse would break the mesh's topology or exceed [`DecimateOptions.targetError`], and on
   * a 200-shell character — every shell a closed border it will not collapse across — that is
   * routine. Read it with [`achievedRatio`] and say what actually happened; a UI that reports
   * "decimated to 25%" over a result that came back at 91% is the failure this field exists to
   * prevent.
   *
   * In error-driven mode ([`DecimateOptions.ratio`] `0`) there is no triangle target to miss, so
   * this is always true and [`error`] is the number that means something.
   */
  metTarget: boolean;
  /** True when nothing at all was removed — a distinct outcome from "missed the target by a bit". */
  unchanged: boolean;
  /** Deformation introduced, relative to mesh extents. Same units as [`DecimateOptions.targetError`]. */
  error: number;
  /** The same error in the mesh's own units. */
  errorAbsolute: number;
  /** Attribute names fed to the error metric; empty when plain `simplify` ran. */
  attributesUsed: string[];
  /** Joints left driving nothing. See [`starvedJoints`] for what this can and cannot tell you. */
  rig: JointStarvation;
  /** Wall clock for the simplifier call and the checks around it. */
  elapsedMs: number;
}

/** The attributes worth handing the error metric, in the fixed order they are packed in. */
const DECIMATE_ATTRIBUTES = [
  { name: 'normal', size: 3, weight: 1 },
  { name: 'uv', size: 2, weight: 1 }
] as const;

/**
 * meshoptimizer decimation: fewer triangles over the SAME vertices.
 *
 * # 🔴 Why this is safe to offer at all
 *
 * `meshopt_simplify` and `meshopt_simplifyWithAttributes` GENERATE A NEW INDEX BUFFER OVER THE SAME
 * VERTEX ARRAY. No vertex moves, no attribute is read back and rewritten. That is precisely the
 * index-only discipline this whole file is built on (see [`removeComponents`]), so skin weights and
 * UVs survive decimation for exactly the same reason they survive shell deletion: nothing touches
 * them. `simplifyWithUpdate` breaks that rule and is not used or declared here.
 *
 * ⚠️ Do not over-read the guarantee. Triangles genuinely change which vertices they reference —
 * that is what decimation IS. What is guaranteed is that every surviving index still names a real
 * vertex whose position, UV and skin weights are the bytes the exporter wrote, in one consistent
 * tuple. What is NOT guaranteed is that the character still looks or deforms right; see
 * [`starvedJoints`], and the report below.
 *
 * # `simplifyWithAttributes`, not `simplify` — the call, and why
 *
 * Plain `simplify` is already aware of attribute DISCONTINUITIES (it infers seams from the index
 * buffer and tries not to collapse across them) but it knows nothing about attribute VALUES. It
 * will happily slide a vertex along a locally flat surface for free, because the position error is
 * zero — and on an asset whose texture is a projection rather than a hand-authored unwrap, sliding
 * a vertex drags the texture across the face. Silhouette is not the failure mode on these models;
 * texture swim and broken shading are, and only the attribute-aware variant can see either.
 *
 * The counter-argument is cost, and it is real but small: measured below, attribute-aware is ~1.8×
 * the time and needs a `vertexCount × 5` scratch `Float32Array` (4.4 MB at 220k verts). It is also
 * strictly more likely to MISS the requested ratio, since it has more error to spend — which is an
 * argument for reporting honestly, not for turning it off. Callers that want the cheap path pass
 * `attributeAware: false`, and a geometry with neither normals nor UVs takes it automatically.
 *
 * # Timing, and why there is no chunking here
 *
 * Measured on this machine, unit grid, target 25%, meshoptimizer 1.2 WASM:
 *
 * | triangles | `simplify` | `simplifyWithAttributes` |
 * |-----------|-----------|--------------------------|
 * |  45,000   |   17 ms   |   17 ms                  |
 * | 180,000   |   51 ms   |   82 ms                  |
 * | 441,800   |  138 ms   |  249 ms                  |
 *
 * A quarter of a second at the top of our range is one dropped frame, not a frozen tab, so the
 * [`interiorFaces`] machinery — chunks, `AbortSignal`, progress — would be pure ceremony here. It
 * would also not WORK: this is a single synchronous call into WASM with no yield point inside it,
 * so the only way to get the main thread back is a worker, which costs a structured copy of the
 * position and index buffers to save 250 ms. Not worth it. The function is `async` solely because
 * [`loadSimplifier`] is.
 *
 * ⚠️ The one caveat on those numbers: a smooth grid is the easy case. A 200-shell character has far
 * more border edges the simplifier must refuse to collapse, which makes it faster, not slower — it
 * runs out of legal collapses early. If a real asset ever lands past ~500 ms, revisit.
 *
 * # 🔴 What this invalidates
 *
 * Triangle order is NOT preserved — every face id changes. A `ComponentLabels` from
 * [`labelComponents`], any `faceIndex` the user has picked, and any interior-face set are all dead
 * the moment this is applied. [`applyIndex`] drops the BVH for you; the labels are the caller's
 * problem and must be recomputed, not reused.
 *
 * ⚠️ Nothing here mutates the geometry. The caller applies the result — or does not, having read
 * the report.
 */
export async function decimate(
  geometry: THREE.BufferGeometry,
  options: DecimateOptions = {}
): Promise<DecimateReport> {
  const started = performance.now();
  const index = requireIndex(geometry);
  const source = index.array as ArrayLike<number>;
  const inputTriangles = index.count / 3;
  const positions = readPositions(geometry);
  const vertexCount = positions.length / 3;

  // 🔴 Same guard as [`labelComponents`], for a worse reason. An out-of-range index is copied
  // verbatim into the WASM heap and dereferenced against a position array sized from OUR count, so
  // meshoptimizer reads whatever is next in linear memory. That is a wrong mesh, not a crash.
  for (let i = 0; i < index.count; i++) {
    if (source[i] >= vertexCount) {
      throw new Error(
        `Index buffer references vertex ${source[i]} but the mesh only has ${vertexCount}. The ` +
          'file is corrupt.'
      );
    }
  }

  const requestedError = options.targetError ?? 1;
  if (!(requestedError >= 0) || requestedError > 1) {
    throw new Error(`Decimation error budget must be between 0 and 1, got ${options.targetError}.`);
  }

  let requestedTriangles: number;
  if (options.targetTriangles !== undefined) {
    if (!Number.isFinite(options.targetTriangles) || options.targetTriangles < 0) {
      throw new Error(`Triangle target must be 0 or more, got ${options.targetTriangles}.`);
    }
    requestedTriangles = Math.min(inputTriangles, Math.floor(options.targetTriangles));
  } else {
    const ratio = options.ratio ?? 0.5;
    // ⚠️ `!(ratio >= 0)` rather than `ratio < 0`, so an empty number input's NaN is rejected here
    // instead of becoming `Math.floor(NaN) = NaN`, failing meshopt's `target_index_count % 3 == 0`
    // assert, and surfacing to the user as the string "Assertion failed".
    if (!(ratio >= 0) || ratio > 1) {
      throw new Error(`Decimation ratio must be between 0 and 1, got ${options.ratio}.`);
    }
    requestedTriangles = Math.floor(inputTriangles * ratio);
  }

  // 🔴 Feed a Uint32Array, for the reason spelled out in [`pruneIsolated`]: the wrapper hands back
  // an array of the INPUT's constructor, so a Uint16 index in means a Uint16 out and our declared
  // return type would be a lie. This one also matters for correctness — a decimated mesh keeps its
  // original vertex count, so a mesh that legitimately needs 32-bit indices must not round-trip
  // through 16.
  const indices = source instanceof Uint32Array ? source : new Uint32Array(source);

  const simplifier = await loadSimplifier();
  const targetIndexCount = requestedTriangles * 3;

  const attributes = collectDecimateAttributes(geometry, vertexCount, options);
  let result: [Uint32Array, number];
  if (attributes) {
    result = simplifier.simplifyWithAttributes(
      indices,
      positions,
      3,
      attributes.values,
      attributes.stride,
      attributes.weights,
      null,
      targetIndexCount,
      requestedError
    );
  } else {
    result = simplifier.simplify(indices, positions, 3, targetIndexCount, requestedError);
  }
  const [newIndex, error] = result;

  if (newIndex.length === 0) {
    // Reachable: ratio 0 with a generous error budget on a mesh with no borders to hold it back.
    throw new Error(
      'Decimation removed every triangle. Ask for a triangle count above zero, or lower the error ' +
        'budget.'
    );
  }

  const outputTriangles = newIndex.length / 3;
  return {
    // The downcast only ever pays after [`compactVertices`] — decimation leaves the vertex count
    // alone, so a 220k-vertex character stays Uint32 no matter how hard it is decimated. See
    // [`allocIndex`].
    index: narrowIndex(newIndex, vertexCount),
    inputTriangles,
    outputTriangles,
    requestedTriangles,
    achievedRatio: outputTriangles / inputTriangles,
    metTarget: requestedTriangles === 0 || outputTriangles <= requestedTriangles,
    unchanged: outputTriangles === inputTriangles,
    error,
    errorAbsolute: error * simplifier.getScale(positions, 3),
    attributesUsed: attributes ? attributes.names : [],
    rig: starvedJoints(geometry, source, newIndex),
    elapsedMs: performance.now() - started
  };
}

/** `Uint16Array` when the vertex count allows, per [`allocIndex`]'s rule; otherwise pass through. */
function narrowIndex(index: Uint32Array, vertexCount: number): IndexArray {
  if (vertexCount > 65535) return index;
  const out = allocIndex(vertexCount, index.length);
  out.set(index);
  return out;
}

interface DecimateAttributes {
  values: Float32Array;
  stride: number;
  weights: number[];
  names: string[];
}

/**
 * Pack `normal` and `uv` into the one interleaved `Float32Array` `simplifyWithAttributes` wants, or
 * `null` when there is nothing worth packing and plain `simplify` should run instead.
 *
 * ⚠️ Values are read through `getComponent`, which DENORMALIZES — the same deliberate choice
 * [`readPositions`] documents and for the same reason: a `KHR_mesh_quantization` normal is a
 * normalized Int8 and its raw bits are in the wrong space entirely. `getComponent` also flattens an
 * `InterleavedBufferAttribute` for free, which `GLTFLoader` produces routinely. Read-only, so the
 * normalized-integer trap that governs [`compactVertices`] does not apply — nothing is written
 * back, so nothing is requantised.
 *
 * ⚠️ Non-finite values are replaced with 0. meshoptimizer does not reject them and they do NOT
 * reliably surface as a NaN error: measured, one NaN normal in 1681 vertices moved the reported
 * error by 0.5% and warned about nothing. A generated mesh gets NaN normals from `normalize()` on a
 * zero-area triangle, which these assets have by the thousand, so the choice is a slightly wrong
 * attribute term or an arbitrary one. Zero is the slightly wrong one.
 *
 * ⚠️ An attribute of the wrong `itemSize` is skipped rather than padded — a 4-component "uv" is not
 * a UV, and guessing which two components to feed the error metric is how you preserve the wrong
 * thing. The mesh is still decimated, just without that attribute.
 */
function collectDecimateAttributes(
  geometry: THREE.BufferGeometry,
  vertexCount: number,
  options: DecimateOptions
): DecimateAttributes | null {
  if (options.attributeAware === false) return null;

  const weightOverrides = options.attributeWeights ?? {};
  const used = DECIMATE_ATTRIBUTES.map((spec) => {
    const attr = geometry.getAttribute(spec.name);
    if (!attr || attr.itemSize !== spec.size || attr.count < vertexCount) return null;
    const override = weightOverrides[spec.name];
    // A NaN or negative weight trips meshopt's `attribute_weights[i] >= 0` assert, which reaches
    // the user as the string "Assertion failed"; `Infinity` passes that assert and then makes every
    // attribute error infinite, so the mesh comes back untouched with no explanation. Both fall
    // back to the default.
    const weight =
      typeof override === 'number' && Number.isFinite(override) && override >= 0
        ? override
        : spec.weight;
    return { name: spec.name, size: spec.size, weight, attr };
  }).filter((entry): entry is NonNullable<typeof entry> => entry !== null);

  if (used.length === 0) return null;

  const stride = used.reduce((sum, entry) => sum + entry.size, 0);
  const values = new Float32Array(vertexCount * stride);
  const weights: number[] = [];
  let offset = 0;
  for (const entry of used) {
    for (let k = 0; k < entry.size; k++) weights.push(entry.weight);
    for (let v = 0; v < vertexCount; v++) {
      for (let k = 0; k < entry.size; k++) {
        const value = entry.attr.getComponent(v, k);
        values[v * stride + offset + k] = Number.isFinite(value) ? value : 0;
      }
    }
    offset += entry.size;
  }
  return { values, stride, weights, names: used.map((entry) => entry.name) };
}

/* ------------------------------------------------------------------------------------------- */
/* Interior-face detection                                                                      */
/* ------------------------------------------------------------------------------------------- */

/**
 * BVHs are expensive to build (hundreds of ms at 440k tris) and there is no reason to rebuild one
 * per repair, so they are cached against the geometry. A `WeakMap` rather than a property so a
 * discarded geometry takes its BVH with it and nothing has to remember to clean up.
 */
const bvhCache = new WeakMap<THREE.BufferGeometry, MeshBVH>();

/**
 * 🔴 `indirect: true` is not optional here, it is the difference between working and silently
 * scrambling every face id in the UI.
 *
 * In default (direct) mode `GeometryBVH` sets `primitiveBuffer = geometry.index.array` and the
 * build PARTITIONS THAT ARRAY IN PLACE. Triangle 12 becomes triangle 40,000. Every `faceIndex` the
 * user already picked, and the whole `faceComponent` array from [`labelComponents`], now point at
 * different triangles — with no error, no visual change, and a "delete this island" that deletes
 * something across the model.
 *
 * With `indirect: true` the build sorts a private `_indirectBuffer` and leaves the index alone. The
 * reported `faceIndex` is still the ORIGINAL triangle number: `intersectClosestTri` resolves
 * `_indirectBuffer[i]` before calling `intersectTri`, which then does
 * `intersection.faceIndex = tri`.
 * So no `resolveTriangleIndex` call is needed on hits, only on raw shapecast iteration.
 */
function getBvh(geometry: THREE.BufferGeometry): MeshBVH {
  let bvh = bvhCache.get(geometry);
  if (!bvh) {
    bvh = new MeshBVH(geometry, { indirect: true });
    bvhCache.set(geometry, bvh);
  }
  return bvh;
}

/**
 * Drop the cached BVH for a geometry. Call after ANY change to the index or positions — a BVH built
 * over a stale index reports hits on triangles that no longer exist.
 *
 * [`applyIndex`] and [`compactVertices`] already do this; this export exists for teardown when the
 * panel closes and for callers that mutate geometry by some other route.
 */
export function disposeBvh(geometry: THREE.BufferGeometry): void {
  bvhCache.delete(geometry);
}

const _pickRay = new THREE.Ray();
const _pickInverse = new THREE.Matrix4();

/**
 * The matrix that carries `geometry.attributes.position` to WORLD, at rest pose.
 *
 * 🔴 It is NOT always `matrixWorld`, and the difference is silent. three renders a skinned vertex as
 * `matrixWorld · bindMatrixInverse · Σwᵢ(boneWorldᵢ · boneInverseᵢ) · bindMatrix · p`. At rest the Σ
 * is the identity, and `SkinnedMesh.updateMatrixWorld` sets `bindMatrixInverse` from the bind mode:
 *
 *   - `AttachedBindMode` (the default, and what `GLTFLoader` produces) → `bindMatrixInverse` is
 *     `matrixWorld⁻¹`, the two cancel, and the surface sits at **`bindMatrix · p`**.
 *   - `DetachedBindMode` → `bindMatrixInverse` is `bindMatrix⁻¹`, those cancel instead, and the
 *     surface sits at `matrixWorld · p`.
 *
 * ⚠️ And `GLTFLoader` calls `mesh.bind( skeleton, _identityMatrix )` — verified in the r185 loader,
 * 2026-08-17 — so for our characters `bindMatrix` is the IDENTITY while `matrixWorld` carries
 * whatever node transform the file put above the mesh. Using `matrixWorld` to unproject the ray
 * would then miss by exactly that transform, and the miss is invisible on the assets whose node
 * transform happens to be identity too. `stage.ts`'s `restPosition` already takes the `bindMatrix`
 * side of this and agrees.
 */
function restToWorld(mesh: THREE.Mesh | THREE.SkinnedMesh): THREE.Matrix4 {
  const skinned = mesh as THREE.SkinnedMesh;
  if (skinned.isSkinnedMesh && skinned.bindMode === THREE.AttachedBindMode) {
    return skinned.bindMatrix;
  }
  return mesh.matrixWorld;
}

/**
 * The nearest triangle a world-space ray hits, as a `faceIndex` into the geometry's CURRENT index
 * buffer — the same numbering [`labelComponents`] and [`componentAtFace`] speak. `-1` for a miss.
 *
 * This is the cheap picking path, and the reason hover-highlighting is affordable at all:
 * `Raycaster.intersectObjects` on a `SkinnedMesh` runs `getVertexPosition` — a weighted sum of up to
 * four bone matrices — for all three corners of every one of ~440k triangles. The BVH visits a few
 * dozen. It shares the cached tree [`interiorFaces`] builds, so the FIRST call after a geometry
 * change pays the build (hundreds of ms at 440k tris) and the rest are free.
 *
 * ## 🔴 The face id needs no un-permuting, and here is how that was checked
 *
 * [`getBvh`] builds with `{ indirect: true }`, which raises the obvious worry that a hit reports a
 * position in the BVH's private permutation rather than a triangle number. It does not. In
 * `three-mesh-bvh@0.9.14`, `MeshBVH.raycastFirst` dispatches to `raycastFirst_indirect`, whose leaf
 * case calls `intersectClosestTri_indirect`; that resolves `_indirectBuffer[i]` FIRST and passes the
 * resolved triangle to `intersectTri`, which finishes with `intersection.faceIndex = tri`
 * (`src/core/utils/iterationUtils_indirect.generated.js` and
 * `src/utils/ThreeRayIntersectUtilities.js`, read 2026-08-17). `resolveTriangleIndex` is only needed
 * on raw `shapecast` iteration, where the callback is handed the buffer position instead — see
 * `iterateOverTriangles_indirect` in the same file, which calls it explicitly.
 *
 * So the value returned here indexes the geometry's index buffer directly. Getting this wrong would
 * highlight one part of the character and delete another.
 *
 * ## 🔴 REST POSE ONLY
 *
 * The BVH is built over the raw `position` attribute, which is bind space. That matches the rendered
 * surface only while the skinning bracket is the identity — i.e. at rest. Under a playing clip the
 * bones have moved, the drawn pixels have moved with them, and this function happily reports the
 * face that *would* be under the cursor if the model were standing still. Callers guard on "this GLB
 * carries no animation clips", the same condition the joint editor already refuses to work without.
 *
 * ⚠️ `DoubleSide`, deliberately: a mesh assembled from ~200 independently generated shells has no
 * trustworthy winding, and an inverted shell must still be pickable. Same argument as
 * [`interiorFaces`].
 */
export function raycastFace(
  mesh: THREE.Mesh | THREE.SkinnedMesh,
  raycaster: THREE.Raycaster
): number {
  return raycastRest(mesh, raycaster)?.faceIndex ?? -1;
}

/**
 * The one cast both [`raycastFace`] and [`raycastSurface`] ride on, so the rest-space rule and the
 * cached tree exist in exactly one place.
 *
 * ⚠️ Uses the module-level scratch ray, which is safe only because this is synchronous end to end.
 * Anything that yields mid-pass must keep its own `THREE.Ray` — see the note in [`interiorFaces`],
 * which is now interleaved with hover picking and pays exactly that rule.
 */
function raycastRest(
  mesh: THREE.Mesh | THREE.SkinnedMesh,
  raycaster: THREE.Raycaster
): THREE.Intersection | null {
  const geometry = mesh.geometry;
  // No index means no triangle identity to report. Return a miss rather than throwing: this runs on
  // pointermove, and an exception per mouse movement is not a diagnostic, it is a freeze.
  if (geometry.getIndex() === null) return null;

  _pickInverse.copy(restToWorld(mesh)).invert();
  _pickRay.copy(raycaster.ray).applyMatrix4(_pickInverse);
  // `near`/`far` stay 0/Infinity. `Ray.applyMatrix4` renormalises the direction, so a scaled
  // transform makes local distances a different unit from the raycaster's — and we want the nearest
  // triangle regardless of range anyway.
  return getBvh(geometry).raycastFirst(_pickRay, THREE.DoubleSide);
}

/** Where a ray met the rest surface. Everything in WORLD space — see [`raycastSurface`]. */
export interface SurfaceHit {
  /** Triangle number in the geometry's CURRENT index buffer, as [`raycastFace`] reports it. */
  faceIndex: number;
  /**
   * WORLD-space hit point. Exactly what `weight-paint`'s `DabOptions.point` wants, and the reason
   * this function exists: a face index alone cannot centre a brush.
   */
  point: THREE.Vector3;
  /**
   * WORLD-space geometric normal of that triangle, flipped to face the ray's origin.
   *
   * 🔴 Flipped, not taken as wound. A character assembled from ~200 independently generated shells
   * has no trustworthy winding — which is why every pick here is `DoubleSide` — so half the hits
   * would come back pointing INTO the surface. Fed to `DabOptions.normal` with `frontFacing` on,
   * a back-pointing normal culls every vertex the brush was aimed at and the dab silently does
   * nothing. The side the ray came from is the side the user is looking at, and that is knowable.
   */
  normal: THREE.Vector3;
}

/** Scratch for the local→world normal transform, `(M⁻¹)ᵀ`. */
const _pickNormal = new THREE.Matrix3();

/**
 * As [`raycastFace`], but reporting WHERE on the triangle the ray landed.
 *
 * Same cached BVH, same 🔴 REST-POSE-ONLY rule, same `DoubleSide` argument — read
 * [`raycastFace`]'s notes, all of which apply unchanged. This is the picking path for a paint
 * brush, which needs a point in the world to centre a sphere on rather than a triangle number to
 * look up in a label map.
 *
 * ⚠️ Allocates nothing extra: `three-mesh-bvh` already hands back a freshly cloned `point` and a
 * fresh `face.normal` per hit, and both are transformed in place.
 */
export function raycastSurface(
  mesh: THREE.Mesh | THREE.SkinnedMesh,
  raycaster: THREE.Raycaster
): SurfaceHit | null {
  const hit = raycastRest(mesh, raycaster);
  // ⚠️ `Intersection.faceIndex` is `number | null | undefined` in three's own types — it is absent
  // for a line or point intersection. `??` rather than a truthiness test, because face 0 is real.
  const faceIndex = hit?.faceIndex ?? -1;
  if (!hit || faceIndex < 0) return null;

  const toWorld = restToWorld(mesh);
  const point = hit.point.applyMatrix4(toWorld);

  const normal = hit.face?.normal ?? new THREE.Vector3();
  normal.applyMatrix3(_pickNormal.getNormalMatrix(toWorld));
  // A degenerate triangle has no normal to speak of; facing the camera is the honest fallback and
  // keeps `frontFacing` culling from rejecting everything on a zero vector.
  if (normal.lengthSq() < 1e-20) normal.copy(raycaster.ray.direction).negate();
  else if (normal.dot(raycaster.ray.direction) > 0) normal.negate().normalize();
  else normal.normalize();

  return { faceIndex, point, normal };
}

/**
 * How long one chunk of [`interiorFaces`] may hold the main thread before yielding, in ms.
 *
 * 10 ms leaves ~6 ms of a 60 Hz frame for the browser's own style/layout/paint — enough to actually
 * move a progress bar and service a Cancel click — while keeping the cost of yielding (one
 * `MessageChannel` round trip, comfortably under 0.2 ms) around a percent or two of wall clock.
 * Below ~4 ms the yield starts to dominate; above ~16 ms every chunk overruns a frame and the bar
 * moves in visible steps again.
 *
 * ⚠️ A budget, NOT a face count. The cost per face is wildly asymmetric — an exterior face exits on
 * the first or second viewpoint, an interior one pays all 64 — so any fixed face count is either
 * pointlessly small over open surface or a multi-frame stall inside a buried region. The clock does
 * not care which kind of face it just finished.
 */
const CHUNK_BUDGET_MS = 10;

/** A one-shot yield back to the event loop, plus the teardown for whatever it rides on. */
interface Yielder {
  /** Resolves in a FRESH TASK — i.e. after the browser has had its chance to render. */
  next(): Promise<void>;
  close(): void;
}

/**
 * 🔴 A microtask is not a yield. `await Promise.resolve()` drains inside the SAME task, before the
 * event loop ever reaches its "update the rendering" step, so a loop built on it repaints nothing,
 * dispatches no clicks, and freezes the tab exactly as badly as the synchronous version did — while
 * looking, in the source, like it yields. Only a macrotask boundary gives the browser a rendering
 * opportunity.
 *
 * `MessageChannel` is the macrotask to use. `setTimeout(…, 0)` also lands after a paint but is
 * clamped to ~4 ms once five timers are nested, which at a 10 ms budget throws away nearly a third
 * of the wall clock for nothing. `requestAnimationFrame` ties us to the display refresh and stops
 * dead in a background tab, turning a long pass into an unfinished one. `scheduler.yield()` is the
 * purpose-built answer and would be a straight swap for `next()`, but it is not everywhere we ship,
 * so the fallback has to exist anyway — and one code path is one behaviour to reason about when a
 * bug report says "it froze".
 *
 * ⚠️ Assigning `onmessage` implicitly starts the port. Using `addEventListener` instead would need
 * an explicit `port1.start()`, and without it the first `next()` never resolves.
 */
function createYielder(): Yielder {
  const channel = new MessageChannel();
  let resume: (() => void) | null = null;
  channel.port1.onmessage = () => {
    const fn = resume;
    resume = null;
    fn?.();
  };
  return {
    next: () =>
      new Promise<void>((resolve) => {
        // Wrapped rather than `resume = resolve`: a `Promise<void>` executor's `resolve` still takes
        // a parameter, and a one-parameter function is not assignable to a zero-parameter type.
        resume = () => resolve();
        channel.port2.postMessage(0);
      }),
    close: () => {
      channel.port1.onmessage = null;
      channel.port1.close();
      channel.port2.close();
    }
  };
}

/**
 * Everything about a mesh that, if it changes, invalidates a face id computed against it.
 *
 * 🔴 This is the whole reason a chunked pass needs more care than a synchronous one. A synchronous
 * loop cannot be interrupted; a chunked one spans hundreds of frames, and in any of them the user
 * can delete a piece. [`applyIndex`] then installs a NEW index attribute, [`disposeBvh`]s the tree
 * and renumbers every triangle after the first deleted one. Face 200,000 from chunk one now names a
 * different triangle, and returning that set means the caller deletes the wrong part of the
 * character with no error anywhere. Detect it and refuse.
 */
interface GeometryStamp {
  geometry: THREE.BufferGeometry;
  index: THREE.BufferAttribute;
  indexVersion: number;
  position: THREE.BufferAttribute | THREE.InterleavedBufferAttribute;
  positionVersion: number;
  bvh: MeshBVH;
}

/** An `InterleavedBufferAttribute` has no version of its own; the buffer it rides on carries it. */
function attributeVersion(attr: THREE.BufferAttribute | THREE.InterleavedBufferAttribute): number {
  return isInterleaved(attr) ? attr.data.version : attr.version;
}

function stampGeometry(mesh: THREE.Mesh, bvh: MeshBVH): GeometryStamp {
  const geometry = mesh.geometry;
  const index = requireIndex(geometry);
  const position = requirePositionAttribute(geometry);
  return {
    geometry,
    index,
    indexVersion: index.version,
    position,
    positionVersion: attributeVersion(position),
    bvh
  };
}

/**
 * Six O(1) comparisons, so this is affordable at every chunk boundary. Between them they cover
 * every route a geometry changes by in this codebase:
 *
 * - a different geometry hanging off the mesh we were handed — the GLB was reloaded under us;
 * - a different index attribute — [`applyIndex`], which always installs a fresh `BufferAttribute`
 *   rather than writing into the old one, and [`compactVertices`];
 * - a bumped `version` on the index or the positions — someone edited a buffer in place and set
 *   `needsUpdate`, which the identity checks alone would miss;
 * - a different position attribute — [`compactVertices`] rebuilds every attribute;
 * - a different cached BVH — the backstop. Any caller that mutates by some other route is still
 *   required to [`disposeBvh`], and that alone trips this.
 *
 * ⚠️ The positions [`readPositions`] handed out may be a `subarray` ALIAS of the live attribute, so
 * an in-place vertex edit would silently change the centroids mid-pass; the `version` check is what
 * covers that. A rebuilt attribute leaves the alias pointing at the old array instead — stale, not
 * unsafe, and caught the same way.
 */
function assertUnchanged(mesh: THREE.Mesh, stamp: GeometryStamp): void {
  const geometry = mesh.geometry;
  const changed =
    geometry !== stamp.geometry ||
    geometry.getIndex() !== stamp.index ||
    stamp.index.version !== stamp.indexVersion ||
    geometry.getAttribute('position') !== stamp.position ||
    attributeVersion(stamp.position) !== stamp.positionVersion ||
    bvhCache.get(geometry) !== stamp.bvh;
  if (changed) {
    throw new Error(
      'The mesh changed while the interior-face pass was running, so the faces it had found no ' +
        'longer name the same triangles. Nothing was applied — run the pass again.'
    );
  }
}

/**
 * Faces that no ray from any viewpoint on a surrounding sphere can reach — the guts of the ~200
 * interpenetrating shells. Returns faceIndex values; feed them to the caller's own index filter or
 * convert to a component selection. Nothing is mutated.
 *
 * ## Method, and why this one
 *
 * For every face, shoot from its centroid towards each viewpoint in turn and stop at the first
 * viewpoint that is not blocked. Blocked from all of them ⇒ interior.
 *
 * Deliberately NOT doing back-face culling first, even though it would roughly halve the work.
 * Culling needs a trustworthy winding order, and a mesh assembled from 200 independently generated
 * shells does not have one — inverted shells are exactly the case we are trying to detect, and
 * culling would make every face of an inverted shell "invisible from the front" and delete a whole
 * limb. Casting both ways costs time and buys correctness.
 *
 * Also deliberately using the ray's `near` parameter rather than nudging the origin along the face
 * normal. The nudge needs a normal and therefore needs the winding again; `near` needs neither.
 *
 * ## Cost, and why it is chunked rather than moved to a worker
 *
 * Worst case is `faces × viewpoints` rays, but exterior faces exit on the first or second viewpoint
 * so the real bill is roughly `exterior×2 + interior×viewpoints`. At 440k faces with a heavily
 * buried model and 64 viewpoints that is several million rays and several SECONDS of work. Run
 * straight through that is a frozen tab with no progress, no cancel, and a browser offering to kill
 * the page — so the loop runs in [`CHUNK_BUDGET_MS`] slices and yields to the event loop between
 * them. The answer is bit-identical to the old synchronous one: the loop body below is untouched,
 * only the control flow around it changed.
 *
 * A worker was the other option and is the wrong one here, for two reasons that are specific to
 * this function rather than general:
 *
 * - It would cost a structured copy of the position and index buffers — megabytes at 440k tris — to
 *   save work we are already handing back to the browser 60 times a second. `analyseSkin`'s caller
 *   turned the same trade down for the same reason.
 * - 🔴 It cannot see [`bvhCache`]. The BVH costs hundreds of ms to build at this size, and a worker
 *   would build a SECOND one over its own copy — while [`raycastFace`], which runs on every
 *   pointermove for hover picking, keeps using the main-thread tree. Two trees, two builds, and the
 *   worker's dies with the worker.
 *
 * ## Cancellation leaves nothing behind
 *
 * There is no partial state to unwind: the only thing this function writes is the local `Set` it
 * was going to return, which the rejection simply drops. The one lasting side effect is [`getBvh`]
 * populating [`bvhCache`], and that happens up front, is a pure memo of the geometry as it stands,
 * and is exactly what we want left warm for the next attempt.
 *
 * 🔴 It also rejects — with a plain `Error`, not the signal's reason — if the mesh is edited while
 * the pass is running, because the face ids it has already collected would then name the wrong
 * triangles. See [`assertUnchanged`] for what "edited" is taken to mean and how it is detected.
 *
 * ## ⚠️ Known approximations
 *
 * - Centroid-only. A face whose centroid is occluded but whose corner peeks out is called interior.
 *   At these triangle densities a face that thin is not visible anyway.
 * - Bind space. Positions are read from `geometry.attributes.position` and `mesh.matrixWorld` is
 *   ignored, so the viewpoint sphere is built around the REST POSE. Pose the rig first.
 */
export async function interiorFaces(
  mesh: THREE.Mesh,
  options: InteriorOptions = {}
): Promise<Set<number>> {
  const signal = options.signal;
  const onProgress = options.onProgress;
  // Before the BVH build, which is the expensive part and would otherwise run in full for a request
  // that was already cancelled.
  signal?.throwIfAborted();

  const geometry = mesh.geometry;
  const index = requireIndex(geometry);
  const idx = index.array as ArrayLike<number>;
  const triCount = index.count / 3;
  const positions = readPositions(geometry);
  const bounds = boundsOf(positions);

  // 🔴 NON-FINITE GUARDS, AND THEY ARE NOT PARANOIA — THE FAILURE IS "DELETE THE CHARACTER".
  //
  // `Math.max(4, Math.floor(NaN))` is `NaN`. `new Float64Array(NaN * 3)` is length 0 and `v < NaN`
  // is false, so NOT ONE RAY IS CAST — and a face that no ray reached is, by this function's own
  // definition, interior. Every triangle in the model comes back in the returned set, the caller
  // deletes them, and the character is gone. There is no error and no warning anywhere on that path.
  //
  // A `NaN` epsilon is the same shape of bug by a different route: it becomes the ray's `near`, and
  // every intersection test against it fails, so again every face is classified interior.
  //
  // One `<input type="number">` bound straight to either option, left empty, produces `NaN`. That is
  // the whole distance between here and a destroyed asset, so both are clamped rather than trusted.
  const requestedViewpoints = options.viewpoints ?? 64;
  const viewpoints = Number.isFinite(requestedViewpoints)
    ? Math.max(4, Math.floor(requestedViewpoints))
    : 64;
  const defaultEpsilon = bounds.diagonal * 1e-5;
  const requestedEpsilon = options.epsilon ?? defaultEpsilon;
  const epsilon = Number.isFinite(requestedEpsilon) ? requestedEpsilon : defaultEpsilon;
  const requestedBudget = options.budgetMs ?? CHUNK_BUDGET_MS;
  const budgetMs = Number.isFinite(requestedBudget)
    ? Math.min(100, Math.max(1, requestedBudget))
    : CHUNK_BUDGET_MS;

  const cx = (bounds.min[0] + bounds.max[0]) / 2;
  const cy = (bounds.min[1] + bounds.max[1]) / 2;
  const cz = (bounds.min[2] + bounds.max[2]) / 2;
  // 1.5× the bounding-sphere radius: comfortably outside the mesh, close enough that the viewpoints
  // still form a meaningful set of directions rather than a point at infinity.
  const radius = (bounds.diagonal / 2) * 1.5 || 1;

  // Fibonacci sphere — near-uniform without the pole clustering of a lat/long grid. Clustering
  // matters: wasted viewpoints at the poles are viewpoints not looking into an armpit.
  const eyes = new Float64Array(viewpoints * 3);
  const golden = Math.PI * (3 - Math.sqrt(5));
  for (let i = 0; i < viewpoints; i++) {
    const y = 1 - (i / (viewpoints - 1)) * 2;
    const r = Math.sqrt(Math.max(0, 1 - y * y));
    const theta = golden * i;
    eyes[i * 3] = cx + Math.cos(theta) * r * radius;
    eyes[i * 3 + 1] = cy + y * radius;
    eyes[i * 3 + 2] = cz + Math.sin(theta) * r * radius;
  }

  const bvh = getBvh(geometry);
  const stamp = stampGeometry(mesh, bvh);
  // ⚠️ A LOCAL ray, not the module-level `_pickRay` this file keeps for [`raycastFace`]. That was
  // merely tidy while this ran synchronously; now that the pass spans frames, hover picking runs
  // BETWEEN our chunks and would be writing into the same object.
  const ray = new THREE.Ray();
  const interior = new Set<number>();
  const third = 1 / 3;

  const yielder = createYielder();
  try {
    onProgress?.(0, triCount);
    let deadline = performance.now() + budgetMs;
    for (let f = 0; f < triCount; f++) {
      // The clock is read once per FACE rather than once per fixed batch of faces. A batch big
      // enough to be worth amortising over is also big enough to be 64 rays × N deep inside a
      // buried region, i.e. the stall we are removing; `performance.now()` at ~440k calls across a
      // multi-second pass costs well under a percent, which is the cheaper end of that trade.
      if (performance.now() >= deadline) {
        // Report before yielding, so the bar's new value is on screen in the paint the yield buys.
        onProgress?.(f, triCount);
        await yielder.next();
        // Both checks after the resume, in this order: a cancel is the expected outcome and should
        // not be reported as a mesh-changed error if the user cancelled *because* they went and
        // edited the mesh.
        signal?.throwIfAborted();
        assertUnchanged(mesh, stamp);
        deadline = performance.now() + budgetMs;
      }

      const a = idx[f * 3];
      const b = idx[f * 3 + 1];
      const c = idx[f * 3 + 2];
      const ox = (positions[a * 3] + positions[b * 3] + positions[c * 3]) * third;
      const oy = (positions[a * 3 + 1] + positions[b * 3 + 1] + positions[c * 3 + 1]) * third;
      const oz = (positions[a * 3 + 2] + positions[b * 3 + 2] + positions[c * 3 + 2]) * third;
      ray.origin.set(ox, oy, oz);

      let visible = false;
      for (let v = 0; v < viewpoints; v++) {
        const dx = eyes[v * 3] - ox;
        const dy = eyes[v * 3 + 1] - oy;
        const dz = eyes[v * 3 + 2] - oz;
        const far = Math.sqrt(dx * dx + dy * dy + dz * dz);
        if (far <= epsilon) continue;
        ray.direction.set(dx / far, dy / far, dz / far);
        // DoubleSide: an inverted shell must still block, or its interior reads as open sky.
        if (bvh.raycastFirst(ray, THREE.DoubleSide, epsilon, far) === null) {
          visible = true;
          break;
        }
      }
      if (!visible) interior.add(f);
    }
  } finally {
    // Ports keep the channel — and this closure's captured buffers — alive until both ends close.
    // `finally` because the abort and mesh-changed paths leave through a throw, not the loop end.
    yielder.close();
  }

  // Nothing runs between the last chunk's face and here, so the stamp checked at that chunk's start
  // still holds: no further verification is needed to hand this back.
  onProgress?.(triCount, triCount);
  return interior;
}

/* ------------------------------------------------------------------------------------------- */
/* Applying an edit                                                                             */
/* ------------------------------------------------------------------------------------------- */

/**
 * Recompute every bounding volume that can go stale. Both objects, always.
 *
 * 🔴 There are two independent pairs and three.js consults all of them:
 * `SkinnedMesh.raycast` tests `this.boundingSphere` (its own, posed) and then
 * `this.geometry.boundingBox` (bind space) before it looks at a single triangle. The renderer
 * frustum-culls against `geometry.boundingSphere`. Refresh one pair and picking or drawing silently
 * dies in whichever region the other pair still covers.
 *
 * ⚠️ `computeBoundingBox`/`computeBoundingSphere` walk the POSITION ATTRIBUTE, not the index. So
 * deleting a floating island does NOT shrink the bounds — they stay correct but loose until
 * [`compactVertices`] actually removes the orphaned vertices. Refresh anyway: the values
 * `GLTFLoader` seeded from the accessor min/max are not guaranteed to have been right in the first
 * place.
 */
export function refreshBounds(mesh: THREE.Mesh | THREE.SkinnedMesh): void {
  mesh.geometry.computeBoundingBox();
  mesh.geometry.computeBoundingSphere();

  const skinned = mesh as THREE.SkinnedMesh;
  if (skinned.isSkinnedMesh) {
    // `SkinnedMesh.computeBounding*` goes through `getVertexPosition` → `applyBoneTransform`, which
    // dereferences `this.skeleton`. An unbound SkinnedMesh throws; leaving the volumes null makes
    // `raycast` compute them itself later, which is the correct fallback.
    if (skinned.skeleton) {
      skinned.computeBoundingBox();
      skinned.computeBoundingSphere();
    } else {
      // @types/three declares these non-nullable while documenting `@default null` — three itself
      // null-checks them, so the cast restores the type the runtime actually has.
      const volumes = skinned as unknown as {
        boundingBox: THREE.Box3 | null;
        boundingSphere: THREE.Sphere | null;
      };
      volumes.boundingBox = null;
      volumes.boundingSphere = null;
    }
  }
}

/**
 * Install a new index buffer and refresh everything that just went stale.
 *
 * A fresh `BufferAttribute` is used rather than writing into the old one. Writing in place would
 * need the r185 update plumbing — `addUpdateRange()` / `clearUpdateRanges()`, note the ARRAY
 * `updateRanges`, the singular `updateRange` object is gone — and would still be wrong, because the
 * new buffer is a different length and a partial upload cannot shrink a GPU buffer. A new attribute
 * object makes `WebGLAttributes` allocate a correctly sized one; the old GPU buffer is keyed off
 * the old attribute in a WeakMap and goes when it does.
 */
export function applyIndex(mesh: THREE.SkinnedMesh, index: IndexArray): void {
  const geometry = mesh.geometry;
  if (index.length === 0 || index.length % 3 !== 0) {
    throw new Error(`Refusing to apply an index buffer of ${index.length} entries.`);
  }

  const groups = geometry.groups;
  if (groups.length > 1) {
    // 🔴 Groups are ranges into the index buffer. Renumbering the buffer invalidates every one of
    // them, and there is no way to recover the mapping from the index array alone. Rather than
    // guess, refuse: a wrong group assigns the face material to the body.
    throw new Error(
      `This mesh is split across ${groups.length} material groups. Repairs only support a single ` +
        'material; merge the materials first.'
    );
  }
  if (groups.length === 1) {
    // ⚠️ Do NOT clear the group instead. If `mesh.material` is an array, `WebGLRenderer` iterates
    // `geometry.groups` and an empty list draws nothing — the mesh vanishes with no error.
    groups[0].start = 0;
    groups[0].count = index.length;
  }
  if (geometry.drawRange.count !== Infinity) {
    // A leftover finite range silently draws only part of the new buffer.
    geometry.setDrawRange(0, Infinity);
  }

  geometry.setIndex(new THREE.BufferAttribute(index, 1));
  disposeBvh(geometry);
  refreshBounds(mesh);
}

/* ------------------------------------------------------------------------------------------- */
/* compactVertices — the one that rebuilds attributes                                           */
/* ------------------------------------------------------------------------------------------- */

function allocLike(source: AttributeArray, length: number): AttributeArray {
  // Preserving `array.constructor` is the whole point: skinIndex is a Uint8/Uint16, skinWeight is
  // usually a normalized Uint8/Uint16, and promoting either to Float32 changes the accessor type
  // the GLB writer emits and quietly inflates the file.
  const Ctor = source.constructor as unknown as new (n: number) => AttributeArray;
  return new Ctor(length);
}

/**
 * Rebuild one attribute under a vertex remap.
 *
 * 🔴 This is the normalized-integer trap, and it is the reason [`compactVertices`] is the only
 * function in this file that can corrupt anything. `BufferAttribute.getX` DENORMALIZES when
 * `normalized` is true and `setX` RENORMALIZES on the way back. `WEIGHTS_0` and `JOINTS_0` come out
 * of a GLB as normalized `Uint8`/`Uint16` routinely, so a `getX`/`setX` round trip runs
 * `denormalize(normalize(v))` and quantises every skin weight — a visible wobble in the deformation
 * with nothing in the console. Copy raw elements off the underlying typed array instead and carry
 * `constructor`, `itemSize` and `normalized` across untouched. The bits that come out are the bits
 * that went in.
 *
 * ⚠️ An `InterleavedBufferAttribute` is de-interleaved here rather than preserved. `GLTFLoader`
 * produces them for tightly packed accessors sharing a bufferView, and there is no sane way to
 * compact one in place — the other attributes in the same buffer have their own lifetimes. The
 * result is functionally identical and marginally larger in memory.
 */
function remapAttribute(
  attr: THREE.BufferAttribute | THREE.InterleavedBufferAttribute,
  remap: Int32Array,
  newCount: number
): THREE.BufferAttribute {
  const itemSize = attr.itemSize;
  let src: AttributeArray;
  let stride: number;
  let offset: number;
  // `gpuType` only exists on a plain BufferAttribute; capture it before the union widens back.
  let gpuType: THREE.AttributeGPUType | undefined;
  if (isInterleaved(attr)) {
    src = attr.data.array;
    stride = attr.data.stride;
    offset = attr.offset;
  } else {
    src = attr.array;
    stride = itemSize;
    offset = 0;
    gpuType = attr.gpuType;
  }

  const dst = allocLike(src, newCount * itemSize);
  const oldCount = remap.length;
  for (let v = 0; v < oldCount; v++) {
    const n = remap[v];
    if (n < 0) continue;
    const from = v * stride + offset;
    const to = n * itemSize;
    for (let k = 0; k < itemSize; k++) dst[to + k] = src[from + k];
  }

  const out = new THREE.BufferAttribute(dst, itemSize, attr.normalized);
  // `gpuType` decides `vertexAttribIPointer` vs `vertexAttribPointer`. It defaults to FloatType, so
  // an integer attribute that was explicitly marked IntType would silently switch upload paths.
  if (gpuType !== undefined) out.gpuType = gpuType;
  return out;
}

/**
 * Drop vertices no surviving triangle references, renumbering the index and every attribute.
 *
 * Optional, and purely a file-size step — the index-only edits above leave orphaned vertices in the
 * buffer where they cost bytes and nothing else. Run it LAST, after [`applyIndex`], because it
 * works from whatever index the geometry currently carries.
 *
 * Pays twice at the small LOD tiers: the vertex accessors shrink, and dropping below 65535 vertices
 * lets the index downcast to Uint16 (see [`allocIndex`]).
 *
 * ⚠️ Vertex ids change; face ids do NOT. Triangle order is preserved exactly, so a
 * `ComponentLabels` computed before this call stays valid after it. Anything the caller is holding
 * that indexes by VERTEX is invalid.
 *
 * ⚠️ Morph targets are remapped too. Skipping them leaves a morph attribute at the old vertex count
 * and three throws on the next render — but only if the asset has morphs at all, which is exactly
 * the kind of thing that ships broken because the test character had none.
 *
 * ⚠️ Does not refresh bounds — it takes a geometry, not a mesh, and the SkinnedMesh's own volumes
 * are not reachable from here. Call [`refreshBounds`] on the mesh afterwards.
 */
export function compactVertices(geometry: THREE.BufferGeometry): void {
  const index = requireIndex(geometry);
  const idx = index.array as ArrayLike<number>;
  const position = requirePositionAttribute(geometry);
  const oldCount = position.count;

  const used = new Uint8Array(oldCount);
  for (let i = 0; i < index.count; i++) {
    const v = idx[i];
    if (v >= oldCount) {
      throw new Error(
        `Index buffer references vertex ${v} but the mesh only has ${oldCount}. Refusing to ` +
          'compact a corrupt mesh.'
      );
    }
    used[v] = 1;
  }

  // Ascending old order rather than order-of-first-use: it keeps the vertex buffer in its original
  // sequence, so the GLB diff stays local and any external table keyed on "roughly where in the
  // model" still makes sense. Vertex-cache locality is meshoptimizer's job, not ours.
  const remap = new Int32Array(oldCount);
  let newCount = 0;
  for (let v = 0; v < oldCount; v++) remap[v] = used[v] === 1 ? newCount++ : -1;

  if (newCount === oldCount) {
    // Nothing orphaned. Bail before reallocating ~20 MB of attributes for no change.
    return;
  }
  if (newCount === 0) {
    throw new Error('Compaction would remove every vertex; the index buffer is empty.');
  }

  for (const name of Object.keys(geometry.attributes)) {
    geometry.setAttribute(name, remapAttribute(geometry.attributes[name], remap, newCount));
  }
  // ⚠️ `geometry.morphAttributes` is NOT a Record in @types/three — it is an object with optional
  // `position`/`normal`/`color` keys only, so `Object.keys(...)` indexing does not type-check and
  // there is nowhere for a fourth morph channel to live anyway. Enumerate the three that exist.
  const morphChannels = ['position', 'normal', 'color'] as const;
  for (const channel of morphChannels) {
    const targets = geometry.morphAttributes[channel];
    if (!targets) continue;
    geometry.morphAttributes[channel] = targets.map((attr) =>
      remapAttribute(attr, remap, newCount)
    );
  }

  const newIndex = allocIndex(newCount, index.count);
  for (let i = 0; i < index.count; i++) newIndex[i] = remap[idx[i]];
  geometry.setIndex(new THREE.BufferAttribute(newIndex, 1));

  disposeBvh(geometry);
  geometry.computeBoundingBox();
  geometry.computeBoundingSphere();
}
