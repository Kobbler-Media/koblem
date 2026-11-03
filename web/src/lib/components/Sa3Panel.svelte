<script lang="ts">
  import {
    sa3Generate,
    sa3Edit,
    cancelQueueJob,
    getSa3Job,
    getSa3Active,
    fetchMusicTrack,
    JOB_POLL_MS,
    type MusicTrack,
    type Sa3EditMode,
    type Sa3GenerateRequest,
    type Sa3GenerateResponse,
    type Sa3Job,
    type Sa3Props
  } from '#lib/api/client.ts';
  import { blockedSummary } from '#lib/queue/format.ts';
  import type { EnqueueLike, QueueSubmitOptions } from '#lib/queue/options.ts';
  import QueueChip from '#lib/components/queue/QueueChip.svelte';
  import { onDestroy, onMount } from 'svelte';
  import { str, num } from '#lib/restore.ts';

  // SA3 engine reachability (for the disabled state) + a callback to bump the
  // shared track-history list after a successful render.
  let {
    sa3Props = null,
    onRendered
  }: {
    sa3Props?: Sa3Props | null;
    onRendered?: () => void;
  } = $props();

  // Two surfaces: text→audio (t2a) and audio editing (a2a / inpaint / continuation).
  let tab = $state<'generate' | 'edit'>('generate');

  // ── shared knobs ──
  let prompt = $state('8-bit retro arcade power-up jingle, chiptune, square wave');
  let seconds = $state(10);
  let useSeed = $state(false);
  let seed = $state(0);
  let sampler = $state<'pingpong' | 'euler'>('pingpong');
  let model = $state<'q8' | 'q6'>('q8');

  // ── edit-only ──
  let editMode = $state<Sa3EditMode>('a2a');
  let inputFiles = $state<FileList | null>(null);
  let strength = $state(0.7);
  let mask = $state('');
  let editSeconds = $state(20);

  // ── result ──
  let rendering = $state(false);
  let error = $state('');
  let result = $state<Sa3GenerateResponse | null>(null);
  let trackUrl = $state('');
  // Only `generate` queues; `/edit` still answers with the finished clip inline.
  let jobId = $state<string | null>(null);
  let queued = $state(false);
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);

  onMount(async () => {
    try {
      const { job } = await getSa3Active();
      if (job) adopt(job);
    } catch {
      // Nothing to re-attach to, or the API is unreachable.
    }
  });

  onDestroy(() => {
    stopPoll();
    revoke();
  });

  function revoke() {
    if (trackUrl) URL.revokeObjectURL(trackUrl);
    trackUrl = '';
  }

  function startPoll() {
    stopPoll();
    pollTimer = setInterval(pollOnce, JOB_POLL_MS);
  }

  function stopPoll() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  /** Take over a render already in flight — a fresh 202, or one `/active` found after a reload. */
  function adopt(job: Sa3Job) {
    revoke();
    result = null;
    error = '';
    jobId = job.id;
    rendering = true;
    queued = job.state === 'queued';
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    startPoll();
  }

  async function pollOnce() {
    if (!jobId) return;
    let job: Sa3Job;
    try {
      job = await getSa3Job(jobId);
    } catch (e) {
      // A dropped poll is not a dropped render — the queue owns it. Keep polling; only a terminal
      // state or an explicit cancel ends this loop.
      console.warn('sa3 poll failed, will retry', e);
      return;
    }
    queued = job.state === 'queued';
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    if (job.state === 'queued' || job.state === 'rendering') return;

    stopPoll();
    rendering = false;
    jobId = null;
    queued = false;
    queuedBecause = '';
    if (job.state === 'done') {
      await adoptTracks(job.tracks, { duration_ms: job.duration_ms });
    } else if (job.state === 'cancelled') {
      error = 'render cancelled';
    } else {
      error = job.error || 'render failed';
    }
  }

  async function adoptTracks(tracks: MusicTrack[], extra: Partial<Sa3GenerateResponse> = {}) {
    result = { tracks, ...extra };
    const track = tracks[0];
    if (track) trackUrl = await fetchMusicTrack(track.url);
    onRendered?.();
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

  // Repopulate this panel from a saved sa3 generation (dispatched by the music
  // page's history restore). Edit-mode source audio can't be re-attached.
  export function restore(meta: Record<string, unknown>) {
    const md = str(meta, 'mode');
    if (md === 'generate') tab = 'generate';
    else if (md === 'a2a' || md === 'inpaint' || md === 'continuation') {
      tab = 'edit';
      editMode = md;
    }
    const p = str(meta, 'prompt');
    if (p !== undefined) prompt = p;
    const se = num(meta, 'seconds');
    if (se !== undefined) seconds = se;
    const sd = num(meta, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    const sm = str(meta, 'sampler');
    if (sm === 'pingpong' || sm === 'euler') sampler = sm;
    const mo = str(meta, 'model');
    if (mo === 'q8' || mo === 'q6') model = mo;
    error = '';
  }

  const disabled = $derived(!sa3Props?.healthy || rendering);

  async function renderGenerate() {
    if (!prompt.trim()) {
      error = 'prompt is required';
      return;
    }
    const req: Sa3GenerateRequest = {
      prompt: prompt.trim(),
      seconds,
      sampler,
      model
    };
    if (useSeed) req.seed = seed;
    revoke();
    result = null;
    error = '';
    accepted = null;
    rendering = true;
    queued = true;
    queuedBecause = '';
    try {
      const res = await sa3Generate(req, queueOpts);
      accepted = res;
      jobId = res.job_id;
      queuedBecause = res.blocked_reason ? blockedSummary(res.blocked_reason) : '';
      startPoll();
    } catch (e) {
      rendering = false;
      queued = false;
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function renderEdit() {
    const input = inputFiles?.[0] ?? null;
    if (!input) {
      error = 'select an input audio file';
      return;
    }
    if (!prompt.trim()) {
      error = 'prompt is required';
      return;
    }
    if (editMode === 'inpaint' && !mask.trim()) {
      error = 'inpaint needs a mask, e.g. "2:4,8:10" (seconds to regenerate)';
      return;
    }
    const opts: Parameters<typeof sa3Edit>[3] = { sampler, model };
    if (useSeed) opts.seed = seed;
    if (editMode === 'a2a') opts.strength = strength;
    if (editMode === 'inpaint') opts.mask = mask.trim();
    if (editMode === 'continuation') opts.seconds = editSeconds;
    revoke();
    result = null;
    error = '';
    rendering = true;
    try {
      // 🔴 Still synchronous: only `generate` was cut over to the queue.
      const res = await sa3Edit(editMode, prompt.trim(), input, opts);
      await adoptTracks(res.tracks ?? [], res);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      rendering = false;
    }
  }
</script>

<p class="blurb muted">
  Stable Audio 3 — text→audio for game SFX and short music, plus editing of an existing clip. Light
  engine: renders are quick and the GPU yields to TTS.
</p>

<!-- Mode -->
<div class="card">
  <div class="row">
    <label class="toggle">
      <input type="radio" bind:group={tab} value="generate" /> Generate
    </label>
    <label class="toggle">
      <input type="radio" bind:group={tab} value="edit" /> Edit a clip
    </label>
  </div>
</div>

<div class="card">
  <label>
    Prompt
    <textarea bind:value={prompt} rows="2" placeholder="describe the sound…"></textarea>
  </label>

  {#if tab === 'generate'}
    <label>
      Length <span class="hint">{seconds}s</span>
      <input type="range" min="1" max="120" step="1" bind:value={seconds} />
    </label>
  {:else}
    <div class="knobs">
      <label>
        Mode
        <select bind:value={editMode}>
          <option value="a2a">a2a — restyle</option>
          <option value="inpaint">inpaint — regenerate regions</option>
          <option value="continuation">continuation — extend</option>
        </select>
      </label>
      <label>
        Input audio <span class="hint">(44.1 kHz wav)</span>
        <input type="file" accept="audio/*" bind:files={inputFiles} />
      </label>
    </div>
    {#if editMode === 'a2a'}
      <label>
        Strength <span class="hint">(how much to change) {strength}</span>
        <input type="range" min="0.1" max="1" step="0.05" bind:value={strength} />
      </label>
    {:else if editMode === 'inpaint'}
      <label>
        Mask <span class="hint">— seconds to regenerate, e.g. "2:4,8:10"</span>
        <input type="text" bind:value={mask} placeholder="2:4,8:10" />
      </label>
    {:else}
      <label>
        Total length <span class="hint">{editSeconds}s — must exceed the input</span>
        <input type="range" min="2" max="120" step="1" bind:value={editSeconds} />
      </label>
    {/if}
  {/if}
</div>

<div class="card">
  <h3>Settings</h3>
  <div class="knobs">
    <label>
      Sampler
      <select bind:value={sampler}>
        <option value="pingpong">pingpong (default)</option>
        <option value="euler">euler (deterministic)</option>
      </select>
    </label>
    <label>
      Quant
      <select bind:value={model}>
        <option value="q8">Q8 (best)</option>
        <option value="q6">Q6 (smaller)</option>
      </select>
    </label>
    <label class="toggle">
      <input type="checkbox" bind:checked={useSeed} /> Fixed seed
    </label>
    <label>
      Seed
      <input type="number" bind:value={seed} disabled={!useSeed} />
    </label>
  </div>
</div>

<div class="actions">
  <button class="btn-primary" {disabled} onclick={tab === 'generate' ? renderGenerate : renderEdit}>
    {rendering ? 'Rendering…' : 'Render'}
  </button>
  {#if tab === 'generate'}
    <QueueChip kind="sa3" bind:options={queueOpts} response={accepted} />
  {/if}
  {#if jobId}
    <button class="btn-sec" onclick={cancel}>Cancel</button>
    <span class="muted">
      {queued ? `Queued${queuedBecause ? ` — ${queuedBecause}` : ''}` : 'Rendering'}
    </span>
  {/if}
  {#if error}<span class="error">{error}</span>{/if}
</div>

{#if result && trackUrl}
  {@const track = result.tracks[0]}
  <div class="card">
    <h3>Result</h3>
    <div class="meta">
      {#if result.seconds_used}<span>{result.seconds_used}s</span>{/if}
      {#if result.duration_ms}<span>· {(result.duration_ms / 1000).toFixed(1)}s wall</span>{/if}
    </div>
    <div class="track">
      <div class="track-head">
        <span class="track-name">Rendered</span>
        <a class="btn-sec" href={trackUrl} download={track.filename}>Download</a>
      </div>
      <audio controls src={trackUrl}></audio>
    </div>
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
  .row {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
  }
  .knobs {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px 16px;
    align-items: end;
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
  .meta {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
    margin-bottom: 8px;
    flex-wrap: wrap;
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
</style>
