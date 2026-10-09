<script lang="ts">
  import {
    submitWanVace,
    getWanJob,
    listWanJobs,
    getWanVaceProps,
    fetchWanClip,
    cancelWanRender,
    cancelQueueJob,
    wanVaceScriptFromLlm,
    wanPromptFromLlm,
    JOB_POLL_MS,
    type WanVaceGenerateRequest,
    type WanJobStatus,
    type WanVaceProps,
    type WanSegment,
    type WanVaceMode,
    type WanImageSource,
    type LtxPromptImage
  } from '$lib/api/client';
  import { blockedSummary } from '$lib/queue/format';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import { watchLiveJobs, type LiveWatch } from '$lib/queue/live';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import { assetRefFor } from '$lib/ltx/assetRef';
  import { planActiveJob } from '$lib/ltx/activeJob';
  import { adoptedElapsedSec } from '$lib/ltx/elapsed';
  import type { Generation } from '$lib/types';
  import { str, num, arr, reattachNote } from '$lib/restore';
  import { onMount, onDestroy } from 'svelte';

  // This device's own render, so a reload re-attaches without waiting on the job list. A fast path
  // only: the server's list is what decides (`planActiveJob`), which is what makes a render started
  // on ANOTHER device show up here at all.
  const ACTIVE_JOB_KEY = 'wan-active-job';
  // How often we re-ask the server what is rendering WHEN THE QUEUE STREAM IS DOWN, and only then.
  // The stream carries the transition ("a render of this kind started") that this used to poll for;
  // see `watchLiveJobs`.
  const ACTIVE_POLL_MS = 12000;

  let historyKey = $state(0);
  let restoreNote = $state('');

  // ── Mode + script ──────────────────────────────────────────────────────
  let mode = $state<WanVaceMode>('t2v');
  let segments = $state<WanSegment[]>([{ prompt: '', seconds: 3 }]);

  function addSegment() {
    segments = [...segments, { prompt: '', seconds: 3 }];
  }
  function removeSegment(i: number) {
    segments = segments.filter((_, idx) => idx !== i);
    // Keep the per-shot parallel arrays aligned with segments, or a removed shot's
    // improve note / spinner would stick to the shot that shifts into its slot.
    segInstruction = segInstruction.filter((_, idx) => idx !== i);
    segImproving = segImproving.filter((_, idx) => idx !== i);
    segDrafting = segDrafting.filter((_, idx) => idx !== i);
  }

  // LLM script helper: expand a brief into N continuation beats. The server's system prompt
  // keeps the SAME subject across every shot (VACE identity-continuity), so the render doesn't
  // drift to a new person/scene when segments diverge.
  let brief = $state('');
  let draftCount = $state(4);
  let drafting = $state(false);

  async function draftScript() {
    // Anchor the shots to whatever subject image is available: the i2v init frame, else the
    // identity-anchor reference. The VL model then keeps that exact subject across every beat.
    const anchor = initFile ?? refFile;
    if (!brief.trim() && !anchor) return;
    drafting = true;
    error = '';
    try {
      const image = anchor ? await assetRefFor(anchor) : undefined;
      const res = await wanVaceScriptFromLlm(brief, draftCount, segments[0]?.seconds ?? 3, image);
      if (res.segments?.length) {
        segments = res.segments.map((p) => ({ prompt: p, seconds: segments[0]?.seconds ?? 3 }));
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      drafting = false;
    }
  }

  // Per-shot "Draft with LLM": write a detailed Wan prompt for one shot/beat, anchored to the
  // subject image (i2v init frame, else the identity-anchor reference) + neighbour prompts. Wan is
  // video-only, so no audio is described; continuation adds the same-subject clause server-side.
  let segDrafting = $state<boolean[]>([]);
  async function draftSegment(i: number) {
    segDrafting[i] = true;
    error = '';
    try {
      const images: LtxPromptImage[] = [];
      const anchor = (mode === 'i2v' ? initFile : null) ?? refFile;
      if (anchor)
        images.push({
          image_b64: await assetRefFor(anchor),
          role: mode === 'i2v' ? 'opening' : 'scene'
        });
      const res = await wanPromptFromLlm({
        idea: segments[i].prompt.trim(),
        mode,
        seconds: segments[i].seconds,
        images,
        prev_prompt: segments[i - 1]?.prompt?.trim() || undefined,
        next_prompt: segments[i + 1]?.prompt?.trim() || undefined
      });
      if (res.prompt) segments[i].prompt = res.prompt;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      segDrafting[i] = false;
    }
  }

  // Per-shot "Improve": revise the existing prompt in place under the same Wan
  // rules, applying the row's note (blank = reword for a fresh take on the same
  // scene). Same subject image + neighbour context as a fresh draft.
  let segInstruction = $state<string[]>([]);
  let segImproving = $state<boolean[]>([]);
  async function improveSegment(i: number) {
    if (!segments[i].prompt.trim()) return;
    segImproving[i] = true;
    error = '';
    try {
      const images: LtxPromptImage[] = [];
      const anchor = (mode === 'i2v' ? initFile : null) ?? refFile;
      if (anchor)
        images.push({
          image_b64: await assetRefFor(anchor),
          role: mode === 'i2v' ? 'opening' : 'scene'
        });
      const res = await wanPromptFromLlm({
        rewrite: segments[i].prompt.trim(),
        instruction: segInstruction[i]?.trim() || undefined,
        mode,
        seconds: segments[i].seconds,
        images,
        prev_prompt: segments[i - 1]?.prompt?.trim() || undefined,
        next_prompt: segments[i + 1]?.prompt?.trim() || undefined
      });
      if (res.prompt) segments[i].prompt = res.prompt;
      segInstruction[i] = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      segImproving[i] = false;
    }
  }

  // ── Images ──────────────────────────────────────────────────────────────
  // Init frame (i2v) + identity-anchor reference.
  let initFiles = $state<FileList | null>(null);
  let initFile = $derived(initFiles && initFiles.length ? initFiles[0] : null);
  let refFiles = $state<FileList | null>(null);
  let refFile = $derived(refFiles && refFiles.length ? refFiles[0] : null);

  // ── Params ────────────────────────────────────────────────────────────
  let width = $state(1280);
  let height = $state(720);
  let fps = $state(24);
  let frames = $state(65);
  let quant = $state('nvfp4');
  let useSeed = $state(false);
  let seed = $state(42);
  let negativePrompt = $state('');

  // ── Result / status ─────────────────────────────────────────────────────
  let props = $state<WanVaceProps | null>(null);
  let running = $state(false);
  let error = $state('');
  let job = $state<WanJobStatus | null>(null);
  let resultUrl = $state<string | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);
  // A failed/cancelled render leaves banked segments on the server; offer to resume.
  const resumable = $derived(job?.resumable === true && !!job?.resume_job_id);

  // ── Render progress ───────────────────────────────────────────────────────
  let elapsed = $state(0);
  let progressTimer: ReturnType<typeof setInterval> | null = null;
  let liveWatch: LiveWatch | null = null;
  // Set once the panel is torn down. Not reactive: its only job is to stop already-running async
  // work from re-arming timers or minting object URLs after teardown.
  let destroyed = false;
  const expectedSec = $derived(props?.last_render_sec ?? null);
  function stopTimer() {
    if (progressTimer) {
      clearInterval(progressTimer);
      progressTimer = null;
    }
  }
  function stopPoll() {
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
  }
  // `baseSec` is how long the render has ALREADY been going — non-zero only when adopting one
  // started elsewhere, where counting from the attach would under-report for the rest of the run.
  function startProgress(baseSec = 0) {
    stopTimer();
    if (destroyed) return;
    elapsed = baseSec;
    const startedAt = Date.now();
    progressTimer = setInterval(() => {
      elapsed = baseSec + (Date.now() - startedAt) / 1000;
    }, 250);
  }

  function initSource(): WanImageSource {
    return mode === 'i2v' && initFile ? { kind: 'upload' } : { kind: 'none' };
  }
  function refSource(): WanImageSource {
    return refFile ? { kind: 'upload' } : { kind: 'none' };
  }

  onMount(async () => {
    try {
      props = await getWanVaceProps();
    } catch {
      // best-effort; the page still works with the defaults above.
    }
    if (destroyed) return;
    await adoptActive();
    if (destroyed) return;
    liveWatch = watchLiveJobs({
      kinds: ['wan_vace'],
      downMs: ACTIVE_POLL_MS,
      check: adoptActive
    });
  });

  onDestroy(() => {
    destroyed = true;
    stopTimer();
    stopPoll();
    liveWatch?.stop();
    liveWatch = null;
    if (resultUrl) URL.revokeObjectURL(resultUrl);
  });

  // Attach to whatever the engine is rendering, wherever it was started. Never takes over from a
  // render already streaming into this panel — that is either the same job or one this tab is
  // driving, and two poll loops would fight over the shared timer.
  async function adoptActive() {
    if (destroyed || running) return;
    let jobs: WanJobStatus[] | null = null;
    try {
      jobs = (await listWanJobs()).jobs;
    } catch {
      // Server unreachable: planActiveJob falls back to the remembered id.
    }
    // Re-checked after the await: onDestroy cannot cancel a request already in flight, and
    // adopting here would start timers nothing is left to clear.
    if (destroyed || running) return;
    const plan = planActiveJob(localStorage.getItem(ACTIVE_JOB_KEY), jobs);
    if (plan.forget) localStorage.removeItem(ACTIVE_JOB_KEY);
    if (!plan.adopt) return;
    localStorage.setItem(ACTIVE_JOB_KEY, plan.adopt);
    if (plan.running) {
      running = true;
      startProgress(adoptedElapsedSec(plan.createdAt, Date.now()));
    }
    watchJob(plan.adopt);
  }

  function watchJob(jobId: string) {
    stopPoll();
    if (destroyed) return;
    const tick = async () => {
      let s: WanJobStatus;
      try {
        s = await getWanJob(jobId);
      } catch {
        // 🔴 A dropped poll is not a dropped render — the queue owns the row and a queued job can
        // sit for hours. Keep polling; only a terminal status stops this loop.
        return;
      }
      if (destroyed) return;
      job = s;
      if (s.status === 'completed') {
        stopPoll();
        stopTimer();
        running = false;
        localStorage.removeItem(ACTIVE_JOB_KEY);
        if (s.result_url) {
          try {
            const url = await fetchWanClip(s.result_url);
            if (destroyed) {
              URL.revokeObjectURL(url);
              return;
            }
            if (resultUrl) URL.revokeObjectURL(resultUrl);
            resultUrl = url;
          } catch (e) {
            if (destroyed) return;
            error = e instanceof Error ? e.message : String(e);
          }
        }
        historyKey += 1;
      } else if (s.status === 'failed' || s.status === 'cancelled') {
        stopPoll();
        stopTimer();
        running = false;
        localStorage.removeItem(ACTIVE_JOB_KEY);
        error = s.error || `render ${s.status}`;
      }
    };
    void tick();
    pollTimer = setInterval(() => void tick(), JOB_POLL_MS);
  }

  function buildRequest(resumeJobId?: string): WanVaceGenerateRequest | null {
    // t2v/i2v use one prompt; continuation uses every non-empty segment.
    const valid = segments.filter((s) => s.prompt.trim().length > 0);
    if (valid.length === 0) {
      error = 'Add at least one prompt.';
      return null;
    }
    const useSegments = mode === 'continuation' ? valid : [valid[0]];
    return {
      mode,
      segments: useSegments.map((s) => ({ prompt: s.prompt, seconds: s.seconds })),
      init_source: initSource(),
      reference_source: refSource(),
      negative_prompt: negativePrompt.trim() || undefined,
      width,
      height,
      fps,
      frames,
      quant,
      seed: useSeed ? seed : -1,
      save_clip: true,
      ...(resumeJobId ? { resume_job_id: resumeJobId } : {})
    };
  }

  async function startRender(req: WanVaceGenerateRequest) {
    running = true;
    error = '';
    job = null;
    accepted = null;
    if (resultUrl) {
      URL.revokeObjectURL(resultUrl);
      resultUrl = null;
    }
    startProgress();

    const init = mode === 'i2v' ? initFile : null;
    try {
      const res = await submitWanVace(req, init, refFile, queueOpts);
      accepted = res;
      queuedBecause = res.blocked_reason ? blockedSummary(res.blocked_reason) : '';
      localStorage.setItem(ACTIVE_JOB_KEY, res.job_id);
      watchJob(res.job_id);
    } catch (e) {
      if (destroyed) return;
      error = e instanceof Error ? e.message : String(e);
      running = false;
      stopTimer();
    }
  }

  async function render() {
    const req = buildRequest();
    if (req) await startRender(req);
  }

  async function resume() {
    const rid = job?.resume_job_id;
    if (!rid) return;
    const req = buildRequest(rid);
    if (req) await startRender(req);
  }

  async function cancel() {
    try {
      // 🔴 Off the QUEUE, not the engine: the engine's own /cancel only reaches a row that is
      // already running, and a render that has not started yet is exactly the one worth dropping.
      if (job) await cancelQueueJob(job.job_id);
      else await cancelWanRender();
    } catch {
      // best-effort
    }
  }

  // Repopulate the form from a saved generation's request_meta; uploaded init /
  // reference frames can't be re-attached, so reattachNote flags that.
  function restore(g: Generation) {
    const m = g.request_meta;
    const md = str(m, 'mode');
    if (md === 't2v' || md === 'i2v' || md === 'continuation') mode = md as WanVaceMode;
    const np = str(m, 'negative_prompt');
    if (np !== undefined) negativePrompt = np;
    const w = num(m, 'width');
    if (w !== undefined) width = w;
    const h = num(m, 'height');
    if (h !== undefined) height = h;
    const f = num(m, 'fps');
    if (f !== undefined) fps = f;
    const fr = num(m, 'frames');
    if (fr !== undefined) frames = fr;
    const q = str(m, 'quant');
    if (q !== undefined) quant = q;
    const sd = num(m, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    const fallbackSecs = segments[0]?.seconds ?? 3;
    const segArr = arr(m, 'segments');
    const scripts = arr(m, 'scripts');
    if (segArr && segArr.length) {
      segments = segArr.map((el) => {
        const o = el && typeof el === 'object' ? (el as Record<string, unknown>) : {};
        return { prompt: str(o, 'prompt') ?? '', seconds: num(o, 'seconds') ?? fallbackSecs };
      });
    } else if (scripts && scripts.length) {
      segments = scripts.map((s) => ({
        prompt: typeof s === 'string' ? s : '',
        seconds: fallbackSecs
      }));
    }
    restoreNote = reattachNote(m);
    error = '';
  }
</script>

<div class="video-panel">
  {#if props}
    <p class="muted panel-status">
      Wan2.2-VACE · {mode.toUpperCase()} ·
      {#if props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
          >engine offline</span
        >{/if}
    </p>
  {/if}

  {#if error}
    <div class="error">{error}</div>
  {/if}

  <section class="card">
    <h3>Mode</h3>
    <select bind:value={mode}>
      <option value="t2v">Text → video</option>
      <option value="i2v">Image → video (start from a frame)</option>
      <option value="continuation">Continuation (per-segment prompt list)</option>
    </select>

    {#if mode === 'i2v'}
      <div class="cap">Init frame <span class="hint">— the clip starts from this image</span></div>
      <div class="dz"><FileDropzone bind:files={initFiles} accept="image/*" /></div>
    {/if}

    <div class="cap">
      Reference image <span class="hint"
        >— optional; identity anchor (keeps the subject consistent)</span
      >
    </div>
    <div class="dz"><FileDropzone bind:files={refFiles} accept="image/*" /></div>
  </section>

  <section class="card">
    <h3>{mode === 'continuation' ? 'Script' : 'Prompt'}</h3>
    {#if mode === 'continuation'}
      <p class="hint">
        Each line is one beat, windowed server-side into one continuous clip. Drafting keeps the
        SAME subject across every beat{#if initFile || refFile}, anchored to the image above{/if} — VACE
        drifts to a new subject when the beats diverge.
      </p>
      <div class="idea-row">
        <input
          type="text"
          placeholder={initFile || refFile
            ? 'Brief — optional; the image above sets the subject'
            : 'Brief — e.g. a dancer spinning through a neon alley'}
          bind:value={brief}
        />
        <label class="inline"
          >Beats <input type="number" min="1" max="16" bind:value={draftCount} /></label
        >
        <button
          class="btn-sec"
          onclick={draftScript}
          disabled={drafting || (!brief.trim() && !initFile && !refFile)}
        >
          {drafting ? 'Drafting…' : 'Draft with LLM'}
        </button>
      </div>
    {/if}
    {#each segments as seg, i (i)}
      <div class="segment">
        {#if mode === 'continuation'}<span class="seg-num">{i + 1}</span>{/if}
        <div class="seg-prompt">
          <textarea
            rows="3"
            placeholder="Prompt — subject, motion, setting, camera"
            bind:value={seg.prompt}></textarea>
          <button
            class="btn-sec seg-draft"
            onclick={() => draftSegment(i)}
            disabled={segDrafting[i]}
            title="Write a detailed Wan prompt with the LLM (uses the subject image + neighbours)"
          >
            {segDrafting[i] ? '…' : '✨ Draft'}
          </button>
          {#if seg.prompt.trim()}
            <div class="seg-improve">
              <input
                type="text"
                placeholder="Improve this shot — what to change? (blank = reword)"
                bind:value={segInstruction[i]}
                onkeydown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault();
                    improveSegment(i);
                  }
                }}
              />
              <button
                class="btn-sec"
                onclick={() => improveSegment(i)}
                disabled={segImproving[i]}
                title="Revise this prompt under the same rules, applying your note — keeps what works"
              >
                {segImproving[i] ? '…' : '✨ Improve'}
              </button>
            </div>
          {/if}
        </div>
        {#if mode === 'continuation'}
          <button
            class="btn-sec seg-rm"
            onclick={() => removeSegment(i)}
            disabled={segments.length <= 1}
            title="Remove beat">✕</button
          >
        {/if}
      </div>
    {/each}
    {#if mode === 'continuation'}
      <button class="btn-sec add" onclick={addSegment}>+ Add beat</button>
    {/if}

    <div class="cap">Negative prompt <span class="hint">— optional</span></div>
    <input type="text" placeholder="things to avoid" bind:value={negativePrompt} />
  </section>

  <section class="card">
    <h3>Settings</h3>
    <div class="knobs">
      <label>Width <input type="number" step="16" bind:value={width} /></label>
      <label>Height <input type="number" step="16" bind:value={height} /></label>
      <label>FPS <input type="number" bind:value={fps} /></label>
      <label>Frames <input type="number" bind:value={frames} /></label>
      <label
        >Quant
        <select bind:value={quant}>
          <option value="nvfp4">nvfp4 (default)</option>
          <option value="q4_k">q4_k (fallback)</option>
        </select>
      </label>
      {#if useSeed}
        <label>Seed <input type="number" bind:value={seed} /></label>
      {/if}
    </div>
    <div class="toggles">
      <label class="toggle"><input type="checkbox" bind:checked={useSeed} /> Fixed seed</label>
    </div>
    <p class="hint">Frames cap ~65 for a monolithic render; longer clips window server-side.</p>
  </section>

  <div class="actions">
    <button class="btn-primary" onclick={render} disabled={running}>
      {running ? 'Rendering…' : 'Render'}
    </button>
    <QueueChip kind="wan_vace" bind:options={queueOpts} response={accepted} />
    {#if running}
      <button class="btn-sec" onclick={cancel}>Cancel</button>
    {:else if resumable}
      <button class="btn-sec" onclick={resume}>Resume from last segment</button>
      <span class="muted"
        >A previous render stopped partway — continue from its banked segments.</span
      >
    {:else}
      <span class="muted">Render runs server-side; safe to navigate away or reload.</span>
    {/if}
  </div>

  {#if restoreNote}
    <p class="restore-note">{restoreNote}</p>
  {/if}

  {#if running}
    <div class="card progress">
      <!-- 🔴 While QUEUED the elapsed clock is the WAIT, not the render, so counting it against a
           render estimate would draw a bar that is already half full before anything started. -->
      {#if job?.status === 'queued'}
        <div class="bar indeterminate"></div>
        <span class="ptext">Queued{queuedBecause ? ` — ${queuedBecause}` : ''}</span>
      {:else}
        <div class="bar" class:indeterminate={!expectedSec}>
          {#if expectedSec}
            <div class="fill" style="width:{Math.min(99, (elapsed / expectedSec) * 100)}%"></div>
          {/if}
        </div>
        <span class="ptext">
          {elapsed.toFixed(0)}s elapsed{#if expectedSec}
            · ~{expectedSec.toFixed(0)}s expected (last render){/if}
        </span>
      {/if}
    </div>
  {/if}

  {#if job?.status === 'completed' && resultUrl}
    <section class="card result">
      <h3>Result</h3>
      <!-- svelte-ignore a11y_media_has_caption -->
      <video src={resultUrl} controls loop playsinline></video>
      <p class="meta">
        {job.segments} segment{job.segments === 1 ? '' : 's'} · rendered in {(
          job.render_ms / 1000
        ).toFixed(1)}s
        {#if job.gate_wait_ms > 0}· {(job.gate_wait_ms / 1000).toFixed(1)}s gate wait{/if}
      </p>
      <a class="btn-sec" href={resultUrl} download={`wan_${job.job_id}.webm`}>Download</a>
    </section>
  {/if}

  <GenerationHistory service="wan-vace" refreshKey={historyKey} onRestore={restore} />
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
  label,
  .cap {
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
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
  }
  .restore-note {
    margin: 10px 0 0;
    padding: 8px 12px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-2);
    color: var(--muted);
    font-size: 13px;
  }

  /* Script drafting row */
  .idea-row {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-top: 8px;
  }
  .idea-row input[type='text'] {
    flex: 1;
  }
  .idea-row label.inline {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    margin-top: 0;
    color: var(--ink);
    font-size: 13px;
    white-space: nowrap;
  }
  .idea-row label.inline input {
    width: 4.5rem;
  }
  .idea-row .btn-sec {
    white-space: nowrap;
  }

  .segment {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    margin-top: 8px;
  }
  .seg-prompt {
    flex: 1;
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    align-items: flex-start;
  }
  .seg-prompt textarea {
    flex: 1 1 auto;
    resize: vertical;
  }
  .seg-improve {
    flex: 0 0 100%;
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .seg-improve input {
    flex: 1 1 auto;
    min-width: 0;
  }
  .seg-improve .btn-sec {
    flex: 0 0 auto;
    white-space: nowrap;
    padding: 6px 10px;
    font-size: 12px;
  }
  .seg-draft {
    flex: 0 0 auto;
    white-space: nowrap;
    padding: 6px 10px;
    font-size: 12px;
  }
  .segment textarea {
    flex: 1;
    resize: vertical;
  }
  .seg-num {
    flex: 0 0 auto;
    width: 1.5rem;
    text-align: center;
    padding-top: 9px;
    color: var(--muted);
    font-size: 13px;
    font-weight: 600;
  }
  .seg-rm {
    flex: 0 0 auto;
    padding: 8px 12px;
  }
  .add {
    margin-top: 12px;
  }
  .dz {
    margin-top: 6px;
  }

  .knobs {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 8px 16px;
    align-items: end;
  }
  .knobs label {
    margin-top: 0;
  }
  .toggles {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
    margin-top: 14px;
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

  .actions {
    margin: 16px 0;
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }

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
  .bar .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.25s linear;
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

  .result video {
    width: 100%;
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
</style>
