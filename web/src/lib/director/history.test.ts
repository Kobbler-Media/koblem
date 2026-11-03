import { describe, expect, it } from 'vitest';
import type { DirectorClip } from '#lib/api/client.ts';
import { histSig, pushSnapshot, restore, snapshotOf, type Snapshot } from './history';

function clip(id: string, over: Partial<DirectorClip> = {}): DirectorClip {
  return { id, prompt: id, mode: 't2v', seconds: 4, keyframes: [], ...over };
}

/** A shot whose video comes from a prior render — i.e. one carrying an engine job id. */
const sourced = (jobId: string) =>
  clip('a', { mode: 'v2v_edit', v2v_source_kind: 'prior_render', v2v_prior_job_id: jobId });

/** The stack as the editor drives it: push, then step back. */
function stackOf(...snaps: Snapshot[]) {
  let stack: Snapshot[] = [];
  let index = -1;
  for (const s of snaps) ({ stack, index } = pushSnapshot(stack, index, s));
  return { stack, index };
}

describe('snapshots', () => {
  it('de-dupes a step that changed nothing', () => {
    const clips = [clip('a')];
    const { stack } = stackOf(
      snapshotOf({ clips, engine: 'ltx25' }),
      snapshotOf({ clips, engine: 'ltx25' })
    );
    expect(stack).toHaveLength(1);
  });

  /** The trap the engine field exists to close: a switch on a project with no prior-render source
   *  changes no clip field, so a clips-only signature coalesces it away — and then the FIRST step
   *  after it is the only one naming the new engine, leaving every older entry able to drag the
   *  engine backwards under an unrelated undo. */
  it('makes an engine switch its own step even when no clip changed', () => {
    const clips = [clip('a')];
    const { stack, index } = stackOf(
      snapshotOf({ clips, engine: 'h3' }),
      snapshotOf({ clips, engine: 'ltx25' })
    );
    expect(stack).toHaveLength(2);
    expect(index).toBe(1);
    expect(histSig(clips, 'h3')).not.toBe(histSig(clips, 'ltx25'));
  });

  it('drops the redo tail when a step is pushed after an undo', () => {
    const { stack } = stackOf(
      snapshotOf({ clips: [clip('a')], engine: 'ltx25' }),
      snapshotOf({ clips: [clip('a'), clip('b')], engine: 'ltx25' })
    );
    const after = pushSnapshot(stack, 0, snapshotOf({ clips: [clip('c')], engine: 'ltx25' }));
    expect(after.stack.map((s) => s.clips.map((c) => c.id))).toEqual([['a'], ['c']]);
    expect(after.index).toBe(1);
  });

  it('retains only the last `limit` steps', () => {
    let stack: Snapshot[] = [];
    let index = -1;
    for (let i = 0; i < 10; i++) {
      ({ stack, index } = pushSnapshot(
        stack,
        index,
        snapshotOf({ clips: [clip(`c${i}`)], engine: 'ltx25' }),
        4
      ));
    }
    expect(stack).toHaveLength(4);
    expect(stack[0].clips[0].id).toBe('c6');
    expect(index).toBe(3);
  });
});

describe('restore', () => {
  /** 🔴 The whole point. Undoing across an engine switch used to hand back the clips — prior-render
   *  job ids and all — while leaving the editor on the engine that cannot resolve them, and
   *  `compile.ts` emits that id straight off the clip. The engine comes back WITH them. */
  it('restores the engine that minted the ids it restores', () => {
    const { stack, index } = stackOf(
      snapshotOf({ clips: [sourced('ltx-dir-7')], engine: 'h3' }),
      // The picker's clear: the ids go, the engine moves.
      snapshotOf({
        clips: [clip('a', { mode: 'v2v_edit', v2v_source_kind: 'prior_render' })],
        engine: 'ltx25'
      })
    );
    const back = restore(stack[index - 1], 'ltx25');
    expect(back.clips[0].v2v_prior_job_id).toBe('ltx-dir-7');
    expect(back.engine).toBe('h3');
    expect(back.dropBank).toBe(true);
  });

  it('redoes back to the switch, ids cleared and engine moved together', () => {
    const { stack, index } = stackOf(
      snapshotOf({ clips: [sourced('ltx-dir-7')], engine: 'h3' }),
      snapshotOf({
        clips: [clip('a', { mode: 'v2v_edit', v2v_source_kind: 'prior_render' })],
        engine: 'ltx25'
      })
    );
    // The editor is sitting on the UNDONE step (h3) and steps forward onto the switch.
    const back = restore(stack[index], 'h3');
    expect(back.clips[0].v2v_prior_job_id).toBeUndefined();
    expect(back.engine).toBe('ltx25');
    expect(back.dropBank).toBe(true);
  });

  /** The other half: an ordinary undo must NOT throw away a working resume chain, so the drop is
   *  asked for only when the step actually moves the engine. */
  it('leaves the bank alone for a step that does not move the engine', () => {
    const back = restore(snapshotOf({ clips: [sourced('ltx-dir-7')], engine: 'ltx25' }), 'ltx25');
    expect(back.dropBank).toBe(false);
  });

  it('hands back containers the editor may mutate without touching the stack', () => {
    const snap = snapshotOf({
      clips: [clip('a', { keyframes: [{ seconds: 1, image_b64: 'x' }], references: [] })],
      engine: 'ltx25'
    });
    const back = restore(snap, 'ltx25');
    back.clips[0].prompt = 'edited';
    back.clips[0].keyframes[0].seconds = 3;
    expect(snap.clips[0].prompt).toBe('a');
    expect(snap.clips[0].keyframes[0].seconds).toBe(1);
  });
});
