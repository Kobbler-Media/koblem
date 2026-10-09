import { describe, expect, it } from 'vitest';
import * as THREE from 'three';
import {
  compactVertices,
  componentAtFace,
  labelComponents,
  removeComponents,
  raycastSurface,
  removeDegenerateTriangles,
  type ComponentLabels
} from './mesh-repair';

function geom(positions: number[], index: number[] | null): THREE.BufferGeometry {
  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute('position', new THREE.BufferAttribute(new Float32Array(positions), 3));
  if (index) geometry.setIndex(new THREE.BufferAttribute(new Uint32Array(index), 1));
  return geometry;
}

/** Four vertices, four faces, offset along x so two of them can be disjoint. */
const TETRA_POSITIONS = (dx: number): number[] => [dx, 0, 0, dx + 1, 0, 0, dx, 1, 0, dx, 0, 1];
const TETRA_FACES = (base: number): number[][] => [
  [base + 0, base + 1, base + 2],
  [base + 0, base + 1, base + 3],
  [base + 0, base + 2, base + 3],
  [base + 1, base + 2, base + 3]
];

function twoTetrahedra(faceOrder: 'blocked' | 'interleaved'): THREE.BufferGeometry {
  const positions = [...TETRA_POSITIONS(0), ...TETRA_POSITIONS(10)];
  const a = TETRA_FACES(0);
  const b = TETRA_FACES(4);
  const faces = faceOrder === 'blocked' ? [...a, ...b] : a.flatMap((face, i) => [face, b[i]]);
  return geom(positions, faces.flat());
}

/**
 * A quad split into two triangles that share NO index value: the shared edge's two corners are
 * duplicated, exactly as an exporter does at a UV seam, a hard normal crease or a skin-influence
 * boundary.
 */
function seamSplitQuad(): THREE.BufferGeometry {
  const A = [0, 0, 0];
  const B = [1, 0, 0];
  const C = [1, 1, 0];
  const D = [0, 1, 0];
  // 0,1,2 = A,B,C   3,4,5 = A,C,D — 3 and 4 are bit-identical duplicates of 0 and 2.
  return geom([...A, ...B, ...C, ...A, ...C, ...D], [0, 1, 2, 3, 4, 5]);
}

function faces(index: ArrayLike<number>): number[][] {
  const out: number[][] = [];
  for (let i = 0; i < index.length; i += 3) out.push([index[i], index[i + 1], index[i + 2]]);
  return out;
}

describe('labelComponents', () => {
  it('separates two disjoint solids', () => {
    const { components, faceComponent } = labelComponents(twoTetrahedra('blocked'));
    expect(components).toHaveLength(2);
    expect(components.map((c) => c.triangles)).toEqual([4, 4]);
    expect(components.map((c) => c.vertices)).toEqual([4, 4]);
    expect([...faceComponent]).toEqual([0, 0, 0, 0, 1, 1, 1, 1]);
  });

  /**
   * 🔴 The weld is not an optimisation, it is the difference between a component list and noise.
   * A GLB out of an image-to-3D pipeline is seam-split everywhere, so two triangles that are
   * physically welded can share no index value at all. Labelling on the raw index buffer reports a
   * shredded mesh — this is where "13,000 disconnected components" came from, and turning the weld
   * off reproduces it in miniature.
   */
  it('welds duplicated vertices at the same position into ONE component', () => {
    const { components, faceComponent } = labelComponents(seamSplitQuad());
    expect(components).toHaveLength(1);
    expect(components[0].triangles).toBe(2);
    expect([...faceComponent]).toEqual([0, 0]);
  });

  it('shreds the same quad with the weld off, which is the bug and not the feature', () => {
    const { components } = labelComponents(seamSplitQuad(), { weld: false });
    expect(components).toHaveLength(2);
  });

  /** ⚠️ A loose grid welds the two faces of a thin sleeve together and the UI then offers you
   *  "delete the shirt". The default is a quantisation grid tight enough to catch only exporter
   *  duplicates. */
  it('does not weld two genuinely separate solids at the default tolerance', () => {
    expect(labelComponents(twoTetrahedra('blocked')).components).toHaveLength(2);
  });

  it('merges them under a deliberately absurd tolerance, so the knob is real', () => {
    expect(labelComponents(twoTetrahedra('blocked'), { tolerance: 100 }).components).toHaveLength(
      1
    );
  });

  it('sorts components by triangle count descending, so components[0] is the body', () => {
    // A four-face tetrahedron plus a lone floating triangle written FIRST in the buffer.
    const positions = [...TETRA_POSITIONS(10), 0, 0, 0, 1, 0, 0, 0, 1, 0];
    const speck = [4, 5, 6];
    const body = TETRA_FACES(0).flat();
    const { components, faceComponent } = labelComponents(geom(positions, [...speck, ...body]));
    expect(components.map((c) => c.triangles)).toEqual([4, 1]);
    expect(components[0].id).toBe(0);
    // The speck is face 0 but component 1 — faceComponent is remapped to match the sorted list.
    expect(faceComponent[0]).toBe(1);
  });

  /** The UI stores component ids in its selection state; a reshuffle between two identical passes
   *  would move the user's checkboxes. */
  it('breaks ties deterministically on first appearance, not on hash order', () => {
    const geometry = twoTetrahedra('interleaved');
    const first = labelComponents(geometry).faceComponent;
    const second = labelComponents(geometry).faceComponent;
    expect([...first]).toEqual([...second]);
    expect([...first]).toEqual([0, 1, 0, 1, 0, 1, 0, 1]);
  });

  it('measures each component against the whole mesh', () => {
    const { components } = labelComponents(twoTetrahedra('blocked'));
    expect(components[0].sizeRatio).toBeLessThan(1);
    expect(components[0].sizeRatio).toBeGreaterThan(0);
    expect(components[0].diagonal).toBeCloseTo(Math.sqrt(3), 5);
    expect(components[0].center.length).toBe(3);
  });

  /** Vertices no triangle references are dead weight, not deletable objects — surfacing them in
   *  the UI would be pure confusion. */
  it('ignores vertices that no triangle references', () => {
    const geometry = geom([...TETRA_POSITIONS(0), 99, 99, 99], TETRA_FACES(0).flat());
    const { components } = labelComponents(geometry);
    expect(components).toHaveLength(1);
    expect(components[0].vertices).toBe(4);
  });

  it('refuses a mesh with no index buffer, with a message that says what to do', () => {
    expect(() => labelComponents(geom(TETRA_POSITIONS(0), null))).toThrow(/no index buffer/);
  });

  it('refuses an index that is not a whole number of triangles', () => {
    expect(() => labelComponents(geom(TETRA_POSITIONS(0), [0, 1, 2, 3]))).toThrow(
      /not a whole number of triangles/
    );
  });

  /** An out-of-range read on a typed array yields undefined → NaN → vertex 0, which would silently
   *  corrupt everything downstream. One cheap pass buys a real error instead. */
  it('refuses an index that points past the end of the vertex buffer', () => {
    expect(() => labelComponents(geom(TETRA_POSITIONS(0), [0, 1, 9]))).toThrow(/is corrupt/);
  });
});

describe('componentAtFace', () => {
  const labels: ComponentLabels = {
    components: [],
    faceComponent: new Uint32Array([0, 1, 1])
  };

  it('maps a face index to its component', () => {
    expect(componentAtFace(labels, 0)).toBe(0);
    expect(componentAtFace(labels, 2)).toBe(1);
  });

  /** ⚠️ three types `Intersection.faceIndex` as `number | undefined` — it is undefined for points,
   *  lines and sprites. Every miss is -1, never a throw and never a stray component id. */
  it('is -1 for a miss, an undefined faceIndex and anything out of range', () => {
    expect(componentAtFace(labels, undefined)).toBe(-1);
    expect(componentAtFace(labels, null)).toBe(-1);
    expect(componentAtFace(labels, -1)).toBe(-1);
    expect(componentAtFace(labels, 3)).toBe(-1);
  });
});

describe('removeComponents', () => {
  it('drops the selected components and keeps the rest', () => {
    const geometry = twoTetrahedra('blocked');
    const labels = labelComponents(geometry);
    const out = removeComponents(geometry, labels, new Set([1]));
    expect(faces(out)).toEqual(TETRA_FACES(0));
  });

  /**
   * Survivor triangle order is preserved so a `ComponentLabels` re-scan stays cheap and the diff
   * against the original accessor stays local. Interleaving the two solids' faces is what makes
   * "preserved" mean something stronger than "the survivors happen to be contiguous".
   */
  it('preserves survivor order when the dropped faces were interleaved through them', () => {
    const geometry = twoTetrahedra('interleaved');
    const labels = labelComponents(geometry);
    expect(faces(removeComponents(geometry, labels, new Set([1])))).toEqual(TETRA_FACES(0));
    expect(faces(removeComponents(geometry, labels, new Set([0])))).toEqual(TETRA_FACES(4));
  });

  /** Index-only: no attribute is read, written or reallocated, so skin weights cannot be damaged. */
  it('touches no attribute and no vertex', () => {
    const geometry = twoTetrahedra('blocked');
    const position = geometry.getAttribute('position');
    const before = [...(position.array as Float32Array)];
    removeComponents(geometry, labelComponents(geometry), new Set([1]));
    expect(geometry.getAttribute('position')).toBe(position);
    expect([...(position.array as Float32Array)]).toEqual(before);
  });

  it('drops nothing for an empty selection', () => {
    const geometry = twoTetrahedra('blocked');
    const out = removeComponents(geometry, labelComponents(geometry), new Set());
    expect(out.length).toBe(24);
  });

  it('refuses to delete the whole mesh', () => {
    const geometry = twoTetrahedra('blocked');
    const labels = labelComponents(geometry);
    expect(() => removeComponents(geometry, labels, new Set([0, 1]))).toThrow(/every triangle/);
  });

  /** Labels are indexed by faceIndex and ANY change to the index buffer invalidates them. */
  it('refuses labels computed against a different index buffer', () => {
    const geometry = twoTetrahedra('blocked');
    const stale: ComponentLabels = { components: [], faceComponent: new Uint32Array(3) };
    expect(() => removeComponents(geometry, stale, new Set([0]))).toThrow(/different index buffer/);
  });

  /** ⚠️ The cutoff is 65535, not 65536: three's own `arrayNeedsUint32` reserves 65535 for
   *  PRIMITIVE_RESTART_FIXED_INDEX, and matching it keeps our index type and three's in agreement. */
  it('downcasts the index to Uint16 at exactly 65535 vertices and no further', () => {
    const labels: ComponentLabels = { components: [], faceComponent: new Uint32Array([0]) };
    const small = geom([...new Array(65535 * 3).fill(0)], [0, 1, 2]);
    const large = geom([...new Array(65536 * 3).fill(0)], [0, 1, 2]);
    expect(removeComponents(small, labels, new Set())).toBeInstanceOf(Uint16Array);
    expect(removeComponents(large, labels, new Set())).toBeInstanceOf(Uint32Array);
  });
});

describe('removeDegenerateTriangles', () => {
  it('drops a triangle with a repeated index', () => {
    const geometry = geom(TETRA_POSITIONS(0), [...TETRA_FACES(0).flat(), 0, 0, 1, 2, 3, 3]);
    expect(faces(removeDegenerateTriangles(geometry))).toEqual(TETRA_FACES(0));
  });

  it('drops a zero-area triangle whose three distinct vertices share one position', () => {
    // Verts 4,5,6 are three DIFFERENT indices at the same point — the defect a repeated-index check
    // alone would miss.
    const positions = [...TETRA_POSITIONS(0), 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5];
    const geometry = geom(positions, [...TETRA_FACES(0).flat(), 4, 5, 6]);
    expect(faces(removeDegenerateTriangles(geometry))).toEqual(TETRA_FACES(0));
  });

  it('drops a collinear sliver', () => {
    const positions = [...TETRA_POSITIONS(0), 0, 0, 0, 0.5, 0, 0, 1, 0, 0];
    const geometry = geom(positions, [...TETRA_FACES(0).flat(), 4, 5, 6]);
    expect(faces(removeDegenerateTriangles(geometry))).toEqual(TETRA_FACES(0));
  });

  /**
   * ⚠️ The threshold is RELATIVE to the mesh diagonal. An absolute epsilon is a bug waiting for the
   * first asset authored in centimetres — the same bar that catches slivers on a 2-unit character
   * would eat real triangles on a 200-unit one.
   */
  it('judges a triangle against the mesh it lives in, not against an absolute epsilon', () => {
    // The SAME 1e-4-sized triangle, on two meshes of wildly different extent. It is real geometry on
    // the small one and dust on the large one — which is exactly why an absolute epsilon would eat
    // real triangles on the first asset authored in a different unit.
    const pair = (span: number): THREE.BufferGeometry =>
      geom([0, 0, 0, span, 0, 0, 0, span, 0, 0, 0, 0, 1e-4, 0, 0, 0, 1e-4, 0], [0, 1, 2, 3, 4, 5]);
    expect(removeDegenerateTriangles(pair(1e-3)).length).toBe(6);
    expect(removeDegenerateTriangles(pair(1e6)).length).toBe(3);
  });

  it('leaves a clean mesh exactly as it found it', () => {
    const geometry = twoTetrahedra('blocked');
    const index = geometry.getIndex()!;
    expect(faces(removeDegenerateTriangles(geometry))).toEqual(faces(index.array));
  });

  /** `!(cross2 > minCross2)` rather than `<=`, so a NaN position is dropped rather than kept. */
  it('drops a triangle with a NaN corner', () => {
    const positions = [...TETRA_POSITIONS(0), Number.NaN, 0, 0, 1, 5, 0, 0, 5, 1];
    const geometry = geom(positions, [...TETRA_FACES(0).flat(), 4, 5, 6]);
    expect(faces(removeDegenerateTriangles(geometry))).toEqual(TETRA_FACES(0));
  });

  it('refuses a mesh where nothing at all survives', () => {
    expect(() => removeDegenerateTriangles(geom([0, 0, 0, 1, 0, 0], [0, 0, 1]))).toThrow(
      /nothing to keep/
    );
  });
});

describe('compactVertices', () => {
  /** A tetrahedron plus three orphans that no surviving triangle references. */
  function orphaned(): THREE.BufferGeometry {
    const positions = [...TETRA_POSITIONS(0), 7, 7, 7, 8, 8, 8, 9, 9, 9];
    return geom(positions, TETRA_FACES(0).flat());
  }

  it('drops unreferenced vertices and renumbers the index onto the survivors', () => {
    const geometry = orphaned();
    compactVertices(geometry);
    expect(geometry.getAttribute('position').count).toBe(4);
    expect(faces(geometry.getIndex()!.array)).toEqual(TETRA_FACES(0));
  });

  it('keeps vertices in ascending old order, so the GLB diff stays local', () => {
    // Drop the MIDDLE vertex of a strip: survivors keep their relative order rather than being
    // reordered by first use.
    const geometry = geom([0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0], [3, 4, 0]);
    compactVertices(geometry);
    const position = geometry.getAttribute('position');
    expect([...(position.array as Float32Array)]).toEqual([0, 0, 0, 3, 0, 0, 4, 0, 0]);
    expect(faces(geometry.getIndex()!.array)).toEqual([[1, 2, 0]]);
  });

  /** ⚠️ Vertex ids change; FACE ids do not. A ComponentLabels computed before the call stays valid. */
  it('preserves triangle order exactly', () => {
    const geometry = orphaned();
    const corners = (): number[][][] => {
      const position = [...(geometry.getAttribute('position').array as Float32Array)];
      return faces(geometry.getIndex()!.array).map((f) =>
        f.map((v) => position.slice(v * 3, v * 3 + 3))
      );
    };
    const before = corners();
    compactVertices(geometry);
    expect(corners()).toEqual(before);
  });

  /**
   * 🔴 The normalized-integer trap: `getX` DENORMALIZES and `setX` RENORMALIZES, so a get/set round
   * trip over a `WEIGHTS_0` that arrived as a normalized Uint8 quantises every skin weight — a
   * visible wobble in the deformation with nothing in the console. The rebuild has to copy raw
   * elements and carry `constructor`, `itemSize` and `normalized` across untouched.
   */
  it('rebuilds a normalized integer attribute bit-for-bit, in its original type', () => {
    const geometry = orphaned();
    const weights = new Uint8Array(7 * 4);
    for (let v = 0; v < 7; v++) weights.set([200 + v, 55 - v, 0, 0], v * 4);
    geometry.setAttribute('skinWeight', new THREE.BufferAttribute(weights, 4, true));
    geometry.setAttribute(
      'skinIndex',
      new THREE.BufferAttribute(new Uint16Array([...new Array(7 * 4).keys()]), 4)
    );

    compactVertices(geometry);

    const out = geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
    expect(out.array).toBeInstanceOf(Uint8Array);
    expect(out.normalized).toBe(true);
    expect(out.itemSize).toBe(4);
    expect([...(out.array as Uint8Array)]).toEqual([
      200, 55, 0, 0, 201, 54, 0, 0, 202, 53, 0, 0, 203, 52, 0, 0
    ]);
    const idx = geometry.getAttribute('skinIndex') as THREE.BufferAttribute;
    expect(idx.array).toBeInstanceOf(Uint16Array);
    expect([...(idx.array as Uint16Array)]).toEqual([
      0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
    ]);
  });

  /** ⚠️ Skipping morphs leaves a morph attribute at the old vertex count and three throws on the
   *  next render — the kind of thing that ships broken because the test character had none. */
  it('remaps morph targets alongside the base attributes', () => {
    const geometry = orphaned();
    geometry.morphAttributes.position = [
      new THREE.BufferAttribute(new Float32Array(7 * 3).fill(1), 3)
    ];
    compactVertices(geometry);
    expect(geometry.morphAttributes.position[0].count).toBe(4);
  });

  /** Dropping below 65535 vertices is half the point of running this at the small LOD tiers. */
  it('downcasts the index once the survivors fit in Uint16', () => {
    const geometry = orphaned();
    expect(geometry.getIndex()!.array).toBeInstanceOf(Uint32Array);
    compactVertices(geometry);
    expect(geometry.getIndex()!.array).toBeInstanceOf(Uint16Array);
  });

  it('bails without reallocating when nothing is orphaned', () => {
    const geometry = geom(TETRA_POSITIONS(0), TETRA_FACES(0).flat());
    const position = geometry.getAttribute('position');
    const index = geometry.getIndex();
    compactVertices(geometry);
    expect(geometry.getAttribute('position')).toBe(position);
    expect(geometry.getIndex()).toBe(index);
  });

  it('refuses a corrupt index rather than compacting around it', () => {
    expect(() => compactVertices(geom(TETRA_POSITIONS(0), [0, 1, 99]))).toThrow(/corrupt/);
  });

  it('refreshes the geometry bounds it just invalidated', () => {
    const geometry = orphaned();
    geometry.computeBoundingBox();
    // The orphans at (9,9,9) are inside the stale box and outside the compacted one.
    expect(geometry.boundingBox!.max.x).toBe(9);
    compactVertices(geometry);
    expect(geometry.boundingBox!.max.x).toBe(1);
    expect(geometry.boundingSphere).not.toBeNull();
  });
});

/*
 * `raycastSurface` — the picking path the weight brush stands on.
 *
 * 🔴 The rule under test is the one that fails INVISIBLY: at rest the rendered world surface is
 * `bindMatrix · p`, NOT `matrixWorld · p`. `GLTFLoader` binds with an identity `bindMatrix`
 * (r185 line 4232) and under the default `AttachedBindMode` `updateMatrixWorld` sets
 * `bindMatrixInverse = matrixWorld⁻¹`, so the two cancel. Unproject with `matrixWorld` instead and
 * every pick is wrong by exactly the mesh node's transform — which is zero on a character sitting
 * at the origin, and silently wrong on one that is not. That is why the offset cases below exist.
 */
describe('raycastSurface', () => {
  /** One unit quad in the z = 0 plane, facing +z. */
  function quad(): THREE.BufferGeometry {
    return geom([-1, -1, 0, 1, -1, 0, 1, 1, 0, -1, 1, 0], [0, 1, 2, 0, 2, 3]);
  }

  /** A raycaster aimed down −z at (x, y). */
  function down(x: number, y: number): THREE.Raycaster {
    return new THREE.Raycaster(new THREE.Vector3(x, y, 10), new THREE.Vector3(0, 0, -1));
  }

  it('reports the world hit point through an identity transform', () => {
    const mesh = new THREE.Mesh(quad());
    mesh.updateMatrixWorld(true);
    const hit = raycastSurface(mesh, down(0.25, -0.5));
    expect(hit).not.toBeNull();
    expect(hit!.faceIndex).toBeGreaterThanOrEqual(0);
    expect(hit!.point.x).toBeCloseTo(0.25, 6);
    expect(hit!.point.y).toBeCloseTo(-0.5, 6);
    expect(hit!.point.z).toBeCloseTo(0, 6);
  });

  it('follows a plain Mesh through a translated and scaled node transform', () => {
    const mesh = new THREE.Mesh(quad());
    mesh.position.set(100, 0, 0);
    mesh.scale.setScalar(2);
    mesh.updateMatrixWorld(true);
    // The quad now spans x ∈ [98, 102]; a ray at x = 100.5 lands 0.25 along in local space.
    const hit = raycastSurface(mesh, down(100.5, 0));
    expect(hit).not.toBeNull();
    expect(hit!.point.x).toBeCloseTo(100.5, 5);
  });

  it('🔴 uses bindMatrix, not matrixWorld, on a SkinnedMesh whose node is off the origin', () => {
    const geometry = quad();
    const count = geometry.getAttribute('position').count;
    geometry.setAttribute('skinIndex', new THREE.BufferAttribute(new Uint16Array(count * 4), 4));
    geometry.setAttribute(
      'skinWeight',
      new THREE.BufferAttribute(
        Float32Array.from({ length: count * 4 }, (_, i) => (i % 4 === 0 ? 1 : 0)),
        4
      )
    );
    const bone = new THREE.Bone();
    const skeleton = new THREE.Skeleton([bone]);
    const mesh = new THREE.SkinnedMesh(geometry, undefined);
    mesh.add(bone);
    // The node sits 100 units away, exactly as a GLB whose mesh node is not at the origin would.
    mesh.position.set(100, 0, 0);
    mesh.updateMatrixWorld(true);
    // 🔴 Identity bindMatrix, which is what `GLTFLoader` supplies. `bindMatrixInverse` then cancels
    // `matrixWorld`, so the rest surface stays at the RAW positions — x ∈ [-1, 1], not [99, 101].
    mesh.bind(skeleton, new THREE.Matrix4());
    mesh.updateMatrixWorld(true);

    expect(raycastSurface(mesh, down(0.25, 0))).not.toBeNull();
    // A `matrixWorld`-based implementation would hit here and miss above.
    expect(raycastSurface(mesh, down(100.25, 0))).toBeNull();
  });

  it('returns a normal facing the camera even when the winding says otherwise', () => {
    // Wound so the geometric normal points AWAY from a ray coming down −z. ~200 independently
    // generated shells have no trustworthy winding, and an unflipped normal culls every vertex the
    // dab was aimed at.
    const mesh = new THREE.Mesh(geom([-1, -1, 0, 1, 1, 0, 1, -1, 0], [0, 1, 2]));
    mesh.updateMatrixWorld(true);
    const ray = down(0.4, -0.2);
    const hit = raycastSurface(mesh, ray);
    expect(hit).not.toBeNull();
    expect(hit!.normal.dot(ray.ray.direction)).toBeLessThan(0);
    expect(hit!.normal.length()).toBeCloseTo(1, 6);
  });

  it('returns null on a clean miss', () => {
    const mesh = new THREE.Mesh(quad());
    mesh.updateMatrixWorld(true);
    expect(raycastSurface(mesh, down(50, 50))).toBeNull();
  });
});
