/**
 * What each MiniMax-H3 reference role means, in the words the picker shows.
 *
 * 🔴 **This is the control that decides the prompt's task-type prefix.** koblem's
 * `h3_prompting.rs` derives the `summary:` bracket — `[video continuation + audio reference]` and
 * so on — from these roles and nothing else. Left unset, the backend falls back to the weakest
 * claim available for each kind, which makes `video editing`, `video continuation` and
 * `audio reuse` unreachable from the UI: every prompt comes out `[reference generation …]`
 * regardless of what the author actually attached.
 *
 * It is genuinely not derivable from the kind. `GUIDE-ref:143` is explicit that the mere presence
 * of a video or an audio file creates no task type — a clip that supplies only camera movement or
 * cutting rhythm is `reference generation`, and only a clip that is directly edited or continued
 * earns `video editing` / `video continuation`. So the question "is this video the thing we are
 * continuing, or is it structural guidance?" is one only the author can answer, and the prompt
 * says something materially different either way.
 *
 * Kept out of `engine.ts` because it is H3 product vocabulary rather than a capability, and out of
 * `h3-prompt.ts` because the composer never reads it — the roles reach the model through koblem's
 * prompt endpoints, as text, not through the assembler.
 */

import type {
  DirectorH3Reference,
  H3ReferenceKind,
  H3ReferenceRole,
  H3TaskType
} from '$lib/api/client';

export interface H3RoleOption {
  value: H3ReferenceRole;
  /** What the picker shows. */
  label: string;
  /** One line under it — what the prompt will claim as a result. */
  blurb: string;
  /** Which reference kinds this role is legal for. */
  kinds: H3ReferenceKind[];
}

export const H3_ROLES: H3RoleOption[] = [
  {
    value: 'identity_source',
    label: 'Identity / style',
    blurb:
      'A face, a wardrobe, a look to carry into the target. Cited inside a <Subject> definition rather than pinned as a frame.',
    kinds: ['image', 'video']
  },
  {
    value: 'structure_source',
    label: 'Structure only',
    blurb:
      'Camera movement, cutting rhythm, composition — guidance, not content. This is the role a clip gets when it is NOT being edited or continued.',
    kinds: ['image', 'video']
  },
  {
    value: 'storyboard',
    label: 'Storyboard',
    blurb: 'A layout to follow. Steers framing without being a frame the target has to match.',
    kinds: ['image']
  },
  {
    value: 'frame_anchor',
    label: 'Frame anchor',
    blurb: 'A concrete frame somewhere inside the target — makes this a keyframe-completion task.',
    kinds: ['image']
  },
  {
    value: 'first_frame',
    label: 'First frame',
    blurb: 'The target opens on this image.',
    kinds: ['image']
  },
  {
    value: 'last_frame',
    label: 'Last frame',
    blurb: 'The target closes on this image.',
    kinds: ['image']
  },
  {
    value: 'edit_source',
    label: 'Edit this clip',
    blurb: 'The source video is directly modified — a video-editing task, not a reference.',
    kinds: ['video']
  },
  {
    value: 'continuation_source',
    label: 'Continue from this clip',
    blurb:
      'The target resumes, extends or transitions out of this video. Also the only role the engine itself acts on.',
    kinds: ['video']
  },
  {
    value: 'audio_reuse',
    label: 'Reuse this audio',
    blurb: 'The signal itself is reused, whole or in part.',
    kinds: ['audio']
  },
  {
    value: 'voice_timbre',
    label: 'Voice / timbre reference',
    blurb: 'Only the voice quality and delivery are referenced. The signal is not copied.',
    kinds: ['audio']
  },
  {
    value: 'audio_continuity',
    label: 'Carry the bed on',
    blurb:
      'The new audio continues this track’s ambience and character without copying it — the audio half of a continuation.',
    kinds: ['audio']
  }
];

export const rolesFor = (kind: H3ReferenceKind): H3RoleOption[] =>
  H3_ROLES.filter((r) => r.kinds.includes(kind));

/**
 * The role a freshly-added reference starts on.
 *
 * The weakest claim for its kind, on purpose: an author who never opens the picker gets a prompt
 * that under-claims rather than one asserting the target is a continuation of a clip that is only
 * sitting there for its lighting.
 */
export function defaultRoleFor(kind: H3ReferenceKind): H3ReferenceRole {
  return kind === 'audio' ? 'voice_timbre' : 'identity_source';
}

export const roleLabel = (role: H3ReferenceRole | undefined): string =>
  H3_ROLES.find((r) => r.value === role)?.label ?? 'unset';

/**
 * The task type ONE asset earns by playing ONE role (`GUIDE-ref:132-139`).
 *
 * 🔴 **This map is where `reference generation` and `keyframe completion` are kept mutually
 * exclusive, and it is exclusive BY CONSTRUCTION because it is per asset.** The guide's rule is not
 * "a summary may not contain both" — a summary legitimately contains both when one image is a first
 * frame and another supplies a wardrobe. It is that a single asset cannot be both a concrete frame
 * anchor and generation guidance: `reference generation` is defined as guidance *"without serving
 * as a concrete frame"* (`GUIDE-ref:135`). A role is a single value, so an asset gets a single task
 * type, and the illegal state is unreachable rather than merely validated against.
 *
 * ⚠️ It is a total map on purpose. An `H3ReferenceRole` added without a task type would otherwise
 * fall out of the derivation silently, and the symptom is a bracket that under-claims — which the
 * model does not refuse, it just conditions more weakly.
 */
export const H3_ROLE_TASK_TYPE: Record<H3ReferenceRole, H3TaskType> = {
  identity_source: 'reference generation',
  structure_source: 'reference generation',
  storyboard: 'reference generation',
  frame_anchor: 'keyframe completion',
  first_frame: 'keyframe completion',
  last_frame: 'keyframe completion',
  edit_source: 'video editing',
  continuation_source: 'video continuation',
  audio_reuse: 'audio reuse',
  voice_timbre: 'audio reference',
  audio_continuity: 'audio reference'
};

/** Does this role make its asset a concrete frame of the target, rather than guidance? */
export const isFrameRole = (role: H3ReferenceRole | undefined): boolean =>
  role === 'first_frame' || role === 'last_frame' || role === 'frame_anchor';

/**
 * The task types a set of in-scope references actually justifies — one per asset, de-duplicated.
 *
 * Order is NOT canonicalised here; `joinTaskTypes` does that at emission time, and doing it twice
 * in two places is how the two end up disagreeing.
 *
 * ⚠️ A reference with no role contributes NOTHING rather than a guess. The weakest-claim fallback
 * lives on the reference itself (`defaultRoleFor`, applied on add), so an unset role here means a
 * document written before roles existed — and inventing `reference generation` for it would put a
 * claim in the bracket that no asset in the request supports.
 */
export function h3TaskTypesForRefs(refs: readonly DirectorH3Reference[]): H3TaskType[] {
  const out = new Set<H3TaskType>();
  for (const r of refs) if (r.role) out.add(H3_ROLE_TASK_TYPE[r.role]);
  return [...out];
}

/**
 * Task types the summary's bracket claims that no asset in scope plays the role for.
 *
 * The half of the per-asset rule that a map alone cannot enforce: the bracket is free text in the
 * block, so an author (or an LLM) can write `[reference generation + keyframe completion]` over a
 * single first-frame picture. That is exactly the pair `GUIDE-ref:135` forbids for one asset, and
 * the symptom is not a refusal — the model simply reads a relationship the request does not
 * contain.
 *
 * ⚠️ Returns only UNJUSTIFIED claims, never missing ones. A bracket that under-claims is the
 * author's business (the guide's types are a description, not a checklist), and a warning on every
 * generation whose roles were left at their defaults would be noise.
 */
export function h3UnjustifiedTaskTypes(
  refs: readonly DirectorH3Reference[],
  claimed: readonly H3TaskType[]
): H3TaskType[] {
  // 🔴 Silence when NOTHING in scope carries a role. That is a document written before roles
  // existed, not a mis-claim, and accusing every one of its task types would put an amber note on
  // every generation of every old project — which is how a warning tier stops being read.
  if (!refs.some((r) => r.role)) return [];
  const justified = new Set(h3TaskTypesForRefs(refs));
  return claimed.filter((t) => !justified.has(t));
}
