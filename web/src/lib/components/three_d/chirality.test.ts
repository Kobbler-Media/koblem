import * as THREE from 'three';
import { describe, expect, it } from 'vitest';

import { chirality, sideSwapPlan } from './chirality';
import { sanitizeName } from './stage';

/** Same builder as `symmetry.test.ts`: world coordinates in, a real parent chain out. */
interface Spec {
  name: string;
  parent: string | null;
  at: [number, number, number];
}

function buildRig(specs: Spec[]): THREE.Bone[] {
  const root = new THREE.Object3D();
  const byName = new Map<string, THREE.Bone>();
  const world = new Map<string, THREE.Vector3>();
  const bones: THREE.Bone[] = [];
  for (const spec of specs) {
    const bone = new THREE.Bone();
    bone.name = sanitizeName(spec.name);
    bone.userData = { name: spec.name };
    const here = new THREE.Vector3(...spec.at);
    const parentWorld = spec.parent ? world.get(spec.parent) : undefined;
    bone.position.copy(parentWorld ? here.clone().sub(parentWorld) : here);
    (spec.parent ? (byName.get(spec.parent) ?? root) : root).add(bone);
    byName.set(spec.name, bone);
    world.set(spec.name, here);
    bones.push(bone);
  }
  return bones;
}

/**
 * A correctly named character: she faces +Z (toes forward of ankles, head forward of neck), so by
 * glTF's own convention — right-handed, +Y up, +Z forward, −X right — her LEFT is at +X.
 */
const CORRECT: Spec[] = [
  { name: 'mixamorig:Hips', parent: null, at: [0, 1.0, 0] },
  { name: 'mixamorig:Spine', parent: 'mixamorig:Hips', at: [0, 1.1, 0] },
  { name: 'mixamorig:Spine1', parent: 'mixamorig:Spine', at: [0, 1.2, 0] },
  { name: 'mixamorig:Spine2', parent: 'mixamorig:Spine1', at: [0, 1.35, 0] },
  { name: 'mixamorig:Neck', parent: 'mixamorig:Spine2', at: [0, 1.5, 0] },
  { name: 'mixamorig:Head', parent: 'mixamorig:Neck', at: [0, 1.6, 0.04] },
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

/** Swap only the NAMES — the same body, labelled the way this pipeline labels it. */
function mirrorNames(specs: Spec[]): Spec[] {
  const flip = (n: string): string =>
    n.includes(':Left')
      ? n.replace(':Left', ':Right')
      : n.includes(':Right')
        ? n.replace(':Right', ':Left')
        : n;
  return specs.map((spec) => ({
    ...spec,
    name: flip(spec.name),
    parent: spec.parent ? flip(spec.parent) : null
  }));
}

/** Turn the body around without touching the names: negate Z, keep X. */
function faceBackwards(specs: Spec[]): Spec[] {
  return specs.map((spec) => ({
    ...spec,
    at: [spec.at[0], spec.at[1], -spec.at[2]] as [number, number, number]
  }));
}

describe('chirality', () => {
  it('accepts a rig whose names agree with the way it faces', () => {
    const result = chirality(buildRig(CORRECT));
    expect(result.facing).toBe(1);
    expect(result.rightSign).toBe(-1);
    expect(result.labelledSign).toBe(-1);
    expect(result.verdict).toBe('ok');
    expect(result.pairs).toBe(8);
  });

  /** 🔴 The bug this module exists for: the body is identical, only the labels moved. */
  it('calls a name-swapped rig mirrored', () => {
    const result = chirality(buildRig(mirrorNames(CORRECT)));
    expect(result.facing).toBe(1);
    expect(result.rightSign).toBe(-1);
    expect(result.labelledSign).toBe(1);
    expect(result.verdict).toBe('mirrored');
  });

  /** Facing is measured, not assumed, so turning the character around flips the correct answer. */
  it('flips its expectation when the character faces the other way', () => {
    expect(chirality(buildRig(faceBackwards(CORRECT))).verdict).toBe('mirrored');
    expect(chirality(buildRig(faceBackwards(mirrorNames(CORRECT)))).verdict).toBe('ok');
  });

  it('reads both cues and notes when they disagree', () => {
    const ok = chirality(buildRig(CORRECT));
    expect(ok.feet?.sign).toBe(1);
    expect(ok.head?.sign).toBe(1);
    expect(ok.cuesDisagree).toBe(false);

    // Head pushed behind the neck — the head cue now points the other way, and the feet still win.
    const conflicted = chirality(
      buildRig(CORRECT.map((s) => (s.name.endsWith(':Head') ? { ...s, at: [0, 1.6, -0.04] } : s)))
    );
    expect(conflicted.cuesDisagree).toBe(true);
    expect(conflicted.facing).toBe(1);
  });

  it('falls back to the head when the feet carry no cue', () => {
    const flatFeet = CORRECT.map((spec) =>
      spec.name.endsWith('ToeBase') ? { ...spec, at: [spec.at[0], 0.02, 0] as Spec['at'] } : spec
    );
    const result = chirality(buildRig(flatFeet));
    expect(result.feet).toBeNull();
    expect(result.head?.sign).toBe(1);
    expect(result.verdict).toBe('ok');
  });

  it('refuses rather than guesses when nothing faces anywhere', () => {
    const noCue = CORRECT.map((spec) =>
      spec.name.endsWith('ToeBase') || spec.name.endsWith(':Head')
        ? { ...spec, at: [spec.at[0], spec.at[1], 0] as Spec['at'] }
        : spec
    );
    const result = chirality(buildRig(noCue));
    expect(result.verdict).toBe('unknown');
    expect(result.reason).toMatch(/cannot be measured/);
  });

  it('refuses on a skeleton that is not a mirror at all', () => {
    const chain = buildRig([
      { name: 'root', parent: null, at: [0, 0, 0] },
      { name: 'a', parent: 'root', at: [0, 1, 0] },
      { name: 'b', parent: 'a', at: [0, 2, 0] }
    ]);
    const result = chirality(chain);
    expect(result.verdict).toBe('unknown');
    expect(result.reason).toMatch(/does not read as a mirror/);
  });

  it('is empty-safe', () => {
    expect(chirality([]).verdict).toBe('unknown');
  });
});

describe('sideSwapPlan', () => {
  it('names every sided core slot and no mid-line one', () => {
    const plan = sideSwapPlan(buildRig(CORRECT));
    expect(plan).toHaveLength(16);
    expect(plan.some((s) => s.from.includes('Hips') || s.from.includes('Spine'))).toBe(false);
    for (const swap of plan) {
      expect(swap.to).not.toBe(swap.from);
      expect(swap.to.startsWith('mixamorig:')).toBe(true);
    }
  });

  it('is an involution — applying the plan twice is the identity', () => {
    const bones = buildRig(CORRECT);
    const first = new Map(sideSwapPlan(bones).map((s) => [s.from, s.to]));
    for (const [from, to] of first) expect(first.get(to)).toBe(from);
  });

  /** ⚠️ A slot whose partner is missing is still on the wrong side and still has to be renamed. */
  it('renames a sided slot even when its partner is absent', () => {
    const bones = buildRig(CORRECT.filter((s) => !s.name.includes(':RightHand')));
    const plan = sideSwapPlan(bones);
    expect(plan.map((s) => s.from)).toContain('mixamorig:LeftHand');
    expect(plan).toHaveLength(15);
  });

  it('leaves anonymous and non-core joints alone', () => {
    const bones = buildRig([
      ...CORRECT,
      { name: 'bone_41', parent: 'mixamorig:Head', at: [0, 1.8, 0] },
      { name: 'mixamorig:LeftHandIndex1', parent: 'mixamorig:LeftHand', at: [0.2, 0.85, 0] }
    ]);
    const plan = sideSwapPlan(bones);
    expect(plan).toHaveLength(16);
    expect(plan.some((s) => s.from.includes('Index') || s.from === 'bone_41')).toBe(false);
  });
});
