<script lang="ts">
  import {
    mattingHealth,
    mattingRemoveSingle,
    mattingRemoveBatch,
    mediaUrl,
    type MattingRemoveRequest,
    type MattingRemoveResponse,
    type MattingHealth,
    type MattingBackend,
    type MattingBgMode
  } from '$lib/api/client';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import type { Generation } from '$lib/types';
  import { str, num, bool, reattachNote } from '$lib/restore';
  import { onMount } from 'svelte';

  let historyKey = $state(0);
  let restoreNote = $state('');

  // Restore the matting knobs from a saved cutout. The source image can't be
  // re-attached (reattachNote flags that).
  function restore(g: Generation) {
    const m = g.request_meta;
    const bm = str(m, 'bg_mode');
    if (bm !== undefined) bgMode = bm as MattingBgMode;
    const bc = str(m, 'bg_color');
    if (bc !== undefined) bgColor = bc;
    const be = str(m, 'backend');
    if (be !== undefined) backend = be as MattingBackend;
    const se = num(m, 'sensitivity');
    if (se !== undefined) sensitivity = se;
    const mb = num(m, 'mask_blur');
    if (mb !== undefined) maskBlur = mb;
    const mo = num(m, 'mask_offset');
    if (mo !== undefined) maskOffset = mo;
    const rf = bool(m, 'refine');
    if (rf !== undefined) refine = rf;
    const iv = bool(m, 'invert');
    if (iv !== undefined) invert = iv;
    const mk = bool(m, 'mask_only');
    if (mk !== undefined) maskOnly = mk;
    const pr = num(m, 'process_res');
    if (pr !== undefined) {
      overrideRes = true;
      processRes = pr;
    }
    if (be !== undefined || se !== undefined || rf !== undefined || overrideRes)
      showAdvanced = true;
    restoreNote = reattachNote(m);
    error = '';
    notice = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  // Input images — one dropzone, multiple allowed (batch).
  let files = $state<File[]>([]);

  // Up-front controls.
  let bgMode = $state<MattingBgMode>('alpha');
  let bgColor = $state('#ffffff');

  // Advanced (behind a details/expander).
  let showAdvanced = $state(false);
  let backend = $state<MattingBackend>('gpu');
  let sensitivity = $state(1.0);
  let maskBlur = $state(0);
  let maskOffset = $state(0);
  let refine = $state(false);
  let invert = $state(false);
  let maskOnly = $state(false);
  let overrideRes = $state(false);
  let processRes = $state(1024);

  // Async UI state.
  let running = $state(false);
  let error = $state('');
  let notice = $state('');
  let result = $state<MattingRemoveResponse | null>(null);
  let imageUrls = $state<string[]>([]);
  let health = $state<MattingHealth | null>(null);

  onMount(async () => {
    try {
      health = await mattingHealth();
    } catch {
      health = { healthy: false, configured: false, worker_alive: false };
    }
  });

  function addFiles(list: FileList) {
    if (!list || !list.length) return;
    files = [...files, ...Array.from(list)];
  }

  function removeFile(idx: number) {
    files = files.filter((_, i) => i !== idx);
  }

  function buildRequest(): MattingRemoveRequest {
    const req: MattingRemoveRequest = { bg_mode: bgMode };
    if (bgMode === 'color') req.bg_color = bgColor;
    if (showAdvanced) {
      req.backend = backend;
      req.sensitivity = sensitivity;
      req.mask_blur = maskBlur;
      req.mask_offset = maskOffset;
      req.refine = refine;
      req.invert = invert;
      req.mask_only = maskOnly;
      if (overrideRes) req.process_res = processRes;
    }
    return req;
  }

  async function handleRemove() {
    if (!files.length) {
      error = 'add at least one image';
      return;
    }
    running = true;
    error = '';
    notice = '';
    result = null;
    imageUrls = [];
    try {
      const req = buildRequest();
      const resp =
        files.length === 1
          ? await mattingRemoveSingle(files[0], req)
          : await mattingRemoveBatch(files, req);
      imageUrls = resp.results.map((r) => mediaUrl(r.url));
      result = resp;
      notice = `Removed background from ${resp.count} image${resp.count === 1 ? '' : 's'}.`;
      historyKey++;
    } catch (e) {
      error = e instanceof Error ? e.message : 'background removal failed';
    } finally {
      running = false;
    }
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Background Removal</h2>
    {#if health}
      <p class="muted">
        BiRefNet / RMBG-2.0 ·
        {#if !health.configured}<span class="down">not configured</span>
        {:else if health.healthy}<span class="ok">engine up</span>{#if health.backend}
            · {health.backend}{/if}
        {:else}<span class="down">engine unreachable</span>{/if}
      </p>
    {/if}
  </div>

  <!-- Image inputs -->
  <div class="card">
    <h3>Images <span class="hint">— drop one or several (batch)</span></h3>
    {#each files as file, i (file.name + i)}
      <div class="ref-row">
        <span class="ref-name" title={file.name}>{file.name}</span>
        <button class="link-btn" type="button" onclick={() => removeFile(i)}>Remove</button>
      </div>
    {/each}
    <FileDropzone
      accept="image/*"
      multiple
      label={files.length ? 'Add more images' : 'Add images'}
      hint="drop one or several — PNG / JPEG / WebP"
      onFiles={addFiles}
    />
  </div>

  <!-- Up-front controls -->
  <div class="card">
    <h3>Output</h3>
    <div class="knobs">
      <label>
        Background
        <select bind:value={bgMode}>
          <option value="alpha">Transparent (alpha cutout)</option>
          <option value="color">Solid colour</option>
        </select>
      </label>
      {#if bgMode === 'color'}
        <label>
          Colour
          <input type="color" bind:value={bgColor} />
        </label>
      {/if}
    </div>

    <details bind:open={showAdvanced} class="advanced">
      <summary>Advanced</summary>
      <div class="knobs">
        <label>
          Backend
          <select bind:value={backend}>
            <option value="gpu">GPU</option>
            <option value="cpu">CPU</option>
          </select>
        </label>
        <label>
          Sensitivity <span class="muted">{sensitivity.toFixed(2)}</span>
          <input type="range" min="0" max="1" step="0.05" bind:value={sensitivity} />
          <span class="hint">mask strength / threshold</span>
        </label>
        <label>
          Mask blur <span class="hint">(px)</span>
          <input type="number" min="0" max="64" bind:value={maskBlur} />
        </label>
        <label>
          Mask offset <span class="hint">(px: dilate + / erode −)</span>
          <input type="number" min="-64" max="64" bind:value={maskOffset} />
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={refine} /> Refine edges
          <span class="hint">(edge decontamination)</span>
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={invert} /> Invert mask
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={maskOnly} /> Mask only
          <span class="hint">(return the grayscale mask)</span>
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={overrideRes} /> Override resolution
        </label>
        {#if overrideRes}
          <label>
            Process res <span class="hint">(256–2048; native 1024)</span>
            <input type="number" min="256" max="2048" step="64" bind:value={processRes} />
          </label>
        {/if}
      </div>
    </details>
  </div>

  <div class="actions">
    <button class="btn-primary" onclick={handleRemove} disabled={running || !files.length}>
      {running ? 'Processing…' : 'Remove background'}
    </button>
    {#if notice && !error}<span class="muted">{notice}</span>{/if}
    {#if error}<span class="error">{error}</span>{/if}
  </div>

  {#if restoreNote}<p class="restore-note">{restoreNote}</p>{/if}

  {#if result}
    <div class="card">
      <h3>Result</h3>
      <div class="meta">
        <span>{result.count} image{result.count === 1 ? '' : 's'}</span>
        {#if result.backend}<span>· {result.backend}</span>{/if}
        {#if result.elapsed_seconds != null}<span>· {result.elapsed_seconds.toFixed(2)}s</span>{/if}
      </div>
      <div class="gallery">
        {#each result.results as _r, i (i)}
          <div class="shot">
            {#if imageUrls[i]}
              <img src={imageUrls[i]} alt={`Cutout ${i + 1}`} />
            {/if}
            <div class="shot-foot">
              <span class="muted">#{i + 1}</span>
              {#if imageUrls[i]}
                <a class="btn-sec" href={imageUrls[i]} download={`cutout-${i + 1}.png`}>Download</a>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  <GenerationHistory
    service="matting"
    refreshKey={historyKey}
    title="Cutout history"
    onRestore={restore}
  />
</div>

<style>
  .card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 20px;
    margin-bottom: 16px;
  }
  .card h3 {
    margin: 0 0 12px;
    font-size: 15px;
  }
  input,
  select {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font-family: inherit;
    font-size: 13px;
  }
  input[type='color'] {
    padding: 2px;
    height: 36px;
  }
  label {
    display: block;
    font-size: 12px;
    color: var(--muted);
    margin-top: 8px;
  }
  label.toggle {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    color: var(--ink);
    font-size: 13px;
  }
  label.toggle input {
    width: auto;
  }
  .hint {
    color: var(--muted);
    font-weight: normal;
    font-size: 11px;
  }
  .ref-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 10px;
    margin-bottom: 6px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    font-size: 12px;
    color: var(--ink);
  }
  .ref-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .link-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    font-size: 12px;
    text-decoration: underline;
  }
  .advanced {
    margin-top: 14px;
    border-top: 1px solid var(--ring);
    padding-top: 10px;
  }
  .advanced summary {
    cursor: pointer;
    font-size: 13px;
    color: var(--muted);
  }
  .knobs {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px 16px;
    align-items: end;
  }
  .actions {
    margin: 16px 0;
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }
  .btn-primary {
    background: var(--accent);
    color: white;
    border: none;
    padding: 12px 28px;
    border-radius: 10px;
    font-size: 15px;
    font-weight: 600;
    cursor: pointer;
  }
  .btn-primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .btn-sec {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
    padding: 8px 12px;
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
    text-decoration: none;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
  }
  .restore-note {
    margin: 10px 0 0;
    padding: 8px 12px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-2);
    color: var(--muted);
    font-size: 13px;
  }
  .meta {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
    margin-bottom: 12px;
    flex-wrap: wrap;
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
  }
  .gallery {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 14px;
  }
  .shot {
    border: 1px solid var(--ring);
    border-radius: 12px;
    overflow: hidden;
    /* checkerboard so transparent cutouts read clearly */
    background-color: var(--bg-2);
    background-image:
      linear-gradient(45deg, rgba(128, 128, 128, 0.25) 25%, transparent 25%),
      linear-gradient(-45deg, rgba(128, 128, 128, 0.25) 25%, transparent 25%),
      linear-gradient(45deg, transparent 75%, rgba(128, 128, 128, 0.25) 75%),
      linear-gradient(-45deg, transparent 75%, rgba(128, 128, 128, 0.25) 75%);
    background-size: 20px 20px;
    background-position:
      0 0,
      0 10px,
      10px -10px,
      -10px 0;
  }
  .shot img {
    display: block;
    width: 100%;
    height: auto;
  }
  .shot-foot {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 10px;
    background: var(--card);
  }
  .shot-foot .btn-sec {
    margin-left: auto;
  }
</style>
