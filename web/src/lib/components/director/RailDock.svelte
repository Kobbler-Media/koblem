<script lang="ts">
  import { DEFAULT_VOCAB, capitalise, type EngineVocab } from '$lib/director/engine';
  import { assetSrc } from '$lib/director/assets.svelte';
  import type { DirectorTake } from '$lib/api/client';
  import type { ShotRenderStatus } from '$lib/director/shot-list';
  import type { RenderTier } from '$lib/director/tier';

  /**
   * The open shot's take, docked to the top of the 320px rail.
   *
   * ## 🔴 Why this is not `StageViewer` in a narrower box
   *
   * The viewer was moved into the rail once already, unchanged, and it was reverted: its head is a
   * row of slate, heading, mode line and two status chips built for an 820px column, and at 296px
   * that row wrapped into an unreadable strip that cost more height than the picture it labelled.
   * Nothing about the rail was wrong — the component was. This one is drawn for the width it
   * actually gets: **one** 16:9 frame, **one** line of text, and a 2×2 of buttons, with everything
   * that cannot survive truncation (mode, fps, duration, the proxy chip, the storyboard button)
   * deliberately left in the shot's own panels below, where there is room to read it.
   *
   * The dock is sticky, and that is the whole reason it is worth having in the rail: while you are
   * writing you are checking one moment against the line that made it, and the take used to scroll
   * off the top of the stage the moment you opened a panel.
   *
   * ## It is a player, not a thumbnail
   *
   * 🔴 It used to be a muted autoplaying loop with an overlay ▶ that did not play — it set
   * {@link expanded} and mounted `StageViewer` across the centre stage, shoving the composer down
   * the page to answer "what does this sound like". Reported 2026-08-13 as the thing that makes the
   * rail useless: the one picture on screen could not be paused, scrubbed, heard or fullscreened.
   *
   * So it carries NATIVE controls. The browser's own bar is the right answer at this width for the
   * reason the old comment got backwards: it is the one transport every user already knows, it
   * hands over fullscreen and volume for free, and it degrades to whatever the platform thinks fits.
   * Expanding is still there, as a corner affordance — a second way to watch, not the only way.
   */
  let {
    shotIndex,
    heading,
    vocab = DEFAULT_VOCAB,
    status,
    proxyStatus,
    stageSource,
    notRenderedStill,
    livePreview = null,
    takes = [],
    elapsed,
    activeExpectedSec,
    perShotEstimate,
    renderTier,
    hasPrompt,
    canRetake = true,
    retakeNote = '',
    retakeUnavailableNote = '',
    expanded = $bindable(false),
    onretake,
    onpreview,
    onrender,
    onopenrenders
  }: {
    shotIndex: number;
    /** The shot's gist / idea / prompt, per `shotHeading` — the muted half of the caption. */
    heading: string;
    vocab?: EngineVocab;
    status: ShotRenderStatus;
    /** The proxy chain's own status. Only the Retake button reads it, and only on the proxy tier. */
    proxyStatus: ShotRenderStatus;
    stageSource: { url: string; proxy: boolean } | null;
    notRenderedStill: { src: string; label: string } | null;
    /** In-flight frame from the engine, as an object URL the caller owns. */
    livePreview?: string | null;
    /**
     * The inspected shot's takes, when they have been read.
     *
     * ⚠️ Loaded on demand (the Takes panel's own button), so an empty list is NOT "never rendered"
     * — it is "not asked for". That is why the chip below falls back to a bare `rendered` instead
     * of claiming take 1.
     */
    takes?: DirectorTake[];
    elapsed: number;
    activeExpectedSec: number;
    perShotEstimate: number;
    renderTier: RenderTier;
    /** Whether the shot has a prompt to render at all. */
    hasPrompt: boolean;
    /**
     * This DEPLOYMENT can re-roll one unit — the capability and the engine's live bank together.
     * See the matching prop on `StageViewer`; `true` by default, so nothing else changes.
     */
    canRetake?: boolean;
    /** What a retake does to the units after it. See `retakeTailNote`. */
    retakeNote?: string;
    /** Why there is no retake, when there is none — the deployment's reason, not the model's. */
    retakeUnavailableNote?: string;
    expanded?: boolean;
    onretake: () => void;
    onpreview: () => void;
    onrender: () => void;
    onopenrenders: () => void;
  } = $props();

  const unit = $derived(capitalise(vocab.renderUnit));
  const busy = $derived(status === 'busy');
  const selectedTake = $derived(takes.find((t) => t.selected) ?? null);
  const chip = $derived(
    status === 'none'
      ? 'not rendered'
      : busy
        ? 'rendering'
        : selectedTake
          ? `take ${selectedTake.variant + 1}`
          : 'rendered'
  );

  // The job's own expectation when there is one, the per-shot average when there is not.
  const expectedSec = $derived(activeExpectedSec || perShotEstimate);
  // Capped short of 100 — a strip that fills before the file lands reads as a finished render.
  const percent = $derived(
    expectedSec > 0 ? Math.min(99, Math.max(2, (elapsed / expectedSec) * 100)) : 0
  );

  // Retake acts on whichever chain the tier is rendering, so it is the tier's status that decides
  // whether there is anything to re-roll. Same condition as the full viewer's.
  const retakeStatus = $derived(renderTier === 'proxy' ? proxyStatus : status);
  // 🔴 The prompt only. A render in flight and an offline engine are queue states — the submit
  // enqueues past both — and a button that read them would change under the cursor.
  const blocked = $derived(!hasPrompt);
  const primaryLabel = $derived(
    renderTier === 'proxy'
      ? proxyStatus === 'none'
        ? 'Preview'
        : 'Re-roll preview'
      : `Render ${vocab.renderUnit}`
  );
</script>

<div class="dock">
  <div class="dk-frame">
    {#if busy && livePreview}
      <img class="dk-fill" src={livePreview} alt="live preview frame" />
    {:else if stageSource}
      <!-- ⚠️ `preload="metadata"` and NOT autoplay: the rail holds a take per shot and a workspace
           full of shots that all start fetching themselves is a lot of bytes for film nobody asked
           to see. Metadata is enough for the duration to appear in the scrub bar.
           🔴 Not muted. Hearing the take is half of what H3 renders — it makes the soundtrack in the
           same pass as the picture — so a player that silently drops that is answering the wrong
           half of the question. -->
      <video
        class="dk-fill"
        src={stageSource.url}
        controls
        playsinline
        preload="metadata"
        poster={notRenderedStill ? assetSrc(notRenderedStill.src) : undefined}
      >
        <!-- A generated take has no caption track and cannot have one; the element is here so the
             a11y rule is answered explicitly rather than by suppressing it. -->
        <track kind="captions" />
      </video>
    {:else if notRenderedStill}
      <img class="dk-fill" src={assetSrc(notRenderedStill.src)} alt={notRenderedStill.label} />
    {/if}
    <!-- Top-right, and only a corner: the native control bar owns the bottom edge and the middle is
         the picture. Expanding is a second way to watch, so it must not sit on top of the first. -->
    <button
      class="dk-expand"
      title="Watch at full width — Escape brings you back"
      aria-label="Watch at full width"
      onclick={() => (expanded = true)}>⤢</button
    >
    {#if busy && percent > 0}
      <div class="dk-strip"><i style="width:{percent}%"></i></div>
    {/if}
  </div>

  <div class="dk-cap">
    <b>{unit} {shotIndex + 1}</b>
    <span class="dk-gist" title={heading}>{heading}</span>
    <span class="dk-chip" class:dk-none={status === 'none'}>{chip}</span>
  </div>

  <div class="dk-acts">
    <button
      class="btn sm"
      onclick={onretake}
      disabled={!canRetake || retakeStatus === 'none'}
      title={!canRetake
        ? retakeUnavailableNote
        : retakeStatus === 'none'
          ? `Render this ${vocab.renderUnit} first, then you can reroll it`
          : `A new seed for this ${vocab.renderUnit}. It ${retakeNote}`}>Retake</button
    >
    <button
      class="btn sm"
      onclick={onpreview}
      disabled={blocked || renderTier === 'proxy'}
      title={renderTier === 'proxy'
        ? 'On the proxy tier the render IS the preview'
        : `Cheap preview up to and including this ${vocab.renderUnit}`}>Preview</button
    >
    <button class="btn sm primary" onclick={onrender} disabled={blocked} title={primaryLabel}
      >{primaryLabel}</button
    >
    <button class="btn sm" onclick={onopenrenders} title="Every render this project has kept"
      >Renders</button
    >
  </div>

  {#if busy}
    <div class="dk-tick">
      {elapsed.toFixed(0)}s{expectedSec > 0 ? ` / ~${expectedSec.toFixed(0)}s` : ' elapsed'}
    </div>
  {/if}
</div>

<style>
  /* 🔴 `dk-` on every class. workspace.css already owns `.dw .chip`, `.dw .play`, `.dw .gist`,
     `.dw .frame` and `.dw .acts`, and those reach in here — a scoped `.chip` only TIES them on
     specificity, so which one wins comes down to stylesheet order. */
  .dock {
    display: flex;
    flex-direction: column;
    gap: 7px;
    min-width: 0;
  }
  .dk-frame {
    position: relative;
    width: 100%;
    aspect-ratio: 16 / 9;
    border-radius: 10px;
    background: var(--bg-3, var(--bg-2));
    border: 1px solid var(--ring);
    overflow: hidden;
  }
  .dk-fill {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .dk-expand {
    position: absolute;
    top: 6px;
    right: 6px;
    width: 26px;
    height: 26px;
    border-radius: 7px;
    border: 1px solid rgba(255, 255, 255, 0.32);
    background: rgba(0, 0, 0, 0.46);
    color: rgba(255, 255, 255, 0.92);
    font-size: 12px;
    line-height: 1;
    padding: 0;
    opacity: 0;
    transition: 0.14s;
  }
  /* Hidden until the pointer is on the picture, so it is not a permanent smudge in the corner of
     every take — but always reachable from the keyboard. */
  .dk-frame:hover .dk-expand,
  .dk-expand:focus-visible {
    opacity: 1;
  }
  .dk-expand:hover {
    background: rgba(0, 0, 0, 0.68);
    border-color: #fff;
  }
  /* 🔴 Pinned to the TOP edge. The native control bar owns the bottom of the frame now, and a
     progress line underneath it is a progress line nobody can see. */
  .dk-strip {
    position: absolute;
    left: 0;
    right: 0;
    top: 0;
    height: 2px;
    background: rgba(255, 255, 255, 0.14);
  }
  .dk-strip i {
    display: block;
    height: 100%;
    background: var(--accent);
  }

  .dk-cap {
    display: flex;
    align-items: baseline;
    gap: 6px;
    min-width: 0;
    font-size: 11.5px;
  }
  .dk-cap b {
    flex: none;
    font-weight: 650;
  }
  /* The one elastic cell in the row: everything else is fixed, so this is what gives. */
  .dk-gist {
    flex: 1 1 auto;
    min-width: 0;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .dk-chip {
    flex: none;
    font-family: var(--mono);
    font-size: 9.5px;
    color: var(--accent);
    border: 1px solid var(--ring);
    border-radius: 5px;
    padding: 1px 5px;
    white-space: nowrap;
  }
  .dk-chip.dk-none {
    color: var(--muted);
  }

  .dk-acts {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 5px;
    min-width: 0;
  }
  /* `.btn` is an inline-flex sized by its label; in a fixed 1fr cell an over-long label would push
     the grid wider than the rail rather than wrap. Centre it and let the cell clip. */
  .dk-acts .btn {
    justify-content: center;
    min-width: 0;
    overflow: hidden;
  }
  .dk-tick {
    font-size: 10.5px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }
</style>
