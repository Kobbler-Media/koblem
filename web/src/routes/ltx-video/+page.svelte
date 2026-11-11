<script lang="ts">
  import {
    submitLtxVideo,
    submitLtxRelip,
    getLtxJob,
    listLtxJobs,
    ltxVideoScriptFromLlm,
    getLtxVideoProps,
    ltxPromptFromLlm,
    fetchLtxClip,
    cancelLtxRender,
    type DirectorRequest,
    type LtxVideoGenerateRequest,
    type LtxRelipRequest,
    type LtxJobStatus,
    type LtxVideoProps,
    type LtxSegment,
    type LtxImageSource,
    type LtxAudioSource,
    type LtxUpscalerRatio,
    type LtxPromptImage
  } from '#lib/api/client.ts';
  import FileDropzone from '#lib/components/FileDropzone.svelte';
  import GenerationHistory from '#lib/components/GenerationHistory.svelte';
  import QueueChip from '#lib/components/queue/QueueChip.svelte';
  import type { EnqueueLike, QueueSubmitOptions } from '#lib/queue/options.ts';
  import { ltxReceipt } from '#lib/queue/ltx-receipt.ts';
  import UpscalePanel from '#lib/components/video/UpscalePanel.svelte';
  import PinTime from '#lib/components/video/PinTime.svelte';
  import type { Generation } from '#lib/types.ts';
  import { str, num, arr, reattachNote } from '#lib/restore.ts';
  import { framesFromSeconds } from '#lib/ltx/frames.ts';
  import { assetRefFor } from '#lib/ltx/assetRef.ts';
  import { planActiveJob } from '#lib/ltx/activeJob.ts';
  import { adoptedElapsedSec } from '#lib/ltx/elapsed.ts';
  import {
    LTX_VIDEO_ENGINE,
    LTX_VIDEO_HISTORY_SERVICE,
    isVideoTab,
    type VideoTab
  } from '#lib/ltx/videoTabs.ts';
  import { capabilities, unavailableFor } from '#lib/director/engine.ts';
  import { compileForDelivery } from '#lib/director/compile.ts';
  import { onMount, onDestroy } from 'svelte';

  // The engine behind both LTX tabs. Every control below is drawn off `caps`, never off this name —
  // an engine fact and a control gate are different questions and the second one is the only
  // one a template may ask.
  const ENGINE = LTX_VIDEO_ENGINE;
  const caps = capabilities(ENGINE);
  // Every knob hidden above, in the order the panel would have drawn it, so the explanation and
  // the gate cannot drift apart.
  const hiddenControls: string[] = (
    ['upscaleChain', 'cfg', 'customRecipe', 'negativePrompt'] as const
  ).filter((k) => !caps[k]);

  let restoreNote = $state('');

  // Repopulate the ltx25/lipdub form from a saved render. Dub/relip rows have no
  // per-shot scripts; scene-image/keyframe/audio uploads can't be re-attached.
  function restore(g: Generation) {
    const m = g.request_meta;
    const scripts = arr(m, 'scripts');
    if (scripts === undefined) {
      restoreNote = 'This entry was a dub/relip render — nothing to restore here.';
      return;
    }
    segments = scripts.map((s) => ({
      prompt: typeof s === 'string' ? s : '',
      seconds: segSeconds
    }));
    segImageFiles = segments.map(() => null);
    segKeyframes = segments.map(() => []);

    const md = str(m, 'model');
    if (md !== undefined) model = md;
    const np = str(m, 'negative_prompt');
    if (np !== undefined) negativePrompt = np;
    const w = num(m, 'width');
    if (w !== undefined) {
      width = w;
      resPreset = 'custom';
    }
    const h = num(m, 'height');
    if (h !== undefined) height = h;
    const f = num(m, 'fps');
    if (f !== undefined) fps = f;
    const sf = num(m, 'segment_frames');
    if (sf !== undefined) segSeconds = Math.max(1, Math.round(sf / (fps || 24)));
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
    const ur = str(m, 'upscaler_ratio');
    if (ur !== undefined) {
      upscalerRatio = ur as LtxUpscalerRatio;
      resPreset = 'custom';
    }
    const rs = str(m, 'refine_sampler');
    if (rs !== undefined) refineSampler = rs;
    const rst = num(m, 'refine_steps');
    if (rst !== undefined) refineSteps = rst;
    const sd = num(m, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    restoreNote = reattachNote(m);
    error = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  // This device's own render, so a reload re-attaches without waiting on the job list. A fast path
  // only: the server's list is what decides (`planActiveJob`), which is what makes a render started
  // on ANOTHER device show up here at all.
  const ACTIVE_JOB_KEY = 'ltx-active-job';
  // How often we re-ask the server what is rendering while the page sits idle. One tiny in-memory
  // query; stops mattering the moment a render is adopted (the 1.5s job poll takes over).
  const ACTIVE_POLL_MS = 12000;

  let historyKey = $state(0);

  // ── Tab: full render vs lip-dub (relip) ────────────────────────────────
  // 🔴 `upscale` is NOT a render engine — it is LTX-2.5's upsampler, a restorer that takes a
  // finished mp4 and gives a bigger one back. It lives here because this is where video work is,
  // and because Director hands a take to it by deep-linking `?tab=upscale&project=…&take=…`.
  let tab = $state<VideoTab>('ltx25');
  let upscaleProject = $state('');
  let upscaleTier = $state('');
  let upscaleShot = $state('');
  let upscaleVariant = $state('');
  // Lip-dub inputs: a source video + a drive-audio track → re-sync the mouths.
  let dubVideoFiles = $state<FileList | null>(null);
  let dubVideoFile = $derived(dubVideoFiles && dubVideoFiles.length ? dubVideoFiles[0] : null);
  let dubAudioFiles = $state<FileList | null>(null);
  let dubAudioFile = $derived(dubAudioFiles && dubAudioFiles.length ? dubAudioFiles[0] : null);
  let dubPrompt = $state('');
  let dubGuidance = $state(1);
  // Off by default: on this engine two-stage crops the guides before the upsampler, so the source
  // a lip-dub exists to preserve stops conditioning the render halfway through it.
  let dubTwoStage = $state(false);

  // ── Director script (per-shot prompts + optional scene images) ──────────
  let segments = $state<LtxSegment[]>([
    { prompt: '', seconds: 4 },
    { prompt: '', seconds: 4 }
  ]);
  // Per-shot "new scene" image, aligned to `segments` by index. A shot with an image starts
  // a fresh i2v scene (shot 0's = the opener); audio stays continuous across the cut.
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
    segKeyframes[shot][k] = {
      ...segKeyframes[shot][k],
      image_b64: ref,
      name: f.name
    };
  }

  // LLM script helper: expand a brief into N segments.
  let brief = $state('');
  let draftCount = $state(4);
  let drafting = $state(false);

  async function draftScript() {
    if (!brief.trim() && !imageFile) return;
    drafting = true;
    error = '';
    try {
      // i2v: the model sees the opening frame and continues from it. The audio role only
      // toggles whether the prompts describe sound (LTX generates it) or stay silent.
      const image = imageFile ? await assetRefFor(imageFile) : undefined;
      const res = await ltxVideoScriptFromLlm({
        brief,
        n_segments: draftCount,
        seconds_per_segment: segSeconds,
        image,
        audio_supplied: audioRole !== 'none'
      });
      if (res.segments?.length) {
        segments = res.segments.map((p) => ({
          prompt: p,
          seconds: segSeconds
        }));
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
  // frame pins (with positions) + neighbours. Shot 0's reference image counts as its opener.
  let segDrafting = $state<boolean[]>([]);
  async function draftSegment(i: number) {
    segDrafting[i] = true;
    error = '';
    try {
      const images: LtxPromptImage[] = [];
      const scene = segImages[i] ?? (i === 0 ? imageFile : null);
      if (scene)
        images.push({
          image_b64: await assetRefFor(scene),
          role: i === 0 ? 'opening' : 'scene'
        });
      for (const p of segKeyframes[i] ?? []) {
        if (p.image_b64)
          images.push({
            image_b64: p.image_b64,
            role: 'keyframe',
            seconds: p.seconds
          });
      }
      const res = await ltxPromptFromLlm({
        idea: segments[i].prompt.trim(),
        mode: images.length ? 'i2v' : 't2v',
        seconds: segSeconds,
        fps,
        audio_supplied: audioRole !== 'none',
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
      const images: LtxPromptImage[] = [];
      const scene = segImages[i] ?? (i === 0 ? imageFile : null);
      if (scene)
        images.push({
          image_b64: await assetRefFor(scene),
          role: i === 0 ? 'opening' : 'scene'
        });
      for (const p of segKeyframes[i] ?? []) {
        if (p.image_b64)
          images.push({
            image_b64: p.image_b64,
            role: 'keyframe',
            seconds: p.seconds
          });
      }
      const res = await ltxPromptFromLlm({
        rewrite: segments[i].prompt.trim(),
        instruction: segInstruction[i]?.trim() || undefined,
        mode: images.length ? 'i2v' : 't2v',
        seconds: segSeconds,
        fps,
        audio_supplied: audioRole !== 'none',
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

  // ── Reference image (optional, seg-0 i2v) ──────────────────────────────
  let imageFiles = $state<FileList | null>(null);
  let imageFile = $derived(imageFiles && imageFiles.length ? imageFiles[0] : null);

  // ── Audio ───────────────────────────────────────────────────────────────
  // none | voice | bgm | mixed. Voice drives lip-sync; bgm is muxed under; mixed
  // is split (songgen /separate) → voice drives lip-sync + bgm muxed.
  let audioRole = $state<'none' | 'voice' | 'bgm' | 'mixed'>('none');
  let audioFiles = $state<FileList | null>(null);
  let audioFile = $derived(audioFiles && audioFiles.length ? audioFiles[0] : null);

  // ── Params ────────────────────────────────────────────────────────────
  // 🔴 These are DELIVERED sizes, and the request carries `dims_are_target` so they stay that way
  // whatever the upscaler is set to — koblem sizes the base pass down by the chain's factor. Both
  // land on exactly the output the same preset produced when this tab rendered on 2.3 (its 1080p
  // was 960×544 → ×2 = 1920×1088); 2.5 gets there in one pass instead of two. The 2.3-only
  // `1080p-hi` rung is gone with the chain that defined it — ×1.5 off a 1280×704 base is a route
  // to 1080p, not a resolution.
  // "custom" = the user has diverged from a preset via the manual knobs below.
  type ResPreset = '720p' | '1080p' | 'custom';
  const RES_PRESETS: Record<
    Exclude<ResPreset, 'custom'>,
    { w: number; h: number; label: string }
  > = {
    '720p': { w: 1280, h: 704, label: '720p (1280×704)' },
    '1080p': { w: 1920, h: 1088, label: '1080p (1920×1088)' }
  };

  let resPreset = $state<ResPreset>('1080p');
  let width = $state(RES_PRESETS['1080p'].w);
  let height = $state(RES_PRESETS['1080p'].h);
  let upscalerRatio = $state<LtxUpscalerRatio>('none');
  let fps = $state(24);
  // 7s = 169 frames (8*21+1) at 24fps — see DEFAULT_SEGMENT_FRAMES in api/src/routes/ltx_video.rs.
  let segSeconds = $state(7);
  let steps = $state(6);
  let cfgScale = $state(1.0);
  let samplingMethod = $state('lcm');
  // Refine (hires) pass sampler + steps. ver3 "distilled" default = 2-step Euler.
  let refineSampler = $state('euler');
  let refineSteps = $state(2);
  let useSeed = $state(false);
  /** Lipdub LoRA multiplier. 1 = the adapter's trained scale; only sent when changed. */
  let dubLipdubStrength = $state(1);
  let seed = $state(42);
  // Base DiT variant selector (populated from props.variants) + previously-hidden chain knobs.
  let model = $state('base');
  let negativePrompt = $state('');
  let contLatentFrames = $state(3);
  let scheduler = $state('');
  // How hard image pins (keyframes / scene images / opener) hold: 1 = exact image, lower = softer.
  let pinStrength = $state(1.0);

  // Manual knob edits below flip the preset to "custom" so the select reflects that the values
  // have diverged.
  function applyPreset(p: ResPreset) {
    resPreset = p;
    if (p === 'custom') return;
    const cfg = RES_PRESETS[p];
    width = cfg.w;
    height = cfg.h;
  }

  // ── Result / status ─────────────────────────────────────────────────────
  let props = $state<LtxVideoProps | null>(null);
  // Server-registered base-DiT variants for the model selector; always offers "base".
  const variants = $derived(props?.variants?.length ? props.variants : ['base']);
  const variantsShown = $derived(caps.modelVariants && variants.length > 1);
  // Runtime LoRAs the engine offers, and the ones enabled for this render with their strengths.
  // A folded variant has its LoRA baked into the weights at a fixed strength and costs nothing
  // extra; these are the TUNABLE alternative — adjustable and stackable, but each one costs
  // VRAM and sampling time (measured ~+0.7 GB and ~+16% wall-clock for one adapter at
  // 1920x1088). Name carries the file extension because that is what the engine matches on.
  const availableLoras = $derived(props?.loras ?? []);
  let loras = $state<{ path: string; multiplier: number }[]>([]);
  /** The strength to ADD an adapter at, from the server's annotation.
   *
   * 🔴 NOT ALWAYS 1.0, and the gap is silent — these files carry no `.alpha` tensors, so the
   * engine multiplies their delta raw. `omninft` was trained at alpha/r = 2 and renders HALF at
   * 1.0; the close-up face adapter wants ~0.2 and swamps the sheet at 1.0. Neither is an error,
   * just a render that quietly under- or over-fires. */
  function loraDefault(path: string): number {
    return availableLoras.find((l) => l.path === path)?.default_multiplier ?? 1.0;
  }
  function toggleLora(path: string) {
    const i = loras.findIndex((l) => l.path === path);
    if (i >= 0) loras = loras.filter((_, j) => j !== i);
    else loras = [...loras, { path, multiplier: loraDefault(path) }];
  }
  function loraStrength(path: string): number {
    return loras.find((l) => l.path === path)?.multiplier ?? loraDefault(path);
  }
  function setLoraStrength(path: string, v: number) {
    loras = loras.map((l) => (l.path === path ? { ...l, multiplier: v } : l));
  }
  // Whether THIS page's watched render is in flight — a progress readout, never a submit gate: the
  // route enqueues, so a second Render queues behind the first rather than being refused.
  let running = $state(false);
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);
  let error = $state('');
  let job = $state<LtxJobStatus | null>(null);
  let resultUrl = $state<string | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  // Which of this page's flows started `job`, or null for one adopted off the server list — that
  // list says nothing about which form produced a render.
  let jobTab = $state<'ltx25' | 'lipdub' | null>(null);
  // A failed/cancelled render leaves banked segments on the server; offer to resume.
  //
  // 🔴 Three separate answers, and all three are needed. `caps.resume` is the MODEL's;
  // `job.resumable` is the DEPLOYMENT's, already ANDed server-side with a live bank probe against
  // that job's own engine (`resumable_now`); `jobTab` is what the banked key would be continued
  // INTO. Without the last one a failed lip-dub — same engine, same job store, same poll loop —
  // offers a resume here that would continue a chain render out of one relip window's bank.
  const resumable = $derived(
    caps.resume && jobTab === 'ltx25' && job?.resumable === true && !!job?.resume_job_id
  );

  // ── Render progress ───────────────────────────────────────────────────────
  // Elapsed ticks live; the ETA is the server's last render time (best signal we
  // have). Bar caps at 99% so an overrun doesn't read as "done".
  let elapsed = $state(0);
  let progressTimer: ReturnType<typeof setInterval> | null = null;
  let activePollTimer: ReturnType<typeof setInterval> | null = null;
  // Set once the page is torn down. Not reactive: its only job is to stop already-running async
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

  const mode = $derived(`${imageFile ? 'I' : 'T'}${audioRole !== 'none' ? 'A' : ''}2V`);

  function imageSource(): LtxImageSource {
    return imageFile ? { kind: 'upload' } : { kind: 'none' };
  }
  function audioSource(): LtxAudioSource {
    switch (audioRole) {
      case 'voice':
        return { kind: 'voice_upload' };
      case 'bgm':
        return { kind: 'bgm_upload' };
      case 'mixed':
        return { kind: 'mixed_upload' };
      default:
        return { kind: 'none' };
    }
  }

  onMount(async () => {
    const requestedTab = new URLSearchParams(window.location.search).get('tab');
    // Director's hand-off rides on the same query string, read once here so the panel can be
    // mounted already pointing at the take.
    const q = new URLSearchParams(window.location.search);
    upscaleProject = q.get('project') ?? '';
    upscaleTier = q.get('tier') ?? '';
    upscaleShot = q.get('shot') ?? '';
    upscaleVariant = q.get('variant') ?? '';
    if (isVideoTab(requestedTab)) tab = requestedTab;
    // Health light + ETA, from the one service both LTX tabs submit to. Best-effort — the page
    // still works with the defaults above.
    await getLtxVideoProps(ENGINE)
      .then((p) => (props = p))
      .catch(() => {});
    if (destroyed) return;
    await adoptActive();
    if (destroyed) return;
    activePollTimer = setInterval(() => void adoptActive(), ACTIVE_POLL_MS);
  });

  onDestroy(() => {
    // Stop local timers only — DON'T cancel the render. The koblem background task owns
    // the GPU guard and keeps rendering; a reload reconnects to it.
    destroyed = true;
    stopTimer();
    stopPoll();
    if (activePollTimer) clearInterval(activePollTimer);
    activePollTimer = null;
    if (resultUrl) URL.revokeObjectURL(resultUrl);
  });

  // Attach to whatever the LTX engines are rendering, wherever it was started. One koblem job
  // store serves both, so a lip-dub started elsewhere is adopted here too. `standalone`
  // drops the Director renders sharing this job store — they belong to a project, and this page
  // has no part in one. Never takes over from a render already streaming into this tab: that is
  // either the same job or one this tab is driving, and two poll loops would fight over the timer.
  async function adoptActive() {
    if (destroyed || running) return;
    let jobs: LtxJobStatus[] | null = null;
    try {
      jobs = (await listLtxJobs(true)).jobs;
    } catch {
      // Server unreachable: planActiveJob falls back to the remembered id.
    }
    // Re-checked after the await: onDestroy cannot cancel a request already in flight, and
    // adopting here would start timers nothing is left to clear.
    if (destroyed || running) return;
    const plan = planActiveJob(localStorage.getItem(ACTIVE_JOB_KEY), jobs);
    if (plan.forget) localStorage.removeItem(ACTIVE_JOB_KEY);
    if (!plan.adopt) return;
    jobTab = null;
    localStorage.setItem(ACTIVE_JOB_KEY, plan.adopt);
    if (plan.running) {
      running = true;
      startProgress(adoptedElapsedSec(plan.createdAt, Date.now()));
    }
    watchJob(plan.adopt);
  }

  // Poll a koblem job to a terminal state, driving the UI. Tolerates transient poll
  // failures (keeps polling) — only a terminal job status stops it.
  function watchJob(jobId: string) {
    stopPoll();
    if (destroyed) return;
    let fails = 0;
    const tick = async () => {
      let s: LtxJobStatus;
      try {
        s = await getLtxJob(jobId);
        fails = 0;
      } catch {
        // The job may be a transient blip, or gone (404 — swept after TTL / koblem
        // restarted). Tolerate a few failures, then give up so we don't poll forever.
        if (destroyed) return;
        if (++fails >= 20) {
          stopPoll();
          stopTimer();
          running = false;
          localStorage.removeItem(ACTIVE_JOB_KEY);
          error = 'Lost track of the render (it may have finished or expired).';
        }
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
    pollTimer = setInterval(() => void tick(), 1500);
  }

  // A built request paired with the per-shot scene-image files (filtered to the shots actually
  // sent, so `seg_image_<i>` indices line up with `req.segments[i]` server-side).
  type BuiltRequest = {
    req: DirectorRequest;
    segFiles: (File | null)[];
  };

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
    const req: LtxVideoGenerateRequest = {
      segments: shots.map((s) => ({
        prompt: s.prompt,
        seconds: segSeconds,
        // Keyframe pins win over a scene image — a shot with pins renders as a fresh keyframe shot.
        ...(s.pins.length
          ? {
              keyframes: s.pins.map((p) => ({
                image_b64: p.image_b64,
                seconds: p.seconds
              }))
            }
          : s.file
            ? { image: { kind: 'upload' as const } }
            : {})
      })),
      model: variantsShown ? model : 'base',
      // Omitted entirely when none are enabled, so an unused picker cannot change a render.
      ...(loras.length ? { loras } : {}),
      image_source: imageSource(),
      audio_source: audioSource(),
      width,
      height,
      // The knobs above are the OUTPUT, so an upscale chain is sized down INTO them rather than
      // multiplying past them — the picker cannot silently change what gets delivered.
      dims_are_target: true,
      fps,
      segment_frames: framesFromSeconds(segSeconds, fps),
      cont_latent_frames: contLatentFrames,
      steps,
      cfg_scale: cfgScale,
      pin_strength: pinStrength,
      sampling_method: samplingMethod,
      refine_sampler: refineSampler,
      refine_steps: refineSteps,
      seed: useSeed ? seed : -1,
      upscaler_ratio: upscalerRatio,
      save_clip: true,
      ...(negativePrompt.trim() ? { negative_prompt: negativePrompt.trim() } : {}),
      ...(scheduler.trim() ? { scheduler: scheduler.trim() } : {}),
      ...(resumeJobId ? { resume_job_id: resumeJobId } : {})
    };
    // The same pass the Director dispatches through, so the ladder an engine will not honour is
    // stripped in ONE place rather than two that can disagree.
    return { req: compileForDelivery(ENGINE, req), segFiles: shots.map((s) => s.file) };
  }

  async function startRender(built: BuiltRequest) {
    const { req, segFiles } = built;
    running = true;
    error = '';
    accepted = null;
    job = null;
    if (resultUrl) {
      URL.revokeObjectURL(resultUrl);
      resultUrl = null;
    }
    startProgress();

    // The voice/mixed track drives lip-sync; a bgm-only track muxes under.
    const audioPart = audioRole === 'voice' || audioRole === 'mixed' ? audioFile : null;
    const bgmPart = audioRole === 'bgm' ? audioFile : null;

    try {
      const { job_id, status } = await submitLtxVideo(
        req,
        imageFile,
        audioPart,
        bgmPart,
        segFiles,
        undefined,
        null,
        ENGINE,
        queueOpts
      );
      accepted = ltxReceipt(status);
      jobTab = 'ltx25';
      localStorage.setItem(ACTIVE_JOB_KEY, job_id);
      watchJob(job_id);
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
      await cancelLtxRender();
    } catch {
      // best-effort
    }
  }

  // Lip-dub / relip: attach the drive-audio to the source video and re-sync the mouths.
  // Reuses the same job store + poll loop as a full render (the result lands in generations).
  async function relip() {
    if (!dubVideoFile) {
      error = 'Add a source video to lip-dub.';
      return;
    }
    if (!dubAudioFile) {
      error = 'Add a drive-audio track.';
      return;
    }
    running = true;
    error = '';
    accepted = null;
    job = null;
    if (resultUrl) {
      URL.revokeObjectURL(resultUrl);
      resultUrl = null;
    }
    startProgress();

    const req: LtxRelipRequest = {
      prompt: dubPrompt.trim() || undefined,
      a2v_guidance: dubGuidance,
      two_stage: dubTwoStage,
      seed: useSeed ? seed : -1,
      save_clip: true,
      // Only sent when it differs from the adapter's trained scale, so an untouched form leaves the
      // server's own default in place.
      lipdub_strength: dubLipdubStrength === 1 ? undefined : dubLipdubStrength
    };
    try {
      const { job_id, status } = await submitLtxRelip(req, dubVideoFile, dubAudioFile, queueOpts);
      accepted = ltxReceipt(status);
      jobTab = 'lipdub';
      localStorage.setItem(ACTIVE_JOB_KEY, job_id);
      watchJob(job_id);
    } catch (e) {
      if (destroyed) return;
      error = e instanceof Error ? e.message : String(e);
      running = false;
      stopTimer();
    }
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Video</h2>
    {#if props && (tab === 'ltx25' || tab === 'lipdub')}
      <p class="muted">
        {caps.label} · {mode} ·
        {#if props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
            >engine offline</span
          >{/if}
      </p>
    {/if}
  </div>

  {#if error && (tab === 'ltx25' || tab === 'lipdub')}
    <div class="error">{error}</div>
  {/if}

  <div class="tabs">
    <button class="tab" class:on={tab === 'ltx25'} onclick={() => (tab = 'ltx25')}
      >{caps.label}</button
    >
    <button class="tab" class:on={tab === 'lipdub'} onclick={() => (tab = 'lipdub')}>Lipdub</button>
    <button class="tab" class:on={tab === 'upscale'} onclick={() => (tab = 'upscale')}
      >Upscale</button
    >
  </div>

  {#if tab === 'ltx25'}
    <section class="card">
      <h3>Inputs</h3>
      <div class="cap">
        Reference image <span class="hint"
          >— optional; the first shot becomes image-to-video, and the LLM reads it when drafting</span
        >
      </div>
      <div class="dz">
        <FileDropzone bind:files={imageFiles} accept="image/*" />
      </div>

      <div class="cap">Audio</div>
      <select bind:value={audioRole}>
        <option value="none">None — model generates its own audio</option>
        <option value="voice">Voice — drives lip-sync</option>
        <option value="bgm">Background music — muxed under the clip</option>
        <option value="mixed">Mixed — split (voice → lip-sync, music → mux)</option>
      </select>
      {#if audioRole !== 'none'}
        <div class="dz">
          <FileDropzone bind:files={audioFiles} accept="audio/*" />
        </div>
      {/if}
    </section>

    <section class="card">
      <h3>Script</h3>
      <p class="hint">
        Each line is one shot, rendered back-to-back into a single continuous clip.
        {#if imageFile || audioRole !== 'none'}<br />Drafting uses the {mode} intent above.{/if}
      </p>
      <div class="idea-row">
        <input
          type="text"
          placeholder={imageFile
            ? 'Brief — optional; the image above sets the scene'
            : 'Brief — e.g. a neon city at night, energetic'}
          bind:value={brief}
        />
        <label class="inline"
          >Shots <input type="number" min="1" max="16" bind:value={draftCount} /></label
        >
        <button
          class="btn-sec"
          onclick={draftScript}
          disabled={drafting || (!brief.trim() && !imageFile)}
        >
          {drafting ? 'Drafting…' : 'Draft with LLM'}
        </button>
      </div>

      <p class="hint scene-help">
        Add a <strong>scene image</strong> to a shot to cut to a new i2v scene; shots without one
        continue the previous shot. Or add
        <strong>frame pins</strong> to inject a specific image at a time in the shot (identity swap /
        reveal / first &amp; last frame) — the model bridges the motion. Audio stays continuous throughout.
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
        {@const pinFresh =
          i === 0 || pins.some((p) => p.image_b64.length > 0 && p.seconds <= 0.001)}
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
                  {imageFile ? 'opening (reference image)' : 'opening (text-to-video)'}
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
        <label class="wide"
          >Resolution
          <select bind:value={resPreset} onchange={() => applyPreset(resPreset)}>
            <option value="720p">{RES_PRESETS['720p'].label}</option>
            <option value="1080p">{RES_PRESETS['1080p'].label}</option>
            <option value="custom">Custom (manual knobs)</option>
          </select></label
        >
        <label>FPS <input type="number" bind:value={fps} /></label>
        <label>Seconds / shot <input type="number" step="0.5" bind:value={segSeconds} /></label>
        {#if variantsShown}
          <label
            >Model
            <select bind:value={model}>
              {#each variants as v (v)}
                <option value={v}>{v}</option>
              {/each}
            </select></label
          >
        {/if}
      </div>
      {#if availableLoras.length}
        <details class="advanced">
          <summary>Runtime LoRAs ({loras.length} on)</summary>
          <p class="hint">
            Applied on top of the base model, at a strength you choose{variantsShown
              ? '. The model variants already have their LoRA baked in at a fixed strength and cost nothing extra — use these to tune a strength, try a combination, or stack two'
              : ''}. Each adds roughly 0.7 GB of VRAM and ~16% to the render time, so keep the list
            short. Ticking one adds it at ITS OWN trained strength, which is not always 1.00.
          </p>
          <div class="lora-list">
            {#each availableLoras as l (l.path)}
              {@const on = loras.some((x) => x.path === l.path)}
              <div class="lora-row">
                <label class="lora-name">
                  <input type="checkbox" checked={on} onchange={() => toggleLora(l.path)} />
                  <!-- File name stays on screen next to the label: it is the string the engine
                       400s on when a name goes stale. Undescribed adapters label themselves. -->
                  <b>{l.label}</b>{#if l.path !== l.label}
                    · {l.path}{/if}
                </label>
                {#if on}
                  <input
                    type="range"
                    min="0"
                    max="3"
                    step="0.05"
                    value={loraStrength(l.path)}
                    oninput={(e) => setLoraStrength(l.path, Number(e.currentTarget.value))}
                  />
                  <input
                    class="lora-num"
                    type="number"
                    min="0"
                    max="3"
                    step="0.05"
                    value={loraStrength(l.path)}
                    oninput={(e) => setLoraStrength(l.path, Number(e.currentTarget.value))}
                  />
                {/if}
              </div>
              {#if on && (l.blurb || l.trigger)}
                <p class="hint" style="margin:-4px 0 8px 24px">
                  {l.blurb ?? ''}{#if l.trigger}
                    Adds <code>{l.trigger}</code> to the prompt automatically{l.trigger_required
                      ? ''
                      : ' (it also works without)'}.{/if}
                </p>
              {/if}
            {/each}
          </div>
        </details>
      {/if}
      <p class="hint recipe">
        {caps.upscaleChain
          ? 'Production (ver3 distilled): 6-step LCM base → ×2 staged spatial upscale → 2-step Euler refine.'
          : 'One pass at the size you pick, on the distilled checkpoint’s own sampler, schedule and step count. It writes its own soundtrack when you supply none.'}
      </p>
      <details class="advanced">
        <summary>Advanced render controls</summary>
        <p class="hint">
          {caps.upscaleChain
            ? 'Resolution and upscale choose the staged render path. The production default is 960×544 → ×2.'
            : 'Width and height are the delivered size — this engine renders them directly.'}
        </p>
        <div class="knobs advanced-knobs">
          {#if caps.upscaleChain}
            <label
              >Upscaler
              <select bind:value={upscalerRatio} onchange={() => (resPreset = 'custom')}>
                <option value="none">none</option>
                <option value="x1.5">×1.5</option>
                <option value="x2">×2</option>
              </select></label
            >
          {/if}
          <label
            >Width
            <input
              type="number"
              step="32"
              bind:value={width}
              oninput={() => (resPreset = 'custom')}
            /></label
          >
          <label
            >Height
            <input
              type="number"
              step="32"
              bind:value={height}
              oninput={() => (resPreset = 'custom')}
            /></label
          >
          {#if caps.cfg}
            <label
              >CFG scale <input
                type="number"
                min="1"
                max="20"
                step="0.1"
                bind:value={cfgScale}
              /></label
            >
          {/if}
          <label
            title="K overlap latent frames carried between continuation shots (default 3). Lower = harder cuts, less VRAM."
            >Continuity overlap <input
              type="number"
              min="1"
              max="8"
              bind:value={contLatentFrames}
            /></label
          >
          {#if caps.customRecipe}
            <label title="Sampler scheduler override (blank = server default).">
              Scheduler
              <input type="text" placeholder="default" bind:value={scheduler} />
            </label>
          {/if}
          {#if useSeed}
            <label>Seed <input type="number" bind:value={seed} /></label>
          {/if}
        </div>
        {#if caps.customRecipe}
          <div class="cap">
            Two-stage sampling <span class="hint">— ver3 distilled default</span>
          </div>
          <div class="stage-group">
            <div class="stage-label">Stage 1 — Base</div>
            <div class="knobs advanced-knobs">
              <label
                >Base sampler
                <select bind:value={samplingMethod}>
                  <option value="lcm">LCM</option>
                  <option value="euler">Euler</option>
                  <option value="euler_a">Euler ancestral</option>
                  <option value="euler_cfg_pp">Euler CFG++</option>
                  <option value="euler_a_cfg_pp">Euler ancestral CFG++</option>
                  <option value="dpm++2m_sde">DPM++ 2M SDE</option>
                  <option value="dpm++2m_sde_bt">DPM++ 2M SDE (Bt)</option>
                  <option value="res_multistep">RES multistep</option>
                  <option value="er_sde">ER SDE</option>
                </select></label
              >
              <label>Base steps <input type="number" min="1" max="30" bind:value={steps} /></label>
            </div>
          </div>
          <div class="stage-group">
            <div class="stage-label">Stage 2 — Refine / hires</div>
            <div class="knobs advanced-knobs">
              <label
                >Refine sampler
                <select bind:value={refineSampler}>
                  <option value="euler">Euler</option>
                  <option value="lcm">LCM</option>
                  <option value="euler_a">Euler ancestral</option>
                  <option value="euler_cfg_pp">Euler CFG++</option>
                  <option value="euler_a_cfg_pp">Euler ancestral CFG++</option>
                  <option value="dpm++2m_sde">DPM++ 2M SDE</option>
                  <option value="dpm++2m_sde_bt">DPM++ 2M SDE (Bt)</option>
                  <option value="res_multistep">RES multistep</option>
                  <option value="er_sde">ER SDE</option>
                </select></label
              >
              <label
                >Refine steps <input
                  type="number"
                  min="1"
                  max="30"
                  bind:value={refineSteps}
                /></label
              >
            </div>
          </div>
        {/if}
        {#if caps.negativePrompt}
          <div class="cap">
            Negative prompt <span class="hint">— optional; what to avoid</span>
          </div>
          <textarea
            rows="2"
            placeholder="e.g. blurry, distorted, extra limbs"
            bind:value={negativePrompt}></textarea>
        {/if}
        <div class="toggles">
          <label class="toggle"><input type="checkbox" bind:checked={useSeed} /> Fixed seed</label>
        </div>
        <!-- Explained rather than merely absent: "where did the CFG slider go" is the first thing
             anyone asks after a repoint, and the answer is a property of the checkpoint. -->
        {#each hiddenControls as key (key)}
          <p class="hint">{unavailableFor(ENGINE, key)}</p>
        {/each}
        <p class="hint locked-profile">
          {caps.upscaleChain
            ? 'Server-locked profile: max VRAM 9 GiB · imatrix dev-050 NVFP4 base · x2 spatial upscaler · VAE spatial tiles 2×2 · distilled sigma schedule.'
            : 'Server-locked profile: one distilled transformer · the checkpoint’s own sigma schedule, sampler and step count · single pass at the delivered size.'}
        </p>
      </details>
    </section>
  {:else if tab === 'lipdub'}
    <section class="card">
      <h3>Lip-dub</h3>
      <p class="hint">
        Attach a drive-audio track to a source video (or frames) and re-sync the mouths.
      </p>
      <div class="cap">Source video</div>
      <div class="dz">
        <FileDropzone bind:files={dubVideoFiles} accept="video/*" />
      </div>
      <div class="cap">
        Drive audio <span class="hint">— the new voice track</span>
      </div>
      <div class="dz">
        <FileDropzone bind:files={dubAudioFiles} accept="audio/*" />
      </div>
      <div class="cap">
        Mouth-style prompt <span class="hint">— optional</span>
      </div>
      <input type="text" placeholder="e.g. natural, subtle lip motion" bind:value={dubPrompt} />
      <p class="hint recipe">
        The source rides as a frozen full-length reference beside a shot sampled fresh, with the
        drive audio held fixed — the DubIt adapter is what makes the mouths follow it. Rendered in
        65-frame windows and stitched.
      </p>
      <details class="advanced">
        <summary>Advanced Lip-dub controls</summary>
        <p class="hint">
          Two-stage puts the latent upsampler between two samplers — but guides are cropped before
          it, so the frozen source holds stage 1 only and the refine runs with nothing keeping the
          face. Raising A2V above 1 costs a second forward pass per step and can alter the whole
          image. LoRA strength 1 is the adapter's trained scale, which is where it was trained on
          2.3 rather than a figure measured here.
        </p>
        <div class="knobs advanced-knobs">
          <label
            >A2V guidance
            <input type="number" step="0.5" min="1" max="3" bind:value={dubGuidance} /></label
          >
          <label
            >LoRA strength
            <input
              type="number"
              step="0.05"
              min="-2"
              max="2"
              bind:value={dubLipdubStrength}
            /></label
          >
          {#if useSeed}
            <label>Seed <input type="number" bind:value={seed} /></label>
          {/if}
        </div>
        <div class="toggles">
          <label class="toggle"
            ><input type="checkbox" bind:checked={dubTwoStage} /> Two-stage (×2 latent upsampler + refine)</label
          >
          <label class="toggle"><input type="checkbox" bind:checked={useSeed} /> Fixed seed</label>
        </div>
        <p class="hint locked-profile">
          Server-locked profile: one distilled transformer + the DubIt adapter at the strength above
          · the checkpoint’s own sigma schedule, sampler and step count · the reference encoded at
          every frame.
        </p>
      </details>
    </section>
  {:else if tab === 'upscale'}
    <UpscalePanel
      project={upscaleProject}
      tier={upscaleTier}
      shot={upscaleShot}
      variant={upscaleVariant}
    />
  {/if}

  {#if tab === 'ltx25' || tab === 'lipdub'}
    <!-- 🔴 Never disabled and the verb never moves. A render already in flight is a reason to queue
         behind it, not to refuse; and the two things that WOULD make the request malformed — no
         segment prompt, no dub source — are reported by the handlers with the sentence that says
         which one is missing, which a greyed-out button cannot do. -->
    <div class="actions">
      {#if tab === 'lipdub'}
        <button class="btn-primary" onclick={relip}>Lip-dub</button>
      {:else}
        <button class="btn-primary" onclick={render}>Render</button>
      {/if}
      <QueueChip
        kind={tab === 'lipdub' ? 'ltx_relip' : 'ltx25'}
        bind:options={queueOpts}
        response={accepted}
      />
      {#if running}
        <button class="btn-sec" onclick={cancel}>Cancel</button>
      {:else if tab === 'ltx25' && resumable}
        <button class="btn-sec" onclick={resume}>Resume from last segment</button>
        <span class="muted"
          >A previous render stopped partway — continue from its banked segments.</span
        >
      {:else}
        <span class="muted">Render runs server-side; safe to navigate away or reload.</span>
      {/if}
    </div>
  {/if}

  {#if (tab === 'ltx25' || tab === 'lipdub') && running}
    <div class="card progress">
      <div class="bar" class:indeterminate={!expectedSec}>
        {#if expectedSec}
          <div class="fill" style="width:{Math.min(99, (elapsed / expectedSec) * 100)}%"></div>
        {/if}
      </div>
      <span class="ptext">
        {elapsed.toFixed(0)}s elapsed{#if expectedSec}
          · ~{expectedSec.toFixed(0)}s expected (last render){/if}
      </span>
    </div>
  {/if}

  {#if (tab === 'ltx25' || tab === 'lipdub') && job?.status === 'completed' && resultUrl}
    <section class="card result">
      <h3>Result</h3>
      <!-- svelte-ignore a11y_media_has_caption -->
      <video src={resultUrl} controls loop playsinline></video>
      <p class="meta">
        {job.segments} segments · rendered in {(job.render_ms / 1000).toFixed(1)}s
        {#if job.gate_wait_ms > 0}· {(job.gate_wait_ms / 1000).toFixed(1)}s gate wait{/if}
      </p>
      <!-- mp4, not webm: both tabs render on LTX-2.5, whose only audio-carrying save node writes
           mp4 (`LtxJob::clip_ext`). A `.webm` name on those bytes is what breaks the first
           `ffmpeg -c copy` that reads the file back. -->
      <a class="btn-sec" href={resultUrl} download={`ltx_${job.job_id}.mp4`}>Download</a>
    </section>
  {/if}

  {#if restoreNote && (tab === 'ltx25' || tab === 'lipdub')}
    <p class="restore-note">{restoreNote}</p>
  {/if}

  {#if tab === 'ltx25' || tab === 'lipdub'}
    <GenerationHistory
      service={LTX_VIDEO_HISTORY_SERVICE}
      refreshKey={historyKey}
      onRestore={restore}
    />
  {/if}
</div>

<style>
  .tabs {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 16px;
  }
  .tab {
    padding: 8px 16px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-2);
    color: var(--muted);
    font-size: 13px;
    cursor: pointer;
  }
  .tab.on {
    color: var(--accent);
    border-color: var(--accent);
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
  .restore-note {
    margin: 10px 0 0;
    padding: 8px 12px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-2);
    color: var(--muted);
    font-size: 13px;
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
  .advanced > .hint {
    margin-top: 10px;
  }
  .advanced-knobs {
    margin-top: 12px;
  }
  .lora-list {
    margin-top: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .lora-row {
    display: grid;
    grid-template-columns: minmax(180px, 1fr) 2fr auto;
    align-items: center;
    gap: 10px;
  }
  .lora-row .lora-name {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    /* File names are long and unbreakable; let them wrap rather than blow out the grid. */
    overflow-wrap: anywhere;
  }
  .lora-row input[type='range'] {
    width: 100%;
  }
  .lora-num {
    width: 72px;
  }
  @media (max-width: 768px) {
    .lora-row {
      grid-template-columns: 1fr auto;
    }
    .lora-row input[type='range'] {
      grid-column: 1 / -1;
    }
  }
  .stage-group {
    margin-top: 10px;
    padding: 8px 10px;
    border: 1px solid var(--ring);
    border-radius: 8px;
  }
  .stage-group .advanced-knobs {
    margin-top: 8px;
  }
  .stage-label {
    font-size: 12px;
    font-weight: 600;
    color: var(--ink);
  }
  .locked-profile {
    margin-top: 12px;
    line-height: 1.45;
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
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
  .dz {
    margin-top: 6px;
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
  .knobs label.wide {
    grid-column: 1 / -1;
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
