<script lang="ts">
  import Timeline from './Timeline.svelte';
  import type { DirectorClip, LyricWord } from '$lib/api/client';
  import type { ShotRenderStatus } from '$lib/director/shot-list';
  import type { PackedGeneration } from '$lib/director/h3-generations';
  import { DEFAULT_VOCAB, capitalise, type EngineVocab } from '$lib/director/engine';

  // The cut, and the edits that act on it. DESKTOP ONLY — below `DIRECTOR_COMPACT` the phone
  // shell replaces the whole workspace, so this bar and its time axis are never mounted there and
  // have no narrow form to fall back to.
  let {
    clips = $bindable(),
    selectedIds = $bindable(),
    selectedPin = $bindable(),
    pxPerSec = $bindable(),
    windowSeconds = $bindable(),
    fitTimeline = $bindable(),
    playhead = $bindable(),
    playing = $bindable(false),
    audioSrc = null,
    snap = $bindable(),
    batchText = $bindable(),
    lyrics,
    waveform,
    waveDuration,
    fps,
    zoomSel,
    totalDuration,
    selectedIndex,
    canUndo,
    canRedo,
    promptBusy,
    storyboarding,
    imageEngineReady,
    extending,
    anyDraftFailed,
    shotStatus,
    proxyStatus,
    findingSeverity,
    draftFailed,
    onchange,
    onadd,
    onsplit,
    onmerge,
    onduplicate,
    ondelete,
    onundo,
    onredo,
    onzoom,
    onzoomwindow,
    onopenaudio,
    projectSoundtrack = true,
    generations = [],
    mark = null,
    onreworkselected,
    onstoryboardselected,
    oncontinuescene,
    onopengeneration,
    vocab = DEFAULT_VOCAB
  }: {
    /** What this engine calls its units — passed straight through to `Timeline`. See `EngineVocab`. */
    vocab?: EngineVocab;
    clips: DirectorClip[];
    selectedIds: string[];
    selectedPin: { clipId: string; index: number } | null;
    pxPerSec: number;
    windowSeconds: number | null;
    fitTimeline: boolean;
    playhead: number;
    /** Whether the project song is playing — see `Timeline`, which owns the `<audio>`. */
    playing?: boolean;
    /** The project song as a loadable URL, or null. Only the time axis can play it. */
    audioSrc?: string | null;
    snap: boolean;
    batchText: string;
    lyrics: LyricWord[];
    waveform: Float32Array | null;
    waveDuration: number;
    fps: number;
    zoomSel: string;
    totalDuration: number;
    selectedIndex: number;
    canUndo: boolean;
    canRedo: boolean;
    promptBusy: boolean;
    storyboarding: boolean;
    imageEngineReady: boolean;
    extending: boolean;
    anyDraftFailed: boolean;
    shotStatus: (c: DirectorClip) => ShotRenderStatus;
    proxyStatus: (c: DirectorClip) => ShotRenderStatus;
    findingSeverity: (clipId: string) => 'warn' | 'info' | null;
    draftFailed: (clipId: string) => boolean;
    onchange: () => void;
    onadd: () => void;
    onsplit: () => void;
    onmerge: () => void;
    onduplicate: () => void;
    ondelete: () => void;
    onundo: () => void;
    onredo: () => void;
    onzoom: (delta: number) => void;
    onzoomwindow: (v: string) => void;
    onopenaudio: () => void;
    /** Whether a song can be attached to the project. True on both engines — the lane is an
     *  authoring spine (onsets, cuts) before it is ever engine conditioning. */
    projectSoundtrack?: boolean;
    /**
     * MiniMax-H3 generations — the render units the shots are packed into. Empty on LTX, where a
     * shot is a render.
     */
    generations?: PackedGeneration[];
    /**
     * A moment pointed at from elsewhere — a cut time under the cursor in the prompt editor — in
     * SONG seconds.
     */
    mark?: { seconds: number; label: string } | null;
    onreworkselected: () => void;
    onstoryboardselected: () => void;
    oncontinuescene: () => void;
    onopengeneration?: (index: number) => void;
  } = $props();
</script>

<div class="tl">
  <div class="tl-bar">
    <div class="tgroup">
      <button class="tbtn" onclick={onadd}>Add</button>
      <button class="tbtn" onclick={onsplit}>Split</button>
      <button class="tbtn" class:off={selectedIds.length < 2} onclick={onmerge}>Merge</button>
      <button class="tbtn" class:off={selectedIds.length === 0} onclick={onduplicate}
        >Duplicate</button
      >
      <button class="tbtn" class:off={selectedIds.length === 0} onclick={ondelete}>Delete</button>
    </div>
    <div class="tgroup">
      <button class="tbtn" class:off={!canUndo} onclick={onundo}>Undo</button>
      <button class="tbtn" class:off={!canRedo} onclick={onredo}>Redo</button>
    </div>
    <div class="tgroup">
      <button class="tbtn icn" onclick={() => onzoom(-20)}>−</button>
      <button class="tbtn icn" onclick={() => onzoom(20)}>+</button>
      <select
        class="tsel"
        value={zoomSel}
        onchange={(e) => onzoomwindow(e.currentTarget.value)}
        title="Timeline window (seconds visible)"
      >
        <option value="10">10s</option>
        <option value="15">15s</option>
        <option value="30">30s</option>
        <option value="60">60s</option>
        <option value="fit">Fit</option>
        {#if zoomSel === 'custom'}<option value="custom" disabled>Custom</option>{/if}
      </select>
    </div>
    <label class="snap"
      ><input type="checkbox" bind:checked={snap} style="accent-color:var(--accent)" /> Snap</label
    >
    <div class="tl-right">
      {#if projectSoundtrack}
        <button class="btn sm ghost" onclick={onopenaudio}>Audio &amp; lyrics</button>
      {/if}
      <span class="tl-meta"
        >{#if selectedIds.length > 1}{selectedIds.length} selected ·
        {:else}<b
            >{capitalise(vocab.renderUnit)}
            {String(selectedIndex + 1).padStart(2, '0')}</b
          > ·
        {/if}<b
          title="One {vocab.renderUnit} is one render. Timed moments inside it are written into its prompt and the model renders the cuts between them itself."
          >{clips.length}
          {clips.length === 1 ? vocab.renderUnit : vocab.renderUnitPlural}</b
        >
        · <b>{totalDuration.toFixed(1)}s</b></span
      >
    </div>
  </div>

  {#if selectedIds.length > 1}
    <div class="batchbar">
      <b>{selectedIds.length} {vocab.renderUnitPlural} selected</b>
      <input
        class="inp"
        bind:value={batchText}
        placeholder="rework all selected — e.g. push it all to night…"
      />
      <button class="btn sm" onclick={onreworkselected} disabled={promptBusy || !batchText.trim()}
        >Rework selected</button
      >
      <button
        class="btn sm"
        onclick={onstoryboardselected}
        disabled={storyboarding || !imageEngineReady}>Storyboard selected</button
      >
      <button class="btn sm" onclick={onmerge}>Merge into one</button>
      <button class="btn sm danger" onclick={ondelete}>Delete</button>
    </div>
  {/if}

  <div style="padding:4px 8px">
    <Timeline
      bind:clips
      bind:pxPerSec
      bind:windowSeconds
      bind:fitMode={fitTimeline}
      bind:playhead
      bind:playing
      bind:selectedIds
      bind:selectedPin
      {audioSrc}
      {lyrics}
      {waveform}
      {waveDuration}
      {snap}
      {fps}
      {generations}
      {vocab}
      {mark}
      audioLane={projectSoundtrack}
      {onchange}
      {shotStatus}
      {proxyStatus}
      {findingSeverity}
      {draftFailed}
      {onopengeneration}
      onContinueScene={extending ? undefined : oncontinuescene}
    />
  </div>

  <div class="legend">
    <span class="k"><span class="d" style="background:var(--success)"></span> rendered</span>
    <span class="k"><span class="d" style="background:var(--accent)"></span> rendering</span>
    <span class="k"
      ><span class="d" style="background:var(--warning)"></span> edited since render</span
    >
    <span class="k"><span class="d" style="border-color:var(--muted)"></span> not rendered</span>
    {#if anyDraftFailed}<span class="k"
        ><span class="d" style="background:var(--danger);border-color:var(--danger)"></span> draft failed</span
      >{/if}
    {#if extending}<span style="margin-left:auto;color:var(--accent)">Drafting next shot…</span
      >{/if}
  </div>
</div>
