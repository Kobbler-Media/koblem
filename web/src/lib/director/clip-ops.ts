/**
 * The structural edits to the shot list — add, delete, duplicate, merge, split — as transforms over
 * a plain clip array.
 *
 * They are here rather than in the editor because each one has to decide what happens to state that
 * cannot simply be copied along: a merged shot can hold only ONE audio clip and one scene image, a
 * split shot's second half must not replay the first half's audio, and every one of them has to
 * leave a valid selection behind. Those are the rules that turn into data loss when they are wrong,
 * and none of them is visible in a diff of the markup.
 *
 * Every function takes and returns PLAIN clips: the caller snapshots reactive state on the way in,
 * so the deep clones here (which really do copy bytes) are the only ones that happen.
 */

import { uid } from '@koblibs/ui';
import type { DirectorClip } from '#lib/api/client.ts';

export interface ClipSelection {
  clips: DirectorClip[];
  selectedIds: string[];
}

/** Insert a fresh shot after the primary selection, else at the end. */
export function insertClip(
  clips: DirectorClip[],
  primaryId: string | null,
  seconds: number
): ClipSelection {
  const c: DirectorClip = { id: uid(), prompt: '', mode: 't2v', seconds, keyframes: [] };
  // A primary id that is no longer in the list is not a position — it has to mean "no selection"
  // and append, or findIndex's -1 puts the new shot at the head.
  const after = primaryId ? clips.findIndex((x) => x.id === primaryId) : -1;
  const at = after >= 0 ? after + 1 : clips.length;
  const next = clips.slice();
  next.splice(at, 0, c);
  return { clips: next, selectedIds: [c.id] };
}

/** Drop every selected shot, and land the selection on whatever takes the first one's place. */
export function deleteClips(clips: DirectorClip[], selectedIds: string[]): ClipSelection {
  const selected = new Set(selectedIds);
  const firstIdx = clips.findIndex((c) => selected.has(c.id));
  const next = clips.filter((c) => !selected.has(c.id));
  const at = Math.min(firstIdx, next.length - 1);
  return { clips: next, selectedIds: next[at] ? [next[at].id] : [] };
}

/** Copy each selected shot immediately after itself, keeping order, and select the copies. */
export function duplicateClips(clips: DirectorClip[], selectedIds: string[]): ClipSelection {
  const selected = new Set(selectedIds);
  const next = clips.slice();
  const newIds: string[] = [];
  for (let i = next.length - 1; i >= 0; i--) {
    if (selected.has(next[i].id)) {
      const copy: DirectorClip = { ...structuredClone(next[i]), id: uid() };
      next.splice(i + 1, 0, copy);
      newIds.unshift(copy.id);
    }
  }
  return { clips: next, selectedIds: newIds };
}

/**
 * Fold a contiguous run into one shot. Returns null when the run is broken.
 *
 * The merged shot is ONE span, so it can only carry the first shot's scene image, v2v source and
 * audio — the others have nowhere to start from. Keyframes are the exception: they are re-timed onto
 * the joined span rather than dropped.
 */
export function mergeClips(clips: DirectorClip[], selectedIds: string[]): ClipSelection | null {
  if (selectedIds.length < 2) return null;
  const selected = new Set(selectedIds);
  const idxs = clips.map((c, i) => (selected.has(c.id) ? i : -1)).filter((i) => i >= 0);
  if (!idxs.every((v, k) => k === 0 || v === idxs[k - 1] + 1)) return null;
  const first = idxs[0];
  const group = idxs.map((i) => clips[i]);
  let offset = 0;
  const keyframes = group.flatMap((c) => {
    const ks = c.keyframes.map((k) => ({ ...k, seconds: k.seconds + offset }));
    offset += c.seconds;
    return ks;
  });
  const merged: DirectorClip = {
    // 🔴 The FIRST member's id, not a fresh one. `packGenerations` claims a stored H3 block by
    // clip-id overlap, so minting an id here orphans the first shot's authored prompt — the same
    // rule `splitClipAt` follows on the other side of the operation.
    id: group[0].id,
    prompt: group
      .map((c) => c.prompt.trim())
      .filter(Boolean)
      .join(' — '),
    mode: group[0].mode,
    seconds: group.reduce((s, c) => s + c.seconds, 0),
    model: group[0].model,
    image_b64: group[0].image_b64 ?? null,
    v2v_source_kind: group[0].v2v_source_kind,
    v2v_source_b64: group[0].v2v_source_b64 ?? null,
    v2v_prior_job_id: group[0].v2v_prior_job_id ?? null,
    v2v_prior_segment: group[0].v2v_prior_segment,
    v2v_strength: group[0].v2v_strength,
    pin_strength: group[0].pin_strength,
    audio_b64: group[0].audio_b64 ?? null,
    audio_kind: group[0].audio_kind,
    keyframes
  };
  const next = clips.slice();
  next.splice(first, group.length, merged);
  return { clips: next, selectedIds: [merged.id] };
}

/**
 * Cut ONE named shot in two at `atSeconds`, and give the second half `text`.
 *
 * The "something changes here → make it a cut" gesture. Separate from {@link splitClipAt} rather
 * than a wrapper around it, and every difference is load-bearing:
 *
 * - 🔴 **The first half KEEPS the original id.** `splitClipAt` mints two, which is harmless on LTX
 *   and destructive on H3: `packGenerations` re-attaches a generation's authored block by clip-id
 *   OVERLAP, so splitting the only clip of a generation whose ids all changed orphans the block —
 *   the sections, the soundscape and the prompt the author wrote silently become an unclaimed
 *   stored entry and the generation reseeds itself empty.
 * - **The last-frame image MOVES to the second half.** `h3_end_image` anchors the end of the span,
 *   and after a split the end of the span is the second half. Leaving it on the first would pin a
 *   frame to a moment that is now in the middle.
 *
 * Returns null when the moment is outside the shot or within 0.1s of either end — a cut there is a
 * no-op the author did not mean, and 0.1s is also the resolution both halves are rounded to.
 */
export function splitClipForCut(
  clips: DirectorClip[],
  clipId: string,
  atSeconds: number,
  text: string
): ClipSelection | null {
  const i = clips.findIndex((c) => c.id === clipId);
  if (i < 0) return null;
  const c = clips[i];
  // ⚠️ Tested on the RAW moment, before the tenth-second rounding, and with the same strict
  // comparison `splitClipAt` uses. Rounding first turns 0.05 s into 0.1 s and lets a cut through at
  // the very edge — a second half that is the whole shot and a first half that is a frame.
  if (!(atSeconds > 0.1 && atSeconds < c.seconds - 0.1)) return null;
  const firstDur = Math.round(atSeconds * 10) / 10;
  const secondDur = Math.round((c.seconds - atSeconds) * 10) / 10;
  const a: DirectorClip = { ...structuredClone(c), seconds: firstDur };
  const b: DirectorClip = { ...structuredClone(c), id: uid(), seconds: secondDur };
  a.keyframes = c.keyframes.filter((k) => k.seconds <= firstDur);
  b.keyframes = c.keyframes
    .filter((k) => k.seconds > firstDur)
    .map((k) => ({ ...k, seconds: k.seconds - firstDur }));
  // Anchored to the shot's own t=0, so carrying them across would open the second half on the first
  // half's opening frame and replay its audio over it — same rule as `splitClipAt`.
  b.image_b64 = null;
  b.v2v_source_b64 = null;
  b.audio_b64 = null;
  b.audio_kind = undefined;
  // The end of the span moved. Both sides, or the frame is pinned twice.
  b.h3_end_image = c.h3_end_image ?? null;
  a.h3_end_image = null;
  b.prompt = text;
  // The gist is what the shot list and the timeline draw, so a new cut with a body and no gist
  // shows as an unnamed block until somebody summarises it.
  b.gist = text;
  const next = clips.slice();
  next.splice(i, 1, a, b);
  return { clips: next, selectedIds: [b.id] };
}

/**
 * Cut the shot the playhead sits inside into two, at that moment. Returns null when the playhead is
 * outside every shot, or within 0.1s of a boundary (a cut there is a no-op the user did not mean).
 *
 * The second half deliberately loses the scene image, the v2v source and the audio clip: all three
 * are anchored to the shot's own t=0, so carrying them across would start the second half on the
 * first half's opening frame and replay its audio over it.
 */
export function splitClipAt(clips: DirectorClip[], playhead: number): ClipSelection | null {
  let t = 0;
  for (let i = 0; i < clips.length; i++) {
    const c = clips[i];
    const end = t + c.seconds;
    if (playhead > t + 0.1 && playhead < end - 0.1) {
      const firstDur = Math.round((playhead - t) * 10) / 10;
      const secondDur = Math.round((end - playhead) * 10) / 10;
      // 🔴 The first half KEEPS the original id. This used to mint two fresh ones, which is
      // harmless on LTX and destructive on H3: `packGenerations` re-attaches a generation's
      // authored block by clip-id OVERLAP, so splitting the only clip of a generation whose ids all
      // changed orphans the block — the sections, the soundscape and the prompt the author wrote
      // become an unclaimed stored entry and the generation reseeds itself empty. Keeping it is
      // also the better answer on LTX, where takes and renders are keyed by clip id and would
      // otherwise be stranded by a split. `splitClipForCut` has always done this; this function was
      // the one that had not caught up.
      const a: DirectorClip = { ...structuredClone(c), seconds: firstDur };
      const b: DirectorClip = { ...structuredClone(c), id: uid(), seconds: secondDur };
      a.keyframes = c.keyframes.filter((k) => k.seconds <= firstDur);
      b.keyframes = c.keyframes
        .filter((k) => k.seconds > firstDur)
        .map((k) => ({ ...k, seconds: k.seconds - firstDur }));
      b.image_b64 = null;
      b.v2v_source_b64 = null;
      b.audio_b64 = null;
      b.audio_kind = undefined;
      // 🔴 `h3_end_image` anchors the END of the span, and after a cut the end of the span is the
      // SECOND half; leaving a copy on the first would pin a frame to a moment that is now in the
      // middle of the shot.
      b.h3_end_image = a.h3_end_image ?? null;
      a.h3_end_image = null;
      const next = clips.slice();
      next.splice(i, 1, a, b);
      return { clips: next, selectedIds: [a.id] };
    }
    t = end;
  }
  return null;
}
