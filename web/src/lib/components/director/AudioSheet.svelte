<script lang="ts">
  import FileDropzone from '#lib/components/FileDropzone.svelte';
  import type { LyricWord } from '#lib/api/client.ts';

  // The project's audio bed and its word-level transcript. The waveform and lyric chips it produces
  // are what the timeline's audio lane draws, and what a beat snaps to.
  let {
    audioKind = $bindable(),
    h3SoundtrackMode = $bindable(),
    h3SoundtrackDiegetic = $bindable(),
    audioFile,
    waveDuration,
    lyrics,
    transcribing,
    soundtrackIsReference = false,
    fitNote = null,
    onclose,
    onfiles,
    onclear,
    ontranscribe,
    onchange
  }: {
    audioKind: 'none' | 'voice' | 'bgm' | 'mixed';
    /** H3 only — see {@link soundtrackIsReference}. Inert everywhere else. */
    h3SoundtrackMode?: 'replace' | 'keep' | 'mix';
    /**
     * H3 only: can the characters HEAR this track?
     *
     * 🔴 Not cosmetic, and not derivable. It decides which SECTION the prompt writer files the song
     * under, and the writer's fallback is the guide's — `GUIDE-ref:296` sends a reused score to
     * `non_diegetic_music`, which is *defined* as music the characters cannot hear. On a music
     * video that is a direct contradiction with asking the same character to sing it, and the
     * singing stops locking to the vocal.
     */
    h3SoundtrackDiegetic?: boolean;
    audioFile: File | null;
    waveDuration: number;
    lyrics: LyricWord[];
    transcribing: boolean;
    /**
     * Does the supplied track reach the engine as a REFERENCE (`caps.soundtrackRoute ===
     * 'audio_reference'`) rather than as the request's audio source?
     *
     * 🔴 The whole reason the two dials below exist, and NOT `caps.generatesAudio` — which is also
     * true on LTX-2.5 and would draw them there, where both write H3-only document fields that
     * `compile.ts` reads inside `compileH3Timeline` alone. Where the track is an `audio_source` it
     * is sliced onto the request to drive lip-sync and koblem remuxes the original over the
     * finished cut, so there is nothing to displace and nothing to choose. On H3 a synchronised
     * stereo track is denoised in the same pass and the supplied song rides in as an `<Audio N>`
     * reference — so "what happens to the one the model made" is a real question, and so is
     * whether the characters can hear yours.
     */
    soundtrackIsReference?: boolean;
    /**
     * How the song and the timeline fit — generations that will not be scored, a song that outruns
     * the cut. `null` when there is nothing to say.
     *
     * Shown here rather than only logged at render time: every one of these is something the author
     * can fix in ten seconds while looking at the timeline, and cannot fix at all once a
     * fifteen-minute render has already started.
     */
    fitNote?: string | null;
    onclose: () => void;
    onfiles: (files: FileList | null) => void;
    onclear: () => void;
    ontranscribe: () => void;
    onchange: () => void;
  } = $props();
</script>

<div class="overlay" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="modal" style="height:auto;max-width:520px">
    <div class="chat-h">
      <div class="chat-badge">♪</div>
      <div style="flex:1">
        <b>Audio &amp; lyrics</b><small
          >{audioFile ? `1 track · ${lyrics.length} words aligned` : 'no track yet'}</small
        >
      </div>
      <button class="btn icon-only ghost" onclick={onclose}>✕</button>
    </div>
    <div class="sheet-body">
      {#if audioFile}
        <div class="hist-item">
          <div
            class="hist-thumb"
            style="background:linear-gradient(90deg,var(--accent-2),var(--accent))"
          ></div>
          <div style="flex:1">
            <b>{audioFile.name}</b><br /><small
              >{waveDuration.toFixed(1)}s track · cuts snap to lyric onsets</small
            >
          </div>
          <button class="btn sm danger" onclick={onclear}>Remove</button>
        </div>
        <label class="field" style="margin-top:12px"
          ><span>Audio kind</span>
          <select class="inp" bind:value={audioKind} {onchange}>
            <option value="voice">Voice (drives lip-sync)</option>
            <option value="mixed">Mixed (song — voice extracted)</option>
            <option value="bgm">BGM only</option>
            <option value="none">Ignore</option>
          </select>
        </label>
        {#if soundtrackIsReference && audioKind !== 'none'}
          <label class="field"
            ><span>The model's own audio</span>
            <select class="inp" bind:value={h3SoundtrackMode} {onchange}>
              <option value="replace">Replace it with this track</option>
              <option value="keep">Keep it — this track only guides the performance</option>
              <option value="mix">Mix — this track over a ducked version of it</option>
            </select>
          </label>
          <!-- 🔴 Why REPLACE is the default, and why "keep" is not the safe-looking option it
               appears to be. Every mode conditions the render identically: each generation is
               handed its slice as an <Audio N> reference marked `fully_copy`, so performers sing to
               the track whichever you pick. What differs is only what is DELIVERED.
               `fully_copy` is a marker in the prompt, not a copy instruction — the audio still
               comes out of the joint denoise — so "keep" ships a reconstruction of the song you
               supplied rather than the song. That is a lossy cover version of a file you already
               have, which is why it is not the default despite sounding like the conservative
               choice. -->
          <label class="field"
            ><span>Can the characters hear it?</span>
            <select class="inp" bind:value={h3SoundtrackDiegetic} {onchange}>
              <option value={true}>Yes — it plays in the scene and they perform to it</option>
              <option value={false}>No — audience-only score</option>
            </select>
          </label>
          <p class="hint" style="margin-top:-4px">
            {#if h3SoundtrackDiegetic}
              The prompt writer will keep <code>non_diegetic_music</code> at <code>N/A</code> and declare
              the song as playing out loud in the scene. Pick this for a music video.
            {:else}
              The song is filed as an audience-only score. The characters will not be written as
              hearing or reacting to it.
            {/if}
          </p>
          <p class="hint" style="margin-top:-4px">
            {#if h3SoundtrackMode === 'keep'}
              Performers still sing to your track — but the clip ships the audio the model
              <em>rendered</em>, which is its own reconstruction of the song rather than the file
              you supplied. Pick this when the track is a timing guide, not the final mix.
            {:else if h3SoundtrackMode === 'mix'}
              Your track over the model's own audio, ducked underneath — so footsteps, room tone and
              diegetic sound survive under the music.
            {:else}
              Your file becomes the clip's whole audio track. The model still hears the song and
              sings to it; this just makes sure the deliverable carries the original rather than the
              model's reconstruction of it.
            {/if}
          </p>
        {/if}
        {#if fitNote}
          <p class="hint err">{fitNote}</p>
        {/if}
        <button
          class="btn"
          style="width:100%;justify-content:center"
          onclick={ontranscribe}
          disabled={transcribing}>{transcribing ? 'Transcribing…' : 'Align lyrics'}</button
        >
        <p class="hint" style="margin-top:10px">
          The waveform + lyric chips appear on the timeline's audio lane. Drop a new track to
          replace.
        </p>
      {:else}
        <FileDropzone accept="audio/*" label="Drop an audio track" onFiles={onfiles} />
      {/if}
    </div>
  </div>
</div>
