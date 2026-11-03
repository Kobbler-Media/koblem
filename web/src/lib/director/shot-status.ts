/**
 * Per-shot render state, approximated from the banked continue-prefix.
 *
 * There is no per-clip job state in the wire model, so this is the frontend's own reading of it:
 * matches the banked prompt = rendered, differs = edited since, past the banked range / no prompt =
 * not rendered, a render of that tier in flight = busy.
 *
 * INDEX SPACE is the whole reason this is worth isolating. `compile()` emits one segment per RAW
 * clip, but the API drops empty-prompt segments before the engine ever sees them (`seg_specs` in
 * api/src/routes/ltx_video.rs), so the engine's `segment_index` — and therefore every `partials[]`
 * entry and every persisted row — is the PROMPT-FILTERED index, not the raw clip index. Mixing the
 * two silently attributes footage to the wrong shot.
 */

import type { DirectorClip } from '#lib/api/client.ts';
import type { ShotRenderStatus } from './shot-list';

interface WithPrompt {
  id: string;
  prompt: string;
}

/** The shots the engine will actually see, in order. */
export const promptedClips = <T extends WithPrompt>(clips: T[]): T[] =>
  clips.filter((c) => c.prompt.trim());

/** A shot's index in engine space, or -1 when it carries no prompt. */
export const promptedIndexOf = <T extends WithPrompt>(clips: T[], id: string): number =>
  promptedClips(clips).findIndex((c) => c.id === id);

/** One tier's view of what has been rendered. */
export interface TierProgress {
  /** The prompts that tier's last render banked, index-aligned in engine space. */
  banked: string[];
  /**
   * The shots already delivered by a render of this tier that is streaming into THIS editor, or
   * null when nothing of ours is in flight. A render belonging to another project must pass null:
   * its progress says nothing about our shots.
   */
  live: { index: number }[] | null;
  /**
   * `banked[]` is one entry per RENDER UNIT, and that unit is bigger than a shot.
   *
   * 🔴 True on MiniMax-H3, where a unit is a GENERATION and its banked entry is the composed
   * six-section schema block for every shot packed into it. That string is not this shot's prompt
   * and never equals it, so the usual equality test reports `stale` on every shot of every
   * generation forever — a dot that is always red is a dot nobody reads.
   *
   * So when this is set, a banked entry at the index means RENDERED and its absence means NOT
   * rendered, and staleness is not claimed. "You edited this since it rendered" is genuinely not
   * decidable at shot granularity here: the shot's text is one input to a prompt the composer
   * built, not a substring of it.
   */
  bankedIsUnitScoped?: boolean;
}

/**
 * @param engineIndex The clip's index in ENGINE space, when the caller knows a mapping this module
 *   cannot derive. Omit for LTX, where engine space IS the prompt-filtered shot index. Pass it on
 *   MiniMax-H3, where the render unit is the GENERATION and several shots share one index — a
 *   prompt-filtered index there addresses a different generation's footage on any project whose
 *   generations hold more than one shot, which is the packer's normal case.
 */
export function shotStatusIn(
  clips: WithPrompt[],
  clip: WithPrompt,
  tier: TierProgress,
  engineIndex?: number
): ShotRenderStatus {
  const p = clip.prompt.trim();
  if (!p) return 'none';
  const idx = engineIndex ?? promptedIndexOf(clips, clip.id);
  // Mid-render, a shot whose progressive partial already landed is done.
  if (tier.live) return tier.live.some((x) => x.index === idx) ? 'ok' : 'busy';
  if (tier.banked.length === 0) return 'none';
  if (idx < 0 || idx >= tier.banked.length) return 'none';
  if (tier.bankedIsUnitScoped) return tier.banked[idx] ? 'ok' : 'none';
  return tier.banked[idx] === p ? 'ok' : 'stale';
}

/** Resolve an engine-space index back to the clip it belongs to. */
export const clipAtPromptedIndex = (
  clips: DirectorClip[],
  filteredIdx: number
): DirectorClip | null => promptedClips(clips)[filteredIdx] ?? null;

/**
 * The RAW editor number to LABEL an engine-space shot with — what the stage slate shows and what
 * was clicked in the timeline. Falls back to the engine number when the clip is gone.
 */
export function shotNumberAtPromptedIndex(clips: DirectorClip[], filteredIdx: number): number {
  const c = clipAtPromptedIndex(clips, filteredIdx);
  return c ? clips.indexOf(c) + 1 : filteredIdx + 1;
}
