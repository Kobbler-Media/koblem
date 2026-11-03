import type { DirectorH3GenerationBrief, DirectorReviewFinding } from '#lib/api/client.ts';
import { seedH3Prompt } from './h3-document';
import type { PackedGeneration } from './h3-generations';

/**
 * One continuity finding, as the editor holds it.
 *
 * Keyed to the clip's stable id rather than a position, so a finding survives a reorder or a delete
 * between the review and the fix — a removed shot then says so instead of silently pointing at
 * whatever took its place. `clipId` null ⇒ a whole-story note, which has no one-click fix.
 */
export interface ReviewItem {
  severity: DirectorReviewFinding['severity'];
  issue: string;
  clipId: string | null;
  /** The user's own direction for the fix, folded into the rework instruction. */
  notes: string;
  fixing: boolean;
}

/**
 * What the script supervisor is shown on H3: the packing, with the block each render will send.
 *
 * 🔴 **Without this the review can only guess.** The reviewer's job on this engine is largely
 * schema: a cut time that is not strictly increasing or falls outside its render's duration, a
 * `<Subject 2>` addressed but never defined, a defined label with no retention line, a body field
 * that belongs to the other checkpoint. Every one of those lives inside the assembled block — the
 * stored artifact — and none of them is visible in a list of clip bodies, which is all the review
 * request used to carry. A model asked for findings it cannot derive returns invented ones, so the
 * backend now asks for those checks ONLY when this array arrives.
 *
 * 🔴 **The ids stay per CUT.** Each brief names the clip ids inside its render, because a finding is
 * turned into a one-click rework of one clip ({@link ReviewItem.clipId} → `/rework`). Findings keyed
 * to a generation would arrive with nothing to fix.
 *
 * ⚠️ `seedH3Prompt` is the same fallback `compileH3Timeline` applies, so a generation nobody has
 * opened is reviewed on **the bytes that will actually render** rather than on a blank. It is
 * flagged `authored: false` all the same: it is unedited machine assembly of the cut bodies, and a
 * reviewer that thinks a human wrote it files a finding for every section not yet filled in.
 *
 * A generation with no block and nothing to compose one from (every cut still empty) sends no
 * `prompt` at all — the backend then says so in that entry instead of presenting an empty block as
 * an author who wrote nothing.
 *
 * 🔴 **`id` and `gist` are here for CHAT, and `/review` ignores both.** They are on the shared
 * builder rather than a second one because a second builder is how the two surfaces start describing
 * different blocks. `id` is load-bearing on the chat path and nowhere else: the server switches the
 * whole turn onto the generation id space only when EVERY brief carries one, so a single omission
 * silently drops chat back to the cut contract where a `set_prompt` evaporates. There is no "some
 * generations are addressable" state — see `chat_generation_ids`.
 */
export function h3ReviewGenerations(packed: PackedGeneration[]): DirectorH3GenerationBrief[] {
  return packed.map((gen) => {
    const authored = gen.doc.prompt?.trim() ?? '';
    const prompt = authored || seedH3Prompt(gen).trim();
    return {
      // 🔴 Unconditional. See the note above — an `id` omitted for even one generation costs the
      // whole turn its ability to edit a block.
      id: gen.id,
      // Omitted when blank rather than sent empty: absent means "nobody labelled this render", and
      // an empty string would have the model believe somebody labelled it with nothing. The
      // display fallback (`h3GenerationGist`) is deliberately NOT applied — it is the first beat's
      // prose, which the block already carries in full.
      ...(gen.doc.gist?.trim() ? { gist: gen.doc.gist.trim() } : {}),
      // The SNAPPED duration, not the authored sum: it is what renders, and therefore the window a
      // cut time is legal inside. Sending the authored sum would make a legal late cut look late.
      seconds: gen.seconds,
      task: gen.task,
      shots: gen.shots.map((s) => s.clip.id),
      authored: !!authored,
      ...(prompt ? { prompt } : {})
    };
  });
}
