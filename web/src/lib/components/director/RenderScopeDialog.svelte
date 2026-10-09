<script lang="ts">
  import type { DirectorRenderPlan, H3Staleness } from '$lib/api/client';
  import type { EngineVocab } from '$lib/director/engine';
  import { DEFAULT_VOCAB } from '$lib/director/engine';

  /**
   * The choice between continuing a project and starting its tier over.
   *
   * 🔴 This exists because "Render all" is DESTRUCTIVE and did not say so. A completed full run
   * calls `delete_by_project_tier_except`, which drops every take of the tier that this run did not
   * write — including every retake — and unlinks the files. There is no undo. An hour of
   * prompt-chasing could be lost to a button whose label read like "render the project".
   *
   * ⚠️ Shown ONLY when there is something to lose (`existing_takes > 0`) and only on an engine that
   * can answer the question. A first render, or a project whose takes were already wiped, dispatches
   * with no dialog at all — a confirmation nobody can act on is one everybody learns to dismiss.
   */
  let {
    plan,
    vocab = DEFAULT_VOCAB,
    secondsPerGeneration = null,
    onchoose
  }: {
    plan: DirectorRenderPlan;
    vocab?: EngineVocab;
    /** Observed wall time for one generation, for the estimate. `null` hides it. */
    secondsPerGeneration?: number | null;
    /** `null` = cancelled. `rebuild` re-joins the takes without rendering anything. */
    onchoose: (scope: 'stale' | 'all' | 'rebuild' | null) => void;
  } = $props();

  /** Reasons listed in full before collapsing to a count. Beyond this the list stops informing. */
  const MAX_REASONS = 6;

  const total = $derived(plan.total ?? 0);
  const continueCount = $derived(plan.continue_count ?? 0);
  const existing = $derived(plan.existing_takes ?? 0);
  const unit = $derived(vocab.renderUnit);
  const upToDate = $derived(continueCount === 0);
  const units = (n: number) => (n === 1 ? vocab.renderUnit : vocab.renderUnitPlural);

  /** Human reason per generation, for the small list under the Continue option. */
  const WHY: Record<H3Staleness, string> = {
    never_rendered: 'never rendered',
    prompt_changed: 'prompt changed since its take',
    continues_from_stale: 'continues from a shot being re-rendered',
    inside_the_span: 'sits between two shots being re-rendered'
  };

  const reasons = $derived(
    (plan.generations ?? [])
      .map((why, i) => ({ i, why }))
      .filter((r): r is { i: number; why: H3Staleness } => r.why !== null)
  );

  function estimate(n: number): string {
    if (!secondsPerGeneration || n <= 0) return '';
    const mins = Math.round((secondsPerGeneration * n) / 60);
    return mins >= 1 ? ` (~${mins} min)` : '';
  }

  // Escape cancels, matching every other dismissable surface in the editor.
  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onchoose(null);
  }
</script>

<svelte:window {onkeydown} />

<!-- Dismiss on the scrim's OWN surface rather than stopping propagation on the card: a card that
  swallows every click is a non-interactive element carrying a click handler, which is both an a11y
  warning and a real trap for anything that later wants to listen above it. -->
<div
  class="scrim"
  role="presentation"
  onclick={(e) => e.target === e.currentTarget && onchoose(null)}
>
  <!-- tabindex so the modal container itself can take focus, which is what `aria-modal` promises. -->
  <div class="card" role="dialog" aria-modal="true" aria-labelledby="rsd-title" tabindex="-1">
    <!-- Titled with the button that opened it, or the dialog reads as a different action. -->
    <h2 id="rsd-title">{plan.tier === 'proxy' ? 'Preview all' : 'Render all'}</h2>
    <p class="lede">
      This project has {existing} take{existing === 1 ? '' : 's'} on the
      <strong>{plan.tier}</strong> tier. Pick what happens to them.
    </p>

    <button class="opt" disabled={upToDate} onclick={() => onchoose('stale')}>
      <span class="opt-h"
        >Continue{upToDate
          ? ' — nothing to render'
          : ` — render ${continueCount} of ${total} ${units(continueCount)}${estimate(continueCount)}`}</span
      >
      {#if upToDate}
        <span class="opt-d">Every {unit} has a take matching its current prompt.</span>
      {:else}
        <ul class="why">
          {#each reasons.slice(0, MAX_REASONS) as r (r.i)}
            <li><b>{unit} {r.i + 1}</b> — {WHY[r.why]}</li>
          {/each}
          {#if reasons.length > MAX_REASONS}
            <li>…and {reasons.length - MAX_REASONS} more</li>
          {/if}
        </ul>
        <span class="opt-d"
          >Every other take is kept exactly as it is, and the cut is re-joined from all of them when
          this finishes.</span
        >
      {/if}
    </button>

    <button class="opt" onclick={() => onchoose('rebuild')}>
      <span class="opt-h">Combine takes into the cut — no rendering</span>
      <span class="opt-d">
        Re-joins the {total} selected take{total === 1 ? '' : 's'} into one video. Renders nothing, deletes
        nothing, takes seconds.
      </span>
    </button>

    <button class="opt danger" onclick={() => onchoose('all')}>
      <span class="opt-h">Re-render everything — {total} {units(total)}{estimate(total)}</span>
      <span class="opt-d">
        <b>Deletes all {existing} existing take{existing === 1 ? '' : 's'}</b> on this tier, including
        every retake. This cannot be undone.
      </span>
    </button>

    <div class="foot"><button class="ghost" onclick={() => onchoose(null)}>Cancel</button></div>
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    padding: 16px;
    background: rgb(0 0 0 / 0.55);
  }
  .card {
    width: min(560px, 100%);
    max-height: 90vh;
    overflow-y: auto;
    padding: 18px;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: var(--bg-2);
    color: var(--ink);
  }
  h2 {
    margin: 0 0 4px;
    font-size: 1.05rem;
  }
  .lede {
    margin: 0 0 14px;
    color: var(--muted);
    font-size: 0.85rem;
  }
  .opt {
    display: block;
    width: 100%;
    margin-bottom: 10px;
    padding: 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--bg-3);
    color: var(--ink);
    text-align: left;
    cursor: pointer;
  }
  .opt:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .opt:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .opt.danger:hover {
    border-color: var(--danger);
  }
  .opt-h {
    display: block;
    margin-bottom: 4px;
    font-weight: 600;
  }
  .opt-d {
    display: block;
    color: var(--muted);
    font-size: 0.8rem;
  }
  .why {
    margin: 6px 0;
    padding-left: 18px;
    color: var(--muted);
    font-size: 0.8rem;
  }
  .foot {
    display: flex;
    justify-content: flex-end;
  }
  .ghost {
    padding: 7px 12px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: transparent;
    color: var(--muted);
    cursor: pointer;
  }
</style>
