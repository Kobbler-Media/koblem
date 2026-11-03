<script lang="ts">
  import { PREVIEW_TIERS, type SizeTier } from '#lib/director/resolution.ts';
  import { capabilities, checkCanvas, type DirectorEngine } from '#lib/director/engine.ts';

  // The three settings that change what a phone actually SEES — canvas, frame rate, and what a
  // preview means on this engine — with the hint under each one live. Everything else (LoRAs, steps,
  // seed, the custom recipe ladder, the critic) stays in the desktop sheet, which is one row away
  // rather than transcribed narrower and half-complete.
  let {
    engine,
    engineLabel,
    engineBlurb,
    fps = $bindable(24),
    variableFps,
    fixedFpsNote,
    sizeSel,
    sizeTiers,
    outW,
    outH,
    snapNote = null,
    previewFullRes,
    previewSel,
    previewW,
    previewH,
    onsize,
    onpreviewsize,
    onchange,
    onopenfull
  }: {
    engine: DirectorEngine;
    engineLabel: string;
    engineBlurb: string;
    fps?: number;
    variableFps: boolean;
    fixedFpsNote: string;
    sizeSel: string;
    sizeTiers: SizeTier[];
    outW: number;
    outH: number;
    snapNote?: string | null;
    previewFullRes: boolean;
    previewSel: string;
    previewW: number;
    previewH: number;
    onsize: (key: string) => void;
    onpreviewsize: (key: string) => void;
    onchange: () => void;
    onopenfull: () => void;
  } = $props();

  const canvas = $derived(checkCanvas(engine, outW, outH));

  /**
   * What is actually behind "All render settings…" ON THIS ENGINE.
   *
   * 🔴 It named the negative prompt and the quality ladder unconditionally, and both are
   * capability-gated — the desktop sheet hides them on a distilled checkpoint and on H3. A phone
   * user following the promise found a sheet without them, which reads as a broken link.
   */
  const fullSheetHolds = $derived.by(() => {
    const caps = capabilities(engine);
    const items = [
      ...(caps.loras ? ['LoRAs'] : []),
      'steps',
      'seed',
      ...(caps.negativePrompt ? ['the negative prompt'] : []),
      ...(caps.upscaleChain ? ['the quality ladder'] : []),
      'the dailies critic'
    ];
    return `${items.slice(0, -1).join(', ')} and ${items[items.length - 1]}.`;
  });
</script>

<div class="sheetbody">
  <div class="grp">
    <p class="eye">Engine</p>
    <div class="framestate">
      <span class="fs-ico" aria-hidden="true">⌁</span>
      <span class="fs-tx"><b>{engineLabel}</b><span>{outW}×{outH} · {fps} fps</span></span>
    </div>
    <p class="hint">{engineBlurb}</p>
  </div>

  <div class="grp">
    <p class="eye">Canvas</p>
    <label class="field"
      ><span>Size</span>
      <select class="inp" value={sizeSel} onchange={(e) => onsize(e.currentTarget.value)}>
        {#each sizeTiers as t (t.key)}
          <option value={t.key}>{t.w} × {t.h}</option>
        {/each}
        <!-- 🔴 Offered only to DESCRIBE a size the project already has. Choosing it latches the
             custom mode whose W/H inputs live in the desktop sheet, so as a choice it would dirty
             the project, change nothing, and leave the select stuck reading Custom. -->
        {#if sizeSel === 'custom'}<option value="custom">Custom…</option>{/if}
      </select>
    </label>
    {#if sizeSel === 'custom'}
      <p class="hint">
        A custom canvas is entered under all render settings, below — this list holds the tiers the
        engine snaps to.
      </p>
    {/if}
    <!-- The list is bare numbers; the aspect and the reason live down here, where a line can wrap. -->
    <p class="hint" class:bad={!!canvas.error} class:warn={!canvas.error && !!canvas.warning}>
      {canvas.error ??
        canvas.warning ??
        `${sizeTiers.find((t) => t.key === sizeSel)?.label ?? 'Custom'} — ${outW}×${outH}, ${(outW / outH).toFixed(2)}:1.`}
    </p>
    {#if snapNote}<p class="hint warn">{snapNote}</p>{/if}
    <label class="field"
      ><span>Frame rate</span>
      <input
        class="inp"
        type="number"
        inputmode="numeric"
        bind:value={fps}
        disabled={!variableFps}
        {onchange}
      />
    </label>
    {#if !variableFps}<p class="hint">{fixedFpsNote}</p>{/if}
  </div>

  <div class="grp">
    <p class="eye">Preview render</p>
    {#if previewFullRes}
      <p class="hint">
        A preview holds the full {previewW}×{previewH} canvas and cuts the steps. A smaller canvas is
        a different latent shape, so it draws a different composition and a different music bed — the
        preview is the final one under-converged, not a small version of it.
      </p>
    {:else}
      <label class="field"
        ><span>Preview size</span>
        <select
          class="inp"
          value={previewSel}
          onchange={(e) => onpreviewsize(e.currentTarget.value)}
        >
          {#each PREVIEW_TIERS as t (t.key)}
            <option value={t.key}>{t.label} · {t.w} × {t.h}</option>
          {/each}
          {#if previewSel === 'custom'}<option value="custom">Custom…</option>{/if}
        </select>
      </label>
      <p class="hint">
        Previews render at {previewW}×{previewH} and stream at a fraction of a final take. Final renders
        are untouched by this.{previewSel === 'custom'
          ? ' Those dimensions match no tier, so they are edited under all render settings, below.'
          : ''}
      </p>
    {/if}
  </div>

  <button class="btn full" type="button" onclick={onopenfull}>All render settings…</button>
  <p class="hint">{fullSheetHolds}</p>
</div>
