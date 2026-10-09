<script lang="ts">
  import {
    visionEmbed,
    visionClassify,
    visionTextEmbed,
    visionClassifyFromEmbeddings,
    getHealth,
    getSettings
  } from '$lib/api/client';
  import type {
    VisionEmbedResponse,
    VisionClassifyResponse,
    VisionTextEmbedResponse
  } from '$lib/api/client';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import JsonBlock from '$lib/components/JsonBlock.svelte';
  import { onMount } from 'svelte';

  /// 'embed' / 'classify' take image uploads; 'text-embed' takes prompts;
  /// 'classify-from-embeddings' takes pasted embedding JSON + prompts — useful
  /// for probing the SigLIP cache without re-running the image encoder.
  type Mode = 'embed' | 'classify' | 'text-embed' | 'classify-from-embeddings';
  let mode = $state<Mode>('embed');
  let files = $state<FileList | null>(null);

  // Knobs
  let maxNumPatches = $state(729);
  let pooling = $state<'pooler' | 'cls' | 'mean'>('pooler');
  let returnLastHidden = $state(false);
  let returnLogits = $state(false);
  let promptsText = $state('credits\ntitle card\ndialogue scene');
  let saveSample = $state(true);
  /// Pasted JSON — either a full embed response `{embeddings: [...]}` or a
  /// bare array `[[...], [...]]`. Parsed lazily on Run.
  let embeddingsJson = $state('');
  const embeddingsPlaceholder =
    'e.g. {"embeddings": [[0.1, -0.2, …]]} — copied from an /embed Download JSON';

  let running = $state(false);
  let embedResult = $state<VisionEmbedResponse | null>(null);
  let classifyResult = $state<VisionClassifyResponse | null>(null);
  let textEmbedResult = $state<VisionTextEmbedResponse | null>(null);
  let error = $state('');
  let model = $state('');

  onMount(async () => {
    const [healthRes, settingsRes] = await Promise.allSettled([getHealth(), getSettings()]);
    if (healthRes.status === 'fulfilled') model = healthRes.value.vision_model || '';
    if (settingsRes.status === 'fulfilled' && settingsRes.value.gpu_sample_save_default != null) {
      saveSample = Boolean(settingsRes.value.gpu_sample_save_default);
    }
  });

  function parsePrompts(): string[] {
    return promptsText
      .split('\n')
      .map((s) => s.trim())
      .filter(Boolean);
  }

  function parseEmbeddingsJson(): number[][] {
    const raw = embeddingsJson.trim();
    if (!raw) throw new Error('paste embeddings JSON');
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch (e) {
      throw new Error(`invalid JSON: ${e instanceof Error ? e.message : e}`, { cause: e });
    }
    // Accept either a bare 2D array or a `{embeddings: [...]}` object (matches
    // the shape you get from Download JSON on an /embed run).
    const arr = Array.isArray(parsed)
      ? parsed
      : parsed && typeof parsed === 'object' && 'embeddings' in parsed
        ? (parsed as { embeddings: unknown }).embeddings
        : null;
    if (!Array.isArray(arr) || !arr.length || !Array.isArray(arr[0])) {
      throw new Error('expected 2D array or {embeddings: [[...]]}');
    }
    return arr as number[][];
  }

  async function handleRun() {
    running = true;
    error = '';
    embedResult = null;
    classifyResult = null;
    textEmbedResult = null;

    try {
      if (mode === 'embed' || mode === 'classify') {
        if (!files || !files.length) {
          throw new Error('pick at least one image');
        }
        const fileArr = Array.from(files);
        if (mode === 'embed') {
          embedResult = await visionEmbed(fileArr, {
            max_num_patches: maxNumPatches,
            pooling,
            return_last_hidden: returnLastHidden,
            save_sample: saveSample
          });
        } else {
          const prompts = parsePrompts();
          if (!prompts.length) throw new Error('need at least one prompt');
          classifyResult = await visionClassify(fileArr, {
            prompts,
            max_num_patches: maxNumPatches,
            return_logits: returnLogits,
            save_sample: saveSample
          });
        }
      } else if (mode === 'text-embed') {
        const prompts = parsePrompts();
        if (!prompts.length) throw new Error('need at least one prompt');
        textEmbedResult = await visionTextEmbed(prompts);
      } else {
        const prompts = parsePrompts();
        if (!prompts.length) throw new Error('need at least one prompt');
        const image_embeddings = parseEmbeddingsJson();
        classifyResult = await visionClassifyFromEmbeddings({
          image_embeddings,
          prompts,
          return_logits: returnLogits
        });
      }
    } catch (e) {
      error = e instanceof Error ? e.message : 'Run failed';
    } finally {
      running = false;
    }
  }

  function downloadJson(data: unknown, name: string) {
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = name;
    a.click();
    URL.revokeObjectURL(url);
  }

  /// Copy an /embed result's embeddings into the classify-from-embeddings
  /// textarea and switch mode — saves a round-trip through Download/paste.
  function useAsCachedEmbeddings() {
    if (!embedResult) return;
    embeddingsJson = JSON.stringify({ embeddings: embedResult.embeddings }, null, 2);
    mode = 'classify-from-embeddings';
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Vision Playground</h2>
    {#if model}
      <p class="muted">Model: <code>{model}</code></p>
    {/if}
  </div>

  <div class="card">
    <div class="row">
      <label class="toggle">
        <input type="radio" bind:group={mode} value="embed" />
        /v1/embeddings
      </label>
      <label class="toggle">
        <input type="radio" bind:group={mode} value="classify" />
        /v1/classify
      </label>
      <label class="toggle">
        <input type="radio" bind:group={mode} value="text-embed" />
        /v1/text_embeddings
      </label>
      <label class="toggle">
        <input type="radio" bind:group={mode} value="classify-from-embeddings" />
        /v1/classify_from_embeddings
      </label>
    </div>

    {#if mode === 'embed' || mode === 'classify'}
      <FileDropzone
        accept="image/*"
        multiple
        bind:files
        label="Drop images here"
        hint="or click to browse (multiple OK)"
      />
    {/if}

    {#if mode === 'classify' || mode === 'text-embed' || mode === 'classify-from-embeddings'}
      <label>
        Prompts (one per line)
        <textarea rows="4" bind:value={promptsText}></textarea>
      </label>
    {/if}

    {#if mode === 'classify-from-embeddings'}
      <label>
        Image embeddings (paste 2D array or <code>&lbrace;embeddings: [...]&rbrace;</code>)
        <textarea
          rows="8"
          class="embeddings-input"
          placeholder={embeddingsPlaceholder}
          bind:value={embeddingsJson}></textarea>
      </label>
    {/if}
  </div>

  <div class="card">
    <h3>Knobs</h3>
    <div class="knobs">
      {#if mode === 'embed' || mode === 'classify'}
        <label>
          max_num_patches
          <input type="range" min="64" max="2048" step="32" bind:value={maxNumPatches} />
          <span class="muted">{maxNumPatches} patches</span>
        </label>
      {/if}
      {#if mode === 'embed'}
        <label>
          pooling
          <select bind:value={pooling}>
            <option value="pooler">pooler (default)</option>
            <option value="cls">cls token</option>
            <option value="mean">mean (attention-weighted)</option>
          </select>
        </label>
        <label class="toggle">
          <input type="checkbox" bind:checked={returnLastHidden} />
          return last_hidden_state (large)
        </label>
      {/if}
      {#if mode === 'classify' || mode === 'classify-from-embeddings'}
        <label class="toggle">
          <input type="checkbox" bind:checked={returnLogits} />
          return raw logits
        </label>
      {/if}
      {#if mode === 'embed' || mode === 'classify'}
        <label class="toggle">
          <input type="checkbox" bind:checked={saveSample} />
          save sample for replay
        </label>
      {/if}
      {#if mode === 'text-embed'}
        <span class="muted small">No knobs — text encoder has no patch/pooling config.</span>
      {/if}
    </div>
  </div>

  <div class="actions">
    <button class="btn-primary" onclick={handleRun} disabled={running}>
      {running ? 'Running…' : 'Run'}
    </button>
    {#if error}<span class="error">{error}</span>{/if}
  </div>

  {#if embedResult}
    <div class="card">
      <h3>Embeddings</h3>
      <div class="meta">
        <span
          >shape: [{embedResult.embeddings.length}, {embedResult.embeddings[0]?.length ?? 0}]</span
        >
        {#if embedResult.duration_ms != null}<span>· {embedResult.duration_ms} ms</span>{/if}
        {#if embedResult.gate_wait_ms != null}<span>· gate wait {embedResult.gate_wait_ms} ms</span
          >{/if}
      </div>
      <div class="emb-preview">
        {#each embedResult.embeddings as emb, i (i)}
          <details>
            <summary>Image {i + 1} — first 16 dims</summary>
            <code
              >[{emb
                .slice(0, 16)
                .map((v) => v.toFixed(4))
                .join(', ')}, …]</code
            >
          </details>
        {/each}
      </div>
      <div class="result-actions">
        <button class="btn-sec" onclick={() => downloadJson(embedResult, 'embeddings.json')}>
          Download JSON
        </button>
        <button class="btn-sec" onclick={useAsCachedEmbeddings}>
          Use as cached embeddings →
        </button>
      </div>
    </div>
  {/if}

  {#if textEmbedResult}
    <div class="card">
      <h3>Text embeddings</h3>
      <div class="meta">
        <span
          >shape: [{textEmbedResult.embeddings.length}, {textEmbedResult.embeddings[0]?.length ??
            0}]</span
        >
        {#if textEmbedResult.duration_ms != null}
          <span>· {textEmbedResult.duration_ms} ms</span>
        {/if}
        {#if textEmbedResult.gate_wait_ms != null}
          <span>· gate wait {textEmbedResult.gate_wait_ms} ms</span>
        {/if}
      </div>
      <div class="emb-preview">
        {#each textEmbedResult.embeddings as emb, i (i)}
          <details>
            <summary>"{parsePrompts()[i] ?? `prompt ${i + 1}`}" — first 16 dims</summary>
            <code
              >[{emb
                .slice(0, 16)
                .map((v) => v.toFixed(4))
                .join(', ')}, …]</code
            >
          </details>
        {/each}
      </div>
      <button class="btn-sec" onclick={() => downloadJson(textEmbedResult, 'text_embeddings.json')}>
        Download JSON
      </button>
    </div>
  {/if}

  {#if classifyResult}
    <div class="card">
      <h3>Classification scores</h3>
      <div class="meta">
        {#if classifyResult.duration_ms != null}<span>{classifyResult.duration_ms} ms</span>{/if}
        {#if classifyResult.gate_wait_ms != null}<span
            >gate wait {classifyResult.gate_wait_ms} ms</span
          >{/if}
      </div>
      <table>
        <thead>
          <tr>
            <th>Image</th>
            {#each parsePrompts() as p, i (i)}
              <th>{p}</th>
            {/each}
          </tr>
        </thead>
        <tbody>
          {#each classifyResult.scores as row, i (i)}
            <tr>
              <td>Image {i + 1}</td>
              {#each row as v, j (j)}
                <td>{v.toFixed(4)}</td>
              {/each}
            </tr>
          {/each}
        </tbody>
      </table>
      {#if classifyResult.logits}
        <details>
          <summary>Raw logits</summary>
          <JsonBlock content={classifyResult.logits} label="Logits" />
        </details>
      {/if}
      <button class="btn-sec" onclick={() => downloadJson(classifyResult, 'classify.json')}>
        Download JSON
      </button>
    </div>
  {/if}
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
  select,
  textarea {
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
  .row {
    display: flex;
    gap: 16px;
    margin-bottom: 12px;
  }
  .knobs {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px 16px;
  }
  .actions {
    margin: 16px 0;
    display: flex;
    gap: 12px;
    align-items: center;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
  }
  .meta {
    font-size: 12px;
    color: var(--muted);
    display: flex;
    gap: 8px;
    margin-bottom: 8px;
    flex-wrap: wrap;
  }
  .emb-preview {
    margin: 12px 0;
    font-size: 12px;
  }
  .emb-preview code {
    display: block;
    padding: 4px 8px;
    background: var(--bg-2);
    border-radius: 6px;
    font-family: monospace;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    margin: 12px 0;
    font-size: 13px;
  }
  th,
  td {
    text-align: left;
    padding: 6px 8px;
    border-bottom: 1px solid var(--ring);
  }
  .embeddings-input {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px;
    min-height: 120px;
  }
  .result-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .muted.small {
    font-size: 12px;
  }
</style>
