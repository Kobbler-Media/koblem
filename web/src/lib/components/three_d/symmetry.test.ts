import * as THREE from 'three';
import { describe, expect, it } from 'vitest';

import { sanitizeName } from './stage';
import {
  CORE_PAIRS,
  detectMirrorAxis,
  jointPositions,
  mirrorBone,
  mirrorBoneName,
  mirrorCoreName,
  mirrorDelta,
  mirrorJointMove,
  mirrorPairs,
  mirrorPlaneOffset,
  mirrorSource,
  reflectPoint,
  symmetryReport
} from './symmetry';

/**
 * A joint spec in WORLD coordinates, which the builder turns into the local offsets a real rig
 * carries — so every position the module reports has to survive the parent-chain composition rather
 * than being read straight back off `bone.position`.
 */
interface Spec {
  name: string;
  parent: string | null;
  at: [number, number, number];
}

function buildRig(specs: Spec[]): { root: THREE.Object3D; bones: THREE.Bone[] } {
  const root = new THREE.Object3D();
  const byName = new Map<string, THREE.Bone>();
  const world = new Map<string, THREE.Vector3>();
  const bones: THREE.Bone[] = [];
  for (const spec of specs) {
    const bone = new THREE.Bone();
    // 🔴 Exactly what GLTFLoader does to the file's name — the colons are gone on the bone.
    bone.name = sanitizeName(spec.name);
    const here = new THREE.Vector3(...spec.at);
    const parentWorld = spec.parent ? world.get(spec.parent) : undefined;
    bone.position.copy(parentWorld ? here.clone().sub(parentWorld) : here);
    const parent = spec.parent ? byName.get(spec.parent) : undefined;
    (parent ?? root).add(bone);
    byName.set(spec.name, bone);
    world.set(spec.name, here);
    bones.push(bone);
  }
  return { root, bones };
}

/** X across the body, Y up, Z depth — the convention every rig out of this pipeline has used. */
const HUMANOID: Spec[] = [
  { name: 'mixamorig:Hips', parent: null, at: [0, 1.0, 0] },
  { name: 'mixamorig:Spine', parent: 'mixamorig:Hips', at: [0, 1.1, 0] },
  { name: 'mixamorig:Spine1', parent: 'mixamorig:Spine', at: [0, 1.2, 0] },
  { name: 'mixamorig:Spine2', parent: 'mixamorig:Spine1', at: [0, 1.35, 0] },
  { name: 'mixamorig:Neck', parent: 'mixamorig:Spine2', at: [0, 1.5, 0] },
  { name: 'mixamorig:Head', parent: 'mixamorig:Neck', at: [0, 1.6, 0] },
  { name: 'mixamorig:LeftShoulder', parent: 'mixamorig:Spine2', at: [0.08, 1.45, 0] },
  { name: 'mixamorig:LeftArm', parent: 'mixamorig:LeftShoulder', at: [0.2, 1.42, 0] },
  { name: 'mixamorig:LeftForeArm', parent: 'mixamorig:LeftArm', at: [0.2, 1.15, 0] },
  { name: 'mixamorig:LeftHand', parent: 'mixamorig:LeftForeArm', at: [0.2, 0.9, 0] },
  { name: 'mixamorig:RightShoulder', parent: 'mixamorig:Spine2', at: [-0.08, 1.45, 0] },
  { name: 'mixamorig:RightArm', parent: 'mixamorig:RightShoulder', at: [-0.2, 1.42, 0] },
  { name: 'mixamorig:RightForeArm', parent: 'mixamorig:RightArm', at: [-0.2, 1.15, 0] },
  { name: 'mixamorig:RightHand', parent: 'mixamorig:RightForeArm', at: [-0.2, 0.9, 0] },
  { name: 'mixamorig:LeftUpLeg', parent: 'mixamorig:Hips', at: [0.1, 0.95, 0] },
  { name: 'mixamorig:LeftLeg', parent: 'mixamorig:LeftUpLeg', at: [0.1, 0.52, 0] },
  { name: 'mixamorig:LeftFoot', parent: 'mixamorig:LeftLeg', at: [0.1, 0.08, 0] },
  { name: 'mixamorig:LeftToeBase', parent: 'mixamorig:LeftFoot', at: [0.1, 0.02, 0.1] },
  { name: 'mixamorig:RightUpLeg', parent: 'mixamorig:Hips', at: [-0.1, 0.95, 0] },
  { name: 'mixamorig:RightLeg', parent: 'mixamorig:RightUpLeg', at: [-0.1, 0.52, 0] },
  { name: 'mixamorig:RightFoot', parent: 'mixamorig:RightLeg', at: [-0.1, 0.08, 0] },
  { name: 'mixamorig:RightToeBase', parent: 'mixamorig:RightFoot', at: [-0.1, 0.02, 0.1] }
];

/** Same body, rewritten into a Z-across convention. Nothing in the module may assume X. */
const Z_ACROSS: Spec[] = HUMANOID.map((spec) => ({
  ...spec,
  at: [spec.at[2], spec.at[1], spec.at[0]] as [number, number, number]
}));

function move(specs: Spec[], name: string, at: [number, number, number]): Spec[] {
  return specs.map((spec) => (spec.name === name ? { ...spec, at } : spec));
}

function drop(specs: Spec[], name: string): Spec[] {
  return specs.filter((spec) => spec.name !== name);
}

// ── the name table ───────────────────────────────────────────────────────────

describe('mirrorCoreName', () => {
  it('pairs all 8 sided slots, both directions', () => {
    expect(CORE_PAIRS).toHaveLength(8);
    for (const pair of CORE_PAIRS) {
      expect(mirrorCoreName(pair.left)).toBe(pair.right);
      expect(mirrorCoreName(pair.right)).toBe(pair.left);
    }
  });

  /** 🔴 The bone carries the sanitized name; the file and the retarget carry the canonical one. */
  it('takes a sanitized name and still answers in canonical form', () => {
    expect(mirrorCoreName('mixamorigLeftForeArm')).toBe('mixamorig:RightForeArm');
    expect(mirrorCoreName('mixamorig:LeftForeArm')).toBe('mixamorig:RightForeArm');
  });

  it('is null for the mid-line slots and for anything off the table', () => {
    for (const core of ['mixamorig:Hips', 'mixamorig:Spine2', 'mixamorig:Neck', 'mixamorig:Head']) {
      expect(mirrorCoreName(core)).toBeNull();
    }
    expect(mirrorCoreName('mixamorig:LeftEyelid')).toBeNull();
    expect(mirrorCoreName('')).toBeNull();
  });
});

describe('mirrorBoneName', () => {
  it('goes through the core table first, in three space', () => {
    expect(mirrorBoneName('mixamorigLeftArm')).toBe('mixamorigRightArm');
    expect(mirrorBoneName('mixamorigRightToeBase')).toBe('mixamorigLeftToeBase');
  });

  it('mirrors off-table bones on a bounded Left/Right token', () => {
    expect(mirrorBoneName('mixamorigLeftHandThumb1')).toBe('mixamorigRightHandThumb1');
    expect(mirrorBoneName('Left_Skirt_02')).toBe('Right_Skirt_02');
    expect(mirrorBoneName('left_eye')).toBe('right_eye');
    expect(mirrorBoneName('ARM_LEFT')).toBe('ARM_RIGHT');
    expect(mirrorBoneName('Left')).toBe('Right');
  });

  /** 🔴 The whole point of the boundary rules — a naive `replace('Left','Right')` fails all four. */
  it('refuses near-misses that a substring match would pair', () => {
    expect(mirrorBoneName('LeftoverBone')).toBeNull();
    expect(mirrorBoneName('Leftovers')).toBeNull();
    expect(mirrorBoneName('Bright_Eye')).toBeNull();
    expect(mirrorBoneName('Rightmost')).toBeNull();
  });

  it('mirrors an L/R segment but only when separators bound it', () => {
    expect(mirrorBoneName('Bip01_L_Arm')).toBe('Bip01_R_Arm');
    expect(mirrorBoneName('arm_r')).toBe('arm_l');
    expect(mirrorBoneName('LArm')).toBeNull();
    expect(mirrorBoneName('Lamp')).toBeNull();
  });

  it('rewrites every token in one pass, so a two-sided name cannot swap back to itself', () => {
    expect(mirrorBoneName('Left_Arm_Left_Twist')).toBe('Right_Arm_Right_Twist');
    expect(mirrorBoneName('LeftRight')).toBe('RightLeft');
  });

  /** The documented, accepted weakness: a lowercase compound with a real camelCase hinge. */
  it('is fooled by copyright/copyleft, which is why the partner must exist in the skeleton', () => {
    expect(mirrorBoneName('copyright_bone')).toBe('copyleft_bone');
    const { bones } = buildRig([{ name: 'copyright_bone', parent: null, at: [0, 0, 0] }]);
    expect(mirrorBone(bones, bones[0])).toBeNull();
  });

  it('has nothing to say about an unsided name', () => {
    expect(mirrorBoneName('mixamorigHips')).toBeNull();
    expect(mirrorBoneName('Bone_04')).toBeNull();
  });
});

// ── finding the partner bone ─────────────────────────────────────────────────

describe('mirrorBone', () => {
  it('finds the other side of every core pair', () => {
    const { bones } = buildRig(HUMANOID);
    const leftArm = bones.find((b) => b.name === 'mixamorigLeftArm')!;
    expect(mirrorBone(bones, leftArm)?.name).toBe('mixamorigRightArm');
    expect(mirrorSource(bones, leftArm)).toBe('core');
  });

  it('is null for a mid-line joint, which has no other side', () => {
    const { bones } = buildRig(HUMANOID);
    for (const name of ['mixamorigHips', 'mixamorigSpine2', 'mixamorigHead']) {
      const bone = bones.find((b) => b.name === name)!;
      expect(mirrorBone(bones, bone)).toBeNull();
      expect(mirrorSource(bones, bone)).toBeNull();
    }
  });

  it('is null when the partner name is not in the skeleton', () => {
    const { bones } = buildRig(drop(HUMANOID, 'mixamorig:RightHand'));
    const leftHand = bones.find((b) => b.name === 'mixamorigLeftHand')!;
    expect(mirrorBone(bones, leftHand)).toBeNull();
  });

  /** 🔴 Both near-misses present at once: a substring match would happily marry them. */
  it('does not pair LeftoverBone with RightoverBone', () => {
    const { bones } = buildRig([
      ...HUMANOID,
      { name: 'LeftoverBone', parent: 'mixamorig:Hips', at: [0.3, 1.0, 0] },
      { name: 'RightoverBone', parent: 'mixamorig:Hips', at: [-0.3, 1.0, 0] }
    ]);
    const leftover = bones.find((b) => b.name === 'LeftoverBone')!;
    expect(mirrorBone(bones, leftover)).toBeNull();
  });

  it('refuses a duplicate name rather than picking one of them', () => {
    const { bones } = buildRig([
      ...HUMANOID,
      { name: 'Left_Skirt', parent: 'mixamorig:Hips', at: [0.2, 1.0, 0] },
      { name: 'Right_Skirt', parent: 'mixamorig:Hips', at: [-0.2, 1.0, 0] }
    ]);
    const clone = new THREE.Bone();
    clone.name = 'Right_Skirt';
    const skirt = bones.find((b) => b.name === 'Left_Skirt')!;
    expect(mirrorBone([...bones, clone], skirt)).toBeNull();
  });
});

describe('mirrorPairs', () => {
  it('lists each core pair once, core first, with pattern pairs after', () => {
    const { bones } = buildRig([
      ...HUMANOID,
      { name: 'Left_Skirt', parent: 'mixamorig:Hips', at: [0.2, 1.0, 0] },
      { name: 'Right_Skirt', parent: 'mixamorig:Hips', at: [-0.2, 1.0, 0] },
      { name: 'LeftoverBone', parent: 'mixamorig:Hips', at: [0.3, 1.0, 0] },
      { name: 'RightoverBone', parent: 'mixamorig:Hips', at: [-0.3, 1.0, 0] }
    ]);
    const pairs = mirrorPairs(bones);
    expect(pairs).toHaveLength(9);
    expect(pairs.slice(0, 8).every((p) => p.source === 'core')).toBe(true);
    expect(pairs[0].left.name).toBe('mixamorigLeftShoulder');
    expect(pairs[8].source).toBe('pattern');
    expect([pairs[8].left.name, pairs[8].right.name].sort()).toEqual(['Left_Skirt', 'Right_Skirt']);
    expect(pairs.some((p) => p.left.name.includes('over') || p.right.name.includes('over'))).toBe(
      false
    );
  });

  it('is empty on a skeleton with no sided joints at all', () => {
    const { bones } = buildRig([
      { name: 'root', parent: null, at: [0, 0, 0] },
      { name: 'stalk', parent: 'root', at: [0, 1, 0] },
      { name: 'cap', parent: 'stalk', at: [0, 2, 0] }
    ]);
    expect(mirrorPairs(bones)).toEqual([]);
  });
});

// ── positions ────────────────────────────────────────────────────────────────

describe('jointPositions', () => {
  it('composes world positions up the parent chain', () => {
    const { bones } = buildRig(HUMANOID);
    const positions = jointPositions(bones);
    const hand = bones.find((b) => b.name === 'mixamorigLeftHand')!;
    expect(positions.get(hand)!.toArray()).toEqual([0.2, 0.9, 0]);
  });

  /** ⚠️ The bind pose is what a joint repair is measured in; a playing clip must not move it. */
  it('reads the bind pose from a skeleton even after the rig is posed', () => {
    const { root, bones } = buildRig(HUMANOID);
    root.updateMatrixWorld(true);
    const skeleton = new THREE.Skeleton(bones);
    const hand = bones.find((b) => b.name === 'mixamorigLeftHand')!;

    hand.position.x += 0.5;
    expect(jointPositions(bones).get(hand)!.x).toBeCloseTo(0.7, 6);
    expect(jointPositions(bones, skeleton).get(hand)!.x).toBeCloseTo(0.2, 6);
  });

  /** 🔴 The module looks and does not touch — including the cached matrices it reads through. */
  it('does not write the matrices of the bones it measures', () => {
    const { bones } = buildRig(HUMANOID);
    const identity = new THREE.Matrix4();
    detectMirrorAxis(bones);
    symmetryReport(bones, 'x');
    expect(bones.every((b) => b.matrixWorld.equals(identity))).toBe(true);
  });
});

// ── deriving the axis ────────────────────────────────────────────────────────

describe('detectMirrorAxis', () => {
  it('reads X off a clean humanoid, with the plane on the origin', () => {
    const { bones } = buildRig(HUMANOID);
    const detected = detectMirrorAxis(bones);
    expect(detected.axis).toBe('x');
    expect(detected.confidence).toBeGreaterThan(0.99);
    expect(detected.pairs).toBe(8);
    expect(detected.offset).toBeCloseTo(0, 6);
    expect(detected.scores.y).toBeLessThan(0.01);
    expect(detected.scores.z).toBeLessThan(0.01);
  });

  /** ⚠️ Nothing may assume the pipeline's convention — the axis is derived, not declared. */
  it('reads Z off the same body exported Z-across', () => {
    const { bones } = buildRig(Z_ACROSS);
    const detected = detectMirrorAxis(bones);
    expect(detected.axis).toBe('z');
    expect(detected.confidence).toBeGreaterThan(0.99);
    expect(detected.scores.x).toBeLessThan(0.01);
  });

  it('finds the plane where the character actually stands, not at the origin', () => {
    const offset: Spec[] = HUMANOID.map((spec) => ({
      ...spec,
      at: [spec.at[0] + 5, spec.at[1], spec.at[2]] as [number, number, number]
    }));
    const { bones } = buildRig(offset);
    const detected = detectMirrorAxis(bones);
    expect(detected.axis).toBe('x');
    expect(detected.offset).toBeCloseTo(5, 6);
    expect(mirrorPlaneOffset(bones, 'x')).toBeCloseTo(5, 6);
  });

  /** Real characters are never perfect. Confidence has to degrade, not fall off a cliff. */
  it('survives one shoulder sitting 4 cm higher than the other', () => {
    const { bones } = buildRig(move(HUMANOID, 'mixamorig:LeftShoulder', [0.08, 1.49, 0]));
    const detected = detectMirrorAxis(bones);
    expect(detected.axis).toBe('x');
    expect(detected.confidence).toBeGreaterThan(0.95);
  });

  /** 🔴 The legs are labelled backwards, so "left" is on both sides. No axis explains that. */
  it('collapses when the sides are not consistently on one side', () => {
    let specs = HUMANOID;
    for (const slot of ['UpLeg', 'Leg', 'Foot', 'ToeBase']) {
      for (const side of ['Left', 'Right']) {
        const name = `mixamorig:${side}${slot}`;
        const at = HUMANOID.find((s) => s.name === name)!.at;
        specs = move(specs, name, [-at[0], at[1], at[2]]);
      }
    }
    const { bones } = buildRig(specs);
    const detected = detectMirrorAxis(bones);
    expect(detected.pairs).toBe(8);
    expect(detected.confidence).toBeLessThan(0.05);
  });

  /** Every pair sitting on top of its partner is evidence for nothing, not evidence for X. */
  it('collapses when both sides occupy the same place', () => {
    let specs = HUMANOID;
    for (const spec of HUMANOID) {
      if (!spec.name.includes('Right')) continue;
      const left = HUMANOID.find((s) => s.name === spec.name.replace('Right', 'Left'))!;
      specs = move(specs, spec.name, left.at);
    }
    const { bones } = buildRig(specs);
    expect(detectMirrorAxis(bones).confidence).toBeLessThan(0.05);
  });

  /** ⚠️ The axis is a placeholder here, not an answer. The confidence is the part to read. */
  it('has zero confidence on a non-humanoid with nothing to pair', () => {
    const { bones } = buildRig([
      { name: 'root', parent: null, at: [0, 0, 0] },
      { name: 'stalk', parent: 'root', at: [0, 1, 0] },
      { name: 'cap', parent: 'stalk', at: [0.4, 2, 0.3] }
    ]);
    const detected = detectMirrorAxis(bones);
    expect(detected.pairs).toBe(0);
    expect(detected.confidence).toBe(0);
  });

  it('has zero confidence on an empty skeleton', () => {
    expect(detectMirrorAxis([]).confidence).toBe(0);
  });

  /** One pair is an anecdote — the sample term keeps it out of the "confident" band. */
  it('discounts a skeleton carrying only a single pair', () => {
    const { bones } = buildRig([
      { name: 'mixamorig:Hips', parent: null, at: [0, 1, 0] },
      { name: 'mixamorig:LeftHand', parent: 'mixamorig:Hips', at: [0.4, 1, 0] },
      { name: 'mixamorig:RightHand', parent: 'mixamorig:Hips', at: [-0.4, 1, 0] }
    ]);
    const detected = detectMirrorAxis(bones);
    expect(detected.axis).toBe('x');
    expect(detected.pairs).toBe(1);
    expect(detected.confidence).toBeCloseTo(1 / 3, 6);
  });
});

// ── mirroring a delta ────────────────────────────────────────────────────────

describe('mirrorDelta', () => {
  it('negates only the component on the axis, and leaves the input alone', () => {
    const delta = new THREE.Vector3(0.03, -0.05, 0.01);
    expect(mirrorDelta(delta, 'x').toArray()).toEqual([-0.03, -0.05, 0.01]);
    expect(mirrorDelta(delta, 'y').toArray()).toEqual([0.03, 0.05, 0.01]);
    expect(mirrorDelta(delta, 'z').toArray()).toEqual([0.03, -0.05, -0.01]);
    expect(delta.toArray()).toEqual([0.03, -0.05, 0.01]);
  });

  it('is a reflection, so it keeps the length and undoes itself', () => {
    const delta = new THREE.Vector3(0.03, -0.05, 0.01);
    expect(mirrorDelta(delta, 'x').length()).toBeCloseTo(delta.length(), 12);
    expect(mirrorDelta(mirrorDelta(delta, 'x'), 'x').toArray()).toEqual(delta.toArray());
  });
});

describe('reflectPoint', () => {
  it('reflects through a plane that is not on the origin', () => {
    const point = new THREE.Vector3(5.2, 1.4, 0.1);
    expect(reflectPoint(point, { axis: 'x', offset: 5 }).toArray()).toEqual([4.8, 1.4, 0.1]);
    expect(reflectPoint(point, { axis: 'x', offset: 0 }).toArray()).toEqual([-5.2, 1.4, 0.1]);
  });
});

// ── the asymmetry already in the rig ─────────────────────────────────────────

describe('symmetryReport', () => {
  it('finds nothing on a rig that is already symmetric', () => {
    const { bones } = buildRig(HUMANOID);
    const report = symmetryReport(bones, 'x');
    expect(report.pairs).toHaveLength(8);
    expect(report.worst).toBeCloseTo(0, 9);
    expect(report.unpaired).toEqual([]);
    expect(report.plane.offset).toBeCloseTo(0, 9);
  });

  it('names the worst pair first, in canonical form, with the real distance', () => {
    const { bones } = buildRig(move(HUMANOID, 'mixamorig:RightHand', [-0.22, 0.93, 0]));
    const report = symmetryReport(bones, { axis: 'x', offset: 0 });
    expect(report.pairs[0].left).toBe('mixamorig:LeftHand');
    expect(report.pairs[0].right).toBe('mixamorig:RightHand');
    expect(report.pairs[0].source).toBe('core');
    expect(report.pairs[0].offset).toBeCloseTo(Math.hypot(0.02, 0.03), 9);
    expect(report.worst).toBeCloseTo(Math.hypot(0.02, 0.03), 9);
    expect(report.worstRatio).toBeCloseTo(Math.hypot(0.02, 0.03) / report.span, 9);
  });

  /** The cheapest check that the detected axis is right: a wrong one makes every pair enormous. */
  it('explodes when measured against the wrong axis', () => {
    const { bones } = buildRig(HUMANOID);
    expect(symmetryReport(bones, 'y').worst).toBeGreaterThan(0.3);
  });

  it('reports a core slot that exists on one side only', () => {
    const { bones } = buildRig(drop(HUMANOID, 'mixamorig:RightToeBase'));
    const report = symmetryReport(bones, 'x');
    expect(report.unpaired).toEqual(['mixamorig:LeftToeBase']);
    expect(report.pairs).toHaveLength(7);
  });

  it('has nothing to report on a skeleton with no pairs', () => {
    const { bones } = buildRig([{ name: 'root', parent: null, at: [0, 0, 0] }]);
    const report = symmetryReport(bones, 'x');
    expect(report.pairs).toEqual([]);
    expect(report.worst).toBe(0);
    expect(report.worstRatio).toBe(0);
  });
});

// ── what to apply ────────────────────────────────────────────────────────────

describe('mirrorJointMove', () => {
  const plane = { axis: 'x', offset: 0 } as const;

  it('names the partner and the reflected delta', () => {
    const { bones } = buildRig(HUMANOID);
    const leftHand = bones.find((b) => b.name === 'mixamorigLeftHand')!;
    const step = mirrorJointMove(bones, leftHand, new THREE.Vector3(0.03, -0.05, 0), plane)!;
    expect(step.bone.name).toBe('mixamorigRightHand');
    expect(step.delta.toArray()).toEqual([-0.03, -0.05, 0]);
    expect(step.source).toBe('core');
    expect(step.residual).toBeCloseTo(0, 9);
    expect(step.extra).toBe(0);
  });

  /** ⚠️ What `'delta'` promises: the same repair. The asymmetry that was there stays there. */
  it('carries a pre-existing asymmetry forward untouched, and says how big it is', () => {
    const { bones } = buildRig(move(HUMANOID, 'mixamorig:RightHand', [-0.22, 0.9, 0]));
    const leftHand = bones.find((b) => b.name === 'mixamorigLeftHand')!;
    const step = mirrorJointMove(bones, leftHand, new THREE.Vector3(0.03, -0.05, 0), plane)!;
    expect(step.delta.toArray()).toEqual([-0.03, -0.05, 0]);
    expect(step.residual).toBeCloseTo(0.02, 9);
  });

  /** `'snap'` is the bigger claim: it absorbs that asymmetry into this one move. */
  it('snap lands the partner on the exact reflection and prices the difference', () => {
    const { bones } = buildRig(move(HUMANOID, 'mixamorig:RightHand', [-0.22, 0.9, 0]));
    const leftHand = bones.find((b) => b.name === 'mixamorigLeftHand')!;
    const delta = new THREE.Vector3(0.03, -0.05, 0);
    const step = mirrorJointMove(bones, leftHand, delta, plane, 'snap')!;
    // left ends at (0.23, 0.85); its reflection is (-0.23, 0.85); the right hand is at (-0.22, 0.9).
    expect(step.delta.x).toBeCloseTo(-0.01, 9);
    expect(step.delta.y).toBeCloseTo(-0.05, 9);
    expect(step.residual).toBe(0);
    expect(step.extra).toBeCloseTo(step.delta.length() - mirrorDelta(delta, 'x').length(), 12);
  });

  it('is null for a mid-line joint and for a bone whose partner is missing', () => {
    const { bones } = buildRig(drop(HUMANOID, 'mixamorig:RightFoot'));
    const hips = bones.find((b) => b.name === 'mixamorigHips')!;
    const leftFoot = bones.find((b) => b.name === 'mixamorigLeftFoot')!;
    const delta = new THREE.Vector3(0, 0.01, 0);
    expect(mirrorJointMove(bones, hips, delta, plane)).toBeNull();
    expect(mirrorJointMove(bones, leftFoot, delta, plane)).toBeNull();
  });

  it('derives the plane itself when handed a bare axis', () => {
    const shifted: Spec[] = HUMANOID.map((spec) => ({
      ...spec,
      at: [spec.at[0] + 5, spec.at[1], spec.at[2]] as [number, number, number]
    }));
    const { bones } = buildRig(shifted);
    const leftHand = bones.find((b) => b.name === 'mixamorigLeftHand')!;
    const step = mirrorJointMove(bones, leftHand, new THREE.Vector3(0.03, 0, 0), 'x', 'snap')!;
    // Off-centre or not, a snap onto a symmetric rig is the plain reflection.
    expect(step.delta.x).toBeCloseTo(-0.03, 9);
    expect(step.residual).toBe(0);
  });

  it('mirrors a pattern-paired bone too, and says the pairing was a guess', () => {
    const { bones } = buildRig([
      ...HUMANOID,
      { name: 'Left_Skirt', parent: 'mixamorig:Hips', at: [0.2, 1.0, 0] },
      { name: 'Right_Skirt', parent: 'mixamorig:Hips', at: [-0.2, 1.0, 0] }
    ]);
    const skirt = bones.find((b) => b.name === 'Left_Skirt')!;
    const step = mirrorJointMove(bones, skirt, new THREE.Vector3(0.01, 0, 0), plane)!;
    expect(step.bone.name).toBe('Right_Skirt');
    expect(step.source).toBe('pattern');
  });
});
