<script lang="ts">
  /*
   * Normal-map strength, floating over the model it is weakening.
   *
   * 🔴 The slider is `material.normalScale`, NOT the pixels. three's normal chunk does
   * `mapN.xy *= normalScale` and then renormalises, so scaling there is the same operation for
   * free, at full float precision, live, and reversible. `setNormalStrength` exists for one case
   * only — the weakening has to survive into a viewer that reads the image and ignores the
   * material — and that is what the bake button is, a deliberate press, not the default.
   *
   * 🔴 The two must never both be in effect. The bake writes weakened texels, so the owner has to
   * put `normalScale` back to 1 in the same breath or the model shows the strength SQUARED — the
   * same double-apply trap `ColourHud` has with its GPU preview, and the same division of labour
   * answers it. This panel owns the number and nothing else: the owner writes `normalScale`, runs
   * the bake off the delivered pixels, and owns the save.
   *
   * ⚠️ ONE scalar, driving both components of the `Vector2`. `normalScale.y < 0` is the
   * DirectX/OpenGL green-channel flip and glTF mandates the OpenGL convention, so a negative value
   * here would be a convention swap wearing a strength slider's clothes; `x !== y` is an
   * anisotropic bump nobody asked for. Neither is offered.
   */
  let {
    strength = $bindable(),
    applied,
    busy = '',
    hasMap,
    onInput,
    onApply,
    onRevert
  }: {
    /** Normal detail multiplier. 1 = the map as delivered, 0 = flat, >1 exaggerates. */
    strength: number;
    /** Have the weakened texels already been written into the image? */
    applied: boolean;
    busy?: string;
    /** False when the material has no normal map, or has more than one material slot. */
    hasMap: boolean;
    /** Fired on every slider move, before the value is read — lets the owner drop a committed bake
     *  back to the delivered pixels, so a live `normalScale` never scales already-weakened texels. */
    onInput: () => void;
    onApply: () => void;
    onRevert: () => void;
  } = $props();

  /** The map as the baker delivered it. */
  const DELIVERED = 1;

  const touched = $derived(strength !== DELIVERED);

  function reset() {
    strength = DELIVERED;
    onInput();
  }
</script>

<div class="hud">
  <div class="head">
    <span class="title">Normal</span>
    {#if applied}
      <span class="flag ok">baked — save to keep it</span>
    {:else if touched}
      <span class="flag">not in the file</span>
    {/if}
  </div>

  {#if !hasMap}
    <p class="hint">
      This body has no single normal map to weaken — a multi-material character would need a slot
      picker and a per-slot patch, so the control does not offer itself here.
    </p>
  {:else}
    <label class="knob">
      <span class="k">Strength</span>
      <input type="range" min="0" max="2" step="0.05" bind:value={strength} oninput={onInput} />
      <b>{strength.toFixed(2)}</b>
    </label>

    <div class="row">
      <button class="btn-sec small" onclick={onApply} disabled={!!busy || !touched || applied}>
        {busy || 'Bake into the file'}
      </button>
      {#if applied}
        <button class="btn-sec small" onclick={onRevert}>Put the detail back</button>
      {:else if touched}
        <button class="btn-sec small" onclick={reset}>Reset</button>
      {/if}
    </div>
    <p class="hint">
      The slider is the shader's own normal scale: free, full precision, and exactly flat at 0. Bake
      only when the file itself has to carry the weakening — 8 bits cannot encode a flat normal
      (zero falls between codes 127 and 128), so a baked flatten keeps a faint uniform lighting tilt
      the slider does not, and baking above 1 clips the strongest texels.
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
