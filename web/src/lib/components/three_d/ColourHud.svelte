<script lang="ts">
  /*
   * The colour grade, floating over the model it is grading.
   *
   * 🔴 Deliberately NOT a "clean up texture" button. The engine's baker already dilates the gutter
   * and inpaints interior holes, and re-dilating in the browser reintroduces a chart-bleed seam it
   * caps its own fill to avoid (see the header of `texture-repair.ts`). Colour is the one thing the
   * baker will never do, because it is faithful to the source photo on purpose.
   *
   * 🔴 CONTRAST AND SATURATION ARE IDENTITY AT 1, NOT 0. `ColourAdjust` is a multiplier pair, and
   * the panel this replaces initialised them to zero — which asks `adjustColour` for a flat grey and
   * gets one, from the commit and the GPU preview alike. The sliders below therefore carry the real
   * `ColourAdjust` values and rest at `IDENTITY_ADJUST`; there is no second scale to convert between
   * and therefore no conversion to get wrong.
   *
   * While a slider moves, the model is graded on the GPU by `attachLiveGrade`, which compiles the
   * same constants and the same operation order the commit uses. Apply then repaints the texture
   * with the exact 4.19 M-texel CPU pass and marks the edit for the save.
   */
  import { IDENTITY_ADJUST, type ColourAdjust } from './texture-repair';

  let {
    adjust = $bindable(),
    applied,
    busy = '',
    hasMap,
    onInput,
    onApply,
    onRevert
  }: {
    /** The live `ColourAdjust`, in `adjustColour`'s own units. Identity is `IDENTITY_ADJUST`. */
    adjust: Required<ColourAdjust>;
    /** Has the CPU pass already been baked into the texture? */
    applied: boolean;
    busy?: string;
    /** False when the material has no baseColor map, or has more than one material slot. */
    hasMap: boolean;
    /** Fired on every slider move, before the value is read — lets the owner drop a committed bake
     *  back to the delivered pixels so the preview never grades already-graded texels. */
    onInput: () => void;
    onApply: () => void;
    onRevert: () => void;
  } = $props();

  const touched = $derived(
    adjust.exposure !== IDENTITY_ADJUST.exposure ||
      adjust.contrast !== IDENTITY_ADJUST.contrast ||
      adjust.saturation !== IDENTITY_ADJUST.saturation ||
      adjust.temperature !== IDENTITY_ADJUST.temperature
  );

  function reset() {
    adjust = { ...IDENTITY_ADJUST };
    onInput();
  }
</script>

<div class="hud">
  <div class="head">
    <span class="title">Colour</span>
    {#if applied}
      <span class="flag ok">baked — save to keep it</span>
    {:else if touched}
      <span class="flag">preview only</span>
    {/if}
  </div>

  {#if !hasMap}
    <p class="hint">
      This body has no single baseColor map to grade — a multi-material character would need a slot
      picker and a per-slot patch, so the control does not offer itself here.
    </p>
  {:else}
    <label class="knob">
      <span class="k">Exposure</span>
      <input
        type="range"
        min="-2"
        max="2"
        step="0.05"
        bind:value={adjust.exposure}
        oninput={onInput}
      />
      <b>{adjust.exposure.toFixed(2)}</b>
    </label>
    <label class="knob">
      <span class="k">Contrast</span>
      <input
        type="range"
        min="0"
        max="2"
        step="0.05"
        bind:value={adjust.contrast}
        oninput={onInput}
      />
      <b>{adjust.contrast.toFixed(2)}</b>
    </label>
    <label class="knob">
      <span class="k">Saturation</span>
      <input
        type="range"
        min="0"
        max="2"
        step="0.05"
        bind:value={adjust.saturation}
        oninput={onInput}
      />
      <b>{adjust.saturation.toFixed(2)}</b>
    </label>
    <label class="knob">
      <span class="k">Warmth</span>
      <input
        type="range"
        min="-1"
        max="1"
        step="0.05"
        bind:value={adjust.temperature}
        oninput={onInput}
      />
      <b>{adjust.temperature.toFixed(2)}</b>
    </label>

    <div class="row">
      <button class="btn-sec small" onclick={onApply} disabled={!!busy || !touched || applied}>
        {busy || 'Apply to the texture'}
      </button>
      {#if applied}
        <button class="btn-sec small" onclick={onRevert}>Put the colour back</button>
      {:else if touched}
        <button class="btn-sec small" onclick={reset}>Reset</button>
      {/if}
    </div>
    <p class="hint">
      The preview is on the GPU and follows the slider. Apply runs the exact grade over all 4.19
      million texels once — that is the version that gets saved. The texture is the source photo,
      projected: if it came out dark, that is the photo, and the engine will never "correct" it.
    </p>
  {/if}
</div>

<style>
  .hud {
    width: 268px;
    box-sizing: border-box;
    padding: 10px 12px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 10px 30px rgb(0 0 0 / 45%);
    pointer-events: auto;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    justify-content: space-between;
    margin-bottom: 6px;
  }
  .title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .flag {
    font-size: 10px;
    color: var(--warning, #d9952d);
  }
  .flag.ok {
    color: var(--ok, #4ade80);
  }
  .knob {
    display: grid;
    grid-template-columns: 68px 1fr 38px;
    align-items: center;
    gap: 8px;
    margin-top: 5px;
    font-size: 11px;
    color: var(--muted);
  }
  .knob .k {
    white-space: nowrap;
  }
  .knob b {
    color: var(--ink);
    font-size: 11px;
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  .knob input[type='range'] {
    width: 100%;
    min-width: 0;
  }
  .row {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  .btn-sec.small {
    padding: 3px 9px;
    font-size: 11px;
  }
  .hint {
    color: var(--muted);
    font-size: 10.5px;
    line-height: 1.4;
    margin: 8px 0 0;
  }
</style>
