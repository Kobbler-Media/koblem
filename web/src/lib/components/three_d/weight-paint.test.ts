import * as THREE from 'three';
import { describe, expect, it } from 'vitest';

import {
  brushField,
  captureLike,
  condenseInfluences,
  disposeWeightPaintCache,
  EVICTION_FLOOR,
  falloffAt,
  MAX_INFLUENCES,
  readInfluences,
  restoreStroke,
  strokeByteLength,
  transferInfluence,
  verticesInBrush,
  WeightBrush,
  weldedClassCount,
  writeInfluences,
  type Influences
} from './weight-paint';

/* ------------------------------------------------------------------------------------------- */
/* Synthetic rigs                                                                                */
/* ------------------------------------------------------------------------------------------- */

type WeightKind = 'f32' | 'u8' | 'u16';

function weightAttribute(count: number, kind: WeightKind): THREE.BufferAttribute {
  if (kind === 'u8') return new THREE.BufferAttribute(new Uint8Array(count * 4), 4, true);
  if (kind === 'u16') return new THREE.BufferAttribute(new Uint16Array(count * 4), 4, true);
  return new THREE.BufferAttribute(new Float32Array(count * 4), 4);
}

interface Rig {
  mesh: THREE.SkinnedMesh;
  bones: THREE.Bone[];
  count: number;
}

/**
 * A skinned mesh over caller-supplied positions and triangles.
 *
 * `bind(skeleton, identity)` on purpose: `GLTFLoader` calls `mesh.bind( skeleton, _identityMatrix )`,
 * so geometry space and world space coincide on our real assets and the brush's world-space inputs
 * are directly comparable to the positions written here.
 */
function buildRig(
  positions: number[],
  indices: number[],
  boneCount: number,
  kind: WeightKind = 'f32'
): Rig {
  const count = positions.length / 3;
  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute('position', new THREE.BufferAttribute(new Float32Array(positions), 3));
  if (indices.length) geometry.setIndex(indices);
  geometry.setAttribute('skinIndex', new THREE.BufferAttribute(new Uint8Array(count * 4), 4));
  geometry.setAttribute('skinWeight', weightAttribute(count, kind));

  const bones: THREE.Bone[] = [];
  for (let i = 0; i < boneCount; i++) {
    const bone = new THREE.Bone();
    bone.name = `joint${i}`;
    bones.push(bone);
  }
  const mesh = new THREE.SkinnedMesh(geometry, new THREE.MeshBasicMaterial());
  mesh.add(bones[0]);
  mesh.updateMatrixWorld(true);
  mesh.bind(new THREE.Skeleton(bones), new THREE.Matrix4());

  // Everything starts fully bound to joint 0, which is the state a healthy vertex is in.
  for (let v = 0; v < count; v++) writeInfluences(mesh, v, { bones: [0], weights: [1] });
  return { mesh, bones, count };
}

/** An `nx × ny` lattice in the z = 0 plane at unit spacing, triangulated as two faces per quad. */
function buildGridRig(nx: number, ny: number, boneCount = 4, kind: WeightKind = 'f32'): Rig {
  const positions: number[] = [];
  for (let j = 0; j < ny; j++) {
    for (let i = 0; i < nx; i++) positions.push(i, j, 0);
  }
  const indices: number[] = [];
  for (let j = 0; j + 1 < ny; j++) {
    for (let i = 0; i + 1 < nx; i++) {
      const a = j * nx + i;
      indices.push(a, a + 1, a + nx, a + 1, a + nx + 1, a + nx);
    }
  }
  return buildRig(positions, indices, boneCount, kind);
}

function rawWeights(mesh: THREE.SkinnedMesh): number[] {
  const attr = mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
  return Array.from(attr.array as unknown as ArrayLike<number>);
}

function rawJoints(mesh: THREE.SkinnedMesh): number[] {
  const attr = mesh.geometry.getAttribute('skinIndex') as THREE.BufferAttribute;
  return Array.from(attr.array as unknown as ArrayLike<number>);
}

function weightOf(mesh: THREE.SkinnedMesh, vertex: number, bone: number): number {
  const inf = readInfluences(mesh, vertex);
  const i = inf.bones.indexOf(bone);
  return i < 0 ? 0 : inf.weights[i];
}

function sumOf(mesh: THREE.SkinnedMesh, vertex: number): number {
  return readInfluences(mesh, vertex).weights.reduce((a, b) => a + b, 0);
}

/** Every vertex of the mesh sums to 1 — the invariant no operation may break. */
function expectAllSumToOne(rig: Rig, tolerance = 1e-6): void {
  for (let v = 0; v < rig.count; v++) {
    expect(sumOf(rig.mesh, v)).toBeCloseTo(1, -Math.log10(tolerance));
  }
}

function lcg(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

/* ------------------------------------------------------------------------------------------- */

describe('falloffAt', () => {
  it('is 1 at the centre and exactly 0 at the rim for every profile', () => {
    for (const profile of ['smooth', 'linear', 'sharp', 'constant'] as const) {
      expect(falloffAt(0, profile)).toBe(1);
      expect(falloffAt(1, profile)).toBe(0);
      expect(falloffAt(1.5, profile)).toBe(0);
    }
  });

  it('decreases monotonically across the brush', () => {
    for (const profile of ['smooth', 'linear', 'sharp'] as const) {
      let previous = Infinity;
      for (let t = 0; t <= 1.0001; t += 0.05) {
        const f = falloffAt(t, profile);
        expect(f).toBeLessThanOrEqual(previous + 1e-12);
        previous = f;
      }
    }
  });

  /** The default's zero slope at both ends is the reason a dab leaves no visible ring. */
  it('has a flat shoulder and a flat rim on the smooth profile', () => {
    expect(falloffAt(0.01)).toBeGreaterThan(0.999);
    expect(falloffAt(0.99)).toBeLessThan(0.001);
    expect(falloffAt(0.5)).toBeCloseTo(0.5, 6);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('condenseInfluences — the four-influence eviction rule', () => {
  it('leaves a legal binding alone but renormalises it', () => {
    const out = condenseInfluences({ bones: [0, 1], weights: [0.6, 0.6] });
    expect(out.bones).toEqual([0, 1]);
    expect(out.weights[0] + out.weights[1]).toBeCloseTo(1, 12);
    expect(out.evicted).toBe(0);
  });

  it('evicts the smallest influence when a fifth bone arrives', () => {
    const out = condenseInfluences({
      bones: [0, 1, 2, 3, 4],
      weights: [0.4, 0.3, 0.02, 0.2, 0.08]
    });
    expect(out.bones.length).toBe(MAX_INFLUENCES);
    expect(out.bones).not.toContain(2);
    expect(out.evicted).toBe(1);
    expect(out.weights.reduce((a, b) => a + b, 0)).toBeCloseTo(1, 12);
  });

  /** 🔴 Evicting the bone the user is painting would make the whole stroke a silent no-op. */
  it('never evicts the painted bone, even when it is the smallest', () => {
    const out = condenseInfluences(
      { bones: [0, 1, 2, 3, 9], weights: [0.4, 0.3, 0.15, 0.14, 0.01] },
      9
    );
    expect(out.bones).toContain(9);
    expect(out.bones.length).toBe(MAX_INFLUENCES);
    expect(out.bones).not.toContain(3);
    expect(out.weights.reduce((a, b) => a + b, 0)).toBeCloseTo(1, 12);
  });

  it('merges a bone that appears in two slots rather than counting it twice', () => {
    const out = condenseInfluences({ bones: [5, 5, 6], weights: [0.25, 0.25, 0.5] });
    expect(out.bones).toEqual([5, 6]);
    expect(out.weights[0]).toBeCloseTo(0.5, 12);
    expect(out.evicted).toBe(0);
  });

  it('drops sub-epsilon influences and returns nothing for an empty binding', () => {
    expect(condenseInfluences({ bones: [1], weights: [0] }).bones).toEqual([]);
    expect(condenseInfluences({ bones: [], weights: [] }).bones).toEqual([]);
  });

  it('is deterministic when two evictable influences tie', () => {
    const input: Influences = { bones: [7, 3, 1, 2, 0], weights: [0.4, 0.2, 0.2, 0.1, 0.1] };
    const a = condenseInfluences(input);
    const b = condenseInfluences(input);
    expect(a.bones).toEqual(b.bones);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('normalized integer attributes', () => {
  /** 26 + 51 + 77 + 102 = 256. Naive rounding inflates this vertex by 0.4% under animation. */
  it('apportions a Uint8 weight set to sum to exactly 255, not 256', () => {
    const rig = buildRig([0, 0, 0], [], 4, 'u8');
    writeInfluences(rig.mesh, 0, { bones: [0, 1, 2, 3], weights: [0.1, 0.2, 0.3, 0.4] });
    const raw = rawWeights(rig.mesh);
    expect(raw.reduce((a, b) => a + b, 0)).toBe(255);
    expect(sumOf(rig.mesh, 0)).toBeCloseTo(1, 9);
  });

  it('apportions a Uint16 weight set to sum to exactly 65535', () => {
    const rig = buildRig([0, 0, 0], [], 4, 'u16');
    writeInfluences(rig.mesh, 0, { bones: [0, 1, 2], weights: [1 / 3, 1 / 3, 1 / 3] });
    expect(rawWeights(rig.mesh).reduce((a, b) => a + b, 0)).toBe(65535);
  });

  it('round-trips a Uint8 binding within one quantisation step', () => {
    const rig = buildRig([0, 0, 0], [], 4, 'u8');
    const wanted = [0.55, 0.25, 0.15, 0.05];
    writeInfluences(rig.mesh, 0, { bones: [3, 2, 1, 0], weights: wanted });
    const back = readInfluences(rig.mesh, 0);
    expect(back.bones).toEqual([3, 2, 1, 0]);
    back.weights.forEach((w, i) => expect(Math.abs(w - wanted[i])).toBeLessThan(1 / 255));
  });

  /** `getX` denormalizes on read; this module reproduces that divisor by hand. */
  it('denormalizes on read, so a stored 128 reads as ~0.502', () => {
    const rig = buildRig([0, 0, 0], [], 2, 'u8');
    const attr = rig.mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
    (attr.array as Uint8Array).set([128, 127, 0, 0]);
    const inf = readInfluences(rig.mesh, 0);
    expect(inf.weights[0]).toBeCloseTo(128 / 255, 9);
    expect(inf.weights[0]).toBeCloseTo(attr.getX(0), 9);
  });

  /** 🔴 A joint index is the RAW bits, never a denormalized fraction. */
  it('reads joint indices raw and writes them back unscaled', () => {
    const rig = buildRig([0, 0, 0], [], 6, 'u8');
    writeInfluences(rig.mesh, 0, { bones: [5, 3], weights: [0.5, 0.5] });
    expect(rawJoints(rig.mesh).slice(0, 2)).toEqual([5, 3]);
    expect(readInfluences(rig.mesh, 0).bones).toEqual([5, 3]);
  });

  it('keeps a float attribute summing to 1 without quantising it', () => {
    const rig = buildRig([0, 0, 0], [], 4, 'f32');
    writeInfluences(rig.mesh, 0, { bones: [0, 1, 2], weights: [0.11, 0.13, 0.17] });
    expect(sumOf(rig.mesh, 0)).toBeCloseTo(1, 6);
    expect(weightOf(rig.mesh, 0, 0)).toBeCloseTo(0.11 / 0.41, 6);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('the spatial index', () => {
  it('agrees with a brute-force scan on random spheres', () => {
    const rig = buildGridRig(20, 20);
    const positions = rig.mesh.geometry.getAttribute('position') as THREE.BufferAttribute;
    const random = lcg(7);
    for (let trial = 0; trial < 25; trial++) {
      const centre = new THREE.Vector3(random() * 22 - 1, random() * 22 - 1, random() * 2 - 1);
      const radius = 0.2 + random() * 6;
      const expected: number[] = [];
      for (let v = 0; v < rig.count; v++) {
        const p = new THREE.Vector3().fromBufferAttribute(positions, v);
        if (p.distanceTo(centre) <= radius) expected.push(v);
      }
      const found = Array.from(verticesInBrush([rig.mesh], centre, radius)[0]);
      expect(found).toEqual(expected);
    }
  });

  it('reports no duplicates even when the sphere spans many cells', () => {
    const rig = buildGridRig(16, 16);
    const found = verticesInBrush([rig.mesh], new THREE.Vector3(8, 8, 0), 40)[0];
    expect(found.length).toBe(rig.count);
    expect(new Set(found).size).toBe(rig.count);
  });

  it('exposes the falloff as a per-vertex field shaped for the heat overlay', () => {
    const rig = buildGridRig(8, 8);
    const field = brushField([rig.mesh], new THREE.Vector3(3, 3, 0), 2)[0];
    expect(field.length).toBe(rig.count);
    expect(field[3 * 8 + 3]).toBe(1);
    expect(field[0]).toBe(0);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('assign', () => {
  it('drives the target bone up and leaves the vertex summing to 1', () => {
    const rig = buildGridRig(9, 9);
    const brush = new WeightBrush([rig.mesh]);
    const centre = new THREE.Vector3(4, 4, 0);
    brush.dab({ point: centre, radius: 2.5, mode: 'assign', bone: rig.bones[1], strength: 1 });

    const middle = 4 * 9 + 4;
    expect(weightOf(rig.mesh, middle, 1)).toBeCloseTo(1, 6);
    expect(weightOf(rig.mesh, middle, 0)).toBeCloseTo(0, 6);
    expectAllSumToOne(rig);
  });

  it('leaves everything outside the radius untouched', () => {
    const rig = buildGridRig(9, 9);
    const before = rawWeights(rig.mesh);
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(0, 0, 0),
      radius: 1.5,
      mode: 'assign',
      bone: rig.bones[2],
      strength: 1
    });
    const after = rawWeights(rig.mesh);
    const far = 8 * 9 + 8;
    expect(after.slice(far * 4, far * 4 + 4)).toEqual(before.slice(far * 4, far * 4 + 4));
    expect(weightOf(rig.mesh, 0, 2)).toBeCloseTo(1, 6);
  });

  /** A vertex exactly on the rim has falloff 0, so a dab that reaches it must still not move it. */
  it('changes nothing at exactly the brush edge', () => {
    const rig = buildGridRig(5, 5);
    const before = rawWeights(rig.mesh);
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(2, 2, 0),
      // The nearest neighbours sit exactly 1.0 away, i.e. at t = 1.
      radius: 1,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1
    });
    const rim = 2 * 5 + 1;
    expect(rawWeights(rig.mesh).slice(rim * 4, rim * 4 + 4)).toEqual(
      before.slice(rim * 4, rim * 4 + 4)
    );
    // The centre is at t = 0 and must have moved.
    expect(weightOf(rig.mesh, 2 * 5 + 2, 1)).toBeCloseTo(1, 6);
  });

  it('honours a partial target value', () => {
    const rig = buildGridRig(5, 5);
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(2, 2, 0),
      radius: 0.5,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1,
      value: 0.25
    });
    const centre = 2 * 5 + 2;
    expect(weightOf(rig.mesh, centre, 1)).toBeCloseTo(0.25, 6);
    expect(weightOf(rig.mesh, centre, 0)).toBeCloseTo(0.75, 6);
  });

  it('evicts the weakest influence to make room for a fifth bone', () => {
    const rig = buildGridRig(5, 5, 6);
    const centre = 2 * 5 + 2;
    writeInfluences(rig.mesh, centre, {
      bones: [0, 1, 2, 3],
      weights: [0.5, 0.3, 0.18, 0.02]
    });
    const result = new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(2, 2, 0),
      radius: 0.5,
      mode: 'assign',
      bone: rig.bones[4],
      strength: 1,
      value: 0.5
    });

    const after = readInfluences(rig.mesh, centre);
    expect(after.bones.length).toBeLessThanOrEqual(MAX_INFLUENCES);
    expect(after.bones).toContain(4);
    expect(after.bones).not.toContain(3);
    expect(result.evictions).toBeGreaterThan(0);
    expect(after.weights.reduce((a, b) => a + b, 0)).toBeCloseTo(1, 6);
  });

  /** 🔴 The faint rim of a brush must never destroy a real binding for a weight that rounds to 0. */
  it('declines to evict for an influence below the quantisation floor', () => {
    const rig = buildGridRig(5, 5, 6);
    const rim = 2 * 5 + 3;
    const full: Influences = { bones: [0, 1, 2, 3], weights: [0.4, 0.3, 0.2, 0.1] };
    writeInfluences(rig.mesh, rim, full);
    const before = rawWeights(rig.mesh).slice(rim * 4, rim * 4 + 4);

    const result = new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(2, 2, 0),
      radius: 1.0001,
      mode: 'assign',
      bone: rig.bones[4],
      // At t ≈ 0.9999 the smooth profile is ~1e-8, so the incoming weight is far below the floor.
      strength: 1
    });

    expect(result.declined).toBeGreaterThan(0);
    expect(rawWeights(rig.mesh).slice(rim * 4, rim * 4 + 4)).toEqual(before);
    expect(EVICTION_FLOOR).toBeCloseTo(1 / 255, 12);
  });

  it('holds the sum-to-one invariant on a normalized Uint8 rig', () => {
    const rig = buildGridRig(9, 9, 4, 'u8');
    const brush = new WeightBrush([rig.mesh]);
    for (let i = 0; i < 12; i++) {
      brush.dab({
        point: new THREE.Vector3(4 + Math.cos(i) * 2, 4 + Math.sin(i) * 2, 0),
        radius: 3,
        mode: 'assign',
        bone: rig.bones[(i % 3) + 1],
        strength: 0.6
      });
    }
    const attr = rig.mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
    for (let v = 0; v < rig.count; v++) {
      let raw = 0;
      for (let k = 0; k < 4; k++) raw += (attr.array as Uint8Array)[v * 4 + k];
      expect(raw).toBe(255);
    }
  });

  it('skips a mesh whose skeleton does not carry the bone', () => {
    const rig = buildGridRig(4, 4);
    const stranger = new THREE.Bone();
    const before = rawWeights(rig.mesh);
    const result = new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(1, 1, 0),
      radius: 5,
      mode: 'assign',
      bone: stranger,
      strength: 1
    });
    expect(result.meshesSkipped).toBe(1);
    expect(result.vertices).toBe(0);
    expect(rawWeights(rig.mesh)).toEqual(before);
  });

  /** A Uint8 `JOINTS_0` cannot hold bone 300; writing it would truncate to 44. */
  it('refuses a bone index the skinIndex attribute cannot represent', () => {
    const rig = buildGridRig(3, 3, 300);
    const before = rawWeights(rig.mesh);
    const result = new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(1, 1, 0),
      radius: 5,
      mode: 'assign',
      bone: rig.bones[299],
      strength: 1
    });
    expect(result.meshesSkipped).toBe(1);
    expect(rawWeights(rig.mesh)).toEqual(before);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('erase', () => {
  it('pulls weight off the target and shares it out proportionally', () => {
    const rig = buildGridRig(5, 5, 4);
    const centre = 2 * 5 + 2;
    writeInfluences(rig.mesh, centre, { bones: [0, 1, 2], weights: [0.5, 0.3, 0.2] });
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(2, 2, 0),
      radius: 0.5,
      mode: 'erase',
      bone: rig.bones[0],
      strength: 1
    });
    const after = readInfluences(rig.mesh, centre);
    expect(after.bones).not.toContain(0);
    // 0.3 : 0.2 becomes 0.6 : 0.4.
    expect(weightOf(rig.mesh, centre, 1)).toBeCloseTo(0.6, 6);
    expect(weightOf(rig.mesh, centre, 2)).toBeCloseTo(0.4, 6);
    expect(after.weights.reduce((a, b) => a + b, 0)).toBeCloseTo(1, 6);
  });

  /** 🔴 An unskinned vertex deforms nothing — the one failure `applyPrune` also refuses to ship. */
  it('spares a vertex whose only binding is the erased bone', () => {
    const rig = buildGridRig(3, 3);
    const before = rawWeights(rig.mesh);
    const result = new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(1, 1, 0),
      radius: 5,
      mode: 'erase',
      bone: rig.bones[0],
      strength: 1
    });
    expect(result.spared).toBe(rig.count);
    expect(result.vertices).toBe(0);
    expect(rawWeights(rig.mesh)).toEqual(before);
  });

  it('is a no-op on a vertex the bone does not touch', () => {
    const rig = buildGridRig(3, 3, 4);
    const before = rawWeights(rig.mesh);
    const result = new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(1, 1, 0),
      radius: 5,
      mode: 'erase',
      bone: rig.bones[2],
      strength: 1
    });
    expect(result.vertices).toBe(0);
    expect(rawWeights(rig.mesh)).toEqual(before);
  });

  it('partially erases at partial strength and still sums to 1', () => {
    const rig = buildGridRig(5, 5, 4);
    const centre = 2 * 5 + 2;
    writeInfluences(rig.mesh, centre, { bones: [0, 1], weights: [0.8, 0.2] });
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(2, 2, 0),
      radius: 0.5,
      mode: 'erase',
      bone: rig.bones[0],
      strength: 0.5
    });
    expect(weightOf(rig.mesh, centre, 0)).toBeCloseTo(0.4, 6);
    expect(sumOf(rig.mesh, centre)).toBeCloseTo(1, 6);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('smooth', () => {
  /** A 12-column strip, bone 0 on the left half and bone 1 on the right. */
  function stepRig(): Rig {
    const rig = buildGridRig(12, 2, 3);
    for (let v = 0; v < rig.count; v++) {
      const column = v % 12;
      writeInfluences(
        rig.mesh,
        v,
        column < 6 ? { bones: [0], weights: [1] } : { bones: [1], weights: [1] }
      );
    }
    return rig;
  }

  it('converges monotonically instead of oscillating', () => {
    const rig = stepRig();
    const brush = new WeightBrush([rig.mesh]);
    const probe = 5; // last column on the bone-0 side
    const history: number[] = [weightOf(rig.mesh, probe, 1)];

    for (let step = 0; step < 120; step++) {
      brush.dab({
        point: new THREE.Vector3(5.5, 0.5, 0),
        radius: 40,
        mode: 'smooth',
        strength: 1,
        profile: 'constant'
      });
      history.push(weightOf(rig.mesh, probe, 1));
    }

    // Monotone: the probe's share of bone 1 only ever rises. A neighbours-only average would flip
    // it back and forth across the step every iteration instead.
    for (let i = 1; i < history.length; i++) {
      expect(history[i]).toBeGreaterThanOrEqual(history[i - 1] - 1e-9);
    }
    // And it settles: the last steps move far less than the first ones.
    const early = history[2] - history[1];
    const late = history[history.length - 1] - history[history.length - 2];
    expect(late).toBeLessThan(early);
    expect(late).toBeLessThan(1e-3);
    // The whole strip is connected, so it converges on the global mean.
    expect(history[history.length - 1]).toBeGreaterThan(0.4);
    expect(history[history.length - 1]).toBeLessThan(0.6);
  });

  it('softens the step without breaking the sum', () => {
    const rig = stepRig();
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(5.5, 0.5, 0),
      radius: 40,
      mode: 'smooth',
      strength: 1,
      profile: 'constant'
    });
    expect(weightOf(rig.mesh, 5, 1)).toBeGreaterThan(0);
    expect(weightOf(rig.mesh, 6, 0)).toBeGreaterThan(0);
    expect(weightOf(rig.mesh, 0, 0)).toBeCloseTo(1, 6);
    expectAllSumToOne(rig);
  });

  /**
   * 🔴 Jacobi, not Gauss–Seidel: no vertex may see a value another vertex was given by the same dab.
   *
   * A 3×3 lattice triangulated two-faces-per-quad gives the centre vertex 4 six neighbours and
   * vertex 1 four. With bone 1 on the centre alone and a full-strength constant dab, Jacobi has to
   * give the centre `1/7` (its own 1, plus six zeroes) AND vertex 1 `1/5` (four zeroes plus the
   * centre's OLD 1). Gauss–Seidel cannot produce both: whichever of the two it writes second reads
   * the other's fresh value and lands somewhere else entirely.
   */
  it('reads its neighbours from a pre-dab snapshot', () => {
    const rig = buildGridRig(3, 3, 3);
    writeInfluences(rig.mesh, 4, { bones: [1], weights: [1] });
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(1, 1, 0),
      radius: 40,
      mode: 'smooth',
      strength: 1,
      profile: 'constant'
    });
    expect(weightOf(rig.mesh, 4, 1)).toBeCloseTo(1 / 7, 6);
    expect(weightOf(rig.mesh, 1, 1)).toBeCloseTo(1 / 5, 6);
    expectAllSumToOne(rig);
  });

  it('holds the invariant on a normalized Uint8 rig', () => {
    const rig = buildGridRig(8, 8, 3, 'u8');
    for (let v = 0; v < rig.count; v++) {
      writeInfluences(
        rig.mesh,
        v,
        v % 3 === 0 ? { bones: [1], weights: [1] } : { bones: [0], weights: [1] }
      );
    }
    const brush = new WeightBrush([rig.mesh]);
    for (let i = 0; i < 5; i++) {
      brush.dab({ point: new THREE.Vector3(4, 4, 0), radius: 20, mode: 'smooth', strength: 0.7 });
    }
    const attr = rig.mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
    for (let v = 0; v < rig.count; v++) {
      let raw = 0;
      for (let k = 0; k < 4; k++) raw += (attr.array as Uint8Array)[v * 4 + k];
      expect(raw).toBe(255);
    }
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('welded position classes', () => {
  /**
   * Two quads that meet at x = 1 but do not share vertices there — the way a GLB splits a UV seam.
   * Columns 2/3 and 4/5 are bit-identical positions.
   */
  function seamRig(): Rig {
    const positions = [
      0,
      0,
      0,
      0,
      1,
      0, // 0, 1  — left edge
      1,
      0,
      0,
      1,
      1,
      0, // 2, 3  — seam, left copy
      1,
      0,
      0,
      1,
      1,
      0, // 4, 5  — seam, right copy
      2,
      0,
      0,
      2,
      1,
      0 // 6, 7  — right edge
    ];
    const indices = [0, 2, 1, 1, 2, 3, 4, 6, 5, 5, 6, 7];
    return buildRig(positions, indices, 3);
  }

  it('collapses split vertices into one surface point', () => {
    const rig = seamRig();
    expect(rig.count).toBe(8);
    expect(weldedClassCount(rig.mesh)).toBe(6);
  });

  /** 🔴 Painting one twin and not the other tears the surface open under animation. */
  it('writes the same binding to every twin, whatever they started as', () => {
    const rig = seamRig();
    writeInfluences(rig.mesh, 2, { bones: [0], weights: [1] });
    writeInfluences(rig.mesh, 4, { bones: [1], weights: [1] });
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(1, 0, 0),
      radius: 0.5,
      mode: 'assign',
      bone: rig.bones[2],
      strength: 0.5
    });
    expect(readInfluences(rig.mesh, 2)).toEqual(readInfluences(rig.mesh, 4));
    expect(weightOf(rig.mesh, 4, 2)).toBeGreaterThan(0);
  });

  /** Triangle adjacency stops at a seam; welded adjacency does not, which is the point. */
  it('smooths across a seam that triangle adjacency alone would not cross', () => {
    const rig = seamRig();
    for (const v of [6, 7]) writeInfluences(rig.mesh, v, { bones: [1], weights: [1] });

    new WeightBrush([rig.mesh]).dab({
      // Reaches only the seam column: the outer columns are 1.118 away.
      point: new THREE.Vector3(1, 0.5, 0),
      radius: 0.9,
      mode: 'smooth',
      strength: 1,
      profile: 'constant'
    });

    // Bone 1 lives only on the RIGHT side of the seam, so any of it arriving at vertex 2 — which has
    // no triangle touching the right quad — came through the weld.
    expect(weightOf(rig.mesh, 2, 1)).toBeGreaterThan(0);
    expect(readInfluences(rig.mesh, 2)).toEqual(readInfluences(rig.mesh, 4));
    // The outer columns were outside the brush.
    expect(weightOf(rig.mesh, 0, 0)).toBeCloseTo(1, 6);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('stroke records', () => {
  it('restores the touched vertices exactly, bit for bit', () => {
    const rig = buildGridRig(12, 12, 4, 'u8');
    const before = rawWeights(rig.mesh);
    const beforeJoints = rawJoints(rig.mesh);

    const brush = new WeightBrush([rig.mesh]);
    brush.beginStroke();
    for (let i = 0; i < 6; i++) {
      brush.dab({
        point: new THREE.Vector3(3 + i, 6, 0),
        radius: 2.5,
        mode: 'assign',
        bone: rig.bones[2],
        strength: 0.8
      });
    }
    const record = brush.commitStroke()!;
    expect(rawWeights(rig.mesh)).not.toEqual(before);

    restoreStroke(record);
    expect(rawWeights(rig.mesh)).toEqual(before);
    expect(rawJoints(rig.mesh)).toEqual(beforeJoints);
  });

  it('records only the vertices the stroke touched, not the whole attribute', () => {
    const rig = buildGridRig(24, 24, 4, 'u8');
    const brush = new WeightBrush([rig.mesh]);
    brush.beginStroke();
    brush.dab({
      point: new THREE.Vector3(4, 4, 0),
      radius: 2,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1
    });
    const record = brush.commitStroke()!;

    expect(record.vertices).toBeGreaterThan(0);
    expect(record.vertices).toBeLessThan(rig.count / 4);
    // Uint32 vertex id + 4 raw Uint8 joints + 4 raw Uint8 weights.
    expect(record.bytes).toBe(record.vertices * 12);
    expect(strokeByteLength(record)).toBe(record.bytes);
    // Against the full-attribute copy the workbench takes for its coarse restore.
    const fullCopy = (rig.mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute).array
      .byteLength;
    expect(record.bytes).toBeLessThan(fullCopy);
  });

  it('costs 28 bytes a vertex on a Uint16/Float32 layout', () => {
    const rig = buildGridRig(8, 8, 4, 'f32');
    const geometry = rig.mesh.geometry;
    const joints = geometry.getAttribute('skinIndex') as THREE.BufferAttribute;
    geometry.setAttribute(
      'skinIndex',
      new THREE.BufferAttribute(Uint16Array.from(joints.array as unknown as ArrayLike<number>), 4)
    );
    const brush = new WeightBrush([rig.mesh]);
    brush.beginStroke();
    brush.dab({
      point: new THREE.Vector3(4, 4, 0),
      radius: 1.5,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1
    });
    const record = brush.commitStroke()!;
    expect(record.bytes).toBe(record.vertices * 28);
  });

  it('captures a vertex once even when a stroke dabs it repeatedly', () => {
    const rig = buildGridRig(6, 6);
    const brush = new WeightBrush([rig.mesh]);
    brush.beginStroke();
    for (let i = 0; i < 10; i++) {
      brush.dab({
        point: new THREE.Vector3(2, 2, 0),
        radius: 1.2,
        mode: 'assign',
        bone: rig.bones[1],
        strength: 0.3
      });
    }
    const record = brush.commitStroke()!;
    const ids = Array.from(record.meshes[0].vertices);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('cancelling a stroke puts everything back', () => {
    const rig = buildGridRig(10, 10, 4, 'u8');
    const before = rawWeights(rig.mesh);
    const brush = new WeightBrush([rig.mesh]);
    brush.beginStroke();
    brush.dab({
      point: new THREE.Vector3(5, 5, 0),
      radius: 4,
      mode: 'assign',
      bone: rig.bones[3],
      strength: 1
    });
    brush.cancelStroke();
    expect(rawWeights(rig.mesh)).toEqual(before);
  });

  it('gives back a redo record of the same size over the same vertices', () => {
    const rig = buildGridRig(10, 10, 4, 'u8');
    const brush = new WeightBrush([rig.mesh]);
    brush.beginStroke();
    brush.dab({
      point: new THREE.Vector3(5, 5, 0),
      radius: 3,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1
    });
    const undo = brush.commitStroke()!;
    const redo = captureLike(undo);
    const painted = rawWeights(rig.mesh);

    expect(redo.vertices).toBe(undo.vertices);
    expect(redo.bytes).toBe(undo.bytes);
    restoreStroke(undo);
    expect(rawWeights(rig.mesh)).not.toEqual(painted);
    restoreStroke(redo);
    expect(rawWeights(rig.mesh)).toEqual(painted);
  });

  it('records nothing when no stroke is open, and returns null for an empty one', () => {
    const rig = buildGridRig(6, 6);
    const brush = new WeightBrush([rig.mesh]);
    expect(brush.commitStroke()).toBeNull();
    brush.beginStroke();
    expect(brush.commitStroke()).toBeNull();
  });

  /** A rebuilt mesh renumbers vertices, so the record's ids point at different geometry. */
  it('skips a mesh whose vertex count changed since the record was taken', () => {
    const rig = buildGridRig(6, 6, 4, 'u8');
    const brush = new WeightBrush([rig.mesh]);
    brush.beginStroke();
    brush.dab({
      point: new THREE.Vector3(2, 2, 0),
      radius: 2,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1
    });
    const record = brush.commitStroke()!;
    const painted = rawWeights(rig.mesh);

    rig.mesh.geometry.setAttribute('position', new THREE.BufferAttribute(new Float32Array(9), 3));
    restoreStroke(record);
    expect(rawWeights(rig.mesh)).toEqual(painted);
  });
});

/* ------------------------------------------------------------------------------------------- */

describe('brush plumbing', () => {
  it('does nothing for a zero radius or zero strength', () => {
    const rig = buildGridRig(6, 6);
    const before = rawWeights(rig.mesh);
    const brush = new WeightBrush([rig.mesh]);
    brush.dab({ point: new THREE.Vector3(2, 2, 0), radius: 0, mode: 'assign', bone: rig.bones[1] });
    brush.dab({
      point: new THREE.Vector3(2, 2, 0),
      radius: 3,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 0
    });
    expect(rawWeights(rig.mesh)).toEqual(before);
  });

  /** Without it, painting the front of a limb paints the back of it too. */
  it('culls vertices facing away from the brush normal', () => {
    const positions = [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0.2, 1, 0, 0.2, 0, 1, 0.2];
    const normals = [0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, -1, 0, 0, -1, 0, 0, -1];
    const rig = buildRig(positions, [0, 1, 2, 3, 4, 5], 3);
    rig.mesh.geometry.setAttribute(
      'normal',
      new THREE.BufferAttribute(new Float32Array(normals), 3)
    );
    disposeWeightPaintCache(rig.mesh.geometry);

    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(0.3, 0.3, 0),
      radius: 3,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1,
      profile: 'constant',
      normal: new THREE.Vector3(0, 0, 1)
    });

    expect(weightOf(rig.mesh, 0, 1)).toBeCloseTo(1, 6);
    expect(weightOf(rig.mesh, 3, 1)).toBe(0);
    expect(weightOf(rig.mesh, 3, 0)).toBeCloseTo(1, 6);
  });

  it('paints through when no normal is supplied', () => {
    const positions = [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0.2, 1, 0, 0.2, 0, 1, 0.2];
    const rig = buildRig(positions, [0, 1, 2, 3, 4, 5], 3);
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(0.3, 0.3, 0),
      radius: 3,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1
    });
    expect(weightOf(rig.mesh, 3, 1)).toBeGreaterThan(0);
  });

  it('follows the mesh transform when the bind matrix is not the identity', () => {
    const rig = buildGridRig(5, 5);
    // Re-bind with a translation: the same world point now lands somewhere else in geometry space.
    rig.mesh.bind(rig.mesh.skeleton, new THREE.Matrix4().makeTranslation(10, 0, 0));
    new WeightBrush([rig.mesh]).dab({
      point: new THREE.Vector3(12, 2, 0),
      radius: 0.5,
      mode: 'assign',
      bone: rig.bones[1],
      strength: 1
    });
    expect(weightOf(rig.mesh, 2 * 5 + 2, 1)).toBeCloseTo(1, 6);
    expect(weightOf(rig.mesh, 0, 1)).toBe(0);
  });

  it('rebuilds its index when the geometry is replaced under it', () => {
    const rig = buildGridRig(5, 5);
    expect(verticesInBrush([rig.mesh], new THREE.Vector3(2, 2, 0), 0.5)[0].length).toBe(1);
    const moved = new Float32Array(rig.count * 3);
    moved.fill(50);
    rig.mesh.geometry.setAttribute('position', new THREE.BufferAttribute(moved, 3));
    expect(verticesInBrush([rig.mesh], new THREE.Vector3(2, 2, 0), 0.5)[0].length).toBe(0);
    expect(verticesInBrush([rig.mesh], new THREE.Vector3(50, 50, 50), 0.5)[0].length).toBe(
      rig.count
    );
  });
});

describe('transferInfluence', () => {
  /** The dissolve case the brush cannot do: the doomed joint is a vertex's ONLY influence. */
  it('moves a sole influence wholesale and leaves nothing referencing the joint', () => {
    const rig = buildGridRig(4, 4);
    for (let v = 0; v < rig.count; v++) writeInfluences(rig.mesh, v, { bones: [3], weights: [1] });

    const result = transferInfluence([rig.mesh], 3, 1);
    expect(result.vertices).toBe(rig.count);
    expect(result.sole).toBe(rig.count);
    for (let v = 0; v < rig.count; v++) {
      expect(weightOf(rig.mesh, v, 3)).toBe(0);
      expect(weightOf(rig.mesh, v, 1)).toBeCloseTo(1, 6);
    }
    expectAllSumToOne(rig);
  });

  it('sums into a target the vertex already carries, freeing the slot', () => {
    const rig = buildGridRig(3, 3);
    writeInfluences(rig.mesh, 0, { bones: [2, 1, 0], weights: [0.3, 0.5, 0.2] });

    transferInfluence([rig.mesh], 2, 1);
    const after = readInfluences(rig.mesh, 0);
    expect(after.bones).toHaveLength(2);
    expect(weightOf(rig.mesh, 0, 1)).toBeCloseTo(0.8, 5);
    expect(weightOf(rig.mesh, 0, 0)).toBeCloseTo(0.2, 5);
    expect(weightOf(rig.mesh, 0, 2)).toBe(0);
  });

  it('leaves vertices that never referenced the joint untouched', () => {
    const rig = buildGridRig(3, 3);
    writeInfluences(rig.mesh, 4, { bones: [2], weights: [1] });
    const before = rawJoints(rig.mesh).slice();

    const result = transferInfluence([rig.mesh], 2, 1);
    expect(result.vertices).toBe(1);
    const after = rawJoints(rig.mesh);
    for (let v = 0; v < rig.count; v++) {
      if (v === 4) continue;
      for (let k = 0; k < MAX_INFLUENCES; k++) {
        expect(after[v * MAX_INFLUENCES + k]).toBe(before[v * MAX_INFLUENCES + k]);
      }
    }
  });

  /** The record is the undo, and it has to be exact on a quantised attribute too. */
  it('hands back a record that restores the delivered bytes exactly', () => {
    const rig = buildGridRig(4, 4, 4, 'u8');
    writeInfluences(rig.mesh, 5, { bones: [2, 1], weights: [0.4, 0.6] });
    writeInfluences(rig.mesh, 6, { bones: [2], weights: [1] });
    const joints = rawJoints(rig.mesh).slice();
    const weights = rawWeights(rig.mesh).slice();

    const result = transferInfluence([rig.mesh], 2, 0);
    expect(result.before.vertices).toBe(2);
    expect(strokeByteLength(result.before)).toBeGreaterThan(0);
    expect(rawJoints(rig.mesh)).not.toEqual(joints);

    restoreStroke(result.before);
    expect(rawJoints(rig.mesh)).toEqual(joints);
    expect(rawWeights(rig.mesh)).toEqual(weights);
  });

  /** 🔴 A weightless slot still holds the joint INDEX, and the file-side removal refuses to
   *  renumber around a vertex that still points at the joint being cut. */
  it('clears a reference carrying no weight at all', () => {
    const rig = buildGridRig(3, 3);
    const joints = rig.mesh.geometry.getAttribute('skinIndex') as THREE.BufferAttribute;
    // Slot 1 names joint 2 at weight 0 — invisible to readInfluences, still in JOINTS_0.
    joints.setY(4, 2);
    expect(readInfluences(rig.mesh, 4).bones).not.toContain(2);

    const result = transferInfluence([rig.mesh], 2, 1);
    expect(result.vertices).toBe(1);
    expect(result.sole).toBe(0);
    for (let k = 0; k < MAX_INFLUENCES; k++) {
      expect(rawJoints(rig.mesh)[4 * MAX_INFLUENCES + k]).not.toBe(2);
    }
    expectAllSumToOne(rig);
  });

  it('is a no-op when the joint drives nothing, and when source and target are the same', () => {
    const rig = buildGridRig(3, 3);
    const joints = rawJoints(rig.mesh).slice();
    expect(transferInfluence([rig.mesh], 3, 1).vertices).toBe(0);
    expect(transferInfluence([rig.mesh], 0, 0).vertices).toBe(0);
    expect(rawJoints(rig.mesh)).toEqual(joints);
  });
});
