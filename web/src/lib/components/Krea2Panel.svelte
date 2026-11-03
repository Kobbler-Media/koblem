<script lang="ts">
  /**
   * Krea-2-Turbo tab on the Images page.
   *
   * Same shape and styling as the FLUX.2 tab. Krea 2 has no first-party edit
   * MODEL, but the engine serves instruction-based, identity-preserving edits
   * through the `krea2-identity-edit` LoRA plus its `krea2_identity_edit`
   * reference-image preset — so `mode` here plays the part FLUX.2's `model`
   * select plays: **auto** resolves to **edit** as soon as a reference image is
   * attached. There is still no init image and no mask: editing is
   * reference-conditioned, not denoise-from-source.
   *
   * The reference images are TWO LABELLED, ORDERED SLOTS rather than FLUX.2's
   * anonymous list, because the order is part of the recipe the LoRA was trained
   * on — **1 = scene, 2 = person** — and swapping them sharply degrades the
   * result.
   *
   * Everything else — resolution, steps, cfg, seed, sampler/scheduler, negative
   * prompt, batch, VAE tiling, chained background removal — matches FLUX.2's
   * knobs. Someone who never touches the Mode select gets exactly the
   * text-to-image panel that shipped: no `mode` field is sent while it reads
   * Auto with nothing attached, and no multipart upload happens.
   */
  import {
    krea2Generate,
    getKrea2Job,
    getKrea2Active,
    krea2JobResult,
    isEnqueued,
    krea2SuggestPrompt,
    krea2Props,
    cancelKrea2Render,
    cancelQueueJob,
    mattingHealth,
    mattingRemoveSingle,
    mediaUrl,
    fetchMediaBlob,
    JOB_POLL_MS,
    type Krea2GenerateRequest,
    type Krea2GenerateResponse,
    type Krea2Job,
    type Krea2Props,
    type Krea2Mode,
    type ImageBackgroundRemoval,
    type MattingRemoveRequest,
    type MattingHealth,
    type MattingBackend,
    type MattingBgMode
  } from '#lib/api/client.ts';
  import { blockedSummary } from '#lib/queue/format.ts';
  import type { EnqueueLike, QueueSubmitOptions } from '#lib/queue/options.ts';
  import QueueChip from '#lib/components/queue/QueueChip.svelte';
  import FileDropzone from '#lib/components/FileDropzone.svelte';
  import GenerationHistory from '#lib/components/GenerationHistory.svelte';
  import type { Generation } from '#lib/types.ts';
  import { str, num, bool, obj } from '#lib/restore.ts';
  import { onDestroy, onMount } from 'svelte';

  let historyKey = $state(0);
  let restoreNote = $state('');

  // Core
  let brief = $state('');
  let prompt = $state('');
  let negativePrompt = $state('');

  // ── Mode ────────────────────────────────────────────────────────────────
  // `auto` (the default) resolves to `edit` iff a reference image is attached,
  // exactly as FLUX.2's `auto` model resolves to Edit. Explicit `t2i` keeps
  // text-to-image even with an image attached; explicit `edit` without one is a
  // 400 server-side, so it is caught here first.
  let mode = $state<Krea2Mode>('auto');

  // ── Reference images: TWO ORDERED, LABELLED SLOTS ───────────────────────
  // Not an anonymous list. The order is semantic — scene is reference 1, person
  // is reference 2 — because that is the layout the identity-edit LoRA was
  // trained on; swapping them sharply degrades the result. Held as FileLists so
  // each dropzone's own "has file" state clears when the slot is emptied.
  let sceneFiles = $state<FileList | null>(null);
  let personFiles = $state<FileList | null>(null);
  let sceneFile = $derived(sceneFiles?.length ? sceneFiles[0] : null);
  let personFile = $derived(personFiles?.length ? personFiles[0] : null);
  // Slot order, blanks skipped — this array IS the wire order.
  let referenceFiles = $derived([sceneFile, personFile].filter((f): f is File => !!f));

  type Dims = { w: number; h: number };
  let sceneDims = $state<Dims | null>(null);
  let personDims = $state<Dims | null>(null);
  // The reference the engine centre-crops to the requested AR is the first one,
  // so "match source" and the AR warning read from whichever slot leads.
  let primaryDims = $derived(sceneFile ? sceneDims : personDims);

  // Basic knobs. 1024² is Krea 2's default and the bottom of its supported range.
  let width = $state(1024);
  let height = $state(1024);
  let maxDim = $state(2048);
  let recommendedMinDim = $state(1024);
  let batch = $state(1);
  let useSeed = $state(false);
  let seed = $state(0);

  // Advanced. Krea-2-Turbo is the DISTILLED checkpoint: 8 steps, no CFG (the
  // model card's guidance_scale 0.0 is cfg_scale 1.0 in this engine).
  const FALLBACK_STEPS = 8;
  const FALLBACK_CFG = 1;
  const FALLBACK_SCHEDULER = 'discrete';
  // Edit-mode prefills, used only until /props answers: 8 steps favours
  // composition adherence, 12 favours face detail, ~10 balances; cfg stays 1
  // because the DiT is still the distilled one.
  const FALLBACK_EDIT_STEPS = 10;
  const FALLBACK_EDIT_CFG = 1;
  const FALLBACK_GROUNDING = 768;
  const FALLBACK_GROUNDING_MIN = 384;
  const FALLBACK_GROUNDING_MAX = 1024;
  // Above ~2 MP the engine starts duplicating / bleeding the reference.
  const MAX_PIXELS = 2 * 1024 * 1024;
  let showAdvanced = $state(false);
  let overrideSteps = $state(false);
  let steps = $state(FALLBACK_STEPS);
  let overrideCfg = $state(false);
  let cfg = $state(FALLBACK_CFG);
  let sampler = $state('');
  let scheduler = $state('');
  // VAE tiling is ON server-side (the krea2 image starts with --vae-tiling),
  // which is what keeps a 2048² decode from OOMing. So it is an OVERRIDE, not a
  // plain toggle: unchecked = don't send the field at all.
  let overrideVaeTiling = $state(false);
  let vaeTiling = $state(true);
  // Edit only. Unset (= not overridden) leaves the engine preset's own trained
  // grounding size alone rather than restating it.
  let overrideGrounding = $state(false);
  let grounding = $state(FALLBACK_GROUNDING);

  // Background removal — chained onto every rendered image, same as FLUX.2.
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
  let recutting = $state<number | null>(null);

  // Async UI state
  let running = $state(false);
  let suggesting = $state(false);
  let improving = $state(false);
  let promptInstruction = $state('');
  let error = $state('');
  let notice = $state('');
  let result = $state<Krea2GenerateResponse | null>(null);
  let imageUrls = $state<string[]>([]);
  let cutoutUrls = $state<(string | null)[]>([]);
  let props = $state<Krea2Props | null>(null);

  // The queued half of sync-if-fast: null whenever the render came back inline.
  let jobId = $state<string | null>(null);
  let jobState = $state<Krea2Job['state'] | null>(null);
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
  function adoptJob(job: Krea2Job) {
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

  function adoptResult(resp: Krea2GenerateResponse) {
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
    let job: Krea2Job;
    try {
      job = await getKrea2Job(jobId);
    } catch (e) {
      // A dropped poll is not a dropped render — the queue owns it. Keep polling; only a terminal
      // state ends this loop.
      console.warn('krea2 poll failed, will retry', e);
      return;
    }
    jobState = job.state;
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    if (job.state === 'queued' || job.state === 'rendering') return;

    stopPoll();
    running = false;
    jobId = null;
    queuedBecause = '';
    if (job.state === 'done') adoptResult(krea2JobResult(job));
    else if (job.state === 'cancelled') error = 'render cancelled';
    else error = job.error || 'render failed';
  }

  onMount(async () => {
    try {
      props = await krea2Props();
      if (props.default_width) width = props.default_width;
      if (props.default_height) height = props.default_height;
      if (props.max_dim) maxDim = props.max_dim;
      if (props.recommended_min_dim) recommendedMinDim = props.recommended_min_dim;
      if (props.samplers?.length && !sampler) sampler = props.samplers[0];
      // Prefer the engine's own scheduler if it offers the one we default to;
      // otherwise fall back to its first option rather than sending a name it
      // doesn't know.
      if (!scheduler) {
        const want = props.defaults?.default_scheduler ?? FALLBACK_SCHEDULER;
        scheduler = props.schedulers?.includes(want) ? want : (props.schedulers?.[0] ?? '');
      }
    } catch {
      props = {
        healthy: false,
        configured: false,
        samplers: [],
        schedulers: [],
        limits: {},
        defaults: {}
      };
    }
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
      const { job } = await getKrea2Active();
      if (job) adoptJob(job);
    } catch {
      // Nothing to re-attach to, or the API is unreachable.
    }
  });

  // Snap a dimension to the nearest multiple of 16 (VAE 8× downsample × DiT
  // patch size 2). The server snaps too — this just keeps the input honest.
  function snap16(v: number): number {
    const n = Math.round(v / 16) * 16;
    return Math.min(maxDim, Math.max(16, n));
  }

  // ── Mode resolution ─────────────────────────────────────────────────────
  // `edit_available` is false when the identity-edit LoRA isn't in the engine's
  // resolvable `loras[]`. The engine answers an unresolvable LoRA with a bare 400
  // and nothing in its log, so edit mode is disabled outright rather than left
  // hopefully clickable.
  let editAvailable = $derived(props?.edit_available === true);
  const EDIT_UNAVAILABLE_TIP =
    'Edit mode needs the identity-edit LoRA loaded on the krea2 engine. ' +
    "It isn't in the engine's resolvable LoRA list, so an edit render would " +
    'fail with an unexplained 400.';

  // PURE GEOMETRY, and deliberately independent of the mode: it is what DECIDES
  // the mode, so it cannot be derived from it. Kept above `resolvedMode` because
  // `arMismatch` further down reads the resolved mode and would otherwise close a
  // cycle. Same 2% tolerance as the server's AR_KEEP_TOLERANCE.
  let refArMismatch = $derived(
    primaryDims != null &&
      Math.abs(snap16(width) / snap16(height) - primaryDims.w / primaryDims.h) /
        (primaryDims.w / primaryDims.h) >
        0.02
  );

  // Mirrors the server's resolve_mode + plan_geometry. `auto` lands on Edit when a
  // reference is attached, EXCEPT when the requested canvas is a different aspect
  // ratio from the reference — that request is a restage said differently, and
  // cropping the reference to fit would throw part of it away. The server makes the
  // same call; this only has to agree so the prefills and the label are honest.
  let resolvedMode = $derived<'t2i' | 'edit' | 'restage'>(
    !editAvailable
      ? 't2i'
      : mode === 'restage'
        ? 'restage'
        : mode === 'edit'
          ? 'edit'
          : mode === 'auto' && referenceFiles.length > 0
            ? refArMismatch
              ? 'restage'
              : 'edit'
            : 't2i'
  );
  let resolvedLabel = $derived(
    resolvedMode === 'edit' ? 'Edit' : resolvedMode === 'restage' ? 'Restage' : 'Text → image'
  );

  // Per-mode prefills, read from /props (top level, or nested under `defaults`)
  // rather than hardcoded — the server owns these numbers.
  let modeVariant = $derived(
    props?.variants?.[resolvedMode] ?? props?.defaults?.variants?.[resolvedMode]
  );
  let editVariant = $derived(props?.variants?.edit ?? props?.defaults?.variants?.edit);
  let defaultSteps = $derived(
    modeVariant?.default_steps ??
      (resolvedMode !== 't2i'
        ? FALLBACK_EDIT_STEPS
        : (props?.defaults?.default_steps ?? FALLBACK_STEPS))
  );
  let defaultCfg = $derived(
    modeVariant?.default_cfg ??
      (resolvedMode !== 't2i' ? FALLBACK_EDIT_CFG : (props?.defaults?.default_cfg ?? FALLBACK_CFG))
  );
  let defaultGrounding = $derived(editVariant?.default_grounding_px ?? FALLBACK_GROUNDING);
  let groundingMin = $derived(editVariant?.min_grounding_px ?? FALLBACK_GROUNDING_MIN);
  let groundingMax = $derived(editVariant?.max_grounding_px ?? FALLBACK_GROUNDING_MAX);

  // Keep the (hidden-until-checked) Override inputs prefilled with the resolved
  // mode's default until the user opts to override — so switching mode updates
  // them live, and ticking Override starts from the right value.
  $effect(() => {
    if (!overrideSteps) steps = defaultSteps;
  });
  $effect(() => {
    if (!overrideCfg) cfg = defaultCfg;
  });
  $effect(() => {
    if (!overrideGrounding) grounding = defaultGrounding;
  });

  // ── Reference geometry ──────────────────────────────────────────────────

  function readImageDims(file: File): Promise<Dims> {
    return new Promise((resolve, reject) => {
      const url = URL.createObjectURL(file);
      const img = new Image();
      img.onload = () => {
        URL.revokeObjectURL(url);
        resolve({ w: img.naturalWidth, h: img.naturalHeight });
      };
      img.onerror = () => {
        URL.revokeObjectURL(url);
        reject(new Error('could not read image'));
      };
      img.src = url;
    });
  }

  // Read a slot's pixel dimensions, ignoring a result that lands after the slot
  // changed again.
  function trackDims(file: File | null, set: (d: Dims | null) => void) {
    if (!file) {
      set(null);
      return;
    }
    let live = true;
    readImageDims(file)
      .then((d) => {
        if (live) set(d);
      })
      .catch(() => {
        if (live) set(null);
      });
    return () => {
      live = false;
    };
  }
  $effect(() => trackDims(sceneFile, (d) => (sceneDims = d)));
  $effect(() => trackDims(personFile, (d) => (personDims = d)));

  // Largest ×16 box at (approximately) the source aspect ratio that stays inside
  // maxDim per side and ~2 MP. Both bounds matter: the engine centre-crops the
  // reference to whatever AR is requested, and above 2 MP the source duplicates.
  // Prefers accuracy over raw size — it walks down until the AR error is under
  // 0.5 %, so it returns "the source's shape" rather than "nearly the source's
  // shape, but as big as possible".
  function fitToAspect(sw: number, sh: number): Dims {
    const ar = sw / sh;
    let best: { w: number; h: number; err: number } | null = null;
    for (let w = Math.floor(maxDim / 16) * 16; w >= 256; w -= 16) {
      const h = Math.min(maxDim, Math.max(16, Math.round(w / ar / 16) * 16));
      if (w * h > MAX_PIXELS) continue;
      const err = Math.abs(w / h - ar) / ar;
      if (best && err >= best.err) continue;
      best = { w, h, err };
      if (err <= 0.005) break;
    }
    return best ? { w: best.w, h: best.h } : { w: snap16(sw), h: snap16(sh) };
  }

  // What "Match source aspect" would set — shown next to the button so the
  // result is visible before clicking.
  let suggestedFit = $derived(primaryDims ? fitToAspect(primaryDims.w, primaryDims.h) : null);

  function matchSource() {
    if (!suggestedFit) return;
    width = suggestedFit.w;
    height = suggestedFit.h;
  }

  function swapReferences() {
    const s = sceneFiles;
    sceneFiles = personFiles;
    personFiles = s;
  }

  let outputPixels = $derived(snap16(width) * snap16(height));
  // Only worth warning about in EDIT mode now. In edit mode the reference is the
  // canvas, so a mismatched request means the server refits the canvas to the
  // reference's shape (it will not crop the reference); in restage mode a mismatch
  // is the normal case and costs nothing, because the reference is not cropped at
  // all. It stays a warning rather than a silent fix only because the render then
  // comes back at dimensions the user did not type.
  let arMismatch = $derived(resolvedMode === 'edit' && refArMismatch);
  let overTwoMp = $derived(resolvedMode !== 't2i' && outputPixels > MAX_PIXELS);

  // Below the model's supported range the output degrades; warn rather than clamp.
  let belowRange = $derived(
    snap16(width) < recommendedMinDim || snap16(height) < recommendedMinDim
  );
  // At cfg 1 there is no classifier-free guidance, so a negative prompt does
  // nothing. Say so instead of letting it look like it was applied.
  let negativeIsInert = $derived(!!negativePrompt.trim() && (overrideCfg ? cfg : defaultCfg) <= 1);

  function buildRequest(): Krea2GenerateRequest {
    const req: Krea2GenerateRequest = {
      prompt: prompt.trim(),
      width: snap16(width),
      height: snap16(height),
      batch
    };
    // Omitted while the select reads Auto, so the t2i body stays exactly the one
    // that shipped.
    if (mode !== 'auto') req.mode = mode;
    if (negativePrompt.trim()) req.negative_prompt = negativePrompt.trim();
    if (useSeed) req.seed = seed;
    if (showAdvanced) {
      if (overrideSteps) req.steps = steps;
      if (overrideCfg) req.cfg = cfg;
      if (sampler) req.sampler = sampler;
      if (scheduler) req.scheduler = scheduler;
      // Omitted unless overridden — see the `overrideVaeTiling` comment above.
      if (overrideVaeTiling) req.vae_tiling = vaeTiling;
      // Edit only, and only when overridden: unset keeps the engine preset's own
      // trained grounding size.
      if (resolvedMode !== 't2i' && overrideGrounding) req.grounding_px = grounding;
    }
    if (removeBackground && mattingAvailable) {
      const bg: ImageBackgroundRemoval = { enabled: true, ...mattingKnobs() };
      req.remove_background = bg;
    }
    return req;
  }

  // Shared matting knobs — feeds both the chained remove_background and the
  // standalone "redo cutout" call.
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
  // knobs — no re-render.
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

  // "Write my prompt" — expand the brief into a Krea 2 caption-style paragraph.
  async function writePrompt() {
    if (!brief.trim()) {
      error = 'type a brief first';
      return;
    }
    suggesting = true;
    error = '';
    notice = '';
    try {
      const s = await krea2SuggestPrompt(brief.trim());
      prompt = s.prompt;
      notice = 'Wrote a prompt from your brief. Edit freely, then Generate.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'prompt suggestion failed';
    } finally {
      suggesting = false;
    }
  }

  // "Improve" — refine the current prompt in place under the same Krea 2 rules,
  // optionally toward a note; a blank note is a general polish.
  async function improvePrompt() {
    if (!prompt.trim()) return;
    improving = true;
    error = '';
    notice = '';
    try {
      const s = await krea2SuggestPrompt(brief.trim(), {
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
      else await cancelKrea2Render();
      notice = 'Cancelled — the in-flight render was aborted.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'cancel failed';
    }
  }

  async function handleGenerate() {
    if (!prompt.trim()) {
      error = 'a prompt is required — type one or use "Write my prompt"';
      return;
    }
    if ((mode === 'edit' || mode === 'restage') && referenceFiles.length === 0) {
      error = `${mode === 'edit' ? 'Edit' : 'Restage'} mode needs a reference image — attach one, or set Mode to Auto / Text → image`;
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
      // Files are attached ONLY in edit mode, so a t2i render is still a plain
      // JSON POST — no pointless multipart upload of images the server would
      // ignore.
      //
      // 🔴 SYNC-IF-FAST: a render that finished inside the server's window comes back whole and is
      // used exactly as it always was. Only the 202 starts a poll.
      const resp = await krea2Generate(
        buildRequest(),
        resolvedMode !== 't2i' ? { referenceImages: referenceFiles } : undefined,
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

  // Repopulate the form from a saved generation's request_meta. Uploaded
  // reference images can't be re-attached, so an edit render's restore says so.
  // (`had_reference_images` is a COUNT, not the boolean `reattachNote` reads —
  // hence the local note.)
  function restore(g: Generation) {
    const m = g.request_meta;
    const p = str(m, 'prompt');
    if (p !== undefined) prompt = p;
    negativePrompt = str(m, 'negative_prompt') ?? '';
    const md = str(m, 'mode');
    if (md === 'auto' || md === 't2i' || md === 'edit' || md === 'restage') mode = md;
    const gp = num(m, 'grounding_px');
    if (gp !== undefined) {
      overrideGrounding = true;
      grounding = gp;
    }
    const hadRefs = num(m, 'had_reference_images') ?? 0;
    restoreNote = hadRefs
      ? `Settings restored — re-attach the reference image${hadRefs > 1 ? 's (scene first, person second)' : ''} before rendering.`
      : '';
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
    if (vt !== undefined) {
      overrideVaeTiling = true;
      vaeTiling = vt;
    }
    if (
      overrideSteps ||
      overrideCfg ||
      sampler ||
      scheduler ||
      overrideVaeTiling ||
      overrideGrounding
    )
      showAdvanced = true;

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

    error = '';
    notice = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }
</script>

{#if props && !props.configured}
  <div class="card">
    <p class="muted">
      Krea 2 is not configured. Set the <strong>Images (Krea 2)</strong> service URL on the Settings
      page (or <code>KREA2_SERVICE_URL</code>) to enable this tab.
    </p>
  </div>
{:else if props}
  <!-- Parity with the FLUX.2 tab's header line. The page header can't render this
       (it has no krea2 props), so the panel owns its own engine-status line. -->
  <p class="engine-status muted">
    Krea 2 ·
    {#if props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
        >engine unreachable</span
      >{/if}
    {#if props.healthy}
      · {editAvailable ? 'text→image + edit' : 'text→image only'}
    {/if}
  </p>
{/if}

<!-- Prompt -->
<div class="card">
  <label>
    Brief
    <span class="hint">— a casual one-liner; the model writes you a full prompt</span>
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
    {#if resolvedMode !== 't2i'}
      <span class="hint">
        The assist writes text-to-image <em>captions</em>, not edit instructions — in Edit and
        Restage modes, write the instruction yourself.
      </span>
    {/if}
  </label>

  <label>
    Prompt
    {#if resolvedMode !== 't2i'}
      <span class="hint">
        — an edit <em>instruction</em>: say what to change or where to put the subject, not what the
        whole frame looks like
      </span>
    {:else}
      <span class="hint">
        — Krea 2 reads a vision-language encoder asked to <em>describe</em> an image, so write one dense
        descriptive paragraph, not tags or JSON
      </span>
    {/if}
    <textarea
      rows="4"
      bind:value={prompt}
      placeholder={resolvedMode !== 't2i'
        ? 'e.g. create a photo of this person at a night market at dusk, warm string lights behind them'
        : 'e.g. A small wooden cabin sits in deep snow among close-packed pines, framed slightly from below so the roofline cuts across a bruised violet dusk sky; warm amber light spills from two square windows onto the drifts…'}
    ></textarea>
    {#if prompt.trim()}
      <div class="idea-row">
        <button
          type="button"
          class="btn-sec"
          onclick={improvePrompt}
          disabled={improving}
          title="Rewrite the current prompt under the same Krea 2 rules, applying your note — keeps what works"
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
    {#if negativeIsInert}
      <span class="hint warn">
        Inert at CFG {overrideCfg ? cfg : defaultCfg}: the distilled checkpoint runs without
        classifier-free guidance. Raise CFG above 1 in Advanced for this to do anything.
      </span>
    {/if}
  </label>

  <label title={editAvailable ? undefined : EDIT_UNAVAILABLE_TIP}>
    Mode
    <select bind:value={mode}>
      <option value="auto">Auto</option>
      <option value="t2i">Text → image</option>
      <option value="edit" disabled={!editAvailable}>
        Edit in place (reference is the canvas){editAvailable ? '' : ' — unavailable'}
      </option>
      <option value="restage" disabled={!editAvailable}>
        Restage (reference is an identity source){editAvailable ? '' : ' — unavailable'}
      </option>
    </select>
    {#if mode === 'auto' && editAvailable}
      <span class="hint">
        Auto picks <strong>Edit</strong> when you attach a reference image, else
        <strong>Text → image</strong>. Currently → <strong>{resolvedLabel}</strong>.
      </span>
    {/if}
    {#if mode === 'edit'}
      <span class="hint">
        The reference is the canvas, so the render uses <strong>its</strong> aspect ratio — ask for a
        different one and the server fits it back rather than cropping the reference.
      </span>
    {/if}
    {#if mode === 'restage'}
      <span class="hint">
        The reference is an identity source only. The output can be any shape, and the reference is
        read at its native geometry instead of being cropped to match.
      </span>
    {/if}
    {#if mode === 't2i' && referenceFiles.length}
      <span class="hint warn">
        Mode is Text → image, so the attached reference{referenceFiles.length > 1 ? 's' : ''} will be
        ignored.
      </span>
    {/if}
    {#if (mode === 'edit' || mode === 'restage') && !referenceFiles.length}
      <span class="hint warn">
        {mode === 'edit' ? 'Edit' : 'Restage'} mode needs a reference image — attach one below.
      </span>
    {/if}
    {#if !editAvailable}
      <span class="hint warn">
        Edit is unavailable: the identity-edit LoRA (<code
          >{props?.edit_lora ?? 'krea2-identity-edit-v1_2.gguf'}</code
        >) isn't in the engine's resolvable LoRA list, and an unresolvable LoRA comes back as an
        unexplained 400.
      </span>
    {/if}
  </label>
</div>

<!-- Reference images — TWO ORDERED SLOTS. The order is part of the trained
     recipe, so the slots are numbered and labelled rather than an anonymous
     list. -->
<div class="card">
  <h3>
    Reference images
    <span class="hint">
      (optional — attaching one switches Auto → Edit; max {props?.max_reference_images ?? 2})
    </span>
  </h3>
  <p class="hint order-note">
    <strong>The order is part of the recipe</strong> the identity-edit LoRA was trained on:
    <strong>1 = scene</strong>, <strong>2 = person</strong>. Swapping them sharply degrades the
    result. One reference is the ordinary case — put it in slot 1. Two <em>people</em> in one pass blend
    faces; chain two single-reference edits instead.
  </p>

  <div class="ref-slot">
    <span class="slot-head">
      <span class="slot-num">1</span> Scene
      <span class="hint">— the image being edited; sets the aspect ratio</span>
    </span>
    <FileDropzone
      accept="image/*"
      bind:files={sceneFiles}
      disabled={!editAvailable}
      label={sceneFile ? sceneFile.name : 'Reference 1 — scene'}
      hint={editAvailable
        ? 'drop an image or click to browse'
        : 'edit mode unavailable on this engine'}
    />
    {#if sceneFile}
      <div class="ref-row">
        <span class="ref-name">
          {sceneDims ? `${sceneDims.w}×${sceneDims.h}` : 'reading size…'}
        </span>
        {#if personFile}
          <button class="link-btn" type="button" onclick={swapReferences}> Swap slots </button>
        {/if}
        <button class="link-btn" type="button" onclick={() => (sceneFiles = null)}>Remove</button>
      </div>
    {/if}
  </div>

  <div class="ref-slot">
    <span class="slot-head">
      <span class="slot-num">2</span> Person
      <span class="hint">— the identity to carry into the scene (optional)</span>
    </span>
    <FileDropzone
      accept="image/*"
      bind:files={personFiles}
      disabled={!editAvailable}
      label={personFile ? personFile.name : 'Reference 2 — person'}
      hint={editAvailable
        ? 'leave empty for a single-reference edit'
        : 'edit mode unavailable on this engine'}
    />
    {#if personFile}
      <div class="ref-row">
        <span class="ref-name">
          {personDims ? `${personDims.w}×${personDims.h}` : 'reading size…'}
        </span>
        {#if !sceneFile}
          <button class="link-btn" type="button" onclick={swapReferences}> Move to slot 1 </button>
        {/if}
        <button class="link-btn" type="button" onclick={() => (personFiles = null)}>Remove</button>
      </div>
    {/if}
  </div>

  {#if personFile && !sceneFile}
    <p class="hint warn">
      Only slot 2 is filled, so it is sent as the single reference — that works, but slot 1 is the
      conventional place for one image.
    </p>
  {/if}
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
        Seed <span class="hint">(random when off)</span>
        <input type="number" min="0" bind:value={seed} />
      </label>
    {/if}
    <label
      class="toggle"
      title={mattingAvailable
        ? 'Run background removal on every rendered image'
        : 'Background-removal engine not configured'}
    >
      <input type="checkbox" bind:checked={removeBackground} disabled={!mattingAvailable} /> Remove
      background
      <span class="hint">
        {mattingAvailable ? '(also returns a transparent cutout)' : '(engine unavailable)'}
      </span>
    </label>
  </div>

  {#if referenceFiles.length}
    <div class="match-row">
      <button
        class="btn-sec"
        type="button"
        onclick={matchSource}
        disabled={!suggestedFit}
        title="Set width/height to reference 1's aspect ratio (×16, ~2 MP) — the engine centre-crops the reference to whatever AR you ask for, so a mismatch throws away its edges"
      >
        Match source aspect
      </button>
      {#if primaryDims && suggestedFit}
        <span class="hint">
          reference 1 is {primaryDims.w}×{primaryDims.h} (AR {(
            primaryDims.w / primaryDims.h
          ).toFixed(2)}) → {suggestedFit.w}×{suggestedFit.h}
        </span>
      {:else}
        <span class="hint">reading the reference's size…</span>
      {/if}
    </div>
  {/if}

  <p class="hint range-note">
    Supported range is roughly {recommendedMinDim}–{maxDim} per side. Measured on the 5060 Ti: 1024² ≈
    11 s, 1920×1088 ≈ 21 s, 2048² ≈ 72 s.
  </p>
  {#if belowRange}
    <p class="hint warn">
      Below {recommendedMinDim} per side is outside the model's supported range — it will render, but
      quality degrades.
    </p>
  {/if}
  {#if arMismatch}
    <p class="hint warn">
      {snap16(width)}×{snap16(height)} isn't reference 1's aspect ratio. In Edit mode the reference
      <em>is</em> the canvas, so the server will render at the reference's aspect ratio instead of
      the size you typed — it will not crop the reference. Use “Match source aspect” to see the real
      output size, or switch to <strong>Restage</strong> if you want this canvas and the reference is
      only an identity source.
    </p>
  {/if}
  {#if overTwoMp}
    <p class="hint warn">
      {(outputPixels / 1048576).toFixed(1)} MP is above the ~2 MP edit ceiling — the reference starts
      duplicating or bleeding into the output above it.
    </p>
  {/if}

  <details bind:open={showAdvanced} class="advanced">
    <summary>Advanced</summary>
    <div class="knobs">
      <label class="toggle">
        <input type="checkbox" bind:checked={overrideSteps} /> Override steps
        <span class="hint">
          (default {defaultSteps} — {resolvedMode !== 't2i' ? 'edit recipe' : 'distilled'})
        </span>
      </label>
      {#if overrideSteps}
        <label>
          Steps<input type="number" min="1" max="100" bind:value={steps} />
          {#if resolvedMode !== 't2i'}
            <span class="hint">
              8 favours composition adherence, 12 favours face detail, {defaultSteps}
              balances
            </span>
          {/if}
        </label>
      {/if}
      <label class="toggle">
        <input type="checkbox" bind:checked={overrideCfg} /> Override CFG
        <span class="hint">(default {defaultCfg} = no guidance)</span>
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
          <span class="hint">
            {props.defaults?.default_scheduler ?? FALLBACK_SCHEDULER} is the default; simple is the other
            sane pick
          </span>
        </label>
      {/if}
      <label class="toggle">
        <input type="checkbox" bind:checked={overrideVaeTiling} /> Override VAE tiling
        <span class="hint">(server default: on)</span>
      </label>
      {#if overrideVaeTiling}
        <label class="toggle">
          <input type="checkbox" bind:checked={vaeTiling} /> VAE tiling
          <span class="hint">turning it off can OOM the decode at 2048²</span>
        </label>
      {/if}
      {#if resolvedMode !== 't2i'}
        <label class="toggle">
          <input type="checkbox" bind:checked={overrideGrounding} /> Override reference grounding
          <span class="hint">(default {defaultGrounding}px)</span>
        </label>
        {#if overrideGrounding}
          <label>
            Grounding <span class="muted">{grounding}px</span>
            <input
              type="range"
              min={groundingMin}
              max={groundingMax}
              step="64"
              bind:value={grounding}
            />
            <span class="hint">
              longest side the reference is shown to the VL encoder at — lower = stronger edit
              adherence, higher = stronger likeness; lower it if you get duplicated or split
              compositions
            </span>
          </label>
        {/if}
      {/if}
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
  <QueueChip kind="krea2" bind:options={queueOpts} response={accepted} />
  {#if running}
    <button class="btn-sec" onclick={handleCancel} title="Abort the in-flight render">Cancel</button
    >
    {#if jobState === 'queued'}
      <span class="muted">Queued{queuedBecause ? ` — ${queuedBecause}` : ''}</span>
    {:else if jobId}
      <span class="muted">Rendering — safe to navigate away or reload.</span>
    {:else}
      <span class="muted">GPU held for the render; yields to other services.</span>
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
      <!-- mode_used is what `auto` actually resolved to. -->
      <span
        >Krea 2 · {result.mode_used === 'edit'
          ? 'edit'
          : result.mode_used === 'restage'
            ? 'restage'
            : 'text → image'}</span
      >
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
              <a class="btn-sec" href={imageUrls[i]} download={`krea2-${img.seed}.png`}>Download</a>
            {/if}
          </div>
          {#if cutoutUrls[i]}
            <div class="cutout">
              <img src={cutoutUrls[i]} alt={`Cutout ${i + 1}`} />
            </div>
            <div class="shot-foot">
              <span class="muted">background removed</span>
              <a class="btn-sec" href={cutoutUrls[i]} download={`krea2-${img.seed}-cutout.png`}
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
  service="krea2"
  refreshKey={historyKey}
  title="Krea 2 history"
  onRestore={restore}
/>

<style>
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
  .hint.warn {
    color: var(--danger);
    display: block;
    margin-top: 4px;
  }
  .range-note {
    margin: 12px 0 0;
  }
  .order-note {
    display: block;
    margin: 0 0 12px;
    line-height: 1.5;
  }
  .ref-slot {
    margin-top: 12px;
  }
  .ref-slot:first-of-type {
    margin-top: 0;
  }
  .slot-head {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--ink);
    margin-bottom: 6px;
  }
  /* The numbers are the contract (1 = scene, 2 = person), so they read as a
     fixed position rather than decoration. */
  .slot-num {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--accent);
    font-size: 11px;
    font-weight: 600;
  }
  .ref-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 10px;
    margin-top: 6px;
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
    color: var(--muted);
  }
  .link-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    font-size: 12px;
    text-decoration: underline;
    width: auto;
    padding: 0;
  }
  .match-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    margin-top: 12px;
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
  .engine-status {
    margin: 0 0 16px;
    font-size: 13px;
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
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
  .meta {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
    margin-bottom: 12px;
    flex-wrap: wrap;
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
