<script lang="ts">
  import type { EngineVocab } from '$lib/director/engine';
  import { capitalise } from '$lib/director/engine';
  import { joinBadge, repairPlan, type UnitContinuity } from '$lib/director/take-continuity';

  /**
   * What a take selection just did to the shots AFTER it — the one thing selecting a take used to
   * do in silence.
   *
   * Selecting a take is already the golden-take picker: the stitched cut is re-derived from the
   * selected take of every shot, and a retake resolves `continuation.from: "prev"` to the previous
   * shot's SELECTED take. This panel exists for the direction that had nothing: shot 4 was rendered
   * continuing out of whichever take of shot 3 was selected at the time, and moving shot 3 to a
   * different take leaves shot 4's opening frames joining a version of shot 3 that no longer plays.
   *
   * 🔴 **It does not offer a rebuild.** A rebuild re-concatenates the selected takes; it cannot
   * rewind the engine's banked latents, and neither engine is bit-reproducible from a seed — so a
   * rebuild here would hand back a cut with the same wrong seam and a green tick on it. The only
   * repair is a re-render, and the button says so.
   *
   * ⚠️ **One at a time, in order.** Each re-render pins to whatever take of its predecessor is
   * selected when it DISPATCHES, and the harvest makes the fresh take the selected one as it lands.
   * Firing the whole plan at once would pin the second render to the take the first is replacing.
   */
  let {
    rows,
    labelOf,
    vocab,
    busy,
    loaded,
    running,
    onload,
    onrepair
  }: {
    /** One entry per render unit in engine index order. Empty until `onload` has run. */
    rows: UnitContinuity[];
    /** How a unit is named on screen — the composer's `Shot N`, or LTX's raw shot number. */
    labelOf: (index: number) => string;
    vocab: EngineVocab;
    busy: boolean;
    /** Whether `rows` reflects a completed read. Distinguishes "nothing wrong" from "not looked". */
    loaded: boolean;
    /** A render is in flight — the repair button must not queue a second one behind it. */
    running: boolean;
    onload: () => void;
    /** Re-render ONE unit, by engine index. */
    onrepair: (index: number) => void;
  } = $props();

  // Only the rows worth a line. `ok`, `first`, `cut`, `pinned` and `unrendered` are all states that
  // are working as intended — listing them would bury the two that are not, and a warning list that
  // is always long is a warning list nobody reads.
  const problems = $derived(
    rows.filter((r) => r.state === 'broken' || r.state === 'downstream' || r.state === 'unknown')
  );
  const plan = $derived(repairPlan(rows));
  const broken = $derived(rows.filter((r) => r.state === 'broken').length);
  const next = $derived(plan.length ? plan[0] : -1);
</script>

<div class="detail-block">
  <div class="lbl-row">
    <span>Joins</span>
    {#if plan.length}
      <span class="tl-meta">{plan.length} to re-render</span>
    {/if}
  </div>

  <div class="inline-row">
    <button class="btn sm" onclick={onload} disabled={busy}
      >{busy ? 'Checking…' : loaded ? 'Re-check joins' : 'Check joins'}</button
    >
  </div>

  {#if !loaded}
    <p class="hint" style="margin-top:6px">
      Reads every take of every {vocab.renderUnit} and works out which one each join was rendered against,
      so a take you picked here does not leave the {vocab.renderUnitPlural} after it joining footage that
      no longer plays.
    </p>
  {:else if problems.length === 0}
    <p class="hint" style="margin-top:6px">
      Every join is to the take that plays. Nothing to re-render.
    </p>
  {:else}
    {#each problems as p (p.index)}
      <div class="jrow" class:bad={p.state === 'broken'} class:soft={p.state === 'unknown'}>
        <span class="jbadge">{joinBadge(p.state)}</span>
        <span class="jtext">{capitalise(p.reason)}</span>
      </div>
    {/each}

    {#if plan.length}
      <!-- 🔴 The whole plan is NAMED but only the head is offered. Each render's pin is resolved at
           dispatch against the selection of the moment, and a fresh take becomes the selected one as
           it harvests — so the second render has to start after the first has landed, not with it. -->
      <p class="hint" style="margin-top:8px">
        <b>Re-render {plan.map(labelOf).join(', ')}{plan.length > 1 ? ' — in that order' : ''}.</b>
        {#if broken && plan.length > broken}
          Only the first {broken === 1 ? 'one is' : `${broken} are`} wrong on screen today; the rest follow
          because re-rendering hands each of them a new tail to join.
        {/if}
      </p>
      <div class="inline-row" style="margin-top:6px">
        <button
          class="btn sm primary"
          disabled={busy || running || next < 0}
          title="Re-renders this {vocab.renderUnit} against the take that plays now. A rebuild cannot fix this — it re-joins the clips you already have, and the seam is baked into them."
          onclick={() => onrepair(next)}>Re-render {labelOf(next)}</button
        >
        {#if running}<span class="hint">A render is already in flight.</span>{/if}
      </div>
      <p class="hint">
        Rebuilding the cut will not fix this. It re-joins the takes you already have, and the join
        is baked into the frames — only a fresh render can join the take you picked.
      </p>
    {/if}
  {/if}
</div>

<!--
  ⚠️ A scoped block, on purpose. `workspace.css` is 2,400 lines of GLOBAL `.dw …` rules and has
  already had two class-name collisions that threw elements to the top of the viewport. Svelte scopes
  these three names to this component, so they cannot be a third. Everything else here (`.detail-block`,
  `.lbl-row`, `.hint`, `.inline-row`, `.btn`, `.tl-meta`) is deliberately left to the global sheet.
-->
<style>
  .jrow {
    display: flex;
    gap: 8px;
    align-items: baseline;
    margin-top: 6px;
    padding: 6px 8px;
    border-radius: 6px;
    border: 1px solid var(--ring);
    background: var(--bg-2);
  }
  /* A break is evidence, so it gets the colour. `downstream` and `unknown` are consequences and
     absences — colouring them the same would make three different claims look like one. */
  .jrow.bad {
    border-color: var(--danger);
  }
  .jrow.soft {
    opacity: 0.8;
  }
  .jbadge {
    flex: 0 0 auto;
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
    white-space: nowrap;
  }
  .jrow.bad .jbadge {
    color: var(--danger);
  }
  .jtext {
    font-size: 0.82rem;
    color: var(--ink);
  }
</style>
