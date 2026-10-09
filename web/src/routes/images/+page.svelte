<script lang="ts">
  import {
    submitImageGenerate,
    getImageJob,
    getImageActive,
    imageJobResult,
    isEnqueued,
    imageSuggestPrompt,
    imageProps,
    cancelImageRender,
    cancelQueueJob,
    mattingHealth,
    mattingRemoveSingle,
    mediaUrl,
    fetchMediaBlob,
    JOB_POLL_MS,
    type ImageModel,
    type ImageGenerateRequest,
    type ImageGenerateResponse,
    type ImageJob,
    type ImageProps,
    type ImageBackgroundRemoval,
    type MattingRemoveRequest,
    type MattingHealth,
    type MattingBackend,
    type MattingBgMode
  } from '$lib/api/client';
  import { blockedSummary } from '$lib/queue/format';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import { assetRefFor } from '$lib/ltx/assetRef';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import Krea2Panel from '$lib/components/Krea2Panel.svelte';
  import type { Generation } from '$lib/types';
  import { str, num, bool, obj, reattachNote } from '$lib/restore';
  import { onDestroy, onMount } from 'svelte';

  // ── Tab: which image engine ────────────────────────────────────────────
  // FLUX.2 (base + edit) and Krea 2 (text→image only) are separate services on
  // separate gate budget lines; they share this page, not any state.
  type ImageTab = 'flux2' | 'krea2';
  let tab = $state<ImageTab>('flux2');

  let historyKey = $state(0);
  let restoreNote = $state('');

  // Core
  let brief = $state('');
  let prompt = $state('');
  let negativePrompt = $state('');
  let model = $state<ImageModel>('auto');

  // Image inputs
  let initFiles = $state<FileList | null>(null);
  let maskFiles = $state<FileList | null>(null);
  // Reference images are a repeatable list — each is its own dropzone slot plus
  // a trailing "add another" slot.
  let referenceFiles = $state<File[]>([]);

  let initFile = $derived(initFiles && initFiles.length ? initFiles[0] : null);
  let maskFile = $derived(maskFiles && maskFiles.length ? maskFiles[0] : null);
  let hasAnyImage = $derived(!!initFile || !!maskFile || referenceFiles.length > 0);

  // Basic knobs
  let width = $state(512);
  let height = $state(512);
  // Hard ceiling per side (koblem clamps server-side too); kept in sync via props.max_dim.
  let maxDim = $state(1024);
  let batch = $state(1);
  let useSeed = $state(false);
  let seed = $state(0);
  let strength = $state(0.7); // img2img denoise (shown only with an init image)

  // Advanced
  let showAdvanced = $state(false);
  let overrideSteps = $state(false);
  let steps = $state(50); // reactively synced to the selected model's default
  let overrideCfg = $state(false);
  let cfg = $state(4); // reactively synced to the selected model's default
  let sampler = $state('');
  let scheduler = $state('');
  let vaeTiling = $state(false);

  // Background removal — chained onto every rendered image. Default OFF; the
  // tweaks live in Advanced and mirror the Background Removal page.
  let removeBackground = $state(false);
  let bgMode = $state<MattingBgMode>('alpha');
  let bgColor = $state('#ffffff');
  let mattingBackend = $state<MattingBackend>('gpu');
  let sensitivity = $state(1.0);
  let maskBlur = $state(0);
  let maskOffset = $state(0);
  let refine = $state(false);
  let invert = $state(false);
  let overrideRes = $state(false);
  let processRes = $state(1024);
  let mattingAvailable = $state(true);
  // Index of the result image currently being re-cut (null = none in flight).
  let recutting = $state<number | null>(null);

  // Async UI state
  let running = $state(false);
  let suggesting = $state(false);
  let improving = $state(false);
  let promptInstruction = $state('');
  let error = $state('');
  let notice = $state('');
  let result = $state<ImageGenerateResponse | null>(null);
  let imageUrls = $state<string[]>([]);
  let cutoutUrls = $state<(string | null)[]>([]);
  let props = $state<ImageProps | null>(null);

  // The queued half of sync-if-fast: null whenever the render came back inline.
  let jobId = $state<string | null>(null);
  let jobState = $state<ImageJob['state'] | null>(null);
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;

  onDestroy(stopPoll);

  function startPoll() {
    stopPoll();
    pollTimer = setInterval(pollOnce, JOB_POLL_MS);
  }

  function stopPoll() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  /** Take over a render already in flight — a fresh 202, or one `/active` found after a reload. */
  function adoptJob(job: ImageJob) {
    jobId = job.id;
    jobState = job.state;
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    running = true;
    error = '';
    result = null;
    imageUrls = [];
    cutoutUrls = [];
    startPoll();
  }

  function adoptResult(resp: ImageGenerateResponse) {
    imageUrls = resp.images.map((img) => mediaUrl(img.url));
    cutoutUrls = resp.images.map((img) => (img.cutout_url ? mediaUrl(img.cutout_url) : null));
    result = resp;
    historyKey++;
    if (resp.background_removal_error) {
      notice = `Rendered, but background removal failed: ${resp.background_removal_error}`;
    }
  }

  async function pollOnce() {
    if (!jobId) return;
    let job: ImageJob;
    try {
      job = await getImageJob(jobId);
    } catch (e) {
      // A dropped poll is not a dropped render — the queue owns it. Keep polling; only a terminal
      // state ends this loop.
      console.warn('flux2 poll failed, will retry', e);
      return;
    }
    jobState = job.state;
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    if (job.state === 'queued' || job.state === 'rendering') return;

    stopPoll();
    running = false;
    jobId = null;
    queuedBecause = '';
    if (job.state === 'done') adoptResult(imageJobResult(job));
    else if (job.state === 'cancelled') error = 'render cancelled';
    else error = job.error || 'render failed';
  }

  onMount(async () => {
    // Deep-link straight to an engine: /images?tab=krea2
    const requestedTab = new URLSearchParams(window.location.search).get('tab');
    if (requestedTab === 'krea2' || requestedTab === 'flux2') tab = requestedTab;

    try {
      props = await imageProps();
      if (props.default_width) width = props.default_width;
      if (props.default_height) height = props.default_height;
      if (props.max_dim) maxDim = props.max_dim;
      // Seed sampler/scheduler selects with the first option when present.
      if (props.samplers?.length && !sampler) sampler = props.samplers[0];
      if (props.schedulers?.length && !scheduler) scheduler = props.schedulers[0];
      // Per-model step/cfg defaults are applied reactively (see defaultSteps /
      // defaultCfg + the sync effect below), so nothing to seed here.
    } catch {
      props = {
        healthy: false,
        samplers: [],
        schedulers: [],
        limits: {},
        supported_modes: [],
        variants: {},
        default_width: 512,
        default_height: 512
      };
    }
    // Probe the matting service so the default-on toggle reflects reality.
    try {
      const h: MattingHealth = await mattingHealth();
      mattingAvailable = !!h.configured;
      if (!mattingAvailable) removeBackground = false;
    } catch {
      mattingAvailable = false;
      removeBackground = false;
    }
    // Re-attach to a render already in flight, so a reload (or a second device) finds it.
    try {
      const { job } = await getImageActive();
      if (job) adoptJob(job);
    } catch {
      // Nothing to re-attach to, or the API is unreachable.
    }
  });

  // Snap a dimension to the nearest multiple of 16 (the engine requires ×16).
  function snap16(v: number): number {
    const n = Math.round(v / 16) * 16;
    return Math.min(maxDim, Math.max(16, n));
  }

  // Reference-image list management.
  function addReference(list: FileList) {
    if (!list || !list.length) return;
    referenceFiles = [...referenceFiles, ...Array.from(list)];
  }

  function removeReference(idx: number) {
    referenceFiles = referenceFiles.filter((_, i) => i !== idx);
  }

  function buildRequest(): ImageGenerateRequest {
    const req: ImageGenerateRequest = {
      prompt: prompt.trim(),
      model,
      width: snap16(width),
      height: snap16(height),
      batch
    };
    if (negativePrompt.trim()) req.negative_prompt = negativePrompt.trim();
    if (useSeed) req.seed = seed;
    if (initFile) req.strength = strength;
    if (showAdvanced) {
      if (overrideSteps) req.steps = steps;
      if (overrideCfg) req.cfg = cfg;
      if (sampler) req.sampler = sampler;
      if (scheduler) req.scheduler = scheduler;
      req.vae_tiling = vaeTiling;
    }
    if (removeBackground && mattingAvailable) {
      const bg: ImageBackgroundRemoval = { enabled: true, ...mattingKnobs() };
      req.remove_background = bg;
    }
    return req;
  }

  // Shared matting knobs — feeds both the chained remove_background and the
  // standalone "redo cutout" call. bg mode/colour always; the rest behind Advanced.
  function mattingKnobs(): MattingRemoveRequest {
    const k: MattingRemoveRequest = { bg_mode: bgMode };
    if (bgMode === 'color') k.bg_color = bgColor;
    if (showAdvanced) {
      k.backend = mattingBackend;
      k.sensitivity = sensitivity;
      k.mask_blur = maskBlur;
      k.mask_offset = maskOffset;
      k.refine = refine;
      k.invert = invert;
      if (overrideRes) k.process_res = processRes;
    }
    return k;
  }

  // Re-run background removal on one already-rendered image with the current
  // knobs — no FLUX re-render. Adds a cutout if there wasn't one, or replaces it.
  async function redoCutout(i: number) {
    if (!result || !mattingAvailable) return;
    const img = result.images[i];
    if (!img) return;
    recutting = i;
    error = '';
    try {
      const blob = await fetchMediaBlob(img.url);
      const file = new File([blob], `image-${img.seed}.png`, { type: blob.type || 'image/png' });
      const resp = await mattingRemoveSingle(file, mattingKnobs());
      const cut = resp.results[0]?.url;
      if (!cut) throw new Error('no cutout returned');
      cutoutUrls[i] = mediaUrl(cut);
    } catch (e) {
      error = e instanceof Error ? e.message : 'recut failed';
    } finally {
      recutting = null;
    }
  }

  // "Write my prompt" — expand the brief via the 9B, optionally steered by any
  // attached reference images. Mirrors music's expandIdea UX.
  async function writePrompt() {
    if (!brief.trim()) {
      error = 'type a brief first';
      return;
    }
    suggesting = true;
    error = '';
    notice = '';
    try {
      const refs = referenceFiles.length
        ? await Promise.all(referenceFiles.map((f) => assetRefFor(f)))
        : undefined;
      const s = await imageSuggestPrompt(brief.trim(), refs);
      prompt = s.prompt;
      if (s.negative_prompt) negativePrompt = s.negative_prompt;
      notice =
        'Wrote a prompt' +
        (s.negative_prompt ? ' + negative prompt' : '') +
        ' from your brief. Edit freely, then Generate.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'prompt suggestion failed';
    } finally {
      suggesting = false;
    }
  }

  // "Improve" — refine the current prompt in place under the same FLUX.2 rules,
  // optionally toward a note; a blank note is a general polish. The brief + any
  // reference images ride along as context.
  async function improvePrompt() {
    if (!prompt.trim()) return;
    improving = true;
    error = '';
    notice = '';
    try {
      const refs = referenceFiles.length
        ? await Promise.all(referenceFiles.map((f) => assetRefFor(f)))
        : undefined;
      const s = await imageSuggestPrompt(brief.trim(), refs, {
        source: prompt,
        instruction: promptInstruction.trim() || undefined
      });
      prompt = s.prompt;
      promptInstruction = '';
      notice = 'Improved the prompt. Edit freely, then Generate.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'prompt improve failed';
    } finally {
      improving = false;
    }
  }

  async function handleCancel() {
    try {
      // 🔴 Off the QUEUE when there is a row: the engine's own /cancel only reaches a render that
      // is already running, and one still waiting its turn is exactly the one worth dropping.
      // Without an id the render is inside the server's sync window, where only the engine knows.
      if (jobId) await cancelQueueJob(jobId);
      else await cancelImageRender();
      notice = 'Cancelled — the in-flight render was aborted.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'cancel failed';
    }
  }

  // Repopulate the form from a saved generation's request_meta. Uploaded init/
  // mask/reference images can't be re-attached — reattachNote flags that.
  function restore(g: Generation) {
    const m = g.request_meta;
    const p = str(m, 'prompt');
    if (p !== undefined) prompt = p;
    negativePrompt = str(m, 'negative_prompt') ?? '';
    const md = str(m, 'model');
    if (md !== undefined) model = md as ImageModel;
    const w = num(m, 'width');
    if (w !== undefined) width = w;
    const h = num(m, 'height');
    if (h !== undefined) height = h;
    const b = num(m, 'batch');
    if (b !== undefined) batch = b;
    const sd = num(m, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    const st = num(m, 'strength');
    if (st !== undefined) strength = st;
    const steps_ = num(m, 'steps');
    if (steps_ !== undefined) {
      overrideSteps = true;
      steps = steps_;
    }
    const cfg_ = num(m, 'cfg');
    if (cfg_ !== undefined) {
      overrideCfg = true;
      cfg = cfg_;
    }
    const samp = str(m, 'sampler');
    if (samp !== undefined) sampler = samp;
    const sched = str(m, 'scheduler');
    if (sched !== undefined) scheduler = sched;
    const vt = bool(m, 'vae_tiling');
    if (vt !== undefined) vaeTiling = vt;
    if (overrideSteps || overrideCfg || sampler || scheduler || vaeTiling) showAdvanced = true;

    const bg = obj(m, 'remove_background');
    if (bg) {
      removeBackground = bool(bg, 'enabled') ?? true;
      const bm = str(bg, 'bg_mode');
      if (bm !== undefined) bgMode = bm as MattingBgMode;
      const bc = str(bg, 'bg_color');
      if (bc !== undefined) bgColor = bc;
      const be = str(bg, 'backend');
      if (be !== undefined) mattingBackend = be as MattingBackend;
      const se = num(bg, 'sensitivity');
      if (se !== undefined) sensitivity = se;
      const mb = num(bg, 'mask_blur');
      if (mb !== undefined) maskBlur = mb;
      const mo = num(bg, 'mask_offset');
      if (mo !== undefined) maskOffset = mo;
      const rf = bool(bg, 'refine');
      if (rf !== undefined) refine = rf;
      const iv = bool(bg, 'invert');
      if (iv !== undefined) invert = iv;
      const pr = num(bg, 'process_res');
      if (pr !== undefined) {
        overrideRes = true;
        processRes = pr;
      }
    } else {
      removeBackground = false;
    }

    restoreNote = reattachNote(m);
    error = '';
    notice = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  async function handleGenerate() {
    if (!prompt.trim()) {
      error = 'a prompt is required — type one or use "Write my prompt"';
      return;
    }
    running = true;
    error = '';
    notice = '';
    imageUrls = [];
    cutoutUrls = [];
    result = null;
    jobState = null;
    accepted = null;
    queuedBecause = '';
    try {
      const req = buildRequest();
      // 🔴 SYNC-IF-FAST: a render that finished inside the server's window comes back whole and is
      // used exactly as it always was. Only the 202 starts a poll.
      const resp = await submitImageGenerate(
        req,
        { initImage: initFile, mask: maskFile, referenceImages: referenceFiles },
        queueOpts
      );
      if (isEnqueued(resp)) {
        accepted = resp;
        jobId = resp.job_id;
        jobState = 'queued';
        queuedBecause = resp.blocked_reason ? blockedSummary(resp.blocked_reason) : '';
        startPoll();
        return;
      }
      adoptResult(resp);
      running = false;
    } catch (e) {
      error = e instanceof Error ? e.message : 'render failed';
      running = false;
    }
  }

  // "auto" hint: edit when any image is attached, else base.
  let autoResolves = $derived(hasAnyImage ? 'Edit' : 'Base');

  // The variant the current selection will actually render with (auto → edit
  // when an image is attached, else base). Drives the per-model step/cfg
  // defaults the Override controls prefill and display.
  let resolvedVariant = $derived<'base' | 'edit'>(
    model === 'edit' || (model === 'auto' && hasAnyImage) ? 'edit' : 'base'
  );
  const FALLBACK_DEFAULTS = { base: { steps: 50, cfg: 4 }, edit: { steps: 4, cfg: 1 } };
  let defaultSteps = $derived(
    props?.variants?.[resolvedVariant]?.default_steps ?? FALLBACK_DEFAULTS[resolvedVariant].steps
  );
  let defaultCfg = $derived(
    props?.variants?.[resolvedVariant]?.default_cfg ?? FALLBACK_DEFAULTS[resolvedVariant].cfg
  );
  // Keep the (hidden-until-checked) Override inputs prefilled with the selected
  // model's default until the user opts to override — so toggling Override on
  // starts from the right value and switching model updates it live.
  $effect(() => {
    if (!overrideSteps) steps = defaultSteps;
  });
  $effect(() => {
    if (!overrideCfg) cfg = defaultCfg;
  });
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Images</h2>
    {#if tab === 'flux2' && props}
      <p class="muted">
        FLUX.2 ·
        {#if props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
            >engine unreachable</span
          >{/if}
      </p>
    {/if}
  </div>

  <div class="tabs">
    <button class="tab" class:on={tab === 'flux2'} onclick={() => (tab = 'flux2')}>FLUX.2</button>
    <button class="tab" class:on={tab === 'krea2'} onclick={() => (tab = 'krea2')}>Krea 2</button>
  </div>

  {#if tab === 'krea2'}
    <Krea2Panel />
  {:else}
    <!-- Prompt -->
    <div class="card">
      <label>
        Brief <span class="hint">— a casual one-liner; the model writes you a full prompt</span>
        <div class="idea-row">
          <input
            type="text"
            bind:value={brief}
            placeholder="e.g. a cozy cabin in a snowy forest at dusk"
            onkeydown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault();
                writePrompt();
              }
            }}
          />
          <button class="btn-sec" onclick={writePrompt} disabled={suggesting || !brief.trim()}>
            {suggesting ? 'Thinking…' : '✨ Write my prompt'}
          </button>
        </div>
      </label>

      <label>
        Prompt <span class="hint">— what to draw (required)</span>
        <textarea
          rows="3"
          bind:value={prompt}
          placeholder="e.g. a small wooden cabin, warm glowing windows, deep snow, pine trees, dusk light"
        ></textarea>
        {#if prompt.trim()}
          <div class="idea-row">
            <button
              type="button"
              class="btn-sec"
              onclick={improvePrompt}
              disabled={improving}
              title="Rewrite the current prompt under the same FLUX.2 rules, applying your note — keeps what works"
            >
              {improving ? 'Improving…' : '✨ Improve'}
            </button>
            <input
              type="text"
              bind:value={promptInstruction}
              placeholder="what to change? (blank = general polish)"
              onkeydown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  improvePrompt();
                }
              }}
            />
          </div>
        {/if}
      </label>

      <label>
        Negative prompt <span class="hint">— what to avoid (optional)</span>
        <input
          type="text"
          bind:value={negativePrompt}
          placeholder="e.g. blurry, low quality, text, watermark"
        />
      </label>

      <label>
        Model
        <select bind:value={model}>
          <option value="auto">Auto</option>
          <option value="base">Base (text → image)</option>
          <option value="edit">Edit (img2img / Kontext)</option>
        </select>
        {#if model === 'auto'}
          <span class="hint"
            >Auto picks <strong>Edit</strong> when you attach a reference / init / mask, else
            <strong>Base</strong>. Currently → <strong>{autoResolves}</strong>.</span
          >
        {/if}
      </label>
    </div>

    <!-- Image inputs -->
    <div class="card">
      <h3>
        Image inputs <span class="hint">(optional — attaching any switches Auto → Edit)</span>
      </h3>

      <div class="img-input">
        <FileDropzone
          accept="image/*"
          bind:files={initFiles}
          label={initFile ? initFile.name : 'Init image (img2img)'}
          hint="start from this image instead of noise"
        />
        {#if initFile}
          <label>
            Strength <span class="muted">{strength.toFixed(2)}</span>
            <input type="range" min="0.1" max="1" step="0.05" bind:value={strength} />
            <span class="hint"
              >low = stays close to the init image · high = freer reinterpretation</span
            >
          </label>
        {/if}
      </div>

      <div class="img-input">
        <FileDropzone
          accept="image/*"
          bind:files={maskFiles}
          label={maskFile ? maskFile.name : 'Mask (inpaint)'}
          hint="white = repaint, black = keep — pairs with an init image"
        />
      </div>

      <div class="img-input">
        <span class="writer-label"
          >Reference images <span class="hint">— multi-ref Kontext: "make an image like these"</span
          ></span
        >
        {#each referenceFiles as ref, i (ref.name + i)}
          <div class="ref-row">
            <span class="ref-name" title={ref.name}>{ref.name}</span>
            <button class="link-btn" type="button" onclick={() => removeReference(i)}>Remove</button
            >
          </div>
        {/each}
        <FileDropzone
          accept="image/*"
          multiple
          label={referenceFiles.length ? 'Add more reference images' : 'Add reference images'}
          hint="drop one or several — they steer style/subject"
          onFiles={addReference}
        />
      </div>
    </div>

    <!-- Basic knobs -->
    <div class="card">
      <h3>Settings</h3>
      <div class="knobs">
        <label>
          Width <span class="hint">(×16)</span>
          <input
            type="number"
            min="16"
            step="16"
            max={maxDim}
            bind:value={width}
            onchange={() => (width = snap16(width))}
          />
        </label>
        <label>
          Height <span class="hint">(×16)</span>
          <input
            type="number"
            min="16"
            step="16"
            max={maxDim}
            bind:value={height}
            onchange={() => (height = snap16(height))}
          />
        </label>
        <label>
          Batch <span class="hint">(images / seeds)</span>
          <input type="number" min="1" max="8" bind:value={batch} />
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={useSeed} /> Fixed seed
        </label>
        {#if useSeed}
          <label>
            Seed <span class="hint">(blank-equivalent = random when off)</span>
            <input type="number" min="0" bind:value={seed} />
          </label>
        {/if}
        <label
          class="toggle"
          title={mattingAvailable
            ? 'Run background removal on every rendered image'
            : 'Background-removal engine not configured'}
        >
          <input type="checkbox" bind:checked={removeBackground} disabled={!mattingAvailable} />
          Remove background
          <span class="hint"
            >{mattingAvailable
              ? '(also returns a transparent cutout)'
              : '(engine unavailable)'}</span
          >
        </label>
      </div>

      <details bind:open={showAdvanced} class="advanced">
        <summary>Advanced</summary>
        <div class="knobs">
          <label class="toggle">
            <input type="checkbox" bind:checked={overrideSteps} /> Override steps
            <span class="hint">(default {defaultSteps})</span>
          </label>
          {#if overrideSteps}
            <label>Steps<input type="number" min="1" max="100" bind:value={steps} /></label>
          {/if}
          <label class="toggle">
            <input type="checkbox" bind:checked={overrideCfg} /> Override CFG
            <span class="hint">(default {defaultCfg})</span>
          </label>
          {#if overrideCfg}
            <label>CFG<input type="number" min="0" max="20" step="0.1" bind:value={cfg} /></label>
          {/if}
          {#if props && props.samplers.length}
            <label>
              Sampler
              <select bind:value={sampler}>
                {#each props.samplers as s (s)}<option value={s}>{s}</option>{/each}
              </select>
            </label>
          {/if}
          {#if props && props.schedulers.length}
            <label>
              Scheduler
              <select bind:value={scheduler}>
                {#each props.schedulers as s (s)}<option value={s}>{s}</option>{/each}
              </select>
            </label>
          {/if}
          <label class="toggle">
            <input type="checkbox" bind:checked={vaeTiling} /> VAE tiling
            <span class="hint">(lower VRAM, slightly slower)</span>
          </label>
        </div>

        {#if removeBackground && mattingAvailable}
          <div class="subhead">Background removal</div>
          <div class="knobs">
            <label>
              Background
              <select bind:value={bgMode}>
                <option value="alpha">Transparent (alpha cutout)</option>
                <option value="color">Solid colour</option>
              </select>
            </label>
            {#if bgMode === 'color'}
              <label>
                Colour
                <input type="color" bind:value={bgColor} />
              </label>
            {/if}
            <label>
              Backend
              <select bind:value={mattingBackend}>
                <option value="gpu">GPU</option>
                <option value="cpu">CPU</option>
              </select>
            </label>
            <label>
              Sensitivity <span class="muted">{sensitivity.toFixed(2)}</span>
              <input type="range" min="0" max="1" step="0.05" bind:value={sensitivity} />
              <span class="hint">mask strength / threshold</span>
            </label>
            <label>
              Mask blur <span class="hint">(px)</span>
              <input type="number" min="0" max="64" bind:value={maskBlur} />
            </label>
            <label>
              Mask offset <span class="hint">(px: dilate + / erode −)</span>
              <input type="number" min="-64" max="64" bind:value={maskOffset} />
            </label>
            <label class="toggle">
              <input type="checkbox" bind:checked={refine} /> Refine edges
              <span class="hint">(edge decontamination)</span>
            </label>
            <label class="toggle">
              <input type="checkbox" bind:checked={invert} /> Invert mask
            </label>
            <label class="toggle">
              <input type="checkbox" bind:checked={overrideRes} /> Override resolution
            </label>
            {#if overrideRes}
              <label>
                Process res <span class="hint">(256–2048; native 1024)</span>
                <input type="number" min="256" max="2048" step="64" bind:value={processRes} />
              </label>
            {/if}
          </div>
        {/if}
      </details>
    </div>

    <div class="actions">
      <button class="btn-primary" onclick={handleGenerate} disabled={running || suggesting}>
        {running ? 'Rendering…' : 'Generate'}
      </button>
      <QueueChip kind="flux2" bind:options={queueOpts} response={accepted} />
      {#if running}
        <button class="btn-sec" onclick={handleCancel} title="Abort the in-flight render"
          >Cancel</button
        >
        {#if jobState === 'queued'}
          <span class="muted">Queued{queuedBecause ? ` — ${queuedBecause}` : ''}</span>
        {:else if jobId}
          <span class="muted">Rendering — safe to navigate away or reload.</span>
        {:else}
          <span class="muted">GPU held for the render; yields to other heavy services.</span>
        {/if}
      {/if}
      {#if notice && !error}<span class="muted">{notice}</span>{/if}
      {#if error}<span class="error">{error}</span>{/if}
    </div>

    {#if restoreNote}
      <p class="restore-note">{restoreNote}</p>
    {/if}

    {#if result}
      <div class="card">
        <h3>Result</h3>
        <div class="meta">
          <span>{result.model_used} model</span>
          <span>· {result.width}×{result.height}</span>
          <span>· {result.steps} steps</span>
          <span>· cfg {result.cfg}</span>
          <span>· render {(result.render_ms / 1000).toFixed(1)}s</span>
          <span>· gate wait {(result.gate_wait_ms / 1000).toFixed(1)}s</span>
          {#if result.background_removal_seconds != null}
            <span
              >· bg-removal {result.background_removal_seconds.toFixed(
                2
              )}s{#if result.background_removal_backend}
                ({result.background_removal_backend}){/if}</span
            >
          {/if}
        </div>
        <div class="gallery">
          {#each result.images as img, i (img.seed + '-' + i)}
            <div class="shot">
              {#if imageUrls[i]}
                <img src={imageUrls[i]} alt={`Generated image ${i + 1}`} />
              {/if}
              <div class="shot-foot">
                <span class="muted">seed {img.seed}</span>
                {#if mattingAvailable}
                  <button
                    class="btn-sec"
                    onclick={() => redoCutout(i)}
                    disabled={recutting !== null}
                    title="Re-run background removal on this image with the current knobs (no re-render)"
                  >
                    {recutting === i ? 'Cutting…' : cutoutUrls[i] ? 'Redo cutout' : 'Remove bg'}
                  </button>
                {/if}
                {#if imageUrls[i]}
                  <a class="btn-sec" href={imageUrls[i]} download={`image-${img.seed}.png`}
                    >Download</a
                  >
                {/if}
              </div>
              {#if cutoutUrls[i]}
                <div class="cutout">
                  <img src={cutoutUrls[i]} alt={`Cutout ${i + 1}`} />
                </div>
                <div class="shot-foot">
                  <span class="muted">background removed</span>
                  <a class="btn-sec" href={cutoutUrls[i]} download={`image-${img.seed}-cutout.png`}
                    >Download cutout</a
                  >
                </div>
              {/if}
            </div>
          {/each}
        </div>
      </div>
    {/if}

    <GenerationHistory
      service="images"
      refreshKey={historyKey}
      title="Image history"
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
  .idea-row {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-top: 4px;
  }
  .idea-row input {
    flex: 1;
  }
  .idea-row .btn-sec {
    white-space: nowrap;
  }
  .img-input {
    margin-top: 12px;
  }
  .img-input:first-of-type {
    margin-top: 0;
  }
  .writer-label {
    display: block;
    font-size: 12px;
    color: var(--muted);
    margin-bottom: 6px;
  }
  .ref-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 10px;
    margin-bottom: 6px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    font-size: 12px;
    color: var(--ink);
  }
  .ref-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .link-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    font-size: 12px;
    text-decoration: underline;
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
  .subhead {
    margin: 14px 0 8px;
    font-size: 12px;
    font-weight: 600;
    color: var(--ink);
    border-top: 1px solid var(--ring);
    padding-top: 12px;
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
  .btn-primary {
    background: var(--accent);
    color: white;
    border: none;
    padding: 12px 28px;
    border-radius: 10px;
    font-size: 15px;
    font-weight: 600;
    cursor: pointer;
  }
  .btn-primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .btn-sec {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
    padding: 8px 12px;
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
    text-decoration: none;
  }
  .btn-sec:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
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
  .meta {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
    margin-bottom: 12px;
    flex-wrap: wrap;
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
  }
  .gallery {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 14px;
  }
  .shot {
    border: 1px solid var(--ring);
    border-radius: 12px;
    overflow: hidden;
    background: var(--bg-2);
  }
  .shot img {
    display: block;
    width: 100%;
    height: auto;
  }
  .cutout {
    border-top: 1px solid var(--ring);
    /* checkerboard so transparent cutouts read clearly */
    background-color: var(--bg-2);
    background-image:
      linear-gradient(45deg, rgba(128, 128, 128, 0.25) 25%, transparent 25%),
      linear-gradient(-45deg, rgba(128, 128, 128, 0.25) 25%, transparent 25%),
      linear-gradient(45deg, transparent 75%, rgba(128, 128, 128, 0.25) 75%),
      linear-gradient(-45deg, transparent 75%, rgba(128, 128, 128, 0.25) 75%);
    background-size: 20px 20px;
    background-position:
      0 0,
      0 10px,
      10px -10px,
      -10px 0;
  }
  .cutout img {
    display: block;
    width: 100%;
    height: auto;
  }
  .shot-foot {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 10px;
  }
  .shot-foot .btn-sec {
    margin-left: auto;
  }
</style>
