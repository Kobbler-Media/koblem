/**
 * Applying — and undoing — the timeline edits the director chat proposes.
 *
 * Every action names its target by STABLE CLIP ID rather than by position, which is what makes a
 * batch safe: an add or a delete earlier in the same batch cannot shift a later action onto the
 * wrong shot. Each applied action becomes a card carrying the value it replaced, so one edit can be
 * reverted on its own without touching the rest of the turn.
 *
 * The clip list and the story are mutated IN PLACE — the caller hands over its own reactive state,
 * exactly as the editor's own handlers do.
 */

import { uid } from '@koblibs/ui';
import { DIRECTOR_ADD_AT_START, type DirectorChatAction } from '$lib/api/client';
import type { DirectorClip, DirectorH3Generation, StoryContext } from '$lib/api/client';
import { parseH3Block } from './h3-block';
import type { PackedGeneration } from './h3-generations';

export interface ChatEdit {
  id: string;
  label: string;
  kind: 'field' | 'add' | 'delete' | 'story';
  before: string;
  after: string;
  reverted: boolean;
  clipId?: string;
  field?: 'prompt' | 'gist' | 'idea' | 'seconds';
  storyField?: 'backstory' | 'tone' | 'rules';
  /** A detached copy of a deleted shot, so reverting can put it back whole. */
  deletedClip?: DirectorClip;
  deletedAt?: number;

  // ── H3 generation space ─────────────────────────────────────────────────────
  //
  // 🔴 These are set ONLY by {@link applyH3ChatActions}, and `space` is the discriminator the undo
  // path dispatches on. It is a marker rather than an inference from `generationId` because a
  // generation-space turn also mints ordinary `story` edits, which revert through the shared arm.
  /** Present ⇒ this edit was applied against RENDERS, not clips. See {@link H3ChatTarget}. */
  space?: 'generation';
  /** The `DirectorH3Generation.id` this edit landed on. */
  generationId?: string;
  /**
   * ⚠️ `set_seconds` undo is an ARRAY, not one number. A render has no stored length — it is the
   * snapped sum of its beats — so setting it rescales every beat, and one before-value could only
   * restore one of them.
   */
  clipSeconds?: { clipId: string; seconds: number }[];
  /** Every cut a deleted render held, ASCENDING by index so re-inserting lands them back in order. */
  deletedClips?: { clip: DirectorClip; at: number }[];
  /** The stored sections that went with it, or absent when nobody had written any. */
  deletedGeneration?: DirectorH3Generation;
  /**
   * The generation had no `prompt` KEY at all before this edit.
   *
   * 🔴 Not the same as an empty block: `needsH3PromptSeed` tests for the key, so restoring `''`
   * would leave a never-opened render permanently un-seeded — the composer would draw an empty box
   * where it used to compose one from the cut bodies.
   */
  hadNoBlock?: boolean;
}

export interface ChatMsg {
  role: 'user' | 'assistant';
  content: string;
  edits?: ChatEdit[];
}

/** The editor state a chat edit reaches into. */
export interface ChatTarget {
  clips: DirectorClip[];
  story: StoryContext;
  /** Duration for a shot the model adds without naming one. */
  defaultSeconds: number;
  selectedIds: string[];
  select: (ids: string[]) => void;
  /** A detached copy of a clip — the caller owns how its reactive state is unwrapped. */
  detach: (c: DirectorClip) => DirectorClip;
}

/**
 * The editor state a chat edit reaches into **on H3, where a chat action names a RENDER.**
 *
 * 🔴 One shot = one prompt = one `DirectorH3Generation` = one render. A `[Shot N] At MM:SS.mmm` line
 * is a beat INSIDE that prompt — text, with no id of its own. So `set_prompt` here replaces a whole
 * block and `delete_shot` removes a whole render, which is why this is a different target type and
 * not a flag on {@link ChatTarget}: every action means a different thing.
 *
 * It EXTENDS `ChatTarget` because `set_story` is engine-neutral and reverts through the shared arm.
 */
export interface H3ChatTarget extends ChatTarget {
  /**
   * The live packing.
   *
   * 🔴 **Must be a GETTER.** `packedGenerations` is `$derived` and every action below changes its
   * inputs — a captured array is the packing from before the edit, and reading a generation id out
   * of it after an insert names a render that no longer exists.
   */
  readonly generations: PackedGeneration[];
  /**
   * What one render can hold, in seconds, for THIS project — tighter when a soundtrack is attached
   * (345 frames rather than 362). Read only to WARN: a longer `set_seconds` re-packs into two
   * renders, which is visible, and refusing it would be a silent no-op instead.
   */
  maxSeconds: number;
  /** Write a shot's prompt block — the only store of its prose and its cut times. */
  setGenerationPrompt: (id: string, block: string) => void;
  /** Patch a generation's stored sections, materialising the entry on first edit. */
  updateGeneration: (id: string, patch: Partial<DirectorH3Generation>) => void;
  /** Put a whole stored entry back, with its own `clip_ids` — the undo of a delete. */
  putGeneration: (doc: DirectorH3Generation) => void;
  /** Remove a stored entry, returning what was there so an undo can restore exactly that. */
  dropGeneration: (id: string) => DirectorH3Generation | undefined;
  /** Say why an action was dropped. Never silently bin one — a dropped edit the user cannot see
   *  reads as the assistant lying about having made the change. */
  warn: (message: string) => void;
}

/**
 * Replay an assistant turn's history entry with the edits it ALREADY applied marked as done, so the
 * model does not re-emit — and the editor re-apply — them on the next turn.
 */
export const chatHistory = (messages: ChatMsg[]) =>
  messages.map((m) => ({
    role: m.role,
    content:
      m.role === 'assistant' && m.edits?.length
        ? `${m.content}\n\n[Done — already applied: ${m.edits.map((e) => e.label).join('; ')}.]`
        : m.content
  }));

export function applyChatActions(t: ChatTarget, actions: DirectorChatAction[]): ChatEdit[] {
  const edits: ChatEdit[] = [];
  for (const a of actions) {
    const e = applyChatAction(t, a);
    if (e) edits.push(e);
  }
  return edits;
}

function applyChatAction(t: ChatTarget, a: DirectorChatAction): ChatEdit | null {
  const { clips, story } = t;
  const idOf = (id?: string): string | null => (id && clips.some((c) => c.id === id) ? id : null);
  const idxOf = (id: string | null): number => (id ? clips.findIndex((c) => c.id === id) : -1);
  const mk = (p: Omit<ChatEdit, 'id' | 'reverted'>): ChatEdit => ({
    id: uid(),
    reverted: false,
    ...p
  });
  const reselectIfOrphaned = () => {
    const selected = new Set(t.selectedIds);
    if (!clips.some((c) => selected.has(c.id))) t.select(clips[0] ? [clips[0].id] : []);
  };
  switch (a.type) {
    case 'set_prompt':
    case 'set_gist':
    case 'set_idea': {
      const field = a.type === 'set_prompt' ? 'prompt' : a.type === 'set_gist' ? 'gist' : 'idea';
      const id = idOf(a.shot);
      const i = idxOf(id);
      if (i < 0 || typeof a.value !== 'string') return null;
      const before = (clips[i][field] ?? '').toString();
      clips[i][field] = a.value;
      const name = field === 'prompt' ? 'Prompt' : field === 'gist' ? 'Summary' : 'Idea';
      return mk({
        kind: 'field',
        label: `Shot ${i + 1} · ${name}`,
        before,
        after: a.value,
        clipId: id!,
        field
      });
    }
    case 'set_seconds': {
      const id = idOf(a.shot);
      const i = idxOf(id);
      const v = typeof a.value === 'number' ? a.value : Number(a.value);
      if (i < 0 || !Number.isFinite(v) || v <= 0) return null;
      const before = clips[i].seconds;
      clips[i].seconds = v;
      return mk({
        kind: 'field',
        label: `Shot ${i + 1} · Duration`,
        before: `${before}s`,
        after: `${v}s`,
        clipId: id!,
        field: 'seconds'
      });
    }
    case 'delete_shot': {
      const id = idOf(a.shot);
      const i = idxOf(id);
      if (i < 0) return null;
      const deletedClip = t.detach(clips[i]);
      clips.splice(i, 1);
      reselectIfOrphaned();
      return mk({
        kind: 'delete',
        label: `Deleted shot ${i + 1}`,
        before: deletedClip.gist?.trim() || deletedClip.prompt || deletedClip.idea || '(shot)',
        after: '',
        deletedClip,
        deletedAt: i
      });
    }
    case 'add_shot': {
      // `after` is the clip id to insert after, resolved to its CURRENT position so an earlier
      // insert in the same batch shifts it. DIRECTOR_ADD_AT_START = the head; absent = append.
      let pos: number;
      if (a.after === DIRECTOR_ADD_AT_START) pos = 0;
      else {
        const afterIdx = idxOf(idOf(a.after));
        pos = afterIdx >= 0 ? afterIdx + 1 : clips.length;
      }
      const c: DirectorClip = {
        id: uid(),
        idea: a.idea?.trim() || undefined,
        prompt: (a.prompt ?? a.value)?.toString().trim() || '',
        mode: 't2v',
        seconds: t.defaultSeconds,
        keyframes: []
      };
      clips.splice(pos, 0, c);
      t.select([c.id]);
      return mk({
        kind: 'add',
        label: `Added shot ${pos + 1}`,
        before: '',
        after: c.prompt || c.idea || '(new shot)',
        clipId: c.id
      });
    }
    case 'set_story': {
      if (!a.field || typeof a.value !== 'string') return null;
      const f = a.field;
      const before =
        f === 'backstory'
          ? story.backstory
          : f === 'tone'
            ? story.tone
            : f === 'rules'
              ? story.rules
              : null;
      if (before === null) return null;
      if (f === 'backstory') story.backstory = a.value;
      else if (f === 'tone') story.tone = a.value;
      else story.rules = a.value;
      return mk({ kind: 'story', label: `Story · ${f}`, before, after: a.value, storyField: f });
    }
    default:
      return null;
  }
}

// ── H3: the same batch, applied to RENDERS ───────────────────────────────────
//
// 🔴 **Why this is a second applier and not a branch inside the first.** On H3 the addressable unit
// is the generation: one prompt, one render, one delivery. The action VOCABULARY is identical, so
// nothing about an action's shape says which kind of thing it names — the server says, on
// `directorChat`'s `target`, and the caller picks the applier. A branch inside `applyChatAction`
// would have had to carry both id spaces through every arm, which is exactly the confusion that let
// `set_prompt` write `DirectorClip.prompt` and evaporate.

/** The shortest a rescale is allowed to make a beat. Below this a beat is not a beat. */
const H3_CHAT_MIN_BEAT_SECONDS = 0.1;

/**
 * Spread a render's TOTAL length back over the beats inside it, keeping their proportions.
 *
 * 🔴 **A generation has no stored length.** It is the snapped sum of its beats, so "make this shot
 * eight seconds" can only be expressed by moving the beats — there is no single number to assign.
 * Proportional rather than even, because the beat structure is something the author wrote: an even
 * split would silently discard their pacing to satisfy a duration request. A generation whose beats
 * are all zero has no proportions to keep, so it splits evenly.
 *
 * The last beat absorbs the rounding residue, for the same reason `packGenerations` gives it the
 * frame-grid snap: extra time at the end is what a longer render looks like.
 */
export function h3RescaleBeats(current: number[], total: number): number[] {
  if (!current.length) return [];
  const round = (n: number) => Math.round(n * 100) / 100;
  const floor = (n: number) => Math.max(H3_CHAT_MIN_BEAT_SECONDS, round(n));
  const sum = current.reduce((t, s) => t + Math.max(0, s), 0);
  const out = current.map((s) =>
    floor(sum > 0 ? (Math.max(0, s) / sum) * total : total / current.length)
  );
  const drift = total - out.reduce((t, s) => t + s, 0);
  out[out.length - 1] = floor(out[out.length - 1] + drift);
  return out;
}

export function applyH3ChatActions(t: H3ChatTarget, actions: DirectorChatAction[]): ChatEdit[] {
  const edits: ChatEdit[] = [];
  for (const a of actions) {
    const e = applyH3ChatAction(t, a);
    if (e) edits.push(e);
  }
  return edits;
}

function applyH3ChatAction(t: H3ChatTarget, a: DirectorChatAction): ChatEdit | null {
  const mk = (p: Omit<ChatEdit, 'id' | 'reverted' | 'space'>): ChatEdit => ({
    id: uid(),
    reverted: false,
    space: 'generation',
    ...p
  });
  const reselectIfOrphaned = () => {
    const selected = new Set(t.selectedIds);
    if (!t.clips.some((c) => selected.has(c.id))) t.select(t.clips[0] ? [t.clips[0].id] : []);
  };

  // The story bible is engine-neutral — it has no render in it — so it reverts through the shared
  // arm and deliberately carries NO `space` marker.
  if (a.type === 'set_story') return applyChatAction(t, a);
  if (a.type === 'set_idea') {
    // The server strips this from the generation-space contract (`CHAT_SPEC_ACTIONS_GENERATION`), so
    // it should never arrive; refusing it here is the belt to that braces. An "idea" is a one-liner
    // you write BEFORE a prompt exists, and here the prompt IS the document.
    t.warn(
      'Dropped a set_idea: a shot on this engine has no idea — its one-line label is its gist.'
    );
    return null;
  }

  if (a.type === 'add_shot') return addH3Generation(t, a, mk);

  // 🔴 Resolved by EXACT id against the live packing, and an unresolvable one is dropped. Never
  // fall back to a neighbour, an index or the nearest match: a dropped edit is recoverable, an edit
  // applied to the wrong render is not.
  const at = t.generations.findIndex((g) => g.id === a.shot);
  if (at < 0) {
    t.warn(`Dropped a ${a.type}: no shot with id "${a.shot ?? ''}" is in the timeline any more.`);
    return null;
  }
  const gen = t.generations[at];

  switch (a.type) {
    case 'set_prompt': {
      if (typeof a.value !== 'string') return null;
      const before = gen.doc.prompt ?? '';
      // The KEY, not the content — see `ChatEdit.hadNoBlock`.
      const hadNoBlock = typeof gen.doc.prompt !== 'string';
      // Through `setGenerationPrompt`, never `updateGeneration({ prompt })`: it is the one funnel
      // for everything the model is sent.
      t.setGenerationPrompt(gen.id, a.value);
      return mk({
        kind: 'field',
        label: `Shot ${at + 1} · Prompt`,
        before,
        after: a.value,
        generationId: gen.id,
        field: 'prompt',
        ...(hadNoBlock ? { hadNoBlock: true } : {})
      });
    }
    case 'set_gist': {
      if (typeof a.value !== 'string') return null;
      const before = gen.doc.gist ?? '';
      // ⚠️ `updateGeneration`, NOT the block. The gist is display only and never compiled into a
      // request; the schema's `summary:` section is the thing that renders and lives inside `prompt`.
      t.updateGeneration(gen.id, { gist: a.value });
      return mk({
        kind: 'field',
        label: `Shot ${at + 1} · Summary`,
        before,
        after: a.value,
        generationId: gen.id,
        field: 'gist'
      });
    }
    case 'set_seconds': {
      const v = typeof a.value === 'number' ? a.value : Number(a.value);
      if (!Number.isFinite(v) || v <= 0 || !gen.shots.length) return null;
      const clipSeconds = gen.shots.map((s) => ({ clipId: s.clip.id, seconds: s.clip.seconds }));
      const scaled = h3RescaleBeats(
        clipSeconds.map((c) => c.seconds),
        v
      );
      clipSeconds.forEach((c, i) => {
        const clip = t.clips.find((x) => x.id === c.clipId);
        if (clip) clip.seconds = scaled[i];
      });
      // Not clamped, deliberately — the model is TOLD the ceiling and a longer render re-packs into
      // two, which is a visible outcome. It does orphan the second half's block, so it is said out
      // loud rather than corrected behind the author's back.
      if (v > t.maxSeconds + 1e-6) {
        t.warn(
          `Shot ${at + 1} was set to ${v.toFixed(2)}s, past the ${t.maxSeconds.toFixed(2)}s one render can hold — it will pack into two renders, and the second will not carry this prompt.`
        );
      }
      return mk({
        kind: 'field',
        label: `Shot ${at + 1} · Duration`,
        before: `${gen.authoredSeconds.toFixed(2)}s`,
        after: `${v.toFixed(2)}s`,
        generationId: gen.id,
        field: 'seconds',
        clipSeconds
      });
    }
    case 'delete_shot': {
      const removed = gen.shots
        .map((s) => ({ clip: t.detach(s.clip), at: t.clips.findIndex((c) => c.id === s.clip.id) }))
        .filter((r) => r.at >= 0)
        .sort((x, y) => x.at - y.at);
      if (!removed.length) return null;
      const before = gen.doc.gist?.trim() || removed[0].clip.prompt.trim() || '(shot)';
      // Descending, so an earlier removal cannot shift a later index out from under itself.
      for (let i = removed.length - 1; i >= 0; i--) t.clips.splice(removed[i].at, 1);
      const deletedGeneration = t.dropGeneration(gen.id);
      reselectIfOrphaned();
      return mk({
        kind: 'delete',
        label: `Deleted shot ${at + 1}`,
        before,
        after: '',
        generationId: gen.id,
        deletedClips: removed,
        ...(deletedGeneration ? { deletedGeneration } : {})
      });
    }
    default:
      return null;
  }
}

/**
 * Insert a new render after the one the model named.
 *
 * 🔴 **Inherently two-step, because the generation does not exist until the packer re-derives it.**
 * A render is not a stored object with a prompt slot — it is a group of clips, minted by
 * `packGenerations` from the timeline. So the clip goes in first, the packing is re-read, and only
 * then does the block have somewhere to land. The `$derived` packing recomputes on read, which is
 * what makes reading it back mid-function legitimate rather than lucky.
 *
 */
function addH3Generation(
  t: H3ChatTarget,
  a: DirectorChatAction,
  mk: (p: Omit<ChatEdit, 'id' | 'reverted' | 'space'>) => ChatEdit
): ChatEdit | null {
  const block = (a.prompt ?? a.value)?.toString().trim() ?? '';
  // `after` names a RENDER here, so the insertion point is after its LAST cut. An unknown anchor
  // appends, matching the clip applier and the server's own "omit `after` = append".
  let pos: number;
  if (a.after === DIRECTOR_ADD_AT_START) pos = 0;
  else {
    const anchor = t.generations.find((g) => g.id === a.after);
    const lastId = anchor?.shots[anchor.shots.length - 1]?.clip.id;
    const lastAt = lastId ? t.clips.findIndex((c) => c.id === lastId) : -1;
    pos = lastAt >= 0 ? lastAt + 1 : t.clips.length;
  }
  const clip: DirectorClip = {
    id: uid(),
    ...(a.idea?.trim() ? { idea: a.idea.trim() } : {}),
    // ⚠️ LTX reads this; on H3 the prose lives in the generation's block and this stays empty. The
    // block's `[Shot N]` marks are beats inside this one render, never clips.
    prompt: parseH3Block(block).shots[0]?.body.trim() ?? '',
    mode: 't2v',
    seconds: t.defaultSeconds,
    keyframes: []
  };
  t.clips.splice(pos, 0, clip);
  t.select([clip.id]);
  // Re-read: the two mutations above changed the packing's inputs, so the render holding this clip
  // is only nameable now.
  const gen = t.generations.find((g) => g.shots.some((s) => s.clip.id === clip.id));
  // 🔴 The block is written AFTER the re-pack and not before, so the soundscape and the score go to
  // the new render rather than to whichever one the clip briefly belonged to.
  if (gen && block) t.setGenerationPrompt(gen.id, block);
  const at = gen ? t.generations.findIndex((g) => g.id === gen.id) : -1;
  return mk({
    kind: 'add',
    label: `Added shot ${at >= 0 ? at + 1 : t.generations.length}`,
    before: '',
    after: clip.prompt || clip.idea || '(new shot)',
    clipId: clip.id,
    ...(gen ? { generationId: gen.id } : {})
  });
}

/**
 * Undo ONE edit made in the generation id space, falling through to {@link revertChatEdit} for
 * everything else — `set_story`, and every edit from a clip-space turn.
 *
 * ⚠️ Dispatching on `edit.space` rather than on the current engine: a conversation survives an
 * engine switch, and an edit is undone by the rules it was MADE under.
 */
export function revertH3ChatEdit(t: H3ChatTarget, edit: ChatEdit): boolean {
  if (edit.space !== 'generation') return revertChatEdit(t, edit);
  if (edit.reverted) return false;
  switch (edit.kind) {
    case 'field': {
      if (edit.field === 'seconds') {
        // The whole array, or the beats that were not restored keep the rescaled lengths and the
        // render's total silently stays where chat put it.
        for (const s of edit.clipSeconds ?? []) {
          const clip = t.clips.find((c) => c.id === s.clipId);
          if (clip) clip.seconds = s.seconds;
        }
        break;
      }
      const gen = t.generations.find((g) => g.id === edit.generationId);
      if (!gen) return false;
      if (edit.field === 'prompt') {
        // 🔴 Clearing the KEY, not writing `''` — see `ChatEdit.hadNoBlock`. `needsH3PromptSeed`
        // tests for the key, so an empty string would leave a never-opened render permanently
        // un-seeded.
        if (edit.hadNoBlock) t.updateGeneration(gen.id, { prompt: undefined });
        else t.setGenerationPrompt(gen.id, edit.before);
      } else if (edit.field === 'gist') {
        t.updateGeneration(gen.id, { gist: edit.before });
      }
      break;
    }
    case 'add': {
      const i = t.clips.findIndex((c) => c.id === edit.clipId);
      if (i >= 0) {
        t.clips.splice(i, 1);
        const selected = new Set(t.selectedIds);
        if (!t.clips.some((c) => selected.has(c.id))) t.select(t.clips[0] ? [t.clips[0].id] : []);
      }
      // The stored entry only existed because of that clip. Left behind it is an orphan the next
      // `adoptStored` could hand to a different render by clip-id overlap.
      if (edit.generationId) t.dropGeneration(edit.generationId);
      break;
    }
    case 'delete': {
      // Ascending, so each splice lands at the index it was recorded at.
      for (const d of edit.deletedClips ?? []) {
        t.clips.splice(Math.min(d.at, t.clips.length), 0, t.detach(d.clip));
      }
      // Put the sections back with their ORIGINAL `clip_ids`: `adoptStored` re-attaches by overlap,
      // so the entry finds its render again even if the packer mints it a different id.
      if (edit.deletedGeneration) t.putGeneration(edit.deletedGeneration);
      break;
    }
    default:
      return false;
  }
  edit.reverted = true;
  return true;
}

/**
 * Undo ONE chat edit, from its captured before-value and stable clip id. Returns whether anything
 * changed — an edit already reverted, or whose shot has since gone, changes nothing.
 */
export function revertChatEdit(t: ChatTarget, edit: ChatEdit): boolean {
  if (edit.reverted) return false;
  const { clips, story } = t;
  switch (edit.kind) {
    case 'field': {
      const i = clips.findIndex((c) => c.id === edit.clipId);
      if (i < 0) return false;
      if (edit.field === 'seconds') clips[i].seconds = parseFloat(edit.before) || clips[i].seconds;
      else if (edit.field === 'prompt') clips[i].prompt = edit.before;
      else if (edit.field === 'gist') clips[i].gist = edit.before;
      else if (edit.field === 'idea') clips[i].idea = edit.before;
      break;
    }
    case 'add': {
      const i = clips.findIndex((c) => c.id === edit.clipId);
      if (i >= 0) {
        clips.splice(i, 1);
        const selected = new Set(t.selectedIds);
        if (!clips.some((c) => selected.has(c.id))) t.select(clips[0] ? [clips[0].id] : []);
      }
      break;
    }
    case 'delete': {
      if (edit.deletedClip) {
        const at = Math.min(edit.deletedAt ?? clips.length, clips.length);
        clips.splice(at, 0, t.detach(edit.deletedClip));
      }
      break;
    }
    case 'story': {
      if (edit.storyField === 'backstory') story.backstory = edit.before;
      else if (edit.storyField === 'tone') story.tone = edit.before;
      else if (edit.storyField === 'rules') story.rules = edit.before;
      break;
    }
  }
  edit.reverted = true;
  return true;
}
