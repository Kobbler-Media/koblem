<script lang="ts">
  import { capabilities, unavailableFor, type DirectorEngine } from '$lib/director/engine';
  import {
    H3_CHECKPOINT_FLIP_SECONDS,
    H3_DOCUMENTED_MIN_SECONDS,
    H3_MAX_SECONDS,
    h3FrameMode,
    h3HasFrameImage
  } from '$lib/director/h3-generations';
  import { assetSrc } from '$lib/director/assets.svelte';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import PinTime from '$lib/components/video/PinTime.svelte';
  import TakesPanel from './TakesPanel.svelte';
  import {
    V2V_STRENGTH_MAX,
    V2V_STRENGTH_MIN,
    V2V_STRENGTH_STEP,
    isV2v,
    v2vPriorStranded,
    v2vStrengthDefault
  } from '$lib/director/continuity';
  import {
    LORA_STRENGTH_MAX,
    LORA_STRENGTH_MIN,
    LORA_STRENGTH_STEP,
    hasLora,
    loraStrength,
    loraSummary,
    normaliseLoraOverride,
    setLoraStrength,
    toggleLora
  } from '$lib/director/loras';
  import type { ResolvedClipRouting } from '$lib/director/characters';
  import type {
    DirectorCharacter,
    DirectorClip,
    DirectorClipMode,
    DirectorTake,
    ImageProps,
    LtxFrameDrop,
    LtxRuntimeLora,
    LtxVideoProps
  } from '$lib/api/client';

  // Everything about the inspected shot that is not its prompt: how it renders, what it renders
  // from, and which take of it ends up in the cut. The fields write straight onto the clip object,
  // which is the editor's own reactive state — the callbacks are for the things that need the
  // editor's wider context (an upload that must reach the asset store, a render dispatch).
  let {
    engine = 'ltx25',
    projectHasSong = false,
    clip,
    active,
    shotIndex,
    fps,
    modes,
    props,
    imgProps,
    loras,
    frameDrop,
    frameDropNote,
    routing,
    loraNote,
    priorRenders,
    characters,
    snapBusy,
    retakeNote = '',
    canRetake = true,
    resumeNote = '',
    retakeSeedInput = $bindable(),
    takesTier = $bindable(),
    promoteTargetId = $bindable(),
    takeVideoEls = $bindable(),
    takes,
    takeUrls,
    takesBusy,
    criticNote,
    promoteTarget,
    promoteBusy,
    onchange,
    onmode,
    onsceneimage,
    onv2vsource,
    onclipref,
    onremoveclipref,
    onpin,
    onendframe,
    onremovepin,
    onsnapshot,
    onsnapshottoscene,
    onsnapshottoreference,
    onduplicate,
    ondelete,
    onloadtakes,
    onpicktake,
    onreview,
    onpromote
  }: {
    /** The PROJECT's engine. Controls that only one engine has are gated on `caps` below. */
    engine?: DirectorEngine;
    /**
     * Does the project carry a song?
     *
     * H3 only, and it changes what the frame control RECOMMENDS rather than what it does: an exact
     * pin renders on `fl2va`, which takes no audio reference at all, so pinning a frame on a music
     * video silently drops that shot's slice of the track. See the frame-path picker below.
     */
    projectHasSong?: boolean;
    clip: DirectorClip;
    /** Whether this tab is the one on screen. Both panes stay mounted; CSS shows one. */
    active: boolean;
    fps: number;
    /** Raw timeline position — continuity only exists past the first shot. */
    shotIndex: number;
    modes: { value: DirectorClipMode; label: string; hint: string }[];
    props: LtxVideoProps | null;
    imgProps: ImageProps | null;
    /** The PROJECT LoRA set this shot inherits unless it names its own. */
    loras: LtxRuntimeLora[];
    /** The project default this shot's "inherit" resolves to. */
    frameDrop: LtxFrameDrop;
    frameDropNote: string;
    /** What this shot routes to, and any advice about it. */
    routing: ResolvedClipRouting | undefined;
    loraNote: string;
    priorRenders: { job_id: string; serverJobId: string | null; label: string; segments: number }[];
    characters: DirectorCharacter[];
    snapBusy: boolean;
    /**
     * What a retake does to the units AFTER the one it re-renders. See `retakeTailNote`.
     *
     * 🔴 Passed in rather than written here because the answer differs per engine and getting it
     * wrong is expensive in both directions: promising a splice that does not happen leaves a drifting
     * join nobody looks for, and denying one that does costs hours of GPU re-rendering a good tail.
     */
    retakeNote?: string;
    /**
     * Can ONE unit be re-rolled here AT ALL — the capability and the deployment together.
     *
     * 🔴 `retakeNote` describes what a retake does to the units after it, which is only worth
     * saying where one can happen. On LTX-2.5 with no bank directory the engine answers 501, so the
     * sentence described a render the box refuses.
     */
    canRetake?: boolean;
    /** Why not, when `canRetake` is false — `unavailableFor(engine, 'resume')`. */
    resumeNote?: string;
    retakeSeedInput: number;
    takesTier: 'final' | 'proxy';
    promoteTargetId: string | null;
    takeVideoEls: (HTMLVideoElement | null)[];
    takes: DirectorTake[];
    takeUrls: string[];
    takesBusy: boolean;
    criticNote: string;
    promoteTarget: DirectorCharacter | null;
    promoteBusy: boolean;
    onchange: () => void;
    onmode: (m: DirectorClipMode) => void;
    onsceneimage: (files: FileList | null) => void;
    onv2vsource: (files: FileList | null) => void;
    onclipref: (files: FileList | null) => void;
    onremoveclipref: (i: number) => void;
    onpin: (files: FileList | null) => void;
    /** H3 only: attach this shot's closing (`fl2va`) anchor. */
    onendframe: (files: FileList | null) => void;
    onremovepin: (i: number) => void;
    onsnapshot: () => void;
    onsnapshottoscene: () => void;
    onsnapshottoreference: () => void;
    onduplicate: () => void;
    ondelete: () => void;
    onloadtakes: () => void;
    onpicktake: (variant: number) => void;
    onreview: (autoRetake: boolean) => void;
    onpromote: (variant: number, ti: number) => void;
  } = $props();
  const caps = $derived(capabilities(engine));
  const v = $derived(caps.vocab);
  /**
   * 🔴 `renderUnit`, never `subUnit`. Since the beat-collapse migration one `DirectorClip` on H3 is
   * one shot AND one render — a beat is a `[Shot N] At MM:SS.mmm` line inside this shot's prompt,
   * not a clip of its own. So both engines call what this pane edits a "shot", and any label here
   * that still said "beat" was naming a thing that no longer exists.
   */
  const unit = $derived(v.renderUnit);

  /**
   * The modes this engine can actually render.
   *
   * 🔴 The full list used to be handed straight through, so v2v Edit and Restyle were selectable on
   * H3 — and picking one did not fail: the compiler has no v2v path there, so the shot silently
   * compiled as plain t2v and rendered something unrelated to the source clip that had just been
   * attached. Filtered here rather than at the caller because this is the component that renders
   * the buttons, and the next mode added must not be able to slip through a caller that forgot.
   *
   * ⚠️ Filtering these out is NOT the same as "this engine cannot edit video" — see
   * {@link UNAVAILABLE.v2v} and the disclosure under the picker. H3 does it through a numbered
   * `<Video N>` reference on the shot, not through a source attached to one clip.
   */
  const shownModes = $derived(caps.v2v ? modes : modes.filter((m) => !isV2v(m.value)));

  /**
   * Where this shot's footage comes from, as the ENGINE will read it rather than as the document
   * spells it — the two differ for a clip saved against a prior render on an engine that cannot
   * take one, and drawing the document's answer there would offer a picker with one option.
   */
  const priorStranded = $derived(v2vPriorStranded(clip, caps.v2vPriorRenderSource));
  const sourceKind = $derived(
    !priorStranded && clip.v2v_source_kind === 'prior_render' ? 'prior_render' : 'upload'
  );
  /** 🔴 Null while stranded even when bytes are attached — see `v2vPriorStranded`. */
  const attachedSource = $derived(priorStranded ? null : clip.v2v_source_b64);
  const strength = $derived(clip.v2v_strength ?? v2vStrengthDefault(clip.mode));
  /**
   * What that strength resolves to, shown because the number itself does not say. The engine enters
   * a fixed 8-step distilled schedule `round(8 * strength)` steps in, and everything the render
   * does or does not keep follows from that integer rather than from the fraction.
   *
   * ⚠️ Exact on the {@link V2V_STRENGTH_STEP} grid the slider walks, which is the only thing it is
   * used for. The engine rounds in Python, whose `round` is banker's — so a half-step (reachable
   * only by an off-grid value saved before the grid existed) resolves one step lower there than
   * here. The readout is advisory; the plan echo publishes the resolved `steps` per shot.
   */
  const strengthSteps = $derived(Math.round(8 * strength));

  // 362 frames is H3's own generation budget; 20s is LTX's cap.
  const maxShot = $derived(caps.maxFrames ? H3_MAX_SECONDS : 20);

  const modelVariantsShown = $derived(caps.modelVariants && (props?.variants ?? []).length > 1);
  const loraListShown = $derived(caps.loraIdentity === 'path' && (props?.loras ?? []).length > 0);
  const overridesShown = $derived(
    modelVariantsShown || loraListShown || caps.characterRefs || !!routing
  );
  const inheritsEverything = $derived(!clip.model && !clip.loras?.length);
  /**
   * 🔴 `caps.perShotSampling` gates the CARD, the three sampling flags gate the rows inside it, and
   * both are needed. The outer one is false on H3, where these four clip fields reach no compiler
   * at all; the inner three are false on LTX-2.5, where the distilled checkpoint takes them on the
   * wire and ignores them. Seed has no inner flag because every engine that reads `clip.seed`
   * honours it.
   */
  const samplingInherited = $derived(
    clip.seed === undefined &&
      clip.steps === undefined &&
      clip.cfg === undefined &&
      !clip.negative_prompt
  );
  const continuityShown = $derived(caps.chainedShots && clip.mode === 't2v' && shotIndex >= 1);
  const frameModeShown = $derived(caps.endFrame && h3HasFrameImage(clip));
  const pinStrengthShown = $derived(
    caps.pinStrength && (clip.keyframes.length > 0 || (clip.mode === 'i2v' && !!clip.image_b64))
  );
  const framesShown = $derived(clip.mode === 'i2v' || caps.endFrame || pinStrengthShown);
</script>

{#snippet frameSlot(label: string, img: string | null | undefined, remove: () => void)}
  <div class="framerow">
    <span
      class="fthumb"
      class:on={!!img}
      style={img ? `background-image:url('${assetSrc(img)}')` : ''}
    ></span>
    <span class="fname">{label}<small>{img ? 'attached' : 'none attached'}</small></span>
    {#if img}<button class="btn sm" onclick={remove}>Remove</button>{/if}
  </div>
{/snippet}

<div class="pane" class:on={active}>
  <div class="cards">
    <div class="card">
      <h3>Mode</h3>
      <p class="sub">What this {unit} renders from.</p>
      <div class="modelist">
        {#each shownModes as m (m.value)}
          <button class="mode" class:on={clip.mode === m.value} onclick={() => onmode(m.value)}
            ><b>{m.label}</b><small>{m.hint}</small></button
          >
        {/each}
      </div>
      {#if !caps.v2v}
        <!-- 🔴 A ROUTE, not a refusal. This printed "MiniMax-H3 has no video-to-video path", which
             was false: `video editing` is one of the engine's six task types and `edit_source` has
             been a selectable reference role the whole time. What is absent is the per-CLIP source
             field, because the source is addressed by ordinal from inside the prompt. -->
        <details class="why">
          <summary>Editing existing footage on {caps.label}</summary>
          <p>{unavailableFor(engine ?? 'ltx25', 'v2v')}</p>
        </details>
      {/if}
    </div>

    {#if caps.v2v && isV2v(clip.mode)}
      <div class="card">
        <h3>Source clip</h3>
        <p class="sub">The footage this {unit} is built from, and how far the prompt moves it.</p>
        <!-- 🔴 The PICKER goes, not the card. Only the prior-render half is refused here (a banked
             latent the sampler cannot be started from); uploading a clip is the path that works,
             and hiding the whole mode to express the narrow half is what `v2vPriorRenderSource`
             exists to stop. -->
        {#if caps.v2vPriorRenderSource}
          <div class="modelist">
            <button
              class="mode"
              class:on={(clip.v2v_source_kind ?? 'upload') === 'upload'}
              onclick={() => {
                clip.v2v_source_kind = 'upload';
                onchange();
              }}><b>Upload a clip</b><small>Foreign footage</small></button
            >
            <button
              class="mode"
              class:on={clip.v2v_source_kind === 'prior_render'}
              onclick={() => {
                clip.v2v_source_kind = 'prior_render';
                onchange();
              }}><b>From a prior render</b><small>Reuse a rendered {unit}</small></button
            >
          </div>
        {/if}
        {#if priorStranded}
          <!-- Saved pointing at a prior render, on an engine that cannot read one. Said out loud
               rather than falling back to whatever bytes happen to be attached: the two fields are
               independent, so the fallback would render footage this {unit} was moved off. -->
          <p class="hint warn-note">
            This {unit} is set to edit a {unit} you already rendered, which {caps.label} cannot use as
            a source — so it has none. Attach the clip below.
          </p>
        {/if}
        {#if sourceKind === 'upload'}
          {#if attachedSource}
            <div class="inline-row" style="align-items:center">
              <span class="hint">source attached</span><button
                class="btn sm"
                onclick={() => {
                  clip.v2v_source_b64 = null;
                  onchange();
                }}>Remove</button
              >
            </div>
          {:else}
            <FileDropzone accept="video/*" label="Drop source video" onFiles={onv2vsource} />
          {/if}
        {:else}
          <label class="field"
            ><span>Prior render → source segment</span>
            <div class="inline-row">
              {#if priorRenders.length}
                <select
                  class="inp"
                  value={clip.v2v_prior_job_id ?? ''}
                  onchange={(e) => {
                    clip.v2v_prior_job_id = e.currentTarget.value || null;
                    onchange();
                  }}
                >
                  <option value="">— pick a render —</option>
                  {#each priorRenders as r (r.job_id)}<option value={r.serverJobId ?? r.job_id}
                      >{r.label} ({r.segments} {v.renderUnitPlural})</option
                    >{/each}
                </select>
              {:else}
                <input
                  class="inp"
                  placeholder="prior render job id"
                  value={clip.v2v_prior_job_id ?? ''}
                  oninput={(e) => {
                    clip.v2v_prior_job_id = e.currentTarget.value || null;
                    onchange();
                  }}
                />
              {/if}
              <input
                class="inp"
                type="number"
                min="0"
                step="1"
                value={clip.v2v_prior_segment ?? 0}
                oninput={(e) => {
                  clip.v2v_prior_segment = parseInt(e.currentTarget.value, 10) || 0;
                  onchange();
                }}
                style="width:60px;flex:none"
                title="which {unit} of that render"
              />
            </div>
          </label>
        {/if}
        {#if !caps.v2vPriorRenderSource}
          <details class="why">
            <summary>Editing a {unit} you already rendered</summary>
            <p>{unavailableFor(engine ?? 'ltx25', 'v2vPriorRenderSource')}</p>
          </details>
        {/if}
        <label class="field" style="margin-bottom:0"
          ><span
            >{clip.mode === 'v2v_edit' ? 'Edit' : 'Restyle'} strength ·
            <b>{strength.toFixed(3)}</b> · {strengthSteps} of 8 steps</span
          >
          <!-- Eighths, because eighths are all the sampler can land on: the strength picks how many
               of a fixed 8-step schedule are re-run, so the finer slider this used to be spent two
               thirds of its travel moving the readout and not the picture. -->
          <input
            type="range"
            class="slider"
            min={V2V_STRENGTH_MIN}
            max={V2V_STRENGTH_MAX}
            step={V2V_STRENGTH_STEP}
            value={strength}
            oninput={(e) => {
              clip.v2v_strength = parseFloat(e.currentTarget.value);
              onchange();
            }}
          />
          <p class="hint">
            Higher = bigger change from the prompt · lower = keep more of the source.
            <b>{caps.label}’s schedule is front-loaded</b>, so this is not the strength dial it is
            elsewhere: 0.500 already starts at noise 0.975 and is very nearly a fresh generation.
            Real editing lives at <b>0.250 and below</b>; past 0.375 the source is mostly a memory.
          </p>
        </label>
        {#if shotIndex >= 1 && caps.chainedShots}
          <!-- Not a control: there is nothing here to choose. The source becomes the shot's own
               starting latent, which is the same thing a continuation carry writes into, so the
               engine refuses a {unit} that is both and the compiler marks every sourced v2v {unit}
               as a cut. Said here because the Continuity picker is absent on this mode, and its
               absence would otherwise read as "it inherited whatever it had". -->
          <p class="hint" style="margin-top:8px">
            A {unit} built from footage is always a hard cut — it starts from its source, so it cannot
            also carry motion out of the previous {unit}.
          </p>
        {/if}
      </div>
    {/if}

    <!-- Continuity is a property of a CHAIN. H3 shots share no latent state at all, so there is no
         "continue from the previous shot" to choose — every shot is already a cut. -->
    {#if continuityShown}
      <div class="card">
        <h3>Joins the previous {unit}</h3>
        <p class="sub">
          A hard cut renders fresh from this prompt alone — no carried-over motion. Audio stays
          continuous either way.
        </p>
        <label class="field" style="margin-bottom:0"
          ><span>Continuity</span>
          <select
            class="inp"
            value={clip.new_scene ? 'cut' : 'continue'}
            onchange={(e) => {
              clip.new_scene = e.currentTarget.value === 'cut';
              onchange();
            }}
          >
            <option value="continue">Continue → (flow on from the previous {unit})</option>
            <option value="cut">New scene ⟂ (hard cut, fresh from the prompt)</option>
          </select>
        </label>
      </div>
    {/if}

    <div class="card">
      <h3>This {unit}</h3>
      <p class="sub">How long it runs, and what to do with it.</p>
      <label class="field"
        ><span>Duration (s)</span><input
          class="inp narrow"
          type="number"
          min="0.5"
          max={maxShot}
          step="0.5"
          bind:value={clip.seconds}
          oninput={onchange}
        />
      </label>
      {#if caps.maxFrames}
        <p class="hint">
          Up to {H3_MAX_SECONDS.toFixed(2)}s. Under {H3_DOCUMENTED_MIN_SECONDS.toFixed(2)}s is
          outside the range MiniMax document and is flagged — nothing is lengthened, it renders at
          the length you wrote.
        </p>
      {/if}
      <div class="inline-row" style="margin-top:11px">
        <button class="btn sm" style="flex:1;justify-content:center" onclick={onduplicate}
          >Duplicate</button
        ><button class="btn sm danger" style="flex:1;justify-content:center" onclick={ondelete}
          >Delete</button
        >
      </div>
    </div>

    {#if overridesShown}
      <div class="card">
        <h3>
          Model and LoRAs{#if inheritsEverything}<span class="scopetag">inherited</span>{/if}
        </h3>
        <p class="sub">Set here, this {unit} overrides the project default.</p>
        {#if modelVariantsShown}
          <label class="field"
            ><span>Model</span>
            <select class="inp" bind:value={clip.model} {onchange}>
              <option value={undefined}>project default</option>
              {#each props?.variants ?? [] as variant (variant)}<option value={variant}
                  >{variant}</option
                >{/each}
            </select>
          </label>
        {/if}
        {#if caps.characterRefs}
          <label class="field"
            ><span>Drop first frame</span>
            <select
              class="inp"
              value={clip.frame_drop === undefined
                ? 'inherit'
                : clip.frame_drop === 'off'
                  ? 'off'
                  : 'auto'}
              onchange={(e) => {
                // Three states, exactly like the cast selector: "inherit" is UNDEFINED and is
                // never a synonym for "off". A numeric frame_drop coming from the API keeps
                // working — it just shows here as Auto, which is what it behaves like.
                const v = e.currentTarget.value;
                clip.frame_drop = v === 'inherit' ? undefined : v === 'off' ? 'off' : 'auto';
                onchange();
              }}
            >
              <option value="inherit"
                >project default ({frameDrop === 'off' ? 'off' : 'auto'})</option
              >
              <option value="auto">Auto (engine decides)</option>
              <option value="off">Off</option>
            </select>
          </label>
          <p class="hint">{frameDropNote}</p>
        {/if}
        {#if routing}
          <p class="hint">
            {routing.automatic ? '💡' : '📌'}
            {routing.reason}
          </p>
        {/if}
        <!-- `loraIdentity === 'path'`: the LTX checklist, driven by `loras.json` metadata. H3 takes
             LoRAs as well but names them by bare filename, and its picker lives higher up THIS tab,
             in `H3PromptComposer`'s details pane through the shared `H3RenderKnobs`. -->
        {#if loraListShown}
          <details class="loras">
            <summary
              >Runtime LoRAs ({clip.loras?.length
                ? loraSummary(clip.loras, true, props?.loras)
                : `inherited${loras.length ? `: ${loraSummary(loras, true, props?.loras)}` : ' (project has none)'}`})</summary
            >
            <p class="hint">
              Ticking here REPLACES the project set — it is not additive. Untick everything to
              inherit again: "this {unit} runs with none while the project has some" is the one state
              the engine's per-segment <code>lora</code> cannot express.
            </p>
            {#each props?.loras ?? [] as l (l.path)}
              {@const on = hasLora(clip.loras, l.path)}
              {@const inherited = !clip.loras?.length && hasLora(loras, l.path)}
              <div class="lorarow">
                <label class="inline-row" style="gap:6px;flex:1;min-width:0;align-items:center">
                  <input
                    type="checkbox"
                    checked={on}
                    onchange={() => {
                      // `undefined` is the ONE representation of inherit — an override the
                      // user emptied must not persist as `[]`, which compiles to exactly the
                      // same wire bytes and would leave the UI claiming otherwise.
                      clip.loras = normaliseLoraOverride(
                        toggleLora(clip.loras, l.path, props?.loras)
                      );
                      onchange();
                    }}
                  />
                  <!-- Label AND file name: the label is for reading, the file name is the string
                       the engine 400s on when a name goes stale. An undescribed adapter labels
                       itself, so it is not printed twice. -->
                  <span class="nm"
                    ><b>{l.label}</b>{#if l.path !== l.label}
                      · {l.path}{/if}{#if inherited}<em> (inherited)</em>{/if}</span
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
                    value={loraStrength(clip.loras, l.path)}
                    oninput={(e) => {
                      clip.loras = setLoraStrength(
                        clip.loras,
                        l.path,
                        Number(e.currentTarget.value)
                      );
                      onchange();
                    }}
                  />
                {/if}
              </div>
            {/each}
          </details>
          <p class="inherit">Renders with: {loraNote}</p>
        {/if}
      </div>
    {/if}

    {#if caps.perShotSampling}
      <div class="card">
        <h3>
          Sampling{#if samplingInherited}<span class="scopetag">inherited</span>{/if}
        </h3>
        <p class="sub">
          Left blank, each inherits the project value. Pin a seed here to reroll this {unit} without disturbing
          the ones around it.
        </p>
        <label class="field"
          ><span>Seed</span><input
            class="inp narrow"
            type="number"
            step="1"
            placeholder="project seed"
            value={clip.seed ?? ''}
            onchange={(e) => {
              // `onchange` rather than `oninput` throughout this card: these boxes coerce, and on
              // every keystroke a half-typed number coerces to `undefined` and the binding wipes
              // the digits back out from under the cursor.
              const n = Number(e.currentTarget.value);
              clip.seed =
                e.currentTarget.value.trim() && Number.isFinite(n) && n >= 0
                  ? Math.trunc(n)
                  : undefined;
              onchange();
            }}
          />
        </label>
        {#if caps.customRecipe}
          <label class="field"
            ><span>Steps</span><input
              class="inp narrow"
              type="number"
              min="1"
              max="60"
              step="1"
              placeholder="project steps"
              value={clip.steps ?? ''}
              onchange={(e) => {
                const n = Number(e.currentTarget.value);
                clip.steps = Number.isFinite(n) && n > 0 ? Math.trunc(n) : undefined;
                onchange();
              }}
            />
          </label>
        {/if}
        {#if caps.cfg}
          <label class="field"
            ><span>CFG</span><input
              class="inp narrow"
              type="number"
              min="0"
              max="12"
              step="0.5"
              placeholder="project cfg"
              value={clip.cfg ?? ''}
              onchange={(e) => {
                const n = Number(e.currentTarget.value);
                clip.cfg =
                  e.currentTarget.value.trim() && Number.isFinite(n) && n >= 0 ? n : undefined;
                onchange();
              }}
            />
            <p class="hint">Only the <code>dev</code> variant responds above 1.</p>
          </label>
        {/if}
        {#if caps.negativePrompt}
          <label class="field" style="margin-bottom:0"
            ><span>Negative prompt</span><input
              class="inp"
              placeholder="project negative prompt"
              value={clip.negative_prompt ?? ''}
              onchange={(e) => {
                const t = e.currentTarget.value;
                clip.negative_prompt = t.trim() === '' ? undefined : t;
                onchange();
              }}
            />
          </label>
        {/if}
      </div>
    {/if}

    {#if framesShown}
      <div class="card">
        <h3>Frames</h3>
        <p class="sub">
          {caps.endFrame
            ? `The images this ${unit} opens and closes on, and how they reach the model.`
            : `The image this ${unit} opens on.`}
        </p>
        {#if clip.mode === 'i2v'}
          {@render frameSlot('Scene image', clip.image_b64, () => {
            clip.image_b64 = null;
            onchange();
          })}
          {#if !clip.image_b64}
            <FileDropzone
              accept="image/*"
              label="Drop a still, or storyboard this {unit} to generate one"
              onFiles={onsceneimage}
            />
          {/if}
        {/if}
        <!-- H3's fl2va closing anchor. No LTX equivalent: LTX pins an opener and interpolates
             through mid-shot keyframes, but nothing anchors where a shot ENDS. -->
        {#if caps.endFrame}
          {@render frameSlot('Last frame', clip.h3_end_image, () => {
            clip.h3_end_image = null;
            onchange();
          })}
          {#if !clip.h3_end_image}
            <FileDropzone accept="image/*" label="Add last frame" onFiles={onendframe} />
          {/if}
        {/if}

        <!-- 🔴 Two paths, not one setting with a quality slider. They land on DIFFERENT CHECKPOINTS
             and buy different things, and until 2026-08-08 only the expensive one existed — every
             attached frame forced `fl2va`, which threw away every reference AND the project song.
             The measured numbers are in the buttons because the trade is not guessable. See
             `h3-keyframes.ts`. -->
        {#if frameModeShown}
          {@const mode = h3FrameMode(clip)}
          <div class="modelist spaced">
            <button
              class="mode"
              class:on={mode === 'keyframe'}
              onclick={() => {
                clip.h3_frame_mode = 'keyframe';
                onchange();
              }}
              ><b>Open on this image</b><small
                >Keeps every reference and the song. The image becomes a numbered &lt;Picture N&gt;
                and the prompt declares it as this {unit}'s frame — 0.776 frame-0 similarity, and
                free.</small
              ></button
            >
            <button
              class="mode"
              class:on={mode === 'pin'}
              onclick={() => {
                clip.h3_frame_mode = 'pin';
                onchange();
              }}
              ><b>Match this frame exactly</b><small
                >0.968 frame-0 similarity — but it renders on fl2va, which drops every reference{projectHasSong
                  ? ' and this shot’s slice of the song'
                  : ''}, isolates it in a render of its own, and costs a ~{H3_CHECKPOINT_FLIP_SECONDS}s
                checkpoint flip.</small
              ></button
            >
          </div>
          {#if mode === 'pin' && projectHasSong}
            <!-- 🔴 The one combination that is silently destructive. `fl2va` has no audio-reference
                 path at all (`pipeline.py:298`), so the slice is not weakened, it is absent — and
                 the render comes back looking fine, scored by something the model invented. -->
            <p class="hint err">
              ⚠ This project has a song, and an exact pin renders on <b>fl2va</b> — which takes no
              audio reference. This {unit} would be scored by whatever the model invents, not by your
              track. "Open on this image" keeps both.
            </p>
          {:else if mode === 'keyframe'}
            <details class="why">
              <summary>Why the prompt has to declare it</summary>
              <p>
                The declarations that make this work are written into this {unit}'s prompt — the
                <b>Prompt</b> tab says so and offers to write them if they are missing. Sent as a picture
                with no declarations, the same image measures 0.262.
              </p>
            </details>
          {/if}
        {/if}

        {#if pinStrengthShown}
          <label class="field spaced" style="margin-bottom:0"
            ><span>Pin strength · <b>{(clip.pin_strength ?? 1).toFixed(2)}</b></span>
            <input
              type="range"
              class="slider"
              min="0.5"
              max="1"
              step="0.05"
              value={clip.pin_strength ?? 1}
              oninput={(e) => {
                clip.pin_strength = parseFloat(e.currentTarget.value);
                onchange();
              }}
            />
            <p class="hint">
              {(clip.pin_strength ?? 1) >= 0.999
                ? 'exact — the pinned image IS that frame (crispest)'
                : 'softer — the pin blends into the generated motion'} · higher = crisper pin
            </p>
          </label>
        {/if}
      </div>
    {/if}

    {#if caps.midShotKeyframes}
      <div class="card">
        <h3>Keyframe pins <span class="cnt">{clip.keyframes.length}</span></h3>
        <p class="sub">
          Actual frames pinned into the render at a timecode — unlike look references, these are
          rendered.
        </p>
        {#each clip.keyframes as kf, i (i)}
          <div class="pinrow">
            <div
              class="pt"
              style="background-image:url('{assetSrc(kf.image_b64)}');background-size:cover"
            ></div>
            <div style="flex:1;display:flex;flex-direction:column;gap:6px;min-width:0">
              <PinTime bind:seconds={kf.seconds} shotSeconds={clip.seconds} {fps} {onchange} />
              <input bind:value={kf.label} oninput={onchange} placeholder="label / lyric" />
            </div>
            <button class="btn sm danger" onclick={() => onremovepin(i)}>✕</button>
          </div>
        {/each}
        <FileDropzone accept="image/*" label="Add frame pin" onFiles={onpin} />
      </div>
    {/if}

    <div class="card">
      <h3>Look references <span class="cnt">{(clip.references ?? []).length}</span></h3>
      <p class="sub">
        Handed to the LLM <b>as it drafts</b> — they steer the written prompt and are never rendered.
      </p>
      {#if (clip.references ?? []).length}
        <div class="thumbs">
          {#each clip.references ?? [] as ref, i (i)}
            <div
              class="refthumb"
              style="background-image:url('{assetSrc(ref.image_b64)}');background-size:cover"
            >
              <button class="x" onclick={() => onremoveclipref(i)}>×</button>
            </div>
          {/each}
        </div>
      {/if}
      <FileDropzone accept="image/*" label="Add look reference" onFiles={onclipref} />
    </div>

    <div class="card">
      <h3>Storyboard still</h3>
      <p class="sub">A quick FLUX preview of the {unit} — eyeball it before rendering video.</p>
      {#if clip.snapshot_b64}
        <div class="snaprow">
          <img src={assetSrc(clip.snapshot_b64)} alt="storyboard still" />
          <div class="snapacts">
            <button class="btn sm" onclick={onsnapshottoscene}>Use as scene image</button>
            <button class="btn sm" onclick={onsnapshottoreference}>Add as look reference</button>
            <button
              class="btn sm ghost"
              onclick={onsnapshot}
              disabled={snapBusy || (imgProps ? !imgProps.healthy : false)}
              >{snapBusy ? 'Drawing…' : 'Redraw'}</button
            >
          </div>
        </div>
      {:else}
        <button
          class="btn"
          style="width:100%;justify-content:center"
          onclick={onsnapshot}
          disabled={snapBusy || (imgProps ? !imgProps.healthy : false)}
          >{snapBusy ? 'Drawing…' : `🎨 Storyboard this ${unit}`}</button
        >
      {/if}
    </div>

    <div class="card">
      <h3>Takes and seed</h3>
      <p class="sub">
        Which roll of this {unit} lands in the cut, and what the next one is drawn from.
      </p>
      <label class="field"
        ><span>Retake seed</span><input
          class="inp narrow"
          type="number"
          step="1"
          bind:value={retakeSeedInput}
        />
      </label>
      <p class="hint">-1 = fresh random roll{canRetake ? ` · ${retakeNote}` : ''}</p>
      {#if !canRetake && resumeNote}<p class="hint">{resumeNote}</p>{/if}
      {#if caps.generationIsRetakeUnit}
        <p class="hint">Takes belong to the whole {unit} — a retake re-renders it end to end.</p>
      {/if}
      <div class="spaced">
        <TakesPanel
          bind:takesTier
          bind:promoteTargetId
          bind:takeVideoEls
          {takes}
          {takeUrls}
          {takesBusy}
          {criticNote}
          {characters}
          {promoteTarget}
          {promoteBusy}
          onload={onloadtakes}
          onpick={onpicktake}
          {onreview}
          {onpromote}
        />
      </div>
    </div>
  </div>
</div>

<style>
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
    gap: 16px;
    align-items: start;
  }
  .card {
    min-width: 0;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: var(--bg-2);
    padding: 14px 15px 16px;
  }
  .card > h3 {
    margin: 0 0 3px;
    font-size: 11.5px;
    font-weight: 650;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--accent);
  }
  .card > .sub {
    margin: 0 0 11px;
    font-size: 11px;
    line-height: 1.5;
    color: var(--muted);
  }
  .card > .sub b {
    color: var(--ink);
  }
  .card .hint {
    margin: 7px 0 0;
  }
  /* A hint that reports a state the author has to act on, rather than explaining a control. */
  .card .hint.warn-note {
    color: var(--warn, #d0a030);
  }
  .cnt {
    margin-left: 6px;
    font-family: var(--mono);
    font-size: 10.5px;
    font-weight: 500;
    letter-spacing: 0;
    color: var(--faint);
  }
  .scopetag {
    margin-left: 7px;
    padding: 0 5px;
    border: 1px solid var(--ring);
    border-radius: 3px;
    font-size: 9px;
    font-weight: 500;
    letter-spacing: 0.12em;
    color: var(--faint);
  }
  /* A card is 300px at its narrowest, where the two-column `.modes` grid crushes each option's
     description into a two-word column. */
  .modelist {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 10px;
  }
  .spaced {
    margin-top: 11px;
  }
  /* Beats `.dw .inp`'s `width: 100%` on specificity — a lone integer wants no full-width box. */
  .card .narrow {
    max-width: 150px;
  }
  .framerow {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 7px 0;
    border-bottom: 1px dashed var(--ring);
  }
  .fthumb {
    width: 46px;
    height: 26px;
    flex: none;
    border-radius: 4px;
    background: var(--bg-3) center/cover no-repeat;
    box-shadow: inset 0 0 0 1px var(--ring);
  }
  .fthumb.on {
    box-shadow: inset 0 0 0 1px var(--accent);
  }
  .fname {
    flex: 1;
    min-width: 0;
    font-size: 11.5px;
  }
  .fname small {
    display: block;
    font-family: var(--mono);
    font-size: 10px;
    color: var(--faint);
  }
  .lorarow {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 5px;
  }
  .lorarow .nm {
    flex: 1;
    min-width: 0;
    font-size: 11px;
    line-height: 1.4;
    color: var(--muted);
    overflow-wrap: anywhere;
  }
  .lorarow .nm b {
    color: var(--ink);
  }
  .inherit {
    margin: 9px 0 0;
    font-size: 10.5px;
    line-height: 1.5;
    color: var(--faint);
  }
  .loras > summary {
    list-style: none;
    cursor: pointer;
    padding: 4px 0;
    font-size: 11px;
    color: var(--muted);
  }
  .loras > summary::-webkit-details-marker {
    display: none;
  }
  .loras > summary::before {
    content: '▸ ';
    color: var(--faint);
  }
  .loras[open] > summary::before {
    content: '▾ ';
  }
  .loras > summary:hover {
    color: var(--accent);
  }
  .why {
    margin: 11px 0 0;
    border-left: 2px solid var(--ring);
    padding-left: 9px;
  }
  .why > summary {
    list-style: none;
    cursor: pointer;
    font-size: 10.5px;
    color: var(--faint);
  }
  .why > summary::-webkit-details-marker {
    display: none;
  }
  .why > summary::before {
    content: '+ ';
  }
  .why[open] > summary::before {
    content: '− ';
  }
  .why > summary:hover {
    color: var(--accent);
  }
  .why p {
    margin: 6px 0 0;
    font-size: 11px;
    line-height: 1.55;
    color: var(--muted);
  }
  .snaprow {
    display: flex;
    align-items: flex-start;
    gap: 10px;
  }
  .snaprow img {
    width: 112px;
    height: 63px;
    flex: none;
    object-fit: cover;
    border-radius: 8px;
    border: 1px solid var(--ring);
  }
  .snapacts {
    display: flex;
    flex-direction: column;
    gap: 6px;
    flex: 1;
    min-width: 0;
  }
  .snapacts .btn {
    justify-content: center;
  }
</style>
