<script lang="ts">
  import {
    DETAIL_STRENGTH_DEFAULT,
    LTX_PATCH,
    PREVIEW_TIERS,
    parseSigmas,
    snap32,
    type CustomStage,
    type ResPreset,
    type ResolutionPlan,
    type SizeTier
  } from '$lib/director/resolution';
  import {
    LORA_STRENGTH_MAX,
    LORA_STRENGTH_MIN,
    LORA_STRENGTH_STEP,
    hasLora,
    loraStrength,
    loraSummary,
    setLoraStrength,
    toggleLora
  } from '$lib/director/loras';
  import { maxShotSeconds, shotLengthError } from '$lib/ltx/frames';
  import {
    ENGINE_OPTIONS,
    BEATS_RELAY_NOTE,
    H3_FPS,
    UNAVAILABLE,
    capabilities,
    beatsAvailable,
    capitalise,
    checkCanvas,
    reachableMaxFrames,
    reframeForEngine,
    resumeAvailable,
    snappedSeconds,
    unavailableFor
  } from '$lib/director/engine';
  import { H3_MAX_SECONDS } from '$lib/director/h3-generations';
  import H3RenderKnobs from './H3RenderKnobs.svelte';
  import type { DirectorEngine } from '$lib/director/engine';
  import type {
    H3Props,
    LtxFrameDrop,
    LtxQualityProfile,
    LtxRuntimeLora,
    LtxUpscalerRatio,
    LtxVideoProps
  } from '$lib/api/client';

  // The project's render settings. Every field writes straight back into the editor's own state
  // through `bind:` — these values ARE the saved document. The derived readouts (`resPlan`, the
  // size tiers, the snap divisor) are computed by the editor instead, because its render guards
  // read them too.
  let {
    engine = $bindable(),
    beatsNodeMounted,
    shotSeconds = $bindable(),
    fps = $bindable(),
    model = $bindable(),
    loras = $bindable(),
    frameDrop = $bindable(),
    beatsEnabled = $bindable(),
    critiqueEnabled = $bindable(),
    critiqueAutoRetake = $bindable(),
    critiqueThreshold = $bindable(),
    critiqueMaxRetakes = $bindable(),
    upscalerRatio = $bindable(),
    detailStrength = $bindable(),
    resPreset = $bindable(),
    width = $bindable(),
    height = $bindable(),
    proxyW = $bindable(),
    proxyH = $bindable(),
    proxySteps = $bindable(),
    h3Preview = $bindable(),
    h3Loras = $bindable(),
    h3LoraMultipliers = $bindable(),
    h3Steps = $bindable(),
    cfgScale = $bindable(),
    seed = $bindable(),
    negativePrompt = $bindable(),
    customBaseSampler = $bindable(),
    customBaseSigmasText = $bindable(),
    customStages = $bindable(),
    props,
    h3Props,
    hasRenders,
    priorSourceShots,
    rendering,
    qualityProfile,
    snapNote,
    snapDivisor,
    sizeSel,
    sizeTiers,
    resPlan,
    previewSizeSel,
    customBaseSteps,
    frameDropNote,
    onchange,
    onclose,
    onprofile,
    onengine,
    onsize,
    onpreviewsize,
    onsnap,
    onaddstage,
    onremovestage
  }: {
    /** Which engine this PROJECT renders on. Everything below the picker is gated on it. */
    engine: DirectorEngine;
    /** `props.beats_enabled` — whether this deployment's comfyui has the relay node mounted.
     *  Passed raw rather than pre-resolved, so the capability half stays with `caps`. */
    beatsNodeMounted?: boolean;
    shotSeconds: number;
    fps: number;
    model: string | undefined;
    loras: LtxRuntimeLora[];
    frameDrop: LtxFrameDrop;
    beatsEnabled: boolean;
    critiqueEnabled: boolean;
    critiqueAutoRetake: boolean;
    critiqueThreshold: number;
    critiqueMaxRetakes: number;
    upscalerRatio: LtxUpscalerRatio;
    detailStrength: number;
    resPreset: ResPreset;
    width: number;
    height: number;
    proxyW: number;
    proxyH: number;
    proxySteps: number;
    /**
     * Ask the engine for in-flight preview frames while it denoises (H3 only — `caps.preview`).
     *
     * 🔴 **Default OFF, and the default is the decision.** `generate_preview` decodes latents
     * through the RGB-factor path on every emission, and that cost is UNMEASURED against steps that
     * already run 43–50 s at production shape. An unmeasured per-emission cost must not be levied
     * silently on a production render, so it is opt-in per project rather than on by default with
     * an escape hatch.
     */
    h3Preview: boolean;
    /**
     * The LoRAs and step count every H3 {@link EngineVocab.renderUnit} inherits (`caps.omniReferences`).
     *
     * 🔴 Bare engine filenames, NOT `LtxRuntimeLora`. H3's only identifier for an adapter is its
     * filename inside the shim's `minimax_h3/` LoRA directory — no path, no id, no metadata — so the
     * two engines' LoRA state cannot share a type, and `loras` above is the LTX one.
     *
     * 🔴 A DEFAULT. `compile.ts` writes these into any generation that named none of its own; a
     * generation that did keeps what it has. Nothing here is on the wire as a project field — the
     * H3 request has no project level at all.
     */
    h3Loras: string[];
    h3LoraMultipliers: number[];
    h3Steps: number | undefined;
    cfgScale: number | undefined;
    seed: number | undefined;
    negativePrompt: string;
    customBaseSampler: string;
    customBaseSigmasText: string;
    customStages: CustomStage[];
    props: LtxVideoProps | null;
    /** `GET /api/v1/minimax-h3/props`. `null` until fetched, or when H3 is unconfigured. */
    h3Props: H3Props | null;
    /** True once the project has rendered anything — switching engine then reframes rather than
     *  merely re-configures, so the picker says so. */
    hasRenders: boolean;
    /**
     * How many shots take their video from a prior render (`v2v_source_kind: 'prior_render'`).
     *
     * 🔴 The one thing a switch DESTROYS rather than merely invalidates. Those ids name a dir in
     * the engine being left, so the switch clears them — and a per-shot authoring choice that
     * cannot be got back by switching back is not something to drop behind a hint.
     */
    priorSourceShots: number;
    /**
     * A render of THIS project is in flight, and the picker is held shut until it is over.
     *
     * 🔴 Not a nicety. A render carries the engine it was dispatched to, and its completion banks
     * that engine's chain id back into the document — so a switch made while it runs is a switch
     * made underneath a write that has already been sent. The editor refuses that write
     * (`adoptCompletedBank`), and this is the other half: a control that can only produce a state
     * the editor then has to discard should not be reachable.
     */
    rendering: boolean;
    qualityProfile: LtxQualityProfile;
    snapNote: string | null;
    snapDivisor: number;
    sizeSel: string;
    sizeTiers: SizeTier[];
    resPlan: ResolutionPlan;
    previewSizeSel: string;
    customBaseSteps: number;
    frameDropNote: string;
    onchange: () => void;
    onclose: () => void;
    /** Switching profile re-sizes the output for the new factor, so the editor owns the move. */
    onprofile: (next: LtxQualityProfile) => void;
    /** Switching ENGINE invalidates the editor's banked resume targets — the editor owns those. */
    onengine: () => void;
    onsize: (key: string) => void;
    onpreviewsize: (key: string) => void;
    /** Re-snap the output target after anything that changes the profile's total upscale factor. */
    onsnap: () => void;
    onaddstage: () => void;
    onremovestage: (i: number) => void;
  } = $props();

  // Assumes a CONTINUATION shot (the stricter of the two cases) — it pays the seam overlap on top
  // of its visible length, so it hits the engine's temporal window first. A project made entirely
  // of scene cuts could go 16 frames longer, but warning early is the right way to be wrong here.
  const caps = $derived(capabilities(engine));
  /** The capability AND the mounted node — see `beatsAvailable`. */
  const canBeats = $derived(beatsAvailable(engine, beatsNodeMounted));
  /** What this engine calls its units — see `EngineVocab`. Display only. */
  const v = $derived(caps.vocab);
  // The LTX temporal-window guard is an LTX fact (a continuation shot pays the seam overlap on top
  // of its visible length). H3 shots share no state and pay no overlap, so the same number would be
  // a warning about a limit that does not exist here.
  const shotLengthWarning = $derived(
    caps.chainedShots ? shotLengthError(shotSeconds, fps || 24, false) : null
  );
  /**
   * What a shot will ACTUALLY be. H3's frame grid is 17 frames coarse (~0.7 s at 24 fps) and snaps
   * UP, so a shot typed as 5 s renders as 5.17 s. Showing the authored number alone would leave
   * every duration in the timeline quietly wrong, and the drift compounds along the cut.
   */
  const snappedShot = $derived(snappedSeconds(engine, shotSeconds, fps || 24));
  const shotSnapNote = $derived(
    Math.abs(snappedShot - shotSeconds) > 0.005
      ? `Renders as ${snappedShot.toFixed(2)}s — ${caps.label} rounds each ${v.renderUnit} up to its ${caps.framePeriod}n+${caps.frameBase} frame grid.`
      : null
  );
  /**
   * The two canvas rules, kept apart because they are different in kind and the old single note
   * conflated them under *"The render will be refused."*
   *
   * - `.error` — off the ×32 grid. **Model law**, and it really is refused. Reported rather than
   *   snapped: 720 and 1080 are the numbers people type and BOTH are illegal on H3, so the
   *   message has to explain and offer the neighbours.
   * - `.warning` — over the area 1344×768 was measured at. **Advisory.** `diffusers` calls that
   *   figure a *"soft area cap"* and no config in the weights tree carries a max resolution; it
   *   was a 400 until 2026-08-07, and the price was that true 16:9 at 768p (1376×768, 2.4% over)
   *   could not be rendered at all.
   */
  const canvasCheck = $derived(checkCanvas(engine, width, height));

  // ── H3 project LoRAs ────────────────────────────────────────────────────────────────────────
  //
  // 🔴 The rules, the turbo shortcut and the four-rule name validator all live in `H3RenderKnobs`
  // now, and this sheet renders that component at `scope="project"`. They used to be written out
  // here AND in `H3PromptComposer`, ~110 lines apiece, and the two copies of `loraProblem` had
  // already drifted: this one never mentioned the nested-subdirectory case, so the project sheet
  // said "not on the engine" about a file that was visibly on the engine.
  //
  // Only the listing is still read here, for the summary line and to pass down.
  const h3LoraListing = $derived(h3Props?.loras ?? null);

  /** H3 has no upscale ladder, so its sizes are just sizes. `null` on LTX — see `resolution.ts`. */
  const h3Presets = $derived(caps.canvasPresets);
  /** The preset currently matched, or `custom`. Latched so an explicit Custom choice sticks. */
  let h3SizeCustom = $state(false);
  const h3SizeSel = $derived(
    h3SizeCustom
      ? 'custom'
      : (h3Presets?.find((p) => p.w === width && p.h === height)?.label ?? 'custom')
  );
  const engineUnavailable = $derived.by(() => {
    if (engine === 'h3' && h3Props && !h3Props.configured) {
      return 'The MiniMax-H3 service URL is not set — add it under Settings → GPU services before rendering this project.';
    }
    // 🔴 `props` here is the bootstrap for the SELECTED engine (the editor swaps it per engine),
    // so this reads LTX-2.5's health and not 2.3's. It is a warning rather than a block: the
    // service can be down for a moment, and the render will say so properly if it still is.
    if (engine === 'ltx25' && props && !props.healthy) {
      return 'The LTX-2.5 service is not answering — set LTX25_SERVICE_URL (or the ltx25_server_url setting) and check the comfy-api container before rendering this project.';
    }
    return null;
  });

  /**
   * The blurb promises resume and single-shot retake; this says whether THIS box can honour it.
   *
   * 🔴 A capability record cannot answer it. The bank LTX-2.5 resumes from is a directory an
   * operator configures, and without one the same build answers 501 to both — so said at the picker
   * the answer costs nothing, while learned at the render it costs the queue wait. Silent whenever
   * the engine has the bank, and whenever it never claimed one.
   */
  const bankNote = $derived(
    caps.resume && !resumeAvailable(engine, props?.bank_enabled)
      ? unavailableFor(engine, 'resume')
      : null
  );

  /**
   * Why there are no beats, where there is something to say.
   *
   * The mirror of `bankNote`: the capability is real and the deployment is what is missing, so the
   * absence is surprising and worth one sentence. Silent when the node is mounted, and on an engine
   * that never claimed Prompt Relay.
   */
  const beatsNote = $derived(caps.beats && !canBeats ? unavailableFor(engine, 'beats') : null);

  /**
   * Why there is no quality profile, where there is something to say.
   *
   * The whole Output & quality ladder is one control on `upscaleChain`, and on LTX-2.5 the absence
   * is surprising: the plugin HAS a two-stage template, it just declines it for any shot carrying
   * an opening frame, keyframes or character references — which is nearly every Director shot.
   * Empty (and so silent) on H3, whose sizes are simply sizes and where the row never existed.
   */
  const upscaleNote = $derived(caps.upscaleChain ? null : unavailableFor(engine, 'upscaleChain'));

  /**
   * The longest a single shot may be.
   *
   * 🔴 `maxShotSeconds` is LTX's temporal-window arithmetic and answered with LTX's 241-frame
   * ceiling on every engine — so the H3 shot-length box was capped by a limit belonging to a model
   * the project is not rendering on. H3's own ceiling is the generation budget: no single shot can
   * outlast the generation that has to contain it.
   */
  const maxShot = $derived(caps.maxFrames ? H3_MAX_SECONDS : maxShotSeconds(fps || 24, false));

  /**
   * Two different ceilings, and both are worth showing.
   *
   * The spec cap never changes; what THIS box will currently run is a function of VRAM, the loaded
   * checkpoint and whatever reachability work has landed, and the shim answers it at
   * `/h3/v1/capabilities`. Showing only the spec cap invites a 15s render on a box that tops out
   * lower; showing only the reachable one hides that the timeline is being constrained by this
   * machine rather than by the model.
   */
  const reachable = $derived(reachableMaxFrames(h3Props));
  const frameCeilingNote = $derived.by(() => {
    if (!caps.maxFrames) return null;
    const spec = `${caps.maxFrames} frames (${(caps.maxFrames / H3_FPS).toFixed(2)}s)`;
    if (reachable === null) {
      return `A ${v.renderUnit} renders up to ${spec}. This box has not reported what it can currently reach — ask the engine (Settings → GPU services) if a long ${v.renderUnit} fails.`;
    }
    if (reachable >= caps.maxFrames) {
      return `A ${v.renderUnit} renders up to ${spec}, and this box currently reaches all of it.`;
    }
    return `The model allows ${spec}, but this box currently reaches only ${reachable} frames (${(reachable / H3_FPS).toFixed(2)}s) — longer ${v.renderUnitPlural} will be refused here.`;
  });
</script>

<div class="overlay" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="sheet">
    <div class="sheet-h">
      <h3>Settings</h3>
      <button class="btn icon-only ghost" onclick={onclose}>✕</button>
    </div>
    <div class="sheet-body">
      <div class="group-label">Engine</div>
      <label class="field"
        ><span>Render with</span>
        <select
          class="inp"
          value={engine}
          disabled={rendering}
          onchange={(e) => {
            const next = e.currentTarget.value as DirectorEngine;
            // 🔴 The one part of the switch that is a DELETION: a shot sourced from a prior render
            // loses that source, because the id names a dir in the engine being left. Everything
            // else here is invalidation — the takes stay on disk and the chain comes back on the
            // way back — so this is the only part that has to be chosen rather than announced.
            // Put back on the select rather than reassigned, since `engine` never moved.
            if (
              priorSourceShots > 0 &&
              !confirm(
                // ⚠️ "points at", not "takes its video from": on an engine whose
                // `v2vPriorRenderSource` is false the pointer is kept but unreadable, so the shot
                // is not sourced from it today — and the switch still destroys the pointer.
                `${priorSourceShots} ${priorSourceShots === 1 ? 'shot points' : 'shots point'} at a prior render as ${priorSourceShots === 1 ? 'its' : 'their'} source. Switching to ${capabilities(next).label} clears that pointer — ${priorSourceShots === 1 ? 'it' : 'they'} will need a new source. Switch anyway?`
              )
            ) {
              e.currentTarget.value = engine;
              return;
            }
            engine = next;
            // 🔴 An LTX project's canvas is not an H3 canvas — see `reframeForEngine`, which the
            // undo path calls too. On the change event rather than in an effect: it must happen
            // exactly once per switch and must not fight the user afterwards.
            const home = reframeForEngine(next, width, height);
            if (home) {
              width = home.w;
              height = home.h;
              h3SizeCustom = false;
            }
            // Same reason this reframe is here: the editor's banked chain ids belong to the engine
            // that minted them, and exactly one switch must drop them.
            onengine();
          }}
        >
          {#each ENGINE_OPTIONS as o (o.value)}<option value={o.value}>{o.label}</option>{/each}
        </select>
        <p class="hint" style="margin-top:4px">{caps.blurb}</p>
      </label>
      {#if rendering}
        <p class="hint" style="margin:4px 0 0">
          Held while this project renders — the render is already on its way to {caps.label}, and
          its result is banked in that engine's store.
        </p>
      {/if}
      {#if engineUnavailable}
        <p class="hint err" style="margin:4px 0 0">{engineUnavailable}</p>
      {/if}
      {#if bankNote}
        <p class="hint" style="margin:4px 0 0">{bankNote}</p>
      {/if}
      <!-- Switching engine is not a re-configure once anything has rendered: the two engines take
           different conditioning, so the existing takes were made under rules the new engine does
           not have. The FILES are safe — the takes stay on disk, keep their own engine (`0039`) and
           come back if you switch back — but everything that NAMES the old engine's job store goes:
           the banked chain both tiers resume from, and any per-shot prior-render source. Say which
           is which, because "keeps them" read as a promise about all of it. -->
      {#if hasRenders}
        <p class="hint" style="margin:4px 0 0">
          This project already has renders. The takes stay on disk and come back if you switch back,
          but the banked chain does not travel: the next render starts from the first shot, and new
          shots will be conditioned differently — expect the look to move.
        </p>
      {/if}
      {#if priorSourceShots > 0}
        <p class="hint" style="margin:4px 0 0">
          {priorSourceShots}
          {priorSourceShots === 1 ? 'shot takes its video' : 'shots take their video'} from a prior render
          on {caps.label}. Switching clears that source — {priorSourceShots === 1 ? 'it' : 'they'} will
          need a new one.
        </p>
      {/if}

      <div class="group-label">Timing</div>
      <div class="rowgrid">
        <label class="field"
          ><span>{capitalise(v.subUnit ?? v.renderUnit)} length (s)</span><input
            class="inp"
            type="number"
            min="1"
            max={maxShot}
            step="0.5"
            bind:value={shotSeconds}
            oninput={onchange}
          /></label
        >
        {#if caps.variableFps}
          <label class="field"
            ><span>FPS</span><input
              class="inp"
              type="number"
              min="8"
              max="30"
              bind:value={fps}
              oninput={onchange}
            /></label
          >
        {:else}
          <label class="field"
            ><span>FPS</span><input class="inp" type="number" value="24" disabled />
            <p class="hint" style="margin-top:4px">{UNAVAILABLE.variableFps}</p></label
          >
        {/if}
      </div>
      {#if caps.multiShotPerGeneration}
        <p class="hint" style="margin:4px 0 0">
          A <b>{v.renderUnit}</b> here is up to {H3_MAX_SECONDS.toFixed(2)}s and its prompt can name
          several <b>{v.subUnitPlural}</b> — timed moments the model renders the transitions between
          itself, so a cut inside one {v.renderUnit} is free while a cut between two is a seam. It is
          also the delivery unit and the retake unit.
        </p>
        {#if frameCeilingNote}
          <p class="hint" style="margin:4px 0 0">{frameCeilingNote}</p>
        {/if}
      {:else if shotSnapNote}
        <p class="hint" style="margin:4px 0 0">{shotSnapNote}</p>
      {/if}
      <!-- The server REFUSES a shot over the engine's temporal window rather than letting the DiT
           silently tile it, so say so here — before the user spends minutes of GPU time on a
           request that will come back as a failed job. -->
      {#if shotLengthWarning}
        <p class="hint err" style="margin:4px 0 0">{shotLengthWarning}</p>
      {/if}
      {#if caps.modelVariants && (props?.variants ?? []).length > 1}
        <label class="field"
          ><span>Model</span>
          <select class="inp" bind:value={model} {onchange}
            >{#each props?.variants ?? [] as variant (variant)}<option value={variant}
                >{variant}</option
              >{/each}</select
          >
        </label>
      {/if}
      <!-- `loraIdentity === 'path'`, not `caps.loras`: H3 takes LoRAs too, but names them by bare
           filename with no catalogue behind them, and `props` (LtxVideoProps) is fetched on an H3
           project as well — so `caps.loras` alone would render this checklist there and write
           `{path, multiplier}` objects into a field the shim validates as strings. -->
      {#if caps.loraIdentity === 'path' && (props?.loras ?? []).length}
        <details class="field">
          <summary
            >Runtime LoRAs ({loras.length
              ? loraSummary(loras, true, props?.loras)
              : 'none'})</summary
          >
          <p class="hint" style="margin:4px 0 8px">
            Applied on top of the model, at whatever strength you set — the tunable counterpart to a
            folded variant. Don't add the adapter a variant already bakes in; it would be applied
            twice. Every shot inherits this set unless its inspector names its own. Ticking one adds
            it at ITS OWN trained strength, which is not always 1.00.
          </p>
          {#each props?.loras ?? [] as l (l.path)}
            {@const on = hasLora(loras, l.path)}
            <div class="inline-row" style="gap:8px;margin-bottom:4px">
              <label class="inline-row" style="gap:6px;flex:1;min-width:0">
                <input
                  type="checkbox"
                  checked={on}
                  onchange={() => {
                    loras = toggleLora(loras, l.path, props?.loras);
                    onchange();
                  }}
                />
                <span class="tl-meta" style="overflow-wrap:anywhere"
                  ><b>{l.label}</b>{#if l.path !== l.label}
                    · {l.path}{/if}</span
                >
              </label>
              {#if on}
                <input
                  class="inp"
                  style="width:5.5rem;flex:none"
                  type="number"
                  min={LORA_STRENGTH_MIN}
                  max={LORA_STRENGTH_MAX}
                  step={LORA_STRENGTH_STEP}
                  value={loraStrength(loras, l.path)}
                  oninput={(e) => {
                    loras = setLoraStrength(loras, l.path, Number(e.currentTarget.value));
                    onchange();
                  }}
                />
              {/if}
            </div>
            {#if on && (l.blurb || l.trigger)}
              <p class="hint" style="margin:-2px 0 8px 22px">
                {l.blurb ?? ''}{#if l.trigger}
                  Adds <code>{l.trigger}</code> to the prompt of every {v.renderUnit} this is on{l.trigger_required
                    ? ''
                    : ' (it also works without)'}.{/if}
              </p>
            {/if}
          {/each}
        </details>
      {/if}
      <!-- ── H3: the project's LoRAs and step count ─────────────────────────────────────────────
           🔴 A SEPARATE block from the LTX one above rather than the same one un-gated, because the
           two engines do not share an identifier for an adapter. LTX names a path and carries a
           per-adapter label, blurb and trigger word from `loras.json`; H3's wire name is a BARE
           FILENAME in the engine's `minimax_h3/` directory and there is nothing else — no id, no
           hash, no metadata. Rendering H3's names through the LTX picker would invent labels that do
           not exist and, worse, write `{path, multiplier}` objects into a field the shim validates
           as a list of strings.

           🔴 These two knobs are ONE decision and are drawn together for that reason. The turbo LoRA
           does not lower `steps` and is not free — it costs ~5%/step to carry — so loading it and
           leaving the count at 20 buys nothing at all. 8 steps WITH it against 20 without is −39%
           wall at the same 6528 MiB peak (measured 2026-08-07); that is the whole feature, and a UI
           that let you set one without seeing the other is how you collect the tax and not the win. -->
      {#if caps.loraIdentity === 'filename'}
        <details class="field" open={h3Loras.length > 0 || h3Steps !== undefined}>
          <summary
            >{capitalise(v.renderUnitPlural)} render with ({h3Loras.length
              ? `${h3Loras.length} LoRA${h3Loras.length === 1 ? '' : 's'}`
              : 'no LoRAs'}, {h3Steps !== undefined
              ? `${h3Steps} steps`
              : "the engine's own step count"})</summary
          >
          <p class="hint" style="margin:4px 0 8px">
            The default for every {v.renderUnit} in this project. A {v.renderUnit} that names its own
            LoRAs on its {v.renderUnit} details tab keeps them — this fills in the ones that have not.
          </p>
          <!-- 🔴 The SAME component the per-shot override uses, at `scope="project"`. It was ~110
               lines here and ~110 in `H3PromptComposer`, with the four-rule `loraProblem` validator
               written out twice — and the two wordings had already drifted on the nested-directory
               case. `scope` is what decides whether an empty field means "the engine's default" or
               "inherit"; no canvas is passed, because the project's canvas belongs with the other
               size settings below rather than duplicated here. -->
          <H3RenderKnobs
            scope="project"
            vocab={v}
            loras={h3Loras}
            multipliers={h3LoraMultipliers}
            steps={h3Steps}
            catalogue={h3LoraListing}
            onloras={(names, mults) => {
              h3Loras = names;
              h3LoraMultipliers = mults;
              onchange();
            }}
            onsteps={(n) => {
              h3Steps = n;
              onchange();
            }}
          />
        </details>
      {/if}
      {#if caps.characterRefs}
        <label class="field">
          <span>Drop the reference out of the first frame</span>
          <select
            class="inp"
            value={frameDrop === 'off' ? 'off' : 'auto'}
            onchange={(e) => {
              frameDrop = e.currentTarget.value === 'off' ? 'off' : 'auto';
              onchange();
            }}
          >
            <option value="auto">Auto (engine decides, per {v.renderUnit})</option>
            <option value="off">Off (never trim)</option>
          </select>
          <p class="hint" style="margin-top:4px">
            {frameDropNote} Any shot can override this in its own inspector.
          </p>
        </label>
      {/if}

      {#if beatsNote}
        <div class="group-label">Beats</div>
        <p class="hint">{beatsNote}</p>
      {/if}
      {#if canBeats}
        <div class="group-label">Beats</div>
        <label class="field">
          <span>
            <input type="checkbox" bind:checked={beatsEnabled} {onchange} />
            Let the LLM place timed beats inside shots
          </span>
          <p class="hint" style="margin-top:4px">
            A beat pins one action to one moment inside a shot instead of letting the model choose
            when it happens. Turn this off for plain single-action shots, or when the planner is
            being beat-happy — it will then split a two-moment shot into two shots instead. Beats
            you place by hand are never touched either way.
          </p>
          <p class="hint" style="margin-top:4px">{BEATS_RELAY_NOTE}</p>
        </label>
      {/if}

      <div class="group-label">Dailies review</div>
      <label class="field">
        <span>
          <input type="checkbox" bind:checked={critiqueEnabled} {onchange} />
          Review the footage after a full render
        </span>
        <p class="hint" style="margin-top:4px">
          The vision model watches the rendered frames — not the prompt — and scores each shot
          against what it was asked for, per beat where a shot has them. Verdicts show up on each
          shot's takes.
        </p>
      </label>
      {#if critiqueEnabled}
        <label class="field">
          <span>
            <input type="checkbox" bind:checked={critiqueAutoRetake} {onchange} />
            Re-roll shots that fail
          </span>
          <p class="hint" style="margin-top:4px">
            Each re-roll is a NEW take, so nothing is overwritten and every option is still there in
            the morning. Each shot ends up on its best-scoring take; you can pick a different one
            any time.
          </p>
        </label>
        <div class="rowgrid">
          <label class="field"
            ><span>Fail below</span><input
              class="inp"
              type="number"
              min="0"
              max="1"
              step="0.05"
              bind:value={critiqueThreshold}
              oninput={onchange}
            /></label
          >
          <label class="field"
            ><span>Extra takes / {v.renderUnit}</span><input
              class="inp"
              type="number"
              min="1"
              max="5"
              bind:value={critiqueMaxRetakes}
              oninput={onchange}
              disabled={!critiqueAutoRetake}
            /></label
          >
        </div>
      {/if}

      <div class="group-label">Output &amp; quality</div>
      {#if caps.upscaleChain}
        <label class="field"
          ><span>Quality profile</span>
          <select
            class="inp"
            value={qualityProfile}
            onchange={(e) => onprofile(e.currentTarget.value as LtxQualityProfile)}
          >
            <option value="standard">Standard — 6-step base + ×2 refine (fastest)</option>
            <option value="high">High — 8-step base + ×2 3-step refine</option>
            <option value="upscale_4x">4× Upscale — quarter base + two ×2 refines</option>
            <option value="custom">Custom — dial base + upscale stages</option>
          </select>
        </label>
      {:else if upscaleNote}
        <p class="hint">{upscaleNote}</p>
      {/if}
      {#if h3Presets}
        <!-- 🔴 H3's own sizes, NOT the LTX upscale ladder. The LTX tiers are 1280×720 / 1920×1080 /
             2560×1440 snapped through a quality profile's divisor — none of that exists here (no
             upscale chain), and two of those three are not even legal H3 canvases. These are
             MiniMax's documented aspects, all on the ×32 grid.
             ⚠️ 1344×768 is 7:4 (1.75), NOT 16:9 (1.7778): it is what an area cap does to a true
             16:9 canvas at a 768 short edge. Both are offered and both are labelled honestly. -->
        <label class="field"
          ><span>Output size</span>
          <select
            class="inp"
            value={h3SizeSel}
            onchange={(e) => {
              const v = e.currentTarget.value;
              if (v === 'custom') {
                h3SizeCustom = true;
              } else {
                const p = h3Presets?.find((x) => x.label === v);
                if (p) {
                  h3SizeCustom = false;
                  width = p.w;
                  height = p.h;
                }
              }
              onchange();
            }}
          >
            {#each h3Presets as p (p.label)}<option value={p.label}>{p.label}</option>{/each}
            <option value="custom">Custom size…</option>
          </select>
          <p class="hint" style="margin-top:4px">
            Every size here is legal and inside the budget 1344×768 was measured at, except 1376×768
            which is 2.4% over — that is advisory, not a limit.
          </p>
        </label>
      {:else}
        <label class="field"
          ><span>Output size</span>
          <select class="inp" value={sizeSel} onchange={(e) => onsize(e.currentTarget.value)}>
            <!-- The base pass is only worth naming where there is a refine to put it back. On an
                 engine with no upscale chain the tier IS the render, and printing "(base 1280x736)"
                 beside 1280x736 advertises a pass that does not exist. -->
            {#each sizeTiers as t (t.key)}<option value={t.key}
                >{t.label} — {t.w}×{t.h}{caps.upscaleChain ? ` (base ${t.bw}×${t.bh})` : ''}</option
              >{/each}
            <option value="custom">Custom size…</option>
          </select>
        </label>
      {/if}
      <!-- 🔴 `caps.upscaleChain` as well as the profile. This read `qualityProfile === 'standard'`
           alone, and the profile PICKER above is hidden on H3 — so an H3 project sat at the LTX
           default of `standard` and the upscaler select rendered on an engine with no upscale chain
           at all. Nothing downstream read it (`compileH3Timeline` emits no `upscaler_ratio`), which
           is the worse half: the control was inert, so setting it looked like it did something and
           produced a byte-identical request. Every other rung of this ladder was already gated on
           the capability; this one was the row that fell through. -->
      {#if caps.upscaleChain && qualityProfile === 'standard'}
        <label class="field"
          ><span>Standard upscaler</span>
          <select
            class="inp"
            bind:value={upscalerRatio}
            onchange={() => {
              resPreset = 'custom';
              onsnap();
            }}
          >
            <option value="none">None (native)</option>
            <option value="x1.5">×1.5</option>
            <option value="x2">×2</option>
          </select>
        </label>
      {/if}
      {#if caps.upscaleChain && (qualityProfile === 'high' || qualityProfile === 'upscale_4x')}
        <label class="field"
          ><span>Detail strength · <b>{detailStrength.toFixed(2)}</b></span>
          <input
            type="range"
            class="slider"
            min="0.5"
            max="1"
            step="0.05"
            bind:value={detailStrength}
            oninput={onchange}
          />
          <p class="hint" style="margin-top:4px">
            Renoise at the final {qualityProfile === 'upscale_4x' ? '4×' : '×2'} refine. Higher = sharper,
            more new detail (can drift); lower = truer to the base. Default {DETAIL_STRENGTH_DEFAULT}.
          </p>
        </label>
      {/if}
      {#if (h3Presets ? h3SizeSel : sizeSel) === 'custom'}
        <!-- The divisor is the ladder's on an engine with an upscale chain (the base pass has to
             divide back out cleanly), and the engine's own dimension grid on one without. Reading
             `snapDivisor` unconditionally quoted an LTX ladder factor at a project that has no
             ladder. -->
        {@const divisor = caps.upscaleChain ? snapDivisor : caps.dimMultiple}
        <div class="rowgrid">
          <!-- 🔴 `onsnap` is the LTX UPSCALE LADDER re-snap (`snapTargetToProfile`) and must not
               fire on an engine that has no ladder — it would quote and enforce a divisor derived
               from a quality profile this project does not have. On H3 nothing snaps at all: an
               off-grid canvas is REPORTED below, with the offending side named and the legal
               neighbours offered, because 720 and 1080 are the numbers people type and both are
               illegal here. -->
          <label class="field"
            ><span>Width (×{divisor})</span><input
              class="inp"
              type="number"
              min={divisor}
              step={divisor}
              bind:value={width}
              oninput={() => {
                resPreset = 'custom';
                onchange();
              }}
              onchange={caps.upscaleChain ? onsnap : onchange}
            /></label
          >
          <label class="field"
            ><span>Height (×{divisor})</span><input
              class="inp"
              type="number"
              min={divisor}
              step={divisor}
              bind:value={height}
              oninput={() => {
                resPreset = 'custom';
                onchange();
              }}
              onchange={caps.upscaleChain ? onsnap : onchange}
            /></label
          >
        </div>
      {/if}
      {#if snapNote && caps.upscaleChain}<p class="hint" style="color:var(--warning)">
          ⤷ {snapNote}
        </p>{/if}
      <!-- Two rules, and they are NOT the same kind of thing — saying "the render will be refused"
           for both is what made the area figure look like a law. The ×32 grid IS a law and is
           refused; the area figure is guidance about VRAM and renders. -->
      {#if canvasCheck.error}
        <p class="hint err" style="margin:4px 0 0">
          {canvasCheck.error}
          {#if canvasCheck.suggest}
            {@const fix = canvasCheck.suggest}
            <button
              type="button"
              class="btn sm ghost"
              style="margin-left:6px"
              onclick={() => {
                width = fix.w;
                height = fix.h;
                onchange();
              }}>Use {fix.w}×{fix.h}</button
            >
          {/if}
        </p>
      {:else if canvasCheck.warning}
        <p class="hint" style="color:var(--warning);margin:4px 0 0">⚠ {canvasCheck.warning}</p>
      {/if}
      <!-- The "Schedules" pair (video / audio flow shift) used to live here. Both are gone from the
           wire contract entirely — the flow shift is fixed at 12.0 on the WanGP backend — and they
           were broken anyway: written into the compiled request, never read back by
           `readTimelineDoc`, so reopening a project silently reset them while the sheet went on
           showing whatever had last been typed. Deleted rather than round-tripped: there is no
           longer a field to round-trip to. -->
      <!-- 🔴 The ladder readout is LTX-only: it names a base pass and each ×N refine stage, and on
           an engine with no upscale chain every rung is the same number, which reads as noise. H3
           gets its one canvas and the pixel count that actually predicts VRAM. -->
      {#if caps.upscaleChain}
        <div class="res-readout">
          Output <span class="out">{resPlan.targetW}×{resPlan.targetH}</span>
          — renders {#each resPlan.ladder as rung, i (i)}{#if i > 0}
              →
            {/if}<b>{i === 0 ? 'base ' : ''}{rung.w}×{rung.h}</b
            >{/each}.{#if resPlan.targetW * resPlan.targetH > 1920 * 1088}<span
              style="color:var(--warning)"
            >
              · large, OOM risk</span
            >{/if}
        </div>
      {:else}
        <div class="res-readout">
          Output <span class="out">{width}×{height}</span>
          — one pass, {(width * height).toLocaleString()} pixels.
        </div>
      {/if}

      <div class="group-label">Preview render</div>
      {#if caps.previewIsFullRes}
        <!-- 🔴 No size control here, and its absence is the design rather than a hidden knob. The
             initial noise is `randn(latent_shape, manual_seed(seed))`, so a smaller canvas is a
             different SHAPE and therefore a different DRAW — at the same seed you get fresh
             composition, a fresh camera path and (because the audio noise comes off the same
             generator right after) a different music bed. A cheap preview that predicts nothing
             about the render it precedes is worth less than no preview, so the only lever offered
             is the one that keeps the noise byte-identical: STEPS. -->
        <p class="hint" style="margin-top:-2px">
          The "Preview this shot" stream renders the <b>same canvas at the same seed</b> as the Output
          render above, with fewer steps. So the framing, the subject placement and the audio bed are
          the ones you will get — under-converged, not smaller. Roughly twice as fast, and it uses the
          same VRAM, which is the real ceiling.
        </p>
      {:else}
        <p class="hint" style="margin-top:-2px">
          The "Preview this shot" stream: a cheap, single-pass render (no upscale) that KEEPS audio
          + LoRAs, so you can judge the prompt / music reactivity / lipsync before committing to the
          full Output render above.
        </p>
        <label class="field"
          ><span>Preview size</span>
          <select
            class="inp"
            value={previewSizeSel}
            onchange={(e) => onpreviewsize(e.currentTarget.value)}
          >
            {#each PREVIEW_TIERS as t (t.key)}<option value={t.key}>{t.label} — {t.w}×{t.h}</option
              >{/each}
            <option value="custom">Custom size…</option>
          </select>
        </label>
        {#if previewSizeSel === 'custom'}
          <!-- ×LTX_PATCH, NOT the output profile's snapDivisor: the proxy is single-pass, so these
               dims are its base pass and there is no upscale factor to divide back out. -->
          <div class="rowgrid">
            <label class="field"
              ><span>Width (×{LTX_PATCH})</span><input
                class="inp"
                type="number"
                min={LTX_PATCH}
                step={LTX_PATCH}
                bind:value={proxyW}
                oninput={onchange}
              /></label
            >
            <label class="field"
              ><span>Height (×{LTX_PATCH})</span><input
                class="inp"
                type="number"
                min={LTX_PATCH}
                step={LTX_PATCH}
                bind:value={proxyH}
                oninput={onchange}
              /></label
            >
          </div>
        {/if}
      {/if}
      <label class="field"
        ><span>Preview steps</span><input
          class="inp"
          type="number"
          min="1"
          max="30"
          step="1"
          bind:value={proxySteps}
          oninput={onchange}
        /></label
      >
      <p class="hint" style="margin-top:-2px">
        {#if caps.previewIsFullRes}
          More steps = closer to the committed render, fewer = sooner. Renders at {resPlan.targetW}×{resPlan.targetH}
          · {proxySteps} steps.
        {:else}
          Bigger / more steps = slower but a truer preview — bump the size if faces are too small to
          judge lipsync. Renders at {snap32(proxyW)}×{snap32(proxyH)} · {proxySteps} steps.
        {/if}
      </p>

      {#if caps.preview}
        <!-- A different thing from the "Preview render" tier above, which is a whole second render.
             This is the engine emitting a frame DURING the render you are already waiting on. -->
        <div class="group-label">Live preview frames</div>
        <label class="inline-row" style="gap:8px;margin:0 0 4px">
          <input
            type="checkbox"
            bind:checked={h3Preview}
            {onchange}
            style="accent-color:var(--accent)"
          />
          <span>Emit a preview frame while rendering</span>
        </label>
        <!-- 🔴 Why this is OFF by default. `generate_preview` decodes latents through the
             RGB-factor path on EVERY emission, and that cost has never been measured against
             steps that already take 43-50 s at production shape. An unmeasured per-emission cost
             does not get levied silently on an ~830 s-per-generation production render — so it is
             asked for, per project, by someone who has decided the picture is worth the time. -->
        <p class="hint" style="margin-top:-2px">
          Shows the picture as it converges, in the stage, instead of a spinner for the ~14 minutes
          a {v.renderUnit} takes. <b>Off by default on purpose:</b> each emission decodes latents through
          the RGB-factor path and that cost has not been measured — so it is not levied on a final render
          unless you ask for it. Turning it off mid-render does nothing; it is fixed at dispatch.
        </p>
      {/if}

      <div class="group-label">Advanced recipe</div>
      {#if caps.upscaleChain && (qualityProfile === 'high' || qualityProfile === 'upscale_4x')}
        <p class="hint">
          This profile bakes its own recipe — base steps + CFG are baked, so those knobs aren't
          shown. Override the negative prompt below.
        </p>
      {/if}
      <div class="rowgrid">
        {#if caps.cfg && (qualityProfile === 'standard' || qualityProfile === 'custom')}
          <!-- No Steps field: the base step count is set by the sigma schedule (Standard's fixed
               6-value list; Custom's editable base-sigmas box), not a separate steps number — a
               steps override is ignored by the engine, so exposing it only misled. -->
          <label class="field"
            ><span>CFG scale</span><input
              class="inp"
              type="number"
              min="1"
              max="12"
              step="0.1"
              placeholder="1.0"
              bind:value={cfgScale}
              oninput={onchange}
            /></label
          >
        {/if}
        <label class="field"
          ><span>Seed</span><input
            class="inp"
            type="number"
            step="1"
            placeholder="random"
            bind:value={seed}
            oninput={onchange}
          /></label
        >
      </div>
      {#if caps.negativePrompt}
        <label class="field"
          ><span>Negative prompt</span><input
            class="inp"
            placeholder="(none)"
            bind:value={negativePrompt}
            oninput={onchange}
          /></label
        >
      {:else}
        <!-- Explained rather than merely absent: "where did my negative prompt go" is the first
             thing anyone asks after switching engine, and the answer is a property of the model. -->
        <p class="hint">{unavailableFor(engine, 'negativePrompt')}</p>
      {/if}
      {#if caps.customRecipe && qualityProfile === 'custom'}
        <label class="field"
          ><span>Base sampler</span>
          <select class="inp" bind:value={customBaseSampler} {onchange}>
            <option value="lcm">lcm</option>
            <option value="euler">euler</option>
            <option value="euler_cfg_pp">euler_cfg_pp</option>
            <option value="euler_a_cfg_pp">euler_a_cfg_pp</option>
            <option value="euler_a">euler_a</option>
            <option value="dpm++2m_sde">dpm++2m_sde</option>
            <option value="dpm++2m_sde_bt">dpm++2m_sde_bt</option>
            <option value="res_multistep">res_multistep</option>
            <option value="er_sde">er_sde</option>
          </select>
        </label>
        <label class="field"
          ><span>Base sigmas · <b>{customBaseSteps}-step base</b></span><input
            class="inp"
            bind:value={customBaseSigmasText}
            oninput={onchange}
          />
          <p class="hint" style="margin-top:4px">
            Comma-separated, high→0. The count sets the base steps ({parseSigmas(
              customBaseSigmasText
            ).length} sigmas − 1 = {customBaseSteps}) — this drives step count, not a Steps field.
            Default is an 8-step schedule.
          </p></label
        >
        <div class="lbl-row" style="margin:6px 0 9px">
          <span>Upscale stages</span><button class="miniai" onclick={onaddstage}>Add stage</button>
        </div>
        {#each customStages as stage, i (i)}
          <div class="stage-cfg">
            <div class="rowgrid">
              <label class="field" style="margin:0 0 6px"
                ><span>Upscaler</span><select
                  class="inp"
                  bind:value={stage.upscaler}
                  onchange={onsnap}
                  ><option value="x2">×2</option><option value="x1.5">×1.5</option></select
                ></label
              >
              <label class="field" style="margin:0 0 6px"
                ><span>Sampler</span><select class="inp" bind:value={stage.sample_method} {onchange}
                  ><option value="euler_cfg_pp">euler_cfg_pp</option><option value="euler"
                    >euler</option
                  ><option value="euler_a_cfg_pp">euler_a_cfg_pp</option><option value="euler_a"
                    >euler_a</option
                  ><option value="dpm++2m_sde">dpm++2m_sde</option><option value="dpm++2m_sde_bt"
                    >dpm++2m_sde_bt</option
                  ><option value="res_multistep">res_multistep</option><option value="er_sde"
                    >er_sde</option
                  ></select
                ></label
              >
            </div>
            <div class="rowgrid">
              <label class="field" style="margin:0 0 6px"
                ><span>Steps</span><input
                  class="inp"
                  type="number"
                  min="1"
                  max="30"
                  step="1"
                  bind:value={stage.steps}
                  oninput={onchange}
                /></label
              >
              <label class="field" style="margin:0 0 6px"
                ><span>CFG</span><input
                  class="inp"
                  type="number"
                  min="1"
                  max="12"
                  step="0.1"
                  bind:value={stage.cfg}
                  oninput={onchange}
                /></label
              >
            </div>
            <label class="field"
              ><span>Sigmas</span><input
                class="inp"
                bind:value={stage.sigmasText}
                oninput={onchange}
              /></label
            >
            <button
              class="btn sm danger"
              style="width:100%;justify-content:center"
              onclick={() => onremovestage(i)}>Remove stage</button
            >
          </div>
        {/each}
        {#if customStages.length === 0}<p class="hint">
            No upscale stages — renders the base pass only.
          </p>{/if}
      {/if}
    </div>
  </div>
</div>
