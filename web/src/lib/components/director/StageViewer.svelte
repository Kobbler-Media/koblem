<script lang="ts">
  import { DEFAULT_VOCAB, type EngineVocab } from '$lib/director/engine';
  import { assetSrc } from '$lib/director/assets.svelte';
  import type { DirectorClip } from '$lib/api/client';
  import type { ShotRenderStatus } from '$lib/director/shot-list';
  import type { RenderTier } from '$lib/director/tier';
  import { formatDuration, type H3Telemetry } from '$lib/director/h3-telemetry';

  // The stage: which shot is open, what it looks like right now, and the two buttons that put it
  // through the engine. What plays here is picked by tier — on `proxy` a 512×288 clip wins, because
  // it is the one a phone can actually stream.
  let {
    clip,
    shotIndex,
    fps,
    status,
    proxyStatus,
    expanded = $bindable(),
    collapsed = $bindable(false),
    heading,
    modeLabel,
    stageSource,
    notRenderedStill,
    elapsed,
    activeExpectedSec,
    perShotEstimate,
    telemetry = null,
    livePreview = null,
    vocab = DEFAULT_VOCAB,
    proxyW,
    proxyH,
    previewFullRes,
    renderTier,
    snapBusy,
    imageEngineReady,
    canRetake = true,
    retakeNote = '',
    retakeUnavailableNote = '',
    onretake,
    onsnapshot,
    onpreview,
    onrender,
    onopenrenders
  }: {
    /**
     * What this engine calls its units.
     *
     * ⚠️ Almost every "shot" in this component is ALREADY right on both engines, and that is the
     * point of the vocabulary rather than an oversight: Retake, Preview and Render all act on the
     * RENDER UNIT, which is a shot on LTX and a shot on H3. What is not the render unit is the
     * storyboard still, which belongs to the one clip — a cut, where a render has cuts.
     */
    vocab?: EngineVocab;
    clip: DirectorClip;
    shotIndex: number;
    fps: number;
    status: ShotRenderStatus;
    proxyStatus: ShotRenderStatus;
    expanded: boolean;
    /**
     * Hide the picture and give the whole stage to the prompt.
     *
     * 🔴 The picture goes; **the shot's controls stay.** Collapsed is "I am writing now", not "I have
     * left this shot" — an author who has to expand the viewer to find Render has been given a worse
     * version of scrolling. The status line above stays too, because "rendering" and "edited since
     * render" are the two facts that decide whether the next keystroke matters.
     *
     * Separate from {@link expanded} rather than a third value of it: they are opposite ends of the
     * same axis but they are set from opposite intents, and one tri-state control would make
     * "bigger" and "out of my way" the same button pressed different numbers of times.
     */
    collapsed?: boolean;
    heading: string;
    modeLabel: string;
    /** The clip this stage plays, and whether it is the proxy one. */
    stageSource: { url: string; proxy: boolean } | null;
    /** Best still to show instead of a blank gradient before a shot has ever rendered. */
    notRenderedStill: { src: string; label: string } | null;
    elapsed: number;
    activeExpectedSec: number;
    perShotEstimate: number;
    /**
     * The engine's own live telemetry for the generation THIS shot is in, or `null`.
     *
     * 🔴 Only MiniMax-H3 reports any of this — an LTX job carries none of these keys and the
     * caller passes `null`, so every readout below is additive and the LTX path is byte-identical
     * to what it was. `null` also covers "H3, but the shim said nothing yet": absence is UNKNOWN,
     * and the elapsed/estimate line underneath is what a render with no telemetry still shows.
     *
     * The single most valuable member is `swapping` (`phase: "loading"`). It is a full ~20 GB
     * checkpoint reload between two generations — minutes with the step counter frozen — and left
     * undrawn it is indistinguishable from a hang on a render that legitimately takes an hour.
     */
    telemetry: H3Telemetry | null;
    /**
     * An in-flight preview frame for this shot's generation, as an object URL, or `null`.
     *
     * Opt-in per project (`generate_preview` decodes latents through the RGB-factor path on every
     * emission and that cost is unmeasured), and never an error when absent — a render that did not
     * ask for previews simply has none. The caller owns and revokes the URL.
     */
    livePreview: string | null;
    /** The canvas a PREVIEW renders at — equal to the output canvas when `previewFullRes`. */
    proxyW: number;
    proxyH: number;
    /**
     * The preview tier is the full canvas at fewer steps, so it is PREDICTIVE of the final render
     * (same latent shape ⇒ byte-identical initial noise ⇒ same framing and same audio bed) rather
     * than merely cheap. Changes what the preview controls promise: "same shot, sooner", not
     * "a rough small version".
     */
    previewFullRes: boolean;
    renderTier: RenderTier;
    snapBusy: boolean;
    imageEngineReady: boolean;
    /**
     * This DEPLOYMENT can re-roll one unit — the capability and the engine's live bank together.
     *
     * 🔴 Not a property of the model. LTX-2.5 implements a single-shot retake and answers 501 to it
     * when the box has no bank directory configured, so a button drawn off the capability alone
     * dispatches, waits out the queue, and lands as an error on the job. Defaults `true` so the
     * engines that always could are unchanged.
     */
    canRetake?: boolean;
    /** What a retake does to the units after it. See `retakeTailNote`. */
    retakeNote?: string;
    /** Why there is no retake, when there is none — the deployment's reason, not the model's. */
    retakeUnavailableNote?: string;
    onretake: () => void;
    onsnapshot: () => void;
    onpreview: () => void;
    onrender: () => void;
    onopenrenders: () => void;
  } = $props();
</script>

<div class="stage-top">
  <div class="shot-head">
    <div class="slate">
      {String(shotIndex + 1).padStart(2, '0')}
    </div>
    <div>
      <h1 title={heading}>
        {heading}
      </h1>
      <div class="sub">
        {clip.seconds.toFixed(1)}s · {modeLabel} · {fps} fps
      </div>
    </div>
    <div class="rstate" class:ok={status === 'ok' || status === 'stale'}>
      <span class="rdot"></span>{status === 'busy'
        ? 'Rendering'
        : status === 'stale'
          ? 'Rendered · edited'
          : status === 'ok'
            ? 'Rendered'
            : 'Not rendered'}
    </div>
    {#if proxyStatus !== 'none'}
      <div
        class="rstate"
        class:ok={proxyStatus === 'ok' || proxyStatus === 'stale'}
        title="Proxy preview status (separate from the final render)"
      >
        <span class="rdot"></span>Preview · {proxyStatus === 'busy'
          ? 'rendering'
          : proxyStatus === 'stale'
            ? 'edited since'
            : 'ready'}
      </div>
    {/if}
    <!-- In the shot head rather than on the viewer, because when it is collapsed the viewer is the
         thing that is not there — a control that hides itself has no way back.
         🔴 Drawn as the same disclosure the rail's folds use: a chevron that ROTATES between the two
         states, plus the noun it acts on. It read as a toggle chip before, which said nothing about
         what pressing it would do — a triangle that turns is the one shape in this app that already
         means "this opens and closes". -->
    <button
      class="viewer-fold"
      class:open={!collapsed}
      title={collapsed ? 'Show the video' : 'Hide the video and give the whole width to the prompt'}
      aria-label={collapsed ? 'Show the video' : 'Hide the video'}
      aria-expanded={!collapsed}
      onclick={() => (collapsed = !collapsed)}
      ><span class="chev" aria-hidden="true">▶</span>Video</button
    >
  </div>
  {#if !collapsed}
    <div class="viewer" class:big={expanded}>
      <div class="frame"></div>
      <span class="slug">SHOT {String(shotIndex + 1).padStart(2, '0')}</span>
      <button
        class="viewer-expand"
        title={expanded
          ? 'Put it back in the rail'
          : 'Watch at full width \u2014 Escape brings you back'}
        aria-label={expanded ? 'Put the video back in the rail' : 'Watch at full width'}
        onclick={() => (expanded = !expanded)}>{expanded ? '⤡' : '⤢'}</button
      >
      {#if stageSource}
        <!-- controls give scrub + native fullscreen (⛶). The final clip stays muted so
           autoplay is allowed (the control bar unmutes); the proxy plays with sound,
           because judging music reactivity / lipsync is what it is for. -->
        <video
          src={stageSource.url}
          muted={!stageSource.proxy}
          loop
          autoplay
          playsinline
          controls
          class="stage-vid"
        ></video>
        {#if stageSource.proxy}
          <span class="live-tag muted"
            >{previewFullRes ? 'Preview · fewer steps' : 'Proxy preview'} · {proxyW}×{proxyH}</span
          >
        {:else}
          <span class="live-tag">Live · shot just rendered</span>
        {/if}
      {:else if status === 'none'}
        {#if notRenderedStill}
          <img class="stage-vid" src={assetSrc(notRenderedStill.src)} alt="shot preview" />
          <span class="live-tag muted">Not rendered · {notRenderedStill.label}</span>
          {#if clip.keyframes.length}
            <div class="kf-row kf-float">
              {#each clip.keyframes as kf, i (i)}
                <div
                  class="kf"
                  style="background-image:url('{assetSrc(kf.image_b64)}');background-size:cover"
                >
                  <span>{kf.seconds.toFixed(1)}s</span>
                </div>
              {/each}
            </div>
          {/if}
        {:else}
          <div class="prestate">
            <div class="ps-title">Not rendered yet</div>
            <div class="ps-sub">
              Add an i2v start frame, storyboard this {vocab.subUnit ?? vocab.renderUnit}, or pin a
              keyframe to preview it here — or render to see footage.
            </div>
          </div>
        {/if}
      {:else if status === 'busy'}
        {#if livePreview}
          <!-- The engine's in-flight frame, in the video area where the placeholder would be. The
             URL never changes across a render — the caller re-fetches on `preview_seq` and hands a
             fresh object URL down, which is what re-renders this. -->
          <img class="stage-vid" src={livePreview} alt="live preview frame" />
          <span class="live-tag">Live preview · in progress</span>
        {:else}
          <div class="prestate">
            <!-- 🔴 A checkpoint swap is not a stall. It is minutes of frozen step counter while ~20 GB
               of DiT and text encoder load, and on a render that legitimately takes an hour it is
               the thing most likely to be mistaken for a hang and cancelled. -->
            <div class="ps-title">
              {telemetry?.phaseLabel ?? 'Rendering…'}
            </div>
            <div class="ps-sub">
              {#if telemetry?.phaseNote}
                {telemetry.phaseNote}
              {:else}
                In the render queue — {elapsed.toFixed(0)}s{activeExpectedSec
                  ? ` / ~${activeExpectedSec.toFixed(0)}s`
                  : ''}.
              {/if}
            </div>
          </div>
        {/if}
        {#if telemetry?.known}
          <!-- Every part of this line is omitted when the engine did not report it. There is no
             "step 0 of 0" and no 0% bar: absence means unknown, and a bar drawn at 0 for an unknown
             reads as "finished" at one glance and "not started" at the next. -->
          <div class="h3-tel">
            {#if telemetry.percent !== null}
              <div class="tel-track" class:idle={telemetry.swapping}>
                <div class="tel-fill" style="width:{telemetry.percent}%"></div>
              </div>
            {/if}
            <div class="tel-line">
              {#if telemetry.phaseLabel && livePreview}<span
                  class="tel-phase"
                  class:swap={telemetry.swapping}>{telemetry.phaseLabel}</span
                >{/if}
              {#if telemetry.generationLabel}<span>{telemetry.generationLabel}</span>{/if}
              {#if telemetry.stepLabel}<span>{telemetry.stepLabel}</span>{/if}
              {#if telemetry.percent !== null}<span>{telemetry.percent.toFixed(0)}%</span>{/if}
              {#if telemetry.etaSeconds !== null}<span
                  title="The engine's own estimate for the whole job, including any checkpoint reload"
                  >~{formatDuration(telemetry.etaSeconds)} total</span
                >{/if}
            </div>
          </div>
        {/if}
      {:else}
        <button class="play" aria-label="Watch in the Renders drawer" onclick={onopenrenders}
        ></button>
      {/if}
    </div>
  {/if}
  <div class="vfoot">
    <button
      class="btn sm"
      onclick={onretake}
      disabled={!canRetake || (renderTier === 'proxy' ? proxyStatus : status) === 'none'}
      title={!canRetake
        ? retakeUnavailableNote
        : (renderTier === 'proxy' ? proxyStatus : status) === 'none'
          ? 'Render this shot first, then you can reroll it'
          : `A new seed for this shot. It ${retakeNote}`}
      >Retake this {renderTier === 'proxy' ? 'preview' : 'shot'}</button
    >
    <div class="spacer"></div>
    <button class="btn sm" onclick={onsnapshot} disabled={snapBusy || !imageEngineReady}
      >{snapBusy ? 'Drawing…' : 'Storyboard still'}</button
    >
    {#if perShotEstimate > 0}<span class="est">~{perShotEstimate.toFixed(0)}s</span>{/if}
    {#if renderTier === 'final'}
      <!-- On the proxy tier the primary button IS this, so the pair would be the same
           button twice. -->
      <button
        class="btn sm"
        onclick={onpreview}
        disabled={!clip?.prompt?.trim()}
        title={previewFullRes
          ? `Preview at ${proxyW}×${proxyH} — the SAME canvas and the same seed as the final render, just fewer steps, so the framing, the subject placement and the audio bed are the ones you will get. Under-converged, not smaller.`
          : `Cheap ${proxyW}×${proxyH} proxy preview (with audio) up to and including this shot — check the prompt / music reactivity / lipsync before the full render`}
        >Preview this shot</button
      >
    {/if}
    <button
      class="btn sm primary"
      onclick={onrender}
      disabled={!clip?.prompt?.trim()}
      title={renderTier === 'proxy'
        ? previewFullRes
          ? `Preview at ${proxyW}×${proxyH} — same canvas and seed as the final, fewer steps`
          : `Cheap ${proxyW}×${proxyH} preview (with audio) up to and including this shot`
        : !canRetake
          ? // Without a bank to reach into there is only ONE move here, whether or not the shot has
            // rendered: its whole span again. Saying "reroll" would name a cheaper thing than what
            // the button does, and the difference is every earlier shot's GPU time.
            `Renders this shot's whole span again — ${retakeUnavailableNote}`
          : status === 'none'
            ? 'Render up to and including this shot (resumes the banked prefix)'
            : 'Reroll this shot with a new seed'}
      >{renderTier === 'proxy'
        ? proxyStatus === 'none'
          ? 'Preview this shot'
          : 'Re-roll preview'
        : 'Render this shot'}</button
    >
  </div>
</div>
