<script lang="ts">
  import { DEFAULT_VOCAB, type EngineVocab } from '#lib/director/engine.ts';
  // Stage 1 of auto-direct: a brief (and optionally a song) in, a proposed shot list out. Nothing
  // reaches the timeline until the owner reviews the beats — the plan sheet does that.
  let {
    autoBrief = $bindable(),
    autoMaxShots = $bindable(),
    autoDirecting,
    audioFile,
    vocab = DEFAULT_VOCAB,
    hasAudio,
    onaudio,
    onopenaudio,
    projectSoundtrack = true,
    ondirect
  }: {
    autoBrief: string;
    autoMaxShots: number;
    autoDirecting: boolean;
    audioFile: File | null;
    /** Whether a track is actually stored — an auto-direct can run off the song alone. */
    hasAudio: boolean;
    onaudio: (files: FileList | null) => void;
    onopenaudio: () => void;
    /** Whether a song can be attached to the project at all. True on both engines. */
    projectSoundtrack?: boolean;
    /** What this engine calls its units. The planner emits CUTS — see `EngineVocab`. */
    vocab?: EngineVocab;
    ondirect: () => void;
  } = $props();
</script>

<label class="field"
  ><span>Brief</span>
  <textarea
    class="inp"
    rows="3"
    bind:value={autoBrief}
    placeholder="a moody neon noir chase, ending on a rooftop as the rain breaks"></textarea>
</label>
<div class="rowgrid">
  <label class="field"
    ><span>Max {vocab.subUnitPlural ?? vocab.renderUnitPlural}</span><input
      class="inp"
      type="number"
      min="1"
      max="64"
      bind:value={autoMaxShots}
    /></label
  >
  <!-- Shown on both engines. The planner cuts to word onsets, which is timeline metadata rather
       than anything the engine sees; how the track then REACHES the engine is
       `caps.soundtrackRoute`'s business, and on H3 it does reach it as per-generation slices. -->
  {#if projectSoundtrack}
    <label class="field"
      ><span>Song (optional)</span>
      {#if !audioFile}
        <label class="drop" style="min-height:34px;padding:0;font-size:11px"
          >Add song
          <input
            type="file"
            accept="audio/*"
            hidden
            onchange={(e) => onaudio(e.currentTarget.files)}
          />
        </label>
      {:else}
        <button class="drop" style="min-height:34px;padding:0;font-size:11px" onclick={onopenaudio}
          >♪ {audioFile.name}</button
        >
      {/if}
    </label>
  {/if}
</div>
<button
  class="btn primary"
  style="width:100%;justify-content:center"
  onclick={ondirect}
  disabled={autoDirecting || (!autoBrief.trim() && !hasAudio)}
>
  {autoDirecting ? 'Directing…' : 'Auto-direct'}
</button>
<p class="hint" style="margin-top:8px">
  Plans the beats and shows them for review. On Go it drafts every prompt in the background.
</p>
