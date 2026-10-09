import { describe, expect, it } from 'vitest';
import * as THREE from 'three';
import {
  analyseSkin,
  applyPrune,
  boneVertexMask,
  coreCandidates,
  coreCoverage,
  previewPrune,
  sanitizeName
} from './stage';

/** `[boneIndex, weight]` per occupied slot, in slot order. */
type Influence = [number, number];

interface VertexSpec {
  at: [number, number, number];
  influences: Influence[];
}

/**
 * A synthetic rig in ONE space: bones are direct children of the root at the given world positions
 * and the mesh binds with an identity `bindMatrix`, so a vertex's rest position is literally the
 * number written into the position attribute. That is what makes the distance ratios below readable
 * by hand.
 */
function rig(
  boneWorld: [number, number, number][],
  vertices: VertexSpec[],
  boneNames?: string[]
): { root: THREE.Object3D; mesh: THREE.SkinnedMesh; bones: THREE.Bone[] } {
  const root = new THREE.Object3D();
  const bones = boneWorld.map((p, i) => {
    const bone = new THREE.Bone();
    bone.name = boneNames?.[i] ?? `bone${i}`;
    bone.position.set(p[0], p[1], p[2]);
    root.add(bone);
    return bone;
  });

  const position = new Float32Array(vertices.length * 3);
  const skinIndex = new Uint16Array(vertices.length * 4);
  const skinWeight = new Float32Array(vertices.length * 4);
  vertices.forEach((v, i) => {
    position.set(v.at, i * 3);
    v.influences.forEach(([bone, weight], slot) => {
      skinIndex[i * 4 + slot] = bone;
      skinWeight[i * 4 + slot] = weight;
    });
  });

  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute('position', new THREE.BufferAttribute(position, 3));
  geometry.setAttribute('skinIndex', new THREE.BufferAttribute(skinIndex, 4));
  geometry.setAttribute('skinWeight', new THREE.BufferAttribute(skinWeight, 4));

  const mesh = new THREE.SkinnedMesh(geometry, new THREE.MeshStandardMaterial());
  root.add(mesh);
  root.updateMatrixWorld(true);
  mesh.bind(new THREE.Skeleton(bones), new THREE.Matrix4());
  return { root, mesh, bones };
}

/** A mesh that has geometry but no skin attributes — the null hole `analyseSkin` must keep. */
function unskinnedMesh(vertexCount: number, skeleton?: THREE.Skeleton): THREE.SkinnedMesh {
  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute(
    'position',
    new THREE.BufferAttribute(new Float32Array(vertexCount * 3), 3)
  );
  const mesh = new THREE.SkinnedMesh(geometry, new THREE.MeshStandardMaterial());
  if (skeleton) mesh.bind(skeleton, new THREE.Matrix4());
  return mesh;
}

function weightsOf(mesh: THREE.SkinnedMesh, vertex: number): number[] {
  const a = mesh.geometry.getAttribute('skinWeight');
  return [a.getX(vertex), a.getY(vertex), a.getZ(vertex), a.getW(vertex)];
}

describe('sanitizeName', () => {
  /**
   * 🔴 The whole naming path rests on this. `GLTFLoader` runs every node name through
   * `PropertyBinding.sanitizeNodeName`, which DELETES `[ ] . : /` rather than escaping them —
   * they are the animation track-path grammar. So the file's `mixamorig:LeftArm` is
   * `mixamorigLeftArm` on a `THREE.Bone`, and comparing `MIXAMO_CORE` against `bone.name` directly
   * reports every core slot missing on every rig this pipeline has ever produced.
   */
  it('deletes the five track-path characters instead of escaping them', () => {
    expect(sanitizeName('mixamorig:LeftArm')).toBe('mixamorigLeftArm');
    expect(sanitizeName('Bone.001')).toBe('Bone001');
    expect(sanitizeName('Armature[0]')).toBe('Armature0');
    expect(sanitizeName('root/hips')).toBe('roothips');
    expect(sanitizeName('a[b].c:d/e')).toBe('abcde');
  });

  it('replaces whitespace with an underscore rather than deleting it', () => {
    expect(sanitizeName('Left Arm')).toBe('Left_Arm');
    expect(sanitizeName('Left\tArm')).toBe('Left_Arm');
    expect(sanitizeName('mixamorig:Left Fore Arm')).toBe('mixamorigLeft_Fore_Arm');
  });

  it('is idempotent, so a name may be pushed through it twice safely', () => {
    const once = sanitizeName('mixamorig:Left Arm.001');
    expect(sanitizeName(once)).toBe(once);
  });

  it('leaves an already-clean name and an empty one alone', () => {
    expect(sanitizeName('mixamorigHips')).toBe('mixamorigHips');
    expect(sanitizeName('')).toBe('');
  });
});

describe('coreCoverage', () => {
  /** 🔴 Matched in THREE-space, reported in CANONICAL space. */
  it('matches through sanitizeName and reports colon-bearing names', () => {
    const bones = ['mixamorigHips', 'mixamorigSpine', 'someBlob'].map((name) => {
      const bone = new THREE.Bone();
      bone.name = name;
      return bone;
    });
    const { present, missing, unnamed } = coreCoverage(bones);
    expect(present).toEqual(['mixamorig:Hips', 'mixamorig:Spine']);
    expect(missing).toContain('mixamorig:Head');
    expect(missing).not.toContain('mixamorig:Hips');
    expect(present.length + missing.length).toBe(22);
    expect(unnamed.map((b) => b.name)).toEqual(['someBlob']);
  });

  it('reports all 22 missing for a skeleton carrying the canonical names verbatim', () => {
    // The failure mode the sanitize rule exists for: colons in `bone.name` never survive the loader,
    // so a rig that somehow kept them matches nothing.
    const bone = new THREE.Bone();
    bone.name = 'mixamorig:Hips';
    const { present, unnamed } = coreCoverage([bone]);
    expect(present).toEqual([]);
    expect(unnamed).toHaveLength(1);
  });
});

describe('coreCandidates', () => {
  /**
   * Ranked by SKELETON TOPOLOGY, not by name or by proximity: a joint with no core name has nothing
   * to match on, but the joints around it do. A skeleton missing only `LeftForeArm` still has
   * `LeftArm`, and the elbow is the thing hanging directly off it.
   */
  function armRig(): THREE.Bone[] {
    const named = (name: string): THREE.Bone => {
      const bone = new THREE.Bone();
      bone.name = name;
      return bone;
    };
    const hips = named('mixamorigHips');
    const shoulder = named('mixamorigLeftShoulder');
    const arm = named('mixamorigLeftArm');
    const elbow = named('joint_47');
    const wrist = named('joint_48');
    const fingertip = named('joint_49');
    const stray = named('joint_02');

    hips.add(shoulder);
    shoulder.add(arm);
    arm.add(elbow);
    elbow.add(wrist);
    wrist.add(fingertip);
    hips.add(stray);
    hips.updateMatrixWorld(true);

    // ⚠️ The arbitrary joint deliberately comes FIRST in skeleton order — if topology did not beat
    // file order the dropdown would open on it.
    return [stray, hips, shoulder, arm, elbow, wrist, fingertip];
  }

  it('puts the direct child of the correctly-named parent first, ahead of file order', () => {
    const names = coreCandidates(armRig(), 'mixamorig:LeftForeArm').map((b) => b.name);
    expect(names[0]).toBe('joint_47');
  });

  it('then deeper descendants nearest-first, then everything else in skeleton order', () => {
    const names = coreCandidates(armRig(), 'mixamorig:LeftForeArm').map((b) => b.name);
    expect(names).toEqual(['joint_47', 'joint_48', 'joint_49', 'joint_02']);
  });

  /** Bones already holding a core name are spoken for; offering one would take it from its clip. */
  it('never offers a bone that already fills a core slot', () => {
    const names = coreCandidates(armRig(), 'mixamorig:LeftForeArm').map((b) => b.name);
    expect(names).not.toContain('mixamorigLeftArm');
    expect(names).not.toContain('mixamorigHips');
    expect(names).not.toContain('mixamorigLeftShoulder');
  });

  /** ⚠️ The root has no anatomical parent, so there is no topology left to reason from. */
  it('falls back to skeleton order for Hips, where no parent exists', () => {
    const names = coreCandidates(armRig(), 'mixamorig:Hips').map((b) => b.name);
    expect(names).toEqual(['joint_02', 'joint_47', 'joint_48', 'joint_49']);
  });

  /** The anchor is missing too, so rank 1 and 2 are both empty — file order is all that is left. */
  it('falls back to skeleton order when the canonical parent is itself absent', () => {
    const bones = ['joint_a', 'joint_b'].map((name) => {
      const bone = new THREE.Bone();
      bone.name = name;
      return bone;
    });
    expect(coreCandidates(bones, 'mixamorig:LeftForeArm').map((b) => b.name)).toEqual([
      'joint_a',
      'joint_b'
    ]);
  });

  it('accepts a core name in either spelling, since callers hold both', () => {
    const canonical = coreCandidates(armRig(), 'mixamorig:LeftForeArm').map((b) => b.name);
    const sanitized = coreCandidates(armRig(), 'mixamorigLeftForeArm').map((b) => b.name);
    expect(sanitized).toEqual(canonical);
  });
});

describe('analyseSkin', () => {
  /** Bone 1 sits 100 units away; the mesh is 1 unit wide, so its influence reaches ratio 9. */
  const leakRig = () =>
    rig(
      [
        [0, 0, 0],
        [10, 0, 0]
      ],
      [
        { at: [0, 0, 0], influences: [[0, 1]] },
        { at: [1, 0, 0], influences: [[1, 1]] }
      ]
    );

  it('measures reach as a fraction of the model, so a threshold means one thing at any scale', () => {
    const { mesh } = leakRig();
    const report = analyseSkin([mesh]);
    expect(report.referenceLength).toBe(1);
    expect(report.vertices).toBe(2);
    expect(report.influences).toBe(2);
    expect(report.maxRatio).toBeCloseTo(9, 6);
  });

  it('ignores zero-weight slots rather than charging them to bone 0', () => {
    const { mesh } = rig(
      [
        [0, 0, 0],
        [10, 0, 0]
      ],
      [{ at: [0, 0, 0], influences: [[0, 1]] }]
    );
    // The three unused slots default to bone 0 at weight 0 and must not be counted.
    expect(analyseSkin([mesh]).influences).toBe(1);
  });

  it('builds the tail as a distribution, counting influences and vertices apart', () => {
    const { mesh } = leakRig();
    const { tail } = analyseSkin([mesh]);
    expect(tail.map((t) => t.ratio)).toEqual([0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.4, 0.5, 0.75, 1]);
    // One influence at ratio 9 crosses every threshold; the vertex is counted once per threshold.
    expect(tail.every((t) => t.influences === 1 && t.vertices === 1)).toBe(true);
  });

  it('ranks the worst bone first and names it', () => {
    const { mesh } = leakRig();
    const { bones } = analyseSkin([mesh]);
    expect(bones[0]).toMatchObject({ index: 1, name: 'bone1', influences: 1 });
    expect(bones[0].maxRatio).toBeCloseTo(9, 6);
    expect(bones[1].maxRatio).toBe(0);
  });

  /**
   * 🔴 `vertexRatios` is indexed by the caller's OWN mesh subscript — the overlay paints
   * `vertexRatios[m]` onto `meshes[m]`. A mesh with no skin attributes must therefore still occupy
   * its slot as a zeroed array of the right length. Compacting the holes away would shift every
   * later mesh's ratios onto the wrong geometry, and nothing downstream checks the length.
   */
  it('keeps a slot for every input mesh, including one with no skin attributes', () => {
    const { mesh, bones } = leakRig();
    const bald = unskinnedMesh(5, new THREE.Skeleton(bones));
    const report = analyseSkin([bald, mesh]);

    expect(report.vertexRatios).toHaveLength(2);
    expect(report.vertexRatios[0]).toHaveLength(5);
    expect([...report.vertexRatios[0]]).toEqual([0, 0, 0, 0, 0]);
    // The leak is still on the mesh that actually has it, at its own vertex index.
    expect(report.vertexRatios[1]).toHaveLength(2);
    expect(report.vertexRatios[1][0]).toBe(0);
    expect(report.vertexRatios[1][1]).toBeCloseTo(9, 5);
    // Only the skinned mesh contributed to the measurement.
    expect(report.vertices).toBe(2);
  });

  it('keeps the slots when the unskinned mesh comes last, too', () => {
    const { mesh, bones } = leakRig();
    const report = analyseSkin([mesh, unskinnedMesh(3, new THREE.Skeleton(bones))]);
    expect(report.vertexRatios.map((r) => r.length)).toEqual([2, 3]);
    expect(report.vertexRatios[0][1]).toBeCloseTo(9, 5);
  });

  it('returns an empty report — with the slots intact — when nothing is skinned at all', () => {
    const report = analyseSkin([unskinnedMesh(4)]);
    expect(report).toMatchObject({ referenceLength: 0, vertices: 0, influences: 0, maxRatio: 0 });
    expect(report.tail).toEqual([]);
    expect(report.bones).toEqual([]);
    expect(report.vertexRatios.map((r) => r.length)).toEqual([4]);
  });

  it('has nothing to say about no meshes at all', () => {
    expect(analyseSkin([]).vertexRatios).toEqual([]);
  });
});

describe('boneVertexMask', () => {
  const maskRig = () =>
    rig(
      [
        [0, 0, 0],
        [1, 0, 0]
      ],
      [
        { at: [0, 0, 0], influences: [[0, 1]] },
        {
          at: [1, 0, 0],
          influences: [
            [0, 0.9],
            [1, 0.1]
          ]
        },
        {
          at: [2, 0, 0],
          influences: [
            [0, 0.5],
            [1, 0.5]
          ]
        }
      ]
    );

  /** The default is a READABILITY threshold: every vertex near a joint carries a trace of its
   *  neighbours, so a mask at weight > 0 lights up half the torso for a wrist. */
  it('lights only the vertices the bone actually drives, at the default threshold', () => {
    const [mask] = boneVertexMask([maskRig().mesh], 1);
    expect([...mask]).toEqual([0, 0, 1]);
  });

  it('honours a lower threshold, which is what makes the default a choice and not a rule', () => {
    const { mesh } = maskRig();
    expect([...boneVertexMask([mesh], 1, 0.05)[0]]).toEqual([0, 1, 1]);
  });

  it('finds the bone in any of the four slots', () => {
    const { mesh } = rig(
      [
        [0, 0, 0],
        [0, 0, 0],
        [0, 0, 0],
        [0, 0, 0],
        [0, 0, 0]
      ],
      [
        {
          at: [0, 0, 0],
          influences: [
            [1, 0.25],
            [2, 0.25],
            [3, 0.25],
            [4, 0.25]
          ]
        }
      ]
    );
    for (const bone of [1, 2, 3, 4]) {
      expect([...boneVertexMask([mesh], bone)[0]]).toEqual([1]);
    }
    expect([...boneVertexMask([mesh], 0)[0]]).toEqual([0]);
  });

  it('returns a zeroed mask of the right length for a mesh with no skin attributes', () => {
    const masks = boneVertexMask([unskinnedMesh(7), maskRig().mesh], 1);
    expect(masks).toHaveLength(2);
    expect(masks[0]).toHaveLength(7);
    expect([...masks[0]]).toEqual([0, 0, 0, 0, 0, 0, 0]);
    expect(masks[1]).toHaveLength(3);
  });
});

describe('applyPrune', () => {
  /** Bone 1 is 100 units from a 1-unit-wide mesh, so any sane threshold reaches it. */
  const farRig = (vertices: VertexSpec[]) =>
    rig(
      [
        [0, 0, 0],
        [100, 0, 0]
      ],
      vertices
    );

  /**
   * 🔴 A vertex whose influences would ALL be pruned is left exactly as it was. Zeroing it would
   * leave it with no binding at all, and an unskinned vertex deforms nothing — the one failure mode
   * the engine itself refuses to ship. It is also why the per-bone tally has to be un-charged: the
   * report would otherwise claim a prune that never happened.
   */
  it('spares a vertex whose every influence is beyond the threshold', () => {
    const { mesh } = farRig([
      { at: [0, 0, 0], influences: [[0, 1]] },
      { at: [1, 0, 0], influences: [[1, 1]] }
    ]);
    const before = weightsOf(mesh, 1);

    const result = applyPrune([mesh], 0.5);
    expect(result.verticesSpared).toBe(1);
    expect(result.verticesTouched).toBe(0);
    expect(result.prunedInfluences).toBe(0);
    expect(result.byBone).toEqual([]);
    expect(weightsOf(mesh, 1)).toEqual(before);
  });

  it('renormalises the survivors so the vertex keeps its total weight', () => {
    const { mesh } = farRig([
      { at: [0, 0, 0], influences: [[0, 1]] },
      {
        at: [1, 0, 0],
        influences: [
          [0, 0.5],
          [1, 0.5]
        ]
      }
    ]);

    const result = applyPrune([mesh], 2);
    expect(result.verticesTouched).toBe(1);
    expect(result.prunedInfluences).toBe(1);
    expect(result.byBone).toEqual([{ name: 'bone1', influences: 1 }]);

    const weights = weightsOf(mesh, 1);
    expect(weights[1]).toBe(0);
    expect(weights[0]).toBeCloseTo(1, 6);
    expect(weights.reduce((a, b) => a + b, 0)).toBeCloseTo(1, 6);
    // Untouched vertices stay untouched.
    expect(weightsOf(mesh, 0)).toEqual([1, 0, 0, 0]);
  });

  /**
   * ⚠️ The scale is `total / survivors`, so the vertex keeps whatever total it ARRIVED with — it is
   * not forced to 1. On a well-formed glTF the two are the same thing; on an asset whose weights do
   * not already sum to 1 this preserves the fault rather than repairing it. Pinned here because the
   * source comment says "a total weight of exactly 1", which is only true of a normalised input.
   */
  it('preserves the incoming total rather than forcing it to 1', () => {
    const { mesh } = farRig([
      { at: [0, 0, 0], influences: [[0, 1]] },
      {
        at: [1, 0, 0],
        influences: [
          [0, 0.4],
          [1, 0.4]
        ]
      }
    ]);
    applyPrune([mesh], 2);
    expect(weightsOf(mesh, 1).reduce((a, b) => a + b, 0)).toBeCloseTo(0.8, 6);
  });

  it('scales three survivors proportionally rather than flattening them', () => {
    const { mesh } = rig(
      [
        [0, 0, 0],
        [0, 0, 0],
        [0, 0, 0],
        [100, 0, 0]
      ],
      [
        { at: [0, 0, 0], influences: [[0, 1]] },
        {
          at: [1, 0, 0],
          influences: [
            [0, 0.1],
            [1, 0.2],
            [2, 0.3],
            [3, 0.4]
          ]
        }
      ]
    );

    applyPrune([mesh], 2);
    const weights = weightsOf(mesh, 1);
    expect(weights[3]).toBe(0);
    // 0.1 : 0.2 : 0.3 preserved, summed back up to the original 1.0.
    expect(weights[0]).toBeCloseTo(1 / 6, 5);
    expect(weights[1]).toBeCloseTo(2 / 6, 5);
    expect(weights[2]).toBeCloseTo(3 / 6, 5);
    expect(weights.reduce((a, b) => a + b, 0)).toBeCloseTo(1, 5);
  });

  it('marks the attribute for re-upload, or the GPU keeps rendering the old skin', () => {
    const { mesh } = farRig([
      { at: [0, 0, 0], influences: [[0, 1]] },
      {
        at: [1, 0, 0],
        influences: [
          [0, 0.5],
          [1, 0.5]
        ]
      }
    ]);
    // ⚠️ `BufferAttribute.needsUpdate` is a write-only setter that bumps `version`; reading it back
    // gives `undefined`. The version counter is the observable half.
    const attribute = mesh.geometry.getAttribute('skinWeight') as THREE.BufferAttribute;
    const before = attribute.version;
    applyPrune([mesh], 2);
    expect(attribute.version).toBeGreaterThan(before);
  });

  it('does nothing at a non-positive threshold', () => {
    const { mesh } = farRig([{ at: [0, 0, 0], influences: [[1, 1]] }]);
    const before = weightsOf(mesh, 0);
    expect(applyPrune([mesh], 0)).toEqual({
      prunedInfluences: 0,
      verticesTouched: 0,
      verticesSpared: 0,
      byBone: []
    });
    expect(weightsOf(mesh, 0)).toEqual(before);
  });

  it('has nothing to prune on a mesh with no skin attributes', () => {
    expect(applyPrune([unskinnedMesh(3)], 0.1).verticesTouched).toBe(0);
  });

  it('tallies per bone across meshes, worst first', () => {
    const { mesh, bones } = farRig([
      { at: [0, 0, 0], influences: [[0, 1]] },
      {
        at: [1, 0, 0],
        influences: [
          [0, 0.5],
          [1, 0.5]
        ]
      },
      {
        at: [1, 0, 0],
        influences: [
          [0, 0.5],
          [1, 0.5]
        ]
      }
    ]);
    expect(bones).toHaveLength(2);
    expect(applyPrune([mesh], 2).byBone).toEqual([{ name: 'bone1', influences: 2 }]);
  });
});

describe('previewPrune', () => {
  it('counts exactly what applyPrune would take, without touching a buffer', () => {
    const vertices: VertexSpec[] = [
      { at: [0, 0, 0], influences: [[0, 1]] },
      {
        at: [1, 0, 0],
        influences: [
          [0, 0.5],
          [1, 0.5]
        ]
      },
      // Spared: its only binding is the far one.
      { at: [1, 0, 0], influences: [[1, 1]] }
    ];
    const preview = rig(
      [
        [0, 0, 0],
        [100, 0, 0]
      ],
      vertices
    );
    const commit = rig(
      [
        [0, 0, 0],
        [100, 0, 0]
      ],
      vertices
    );

    const before = [0, 1, 2].map((i) => weightsOf(preview.mesh, i));
    const previewed = previewPrune([preview.mesh], 2);
    expect([0, 1, 2].map((i) => weightsOf(preview.mesh, i))).toEqual(before);

    expect(previewed).toEqual(applyPrune([commit.mesh], 2));
    expect(previewed.verticesTouched).toBe(1);
    expect(previewed.verticesSpared).toBe(1);
  });
});
