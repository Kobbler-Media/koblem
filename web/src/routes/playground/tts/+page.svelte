<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import {
    ttsGenerate,
    ttsGenerateStreamingFa,
    listTtsVoices,
    uploadTtsVoice,
    deleteTtsVoice,
    downloadTtsVoiceSample,
    downloadTtsVoiceRefText,
    sttTranscribe,
    searchAudiobooks,
    getAudiobook,
    fetchAudiobookClipWav,
    getHealth,
    getSettings
  } from '#lib/api/client.ts';
  import {
    blobToMonoSamples,
    createFaPlayback,
    detectBestWindow,
    encodeWavMono16,
    FaTranscript,
    fileListWith,
    sliceSamples,
    WaveformTrim,
    type FaPlayback
  } from '@koblibs/ui';
  import type {
    TtsGenerateRequest,
    TtsVoice,
    AudiobookSummary,
    AudiobookDetail
  } from '#lib/api/client.ts';
  import FileDropzone from '#lib/components/FileDropzone.svelte';
  import GenerationHistory from '#lib/components/GenerationHistory.svelte';
  import type { Generation } from '#lib/types.ts';
  import { str, num, bool } from '#lib/restore.ts';

  let historyKey = $state(0);
  let restoreNote = $state('');

  // Repopulate the form from a saved TTS generation (stored req is 1:1).
  function restore(g: Generation) {
    const m = g.request_meta;
    const inp = str(m, 'input');
    if (inp !== undefined) input = inp;
    const v = str(m, 'voice');
    if (v !== undefined) qVoiceId = v;
    const fmt = str(m, 'response_format');
    if (fmt !== undefined) qFormat = fmt as typeof qFormat;
    const instr = str(m, 'instructions');
    if (instr !== undefined) qInstructions = instr;
    const br = num(m, 'bitrate_kbps');
    if (br !== undefined) qBitrate = br;
    const temp = num(m, 'temperature');
    if (temp !== undefined) qTemperature = temp;
    const tp = num(m, 'top_p');
    if (tp !== undefined) qTopP = tp;
    const tk = num(m, 'top_k');
    if (tk !== undefined) qTopK = tk;
    const rp = num(m, 'repetition_penalty');
    if (rp !== undefined) qRepPenalty = rp;
    const sd = num(m, 'seed');
    if (sd !== undefined) qSeed = sd;
    const mnf = num(m, 'max_new_frames');
    if (mnf !== undefined) qMaxNewFrames = mnf;
    const spd = num(m, 'speed');
    if (spd !== undefined) qSpeed = spd;
    const sp = str(m, 'speaker');
    if (sp !== undefined) qSpeaker = sp;
    const lg = bool(m, 'long');
    if (lg !== undefined) qLong = lg;
    const cw = num(m, 'chunk_words');
    if (cw !== undefined) qChunkWords = cw;
    const sbs = num(m, 'stream_batch_size');
    if (sbs !== undefined) qStreamBatchSize = sbs;
    const sfbs = num(m, 'stream_first_batch_size');
    if (sfbs !== undefined) qStreamFirstBatchSize = sfbs;
    const ss = bool(m, 'save_sample');
    if (ss !== undefined) saveSample = ss;
    restoreNote = '';
    qError = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  // ── Shared ────────────────────────────────────────────────────────────
  let saveSample = $state(true);
  let input = $state('Hello, world. This is a TTS test from koblem.');

  // ── Engine state ──────────────────────────────────────────────────────
  let ttsModel = $state('');
  let ttsModelId = $state('');

  let qVoices = $state<TtsVoice[]>([]);
  // An empty id means "no voice" — the engine picks one and holds it for the
  // request. There is no "default" clone to name, so this is a real option
  // rather than a placeholder.
  let qVoicesView = $derived(qVoices);
  // Built-ins (`default` — the model's own narrator) are selectable but have no
  // reference clip to download and cannot be deleted, so the management list
  // covers clones only. Keyed on the engine's `builtin` flag rather than the id,
  // so a second built-in would not show up as a broken clone row.
  let clonedVoices = $derived(qVoices.filter((v) => !v.builtin));
  let qVoiceId = $state('');
  let qInstructions = $state('');

  /// The tokenizer's built-in speakers. A closed, authoritative list.
  const SPEAKERS = ['S0', 'S1', 'S2', 'S3', 'S4', 'S5', 'S6', 'S7', 'S8', 'S9'];
  let qSpeaker = $state('S0');

  /// Vocal events are plain text, not tokens — there is no vocabulary to
  /// validate against, and an unrecognised one is READ ALOUD rather than
  /// performed. Nothing downstream knows more than this list does: the engine
  /// passes them through to the text encoder untouched, and upstream (model
  /// card + the reference PyTorch repo) names these four with "such as" and
  /// publishes no closed set. So this is a shortcut bar, not a palette — do not
  /// go looking for the authoritative list, there isn't one.
  const VOCAL_EVENTS = ['(laugh)', '(cough)', '(clears throat)', '(sigh)'];

  let qFormat = $state<'wav' | 'pcm' | 'mp3'>('wav');
  let qBitrate = $state<number | null>(64);
  let qTemperature = $state<number | null>(0.9);
  let qTopP = $state<number | null>(1.0);
  let qTopK = $state<number | null>(50);
  let qRepPenalty = $state<number | null>(null);
  let qSeed = $state<number | null>(null);
  // FRAMES at 12.5 Hz, not audio tokens. Left unset on purpose: raising it does
  // not buy longer output, it turns a silent truncation into a context
  // overflow. Long-form mode is the answer to long text.
  let qMaxNewFrames = $state<number | null>(null);
  /// Requested render rate. `null` = natural. This is a real re-render, not a
  /// player knob: changing it means synthesising again.
  let qSpeed = $state<number | null>(null);
  // Long-form is the engine default and the reason a whole document keeps one
  // voice. Turning it off truncates anything past ~60 s.
  let qLong = $state(true);
  let qChunkWords = $state<number | null>(null);
  let qStreamBatchSize = $state<number | null>(null);
  let qStreamFirstBatchSize = $state<number | null>(1);
  // Browser <audio> can't play raw PCM (no header).
  let qPlaybackHint = $derived(
    qFormat === 'pcm' ? 'PCM has no header — browser playback will fail. Download to inspect.' : ''
  );

  function insertVocalEvent(ev: string) {
    input = input ? `${input.trimEnd()} ${ev} ` : `${ev} `;
  }

  // ── Voice clone flow ──────────────────────────────────────────────────
  let qUploadName = $state('');
  let qUploadFile = $state<FileList | null>(null);
  let qUploadRefText = $state('');
  let qUploading = $state(false);
  let qDeletingVoice = $state<string | null>(null);

  /// Single-source picker for the "add a new voice" section. Only one of
  /// these sub-panels is rendered at a time so the page isn't a wall of
  /// controls.
  type QVoiceSource = 'record' | 'upload' | 'audiobook';
  let qVoiceSource = $state<QVoiceSource>('audiobook');

  // Library-audiobook clone source: pick book + chapter → koblem fetches a
  // 60s preview WAV → browser decodes + draws a waveform → user drags a
  // ≤10s trim window with ms precision → browser builds the trimmed WAV and
  // uploads via the active engine's clone path, optionally STT'ing the
  // trimmed bytes for ICL ref_text first.
  const LIB_PREVIEW_SECONDS = 60;
  let qLibName = $state('');
  let qLibSearch = $state('');
  let qLibResults = $state<AudiobookSummary[]>([]);
  let qLibSelectedBook = $state<AudiobookDetail | null>(null);
  let qLibChapterIdx = $state<number | null>(null);
  let qLibPreviewOffset = $state(0);
  let qLibSamples: Float32Array | null = $state(null);
  let qLibSampleRate = $state(0);
  let qLibTrimStart = $state(0);
  let qLibTrimEnd = $state(0);
  let qLibFetchingPreview = $state(false);
  let qLibTranscribe = $state(true);
  let qLibUploading = $state(false);
  let qLibError = $state('');

  // Mic-record clone flow: record (max 30s, live volume meter) → decode →
  // auto-pick best 10s window → user adjusts trim handles → encode WAV +
  // transcribe → upload clone.
  const REC_MAX_SECONDS = 30;
  const TRIM_MAX_SECONDS = 10;
  let qRecState = $state<'idle' | 'recording' | 'processing' | 'trimming'>('idle');
  let qRecError = $state('');
  let qRecElapsedMs = $state(0);
  let qRecVolume = $state(0); // 0..1, UI bar
  let qRecApplying = $state(false);
  let qRecorder: MediaRecorder | null = null;
  let qRecChunks: Blob[] = [];
  let qRecStream: MediaStream | null = null;
  let qRecStartedAt = 0;
  let qRecTimer: ReturnType<typeof setInterval> | null = null;
  let qRecMaxStopTimer: ReturnType<typeof setTimeout> | null = null;
  let qRecMeterCtx: AudioContext | null = null;
  let qRecMeterRaf = 0;
  let qRecSamples: Float32Array | null = $state(null);
  let qRecSampleRate = $state(0);
  let qRecTrimStart = $state(0);
  let qRecTrimEnd = $state(0);

  let qGenerating = $state(false);
  let qAudioUrl = $state('');
  let qResultMeta = $state<{
    gateWaitMs?: number;
    ttfaMs?: number;
    /// Wall ms from Generate click to the audio element actually starting
    /// playback (the `playing` event). On non-mp3 formats this includes the
    /// buffer-then-play wait since MSE only streams mp3 in Chrome.
    ttfpMs?: number;
    totalMs?: number;
    audioDurationMs?: number;
    rtf?: number;
    sampleId?: number;
    firstAlignMs?: number;
    audioMs?: number;
    /// What the render actually ran at. Differs from `qSpeed` when the engine
    /// refused the requested rate and fell back.
    speedApplied?: number;
  } | null>(null);
  let qAudioEl = $state<HTMLAudioElement | null>(null);
  let qPlayListener: (() => void) | null = null;
  let qError = $state('');

  // ── Forced-alignment mode (dev tool, default OFF) ─────────────────────
  // When on, requests SSE+PCM+align from tier-2, shows live word highlight
  // and per-word timings. Bypasses the mp3-MSE / blob playback paths.
  let qFaEnabled = $state(false);
  let qFa = $state<FaPlayback | null>(null);
  let qShowWordTable = $state(false);

  // Engine-aware download filename for the result card.
  let downloadName = $derived(`tts.${qFormat}`);

  onMount(async () => {
    const [ttsVoicesRes, healthRes, settingsRes] = await Promise.allSettled([
      listTtsVoices(),
      getHealth(),
      getSettings()
    ]);
    if (ttsVoicesRes.status === 'fulfilled') {
      qVoices = ttsVoicesRes.value.voices;
      ttsModelId = ttsVoicesRes.value.model_id || '';
    }
    if (healthRes.status === 'fulfilled') {
      ttsModel = healthRes.value.tts_model || '';
    }
    if (settingsRes.status === 'fulfilled' && settingsRes.value.gpu_sample_save_default != null) {
      saveSample = Boolean(settingsRes.value.gpu_sample_save_default);
    }
  });

  onDestroy(() => {
    qFa?.stop();
    qFa = null;
    if (qAudioUrl) URL.revokeObjectURL(qAudioUrl);
  });

  // ── Knob helper ───────────────────────────────────────────────────────
  function parseKnobValue(raw: string): number | null {
    if (raw === '') return null;
    const n = Number(raw);
    return Number.isFinite(n) ? n : null;
  }

  // ── Voice management ──────────────────────────────────────────────────
  async function qRefreshVoices() {
    try {
      const res = await listTtsVoices();
      qVoices = res.voices;
      ttsModelId = res.model_id || ttsModelId;
    } catch {
      // best-effort
    }
  }
  function uploadActiveVoice(name: string, file: File, refText?: string) {
    return uploadTtsVoice(name, file, refText);
  }
  function selectClonedVoice(name: string) {
    qVoiceId = name;
  }

  async function qHandleUpload() {
    if (!qUploadFile || !qUploadFile.length || !qUploadName.trim()) return;
    qUploading = true;
    try {
      await uploadActiveVoice(
        qUploadName.trim(),
        qUploadFile[0],
        qUploadRefText.trim() || undefined
      );
      const cloned = qUploadName.trim();
      qUploadName = '';
      qUploadFile = null;
      qUploadRefText = '';
      // If a recording-derived clip was the source, drop the trim/preview
      // state too so the section returns to idle for the next clone.
      if (qRecState === 'trimming' || qRecSamples) {
        qResetRecordingState();
        qRecState = 'idle';
      }
      await qRefreshVoices();
      selectClonedVoice(cloned);
    } catch (e) {
      qError = e instanceof Error ? e.message : 'Upload failed';
    } finally {
      qUploading = false;
    }
  }
  function qResetRecorderTeardown() {
    qRecStream?.getTracks().forEach((t) => t.stop());
    qRecStream = null;
    if (qRecTimer != null) {
      clearInterval(qRecTimer);
      qRecTimer = null;
    }
    if (qRecMaxStopTimer != null) {
      clearTimeout(qRecMaxStopTimer);
      qRecMaxStopTimer = null;
    }
    if (qRecMeterRaf) {
      cancelAnimationFrame(qRecMeterRaf);
      qRecMeterRaf = 0;
    }
    qRecMeterCtx?.close().catch(() => {});
    qRecMeterCtx = null;
    qRecVolume = 0;
  }

  function qResetRecordingState() {
    qResetRecorderTeardown();
    qRecChunks = [];
    qRecorder = null;
    qRecSamples = null;
    qRecSampleRate = 0;
    qRecTrimStart = 0;
    qRecTrimEnd = 0;
    qRecError = '';
    qRecElapsedMs = 0;
    qRecApplying = false;
  }

  async function qStartRecording() {
    qResetRecordingState();
    try {
      qRecStream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch (e) {
      qRecError = e instanceof Error ? `Mic access failed: ${e.message}` : 'Mic access failed';
      return;
    }
    qRecChunks = [];
    qRecorder = new MediaRecorder(qRecStream);
    qRecorder.ondataavailable = (ev) => {
      if (ev.data && ev.data.size > 0) qRecChunks.push(ev.data);
    };
    qRecorder.onstop = () => {
      const recordedMime = qRecorder?.mimeType || 'audio/webm';
      const recorded = new Blob(qRecChunks, { type: recordedMime });
      qRecChunks = [];
      qRecorder = null;
      qResetRecorderTeardown();
      void qFinaliseRecording(recorded);
    };

    // Volume meter — peak amplitude per frame, smoothed with EMA.
    try {
      qRecMeterCtx = new AudioContext();
      const src = qRecMeterCtx.createMediaStreamSource(qRecStream);
      const analyser = qRecMeterCtx.createAnalyser();
      analyser.fftSize = 1024;
      analyser.smoothingTimeConstant = 0.4;
      src.connect(analyser);
      const buf = new Uint8Array(analyser.fftSize);
      const tick = () => {
        analyser.getByteTimeDomainData(buf);
        let peak = 0;
        for (let i = 0; i < buf.length; i++) {
          const v = Math.abs(buf[i] - 128) / 128;
          if (v > peak) peak = v;
        }
        qRecVolume = qRecVolume * 0.6 + peak * 0.4;
        qRecMeterRaf = requestAnimationFrame(tick);
      };
      qRecMeterRaf = requestAnimationFrame(tick);
    } catch {
      // Volume meter is non-critical — continue without it.
    }

    qRecStartedAt = performance.now();
    qRecElapsedMs = 0;
    qRecTimer = setInterval(() => {
      qRecElapsedMs = performance.now() - qRecStartedAt;
    }, 100);
    // Hard cap — auto-stop after REC_MAX_SECONDS.
    qRecMaxStopTimer = setTimeout(() => qStopRecording(), REC_MAX_SECONDS * 1000);

    qRecorder.start();
    qRecState = 'recording';
  }

  function qStopRecording() {
    if (qRecorder && qRecorder.state !== 'inactive') {
      qRecorder.stop();
      qRecState = 'processing';
    }
  }

  async function qFinaliseRecording(recorded: Blob) {
    try {
      const { samples, sampleRate } = await blobToMonoSamples(recorded);
      qRecSamples = samples;
      qRecSampleRate = sampleRate;
      const { start, end } = detectBestWindow(samples, sampleRate, TRIM_MAX_SECONDS);
      qRecTrimStart = start;
      qRecTrimEnd = end;
      qRecState = 'trimming';
    } catch (e) {
      qRecError = e instanceof Error ? `Audio decode failed: ${e.message}` : 'Audio decode failed';
      qRecState = 'idle';
    }
  }

  async function qApplyTrim() {
    if (!qRecSamples) return;
    qRecApplying = true;
    qRecError = '';
    try {
      const trimmed = sliceSamples(qRecSamples, qRecTrimStart, qRecTrimEnd);
      const wav = encodeWavMono16(trimmed, qRecSampleRate);
      const baseName = qUploadName.trim() || 'recorded-voice';
      const wavFile = new File([wav], `${baseName}.wav`, { type: 'audio/wav' });
      qUploadFile = fileListWith(wavFile);
      try {
        const result = await sttTranscribe(wavFile, {});
        qUploadRefText = result.text.trim();
      } catch (e) {
        qRecError = e instanceof Error ? `Transcribe failed: ${e.message}` : 'Transcribe failed';
      }
    } catch (e) {
      qRecError = e instanceof Error ? `Trim failed: ${e.message}` : 'Trim failed';
    } finally {
      qRecApplying = false;
    }
  }

  function qRetake() {
    qResetRecordingState();
    qRecState = 'idle';
    void qStartRecording();
  }

  function qDiscardRecording() {
    qResetRecordingState();
    qRecState = 'idle';
  }

  function qTriggerBrowserDownload(blob: Blob, filename: string) {
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    setTimeout(() => {
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
    }, 100);
  }

  async function qDownloadVoiceSample(name: string) {
    qError = '';
    try {
      const blob = await downloadTtsVoiceSample(name);
      qTriggerBrowserDownload(blob, `${name}.wav`);
    } catch (e) {
      qError = e instanceof Error ? `Download failed: ${e.message}` : 'Download failed';
    }
  }

  async function qDownloadVoiceRefText(name: string) {
    qError = '';
    try {
      const text = await downloadTtsVoiceRefText(name);
      qTriggerBrowserDownload(
        new Blob([text], { type: 'text/plain;charset=utf-8' }),
        `${name}-ref.txt`
      );
    } catch (e) {
      qError = e instanceof Error ? `Download failed: ${e.message}` : 'Download failed';
    }
  }

  async function qLibSearch_handler() {
    if (!qLibSearch.trim()) return;
    qLibError = '';
    try {
      qLibResults = await searchAudiobooks(qLibSearch.trim());
    } catch (e) {
      qLibError = e instanceof Error ? e.message : 'Search failed';
    }
  }

  async function qLibPickBook(id: string) {
    qLibError = '';
    qLibSamples = null;
    qLibChapterIdx = null;
    try {
      qLibSelectedBook = await getAudiobook(id);
      // Auto-pick the second chapter when there are ≥3 (intro is usually
      // chapter 0). Don't fetch the preview yet — wait for an explicit click
      // so the user knows which chapter is being downloaded.
      const defaultIdx = qLibSelectedBook && qLibSelectedBook.chapters.length >= 3 ? 1 : 0;
      void qLibPickChapter(defaultIdx);
    } catch (e) {
      qLibError = e instanceof Error ? e.message : 'Pick failed';
    }
  }

  /// Load a 60s preview at offset 30s (skip credits / opening boilerplate).
  /// Caller can re-pan by calling qLibFetchPreviewAt with a different offset.
  async function qLibPickChapter(idx: number) {
    if (!qLibSelectedBook) return;
    qLibChapterIdx = idx;
    const ch = qLibSelectedBook.chapters[idx];
    // 30s skip is a compromise: long chapters won't end in credits, but
    // very short chapters (intro) should start at 0 so we don't run off
    // the end. Cap at duration - preview when known.
    const startOffset =
      ch?.duration_seconds != null
        ? Math.max(0, Math.min(30, ch.duration_seconds - LIB_PREVIEW_SECONDS))
        : 30;
    await qLibFetchPreviewAt(startOffset);
  }

  async function qLibFetchPreviewAt(offsetSeconds: number) {
    if (!qLibSelectedBook || qLibChapterIdx == null) return;
    qLibFetchingPreview = true;
    qLibError = '';
    try {
      const blob = await fetchAudiobookClipWav(
        qLibSelectedBook.id,
        qLibChapterIdx,
        offsetSeconds,
        LIB_PREVIEW_SECONDS
      );
      const { samples, sampleRate } = await blobToMonoSamples(blob);
      qLibPreviewOffset = offsetSeconds;
      qLibSamples = samples;
      qLibSampleRate = sampleRate;
      const { start, end } = detectBestWindow(samples, sampleRate, TRIM_MAX_SECONDS);
      qLibTrimStart = start;
      qLibTrimEnd = end;
    } catch (e) {
      qLibError = e instanceof Error ? e.message : 'Preview fetch failed';
    } finally {
      qLibFetchingPreview = false;
    }
  }

  async function qLibUploadClone() {
    if (!qLibName.trim() || !qLibSamples || !qLibSelectedBook) return;
    qLibUploading = true;
    qLibError = '';
    try {
      const trimmed = sliceSamples(qLibSamples, qLibTrimStart, qLibTrimEnd);
      const wav = encodeWavMono16(trimmed, qLibSampleRate);
      const baseName = qLibName.trim();
      const wavFile = new File([wav], `${baseName}.wav`, { type: 'audio/wav' });

      let refText: string | undefined;
      if (qLibTranscribe) {
        try {
          const result = await sttTranscribe(wavFile, {});
          refText = result.text.trim();
        } catch (e) {
          qLibError = e instanceof Error ? `Transcribe failed: ${e.message}` : 'Transcribe failed';
        }
      }

      await uploadActiveVoice(baseName, wavFile, refText);
      qLibName = '';
      qLibSelectedBook = null;
      qLibResults = [];
      qLibSearch = '';
      qLibSamples = null;
      qLibChapterIdx = null;
      await qRefreshVoices();
      selectClonedVoice(baseName);
    } catch (e) {
      qLibError = e instanceof Error ? e.message : 'Upload failed';
    } finally {
      qLibUploading = false;
    }
  }

  async function qHandleDelete(name: string) {
    if (name === 'default') return;
    if (!confirm(`Delete voice "${name}"?`)) return;
    qDeletingVoice = name;
    try {
      await deleteTtsVoice(name);
      if (qVoiceId === name) qVoiceId = 'default';
      await qRefreshVoices();
    } catch (e) {
      qError = e instanceof Error ? e.message : 'Delete failed';
    } finally {
      qDeletingVoice = null;
    }
  }
  /// A refused rate is not an error — the engine falls back and returns real
  /// audio — but it is the one outcome that looks exactly like success, so the
  /// requested value is snapped back to what was rendered rather than left
  /// standing as a claim the audio doesn't support.
  function noteRefusedSpeed(applied: number | undefined) {
    if (applied == null || !Number.isFinite(applied)) return;
    const requested = qSpeed ?? 1;
    if (Math.abs(applied - requested) < 1e-9) return;
    qError = `Engine refused speed ${requested}x — rendered at ${applied}x.`;
    qSpeed = applied === 1 ? null : applied;
  }

  async function qHandleGenerate() {
    qGenerating = true;
    qError = '';
    if (qAudioUrl) URL.revokeObjectURL(qAudioUrl);
    qAudioUrl = '';
    qResultMeta = null;
    qFa?.stop();
    qFa = null;

    const generateStart = performance.now();

    const req: TtsGenerateRequest = {
      input,
      save_sample: saveSample
    };
    // An empty voice is meaningful: the engine picks one and holds it.
    if (qVoiceId) req.voice = qVoiceId;
    if (qSpeaker && qSpeaker !== 'S0') req.speaker = qSpeaker;
    if (qInstructions.trim()) {
      req.instructions = qInstructions.trim();
    }
    if (qTemperature != null) req.temperature = qTemperature;
    if (qTopP != null) req.top_p = qTopP;
    if (qTopK != null) req.top_k = qTopK;
    if (qRepPenalty != null) req.repetition_penalty = qRepPenalty;
    if (qSeed != null) req.seed = qSeed;
    if (qMaxNewFrames != null) req.max_new_frames = qMaxNewFrames;
    if (qSpeed != null && qSpeed !== 1) req.speed = qSpeed;
    // Only send `long` when turning it OFF — it is the server default.
    if (!qLong) req.long = false;
    if (qChunkWords != null) req.chunk_words = qChunkWords;
    // Their labels say "alignment path only" and that is now literal: the
    // buffered path strips them server-side, because the engine reads a batch
    // knob with no transport as "stream this" and answers CHUNKED.
    if (qStreamBatchSize != null) req.stream_batch_size = qStreamBatchSize;
    if (qStreamFirstBatchSize != null) req.stream_first_batch_size = qStreamFirstBatchSize;

    if (qFaEnabled) {
      try {
        const abortController = new AbortController();
        const { events, gateWaitMs, sampleId, speedApplied } = await ttsGenerateStreamingFa(req, {
          signal: abortController.signal
        });
        noteRefusedSpeed(speedApplied);
        const fa = createFaPlayback({
          events,
          text: input,
          abortController,
          onError: (m) => (qError = m)
        });
        qFa = fa;
        qResultMeta = { gateWaitMs, sampleId, speedApplied };
        await fa.start();
        qResultMeta = {
          ...(qResultMeta ?? {}),
          ttfaMs: fa.timings.ttfaMs,
          firstAlignMs: fa.timings.firstAlignMs,
          audioMs: fa.timings.audioMs,
          totalMs: performance.now() - generateStart
        };
        historyKey++;
      } catch (e) {
        qError = e instanceof Error ? e.message : 'Generate failed';
      } finally {
        qGenerating = false;
      }
      return;
    }

    // Buffered path: ask for a container the browser can play back directly.
    // Streaming lives on the forced-alignment path above, which is the only
    // streaming transport the engine documents (SSE + PCM).
    req.stream_format = 'none';
    req.response_format = qFormat;
    if (qFormat === 'mp3' && qBitrate != null) {
      req.bitrate_kbps = qBitrate;
    }

    if (qPlayListener && qAudioEl) {
      qAudioEl.removeEventListener('playing', qPlayListener);
    }
    qPlayListener = () => {
      const ttfpMs = performance.now() - generateStart;
      qResultMeta = qResultMeta ? { ...qResultMeta, ttfpMs } : { ttfpMs };
    };
    qAudioEl?.addEventListener('playing', qPlayListener, { once: true });

    try {
      const result = await ttsGenerate(req);
      noteRefusedSpeed(result.speedApplied);
      if (result.truncated) {
        // Plays fine, just stops mid-sentence — the one failure that looks
        // exactly like success. Non-fatal: the clip is still shown.
        qError = 'TTS stopped early — the text was truncated. Turn long-form back on.';
      }
      qAudioUrl = URL.createObjectURL(result.audio);
      requestAnimationFrame(() => {
        qAudioEl?.play().catch(() => {});
      });
      qResultMeta = {
        gateWaitMs: result.gateWaitMs,
        ttfaMs: result.ttfaMs,
        totalMs: result.totalMs,
        sampleId: result.sampleId,
        speedApplied: result.speedApplied
      };
      historyKey++;
    } catch (e) {
      qError = e instanceof Error ? e.message : 'Generate failed';
    } finally {
      qGenerating = false;
    }
  }

  function qHandleAudioMetadata() {
    if (!qAudioEl || !qResultMeta) return;
    const d = qAudioEl.duration;
    if (!Number.isFinite(d) || d <= 0) return;
    const audioDurationMs = Math.round(d * 1000);
    const synthMs = (qResultMeta.totalMs ?? 0) - (qResultMeta.gateWaitMs ?? 0);
    const rtf = synthMs > 0 ? synthMs / audioDurationMs : undefined;
    qResultMeta = { ...qResultMeta, audioDurationMs, rtf };
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>TTS Playground</h2>
    <div class="model-line">
      {#if ttsModel}
        <span class="muted">Model: <code>{ttsModel}</code></span>
        {#if ttsModelId}<span class="muted small"> · id: <code>{ttsModelId}</code></span>{/if}
      {/if}
    </div>
  </div>

  <div class="card">
    <h3>Input</h3>

    <textarea rows="4" bind:value={input} placeholder="Text to speak…"></textarea>

    <div class="vocal-events">
      <span class="small muted">Insert</span>
      {#each VOCAL_EVENTS as ev (ev)}
        <button type="button" class="event-chip" onclick={() => insertVocalEvent(ev)}>{ev}</button>
      {/each}
      <span class="hint">
        Performed cues, not tokens: there is no vocabulary to pick from, and an unrecognised cue is
        read aloud rather than performed. These four are the ones the model card names — any
        parenthesised cue is worth a try. Chinese text uses brackets instead: <code>[笑]</code>.
      </span>
    </div>

    <label>
      Voice
      <select bind:value={qVoiceId}>
        {#each qVoicesView as v (v.id)}
          <option value={v.id}>{v.name}</option>
        {/each}
      </select>
      <span class="hint">
        A saved clone holds one identity across the whole passage. Without one the engine picks a
        voice per request — consistent within a render, different between them.
      </span>
    </label>

    <label>
      Speaker
      <select bind:value={qSpeaker}>
        {#each SPEAKERS as sp (sp)}
          <option value={sp}>{sp}</option>
        {/each}
      </select>
      <span class="hint">
        Built-in narrator token. Use it to change the default voice; for a stable one, save a clone.
      </span>
    </label>

    <label>
      Delivery instructions
      <textarea rows="2" bind:value={qInstructions} placeholder="e.g. Warm and gentle, unhurried."
      ></textarea>
      <span class="hint">
        Description-driven prosody / style. With a cloned voice it steers delivery and the clone
        keeps its identity.
      </span>
    </label>

    <h3 class="subhead">Add a new voice</h3>
    <div class="source-pills" role="tablist" aria-label="Voice source">
      <button
        type="button"
        class="source-pill"
        class:active={qVoiceSource === 'audiobook'}
        role="tab"
        aria-selected={qVoiceSource === 'audiobook'}
        onclick={() => (qVoiceSource = 'audiobook')}
      >
        📚 From audiobook
      </button>
      <button
        type="button"
        class="source-pill"
        class:active={qVoiceSource === 'record'}
        role="tab"
        aria-selected={qVoiceSource === 'record'}
        onclick={() => (qVoiceSource = 'record')}
      >
        🎙️ Record from mic
      </button>
      <button
        type="button"
        class="source-pill"
        class:active={qVoiceSource === 'upload'}
        role="tab"
        aria-selected={qVoiceSource === 'upload'}
        onclick={() => (qVoiceSource = 'upload')}
      >
        ⬆️ Upload WAV
      </button>
    </div>

    {#if qVoiceSource === 'upload'}
      <div class="upload">
        <input type="text" bind:value={qUploadName} placeholder="new-voice-name" />
        <FileDropzone
          accept="audio/*"
          bind:files={qUploadFile}
          label="Drop reference clip"
          hint="WAV or compatible audio"
        />
        <button
          class="btn-primary"
          disabled={qUploading || !qUploadName.trim() || !qUploadFile?.length}
          onclick={qHandleUpload}
        >
          {qUploading ? 'Uploading…' : 'Upload clone'}
        </button>
      </div>
      <label>
        ref_text (optional — boosts ICL fidelity)
        <textarea
          rows="2"
          bind:value={qUploadRefText}
          placeholder="Known transcript of the reference clip"></textarea>
      </label>
    {/if}

    {#if qVoiceSource === 'record'}
      <div class="rec-section">
        {#if qRecState === 'idle'}
          <div class="rec-row">
            <button class="btn-sec rec-btn" onclick={qStartRecording}>
              <span class="rec-dot"></span> Record from mic
            </button>
            <span class="hint">
              Up to {REC_MAX_SECONDS}s. After stopping, pick a ≤{TRIM_MAX_SECONDS}s window —
              auto-detected, draggable. Then transcribe + Upload clone.
            </span>
          </div>
        {:else if qRecState === 'recording'}
          <div class="rec-row">
            <button class="btn-sec rec-btn recording" onclick={qStopRecording}>
              <span class="rec-dot pulsing"></span> Stop ({(qRecElapsedMs / 1000).toFixed(1)}s / {REC_MAX_SECONDS}s)
            </button>
            <div class="vol-meter" aria-hidden="true">
              <div class="vol-fill" style="width: {Math.min(100, qRecVolume * 140)}%"></div>
            </div>
            <span class="hint">Auto-stops at {REC_MAX_SECONDS}s.</span>
          </div>
        {:else if qRecState === 'processing'}
          <div class="rec-row">
            <button class="btn-sec rec-btn" disabled>Processing…</button>
            <span class="hint">Decoding + finding the best window.</span>
          </div>
        {:else if qRecSamples}
          <!-- trimming -->
          <div class="trim-block">
            <WaveformTrim
              samples={qRecSamples}
              sampleRate={qRecSampleRate}
              maxSeconds={TRIM_MAX_SECONDS}
              initialStart={qRecTrimStart}
              initialEnd={qRecTrimEnd}
              onchange={(s, e) => {
                qRecTrimStart = s;
                qRecTrimEnd = e;
              }}
            />
            <div class="trim-actions">
              <input
                type="text"
                bind:value={qUploadName}
                placeholder="new-voice-name"
                class="trim-name"
              />
              <button class="btn-primary" onclick={qApplyTrim} disabled={qRecApplying}>
                {qRecApplying ? 'Transcribing…' : 'Use this clip'}
              </button>
              <button
                class="btn-primary"
                disabled={qUploading || !qUploadName.trim() || !qUploadFile?.length}
                onclick={qHandleUpload}
              >
                {qUploading ? 'Uploading…' : 'Upload clone'}
              </button>
              <button class="btn-sec" onclick={qRetake} disabled={qRecApplying}> Retake </button>
              <button class="btn-sec" onclick={qDiscardRecording} disabled={qRecApplying}>
                Discard
              </button>
            </div>
          </div>
        {/if}
        {#if qRecError}
          <span class="error">{qRecError}</span>
        {/if}
        <label class="ref-text-hint">
          ref_text (optional — boosts ICL fidelity)
          <textarea
            rows="2"
            bind:value={qUploadRefText}
            placeholder="Auto-filled by parakeet on 'Use this clip' — edit if needed"></textarea>
        </label>
      </div>
    {/if}

    {#if qVoiceSource === 'audiobook'}
      <div class="library-clone">
        <span class="hint">
          Pick a chapter, drag the {TRIM_MAX_SECONDS}s window over a clean passage, then Upload. STT
          (parakeet) auto-fills ICL ref_text from the trimmed bytes.
        </span>
        {#if !qLibSelectedBook}
          <div class="lib-search-row">
            <input
              type="text"
              bind:value={qLibSearch}
              placeholder="Search audiobooks…"
              onkeydown={(e) => e.key === 'Enter' && qLibSearch_handler()}
            />
            <button class="btn-sec" onclick={qLibSearch_handler}>Search</button>
          </div>
          {#if qLibResults.length > 0}
            <ul class="lib-results">
              {#each qLibResults as book (book.id)}
                <li>
                  <button class="lib-result-btn" onclick={() => qLibPickBook(book.id)}>
                    <div class="lib-result-title">{book.title}</div>
                    <div class="lib-result-meta">
                      {#if book.author}<span>{book.author}</span>{/if}
                      {#if book.narrator}
                        <span class="muted">narrated by {book.narrator}</span>
                      {/if}
                    </div>
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        {:else}
          <div class="lib-selected">
            <div class="lib-selected-header">
              <div>
                <div class="lib-result-title">{qLibSelectedBook.title}</div>
                <div class="lib-result-meta">
                  {#if qLibSelectedBook.author}
                    <span>{qLibSelectedBook.author}</span>
                  {/if}
                  {#if qLibSelectedBook.narrator}
                    <span class="muted">narrated by {qLibSelectedBook.narrator}</span>
                  {/if}
                  <span class="muted">· {qLibSelectedBook.chapters.length} chapters</span>
                </div>
              </div>
              <button
                class="link-btn"
                type="button"
                onclick={() => {
                  qLibSelectedBook = null;
                  qLibSamples = null;
                  qLibChapterIdx = null;
                }}
              >
                ← pick different
              </button>
            </div>

            <div class="lib-chapter-list">
              {#each qLibSelectedBook.chapters as ch, i (ch.index)}
                <button
                  type="button"
                  class="lib-chapter-btn"
                  class:active={qLibChapterIdx === i}
                  onclick={() => qLibPickChapter(i)}
                >
                  <span class="lib-chapter-title">{ch.title}</span>
                  {#if ch.duration_seconds != null}
                    <span class="muted">
                      {Math.floor(ch.duration_seconds / 60)}:{String(
                        Math.round(ch.duration_seconds % 60)
                      ).padStart(2, '0')}
                    </span>
                  {/if}
                </button>
              {/each}
            </div>

            {#if qLibFetchingPreview}
              <div class="lib-preview-status">
                <span class="muted">Fetching {LIB_PREVIEW_SECONDS}s preview…</span>
              </div>
            {:else if qLibSamples}
              <div class="lib-trim-block">
                <div class="lib-pan-row">
                  <span class="muted small">
                    Showing {LIB_PREVIEW_SECONDS}s from {Math.floor(
                      qLibPreviewOffset / 60
                    )}:{String(Math.floor(qLibPreviewOffset % 60)).padStart(2, '0')}.
                  </span>
                  <button
                    type="button"
                    class="link-btn"
                    onclick={() =>
                      qLibFetchPreviewAt(Math.max(0, qLibPreviewOffset - LIB_PREVIEW_SECONDS))}
                    disabled={qLibPreviewOffset <= 0}
                  >
                    ← prev {LIB_PREVIEW_SECONDS}s
                  </button>
                  <button
                    type="button"
                    class="link-btn"
                    onclick={() => qLibFetchPreviewAt(qLibPreviewOffset + LIB_PREVIEW_SECONDS)}
                  >
                    next {LIB_PREVIEW_SECONDS}s →
                  </button>
                </div>
                <WaveformTrim
                  samples={qLibSamples}
                  sampleRate={qLibSampleRate}
                  maxSeconds={TRIM_MAX_SECONDS}
                  initialStart={qLibTrimStart}
                  initialEnd={qLibTrimEnd}
                  onchange={(s, e) => {
                    qLibTrimStart = s;
                    qLibTrimEnd = e;
                  }}
                />
                <div class="lib-clone-row">
                  <input
                    type="text"
                    bind:value={qLibName}
                    placeholder="new-voice-name"
                    class="lib-name-input"
                  />
                  <label class="lib-toggle">
                    <input type="checkbox" bind:checked={qLibTranscribe} />
                    STT ref_text
                  </label>
                  <button
                    class="btn-primary"
                    disabled={qLibUploading || !qLibName.trim()}
                    onclick={qLibUploadClone}
                  >
                    {qLibUploading ? 'Uploading…' : 'Upload clone'}
                  </button>
                </div>
              </div>
            {/if}
          </div>
        {/if}
        {#if qLibError}
          <div class="error">{qLibError}</div>
        {/if}
      </div>
    {/if}

    <details class="knobs-details">
      <summary>Advanced generation knobs</summary>
      <span class="hint">
        Defaults are streamed SSE + long-form: the lowest-VRAM path, and the one that holds a single
        voice across a long passage. Override here.
      </span>
      <div class="knobs">
        <label>
          Response format
          <select bind:value={qFormat}>
            <option value="wav">wav</option>
            <option value="pcm">pcm</option>
            <option value="mp3">mp3</option>
          </select>
        </label>
        <label>
          bitrate_kbps {#if qFormat !== 'mp3'}<span class="hint">(mp3 only)</span>{/if}
          <input
            type="number"
            step="1"
            min="32"
            max="192"
            value={qBitrate ?? ''}
            placeholder="64"
            disabled={qFormat !== 'mp3'}
            oninput={(e) =>
              (qBitrate = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label>
          seed (deterministic; empty = random)
          <input
            type="number"
            step="1"
            value={qSeed ?? ''}
            placeholder="random"
            oninput={(e) => (qSeed = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label>
          temperature
          <input
            type="number"
            step="0.05"
            min="0"
            value={qTemperature ?? ''}
            placeholder="0.9"
            oninput={(e) =>
              (qTemperature = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label>
          top_p
          <input
            type="number"
            step="0.05"
            min="0"
            max="1"
            value={qTopP ?? ''}
            placeholder="1.0"
            oninput={(e) => (qTopP = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label>
          top_k
          <input
            type="number"
            step="1"
            min="0"
            value={qTopK ?? ''}
            placeholder="50"
            oninput={(e) => (qTopK = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label>
          repetition_penalty
          <input
            type="number"
            step="0.05"
            min="1"
            value={qRepPenalty ?? ''}
            placeholder="default"
            oninput={(e) =>
              (qRepPenalty = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={qLong} />
          long-form
          <span class="hint">
            On by default. Chunks internally with rolling context so one voice carries the whole
            passage. Off truncates past ~60 s.
          </span>
        </label>
        <label>
          chunk_words {#if !qLong}<span class="hint">(long-form only)</span>{/if}
          <input
            type="number"
            step="10"
            min="10"
            value={qChunkWords ?? ''}
            placeholder="120"
            disabled={!qLong}
            oninput={(e) =>
              (qChunkWords = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label>
          speed
          <input
            type="number"
            step="0.05"
            min="0.25"
            max="4"
            value={qSpeed ?? ''}
            placeholder="1.0"
            oninput={(e) => (qSpeed = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
          <span class="hint">
            A real re-render at that rate (WSOLA, pitch preserved), not a player knob — the
            alignment comes back in the rendered timebase. Supported range 0.25–4.0; outside it the
            engine refuses and renders at 1.0 instead, which is reported above the player.
          </span>
        </label>
        <label>
          max_new_frames
          <input
            type="number"
            step="50"
            min="1"
            value={qMaxNewFrames ?? ''}
            placeholder="750"
            oninput={(e) =>
              (qMaxNewFrames = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
          <span class="hint">
            12.5 Hz FRAMES, not audio tokens. Raising this does not buy longer output — past the
            context window it turns a silent truncation into a hard error. Use long-form instead.
          </span>
        </label>
        <label>
          stream_batch_size <span class="hint">(alignment path only)</span>
          <input
            type="number"
            step="1"
            min="1"
            value={qStreamBatchSize ?? ''}
            placeholder="30"
            oninput={(e) =>
              (qStreamBatchSize = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label>
          stream_first_batch_size <span class="hint">(alignment path only)</span>
          <input
            type="number"
            step="1"
            min="1"
            value={qStreamFirstBatchSize ?? ''}
            placeholder="1"
            oninput={(e) =>
              (qStreamFirstBatchSize = parseKnobValue((e.currentTarget as HTMLInputElement).value))}
          />
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={saveSample} />
          save sample for replay
        </label>
      </div>
    </details>

    <details class="existing-voices-details">
      <summary>
        Existing clones ({clonedVoices.length})
      </summary>
      <div class="voice-list">
        {#each clonedVoices as v (v.id)}
          <div class="voice-row">
            <span class="voice-name">{v.name}</span>
            <div class="voice-actions">
              <button
                class="btn-sec voice-icon-btn"
                title="Download reference WAV"
                aria-label="Download {v.name} sample.wav"
                onclick={() => qDownloadVoiceSample(v.id)}
              >
                ↓ wav
              </button>
              <button
                class="btn-sec voice-icon-btn"
                title="Download ref_text (404 if none registered)"
                aria-label="Download {v.name} ref_text"
                onclick={() => qDownloadVoiceRefText(v.id)}
              >
                ↓ txt
              </button>
              <button
                class="btn-sec"
                disabled={qDeletingVoice === v.id}
                onclick={() => qHandleDelete(v.id)}
              >
                {qDeletingVoice === v.id ? 'Deleting…' : 'Delete'}
              </button>
            </div>
          </div>
        {/each}
        {#if clonedVoices.length === 0}
          <div class="muted small">No clones registered yet.</div>
        {/if}
      </div>
    </details>
  </div>

  <div class="actions">
    <button class="btn-primary" onclick={qHandleGenerate} disabled={qGenerating}>
      {qGenerating ? 'Generating…' : 'Generate'}
    </button>
    <label class="toggle">
      <input type="checkbox" bind:checked={qFaEnabled} />
      Forced alignment (SSE+PCM, word timings)
    </label>
    {#if qFaEnabled && qFa && qFa.state !== 'idle' && qFa.state !== 'error'}
      <button class="btn-secondary" onclick={() => qFa?.stop()}>Stop</button>
    {/if}
    {#if qError}<span class="error">{qError}</span>{/if}
    {#if !qFaEnabled && qPlaybackHint}<span class="hint">{qPlaybackHint}</span>{/if}
  </div>
  {#if restoreNote}<p class="hint">{restoreNote}</p>{/if}

  {#if qFaEnabled && qFa}
    <div class="card">
      <h3>Forced-alignment playback</h3>
      <FaTranscript text={input} activeWordIndex={qFa.activeWordIndex} />
      <div class="meta-grid" style="margin-top: 12px">
        <span class="meta-item"
          ><span class="meta-label">state</span><span class="meta-val">{qFa.state}</span></span
        >
        {#if qResultMeta?.gateWaitMs != null}
          <span class="meta-item"
            ><span class="meta-label">gate wait</span><span class="meta-val"
              >{qResultMeta.gateWaitMs} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.speedApplied != null && qResultMeta.speedApplied !== 1}
          <span class="meta-item"
            ><span class="meta-label">rendered at</span><span class="meta-val"
              >{qResultMeta.speedApplied}x</span
            ></span
          >
        {/if}
        {#if qResultMeta?.ttfaMs != null}
          <span class="meta-item"
            ><span class="meta-label">TTFA</span><span class="meta-val"
              >{Math.round(qResultMeta.ttfaMs)} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.firstAlignMs != null}
          <span class="meta-item"
            ><span class="meta-label">first align</span><span class="meta-val"
              >{Math.round(qResultMeta.firstAlignMs)} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.audioMs != null}
          <span class="meta-item"
            ><span class="meta-label">audio</span><span class="meta-val"
              >{(qResultMeta.audioMs / 1000).toFixed(2)} s</span
            ></span
          >
        {/if}
        {#if qResultMeta?.totalMs != null}
          <span class="meta-item"
            ><span class="meta-label">total wall</span><span class="meta-val"
              >{Math.round(qResultMeta.totalMs)} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.sampleId != null}
          <span class="meta-item"
            ><span class="meta-label">sample</span><span class="meta-val"
              >#{qResultMeta.sampleId}</span
            ></span
          >
        {/if}
        <span class="meta-item"
          ><span class="meta-label">active</span><span class="meta-val">#{qFa.activeWordIndex}</span
          ></span
        >
        <span class="meta-item"
          ><span class="meta-label">words</span><span class="meta-val">{qFa.words.length}</span
          ></span
        >
      </div>
      <details bind:open={qShowWordTable} style="margin-top: 12px">
        <summary>Per-word timings ({qFa.words.length})</summary>
        <table class="word-table">
          <thead>
            <tr><th>idx</th><th>text</th><th>t0 (ms)</th><th>t1 (ms)</th><th>dur</th></tr>
          </thead>
          <tbody>
            {#each qFa.words as w (w.globalWordIndex)}
              <tr class:active={w.globalWordIndex === qFa.activeWordIndex}>
                <td>{w.globalWordIndex}</td>
                <td>{w.text}</td>
                <td>{w.t0Ms}</td>
                <td>{w.t1Ms}</td>
                <td>{w.t1Ms - w.t0Ms}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </details>
    </div>
  {:else if qAudioUrl}
    <div class="card">
      <h3>Result</h3>
      <audio
        bind:this={qAudioEl}
        controls
        src={qAudioUrl}
        onloadedmetadata={qHandleAudioMetadata}
        ondurationchange={qHandleAudioMetadata}
      ></audio>
      <div class="meta-grid">
        {#if qResultMeta?.gateWaitMs != null}
          <span class="meta-item"
            ><span class="meta-label">gate wait</span><span class="meta-val"
              >{qResultMeta.gateWaitMs} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.speedApplied != null && qResultMeta.speedApplied !== 1}
          <span class="meta-item"
            ><span class="meta-label">rendered at</span><span class="meta-val"
              >{qResultMeta.speedApplied}x</span
            ></span
          >
        {/if}
        {#if qResultMeta?.ttfaMs != null}
          <span class="meta-item"
            ><span class="meta-label">TTFA</span><span class="meta-val"
              >{Math.round(qResultMeta.ttfaMs)} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.ttfpMs != null}
          <span class="meta-item"
            ><span class="meta-label">TTFP</span><span class="meta-val"
              >{Math.round(qResultMeta.ttfpMs)} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.totalMs != null}
          <span class="meta-item"
            ><span class="meta-label">total wall</span><span class="meta-val"
              >{Math.round(qResultMeta.totalMs)} ms</span
            ></span
          >
        {/if}
        {#if qResultMeta?.audioDurationMs != null}
          <span class="meta-item"
            ><span class="meta-label">audio</span><span class="meta-val"
              >{(qResultMeta.audioDurationMs / 1000).toFixed(2)} s</span
            ></span
          >
        {/if}
        {#if qResultMeta?.rtf != null}
          <span class="meta-item"
            ><span class="meta-label">RTF</span><span class="meta-val"
              >{qResultMeta.rtf.toFixed(3)}×</span
            ></span
          >
        {/if}
        {#if qResultMeta?.sampleId != null}
          <span class="meta-item"
            ><span class="meta-label">sample</span><span class="meta-val"
              >#{qResultMeta.sampleId}</span
            ></span
          >
        {/if}
      </div>
      <div class="meta">
        <a href={qAudioUrl} download={downloadName}>Download</a>
      </div>
    </div>
  {/if}

  <GenerationHistory
    service="tts"
    refreshKey={historyKey}
    title="Speech history"
    onRestore={restore}
  />
</div>

<style>
  .card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 20px;
  }
  .card h3 {
    margin: 0 0 12px;
    font-size: 15px;
  }
  .subhead {
    margin-top: 18px !important;
    border-top: 1px solid var(--ring);
    padding-top: 14px;
  }
  textarea,
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
  .upload {
    display: grid;
    grid-template-columns: minmax(150px, 1fr) minmax(220px, 2fr) auto;
    gap: 8px;
    margin-top: 12px;
    align-items: stretch;
  }
  @media (max-width: 768px) {
    .upload {
      grid-template-columns: 1fr;
    }
  }
  .knobs {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px 16px;
  }
  .actions {
    margin: 16px 0;
    display: flex;
    gap: 12px;
    align-items: center;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
  }
  .btn-secondary {
    background: var(--bg-2);
    color: var(--ink);
    border: 1px solid var(--ring);
    border-radius: 8px;
    padding: 8px 14px;
    font-family: inherit;
    font-size: 13px;
    cursor: pointer;
  }
  .btn-secondary:hover {
    background: var(--card);
  }
  .word-table {
    margin-top: 10px;
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }
  .word-table th,
  .word-table td {
    text-align: left;
    padding: 4px 8px;
    border-bottom: 1px solid var(--ring);
  }
  .word-table tr.active {
    background: color-mix(in srgb, var(--accent, #4f8cff) 25%, transparent);
  }
  .meta {
    margin-top: 8px;
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .meta-grid {
    margin-top: 10px;
    display: flex;
    flex-wrap: wrap;
    gap: 6px 12px;
    font-size: 12px;
  }
  .meta-item {
    display: inline-flex;
    align-items: baseline;
    gap: 6px;
    padding: 4px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
  }
  .meta-label {
    color: var(--muted);
    text-transform: uppercase;
    font-size: 10px;
    letter-spacing: 0.04em;
  }
  .meta-val {
    color: var(--ink);
    font-variant-numeric: tabular-nums;
    font-weight: 500;
  }
  .rec-section {
    margin-top: 12px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .rec-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }
  .vol-meter {
    flex: 0 1 200px;
    min-width: 120px;
    height: 8px;
    border-radius: 4px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    overflow: hidden;
  }
  .vol-fill {
    height: 100%;
    background: linear-gradient(90deg, #4d8fff 0%, #4d8fff 60%, #f5b800 80%, #d44 100%);
    transition: width 60ms linear;
  }
  .trim-block {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .trim-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
  }
  .trim-name {
    flex: 1;
    min-width: 140px;
    max-width: 240px;
  }
  .rec-btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  .rec-btn.recording {
    background: var(--danger-soft, rgba(220, 80, 80, 0.15));
    border-color: var(--danger, #d44);
    color: var(--danger, #d44);
  }
  .rec-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: currentColor;
    opacity: 0.55;
  }
  .rec-dot.pulsing {
    background: var(--danger, #d44);
    opacity: 1;
    animation: rec-pulse 1.1s ease-in-out infinite;
  }
  @keyframes rec-pulse {
    0%,
    100% {
      transform: scale(1);
      opacity: 1;
    }
    50% {
      transform: scale(1.6);
      opacity: 0.4;
    }
  }
  .voice-list {
    margin-top: 16px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .voice-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
  }
  .voice-name {
    font-size: 13px;
    color: var(--ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .voice-row .btn-sec {
    font-size: 12px;
    padding: 4px 10px;
  }
  .voice-actions {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .voice-icon-btn {
    font-variant-numeric: tabular-nums;
  }
  .hint {
    font-size: 11px;
    color: var(--muted);
    font-weight: 400;
  }
  .small {
    font-size: 11px;
  }
  .model-line {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    align-items: baseline;
  }

  .library-clone {
    display: grid;
    gap: 8px;
    margin: 8px 0;
  }

  .vocal-events {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin: 8px 0 4px;
  }
  .event-chip {
    background: var(--bg-2, rgba(0, 0, 0, 0.04));
    border: 1px solid var(--ring);
    border-radius: 999px;
    padding: 3px 10px;
    color: var(--muted);
    font-family: inherit;
    font-size: 12px;
    line-height: 1.5;
    cursor: pointer;
    transition: all 100ms ease;
  }
  .event-chip:hover {
    background: var(--card);
    border-color: var(--accent, #4d8fff);
    color: var(--ink);
  }
  /* Always its own line under the chips, however many of them wrap. */
  .vocal-events .hint {
    flex-basis: 100%;
  }

  .source-pills {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin: 8px 0;
  }
  .source-pill {
    background: var(--bg-2, rgba(0, 0, 0, 0.04));
    border: 1px solid var(--ring);
    border-radius: 999px;
    padding: 6px 14px;
    font-size: 13px;
    cursor: pointer;
    color: var(--muted);
    transition: all 100ms ease;
  }
  .source-pill:hover {
    background: var(--bg-3, rgba(0, 0, 0, 0.06));
  }
  .source-pill.active {
    background: var(--card);
    border-color: var(--accent, #4d8fff);
    color: var(--ink);
    font-weight: 500;
  }

  .ref-text-hint {
    margin-top: 8px;
  }

  .existing-voices-details,
  .knobs-details {
    border: 1px solid var(--ring);
    border-radius: 10px;
    padding: 6px 12px;
    margin-top: 12px;
  }
  .existing-voices-details summary,
  .knobs-details summary {
    cursor: pointer;
    font-size: 13px;
    font-weight: 500;
    color: var(--ink);
    padding: 4px 0;
  }

  .lib-search-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }

  .lib-search-row input[type='text'] {
    flex: 1;
  }

  .lib-name-input {
    flex: 1;
    min-width: 140px;
    max-width: 240px;
  }

  .lib-results {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 240px;
    overflow-y: auto;
    border: 1px solid var(--ring);
    border-radius: 8px;
  }

  .lib-results li {
    border-bottom: 1px solid var(--ring);
  }

  .lib-results li:last-child {
    border-bottom: none;
  }

  .lib-result-btn {
    display: block;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    padding: 10px 12px;
    cursor: pointer;
  }

  .lib-result-btn:hover {
    background: var(--bg-2, rgba(0, 0, 0, 0.03));
  }

  .lib-result-title {
    font-weight: 500;
    color: var(--fg, #1a1a1a);
  }

  .lib-result-meta {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 2px;
    font-size: 0.85em;
    color: var(--muted, #6b7280);
  }

  .lib-selected {
    display: grid;
    gap: 10px;
  }

  .lib-selected-header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 12px;
  }

  .link-btn {
    background: transparent;
    border: none;
    padding: 4px 6px;
    color: var(--accent, #1e6fff);
    cursor: pointer;
    font-size: 0.85em;
  }
  .link-btn:hover:not(:disabled) {
    text-decoration: underline;
  }
  .link-btn:disabled {
    color: var(--muted, #9aa3ad);
    cursor: not-allowed;
  }

  .lib-chapter-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    max-height: 220px;
    overflow-y: auto;
    border: 1px solid var(--ring);
    border-radius: 8px;
    padding: 4px;
  }

  .lib-chapter-btn {
    display: flex;
    justify-content: space-between;
    align-items: center;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    padding: 6px 10px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.9em;
  }
  .lib-chapter-btn:hover {
    background: var(--bg-2, rgba(0, 0, 0, 0.03));
  }
  .lib-chapter-btn.active {
    background: var(--accent-soft, rgba(30, 111, 255, 0.12));
    color: var(--accent, #1e6fff);
    font-weight: 500;
  }
  .lib-chapter-title {
    flex: 1;
  }

  .lib-trim-block {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .lib-preview-status,
  .lib-pan-row {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    font-size: 0.85em;
  }

  .lib-clone-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .lib-toggle {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    cursor: pointer;
  }
</style>
