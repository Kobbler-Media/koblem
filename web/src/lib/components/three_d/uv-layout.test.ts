/*
 * The UV-layout half of `texture-repair.ts`: what the atlas panel reads before it draws anything.
 *
 * 🔴 WHAT IS NOT TESTED HERE, AND WHY. `buildCoverageCountRect` needs a `WebGLRenderer`, a real
 * GL context and a render target, none of which exist under vitest's node environment. Mocking one
 * would test the mock: the questions that pass answers — which texels a fill rule actually reaches,
 * and whether two triangles land on the same one — are exactly the questions a fake rasteriser
 * cannot answer. It stays verified by eye against a real asset. `summariseCoverage` IS tested,
 * because it is a pure reduction of that pass's bytes and carries the arithmetic the UI shows.
 */
import * as THREE from 'three';
import { describe, expect, it } from 'vitest';

import {
  analyseUvLayout,
  buildUvHitGrid,
  countSubTexelFaces,
  islandFaces,
  summariseCoverage,
  uvHitFace,
  uvIslandOutline
} from './texture-repair';

/** A geometry carrying nothing but UVs and an index — everything under test reads only those. */
function geometryFromUvs(uvs: number[], index: number[] | null = null): THREE.BufferGeometry {
  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute('uv', new THREE.BufferAttribute(new Float32Array(uvs), 2));
  // A position attribute is not read by any of this, but a geometry without one is not a thing the
  // rest of the app can produce, so the fixtures carry a matching one.
  const positions = new Float32Array((uvs.length / 2) * 3);
  geometry.setAttribute('position', new THREE.BufferAttribute(positions, 3));
  if (index) geometry.setIndex(index);
  return geometry;
}

/** One axis-aligned quad as two triangles, sharing its two middle vertices. */
function quad(x: number, y: number, size: number): { uvs: number[]; index: number[] } {
  return {
    uvs: [x, y, x + size, y, x + size, y + size, x, y + size],
    index: [0, 1, 2, 0, 2, 3]
  };
}

describe('analyseUvLayout', () => {
  it('reads one quad as one island with its real area and bounds', () => {
    const q = quad(0.25, 0.25, 0.5);
    const layout = analyseUvLayout(geometryFromUvs(q.uvs, q.index));

    expect(layout.faceCount).toBe(2);
    expect(layout.islands).toHaveLength(1);
    expect(layout.islands[0].faces).toBe(2);
    expect(layout.islands[0].vertices).toBe(4);
    expect(layout.islands[0].area).toBeCloseTo(0.25, 6);
    expect(layout.uvArea).toBeCloseTo(0.25, 6);
    expect(layout.minU).toBeCloseTo(0.25, 6);
    expect(layout.maxU).toBeCloseTo(0.75, 6);
    expect(layout.outsideFaces).toBe(0);
    expect(layout.degenerateCount).toBe(0);
    expect(Array.from(layout.faceIsland)).toEqual([0, 0]);
  });

  it('separates two charts and orders them biggest first', () => {
    const big = quad(0.05, 0.05, 0.4);
    const small = quad(0.6, 0.6, 0.2);
    const uvs = [...big.uvs, ...small.uvs];
    const index = [...big.index, ...small.index.map((i) => i + 4)];
    const layout = analyseUvLayout(geometryFromUvs(uvs, index));

    expect(layout.islands).toHaveLength(2);
    expect(layout.islands[0].area).toBeGreaterThan(layout.islands[1].area);
    expect(layout.islands[0].area).toBeCloseTo(0.16, 6);
    expect(layout.islands[1].area).toBeCloseTo(0.04, 6);
    // `id` is the position after the sort, and `faceIsland` agrees with it — the remap is the part
    // that is easy to get wrong and would light up the wrong island on the model.
    expect(layout.islands.map((i) => i.id)).toEqual([0, 1]);
    expect(Array.from(layout.faceIsland)).toEqual([0, 0, 1, 1]);
    expect(layout.islands[0].face).toBe(0);
    expect(layout.islands[1].face).toBe(2);
  });

  /**
   * 🔴 The case the whole weld exists for. These GLBs are seam-split — the exporter duplicates a
   * vertex at every normal crease — so two triangles of ONE chart can share no index value. Without
   * the weld this reports two islands and the panel claims the character is shredded.
   */
  it('welds vertices that were split at a crease back into one island', () => {
    // The same quad, but the diagonal's two vertices are duplicated: 6 vertices, 0 shared indices.
    const uvs = [
      0.2, 0.2, 0.8, 0.2, 0.8, 0.8, /* duplicates of (0.2,0.2) and (0.8,0.8): */ 0.2, 0.2, 0.8, 0.8,
      0.2, 0.8
    ];
    const index = [0, 1, 2, 3, 4, 5];
    const layout = analyseUvLayout(geometryFromUvs(uvs, index));

    expect(layout.islands).toHaveLength(1);
    expect(layout.islands[0].faces).toBe(2);
    // Four distinct UV points, not six vertices.
    expect(layout.islands[0].vertices).toBe(4);
  });

  it('does not weld two charts that merely sit close together', () => {
    const a = quad(0.1, 0.1, 0.2);
    const b = quad(0.3001, 0.1, 0.2);
    const layout = analyseUvLayout(
      geometryFromUvs([...a.uvs, ...b.uvs], [...a.index, ...b.index.map((i) => i + 4)])
    );
    expect(layout.islands).toHaveLength(2);
  });

  /**
   * A zero-area UV triangle samples ONE texel for a whole face — a real defect in a projected
   * atlas, and invisible in the 3D view until you notice a flat-coloured polygon.
   */
  it('finds zero-area UV triangles and attributes them to their island', () => {
    const q = quad(0.1, 0.1, 0.3);
    // A third triangle whose three corners are the same point.
    const uvs = [...q.uvs, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2];
    const index = [...q.index, 4, 5, 6];
    const layout = analyseUvLayout(geometryFromUvs(uvs, index));

    expect(layout.degenerateCount).toBe(1);
    expect(layout.degenerateFaces).toEqual([2]);
    expect(layout.faceArea[2]).toBe(0);
    // It sits inside the quad's UV footprint but shares no vertex with it, so it is its own chart.
    const owner = layout.islands[layout.faceIsland[2]];
    expect(owner.degenerate).toBe(1);
  });

  it('caps the listed degenerates without lying about how many there are', () => {
    const uvs: number[] = [];
    const index: number[] = [];
    for (let i = 0; i < 5; i++) {
      const base = uvs.length / 2;
      uvs.push(0.1 * i, 0.5, 0.1 * i, 0.5, 0.1 * i, 0.5);
      index.push(base, base + 1, base + 2);
    }
    const layout = analyseUvLayout(geometryFromUvs(uvs, index), { maxDegenerateListed: 2 });
    expect(layout.degenerateCount).toBe(5);
    expect(layout.degenerateFaces).toHaveLength(2);
  });

  it('flags UVs that run outside the atlas, per face and per island', () => {
    const inside = quad(0.1, 0.1, 0.2);
    const outside = quad(0.9, 0.9, 0.4);
    const layout = analyseUvLayout(
      geometryFromUvs(
        [...inside.uvs, ...outside.uvs],
        [...inside.index, ...outside.index.map((i) => i + 4)]
      )
    );

    expect(layout.outsideFaces).toBe(2);
    expect(layout.maxU).toBeCloseTo(1.3, 6);
    const strayed = layout.islands.find((i) => i.outside);
    const kept = layout.islands.find((i) => !i.outside);
    expect(strayed).toBeDefined();
    expect(kept).toBeDefined();
    expect(strayed!.faces).toBe(2);
  });

  it('reads a non-indexed geometry through its implicit index', () => {
    const layout = analyseUvLayout(geometryFromUvs([0, 0, 1, 0, 1, 1]));
    expect(layout.faceCount).toBe(1);
    expect(layout.islands).toHaveLength(1);
    expect(layout.islands[0].area).toBeCloseTo(0.5, 6);
  });

  it('refuses a mesh with no UVs in words a user can act on', () => {
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute('position', new THREE.BufferAttribute(new Float32Array(9), 3));
    expect(() => analyseUvLayout(geometry)).toThrow(/no UV coordinates/);
  });
});

describe('countSubTexelFaces', () => {
  /** Not a defect on its own — it is how you tell that the atlas is too small for the geometry. */
  it('counts the faces that land inside a single texel of a given map', () => {
    const big = quad(0.1, 0.1, 0.5);
    // A triangle roughly 1/2048 of the atlas across: sub-texel on a 64² map, not on a 4096² one.
    const tiny = [0.9, 0.9, 0.9005, 0.9, 0.9005, 0.9005];
    const layout = analyseUvLayout(geometryFromUvs([...big.uvs, ...tiny], [...big.index, 4, 5, 6]));
    expect(countSubTexelFaces(layout, 64, 64)).toBe(1);
    expect(countSubTexelFaces(layout, 4096, 4096)).toBe(0);
  });
});

describe('islandFaces', () => {
  it('collects an island into the face set the 3D view highlights', () => {
    const a = quad(0.05, 0.05, 0.3);
    const b = quad(0.5, 0.5, 0.2);
    const layout = analyseUvLayout(
      geometryFromUvs([...a.uvs, ...b.uvs], [...a.index, ...b.index.map((i) => i + 4)])
    );
    expect(islandFaces(layout, 1)).toEqual(new Set([2, 3]));
    expect(islandFaces(layout, -1).size).toBe(0);
    expect(islandFaces(layout, 99).size).toBe(0);
  });
});

describe('uvIslandOutline', () => {
  it('keeps the border of a chart and drops the edges inside it', () => {
    const q = quad(0.25, 0.25, 0.5);
    const geometry = geometryFromUvs(q.uvs, q.index);
    const outline = uvIslandOutline(geometry, analyseUvLayout(geometry));

    // Four sides survive; the diagonal is shared by both triangles and does not.
    expect(outline.count).toBe(4);
    expect(outline.segments).toHaveLength(16);
    expect(Array.from(outline.island)).toEqual([0, 0, 0, 0]);
    for (let i = 0; i < outline.segments.length; i++) {
      expect(outline.segments[i]).toBeGreaterThanOrEqual(0.25);
      expect(outline.segments[i]).toBeLessThanOrEqual(0.75);
    }
  });

  /** The seam-split quad must outline as ONE quad, not as two triangles — that is the weld again. */
  it('sees through a crease split when it decides what is a border', () => {
    const uvs = [0.2, 0.2, 0.8, 0.2, 0.8, 0.8, 0.2, 0.2, 0.8, 0.8, 0.2, 0.8];
    const geometry = geometryFromUvs(uvs, [0, 1, 2, 3, 4, 5]);
    const outline = uvIslandOutline(geometry, analyseUvLayout(geometry));
    expect(outline.count).toBe(4);
  });

  it('labels each border edge with the island it belongs to', () => {
    const a = quad(0.05, 0.05, 0.3);
    const b = quad(0.5, 0.5, 0.2);
    const geometry = geometryFromUvs(
      [...a.uvs, ...b.uvs],
      [...a.index, ...b.index.map((i) => i + 4)]
    );
    const outline = uvIslandOutline(geometry, analyseUvLayout(geometry));
    expect(outline.count).toBe(8);
    const perIsland = [0, 0];
    for (let i = 0; i < outline.count; i++) perIsland[outline.island[i]]++;
    expect(perIsland).toEqual([4, 4]);
  });
});

describe('buildUvHitGrid / uvHitFace', () => {
  it('finds the triangle under a point and misses the gutter', () => {
    const q = quad(0.25, 0.25, 0.5);
    const geometry = geometryFromUvs(q.uvs, q.index);
    const grid = buildUvHitGrid(geometry);

    // Below the diagonal is face 0, above it is face 1.
    expect(uvHitFace(grid, 0.7, 0.3)).toBe(0);
    expect(uvHitFace(grid, 0.3, 0.7)).toBe(1);
    // Outside the chart entirely.
    expect(uvHitFace(grid, 0.1, 0.1)).toBe(-1);
    expect(uvHitFace(grid, 0.95, 0.95)).toBe(-1);
  });

  it('resolves a hover to the island the layout named', () => {
    const a = quad(0.05, 0.05, 0.3);
    const b = quad(0.5, 0.5, 0.2);
    const geometry = geometryFromUvs(
      [...a.uvs, ...b.uvs],
      [...a.index, ...b.index.map((i) => i + 4)]
    );
    const layout = analyseUvLayout(geometry);
    const grid = buildUvHitGrid(geometry);
    const face = uvHitFace(grid, 0.6, 0.55);
    expect(face).toBeGreaterThanOrEqual(2);
    expect(layout.faceIsland[face]).toBe(1);
  });

  /** A triangle with no area covers nothing, so pointing at its corner must not select it. */
  it('never returns a degenerate triangle', () => {
    const geometry = geometryFromUvs([0.5, 0.5, 0.5, 0.5, 0.5, 0.5]);
    const grid = buildUvHitGrid(geometry);
    expect(uvHitFace(grid, 0.5, 0.5)).toBe(-1);
  });

  /** A UV that ran off the atlas is still hittable — the bucket is clamped, the test is not. */
  it('still hits a triangle whose UVs run outside [0,1]', () => {
    const geometry = geometryFromUvs([0.8, 0.5, 1.6, 0.5, 1.6, 0.9]);
    const grid = buildUvHitGrid(geometry);
    expect(uvHitFace(grid, 1.4, 0.6)).toBe(0);
  });

  /** A triangle spanning the whole grid is not bucketed into 65k cells; it is checked every time. */
  it('keeps a grid-spanning triangle out of the buckets and still finds it', () => {
    const q = quad(0.4, 0.4, 0.05);
    const uvs = [...q.uvs, 0, 0, 1, 0, 0, 1];
    const geometry = geometryFromUvs(uvs, [...q.index, 4, 5, 6]);
    const grid = buildUvHitGrid(geometry, { cells: 32 });
    expect(Array.from(grid.oversize)).toEqual([2]);
    expect(uvHitFace(grid, 0.1, 0.1)).toBe(2);
    // The small chart still wins inside its own cell, because buckets are searched first.
    expect(uvHitFace(grid, 0.43, 0.42)).toBe(0);
  });
});

describe('summariseCoverage', () => {
  it('splits a count buffer into covered, overlapping and the worst texel', () => {
    const counts = Uint8Array.from([0, 1, 1, 2, 0, 3, 0, 0]);
    expect(summariseCoverage(counts)).toEqual({
      total: 8,
      covered: 4,
      overlapping: 2,
      maxCount: 3
    });
  });

  it('reports an empty pass rather than pretending it measured something', () => {
    expect(summariseCoverage(new Uint8Array(16))).toEqual({
      total: 16,
      covered: 0,
      overlapping: 0,
      maxCount: 0
    });
  });
});
