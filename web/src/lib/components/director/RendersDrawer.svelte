<script lang="ts">
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import TierToggle from './TierToggle.svelte';
  import H3PlanEcho from './H3PlanEcho.svelte';
  import type { HeldClip } from '$lib/director/renders';
  import type { RenderTier, TierPreference } from '$lib/director/tier';
  import type { DirectorClip, JobKind, LtxBankListing } from '$lib/api/client';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import { DEFAULT_VOCAB, UNAVAILABLE, type EngineVocab } from '$lib/director/engine';
  import { formatDuration, type H3Telemetry } from '$lib/director/h3-telemetry';
  import type { H3Plan } from '$lib/director/h3-plan';
  import type { Generation } from '$lib/types';

  // Everything a render produced: the stitched cut of whichever tier this client plays, the
  // per-shot clips that landed before it, the live progress of anything in flight, and the
  // dispatch buttons that are only reachable once you are looking at the output.
  let {
    continueFromLast = $bindable(),
    previewShots = $bindable(),
    cutSource,
    resultUrl,
    proxyResultUrl,
    partialShots,
    proxyShots,
    proxyW,
    proxyH,
    previewFullRes,
    outW,
    outH,
    running,
    renderIsMine,
    renderElsewhere,
    renderOwnerName,
    activeRenderTier,
    elapsed,
    activeExpectedSec,
    progressPct,
    telemetry = null,
    h3Plan = null,
    vocab = DEFAULT_VOCAB,
    historyService = 'ltx-video',
    reachableNote = null,
    promptedCount,
    queueKind,
    queueOptions = $bindable(),
    queueAccepted,
    nothingToRender,
    continueMisaligned,
    v2vMissingSource,
    priorRenders,
    summary,
    summaryStale,
    summarizing,
    renderError,
    restoreNote,
    historyKey,
    tierPref,
    renderTier,
    narrow,
    resume = true,
    resumeNote,
    canRetake = true,
    retakeNote = '',
    bank = null,
    bankBusy = false,
    bankError = null,
    onloadbank = null,
    expectedShots,
    partialShotNo,
    onrenderunit,
    onclose,
    onshot,
    onrender,
    onquickpreview,
    onretakelast,
    onsummarise,
    oncancel,
    ontier,
    onrestore
  }: {
    continueFromLast: boolean;
    previewShots: number;
    /** The cut this client plays — proxy substitutes only on the proxy tier. */
    cutSource: { url: string; proxy: boolean } | null;
    resultUrl: string | null;
    proxyResultUrl: string | null;
    partialShots: HeldClip[];
    proxyShots: HeldClip[];
    /** The canvas a PREVIEW renders at — equal to the output canvas when `previewFullRes`. */
    proxyW: number;
    proxyH: number;
    /**
     * The preview tier is the full canvas at fewer steps rather than a smaller canvas — so it is
     * PREDICTIVE of the final render (same latent shape ⇒ byte-identical initial noise ⇒ the same
     * framing and the same audio bed) and it is not smaller to stream. "Sooner", not "rougher".
     */
    previewFullRes: boolean;
    outW: number;
    outH: number;
    running: boolean;
    renderIsMine: boolean;
    renderElsewhere: boolean;
    renderOwnerName: string;
    activeRenderTier: RenderTier;
    elapsed: number;
    activeExpectedSec: number;
    progressPct: number;
    /**
     * The engine's own live job telemetry, or `null` on an engine that reports none.
     *
     * 🔴 It does not replace `progressPct` — that is the editor's own elapsed-vs-last-render
     * estimate and is the ONLY progress an LTX render has. This is added beside it, and every
     * member is independently nullable: absence means unknown, never zero. Only MiniMax-H3 fills
     * it in, so the LTX drawer is unchanged.
     */
    telemetry: H3Telemetry | null;
    /**
     * The last H3 dispatch's PLAN ECHO, stamped onto the project. `null` for a project that has
     * never rendered on H3 — the panel is simply absent rather than empty.
     */
    h3Plan: H3Plan | null;
    /** What this engine calls its units — forwarded to `H3PlanEcho`. See `EngineVocab`. */
    vocab?: EngineVocab;
    /**
     * Which `generations` service the History list reads. See `historyServiceFor`.
     *
     * 🔴 A prop rather than a constant because the two engines file renders under DIFFERENT
     * services, and asking for the wrong one returns an empty list rather than an error: this was
     * hardcoded to `ltx-video`, so H3 projects showed no history at all while their rows accumulated
     * under `minimax-h3`. The default keeps every LTX caller unchanged.
     */
    historyService?: string;
    /**
     * The engine says this box currently tops out below the spec cap and some generation in this
     * timeline is over it. A WARNING string, or `null` — including when the engine reports no
     * reduction, is down, or never sent the field. Nothing here re-caps the authoring limits; see
     * `reachableCeilingNote` for why a runtime ceiling must not reach the packer.
     */
    reachableNote: string | null;
    promptedCount: number;
    queueKind: JobKind;
    queueOptions: QueueSubmitOptions;
    queueAccepted: EnqueueLike | null;
    nothingToRender: boolean;
    continueMisaligned: boolean;
    v2vMissingSource: DirectorClip[];
    priorRenders: { job_id: string; serverJobId: string | null; label: string; segments: number }[];
    summary: string | null;
    summaryStale: boolean;
    summarizing: boolean;
    renderError: string | null;
    restoreNote: string;
    historyKey: number;
    tierPref: TierPreference;
    renderTier: RenderTier;
    narrow: boolean;
    /**
     * Whether this DEPLOYMENT can continue a render from a banked prefix at all.
     *
     * False on MiniMax-H3, which keeps no latent bank, and false on an LTX-2.5 box whose bank
     * directory is unset — the capability alone was not enough, because the same 2.5 build answers
     * 501 to `resume_job_id` without one. `resumeAvailable` is what the caller reads. Undrawn, the
     * tick-box is a live control setting a flag that is then dropped or refused: the user asks for
     * an incremental render and silently gets a full one, at full cost, with no way to tell.
     */
    resume?: boolean;
    /**
     * WHY there is no continue tick-box, when there is none.
     *
     * Passed in rather than looked up from a map here: the two engines that lack a bank lack it
     * for different reasons, and printing H3's explanation on an LTX-2.5 project would answer the
     * question by naming a model the project is not rendering on. Defaults to H3's, which is the
     * only engine that had this before.
     */
    resumeNote?: string;
    /**
     * This deployment can re-roll ONE unit. Not the same question as {@link resume}: H3 renders a
     * generation on its own with no bank at all, while an LTX chain reaches its retake through one.
     */
    canRetake?: boolean;
    /**
     * What a retake does to the units AFTER the one it re-renders. See `retakeTailNote`.
     *
     * 🔴 It is not the same answer on every engine and the difference is worth a render: LTX-2.3
     * end-pins the banked tail to the retaken shot's new final frame, LTX-2.5 restores the tail as
     * it was. Saying "splices it back in" on the second is a promise the engine does not make, and
     * the drift it hides only shows up when somebody watches the join.
     */
    retakeNote?: string;
    /**
     * The engine's latent bank as it is on disk, once it has been asked for. `null` until then.
     *
     * 🔴 Not derivable from `priorRenders`. A resume id lives in the saved document forever; the
     * latents it names are dropped by a byte-bounded, newest-first-keep sweep, so a render old
     * enough to have been swept is no longer resumable however recently it was watched — and the
     * tick-box above cannot tell. The resume still "works": it re-renders the whole prefix at full
     * cost and reports itself as a continue.
     */
    bank?: LtxBankListing | null;
    bankBusy?: boolean;
    bankError?: string | null;
    /** Read the bank. `null` where this deployment has none to read — the section is then absent. */
    onloadbank?: (() => void) | null;
    /**
     * How many RENDER UNITS this tier is supposed to end up with — prompted shots on LTX,
     * packed generations on H3. `0` = unknown, and the gap readout stays quiet.
     *
     * 🔴 Without it "12 rendered" has no denominator, and a run that died at generation 12 of 16
     * is indistinguishable from a complete one. On a ~16-generation cut that is the difference
     * between "re-render the missing one" and "re-render eight hours".
     */
    expectedShots: number;
    /** Engine-space index → the RAW editor shot number, so labels match the stage slate. */
    partialShotNo: (filteredIdx: number) => number;
    /**
     * Render the one missing unit at `filteredIdx`, on the FINAL tier. Null when this engine
     * cannot render a single unit on its own (LTX renders a prefix, not a hole).
     */
    onrenderunit: ((filteredIdx: number) => void) | null;
    onclose: () => void;
    /** Open one of the per-shot clips in the stage. */
    onshot: (filteredIdx: number) => void;
    onrender: () => void;
    onquickpreview: () => void;
    onretakelast: () => void;
    onsummarise: () => void;
    oncancel: () => void;
    ontier: (next: TierPreference) => void;
    onrestore: (g: Generation) => void;
  } = $props();

  /**
   * How many per-shot clips mount as live `<video>` elements at once.
   *
   * Each one is a decoder. A long MiniMax-H3 cut is ~16 generations of 362f/1344×768 x265 CRF 8,
   * so an uncapped strip of `autoplay loop` videos was 16 simultaneous HEVC decodes **on the same
   * box that is running the render** — and browsers hardware-decode only a handful before the rest
   * fall back to software. This caps the DECODERS, which is the part that competes with the GPU
   * job; the bytes are already in memory as blobs either way (see `planHydration`).
   *
   * The LATEST are kept rather than the first: during a render the tail is the frontier, and on the
   * final tier the strip is replaced by the stitched cut the moment the render lands.
   */
  const PARTIAL_CAP = 6;
  let showAllFinal = $state(false);
  let showAllProxy = $state(false);

  /** Enough keys to recognise the last render or two; the listing is engine-wide and can be long. */
  const BANK_KEYS_SHOWN = 12;
  const gb = (bytes: number): string =>
    bytes >= 1e9 ? `${(bytes / 1e9).toFixed(1)} GB` : `${Math.round(bytes / 1e6)} MB`;

  /** The visible tail of a strip. */
  function capStrip(shots: HeldClip[], all: boolean): HeldClip[] {
    return all || shots.length <= PARTIAL_CAP ? shots : shots.slice(-PARTIAL_CAP);
  }

  // Play on demand. Nothing autoplays, `preload="metadata"` holds every tile at its poster frame,
  // and a clip only decodes while the pointer is on it. Touch clients never hover — there the tile
  // is a still and the click opens it in the stage, which is the primary action anyway.
  function hoverPlay(e: MouseEvent & { currentTarget: HTMLVideoElement }) {
    void e.currentTarget.play().catch(() => {});
  }
  function hoverStop(e: MouseEvent & { currentTarget: HTMLVideoElement }) {
    e.currentTarget.pause();
    e.currentTarget.currentTime = 0;
  }
</script>

{#snippet strip(shots: HeldClip[], all: boolean, showAll: () => void)}
  <div class="partials-row">
    {#each capStrip(shots, all) as shot (shot.index)}
      <button
        class="partial-shot"
        onclick={() => onshot(shot.index)}
        title="Show shot {partialShotNo(shot.index)} in the stage"
      >
        <video
          src={shot.url}
          muted
          loop
          playsinline
          preload="metadata"
          class="partial-vid"
          onmouseenter={hoverPlay}
          onmouseleave={hoverStop}
        ></video>
        <span class="partial-label">Shot {partialShotNo(shot.index)}</span>
      </button>
    {/each}
  </div>
  {#if !all && shots.length > PARTIAL_CAP}
    <button class="btn sm ghost" style="align-self:flex-start" onclick={showAll}
      >Show all {shots.length} — mounts {shots.length - PARTIAL_CAP} more players</button
    >
  {/if}
{/snippet}

<!--
  Which render units this tier is still missing, and a way to fill each one.

  🔴 A long H3 cut is one ~8-hour job. A cancel or a crash keeps every generation that finished —
  the engine reports them progressively, koblem harvests them on the terminal poll too, and the
  stitch orders takes by shot index across job ids — so the recovery from "died at 12 of 16" is
  supposed to be "render 12, 13, 14, 15". None of that was reachable, because nothing in the UI
  ever said WHICH units were missing. This is that readout.

  Rendered strictly from what this client holds, so it is honest after a reload as well as live.
-->
{#snippet gaps(shots: HeldClip[])}
  {@const have = new Set(shots.map((s) => s.index))}
  {@const missing = Array.from({ length: expectedShots }, (_, i) => i).filter((i) => !have.has(i))}
  {#if expectedShots > 0 && missing.length > 0 && shots.length > 0}
    <div class="partials-head" style="color:var(--warning)">
      {missing.length} not rendered — the cut is incomplete until {missing.length > 1
        ? 'they land'
        : 'it lands'}.
    </div>
    <div class="partials-row">
      {#each missing as idx (idx)}
        <button
          class="btn sm"
          onclick={() => onrenderunit?.(idx)}
          disabled={!onrenderunit}
          title={onrenderunit
            ? 'Render only this one — everything else is already on disk'
            : 'This engine renders a prefix, not a single unit — use “render through” this shot'}
          >{onrenderunit ? 'Render' : 'Shot'}
          {partialShotNo(idx)}</button
        >
      {/each}
    </div>
  {/if}
{/snippet}

<div class="overlay" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="drawer">
    <div class="sheet-h">
      <h3>Renders</h3>
      <button class="btn icon-only ghost" style="margin-left:auto" onclick={onclose}>✕</button>
    </div>
    <div class="render-grid">
      <div>
        {#if v2vMissingSource.length > 0 || (continueFromLast && continueMisaligned)}
          <div class="warns">
            {#if v2vMissingSource.length > 0}<div class="wrow">
                ⚠ <span
                  >{v2vMissingSource.length} v2v shot{v2vMissingSource.length > 1
                    ? 's have'
                    : ' has'} no source — render{v2vMissingSource.length > 1 ? '' : 's'} as plain text-to-video
                  until you attach one.</span
                >
              </div>{/if}
            {#if continueFromLast && continueMisaligned}<div class="wrow">
                ⚠ <span
                  >Continue-from-last can't line up (shots were reordered/edited), so this run
                  renders the whole cut fresh.</span
                >
              </div>{/if}
          </div>
        {/if}
        {#if cutSource}
          <!-- svelte-ignore a11y_media_has_caption -->
          <video
            src={cutSource.url}
            controls
            loop
            class="result-video"
            style="object-fit:contain;background:#000"
          ></video>
          {#if cutSource.proxy}
            <p class="hint" style="margin-top:6px">
              Playing the <b>{previewFullRes ? 'preview' : 'proxy'}</b> cut ({proxyW}×{proxyH}){previewFullRes
                ? ' — same canvas and seed as the final, fewer steps'
                : ''}{resultUrl ? ' — the full-resolution cut is below' : ''}.
            </p>
          {/if}
        {:else}
          <div class="result-video">
            {renderIsMine ? '' : 'No render yet'}
          </div>
        {/if}
        {#if partialShots.length > 0 && !resultUrl}
          <!-- Progressive delivery: each shot appears here the instant the engine finishes it,
               while the rest of the chain renders. Replaced by the stitched result when it lands.
               `shot.index` is the engine's PROMPT-FILTERED segment index (the API drops
               empty-prompt segments — see shotStatus), so resolve it against the PROMPTED
               clips by the editor. The label uses the RAW editor number (partialShotNo)
               so it matches the stage slate and the timeline you clicked. -->
          <div class="partials-strip">
            <div class="partials-head">
              Shots so far — {partialShots.length}{expectedShots > 0 ? ` of ${expectedShots}` : ''} rendered{!showAllFinal &&
              partialShots.length > PARTIAL_CAP
                ? ` · showing the latest ${PARTIAL_CAP}`
                : ''} · hover a tile to play it
            </div>
            {@render strip(partialShots, showAllFinal, () => (showAllFinal = true))}
            {@render gaps(partialShots)}
          </div>
        {/if}
        {#if proxyResultUrl || proxyShots.length > 0}
          <!-- Preview tier: the {proxyW}×{proxyH} preview stream (with audio), kept alongside the
               final render so you can compare prompt-fidelity / reactivity before committing.
               Present once this client has previewed (or switched to the preview tier) — the other
               tier's clips are not downloaded on spec. -->
          <div class="partials-strip">
            <div class="partials-head">
              {previewFullRes
                ? `Preview · ${proxyW}×${proxyH} · same seed, fewer steps · with audio`
                : `Proxy preview · ${proxyW}×${proxyH} · with audio`}
            </div>
            {#if proxyResultUrl && !cutSource?.proxy}
              <!-- svelte-ignore a11y_media_has_caption -->
              <video
                src={proxyResultUrl}
                controls
                loop
                class="result-video"
                style="object-fit:contain;background:#000"
              ></video>
            {/if}
            {#if proxyShots.length > 0 && !proxyResultUrl}
              {@render strip(proxyShots, showAllProxy, () => (showAllProxy = true))}
            {/if}
          </div>
        {/if}
        {#if cutSource?.proxy && resultUrl}
          <!-- Only when the full cut has actually been downloaded — i.e. this client asked for
               the final tier at some point. The tier decides what is fetched, so on a proxy
               client the full-resolution file is not on the wire at all; switching the tier is
               what pulls it. Still behind a click here, since it is the heavy one. -->
          <details class="partials-strip">
            <summary class="partials-head">Full-resolution cut · {outW}×{outH}</summary>
            <!-- svelte-ignore a11y_media_has_caption -->
            <video
              src={resultUrl}
              controls
              loop
              preload="none"
              class="result-video"
              style="object-fit:contain;background:#000;margin-top:8px"
            ></video>
          </details>
        {/if}
        {#if running}
          <div class="ptrack">
            <!-- The ENGINE's own fraction wins when it reports one: it knows about the checkpoint
                 reload folded into the job, which the elapsed-vs-last-render estimate does not.
                 `?? progressPct` and not `|| progressPct` — 0% is a real answer, "did not say" is
                 null, and conflating them is the whole "0 of 0 draws as finished" failure. -->
            <div class="pfill" style="width:{telemetry?.percent ?? progressPct}%"></div>
          </div>
          <div style="display:flex;justify-content:space-between;margin:6px 0 12px">
            <span class="hint"
              >{renderElsewhere
                ? `Rendering “${renderOwnerName}”`
                : activeRenderTier === 'proxy'
                  ? 'Proxy preview'
                  : 'Rendering'} · {elapsed.toFixed(0)}s{activeExpectedSec
                ? ` / ~${activeExpectedSec.toFixed(0)}s`
                : ''}</span
            ><button class="btn sm danger" onclick={oncancel}>Cancel</button>
          </div>
          {#if telemetry?.known}
            <!-- The job-level line: which generation of how many, what the engine is doing, and its
                 own whole-job ETA. Each piece appears only if the engine reported it. -->
            <div class="h3-job" class:swap={telemetry.swapping}>
              <div class="h3-job-line">
                {#if telemetry.phaseLabel}<b>{telemetry.phaseLabel}</b>{/if}
                {#if telemetry.generationLabel}<span>{telemetry.generationLabel}</span>{/if}
                {#if telemetry.stepLabel}<span>{telemetry.stepLabel}</span>{/if}
                {#if telemetry.percent !== null}<span>{telemetry.percent.toFixed(0)}%</span>{/if}
                {#if telemetry.etaSeconds !== null}<span
                    title="The engine's own estimate for the whole job at submit time, including any checkpoint reload a mixed fl2va/ref2va chain forces"
                    >~{formatDuration(telemetry.etaSeconds)} for the job</span
                  >{/if}
              </div>
              {#if telemetry.phaseNote}
                <!-- 🔴 Minutes of frozen counter with a cause. Undrawn, this is the moment an
                     hour-old render gets cancelled for looking hung. -->
                <p class="hint" style="margin:4px 0 0">{telemetry.phaseNote}</p>
              {/if}
            </div>
          {/if}
        {/if}
        <div style="margin:12px 0">
          <TierToggle pref={tierPref} tier={renderTier} {narrow} onchange={ontier} />
        </div>
        {#if reachableNote}
          <!-- 🔴 A WARNING on the render control, never a re-cap of the authoring limits. The
               reachable ceiling is a RUNTIME property of a busy box: feeding it into the packer
               would silently re-pack a saved timeline because the GPU happened to be warm, and
               re-pack it back later. So the timeline is untouched and this says what will happen. -->
          <p class="hint err" style="margin:0 0 8px">⚠ {reachableNote}</p>
        {/if}
        <div class="inline-row" style="flex-wrap:wrap;gap:7px">
          <button class="btn sm primary" onclick={onrender} disabled={nothingToRender}
            >{renderTier === 'proxy'
              ? 'Preview all'
              : continueFromLast && !continueMisaligned
                ? 'Render new shots'
                : 'Render all'}</button
          >
          <QueueChip kind={queueKind} bind:options={queueOptions} response={queueAccepted} />
          <span
            class="btn sm"
            style="gap:4px"
            title="Cheap proxy preview (with audio) of the first N prompted shots"
            >Proxy preview · first <input
              value={previewShots}
              oninput={(e) => (previewShots = parseInt(e.currentTarget.value, 10) || 1)}
              style="width:34px;background:var(--bg-3);border:1px solid var(--ring);border-radius:5px;color:var(--ink);padding:1px 4px;font-size:11px"
            />
            shots
            <button class="miniai" onclick={onquickpreview} disabled={nothingToRender}>Go</button>
          </span>
          {#if priorRenders.length > 0}<button
              class="btn sm"
              onclick={onretakelast}
              disabled={!canRetake}
              title={canRetake
                ? `A new seed for the last shot. It ${retakeNote}`
                : (resumeNote ?? UNAVAILABLE.resume)}>Retake last shot</button
            >{/if}
          <button class="btn sm" onclick={onsummarise} disabled={summarizing}
            >{summarizing ? 'Summarising…' : 'Summarise'}</button
          >
          {#if resultUrl}<a class="btn sm" href={resultUrl} download="director.webm">Download</a
            >{/if}
        </div>
        {#if priorRenders.length > 0 && resume}
          <label
            class="field"
            style="margin-top:11px;display:flex;flex-direction:row;align-items:center;gap:8px"
            ><input
              type="checkbox"
              bind:checked={continueFromLast}
              disabled={running}
              style="accent-color:var(--accent)"
            /><span>Continue from last render — reuse rendered shots, only render new ones</span
            ></label
          >
        {:else if priorRenders.length > 0}
          <!-- Explained where the control was, rather than left as an unexplained absence: "why
               does it re-render everything" is the question this answers. -->
          <p class="hint" style="margin-top:11px">{resumeNote ?? UNAVAILABLE.resume}</p>
        {/if}
        <!--
          What the resume is actually resuming FROM, read off the engine's own disk.

          🔴 The tick-box above is drawn from the saved document's resume id, which never expires.
          The latents it names do: an entry is written when a segment completes and dropped by a
          byte-bounded, newest-first-keep sweep, so a render old enough to have been swept resumes
          into nothing — and "resuming" it re-renders the whole prefix at full cost while reporting
          itself as a continue. Nothing koblem holds can answer that; only the engine can, so this
          asks it. On demand, because it is a live round trip and no other readout needs one.
        -->
        {#if onloadbank}
          <div style="margin-top:11px">
            <div class="inline-row">
              <button class="btn sm" onclick={onloadbank} disabled={bankBusy}
                >{bankBusy
                  ? 'Reading…'
                  : bank
                    ? 'Re-read the bank'
                    : 'Check the latent bank'}</button
              >
              {#if bank}
                <span class="hint"
                  >{bank.count}
                  {bank.count === 1 ? 'segment' : 'segments'} · {gb(bank.bytes)} of {gb(
                    bank.cap_bytes
                  )}</span
                >
              {/if}
            </div>
            {#if bankError}
              <p class="hint" style="color:var(--danger);margin-top:6px">{bankError}</p>
            {:else if bank && bank.count === 0}
              <p class="hint" style="margin-top:6px">
                The bank is empty — every render here starts from the first shot, whatever the
                continue tick-box says.
              </p>
            {:else if bank}
              <p class="hint" style="margin-top:6px">
                Newest first. A render whose keys are gone has been swept out and can no longer be
                continued or retaken — it re-renders in full.
              </p>
              <div class="partials-row" style="flex-wrap:wrap">
                {#each bank.entries.slice(0, BANK_KEYS_SHOWN) as e (e.key)}
                  <span
                    class="bank-key"
                    title="{e.width}×{e.height} · {e.frames} frames · {gb(e.size_bytes)}"
                    >{e.key}</span
                  >
                {/each}
                {#if bank.entries.length > BANK_KEYS_SHOWN}
                  <span class="hint">+{bank.entries.length - BANK_KEYS_SHOWN} more</span>
                {/if}
              </div>
            {/if}
          </div>
        {/if}
        {#if summary}
          <div class="callout" style="margin-top:6px">
            {summary}
            {#if summaryStale}<span style="color:var(--warning)">Shots changed — re-summarise.</span
              >{/if}
          </div>
        {/if}
        {#if renderError}<p class="hint" style="color:var(--danger);margin-top:8px">
            {renderError}
          </p>{/if}
        {#if restoreNote}<p class="hint" style="margin-top:8px">
            {restoreNote}
          </p>{/if}
        <!-- After a run: what the engine said it would actually render, and — the load-bearing part
             — what it silently threw away. Collapsed by default; the summary carries the count so a
             run with notes still announces itself. -->
        {#if h3Plan}
          <H3PlanEcho plan={h3Plan} {vocab} />
        {/if}
      </div>
      <div>
        <div class="eye" style="margin-bottom:10px">Render queue</div>
        {#if running}
          <div class="hist-item">
            <span
              class="rs busy"
              style="position:static;flex:none;width:9px;height:9px;border-radius:50%;background:var(--accent)"
            ></span>
            <div style="flex:1">
              <b>{renderElsewhere ? renderOwnerName : `Full cut · ${promptedCount} shots`}</b><br
              /><small
                >rendering — {elapsed.toFixed(0)}s elapsed{renderElsewhere
                  ? ' (another project)'
                  : ''}</small
              >
            </div>
            <button class="btn sm ghost" onclick={oncancel}>Cancel</button>
          </div>
        {:else}
          <p class="hint" style="margin:0 0 12px">
            Nothing rendering. One render runs at a time; a shot render reuses the previous shot's
            continuation frames, so queued renders wait for what they depend on — image-to-video
            shots can start fresh.
          </p>
        {/if}
        <div class="eye" style="margin-bottom:10px">History</div>
        <GenerationHistory
          service={historyService}
          refreshKey={historyKey}
          title=""
          onRestore={onrestore}
        />
      </div>
    </div>
  </div>
</div>

<style>
  /* The job-level H3 line: which generation of how many, the phase, and the engine ETA. Boxed so
     it reads as the ENGINE talking, distinct from the editor's own elapsed/estimate line above. */
  .h3-job {
    margin: 0 0 12px;
    padding: 7px 9px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-2);
  }
  /* A checkpoint swap is the one phase that must not read as a stall. */
  .h3-job.swap {
    border-color: var(--warning);
  }
  .h3-job-line {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: baseline;
    font-family: var(--mono);
    font-size: 11px;
    color: var(--muted);
  }
  .h3-job-line b {
    color: var(--ink);
  }
  .h3-job.swap .h3-job-line b {
    color: var(--warning);
  }
  /* A bank key is a wire string an operator retypes when the engine 400s on one — monospaced and
     selectable, never truncated into something that looks like a key and is not. */
  .bank-key {
    font-family: var(--mono);
    font-size: 11px;
    color: var(--muted);
    padding: 2px 6px;
    border: 1px solid var(--ring);
    border-radius: 5px;
    background: var(--bg-2);
    user-select: all;
  }
</style>
