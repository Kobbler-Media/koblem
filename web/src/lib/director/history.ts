/**
 * The editor's undo stack: what one step HOLDS, and what restoring it means.
 *
 * Kept out of the component because both answers are correctness questions rather than UI ones —
 * see `restore`, where getting the second one wrong resurrected ids the engine picker had just
 * dropped.
 *
 * A snapshot holds the cloned clip OBJECTS, never a serialisation of them. Clips carry base64
 * assets — storyboard stills, i2v scene images, keyframe pins, whole v2v source videos, per-shot
 * audio — and those are immutable strings, so a structured clone shares each one by reference.
 * `JSON.stringify` instead materialises a fresh multi-megabyte copy of every asset in the project
 * — once per pause in typing (the 450ms commit debounce), with 80 of the results retained. That is
 * what made the editor drag on projects with only a handful of shots: the cost tracked the assets
 * attached to them, not the shot count, and the retained stack multiplied it by 80.
 */

import type { DirectorClip, DirectorEngine } from '#lib/api/client.ts';
import { clipCastSignature } from './characters';

/**
 * The document state one undo step restores.
 *
 * 🔴 The ENGINE is in here, and it is not a preference riding along with the clips — it is what
 * makes them readable. `clip.v2v_prior_job_id` is an opaque id in one engine's job store and a 404
 * in the other's, so a clip list and the engine that minted its ids are a single fact. Held apart,
 * undo restored exactly the half that compiles into the render request (`compile.ts` emits
 * `{ kind: 'prior_render', job_id }` straight off the clip) and none of the half that would have
 * made it visible, which is a foreign id shipped to an engine that has never held it.
 */
export interface EditorState {
  clips: DirectorClip[];
  engine: DirectorEngine;
}

export interface Snapshot extends EditorState {
  sig: string;
}

/**
 * Cheap identity for de-duping consecutive snapshots. Covers every field the editor can change
 * and reduces each asset to its LENGTH — swapping an asset for a different one of exactly the
 * same size is indistinguishable here, which costs at most one redundant undo step and never
 * touches the payload bytes.
 *
 * 🔴 The engine is part of the identity for the same reason it is part of the state. A switch on a
 * project with no prior-render source changes no clip field, so without it the switch coalesces
 * away — and the next ordinary edit then pushes the first step that names the new engine, leaving
 * a stack whose older entries silently drag the engine backwards when they are restored.
 */
export function histSig(clips: DirectorClip[], engine: DirectorEngine): string {
  const n = (v: string | null | undefined) => (v ? v.length : 0);
  return [
    engine,
    ...clips.map((c) =>
      [
        c.id,
        c.prompt,
        c.idea ?? '',
        c.gist ?? '',
        c.mode,
        c.seconds,
        c.model ?? '',
        c.new_scene ? 1 : 0,
        c.v2v_source_kind ?? '',
        c.v2v_prior_job_id ?? '',
        c.v2v_prior_segment ?? '',
        c.v2v_strength ?? '',
        c.pin_strength ?? '',
        c.audio_kind ?? '',
        n(c.image_b64),
        n(c.snapshot_b64),
        n(c.v2v_source_b64),
        n(c.audio_b64),
        c.references?.length ?? 0,
        // Enabling/disabling a character changes what renders, so it has to be an undo step. The
        // ids, not the count: swapping one character for another must not look identical — and
        // `*` for the implicit all-on default, so materialising it into an explicit list is still
        // a visible change rather than being coalesced away.
        clipCastSignature(c),
        c.keyframes.map((k) => `${k.seconds}:${k.label ?? ''}:${n(k.image_b64)}`).join(',')
      ].join('')
    )
  ].join('');
}

export function snapshotOf(state: EditorState): Snapshot {
  return { clips: state.clips, engine: state.engine, sig: histSig(state.clips, state.engine) };
}

/** How many steps are retained. Assets are shared by reference, so the cost is the containers. */
export const HISTORY_LIMIT = 80;

/** Push a step, folding a no-op edit into the one before it and trimming to {@link HISTORY_LIMIT}. */
export function pushSnapshot(
  stack: Snapshot[],
  index: number,
  snap: Snapshot,
  limit = HISTORY_LIMIT
): { stack: Snapshot[]; index: number } {
  if (index >= 0 && stack[index]?.sig === snap.sig) return { stack, index };
  const next = [...stack.slice(0, index + 1), snap].slice(-limit);
  return { stack: next, index: next.length - 1 };
}

export interface Restored extends EditorState {
  /**
   * This step moved the engine, so everything the editor holds that names the engine it is moving
   * AWAY from has to go with it — the tier resume targets, the banked prompt prefixes and the
   * prior renders offered as v2v sources.
   *
   * 🔴 Those are NOT in the snapshot, and deliberately: they are minted asynchronously by a render
   * rather than authored, so a stack entry taken before one would resurrect the state from before
   * a render that has since happened. Dropping them is the direction that costs a re-render;
   * restoring them is the direction that ships a 404. The durable rows re-derive both resume ids
   * (`bankedResumeTarget`) and the prompts on the next read, and the prior-render offer is rebuilt
   * on project load.
   */
  dropBank: boolean;
}

/**
 * Read one step back out, against the engine the editor is on NOW.
 *
 * Clones on the way out, so the stack entry stays pristine when the restored clips are then edited
 * (and re-applied by a redo). Fresh containers, but the base64 assets are carried across by
 * reference — a structuredClone here would undo the whole point of the snapshot shape above.
 */
export function restore(snap: Snapshot, liveEngine: DirectorEngine): Restored {
  return {
    clips: snap.clips.map((c) => ({
      ...c,
      keyframes: c.keyframes.map((k) => ({ ...k })),
      ...(c.references ? { references: c.references.map((r) => ({ ...r })) } : {})
    })),
    engine: snap.engine,
    dropBank: snap.engine !== liveEngine
  };
}
