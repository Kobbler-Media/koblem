<script lang="ts">
  import AppbarOverflow from './AppbarOverflow.svelte';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import type { JobKind } from '$lib/api/client';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import type { RenderTier } from '$lib/director/tier';

  // The top bar and the banners under it. The banners are PASSIVE by design: an autosave that lost
  // a race must not interrupt somebody who did nothing, so saving simply stops until one of the
  // buttons here is pressed and the edits stay on screen throughout.
  let {
    projectName = $bindable(),
    queueOptions = $bindable(),
    queueKind,
    queueAccepted,
    saveStatus,
    engineHealthy,
    engineLabel,
    shotCount,
    totalDuration,
    expectedSec,
    draftAllRunning,
    draftAllDone,
    draftAllTotal,
    draftAllLabel,
    draftFailedCount,
    draftableCount,
    stale,
    warnings,
    nothingToRender,
    unpromptedCount,
    saving,
    renderTier,
    proxyW,
    proxyH,
    previewFullRes,
    continueFromLast,
    continueMisaligned,
    onnameinput,
    oncommitname,
    onopenprojects,
    onopenpanels,
    onopenprompts,
    onopensettings,
    onopenrenders,
    onretrydrafts,
    onrender,
    onreload,
    onoverwrite,
    ondismisswarnings,
    ondraftall
  }: {
    projectName: string;
    queueOptions: QueueSubmitOptions;
    /** The queue kind the primary button submits — the Director drives two engines. */
    queueKind: JobKind;
    /** The last 202, or null before one. What actually happened, which the prediction is not. */
    queueAccepted: EnqueueLike | null;
    saveStatus: string;
    engineHealthy: boolean;
    /** The current engine's own name — the Director drives more than one. */
    engineLabel: string;
    shotCount: number;
    totalDuration: number;
    expectedSec: number;
    draftAllRunning: boolean;
    draftAllDone: number;
    draftAllTotal: number;
    draftAllLabel: string;
    draftFailedCount: number;
    /** Shots with an idea but no prompt — the ones "Draft all" can actually act on. */
    draftableCount: number;
    /** The document changed elsewhere, so every write is paused until that is resolved. */
    stale: boolean;
    /** Dropped-id / structural notes the LLM endpoints reported. Surfaced, never silently binned. */
    warnings: string[];
    nothingToRender: boolean;
    unpromptedCount: number;
    saving: boolean;
    renderTier: RenderTier;
    /** The canvas a PREVIEW renders at — equal to the output canvas when `previewFullRes`. */
    proxyW: number;
    proxyH: number;
    /** The preview tier is the full canvas at fewer steps: predictive, not merely cheap. */
    previewFullRes: boolean;
    continueFromLast: boolean;
    continueMisaligned: boolean;
    onnameinput: () => void;
    oncommitname: () => void;
    onopenprojects: () => void;
    onopenpanels: () => void;
    /**
     * Open the Prompts screen — every block in the project at once. `null` on an engine that has no
     * structured prompt to show (LTX writes its prose on the clip), which hides the button rather
     * than offering one that would open an empty sheet.
     */
    onopenprompts: (() => void) | null;
    onopensettings: () => void;
    onopenrenders: () => void;
    onretrydrafts: () => void;
    onrender: () => void;
    onreload: () => void;
    onoverwrite: () => void;
    ondismisswarnings: () => void;
    ondraftall: () => void;
  } = $props();
</script>

<header class="appbar">
  <a class="brand" href="/" title="Back to koblem"
    ><div class="clap"></div>
    <div class="brand-name"><b>Director</b><span>{engineLabel}</span></div></a
  >
  <div class="proj">
    <input
      class="proj-name"
      bind:value={projectName}
      oninput={onnameinput}
      onblur={oncommitname}
      placeholder="Project name"
    />
    <button
      class="btn icon-only ghost proj-caret"
      title="Switch or manage projects"
      aria-label="Switch or manage projects"
      onclick={onopenprojects}>▾</button
    >
    <span class="saved"><span class="dot"></span>{saveStatus || 'Ready'}</span>
  </div>
  <div class="spacer"></div>
  <span
    class="engine"
    class:off={!engineHealthy}
    title={engineHealthy ? 'Render engine ready' : 'Render engine offline'}
  >
    <span class="edot"></span>{engineHealthy ? 'Engine ready' : 'Engine offline'}
  </span>
  <span class="runtime">
    {shotCount} shots ·
    <b>{totalDuration.toFixed(1)}s</b>{#if expectedSec > 0}
      · full cut <b>~{expectedSec.toFixed(0)}s</b>{/if}
  </span>
  {#if draftAllRunning}
    <span class="ticker" title="Drafting prompts — the editor stays usable">
      <span class="spin"></span>Drafting
      <b>{Math.min(draftAllDone + 1, draftAllTotal)}/{draftAllTotal}</b>{#if draftAllLabel}
        · {draftAllLabel}{/if}
    </span>
  {:else if draftFailedCount}
    <span class="ticker bad">
      ⚑ <b>{draftFailedCount}</b> shot{draftFailedCount === 1 ? '' : 's'} failed to draft
      <button class="btn sm ghost" onclick={onretrydrafts}>Retry</button>
    </span>
  {/if}
  <button
    class="btn ghost panels-btn"
    onclick={onopenpanels}
    title="Story context, Cast, Auto-direct, Revise & review">Panels</button
  >
  {#if onopenprompts}
    <button
      class="btn ghost appbar-secondary"
      onclick={onopenprompts}
      title="Every shot's prompt block as text — filter, find and replace across all of them"
      >Prompts</button
    >
  {/if}
  <button class="btn ghost appbar-secondary" onclick={onopensettings}>Settings</button>
  <button class="btn ghost appbar-secondary" onclick={onopenrenders}>Renders</button>
  <AppbarOverflow
    {engineHealthy}
    {saveStatus}
    {shotCount}
    {totalDuration}
    {expectedSec}
    onsettings={onopensettings}
    onrenders={onopenrenders}
    onprompts={onopenprompts}
  />
  <!-- 🔴 Disabled ONLY on a malformed request. A busy engine, a render on another project and an
       engine that is down are queue states, not input problems: the submit enqueues either way, and
       a button that reads the queue would flip under the cursor between paint and click. The
       consequence goes beside it instead. -->
  <button
    class="btn primary"
    onclick={onrender}
    disabled={nothingToRender}
    title={nothingToRender
      ? `${unpromptedCount} shots have no prompt — draft them first`
      : renderTier === 'proxy'
        ? previewFullRes
          ? `Preview the whole cut at ${proxyW}×${proxyH} — the same canvas and seed as the final render, fewer steps. Faster, not smaller.`
          : `Preview the whole cut at ${proxyW}×${proxyH} — cheap to render and small to stream`
        : undefined}
  >
    {renderTier === 'proxy'
      ? 'Preview all'
      : continueFromLast && !continueMisaligned
        ? 'Render new'
        : 'Render all'}
  </button>
  <QueueChip kind={queueKind} bind:options={queueOptions} response={queueAccepted} />
</header>

{#if stale || warnings.length || nothingToRender || unpromptedCount}
  <div class="warn-zone">
    <!-- Passive by design: an autosave that lost a race must not interrupt somebody who did
         nothing. Saving is off until one of these buttons is pressed; the edits stay on screen. -->
    {#if stale}
      <div class="warn-bar bad">
        <span class="sev bad">changed elsewhere</span>
        <span class="msg"
          >This project was edited on another device, so saving is paused — your changes here are
          still on screen and nothing has been sent. Reload to take that copy (your unsaved edits
          here are lost), or overwrite it with what you have.</span
        >
        <button class="btn sm" onclick={onreload} disabled={saving}>Reload</button>
        <button class="btn sm danger" onclick={onoverwrite} disabled={saving}>Overwrite</button>
      </div>
    {/if}
    {#if warnings.length}
      <div class="warn-bar">
        <span class="sev warn">dropped</span>
        <ul>
          {#each warnings as w, i (i)}<li>{w}</li>{/each}
        </ul>
        <button class="btn sm ghost" onclick={ondismisswarnings}>Dismiss</button>
      </div>
    {/if}
    {#if nothingToRender}
      <div class="warn-bar bad">
        <span class="sev bad">not renderable</span>
        <span class="msg"
          >{unpromptedCount} shot{unpromptedCount === 1 ? ' has' : 's have'} no prompt — draft
          {unpromptedCount === 1 ? 'it' : 'them'} first.</span
        >
        {#if draftableCount}
          <button class="btn sm" onclick={ondraftall} disabled={draftAllRunning}>
            {draftAllRunning ? 'Drafting…' : 'Draft all'}
          </button>
        {/if}
      </div>
    {:else if unpromptedCount}
      <div class="warn-bar">
        <span class="sev warn">skipped</span>
        <span class="msg"
          >{unpromptedCount} shot{unpromptedCount === 1 ? ' has' : 's have'} no prompt and won't be rendered.</span
        >
        {#if draftableCount}
          <button class="btn sm" onclick={ondraftall} disabled={draftAllRunning}>
            {draftAllRunning ? 'Drafting…' : 'Draft all'}
          </button>
        {/if}
      </div>
    {/if}
  </div>
{/if}
