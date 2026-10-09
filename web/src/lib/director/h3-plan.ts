/**
 * The H3 PLAN ECHO — what the engine said it would actually render, read back off the project.
 *
 * When an H3 render is dispatched the engine answers 202 with a plan: the geometry the caller did
 * NOT choose. koblem stamps that whole document onto the project as server metadata under
 * `h3_plan` (`stamp_h3_plan` in `routes/ltx_director.rs`), deliberately as server-managed metadata
 * so the editor's next autosave cannot drop it, and so the answer survives the job ageing out of
 * the store. There is no endpoint to add: `DirectorProjectSummary.metadata` already carries it, and
 * {@link readH3Plan} is the accessor.
 *
 * ⚠️ **The echo is captured at the 202 but WRITTEN when the job ends.** `stamp_h3_plan` runs after
 * `run_h3_job` returns — including on its failure path — so a project row read while a render is in
 * flight still holds the PREVIOUS run's plan. The only honest moment to re-read it is when the job
 * goes terminal, which is why the panel appears after a run rather than during one, and why `at` is
 * shown: it is what tells two plans apart.
 *
 * ## Three lists that must never be merged
 *
 * The panel that draws this keeps them visually apart, and the reason is not tidiness — they are
 * three different failures with three different fixes:
 *
 * - `warnings` — **it happened, but not as you asked.** A frame count snapped up onto the 5+17k
 *   grid, a timeline split across generations so there is now a seam. The render is what you get.
 * - `ignored` — **the engine threw it away.** 🔴 This is the load-bearing one. WanGP filters its
 *   task dict against `generate_media`'s signature and drops unknown keys *with no warning at all*
 *   (`api_cli.py:121`), so the default renders, the job succeeds, and nobody finds out. This array
 *   is the ONLY channel through which that becomes visible.
 * - `dropped` — **koblem never sent it.** Its allowlist declined the field on the way out, which is
 *   what happens to a project that used to be LTX and still carries LoRAs, beats, a negative
 *   prompt or `pin_strength` in its saved doc.
 *
 * Everything is optional, because the shim may stop sending a diagnostic field and an accepted
 * render must not become an error over one. Nothing here is derived or corrected: this is a REPORT.
 */

/** One generation as the engine will actually render it. Every field optional — see the note above. */
export interface H3GenerationPlan {
  index?: number;
  task?: string;
  /**
   * 🔴 **SNAPPED.** The engine rounds a frame count UP onto its 5+17k grid, so this is what
   * renders, not what was asked for. A generation authored at 350 frames appears here as 362 and
   * the difference is a `warnings` entry.
   */
  frames?: number;
  seconds?: number;
  width?: number;
  height?: number;
  steps?: number;
  seed?: number;
  /** The weight set this lands on. DERIVED from what is in scope, never chosen. */
  checkpoint?: string;
  /**
   * This generation forces a full `release_model()` + ~20 GB reload because it wants a different
   * checkpoint than the one in VRAM. Folded into `eta_seconds`, and the reason a render appears to
   * pause for minutes between two generations.
   */
  reload_required?: boolean;
  eta_seconds?: number;
  prompt_chars?: number;
  /**
   * 🔴 **What actually conditioned this generation, in the engine's own words.** The earliest
   * possible signal that conditioning was dropped: `task: "t2va"` where `ref2va` was expected means
   * every reference went out of scope, and a `references[]` missing its `<Audio 1>` means the song
   * never attached — both of which render perfectly happily and cost an hour to discover in the
   * output. The shim builds this from the request it PARSED (`contract.py:789`), not from what was
   * sent, so it is the far side of every allowlist and every silent drop.
   *
   * `audio_ordinal` is the second label a `with_audio` video packs: `_presentation` emits
   * `<Audio k>` for the extracted soundtrack immediately BEFORE `<Video k>`, and a prompt may
   * address either. An echo that named only the video was under-reporting a handle the author had.
   */
  references?: {
    ordinal?: string | null;
    audio_ordinal?: string | null;
    kind?: string;
    role?: string;
    with_audio?: boolean;
    tail_frames?: number | null;
    label?: string | null;
  }[];
  /**
   * What this generation ACTUALLY cost on the GPU, measured by the shim and folded into the stamp
   * by koblem when the render ended (`H3SubmitPlan::record_timing`).
   *
   * 🔴 Absent means NOT MEASURED — a generation the render never reached, or a plan captured before
   * the job ended. Never draw it as zero: the whole reason this field exists is that the editor's
   * own figure is a session wall clock which counted a failed submit, a 54.2 s queue and a retry,
   * and reported a 24.9-minute render as "~1 hr".
   */
  render_seconds?: number;
}

/** The stamped plan, as `metadata.h3_plan`. */
export interface H3Plan {
  generation_plans: H3GenerationPlan[];
  /** Sum of the per-generation ETAs, INCLUDING any reload the chain forces. */
  total_eta_seconds: number | null;
  canvas: { width?: number; height?: number; fps?: number } | null;
  /** Happened differently. */
  warnings: string[];
  /** Reached the engine and was discarded there. */
  ignored: string[];
  /** koblem's allowlist never sent it. */
  dropped: string[];
  /** Totals summed from the per-generation plans — there is no top-level number to trust. */
  frames: number | null;
  duration_seconds: number | null;
  /**
   * How long this render waited for the GPU before it started, in milliseconds.
   *
   * 🔴 Kept apart from {@link render_ms} on purpose and never summed into it. A queue is somebody
   * else's render; folding it into the duration makes this engine look slow for being busy, and it
   * is half of why a 24.9-minute render was reported as "~1 hr" (the other half was a failed submit
   * and a retry, which the session wall clock also counted).
   */
  gate_wait_ms: number | null;
  /** GPU-acquired to render-complete, in milliseconds. `null` on a plan captured before the end. */
  render_ms: number | null;
  /** RFC3339 stamp of the run this plan came back from. */
  at: string | null;
}

function str(v: unknown): string | null {
  return typeof v === 'string' && v.trim() ? v : null;
}

function numOrNull(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null;
}

/** Only the strings, and only the non-empty ones — a stray null must not render as a blank row. */
function strings(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string' && !!x.trim()) : [];
}

/**
 * Read the plan echo off a project's server metadata. `null` when the project has never run on H3,
 * or when the stamp is not an object — an older koblem simply does not write the key.
 *
 * 🔴 Takes the metadata bag rather than fetching: `h3_plan` is stamped SERVER-side after dispatch,
 * so the way to see a fresh one is to re-read the project row (the list endpoint's summary carries
 * metadata and is the cheap read), not to poll a plan endpoint that does not exist.
 */
export function readH3Plan(metadata: Record<string, unknown> | null | undefined): H3Plan | null {
  const raw = metadata?.h3_plan;
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  const p = raw as Record<string, unknown>;
  const plans = Array.isArray(p.generation_plans)
    ? (p.generation_plans.filter(
        (g) => !!g && typeof g === 'object' && !Array.isArray(g)
      ) as H3GenerationPlan[])
    : [];
  const canvas =
    p.canvas && typeof p.canvas === 'object' && !Array.isArray(p.canvas)
      ? (p.canvas as H3Plan['canvas'])
      : null;
  return {
    generation_plans: plans,
    total_eta_seconds: numOrNull(p.total_eta_seconds),
    canvas,
    warnings: strings(p.warnings),
    ignored: strings(p.ignored),
    dropped: strings(p.dropped),
    frames: numOrNull(p.frames),
    duration_seconds: numOrNull(p.duration_seconds),
    gate_wait_ms: numOrNull(p.gate_wait_ms),
    render_ms: numOrNull(p.render_ms),
    at: str(p.at)
  };
}

/** Nothing at all to say — no plans, no notes. Keeps the panel out of the way after a clean run. */
export function planIsEmpty(plan: H3Plan | null): boolean {
  return (
    !plan ||
    (plan.generation_plans.length === 0 &&
      plan.warnings.length === 0 &&
      plan.ignored.length === 0 &&
      plan.dropped.length === 0)
  );
}

/** How many of this plan's generations force a checkpoint reload — the pauses to expect. */
export function planReloads(plan: H3Plan | null): number {
  return plan ? plan.generation_plans.filter((g) => g.reload_required === true).length : 0;
}

/** Total count of things that did not happen as authored, across all three lists. */
export function planNoteCount(plan: H3Plan | null): number {
  return plan ? plan.warnings.length + plan.ignored.length + plan.dropped.length : 0;
}
