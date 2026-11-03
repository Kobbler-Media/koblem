<script lang="ts">
  import type { LyricWord } from '#lib/api/client.ts';
  import { tickLabel } from '#lib/director/timeline-ticks.ts';

  // Song setup, slice role and re-transcribe. The bar on the Shots screen is playback only; this is
  // where the track is actually configured. The full desktop sheet — drop zone, lyric lane, per-shot
  // upload kind — stays one tap away rather than being rewritten narrower.
  let {
    h3SoundtrackMode = $bindable('replace'),
    h3SoundtrackDiegetic = $bindable(true),
    audioFile,
    audioName,
    waveDuration,
    lyrics,
    transcribing,
    soundtrackIsReference,
    audioKind,
    fitNote = null,
    onopenfull,
    ontranscribe,
    onchange
  }: {
    h3SoundtrackMode?: 'replace' | 'keep' | 'mix';
    h3SoundtrackDiegetic?: boolean;
    audioFile: File | null;
    /** The stored track's name when this session did not upload it — a reload has no `File`. */
    audioName: string | null;
    waveDuration: number;
    lyrics: LyricWord[];
    transcribing: boolean;
    /** See {@link AudioSheet}'s prop of the same name — the dials below are H3's, not "generates audio". */
    soundtrackIsReference: boolean;
    /**
     * The desktop sheet's Audio kind. Read-only here: this screen offers no picker for it, but
     * `Ignore` means the track never reaches the engine at all, so the slice dials would be
     * describing something that does not happen.
     */
    audioKind: 'none' | 'voice' | 'bgm' | 'mixed';
    fitNote?: string | null;
    onopenfull: () => void;
    ontranscribe: () => void;
    onchange: () => void;
  } = $props();

  const name = $derived(audioFile?.name ?? audioName);
</script>

<div class="sheetbody">
  <div class="grp">
    <p class="eye">Song</p>
    {#if name}
      <p class="hint">
        {name}{waveDuration > 0 ? ` · ${tickLabel(waveDuration, 1)}` : ''} · {lyrics.length}
        {lyrics.length === 1 ? 'word' : 'words'} aligned
      </p>
    {:else}
      <p class="hint">
        No track on this project yet. A song gives the cut its clock — shots snap to word onsets and
        beats can be placed against what is being sung.
      </p>
    {/if}
    <button class="btn full" type="button" onclick={onopenfull}
      >{name ? 'Replace, trim or clear the track…' : 'Add a track…'}</button
    >
    {#if name}
      <button class="btn full" type="button" onclick={ontranscribe} disabled={transcribing}
        >{transcribing ? 'Transcribing…' : 'Re-transcribe lyrics'}</button
      >
    {/if}
    {#if fitNote}<p class="hint warn">{fitNote}</p>{/if}
  </div>

  {#if soundtrackIsReference && audioKind !== 'none'}
    <div class="grp">
      <p class="eye">How the engine hears it</p>
      <label class="field"
        ><span>Per-shot slice</span>
        <select class="inp" bind:value={h3SoundtrackMode} {onchange}>
          <option value="replace">Replace</option>
          <option value="keep">Keep the model's own</option>
          <option value="mix">Mix</option>
        </select>
      </label>
      <p class="hint">
        Replace means performers sing to your track and the deliverable carries it. Shots under two
        seconds cannot carry a slice and render on the model's own bed.
      </p>
      <label class="field"
        ><span>Can the characters hear it?</span>
        <select
          class="inp"
          value={h3SoundtrackDiegetic ? 'yes' : 'no'}
          onchange={(e) => {
            h3SoundtrackDiegetic = e.currentTarget.value === 'yes';
            onchange();
          }}
        >
          <option value="yes">Yes — they are singing to it</option>
          <option value="no">No — it is score over the picture</option>
        </select>
      </label>
      <p class="hint">
        This decides which section the prompt writer files the song under. "No" files it as
        non-diegetic music, which is defined as music the characters cannot hear — and the singing
        stops locking to the vocal.
      </p>
    </div>
  {/if}
</div>
