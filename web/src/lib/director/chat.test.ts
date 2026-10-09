import { describe, expect, it } from 'vitest';
import { DIRECTOR_ADD_AT_START, type DirectorChatAction } from '$lib/api/client';
import type { DirectorClip, DirectorH3Generation, StoryContext } from '$lib/api/client';
import {
  applyChatActions,
  applyH3ChatActions,
  chatHistory,
  h3RescaleBeats,
  revertChatEdit,
  revertH3ChatEdit,
  type ChatMsg,
  type ChatTarget,
  type H3ChatTarget
} from './chat';
import { needsH3PromptSeed } from './h3-document';
import { packGenerations } from './h3-generations';

function clip(id: string, over: Partial<DirectorClip> = {}): DirectorClip {
  return { id, prompt: `prompt ${id}`, mode: 't2v', seconds: 4, keyframes: [], ...over };
}

function target(over: Partial<ChatTarget> = {}): ChatTarget {
  const story: StoryContext = { backstory: 'b', tone: 't', rules: 'r', references: [] };
  const t: ChatTarget = {
    clips: [clip('a'), clip('b'), clip('c')],
    story,
    defaultSeconds: 4,
    selectedIds: ['a'],
    select: (ids) => {
      t.selectedIds = ids;
    },
    detach: (c) => structuredClone(c),
    ...over
  };
  return t;
}

const act = (a: DirectorChatAction) => a;

describe('applyChatActions — field edits', () => {
  it('writes the field and records what it replaced', () => {
    const t = target();
    const [edit] = applyChatActions(t, [act({ type: 'set_prompt', shot: 'b', value: 'new' })]);
    expect(t.clips[1].prompt).toBe('new');
    expect(edit).toMatchObject({
      kind: 'field',
      field: 'prompt',
      before: 'prompt b',
      after: 'new'
    });
    expect(edit.label).toBe('Shot 2 · Prompt');
  });

  it('ignores an action naming a shot that is not there', () => {
    const t = target();
    expect(applyChatActions(t, [act({ type: 'set_gist', shot: 'gone', value: 'x' })])).toEqual([]);
  });

  it('refuses a duration that is not a positive number', () => {
    const t = target();
    expect(applyChatActions(t, [act({ type: 'set_seconds', shot: 'a', value: 0 })])).toEqual([]);
    expect(applyChatActions(t, [act({ type: 'set_seconds', shot: 'a', value: 'x' })])).toEqual([]);
    expect(t.clips[0].seconds).toBe(4);
  });
});

describe('applyChatActions — structural edits', () => {
  /** The point of naming targets by id: an insert earlier in the same batch must not shift a later
   *  action onto a different shot. */
  it('keeps a batch aligned across an insert', () => {
    const t = target();
    applyChatActions(t, [
      act({ type: 'add_shot', after: DIRECTOR_ADD_AT_START, prompt: 'new head' }),
      act({ type: 'set_prompt', shot: 'c', value: 'edited c' })
    ]);
    expect(t.clips.map((c) => c.prompt)).toEqual(['new head', 'prompt a', 'prompt b', 'edited c']);
  });

  it('appends when the anchor shot is unknown', () => {
    const t = target();
    applyChatActions(t, [act({ type: 'add_shot', after: 'gone', prompt: 'tail' })]);
    expect(t.clips[3].prompt).toBe('tail');
  });

  it('gives a new shot the project default duration and selects it', () => {
    const t = target({ defaultSeconds: 7 });
    applyChatActions(t, [act({ type: 'add_shot', after: 'a', idea: ' an idea ' })]);
    expect(t.clips[1]).toMatchObject({ seconds: 7, prompt: '', idea: 'an idea' });
    expect(t.selectedIds).toEqual([t.clips[1].id]);
  });

  it('moves the selection off a deleted shot but leaves an unaffected one alone', () => {
    const t = target({ selectedIds: ['a'] });
    applyChatActions(t, [act({ type: 'delete_shot', shot: 'a' })]);
    expect(t.selectedIds).toEqual(['b']);

    const u = target({ selectedIds: ['a'] });
    applyChatActions(u, [act({ type: 'delete_shot', shot: 'c' })]);
    expect(u.selectedIds).toEqual(['a']);
  });

  it('only writes the three story fields it knows', () => {
    const t = target();
    const [edit] = applyChatActions(t, [act({ type: 'set_story', field: 'tone', value: 'noir' })]);
    expect(t.story.tone).toBe('noir');
    expect(edit).toMatchObject({ kind: 'story', before: 't', after: 'noir' });
    expect(applyChatActions(t, [act({ type: 'set_story', value: 'x' })])).toEqual([]);
  });
});

describe('revertChatEdit', () => {
  it('puts a field back and will not fire twice', () => {
    const t = target();
    const [edit] = applyChatActions(t, [act({ type: 'set_prompt', shot: 'b', value: 'new' })]);
    expect(revertChatEdit(t, edit)).toBe(true);
    expect(t.clips[1].prompt).toBe('prompt b');
    expect(revertChatEdit(t, edit)).toBe(false);
  });

  it('removes a shot the chat added', () => {
    const t = target();
    const [edit] = applyChatActions(t, [act({ type: 'add_shot', after: 'a', prompt: 'x' })]);
    revertChatEdit(t, edit);
    expect(t.clips.map((c) => c.id)).toEqual(['a', 'b', 'c']);
  });

  /** A deleted shot comes back WHOLE, at its own position — the detached copy is why. */
  it('restores a deleted shot in place, with its media', () => {
    const t = target({
      clips: [
        clip('a'),
        clip('b', { image_b64: 'img', keyframes: [{ image_b64: 'k', seconds: 1 }] })
      ]
    });
    const [edit] = applyChatActions(t, [act({ type: 'delete_shot', shot: 'b' })]);
    expect(t.clips).toHaveLength(1);
    revertChatEdit(t, edit);
    expect(t.clips[1]).toMatchObject({ id: 'b', image_b64: 'img' });
    expect(t.clips[1].keyframes).toEqual([{ image_b64: 'k', seconds: 1 }]);
  });

  it('clamps a restore whose position no longer exists', () => {
    const t = target();
    const [edit] = applyChatActions(t, [act({ type: 'delete_shot', shot: 'c' })]);
    t.clips.splice(0, t.clips.length);
    revertChatEdit(t, edit);
    expect(t.clips.map((c) => c.id)).toEqual(['c']);
  });

  it('puts a story field back', () => {
    const t = target();
    const [edit] = applyChatActions(t, [act({ type: 'set_story', field: 'rules', value: 'x' })]);
    revertChatEdit(t, edit);
    expect(t.story.rules).toBe('r');
  });
});

describe('chatHistory', () => {
  /** Without the marker the model re-emits edits it already made, and the editor applies them twice. */
  it('marks the edits an assistant turn already applied', () => {
    const messages: ChatMsg[] = [
      { role: 'user', content: 'make it night' },
      {
        role: 'assistant',
        content: 'done',
        edits: [
          {
            id: '1',
            label: 'Shot 1 · Prompt',
            kind: 'field',
            before: '',
            after: '',
            reverted: false
          }
        ]
      }
    ];
    const out = chatHistory(messages);
    expect(out[0].content).toBe('make it night');
    expect(out[1].content).toContain('[Done — already applied: Shot 1 · Prompt.]');
  });

  it('leaves a turn that changed nothing untouched', () => {
    expect(chatHistory([{ role: 'assistant', content: 'no' }])[0].content).toBe('no');
  });
});

// ── H3: a chat action names a RENDER ─────────────────────────────────────────

/**
 * A t2va block with one beat in it. `[Shot 1]` is a BEAT, not a cut the timeline packs — a whole
 * block is one render, whatever number of marks it carries.
 */
const genBlock = (body: string, sound = 'Room tone.') =>
  `integrated_multimodal_description: [Shot 1] ${body}\n\noverall_soundscape: ${sound}\n\nnon_diegetic_music: N/A`;

/**
 * A stand-in for the workspace's own generation writers.
 *
 * 🔴 The packing is REAL (`packGenerations` over the live clip array), because the failures these
 * actions have to survive are packing failures: a new render being merged into its neighbour, a
 * deleted one freeing the budget so the next one's boundary dissolves. A stubbed packing would
 * mock away the only thing worth testing.
 */
function h3Target(clips: DirectorClip[], stored: DirectorH3Generation[] = []) {
  const warnings: string[] = [];
  const story: StoryContext = { backstory: 'b', tone: 't', rules: 'r', references: [] };
  const t: H3ChatTarget = {
    clips,
    story,
    defaultSeconds: 4,
    selectedIds: [clips[0]?.id ?? ''],
    select: (ids) => {
      t.selectedIds = ids;
    },
    detach: (c) => structuredClone(c),
    get generations() {
      return packGenerations({ clips: t.clips, references: [], stored });
    },
    maxSeconds: 362 / 24,
    updateGeneration(id, patch) {
      const gen = t.generations.find((g) => g.id === id);
      const at = stored.findIndex((g) => g.id === id);
      if (at >= 0) stored[at] = { ...stored[at], ...patch };
      else stored.push({ id, clip_ids: gen?.shots.map((s) => s.clip.id) ?? [], ...patch });
    },
    // Matches production exactly: the block is the only store, so writing it writes nothing else.
    setGenerationPrompt(id, block) {
      t.updateGeneration(id, { prompt: block });
    },
    putGeneration(doc) {
      const at = stored.findIndex((g) => g.id === doc.id);
      if (at >= 0) stored[at] = doc;
      else stored.push(doc);
    },
    dropGeneration(id) {
      const at = stored.findIndex((g) => g.id === id);
      return at < 0 ? undefined : stored.splice(at, 1)[0];
    },
    warn: (m) => {
      warnings.push(m);
    }
  };
  return { t, stored, warnings };
}

describe('applyH3ChatActions — the id names a render', () => {
  it('writes the whole block and leaves every clip body alone', () => {
    const { t } = h3Target([clip('a'), clip('b')]);
    const genId = t.generations[0].id;
    const block = genBlock('A wide shot of the harbour.');
    const [edit] = applyH3ChatActions(t, [act({ type: 'set_prompt', shot: genId, value: block })]);
    expect(t.generations[0].doc.prompt).toBe(block);
    // 🔴 The block is the only store. A chat edit that also wrote `clip.prompt` would be recreating
    // the mirror whose two copies of every beat produced the drift box.
    expect(t.clips[0].prompt).toBe('prompt a');
    expect(t.clips[1].prompt).toBe('prompt b');
    expect(edit).toMatchObject({ space: 'generation', field: 'prompt', generationId: genId });
  });

  /** A generation id that resolves to nothing must never fall through onto a neighbour. */
  it('drops an unresolvable id with a warning, changing nothing', () => {
    const { t, warnings } = h3Target([clip('a')]);
    expect(applyH3ChatActions(t, [act({ type: 'set_prompt', shot: 'gone', value: 'x' })])).toEqual(
      []
    );
    expect(t.clips[0].prompt).toBe('prompt a');
    expect(warnings).toHaveLength(1);
  });

  it('refuses set_idea — a render has no idea, only a gist', () => {
    const { t, warnings } = h3Target([clip('a')]);
    const id = t.generations[0].id;
    expect(applyH3ChatActions(t, [act({ type: 'set_idea', shot: id, value: 'x' })])).toEqual([]);
    expect(warnings).toHaveLength(1);
  });

  it('writes the gist to the generation, never into the block', () => {
    const { t } = h3Target([clip('a')]);
    const id = t.generations[0].id;
    applyH3ChatActions(t, [act({ type: 'set_gist', shot: id, value: 'the harbour at dusk' })]);
    expect(t.generations[0].doc.gist).toBe('the harbour at dusk');
    expect(t.generations[0].doc.prompt).toBeUndefined();
  });

  /**
   * A render still has no stored length of its own — it is the length of the cut it holds, and that
   * is now exactly one. So a duration lands on that cut and cannot reach the neighbouring render,
   * whose own length the model did not ask about.
   */
  it('retimes only the cut the id names, and the undo restores it', () => {
    const { t } = h3Target([clip('a', { seconds: 2 }), clip('b', { seconds: 6 })]);
    const id = t.generations[0].id;
    const [edit] = applyH3ChatActions(t, [act({ type: 'set_seconds', shot: id, value: 12 })]);
    expect(t.clips.map((c) => c.seconds)).toEqual([12, 6]);
    // ⚠️ Still an ARRAY, and it stays one: the undo has to name the clip it restores, because the
    // generation id it was made against is re-derived and need not survive the edit.
    expect(edit.clipSeconds).toEqual([{ clipId: 'a', seconds: 2 }]);
    expect(revertH3ChatEdit(t, edit)).toBe(true);
    expect(t.clips.map((c) => c.seconds)).toEqual([2, 6]);
  });

  it('says so when a duration will not fit in one render', () => {
    const { t, warnings } = h3Target([clip('a')]);
    applyH3ChatActions(t, [act({ type: 'set_seconds', shot: t.generations[0].id, value: 30 })]);
    expect(warnings).toHaveLength(1);
  });

  /**
   * 🔴 The hazard `holdBoundary` was written for is gone. It existed because inserting a short render
   * freed the budget the NEXT boundary rested on, `adoptStored` then handed the neighbour that
   * render's block by clip-id overlap, and somebody's soundscape ended up on a different video. A
   * boundary can no longer dissolve — every clip is its own render — so nothing needs pinning and
   * the new block lands between the two that were already there.
   */
  it('adds a render of its own, with no boundary left to pin', () => {
    const { t } = h3Target([clip('a', { seconds: 8 }), clip('b', { seconds: 8 })]);
    expect(t.generations).toHaveLength(2);
    t.setGenerationPrompt(t.generations[1].id, genBlock('The tail render.', 'Distant traffic.'));
    const anchor = t.generations[0].id;
    const [edit] = applyH3ChatActions(t, [
      act({ type: 'add_shot', after: anchor, prompt: genBlock('A new middle render.') })
    ]);
    expect(t.generations).toHaveLength(3);
    expect(t.generations[1].shots).toHaveLength(1);
    expect(t.generations[1].doc.prompt).toContain('A new middle render.');
    expect(t.generations[2].doc.prompt).toContain('The tail render.');

    expect(revertH3ChatEdit(t, edit)).toBe(true);
    expect(t.clips.map((c) => c.id)).toEqual(['a', 'b']);
    expect(t.generations).toHaveLength(2);
    expect(t.generations[1].doc.prompt).toContain('The tail render.');
  });

  it('deletes every cut in the render, and the undo puts them back with its sections', () => {
    const { t } = h3Target([clip('a', { seconds: 8 }), clip('b', { seconds: 8 })]);
    const tail = t.generations[1].id;
    t.setGenerationPrompt(tail, genBlock('The tail render.'));
    const [edit] = applyH3ChatActions(t, [act({ type: 'delete_shot', shot: tail })]);
    expect(t.clips.map((c) => c.id)).toEqual(['a']);
    expect(t.generations).toHaveLength(1);
    expect(revertH3ChatEdit(t, edit)).toBe(true);
    expect(t.clips.map((c) => c.id)).toEqual(['a', 'b']);
    expect(t.generations[1].doc.prompt).toContain('The tail render.');
  });

  /** `needsH3PromptSeed` tests for the KEY, so an undo that wrote `''` would leave a render that
   *  nobody had opened permanently un-seeded. */
  it('undoing the first block restores the un-seeded state, not an empty one', () => {
    const { t } = h3Target([clip('a')]);
    const id = t.generations[0].id;
    const [edit] = applyH3ChatActions(t, [
      act({ type: 'set_prompt', shot: id, value: genBlock('x') })
    ]);
    expect(edit.hadNoBlock).toBe(true);
    expect(revertH3ChatEdit(t, edit)).toBe(true);
    expect(needsH3PromptSeed(t.generations[0].doc)).toBe(true);
  });

  /** The story bible has no render in it, so it stays on the shared arm — and carries no marker. */
  it('applies set_story through the engine-neutral path', () => {
    const { t } = h3Target([clip('a')]);
    const [edit] = applyH3ChatActions(t, [
      act({ type: 'set_story', field: 'tone', value: 'noir' })
    ]);
    expect(t.story.tone).toBe('noir');
    expect(edit.space).toBeUndefined();
    expect(revertH3ChatEdit(t, edit)).toBe(true);
    expect(t.story.tone).toBe('t');
  });
});

describe('h3RescaleBeats', () => {
  it('keeps the proportions the author wrote', () => {
    expect(h3RescaleBeats([1, 3], 8)).toEqual([2, 6]);
  });

  it('splits evenly when there are no proportions to keep', () => {
    expect(h3RescaleBeats([0, 0], 9)).toEqual([4.5, 4.5]);
  });

  it('puts the rounding residue on the last beat, so the beats still sum to what was asked', () => {
    const out = h3RescaleBeats([1, 1, 1], 10);
    expect(out.reduce((a, b) => a + b, 0)).toBeCloseTo(10, 5);
  });
});

describe('selection across a batch', () => {
  /** An add REPLACES the selection array, so a later delete in the same batch has to read the
   *  current one — not the array the batch started with. */
  it('reads the selection as it stands, not as the batch started', () => {
    const t = target({ selectedIds: ['a'] });
    applyChatActions(t, [
      act({ type: 'add_shot', after: 'c', prompt: 'tail' }),
      act({ type: 'delete_shot', shot: 'a' })
    ]);
    // The add moved the selection onto the new tail shot, which the delete left alone.
    expect(t.selectedIds).toEqual([t.clips[t.clips.length - 1].id]);
  });
});
