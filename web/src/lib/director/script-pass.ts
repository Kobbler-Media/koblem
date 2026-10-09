/**
 * Writing EVERY render unit's prompt in ONE LLM pass — the batch the Director never had.
 *
 * ## What this is, and what it is not
 *
 * The per-unit paths (`draftClipCore`, `rerollClipPrompt`, `reworkClipCore`) each spend one call on
 * one render unit and are handed that unit's neighbours as context. `Draft all` is N of those, in
 * order. This is the other shape: `POST /api/v1/ltx-video/script-from-llm` takes a brief and `n`
 * and answers with `n` finished units in one turn — which is what the standalone LTX pages have
 * always used (`routes/ltx-video/+page.svelte`, `Ltx2bPanel.svelte`) and what the owner asked for
 * on H3: *"whole project in one pass should exist like it used to for ltx"*.
 *
 * 🔴 **The reply cannot mint anything.** On H3 the generations are DERIVED from the clips by
 * `packGenerations` and are never stored as a packing, so there is no such thing as "the batch
 * returned a 6th generation" — there are exactly as many generations as the timeline packs, and a
 * 6th block has nowhere to land. Every number this module computes therefore comes FROM the current
 * packing and goes back onto it by position: `n_segments` is the unit count the timeline already
 * has, and block `i` is written onto unit `i`. The one-pass writer re-prompts a timeline; it does
 * not lay one out. Laying one out is `planDirectorTimeline` (auto-direct), which is the adjacent
 * feature and stays that way.
 *
 * ## 🔴 The words here are the MODEL's, not the UI's
 *
 * `EngineVocab` calls an H3 generation a "shot" on screen, because that is what renders and
 * retakes. The system prompt this brief is appended to does not: `h3_prompting::script_system_prompt`
 * says *"break the brief into N GENERATIONS … may hold SEVERAL `[Shot N]` blocks"*, and the block
 * contract asks for one complete block per generation separated by `===GENERATION===`. So the unit
 * word here is `generation` and the part word is `shot` — the opposite of the screen. Passing
 * `vocab.renderUnit` in would tell the writer that a generation is a shot, which is precisely the
 * ambiguity the block's own `[Shot N]` tags then resolve the wrong way.
 */

/**
 * The server clamps `n_segments` to `1..24` (`ltx_video.rs`: `req.n_segments.clamp(1, 24)`).
 *
 * 🔴 Mirrored here so the editor can REFUSE rather than send 30 and quietly write the first 24 —
 * a silent partial write is the failure mode this codebase pays for everywhere else. The owner's
 * own framing was *"i probably won't use it for any reasonable length thing but should work"*, so
 * the honest behaviour at the ceiling is to say the number out loud and point at `Draft all`.
 */
export const SCRIPT_PASS_MAX_UNITS = 24;

/** One render unit as the batch brief needs to describe it. */
export interface ScriptPassUnit {
  /** Generation id on H3, clip id on LTX. The caller applies block `i` back onto unit `i`. */
  id: string;
  /**
   * What this unit RENDERS at — the snapped duration on H3, not the authored sum.
   *
   * It is stated per unit in the brief because every `MM:SS.mmm` inside a block is absolute from
   * the start of ITS generation, so a writer given one length for all of them places the cuts of
   * every other generation against the wrong clock.
   */
  seconds: number;
  /**
   * What each part of this unit is FOR, in order — one entry per `[Shot N]` the timeline packs.
   *
   * Empty strings are kept, not filtered: a beat nobody has written an idea for still occupies a
   * position, and dropping it would shorten the count the writer is told to hit. The brief renders
   * those as an explicit "you decide".
   *
   * On LTX a render has no parts, so this is the shot's own one-liner in a list of one.
   */
  parts: string[];
}

/** How the two engines' units are named TO THE WRITER. See this module's header. */
export interface ScriptPassWords {
  /** `generation` on H3, `shot` on LTX. */
  unit: string;
  /** `shot` on H3 — the `[Shot N]` tags inside one block. `null` on LTX, where a render has none. */
  part: string | null;
}

/**
 * The `seconds_per_segment` to send: the SHORTEST unit, and deliberately not the mean.
 *
 * The endpoint takes ONE scalar for the whole batch and there is no way to omit it — an absent
 * value becomes `4.0` server-side — so on a timeline whose units differ, one of them is going to be
 * described wrongly to `h3_prompting::render_facts` ("THIS RENDER — exactly N frames … every time
 * you write is measured against it"). The two directions of that error are not symmetrical:
 *
 * - **Too short** ⇒ the writer places its cuts inside a smaller window than it has. The last beat
 *   simply plays longer, which is what the packer's snap-up already does to every generation.
 * - **Too long** ⇒ cut times past the end of a shorter generation. Those are sent, and a `[Shot N]`
 *   the render never reaches is a wrong render with nothing on screen to say so.
 *
 * So the safe direction is down. 🔴 It is only the FLOOR: {@link scriptPassBrief} states each
 * unit's real duration next to it, and `h3_prompting::multi_generation_pacing` already tells the
 * writer *"Each entry you write is one GENERATION with its own length … Pace each one to its OWN
 * length"* — so the per-unit numbers in the brief are the ones that bind, and this scalar is the
 * conservative default behind them.
 */
export function scriptPassSeconds(units: readonly ScriptPassUnit[]): number | undefined {
  const secs = units.map((u) => u.seconds).filter((s) => s > 0);
  return secs.length ? Math.min(...secs) : undefined;
}

/** `15.083` → `15.08s`. Two decimals because that is the resolution a cut time is written at. */
const secs = (s: number): string => `${s.toFixed(2)}s`;

/**
 * The brief the batch writer is handed: the author's own, then the timeline it must write INTO.
 *
 * 🔴 The listing is the whole point. Without it the endpoint writes `n` units from a one-line brief
 * and every idea, gist and hand-written body already on the timeline is thrown away — which is a
 * different feature (auto-direct) wearing this one's button. With it, the pass re-prompts the
 * project that exists: same order, same lengths, same number of beats per unit.
 *
 * 🔴 **The part count is stated as a requirement**, for the same reason `h3_prompting::cut_plan_facts`
 * states it on the single-unit path: the `[Shot N]` numbers ARE the positions the author divided
 * the unit at, so a unit that comes back with two bodies where the timeline packs three does not
 * leave a gap — it renders the third division's span under the second body. The envelope has no
 * per-unit count field on this route, so the brief is the only place it can be said.
 *
 * ⚠️ It is a REQUEST, not a guarantee. A writer that ignores the count produces surplus or missing
 * `[Shot N]` tags; the block is stored as written and renders that way, so what the author sees in
 * the composer is what the engine gets.
 */
export function scriptPassBrief(
  brief: string,
  units: readonly ScriptPassUnit[],
  words: ScriptPassWords
): string {
  const head = brief.trim();
  const lines: string[] = [];
  lines.push(
    `THE TIMELINE IS ALREADY LAID OUT. Write these ${units.length} ${words.unit}${units.length === 1 ? '' : 's'} ` +
      `in this order, at these lengths, keeping each one to what it is for. Do not reorder them, ` +
      `do not merge or split them, and do not add any.`
  );
  units.forEach((u, i) => {
    const n = i + 1;
    const label = `${words.unit.charAt(0).toUpperCase()}${words.unit.slice(1)} ${n}`;
    if (!words.part) {
      // LTX: a render has no parts, so the unit's own one-liner is the whole line.
      const only = u.parts.find((p) => p.trim())?.trim();
      lines.push(`\n${label} — ${secs(u.seconds)}: ${only || '(you decide — carry the story on)'}`);
      return;
    }
    const count = u.parts.length;
    lines.push(
      `\n${label} — ${secs(u.seconds)}, exactly ${count} ${words.part}${count === 1 ? '' : 's'}:`
    );
    u.parts.forEach((p, j) => {
      const text = p.trim();
      lines.push(
        `  [${capitalisePart(words.part!)} ${j + 1}] ${text || '(you decide — nothing written for this one yet)'}`
      );
    });
  });
  const body = lines.join('\n');
  return head ? `${head}\n\n${body}` : body;
}

const capitalisePart = (s: string): string => s.charAt(0).toUpperCase() + s.slice(1);

/** What the caller could not use — surplus blocks, missing blocks. Reported, never swallowed. */
export interface ScriptPassOutcome {
  applied: number;
  warnings: string[];
}

/**
 * Reconcile what came back with what the timeline has room for.
 *
 * 🔴 Both mismatches are one-sided and neither is repairable here: a SHORT reply leaves the tail
 * units holding whatever they held before (nothing is cleared — a half-written batch must not also
 * be a destructive one), and a LONG reply has no unit to land on, because generations are derived
 * from clips and this module cannot create clips. Both are said out loud.
 */
export function scriptPassOutcome(
  unitCount: number,
  blockCount: number,
  /** What a unit is called ON SCREEN — `vocab.renderUnitPlural`, so there is one spelling of it. */
  unitPlural: string
): ScriptPassOutcome {
  const applied = Math.min(unitCount, blockCount);
  const warnings: string[] = [];
  if (blockCount < unitCount) {
    warnings.push(
      `The writer returned ${blockCount} of ${unitCount} prompts — the last ${unitCount - applied} ${unitPlural} were left exactly as they were. Run it again, or draft those on their own.`
    );
  }
  if (blockCount > unitCount) {
    warnings.push(
      `The writer returned ${blockCount} prompts for ${unitCount} ${unitPlural}; the extra ${blockCount - unitCount} were dropped. Nothing on the timeline was renumbered or created.`
    );
  }
  return { applied, warnings };
}
