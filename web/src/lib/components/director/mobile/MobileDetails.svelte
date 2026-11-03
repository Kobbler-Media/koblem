<script lang="ts">
  import type {
    DirectorClip,
    DirectorH3Generation,
    H3ContinuationMode,
    H3LoraListing,
    LtxLoraInfo,
    LtxRuntimeLora
  } from '#lib/api/client.ts';
  import type { EngineCapabilities } from '#lib/director/engine.ts';
  import { H3_CHECKPOINT_FLIP_SECONDS } from '#lib/director/h3-generations.ts';
  import {
    hasLora,
    loraStrength,
    loraSummary,
    LORA_STRENGTH_MAX,
    LORA_STRENGTH_MIN,
    LORA_STRENGTH_STEP,
    normaliseLoraOverride,
    setLoraStrength,
    toggleLora
  } from '#lib/director/loras.ts';

  /**
   * How this render unit is CONFIGURED — length, what it opens on, how it joins the one before,
   * and the knobs. Everything the model reads lives on the other three views.
   */
  let {
    clip,
    doc = null,
    caps,
    fps,
    maxFrames = null,
    frames,
    renderSeconds = null,
    partUnit = 'beat',
    unit,
    songNote = null,
    reachableNote = null,
    opensOnImage = false,
    frameMode = 'pin',
    frameStill = '',
    refsInScope = 0,
    joinMode = null,
    joinDepth = 7,
    joinNote = null,
    loraCatalogue = null,
    projectLoras = [],
    ltxLoras = [],
    projectLtxLoras = [],
    loraNote = null,
    projectSteps = undefined,
    projectWidth = 0,
    projectHeight = 0,
    onseconds,
    onframeimage,
    onframemode,
    onremoveframe,
    onjoin,
    onjoindepth,
    onupdate,
    onchange
  }: {
    clip: DirectorClip;
    /** The generation's non-block state. `null` on engines with no structured prompt. */
    doc?: DirectorH3Generation | null;
    caps: EngineCapabilities;
    fps: number;
    /** The frame ceiling in force for THIS project — see `MobilePrompt`'s own prop doc. */
    maxFrames?: number | null;
    /** What the RENDER UNIT will actually render, after the engine's frame grid. */
    frames: number;
    /**
     * The render unit's own delivered length, when this clip is only PART of it — `null` when the
     * clip is the whole render unit.
     *
     * 🔴 Without it the row read "4.20s" over "362 frames", which are two different things: the
     * stepper moves this clip's authored seconds, while `frames` belongs to the generation that
     * packs it. Naming both is the only way the numbers can be read.
     */
    renderSeconds?: number | null;
    /** What this clip is called INSIDE a multi-part render unit — `beat` on H3. */
    partUnit?: string;
    unit: string;
    /** Whether the song slice still lines up at this length, in one sentence. */
    songNote?: string | null;
    reachableNote?: string | null;
    opensOnImage?: boolean;
    frameMode?: 'pin' | 'keyframe';
    frameStill?: string;
    refsInScope?: number;
    joinMode?: H3ContinuationMode | null;
    joinDepth?: number;
    joinNote?: string | null;
    /** `loraIdentity: 'filename'` — what the engine has on disk, by bare name. */
    loraCatalogue?: H3LoraListing | null;
    projectLoras?: string[];
    /**
     * `loraIdentity: 'path'` — the annotated checklist an LTX shot overrides from.
     *
     * 🔴 A different control, not the same one with different data: these write
     * `{path, multiplier}` objects onto the CLIP, where the filename engine writes bare strings
     * onto the generation. `caps.loras` cannot tell them apart, which is what `loraIdentity` is for.
     */
    ltxLoras?: LtxLoraInfo[];
    /** The project-level set an un-overridden shot inherits, for the "(inherited)" marks. */
    projectLtxLoras?: LtxRuntimeLora[];
    /** What this shot ends up rendering with, in words. */
    loraNote?: string | null;
    projectSteps?: number | undefined;
    projectWidth?: number;
    projectHeight?: number;
    onseconds: (next: number) => void;
    onframeimage: () => void;
    onframemode: (mode: 'pin' | 'keyframe') => void;
    onremoveframe: () => void;
    onjoin: (mode: H3ContinuationMode) => void;
    onjoindepth: (k: number) => void;
    onupdate: (patch: Partial<DirectorH3Generation>) => void;
    onchange: () => void;
  } = $props();

  const listId = $props.id();

  /**
   * The stepper walks the ENGINE's own grid, so a tap cannot land off it. On H3 that is `17n+5`
   * clamped into 22..362; on LTX it is `8n+1` with no clamp.
   *
   * 🔴 `caps.fixedFps` rather than the document's `fps`: on an engine defined at one rate the packer
   * computes on that constant whatever is stored, and a stepper on the stored rate walks a grid the
   * packer never snaps to.
   */
  const rate = $derived(caps.fixedFps ?? fps);
  const step = $derived(caps.framePeriod / rate);
  const shorter = $derived(Math.max(step, clip.seconds - step));
  const longer = $derived(clip.seconds + step);
  // `frames` bounds the whole RENDER UNIT, which is what the engine's floor and ceiling are about;
  // the second clause is this clip's own, so `−` cannot round back UP to one step.
  const atFloor = $derived(
    (caps.minFrames !== null && frames <= caps.minFrames) || clip.seconds <= step
  );
  // The project's own ceiling, not the spec cap: a song lowers it to 345 frames (`h3MaxFrames`).
  const ceiling = $derived(maxFrames ?? caps.maxFrames);
  const atCeiling = $derived(ceiling !== null && frames >= ceiling);

  /**
   * What the shot OPENS on, derived and stated. The engine picks its checkpoint from the inputs, so
   * there is nothing here to choose between — only the one action that changes what the inputs are.
   */
  const frameState = $derived.by(() => {
    if (!opensOnImage)
      return {
        glyph: '◔',
        title: 'Starts from the prompt',
        sub:
          refsInScope > 0
            ? `${refsInScope} reference${refsInScope === 1 ? '' : 's'} in scope — the prompt can name them`
            : 'No opening image and no references'
      };
    if (frameMode === 'pin')
      return {
        glyph: '◧',
        title: 'Opens on an exact frame',
        sub: `The attached image is frame 0${refsInScope > 0 ? ` — that takes ${refsInScope} reference${refsInScope === 1 ? '' : 's'} out of scope` : ''}`
      };
    return {
      glyph: '◨',
      title: 'Opens on its references',
      sub: `The attached image is one of them, so the ${refsInScope} in scope all survive`
    };
  });

  const lora = $derived(doc?.loras?.[0] ?? '');
  const loraMul = $derived(doc?.lora_multipliers?.[0] ?? 1);
</script>

<div class="rowline">
  <span class="lbl">Length</span>
  <div class="dur-step">
    <button type="button" aria-label="Shorter" disabled={atFloor} onclick={() => onseconds(shorter)}
      >−</button
    >
    <span class="val">{clip.seconds.toFixed(2)}s</span>
    <button type="button" aria-label="Longer" disabled={atCeiling} onclick={() => onseconds(longer)}
      >+</button
    >
  </div>
</div>
<p class="hint" style="margin:-4px 0 11px">
  {#if renderSeconds !== null}
    This {partUnit} is {clip.seconds.toFixed(2)}s of a {renderSeconds.toFixed(2)}s {unit} —
  {/if}{frames} frames at {rate} fps, snapped up onto {caps.framePeriod}n+{caps.frameBase}.{songNote
    ? ` ${songNote}`
    : ''}
</p>
{#if reachableNote}
  <div class="alert">
    <span class="gl" aria-hidden="true">◈</span><span class="ab">{reachableNote}</span>
  </div>
{/if}

<div class="grp">
  <p class="eye">Opening frame</p>
  <div class="framestate">
    <span class="fs-ico" aria-hidden="true">
      {#if frameStill}<img src={frameStill} alt="" />{:else}{frameState.glyph}{/if}
    </span>
    <span class="fs-tx"><b>{frameState.title}</b><span>{frameState.sub}</span></span>
  </div>
  {#if !opensOnImage}
    <button class="btn full" type="button" onclick={onframeimage}>Open on an image</button>
    <p class="hint">
      An image makes the first frame exact. Without one the {unit} is drawn entirely from the prompt and
      whatever references are in scope.
    </p>
  {:else if caps.keyframesAndRefsCompose}
    <button class="btn full" type="button" onclick={onremoveframe}>Remove the opening image</button>
  {:else if frameMode === 'pin'}
    <button class="btn full" type="button" onclick={() => onframemode('keyframe')}
      >Keep the references instead</button
    >
    <p class="hint">
      A pinned frame takes {refsInScope}
      {refsInScope === 1 ? 'reference' : 'references'} out of scope — a {unit} holds one or the other,
      not both — and flips the checkpoint, which costs about {H3_CHECKPOINT_FLIP_SECONDS}s of reload
      per switch.
    </p>
    <button class="btn full ghost" type="button" onclick={onremoveframe}
      >Remove the opening image</button
    >
  {:else}
    <button class="btn full" type="button" onclick={() => onframemode('pin')}
      >Pin an exact opening frame</button
    >
    <p class="hint">
      Declared as a reference, the image holds the references and the song slice but only
      approximates frame 0. Pinning it is exact and drops both.
    </p>
    <button class="btn full ghost" type="button" onclick={onremoveframe}
      >Remove the opening image</button
    >
  {/if}
</div>

{#if caps.crossGenerationContinuation && joinMode !== null}
  <details class="fold">
    <summary>Join to the previous {unit} <span class="chev" aria-hidden="true">›</span></summary>
    <div class="fbody">
      <div class="field">
        <span>Join</span>
        <select
          class="inp"
          value={joinMode}
          onchange={(e) => onjoin(e.currentTarget.value as H3ContinuationMode)}
        >
          <option value="latents">Continue</option>
          <option value="frame">Last frame</option>
          <option value="none">Hard cut</option>
        </select>
      </div>
      <p class="hint">
        {#if joinMode === 'latents'}
          Continues out of the previous {unit}'s last latents. Continuity inside a {unit} is free; across
          this join it is approximate.
        {:else if joinMode === 'frame'}
          Opens on the previous {unit}'s final frame as a picture. Weaker than latents, but it
          survives a re-render of either side.
        {:else}
          Nothing carries over. This {unit} is drawn from its own prompt alone.
        {/if}
      </p>
      {#if joinNote}<p class="hint">{joinNote}</p>{/if}
      {#if joinMode === 'latents'}
        <div class="field">
          <span>Pin depth</span>
          <input
            class="inp"
            inputmode="numeric"
            value={joinDepth}
            onchange={(e) => {
              const v = Number(e.currentTarget.value);
              if (Number.isFinite(v)) onjoindepth(Math.max(1, Math.trunc(v)));
            }}
          />
        </div>
      {/if}
    </div>
  </details>
{/if}

<!-- `loraIdentity`, not `caps.loras`: the two engines that take LoRAs take DIFFERENT things, and
     this fold's other two rows are generation state that only the structured engine has. -->
{#if caps.loraIdentity === 'path' && ltxLoras.length}
  <details class="fold">
    <summary
      >Runtime LoRAs · {clip.loras?.length
        ? loraSummary(clip.loras, true, ltxLoras)
        : 'inherited'}<span class="chev" aria-hidden="true">›</span></summary
    >
    <div class="fbody">
      <p class="hint">
        Ticking here REPLACES the project set — it is not additive. Untick everything to inherit
        again: "this {unit} runs with none while the project has some" is the one state the engine's per-segment
        override cannot express.
      </p>
      <div class="mlist">
        {#each ltxLoras as l (l.path)}
          {@const on = hasLora(clip.loras, l.path)}
          {@const inherited = !clip.loras?.length && hasLora(projectLtxLoras, l.path)}
          <button
            class="mrow"
            type="button"
            aria-pressed={on}
            onclick={() => {
              // `undefined` is the ONE representation of inherit — an emptied override must not
              // persist as `[]`, which compiles to the same bytes and would claim otherwise.
              clip.loras = normaliseLoraOverride(toggleLora(clip.loras, l.path, ltxLoras));
              onchange();
            }}
          >
            <span class="gl" aria-hidden="true">{on ? '☑' : '☐'}</span>
            <!-- Label AND file name: the label reads, the file name is the string the engine 400s
                 on when it goes stale. An undescribed adapter labels itself, so it is not twice. -->
            <span class="tx">
              <b>{l.label}</b>
              <span
                >{l.path !== l.label ? l.path : ''}{inherited
                  ? `${l.path !== l.label ? ' · ' : ''}inherited`
                  : ''}</span
              >
            </span>
            <span class="ch" aria-hidden="true">{on ? '✓' : ''}</span>
          </button>
          {#if on}
            <div class="field">
              <span>{l.label} strength</span>
              <input
                class="inp"
                type="number"
                inputmode="decimal"
                min={LORA_STRENGTH_MIN}
                max={LORA_STRENGTH_MAX}
                step={LORA_STRENGTH_STEP}
                value={loraStrength(clip.loras, l.path)}
                oninput={(e) => {
                  clip.loras = setLoraStrength(clip.loras, l.path, Number(e.currentTarget.value));
                  onchange();
                }}
              />
            </div>
          {/if}
        {/each}
      </div>
      {#if loraNote}<p class="hint">Renders with: {loraNote}</p>{/if}
    </div>
  </details>
{/if}

{#if doc}
  <details class="fold">
    <summary
      >{caps.loraIdentity === 'filename' ? 'LoRAs · steps · canvas' : 'Steps · canvas'}
      <span class="chev" aria-hidden="true">›</span></summary
    >
    <div class="fbody">
      {#if caps.loraIdentity === 'filename'}
        <div class="field">
          <span>Adapter</span>
          <input
            class="inp"
            list={listId}
            value={lora}
            placeholder={projectLoras[0] ?? 'none'}
            onchange={(e) => {
              const v = e.currentTarget.value.trim();
              onupdate({ loras: v ? [v] : [], lora_multipliers: v ? [loraMul] : [] });
            }}
          />
          <datalist id={listId}>
            {#each loraCatalogue?.loras ?? [] as entry (entry.name)}
              <option value={entry.name}></option>
            {/each}
          </datalist>
        </div>
        {#if loraCatalogue && (loraCatalogue.loras?.length ?? 0) === 0}
          <p class="hint warn">
            The engine's LoRA directory is empty, which on this box means the volume is not mounted
            — every named adapter is refused however well spelled.
          </p>
        {/if}
        {#if lora}
          <div class="field">
            <span>Strength</span>
            <input
              class="inp"
              inputmode="decimal"
              value={loraMul}
              onchange={(e) => {
                const v = Number(e.currentTarget.value);
                if (Number.isFinite(v)) onupdate({ lora_multipliers: [v] });
              }}
            />
          </div>
        {/if}
      {/if}
      <div class="field">
        <span>Steps</span>
        <input
          class="inp"
          inputmode="numeric"
          value={doc.steps ?? ''}
          placeholder={projectSteps ? String(projectSteps) : 'project default'}
          onchange={(e) => {
            const v = Number(e.currentTarget.value);
            onupdate({ steps: Number.isFinite(v) && v > 0 ? Math.trunc(v) : undefined });
          }}
        />
      </div>
      {#if caps.canvasPresets}
        <div class="field">
          <span>Canvas for this {unit}</span>
          <select
            class="inp"
            value={doc.width && doc.height ? `${doc.width}x${doc.height}` : ''}
            onchange={(e) => {
              const [w, h] = e.currentTarget.value.split('x').map(Number);
              onupdate(w && h ? { width: w, height: h } : { width: undefined, height: undefined });
            }}
          >
            <option value="">Project default</option>
            {#each caps.canvasPresets as p (p.label)}
              <option value="{p.w}x{p.h}">{p.w} × {p.h}</option>
            {/each}
          </select>
        </div>
        <p class="hint">
          An override, not a required choice. Left on the project default this {unit} renders at
          {projectWidth} × {projectHeight}.
        </p>
      {/if}
    </div>
  </details>
{/if}

{#if doc}
  <details class="fold">
    <summary>Seed <span class="chev" aria-hidden="true">›</span></summary>
    <div class="fbody">
      <input
        class="inp"
        inputmode="numeric"
        value={doc.seed ?? ''}
        placeholder="engine-random"
        onchange={(e) => {
          const v = Number(e.currentTarget.value);
          onupdate({ seed: Number.isFinite(v) && v >= 0 ? Math.trunc(v) : undefined });
        }}
      />
      <p class="hint">
        The same seed with the same prompt draws the same {unit}. Empty means a fresh roll every
        render.
      </p>
    </div>
  </details>
{:else}
  <details class="fold">
    <summary>Seed <span class="chev" aria-hidden="true">›</span></summary>
    <div class="fbody">
      <input
        class="inp"
        inputmode="numeric"
        value={clip.seed ?? ''}
        placeholder="engine-random"
        onchange={(e) => {
          const v = Number(e.currentTarget.value);
          clip.seed = Number.isFinite(v) && v >= 0 ? Math.trunc(v) : undefined;
          onchange();
        }}
      />
    </div>
  </details>
{/if}
