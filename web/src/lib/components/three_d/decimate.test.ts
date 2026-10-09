import { beforeAll, describe, expect, it } from 'vitest';
import * as THREE from 'three';
import { decimate, starvedJoints } from './mesh-repair';

/**
 * These run against the REAL meshoptimizer WASM module, not a stub. A stub would prove nothing:
 * every claim under test — "no vertex moves", "the index still addresses live vertices", "the
 * achieved ratio is reported honestly" — is a claim about what meshoptimizer actually does, and a
 * fake that returns a plausible index buffer would pass all of them while telling us nothing.
 *
 * Instantiation costs ~10 ms once, and nothing here renders.
 */
beforeAll(async () => {
  const mod = await import('meshoptimizer/simplifier');
  await mod.MeshoptSimplifier.ready;
});

/**
 * A displaced grid: `2n²` triangles over `(n+1)²` vertices, with normals and UVs so the
 * attribute-aware path is the one exercised by default.
 *
 * Displaced rather than flat on purpose. A perfectly planar grid has zero position error for every
 * possible collapse, so the simplifier's choices are arbitrary and a test built on it measures the
 * tie-break rather than the algorithm.
 */
function grid(n: number): THREE.BufferGeometry {
  const vertexCount = (n + 1) * (n + 1);
  const position = new Float32Array(vertexCount * 3);
  const normal = new Float32Array(vertexCount * 3);
  const uv = new Float32Array(vertexCount * 2);

  for (let y = 0; y <= n; y++) {
    for (let x = 0; x <= n; x++) {
      const v = y * (n + 1) + x;
      const fx = x / n;
      const fy = y / n;
      position[v * 3] = fx * 2 - 1;
      position[v * 3 + 1] = fy * 2 - 1;
      position[v * 3 + 2] = Math.sin(fx * 9) * Math.cos(fy * 7) * 0.25;
      normal[v * 3 + 2] = 1;
      uv[v * 2] = fx;
      uv[v * 2 + 1] = fy;
    }
  }

  const index = new Uint32Array(n * n * 6);
  let k = 0;
  for (let y = 0; y < n; y++) {
    for (let x = 0; x < n; x++) {
      const a = y * (n + 1) + x;
      const b = a + 1;
      const c = a + (n + 1);
      const d = c + 1;
      index[k++] = a;
      index[k++] = c;
      index[k++] = b;
      index[k++] = b;
      index[k++] = c;
      index[k++] = d;
    }
  }

  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute('position', new THREE.BufferAttribute(position, 3));
  geometry.setAttribute('normal', new THREE.BufferAttribute(normal, 3));
  geometry.setAttribute('uv', new THREE.BufferAttribute(uv, 2));
  geometry.setIndex(new THREE.BufferAttribute(index, 1));
  return geometry;
}

/**
 * Skin the grid onto `joints` bones in vertical bands, plus one bone that drives a single vertex
 * in the middle of the sheet.
 *
 * That last bone is the whole point: it is the joint decimation is most likely to starve, and it is
 * placed in the interior rather than on the border precisely because the border is where
 * meshoptimizer refuses to collapse.
 */
function skin(geometry: THREE.BufferGeometry, bands: number, lonelyVertex: number): void {
  const count = geometry.getAttribute('position').count;
  const skinIndex = new Uint16Array(count * 4);
  const skinWeight = new Float32Array(count * 4);
  for (let v = 0; v < count; v++) {
    skinIndex[v * 4] = Math.min(bands - 1, Math.floor((v / count) * bands));
    skinWeight[v * 4] = 1;
  }
  skinIndex[lonelyVertex * 4 + 1] = bands; // the extra, single-vertex joint
  skinWeight[lonelyVertex * 4] = 0.5;
  skinWeight[lonelyVertex * 4 + 1] = 0.5;
  geometry.setAttribute('skinIndex', new THREE.BufferAttribute(skinIndex, 4));
  geometry.setAttribute('skinWeight', new THREE.BufferAttribute(skinWeight, 4));
}

function snapshot(array: ArrayLike<number> & { slice(a?: number, b?: number): unknown }): unknown {
  return array.slice();
}

describe('decimate', () => {
  it('returns fewer triangles over the same vertex array', async () => {
    const geometry = grid(40);
    const before = geometry.getIndex()!.count / 3;

    const report = await decimate(geometry, { ratio: 0.25 });

    expect(report.inputTriangles).toBe(before);
    expect(report.outputTriangles).toBeLessThan(before);
    expect(report.outputTriangles).toBeLessThanOrEqual(report.requestedTriangles);
    expect(report.metTarget).toBe(true);
    expect(report.unchanged).toBe(false);
    // Nothing is applied — the caller does that.
    expect(geometry.getIndex()!.count / 3).toBe(before);
  });

  /**
   * 🔴 The load-bearing invariant. An index past the end of the vertex array reads `undefined` from
   * a typed array, which becomes NaN, which three renders as vertex 0 — a mesh with a spike to the
   * origin and no error anywhere.
   */
  it('emits only indices that address a live vertex', async () => {
    const geometry = grid(30);
    const vertexCount = geometry.getAttribute('position').count;
    const original = new Set(Array.from(geometry.getIndex()!.array as ArrayLike<number>));

    const report = await decimate(geometry, { ratio: 0.3 });

    expect(report.index.length % 3).toBe(0);
    for (const v of report.index) {
      expect(Number.isInteger(v)).toBe(true);
      expect(v).toBeGreaterThanOrEqual(0);
      expect(v).toBeLessThan(vertexCount);
      // Stronger than the contract needs, and true in meshoptimizer 1.2: a collapse retargets a
      // corner onto a vertex that was already drawn, never onto an orphan.
      expect(original.has(v)).toBe(true);
    }
  });

  /** The entire argument for offering this at all: the vertex data is never written. */
  it('leaves the position, normal and uv arrays byte-identical', async () => {
    const geometry = grid(30);
    const position = geometry.getAttribute('position') as THREE.BufferAttribute;
    const normal = geometry.getAttribute('normal') as THREE.BufferAttribute;
    const uv = geometry.getAttribute('uv') as THREE.BufferAttribute;
    const positionBefore = snapshot(position.array as Float32Array);
    const normalBefore = snapshot(normal.array as Float32Array);
    const uvBefore = snapshot(uv.array as Float32Array);

    await decimate(geometry, { ratio: 0.2 });

    // Same object, same contents. `readPositions` hands meshoptimizer a `subarray` ALIAS of this
    // array; if anything in the chain ever wrote through it, this is the test that catches it.
    expect(geometry.getAttribute('position')).toBe(position);
    expect(position.array).toEqual(positionBefore);
    expect(normal.array).toEqual(normalBefore);
    expect(uv.array).toEqual(uvBefore);
  });

  it('leaves skin weights byte-identical', async () => {
    const geometry = grid(30);
    skin(geometry, 4, 500);
    const skinIndex = geometry.getAttribute('skinIndex') as THREE.BufferAttribute;
    const skinWeight = geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
    const indexBefore = snapshot(skinIndex.array as Uint16Array);
    const weightBefore = snapshot(skinWeight.array as Float32Array);

    await decimate(geometry, { ratio: 0.2 });

    expect(skinIndex.array).toEqual(indexBefore);
    expect(skinWeight.array).toEqual(weightBefore);
  });

  /**
   * ⚠️ The honesty test. A tight error budget makes the requested ratio unreachable, and the report
   * has to say so rather than hand back 91% of the mesh alongside a cheerful "done".
   */
  it('admits it when the requested ratio could not be reached', async () => {
    const geometry = grid(40);
    const report = await decimate(geometry, { ratio: 0.1, targetError: 1e-6 });

    expect(report.metTarget).toBe(false);
    expect(report.outputTriangles).toBeGreaterThan(report.requestedTriangles);
    expect(report.achievedRatio).toBeGreaterThan(0.1);
    expect(report.achievedRatio).toBeCloseTo(report.outputTriangles / report.inputTriangles, 10);
    expect(report.error).toBeLessThanOrEqual(1e-6);
  });

  /** "Nothing happened" is a distinct outcome from "missed the target", and reads differently. */
  it('flags a decimation that removed nothing at all', async () => {
    const geometry = grid(20);
    const report = await decimate(geometry, { ratio: 0.5, targetError: 0 });

    expect(report.unchanged).toBe(true);
    expect(report.metTarget).toBe(false);
    expect(report.achievedRatio).toBe(1);
  });

  it('reports the error in mesh units as well as relative units', async () => {
    const geometry = grid(30);
    const report = await decimate(geometry, { ratio: 0.25 });

    expect(report.error).toBeGreaterThan(0);
    expect(report.errorAbsolute).toBeGreaterThan(0);
    // The grid spans [-1..1], so one relative unit is 2 mesh units.
    expect(report.errorAbsolute).toBeCloseTo(report.error * 2, 6);
  });

  it('uses the attribute-aware path when normals and uvs are there, and says so', async () => {
    const geometry = grid(20);
    expect((await decimate(geometry, { ratio: 0.4 })).attributesUsed).toEqual(['normal', 'uv']);
    expect(
      (await decimate(geometry, { ratio: 0.4, attributeAware: false })).attributesUsed
    ).toEqual([]);
  });

  it('falls back to plain simplify when there is nothing to weight by', async () => {
    const geometry = grid(20);
    geometry.deleteAttribute('normal');
    geometry.deleteAttribute('uv');
    const report = await decimate(geometry, { ratio: 0.4 });
    expect(report.attributesUsed).toEqual([]);
    expect(report.outputTriangles).toBeLessThan(report.inputTriangles);
  });

  /** ⚠️ A `<input type="number">` left empty yields NaN, which must not reach meshopt's assert. */
  it('rejects a nonsense ratio or error budget with a readable message', async () => {
    const geometry = grid(10);
    await expect(decimate(geometry, { ratio: NaN })).rejects.toThrow(/ratio must be between/i);
    await expect(decimate(geometry, { ratio: 2 })).rejects.toThrow(/ratio must be between/i);
    await expect(decimate(geometry, { targetError: NaN })).rejects.toThrow(/error budget/i);
    await expect(decimate(geometry, { targetTriangles: -1 })).rejects.toThrow(/0 or more/i);
    // None of these is meshoptimizer's bare `Error('Assertion failed')`.
    await expect(decimate(geometry, { ratio: NaN })).rejects.not.toThrow(/Assertion failed/);
  });

  it('rejects an index buffer that points past the vertex array', async () => {
    const geometry = grid(10);
    const index = geometry.getIndex()!.array as Uint32Array;
    index[0] = 99999;
    await expect(decimate(geometry, { ratio: 0.5 })).rejects.toThrow(/only has \d+/);
  });

  it('accepts an absolute triangle target', async () => {
    const geometry = grid(30);
    const report = await decimate(geometry, { targetTriangles: 400 });
    expect(report.requestedTriangles).toBe(400);
    expect(report.outputTriangles).toBeLessThanOrEqual(400);
  });

  /** Error-driven mode: no triangle target, so there is no target to miss. */
  it('treats ratio 0 as "as far as the error allows"', async () => {
    const geometry = grid(30);
    const report = await decimate(geometry, { ratio: 0, targetError: 0.02 });
    expect(report.requestedTriangles).toBe(0);
    expect(report.metTarget).toBe(true);
    expect(report.outputTriangles).toBeGreaterThan(0);
    expect(report.outputTriangles).toBeLessThan(report.inputTriangles);
  });

  it('takes well under a second on a mesh the size of a delivered character', async () => {
    // ~180k triangles. The measured budget is 82 ms attribute-aware; a second is the bar for "does
    // this need the chunking machinery", not a performance assertion, so it is set far above the
    // measurement and will only fire on a real regression.
    const geometry = grid(300);
    const report = await decimate(geometry, { ratio: 0.25 });
    expect(report.outputTriangles).toBeLessThan(report.inputTriangles);
    expect(report.elapsedMs).toBeLessThan(1000);
  }, 20000);
});

describe('starvedJoints', () => {
  it('names the joint a decimation left driving nothing', async () => {
    const geometry = grid(30);
    // Vertex 500 is interior; its private joint is the one with only that vertex to hold on to.
    skin(geometry, 4, 500);
    const before = geometry.getIndex()!.array as Uint32Array;

    // Hard enough that an interior vertex with no special claim on the surface is collapsed away.
    const report = await decimate(geometry, { ratio: 0.05 });

    expect(report.rig.checked).toBe(true);
    expect(report.rig.joints).toBe(5);
    // Either it survived or it starved, but the report must agree with the index buffer either way.
    const survivors = new Set(Array.from(report.index));
    const lonelySurvived = survivors.has(500);
    expect(report.rig.starved.includes(4)).toBe(!lonelySurvived);

    // And the direct call agrees with what `decimate` embedded.
    expect(starvedJoints(geometry, before, report.index).starved).toEqual(report.rig.starved);
  });

  it('reports nothing starved when the edit keeps every joint', () => {
    const geometry = grid(10);
    skin(geometry, 4, 5);
    const index = geometry.getIndex()!.array as Uint32Array;
    const result = starvedJoints(geometry, index, index);
    expect(result.checked).toBe(true);
    expect(result.starved).toEqual([]);
  });

  it('catches a joint starved by shell deletion, not just decimation', () => {
    const geometry = grid(10);
    skin(geometry, 4, 5);
    const index = geometry.getIndex()!.array as Uint32Array;
    // Keep only the first quarter of the triangles — the later bands lose every vertex.
    const kept = index.slice(0, Math.floor(index.length / 4 / 3) * 3);
    const result = starvedJoints(geometry, index, kept);
    expect(result.starved.length).toBeGreaterThan(0);
    expect(result.starved).toContain(3);
  });

  /**
   * 🔴 A GLB pads unused influence slots with joint 0 at weight 0. Counting joint ids without
   * testing the weight would report joint 0 as driving every vertex — and every other joint as
   * starved the instant anything is deleted.
   */
  it('does not count a zero-weight influence as an influence', () => {
    const geometry = grid(6);
    const count = geometry.getAttribute('position').count;
    const skinIndex = new Uint16Array(count * 4);
    const skinWeight = new Float32Array(count * 4);
    for (let v = 0; v < count; v++) {
      skinIndex[v * 4] = 1; // everything is really driven by joint 1
      skinWeight[v * 4] = 1;
      skinIndex[v * 4 + 1] = 0; // padding: joint 0 at weight 0
    }
    geometry.setAttribute('skinIndex', new THREE.BufferAttribute(skinIndex, 4));
    geometry.setAttribute('skinWeight', new THREE.BufferAttribute(skinWeight, 4));

    const index = geometry.getIndex()!.array as Uint32Array;
    const result = starvedJoints(geometry, index, index.slice(0, 3));
    expect(result.joints).toBe(2);
    // Joint 0 never drove anything, so losing it is not news.
    expect(result.starved).toEqual([]);
  });

  it('says it did not check rather than guessing on an unskinned mesh', () => {
    const geometry = grid(6);
    const index = geometry.getIndex()!.array as Uint32Array;
    expect(starvedJoints(geometry, index, index)).toEqual({
      starved: [],
      joints: 0,
      checked: false
    });
  });

  /** glTF forbids a normalized JOINTS_0; `getComponent` would denormalize it to nonsense. */
  it('refuses a normalized joint attribute instead of reporting garbage', () => {
    const geometry = grid(6);
    skin(geometry, 4, 5);
    const skinIndex = geometry.getAttribute('skinIndex') as THREE.BufferAttribute;
    geometry.setAttribute(
      'skinIndex',
      new THREE.BufferAttribute(skinIndex.array as Uint16Array, 4, true)
    );
    const index = geometry.getIndex()!.array as Uint32Array;
    expect(starvedJoints(geometry, index, index).checked).toBe(false);
  });
});
