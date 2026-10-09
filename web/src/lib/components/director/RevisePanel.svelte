<script lang="ts">
  // Whole-script operations: rework everything toward one change, review it for continuity, backfill
  // summaries, draft the shots that only have an idea, rewrite every prompt in one LLM pass,
  // storyboard them, or extend the story.
  //
  // ⚠️ Two of these write prompts and they are not the same thing. `Draft all` is one call PER unit,
  // fills only the ones that have an idea and no prompt, and leaves everything else alone. `Rewrite
  // all … in one pass` is ONE call for the whole project and REPLACES every prompt. They are
  // mutually disabled while either runs, because both write the same fields.
  let {
    reworkInstruction = $bindable(),
    extendDirection = $bindable(),
    extendCount = $bindable(),
    reworking,
    reviewing,
    gistingAll,
    draftAllRunning,
    storyboarding,
    storyboardProgress,
    extending,
    imageEngineReady,
    reviewRan,
    findingCount,
    scriptingAll,
    scriptUnits,
    scriptUnitCap,
    unitPlural,
    onrework,
    onreview,
    ongistall,
    ondraftall,
    onscriptall,
    onstoryboardall,
    onextend,
    onopenfindings
  }: {
    reworkInstruction: string;
    extendDirection: string;
    extendCount: number;
    reworking: boolean;
    reviewing: boolean;
    gistingAll: boolean;
    draftAllRunning: boolean;
    storyboarding: boolean;
    storyboardProgress: string;
    extending: boolean;
    imageEngineReady: boolean;
    /** Whether a review has run at all — "no findings" only means something once it has. */
    reviewRan: boolean;
    findingCount: number;
    scriptingAll: boolean;
    /**
     * How many render units one pass would write — generations on H3, clips on LTX.
     *
     * Passed in rather than counted here: the panel has neither the packing nor the clips, and the
     * count is the request's `n_segments`, so it has to be the one the caller will actually send.
     */
    scriptUnits: number;
    /** The endpoint's own ceiling on `n_segments`. Over it the action refuses rather than truncates. */
    scriptUnitCap: number;
    /** `vocab.renderUnitPlural` — one spelling of the word, on either engine. */
    unitPlural: string;
    onrework: () => void;
    onreview: () => void;
    ongistall: () => void;
    ondraftall: () => void;
    onscriptall: () => void;
    onstoryboardall: () => void;
    onextend: () => void;
    onopenfindings: () => void;
  } = $props();

  // Over the endpoint's ceiling the caller refuses outright (a clamped `n_segments` would answer
  // with 24 prompts for a 30-unit timeline and land them on the first 24 in silence). Disabled here
  // so the reason is visible BEFORE the click rather than as an error after it.
  const overCap = $derived(scriptUnits > scriptUnitCap);
</script>

<label class="field"
  ><span>Rework whole script</span>
  <input
    class="inp"
    bind:value={reworkInstruction}
    onkeydown={(e) => e.key === 'Enter' && onrework()}
    placeholder="move it to night, make the lead a woman…"
  />
</label>
<div class="inline-row" style="flex-wrap:wrap;gap:6px;margin-bottom:11px">
  <button class="btn sm" onclick={onrework} disabled={reworking || !reworkInstruction.trim()}
    >{reworking ? 'Reworking…' : 'Rework'}</button
  >
  <button class="btn sm" onclick={onreview} disabled={reviewing}
    >{reviewing ? 'Reviewing…' : 'Review'}</button
  >
  <button class="btn sm" onclick={ongistall} disabled={gistingAll}
    >{gistingAll ? 'Summarising…' : 'Gist all'}</button
  >
  <button
    class="btn sm"
    onclick={ondraftall}
    disabled={draftAllRunning || scriptingAll}
    title="Draft a full prompt for every shot that has an idea but no prompt"
    >{draftAllRunning ? 'Drafting…' : 'Draft all'}</button
  >
  <button class="btn sm" onclick={onstoryboardall} disabled={storyboarding || !imageEngineReady}
    >{storyboarding ? `Storyboarding ${storyboardProgress}…` : 'Storyboard all'}</button
  >
  <!-- ⚠️ The one destructive button in this row, and it says so twice: in the label ("Rewrite") and
       in the tooltip. The caller also asks before it runs — a single-unit draft replaces one prompt
       and warns on its own button, and doing it to every unit at once deserves at least as much. -->
  <button
    class="btn sm"
    onclick={onscriptall}
    disabled={scriptingAll || draftAllRunning || overCap || scriptUnits === 0}
    title={overCap
      ? `One pass writes at most ${scriptUnitCap} ${unitPlural} and this project has ${scriptUnits}. Use Draft all, which writes them one at a time.`
      : `Write all ${scriptUnits} ${unitPlural} in ONE LLM pass, told the timeline's order, lengths and beat counts. REPLACES every prompt, hand-written text included.`}
    >{scriptingAll
      ? 'Writing…'
      : `Rewrite all${scriptUnits ? ` ${scriptUnits}` : ''} in one pass`}</button
  >
</div>
<label class="field"
  ><span>Extend story</span>
  <div class="inline-row">
    <input
      class="inp"
      style="flex:2"
      bind:value={extendDirection}
      onkeydown={(e) => e.key === 'Enter' && onextend()}
      placeholder="where next? — wrap it up, add a twist…"
    />
    <input class="inp" type="number" min="1" max="12" bind:value={extendCount} style="width:52px" />
    <button class="btn sm" onclick={onextend} disabled={extending}
      >{extending ? '…' : 'Extend'}</button
    >
  </div>
</label>
{#if reviewRan}
  <div class="lbl-row" style="margin-bottom:2px"><span>Findings</span></div>
  {#if findingCount === 0}
    <p class="hint" style="margin:0">✓ No issues found — the script reads clean.</p>
  {:else}
    <p class="hint" style="margin:0 0 9px">
      {findingCount} issue{findingCount > 1 ? 's' : ''} — affected shots are flagged on the timeline.
    </p>
    <button class="btn sm" style="width:100%;justify-content:center" onclick={onopenfindings}
      >Review findings</button
    >
  {/if}
{/if}
