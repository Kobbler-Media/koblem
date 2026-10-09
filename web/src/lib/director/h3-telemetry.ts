/**
 * Reading MiniMax-H3's in-flight render telemetry off the shared job poll.
 *
 * 🔴 **The one rule this module exists to enforce: absence is UNKNOWN, never zero.**
 *
 * Every field here is optional on the wire and simply not present on an LTX job — that engine has
 * no numeric progress at all, and never will (see `LtxJobStatus`). A reader that defaults them to 0
 * renders "step 0 of 0" and a progress bar at 0%, which on a bar is indistinguishable from a render
 * that has not started and, at the far end, from one that has finished. So every number below is
 * `number | null` and every label is `string | null`; a caller draws nothing rather than drawing a
 * lie. That is also why this returns a struct of nullables instead of pre-formatted strings with
 * fallbacks baked in.
 *
 * ## Why it is worth drawing at all
 *
 * One H3 generation is ~830 s at production shape and a job is a chain of them, so a render is
 * routinely an hour of a spinner. And the single most valuable thing on the list is not the
 * percentage: it is `phase: "loading"`. A chain that switches checkpoint family (`fl2va` ↔
 * `ref2va`) pays a full ~20 GB DiT + text-encoder reload between two generations, during which the
 * step counter does not move and nothing is emitted. Undrawn, that is minutes of silence that looks
 * exactly like a hang — and a user who reads it as one cancels an hour-old render. Named, it is a
 * checkpoint swap with an end.
 *
 * LTX is unaffected by everything here: its jobs carry none of these keys, `hasH3Telemetry` is
 * false, and the LTX path keeps its own elapsed/estimate progress notion untouched.
 */

import type { H3Phase, LtxJobStatus } from '$lib/api/client';
import { planReloads, type H3GenerationPlan, type H3Plan } from './h3-plan';
import { vocabFor } from './engine';

/**
 * What one render is CALLED in the sentences below — `"shot"`.
 *
 * Read off the capability record rather than typed as a literal, so there is one place the word
 * lives even in the modules that can only ever be H3. ⚠️ Display only: `generation_index`,
 * `generation_plans` and `DirectorH3Generation` all keep their names, because they are the wire and
 * the document rather than the label. See `EngineVocab`.
 */
const UNIT = vocabFor('h3').renderUnit;

/** A finite, non-negative number off the wire, or null. Absence, nulls and NaN all mean unknown. */
function num(v: number | null | undefined): number | null {
  return typeof v === 'number' && Number.isFinite(v) && v >= 0 ? v : null;
}

/**
 * What the engine is doing, in words a director understands.
 *
 * 🔴 `loading` deliberately does NOT say "loading" — that word reads as "still starting up" next to
 * a frozen step counter. It says the render is swapping checkpoints, because that is a thing with a
 * cause and an end rather than a thing that has stopped.
 *
 * An unrecognised phase falls back to the raw string: the shim owns this vocabulary and may grow
 * it, and printing what it said is better than claiming the render is in an unknown state.
 */
export function h3PhaseLabel(phase: string | null | undefined): string | null {
  switch (phase) {
    case 'denoising':
      return 'Denoising';
    case 'decoding':
      return 'Decoding video';
    case 'encoding':
      return 'Encoding the file';
    case 'loading':
      return 'Swapping checkpoint';
    default:
      return phase?.trim() || null;
  }
}

/**
 * The sentence that stops a checkpoint swap being read as a hang. `null` for every other phase —
 * they need no defending.
 */
export function h3PhaseNote(phase: string | null | undefined): string | null {
  return phase === 'loading'
    ? `This ${UNIT} needs the other checkpoint, so the engine is loading ~20 GB of DiT and text encoder. The step counter is frozen for a few minutes — the render has not stalled.`
    : null;
}

/** The live telemetry of one H3 job, with every unknown left as `null`. */
export interface H3Telemetry {
  /** The job carried at least one telemetry field — i.e. it is an H3 job on a shim that reports. */
  known: boolean;
  /** Raw wire value, kept so a caller can branch on `loading` without re-deriving it. */
  phase: H3Phase | string | null;
  phaseLabel: string | null;
  phaseNote: string | null;
  /** The checkpoint reload. The one phase that must never look like a stall. */
  swapping: boolean;
  /** 0–100 across the WHOLE job, or null when the engine has not said. Never defaults to 0. */
  percent: number | null;
  step: number | null;
  totalSteps: number | null;
  /** `"step 7 of 20"`, or null unless BOTH numbers are known — a step with no total is not progress. */
  stepLabel: string | null;
  /** 1-based for display; the wire is 0-based. */
  generation: number | null;
  generationCount: number | null;
  /** `"shot 2 of 4"`, or just `"shot 2"` when the count is unknown. */
  generationLabel: string | null;
  /** The ENGINE's own whole-job ETA at submit, including any reload. Not the editor's estimate. */
  etaSeconds: number | null;
}

/**
 * Read an H3 job's telemetry. Safe on an LTX job and on a null one — both give `known: false` and
 * nothing but nulls, so a component can call it unconditionally and gate on `known`.
 *
 * `generationCount` comes from `segments`, which on H3 counts GENERATIONS (the render unit), not
 * shots. It is only reported when the job also carried a `generation_index`: an LTX job has a
 * `segments` too, and reading it here would invent a generation count for an engine with no
 * generations.
 */
export function readH3Telemetry(job: LtxJobStatus | null | undefined): H3Telemetry {
  // An empty/whitespace phase is normalised to null: it is a shim that sent the key with nothing in
  // it, and treating it as "reported" would flip `known` on and draw an empty readout.
  const rawPhase = job?.phase;
  const phase = typeof rawPhase === 'string' && rawPhase.trim() ? rawPhase : null;
  const progress = num(job?.progress);
  const step = num(job?.current_step);
  const totalSteps = num(job?.total_steps);
  const genIndex = num(job?.generation_index);
  const eta = num(job?.total_eta_seconds);
  const genCount = genIndex !== null ? num(job?.segments) : null;
  const generation = genIndex === null ? null : genIndex + 1;
  return {
    known:
      phase !== null ||
      progress !== null ||
      step !== null ||
      totalSteps !== null ||
      genIndex !== null ||
      eta !== null,
    phase,
    phaseLabel: h3PhaseLabel(phase),
    phaseNote: h3PhaseNote(phase),
    swapping: phase === 'loading',
    // Clamped rather than trusted: a bar past 100% is a layout bug, and the engine's fraction is a
    // model of its own work rather than a measurement.
    percent: progress === null ? null : Math.max(0, Math.min(100, progress * 100)),
    step,
    totalSteps,
    // 🔴 Both or neither. `total_steps` alone is a denominator with no numerator, and `current_step`
    // alone reads as "step 7 of ∞" — which is the "0 of 0" failure wearing a different hat.
    stepLabel:
      step !== null && totalSteps !== null && totalSteps > 0
        ? `step ${step} of ${totalSteps}`
        : null,
    generation,
    generationCount: genCount,
    // 🔴 The FIELD keeps its wire-shaped name and the LABEL does not. `generation_index` is what
    // the engine reports and `DirectorH3Generation` is what koblem stores, but what the author is
    // watching render is a shot — see `EngineVocab`.
    generationLabel:
      generation === null
        ? null
        : genCount && genCount > 0
          ? `${UNIT} ${generation} of ${genCount}`
          : `${UNIT} ${generation}`,
    etaSeconds: eta
  };
}

// ── After the fact: what the render COST, off the stamped plan echo ──────────────────────────
//
// Everything above is live and dies with the job. Everything below is read off `metadata.h3_plan`,
// which koblem stamps onto the project row and which therefore outlives the job store's TTL — so
// it is the only place a finished render's real numbers can still be found an hour later.

/**
 * What one `fl2va` ↔ `ref2va` flip costs, in seconds. **Measured 2026-08-07**, not estimated:
 * 31.9 s of `release_model()` + ~20 GB reload, plus 32.5 s of extra first step on the fresh
 * checkpoint. The second half is the part everyone forgets, and it is why "the reload is only 30
 * seconds" under-reports the bill by half.
 *
 * 🔴 The number is what makes the warning actionable. "This chain switches checkpoint 13 times" is
 * a fact nobody can price; "13 minutes of this render is checkpoint reloads" is a decision — and
 * the usual fix is free (a frame pin forces `fl2va`, and a prose keyframe on `ref2va` does not).
 */
export const H3_CHECKPOINT_FLIP_SECONDS = 64;

/** How many of this plan's generations force a reload, and what that costs in wall clock. */
export interface H3ReloadCost {
  reloads: number;
  seconds: number;
  /** `"13 checkpoint switches — about 13m 52s of this render is reloading weights"`, or null. */
  note: string | null;
}

export function h3ReloadCost(plan: H3Plan | null): H3ReloadCost {
  const reloads = planReloads(plan);
  const seconds = reloads * H3_CHECKPOINT_FLIP_SECONDS;
  return {
    reloads,
    seconds,
    note:
      reloads === 0
        ? null
        : `${reloads} checkpoint switch${reloads === 1 ? '' : 'es'} — about ${formatDuration(seconds)} of this render is loading weights, not rendering.`
  };
}

/** The measured wall clock of a finished render, split into the two halves that get confused. */
export interface H3RenderWall {
  /** Guard-acquired to media-fetched. `null` when the plan predates the measurement. */
  render: string | null;
  /** Time spent waiting for the GPU before any of that. `null` when unknown; omitted when 0. */
  gateWait: string | null;
  /** Sum of the per-generation figures the shim reported, when every generation reported one. */
  generations: string | null;
}

/**
 * 🔴 **The three numbers the "~1 hr" report conflated.** A 24.9-minute render was reported as an
 * hour because the editor was timing its own session: a failed submit, a 54.2 s wait for the card,
 * a retry, and then the render. These are the render's own numbers, and they are drawn apart
 * because the fix for each is different — a long queue is scheduling, a long render is geometry.
 *
 * Every field is `null` when unknown. Nothing here defaults to zero: a plan stamped by an older
 * koblem carries no timing at all, and "0s" would read as an instant render.
 */
export function h3RenderWall(plan: H3Plan | null): H3RenderWall {
  const plans = plan?.generation_plans ?? [];
  // 🔴 Summed only when EVERY generation reported. A partial sum drawn next to the whole-job
  // duration reads as "the rest was overhead", when the truth is that the render stopped early.
  const per = plans.map((g) => g.render_seconds).filter((s): s is number => Number.isFinite(s));
  const total = per.reduce((a, b) => a + b, 0);
  return {
    render: plan?.render_ms == null ? null : formatDuration(plan.render_ms / 1000),
    // Zero is a real measurement here (the card was free) and it is not worth a line of its own.
    gateWait: plan?.gate_wait_ms ? formatDuration(plan.gate_wait_ms / 1000) : null,
    generations: plans.length > 0 && per.length === plans.length ? formatDuration(total) : null
  };
}

/**
 * What actually conditioned one generation, as the engine parsed it — the earliest signal that
 * conditioning was dropped.
 *
 * 🔴 `t2va` is the tell. Every H3 task renders and returns a 200; the difference between `ref2va`
 * and `t2va` is whether the references and the song reached the model at all, and a chain that
 * silently fell back to `t2va` looks exactly like a chain that worked until you watch it. Same for
 * a `references[]` that is missing its `<Audio 1>`: the picture is fine and the song is gone.
 *
 * ⚠️ It cannot say what was EXPECTED — the plan does not carry the author's intent. What it can do
 * is name what arrived, so a missing label is visible instead of inferred from the output.
 */
export interface H3Conditioning {
  /** `["<Picture 1> · Mia", "<Audio 1> + <Video 1> · the song"]`, in the engine's packing order. */
  labels: string[];
  /** No conditioning of any kind reached this generation. */
  bare: boolean;
  /** The sentence to draw when something looks wrong, or null when there is nothing to say. */
  note: string | null;
}

export function h3Conditioning(g: H3GenerationPlan): H3Conditioning {
  const refs = g.references ?? [];
  const labels = refs.map((r) => {
    const ords = [r.audio_ordinal, r.ordinal].filter((s): s is string => !!s && !!s.trim());
    const head = ords.length ? ords.join(' + ') : (r.kind ?? 'reference');
    return r.label?.trim() ? `${head} · ${r.label.trim()}` : head;
  });
  const bare = refs.length === 0;
  const isT2va = g.task === 't2va';
  return {
    labels,
    bare,
    note: isT2va
      ? `Rendered as t2va — text only. Nothing conditioned this ${UNIT}: no references, no keyframe, no song. If you scoped any, they did not reach the engine.`
      : bare && g.task
        ? `Rendered as ${g.task} with no references echoed back. On a task that exists to carry them, that is conditioning that went missing between the editor and the model.`
        : null
  };
}

/**
 * `"14m 20s"` / `"48s"` — a duration a person reads at a glance.
 *
 * H3 ETAs are in the hundreds-to-thousands of seconds, where a bare `"1350s"` is a number nobody
 * converts in their head. Returns null for an unknown duration so the caller omits the readout
 * entirely rather than printing a zero.
 */
export function formatDuration(seconds: number | null | undefined): string | null {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds) || seconds < 0) return null;
  const total = Math.round(seconds);
  if (total < 60) return `${total}s`;
  const m = Math.floor(total / 60);
  const s = total % 60;
  if (m < 60) return s ? `${m}m ${s}s` : `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}
