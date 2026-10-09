/*
 * Skin-weight painting in the browser.
 *
 * # Why this exists
 *
 * `stage.ts` can only DELETE a bad influence (`applyPrune`) — it finds the vertices bound to a joint
 * half a body away and zeroes them. That is the right repair for a leak and the wrong one for the
 * far commoner defect: a vertex bound to a *plausible but wrong* joint, or a seam where two adjacent
 * vertices disagree about which joint owns them. Those need REBINDING, and rebinding by hand needs a
 * brush.
 *
 * # What this module is, and is not
 *
 * Pure TypeScript over `THREE.SkinnedMesh` geometry. No Svelte, no DOM, no rendering. The caller owns
 * the pointer, the canvas, the cursor ring and the undo stack; this owns the maths and the buffers.
 * Feed it a WORLD-space point (from a raycast) and a WORLD-space radius and it edits `skinIndex` /
 * `skinWeight` in place, flags the touched slice for re-upload, and hands back a compact record of
 * what it changed.
 *
 * # The four things that are easy to get wrong, and how they are handled here
 *
 * 1. **Sum to one.** glTF skinning is `Σ wᵢ · Mᵢ · v`. A vertex whose four weights sum to 0.8 shrinks
 *    to 80% of its offset from the joint under animation, and one summing to 1.2 inflates — with no
 *    error anywhere. Every operation below is formulated as a LERP BETWEEN TWO VECTORS THAT EACH SUM
 *    TO ONE, which sums to one for free, and the final write re-apportions so it is exact even after
 *    integer quantisation. See [`writeInfluences`].
 *
 * 2. **There are only four slots.** `JOINTS_0`/`WEIGHTS_0` are `vec4`. Painting a fifth bone onto a
 *    vertex EVICTS one. See [`condenseInfluences`] for the rule and the reason.
 *
 * 3. **The attributes are usually normalized integers.** `WEIGHTS_0` arrives from a GLB as a
 *    normalized `Uint8` or `Uint16` routinely. See the block above [`readInfluences`].
 *
 * 4. **Co-located vertices must stay in agreement.** A GLB splits vertices at UV and material seams,
 *    so one point on the surface is often three vertices. Painting the one the ray happened to hit
 *    and leaving its twins TEARS THE SURFACE under animation. Every operation here works on WELDED
 *    POSITION CLASSES and writes the result to every member. See [`buildTopology`].
 *
 * # 🔴 REST POSE ONLY
 *
 * Same restriction as `mesh-repair.ts`'s `raycastFace`, for the same reason: the spatial index is
 * built over the raw `position` attribute, which matches the drawn surface only while the skinning
 * bracket is the identity. Under a playing clip this paints the vertices that WOULD be under the
 * cursor if the model were standing still. The caller must refuse to paint on a GLB carrying clips,
 * exactly as the joint editor already does.
 */

import * as THREE from 'three';

/* ------------------------------------------------------------------------------------------- */
/* Public shapes                                                                                */
/* ------------------------------------------------------------------------------------------- */

/** glTF's hard limit: `JOINTS_0` and `WEIGHTS_0` are `vec4`. */
export const MAX_INFLUENCES = 4;

/**
 * The weight below which an incoming influence is not worth evicting an existing one for.
 *
 * `1/255` is one quantisation step of a normalized `Uint8` `WEIGHTS_0` — an influence below it
 * rounds to zero in the file anyway, so trading a real binding for it is a pure loss. This is what
 * stops the faint rim of the brush from quietly demolishing the skin of everything it grazes.
 */
export const EVICTION_FLOOR = 1 / 255;

/** Influences smaller than this are treated as absent. Below float32's useful precision for a ratio. */
const MIN_WEIGHT = 1e-6;

/** A vertex's binding, as a sparse list. Parallel arrays, never longer than they need to be. */
export interface Influences {
  /** Indices into `mesh.skeleton.bones`. */
  bones: number[];
  /** Weights, denormalized to 0..1. Sum to 1 for any vertex this module has written. */
  weights: number[];
}

export type PaintMode = 'assign' | 'smooth' | 'erase';

/**
 * Brush profiles, as a function of `t = distance / radius`.
 *
 * All reach exactly 0 at `t = 1` — a brush with a hard edge leaves a visible ring of half-painted
 * vertices at the boundary of every dab, and those rings do not blend away with more strokes.
 */
export type FalloffProfile = 'smooth' | 'linear' | 'sharp' | 'constant';

export interface DabOptions {
  /** WORLD-space centre, i.e. `intersection.point` from a raycast. */
  point: THREE.Vector3;
  /** WORLD-space radius. */
  radius: number;
  mode: PaintMode;
  /** The joint being painted. Required by `assign` and `erase`, ignored by `smooth`. */
  bone?: THREE.Bone | null;
  /** How far toward the goal a single dab travels at full falloff. 0..1, default 0.5. */
  strength?: number;
  /** `assign` only: the weight the target bone is being driven TO. 0..1, default 1. */
  value?: number;
  profile?: FalloffProfile;
  /**
   * WORLD-space surface normal at `point`. When given together with `frontFacing`, vertices whose
   * own normal points away from it are left alone — without it a brush on the front of a thigh
   * paints the back of the thigh too, because a sphere does not know about surfaces.
   */
  normal?: THREE.Vector3 | null;
  /** Cull vertices facing away from `normal`. Ignored when `normal` is absent. Default true. */
  frontFacing?: boolean;
}

export interface DabResult {
  /** Vertices actually written. */
  vertices: number;
  /** Welded surface points written — `vertices` counts each split twin separately. */
  classes: number;
  /** Influences dropped to make room for a fifth bone. */
  evictions: number;
  /** Vertices skipped because the incoming weight was too faint to justify an eviction. */
  declined: number;
  /** Vertices left exactly as they were because the operation would have unskinned them. */
  spared: number;
  /** Meshes skipped because the target bone is not in their skeleton, or is unrepresentable. */
  meshesSkipped: number;
}

/** Pre-stroke state for one mesh, as RAW attribute elements. */
export interface MeshStrokeRecord {
  mesh: THREE.SkinnedMesh;
  /** Vertex ids, in first-touch order. */
  vertices: Uint32Array;
  /** `MAX_INFLUENCES` raw `skinIndex` elements per entry of `vertices`. */
  joints: THREE.TypedArray;
  /** `MAX_INFLUENCES` raw `skinWeight` elements per entry of `vertices`. */
  weights: THREE.TypedArray;
  /** `position.count` when the record was taken. A mismatch on restore means the mesh was rebuilt. */
  count: number;
}

/**
 * One stroke's worth of undo, holding ONLY the vertices the stroke touched.
 *
 * A full `skinWeight` copy of a hero tier is ~3.5 MB (and `RigWorkbench.copyWeights` takes exactly
 * that, which is why it is reserved for the coarse "put the weights back" button). A stroke touches
 * thousands of vertices, not a quarter of a million, so this is three to four orders of magnitude
 * smaller — see [`strokeByteLength`] for the exact per-vertex cost.
 */
export interface StrokeRecord {
  meshes: MeshStrokeRecord[];
  vertices: number;
  bytes: number;
}

/* ------------------------------------------------------------------------------------------- */
/* Attribute access — the normalized-integer rules                                              */
/* ------------------------------------------------------------------------------------------- */

type AttributeLike = THREE.BufferAttribute | THREE.InterleavedBufferAttribute;

function isInterleaved(attr: AttributeLike): attr is THREE.InterleavedBufferAttribute {
  return (attr as THREE.InterleavedBufferAttribute).isInterleavedBufferAttribute === true;
}

/**
 * ⚠️ `BufferGeometry.getAttribute` is typed as always returning an attribute and returns `undefined`
 * at runtime for one the geometry does not have. Same cast `stage.ts` makes, named once.
 */
function optionalAttribute(
  geometry: THREE.BufferGeometry,
  name: string
): AttributeLike | undefined {
  return geometry.getAttribute(name) as AttributeLike | undefined;
}

/** Where element `k` of vertex `v` lives in the underlying typed array, and what scales it. */
interface RawView {
  array: THREE.TypedArray;
  stride: number;
  offset: number;
  /**
   * The divisor `getX` would apply. 1 for a float or a non-normalized integer attribute; 255 for a
   * normalized `Uint8`, 65535 for a normalized `Uint16`, and so on — the exact table three's
   * `MathUtils.denormalize` switches on, verified against r185.
   */
  scale: number;
  /** True when a write has to land on an integer, i.e. when `scale !== 1`. */
  quantised: boolean;
}

/** Full-scale value for a normalized attribute of this array type. Mirrors three's `denormalize`. */
function fullScale(array: THREE.TypedArray): number {
  if (array instanceof Uint8Array || array instanceof Uint8ClampedArray) return 255;
  if (array instanceof Uint16Array) return 65535;
  if (array instanceof Uint32Array) return 4294967295;
  if (array instanceof Int8Array) return 127;
  if (array instanceof Int16Array) return 32767;
  if (array instanceof Int32Array) return 2147483647;
  return 1;
}

/** The largest joint index this attribute can hold. A bone beyond it cannot be painted. */
function indexCapacity(array: THREE.TypedArray): number {
  if (array instanceof Uint8Array || array instanceof Uint8ClampedArray) return 255;
  if (array instanceof Uint16Array) return 65535;
  if (array instanceof Uint32Array) return 4294967295;
  if (array instanceof Int8Array) return 127;
  if (array instanceof Int16Array) return 32767;
  if (array instanceof Int32Array) return 2147483647;
  // Float32 holds every integer exactly up to 2^24.
  return 16777216;
}

function rawView(attr: AttributeLike): RawView {
  const interleaved = isInterleaved(attr);
  const array = interleaved ? attr.data.array : (attr as THREE.BufferAttribute).array;
  return {
    array,
    stride: interleaved ? attr.data.stride : attr.itemSize,
    offset: interleaved ? attr.offset : 0,
    scale: attr.normalized ? fullScale(array) : 1,
    quantised: attr.normalized && fullScale(array) !== 1
  };
}

function base(view: RawView, vertex: number): number {
  return vertex * view.stride + view.offset;
}

/** The upload counter, which lives on the shared buffer for an interleaved attribute. */
function attributeVersion(attr: AttributeLike): number {
  return isInterleaved(attr) ? attr.data.version : (attr as THREE.BufferAttribute).version;
}

function markDirty(attr: AttributeLike, first: number, last: number): void {
  const view = rawView(attr);
  const start = base(view, first);
  const length = base(view, last) + MAX_INFLUENCES - start;
  if (isInterleaved(attr)) {
    attr.data.addUpdateRange(start, length);
    attr.data.needsUpdate = true;
  } else {
    attr.addUpdateRange(start, length);
    attr.needsUpdate = true;
  }
}

/**
 * # 🔴 The normalized-integer rule, and why it is the opposite of `mesh-repair`'s
 *
 * `mesh-repair.ts`'s `remapAttribute` forbids the `getX`/`setX` round trip and copies raw elements,
 * because it is MOVING a value it does not want to change and `denormalize(normalize(v))` quantises
 * every skin weight for nothing.
 *
 * Here we are CHANGING the value, so denormalising on read and requantising on write is exactly
 * right — but this module does it by hand rather than through `getX`/`setX`, for two reasons:
 *
 * - **The sum has to be exact in the STORED domain.** Four floats summing to 1.0 do not generally
 *   round to four integers summing to 255: `0.1/0.2/0.3/0.4 → 26/51/77/102 = 256`. Written through
 *   `setX` the vertex would inflate by 0.4% under animation, invisibly. [`writeInfluences`] uses
 *   largest-remainder apportionment so the stored integers sum to EXACTLY full scale.
 * - **`skinIndex` must never be denormalized.** glTF requires `JOINTS_0` unnormalized, but a file
 *   that sets the flag anyway would make `getX` divide a joint index by 255 and return a fraction.
 *   The raw bits ARE the joint index in every case, so this reads them raw and rounds.
 *
 * The scale table is three's own (`MathUtils.denormalize`, r185), so a value read here and a value
 * read through `getX` agree.
 */
export function readInfluences(mesh: THREE.SkinnedMesh, vertex: number): Influences {
  const skinIndex = optionalAttribute(mesh.geometry, 'skinIndex');
  const skinWeight = optionalAttribute(mesh.geometry, 'skinWeight');
  if (!skinIndex || !skinWeight) return { bones: [], weights: [] };
  return readRaw(rawView(skinIndex), rawView(skinWeight), vertex);
}

function readRaw(joints: RawView, weights: RawView, vertex: number): Influences {
  const jb = base(joints, vertex);
  const wb = base(weights, vertex);
  const out: Influences = { bones: [], weights: [] };
  for (let k = 0; k < MAX_INFLUENCES; k++) {
    const w = weights.array[wb + k] / weights.scale;
    if (w <= MIN_WEIGHT) continue;
    out.bones.push(Math.round(joints.array[jb + k]));
    out.weights.push(w);
  }
  return out;
}

/**
 * Write a binding back, renormalised to sum to exactly 1.
 *
 * On a normalized integer attribute "exactly" means the STORED integers sum to full scale, reached
 * by largest-remainder (Hamilton) apportionment: floor everything, then hand the shortfall to the
 * largest fractional parts. Ties break on slot order so the result is deterministic — a brush that
 * produces different bytes for the same gesture is not something you can debug.
 *
 * Empty slots are written as joint 0 / weight 0, which is what every exporter emits and what
 * `analyseSkin` already skips.
 */
export function writeInfluences(
  mesh: THREE.SkinnedMesh,
  vertex: number,
  influences: Influences
): void {
  const skinIndex = optionalAttribute(mesh.geometry, 'skinIndex');
  const skinWeight = optionalAttribute(mesh.geometry, 'skinWeight');
  if (!skinIndex || !skinWeight) return;
  writeRaw(rawView(skinIndex), rawView(skinWeight), vertex, influences);
  markDirty(skinIndex, vertex, vertex);
  markDirty(skinWeight, vertex, vertex);
}

function writeRaw(joints: RawView, weights: RawView, vertex: number, influences: Influences): void {
  const jb = base(joints, vertex);
  const wb = base(weights, vertex);

  let total = 0;
  for (const w of influences.weights) total += w;

  const slots: number[] = new Array(MAX_INFLUENCES).fill(0);
  const bones: number[] = new Array(MAX_INFLUENCES).fill(0);
  if (total > 0) {
    const n = Math.min(influences.bones.length, MAX_INFLUENCES);
    for (let k = 0; k < n; k++) {
      bones[k] = influences.bones[k];
      slots[k] = influences.weights[k] / total;
    }
  }

  if (weights.quantised) {
    const scale = weights.scale;
    const floors: number[] = [];
    let sum = 0;
    for (let k = 0; k < MAX_INFLUENCES; k++) {
      const exact = slots[k] * scale;
      const f = Math.floor(exact);
      floors.push(f);
      sum += f;
    }
    // Hand the shortfall to the largest fractional parts. `deficit` is at most MAX_INFLUENCES - 1
    // whenever the input sums to 1, and the modulo is defence against an input that does not.
    const order = floors
      .map((f, k) => ({ k, frac: slots[k] * scale - f }))
      .sort((a, b) => b.frac - a.frac || a.k - b.k);
    const deficit = Math.max(0, Math.round(scale - sum));
    for (let i = 0; i < deficit; i++) floors[order[i % order.length].k] += 1;
    for (let k = 0; k < MAX_INFLUENCES; k++) {
      joints.array[jb + k] = bones[k];
      weights.array[wb + k] = floors[k];
    }
    return;
  }

  for (let k = 0; k < MAX_INFLUENCES; k++) {
    joints.array[jb + k] = bones[k];
    weights.array[wb + k] = slots[k] * weights.scale;
  }
}

/* ------------------------------------------------------------------------------------------- */
/* Falloff                                                                                      */
/* ------------------------------------------------------------------------------------------- */

/**
 * Brush profile at `t = distance / radius`, clamped to 0 outside the brush.
 *
 * `smooth` is the default and is the inverse smoothstep `1 - (3t² - 2t³)`: value 1 at the centre, 0
 * at the rim, and ZERO SLOPE at both ends. The zero slope at the rim is the point — a linear profile
 * leaves a discontinuity in the second derivative of the painted field, which reads as a faint ring
 * around every dab once the mesh deforms.
 */
export function falloffAt(t: number, profile: FalloffProfile = 'smooth'): number {
  if (!(t >= 0)) return 1;
  if (t >= 1) return 0;
  switch (profile) {
    case 'constant':
      return 1;
    case 'linear':
      return 1 - t;
    case 'sharp':
      return (1 - t) * (1 - t);
    case 'smooth':
    default:
      return 1 - t * t * (3 - 2 * t);
  }
}

/* ------------------------------------------------------------------------------------------- */
/* The eviction rule                                                                            */
/* ------------------------------------------------------------------------------------------- */

export interface CondenseResult extends Influences {
  /** How many influences were dropped to fit in four slots. */
  evicted: number;
}

/**
 * Fold a working binding down to at most four influences summing to 1.
 *
 * # 🔴 The eviction rule
 *
 * Smoothing unions the bones of every neighbour, so a vertex routinely arrives here with six or
 * eight candidate influences and four slots. The rule, in order:
 *
 * 1. **Duplicates merge.** The same bone in two slots is a malformed rig, not two influences; their
 *    weights add.
 * 2. **The painted bone is immune.** If `protectedBone` survives at all it keeps its slot regardless
 *    of size. Evicting the bone the user is actively painting makes the stroke a silent no-op, and a
 *    tool that ignores you is worse than one that does the wrong thing visibly.
 * 3. **Everything else: smallest weight goes first**, ties broken by higher bone index so the result
 *    is deterministic. Smallest-first is the choice with the smallest positional consequence — the
 *    dropped influence's mass is redistributed proportionally, so the vertex moves by roughly the
 *    dropped weight times the distance between the dropped joint and the survivors.
 * 4. **Survivors are renormalised** to sum to exactly 1, exactly as `applyPrune` does. Dropping an
 *    influence without renormalising leaves the vertex summing to less than 1, which shrinks it
 *    toward its joints under animation.
 *
 * ⚠️ What this deliberately does NOT do is decide whether the eviction was worth making. That is the
 * caller's judgement and it is made at the paint site against [`EVICTION_FLOOR`]: an incoming
 * influence too faint to survive quantisation must never cost a real binding.
 */
export function condenseInfluences(
  influences: Influences,
  protectedBone: number | null = null
): CondenseResult {
  const merged = new Map<number, number>();
  for (let i = 0; i < influences.bones.length; i++) {
    const w = influences.weights[i];
    if (!(w > MIN_WEIGHT)) continue;
    const b = influences.bones[i];
    merged.set(b, (merged.get(b) ?? 0) + w);
  }

  const entries = [...merged.entries()].map(([bone, weight]) => ({ bone, weight }));
  if (!entries.length) return { bones: [], weights: [], evicted: 0 };

  let evicted = 0;
  let kept = entries;
  if (entries.length > MAX_INFLUENCES) {
    const ranked = [...entries].sort((a, b) => b.weight - a.weight || a.bone - b.bone);
    const survivors: typeof entries = [];
    const immune = ranked.find((e) => e.bone === protectedBone);
    if (immune) survivors.push(immune);
    for (const e of ranked) {
      if (survivors.length >= MAX_INFLUENCES) break;
      if (e === immune) continue;
      survivors.push(e);
    }
    evicted = entries.length - survivors.length;
    // Back to the original order so the stored slots stay as stable as they can between dabs.
    const keep = new Set(survivors);
    kept = entries.filter((e) => keep.has(e));
  }

  let total = 0;
  for (const e of kept) total += e.weight;
  return {
    bones: kept.map((e) => e.bone),
    weights: kept.map((e) => e.weight / total),
    evicted
  };
}

/** Does adding `bone` to this binding require throwing something out? */
function needsEviction(influences: Influences, bone: number): boolean {
  if (influences.bones.includes(bone)) return false;
  let distinct = 0;
  const seen = new Set<number>();
  for (let i = 0; i < influences.bones.length; i++) {
    if (!(influences.weights[i] > MIN_WEIGHT)) continue;
    if (seen.has(influences.bones[i])) continue;
    seen.add(influences.bones[i]);
    distinct++;
  }
  return distinct >= MAX_INFLUENCES;
}

/* ------------------------------------------------------------------------------------------- */
/* Sparse vector arithmetic                                                                     */
/* ------------------------------------------------------------------------------------------- */

function accumulate(into: Map<number, number>, influences: Influences, scale: number): void {
  for (let i = 0; i < influences.bones.length; i++) {
    const b = influences.bones[i];
    into.set(b, (into.get(b) ?? 0) + influences.weights[i] * scale);
  }
}

function fromMap(map: Map<number, number>): Influences {
  const bones: number[] = [];
  const weights: number[] = [];
  for (const [bone, weight] of map) {
    if (!(weight > MIN_WEIGHT)) continue;
    bones.push(bone);
    weights.push(weight);
  }
  return { bones, weights };
}

/**
 * `(1 - a)·from + a·to`.
 *
 * 🔴 This is the shape EVERY operation below reduces to, and it is the whole renormalisation story:
 * `from` sums to 1 and `to` sums to 1, so the result sums to `(1 - a) + a = 1` by construction, for
 * any `a`. There is no place left for a rounding drift to accumulate over a thousand dabs, because
 * nothing is ever incremented in place — each dab computes a fresh convex combination.
 */
function lerpInfluences(from: Influences, to: Influences, a: number): Influences {
  if (a <= 0) return from;
  if (a >= 1) return to;
  const acc = new Map<number, number>();
  accumulate(acc, from, 1 - a);
  accumulate(acc, to, a);
  return fromMap(acc);
}

/** The vector with `bone` at `value` and the existing OTHER influences sharing the remainder. */
function goalAssign(current: Influences, bone: number, value: number): Influences {
  const v = Math.min(1, Math.max(0, value));
  const acc = new Map<number, number>();
  let others = 0;
  for (let i = 0; i < current.bones.length; i++) {
    if (current.bones[i] === bone) continue;
    others += current.weights[i];
  }
  if (others > MIN_WEIGHT && v < 1) {
    const k = (1 - v) / others;
    for (let i = 0; i < current.bones.length; i++) {
      if (current.bones[i] === bone) continue;
      acc.set(current.bones[i], (acc.get(current.bones[i]) ?? 0) + current.weights[i] * k);
    }
    acc.set(bone, v);
  } else {
    // Nowhere to put the remainder — the bone takes the lot rather than the vertex losing mass.
    acc.set(bone, 1);
  }
  return fromMap(acc);
}

/** The vector with `bone` removed and its mass shared proportionally. `null` when it cannot be. */
function goalErase(current: Influences, bone: number): Influences | null {
  let others = 0;
  for (let i = 0; i < current.bones.length; i++) {
    if (current.bones[i] === bone) continue;
    others += current.weights[i];
  }
  // 🔴 The vertex is bound to this bone and nothing else. Erasing would UNSKIN it, which is the one
  // failure `applyPrune` refuses to ship for the same reason: an unskinned vertex deforms nothing.
  if (!(others > MIN_WEIGHT)) return null;
  const acc = new Map<number, number>();
  for (let i = 0; i < current.bones.length; i++) {
    if (current.bones[i] === bone) continue;
    acc.set(current.bones[i], (acc.get(current.bones[i]) ?? 0) + current.weights[i] / others);
  }
  return fromMap(acc);
}

/* ------------------------------------------------------------------------------------------- */
/* Spatial index + welded topology                                                              */
/* ------------------------------------------------------------------------------------------- */

/**
 * # Why a uniform grid and not the BVH
 *
 * The obvious move is to reuse `mesh-repair.ts`'s cached `MeshBVH` — it is already built, already
 * paid for, and already the thing under the cursor. It is the wrong tool here, three times over:
 *
 * - **It is not exported.** `getBvh` is module-private; only `raycastFace` and `disposeBvh` are
 *   public. Building our own second tree would cost hundreds of milliseconds at 440k triangles and
 *   another ~50 MB, and a tree built WITHOUT `{ indirect: true }` partitions `geometry.index` IN
 *   PLACE — scrambling every `faceIndex` the repair UI is holding, silently.
 * - **It indexes triangles, not vertices.** A sphere shapecast returns faces; turning those into a
 *   deduplicated vertex set is a per-query hash on top of the traversal.
 * - **A brush query is not a ray query.** A BVH earns its build cost on long thin queries against
 *   the whole model. A brush is a small sphere in a known place — the query a uniform grid answers
 *   in one bucket walk.
 *
 * So: a dense uniform grid over the raw `position` attribute, built once per geometry and cached
 * against it. `O(n)` single-pass build (a few ms at 250k), `count` Int32 for the linked list plus at
 * most 64³ Int32 cell heads — about 2 MB total, against 3.5 MB for one copy of `skinWeight`.
 *
 * The divisions are capped at 64 per axis so the head array is dense and the cell key is a plain
 * integer; with no hashing there are no collisions and therefore no duplicate visits to dedupe. A
 * dab scans the cells its sphere overlaps, so the cost tracks the BRUSH volume, not the vertex
 * count — which is the property that makes 250k vertices at pointer-move rate affordable at all.
 */
interface Grid {
  positions: Float32Array;
  minX: number;
  minY: number;
  minZ: number;
  cell: number;
  dimX: number;
  dimY: number;
  dimZ: number;
  head: Int32Array;
  next: Int32Array;
}

const MAX_DIVISIONS = 64;

/**
 * Positions as a tight `Float32Array`.
 *
 * ⚠️ Same call as `mesh-repair`'s `readPositions` and for the same reason: `getX/getY/getZ` are the
 * right tool for a POSITION because under `KHR_mesh_quantization` it is a normalized `Int16` and the
 * decoded float is what three renders. The plain case is aliased rather than copied — nothing here
 * writes through it.
 */
function readPositions(attr: AttributeLike): Float32Array {
  const count = attr.count;
  if (!isInterleaved(attr)) {
    const plain = attr as THREE.BufferAttribute;
    if (plain.array instanceof Float32Array && !plain.normalized) {
      return plain.array.subarray(0, count * 3);
    }
  }
  const out = new Float32Array(count * 3);
  for (let i = 0; i < count; i++) {
    out[i * 3] = attr.getX(i);
    out[i * 3 + 1] = attr.getY(i);
    out[i * 3 + 2] = attr.getZ(i);
  }
  return out;
}

function buildGrid(positions: Float32Array, count: number): Grid {
  let minX = Infinity;
  let minY = Infinity;
  let minZ = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  let maxZ = -Infinity;
  for (let i = 0; i < count; i++) {
    const x = positions[i * 3];
    const y = positions[i * 3 + 1];
    const z = positions[i * 3 + 2];
    if (x < minX) minX = x;
    if (y < minY) minY = y;
    if (z < minZ) minZ = z;
    if (x > maxX) maxX = x;
    if (y > maxY) maxY = y;
    if (z > maxZ) maxZ = z;
  }
  if (!Number.isFinite(minX)) {
    minX = minY = minZ = 0;
    maxX = maxY = maxZ = 0;
  }

  const span = Math.max(maxX - minX, maxY - minY, maxZ - minZ);
  // `cbrt(count)` aims at roughly one vertex per cell for a volume-filling point set; a surface puts
  // rather more in each occupied cell, which is fine — it is the SPHERE that bounds the work.
  const divisions = Math.min(MAX_DIVISIONS, Math.max(1, Math.ceil(Math.cbrt(Math.max(count, 1)))));
  const cell = span > 0 ? span / divisions : 1;
  const dimX = Math.max(1, Math.min(MAX_DIVISIONS, Math.floor((maxX - minX) / cell) + 1));
  const dimY = Math.max(1, Math.min(MAX_DIVISIONS, Math.floor((maxY - minY) / cell) + 1));
  const dimZ = Math.max(1, Math.min(MAX_DIVISIONS, Math.floor((maxZ - minZ) / cell) + 1));

  const head = new Int32Array(dimX * dimY * dimZ).fill(-1);
  const next = new Int32Array(count).fill(-1);
  const grid: Grid = { positions, minX, minY, minZ, cell, dimX, dimY, dimZ, head, next };
  for (let i = 0; i < count; i++) {
    const key = cellKey(
      grid,
      axisCell(positions[i * 3], minX, cell, dimX),
      axisCell(positions[i * 3 + 1], minY, cell, dimY),
      axisCell(positions[i * 3 + 2], minZ, cell, dimZ)
    );
    next[i] = head[key];
    head[key] = i;
  }
  return grid;
}

function axisCell(value: number, min: number, cell: number, dim: number): number {
  const c = Math.floor((value - min) / cell);
  return c < 0 ? 0 : c >= dim ? dim - 1 : c;
}

function cellKey(grid: Grid, ix: number, iy: number, iz: number): number {
  return (ix * grid.dimY + iy) * grid.dimZ + iz;
}

/** Every vertex within `radius` of the point, with its squared distance. Local/geometry space. */
function forEachInSphere(
  grid: Grid,
  x: number,
  y: number,
  z: number,
  radius: number,
  visit: (vertex: number, distanceSq: number) => void
): void {
  if (!(radius > 0)) return;
  const r2 = radius * radius;
  const lo = (v: number, min: number, dim: number) => axisCell(v - radius, min, grid.cell, dim);
  const hi = (v: number, min: number, dim: number) => axisCell(v + radius, min, grid.cell, dim);
  const x0 = lo(x, grid.minX, grid.dimX);
  const x1 = hi(x, grid.minX, grid.dimX);
  const y0 = lo(y, grid.minY, grid.dimY);
  const y1 = hi(y, grid.minY, grid.dimY);
  const z0 = lo(z, grid.minZ, grid.dimZ);
  const z1 = hi(z, grid.minZ, grid.dimZ);
  const p = grid.positions;
  for (let ix = x0; ix <= x1; ix++) {
    for (let iy = y0; iy <= y1; iy++) {
      for (let iz = z0; iz <= z1; iz++) {
        for (let v = grid.head[cellKey(grid, ix, iy, iz)]; v !== -1; v = grid.next[v]) {
          const dx = p[v * 3] - x;
          const dy = p[v * 3 + 1] - y;
          const dz = p[v * 3 + 2] - z;
          const d2 = dx * dx + dy * dy + dz * dz;
          if (d2 <= r2) visit(v, d2);
        }
      }
    }
  }
}

/**
 * Welded position classes, and (lazily) the class-level edge graph.
 *
 * # 🔴 Why weld at all
 *
 * A GLB splits a vertex wherever the UV, the normal or the material changes, so one point on the
 * surface is commonly two or three vertices sharing an identical position. Two consequences, both of
 * which this module has to handle rather than inherit:
 *
 * - **Painting must write all of them.** They render as one point. Rebinding the one the ray
 *   happened to reach leaves its twins on the old joint, and the surface pulls apart along the seam
 *   the moment the joint moves. Every write below goes to the whole class.
 * - **Smoothing must cross them.** Triangle adjacency stops dead at a seam, so an unwelded smooth
 *   averages a seam vertex only with the half of its neighbourhood on its own side — producing a
 *   different answer on each side of the seam, which is the exact defect it was invoked to fix.
 *
 * Welding is by EXACT position equality, not a tolerance. Split vertices carry bit-identical
 * positions because they came from one source vertex; a tolerance would additionally weld
 * genuinely-distinct surfaces that happen to touch, which on a mesh assembled from ~200
 * interpenetrating shells is most of the model.
 */
interface Topology {
  classOf: Int32Array;
  classStart: Uint32Array;
  classMembers: Uint32Array;
  classCount: number;
  /** Built on the first `smooth`, never for `assign`/`erase`. */
  adjacency: { start: Uint32Array; list: Uint32Array } | null;
}

interface PaintCache {
  attribute: AttributeLike;
  version: number;
  count: number;
  grid: Grid;
  topology: Topology;
}

const caches = new WeakMap<THREE.BufferGeometry, PaintCache>();

/**
 * Drop the cached grid and topology for a geometry.
 *
 * Same discipline as `mesh-repair.ts`'s `disposeBvh`, and needed for the same reason: an index built
 * over stale positions reports vertices that have moved. `compactVertices` renumbering is caught
 * automatically (the vertex count and the attribute identity both change), but a caller that edits
 * positions in place has to say so.
 */
export function disposeWeightPaintCache(geometry: THREE.BufferGeometry): void {
  caches.delete(geometry);
}

function paintCache(geometry: THREE.BufferGeometry): PaintCache | null {
  const attribute = optionalAttribute(geometry, 'position');
  if (!attribute) return null;
  const existing = caches.get(geometry);
  if (
    existing &&
    existing.attribute === attribute &&
    existing.count === attribute.count &&
    existing.version === attributeVersion(attribute)
  ) {
    return existing;
  }
  const positions = readPositions(attribute);
  const count = attribute.count;
  const grid = buildGrid(positions, count);
  const fresh: PaintCache = {
    attribute,
    version: attributeVersion(attribute),
    count,
    grid,
    topology: buildTopology(positions, count)
  };
  caches.set(geometry, fresh);
  return fresh;
}

const _hashScratch = new Float32Array(3);
const _hashBits = new Uint32Array(_hashScratch.buffer);

/** FNV-1a over the three float32 BIT PATTERNS, so equal positions always hash equal. */
function positionHash(x: number, y: number, z: number): number {
  _hashScratch[0] = x;
  _hashScratch[1] = y;
  _hashScratch[2] = z;
  let h = 2166136261;
  h = Math.imul(h ^ _hashBits[0], 16777619);
  h = Math.imul(h ^ _hashBits[1], 16777619);
  h = Math.imul(h ^ _hashBits[2], 16777619);
  return h >>> 0;
}

function buildTopology(positions: Float32Array, count: number): Topology {
  const representative = new Int32Array(count);
  const chain = new Int32Array(count).fill(-1);
  const heads = new Map<number, number>();

  for (let v = 0; v < count; v++) {
    const x = positions[v * 3];
    const y = positions[v * 3 + 1];
    const z = positions[v * 3 + 2];
    const h = positionHash(x, y, z);
    const head = heads.get(h) ?? -1;
    let found = -1;
    // Only representatives ever enter a chain, so these walks stay one or two links long even when
    // the hash collides.
    for (let w = head; w !== -1; w = chain[w]) {
      if (positions[w * 3] === x && positions[w * 3 + 1] === y && positions[w * 3 + 2] === z) {
        found = w;
        break;
      }
    }
    if (found === -1) {
      chain[v] = head;
      heads.set(h, v);
      representative[v] = v;
    } else {
      representative[v] = found;
    }
  }

  const classOf = new Int32Array(count);
  const idOfRep = new Int32Array(count).fill(-1);
  let classCount = 0;
  for (let v = 0; v < count; v++) {
    const r = representative[v];
    if (idOfRep[r] < 0) idOfRep[r] = classCount++;
    classOf[v] = idOfRep[r];
  }

  const classStart = new Uint32Array(classCount + 1);
  for (let v = 0; v < count; v++) classStart[classOf[v] + 1]++;
  for (let c = 0; c < classCount; c++) classStart[c + 1] += classStart[c];
  const cursor = classStart.slice(0, classCount);
  const classMembers = new Uint32Array(count);
  for (let v = 0; v < count; v++) classMembers[cursor[classOf[v]]++] = v;

  return { classOf, classStart, classMembers, classCount, adjacency: null };
}

/**
 * Class-level edge graph, deduplicated, in CSR form. Built on demand — `assign` and `erase` never
 * need it and it is the most expensive structure here (~5 MB at 440k triangles after dedupe).
 *
 * Non-indexed geometry is handled by treating consecutive triples as triangles, which is what three
 * does when it draws one.
 */
function adjacencyOf(
  geometry: THREE.BufferGeometry,
  topology: Topology
): {
  start: Uint32Array;
  list: Uint32Array;
} {
  if (topology.adjacency) return topology.adjacency;

  const index = geometry.getIndex();
  const classOf = topology.classOf;
  const triangles = index ? index.count : (optionalAttribute(geometry, 'position')?.count ?? 0);
  const corner = (i: number): number => (index ? index.getX(i) : i);

  const degree = new Uint32Array(topology.classCount + 1);
  const bump = (a: number, b: number): void => {
    const ca = classOf[a];
    const cb = classOf[b];
    if (ca === cb) return;
    degree[ca + 1]++;
    degree[cb + 1]++;
  };
  for (let t = 0; t + 2 < triangles; t += 3) {
    const a = corner(t);
    const b = corner(t + 1);
    const c = corner(t + 2);
    bump(a, b);
    bump(b, c);
    bump(c, a);
  }
  for (let c = 0; c < topology.classCount; c++) degree[c + 1] += degree[c];

  const cursor = degree.slice(0, topology.classCount);
  const list = new Uint32Array(degree[topology.classCount]);
  const put = (a: number, b: number): void => {
    const ca = classOf[a];
    const cb = classOf[b];
    if (ca === cb) return;
    list[cursor[ca]++] = cb;
    list[cursor[cb]++] = ca;
  };
  for (let t = 0; t + 2 < triangles; t += 3) {
    const a = corner(t);
    const b = corner(t + 1);
    const c = corner(t + 2);
    put(a, b);
    put(b, c);
    put(c, a);
  }

  // Dedupe in place. Every edge is entered twice (once per adjacent triangle) and a smoothing kernel
  // that counts a neighbour twice is a DIFFERENT operator on a boundary edge than in the interior,
  // which shows up as the mesh border smoothing at a visibly different rate.
  const start = new Uint32Array(topology.classCount + 1);
  let write = 0;
  for (let c = 0; c < topology.classCount; c++) {
    const from = degree[c];
    const to = degree[c + 1];
    const row = list.subarray(from, to);
    row.sort();
    let previous = -1;
    for (let i = 0; i < row.length; i++) {
      if (row[i] === previous) continue;
      previous = row[i];
      list[write++] = previous;
    }
    start[c + 1] = write;
  }
  const adjacency = { start, list: list.slice(0, write) };
  topology.adjacency = adjacency;
  return adjacency;
}

/* ------------------------------------------------------------------------------------------- */
/* World ↔ geometry space                                                                       */
/* ------------------------------------------------------------------------------------------- */

/**
 * The matrix carrying `geometry.attributes.position` to WORLD at rest.
 *
 * 🔴 Not always `matrixWorld`, and the difference is silent. Reproduces the rule `mesh-repair.ts`'s
 * `restToWorld` documents in full: three renders a skinned vertex as
 * `matrixWorld · bindMatrixInverse · Σwᵢ(boneWorldᵢ · boneInverseᵢ) · bindMatrix · p`, the Σ is the
 * identity at rest, and `AttachedBindMode` (what `GLTFLoader` produces) makes `bindMatrixInverse`
 * equal `matrixWorld⁻¹` — so the rest surface sits at `bindMatrix · p`. Using `matrixWorld` here
 * would miss by exactly the node transform, invisibly on any asset whose node transform is identity.
 */
function restToWorld(mesh: THREE.SkinnedMesh): THREE.Matrix4 {
  return mesh.isSkinnedMesh && mesh.bindMode === THREE.AttachedBindMode
    ? mesh.bindMatrix
    : mesh.matrixWorld;
}

const _localPoint = new THREE.Vector3();
const _inverse = new THREE.Matrix4();
const _normalToLocal = new THREE.Matrix3();
const _localNormal = new THREE.Vector3();
const _vertexNormal = new THREE.Vector3();

interface LocalBrush {
  x: number;
  y: number;
  z: number;
  radius: number;
  normal: THREE.Vector3 | null;
}

/**
 * Put a world-space brush into a mesh's geometry space.
 *
 * ⚠️ The radius is scaled by the transform's LARGEST axis scale. Under a non-uniform scale a
 * world-space sphere is an ellipsoid in geometry space, and this approximates it with the enclosing
 * sphere — the brush then reaches slightly further along the squashed axes than the cursor ring
 * suggests. Our characters carry a uniform (usually identity) transform, so this is exact in
 * practice and conservative when it is not.
 */
function toLocal(mesh: THREE.SkinnedMesh, options: DabOptions): LocalBrush {
  const matrix = restToWorld(mesh);
  _inverse.copy(matrix).invert();
  _localPoint.copy(options.point).applyMatrix4(_inverse);
  const scale = matrix.getMaxScaleOnAxis() || 1;
  let normal: THREE.Vector3 | null = null;
  if (options.normal && options.frontFacing !== false) {
    // World → local for a NORMAL is `Mᵀ` (the inverse of the normal matrix `(M⁻¹)ᵀ`). Only the sign
    // of the dot product matters, so the missing normalisation factor is irrelevant.
    _normalToLocal.setFromMatrix4(matrix).transpose();
    normal = _localNormal.copy(options.normal).applyMatrix3(_normalToLocal).normalize();
  }
  return {
    x: _localPoint.x,
    y: _localPoint.y,
    z: _localPoint.z,
    radius: options.radius / scale,
    normal
  };
}

/* ------------------------------------------------------------------------------------------- */
/* Stroke recording                                                                             */
/* ------------------------------------------------------------------------------------------- */

interface MeshScratch {
  /** 1 where this stroke has already captured the vertex's pre-stroke state. */
  seen: Uint8Array;
  vertices: number[];
  joints: number[];
  weights: number[];
}

function allocLike(source: THREE.TypedArray, length: number): THREE.TypedArray {
  // Preserving the constructor is the point: a Uint8 `skinWeight` restored into a Float32Array would
  // read back 255× too large. Same rule as `mesh-repair`'s `allocLike`.
  const Ctor = source.constructor as unknown as new (n: number) => THREE.TypedArray;
  return new Ctor(length);
}

/**
 * The undo cost of one stroke, in bytes.
 *
 * The record holds, per touched vertex: a `Uint32` vertex id, four raw `skinIndex` elements and four
 * raw `skinWeight` elements. So the per-vertex cost depends entirely on the attribute types the GLB
 * shipped:
 *
 * | `JOINTS_0` | `WEIGHTS_0` | bytes / vertex |
 * |---|---|---|
 * | `Uint8`  | `Uint8` normalized  | **12** |
 * | `Uint16` | `Uint16` normalized | **20** |
 * | `Uint16` | `Float32`           | **28** |
 *
 * A 5,000-vertex stroke on the common `Uint8`/`Uint8` layout is therefore **60 KB**, against 3.5 MB
 * for the full-attribute copy `RigWorkbench.copyWeights` takes. A hundred strokes of history cost
 * about what two full copies do.
 */
export function strokeByteLength(record: StrokeRecord): number {
  let bytes = 0;
  for (const m of record.meshes) {
    bytes += m.vertices.byteLength + m.joints.byteLength + m.weights.byteLength;
  }
  return bytes;
}

/**
 * Put a stroke record back. Exact — the raw attribute elements are restored bit for bit, so an undo
 * cannot quantise anything the stroke did not already quantise.
 *
 * ⚠️ A vertex-count mismatch means the mesh was rebuilt (only `compactVertices` renumbers vertices)
 * and the record's vertex ids now point at different geometry. That mesh is SKIPPED rather than
 * scrambled, mirroring `RigWorkbench.writeWeights`' length guard.
 */
export function restoreStroke(record: StrokeRecord): void {
  for (const entry of record.meshes) {
    const skinIndex = optionalAttribute(entry.mesh.geometry, 'skinIndex');
    const skinWeight = optionalAttribute(entry.mesh.geometry, 'skinWeight');
    const position = optionalAttribute(entry.mesh.geometry, 'position');
    if (!skinIndex || !skinWeight || !position || position.count !== entry.count) continue;
    const joints = rawView(skinIndex);
    const weights = rawView(skinWeight);
    let first = Infinity;
    let last = -Infinity;
    for (let i = 0; i < entry.vertices.length; i++) {
      const v = entry.vertices[i];
      const jb = base(joints, v);
      const wb = base(weights, v);
      for (let k = 0; k < MAX_INFLUENCES; k++) {
        joints.array[jb + k] = entry.joints[i * MAX_INFLUENCES + k];
        weights.array[wb + k] = entry.weights[i * MAX_INFLUENCES + k];
      }
      if (v < first) first = v;
      if (v > last) last = v;
    }
    if (last >= 0) {
      markDirty(skinIndex, first, last);
      markDirty(skinWeight, first, last);
    }
  }
}

/** What a transfer moved, and the undo for it. */
export interface TransferResult {
  /** Vertices that were bound to the joint at all. */
  vertices: number;
  /** Of those, the ones the joint was the ONLY influence on — they are now bound entirely to the
   *  target, and they are the reason `erase` cannot do this job: erasing a vertex's last influence
   *  would unskin it, so the brush spares exactly these. */
  sole: number;
  /** The state before, over exactly the vertices touched. Feed it to [`restoreStroke`]. */
  before: StrokeRecord;
}

/**
 * Move every influence a joint has onto another joint.
 *
 * This is the half of "delete a bone" that matters to the character: after it, no vertex references
 * the joint at all, so removing it from the skeleton cannot change how the mesh deforms. Run it
 * BEFORE the removal, never after — a joint index that vanishes from under a live binding is a
 * vertex bound to whatever bone inherits its index.
 *
 * The merge rule is a sum, then a renormalise: a vertex weighted 0.3 to the doomed joint and 0.5 to
 * its parent comes out 0.8 on the parent, and `writeRaw` re-apportions the four slots so the stored
 * integers still sum to exactly full scale. A vertex already carrying both keeps ONE slot where it
 * had two, which is a slot given back rather than an eviction.
 *
 * ⚠️ Co-located vertices need no special handling here, unlike the brush: this rewrites a binding by
 * JOINT ID over the whole mesh, so a seam's three copies are all touched by the same rule and cannot
 * come out of agreement.
 */
export function transferInfluence(
  meshes: THREE.SkinnedMesh[],
  from: number,
  to: number
): TransferResult {
  const records: MeshStrokeRecord[] = [];
  let vertices = 0;
  let sole = 0;
  if (from === to) return { vertices: 0, sole: 0, before: { meshes: [], vertices: 0, bytes: 0 } };

  for (const mesh of meshes) {
    const skinIndex = optionalAttribute(mesh.geometry, 'skinIndex');
    const skinWeight = optionalAttribute(mesh.geometry, 'skinWeight');
    const position = optionalAttribute(mesh.geometry, 'position');
    if (!skinIndex || !skinWeight || !position) continue;
    const joints = rawView(skinIndex);
    const weights = rawView(skinWeight);

    // Two passes: which vertices, then the edit. The record has to be allocated at the exact size,
    // and a growing array of typed-array rows would cost more than the second walk.
    const touched: number[] = [];
    for (let v = 0; v < position.count; v++) {
      const jb = base(joints, v);
      for (let k = 0; k < MAX_INFLUENCES; k++) {
        // 🔴 Any slot NAMING the joint, whatever its weight. A slot carrying the joint at a weight
        // below `MIN_WEIGHT` drives nothing and is invisible to `readInfluences` — but the joint
        // INDEX is still sitting in `JOINTS_0`, and `glb-patch`'s removal refuses to renumber a file
        // where a vertex still points at the joint being cut. Filtering by weight here left exactly
        // those vertices behind and turned a dissolve into a save that would not go through.
        if (Math.round(joints.array[jb + k]) !== from) continue;
        touched.push(v);
        break;
      }
    }
    if (!touched.length) continue;

    const ids = Uint32Array.from(touched);
    const jOut = allocLike(joints.array, ids.length * MAX_INFLUENCES);
    const wOut = allocLike(weights.array, ids.length * MAX_INFLUENCES);
    for (let i = 0; i < ids.length; i++) {
      const jb = base(joints, ids[i]);
      const wb = base(weights, ids[i]);
      for (let k = 0; k < MAX_INFLUENCES; k++) {
        jOut[i * MAX_INFLUENCES + k] = joints.array[jb + k];
        wOut[i * MAX_INFLUENCES + k] = weights.array[wb + k];
      }
    }
    records.push({ mesh, vertices: ids, joints: jOut, weights: wOut, count: position.count });

    for (const v of ids) {
      // `readRaw` drops the slots below `MIN_WEIGHT`, so a weightless reference to the joint simply
      // does not come back — and the write below therefore erases it. That is the point.
      const current = readRaw(joints, weights, v);
      if (current.bones.length === 1 && current.bones[0] === from) sole++;
      const next: Influences = { bones: [], weights: [] };
      for (let k = 0; k < current.bones.length; k++) {
        const bone = current.bones[k] === from ? to : current.bones[k];
        const at = next.bones.indexOf(bone);
        if (at >= 0) next.weights[at] += current.weights[k];
        else {
          next.bones.push(bone);
          next.weights.push(current.weights[k]);
        }
      }
      writeRaw(joints, weights, v, next);
      vertices++;
    }
    markDirty(skinIndex, ids[0], ids[ids.length - 1]);
    markDirty(skinWeight, ids[0], ids[ids.length - 1]);
  }

  const before: StrokeRecord = { meshes: records, vertices, bytes: 0 };
  before.bytes = strokeByteLength(before);
  return { vertices, sole, before };
}

/**
 * Re-read the CURRENT state of exactly the vertices a record covers.
 *
 * This is the redo half: `commitStroke` hands back the state BEFORE the stroke, and calling this on
 * it immediately afterwards captures the state AFTER, at the same size and over the same vertices.
 * A history entry is then two of these and costs twice [`strokeByteLength`].
 */
export function captureLike(record: StrokeRecord): StrokeRecord {
  const meshes: MeshStrokeRecord[] = [];
  let vertices = 0;
  for (const entry of record.meshes) {
    const skinIndex = optionalAttribute(entry.mesh.geometry, 'skinIndex');
    const skinWeight = optionalAttribute(entry.mesh.geometry, 'skinWeight');
    const position = optionalAttribute(entry.mesh.geometry, 'position');
    if (!skinIndex || !skinWeight || !position || position.count !== entry.count) continue;
    const joints = rawView(skinIndex);
    const weights = rawView(skinWeight);
    const jOut = allocLike(joints.array, entry.vertices.length * MAX_INFLUENCES);
    const wOut = allocLike(weights.array, entry.vertices.length * MAX_INFLUENCES);
    for (let i = 0; i < entry.vertices.length; i++) {
      const jb = base(joints, entry.vertices[i]);
      const wb = base(weights, entry.vertices[i]);
      for (let k = 0; k < MAX_INFLUENCES; k++) {
        jOut[i * MAX_INFLUENCES + k] = joints.array[jb + k];
        wOut[i * MAX_INFLUENCES + k] = weights.array[wb + k];
      }
    }
    vertices += entry.vertices.length;
    meshes.push({
      mesh: entry.mesh,
      vertices: entry.vertices.slice(),
      joints: jOut,
      weights: wOut,
      count: entry.count
    });
  }
  const record2: StrokeRecord = { meshes, vertices, bytes: 0 };
  record2.bytes = strokeByteLength(record2);
  return record2;
}

/* ------------------------------------------------------------------------------------------- */
/* The brush                                                                                    */
/* ------------------------------------------------------------------------------------------- */

/**
 * A painting session over a set of skinned meshes.
 *
 * Lifecycle mirrors a pointer gesture:
 *
 * ```ts
 * const brush = new WeightBrush(stage.skinnedMeshes);
 * brush.beginStroke();                       // pointerdown
 * brush.dab({ point, radius, mode, bone });  // every pointermove
 * const before = brush.commitStroke();       // pointerup — push `before` onto the undo stack
 * ```
 *
 * ⚠️ Dabs OUTSIDE a stroke are applied but not recorded, which means they cannot be undone. That is
 * deliberate rather than defensive: silently opening a stroke for a stray dab would leave an
 * unbounded record accumulating with nothing to close it.
 */
export class WeightBrush {
  private readonly meshes: THREE.SkinnedMesh[];
  private readonly scratch = new Map<THREE.SkinnedMesh, MeshScratch>();
  private recording = false;

  constructor(meshes: THREE.SkinnedMesh[]) {
    this.meshes = meshes.filter((m) => m && m.isSkinnedMesh && m.skeleton);
  }

  /** Start recording pre-stroke state. Discards any uncommitted stroke. */
  beginStroke(): void {
    this.reset();
    this.recording = true;
  }

  /** Close the stroke and hand back the pre-stroke state of only the vertices it touched. */
  commitStroke(): StrokeRecord | null {
    if (!this.recording) return null;
    const meshes: MeshStrokeRecord[] = [];
    let vertices = 0;
    for (const [mesh, scratch] of this.scratch) {
      if (!scratch.vertices.length) continue;
      const skinIndex = optionalAttribute(mesh.geometry, 'skinIndex');
      const skinWeight = optionalAttribute(mesh.geometry, 'skinWeight');
      if (!skinIndex || !skinWeight) continue;
      const joints = allocLike(rawView(skinIndex).array, scratch.joints.length);
      joints.set(scratch.joints);
      const weights = allocLike(rawView(skinWeight).array, scratch.weights.length);
      weights.set(scratch.weights);
      vertices += scratch.vertices.length;
      meshes.push({
        mesh,
        vertices: Uint32Array.from(scratch.vertices),
        joints,
        weights,
        count: optionalAttribute(mesh.geometry, 'position')?.count ?? 0
      });
    }
    this.reset();
    this.recording = false;
    const record: StrokeRecord = { meshes, vertices, bytes: 0 };
    record.bytes = strokeByteLength(record);
    return record.meshes.length ? record : null;
  }

  /** Abandon the stroke, putting every touched vertex back where it started. */
  cancelStroke(): void {
    const record = this.commitStroke();
    if (record) restoreStroke(record);
  }

  /** Release the per-stroke scratch. The cached grid and topology live on the geometry, not here. */
  dispose(): void {
    this.reset();
    this.recording = false;
  }

  private reset(): void {
    // The `seen` masks are kept — one `Uint8Array(count)` per mesh, reused for every stroke — but
    // only the entries this stroke actually set are cleared, so the cost is the stroke's size and
    // not the model's.
    for (const scratch of this.scratch.values()) {
      for (const v of scratch.vertices) scratch.seen[v] = 0;
      scratch.vertices.length = 0;
      scratch.joints.length = 0;
      scratch.weights.length = 0;
    }
  }

  private scratchFor(mesh: THREE.SkinnedMesh, count: number): MeshScratch {
    let s = this.scratch.get(mesh);
    if (!s || s.seen.length !== count) {
      s = { seen: new Uint8Array(count), vertices: [], joints: [], weights: [] };
      this.scratch.set(mesh, s);
    }
    return s;
  }

  /**
   * Paint one dab.
   *
   * Every mode reduces to `new = (1 - a)·current + a·goal` with `a = strength × falloff` and both
   * vectors summing to 1, so the sum-to-one invariant holds for free and cannot drift across a
   * stroke of a thousand dabs — see [`lerpInfluences`].
   */
  dab(options: DabOptions): DabResult {
    const result: DabResult = {
      vertices: 0,
      classes: 0,
      evictions: 0,
      declined: 0,
      spared: 0,
      meshesSkipped: 0
    };
    const strength = Math.min(1, Math.max(0, options.strength ?? 0.5));
    if (!(options.radius > 0) || strength <= 0) return result;
    const profile = options.profile ?? 'smooth';

    for (const mesh of this.meshes) {
      const geometry = mesh.geometry;
      const skinIndexAttr = optionalAttribute(geometry, 'skinIndex');
      const skinWeightAttr = optionalAttribute(geometry, 'skinWeight');
      const cache = paintCache(geometry);
      if (!skinIndexAttr || !skinWeightAttr || !cache) continue;

      const joints = rawView(skinIndexAttr);
      const weights = rawView(skinWeightAttr);

      let target = -1;
      if (options.mode !== 'smooth') {
        const bone = options.bone ?? null;
        // 🔴 `skinIndex` values index THIS mesh's skeleton. A model with two skinned meshes on two
        // skeletons gives the same bone two different indices, and painting one mesh's index into
        // the other binds the vertex to whatever joint happens to sit at that position.
        target = bone ? mesh.skeleton.bones.indexOf(bone) : -1;
        if (target < 0) {
          result.meshesSkipped++;
          continue;
        }
        // ⚠️ A Uint8 `JOINTS_0` cannot hold bone 300. Writing it would truncate to 44 and bind the
        // vertex to an unrelated joint, so the mesh is refused instead.
        if (target > indexCapacity(joints.array)) {
          result.meshesSkipped++;
          continue;
        }
      }

      const brush = toLocal(mesh, options);
      const topology = cache.topology;
      const normalAttr = brush.normal ? optionalAttribute(geometry, 'normal') : undefined;

      // Falloff per welded class: the strongest of any member vertex inside the sphere.
      const touched = new Map<number, number>();
      forEachInSphere(cache.grid, brush.x, brush.y, brush.z, brush.radius, (v, d2) => {
        if (normalAttr && brush.normal) {
          _vertexNormal.set(normalAttr.getX(v), normalAttr.getY(v), normalAttr.getZ(v));
          if (_vertexNormal.dot(brush.normal) <= 0) return;
        }
        const f = falloffAt(Math.sqrt(d2) / brush.radius, profile);
        if (f <= 0) return;
        const c = topology.classOf[v];
        const previous = touched.get(c);
        if (previous === undefined || f > previous) touched.set(c, f);
      });
      if (!touched.size) continue;

      // 🔴 Smoothing reads its neighbours from a SNAPSHOT taken before anything is written. Reading
      // live would make it Gauss–Seidel — the answer would depend on the order the grid happened to
      // hand back the vertices, and the same gesture would produce different geometry twice.
      let adjacency: { start: Uint32Array; list: Uint32Array } | null = null;
      let snapshot: Map<number, Influences> | null = null;
      if (options.mode === 'smooth') {
        adjacency = adjacencyOf(geometry, topology);
        snapshot = new Map<number, Influences>();
        const sample = (c: number): void => {
          if (snapshot!.has(c)) return;
          snapshot!.set(c, readRaw(joints, weights, topology.classMembers[topology.classStart[c]]));
        };
        for (const c of touched.keys()) {
          sample(c);
          for (let i = adjacency.start[c]; i < adjacency.start[c + 1]; i++)
            sample(adjacency.list[i]);
        }
      }

      let first = Infinity;
      let last = -Infinity;
      const scratch = this.recording ? this.scratchFor(mesh, cache.count) : null;

      for (const [c, falloff] of touched) {
        const representative = topology.classMembers[topology.classStart[c]];
        const current = snapshot?.get(c) ?? readRaw(joints, weights, representative);
        const a = Math.min(1, strength * falloff);
        if (a <= 0) continue;

        let goal: Influences | null;
        if (options.mode === 'smooth') {
          goal = smoothGoal(c, adjacency!, snapshot!, current);
        } else if (options.mode === 'erase') {
          if (!current.bones.includes(target)) continue;
          goal = goalErase(current, target);
          if (!goal) {
            result.spared += classSize(topology, c);
            continue;
          }
        } else {
          const value = Math.min(1, Math.max(0, options.value ?? 1));
          // 🔴 The eviction floor. At the rim of the brush `a` is nearly zero, so painting a bone
          // that is not already on the vertex would add an influence worth less than one Uint8 step
          // — and to make room for it we would have to throw a real binding away. That trade is
          // always a loss, so the vertex is declined instead.
          const incoming = (1 - a) * weightOf(current, target) + a * value;
          if (incoming < EVICTION_FLOOR && needsEviction(current, target)) {
            result.declined += classSize(topology, c);
            continue;
          }
          goal = goalAssign(current, target, value);
        }
        if (!goal) continue;

        const blended = lerpInfluences(current, goal, a);
        const condensed = condenseInfluences(blended, options.mode === 'assign' ? target : null);
        if (!condensed.bones.length) {
          result.spared += classSize(topology, c);
          continue;
        }
        result.evictions += condensed.evicted;
        result.classes++;

        for (let m = topology.classStart[c]; m < topology.classStart[c + 1]; m++) {
          const v = topology.classMembers[m];
          if (scratch && !scratch.seen[v]) {
            scratch.seen[v] = 1;
            scratch.vertices.push(v);
            const jb = base(joints, v);
            const wb = base(weights, v);
            for (let k = 0; k < MAX_INFLUENCES; k++) {
              scratch.joints.push(joints.array[jb + k]);
              scratch.weights.push(weights.array[wb + k]);
            }
          }
          writeRaw(joints, weights, v, condensed);
          result.vertices++;
          if (v < first) first = v;
          if (v > last) last = v;
        }
      }

      // One update range per dab rather than one per vertex. `WebGLAttributes` merges ranges before
      // uploading, but a dab touching a few thousand vertices would still hand it a few thousand
      // objects to sort every pointermove.
      if (last >= 0) {
        markDirty(skinIndexAttr, first, last);
        markDirty(skinWeightAttr, first, last);
      }
    }

    return result;
  }
}

function classSize(topology: Topology, c: number): number {
  return topology.classStart[c + 1] - topology.classStart[c];
}

function weightOf(influences: Influences, bone: number): number {
  const i = influences.bones.indexOf(bone);
  return i < 0 ? 0 : influences.weights[i];
}

/**
 * The mean binding over a class and its neighbours.
 *
 * 🔴 The class ITSELF is in the average, and that is what makes the operator converge rather than
 * oscillate. Averaging only the neighbours is Jacobi iteration on a graph whose Laplacian has a
 * zero diagonal; on any near-bipartite patch — which a quad-derived triangle mesh very much is —
 * its extreme eigenvalue is negative and repeated application makes the weights flip back and forth
 * between two states instead of settling. Including the centre gives the iteration matrix a strictly
 * positive diagonal, so every eigenvalue lands in `(-1, 1]` and the sequence damps monotonically
 * toward the local mean.
 *
 * Each averaged vector sums to 1, so the mean of `n` of them does too, and the caller's lerp against
 * it keeps the invariant.
 */
function smoothGoal(
  c: number,
  adjacency: { start: Uint32Array; list: Uint32Array },
  snapshot: Map<number, Influences>,
  fallback: Influences
): Influences {
  const acc = new Map<number, number>();
  let n = 0;
  accumulate(acc, snapshot.get(c) ?? fallback, 1);
  n++;
  for (let i = adjacency.start[c]; i < adjacency.start[c + 1]; i++) {
    const neighbour = snapshot.get(adjacency.list[i]);
    if (!neighbour || !neighbour.bones.length) continue;
    accumulate(acc, neighbour, 1);
    n++;
  }
  if (n > 1) for (const [bone, weight] of acc) acc.set(bone, weight / n);
  return fromMap(acc);
}

/* ------------------------------------------------------------------------------------------- */
/* Preview helpers — for drawing the cursor, not for editing                                     */
/* ------------------------------------------------------------------------------------------- */

/**
 * The brush's falloff as a per-vertex field, one `Float32Array` per mesh in the same order and
 * length as `meshes`.
 *
 * Shaped to drop straight into `HeatOverlay.setRatios` with the threshold at 1, which draws the
 * brush footprint ON the deforming surface — the only cursor that tells the truth about what a dab
 * will reach, since a flat ring on the screen says nothing about the far side of a limb.
 */
export function brushField(
  meshes: THREE.SkinnedMesh[],
  point: THREE.Vector3,
  radius: number,
  profile: FalloffProfile = 'smooth'
): Float32Array[] {
  return meshes.map((mesh) => {
    const cache = paintCache(mesh.geometry);
    const field = new Float32Array(cache?.count ?? 0);
    if (!cache) return field;
    const brush = toLocal(mesh, { point, radius, mode: 'smooth' });
    forEachInSphere(cache.grid, brush.x, brush.y, brush.z, brush.radius, (v, d2) => {
      field[v] = falloffAt(Math.sqrt(d2) / brush.radius, profile);
    });
    return field;
  });
}

/** The vertices a dab would reach, per mesh. Ascending, no duplicates. For tests and for readouts. */
export function verticesInBrush(
  meshes: THREE.SkinnedMesh[],
  point: THREE.Vector3,
  radius: number
): Uint32Array[] {
  return meshes.map((mesh) => {
    const cache = paintCache(mesh.geometry);
    if (!cache) return new Uint32Array(0);
    const brush = toLocal(mesh, { point, radius, mode: 'smooth' });
    const hits: number[] = [];
    forEachInSphere(cache.grid, brush.x, brush.y, brush.z, brush.radius, (v) => hits.push(v));
    return Uint32Array.from(hits.sort((a, b) => a - b));
  });
}

/**
 * How many welded surface points a mesh's vertices collapse to. A ratio far below 1 means the GLB is
 * heavily split, which is exactly when unwelded painting would tear seams.
 */
export function weldedClassCount(mesh: THREE.SkinnedMesh): number {
  return paintCache(mesh.geometry)?.topology.classCount ?? 0;
}
