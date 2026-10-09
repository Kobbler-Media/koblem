<script lang="ts">
  import { formatDuration, type H3Telemetry } from '$lib/director/h3-telemetry';
  import { capitalise, DEFAULT_VOCAB, type EngineVocab } from '$lib/director/engine';
  import type { RenderTier, TierPreference } from '$lib/director/tier';
  import { formatShotRanges } from '$lib/director/mobile';
  import { tickLabel } from '$lib/director/timeline-ticks';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import type { JobKind } from '$lib/api/client';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';

  // The whole cut. Per-shot render, retake and takes live on the Prompt screen where the shot is;
  // what is left here is genuinely cut-wide — the tier, the scope of a full render, live progress,
  // the partial strip and the history.
  let {
    vocab = DEFAULT_VOCAB,
    queueOptions = $bindable(),
    queueKind,
    queueAccepted,
    cutSource = null,
    totalDuration,
    unitCount,
    staleNos = [],
    keepNos = [],
    tierPref,
    tier,
    previewFullRes,
    previewW,
    previewH,
    running,
    renderIsMine,
    renderElsewhere,
    renderOwnerName = null,
    progressPct,
    elapsed,
    activeExpectedSec,
    telemetry = null,
    partials = [],
    partialLabel,
    partialSeconds,
    expectedUnits,
    nothingToRender,
    renderError = null,
    reachableNote = null,
    historyCount,
    ontier,
    onrender,
    oncancel,
    onopenhistory,
    onopenshot
  }: {
    vocab?: EngineVocab;
    queueOptions: QueueSubmitOptions;
    queueKind: JobKind;
    queueAccepted: EnqueueLike | null;
    cutSource?: { url: string; proxy: boolean } | null;
    totalDuration: number;
    /** Render units, which is generations on H3 and prompted shots on LTX. */
    unitCount: number;
    /** RAW editor numbers of the shots a full render would redo, and of the ones it would keep. */
    staleNos?: number[];
    keepNos?: number[];
    tierPref: TierPreference;
    tier: RenderTier;
    previewFullRes: boolean;
    previewW: number;
    previewH: number;
    running: boolean;
    renderIsMine: boolean;
    renderElsewhere: boolean;
    renderOwnerName?: string | null;
    progressPct: number;
    elapsed: number;
    activeExpectedSec: number;
    telemetry?: H3Telemetry | null;
    partials?: { index: number; url: string }[];
    partialLabel: (engineIndex: number) => string;
    partialSeconds: (engineIndex: number) => number | null;
    expectedUnits: number;
    nothingToRender: boolean;
    renderError?: string | null;
    reachableNote?: string | null;
    historyCount: number;
    ontier: (next: TierPreference) => void;
    onrender: () => void;
    oncancel: () => void;
    onopenhistory: () => void;
    onopenshot: (engineIndex: number) => void;
  } = $props();

  const TIERS: { value: TierPreference; label: string }[] = [
    { value: 'proxy', label: 'Preview' },
    { value: 'final', label: 'Final' },
    { value: 'auto', label: 'Auto' }
  ];

  // 🔴 The preview tier does not mean the same thing on both engines, and the difference is what
  // the author is choosing between. On H3 the canvas is held and the steps are cut, because a
  // smaller canvas is a different latent shape and therefore a different draw — the preview is the
  // final one under-converged, not a small version of it.
  const tierNote = $derived(
    tierPref === 'proxy'
      ? previewFullRes
        ? `Preview holds the full ${previewW}×${previewH} canvas and cuts the steps, so the framing and the music bed are the ones the final render will have. Cheaper, not tiny.`
        : `Preview renders at ${previewW}×${previewH} — cheap to make and small to stream. Final renders stay untouched.`
      : tierPref === 'final'
        ? `Full resolution — a finished ${vocab.renderUnit} is megabytes, and every playback fetches all of it.`
        : `Auto: this screen is narrow, so ${vocab.renderUnitPlural} render and play as previews.`
  );

  const everything = $derived(staleNos.length === 0 || keepNos.length === 0);
  const remaining = $derived(Math.max(0, activeExpectedSec - elapsed));

  // 🔴 A missing prompt is the only thing that makes the request malformed. A render in flight —
  // here or on another project — and an engine that is down are queue states: the submit enqueues
  // regardless, and a button that read them would flip under the thumb between paint and tap.
  const buttonLabel = $derived(
    nothingToRender
      ? `Nothing to render — write a prompt first`
      : everything
        ? `▶ Render the cut · ${unitCount} ${unitCount === 1 ? vocab.renderUnit : vocab.renderUnitPlural}`
        : `▶ Render ${staleNos.length} stale ${staleNos.length === 1 ? vocab.renderUnit : vocab.renderUnitPlural}`
  );
</script>

<div class="viewer">
  {#if cutSource}
    <!-- svelte-ignore a11y_media_has_caption -->
    <video src={cutSource.url} controls playsinline preload="metadata"></video>
  {:else}
    <span class="ctl" aria-hidden="true">▶</span>
  {/if}
  <span class="tag"
    >{cutSource ? (cutSource.proxy ? 'Preview cut' : 'Cut') : 'No cut yet'} · {unitCount}
    {unitCount === 1 ? vocab.renderUnit : vocab.renderUnitPlural}</span
  >
</div>

<div class="tierseg" role="group" aria-label="Render quality">
  {#each TIERS as t (t.value)}
    <button type="button" aria-pressed={tierPref === t.value} onclick={() => ontier(t.value)}
      >{t.label}</button
    >
  {/each}
</div>
<p class="hint" style="margin:-4px 0 10px">{tierNote} Rendering and playing at <b>{tier}</b>.</p>

{#if reachableNote}
  <div class="alert"><span class="gl">◈</span><span class="ab">{reachableNote}</span></div>
{/if}
{#if renderError}
  <div class="alert bad"><span class="gl">✕</span><span class="ab">{renderError}</span></div>
{/if}
{#if renderElsewhere}
  <div class="alert">
    <span class="gl">◈</span>
    <span class="ab"
      >A render started {renderOwnerName ? `on ${renderOwnerName}` : 'in another project'} is on the engine.
      Starting one here queues behind it — the progress below stays that project's.</span
    >
  </div>
{/if}

<div class="scope">
  <div class="hd">
    <b>Render the cut</b>
    {#if staleNos.length}<span class="pill stale"><i></i>{staleNos.length} stale</span>{/if}
  </div>
  <p class="det">
    {#if nothingToRender}
      Nothing carries a prompt yet, so there is nothing for the engine to draw.
    {:else if everything}
      Renders all {unitCount}
      {unitCount === 1 ? vocab.renderUnit : vocab.renderUnitPlural} at <b>{tier}</b>.
    {:else}
      Re-renders <u>{vocab.renderUnitPlural} {formatShotRanges(staleNos)}</u>. {capitalise(
        vocab.renderUnitPlural
      )}
      {formatShotRanges(keepNos)} keep their takes.
    {/if}
  </p>
</div>

<button class="btn primary full" type="button" onclick={onrender} disabled={nothingToRender}
  >{buttonLabel}</button
>
<p class="qline">
  <QueueChip kind={queueKind} bind:options={queueOptions} response={queueAccepted} />
</p>

{#if running && renderIsMine}
  <div class="ptrack">
    <span class="pfill" style="width:{Math.round(progressPct)}%"></span>
  </div>
  <div class="telrow">
    <span
      >{telemetry?.generationLabel ??
        telemetry?.phaseLabel ??
        `${vocab.renderUnit} in flight`}</span
    >
    <span
      >{telemetry?.stepLabel ?? `${elapsed.toFixed(0)}s`}{remaining > 0
        ? ` · ~${formatDuration(remaining)} left`
        : ''}</span
    >
  </div>
  {#if telemetry?.swapping}
    <p class="hint" style="margin-top:5px">
      {telemetry.phaseNote ?? 'Loading the other checkpoint — this is not a stall.'}
    </p>
  {/if}
  <button class="btn danger full sm" type="button" onclick={oncancel} style="margin-top:8px"
    >Cancel render</button
  >
{/if}

<p class="eye" style="margin-top:14px">
  {capitalise(vocab.renderUnitPlural)} in this take
</p>
{#if partials.length === 0}
  <p class="hint">
    Nothing rendered at <b>{tier}</b> yet. What lands here streams in {vocab.renderUnit} by {vocab.renderUnit}
    while the render runs.
  </p>
{:else}
  <div class="strip">
    {#each Array.from({ length: Math.max(expectedUnits, partials.length) }, (_, i) => i) as i (i)}
      {@const p = partials.find((x) => x.index === i)}
      {@const secs = partialSeconds(i)}
      <div class="tile">
        <button
          class="tv"
          class:pending={!p}
          type="button"
          onclick={() => onopenshot(i)}
          aria-label="Open {vocab.renderUnit} {partialLabel(i)}"
        >
          {#if p}
            <video src={p.url} preload="metadata" playsinline muted></video>
            <span class="pl">▶</span>
          {/if}
        </button>
        <div class="cap">
          <span>{partialLabel(i)}</span><span>{secs === null ? '—' : `${secs.toFixed(1)}s`}</span>
        </div>
      </div>
    {/each}
  </div>
{/if}

<details class="fold" style="margin-top:11px">
  <summary>Stitched cut · {tickLabel(totalDuration, 1)} <span class="chev">›</span></summary>
  <div class="fbody">
    <p class="hint">
      {#if cutSource}
        The {cutSource.proxy ? 'preview' : 'final'} cut above is every selected take joined in order,
        muxed against the original song file — the model's own audio is a reconstruction, so the deliverable
        uses your track.
      {:else}
        No stitched cut yet. One is written at the end of every full render.
      {/if}
    </p>
  </div>
</details>

<div class="mlist">
  <button class="mrow" type="button" onclick={onopenhistory}>
    <span class="gl" aria-hidden="true">▤</span>
    <span class="tx"
      ><b>Render history</b><span
        >{historyCount === 0
          ? 'No renders yet'
          : `${historyCount} render${historyCount === 1 ? '' : 's'} for this project, newest first`}</span
      ></span
    >
    <span class="ch" aria-hidden="true">›</span>
  </button>
</div>
