<script lang="ts">
  import {
    music3Generate,
    music3Suggest,
    music3Rework,
    getMusic3Job,
    getMusic3Active,
    cancelQueueJob,
    fetchMusicTrack,
    type Music3GenerateRequest,
    type Music3Job,
    type Music3ReworkScope,
    type Music3Progress,
    type Music3Props
  } from '$lib/api/client';
  import { blockedSummary } from '$lib/queue/format';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import { onDestroy, onMount } from 'svelte';
  import { str, num, bool } from '$lib/restore';

  let {
    music3Props = null,
    onRendered
  }: {
    music3Props?: Music3Props | null;
    onRendered?: () => void;
  } = $props();

  // ── the two inputs the model actually takes ──
  let caption = $state('');
  let lyrics = $state('');

  // ── the rant expander ──
  let brief = $state('');
  let captionOnly = $state(false);
  let suggesting = $state(false);
  let family = $state('');

  // ── rework: the iterate loop ──
  // "/suggest gets a first draft; Rework is how you say 'chorus needs to hit harder' without
  // starting over." Subordinate to Render on purpose — it edits inputs, it does not produce audio.
  let instruction = $state('');
  let reworkScope = $state<Music3ReworkScope>('both');
  let reworking = $state(false);
  let reworkNote = $state('');

  // ── settings ──
  // 🔴 `maxDuration` is a CEILING. The model's autoregressive stage decides the real length and
  // routinely ends a song early; the resolved value comes back per track and is what we display.
  let maxDuration = $state(120);
  let useSeed = $state(false);
  let seed = $state(0);
  let takes = $state(1);
  // 🔴 No 'wav': ComfyUI has no wav writer at all, and the engine 400s on it. flac IS the
  // lossless option and is labelled as such below, because someone wanting "uncompressed" will
  // look for wav first and needs to land somewhere real.
  let format = $state<'flac' | 'mp3' | 'opus'>('mp3');
  // 🔴 TRUE since 2026-08-16, matching the engine's own default. At the tile size the engine
  // ships (256) tiled decode is both the fastest and the LEANEST arm — a 40 s song is 71.9 s
  // and peaks at 10.2 GiB, against untiled's 96.2 s and 12.5 GiB — so the VRAM-clash reason
  // this started as `false` is gone. The panel still sends the field explicitly on every
  // request, so this default is what a caller actually gets.
  let tiledDecode = $state(true);

  let showAdvanced = $state(false);
  let steps = $state(30);
  let cfg = $state(1.7);
  let cfgScale = $state(1.7);
  let topK = $state(50);
  let sampler = $state('euler');
  let scheduler = $state('simple');

  // ── render state ──
  let rendering = $state(false);
  let error = $state('');
  let result = $state<Music3Job | null>(null);
  let trackUrls = $state<string[]>([]);
  let progress = $state<Music3Progress | null>(null);
  let jobId = $state<string | null>(null);
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  // The cap this result was rendered with, captured at submit so the finished-track line can show
  // the gap between what was asked for and what arrived. Reading `maxDuration` there would report
  // whatever the slider says NOW, which the user may already have moved.
  let renderedCap = $state<number | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);

  // 🔴 Re-attach to a render already in flight. This is the payoff of the job store: the whole
  // reason it exists is that a ~4.5-minute render used to die with the tab, so a freshly-loaded
  // panel must pick one back up rather than leave it running invisibly.
  onMount(async () => {
    try {
      const { job } = await getMusic3Active();
      if (job) adopt(job);
    } catch {
      // No active render, or the API is unreachable — either way there is nothing to re-attach to.
    }
  });

  onDestroy(() => {
    stopPoll();
    revoke();
  });

  /** Take over an in-flight job (fresh submit, or one found by `/active` after a reload). */
  function adopt(job: Music3Job) {
    jobId = job.id;
    result = null;
    error = '';
    rendering = true;
    progress = job.progress;
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    startPoll();
  }

  function revoke() {
    for (const u of trackUrls) URL.revokeObjectURL(u);
    trackUrls = [];
  }

  // ── capabilities ──
  // The engine's `/music3/v1/capabilities` is the authority on the model's limits. Reading it
  // here means the form and the engine cannot drift; the fallbacks only cover an unreachable
  // engine, where the form is disabled anyway.
  const caps = $derived(music3Props?.capabilities ?? null);

  function capNum(key: string, fallback: number): number {
    const v = caps?.[key];
    return typeof v === 'number' && Number.isFinite(v) ? v : fallback;
  }
  function capList(key: string, fallback: string[]): string[] {
    const v = caps?.[key];
    return Array.isArray(v) && v.every((x) => typeof x === 'string') && v.length
      ? (v as string[])
      : fallback;
  }

  const maxSeconds = $derived(capNum('max_duration_seconds', 360));
  const samplers = $derived(capList('samplers', ['euler', 'res_multistep', 'dpmpp_2m', 'heun']));
  const schedulers = $derived(capList('schedulers', ['simple', 'normal', 'karras', 'beta']));
  const formats = $derived(capList('formats', ['flac', 'mp3', 'opus']));

  /** Human label for a format. flac is called out as the lossless one by name. */
  function formatLabel(f: string): string {
    if (f === 'flac') return 'flac — lossless';
    if (f === 'mp3') return 'mp3 — compressed';
    if (f === 'opus') return 'opus — compressed, smallest';
    return f;
  }

  // Which phases report a step count and which do not, as published by the engine. Reading these
  // rather than hardcoding 'denoising'/'conditioning' is what keeps the "unstepped stage looks
  // like a hang" fix working when the engine adds a phase.
  const progressCaps = $derived.by(() => {
    const p = caps?.progress;
    return p && typeof p === 'object'
      ? (p as { stepped_phases?: string[]; unstepped_phases?: string[] })
      : null;
  });
  const steppedPhases = $derived(strList(progressCaps?.stepped_phases, ['denoising']));
  const unsteppedPhases = $derived(
    strList(progressCaps?.unstepped_phases, ['conditioning', 'loading', 'decoding'])
  );

  function strList(v: unknown, fallback: string[]): string[] {
    return Array.isArray(v) && v.length && v.every((x) => typeof x === 'string')
      ? (v as string[])
      : fallback;
  }

  const disabled = $derived(!music3Props?.healthy || rendering);

  /** Repopulate from a saved music3 generation (dispatched by the music page's history restore). */
  export function restore(meta: Record<string, unknown>) {
    const md = num(meta, 'max_duration');
    if (md !== undefined) maxDuration = md;
    const sd = num(meta, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    const st = num(meta, 'steps');
    if (st !== undefined) steps = st;
    const tk = num(meta, 'takes');
    if (tk !== undefined) takes = tk;
    const fm = str(meta, 'format');
    if (fm === 'flac' || fm === 'mp3' || fm === 'opus') format = fm;
    const td = bool(meta, 'tiled_decode');
    if (td !== undefined) tiledDecode = td;
    // 🔴 The caption and lyrics themselves are NOT in the log meta — only their character counts
    // are, deliberately, so a render log never carries a whole song's text. Say so rather than
    // silently restoring a form with the settings but no words.
    error = '';
  }

  // ── the rant expander ──

  async function expand() {
    if (!brief.trim() && !caption.trim()) {
      error = 'describe the song you want first';
      return;
    }
    suggesting = true;
    error = '';
    try {
      const res = await music3Suggest({
        brief: brief.trim(),
        caption_only: captionOnly,
        caption: caption.trim() || undefined,
        max_duration: maxDuration
      });
      caption = res.caption;
      if (res.lyrics) lyrics = res.lyrics;
      family = res.family;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      suggesting = false;
    }
  }

  /** Rework is live only once there is something to rework. */
  const canRework = $derived(Boolean(caption.trim() || lyrics.trim()));

  async function applyRework() {
    if (!instruction.trim()) {
      error = 'say what you want changed';
      return;
    }
    reworking = true;
    error = '';
    reworkNote = '';
    try {
      const res = await music3Rework({
        caption: caption.trim(),
        lyrics: lyrics.trim() || undefined,
        instruction: instruction.trim(),
        scope: reworkScope,
        max_duration: maxDuration
      });
      // Safe to overwrite: the server refuses rather than returning an empty half, and echoes the
      // out-of-scope half back from the request untouched.
      caption = res.caption;
      lyrics = res.lyrics ?? '';
      family = res.family;
      reworkNote = res.family_changed ? `style moved to ${res.family}` : 'applied';
      // Clear the box — the instruction has been consumed, and leaving it invites double-applying
      // the same change on the next click.
      instruction = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      reworking = false;
    }
  }

  // ── render ──

  function startPoll() {
    stopPoll();
    // 1 s against a render measured in minutes is cheap, and it keeps the phase label responsive
    // through the stage that reports no steps at all.
    pollTimer = setInterval(pollOnce, 1000);
  }

  function stopPoll() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  async function pollOnce() {
    if (!jobId) return;
    let job: Music3Job;
    try {
      job = await getMusic3Job(jobId);
    } catch (e) {
      // A dropped poll is not a dropped render — the task owns it. Keep polling; only a terminal
      // state or an explicit cancel ends this loop.
      console.warn('music3 poll failed, will retry', e);
      return;
    }
    progress = job.progress;
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    if (job.state === 'queued' || job.state === 'rendering') return;

    stopPoll();
    rendering = false;
    jobId = null;
    queuedBecause = '';
    if (job.state === 'done') {
      result = job;
      trackUrls = await Promise.all(job.tracks.map((t) => fetchMusicTrack(t.url)));
      onRendered?.();
    } else if (job.state === 'cancelled') {
      error = 'render cancelled';
    } else {
      error = job.error || 'render failed';
    }
    progress = null;
  }

  async function render() {
    if (!caption.trim()) {
      error = 'a caption is required — describe the music';
      return;
    }
    revoke();
    result = null;
    error = '';
    progress = null;
    accepted = null;
    renderedCap = maxDuration;
    rendering = true;
    try {
      const req: Music3GenerateRequest = {
        caption: caption.trim(),
        lyrics: lyrics.trim(),
        max_duration: maxDuration,
        takes,
        format,
        tiled_decode: tiledDecode
      };
      if (useSeed) req.seed = seed;
      if (showAdvanced) {
        req.steps = steps;
        req.cfg = cfg;
        req.cfg_scale = cfgScale;
        req.top_k = topK;
        req.sampler = sampler;
        req.scheduler = scheduler;
      }
      // Returns as soon as the render is ACCEPTED; the result arrives through the poll.
      const res = await music3Generate(req, queueOpts);
      accepted = res;
      jobId = res.job_id;
      progress = null;
      queuedBecause = res.blocked_reason ? blockedSummary(res.blocked_reason) : '';
      startPoll();
    } catch (e) {
      rendering = false;
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function cancel() {
    if (!jobId) return;
    try {
      // 🔴 Off the QUEUE, not the engine: the engine's own /cancel only reaches a render that is
      // already running, and one still waiting its turn is exactly the one worth dropping.
      await cancelQueueJob(jobId);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  /** Friendly wording for the phases we know by name; anything else shows its raw word. */
  function phaseName(p: string): string {
    if (p === 'conditioning') return 'Writing the song';
    if (p === 'denoising') return 'Rendering audio';
    if (p === 'decoding') return 'Decoding';
    if (p === 'loading') return 'Loading weights';
    return p;
  }

  /** 🔴 The label that stops a healthy render reading as a hang. The song-writing (autoregressive)
   *  stage emits no step count and is a large share of wall clock, so an unstepped phase says so
   *  explicitly. Membership comes from the engine's published lists, not from a word compared
   *  here — a phase the UI has never heard of is treated as unstepped, which is the safe default. */
  const phaseLabel = $derived.by(() => {
    const p = progress?.phase ?? '';
    if (!p) return 'Queued';
    const stepped = steppedPhases.includes(p);
    return stepped ? phaseName(p) : `${phaseName(p)} (no step count in this stage — be patient)`;
  });

  /** Steps, only when the engine is actually reporting them AND this phase is a stepped one.
   *  `null` means UNKNOWN and must not be drawn as 0 — a 0/0 bar reads as finished. */
  const stepLabel = $derived.by(() => {
    const p = progress?.phase ?? '';
    if (p && unsteppedPhases.includes(p)) return '';
    const cur = progress?.current_step;
    const tot = progress?.total_steps;
    return typeof cur === 'number' && typeof tot === 'number' && tot > 0 ? `${cur}/${tot}` : '';
  });

  /** Did the model stop before the cap? The 1 s slack keeps a rounding difference from reading as
   *  an early finish. */
  function endedEarly(delivered?: number | null): boolean {
    return renderedCap != null && typeof delivered === 'number' && delivered < renderedCap - 1;
  }

  function fmtSeconds(s?: number | null): string {
    if (typeof s !== 'number' || !Number.isFinite(s)) return '—';
    const m = Math.floor(s / 60);
    const r = Math.round(s % 60);
    return m > 0 ? `${m}m ${r}s` : `${r}s`;
  }
</script>

<p class="blurb muted">
  MiniMax Music 3 — a complete song (44.1 kHz stereo, up to {Math.round(maxSeconds / 60)} minutes) from
  a structured caption plus tagged lyrics. Text only: this engine takes no audio input, so there is no
  cover, repaint, continuation or timbre reference.
</p>

<div class="card">
  <!-- The rant → caption+lyrics expander. This is the intended way in: the model wants a long,
       specific description, and writing one by hand is the part people skip. -->
  <div class="expander">
    <label>
      Describe the song <span class="hint">— ramble, be specific, mention the feel</span>
      <textarea
        bind:value={brief}
        rows="3"
        placeholder="a slow, aching breakup song — female voice, close and breathy, just piano and a bit of string swell at the end; rainy-night, one-take feel…"
      ></textarea>
    </label>
    <div class="idea-row">
      <button class="btn-sec" disabled={suggesting || rendering} onclick={expand}>
        {suggesting ? 'Writing…' : 'Write caption & lyrics'}
      </button>
      <label class="toggle">
        <input type="checkbox" bind:checked={captionOnly} disabled={suggesting} /> caption only
      </label>
      {#if family}
        <span class="hint" title="the style family the router picked">style: {family}</span>
      {/if}
    </div>
  </div>

  <label>
    Caption
    <span class="hint">
      — the music description. Three sections: Global Metadata, Vocal Details, Arrangement.
    </span>
    <textarea
      bind:value={caption}
      rows="12"
      placeholder="Global Metadata&#10;…genre, tempo, emotional progression, production profile&#10;&#10;Vocal Details&#10;…&#10;&#10;Arrangement&#10;…section-by-section timeline"
    ></textarea>
  </label>

  <label>
    Lyrics
    <span class="hint">
      — leave empty for an instrumental. The <code>[Section]</code> tags are the only part the model reads
      as structure; the words themselves only carry mood.
    </span>
    <textarea
      bind:value={lyrics}
      rows="10"
      placeholder="[Intro]&#10;&#10;[Verse]&#10;…&#10;&#10;[Chorus]&#10;…"></textarea>
  </label>

  <!-- Rework: the iterate loop. Deliberately quiet — a single line, secondary button, no card of
       its own — so it never competes with Render for attention. -->
  {#if canRework}
    <div class="rework">
      <input
        type="text"
        bind:value={instruction}
        disabled={reworking || rendering}
        placeholder="change something — “chorus needs to hit harder”, “less mopey”, “lose the strings”…"
        onkeydown={(e) => {
          if (e.key === 'Enter') applyRework();
        }}
      />
      <select bind:value={reworkScope} disabled={reworking || rendering} title="what it may edit">
        <option value="both">both</option>
        <option value="caption">caption only</option>
        <option value="lyrics">lyrics only</option>
      </select>
      <button class="btn-sec" disabled={reworking || rendering} onclick={applyRework}>
        {reworking ? 'Reworking…' : 'Rework'}
      </button>
      {#if reworkNote}<span class="hint">{reworkNote}</span>{/if}
    </div>
  {/if}
</div>

<div class="card">
  <h3>Settings</h3>
  <div class="knobs">
    <label class="wide">
      <!-- 🔴 This is a CAP, and the gap is large enough that soft wording misleads: a measured
           120 s request delivered 71 s of audio — 41% short, on default settings and an ordinary
           caption. The model ends the song when it decides it is over. The result block below
           reports what actually came out, probed from the delivered file. -->
      Cap: {fmtSeconds(maxDuration)}
      <span class="hint">— an upper limit, not a target. Songs often come out much shorter.</span>
      <input type="range" min="10" max={maxSeconds} step="5" bind:value={maxDuration} />
    </label>
    <label>
      Takes
      <input type="number" min="1" max="4" bind:value={takes} />
    </label>
    <label>
      Format
      <select bind:value={format}>
        {#each formats as f (f)}
          <option value={f}>{formatLabel(f)}</option>
        {/each}
      </select>
    </label>
    <label class="toggle">
      <input type="checkbox" bind:checked={useSeed} /> Fixed seed
    </label>
    <label>
      Seed
      <input type="number" bind:value={seed} disabled={!useSeed} />
    </label>
    <div class="wide">
      <label class="toggle">
        <input type="checkbox" bind:checked={tiledDecode} /> Tiled decode
      </label>
      <span class="hint">
        On by default: ~25% faster <em>and</em> ~2 GiB leaner than plain decode. Turn it off only to render
        the untiled reference — every tile setting was graded against it.
      </span>
    </div>
  </div>

  <details class="advanced" bind:open={showAdvanced}>
    <summary>Advanced</summary>
    <div class="knobs">
      <label>
        Steps
        <input type="number" min="1" max="200" bind:value={steps} />
      </label>
      <label>
        CFG <span class="hint">(sampler)</span>
        <input type="number" step="0.1" bind:value={cfg} />
      </label>
      <label>
        <!-- A different stage from `cfg`, not another name for it. -->
        CFG scale <span class="hint">(song-writing stage)</span>
        <input type="number" step="0.1" bind:value={cfgScale} />
      </label>
      <label>
        Top-k
        <input type="number" min="1" max="16384" bind:value={topK} />
      </label>
      <label>
        Sampler
        <select bind:value={sampler}>
          {#each samplers as s (s)}<option value={s}>{s}</option>{/each}
        </select>
      </label>
      <label>
        Scheduler
        <select bind:value={scheduler}>
          {#each schedulers as s (s)}<option value={s}>{s}</option>{/each}
        </select>
      </label>
    </div>
  </details>
</div>

<div class="actions">
  <button class="btn-primary" {disabled} onclick={render}>
    {rendering ? 'Rendering…' : 'Render'}
  </button>
  <QueueChip kind="music3" bind:options={queueOpts} response={accepted} />
  {#if jobId}
    <button class="btn-sec" onclick={cancel}>Cancel</button>
  {/if}
  {#if rendering}
    <!-- 🔴 The phase label is the whole point of this block. The song-writing stage emits no step
         count, so a bare progress bar sits at zero for minutes and reads as a hang. -->
    <span class="muted">{phaseLabel}</span>
    {#if queuedBecause}<span class="muted">— {queuedBecause}</span>{/if}
    {#if stepLabel}<span class="muted">step {stepLabel}</span>{/if}
    <!-- 🔴 The engine's ETA, shown as TEXT and never as a bar denominator. It is an UPPER
         estimate and songs routinely finish well under it, so a bar keyed on it would reach
         100% and then sit there — the exact "looks stuck at the end" failure the phase label
         exists to avoid at the start. -->
    {#if typeof progress?.eta_seconds === 'number'}
      <span class="muted">~{fmtSeconds(progress.eta_seconds)} estimated</span>
    {/if}
    {#if (progress?.track_count ?? 0) > 1}
      <span class="muted">
        take {(progress?.track_index ?? 0) + 1} of {progress?.track_count}
      </span>
    {/if}
  {/if}
  {#if error}<span class="error">{error}</span>{/if}
</div>

{#if result}
  <div class="card">
    <h3>Result</h3>
    {#if result.warnings.length}
      <ul class="warn">
        {#each result.warnings as w, i (i)}<li>{w}</li>{/each}
      </ul>
    {/if}
    {#if result.ignored.length}
      <!-- Surfaced, never filtered: a setting the engine discarded is otherwise invisible. -->
      <p class="warn">Ignored by the engine: {result.ignored.join(', ')}</p>
    {/if}
    {#each result.tracks as track, i (track.filename)}
      <div class="track">
        <div class="track-head">
          <!-- 🔴 The RESOLVED length, probed from the delivered file — never the requested cap.
               The cap is shown beside it only when it actually differs, so the 41%-short case
               reads as the model's decision rather than as a bug in the length control. -->
          <span class="track-name">{fmtSeconds(track.duration_seconds)}</span>
          {#if endedEarly(track.duration_seconds)}
            <span class="muted">
              (cap was {fmtSeconds(renderedCap)} — the model ended it early)
            </span>
          {/if}
          {#if track.seed != null}<span class="muted">seed {track.seed}</span>{/if}
          {#if result.duration_ms}<span class="muted">
              {(result.duration_ms / 1000).toFixed(1)}s wall
            </span>{/if}
          <a class="btn-sec" href={trackUrls[i]} download={track.filename}>Download</a>
        </div>
        {#if trackUrls[i]}<audio controls src={trackUrls[i]}></audio>{/if}
      </div>
    {/each}
  </div>
{/if}

<style>
  .blurb {
    margin: 0 0 16px;
    font-size: 13px;
  }
  .card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 20px;
    margin-bottom: 16px;
  }
  .card h3 {
    margin: 0 0 12px;
    font-size: 15px;
  }
  input,
  select,
  textarea {
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
  textarea {
    resize: vertical;
    line-height: 1.4;
  }
  label {
    display: block;
    font-size: 12px;
    color: var(--muted);
    margin-top: 8px;
  }
  label.toggle {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    color: var(--ink);
    font-size: 13px;
  }
  label.toggle input {
    width: auto;
  }
  .hint {
    color: var(--muted);
    font-weight: normal;
    font-size: 11px;
  }
  .expander {
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    margin-bottom: 10px;
  }
  .idea-row {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-top: 8px;
    flex-wrap: wrap;
  }
  .rework {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  .rework input[type='text'] {
    flex: 1;
    min-width: 180px;
  }
  .rework .btn-sec {
    white-space: nowrap;
  }
  .knobs {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px 16px;
    align-items: end;
  }
  .wide {
    grid-column: 1 / -1;
  }
  .advanced {
    margin-top: 14px;
    border-top: 1px solid var(--ring);
    padding-top: 10px;
  }
  .advanced summary {
    cursor: pointer;
    font-size: 13px;
    color: var(--muted);
  }
  .actions {
    margin: 16px 0;
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
  }
  .warn {
    color: var(--warning);
    font-size: 12px;
    margin: 0 0 8px;
    padding-left: 16px;
  }
  .track {
    border-top: 1px solid var(--ring);
    padding: 12px 0;
  }
  .track:first-of-type {
    border-top: none;
  }
  .track-head {
    display: flex;
    gap: 12px;
    align-items: center;
    margin-bottom: 8px;
    flex-wrap: wrap;
  }
  .track-name {
    font-size: 14px;
    font-weight: 600;
  }
  .track audio {
    width: 100%;
  }
  .track-head .btn-sec {
    margin-left: auto;
    text-decoration: none;
  }
  code {
    font-size: 0.9em;
  }
</style>
