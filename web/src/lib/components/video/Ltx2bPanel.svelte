<script lang="ts">
  import {
    submitLtx2b,
    getLtx2bJob,
    listLtx2bJobs,
    getLtx2bProps,
    cancelLtx2bRender,
    cancelQueueJob,
    ltxVideoScriptFromLlm,
    ltxPromptFromLlm,
    fetchLtxClip,
    JOB_POLL_MS,
    type LtxVideoGenerateRequest,
    type LtxJobStatus,
    type LtxVideoProps,
    type LtxSegment,
    type LtxPromptImage
  } from '$lib/api/client';
  import { blockedSummary } from '$lib/queue/format';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import { watchLiveJobs, type LiveWatch } from '$lib/queue/live';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import PinTime from '$lib/components/video/PinTime.svelte';
  import { framesFromSeconds } from '$lib/ltx/frames';
  import { assetRefFor } from '$lib/ltx/assetRef';
  import { planActiveJob } from '$lib/ltx/activeJob';
  import { adoptedElapsedSec } from '$lib/ltx/elapsed';
  import type { Generation } from '$lib/types';
  import { str, num, arr, reattachNote } from '$lib/restore';
  import { onMount, onDestroy } from 'svelte';

  // This device's own render, so a reload re-attaches without waiting on the job list. A fast path
  // only: the server's list is what decides (`planActiveJob`), which is what makes a render started
  // on ANOTHER device show up here at all. Keyed separately from ltx-video — separate engine,
  // separate job store.
  const ACTIVE_JOB_KEY = 'ltx-2b-active-job';
  // How often we re-ask the server what is rendering WHEN THE QUEUE STREAM IS DOWN, and only then.
  // The stream carries the transition ("a render of this kind started") that this used to poll for;
  // see `watchLiveJobs`.
  const ACTIVE_POLL_MS = 12000;

  let historyKey = $state(0);
  let restoreNote = $state('');

  // ── Director script (per-shot prompts + optional scene images) ──────────
  let segments = $state<LtxSegment[]>([
    { prompt: '', seconds: 4 },
    { prompt: '', seconds: 4 }
  ]);
  // Per-shot "new scene" image, aligned to `segments` by index. A shot with an image starts
  // a fresh i2v scene (shot 0's = the opener). Shots without one continue the previous shot.
  let segImageFiles = $state<(FileList | null)[]>([null, null]);
  const segImages = $derived(segImageFiles.map((fl) => (fl && fl.length ? fl[0] : null)));

  // Per-shot frame pins (keyframes): image (base64) + time-in-shot. A shot with pins renders as
  // a fresh keyframe shot (frame 0 = first, shot length = last, middle = a reveal / identity swap).
  type Pin = { image_b64: string; name: string; seconds: number };
  let segKeyframes = $state<Pin[][]>([[], []]);

  function addSegment() {
    segments = [...segments, { prompt: '', seconds: 4 }];
    segImageFiles = [...segImageFiles, null];
    segKeyframes = [...segKeyframes, []];
  }
  function removeSegment(i: number) {
    segments = segments.filter((_, idx) => idx !== i);
    segImageFiles = segImageFiles.filter((_, idx) => idx !== i);
    segKeyframes = segKeyframes.filter((_, idx) => idx !== i);
    // Keep the per-shot improve arrays aligned so a note doesn't stick to the
    // shot that shifts into the removed slot.
    segInstruction = segInstruction.filter((_, idx) => idx !== i);
    segImproving = segImproving.filter((_, idx) => idx !== i);
    segDrafting = segDrafting.filter((_, idx) => idx !== i);
  }
  function addPin(shot: number) {
    segKeyframes[shot] = [...segKeyframes[shot], { image_b64: '', name: '', seconds: 0 }];
  }
  function removePin(shot: number, k: number) {
    segKeyframes[shot] = segKeyframes[shot].filter((_, idx) => idx !== k);
  }
  async function setPinImage(shot: number, k: number, files: FileList | null) {
    const f = files && files.length ? files[0] : null;
    if (!f) return;
    // Stored once, referenced thereafter: a pin is full-resolution and rides in BOTH the render
    // request and every per-shot LLM draft, so inlining it re-uploaded the same megabytes on each.
    const ref = await assetRefFor(f);
    segKeyframes[shot][k] = { ...segKeyframes[shot][k], image_b64: ref, name: f.name };
  }

  // LLM script helper: expand a brief into N segments. (Reuses the model-agnostic ltx-video
  // drafting endpoint — it's just an LLM prompt-expander, not tied to a GPU service.)
  let brief = $state('');
  let draftCount = $state(4);
  let drafting = $state(false);

  // The opening frame (shot 0's scene image) doubles as the i2v reference the LLM reads.
  const openerImage = $derived(segImages[0]);

  async function draftScript() {
    if (!brief.trim() && !openerImage) return;
    drafting = true;
    error = '';
    try {
      // i2v: the model sees the opening frame and continues from it. 2B is video-only, so pass
      // audioSupplied=true → the prompts describe only visuals, never sound.
      const image = openerImage ? await assetRefFor(openerImage) : undefined;
      const res = await ltxVideoScriptFromLlm({
        brief,
        n_segments: draftCount,
        seconds_per_segment: segSeconds,
        image,
        audio_supplied: true
      });
      if (res.segments?.length) {
        segments = res.segments.map((p) => ({ prompt: p, seconds: segSeconds }));
        // Keep per-shot scene images + pins aligned; preserve any already set by index.
        segImageFiles = res.segments.map((_, i) => segImageFiles[i] ?? null);
        segKeyframes = res.segments.map((_, i) => segKeyframes[i] ?? []);
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      drafting = false;
    }
  }

  // Per-shot "Draft with LLM": write a detailed LTX prompt for one shot, feeding its scene image +
  // frame pins (with positions) + neighbours. 2B is video-only, so audio_supplied=true (no sound).
  let segDrafting = $state<boolean[]>([]);
  async function draftSegment(i: number) {
    segDrafting[i] = true;
    error = '';
    try {
      const scene = segImages[i];
      const images: LtxPromptImage[] = [];
      if (scene)
        images.push({ image_b64: await assetRefFor(scene), role: i === 0 ? 'opening' : 'scene' });
      for (const p of segKeyframes[i] ?? []) {
        if (p.image_b64)
          images.push({ image_b64: p.image_b64, role: 'keyframe', seconds: p.seconds });
      }
      const res = await ltxPromptFromLlm({
        idea: segments[i].prompt.trim(),
        mode: images.length ? 'i2v' : 't2v',
        seconds: segSeconds,
        fps,
        audio_supplied: true,
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

  // Per-shot "Improve": revise the existing prompt in place under the same LTX
  // rules, applying the row's note (blank = reword for a fresh take). Same scene
  // image + neighbour context as a fresh draft.
  let segInstruction = $state<string[]>([]);
  let segImproving = $state<boolean[]>([]);
  async function improveSegment(i: number) {
    if (!segments[i].prompt.trim()) return;
    segImproving[i] = true;
    error = '';
    try {
      const scene = segImages[i];
      const images: LtxPromptImage[] = [];
      if (scene)
        images.push({ image_b64: await assetRefFor(scene), role: i === 0 ? 'opening' : 'scene' });
      for (const p of segKeyframes[i] ?? []) {
        if (p.image_b64)
          images.push({ image_b64: p.image_b64, role: 'keyframe', seconds: p.seconds });
      }
      const res = await ltxPromptFromLlm({
        rewrite: segments[i].prompt.trim(),
        instruction: segInstruction[i]?.trim() || undefined,
        mode: images.length ? 'i2v' : 't2v',
        seconds: segSeconds,
        fps,
        audio_supplied: true,
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

  // ── Params ────────────────────────────────────────────────────────────
  // 2B defaults: 1216×704, 24fps, 8-step distilled render.
  let width = $state(1216);
  let height = $state(704);
  let fps = $state(24);
  let segSeconds = $state(4);
  let steps = $state(8);
  let cfgScale = $state(1.0);
  let samplingMethod = $state('euler_a');
  let useSeed = $state(false);
  let seed = $state(42);
  let negativePrompt = $state('');
  let contLatentFrames = $state(3);
  let scheduler = $state('');
  // How hard image pins (keyframes / scene images / opener) hold: 1 = exact image, lower = softer.
  let pinStrength = $state(1.0);

  // ── Result / status ─────────────────────────────────────────────────────
  let props = $state<LtxVideoProps | null>(null);
  let running = $state(false);
  let error = $state('');
  let job = $state<LtxJobStatus | null>(null);
  let resultUrl = $state<string | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);
  // A failed/cancelled render leaves banked segments on the server; offer to resume.
  const resumable = $derived(job?.resumable === true && !!job?.resume_job_id);

  // ── Render progress ───────────────────────────────────────────────────────
  // Elapsed ticks live; the ETA is the server's last render time (best signal we
  // have). Bar caps at 99% so an overrun doesn't read as "done".
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

  const mode = $derived(`${openerImage ? 'I' : 'T'}2V`);

  onMount(async () => {
    try {
      props = await getLtx2bProps();
    } catch {
      // props are best-effort; the page still works with the defaults above.
    }
    if (destroyed) return;
    await adoptActive();
    if (destroyed) return;
    liveWatch = watchLiveJobs({
      kinds: ['ltx_2b'],
      downMs: ACTIVE_POLL_MS,
      check: adoptActive
    });
  });

  onDestroy(() => {
    // Stop local timers only — DON'T cancel the render. The koblem background task owns
    // the GPU guard and keeps rendering; a reload reconnects to it.
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
    let jobs: LtxJobStatus[] | null = null;
    try {
      jobs = (await listLtx2bJobs()).jobs;
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

  // Poll a queue row to a terminal state, driving the UI.
  function watchJob(jobId: string) {
    stopPoll();
    if (destroyed) return;
    const tick = async () => {
      let s: LtxJobStatus;
      try {
        s = await getLtx2bJob(jobId);
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
            const url = await fetchLtxClip(s.result_url);
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

  // A built request paired with the per-shot scene-image files (filtered to the shots actually
  // sent, so `seg_image_<i>` indices line up with `req.segments[i]` server-side).
  type BuiltRequest = { req: LtxVideoGenerateRequest; segFiles: (File | null)[] };

  function buildRequest(resumeJobId?: string): BuiltRequest | null {
    // Keep prompt + scene image + pins aligned through the empty-prompt filter.
    const shots = segments
      .map((s, i) => ({
        prompt: s.prompt.trim(),
        file: segImages[i] ?? null,
        pins: (segKeyframes[i] ?? []).filter((p) => p.image_b64.length > 0)
      }))
      .filter((s) => s.prompt.length > 0);
    if (shots.length === 0) {
      error = 'Add at least one segment prompt.';
      return null;
    }
    // 2B is video-only + a single model, so the request omits `model`, `audio_source`, and
    // `upscaler_ratio` — the backend reuses LtxVideoGenerateRequest and defaults the rest.
    const req: LtxVideoGenerateRequest = {
      segments: shots.map((s) => ({
        prompt: s.prompt,
        seconds: segSeconds,
        // Keyframe pins win over a scene image — a shot with pins renders as a fresh keyframe shot.
        ...(s.pins.length
          ? { keyframes: s.pins.map((p) => ({ image_b64: p.image_b64, seconds: p.seconds })) }
          : s.file
            ? { image: { kind: 'upload' as const } }
            : {})
      })),
      width,
      height,
      fps,
      segment_frames: framesFromSeconds(segSeconds, fps),
      cont_latent_frames: contLatentFrames,
      steps,
      cfg_scale: cfgScale,
      pin_strength: pinStrength,
      sampling_method: samplingMethod,
      seed: useSeed ? seed : -1,
      save_clip: true,
      ...(negativePrompt.trim() ? { negative_prompt: negativePrompt.trim() } : {}),
      ...(scheduler.trim() ? { scheduler: scheduler.trim() } : {}),
      ...(resumeJobId ? { resume_job_id: resumeJobId } : {})
    };
    return { req, segFiles: shots.map((s) => s.file) };
  }

  async function startRender(built: BuiltRequest) {
    const { req, segFiles } = built;
    running = true;
    error = '';
    job = null;
    accepted = null;
    if (resultUrl) {
      URL.revokeObjectURL(resultUrl);
      resultUrl = null;
    }
    startProgress();

    try {
      const res = await submitLtx2b(req, segFiles, queueOpts);
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
    const built = buildRequest();
    if (built) await startRender(built);
  }

  async function resume() {
    const rid = job?.resume_job_id;
    if (!rid) return;
    const built = buildRequest(rid);
    if (built) await startRender(built);
  }

  async function cancel() {
    try {
      // 🔴 Off the QUEUE, not the engine: the engine's own /cancel only reaches a row that is
      // already running, and a render that has not started yet is exactly the one worth dropping.
      if (job) await cancelQueueJob(job.job_id);
      else await cancelLtx2bRender();
    } catch {
      // best-effort
    }
  }

  // Repopulate the form from a saved generation's request_meta; uploaded scene
  // images / keyframe pins can't be re-attached, so reattachNote flags that.
  function restore(g: Generation) {
    const m = g.request_meta;
    const f = num(m, 'fps');
    if (f !== undefined) fps = f;
    const w = num(m, 'width');
    if (w !== undefined) width = w;
    const h = num(m, 'height');
    if (h !== undefined) height = h;
    const np = str(m, 'negative_prompt');
    if (np !== undefined) negativePrompt = np;
    const sf = num(m, 'segment_frames');
    if (sf !== undefined) segSeconds = Math.round((sf / fps) * 2) / 2;
    const cl = num(m, 'cont_latent_frames');
    if (cl !== undefined) contLatentFrames = cl;
    const st = num(m, 'steps');
    if (st !== undefined) steps = st;
    const cf = num(m, 'cfg_scale');
    if (cf !== undefined) cfgScale = cf;
    const ps = num(m, 'pin_strength');
    if (ps !== undefined) pinStrength = ps;
    const sm = str(m, 'sampling_method');
    if (sm !== undefined) samplingMethod = sm;
    const sc = str(m, 'scheduler');
    if (sc !== undefined) scheduler = sc;
    const sd = num(m, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    const scripts = arr(m, 'scripts');
    if (scripts && scripts.length) {
      segments = scripts.map((s) => ({
        prompt: typeof s === 'string' ? s : '',
        seconds: segSeconds
      }));
      segImageFiles = segments.map(() => null);
      segKeyframes = segments.map(() => []);
    }
    restoreNote = reattachNote(m);
    error = '';
  }
</script>

<div class="video-panel">
  {#if props}
    <p class="muted panel-status">
      LTX-Video 0.9.8 2B · video-only · {mode} ·
      {#if props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
          >engine offline</span
        >{/if}
    </p>
  {/if}

  {#if error}
    <div class="error">{error}</div>
  {/if}

  <section class="card">
    <h3>Script</h3>
    <p class="hint">
      Each line is one shot, rendered back-to-back into a single continuous clip.
      {#if openerImage}<br />Drafting uses the opening scene image on shot 1.{/if}
    </p>
    <div class="idea-row">
      <input
        type="text"
        placeholder={openerImage
          ? 'Brief — optional; the scene image on shot 1 sets the scene'
          : 'Brief — e.g. a neon city at night, energetic'}
        bind:value={brief}
      />
      <label class="inline"
        >Shots <input type="number" min="1" max="16" bind:value={draftCount} /></label
      >
      <button
        class="btn-sec"
        onclick={draftScript}
        disabled={drafting || (!brief.trim() && !openerImage)}
      >
        {drafting ? 'Drafting…' : 'Draft with LLM'}
      </button>
    </div>

    <p class="hint scene-help">
      Add a <strong>scene image</strong> to a shot to cut to a new i2v scene (shot 1's = the
      opener); shots without one continue the previous shot. Or add <strong>frame pins</strong> to inject
      a specific image at a time in the shot (identity swap / reveal / first &amp; last frame) — the model
      bridges the motion.
    </p>
    <label class="pin-strength"
      >Pin strength
      <input type="range" min="0.5" max="1" step="0.05" bind:value={pinStrength} />
      <span class="hint"
        >{pinStrength >= 0.999 ? 'exact' : pinStrength.toFixed(2)} — higher = the pinned image is held
        exactly; lower blends it into the motion</span
      >
    </label>
    {#each segments as seg, i (i)}
      {@const pins = segKeyframes[i] ?? []}
      {@const hasPins = pins.some((p) => p.image_b64.length > 0)}
      {@const pinFresh = i === 0 || pins.some((p) => p.image_b64.length > 0 && p.seconds <= 0.001)}
      <div class="segment" class:scene-cut={!!segImages[i] || hasPins}>
        <span class="seg-num">{i + 1}</span>
        <div class="seg-body">
          <div class="seg-prompt">
            <textarea
              rows="3"
              placeholder={`Shot ${i + 1} — subject, motion, setting, camera`}
              bind:value={seg.prompt}></textarea>
            <button
              class="btn-sec seg-draft"
              onclick={() => draftSegment(i)}
              disabled={segDrafting[i]}
              title="Write a detailed LTX prompt with the LLM (uses this shot's image + neighbours)"
            >
              {segDrafting[i] ? '…' : '✨ Draft'}
            </button>
          </div>
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
          <div class="seg-scene">
            <div class="seg-dz">
              <FileDropzone
                accept="image/*"
                disabled={hasPins}
                label={segImages[i] ? segImages[i]!.name : 'Scene image'}
                hint={hasPins ? '(using pins)' : i === 0 ? 'opening frame' : 'cut to new scene'}
                onFiles={(fl) => (segImageFiles[i] = fl)}
              />
            </div>
            <span class="seg-badge">
              {#if hasPins}
                {pinFresh ? '◆ keyframe shot' : '↳ continues + pins'}
              {:else if segImages[i]}
                {i === 0 ? '◆ opening scene' : '◆ new scene'}
              {:else if i === 0}
                opening (text-to-video)
              {:else}
                ↳ continues shot {i}
              {/if}
            </span>
          </div>
          <div class="seg-pins">
            {#each pins as pin, k (k)}
              <div class="pin-row">
                <div class="pin-dz">
                  <FileDropzone
                    accept="image/*"
                    label={pin.name || 'Pin image'}
                    hint="frame-pinned"
                    onFiles={(fl) => setPinImage(i, k, fl)}
                  />
                </div>
                <PinTime bind:seconds={pin.seconds} shotSeconds={segSeconds} {fps} />
                <button class="btn-sec pin-rm" onclick={() => removePin(i, k)} title="Remove pin"
                  >✕</button
                >
              </div>
            {/each}
            <button class="btn-sec pin-add" onclick={() => addPin(i)}
              >+ frame pin <span class="hint"
                >— identity / reveal at a frame (Start / Middle / End)</span
              ></button
            >
          </div>
        </div>
        <button
          class="btn-sec seg-rm"
          onclick={() => removeSegment(i)}
          disabled={segments.length <= 1}
          title="Remove shot">✕</button
        >
      </div>
    {/each}
    <button class="btn-sec add" onclick={addSegment}>+ Add shot</button>
  </section>

  <section class="card">
    <h3>Settings</h3>
    <div class="knobs">
      <label>Width <input type="number" step="32" bind:value={width} /></label>
      <label>Height <input type="number" step="32" bind:value={height} /></label>
      <label>FPS <input type="number" bind:value={fps} /></label>
      <label>Seconds / shot <input type="number" step="0.5" bind:value={segSeconds} /></label>
      <label>Steps <input type="number" min="1" max="30" bind:value={steps} /></label>
    </div>
    <p class="hint recipe">
      LTX-Video 0.9.8 2B distilled · fast video-only toy · 8-step render at 1216×704 / 24fps.
    </p>
    <details class="advanced">
      <summary>Advanced render controls</summary>
      <div class="knobs advanced-knobs">
        <label
          >CFG scale <input
            type="number"
            min="1"
            max="20"
            step="0.1"
            bind:value={cfgScale}
          /></label
        >
        <label
          >Sampler
          <select bind:value={samplingMethod}>
            <option value="euler_a">Euler ancestral</option>
            <option value="euler">Euler</option>
            <option value="euler_cfg_pp">Euler CFG++</option>
            <option value="euler_a_cfg_pp">Euler ancestral CFG++</option>
            <option value="dpm++2m_sde">DPM++ 2M SDE</option>
            <option value="dpm++2m_sde_bt">DPM++ 2M SDE (Bt)</option>
            <option value="res_multistep">RES multistep</option>
            <option value="er_sde">ER SDE</option>
          </select></label
        >
        <label
          title="K overlap latent frames carried between continuation shots (default 3). Lower = harder cuts, less VRAM."
          >Continuity overlap <input
            type="number"
            min="1"
            max="8"
            bind:value={contLatentFrames}
          /></label
        >
        <label title="Sampler scheduler override (blank = server default).">
          Scheduler
          <input type="text" placeholder="default" bind:value={scheduler} />
        </label>
        {#if useSeed}
          <label>Seed <input type="number" bind:value={seed} /></label>
        {/if}
      </div>
      <div class="cap">Negative prompt <span class="hint">— optional; what to avoid</span></div>
      <textarea
        rows="2"
        placeholder="e.g. blurry, distorted, extra limbs"
        bind:value={negativePrompt}></textarea>
      <div class="toggles">
        <label class="toggle"><input type="checkbox" bind:checked={useSeed} /> Fixed seed</label>
      </div>
    </details>
  </section>

  <div class="actions">
    <button class="btn-primary" onclick={render} disabled={running}>
      {running ? 'Rendering…' : 'Render'}
    </button>
    <QueueChip kind="ltx_2b" bind:options={queueOpts} response={accepted} />
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
        {job.segments} segments · rendered in {(job.render_ms / 1000).toFixed(1)}s
        {#if job.gate_wait_ms > 0}· {(job.gate_wait_ms / 1000).toFixed(1)}s gate wait{/if}
      </p>
      <a class="btn-sec" href={resultUrl} download={`ltx2b_${job.job_id}.webm`}>Download</a>
    </section>
  {/if}

  <GenerationHistory service="ltx-2b" refreshKey={historyKey} onRestore={restore} />
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
  .recipe {
    margin-top: 12px;
    line-height: 1.45;
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
  .advanced-knobs {
    margin-top: 12px;
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

  /* Per-shot rows (storyboard) */
  .scene-help {
    margin: 4px 0 2px;
  }
  .pin-strength {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    margin: 2px 0 6px;
    flex-wrap: wrap;
  }
  .pin-strength input[type='range'] {
    flex: 0 1 160px;
  }
  .segment {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    margin-top: 8px;
    padding: 8px;
    border: 1px solid var(--border, rgba(255, 255, 255, 0.08));
    border-radius: 8px;
  }
  .segment.scene-cut {
    border-color: var(--accent, #7c9cff);
    box-shadow: inset 3px 0 0 var(--accent, #7c9cff);
  }
  .seg-body {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 6px;
    min-width: 0;
  }
  .segment textarea {
    width: 100%;
    resize: vertical;
  }
  .seg-prompt {
    display: flex;
    gap: 6px;
    align-items: flex-start;
  }
  .seg-prompt textarea {
    flex: 1 1 auto;
  }
  .seg-draft {
    flex: 0 0 auto;
    white-space: nowrap;
    padding: 6px 10px;
    font-size: 12px;
  }
  .seg-improve {
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
  .seg-scene {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .seg-dz {
    flex: 0 1 220px;
    font-size: 12px;
  }
  .seg-badge {
    font-size: 12px;
    color: var(--muted);
    white-space: nowrap;
  }
  .seg-pins {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .pin-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .pin-dz {
    flex: 0 1 200px;
    font-size: 12px;
  }
  .pin-rm,
  .pin-add {
    padding: 4px 10px;
    font-size: 12px;
  }
  .segment.scene-cut .seg-badge {
    color: var(--accent, #7c9cff);
    font-weight: 600;
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

  /* Settings */
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

  /* Render progress */
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

  /* Result */
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
