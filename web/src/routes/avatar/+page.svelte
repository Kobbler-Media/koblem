<script lang="ts">
  import {
    avatarGenerate,
    avatarScriptFromLlm,
    cancelQueueJob,
    getAvatarProps,
    getAvatarJob,
    getAvatarActive,
    fetchAvatarClip,
    listAvatarAssets,
    uploadAvatarAsset,
    deleteAvatarAsset,
    fetchAvatarAssetUrl,
    listTtsVoices,
    JOB_POLL_MS,
    type AvatarAsset,
    type AvatarGenerateRequest,
    type AvatarJob,
    type AvatarImageSource,
    type AvatarAudioSource,
    type AvatarOffload,
    type AvatarBsaConfig,
    type AvatarProps,
    type TtsVoice
  } from '$lib/api/client';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import { blockedSummary } from '$lib/queue/format';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import type { Generation } from '$lib/types';
  import { str, num, bool, obj, reattachNote } from '$lib/restore';
  import { WaveformTrim, blobToMonoSamples, sliceSamples, encodeWavMono16 } from '@koblibs/ui';
  import { onMount, onDestroy } from 'svelte';

  let historyKey = $state(0);
  let restoreNote = $state('');

  // Core inputs (the kid-presses-Go flow).
  let imageFile = $state<File | null>(null);
  let imageFiles = $state<FileList | null>(null);
  // Selected library asset (when picking a saved character instead of
  // uploading). When set, imageFile is ignored and the request goes out as
  // image_source: { kind: "library_asset_id", id }.
  let selectedAssetId = $state<string | null>(null);
  let selectedAssetName = $state<string | null>(null);
  // The asset library, fetched on mount + after upload/delete.
  let assets = $state<AvatarAsset[]>([]);
  // Cached object URLs for the thumbnail row (revoked on destroy).
  let assetThumbs = $state<Record<string, string>>({});
  // Tracks whether we've already offered to save the most recent upload, so
  // we don't re-pester after every render.
  let lastOfferedFile = $state<File | null>(null);
  // True when we're mid-upload to /assets — keeps the UI from racing.
  let savingAsset = $state(false);
  // --- Crop tool (upload path only) -------------------------------------
  // 480p/720p are quality tiers, not fixed shapes. Orientation picks the shape:
  // portrait 480/832 ≈ 0.577, landscape transposes it (832/480 ≈ 1.733). The
  // aspect-locked crop box matches the output so the region isn't distorted.
  let orientation = $state<'portrait' | 'landscape'>('portrait');
  // width / height of the output (and thus the crop box).
  let CROP_ASPECT = $derived(orientation === 'landscape' ? 832 / 480 : 480 / 832);
  // Object URL for the uploaded file (the <img> the crop overlay sits on).
  let cropImageUrl = $state<string | null>(null);
  // Natural pixel dimensions of the loaded image.
  let imgNaturalW = $state(0);
  let imgNaturalH = $state(0);
  // Crop region in NATURAL image pixel coordinates.
  let crop = $state<{ x: number; y: number; w: number; h: number } | null>(null);
  // Live geometry of the rendered <img> (CSS px) so we can map pointer moves
  // back into natural-pixel space; measured on load/resize.
  let cropImgEl = $state<HTMLImageElement | null>(null);

  // Last object URL we minted for the crop preview. Plain (untracked) so the
  // upload $effect can revoke the prior URL without depending on cropImageUrl
  // (reading the $state we also write would re-trigger the effect forever).
  let lastCropObjectUrl: string | null = null;

  $effect(() => {
    const f = imageFiles?.[0] ?? null;
    imageFile = f;
    // Clear the library selection whenever the user picks a fresh upload.
    if (f) {
      selectedAssetId = null;
      selectedAssetName = null;
    }
    // Reset + (re)build the crop preview for the new upload. Revoke the prior
    // object URL so we don't leak. crop is reset here and re-seeded once the
    // <img> reports its natural dimensions in onCropImageLoad().
    if (lastCropObjectUrl) {
      URL.revokeObjectURL(lastCropObjectUrl);
      lastCropObjectUrl = null;
    }
    crop = null;
    imgNaturalW = 0;
    imgNaturalH = 0;
    if (f) {
      lastCropObjectUrl = URL.createObjectURL(f);
      cropImageUrl = lastCropObjectUrl;
    } else {
      cropImageUrl = null;
    }
  });

  // Largest centered box at CROP_ASPECT that fits a w×h image (natural px).
  function defaultCrop(w: number, h: number) {
    let cw = w;
    let ch = cw / CROP_ASPECT;
    if (ch > h) {
      ch = h;
      cw = ch * CROP_ASPECT;
    }
    return { x: (w - cw) / 2, y: (h - ch) / 2, w: cw, h: ch };
  }

  function onCropImageLoad(e: Event) {
    const el = e.currentTarget as HTMLImageElement;
    cropImgEl = el;
    imgNaturalW = el.naturalWidth;
    imgNaturalH = el.naturalHeight;
    if (imgNaturalW > 0 && imgNaturalH > 0) {
      crop = defaultCrop(imgNaturalW, imgNaturalH);
    }
  }

  // Flipping orientation changes the locked aspect — re-seed the box to the
  // largest centered crop at the new shape.
  $effect(() => {
    CROP_ASPECT; // track the aspect (orientation)
    if (imgNaturalW > 0 && imgNaturalH > 0) {
      crop = defaultCrop(imgNaturalW, imgNaturalH);
    }
  });

  // Scale factor: natural pixels per one CSS pixel of the rendered <img>.
  function naturalPerCss(): number {
    if (!cropImgEl || cropImgEl.clientWidth === 0) return 1;
    return imgNaturalW / cropImgEl.clientWidth;
  }

  // Active pointer drag for the crop box. mode 'move' translates the whole
  // box; the corner modes resize while keeping the aspect locked.
  type CropDrag = {
    mode: 'move' | 'nw' | 'ne' | 'sw' | 'se';
    startX: number; // pointer start, natural px
    startY: number;
    orig: { x: number; y: number; w: number; h: number };
  };
  let cropDrag: CropDrag | null = null;

  function cropPointerDown(mode: CropDrag['mode'], e: PointerEvent) {
    if (!crop) return;
    e.preventDefault();
    e.stopPropagation();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    const k = naturalPerCss();
    cropDrag = {
      mode,
      startX: e.clientX * k,
      startY: e.clientY * k,
      orig: { ...crop }
    };
  }

  function cropPointerMove(e: PointerEvent) {
    if (!cropDrag || !crop) return;
    e.preventDefault();
    const k = naturalPerCss();
    const dx = e.clientX * k - cropDrag.startX;
    const dy = e.clientY * k - cropDrag.startY;
    const o = cropDrag.orig;
    const W = imgNaturalW;
    const H = imgNaturalH;
    const MIN = 32; // min crop width in natural px

    if (cropDrag.mode === 'move') {
      let nx = o.x + dx;
      let ny = o.y + dy;
      nx = Math.max(0, Math.min(nx, W - o.w));
      ny = Math.max(0, Math.min(ny, H - o.h));
      crop = { x: nx, y: ny, w: o.w, h: o.h };
      return;
    }

    // Corner resize: pick the dominant axis delta, derive the other from the
    // locked aspect, then anchor the opposite corner so it stays put. Clamp the
    // new width so the box never leaves the image on the moving sides.
    // Anchor (fixed corner) in natural px:
    const anchorX = cropDrag.mode === 'nw' || cropDrag.mode === 'sw' ? o.x + o.w : o.x;
    const anchorY = cropDrag.mode === 'nw' || cropDrag.mode === 'ne' ? o.y + o.h : o.y;
    // Proposed new width from horizontal drag (sign depends on which corner).
    const grow = cropDrag.mode === 'se' || cropDrag.mode === 'ne' ? dx : -dx;
    let nw = o.w + grow;
    // Max width so both width and the aspect-derived height fit within image,
    // measured from the anchor toward the dragged corner.
    const maxWByX = cropDrag.mode === 'se' || cropDrag.mode === 'ne' ? W - anchorX : anchorX;
    const maxHByY = cropDrag.mode === 'se' || cropDrag.mode === 'sw' ? H - anchorY : anchorY;
    const maxWByH = maxHByY * CROP_ASPECT;
    nw = Math.max(MIN, Math.min(nw, maxWByX, maxWByH));
    const nh = nw / CROP_ASPECT;
    const nx = cropDrag.mode === 'nw' || cropDrag.mode === 'sw' ? anchorX - nw : anchorX;
    const ny = cropDrag.mode === 'nw' || cropDrag.mode === 'ne' ? anchorY - nh : anchorY;
    crop = { x: nx, y: ny, w: nw, h: nh };
  }

  function cropPointerUp(e: PointerEvent) {
    if (cropDrag) {
      try {
        (e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId);
      } catch {
        // capture may already be gone; ignore
      }
    }
    cropDrag = null;
  }

  // Inline style (in % of the rendered image box) for the crop rectangle.
  let cropBoxStyle = $derived(
    crop && imgNaturalW > 0 && imgNaturalH > 0
      ? `left:${(crop.x / imgNaturalW) * 100}%;top:${(crop.y / imgNaturalH) * 100}%;` +
          `width:${(crop.w / imgNaturalW) * 100}%;height:${(crop.h / imgNaturalH) * 100}%;`
      : ''
  );

  // Render the chosen crop region onto an offscreen canvas at the target tier
  // and return it as a File, ready to send in place of the raw upload. Falls
  // back to the centered default crop if the box was never initialised.
  const RES_DIMS: Record<'480p' | '720p', { w: number; h: number }> = {
    '480p': { w: 480, h: 832 },
    '720p': { w: 720, h: 1248 }
  };
  // Output dims for the chosen tier, transposed when landscape.
  function targetDims(): { w: number; h: number } {
    const { w, h } = RES_DIMS[resolution];
    return orientation === 'landscape' ? { w: h, h: w } : { w, h };
  }
  let orientCropDims = $derived.by(() => {
    const { w, h } = targetDims();
    return `${w}×${h}`;
  });
  async function cropUploadToFile(file: File): Promise<File> {
    const target = targetDims();
    // Decode the source image off the file (independent of the live preview).
    const url = URL.createObjectURL(file);
    try {
      const img = await new Promise<HTMLImageElement>((resolve, reject) => {
        const el = new Image();
        el.onload = () => resolve(el);
        el.onerror = () => reject(new Error('could not decode image'));
        el.src = url;
      });
      const region = crop ?? defaultCrop(img.naturalWidth, img.naturalHeight);
      const canvas = document.createElement('canvas');
      canvas.width = target.w;
      canvas.height = target.h;
      const ctx = canvas.getContext('2d');
      if (!ctx) return file; // no 2d context — fall back to raw upload
      ctx.drawImage(img, region.x, region.y, region.w, region.h, 0, 0, target.w, target.h);
      const blob = await new Promise<Blob | null>((resolve) =>
        canvas.toBlob((b) => resolve(b), 'image/jpeg', 0.95)
      );
      if (!blob) return file; // encode failed — fall back to raw upload
      const baseName = file.name.replace(/\.[^.]+$/, '') || 'portrait';
      return new File([blob], `${baseName}-crop.jpg`, { type: 'image/jpeg' });
    } finally {
      URL.revokeObjectURL(url);
    }
  }

  // Audio source: 'tts' = render speech from a script via tts-breeze;
  // 'upload' = pre-recorded wav/mp3 driving lip-sync directly.
  let audioMode = $state<'tts' | 'upload'>('tts');

  // Upload path. The dropped clip is decoded to mono PCM once, then windowed
  // with <WaveformTrim> (the same trim control the voice-clone editor uses).
  // On Go the selected [trimStart, trimEnd) window is sliced to a WAV; its
  // length sizes the render (the supervisor's _frames_for_duration walks
  // segment count from duration_sec). trimStart/trimEnd are sample indices.
  let audioFile = $state<File | null>(null);
  let audioFiles = $state<FileList | null>(null);
  let audioSamples = $state<Float32Array | null>(null);
  let audioSampleRate = $state(0);
  let trimStart = $state(0);
  let trimEnd = $state(0);
  let audioDecodeError = $state<string | null>(null);
  $effect(() => {
    const f = audioFiles?.[0] ?? null;
    audioFile = f;
    audioSamples = null;
    audioDecodeError = null;
    if (!f) return;
    let cancelled = false;
    blobToMonoSamples(f)
      .then(({ samples, sampleRate }) => {
        if (cancelled) return;
        audioSamples = samples;
        audioSampleRate = sampleRate;
        trimStart = 0;
        trimEnd = samples.length;
      })
      .catch((e) => {
        if (!cancelled)
          audioDecodeError = e instanceof Error ? e.message : 'could not decode audio';
      });
    return () => {
      cancelled = true;
    };
  });
  const trimDurationSec = $derived(
    audioSampleRate > 0 ? (trimEnd - trimStart) / audioSampleRate : 0
  );
  // The selected window length drives the render duration for the upload path.
  $effect(() => {
    if (audioMode === 'upload' && trimDurationSec > 0) durationSec = trimDurationSec;
  });

  // TTS path
  let scriptBrief = $state('');
  let script = $state('');
  let voice = $state('default');
  // Catalog of voices fetched from /api/v1/tts/voices on mount.
  let voiceCatalog = $state<TtsVoice[]>([]);
  let voicesLoading = $state(false);
  // tts-breeze is the only TTS backend koblem speaks; the engine field is
  // sent only as a forward-compat hook and is ignored server-side.
  const engine = 'breeze' as const;
  let durationSec = $state(8); // seconds; no upper bound, default 8

  // Advanced (hidden by default).
  let showAdvanced = $state(false);
  let prompt = $state('a person talking');
  let segmentFrames = $state(53); // PERF.md sweet spot
  let contCondFrames = $state(13);
  let steps = $state(8);
  let cfgScale = $state(1.0);
  let useSeed = $state(false);
  let seed = $state(42);
  let audioMouthScale = $state(1.0);
  // Optional viseme-softening knob — a lowpass (e.g. 3000 Hz) smooths out
  // high-frequency consonant transients that drive mouth open/closed jitter.
  // OFF by default (0 = no filter); only reach for it if audio_mouth_scale
  // alone can't tame the jitter.
  let audioLowpassHz = $state(0);
  // Model tiers only: 480p (480x832, default) | 720p (720x1248). 720p is ~2.7x
  // the activations of 480p and OOMs the 12GB card — see README/HANDOFF.
  let resolution = $state<'480p' | '720p'>('480p');
  let offload = $state<AvatarOffload>('true');
  let vaeTiling = $state(true);

  // Perf knobs sub-panel (deep advanced).
  let showPerfKnobs = $state(false);
  let bsaEnable = $state(false);
  let bsaRadius = $state(1);
  let bsaSelfFrame = $state(true);
  let bsaBitmap = $state(true);
  let debugEnvText = $state('');

  // Async UI state.
  let running = $state(false);
  let drafting = $state(false);
  let improving = $state(false);
  let scriptInstruction = $state('');
  let error = $state('');
  let notice = $state('');
  let result = $state<AvatarJob | null>(null);
  let clipUrl = $state<string | null>(null);
  let props = $state<AvatarProps | null>(null);

  let jobId = $state<string | null>(null);
  let queued = $state(false);
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);
  let pollTimer: ReturnType<typeof setInterval> | null = null;

  function startPoll() {
    stopPoll();
    pollTimer = setInterval(pollOnce, JOB_POLL_MS);
  }

  function stopPoll() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  /** Take over a render already in flight — a fresh 202, or one `/active` found after a reload. */
  function adoptJob(job: AvatarJob) {
    if (clipUrl) {
      URL.revokeObjectURL(clipUrl);
      clipUrl = null;
    }
    result = null;
    error = '';
    jobId = job.id;
    running = true;
    queued = job.state === 'queued';
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    startPoll();
  }

  async function pollOnce() {
    if (!jobId) return;
    let job: AvatarJob;
    try {
      job = await getAvatarJob(jobId);
    } catch (e) {
      // A dropped poll is not a dropped render — the queue owns it. Keep polling; only a terminal
      // state or an explicit cancel ends this loop.
      console.warn('avatar poll failed, will retry', e);
      return;
    }
    queued = job.state === 'queued';
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    if (job.state === 'queued' || job.state === 'rendering') return;

    stopPoll();
    running = false;
    jobId = null;
    queued = false;
    queuedBecause = '';
    if (job.state !== 'done') {
      error = job.state === 'cancelled' ? 'render cancelled' : job.error || 'render failed';
      return;
    }
    result = job;
    historyKey++;
    if (job.clip) clipUrl = await fetchAvatarClip(job.clip.url);
    notice = job.render_ms ? `Rendered in ${(job.render_ms / 1000).toFixed(1)}s.` : 'Rendered.';
  }

  onMount(async () => {
    try {
      props = await getAvatarProps();
      if (props.default_segment_frames) segmentFrames = props.default_segment_frames;
      if (props.default_cont_cond_frames) contCondFrames = props.default_cont_cond_frames;
      if (props.default_steps) steps = props.default_steps;
      if (props.default_resolution === '720p' || props.default_resolution === '480p') {
        resolution = props.default_resolution;
      }
    } catch {
      props = {
        healthy: false,
        default_segment_frames: 53,
        default_cont_cond_frames: 13,
        default_steps: 8,
        default_resolution: '480p'
      };
    }
    await refreshAssets();
    await refreshVoices();
    // Re-attach to a render already in flight, so a reload (or a second device) finds it.
    try {
      const { job } = await getAvatarActive();
      if (job) adoptJob(job);
    } catch {
      // Nothing to re-attach to, or the API is unreachable.
    }
  });

  onDestroy(() => {
    stopPoll();
    if (clipUrl) URL.revokeObjectURL(clipUrl);
    if (lastCropObjectUrl) URL.revokeObjectURL(lastCropObjectUrl);
    for (const u of Object.values(assetThumbs)) URL.revokeObjectURL(u);
  });

  async function refreshVoices() {
    voicesLoading = true;
    try {
      const r = await listTtsVoices();
      voiceCatalog = r.voices;
      // If the current voice isn't in the catalog (or initial 'default' is
      // missing), snap to the first available so the dropdown matches.
      if (voiceCatalog.length > 0 && !voiceCatalog.some((v) => v.id === voice)) {
        voice = voiceCatalog[0].id;
      }
    } catch {
      // TTS unreachable — leave the catalog empty; the text input falls
      // back to a free-form voice id.
    } finally {
      voicesLoading = false;
    }
  }

  async function refreshAssets() {
    try {
      const resp = await listAvatarAssets();
      assets = resp.assets;
      // Lazily fetch any new thumbnails. Asset URLs are auth-gated so we
      // can't just point an <img src> at them — fetch + objectURL.
      const next: Record<string, string> = {};
      for (const a of assets) {
        if (assetThumbs[a.id]) {
          next[a.id] = assetThumbs[a.id];
          continue;
        }
        try {
          next[a.id] = await fetchAvatarAssetUrl(a.id);
        } catch {
          // Skip — broken file on disk; the row stays visible without a thumb.
        }
      }
      // Revoke object URLs for assets that vanished from the list.
      for (const [id, url] of Object.entries(assetThumbs)) {
        if (!next[id]) URL.revokeObjectURL(url);
      }
      assetThumbs = next;
    } catch {
      // Listing failed — best-effort; the picker just stays empty.
    }
  }

  async function pickSavedCharacter(asset: AvatarAsset) {
    selectedAssetId = asset.id;
    selectedAssetName = asset.filename;
    // Clear the upload-side picker so the request goes out as library_asset_id.
    imageFile = null;
    imageFiles = null;
    lastOfferedFile = null;
    notice = `Using saved character: ${asset.filename}`;
  }

  async function handleDeleteAsset(asset: AvatarAsset) {
    if (!confirm(`Remove saved character "${asset.filename}"?`)) return;
    try {
      await deleteAvatarAsset(asset.id);
      if (selectedAssetId === asset.id) {
        selectedAssetId = null;
        selectedAssetName = null;
      }
      await refreshAssets();
    } catch (e) {
      error = e instanceof Error ? e.message : 'delete failed';
    }
  }

  /// Save the just-uploaded portrait as a re-usable library asset. Called
  /// from the "Save character?" button after the user picks a fresh image.
  async function saveCurrentAsAsset() {
    if (!imageFile) return;
    savingAsset = true;
    try {
      const saved = await uploadAvatarAsset(imageFile);
      lastOfferedFile = imageFile; // suppress the prompt for this same file
      notice = `Saved character "${saved.filename}".`;
      await refreshAssets();
    } catch (e) {
      error = e instanceof Error ? e.message : 'save failed';
    } finally {
      savingAsset = false;
    }
  }

  // Show the "Save this character?" prompt for fresh uploads that aren't
  // already in the library. We compare on file name + size to avoid offering
  // the same file twice in a row.
  let saveOfferVisible = $derived(
    !!imageFile &&
      imageFile !== lastOfferedFile &&
      !selectedAssetId &&
      !assets.some((a) => imageFile && a.filename === imageFile.name && a.created_at > 0)
  );

  function parseDebugEnv(text: string): Record<string, string> | undefined {
    const out: Record<string, string> = {};
    for (const raw of text.split(/[\n;]/)) {
      const line = raw.trim();
      if (!line) continue;
      const eq = line.indexOf('=');
      if (eq < 0) continue;
      const k = line.slice(0, eq).trim();
      const v = line.slice(eq + 1).trim();
      if (!k) continue;
      if (k.startsWith('LONGCAT_') || k.startsWith('GGML_') || k.startsWith('CUDA_')) {
        out[k] = v;
      }
    }
    return Object.keys(out).length ? out : undefined;
  }

  function buildRequest(): AvatarGenerateRequest {
    const imageSource: AvatarImageSource = selectedAssetId
      ? { kind: 'library_asset_id', id: selectedAssetId }
      : { kind: 'upload' };
    let audioSource: AvatarAudioSource;
    if (audioMode === 'upload') {
      if (!audioFile) {
        error = 'drop an audio file or switch to TTS mode';
        throw new Error(error);
      }
      audioSource = { kind: 'upload' };
    } else if (script.trim()) {
      audioSource = { kind: 'tts', script: script.trim(), voice, engine };
    } else if (scriptBrief.trim()) {
      audioSource = {
        kind: 'tts',
        script_from_brief: scriptBrief.trim(),
        voice,
        engine
      };
    } else {
      error = 'enter a script or a brief';
      throw new Error(error);
    }
    const req: AvatarGenerateRequest = {
      image_source: imageSource,
      audio_source: audioSource,
      duration_sec: durationSec,
      // Orientation only governs the upload crop. Library assets ship raw
      // (no crop UI), so don't impose a shape the stored image may not match.
      orientation: selectedAssetId ? 'portrait' : orientation
    };
    if (showAdvanced) {
      req.prompt = prompt;
      req.segment_frames = segmentFrames;
      req.cont_cond_frames = contCondFrames;
      req.steps = steps;
      req.cfg_scale = cfgScale;
      if (useSeed) req.seed = seed;
      req.audio_mouth_scale = audioMouthScale;
      req.audio_lowpass_hz = audioLowpassHz;
      req.resolution = resolution;
      req.offload = offload;
      req.vae_tiling = vaeTiling;
    }
    if (showPerfKnobs) {
      const bsa: AvatarBsaConfig = {
        enable: bsaEnable,
        radius: bsaRadius,
        self_frame: bsaSelfFrame,
        bitmap: bsaBitmap
      };
      if (bsaEnable) req.bsa = bsa;
      const env = parseDebugEnv(debugEnvText);
      if (env) req.debug_env = env;
    }
    return req;
  }

  async function draftScript() {
    if (!scriptBrief.trim()) {
      error = 'type a brief first';
      return;
    }
    drafting = true;
    error = '';
    notice = '';
    try {
      const r = await avatarScriptFromLlm(scriptBrief, durationSec);
      script = r.script;
      notice = 'Drafted a script — edit freely, then press Go.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'script draft failed';
    } finally {
      drafting = false;
    }
  }

  // Improve the current script in place under the same rules — optionally toward
  // a note; a blank note is a general polish. The brief + target length ride
  // along as context.
  async function improveScript() {
    if (!script.trim()) return;
    improving = true;
    error = '';
    notice = '';
    try {
      const r = await avatarScriptFromLlm(scriptBrief, durationSec, {
        source: script,
        instruction: scriptInstruction.trim() || undefined
      });
      script = r.script;
      scriptInstruction = '';
      notice = 'Improved the script — edit freely, then press Go.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'script improve failed';
    } finally {
      improving = false;
    }
  }

  async function handleCancel() {
    if (!jobId) return;
    try {
      // 🔴 Off the QUEUE, not the engine: the engine's own /cancel only reaches a render that is
      // already running, and one still waiting its turn is exactly the one worth dropping.
      await cancelQueueJob(jobId);
      notice = 'Cancelled — the render was dropped.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'cancel failed';
    }
  }

  // Repopulate the form from a saved generation's request_meta. Uploaded
  // portrait/audio bytes can't be re-attached — reattachNote flags that.
  function restore(g: Generation) {
    const m = g.request_meta;

    const as_ = obj(m, 'audio_source');
    const tts = as_ ? obj(as_, 'Tts') : undefined;
    if (tts) {
      audioMode = 'tts';
      const scriptText = str(m, 'script_used') ?? str(tts, 'script');
      if (scriptText !== undefined) script = scriptText;
      const sb = str(tts, 'script_from_brief');
      if (sb !== undefined) scriptBrief = sb;
      const v = str(tts, 'voice');
      if (v !== undefined) voice = v;
    }

    const p = str(m, 'prompt');
    if (p !== undefined) prompt = p;
    const res = str(m, 'resolution');
    if (res !== undefined) resolution = res as '480p' | '720p';
    const or = str(m, 'orientation');
    if (or !== undefined) orientation = or as 'portrait' | 'landscape';
    const dur = num(m, 'duration_sec');
    if (dur !== undefined) durationSec = dur;
    const sf = num(m, 'segment_frames');
    if (sf !== undefined) segmentFrames = sf;
    const ccf = num(m, 'cont_cond_frames');
    if (ccf !== undefined) contCondFrames = ccf;
    const st = num(m, 'steps');
    if (st !== undefined) steps = st;
    const cfg = num(m, 'cfg_scale');
    if (cfg !== undefined) cfgScale = cfg;
    const sd = num(m, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    const ams = num(m, 'audio_mouth_scale');
    if (ams !== undefined) audioMouthScale = ams;
    const alp = num(m, 'audio_lowpass_hz');
    if (alp !== undefined) audioLowpassHz = alp;
    const off = str(m, 'offload');
    if (off !== undefined) offload = off as AvatarOffload;
    const vt = bool(m, 'vae_tiling');
    if (vt !== undefined) vaeTiling = vt;

    if (
      p !== undefined ||
      sf !== undefined ||
      ccf !== undefined ||
      st !== undefined ||
      cfg !== undefined ||
      sd !== undefined ||
      ams !== undefined ||
      alp !== undefined ||
      res !== undefined ||
      off !== undefined ||
      vt !== undefined
    ) {
      showAdvanced = true;
    }

    restoreNote = reattachNote(m);
    error = '';
    notice = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  async function handleGo() {
    if (!imageFile && !selectedAssetId) {
      error = 'pick a reference portrait first';
      return;
    }
    if (audioMode === 'upload' && !audioFile) {
      error = 'drop an audio file or switch to TTS mode';
      return;
    }
    if (audioMode === 'upload' && audioFile && !audioSamples) {
      error = audioDecodeError ?? 'audio is still decoding — give it a sec';
      return;
    }
    if (audioMode === 'tts' && !script.trim() && !scriptBrief.trim()) {
      error = 'enter a script or ✨ to write one from a brief';
      return;
    }
    running = true;
    error = '';
    notice = '';
    accepted = null;
    if (clipUrl) {
      URL.revokeObjectURL(clipUrl);
      clipUrl = null;
    }
    result = null;
    try {
      const req = buildRequest();
      // When using a library_asset_id the server resolves the bytes; only
      // pass the file when we're actually uploading. For the upload path we
      // crop+resize client-side to the exact portrait tier the request asks
      // for so the user controls the framing (server would otherwise
      // center-crop). Library/URL sources are left untouched.
      const uploadImage = selectedAssetId || !imageFile ? null : await cropUploadToFile(imageFile);
      // Slice the selected window to a fresh mono WAV. Besides honouring the
      // trim, this materialises a self-contained in-memory File: a raw File
      // straight from the drop FileList can serialise into multipart with an
      // EMPTY body (the image dodges this only because cropUploadToFile
      // re-encodes through a canvas), whereas this Blob carries its bytes.
      const uploadAudio =
        audioMode === 'upload' && audioSamples
          ? new File(
              [encodeWavMono16(sliceSamples(audioSamples, trimStart, trimEnd), audioSampleRate)],
              `${(audioFile?.name ?? 'clip').replace(/\.[^.]+$/, '')}-clip.wav`,
              { type: 'audio/wav' }
            )
          : null;
      // Returns as soon as the render is ACCEPTED; the clip arrives through the poll.
      const res = await avatarGenerate(req, uploadImage, uploadAudio, queueOpts);
      accepted = res;
      jobId = res.job_id;
      queued = true;
      queuedBecause = res.blocked_reason ? blockedSummary(res.blocked_reason) : '';
      // 🔴 The only place the spoken script exists: the 9B wrote it a moment ago and the queue row
      // keeps no copy the poll could read back.
      if (res.script_used && !script) script = res.script_used;
      startPoll();
    } catch (e) {
      error = e instanceof Error ? e.message : 'render failed';
      running = false;
    }
  }

  // Rough render-time estimate, seeded from props.last_render_sec if known.
  // 12 s/new-frame @ 53f / 8 steps was the lap-31.1 baseline (PERF.md Table 2);
  // chained renders multiply by segments.
  const SEC_PER_NEW_FRAME = 12;
  const FPS = 25;
  function roughEstimate(): string {
    if (props?.last_render_sec && durationSec * 25 <= 53)
      return `~${props.last_render_sec.toFixed(0)}s`;
    const targetFrames = Math.round(durationSec * FPS);
    const newPerSeg = Math.max(1, segmentFrames - contCondFrames);
    const segments =
      targetFrames <= segmentFrames ? 1 : Math.ceil((targetFrames - segmentFrames) / newPerSeg) + 1;
    const sec =
      segments === 1
        ? SEC_PER_NEW_FRAME * targetFrames
        : segments * segmentFrames * SEC_PER_NEW_FRAME;
    return `~${(sec / 60).toFixed(1)} min`;
  }
</script>

<div class="page">
  <header class="header">
    <div>
      <h1>Avatar</h1>
      <p class="muted">
        Talking avatar — image + audio → video. {#if props && !props.healthy}<span class="warn"
            >(service offline)</span
          >{/if}
      </p>
    </div>
  </header>

  <!-- Image picker -->
  <div class="card">
    <h3>Character</h3>
    <FileDropzone
      accept="image/*"
      bind:files={imageFiles}
      label={selectedAssetName
        ? `Saved: ${selectedAssetName}`
        : imageFile
          ? imageFile.name
          : 'Drop a portrait or click to pick'}
      hint="PNG / JPG — full-face works best"
    />
    {#if cropImageUrl && !selectedAssetId}
      <div class="crop-tool">
        <div class="orient-toggle" role="group" aria-label="Output orientation">
          <button
            type="button"
            class:active={orientation === 'portrait'}
            onclick={() => (orientation = 'portrait')}
          >
            Portrait
          </button>
          <button
            type="button"
            class:active={orientation === 'landscape'}
            onclick={() => (orientation = 'landscape')}
          >
            Landscape
          </button>
        </div>
        <div class="crop-help">
          Drag the box to choose what gets used; drag a corner to resize. Locked to the {orientation}
          shape the avatar renders ({orientCropDims}).
        </div>
        <div class="crop-stage">
          <img
            class="crop-img"
            src={cropImageUrl}
            alt="Reference portrait to crop"
            onload={onCropImageLoad}
            draggable="false"
          />
          {#if crop && imgNaturalW > 0}
            <!-- The box's huge translucent box-shadow dims everything outside it. -->
            <div
              class="crop-box"
              style={cropBoxStyle}
              role="application"
              aria-label="Crop region — drag to move, drag a corner to resize"
              onpointerdown={(e) => cropPointerDown('move', e)}
              onpointermove={cropPointerMove}
              onpointerup={cropPointerUp}
              onpointercancel={cropPointerUp}
            >
              <!-- svelte-ignore a11y_no_static_element_interactions -->
              <span
                class="crop-handle nw"
                onpointerdown={(e) => cropPointerDown('nw', e)}
                onpointermove={cropPointerMove}
                onpointerup={cropPointerUp}
                onpointercancel={cropPointerUp}
              ></span>
              <!-- svelte-ignore a11y_no_static_element_interactions -->
              <span
                class="crop-handle ne"
                onpointerdown={(e) => cropPointerDown('ne', e)}
                onpointermove={cropPointerMove}
                onpointerup={cropPointerUp}
                onpointercancel={cropPointerUp}
              ></span>
              <!-- svelte-ignore a11y_no_static_element_interactions -->
              <span
                class="crop-handle sw"
                onpointerdown={(e) => cropPointerDown('sw', e)}
                onpointermove={cropPointerMove}
                onpointerup={cropPointerUp}
                onpointercancel={cropPointerUp}
              ></span>
              <!-- svelte-ignore a11y_no_static_element_interactions -->
              <span
                class="crop-handle se"
                onpointerdown={(e) => cropPointerDown('se', e)}
                onpointermove={cropPointerMove}
                onpointerup={cropPointerUp}
                onpointercancel={cropPointerUp}
              ></span>
            </div>
          {/if}
        </div>
      </div>
    {/if}
    {#if saveOfferVisible}
      <div class="save-offer">
        <span>Save this character for next time?</span>
        <button class="btn-sec" disabled={savingAsset} onclick={saveCurrentAsAsset}>
          {savingAsset ? 'Saving…' : 'Save'}
        </button>
        <button
          class="link-btn"
          onclick={() => {
            lastOfferedFile = imageFile;
          }}
        >
          Not now
        </button>
      </div>
    {/if}
    {#if assets.length > 0}
      <div class="saved-row">
        <div class="saved-label">Saved characters</div>
        <div class="saved-scroll">
          {#each assets as a (a.id)}
            <div
              class="saved-tile"
              class:selected={selectedAssetId === a.id}
              title={a.filename}
              role="button"
              tabindex="0"
              onclick={() => pickSavedCharacter(a)}
              onkeydown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  pickSavedCharacter(a);
                }
              }}
            >
              {#if assetThumbs[a.id]}
                <img src={assetThumbs[a.id]} alt={a.filename} />
              {:else}
                <div class="saved-tile-placeholder">…</div>
              {/if}
              <span class="saved-tile-name">{a.filename}</span>
              <button
                class="saved-tile-remove"
                type="button"
                aria-label="Remove saved character"
                title="Remove"
                onclick={(e) => {
                  e.stopPropagation();
                  handleDeleteAsset(a);
                }}>×</button
              >
            </div>
          {/each}
        </div>
      </div>
    {/if}
  </div>

  <!-- Audio source: TTS-from-script OR pre-recorded audio upload -->
  <div class="card">
    <h3>What they should say</h3>
    <div class="audio-mode-tabs">
      <button
        class="tab"
        class:active={audioMode === 'tts'}
        onclick={() => (audioMode = 'tts')}
        type="button"
      >
        ✨ Write a script
      </button>
      <button
        class="tab"
        class:active={audioMode === 'upload'}
        onclick={() => (audioMode = 'upload')}
        type="button"
      >
        🎙️ Upload audio
      </button>
    </div>

    {#if audioMode === 'tts'}
      <label>
        Brief <span class="hint">— a one-liner the 9B turns into a spoken script</span>
        <div class="idea-row">
          <input
            type="text"
            bind:value={scriptBrief}
            placeholder="e.g. tell me a joke about robots"
            onkeydown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault();
                draftScript();
              }
            }}
          />
          <button class="btn-sec" onclick={draftScript} disabled={drafting || !scriptBrief.trim()}>
            {drafting ? 'Thinking…' : '✨ Write me a script'}
          </button>
        </div>
      </label>
      <label>
        Script <span class="hint">— the exact words to speak (edit freely)</span>
        <textarea rows="4" bind:value={script} placeholder="Or paste a script here."></textarea>
        {#if script.trim()}
          <div class="idea-row">
            <button
              type="button"
              class="btn-sec"
              onclick={improveScript}
              disabled={improving}
              title="Rewrite the current script under the same rules, applying your note — keeps what works"
            >
              {improving ? 'Improving…' : '✨ Improve'}
            </button>
            <input
              type="text"
              bind:value={scriptInstruction}
              placeholder="what to change? (blank = general polish)"
              onkeydown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  improveScript();
                }
              }}
            />
          </div>
        {/if}
      </label>
      <div class="row">
        <label class="toggle">
          Voice
          {#if voiceCatalog.length > 0}
            <select bind:value={voice}>
              {#each voiceCatalog as v (v.id)}
                <option value={v.id}>{v.name}</option>
              {/each}
            </select>
          {:else}
            <input
              type="text"
              bind:value={voice}
              placeholder={voicesLoading ? 'loading voices…' : 'default'}
            />
          {/if}
        </label>
      </div>
    {:else}
      <FileDropzone
        accept="audio/*"
        bind:files={audioFiles}
        label={audioFile ? audioFile.name : 'Drop a wav / mp3 / m4a'}
        hint="Drag the handles to grab just the bit you want — no audio editor needed."
      />
      {#if audioDecodeError}
        <p class="err">Couldn't read that audio: {audioDecodeError}</p>
      {:else if audioFile && !audioSamples}
        <p class="muted">Decoding {audioFile.name}…</p>
      {:else if audioSamples}
        <div class="trim-block">
          <WaveformTrim
            samples={audioSamples}
            sampleRate={audioSampleRate}
            maxSeconds={Infinity}
            initialStart={trimStart}
            initialEnd={trimEnd}
            onchange={(s, e) => {
              trimStart = s;
              trimEnd = e;
            }}
          />
          <p class="muted">
            The window sets the avatar's wall; the whisper encoder resamples to 16 kHz mono
            internally, so any common format works.
          </p>
        </div>
      {/if}
    {/if}
  </div>

  <!-- Duration + Go -->
  <div class="card">
    <h3>Length</h3>
    {#if audioMode === 'upload'}
      {#if trimDurationSec > 0}
        <p class="muted">
          Selected window: <strong>{trimDurationSec.toFixed(1)} s</strong>
          → ~{roughEstimate()} render wall.
        </p>
      {:else if audioFile}
        <p class="muted">Decoding audio…</p>
      {:else}
        <p class="muted">Drop an audio file above, then window it.</p>
      {/if}
    {:else}
      <label>
        Duration: <strong>{durationSec.toFixed(1)}s</strong>
        <input type="number" min="2" step="0.5" bind:value={durationSec} />
        <span class="hint">est. wall {roughEstimate()}</span>
      </label>
    {/if}
    <div class="go-row">
      <button class="btn-primary" onclick={handleGo} disabled={running || drafting}>
        {running ? 'Rendering…' : 'Go'}
      </button>
      <QueueChip kind="avatar" bind:options={queueOpts} response={accepted} />
      {#if jobId}
        <button class="btn-sec" onclick={handleCancel} title="Drop this render"> Cancel </button>
        <span class="muted">
          {queued
            ? `Queued${queuedBecause ? ` — ${queuedBecause}` : ''}`
            : 'Rendering — safe to navigate away or reload.'}
        </span>
      {/if}
    </div>
  </div>

  <!-- Notices -->
  {#if error}
    <div class="card error">{error}</div>
  {/if}
  {#if notice}
    <div class="card notice">{notice}</div>
  {/if}
  {#if restoreNote}
    <p class="restore-note">{restoreNote}</p>
  {/if}

  <!-- Result -->
  {#if clipUrl && result?.clip}
    <div class="card">
      <h3>Result</h3>
      <!-- svelte-ignore a11y_media_has_caption -->
      <video src={clipUrl} controls autoplay loop playsinline></video>
      <p class="muted">
        {Math.round(result.clip.bytes / 1024)} KB
        {#if result.render_ms}· render {(result.render_ms / 1000).toFixed(1)}s{/if}
        {#if result.gate_wait_ms}· gate wait {(result.gate_wait_ms / 1000).toFixed(1)}s{/if}
      </p>
      {#if script.trim()}
        <details class="script-used">
          <summary>Script that was spoken</summary>
          <pre>{script}</pre>
        </details>
      {/if}
    </div>
  {/if}

  <!-- Advanced -->
  <details class="card advanced" bind:open={showAdvanced}>
    <summary>Advanced</summary>
    <div class="knobs">
      <label>
        Prompt <span class="hint">(default "a person talking")</span>
        <input type="text" bind:value={prompt} />
      </label>
      <div class="row">
        <label class="toggle">
          <span class="lbl">Segment frames</span>
          <span class="hint">PERF.md sweet spot 53</span>
          <input type="number" min="13" max="93" bind:value={segmentFrames} />
        </label>
        <label class="toggle">
          <span class="lbl">Cont cond frames</span>
          <span class="hint">overlap held between segments</span>
          <input type="number" min="0" max="40" bind:value={contCondFrames} />
        </label>
        <label class="toggle">
          <span class="lbl">Steps</span>
          <span class="hint">8 default; &lt;8 faster/softer</span>
          <input type="number" min="1" max="12" bind:value={steps} />
        </label>
        <label class="toggle">
          <span class="lbl">CFG scale</span>
          <span class="hint">distilled DMD — keep ~1.0</span>
          <input type="number" min="0" max="10" step="0.1" bind:value={cfgScale} />
        </label>
      </div>
      <div class="row">
        <label class="toggle">
          <span class="lbl">Resolution</span>
          <span class="hint">720p OOMs the 12GB card</span>
          <select bind:value={resolution}>
            <option value="480p">480p (480×832)</option>
            <option value="720p">720p (720×1248)</option>
          </select>
        </label>
        <label class="toggle">
          <span class="lbl">Offload</span>
          <span class="hint">on = ~5.7GB (single-seg only)</span>
          <select bind:value={offload}>
            <option value="true">true (prod default)</option>
            <option value="false">false (perf-bench)</option>
            <option value="auto">auto</option>
          </select>
        </label>
        <label class="toggle">
          <span class="lbl">Audio mouth scale</span>
          <span class="hint">viseme exaggeration</span>
          <input type="number" min="0" max="3" step="0.1" bind:value={audioMouthScale} />
        </label>
        <label class="toggle">
          <span class="lbl">Audio lowpass (Hz)</span>
          <span class="hint">0 = off; raise to soften jitter</span>
          <input type="number" min="0" max="8000" step="100" bind:value={audioLowpassHz} />
        </label>
      </div>
      <div class="row checks">
        <label class="check">
          <input type="checkbox" bind:checked={vaeTiling} />
          <span class="lbl">VAE tiling</span>
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={useSeed} />
          <span class="lbl">Fixed seed</span>
          <input type="number" min="0" bind:value={seed} disabled={!useSeed} class="inline-num" />
        </label>
      </div>
    </div>

    <!-- Perf knobs sub-panel -->
    <details class="perf-knobs" bind:open={showPerfKnobs}>
      <summary>Perf knobs (non-default, opt-in)</summary>
      <div class="knobs">
        <p class="hint">
          BSA bit-slice attention is the bit-exact bitmap config (radius=1 + self_frame + bitmap).
          Owner-approved; opt-in until default flips.
        </p>
        <label class="check">
          <input type="checkbox" bind:checked={bsaEnable} />
          <span class="lbl">Enable BSA</span>
        </label>
        {#if bsaEnable}
          <div class="row checks">
            <label class="toggle">
              <span class="lbl">Radius</span>
              <input type="number" min="1" max="3" bind:value={bsaRadius} />
            </label>
            <label class="check">
              <input type="checkbox" bind:checked={bsaSelfFrame} />
              <span class="lbl">self_frame</span>
            </label>
            <label class="check">
              <input type="checkbox" bind:checked={bsaBitmap} />
              <span class="lbl">bitmap (bit-exact path)</span>
            </label>
          </div>
        {/if}
        <label>
          Debug env <span class="hint">— LONGCAT_* / GGML_* / CUDA_* (one per line: KEY=VALUE)</span
          >
          <textarea rows="3" bind:value={debugEnvText} placeholder="LONGCAT_OP_PROFILE=1"
          ></textarea>
        </label>
      </div>
    </details>
  </details>

  <GenerationHistory
    service="avatar"
    refreshKey={historyKey}
    title="Clip history"
    onRestore={restore}
  />
</div>

<style>
  .page {
    max-width: 880px;
    margin: 24px auto;
    padding: 0 16px;
    color: var(--ink);
  }
  .header {
    margin-bottom: 16px;
  }
  .header h1 {
    margin: 0;
    font-size: 22px;
  }
  .muted {
    color: var(--muted);
    font-size: 12px;
    margin: 2px 0 0;
  }
  .err {
    color: var(--danger);
    font-size: 12px;
    margin: 6px 0 0;
  }
  .trim-block {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 10px;
    --wf-bg: var(--bg-2);
    --wf-active: var(--accent);
    --wf-inactive: var(--muted);
    --wf-region-bg: color-mix(in srgb, var(--accent) 14%, transparent);
    --wf-handle: var(--accent);
    --wf-handle-start: var(--accent);
    --wf-handle-end: var(--accent);
    --wf-btn-bg: var(--accent);
    --wf-fg: var(--muted);
    --wf-time-fg: var(--ink);
    --wf-duration: var(--accent);
  }
  .warn {
    color: var(--danger);
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
  /* Labeled input/select: label + hint stacked on top, control pinned to the
     bottom so every control in a row lines up even when some hints wrap. */
  label.toggle {
    display: flex;
    flex-direction: column;
    gap: 3px;
    align-items: stretch;
    color: var(--ink);
    font-size: 13px;
    margin-top: 0;
    flex: 1 1 150px;
    min-width: 140px;
  }
  label.toggle .lbl {
    font-weight: 500;
    color: var(--ink);
  }
  label.toggle input,
  label.toggle select {
    margin-top: auto;
  }
  /* Checkbox toggle: checkbox + label inline on one row (never stacked). */
  label.check {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 8px;
    margin-top: 0;
    min-width: 140px;
    color: var(--ink);
    font-size: 13px;
  }
  label.check input[type='checkbox'] {
    width: auto;
    margin: 0;
    flex: none;
  }
  label.check .lbl {
    font-weight: 500;
  }
  label.check .inline-num {
    width: 90px;
    margin-left: 4px;
  }
  .hint {
    color: var(--muted);
    font-weight: normal;
    font-size: 11px;
    line-height: 1.3;
  }
  .row {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
    align-items: stretch;
    margin-top: 8px;
  }
  .row.checks {
    align-items: center;
    gap: 24px;
  }
  .audio-mode-tabs {
    display: flex;
    gap: 4px;
    margin-bottom: 12px;
    padding: 4px;
    background: var(--bg-2);
    border-radius: 10px;
    border: 1px solid var(--ring);
  }
  .audio-mode-tabs .tab {
    flex: 1;
    background: transparent;
    border: none;
    color: var(--muted);
    padding: 8px 12px;
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
    transition:
      background 0.15s,
      color 0.15s;
  }
  .audio-mode-tabs .tab:hover {
    color: var(--ink);
  }
  .audio-mode-tabs .tab.active {
    background: var(--card);
    color: var(--ink);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.15);
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
  .btn-sec {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
    padding: 8px 12px;
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn-sec:disabled {
    opacity: 0.5;
    cursor: not-allowed;
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
  .go-row {
    display: flex;
    justify-content: center;
    margin-top: 16px;
  }
  .error {
    border-color: var(--danger);
    color: var(--danger);
  }
  .notice {
    border-color: var(--accent);
    color: var(--accent);
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
  video {
    width: 100%;
    max-height: 640px;
    border-radius: 12px;
    background: black;
  }
  .advanced > summary,
  .perf-knobs > summary {
    cursor: pointer;
    font-size: 13px;
    color: var(--muted);
  }
  .advanced[open] > summary,
  .perf-knobs[open] > summary {
    margin-bottom: 12px;
  }
  .perf-knobs {
    margin-top: 14px;
    border-top: 1px solid var(--ring);
    padding-top: 10px;
  }
  .knobs {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .script-used pre {
    margin: 8px 0 0;
    padding: 8px;
    background: var(--bg-2);
    border-radius: 8px;
    white-space: pre-wrap;
    font-size: 12px;
  }

  .save-offer {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 10px;
    padding: 8px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    font-size: 12px;
    color: var(--ink);
  }
  .save-offer span {
    flex: 1;
  }
  .link-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    font-size: 12px;
    text-decoration: underline;
  }

  .saved-row {
    margin-top: 14px;
  }
  .saved-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }
  .saved-scroll {
    display: flex;
    gap: 10px;
    overflow-x: auto;
    padding: 4px 2px 10px;
  }
  .saved-tile {
    position: relative;
    flex: 0 0 92px;
    width: 92px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 10px;
    padding: 6px 6px 8px;
    color: var(--ink);
    cursor: pointer;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
  }
  .saved-tile.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent-dim);
  }
  .saved-tile img {
    width: 80px;
    height: 80px;
    object-fit: cover;
    border-radius: 6px;
    display: block;
  }
  .saved-tile-placeholder {
    width: 80px;
    height: 80px;
    background: var(--ring);
    border-radius: 6px;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--muted);
    font-size: 18px;
  }
  .saved-tile-name {
    font-size: 11px;
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 80px;
  }
  .saved-tile-remove {
    position: absolute;
    top: 2px;
    right: 2px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: rgba(0, 0, 0, 0.4);
    color: white;
    border: none;
    cursor: pointer;
    font-size: 14px;
    line-height: 1;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  /* Crop tool (upload path) */
  .crop-tool {
    margin-top: 14px;
  }
  .orient-toggle {
    display: inline-flex;
    gap: 4px;
    padding: 4px;
    margin-bottom: 8px;
    background: var(--bg-2);
    border-radius: 10px;
    border: 1px solid var(--ring);
  }
  .orient-toggle button {
    background: transparent;
    border: none;
    color: var(--muted);
    padding: 6px 14px;
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
    transition:
      background 0.15s,
      color 0.15s;
  }
  .orient-toggle button:hover {
    color: var(--ink);
  }
  .orient-toggle button.active {
    background: var(--card);
    color: var(--ink);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.15);
  }
  .crop-help {
    font-size: 12px;
    color: var(--muted);
    margin-bottom: 8px;
    line-height: 1.4;
  }
  .crop-stage {
    position: relative;
    display: inline-block;
    max-width: 100%;
    border-radius: 10px;
    overflow: hidden;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    line-height: 0;
    /* Touch devices: let our pointer handlers own the gesture. */
    touch-action: none;
  }
  .crop-img {
    display: block;
    max-width: 100%;
    max-height: 420px;
    width: auto;
    height: auto;
    user-select: none;
    -webkit-user-drag: none;
  }
  .crop-box {
    position: absolute;
    box-sizing: border-box;
    border: 2px solid var(--accent);
    /* Dim everything outside the box. */
    box-shadow: 0 0 0 100vmax rgba(0, 0, 0, 0.55);
    cursor: move;
    touch-action: none;
  }
  .crop-handle {
    position: absolute;
    width: 14px;
    height: 14px;
    background: var(--accent);
    border: 2px solid white;
    border-radius: 3px;
    box-sizing: border-box;
    touch-action: none;
  }
  .crop-handle.nw {
    top: -8px;
    left: -8px;
    cursor: nwse-resize;
  }
  .crop-handle.ne {
    top: -8px;
    right: -8px;
    cursor: nesw-resize;
  }
  .crop-handle.sw {
    bottom: -8px;
    left: -8px;
    cursor: nesw-resize;
  }
  .crop-handle.se {
    bottom: -8px;
    right: -8px;
    cursor: nwse-resize;
  }
</style>
