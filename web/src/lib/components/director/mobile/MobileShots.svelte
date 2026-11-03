<script lang="ts">
  import { waveformPeaks } from '@koblibs/ui';
  import { assetSrc } from '#lib/director/assets.svelte.ts';
  import type { DirectorClip } from '#lib/api/client.ts';
  import {
    moveShot,
    shotGist,
    shotStateLabel,
    type ShotRenderStatus
  } from '#lib/director/shot-list.ts';
  import type { RenderTier } from '#lib/director/tier.ts';
  import { h3GenerationGist, type PackedGeneration } from '#lib/director/h3-generations.ts';
  import {
    DEFAULT_VOCAB,
    type EngineCapabilities,
    type EngineVocab
  } from '#lib/director/engine.ts';
  import type { TimelineLayout } from '#lib/director/timeline-layout.ts';
  import { tickLabel } from '#lib/director/timeline-ticks.ts';

  // The cut, as a list. Time is not a usable axis in the hand, so what the timeline draws on one
  // becomes a row: order, poster, gist, render state in words, and ↑/↓ instead of a drag.
  let {
    clips = $bindable([]),
    selectedIds = $bindable([]),
    playhead = $bindable(0),
    playing = $bindable(false),
    generations = [],
    vocab = DEFAULT_VOCAB,
    caps,
    tier,
    layout,
    audioSrc = null,
    waveform = null,
    songSeconds = 0,
    shotStatus,
    proxyStatus,
    shotSource,
    findingSeverity,
    draftFailed,
    onchange,
    onopen,
    onadd,
    oncontinuescene,
    onduplicate,
    ondelete
  }: {
    clips: DirectorClip[];
    selectedIds: string[];
    /** Delivered seconds — the same clock the song and the timeline's x-axis are on. */
    playhead: number;
    playing: boolean;
    generations?: PackedGeneration[];
    vocab?: EngineVocab;
    caps: EngineCapabilities;
    tier: RenderTier;
    layout: TimelineLayout;
    audioSrc?: string | null;
    waveform?: Float32Array | null;
    songSeconds?: number;
    shotStatus: (c: DirectorClip) => ShotRenderStatus;
    proxyStatus: (c: DirectorClip) => ShotRenderStatus;
    /** The take this client would play for a shot, at its tier. `null` when there is nothing yet. */
    shotSource: (c: DirectorClip) => string | null;
    findingSeverity?: (clipId: string) => 'warn' | 'info' | null;
    draftFailed?: (clipId: string) => boolean;
    onchange: () => void;
    onopen: (clipId: string) => void;
    onadd: () => void;
    oncontinuescene: () => void;
    onduplicate: () => void;
    ondelete: () => void;
  } = $props();

  const clipId = $props.id();
  let audioEl = $state<HTMLAudioElement | null>(null);
  let playingShot = $state<string | null>(null);
  let actionsOpen = $state(false);

  const selectedSet = $derived(new Set(selectedIds));

  // Everything a row draws that lives on the RENDER rather than the clip. On H3 the authored text
  // is the generation's, so a shot with an empty `clip.prompt` may still be fully written.
  const shotMeta = $derived.by(() => {
    const m = new Map<
      string,
      { first: boolean; rendered: number; gist: string; written: boolean; hardCut: boolean }
    >();
    for (const g of generations) {
      const gist = h3GenerationGist(g);
      const written = !!g.doc.prompt?.trim();
      g.shots.forEach((s, i) =>
        m.set(s.clip.id, {
          first: i === 0,
          rendered: s.renderedSeconds,
          gist,
          written,
          hardCut: g.continuation?.mode === 'none'
        })
      );
    }
    return m;
  });

  const shotAtPlayhead = $derived.by(() => {
    const hit = layout.clips.find((l) => playhead >= l.start && playhead < l.start + l.width);
    if (!hit) return '';
    const i = clips.findIndex((c) => c.id === hit.id);
    return i < 0 ? '' : `${vocab.renderUnit} ${i + 1}`;
  });

  // 64 buckets is what a 300px-wide bar can resolve; more is noise the eye reads as a solid block.
  const bars = $derived(waveform?.length ? waveformPeaks(waveform, 64) : new Float32Array(0));
  const playedPct = $derived(songSeconds > 0 ? Math.min(100, (playhead / songSeconds) * 100) : 0);

  // `playing` is the workspace's, not this component's — the keyboard transport and a shot take
  // both move it — so the element follows the flag rather than only the button.
  $effect(() => {
    const a = audioEl;
    if (!a) return;
    if (playing && a.paused) void a.play().catch(() => (playing = false));
    else if (!playing && !a.paused) a.pause();
  });

  function toggleSong() {
    if (!audioEl) return;
    if (!playing) playingShot = null;
    playing = !playing;
  }

  function seekTo(at: number) {
    if (songSeconds <= 0) return;
    playhead = Math.max(0, Math.min(songSeconds, at));
    if (audioEl) audioEl.currentTime = playhead;
  }

  function seek(e: MouseEvent) {
    // 🔴 `detail === 0` is a KEYBOARD activation of the button, where `clientX` is 0 — so Enter or
    // Space would compute an offset of zero and rewind the song to the start. The arrows below are
    // what the keyboard seeks with.
    if (e.detail === 0) return;
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (rect.width <= 0) return;
    seekTo(((e.clientX - rect.left) / rect.width) * songSeconds);
  }

  const SEEK_STEP = 5;
  function seekKey(e: KeyboardEvent) {
    const at =
      e.key === 'ArrowRight'
        ? playhead + SEEK_STEP
        : e.key === 'ArrowLeft'
          ? playhead - SEEK_STEP
          : e.key === 'Home'
            ? 0
            : e.key === 'End'
              ? songSeconds
              : null;
    if (at === null) return;
    e.preventDefault();
    seekTo(at);
  }

  function move(i: number, delta: number) {
    const next = moveShot(clips, i, delta);
    // A no-op reorder must not report an edit: it would dirty the project and, under optimistic
    // concurrency, risk a 409 banner for a tap that changed nothing.
    if (!next) return;
    clips = next;
    onchange();
  }

  function open(c: DirectorClip) {
    selectedIds = [c.id];
    onopen(c.id);
  }

  function poster(c: DirectorClip) {
    const still = c.image_b64 || c.snapshot_b64 || c.keyframes[0]?.image_b64;
    return still ? assetSrc(still) : null;
  }

  function tapPoster(c: DirectorClip) {
    if (playingShot === c.id) {
      playingShot = null;
      return;
    }
    if (!shotSource(c)) {
      open(c);
      return;
    }
    // Two sources at once is never what was meant, and on a phone the song is the one still audible
    // after the take starts.
    playing = false;
    playingShot = c.id;
  }
</script>

{#if audioSrc}
  <div class="songbar">
    <button
      class="sb-play"
      type="button"
      onclick={toggleSong}
      aria-label={playing ? 'Pause the song' : 'Play the song'}>{playing ? '❚❚' : '▶'}</button
    >
    <div class="track">
      <button
        class="seek"
        type="button"
        onclick={seek}
        onkeydown={seekKey}
        aria-label="Seek the song — arrows move {SEEK_STEP} seconds"
      >
        {#if bars.length}
          <svg class="wave" viewBox="0 0 64 24" preserveAspectRatio="none" aria-hidden="true">
            <defs>
              <clipPath id="{clipId}-played">
                <rect x="0" y="0" width={(playedPct / 100) * 64} height="24" />
              </clipPath>
            </defs>
            {#snippet lanes()}
              {#each bars as b, i (i)}
                <rect
                  x={i + 0.15}
                  y={12 - Math.max(0.5, b * 11)}
                  width="0.7"
                  height={Math.max(1, b * 22)}
                />
              {/each}
            {/snippet}
            <g class="bed">{@render lanes()}</g>
            <g class="played" clip-path="url(#{clipId}-played)">{@render lanes()}</g>
          </svg>
        {:else}
          <span class="wave"></span>
        {/if}
      </button>
      <div class="tt">
        <span>{tickLabel(playhead, 1)}</span>
        <span>{shotAtPlayhead}</span>
        <span>{tickLabel(songSeconds, 1)}</span>
      </div>
    </div>
  </div>
  <audio
    bind:this={audioEl}
    src={audioSrc}
    preload="metadata"
    ontimeupdate={() => audioEl && (playhead = audioEl.currentTime)}
    onended={() => (playing = false)}
  ></audio>
{/if}

{#each clips as c, i (c.id)}
  {@const meta = shotMeta.get(c.id)}
  {@const fin = shotStatus(c)}
  {@const prox = proxyStatus(c)}
  {@const src = shotSource(c)}
  {@const still = poster(c)}
  {@const dfail = draftFailed ? draftFailed(c.id) : false}
  {@const flag = findingSeverity ? findingSeverity(c.id) : null}
  {@const written = meta ? meta.written : !!c.prompt.trim()}
  <div class="shot" data-sel={selectedSet.has(c.id) ? 1 : 0} data-bad={dfail ? 1 : 0}>
    <button
      class="thumb"
      class:empty={!still && !src}
      type="button"
      onclick={() => tapPoster(c)}
      aria-label={src
        ? playingShot === c.id
          ? `Stop ${vocab.renderUnit} ${i + 1}`
          : `Play ${vocab.renderUnit} ${i + 1}`
        : `Open ${vocab.renderUnit} ${i + 1}`}
    >
      {#if playingShot === c.id && src}
        <!-- svelte-ignore a11y_media_has_caption -->
        <video {src} autoplay playsinline onended={() => (playingShot = null)}></video>
      {:else if still}
        <img src={still} alt="" />
      {/if}
      {#if !still && !src}<span class="gl">＋</span>{/if}
      {#if src && playingShot !== c.id}<span class="pv">▶</span>{/if}
      <!-- `caps.beats` is the `clip.beats` ARRAY, not "this engine has beats". Off it the compiler
           drops the array, so a dot per entry would count something the render never sees. -->
      {#if caps.beats && (c.beats ?? []).length}
        <span class="bt"
          >{#each c.beats ?? [] as _beat, bi (bi)}<span></span>{/each}</span
        >
      {/if}
      <span class="idx">{i + 1}</span>
      <span class="dur">{(meta?.rendered ?? c.seconds).toFixed(1)}s</span>
    </button>

    <button class="open" type="button" onclick={() => open(c)}>
      <span class="states">
        {#if dfail}
          <span class="pill bad"><i></i>draft failed</span>
        {:else if !written}
          <span class="pill none"><i></i>no prompt</span>
        {/if}
        {#if tier === 'proxy'}
          <span class="pill {prox}"><i></i>preview · {shotStateLabel(prox)}</span>
          {#if fin !== 'none'}<span class="pill {fin}"><i></i>final · {shotStateLabel(fin)}</span
            >{/if}
        {:else}
          <span class="pill {fin}"><i></i>{shotStateLabel(fin)}</span>
          {#if prox !== 'none'}<span class="pill {prox}"
              ><i></i>preview · {shotStateLabel(prox)}</span
            >{/if}
        {/if}
        {#if meta?.first && i > 0 && meta.hardCut}<span class="pill cut"><i></i>hard cut</span>{/if}
        {#if flag}<span class="pill stale"><i></i>review finding</span>{/if}
      </span>
      <span class="line" class:blank={!written}>
        {written ? meta?.gist || shotGist(c) : 'No prompt yet — tap to write one'}
      </span>
    </button>

    <span class="arrows">
      <button
        type="button"
        aria-label="Move {vocab.renderUnit} {i + 1} earlier"
        disabled={i === 0}
        onclick={() => move(i, -1)}>↑</button
      >
      <button
        type="button"
        aria-label="Move {vocab.renderUnit} {i + 1} later"
        disabled={i === clips.length - 1}
        onclick={() => move(i, 1)}>↓</button
      >
    </span>
  </div>
{/each}

{#if clips.length === 0}
  <p class="hint">No {vocab.renderUnitPlural} yet — add one to start directing.</p>
{/if}

<div class="addrow">
  <button class="btn full" type="button" onclick={onadd}>＋ Add {vocab.renderUnit}</button>
  <button
    class="btn ghost icn"
    type="button"
    aria-expanded={actionsOpen}
    aria-label="More {vocab.renderUnit} actions"
    onclick={() => (actionsOpen = !actionsOpen)}>⋯</button
  >
</div>

{#if actionsOpen}
  <div class="grp" style="margin-top:8px">
    <p class="eye">The selected {vocab.renderUnit}</p>
    <button class="btn full" type="button" onclick={oncontinuescene} disabled={clips.length === 0}
      >＋ Continue scene</button
    >
    <button class="btn full" type="button" onclick={onduplicate} disabled={selectedIds.length === 0}
      >⧉ Duplicate</button
    >
    <button
      class="btn full danger"
      type="button"
      onclick={ondelete}
      disabled={selectedIds.length === 0}>✕ Delete</button
    >
    <p class="hint">
      Continue scene drafts the next {vocab.renderUnit} from the story so far — no render needed.
    </p>
  </div>
{/if}

<p class="hint" style="margin-top:11px">
  Trimming, pin placement, split-at-playhead and merge need a time axis to drag on, so they are on
  wider screens only. Length, timed changes and keyframe pins are all editable in a {vocab.renderUnit}'s
  own Prompt screen.
</p>
