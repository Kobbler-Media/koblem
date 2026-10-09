/*
 * Which side of a character is its OWN left, and does the rig's naming agree.
 *
 * # Why this exists
 *
 * `Left`/`Right` in every humanoid naming convention — Mixamo, Unity Humanoid, VRM, Rigify — is
 * relative to the CHARACTER, not the viewer. `mixamorig:RightArm` is the arm on the character's own
 * right, which appears on the viewer's LEFT when the character faces the camera.
 *
 * A bare skeleton does not determine that. A mirrored skeleton is not a rotation of the original,
 * so "which side is left" is recoverable only from a FACING cue, and the rig namer upstream derives
 * facing from the feet (toes lie forward of ankles). It then turns facing into a side with
 *
 *     right = up × forward
 *
 * which is BACKWARDS — the cross product that gives a right vector is `forward × up`. So every rig
 * this pipeline has produced carries `Left`/`Right` swapped, consistently, on all 16 sided joints.
 *
 * 🔴 **The measurement that settles it** (`miku_rigged.glb`, 2026-08-18): `LeftToeBase` z=+0.121 vs
 * `LeftFoot` z=+0.043 — toes forward of ankles, so she faces +Z; `Head` z=+0.309 vs `Neck` z=+0.285
 * corroborates. glTF 2.0 §3.5 is explicit — right-handed, +Y up, +Z forward, **−X right** — so her
 * own right is at −X, and the bone named `RightFoot` sits at x=+0.223.
 *
 * # Why nothing downstream caught it
 *
 * The motion retarget does not trust the names for chirality: `decide_lr_swap` measures which side
 * the rig's "L" joints actually sit on and compares it to the source clip's own body frame, so a
 * mirrored rig gets `swap=ON` and animates correctly anyway. The labels are wrong where a HUMAN or
 * another ENGINE reads them — this editor, a Unity/VRM/Mixamo import, anything hand-authored.
 *
 * # What this module does
 *
 * Measures facing from the same two cues, applies the correct handedness rule, and reports whether
 * the names agree. It reads positions only; [`sideSwapPlan`] returns the renames that would fix a
 * mirrored rig and applies nothing.
 *
 * ⚠️ Both cues are side-INDEPENDENT by construction: toe-vs-ankle compares two joints of the SAME
 * leg and head-vs-neck is mid-line, so neither can be fooled by the very labelling under test. That
 * is what keeps this from being circular the way the engine's own falsifier is.
 */

import * as THREE from 'three';

import {
  CORE_PAIRS,
  MIRROR_MIN_CONFIDENCE,
  detectMirrorAxis,
  jointPositions,
  skeletonSpan
} from './symmetry';
import { sanitizeName } from './stage';

/** One facing measurement: the sign of the forward axis, and how strong the evidence was. */
export interface FacingCue {
  /** +1 = the character faces +Z, −1 = it faces −Z. */
  sign: 1 | -1;
  /** The cue's displacement along Z as a fraction of the skeleton's longest dimension. Scale-free,
   *  so the same floor means the same thing on a 2-metre character and a 2-unit one. */
  margin: number;
}

export type ChiralityVerdict = 'ok' | 'mirrored' | 'unknown';

export interface Chirality {
  verdict: ChiralityVerdict;
  /** Why `unknown`, in a sentence worth showing. Empty otherwise. */
  reason: string;
  /** Toes forward of ankles, summed over both feet. The primary cue. */
  feet: FacingCue | null;
  /** Head forward of neck. Independent of the feet, and of the naming. */
  head: FacingCue | null;
  /** The facing the two cues settled on, or 0 if there was none. */
  facing: 1 | -1 | 0;
  /** True when both cues exist and disagree — the character is not a reliable humanoid, and the
   *  answer below rests on the feet alone. */
  cuesDisagree: boolean;
  /** The sign of X that is the character's OWN right, given `facing`. */
  rightSign: 1 | -1 | 0;
  /** The sign of X the bones NAMED `Right*` actually sit on. */
  labelledSign: 1 | -1 | 0;
  /** Mean |x(Right) − x(Left)| over the pairs, as a fraction of the skeleton span. Near zero means
   *  the sided joints are on the midline and the reading is worthless. */
  separation: number;
  /** Sided core pairs with both halves present — the sample this verdict was measured over. */
  pairs: number;
}

/** Below this the cue is a rounding error rather than a direction. 1% of the skeleton's span. */
const CUE_FLOOR = 0.01;

/** Below this the two sides are effectively on the midline and their X sign says nothing. */
const SEPARATION_FLOOR = 0.02;

/** The across-the-body axis this module can read a facing against. Anything else is `unknown`. */
const AX_LAT = 'x';
/** The component index of the forward axis. glTF is +Y up, so across is X and forward is Z. */
const AX_FWD = 2;

function sign(value: number): 1 | -1 | 0 {
  return value > 0 ? 1 : value < 0 ? -1 : 0;
}

function cue(displacement: number, span: number): FacingCue | null {
  const margin = span > 0 ? Math.abs(displacement) / span : 0;
  const s = sign(displacement);
  if (!s || margin < CUE_FLOOR) return null;
  return { sign: s, margin };
}

function byName(bones: THREE.Bone[]): Map<string, THREE.Bone> {
  return new Map(bones.map((bone) => [bone.name, bone]));
}

/**
 * Measure the character's chirality and check the naming against it.
 *
 * Pass the `skeleton` to read the BIND pose rather than the live transforms — same rule as
 * [`jointPositions`], and the honest reading on anything carrying a clip.
 */
export function chirality(bones: THREE.Bone[], skeleton?: THREE.Skeleton | null): Chirality {
  const blank: Chirality = {
    verdict: 'unknown',
    reason: '',
    feet: null,
    head: null,
    facing: 0,
    cuesDisagree: false,
    rightSign: 0,
    labelledSign: 0,
    separation: 0,
    pairs: 0
  };
  if (!bones.length) return { ...blank, reason: 'No skeleton loaded.' };

  const positions = jointPositions(bones, skeleton);
  const span = skeletonSpan(positions.values());
  const at = byName(bones);
  const pos = (canonical: string): THREE.Vector3 | null => {
    const bone = at.get(sanitizeName(canonical));
    return bone ? (positions.get(bone) ?? null) : null;
  };

  // 🔴 The lateral axis is DERIVED, not assumed. Everything below reads X as across-the-body and Z
  // as forward; a rig exported in another convention gets "unknown" rather than a confident lie.
  const axis = detectMirrorAxis(bones, skeleton);
  const readsAsMirror = axis.confidence >= MIRROR_MIN_CONFIDENCE;
  if (axis.axis !== AX_LAT || !readsAsMirror) {
    return {
      ...blank,
      reason: readsAsMirror
        ? `This skeleton mirrors across ${axis.axis.toUpperCase()}, not X, so which way it faces ` +
          'cannot be read off Z. Chirality is not measured here.'
        : 'This skeleton does not read as a mirror at all — normal for a non-humanoid rig, and ' +
          'there is no left or right to check.'
    };
  }

  // ── cue 1: toes forward of ankles, over whichever feet are named ───────────
  let feetSum = 0;
  let feetSeen = 0;
  for (const side of ['Left', 'Right']) {
    const ankle = pos(`mixamorig:${side}Foot`);
    const toe = pos(`mixamorig:${side}ToeBase`);
    if (!ankle || !toe) continue;
    feetSum += toe.getComponent(AX_FWD) - ankle.getComponent(AX_FWD);
    feetSeen++;
  }
  const feet = feetSeen ? cue(feetSum, span) : null;

  // ── cue 2: the head sits forward of the neck ───────────────────────────────
  const head = pos('mixamorig:Head');
  const neck = pos('mixamorig:Neck');
  const headCue =
    head && neck ? cue(head.getComponent(AX_FWD) - neck.getComponent(AX_FWD), span) : null;

  const facing = feet?.sign ?? headCue?.sign ?? 0;
  const cuesDisagree = !!feet && !!headCue && feet.sign !== headCue.sign;
  if (!facing) {
    return {
      ...blank,
      feet,
      head: headCue,
      reason:
        'Neither cue is readable — no toes forward of ankles, no head forward of a neck. Which ' +
        'way this character faces cannot be measured, so neither can its left from its right.'
    };
  }

  // 🔴 THE RULE, and the one line the engine has backwards. glTF 2.0 §3.5: right-handed, +Y up,
  // +Z forward, −X right. So `right = forward × up`, giving −facing along X — NOT `up × forward`.
  const rightSign = -facing as 1 | -1;

  // ── what the names claim ───────────────────────────────────────────────────
  let separationSum = 0;
  let pairs = 0;
  for (const pair of CORE_PAIRS) {
    const left = pos(pair.left);
    const right = pos(pair.right);
    if (!left || !right) continue;
    separationSum += right.getComponent(0) - left.getComponent(0);
    pairs++;
  }
  if (!pairs) {
    return {
      ...blank,
      feet,
      head: headCue,
      facing,
      cuesDisagree,
      rightSign,
      reason: 'No sided core slot has both halves named, so there is no labelling to check.'
    };
  }
  const separation = span > 0 ? Math.abs(separationSum / pairs) / span : 0;
  const labelledSign = sign(separationSum);
  if (separation < SEPARATION_FLOOR) {
    return {
      ...blank,
      feet,
      head: headCue,
      facing,
      cuesDisagree,
      rightSign,
      pairs,
      separation,
      labelledSign,
      reason:
        'The joints named Left and Right sit on top of each other, so which side they are on ' +
        'says nothing. Nothing to check the naming against.'
    };
  }

  return {
    verdict: labelledSign === rightSign ? 'ok' : 'mirrored',
    reason: '',
    feet,
    head: headCue,
    facing,
    cuesDisagree,
    rightSign,
    labelledSign,
    separation,
    pairs
  };
}

/** One bone's half of a sides swap: the name it carries now, and the one it should carry. */
export interface SideSwap {
  bone: THREE.Bone;
  /** CANONICAL, colon-bearing — what the file says and what the retarget matches on. */
  from: string;
  to: string;
}

/**
 * The renames that turn a mirrored rig into a correctly named one.
 *
 * Every sided core name present is paired with its opposite, INCLUDING a slot whose partner is
 * missing: a lone `RightFoot` on a mirrored rig is still the character's left foot, and leaving it
 * alone would be the one wrong name in an otherwise fixed skeleton.
 *
 * ⚠️ The result is a PERMUTATION and has to be applied as one — `LeftArm` and `RightArm` swap onto
 * each other, so applying it a name at a time leaves two bones sharing a name in between. Nothing
 * here mutates; the caller applies the whole plan and pushes ONE undo entry.
 */
export function sideSwapPlan(bones: THREE.Bone[]): SideSwap[] {
  const swap = new Map<string, string>();
  for (const pair of CORE_PAIRS) {
    swap.set(sanitizeName(pair.left), pair.right);
    swap.set(sanitizeName(pair.right), pair.left);
  }
  const plan: SideSwap[] = [];
  for (const bone of bones) {
    const to = swap.get(bone.name);
    if (!to) continue;
    plan.push({ bone, from: (bone.userData?.name as string | undefined) ?? bone.name, to });
  }
  return plan;
}
