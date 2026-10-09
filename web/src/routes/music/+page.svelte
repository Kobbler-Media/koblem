<script lang="ts">
  import {
    musicGenerate,
    musicTransform,
    musicUnderstand,
    musicDraftLyrics,
    musicFixRhymes,
    musicSuggest,
    getMusicProps,
    fetchMusicTrack,
    getSonggenProps,
    getSa3Props,
    getMusic3Props,
    type MusicMode,
    type MusicGenerateRequest,
    type MusicGenerateResponse,
    type MusicProps,
    type SonggenProps,
    type Sa3Props,
    type Music3Props,
    type RhymeFlag
  } from '$lib/api/client';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import SonggenPanel from '$lib/components/SonggenPanel.svelte';
  import Sa3Panel from '$lib/components/Sa3Panel.svelte';
  import Music3Panel from '$lib/components/Music3Panel.svelte';
  import type { Generation } from '$lib/types';
  import { str, num, bool, reattachNote } from '$lib/restore';
  import { onMount, onDestroy, tick } from 'svelte';

  let historyKey = $state(0);
  let restoreNote = $state('');

  // Which music engine the page drives. Co-equal engines that time-share the GPU:
  // acestep (lighter/quicker), songgen (LeVo2, longer + clone/continue), sa3
  // (Stable Audio 3 — game SFX / short audio + a2a/inpaint/continuation editing),
  // and music3 (MiniMax Music 3 — full songs, caption + lyrics, no audio input).
  let engine = $state<'acestep' | 'songgen' | 'sa3' | 'music3'>('acestep');
  let songgenProps = $state<SonggenProps | null>(null);
  let sa3Props = $state<Sa3Props | null>(null);
  let music3Props = $state<Music3Props | null>(null);
  // Child-panel instances (bind:this) so history restore can push settings in.
  let songgenPanel = $state<{ restore: (m: Record<string, unknown>) => void }>();
  let sa3Panel = $state<{ restore: (m: Record<string, unknown>) => void }>();
  let music3Panel = $state<{ restore: (m: Record<string, unknown>) => void }>();

  let mode = $state<MusicMode>('generate');

  // Core
  let idea = $state('');
  let caption = $state('upbeat indie pop with bright synths and a catchy hook');
  let lyrics = $state('');
  let lyricsSource = $state<'ace' | 'llm'>('ace');
  let lyricsPrompt = $state('');
  // Advisory eye-rhymes on the drafted lyrics; the Fix button is opt-in.
  let rhymeFlags = $state<RhymeFlag[]>([]);
  let fixingRhymes = $state(false);

  // Counts
  let songs = $state(1); // distinct songs (generate)
  let takes = $state(1); // seed-variations per song

  // Common settings
  let autoDuration = $state(true);
  let duration = $state(90);
  let useSeed = $state(false);
  let seed = $state(0);
  let outputFormat = $state<'mp3' | 'wav16' | 'wav24' | 'wav32'>('mp3');
  let vocalLanguage = $state('en'); // sung-vocal language; '' / 'unknown' = model picks
  let synthModel = $state('');

  // Mode-specific
  let coverStrength = $state(0.7);
  let coverReference = $state(false); // false = faithful remix (cover-nofsq); true = loose reference (cover/FSQ)
  let coverNoise = $state(0); // cover_noise_strength: 0 = pure noise (most freedom)
  let track = $state('vocals'); // extract/lego/complete: the stem to isolate/generate/add
  const isStem = $derived(mode === 'extract' || mode === 'lego' || mode === 'complete');
  let repaintStart = $state(0);
  let repaintEnd = $state(-1);
  let sourceFiles = $state<FileList | null>(null); // cover/repaint source

  // Advanced
  let showAdvanced = $state(false);
  let overrideSteps = $state(false);
  let inferenceSteps = $state(8);
  let overrideShift = $state(false);
  let shift = $state(3.0);
  let refFiles = $state<FileList | null>(null); // timbre reference (all modes)

  // Async UI state
  let running = $state(false);
  let understanding = $state(false);
  let drafting = $state(false);
  let improving = $state(false);
  let lyricInstruction = $state('');
  // Experimental: the LLM's length estimate for the drafted lyrics, shown as an
  // opt-in hint under the lyrics. null = none offered.
  let suggestedSeconds = $state<number | null>(null);
  let suggesting = $state(false);
  let error = $state('');
  let notice = $state('');
  let result = $state<MusicGenerateResponse | null>(null);
  let trackUrls = $state<string[]>([]);
  let props = $state<MusicProps | null>(null);

  onMount(async () => {
    try {
      props = await getMusicProps();
    } catch {
      props = { dit_models: [], lm_models: [], healthy: false };
    }
    try {
      songgenProps = await getSonggenProps();
    } catch {
      songgenProps = { healthy: false };
    }
    try {
      sa3Props = await getSa3Props();
    } catch {
      sa3Props = { healthy: false };
    }
    try {
      music3Props = await getMusic3Props();
    } catch {
      music3Props = { configured: false, healthy: false };
    }
  });

  onDestroy(() => revokeUrls());

  function revokeUrls() {
    for (const u of trackUrls) URL.revokeObjectURL(u);
    trackUrls = [];
  }

  // Repopulate the form from a saved generation. Music mixes three engines; set
  // engine first, then map the shared page vars. songgen/sa3 panel-only knobs
  // (gen_type/temp/top_k/cfg/fade, sampler/steps/model/seconds) have no page
  // control and can't be restored from here; uploaded source/timbre audio can't
  // be re-attached (reattachNote flags that).
  function restore(g: Generation) {
    const m = g.request_meta;
    const eng = str(m, 'engine');
    if (eng === 'acestep' || eng === 'songgen' || eng === 'sa3' || eng === 'music3') engine = eng;

    const md = str(m, 'mode');
    if (
      md === 'generate' ||
      md === 'cover' ||
      md === 'repaint' ||
      md === 'extract' ||
      md === 'lego' ||
      md === 'complete'
    )
      mode = md;
    const trk = str(m, 'track');
    if (trk !== undefined) track = trk;

    const cap = str(m, 'caption') ?? str(m, 'description') ?? str(m, 'prompt');
    if (cap !== undefined) caption = cap;
    const ly = str(m, 'lyrics') ?? str(m, 'lyric') ?? str(m, 'lyrics_used');
    if (ly !== undefined) lyrics = ly;
    const ls = str(m, 'lyrics_source');
    if (ls !== undefined) lyricsSource = ls as 'ace' | 'llm';
    const lp = str(m, 'lyrics_prompt');
    if (lp !== undefined) lyricsPrompt = lp;

    const sg = num(m, 'songs');
    if (sg !== undefined) songs = sg;
    const tk = num(m, 'takes');
    if (tk !== undefined) takes = tk;

    const dur = num(m, 'duration');
    if (dur !== undefined) {
      autoDuration = false;
      duration = dur;
    }
    const sd = num(m, 'seed');
    if (sd !== undefined) {
      useSeed = true;
      seed = sd;
    } else {
      useSeed = false;
    }

    const of = str(m, 'output_format');
    if (of !== undefined) outputFormat = of as 'mp3' | 'wav16' | 'wav24' | 'wav32';
    const vl = str(m, 'vocal_language');
    if (vl !== undefined) vocalLanguage = vl;
    const sm = str(m, 'synth_model');
    if (sm !== undefined) synthModel = sm;

    const cst = num(m, 'cover_strength');
    if (cst !== undefined) coverStrength = cst;
    const cref = bool(m, 'cover_reference');
    if (cref !== undefined) coverReference = cref;
    const cns = num(m, 'cover_noise_strength');
    if (cns !== undefined) coverNoise = cns;
    const rst = num(m, 'repaint_start');
    if (rst !== undefined) repaintStart = rst;
    const ren = num(m, 'repaint_end');
    if (ren !== undefined) repaintEnd = ren;

    const isteps = num(m, 'inference_steps');
    if (isteps !== undefined) {
      overrideSteps = true;
      inferenceSteps = isteps;
    }
    const sh = num(m, 'shift');
    if (sh !== undefined) {
      overrideShift = true;
      shift = sh;
    }
    if (overrideSteps || overrideShift) showAdvanced = true;

    // songgen/sa3 form state lives in child panels — dispatch into the newly
    // mounted panel after the engine switch renders it.
    if (eng === 'songgen') tick().then(() => songgenPanel?.restore(m));
    else if (eng === 'sa3') tick().then(() => sa3Panel?.restore(m));
    else if (eng === 'music3') tick().then(() => music3Panel?.restore(m));

    restoreNote = reattachNote(m);
    error = '';
    notice = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  function buildRequest(): MusicGenerateRequest {
    const req: MusicGenerateRequest = {
      mode,
      caption: caption.trim(),
      output_format: outputFormat,
      vocal_language: vocalLanguage,
      takes
    };
    if (lyrics.trim()) req.lyrics = lyrics;
    if (mode === 'generate') {
      req.songs = songs;
      req.lyrics_source = lyricsSource;
      if (lyricsSource === 'llm' && lyricsPrompt.trim()) req.lyrics_prompt = lyricsPrompt;
    }
    if (mode === 'generate' && !autoDuration && duration > 0) req.duration = duration;
    if (useSeed) req.seed = seed;
    if (synthModel) req.synth_model = synthModel;
    if (showAdvanced && overrideSteps) req.inference_steps = inferenceSteps;
    if (showAdvanced && overrideShift) req.shift = shift;
    if (mode === 'cover') {
      req.cover_strength = coverStrength;
      req.cover_reference = coverReference;
      if (coverNoise > 0) req.cover_noise_strength = coverNoise;
    }
    if (mode === 'repaint') {
      req.repaint_start = repaintStart;
      req.repaint_end = repaintEnd;
    }
    if (isStem && track.trim()) req.track = track.trim();
    return req;
  }

  // "From a song" — run understand on a file, drop the detected style into the
  // caption. In generate that's "make something in this style"; in cover/repaint
  // it describes the source.
  async function describeFromSong(file: File | undefined | null) {
    if (!file) return;
    understanding = true;
    error = '';
    notice = '';
    try {
      const u = await musicUnderstand(file);
      caption = u.caption;
      const bits = [
        u.bpm ? `${Math.round(u.bpm)} bpm` : '',
        u.keyscale ?? '',
        u.vocal_language && u.vocal_language !== 'unknown' ? u.vocal_language : ''
      ].filter(Boolean);
      notice = `Described the song${bits.length ? ` (${bits.join(' · ')})` : ''} → caption filled.`;
    } catch (e) {
      error = e instanceof Error ? e.message : 'understand failed';
    } finally {
      understanding = false;
    }
  }

  // Expand a casual idea via the 9B. captionOnly → caption only, so the ACE
  // engine writes the lyrics; otherwise caption + matching lyrics in one pass.
  async function expandIdea(captionOnly = false) {
    if (!idea.trim()) {
      error = 'type an idea first';
      return;
    }
    suggesting = true;
    error = '';
    notice = '';
    try {
      const s = await musicSuggest(idea, captionOnly);
      caption = s.caption;
      if (captionOnly) {
        notice =
          'Expanded your idea → caption filled. Edit freely; the ACE engine will write the lyrics. Then Render.';
      } else {
        if (s.lyrics) lyrics = s.lyrics;
        notice =
          'Expanded your idea → caption' +
          (s.lyrics ? ' + lyrics' : '') +
          ' filled. Edit freely, then Render.';
      }
    } catch (e) {
      error = e instanceof Error ? e.message : 'expand failed';
    } finally {
      suggesting = false;
    }
  }

  async function draftLyrics() {
    if (!caption.trim()) {
      error = 'add a caption first';
      return;
    }
    drafting = true;
    error = '';
    try {
      const r = await musicDraftLyrics(buildRequest());
      lyrics = r.lyrics;
      rhymeFlags = r.rhyme_flags ?? [];
      suggestedSeconds = r.suggested_seconds ?? null;
      notice = 'Drafted lyrics with the main model — edit freely before rendering.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'lyric draft failed';
    } finally {
      drafting = false;
    }
  }

  /// Improve the current lyrics in place under the same rules — optionally toward
  /// a note ("make the chorus punchier"); a blank note is a general polish.
  async function improveLyrics() {
    if (!lyrics.trim()) return;
    improving = true;
    error = '';
    try {
      const req = buildRequest();
      req.refine_source = lyrics;
      if (lyricInstruction.trim()) req.refine_instruction = lyricInstruction.trim();
      const r = await musicDraftLyrics(req);
      lyrics = r.lyrics;
      rhymeFlags = r.rhyme_flags ?? [];
      suggestedSeconds = r.suggested_seconds ?? null;
      lyricInstruction = '';
      notice = 'Improved the lyrics with the main model — edit freely before rendering.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'lyric improve failed';
    } finally {
      improving = false;
    }
  }

  /// Apply the LLM's length suggestion to the duration field (turns off auto).
  function useSuggestedLength() {
    if (suggestedSeconds == null) return;
    autoDuration = false;
    duration = Math.round(suggestedSeconds);
    suggestedSeconds = null;
  }

  /// Rewrite only the flagged line endings. Explicit — a rap lyric may want its
  /// slant rhymes left alone, so we flag and let the user decide.
  async function handleFixRhymes() {
    fixingRhymes = true;
    error = '';
    try {
      const r = await musicFixRhymes(lyrics, caption);
      lyrics = r.lyrics;
      rhymeFlags = r.rhyme_flags ?? [];
      notice = rhymeFlags.length
        ? 'Some endings still do not rhyme — fix them by hand, or leave them if they are what you want.'
        : 'Rhymes fixed.';
    } catch (e) {
      error = e instanceof Error ? e.message : 'rhyme fix failed';
    } finally {
      fixingRhymes = false;
    }
  }

  async function handleGenerate() {
    if (!caption.trim() && !isStem) {
      error = 'caption is required';
      return;
    }
    if (mode !== 'generate' && (!sourceFiles || !sourceFiles.length)) {
      error = 'this mode needs a source audio file';
      return;
    }
    if (isStem && !track.trim()) {
      error = 'pick a stem (e.g. vocals, drums, bass)';
      return;
    }
    running = true;
    error = '';
    notice = '';
    revokeUrls();
    result = null;
    try {
      const req = buildRequest();
      const ref = refFiles && refFiles.length ? refFiles[0] : null;
      const resp =
        mode === 'generate'
          ? await musicGenerate(req, ref)
          : await musicTransform(req, sourceFiles![0], ref);
      trackUrls = await Promise.all(resp.tracks.map((t) => fetchMusicTrack(t.url)));
      result = resp;
      historyKey++;
    } catch (e) {
      error = e instanceof Error ? e.message : 'render failed';
    } finally {
      running = false;
    }
  }

  function setIntro() {
    repaintStart = -8;
    repaintEnd = 0;
    notice = 'Repaint set to generate ~8s before the song (intro).';
  }

  function ext(format: string): string {
    return format === 'mp3' ? 'mp3' : 'wav';
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Music</h2>
    {#if engine === 'acestep' && props}
      <p class="muted">
        ACE-Step ·
        {#if props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
            >engine unreachable</span
          >{/if}
      </p>
    {:else if engine === 'songgen' && songgenProps}
      <p class="muted">
        songgen (LeVo2) ·
        {#if songgenProps.healthy}<span class="ok">engine up</span>{:else}<span class="down"
            >engine unreachable</span
          >{/if}
      </p>
    {:else if engine === 'sa3' && sa3Props}
      <p class="muted">
        Stable Audio 3 ·
        {#if sa3Props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
            >engine unreachable</span
          >{/if}
      </p>
    {:else if engine === 'music3' && music3Props}
      <p class="muted">
        MiniMax Music 3 ·
        {#if music3Props.healthy}<span class="ok">engine up</span
          >{:else if !music3Props.configured}<span class="down">not configured</span>{:else}<span
            class="down">engine unreachable</span
          >{/if}
      </p>
    {/if}
  </div>

  <!-- Engine: co-equal music engines that time-share the GPU. acestep is
       lighter/quicker; songgen is LeVo2 (longer, clone/continue); sa3 is Stable
       Audio 3 (game SFX / short audio + a2a/inpaint/continuation editing). -->
  <div class="card">
    <div class="row">
      <label class="toggle">
        <input type="radio" bind:group={engine} value="acestep" /> ACE-Step
        <span class="hint">lighter &amp; quicker</span>
      </label>
      <label class="toggle">
        <input type="radio" bind:group={engine} value="songgen" /> songgen
        <span class="hint">LeVo2 — longer, clone/continue</span>
      </label>
      <label class="toggle">
        <input type="radio" bind:group={engine} value="sa3" /> Stable Audio 3
        <span class="hint">SFX / short audio + editing</span>
      </label>
      <label class="toggle">
        <input type="radio" bind:group={engine} value="music3" /> MiniMax Music 3
        <span class="hint">full songs — caption + lyrics, no audio in</span>
      </label>
    </div>
  </div>

  {#if restoreNote}
    <p class="restore-note">{restoreNote}</p>
  {/if}

  {#if engine === 'acestep'}
    <!-- Mode -->
    <div class="card">
      <div class="row">
        <label class="toggle">
          <input type="radio" bind:group={mode} value="generate" /> Generate
          <span class="hint">new song from a description</span>
        </label>
        <label class="toggle">
          <input type="radio" bind:group={mode} value="cover" /> Cover
          <span class="hint">remix a song, swap lyrics</span>
        </label>
        <label class="toggle">
          <input type="radio" bind:group={mode} value="repaint" /> Repaint
          <span class="hint">regenerate / extend a region</span>
        </label>
        <label class="toggle">
          <input type="radio" bind:group={mode} value="extract" /> Extract
          <span class="hint">isolate one stem (base DiT · slower)</span>
        </label>
        <label class="toggle">
          <input type="radio" bind:group={mode} value="lego" /> Lego
          <span class="hint">add an instrument track (base DiT)</span>
        </label>
        <label class="toggle">
          <input type="radio" bind:group={mode} value="complete" /> Complete
          <span class="hint">build a mix from a stem (base DiT)</span>
        </label>
      </div>
    </div>

    <!-- Core inputs -->
    <div class="card">
      <label>
        Idea <span class="hint"
          >— a casual one-liner; the model expands it into a detailed caption</span
        >
        <div class="idea-row">
          <input
            type="text"
            bind:value={idea}
            placeholder="e.g. a hyped anime techno song about a robot cat"
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
            title="Write only the caption — the ACE engine writes the lyrics"
          >
            {suggesting ? 'Thinking…' : 'Caption only'}
          </button>
        </div>
      </label>

      <label>
        Caption <span class="hint">— the music: style, mood, instruments (required)</span>
        <textarea
          rows="3"
          bind:value={caption}
          placeholder="e.g. dreamy synthwave, warm analog pads, slow tempo"></textarea>
      </label>

      <!-- "From a song" — understand → caption (style-of) -->
      {#if mode === 'generate'}
        <div class="from-song">
          <FileDropzone
            accept="audio/*"
            label={understanding ? 'Listening…' : '🎵 Match a song'}
            hint="drop a track → fills the caption with its style"
            onFiles={(fl) => {
              describeFromSong(fl[0]);
            }}
          />
        </div>
      {/if}

      {#if mode === 'generate'}
        <label>
          Lyrics
          <span class="hint"
            >— blank = written for you; <code>[Instrumental]</code> for no vocals</span
          >
          <textarea rows="6" bind:value={lyrics} placeholder="one line per lyric line…"></textarea>
        </label>

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
              <span class="hint"
                >Same letters, different vowel. Fine if you meant a slant rhyme.</span
              >
            </div>
            <button
              type="button"
              class="btn-sec"
              onclick={handleFixRhymes}
              disabled={fixingRhymes || !lyrics.trim()}
              title="Rewrite just those lines so they rhyme when sung"
            >
              {fixingRhymes ? 'Fixing…' : 'Fix'}
            </button>
          </div>
        {/if}

        {#if lyrics.trim()}
          <div class="lyric-improve">
            <input
              type="text"
              bind:value={lyricInstruction}
              placeholder="Improve these lyrics — what to change? (blank = general polish)"
              onkeydown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  improveLyrics();
                }
              }}
            />
            <button
              type="button"
              class="btn-sec"
              onclick={improveLyrics}
              disabled={improving}
              title="Rewrite the current lyrics under the same rules, applying your note — keeps what works"
            >
              {improving ? 'Improving…' : '✨ Improve'}
            </button>
          </div>
        {/if}

        {#if suggestedSeconds != null && lyrics.trim()}
          <div class="length-hint">
            <span
              >✨ Suggested length: <strong>~{Math.round(suggestedSeconds)}s</strong>
              <span class="hint">(the model's guess for these lyrics)</span></span
            >
            <button type="button" class="btn-sec" onclick={useSuggestedLength}>Use</button>
          </div>
        {/if}

        {#if !lyrics.trim()}
          <div class="lyric-writer">
            <span class="writer-label">Blank lyrics → write them with:</span>
            <label class="toggle">
              <input type="radio" bind:group={lyricsSource} value="ace" /> ACE engine
              <span class="hint">(fast)</span>
            </label>
            <label class="toggle">
              <input type="radio" bind:group={lyricsSource} value="llm" /> Main model (LLM)
              <span class="hint">(steerable)</span>
            </label>
            {#if lyricsSource === 'llm'}
              <label>
                Lyric brief <span class="hint"
                  >— what the words are about (theme/story); caption stays the music</span
                >
                <textarea
                  rows="2"
                  bind:value={lyricsPrompt}
                  placeholder="e.g. a bittersweet song about leaving a small town at dawn"
                ></textarea>
              </label>
              <button class="btn-sec" onclick={draftLyrics} disabled={drafting}>
                {drafting ? 'Drafting…' : '✍ Draft lyrics now (edit before render)'}
              </button>
            {/if}
          </div>
        {/if}
      {:else}
        <!-- cover / repaint -->
        <FileDropzone
          accept="audio/*"
          bind:files={sourceFiles}
          label="Drop source song"
          hint="WAV or MP3 — the track to transform"
        />
        {#if sourceFiles && sourceFiles.length && !isStem}
          <button
            class="btn-sec"
            onclick={() => {
              describeFromSong(sourceFiles?.[0]);
            }}
            disabled={understanding}
          >
            {understanding ? 'Listening…' : '🎵 Describe this source → fill caption'}
          </button>
        {/if}
        {#if isStem}
          <label>
            Stem
            <span class="hint">
              {mode === 'extract'
                ? '— which stem to isolate from the source mix'
                : mode === 'lego'
                  ? '— which instrument to generate over the source backing'
                  : '— which stem(s) to add to the source (e.g. "drums", or "VOCALS | DRUMS")'}
            </span>
            <input list="stem-names" bind:value={track} placeholder="vocals" />
            <datalist id="stem-names">
              <option value="vocals"></option>
              <option value="backing_vocals"></option>
              <option value="drums"></option>
              <option value="bass"></option>
              <option value="guitar"></option>
              <option value="keyboard"></option>
              <option value="percussion"></option>
              <option value="strings"></option>
              <option value="synth"></option>
              <option value="fx"></option>
              <option value="brass"></option>
              <option value="woodwinds"></option>
            </datalist>
          </label>
        {:else}
          <label>
            Lyrics
            {#if mode === 'cover' && !coverReference}<span class="hint"
                >— full replacement lyrics, matching the source structure</span
              >{/if}
            {#if mode === 'cover' && coverReference}<span class="hint"
                >— any lyrics (or [Instrumental]); reference mode won't force them onto the source</span
              >{/if}
            <textarea
              rows="6"
              bind:value={lyrics}
              placeholder={mode === 'repaint'
                ? '[Instrumental] (repaint regenerates a region)'
                : 'one line per lyric line…'}></textarea>
          </label>
        {/if}
      {/if}
    </div>

    <!-- Settings -->
    <div class="card">
      <h3>Settings</h3>
      <div class="knobs">
        {#if mode === 'generate'}
          <label>
            Songs <span class="hint">(distinct options)</span>
            <input type="number" min="1" max="4" bind:value={songs} />
          </label>
        {/if}
        <label>
          {mode === 'generate' ? 'Takes / song' : 'Takes'}
          <span class="hint">(seed variations)</span>
          <input type="number" min="1" max="9" bind:value={takes} />
        </label>

        <label>
          Output
          <select bind:value={outputFormat}>
            <option value="mp3">mp3 (320k)</option>
            <option value="wav16">wav (lossless)</option>
            <option value="wav24">wav 24-bit</option>
            <option value="wav32">wav 32-bit</option>
          </select>
        </label>

        <label>
          Vocal language <span class="hint">(sung words)</span>
          <select bind:value={vocalLanguage}>
            <option value="en">English</option>
            <option value="es">Spanish</option>
            <option value="fr">French</option>
            <option value="de">German</option>
            <option value="it">Italian</option>
            <option value="pt">Portuguese</option>
            <option value="nl">Dutch</option>
            <option value="ru">Russian</option>
            <option value="ja">Japanese</option>
            <option value="ko">Korean</option>
            <option value="zh">Chinese</option>
            <option value="unknown">Let model pick</option>
          </select>
        </label>

        {#if mode === 'generate'}
          <label class="toggle">
            <input type="checkbox" bind:checked={autoDuration} /> Auto duration
          </label>
          {#if !autoDuration}
            <label>
              Duration (s)
              <input type="number" min="10" max="600" bind:value={duration} />
            </label>
          {/if}
        {:else}
          <span class="hint" style="grid-column: 1 / -1;"
            >Length follows the source song (cover/repaint can't change it — use Repaint to add an
            intro/outro).</span
          >
        {/if}

        <label class="toggle">
          <input type="checkbox" bind:checked={useSeed} /> Fixed seed
        </label>
        {#if useSeed}
          <label>
            Seed
            <input type="number" min="0" bind:value={seed} />
          </label>
        {/if}

        {#if mode === 'cover'}
          <label>
            Cover style
            <select bind:value={coverReference}>
              <option value={false}
                >Faithful remix — stay close to the source (needs matching lyrics)</option
              >
              <option value={true}
                >Loose reference — a new song "in the style of" it (any lyrics)</option
              >
            </select>
            <span class="hint">
              {coverReference
                ? 'FSQ path: loosens the source so your caption + new lyrics drive it'
                : 'clean-latent path: tracks the source melody/structure closely'}
            </span>
          </label>
          <label>
            Faithfulness <span class="muted">{coverStrength.toFixed(2)}</span>
            <input type="range" min="0.2" max="1" step="0.05" bind:value={coverStrength} />
            <span class="hint"
              >low = follows your new lyrics · high = keeps the source (may ignore new lyrics)</span
            >
          </label>
          <label>
            Start from source <span class="muted">{coverNoise.toFixed(2)}</span>
            <input type="range" min="0" max="1" step="0.05" bind:value={coverNoise} />
            <span class="hint"
              >0 = start from pure noise (most freedom) · higher = begin nearer the source waveform</span
            >
          </label>
        {/if}

        {#if mode === 'repaint'}
          <label>
            Region start (s) <span class="hint">negative = before song</span>
            <input type="number" step="0.5" bind:value={repaintStart} />
          </label>
          <label>
            Region end (s) <span class="hint">-1 = song end; beyond = after</span>
            <input type="number" step="0.5" bind:value={repaintEnd} />
          </label>
          <button class="btn-sec" onclick={setIntro}>+ Add intro</button>
        {/if}
      </div>

      <details bind:open={showAdvanced} class="advanced">
        <summary>Advanced</summary>
        <div class="knobs">
          {#if props && props.dit_models.length > 1}
            <label>
              DiT model
              <select bind:value={synthModel}>
                <option value="">default (turbo)</option>
                {#each props.dit_models as m (m)}<option value={m}>{m}</option>{/each}
              </select>
            </label>
          {/if}
          <label class="toggle">
            <input type="checkbox" bind:checked={overrideSteps} /> Override steps
          </label>
          {#if overrideSteps}
            <label>Steps<input type="number" min="1" max="100" bind:value={inferenceSteps} /></label
            >
          {/if}
          <label class="toggle">
            <input type="checkbox" bind:checked={overrideShift} /> Override shift
          </label>
          {#if overrideShift}
            <label>Shift<input type="number" step="0.5" bind:value={shift} /></label>
          {/if}
        </div>
        <div class="ref-timbre">
          <span class="writer-label"
            >Reference timbre <span class="hint">— optional clip to match the tone/voice</span
            ></span
          >
          <FileDropzone
            accept="audio/*"
            bind:files={refFiles}
            label="Drop a reference clip"
            hint="optional — conditions the timbre"
          />
        </div>
      </details>
    </div>

    <div class="actions">
      <button class="btn-primary" onclick={handleGenerate} disabled={running || understanding}>
        {running ? 'Rendering…' : 'Render'}
      </button>
      {#if running}<span class="muted">GPU held for the render; yields to TTS.</span>{/if}
      {#if notice && !error}<span class="muted">{notice}</span>{/if}
      {#if error}<span class="error">{error}</span>{/if}
    </div>

    {#if result}
      <div class="card">
        <h3>Result</h3>
        <div class="meta">
          <span>{result.tracks.length} track{result.tracks.length === 1 ? '' : 's'}</span>
          <span>· {result.duration_used.toFixed(0)}s</span>
          {#if result.duration_ms != null}<span
              >· {(result.duration_ms / 1000).toFixed(1)}s wall</span
            >{/if}
        </div>
        {#each result.tracks as track, i (track.filename)}
          <div class="track">
            <div class="track-head">
              <span class="track-name">Track {i + 1}</span>
              <span class="muted">seed {track.seed < 0 ? 'random' : track.seed}</span>
              <a class="btn-sec" href={trackUrls[i]} download={track.filename}
                >Download .{ext(outputFormat)}</a
              >
            </div>
            {#if trackUrls[i]}<audio controls src={trackUrls[i]}></audio>{/if}
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
  {:else if engine === 'songgen'}
    <SonggenPanel bind:this={songgenPanel} {songgenProps} onRendered={() => (historyKey += 1)} />
  {:else if engine === 'sa3'}
    <Sa3Panel bind:this={sa3Panel} {sa3Props} onRendered={() => (historyKey += 1)} />
  {:else}
    <Music3Panel bind:this={music3Panel} {music3Props} onRendered={() => (historyKey += 1)} />
  {/if}

  <GenerationHistory
    service="music"
    refreshKey={historyKey}
    title="Track history"
    onRestore={restore}
  />
</div>

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
  .row {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
  }
  .row label.toggle {
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
  }
  .from-song {
    margin-top: 10px;
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
  .lyric-writer,
  .ref-timbre {
    margin-top: 10px;
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 16px;
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
  .writer-label {
    font-size: 12px;
    color: var(--muted);
    width: 100%;
  }
  .lyric-improve {
    margin-top: 8px;
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .lyric-improve input {
    flex: 1;
    min-width: 0;
  }
  .lyric-improve .btn-sec {
    white-space: nowrap;
  }
  .length-hint {
    margin-top: 8px;
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 13px;
  }
  .length-hint span {
    flex: 1;
    min-width: 0;
  }
  .length-hint .btn-sec {
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
    margin-bottom: 8px;
    flex-wrap: wrap;
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
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
  .lyrics {
    white-space: pre-wrap;
    background: var(--bg-2);
    border-radius: 8px;
    padding: 10px;
    font-size: 12px;
    margin-top: 8px;
  }
</style>
