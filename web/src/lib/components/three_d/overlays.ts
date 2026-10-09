/*
 * The visual layer of the Pixal3D rig editor — the part that draws a repair on the character
 * instead of reporting it as a number.
 *
 * # Why this file exists
 *
 * Every tool in `RigEditor` currently answers in a list. You select a stray shell and the viewport
 * shows nothing; you read "312 tris · 0.4% of the model" and press Delete on faith. You measure the
 * skin and get a table saying 97 influences reach past 35% of the body, with no way to find out
 * WHERE. The engine's own numbers have the same shape, and the whole reason the browser tool was
 * built is that a human looking at the model is the missing check. So the canvas has to be the
 * editor, and everything here is one of the four things it has to draw:
 *
 *   - [`SurfaceOverlay`] — exactly the triangles you are about to delete, lit up on the model.
 *   - [`HoverPicker`]    — which triangle is under the cursor, cheaply enough to run on every move.
 *   - [`BrushCursor`]    — where the paint brush is and how big it is, before a dab is committed.
 *   - [`HeatOverlay`]    — the skin-weight leak, painted per vertex, with the prune threshold as a
 *                          live uniform so dragging the slider makes the specks appear and vanish.
 *   - [`BoneSpotlight`]  — the vertices SEVERAL named bones drive, each in its own tone at once,
 *                          which is how "this forearm is inside this ribcage" becomes a picture.
 *   - [`JointHandles`]   — the grab targets, coloured by whether the joint has a core name, which is
 *                          what makes naming one possible by eye.
 *
 * # The one idea the whole file rests on: shared attributes, private index
 *
 * A hero tier is ~250k vertices and ~20 MB of attributes. An overlay that COPIES those to draw a
 * subset would allocate that per hover, so nothing here copies. Every proxy mesh is handed the
 * body's own `BufferAttribute` OBJECTS via `setAttribute` and owns nothing but its index (and, for
 * the heat map, two small per-vertex channels). It is bound to the body's own `Skeleton` and
 * `bindMatrix`, so it deforms identically — including after a joint move and re-bind, because the
 * skeleton object survives `Skeleton.calculateInverses()`.
 *
 * ## 🔴 The trap that comes with it
 *
 * `BufferGeometry.dispose()` fires an event that walks `geometry.attributes` AND `geometry.index`
 * and deletes each one's GPU buffer — `WebGLAttributes` is keyed by the ATTRIBUTE OBJECT, and the
 * body holds the very same objects. Disposing a proxy geometry naively therefore frees the
 * character's position/uv/skin buffers out from under it, and what you see is not an error: it is
 * the model going blank or flickering some minutes later, in a session where nothing obviously
 * related happened. Every `dispose()` below drops the borrowed attributes (and any borrowed index)
 * from the proxy geometry FIRST. Read that as load-bearing, not tidy.
 *
 * ## 🔴 And why the proxies are siblings of the model, never children of it
 *
 * `ModelStage`'s teardown runs `root.traverse(o => o.geometry?.dispose())`, and `RigEditor` finds
 * the body by traversing for `isSkinnedMesh`. A proxy parented under the model root would be caught
 * by both: the first fires exactly the shared-attribute disposal above, and the second would offer
 * an overlay to `analyseSkin`/`patchGlb` as if it were part of the character. They are added to the
 * scene instead, and land on the body anyway — see [`syncTransform`].
 */

import * as THREE from 'three';

import { disposeBvh, raycastFace, type ComponentLabels } from './mesh-repair';
import { boneVertexMask } from './stage';

/** Under the joint handles, over the model. The skeleton helper sits at 999. */
const SURFACE_ORDER = 990;
const HEAT_ORDER = 980;
/** Above the heat map, below a delete preview: what you are about to destroy outranks a diagnosis. */
const SPOTLIGHT_ORDER = 985;
/** Over everything the brush is painting on, under the joint handles — the cursor is never the
 *  thing being aimed at. */
const BRUSH_ORDER = 995;
const HANDLE_ORDER = 1000;

export type OverlayTone = 'select' | 'danger' | 'hover';

/** UI accent blue, the danger red, and a warm white that reads against both. */
const TONES: Record<OverlayTone, number> = {
  select: 0x58c7ff,
  danger: 0xff5c5c,
  hover: 0xffe9d6
};

/**
 * Put a proxy exactly where the body is drawn, without parenting it to the body.
 *
 * ⚠️ Two matrices matter and only one of them is obvious. `bindMode` is copied so the proxy resolves
 * the skinning bracket the same way the body does; under `AttachedBindMode` — what `GLTFLoader`
 * produces — `bindMatrixInverse` cancels `matrixWorld` outright and the proxy would land correctly
 * whatever transform it had. Under `DetachedBindMode` it would not, so `matrixWorld` is copied too
 * and the automatic update is switched OFF, because a child of the scene would otherwise have its
 * world matrix recomputed to identity on the next frame.
 *
 * `SkinnedMesh.updateMatrixWorld` still runs and still re-derives `bindMatrixInverse` from whatever
 * `matrixWorld` holds, which is why hand-setting it is safe rather than merely lucky.
 */
function syncTransform(proxy: THREE.SkinnedMesh, body: THREE.SkinnedMesh): void {
  proxy.bindMode = body.bindMode;
  proxy.matrixAutoUpdate = false;
  proxy.matrixWorldAutoUpdate = false;
  proxy.matrixWorld.copy(body.matrixWorld);
}

/** Hand a proxy geometry the body's attribute objects. Returns the names it now borrows. */
function borrowAttributes(proxy: THREE.BufferGeometry, body: THREE.BufferGeometry): string[] {
  const borrowed: string[] = [];
  for (const name of Object.keys(body.attributes)) {
    proxy.setAttribute(name, body.attributes[name]);
    borrowed.push(name);
  }
  return borrowed;
}

/* ------------------------------------------------------------------------------------------- */
/* SurfaceOverlay                                                                               */
/* ------------------------------------------------------------------------------------------- */

/**
 * "Show me exactly what I am about to delete."
 *
 * A second `SkinnedMesh` over the body's own vertices, indexed down to a chosen set of triangles and
 * drawn on top of the model in a tone. It answers the question the stray-piece list cannot: a
 * component of 312 triangles at 0.4% of the model is either a floating speck of junk or the tip of a
 * finger, and the only way to tell them apart is to look.
 *
 * Semi-transparent on purpose. At full opacity the highlight replaces the shape underneath and you
 * lose the very thing you were trying to identify.
 */
export class SurfaceOverlay {
  private readonly body: THREE.SkinnedMesh;
  private readonly scene: THREE.Object3D;
  private readonly geometry = new THREE.BufferGeometry();
  private readonly material: THREE.MeshBasicMaterial;
  private readonly mesh: THREE.SkinnedMesh;
  /** Attribute names taken from the body. 🔴 Dropped before `geometry.dispose()`. */
  private borrowed: string[] = [];
  /** OUR index — the one thing this geometry actually owns. Grown, never replaced per show. */
  private index: THREE.BufferAttribute | null = null;
  private indexArray = new Uint32Array(0);

  constructor(body: THREE.SkinnedMesh, scene: THREE.Object3D) {
    this.body = body;
    this.scene = scene;
    this.borrowed = borrowAttributes(this.geometry, body.geometry);

    this.material = new THREE.MeshBasicMaterial({
      color: TONES.select,
      transparent: true,
      opacity: 0.5,
      // Depth test off so a highlight inside the body — an interior shell, a piece behind an arm —
      // is still visible. That is most of what this tool is pointed at.
      depthTest: false,
      depthWrite: false,
      // DoubleSide because ~200 independently generated shells have no trustworthy winding, and a
      // highlight that vanishes on inverted geometry is a highlight that fails on the junk.
      side: THREE.DoubleSide
    });

    this.mesh = new THREE.SkinnedMesh(this.geometry, this.material);
    this.mesh.bind(body.skeleton, body.bindMatrix);
    this.mesh.renderOrder = SURFACE_ORDER;
    // The proxy's bounds are the body's, and the body is one character — culling would only buy a
    // per-frame bounding-sphere computation over 250k vertices we already know is on screen.
    this.mesh.frustumCulled = false;
    this.mesh.visible = false;
    syncTransform(this.mesh, body);
    scene.add(this.mesh);
  }

  /** Highlight these triangles (face indices into the body's CURRENT index buffer). */
  showFaces(faces: ReadonlySet<number> | ArrayLike<number>, tone: OverlayTone): void {
    const list: number[] = [];
    if (isFaceSet(faces)) faces.forEach((f) => list.push(f));
    else for (let i = 0; i < faces.length; i++) list.push(faces[i]);
    this.draw(list, tone);
  }

  /** Highlight one labelled connected component. */
  showComponent(labels: ComponentLabels, id: number, tone: OverlayTone): void {
    const index = this.body.geometry.getIndex();
    // 🔴 Labels are indexed by faceIndex and are invalidated by any change to the index buffer. A
    // stale array is merely LONGER, so nothing downstream would bounds-check its way to noticing —
    // it would just light up, and then delete, a different part of the character. Same guard
    // `removeComponents` throws on; here it silently draws nothing, because a hover is not the place
    // to raise an exception.
    if (!index || labels.faceComponent.length !== index.count / 3) {
      this.hide();
      return;
    }
    const faces: number[] = [];
    for (let f = 0; f < labels.faceComponent.length; f++) {
      if (labels.faceComponent[f] === id) faces.push(f);
    }
    this.draw(faces, tone);
  }

  hide(): void {
    this.mesh.visible = false;
  }

  /**
   * The body's index buffer changed — drop the subset, it addresses a numbering that is gone.
   *
   * Also re-takes the attributes: `compactVertices` replaces every one of them, and a proxy still
   * holding the old objects would keep drawing the pre-compaction mesh with no sign anything was
   * wrong.
   */
  invalidate(): void {
    this.hide();
    this.geometry.setDrawRange(0, 0);
    for (const name of this.borrowed) this.geometry.deleteAttribute(name);
    this.borrowed = borrowAttributes(this.geometry, this.body.geometry);
  }

  dispose(): void {
    this.scene.remove(this.mesh);
    // 🔴 Before `dispose()`, always. These attribute objects belong to the character; letting the
    // geometry's dispose event reach them frees the body's GPU buffers. See the file header.
    for (const name of this.borrowed) this.geometry.deleteAttribute(name);
    this.borrowed = [];
    // The index IS ours, so it stays and is freed with the geometry.
    this.geometry.dispose();
    this.material.dispose();
  }

  /**
   * Rewrite the index in place and point the draw range at the part that is live.
   *
   * 🔴 In place, not a fresh attribute per call, and this is not a micro-optimisation. `three` frees
   * an attribute's GPU buffer only from the owning geometry's dispose event — `WebGLAttributes`
   * holds them in a `WeakMap` and a `WeakMap` losing its key does NOT delete the GL buffer. So
   * `setIndex(new BufferAttribute(...))` on every pointermove leaks one buffer per mouse movement.
   * The buffer here is grown geometrically and replaced only when a selection outgrows it, which
   * happens a handful of times per session at worst.
   */
  private draw(faces: number[], tone: OverlayTone): void {
    const source = this.body.geometry.getIndex();
    if (!source) {
      this.hide();
      return;
    }
    const src = source.array as ArrayLike<number>;
    const triangles = source.count / 3;

    let needed = 0;
    for (let k = 0; k < faces.length; k++) {
      const f = faces[k];
      if (f >= 0 && f < triangles) needed++;
    }
    if (needed === 0) {
      this.hide();
      return;
    }

    let attribute = this.index;
    if (attribute === null || this.indexArray.length < needed * 3) {
      const grown = Math.min(source.count, Math.max(this.indexArray.length * 2, 1536));
      // Always Uint32: this index is never saved, so the Uint16 file-size rule that governs
      // `mesh-repair`'s `allocIndex` buys nothing here, and WebGL2 takes 32-bit indices anyway.
      this.indexArray = new Uint32Array(Math.max(needed * 3, grown));
      attribute = new THREE.BufferAttribute(this.indexArray, 1);
      attribute.setUsage(THREE.DynamicDrawUsage);
      this.index = attribute;
      this.geometry.setIndex(attribute);
    }

    let n = 0;
    for (let k = 0; k < faces.length; k++) {
      const f = faces[k];
      if (f < 0 || f >= triangles) continue;
      this.indexArray[n++] = src[f * 3];
      this.indexArray[n++] = src[f * 3 + 1];
      this.indexArray[n++] = src[f * 3 + 2];
    }
    // Upload only what was written. Note r185's plural `updateRanges` + `addUpdateRange()`; the
    // singular `updateRange` object is gone.
    attribute.clearUpdateRanges();
    attribute.addUpdateRange(0, n);
    attribute.needsUpdate = true;
    // The buffer stays oversized between shows; the draw range is what keeps the stale tail of it
    // from being drawn as garbage triangles.
    this.geometry.setDrawRange(0, n);

    this.material.color.setHex(TONES[tone]);
    this.mesh.matrixWorld.copy(this.body.matrixWorld);
    this.mesh.visible = true;
  }
}

/** `ReadonlySet` has no `length`, so the union is narrowed on the method that only a set has. */
function isFaceSet(faces: ReadonlySet<number> | ArrayLike<number>): faces is ReadonlySet<number> {
  return typeof (faces as ReadonlySet<number>).has === 'function';
}

/* ------------------------------------------------------------------------------------------- */
/* HoverPicker                                                                                  */
/* ------------------------------------------------------------------------------------------- */

/**
 * Which triangle is under the cursor, cheaply enough to ask on every pointer move.
 *
 * A thin wrapper over [`raycastFace`], and the only reason it is a class is the cache discipline:
 * the BVH it rides on is memoised against the geometry and shared with `interiorFaces`, so
 * something has to own "the index changed, throw it away". Getting that wrong reports hits on
 * triangles that no longer exist, which is how a delete lands on the wrong part of the character.
 *
 * ⚠️ The reason this is worth having at all: `Raycaster.intersectObjects` on a `SkinnedMesh` runs
 * `getVertexPosition` — a weighted sum of up to four bone matrices — per corner across ~440k
 * triangles. `ModelStage` deliberately shipped with no hover highlight for exactly that reason. The
 * BVH is what makes hover affordable.
 *
 * ⚠️ REST POSE ONLY, inherited from [`raycastFace`]: the tree is built over the bind-space position
 * attribute, so under a playing clip it answers about a model that is not the one on screen. Guard
 * on "this GLB carries no animation clips" — the same condition joint editing already requires.
 */
export class HoverPicker {
  private readonly body: THREE.SkinnedMesh;

  constructor(body: THREE.SkinnedMesh) {
    this.body = body;
  }

  /** faceIndex or -1. Safe to call on every pointermove. */
  pick(raycaster: THREE.Raycaster): number {
    return raycastFace(this.body, raycaster);
  }

  /** Call after ANY change to the body's index or positions. The next pick rebuilds the tree. */
  invalidate(): void {
    disposeBvh(this.body.geometry);
  }

  dispose(): void {
    disposeBvh(this.body.geometry);
  }
}

/* ------------------------------------------------------------------------------------------- */
/* BrushCursor                                                                                  */
/* ------------------------------------------------------------------------------------------- */

/** Ring thickness as a fraction of the radius. Thin enough to read as an outline at any size. */
const BRUSH_RIM = 0.06;
/** Segments round the ring. 64 is smooth at a brush that fills the viewport and costs nothing. */
const BRUSH_SEGMENTS = 64;

/**
 * "How big is the brush, and where is it about to land?"
 *
 * A camera-facing ring of the brush's WORLD radius, drawn at the hit point with the depth test off.
 *
 * ## 🔴 Deliberately NOT a surface-conforming decal, and not because that was too hard
 *
 * The honest footprint of a dab — which vertices it reaches, at what falloff, wrapping round the far
 * side of a limb — is already solved and already drawn: `weight-paint`'s `brushField()` returns
 * per-mesh `Float32Array`s shaped exactly for [`HeatOverlay.setRatios`], so the workbench pushes it
 * through the existing `heat` prop and the footprint appears ON the deforming surface, through the
 * same skinned proxies as everything else here. A decal drawn by this class would be a SECOND
 * answer to that question, computed differently, and the two would disagree at every seam and every
 * shell boundary — with the prettier one being the wrong one.
 *
 * So this ring claims only what it can prove: a centre and a size. It says "the brush is here and it
 * is this wide"; the heat field says what that actually touches. Two claims, one owner each.
 *
 * ⚠️ Depth test off, like every other overlay in this file. A brush aimed at the inside of a thigh
 * or the far side of an arm still needs a visible cursor, and half of weight painting is exactly
 * those places.
 */
export class BrushCursor {
  private readonly scene: THREE.Object3D;
  private readonly geometry: THREE.RingGeometry;
  private readonly material: THREE.MeshBasicMaterial;
  private readonly mesh: THREE.Mesh;

  constructor(scene: THREE.Object3D) {
    this.scene = scene;
    // A UNIT ring, scaled per `set`. Rebuilding a `RingGeometry` per pointer move would allocate and
    // upload a new vertex buffer sixty times a second to draw the same circle at a different size —
    // the same argument `SurfaceOverlay.draw` makes about its index.
    this.geometry = new THREE.RingGeometry(1 - BRUSH_RIM, 1, BRUSH_SEGMENTS);
    this.material = new THREE.MeshBasicMaterial({
      color: TONES.hover,
      transparent: true,
      opacity: 0.85,
      depthTest: false,
      depthWrite: false,
      // The ring is flat and faces the camera, so it is edge-on to nothing — but a camera that has
      // orbited past it would otherwise cull it for one frame at the crossing.
      side: THREE.DoubleSide
    });
    this.mesh = new THREE.Mesh(this.geometry, this.material);
    this.mesh.renderOrder = BRUSH_ORDER;
    // Its bounding sphere is a disc at a point the user is looking at by construction.
    this.mesh.frustumCulled = false;
    this.mesh.visible = false;
    // 🔴 A SCENE sibling, never a child of the model — see the file header. This one borrows no
    // attributes, so the disposal trap does not apply, but the save path's `isSkinnedMesh`
    // traversal and the stage's `root.traverse(dispose)` both still do.
    scene.add(this.mesh);
  }

  /**
   * Put the ring at a world point, sized to a world radius.
   *
   * Does NOT orient it — [`update`] does that, per frame, because the camera can orbit while the
   * pointer sits still and a ring that only re-aims on pointer movement turns into a line.
   */
  set(point: THREE.Vector3, radius: number): void {
    this.mesh.position.copy(point);
    // A zero scale collapses the ring to a point that never comes back; the floor keeps a
    // mis-configured radius merely invisible rather than permanently broken.
    this.mesh.scale.setScalar(radius > 0 ? radius : 1e-4);
    this.mesh.visible = true;
  }

  /**
   * Face the camera. Call once per frame while visible.
   *
   * ⚠️ `quaternion.copy(camera.quaternion)` rather than `lookAt`: the camera's own rotation IS the
   * billboard orientation, and `lookAt` would re-derive it from a direction and pick its own roll.
   *
   * This object keeps `matrixAutoUpdate` on — unlike the skinned proxies above, which hand-set
   * `matrixWorld` and must switch it off — so the renderer folds these writes into the matrix it
   * already computes for every scene child.
   */
  update(camera: THREE.Camera): void {
    if (!this.mesh.visible) return;
    this.mesh.quaternion.copy(camera.quaternion);
  }

  /** `hover` while the brush is merely aimed, `select` while a stroke is live. */
  setTone(tone: OverlayTone): void {
    this.material.color.setHex(TONES[tone]);
  }

  hide(): void {
    this.mesh.visible = false;
  }

  get visible(): boolean {
    return this.mesh.visible;
  }

  dispose(): void {
    this.scene.remove(this.mesh);
    this.geometry.dispose();
    this.material.dispose();
  }
}

/* ------------------------------------------------------------------------------------------- */
/* HeatOverlay                                                                                  */
/* ------------------------------------------------------------------------------------------- */

/** One proxy per skinned mesh. Not exported: its lifetime is entirely [`HeatOverlay`]'s. */
class HeatProxy {
  readonly body: THREE.SkinnedMesh;
  readonly geometry = new THREE.BufferGeometry();
  readonly mesh: THREE.SkinnedMesh;
  borrowed: string[] = [];
  /** 🔴 Held as raw arrays as well as attributes — see the note in the constructor. */
  ratioArray: Float32Array;
  maskArray: Float32Array;
  ratio: THREE.BufferAttribute;
  mask: THREE.BufferAttribute;

  constructor(body: THREE.SkinnedMesh, material: THREE.Material) {
    this.body = body;
    this.borrowed = borrowAttributes(this.geometry, body.geometry);
    this.geometry.setIndex(body.geometry.getIndex());

    const count = body.geometry.attributes.position?.count ?? 0;
    this.ratioArray = new Float32Array(count);
    this.maskArray = new Float32Array(count).fill(1);
    // 🔴 `BufferAttribute`, NOT `Float32BufferAttribute`. The latter's constructor is
    // `super( new Float32Array( array ), … )` — it COPIES — so the array we then write into on
    // `setRatios` would not be the one the GPU ever sees, and the heat map would stay uniformly
    // cold with nothing to show for it. Verified in the r185 source, 2026-08-17.
    this.ratio = new THREE.BufferAttribute(this.ratioArray, 1);
    this.mask = new THREE.BufferAttribute(this.maskArray, 1);
    this.geometry.setAttribute('aRatio', this.ratio);
    this.geometry.setAttribute('aMask', this.mask);

    this.mesh = new THREE.SkinnedMesh(this.geometry, material);
    this.mesh.bind(body.skeleton, body.bindMatrix);
    this.mesh.renderOrder = HEAT_ORDER;
    this.mesh.frustumCulled = false;
    this.mesh.visible = false;
    syncTransform(this.mesh, body);
  }

  /** Re-take the body's attributes and index, and resize the owned channels to match. */
  resync(): void {
    for (const name of this.borrowed) this.geometry.deleteAttribute(name);
    this.borrowed = borrowAttributes(this.geometry, this.body.geometry);
    this.geometry.setIndex(this.body.geometry.getIndex());

    const count = this.body.geometry.attributes.position?.count ?? 0;
    if (count === this.ratioArray.length) return;
    this.ratioArray = new Float32Array(count);
    this.maskArray = new Float32Array(count).fill(1);
    this.ratio = new THREE.BufferAttribute(this.ratioArray, 1);
    this.mask = new THREE.BufferAttribute(this.maskArray, 1);
    this.geometry.setAttribute('aRatio', this.ratio);
    this.geometry.setAttribute('aMask', this.mask);
  }

  dispose(): void {
    for (const name of this.borrowed) this.geometry.deleteAttribute(name);
    this.borrowed = [];
    // 🔴 The INDEX is borrowed too, and `dispose` frees `geometry.index` alongside the attributes.
    // Miss this one and the character's triangles stop drawing while every attribute survives —
    // which looks like the mesh being deleted rather than a buffer being freed.
    this.geometry.setIndex(null);
    // aRatio/aMask are ours and are freed here, which is the point of the call.
    this.geometry.dispose();
  }
}

const HEAT_VERTEX_PARS = `#include <common>
attribute float aRatio;
attribute float aMask;
varying float vRatio;
varying float vMask;`;

const HEAT_VERTEX_MAIN = `vRatio = aRatio;
vMask = aMask;
#include <begin_vertex>`;

const HEAT_FRAGMENT_PARS = `#include <common>
uniform float uThreshold;
varying float vRatio;
varying float vMask;`;

/**
 * The ramp. Below the threshold it stays cool and nearly transparent so the character reads through
 * it; at or above, it burns. The whole point of the tool is that ~97 vertices out of 247,727 are the
 * defect, so the ramp is tuned to make a few dozen specks obvious rather than to look like a
 * thermal image.
 */
const HEAT_FRAGMENT_MAIN = `float heat = vRatio / max( uThreshold, 0.000001 );
float ramp = clamp( heat, 0.0, 1.0 );
vec3 cool = vec3( 0.12, 0.42, 0.62 );
vec3 warm = vec3( 0.98, 0.78, 0.25 );
vec3 hot = vec3( 1.00, 0.24, 0.24 );
vec3 tint = heat >= 1.0 ? hot : mix( cool, warm, ramp );
float burn = heat >= 1.0 ? 0.92 : mix( 0.10, 0.55, ramp );
vec4 diffuseColor = vec4( tint, burn * clamp( vMask, 0.0, 1.0 ) );`;

/**
 * Paint the skin-weight leak on the model.
 *
 * The defect this exists for is ~97 vertices out of 247,727 bound to a joint half a body away.
 * Today that is a row in a table and a bone name, and finding the vertices means guessing. After
 * this it is red specks on an arm: drag the threshold and watch them appear and vanish, which is
 * also the honest check on the threshold itself — if pruning at your setting lights up the whole
 * torso, the setting is wrong, not the rig.
 *
 * 🔴 The threshold is a UNIFORM, never a geometry rebuild. It is on a slider, so it changes tens of
 * times a second; re-uploading a 250k-vertex channel per drag event would make the control unusable
 * and would teach the user that the tool is slow rather than that the rig is fine.
 *
 * ⚠️ Drawn with the depth test OFF, like the other overlays. The specks are frequently on the far
 * side of the character and a heat map you have to orbit to find defeats the purpose; the low alpha
 * on the cool end is what keeps the see-through body readable.
 */
export class HeatOverlay {
  private readonly scene: THREE.Object3D;
  private readonly proxies: HeatProxy[] = [];
  private readonly material: THREE.MeshBasicMaterial;
  /** The live uniform object, shared into the compiled shader. */
  private readonly threshold = { value: 0.35 };
  private shown = false;

  constructor(meshes: THREE.SkinnedMesh[], scene: THREE.Object3D) {
    this.scene = scene;
    this.material = new THREE.MeshBasicMaterial({
      transparent: true,
      depthTest: false,
      depthWrite: false,
      side: THREE.DoubleSide
    });

    // `onBeforeCompile` over the stock meshbasic shader rather than a hand-written `ShaderMaterial`:
    // meshbasic already carries the skinning chunks and the renderer already feeds it `boneTexture`
    // / `bindMatrix` / `bindMatrixInverse`, so this inherits the deformation instead of
    // reimplementing it. `attribute` and `varying` are the right keywords even under WebGL2 —
    // `WebGLProgram` prepends `#define attribute in` / `#define varying out` for every non-raw
    // shader.
    //
    // ⚠️ These are string replacements against three's own chunk text. If a release renames the
    // `vec4 diffuseColor = …` line the injection silently no-ops and the overlay renders flat, with
    // no error. Checked against r185.
    this.material.onBeforeCompile = (shader) => {
      shader.uniforms.uThreshold = this.threshold;
      shader.vertexShader = shader.vertexShader
        .replace('#include <common>', HEAT_VERTEX_PARS)
        .replace('#include <begin_vertex>', HEAT_VERTEX_MAIN);
      shader.fragmentShader = shader.fragmentShader
        .replace('#include <common>', HEAT_FRAGMENT_PARS)
        .replace('vec4 diffuseColor = vec4( diffuse, opacity );', HEAT_FRAGMENT_MAIN);
    };

    for (const mesh of meshes) {
      const proxy = new HeatProxy(mesh, this.material);
      this.proxies.push(proxy);
      scene.add(proxy.mesh);
    }
  }

  /**
   * Per-vertex worst influence ratio, straight from `SkinReport.vertexRatios`.
   *
   * ⚠️ A length mismatch means the measurement was taken against a different geometry — the only
   * thing that renumbers vertices is `compactVertices`. The ratios are then meaningless per vertex,
   * so that mesh is left cold rather than painted with someone else's numbers.
   */
  setRatios(ratios: Float32Array[]): void {
    this.proxies.forEach((proxy, i) => {
      // Any re-measure follows whatever edit prompted it, so this is the natural place to re-take
      // attributes that `compactVertices` may have replaced wholesale.
      proxy.resync();
      const source = ratios[i];
      proxy.ratioArray.fill(0);
      if (source && source.length === proxy.ratioArray.length) proxy.ratioArray.set(source);
      proxy.ratio.needsUpdate = true;
    });
  }

  /** Vertices at or above this ratio burn red; below ramps cool. Matches the prune threshold. */
  setThreshold(ratio: number): void {
    this.threshold.value = Math.max(ratio, 1e-6);
  }

  /**
   * Restrict the paint to a subset — e.g. one bone's vertices from `boneVertexMask`. `null` = all.
   *
   * A mask of the wrong length paints NOTHING rather than everything: an unexplained blank overlay
   * sends you back to the mask, where the bug is, and a full-body paint would look like a correct
   * answer to a different question.
   */
  setMask(mask: Uint8Array[] | null): void {
    for (let i = 0; i < this.proxies.length; i++) {
      const proxy = this.proxies[i];
      const source = mask ? mask[i] : null;
      if (!mask) {
        proxy.maskArray.fill(1);
      } else if (!source || source.length !== proxy.maskArray.length) {
        proxy.maskArray.fill(0);
      } else {
        for (let v = 0; v < source.length; v++) proxy.maskArray[v] = source[v] === 0 ? 0 : 1;
      }
      proxy.mask.needsUpdate = true;
    }
  }

  get visible(): boolean {
    return this.shown;
  }

  set visible(on: boolean) {
    this.shown = on;
    for (const proxy of this.proxies) {
      proxy.mesh.visible = on;
      if (on) proxy.mesh.matrixWorld.copy(proxy.body.matrixWorld);
    }
  }

  dispose(): void {
    for (const proxy of this.proxies) {
      this.scene.remove(proxy.mesh);
      proxy.dispose();
    }
    this.proxies.length = 0;
    this.material.dispose();
  }
}

/* ------------------------------------------------------------------------------------------- */
/* BoneSpotlight                                                                                */
/* ------------------------------------------------------------------------------------------- */

/** One bone lit in one tone. Later entries in a `set` call win where two bones share a vertex. */
export interface SpotlitBone {
  boneIndex: number;
  tone: OverlayTone;
}

/**
 * 🔴 The per-vertex channel carries a SLOT, not a colour. Three floats per vertex over 250k
 * vertices would be 3 MB re-uploaded per selection change, and the tones are three fixed values —
 * so the vertex says "which tone" and the shader owns "what colour". 0 means unlit and is
 * `discard`ed, which is why the slots start at 1.
 */
const TONE_SLOT: Record<OverlayTone, number> = {
  select: 1,
  danger: 2,
  hover: 3
};

/** One proxy per skinned mesh. Not exported: its lifetime is entirely [`BoneSpotlight`]'s. */
class SpotlightProxy {
  readonly body: THREE.SkinnedMesh;
  readonly geometry = new THREE.BufferGeometry();
  readonly mesh: THREE.SkinnedMesh;
  borrowed: string[] = [];
  /** 🔴 Raw array as well as attribute — same reason as [`HeatProxy`]: it is written in place. */
  toneArray: Float32Array;
  tone: THREE.BufferAttribute;

  constructor(body: THREE.SkinnedMesh, material: THREE.Material) {
    this.body = body;
    this.borrowed = borrowAttributes(this.geometry, body.geometry);
    this.geometry.setIndex(body.geometry.getIndex());

    const count = body.geometry.attributes.position?.count ?? 0;
    // 🔴 `BufferAttribute`, never `Float32BufferAttribute`. The latter COPIES its array, so every
    // write below would land somewhere the GPU never reads. See the same note in `HeatProxy`.
    this.toneArray = new Float32Array(count);
    this.tone = new THREE.BufferAttribute(this.toneArray, 1);
    this.geometry.setAttribute('aTone', this.tone);

    this.mesh = new THREE.SkinnedMesh(this.geometry, material);
    this.mesh.bind(body.skeleton, body.bindMatrix);
    this.mesh.renderOrder = SPOTLIGHT_ORDER;
    this.mesh.frustumCulled = false;
    this.mesh.visible = false;
    syncTransform(this.mesh, body);
  }

  /** Re-take the body's attributes and index, and resize the owned channel to match. */
  resync(): void {
    for (const name of this.borrowed) this.geometry.deleteAttribute(name);
    this.borrowed = borrowAttributes(this.geometry, this.body.geometry);
    this.geometry.setIndex(this.body.geometry.getIndex());

    const count = this.body.geometry.attributes.position?.count ?? 0;
    if (count === this.toneArray.length) return;
    this.toneArray = new Float32Array(count);
    this.tone = new THREE.BufferAttribute(this.toneArray, 1);
    this.geometry.setAttribute('aTone', this.tone);
  }

  dispose(): void {
    for (const name of this.borrowed) this.geometry.deleteAttribute(name);
    this.borrowed = [];
    // 🔴 The INDEX is borrowed too. Miss this and the character's triangles stop drawing while
    // every attribute survives — see the same line in `HeatProxy.dispose`.
    this.geometry.setIndex(null);
    this.geometry.dispose();
  }
}

/**
 * 🔴 A hand-written `ShaderMaterial` here, where `HeatOverlay` patches meshbasic instead.
 *
 * The heat map needed the stock shader's chunks because it tints; this one only ever emits a flat
 * tone and `discard`s everything else, so patching `vec4 diffuseColor = …` would leave a whole lit
 * pipeline running to produce a constant. What it MUST keep is the skinning, and that comes free:
 * `WebGLPrograms` sets `skinning: object.isSkinnedMesh === true` — off the OBJECT, not the material
 * — so `USE_SKINNING` is defined, `skinIndex`/`skinWeight` are declared by the program prefix, and
 * `WebGLRenderer.setProgram` uploads `bindMatrix`/`bindMatrixInverse`/`boneTexture` for any skinned
 * mesh whatever material it wears. Checked against the r185 sources, 2026-08-17.
 *
 * ⚠️ `attribute`/`varying`/`gl_FragColor` are the right keywords even though this compiles to GLSL
 * ES 3.00: three prepends the `#define attribute in` block to every non-RAW ShaderMaterial.
 */
const SPOTLIGHT_VERTEX = `#include <common>
#include <skinning_pars_vertex>
attribute float aTone;
varying float vTone;
void main() {
	vTone = aTone;
	#include <skinbase_vertex>
	#include <begin_vertex>
	#include <skinning_vertex>
	#include <project_vertex>
}`;

/**
 * ⚠️ `if/if` rather than an array lookup. Indexing a uniform array by a varying is legal here but
 * was not under GLSL ES 1.00, and three switches shader language on the renderer's backend — three
 * comparisons cost nothing and cannot become a compile error on a future build.
 *
 * `colorspace_fragment` is not decoration: the tones are `THREE.Color`s, which means they are in the
 * renderer's WORKING space, and skipping the encode would draw them visibly darker than the same
 * hexes on every other overlay.
 */
const SPOTLIGHT_FRAGMENT = `uniform vec3 uSelect;
uniform vec3 uDanger;
uniform vec3 uHover;
uniform float uOpacity;
varying float vTone;
void main() {
	if ( vTone < 0.5 ) discard;
	vec3 tint = uSelect;
	if ( vTone > 1.5 ) tint = uDanger;
	if ( vTone > 2.5 ) tint = uHover;
	gl_FragColor = vec4( tint, uOpacity );
	#include <colorspace_fragment>
}`;

/**
 * Light up the vertices SPECIFIC bones drive, each bone in its own tone, all at once.
 *
 * [`HeatOverlay.setMask`] can already restrict its paint to one bone, and that is the right tool for
 * "does this joint own the limb its name claims". It cannot answer the motion-repair question, which
 * is about a PAIR: the detector reports "left forearm intersects the ribcage on frames 41-58", and
 * the only useful picture puts the forearm in danger red and the ribcage in select blue on the same
 * model at the same time. One mask and one colour cannot say that.
 *
 * ⚠️ Drawn with the depth test off, like every other overlay here. Half the point is showing a limb
 * that is INSIDE the body, which is exactly the case a depth test would hide.
 *
 * ⚠️ `set` is O(bones × vertices) — it runs [`boneVertexMask`] once per bone. That is a few hundred
 * thousand comparisons per entry, so it belongs on a selection change and not in a render loop or
 * on a slider's `input`.
 */
export class BoneSpotlight {
  private readonly meshes: THREE.SkinnedMesh[];
  private readonly scene: THREE.Object3D;
  private readonly proxies: SpotlightProxy[] = [];
  private readonly material: THREE.ShaderMaterial;
  private shown = false;

  constructor(meshes: THREE.SkinnedMesh[], scene: THREE.Object3D) {
    this.meshes = meshes;
    this.scene = scene;
    this.material = new THREE.ShaderMaterial({
      uniforms: {
        // Built from `TONES` rather than repeated, so the spotlight and the surface overlay cannot
        // drift apart into two slightly different blues.
        uSelect: { value: new THREE.Color().setHex(TONES.select) },
        uDanger: { value: new THREE.Color().setHex(TONES.danger) },
        uHover: { value: new THREE.Color().setHex(TONES.hover) },
        // Semi-transparent for the same reason `SurfaceOverlay` is: at full opacity the highlight
        // replaces the shape underneath and you lose the thing you were trying to identify.
        uOpacity: { value: 0.55 }
      },
      vertexShader: SPOTLIGHT_VERTEX,
      fragmentShader: SPOTLIGHT_FRAGMENT,
      transparent: true,
      depthTest: false,
      depthWrite: false,
      side: THREE.DoubleSide
    });

    for (const mesh of meshes) {
      const proxy = new SpotlightProxy(mesh, this.material);
      this.proxies.push(proxy);
      // 🔴 Added to the SCENE, never under the model root. See the file header.
      scene.add(proxy.mesh);
    }
  }

  /**
   * Light these bones. `null` or `[]` clears; later entries win where bones overlap.
   *
   * `minWeight` is [`boneVertexMask`]'s readability threshold, not a correctness one — every vertex
   * near a joint carries a trace of its neighbours, so lighting everything above zero paints half a
   * torso for a wrist.
   */
  set(bones: SpotlitBone[] | null, minWeight?: number): void {
    for (const proxy of this.proxies) {
      // Any change to the lit set follows whatever edit prompted it, so this is the natural place to
      // re-take attributes that `compactVertices` may have replaced wholesale.
      proxy.resync();
      proxy.toneArray.fill(0);
    }
    for (const entry of bones ?? []) {
      const slot = TONE_SLOT[entry.tone];
      const masks = boneVertexMask(this.meshes, entry.boneIndex, minWeight);
      this.proxies.forEach((proxy, i) => {
        const mask = masks[i];
        // A length mismatch means the mask was measured against a vertex numbering that is gone.
        // Leaving that mesh dark is the honest answer; painting it would be someone else's numbers.
        if (!mask || mask.length !== proxy.toneArray.length) return;
        for (let v = 0; v < mask.length; v++) {
          if (mask[v] !== 0) proxy.toneArray[v] = slot;
        }
      });
    }
    for (const proxy of this.proxies) proxy.tone.needsUpdate = true;
  }

  get visible(): boolean {
    return this.shown;
  }

  set visible(on: boolean) {
    this.shown = on;
    for (const proxy of this.proxies) {
      proxy.mesh.visible = on;
      if (on) proxy.mesh.matrixWorld.copy(proxy.body.matrixWorld);
    }
  }

  dispose(): void {
    for (const proxy of this.proxies) {
      this.scene.remove(proxy.mesh);
      proxy.dispose();
    }
    this.proxies.length = 0;
    this.material.dispose();
  }
}

/* ------------------------------------------------------------------------------------------- */
/* JointHandles                                                                                 */
/* ------------------------------------------------------------------------------------------- */

export type JointState = 'core' | 'named' | 'unnamed' | 'dissolved';

/**
 * 🔴 Visibly different colours, because one colour is why naming a joint is currently impossible to
 * do by eye. Every handle used to be the same blue, so the two joints a rig is missing out of the 22
 * core slots looked exactly like the twenty that were fine — and the whole task is "find the unnamed
 * joint that ought to be the left shoulder and assign it". Green reads as done, blue as
 * named-but-not-core, violet as free to claim.
 *
 * `dissolved` is the fourth and reads as gone-but-still-here: the joint drives nothing any more and
 * the save will delete it, but it is still in the live skeleton because every `skinIndex` in the
 * mesh is an index INTO that list. Grey rather than absent, so a staged deletion is something you
 * can see and undo rather than a handle that silently vanished.
 */
const JOINT_COLOURS: Record<JointState, number> = {
  core: 0x5ce49a,
  named: 0x58c7ff,
  unnamed: 0xc27bff,
  dissolved: 0x6b7280
};
const JOINT_SELECTED = 0xffc857;
/** Selected AND hovered: a lighter amber, so hovering the current joint still reads as a change. */
const JOINT_SELECTED_HOVER = 0xffe6ab;
const JOINT_HOVER = 0xffffff;

/**
 * The grab handles for every joint, lifted out of `ModelStage`.
 *
 * ⚠️ Every property of the original is deliberate and kept:
 *
 *   - An `InstancedMesh`, so sixty grab targets are one draw call and one raycast, and `instanceId`
 *     **is** the bone index — there is no lookup table to keep in step and therefore none to get out
 *     of step.
 *   - `depthTest: false` and drawn last, because a joint inside the mesh must still be grabbable,
 *     and most of them are inside the mesh.
 *   - Translation-only instance matrices. A bone's world scale would squash the grab target and its
 *     rotation means nothing to a sphere, so both are dropped on purpose.
 *
 * What is new is colour by [`JointState`] and a size bump on the selected and hovered handles, so
 * which joint is which is answerable at a glance instead of by reading the panel.
 */
export class JointHandles {
  private readonly bones: THREE.Bone[];
  private readonly scene: THREE.Object3D;
  private readonly geometry: THREE.SphereGeometry;
  private readonly material: THREE.MeshBasicMaterial;
  private readonly mesh: THREE.InstancedMesh;
  private states: JointState[];
  private selected = -1;
  private hovered = -1;
  private readonly matrix = new THREE.Matrix4();
  private readonly grow = new THREE.Vector3();
  private readonly colour = new THREE.Color();

  constructor(bones: THREE.Bone[], span: number, scene: THREE.Object3D) {
    this.bones = bones;
    this.scene = scene;
    this.states = bones.map((): JointState => 'unnamed');
    this.geometry = new THREE.SphereGeometry(span * 0.011, 10, 8);
    this.material = new THREE.MeshBasicMaterial({
      depthTest: false,
      transparent: true,
      opacity: 0.9
    });
    // ⚠️ Capacity is `max(1, …)` because an InstancedMesh of zero instances is not a valid buffer,
    // but `count` is the real number — a zero-bone rig then draws nothing, rather than drawing one
    // handle at whatever an all-zeroes matrix collapses to.
    this.mesh = new THREE.InstancedMesh(this.geometry, this.material, Math.max(1, bones.length));
    this.mesh.count = bones.length;
    this.mesh.renderOrder = HANDLE_ORDER;
    this.mesh.frustumCulled = false;
    this.mesh.visible = false;
    scene.add(this.mesh);
    this.paint();
    this.place();
  }

  /** Indexed like `bones`. A short array leaves the tail `unnamed`. */
  setStates(states: JointState[]): void {
    this.states = this.bones.map((_, i): JointState => states[i] ?? 'unnamed');
    this.paint();
  }

  /** -1 = none. */
  setSelected(index: number): void {
    if (index === this.selected) return;
    this.selected = index;
    this.paint();
    this.place();
  }

  /** -1 = none. */
  setHovered(index: number): void {
    if (index === this.hovered) return;
    this.hovered = index;
    this.paint();
    this.place();
  }

  /**
   * Bone index, or -1.
   *
   * ⚠️ Guards on visibility, which `Raycaster` itself does NOT — `intersectObject` tests layers and
   * calls `raycast` regardless of `visible`. `ModelStage` got away with it because the caller only
   * asked while joint editing was on; this class is picked from more than one place.
   */
  pick(raycaster: THREE.Raycaster): number {
    if (!this.mesh.visible || this.bones.length === 0) return -1;
    return raycaster.intersectObject(this.mesh, false)[0]?.instanceId ?? -1;
  }

  /** Per frame: handles follow their bones — both a playing clip and a joint being dragged now. */
  update(): void {
    if (this.mesh.visible) this.place();
  }

  get visible(): boolean {
    return this.mesh.visible;
  }

  set visible(on: boolean) {
    this.mesh.visible = on;
    // Placed on the way in, so the first frame after switching on is never a frame of handles at
    // wherever the bones were when this was last visible.
    if (on) this.place();
  }

  dispose(): void {
    this.scene.remove(this.mesh);
    this.mesh.dispose();
    this.geometry.dispose();
    this.material.dispose();
  }

  private place(): void {
    for (let i = 0; i < this.bones.length; i++) {
      // Translation only — see the class note. Reading the world matrix elements directly avoids
      // decomposing a matrix to throw two thirds of it away.
      const m = this.bones[i].matrixWorld.elements;
      this.matrix.makeTranslation(m[12], m[13], m[14]);
      const scale = i === this.selected ? 1.5 : i === this.hovered ? 1.25 : 1;
      // `Matrix4.scale` post-multiplies, so this is translate-then-scale-in-place: the sphere grows
      // about the joint rather than sliding away from the origin.
      if (scale !== 1) this.matrix.scale(this.grow.set(scale, scale, scale));
      this.mesh.setMatrixAt(i, this.matrix);
    }
    this.mesh.instanceMatrix.needsUpdate = true;
  }

  private paint(): void {
    for (let i = 0; i < this.bones.length; i++) {
      let hex: number;
      if (i === this.selected) hex = i === this.hovered ? JOINT_SELECTED_HOVER : JOINT_SELECTED;
      else if (i === this.hovered) hex = JOINT_HOVER;
      else hex = JOINT_COLOURS[this.states[i]];
      this.mesh.setColorAt(i, this.colour.setHex(hex));
    }
    // Null until the first `setColorAt`, which the loop above skips on a zero-bone rig.
    if (this.mesh.instanceColor) this.mesh.instanceColor.needsUpdate = true;
  }
}
