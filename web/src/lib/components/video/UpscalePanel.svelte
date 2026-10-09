<script lang="ts">
  /*
   * Video → Upscale. LTX-2.5's own spatial upsampler: an mp4 in, a bigger mp4 out.
   *
   * 🔴 THIS PANEL'S JOB IS TO SHOW THE SEAM COUNT BEFORE THE BUTTON IS PRESSED. ComfyUI holds video
   * as fp32, so a window past the engine's host-RAM guard has to be split into segments — and each
   * segment is its own refine, so every join is a place the model can choose different texture.
   * `POST /api/v1/upscale/plan` computes that split without touching the GPU, which is why the plan
   * is re-fetched on every settings change and why the primary action reads "Upscale (1 pass)" or
   * "Upscale (6 segments, 5 seams)" rather than just "Upscale".
   *
   * ⚠️ NOTHING HAS EVER BEEN RENDERED THROUGH THIS ENGINE. It publishes `eta_seconds: null` on
   * purpose and koblem does not invent one, so there is no countdown here — only a phase and a
   * segment counter. Do not add a progress bar keyed on `total_steps`: it is null on the `noise: 0`
   * arm, and where there is a refine it counts three sigmas that are a small share of the wall
   * clock — the tiled VAE decode dominates and reports nothing.
   */
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import {
    uploadUpscaleSource,
    planUpscale,
    upscaleGenerate,
    getUpscaleJob,
    getUpscaleActive,
    cancelUpscale,
    upscaleOutputUrl,
    upscaleOutputPath,
    upscaleSegmentUrl,
    prefetchMediaSignatures
  } from '$lib/api/client';
  import { blockedSummary } from '$lib/queue/format';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import { watchLiveJobs, type LiveWatch } from '$lib/queue/live';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import type {
    UpscaleGenerateRequest,
    UpscaleJob,
    UpscaleProbedGeometry,
    UpscaleScaleMode,
    UpscaleSegmentPlan,
    UpscaleSource
  } from '$lib/types';
  import { onDestroy, onMount } from 'svelte';

  /** Deep-link from Director: `?tab=upscale&project=<id>&tier=<proxy|final>&shot=<n>&variant=<n>`
   *  hands a take straight here. 🔴 Coordinates, never a filename — see `UpscaleSource`. */
  let {
    project = '',
    tier = '',
    shot = '',
    variant = ''
  }: { project?: string; tier?: string; shot?: string; variant?: string } = $props();

  const POLL_MS = 1000;
  /** How often we re-ask the server what is running WHEN THE QUEUE STREAM IS DOWN, and only then —
   *  the stream carries the transition this used to poll for. See `watchLiveJobs`. */
  const ACTIVE_POLL_MS = 12000;

  let source = $state<UpscaleSource | null>(null);
  let sourceLabel = $state('');
  let geometry = $state<UpscaleProbedGeometry | null>(null);
  let uploading = $state(false);

  let scaleMode = $state<UpscaleScaleMode>('multiplier');
  let multiplier = $state(2);
  let shorterSize = $state(1080);
  let longerSize = $state(1920);
  let width = $state(1920);
  let height = $state(1088);
  //: 🔴 THE ONE KNOB THAT MATTERS. Defaults to the measured 0.4; 0 is the upsample-only arm.
  let noise = $state(0.4);
  let detailer = $state(false);
  let startTime = $state(0);
  let duration = $state(0);

  let plan = $state<UpscaleSegmentPlan | null>(null);
  let planError = $state('');
  let planning = $state(false);

  let job = $state<UpscaleJob | null>(null);
  let error = $state('');
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  let outputUrl = $state('');
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);
  let timer: ReturnType<typeof setInterval> | null = null;
  let liveWatch: LiveWatch | null = null;

  const running = $derived(
    !!job && (job.state === 'queued' || job.state === 'rendering' || job.state === 'joining')
  );

  function buildRequest(): UpscaleGenerateRequest | null {
    if (!source) return null;
    const req: UpscaleGenerateRequest = { source, scale_mode: scaleMode };
    if (scaleMode === 'multiplier') req.multiplier = multiplier;
    if (scaleMode === 'shorter_side') req.shorter_size = shorterSize;
    if (scaleMode === 'longer_side') req.longer_size = longerSize;
    if (scaleMode === 'dimensions') {
      req.width = width;
      req.height = height;
    }
    // Only send what differs from the engine's defaults — echoing our own copy of them is how
    // they drift. `noise` always goes, because it is the point of the panel.
    req.noise = noise;
    if (detailer) req.detailer = true;
    if (startTime > 0) req.start_time = startTime;
    if (duration > 0) req.duration = duration;
    return req;
  }

  async function refreshPlan() {
    const req = buildRequest();
    if (!req) {
      plan = null;
      planError = '';
      return;
    }
    planning = true;
    planError = '';
    try {
      const res = await planUpscale(req);
      plan = res.plan;
      geometry = res.source;
    } catch (e) {
      plan = null;
      // The server's refusals here are arithmetic and carry the numbers — show them verbatim
      // rather than "invalid settings", because they say what to change.
      planError = e instanceof Error ? e.message : String(e);
    } finally {
      planning = false;
    }
  }

  async function onFile(file: File) {
    uploading = true;
    error = '';
    try {
      const res = await uploadUpscaleSource(file);
      source = { kind: 'upload', token: res.token };
      sourceLabel = res.filename;
      geometry = res.geometry;
      await refreshPlan();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      uploading = false;
    }
  }

  async function start() {
    const req = buildRequest();
    if (!req) return;
    error = '';
    accepted = null;
    try {
      const res = await upscaleGenerate(req, queueOpts);
      accepted = res;
      queuedBecause = res.blocked_reason ? blockedSummary(res.blocked_reason) : '';
      job = await getUpscaleJob(res.job_id);
      startPolling();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function startPolling() {
    stopPolling();
    timer = setInterval(async () => {
      if (!job) return;
      try {
        job = await getUpscaleJob(job.id);
      } catch {
        // A dropped poll is not a dropped run — the queue owns it. Keep polling; only a terminal
        // state ends this loop.
        return;
      }
      queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
      if (!running) {
        stopPolling();
        queuedBecause = '';
        if (job.state === 'done') await adoptOutput(job.id);
      }
    }, POLL_MS);
  }

  function stopPolling() {
    if (timer) clearInterval(timer);
    timer = null;
  }

  /** 🔴 Sign the media path BEFORE putting it in a `<video src>`. Without this `mediaUrl` falls
   *  back to `?token=`, which puts a real API key in browser history and access logs. */
  async function adoptOutput(id: string) {
    try {
      await prefetchMediaSignatures([upscaleOutputPath(id)]);
    } catch {
      // Signing is an optimisation for the URL's shape, never a gate on showing the result.
    }
    outputUrl = upscaleOutputUrl(id);
  }

  async function stop() {
    if (!job) return;
    try {
      await cancelUpscale(job.id);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(async () => {
    // A Director deep-link arrives already pointing at a take.
    if (project && tier && shot !== '' && variant !== '') {
      const shotIndex = Number(shot);
      const variantIndex = Number(variant);
      if (
        (tier === 'proxy' || tier === 'final') &&
        Number.isFinite(shotIndex) &&
        Number.isFinite(variantIndex)
      ) {
        source = {
          kind: 'director_take',
          project_id: project,
          tier,
          shot: shotIndex,
          variant: variantIndex
        };
        sourceLabel = `${project} · ${tier} shot ${shotIndex} take ${variantIndex + 1}`;
        await refreshPlan();
      }
    }
    // Re-adopt whatever is running, so a reload (or another device) finds it.
    const adopt = async () => {
      if (running) return;
      try {
        const { active } = await getUpscaleActive();
        if (active) {
          job = active;
          queuedBecause = active.blocked_reason ? blockedSummary(active.blocked_reason) : '';
          startPolling();
        }
      } catch {
        /* idle poll; a failure here is not worth surfacing */
      }
    };
    await adopt();
    liveWatch = watchLiveJobs({ kinds: ['upscale'], downMs: ACTIVE_POLL_MS, check: adopt });
  });

  onDestroy(() => {
    stopPolling();
    liveWatch?.stop();
    liveWatch = null;
  });

  function fmtBytes(n: number | null | undefined): string {
    if (!n) return '—';
    const gb = n / 1024 ** 3;
    return gb >= 1 ? `${gb.toFixed(2)} GB` : `${(n / 1024 ** 2).toFixed(0)} MB`;
  }

  function onDropped(list: FileList) {
    const f = list.length ? list[0] : null;
    if (f) void onFile(f);
  }
</script>

<div class="video-panel">
  <p class="muted panel-status">
    LTX-2.5 upsampler · an mp4 in, a bigger mp4 out · frame rate and audio carried through from the
    source
  </p>

  {#if error}
    <div class="error">{error}</div>
  {/if}

  <section class="card">
    <h3>Source</h3>
    <p class="hint">
      LTX-2.5's own upsampler restores and enlarges a clip. There is <strong>no prompt</strong> — it repairs
      the frames it is given and cannot be told what to put there.
    </p>
    <div class="dz">
      <FileDropzone
        accept="video/mp4,video/quicktime,video/webm,video/x-matroska"
        disabled={uploading || running}
        label="Source clip"
        hint="mp4, mov, webm or mkv"
        onFiles={onDropped}
      />
    </div>
    {#if sourceLabel}
      <p class="chosen">{sourceLabel}</p>
    {/if}
    {#if uploading}
      <!-- No percentage: `fetch` with a FormData body gives no upload progress events, and a fake
           bar is worse than none on a file that may take minutes to move. -->
      <p class="hint">Uploading…</p>
    {/if}
    {#if geometry}
      <p class="geometry">
        {geometry.width}×{geometry.height} · {geometry.fps.toFixed(2)} fps ·
        {geometry.duration_seconds.toFixed(1)} s ·
        {geometry.has_audio ? 'has audio' : 'no audio track'}
      </p>
    {/if}

    <details class="advanced">
      <summary>Trim the source</summary>
      <p class="hint">Upscale a window of the clip instead of all of it.</p>
      <div class="knobs advanced-knobs">
        <label
          >Start (s)
          <input
            type="number"
            min="0"
            step="0.1"
            bind:value={startTime}
            disabled={running}
            onchange={refreshPlan}
          /></label
        >
        <label
          >Length (s, 0 = to the end)
          <input
            type="number"
            min="0"
            step="0.1"
            bind:value={duration}
            disabled={running}
            onchange={refreshPlan}
          /></label
        >
      </div>
    </details>
  </section>

  <section class="card">
    <h3>Target size</h3>
    <p class="hint">
      The upsampler is a fixed 2×, so the target is reached by resizing the source to half of it —
      which is why deliverable sizes land on multiples of 64. This decides both the picture and how
      much has to be split.
    </p>
    <div class="knobs">
      <label class="wide"
        >Mode
        <select bind:value={scaleMode} disabled={running} onchange={refreshPlan}>
          <option value="multiplier">Multiply</option>
          <option value="shorter_side">Shorter side</option>
          <option value="longer_side">Longer side</option>
          <option value="dimensions">Exact size</option>
        </select></label
      >
      {#if scaleMode === 'multiplier'}
        <label
          >Multiplier
          <input
            type="number"
            step="0.5"
            min="1"
            max="4"
            bind:value={multiplier}
            disabled={running}
            onchange={refreshPlan}
          /></label
        >
      {:else if scaleMode === 'shorter_side'}
        <label
          >Shorter side (px)
          <input
            type="number"
            step="16"
            bind:value={shorterSize}
            disabled={running}
            onchange={refreshPlan}
          /></label
        >
      {:else if scaleMode === 'longer_side'}
        <label
          >Longer side (px)
          <input
            type="number"
            step="16"
            bind:value={longerSize}
            disabled={running}
            onchange={refreshPlan}
          /></label
        >
      {:else}
        <label
          >Width
          <input
            type="number"
            step="16"
            bind:value={width}
            disabled={running}
            onchange={refreshPlan}
          /></label
        >
        <label
          >Height
          <input
            type="number"
            step="16"
            bind:value={height}
            disabled={running}
            onchange={refreshPlan}
          /></label
        >
      {/if}
    </div>

    <!-- ── the plan: the seam count, before anything renders ──────────── -->
    {#if planError}
      <!-- The server's refusals here are arithmetic and carry the numbers — shown verbatim rather
           than as "invalid settings", because they say what to change. -->
      <p class="plan-error">{planError}</p>
    {:else if plan}
      <div class="plan" class:seamy={plan.seams > 0}>
        <div class="plan-size">{plan.output_width}×{plan.output_height}</div>
        {#if plan.seams === 0}
          <p class="hint">
            <strong>One pass, no seams.</strong> The whole clip fits in a single render.
          </p>
        {:else}
          <p class="hint">
            <strong
              >{plan.segments} segments, {plan.seams} seam{plan.seams === 1 ? '' : 's'}.</strong
            >
            Only {plan.frames_per_prompt} frames fit in one pass at this size, so the clip is split into
            {plan.segment_seconds?.toFixed(1)} s windows and joined afterwards.
          </p>
          <p class="warn">
            🔴 A seam is real. The model carries no memory between segments, so texture it has to
            invent — skin, foliage, fabric, grain — can land differently on either side of a join.
            Lower the target size to render this in one pass.
          </p>
        {/if}
      </div>
    {/if}
  </section>

  <!-- ── the two knobs ──────────────────────────────────────────────── -->
  <section class="card">
    <h3>Refine</h3>
    <label class="noise"
      >Noise
      <input
        type="range"
        min="0"
        max="1"
        step="0.05"
        bind:value={noise}
        disabled={running}
        onchange={refreshPlan}
      />
      <b>{noise.toFixed(2)}</b>
    </label>
    <p class="hint">
      How much of the picture the model may redraw.
      {#if noise === 0}
        <b>0 — upsample only.</b> No transformer at all, so it cannot invent anything. Fastest, and softer:
        it interpolates rather than adding detail.
      {:else if noise <= 0.3}
        Gentle. Adds detail while staying close to the source.
      {:else if noise <= 0.5}
        The default range. Adds detail; it is choosing that detail rather than recovering it, so two
        different noise values disagree about what it is.
      {:else if noise < 0.85}
        High. Redrawing a large share of the picture — detail it chooses rather than recovers.
      {:else if noise < 1}
        🔴 <b>Past a restore.</b> 0.85 is where the official two-stage workflow's <em>second</em>
        stage starts, and its first stage is a generation with nothing to preserve — footage is not. Measured
        at 0.85 this grew a subject a pair of glasses.
      {:else}
        🔴 <b>Pure noise.</b> The input is gone entirely and the model generates from nothing, unprompted.
        Legal, and not an upscale.
      {/if}
    </p>
    <div class="toggles">
      <label class="toggle">
        <input type="checkbox" bind:checked={detailer} disabled={running || noise === 0} />
        Detailer LoRA
      </label>
    </div>
    <p class="hint">
      {#if noise === 0}
        Needs a refine — at noise 0 there is no transformer to attach it to.
      {:else}
        Costs nothing measurable (48.0 s against 48.1 s) and adds high-frequency detail.
      {/if}
    </p>
  </section>

  <!-- ── run ────────────────────────────────────────────────────────── -->
  <div class="actions">
    {#if running}
      <button class="btn-sec" onclick={stop}>Cancel</button>
    {:else}
      <button class="btn-primary" onclick={start} disabled={!source || !plan || planning}>
        {#if !plan}
          Upscale
        {:else if plan.seams === 0}
          Upscale (1 pass)
        {:else}
          Upscale ({plan.segments} segments, {plan.seams} seams)
        {/if}
      </button>
    {/if}
    <QueueChip kind="upscale" bind:options={queueOpts} response={accepted} />
    <span class="muted">Upscale runs server-side; safe to navigate away or reload.</span>
  </div>

  <!-- ── progress ───────────────────────────────────────────────────── -->
  {#if job}
    <div class="card progress">
      <!-- Indeterminate always, never a fill: this engine publishes `eta_seconds: null` and koblem
           does not invent one. `total_steps` is null on the `noise: 0` arm, and where there IS a
           refine it counts three sigmas that are a small share of the wall clock — the tiled VAE
           decode dominates and reports nothing. -->
      {#if running}
        <div class="bar indeterminate"></div>
      {/if}
      <span class="ptext">
        <strong>{job.state}</strong>
        <!-- Counted off the row's own unit counters: `progress.segment` is only set while the row
             is running, and this line has to read correctly while it is still queued. -->
        {#if job.segment_count > 1}
          · segment {Math.min(job.segments_done + 1, job.segment_count)} of {job.segment_count}
        {/if}
        {#if job.progress.phase}
          · {job.progress.phase}
        {/if}
        {#if job.progress.queue_position}
          · queued behind {job.progress.queue_position}
        {/if}
        {#if queuedBecause}
          · {queuedBecause}
        {/if}
      </span>
      {#if job.state === 'joining'}
        <p class="hint">The card is free — koblem is joining the segments.</p>
      {/if}
      {#if job.warnings.length}
        <ul class="warnings">
          {#each job.warnings as w, i (i)}
            <li>{w}</li>
          {/each}
        </ul>
      {/if}
      {#if job.error}
        <div class="error">{job.error}</div>
      {/if}
    </div>
  {/if}

  <!-- ── result ─────────────────────────────────────────────────────── -->
  {#if job?.state === 'done' && outputUrl}
    <section class="card result">
      <h3>Result</h3>
      <!-- svelte-ignore a11y_media_has_caption -->
      <video src={outputUrl} controls playsinline></video>
      <p class="meta">
        {job.segments[0]?.width ?? '?'}×{job.segments[0]?.height ?? '?'} ·
        {fmtBytes(job.output_bytes)}
        {#if job.duration_ms}
          · {(job.duration_ms / 1000).toFixed(0)} s
        {/if}
      </p>
      <a class="btn-sec" href={outputUrl} download>Download</a>
      {#if job.segments.length > 1}
        <details class="advanced">
          <summary>{job.segments.length} segments, before the join</summary>
          <p class="hint">
            When a seam looks wrong the question is always which side of it — these are the
            individual renders.
          </p>
          <ul class="segment-list">
            {#each job.segments as seg (seg.index)}
              <li>
                <a href={upscaleSegmentUrl(job.id, seg.index)} target="_blank" rel="noreferrer">
                  segment {seg.index + 1}
                  {#if seg.start_time !== null}({seg.start_time.toFixed(1)} s){/if}
                </a>
              </li>
            {/each}
          </ul>
        </details>
      {/if}
    </section>
  {/if}
</div>

<style>
  .panel-status {
    margin: 0 0 16px;
  }
  .card {
    padding: 20px;
    margin-bottom: 16px;
  }
  .card h3 {
    margin: 0 0 12px;
    font-size: 15px;
  }
  input,
  select {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font-family: inherit;
    font-size: 13px;
  }
  label {
    display: block;
    font-size: 12px;
    color: var(--muted);
    margin-top: 10px;
  }
  .hint {
    color: var(--muted);
    font-weight: normal;
    font-size: 11px;
    margin: 0;
    line-height: 1.45;
  }
  .dz {
    margin-top: 6px;
  }
  .chosen {
    margin: 8px 0 0;
    font-size: 13px;
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  .geometry {
    margin: 6px 0 0;
    font-size: 12px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  /* Settings */
  .knobs {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 8px 16px;
    align-items: end;
    margin-top: 12px;
  }
  .knobs label {
    margin-top: 0;
  }
  .knobs label.wide {
    grid-column: 1 / -1;
  }
  .toggles {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
    margin: 14px 0;
  }
  label.toggle {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    margin-top: 0;
    color: var(--ink);
    font-size: 13px;
  }
  label.toggle input {
    width: auto;
  }
  label.noise {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0 0 8px;
    color: var(--ink);
    font-size: 13px;
    flex-wrap: wrap;
  }
  label.noise input[type='range'] {
    flex: 0 1 200px;
    padding: 0;
    background: none;
    border: none;
  }
  .advanced {
    margin-top: 14px;
    padding-top: 12px;
    border-top: 1px solid var(--ring);
  }
  .advanced summary {
    cursor: pointer;
    color: var(--ink);
    font-size: 13px;
    font-weight: 600;
  }
  .advanced > .hint {
    margin-top: 10px;
  }
  .advanced-knobs {
    margin-top: 12px;
  }

  /* The plan — the seam count, before anything renders */
  .plan {
    margin-top: 14px;
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .plan.seamy {
    border-color: var(--warning, #d9952d);
  }
  .plan-size {
    font-size: 13px;
    font-weight: 600;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .warn {
    margin: 0;
    color: var(--warning, #d9952d);
    font-size: 11px;
    line-height: 1.45;
  }
  .plan-error {
    margin: 14px 0 0;
    padding: 8px 12px;
    border: 1px solid var(--danger);
    border-radius: 8px;
    color: var(--danger);
    font-size: 12px;
    white-space: pre-wrap;
  }

  .actions {
    margin: 16px 0;
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }

  /* Progress */
  .progress {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .bar {
    height: 6px;
    border-radius: 3px;
    background: var(--bg-2);
    overflow: hidden;
  }
  .bar.indeterminate {
    background: linear-gradient(90deg, var(--bg-2) 0%, var(--accent) 50%, var(--bg-2) 100%);
    background-size: 200% 100%;
    animation: slide 1.2s linear infinite;
  }
  @keyframes slide {
    from {
      background-position: 200% 0;
    }
    to {
      background-position: -200% 0;
    }
  }
  .ptext {
    color: var(--muted);
    font-size: 12px;
  }
  .warnings {
    margin: 0;
    padding-left: 18px;
    font-size: 12px;
    color: var(--warning, #d9952d);
  }

  /* Result */
  .result video {
    width: 100%;
    max-height: 60vh;
    border-radius: 8px;
    background: #000;
    margin-top: 4px;
  }
  .meta {
    color: var(--muted);
    font-size: 12px;
    margin: 8px 0;
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .result a {
    text-decoration: none;
  }
  .segment-list {
    margin: 8px 0 0;
    padding-left: 18px;
    font-size: 12px;
  }
  .segment-list a {
    color: var(--accent);
  }
</style>
