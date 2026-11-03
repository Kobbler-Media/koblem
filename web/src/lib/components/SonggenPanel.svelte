<script lang="ts">
  import {
    songgenGenerate,
    songgenTransform,
    songgenSeparate,
    songgenSuggest,
    songgenDraftLyrics,
    songgenFixRhymes,
    cancelQueueJob,
    getSonggenJob,
    getSonggenActive,
    fetchMusicTrack,
    JOB_POLL_MS,
    type SonggenMode,
    type SonggenGenType,
    type SonggenGenerateRequest,
    type SonggenGenerateResponse,
    type SonggenJob,
    type SonggenSeparateResponse,
    type SonggenProps,
    type RhymeFlag
  } from '#lib/api/client.ts';
  import { blockedSummary } from '#lib/queue/format.ts';
  import type { EnqueueLike, QueueSubmitOptions } from '#lib/queue/options.ts';
  import QueueChip from '#lib/components/queue/QueueChip.svelte';
  import FileDropzone from '#lib/components/FileDropzone.svelte';
  import DescriptionChips from '#lib/components/DescriptionChips.svelte';
  import {
    WaveformTrim,
    blobToMonoSamples,
    blobToChannels,
    sliceSamples,
    encodeWav16,
    detectBestWindow
  } from '@koblibs/ui';
  import { onDestroy, onMount } from 'svelte';
  import { str, num } from '#lib/restore.ts';

  // The songgen engine's reachability (for the disabled state), plus a callback
  // to bump the shared track-history list after a successful render.
  let {
    songgenProps = null,
    onRendered
  }: {
    songgenProps?: SonggenProps | null;
    onRendered?: () => void;
  } = $props();

  // 'separate' is a debug-only mode (htdemucs split, no render) layered on top of
  // the songgen render modes — it reuses Clone's file + 10s-snippet picker.
  let mode = $state<SonggenMode | 'separate'>('generate');

  // Idea → (description [+ lyrics]) via the main 9B
  let idea = $state('');
  let suggesting = $state(false);
  let drafting = $state(false);
  let improving = $state(false);
  let lyricInstruction = $state('');

  // Core. Description is the comma-tag string songgen wants; the chip picker
  // (DescriptionChips) is the default editor, `descFreeform` falls back to raw text.
  let description = $state('female, pop ballad, romantic, piano, strings');
  let descFreeform = $state(false);
  let lyric = $state('');
  // 'llm' = the main 9B writes lyrics when the field is left blank; 'ace' = leave
  // it blank (instrumental). songgen's LeLM only SINGS the lyrics it's given — it
  // can't write them, so there's no "engine writes the lyrics" path here.
  let lyricsSource = $state<'ace' | 'llm'>('llm');
  let lyricsPrompt = $state('');
  // Advisory eye-rhymes on the drafted lyrics; the Fix button is opt-in.
  let rhymeFlags = $state<RhymeFlag[]>([]);
  let fixingRhymes = $state(false);
  let genType = $state<SonggenGenType>('mixed');

  // Length / seed
  let useDuration = $state(false); // off → render to model-max + EOS (lyric-driven)
  let duration = $state(120);
  let useSeed = $state(false);
  let seed = $state(1234);

  // Advanced sampling — defaults mirror the songgen server (HF demo preset:
  // temp 0.8 / top_k 50 / cfg 1.8), so toggling an override on without changing
  // the number sends exactly what the server would default to anyway.
  let showAdvanced = $state(false);
  let overrideTemp = $state(false);
  let temp = $state(0.8);
  let overrideTopK = $state(false);
  let topK = $state(50);
  let overrideCfg = $state(false);
  let cfg = $state(1.8);
  let overrideFade = $state(false);
  let fade = $state(200);

  // clone/continue stems — each decoded to mono PCM so the chorus snippet can
  // be trimmed in-browser (the engine just gets the sliced WAVs).
  // Only the mono downmix stays resident (a full song's stereo PCM is ~100 MB);
  // it drives the waveform/window UI. The original `file` is re-decoded at upload
  // time to slice the window in full STEREO (htdemucs separates on inter-channel
  // cues — a mono downmix smears the stems).
  type Pcm = { samples: Float32Array; sampleRate: number; file: File };
  let vocalFiles = $state<FileList | null>(null);
  let bgmFiles = $state<FileList | null>(null);
  let mixFiles = $state<FileList | null>(null);
  let vocalPcm = $state<Pcm | null>(null);
  let bgmPcm = $state<Pcm | null>(null);
  let mixPcm = $state<Pcm | null>(null);
  let decodeError = $state('');

  // Shared trim window (the chorus snippet) in SECONDS — the stems are the same
  // song, so one window maps onto all of them. The full mix is the nicest thing
  // to scrub for the chorus; fall back to the vocal stem when there's no mix.
  const refPcm = $derived(mixPcm ?? vocalPcm);
  let selStartSec = $state(0);
  let selEndSec = $state(0);
  const SNIPPET_MAX_SEC = 10;

  function decodeStem(files: FileList | null, set: (p: Pcm | null) => void) {
    const f = files?.[0] ?? null;
    if (!f) {
      set(null);
      return;
    }
    let cancelled = false;
    blobToMonoSamples(f)
      .then(({ samples, sampleRate }) => {
        if (!cancelled) set({ samples, sampleRate, file: f });
      })
      .catch((e) => {
        if (!cancelled) {
          set(null);
          decodeError = e instanceof Error ? e.message : 'could not decode audio';
        }
      });
    return () => {
      cancelled = true;
    };
  }

  $effect(() => decodeStem(vocalFiles, (p) => (vocalPcm = p)));
  $effect(() => decodeStem(bgmFiles, (p) => (bgmPcm = p)));
  $effect(() => decodeStem(mixFiles, (p) => (mixPcm = p)));

  // Default the window onto the strongest 10s of the reference (≈ the chorus)
  // whenever the reference audio changes.
  const refWindow = $derived(
    refPcm ? detectBestWindow(refPcm.samples, refPcm.sampleRate, SNIPPET_MAX_SEC) : null
  );
  let lastRefPcm: Pcm | null = null;
  $effect(() => {
    if (refPcm && refPcm !== lastRefPcm && refWindow) {
      lastRefPcm = refPcm;
      selStartSec = refWindow.start / refPcm.sampleRate;
      selEndSec = refWindow.end / refPcm.sampleRate;
    }
  });

  // Slice a decoded stem to the shared [selStartSec, selEndSec] window and
  // encode it as a mono WAV File ready for the multipart upload.
  // Re-decode the source File to full STEREO and slice the selected window. We
  // don't keep stereo PCM resident (memory); this one re-decode is ~100 ms —
  // negligible beside the render/separation it feeds.
  async function trimmedStem(pcm: Pcm, name: string): Promise<File> {
    const { channels, sampleRate } = await blobToChannels(pcm.file);
    const start = Math.max(0, Math.floor(selStartSec * sampleRate));
    const end = Math.min(channels[0]?.length ?? 0, Math.floor(selEndSec * sampleRate));
    const sliced = channels.map((ch) => sliceSamples(ch, start, end));
    return new File([encodeWav16(sliced, sampleRate)], name, { type: 'audio/wav' });
  }

  // Async UI state
  let running = $state(false);
  let error = $state('');
  let result = $state<SonggenGenerateResponse | null>(null);
  let trackUrl = $state<string>('');
  // Separate-mode result: the two demucs stems + their object URLs for playback.
  let stems = $state<SonggenSeparateResponse | null>(null);
  let vocalUrl = $state<string>('');
  let bgmUrl = $state<string>('');
  // Only `generate` queues; clone / continue / separate still answer inline.
  let jobId = $state<string | null>(null);
  let queued = $state(false);
  // 🔴 "Queued" alone is never the answer to "why hasn't it started".
  let queuedBecause = $state('');
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let queueOpts = $state<QueueSubmitOptions>({});
  let accepted = $state<EnqueueLike | null>(null);

  onMount(async () => {
    try {
      const { job } = await getSonggenActive();
      if (job) adopt(job);
    } catch {
      // Nothing to re-attach to, or the API is unreachable.
    }
  });

  onDestroy(() => {
    stopPoll();
    revoke();
  });

  function startPoll() {
    stopPoll();
    pollTimer = setInterval(pollOnce, JOB_POLL_MS);
  }

  function stopPoll() {
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
  }

  /** Take over a render already in flight — a fresh 202, or one `/active` found after a reload. */
  function adopt(job: SonggenJob) {
    revoke();
    result = null;
    stems = null;
    error = '';
    mode = 'generate';
    jobId = job.id;
    running = true;
    queued = job.state === 'queued';
    queuedBecause = job.blocked_reason ? blockedSummary(job.blocked_reason) : '';
    startPoll();
  }

  async function pollOnce() {
    if (!jobId) return;
    let job: SonggenJob;
    try {
      job = await getSonggenJob(jobId);
    } catch (e) {
      // A dropped poll is not a dropped render — the queue owns it. Keep polling; only a terminal
      // state or an explicit cancel ends this loop.
      console.warn('songgen poll failed, will retry', e);
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
    if (job.state === 'done') {
      await adoptResult({ tracks: job.tracks, duration_ms: job.duration_ms ?? undefined });
    } else if (job.state === 'cancelled') {
      error = 'render cancelled';
    } else {
      error = job.error || 'render failed';
    }
  }

  async function adoptResult(res: SonggenGenerateResponse) {
    result = res;
    const track = res.tracks[0];
    if (track) {
      try {
        trackUrl = await fetchMusicTrack(track.url);
      } catch {
        trackUrl = '';
      }
    }
    onRendered?.();
  }

  async function cancelRender() {
    if (!jobId) return;
    try {
      // 🔴 Off the QUEUE, not the engine: the engine's own /cancel only reaches a render that is
      // already running, and one still waiting its turn is exactly the one worth dropping.
      await cancelQueueJob(jobId);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  // Repopulate this panel from a saved songgen generation (dispatched by the
  // music page's history restore). Source audio uploads can't be re-attached.
  export function restore(meta: Record<string, unknown>) {
    const md = str(meta, 'mode');
    if (md === 'generate' || md === 'clone' || md === 'continue' || md === 'separate') mode = md;
    const d = str(meta, 'description') ?? str(meta, 'caption');
    if (d !== undefined) description = d;
    const ly = str(meta, 'lyric') ?? str(meta, 'lyrics_used');
    if (ly !== undefined) lyric = ly;
    const ls = str(meta, 'lyrics_source');
    if (ls === 'ace' || ls === 'llm') lyricsSource = ls;
    const lp = str(meta, 'lyrics_prompt');
    if (lp !== undefined) lyricsPrompt = lp;
    const gt = str(meta, 'gen_type');
    if (gt !== undefined) genType = gt as SonggenGenType;
    const du = num(meta, 'duration');
    if (du !== undefined) {
      useDuration = true;
      duration = du;
    }
    const sd = num(meta, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }
    const tp = num(meta, 'temp');
    if (tp !== undefined) {
      overrideTemp = true;
      temp = tp;
    }
    const tkk = num(meta, 'top_k');
    if (tkk !== undefined) {
      overrideTopK = true;
      topK = tkk;
    }
    const cf = num(meta, 'cfg');
    if (cf !== undefined) {
      overrideCfg = true;
      cfg = cf;
    }
    const fd = num(meta, 'fade');
    if (fd !== undefined) {
      overrideFade = true;
      fade = fd;
    }
    if (overrideTemp || overrideTopK || overrideCfg || overrideFade) showAdvanced = true;
    error = '';
  }

  function revoke() {
    if (trackUrl) URL.revokeObjectURL(trackUrl);
    if (vocalUrl) URL.revokeObjectURL(vocalUrl);
    if (bgmUrl) URL.revokeObjectURL(bgmUrl);
    trackUrl = '';
    vocalUrl = '';
    bgmUrl = '';
  }

  function buildRequest(): SonggenGenerateRequest {
    const req: SonggenGenerateRequest = {
      mode: mode as SonggenMode,
      // description steers style for generate; clone/continue take their style
      // from the audio prompt, so don't impose the (hidden) description on them.
      description: mode === 'generate' ? description.trim() : '',
      gen_type: genType
    };
    if (lyric.trim()) req.lyric = lyric;
    if (mode === 'generate') {
      req.lyrics_source = lyricsSource;
      if (lyricsSource === 'llm' && lyricsPrompt.trim()) req.lyrics_prompt = lyricsPrompt;
    }
    if (useDuration && duration > 0) req.duration = duration;
    if (useSeed) req.seed = seed;
    if (showAdvanced) {
      if (overrideTemp) req.temp = temp;
      if (overrideTopK) req.top_k = topK;
      if (overrideCfg) req.cfg = cfg;
      if (overrideFade) req.fade = fade;
    }
    return req;
  }

  // Expand the one-liner idea into a description (+ lyrics unless descriptionOnly)
  // with the main 9B, to pre-fill the form.
  async function expandIdea(descriptionOnly: boolean) {
    if (!idea.trim()) return;
    suggesting = true;
    error = '';
    try {
      const s = await songgenSuggest(idea, descriptionOnly);
      description = s.description;
      if (s.lyrics) {
        lyric = s.lyrics;
        lyricsSource = 'llm';
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      suggesting = false;
    }
  }

  // Write songgen-format lyrics for the current description with the main 9B.
  async function draftLyrics() {
    // generate writes from the description; clone/continue hide it (style comes
    // from the audio prompt), so there a theme is what the LLM writes from.
    if (!description.trim() && !lyricsPrompt.trim()) {
      error =
        mode === 'generate'
          ? 'description is required to write lyrics'
          : 'add a theme for the lyrics to write';
      return;
    }
    drafting = true;
    error = '';
    try {
      const r = await songgenDraftLyrics(description, lyricsPrompt || undefined);
      lyric = r.lyrics;
      rhymeFlags = r.rhyme_flags ?? [];
      lyricsSource = 'llm';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      drafting = false;
    }
  }

  /// Improve the current lyrics in place under songgen's rules — optionally
  /// toward a note; a blank note is a general polish.
  async function improveLyrics() {
    if (!lyric.trim()) return;
    improving = true;
    error = '';
    try {
      const r = await songgenDraftLyrics(description, lyricsPrompt || undefined, {
        source: lyric,
        instruction: lyricInstruction.trim() || undefined
      });
      lyric = r.lyrics;
      rhymeFlags = r.rhyme_flags ?? [];
      lyricsSource = 'llm';
      lyricInstruction = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      improving = false;
    }
  }

  /// Rewrite only the flagged line endings. Explicit — a rap lyric may want its
  /// slant rhymes left alone, so we flag and let the user decide.
  async function handleFixRhymes() {
    fixingRhymes = true;
    error = '';
    try {
      const r = await songgenFixRhymes(lyric, description || undefined);
      lyric = r.lyrics;
      rhymeFlags = r.rhyme_flags ?? [];
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      fixingRhymes = false;
    }
  }

  async function handleRender() {
    error = '';
    // generate: the LLM writes lyrics from the description, so it's required.
    // clone/continue: the engine requires lyrics; description is OPTIONAL (the
    // style comes from the audio prompt, mirroring songgen-server which only
    // mandates 'lyric' for clone/continue).
    if (mode === 'generate') {
      if (!description.trim()) {
        error = 'description is required';
        return;
      }
    } else if (!lyric.trim()) {
      error = 'lyrics are required for clone / continue';
      return;
    }
    const needStems = mode === 'clone' || mode === 'continue';
    const haveStems = !!vocalPcm && !!bgmPcm;
    const haveMix = !!mixPcm;
    if (needStems && !haveStems && !haveMix) {
      error =
        'clone / continue need a full mix (auto-separated into stems) or both a vocal and a bgm stem';
      return;
    }
    if (needStems && selEndSec - selStartSec < 0.5) {
      error = 'select at least a short snippet to clone from';
      return;
    }
    running = true;
    revoke();
    result = null;
    stems = null;
    accepted = null;
    queued = false;
    queuedBecause = '';
    const req = buildRequest();
    if (!needStems) {
      // 🔴 Only `generate` queues; the result arrives through the poll.
      try {
        const res = await songgenGenerate(req, queueOpts);
        accepted = res;
        jobId = res.job_id;
        queued = true;
        queuedBecause = res.blocked_reason ? blockedSummary(res.blocked_reason) : '';
        startPoll();
      } catch (e) {
        running = false;
        error = e instanceof Error ? e.message : String(e);
      }
      return;
    }
    try {
      // Send pre-split stems when both are present; otherwise send just the
      // trimmed mix and let songgen-server separate it (htdemucs). Each trim
      // re-decodes its source to full stereo (see trimmedStem). Still
      // synchronous: only `generate` was cut over to the queue.
      const vocalFile = haveStems ? await trimmedStem(vocalPcm!, 'vocal.wav') : null;
      const bgmFile = haveStems ? await trimmedStem(bgmPcm!, 'bgm.wav') : null;
      const mixFile = mixPcm ? await trimmedStem(mixPcm, 'mix.wav') : null;
      await adoptResult(
        await songgenTransform(req, mode as 'clone' | 'continue', vocalFile, bgmFile, mixFile)
      );
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      running = false;
    }
  }

  // Debug: split the selected mix snippet into vocal + bgm (htdemucs) — the exact
  // separation Clone runs internally. No render; returns the two stems to play.
  async function handleSeparate() {
    error = '';
    if (!mixPcm) {
      error = 'drop a full mix to separate';
      return;
    }
    if (selEndSec - selStartSec < 0.5) {
      error = 'select at least a short snippet to separate';
      return;
    }
    running = true;
    revoke();
    result = null;
    stems = null;
    try {
      const res = await songgenSeparate(await trimmedStem(mixPcm, 'mix.wav'));
      stems = res;
      [vocalUrl, bgmUrl] = await Promise.all([
        fetchMusicTrack(res.vocal.url).catch(() => ''),
        fetchMusicTrack(res.bgm.url).catch(() => '')
      ]);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      running = false;
    }
  }
</script>

<!-- Mode -->
<div class="card">
  <div class="row">
    <label class="toggle">
      <input type="radio" bind:group={mode} value="generate" /> Generate
      <span class="hint">new song from a description</span>
    </label>
    <label class="toggle">
      <input type="radio" bind:group={mode} value="clone" /> Clone
      <span class="hint">cloned style from a prompt (stems)</span>
    </label>
    <label class="toggle">
      <input type="radio" bind:group={mode} value="continue" /> Continue
      <span class="hint">extend a clip (output = continuation)</span>
    </label>
    <label class="toggle">
      <input type="radio" bind:group={mode} value="separate" /> Separate
      <span class="hint">debug: split a mix into vocal + bgm (what Clone sees)</span>
    </label>
  </div>
</div>

<!-- Core inputs -->
<div class="card">
  {#if mode === 'generate'}
    <label>
      Idea <span class="hint"
        >— a one-liner; the LLM expands it into a description (comma tags) + lyrics</span
      >
      <div class="idea-row">
        <input
          type="text"
          bind:value={idea}
          placeholder="e.g. a triumphant synthwave anthem about chasing the sunrise"
          onkeydown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              expandIdea(false);
            }
          }}
        />
        <button class="btn-sec" onclick={() => expandIdea(false)} disabled={suggesting}>
          {suggesting ? 'Thinking…' : '✨ Expand'}
        </button>
        <button
          class="btn-sec"
          onclick={() => expandIdea(true)}
          disabled={suggesting}
          title="Write only the description — songgen writes the lyrics"
        >
          Description only
        </button>
      </div>
    </label>

    <div class="field">
      <div class="desc-head">
        <span class="field-label"
          >Description
          <span class="hint"
            >— tags only (gender · genre · emotion · instrument), not sentences</span
          >
        </span>
        <label class="toggle"><input type="checkbox" bind:checked={descFreeform} /> free-form</label
        >
      </div>
      {#if descFreeform}
        <textarea
          rows="3"
          bind:value={description}
          placeholder="female, pop ballad, romantic, piano, strings"></textarea>
      {:else}
        <DescriptionChips bind:value={description} />
      {/if}
    </div>
  {/if}

  {#if mode !== 'separate'}
    <label>
      Lyrics
      <span class="hint"
        >— paste normal lyrics (with [Verse]/[Chorus] or "Verse 1"/"Chorus" headers) and they're
        auto-formatted; use ✍️ to have the LLM write them, or instrumental tags like
        [inst-long]/[intro-short] for no vocals</span
      >
      <textarea
        rows="6"
        bind:value={lyric}
        placeholder="[intro-short] ; [verse] line one. line two. ; [chorus] hook. hook. ; [outro-short]"
      ></textarea>
      {#if rhymeFlags.length}
        <div class="rhyme-warn">
          <span class="rhyme-icon" aria-hidden="true">⚠</span>
          <div class="rhyme-body">
            <span class="rhyme-title">
              {rhymeFlags.length === 1 ? 'This pair looks' : 'These pairs look'} like
              {rhymeFlags.length === 1 ? 'a rhyme' : 'rhymes'} but won't sing as
              {rhymeFlags.length === 1 ? 'one' : 'them'}:
            </span>
            <span class="rhyme-pairs">
              {#each rhymeFlags as f (f.a + '/' + f.b)}
                <code>{f.a} / {f.b}</code>
              {/each}
            </span>
            <span class="hint">Same letters, different vowel. Fine if you meant a slant rhyme.</span
            >
          </div>
          <button
            type="button"
            class="btn-sec"
            onclick={handleFixRhymes}
            disabled={fixingRhymes || !lyric.trim()}
            title="Rewrite just those lines so they rhyme when sung"
          >
            {fixingRhymes ? 'Fixing…' : 'Fix'}
          </button>
        </div>
      {/if}
      <div class="idea-row">
        <button
          class="btn-sec"
          onclick={draftLyrics}
          disabled={drafting}
          title="Have the main LLM write songgen-format lyrics"
        >
          {drafting ? 'Writing…' : '✍️ Write lyrics'}
        </button>
        <input
          type="text"
          bind:value={lyricsPrompt}
          placeholder={mode === 'generate'
            ? 'optional theme for the lyrics (story, mood)…'
            : 'theme for the lyrics (story, mood)…'}
        />
      </div>
      {#if lyric.trim()}
        <div class="idea-row">
          <button
            type="button"
            class="btn-sec"
            onclick={improveLyrics}
            disabled={improving}
            title="Rewrite the current lyrics under the same rules, applying your note — keeps what works"
          >
            {improving ? 'Improving…' : '✨ Improve'}
          </button>
          <input
            type="text"
            bind:value={lyricInstruction}
            placeholder="what to change? (blank = general polish)"
            onkeydown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault();
                improveLyrics();
              }
            }}
          />
        </div>
      {/if}
    </label>
  {/if}

  {#if mode === 'generate'}
    <div class="lyric-writer">
      <label class="toggle">
        <input
          type="checkbox"
          checked={lyricsSource === 'llm'}
          onchange={(e) => (lyricsSource = e.currentTarget.checked ? 'llm' : 'ace')}
        />
        If left blank, the main LLM writes the lyrics
        <span class="hint"
          >songgen's LeLM can't write lyrics — it only sings what it's given; off = instrumental</span
        >
      </label>
    </div>
  {/if}

  {#if mode === 'clone' || mode === 'continue' || mode === 'separate'}
    <div class="stems">
      <span class="writer-label">
        {#if mode === 'separate'}
          Audio to separate — drop a full mix; htdemucs splits the selected snippet into vocal + bgm
          so you can hear exactly what Clone feeds songgen
        {:else}
          Audio prompt — {mode === 'clone' ? 'clone its style' : 'continue from it'}: drop a full
          mix (auto-separated into stems), or supply your own vocal + bgm stems
        {/if}
      </span>
      <div class="stem-grid">
        <FileDropzone
          accept="audio/*"
          bind:files={mixFiles}
          label="🎶 Full mix"
          hint={mode === 'separate' ? 'to separate' : 'auto-separated'}
        />
        {#if mode !== 'separate'}
          <FileDropzone
            accept="audio/*"
            bind:files={vocalFiles}
            label="🎤 Vocal stem"
            hint="optional"
          />
          <FileDropzone
            accept="audio/*"
            bind:files={bgmFiles}
            label="🎛️ Bgm stem"
            hint="optional"
          />
        {/if}
      </div>
      {#if refPcm}
        <div class="snippet">
          <span class="hint">
            {mode === 'separate' ? 'Snippet to separate' : 'Snippet to clone'} — pick up to {SNIPPET_MAX_SEC}s,
            ideally the chorus (on the
            {mixPcm ? 'full mix' : 'vocal stem'}{mode === 'separate'
              ? ''
              : '; the same window is applied to every stem'})
          </span>
          <WaveformTrim
            samples={refPcm.samples}
            sampleRate={refPcm.sampleRate}
            maxSeconds={SNIPPET_MAX_SEC}
            initialStart={refWindow?.start}
            initialEnd={refWindow?.end}
            onchange={(s, e) => {
              if (refPcm) {
                selStartSec = s / refPcm.sampleRate;
                selEndSec = e / refPcm.sampleRate;
              }
            }}
          />
        </div>
      {/if}
      {#if decodeError}<span class="error">{decodeError}</span>{/if}
    </div>
  {/if}
</div>

{#if mode !== 'separate'}
  <!-- Settings -->
  <div class="card">
    <div class="row">
      <label class="toggle">
        Stems
        <select bind:value={genType}>
          <option value="mixed">mixed (full)</option>
          <option value="vocal">vocal only</option>
          <option value="bgm">backing only</option>
        </select>
      </label>
      <label class="toggle">
        <input type="checkbox" bind:checked={useDuration} /> Set duration
        <span class="hint">off → renders to length, stops on EOS</span>
      </label>
      {#if useDuration}
        <label>Seconds<input type="number" min="10" max="300" bind:value={duration} /></label>
      {/if}
      <label class="toggle">
        <input type="checkbox" bind:checked={useSeed} /> Fixed seed
      </label>
      {#if useSeed}
        <label>Seed<input type="number" bind:value={seed} /></label>
      {/if}
    </div>

    <details class="advanced" bind:open={showAdvanced}>
      <summary>Advanced sampling</summary>
      <div class="knobs">
        <label class="toggle"><input type="checkbox" bind:checked={overrideTemp} /> temp</label>
        {#if overrideTemp}<label
            ><input type="number" step="0.05" min="0.1" max="2" bind:value={temp} /></label
          >{:else}<span></span>{/if}
        <label class="toggle"><input type="checkbox" bind:checked={overrideTopK} /> top_k</label>
        {#if overrideTopK}<label
            ><input type="number" step="1" min="1" max="500" bind:value={topK} /></label
          >{:else}<span></span>{/if}
        <label class="toggle"><input type="checkbox" bind:checked={overrideCfg} /> cfg</label>
        {#if overrideCfg}<label
            ><input type="number" step="0.1" min="1" max="5" bind:value={cfg} /></label
          >{:else}<span></span>{/if}
        <label class="toggle"><input type="checkbox" bind:checked={overrideFade} /> fade (ms)</label
        >
        {#if overrideFade}<label
            ><input type="number" step="10" min="0" max="2000" bind:value={fade} /></label
          >{:else}<span></span>{/if}
      </div>
    </details>
  </div>
{/if}

<div class="actions">
  <button
    class="btn-primary"
    onclick={() => (mode === 'separate' ? handleSeparate() : handleRender())}
    disabled={running || songgenProps?.healthy === false}
  >
    {running
      ? mode === 'separate'
        ? 'Separating…'
        : 'Rendering…'
      : mode === 'separate'
        ? 'Separate'
        : 'Render'}
  </button>
  {#if mode === 'generate'}
    <QueueChip kind="songgen" bind:options={queueOpts} response={accepted} />
  {/if}
  {#if jobId}
    <button class="btn-sec" onclick={cancelRender}>Cancel</button>
    <span class="muted">
      {queued
        ? `Queued${queuedBecause ? ` — ${queuedBecause}` : ''}`
        : 'Rendering — safe to navigate away or reload.'}
    </span>
  {:else if running}
    <span class="muted"
      >{mode === 'separate'
        ? 'GPU held briefly for htdemucs; yields to TTS.'
        : 'GPU held for the render; yields to TTS. songgen runs ~140–280 s.'}</span
    >
  {/if}
  {#if songgenProps?.healthy === false}<span class="muted">songgen engine unreachable.</span>{/if}
  {#if error}<span class="error">{error}</span>{/if}
</div>

{#if result && result.tracks.length}
  <div class="card">
    <h3>Result</h3>
    <div class="meta">
      {#if result.duration_used != null}<span>{result.duration_used.toFixed(0)}s</span>{/if}
      {#if result.duration_ms != null}<span>· {(result.duration_ms / 1000).toFixed(1)}s wall</span
        >{/if}
    </div>
    {#each result.tracks as track (track.filename)}
      <div class="track">
        <div class="track-head">
          <span class="track-name">{mode}</span>
          <span class="muted">seed {track.seed < 0 ? 'random' : track.seed}</span>
          {#if trackUrl}<a class="btn-sec" href={trackUrl} download={track.filename}>Download</a
            >{/if}
        </div>
        {#if trackUrl}<audio controls src={trackUrl}></audio>{/if}
      </div>
    {/each}
    {#if result.lyrics_used}
      <details>
        <summary>Lyrics used</summary>
        <pre class="lyrics">{result.lyrics_used}</pre>
      </details>
    {/if}
  </div>
{/if}

{#if stems}
  <div class="card">
    <h3>Separated stems <span class="hint">— exactly what Clone feeds songgen</span></h3>
    {#if stems.duration_ms != null}
      <div class="meta"><span>{(stems.duration_ms / 1000).toFixed(1)}s · htdemucs</span></div>
    {/if}
    <div class="track">
      <div class="track-head">
        <span class="track-name">Vocal</span>
        {#if vocalUrl}<a class="btn-sec" href={vocalUrl} download={stems.vocal.filename}>Download</a
          >{/if}
      </div>
      {#if vocalUrl}<audio controls src={vocalUrl}></audio>{/if}
    </div>
    <div class="track">
      <div class="track-head">
        <span class="track-name">Bgm</span>
        {#if bgmUrl}<a class="btn-sec" href={bgmUrl} download={stems.bgm.filename}>Download</a>{/if}
      </div>
      {#if bgmUrl}<audio controls src={bgmUrl}></audio>{/if}
    </div>
  </div>
{/if}

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
  label.toggle input,
  label.toggle select {
    width: auto;
  }
  .hint {
    color: var(--muted);
    font-weight: normal;
    font-size: 11px;
  }
  .field {
    margin-top: 8px;
  }
  .field-label {
    font-size: 12px;
    color: var(--muted);
  }
  .desc-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 12px;
    flex-wrap: wrap;
  }
  .row {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
    align-items: flex-end;
  }
  .idea-row {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-top: 4px;
    flex-wrap: wrap;
  }
  .idea-row input {
    flex: 1;
    min-width: 180px;
  }
  .idea-row .btn-sec {
    white-space: nowrap;
  }

  /* Advisory, not an error — the lyrics are usable, a pair just won't sing as a
     rhyme. Warning tone rather than danger, and the Fix is opt-in. */
  .rhyme-warn {
    margin-top: 10px;
    padding: 10px 12px;
    border: 1px solid var(--warning);
    border-left-width: 3px;
    border-radius: 10px;
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .rhyme-icon {
    color: var(--warning);
    font-size: 16px;
    line-height: 1;
  }
  .rhyme-body {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 8px;
    flex: 1;
    min-width: 0;
  }
  .rhyme-title {
    font-size: 13px;
  }
  .rhyme-pairs {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .rhyme-pairs code {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    padding: 1px 6px;
    font-size: 12px;
    white-space: nowrap;
  }
  .rhyme-warn .btn-sec {
    white-space: nowrap;
  }
  .row label.toggle {
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
  }
  .lyric-writer,
  .stems {
    margin-top: 10px;
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 16px;
  }
  .writer-label {
    font-size: 12px;
    color: var(--muted);
    width: 100%;
  }
  .stem-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
    gap: 10px;
    width: 100%;
  }
  .snippet {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 4px;
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
  .knobs {
    display: grid;
    grid-template-columns: 160px 1fr;
    gap: 8px 16px;
    align-items: end;
    margin-top: 8px;
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
    text-transform: capitalize;
  }
  .track audio {
    width: 100%;
  }
  .track-head .btn-sec {
    margin-left: auto;
    text-decoration: none;
  }
  .lyrics {
    white-space: pre-wrap;
    background: var(--bg-2);
    border-radius: 8px;
    padding: 10px;
    font-size: 12px;
    margin-top: 8px;
  }
</style>
