<script lang="ts">
  import {
    listGenerations,
    deleteGeneration,
    clearGenerations,
    renameGeneration,
    retitleGeneration,
    generationFileUrl,
    fetchTranscriptText,
    ApiError
  } from '$lib/api/client';
  import type { Generation, GenerationOutput } from '$lib/types';

  const PAGE = 50;

  let {
    service,
    refreshKey = 0,
    title = 'History',
    onRestore
  }: {
    service: string;
    refreshKey?: number;
    title?: string;
    // When provided, each card shows a "Use these settings" button that hands the
    // generation back to the page so it can repopulate its form from request_meta.
    onRestore?: (g: Generation) => void;
  } = $props();

  let items = $state<Generation[]>([]);
  let total = $state(0);
  let loading = $state(false);
  let loadingMore = $state(false);
  let clearing = $state(false);
  let error = $state('');
  let deleting = $state<string | null>(null);
  let transcripts = $state<Record<string, string>>({});
  let editingId = $state<string | null>(null);
  let editValue = $state('');
  let savingTitle = $state(false);
  let retitling = $state<string | null>(null);

  let hasMore = $derived(items.length < total);
  // A native <audio> loses its seek bar below ~350px, so an audio history gets a wider grid track
  // (≈3 across) where a thumbnail grid stays dense.
  let cardMin = $derived(
    items.some((g) => g.outputs.some((o) => o.kind === 'audio')) ? '420px' : '240px'
  );
  let lastLoaded = '';

  // Reset to the first page: newest PAGE entries.
  async function load() {
    loading = true;
    error = '';
    try {
      const res = await listGenerations(service, PAGE, 0);
      items = res.items;
      total = res.total;
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Failed to load history';
    } finally {
      loading = false;
    }
  }

  // Append the next page of older entries.
  async function loadMore() {
    if (loadingMore || !hasMore) return;
    loadingMore = true;
    try {
      const res = await listGenerations(service, PAGE, items.length);
      // De-dupe in case a new row shifted the window between fetches.
      const seen = new Set(items.map((g) => g.id));
      items = [...items, ...res.items.filter((g) => !seen.has(g.id))];
      total = res.total;
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Failed to load more';
    } finally {
      loadingMore = false;
    }
  }

  // Reload on mount, on service change, and whenever the parent bumps refreshKey
  // after a successful generation.
  $effect(() => {
    const key = `${service}:${refreshKey}`;
    if (key === lastLoaded) return;
    lastLoaded = key;
    load();
  });

  async function remove(id: string) {
    if (deleting) return;
    deleting = id;
    try {
      await deleteGeneration(id);
      items = items.filter((g) => g.id !== id);
      total = Math.max(0, total - 1);
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Delete failed';
    } finally {
      deleting = null;
    }
  }

  function startEdit(g: Generation) {
    editingId = g.id;
    editValue = g.title || '';
  }

  function cancelEdit() {
    editingId = null;
    editValue = '';
  }

  async function saveEdit(id: string) {
    const title = editValue.trim();
    if (!title || savingTitle) {
      cancelEdit();
      return;
    }
    savingTitle = true;
    try {
      const res = await renameGeneration(id, title);
      items = items.map((g) => (g.id === id ? { ...g, title: res.title } : g));
      cancelEdit();
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Rename failed';
    } finally {
      savingTitle = false;
    }
  }

  async function regen(id: string) {
    if (retitling) return;
    retitling = id;
    error = '';
    try {
      const res = await retitleGeneration(id);
      items = items.map((g) => (g.id === id ? { ...g, title: res.title } : g));
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Retitle failed';
    } finally {
      retitling = null;
    }
  }

  async function clearAll() {
    if (clearing || items.length === 0) return;
    if (!confirm(`Delete ALL ${total} ${service} generations and their files?`)) return;
    clearing = true;
    error = '';
    try {
      await clearGenerations(service);
      items = [];
      total = 0;
    } catch (e) {
      error = e instanceof ApiError ? e.message : 'Clear failed';
    } finally {
      clearing = false;
    }
  }

  async function loadTranscript(id: string, index: number) {
    const key = `${id}:${index}`;
    if (transcripts[key] !== undefined) return;
    try {
      transcripts[key] = await fetchTranscriptText(id, index);
    } catch {
      transcripts[key] = '(failed to load transcript)';
    }
  }

  function summarise(g: Generation): string {
    const m = g.request_meta || {};
    for (const k of [
      'prompt',
      'brief',
      'caption',
      'description',
      'lyrics',
      'lyric',
      'script',
      'script_used',
      'idea',
      'input',
      'text',
      'endpoint'
    ]) {
      const v = (m as Record<string, unknown>)[k];
      if (typeof v === 'string' && v.trim()) return v.trim();
    }
    return g.endpoint;
  }

  function when(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
  }

  function ext(o: GenerationOutput): string {
    const dot = o.filename.lastIndexOf('.');
    return dot >= 0 ? o.filename.slice(dot + 1).toUpperCase() : o.kind.toUpperCase();
  }
</script>

<section class="history">
  <header class="hist-head">
    <h3>{title}{total ? ` (${total})` : ''}</h3>
    <div class="hist-actions">
      <button class="btn-sec" onclick={load} disabled={loading}>
        {loading ? 'Loading…' : 'Refresh'}
      </button>
      {#if items.length > 0}
        <button class="btn-del" onclick={clearAll} disabled={clearing}>
          {clearing ? 'Clearing…' : 'Clear all'}
        </button>
      {/if}
    </div>
  </header>

  {#if error}
    <p class="error">{error}</p>
  {/if}

  {#if !loading && items.length === 0}
    <p class="empty">No saved generations yet.</p>
  {/if}

  <div class="hist-grid" style:--card-min={cardMin}>
    {#each items as g (g.id)}
      <article class="card">
        <header class="card-head">
          {#if editingId === g.id}
            <input
              class="title-edit"
              bind:value={editValue}
              disabled={savingTitle}
              onkeydown={(e) => {
                if (e.key === 'Enter') saveEdit(g.id);
                else if (e.key === 'Escape') cancelEdit();
              }}
              onblur={() => saveEdit(g.id)}
              placeholder="Title…"
              aria-label="Rename generation"
            />
          {:else}
            <div class="title-row">
              <button
                class="summary title-btn"
                title={`${g.title || summarise(g)} — click to rename`}
                onclick={() => startEdit(g)}
              >
                {g.title || summarise(g)}
              </button>
              <button
                class="icon-btn"
                title="Regenerate title"
                onclick={() => regen(g.id)}
                disabled={retitling === g.id}
              >
                {retitling === g.id ? '…' : '↻'}
              </button>
            </div>
          {/if}
        </header>
        <div class="previews">
          {#each g.outputs as out, i (i)}
            {#if out.kind === 'image'}
              <a href={generationFileUrl(g.id, i)} target="_blank" rel="noopener">
                <img class="thumb" src={generationFileUrl(g.id, i)} alt={summarise(g)} />
              </a>
            {:else if out.kind === 'video'}
              <!-- svelte-ignore a11y_media_has_caption -->
              <video class="thumb" controls preload="metadata" src={generationFileUrl(g.id, i)}
              ></video>
            {:else if out.kind === 'audio'}
              <audio controls preload="none" src={generationFileUrl(g.id, i)}></audio>
            {:else if out.kind === 'transcript'}
              <details ontoggle={() => loadTranscript(g.id, i)}>
                <summary>Transcript</summary>
                <pre class="transcript">{transcripts[`${g.id}:${i}`] ?? 'Loading…'}</pre>
              </details>
            {/if}
          {/each}
        </div>
        <footer class="card-foot">
          <span class="sub">
            {when(g.created_at)} · {g.outputs.length}
            {g.outputs.length === 1 ? 'file' : 'files'}
            {#if g.outputs[0]}
              · {ext(g.outputs[0])}{/if}
          </span>
          <div class="card-btns">
            {#if onRestore}
              <button
                class="btn-sec"
                onclick={() => onRestore?.(g)}
                title="Load these settings back into the form"
              >
                Use settings
              </button>
            {/if}
            <button
              class="btn-del"
              onclick={() => remove(g.id)}
              disabled={deleting === g.id}
              title="Delete this generation and its files"
            >
              {deleting === g.id ? '…' : 'Delete'}
            </button>
          </div>
        </footer>
      </article>
    {/each}
  </div>

  {#if hasMore}
    <div class="hist-more">
      <button class="btn-sec" onclick={loadMore} disabled={loadingMore}>
        {loadingMore ? 'Loading…' : `Load older (${items.length} of ${total})`}
      </button>
    </div>
  {/if}
</section>

<style>
  .history {
    margin-top: 28px;
  }
  .hist-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 12px;
  }
  .hist-head h3 {
    margin: 0;
    font-size: 15px;
    color: var(--ink);
  }
  .hist-actions {
    display: flex;
    gap: 8px;
  }
  .hist-more {
    display: flex;
    justify-content: center;
    margin-top: 14px;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
  }
  .empty {
    color: var(--muted);
    font-size: 13px;
  }
  /* Native audio controls drop the seek bar entirely below ~350px, so an audio history needs a
     wider track than a thumbnail grid — see `cardMin`. */
  .hist-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(var(--card-min), 1fr));
    gap: 14px;
  }
  .card {
    border: 1px solid var(--ring);
    border-radius: 12px;
    overflow: hidden;
    background: var(--card);
    display: flex;
    flex-direction: column;
  }
  .previews {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px;
    background: var(--bg-2);
  }
  .thumb {
    display: block;
    width: 100%;
    height: auto;
    border-radius: 8px;
  }
  .previews audio {
    width: 100%;
  }
  .transcript {
    max-height: 200px;
    overflow: auto;
    white-space: pre-wrap;
    font-size: 12px;
    color: var(--ink);
    margin: 6px 0 0;
  }
  details summary {
    cursor: pointer;
    font-size: 13px;
    color: var(--ink);
    padding: 6px;
  }
  .card-head {
    padding: 8px 10px;
  }
  .card-foot {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 10px;
    margin-top: auto;
  }
  .card-btns {
    display: flex;
    gap: 6px;
  }
  .card-btns > button {
    flex: 1;
  }
  .title-row {
    display: flex;
    align-items: center;
    gap: 4px;
    min-width: 0;
  }
  .summary {
    font-size: 13px;
    color: var(--ink);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .title-btn {
    background: transparent;
    border: none;
    padding: 0;
    margin: 0;
    text-align: left;
    cursor: text;
    flex: 1;
    min-width: 0;
  }
  .title-btn:hover {
    text-decoration: underline;
    text-decoration-style: dotted;
  }
  .icon-btn {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    font-size: 13px;
    line-height: 1;
    padding: 2px 4px;
    border-radius: 6px;
    flex-shrink: 0;
  }
  .icon-btn:hover {
    color: var(--ink);
    background: var(--bg-2);
  }
  .icon-btn:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .title-edit {
    font-size: 13px;
    color: var(--ink);
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    padding: 3px 6px;
    width: 100%;
  }
  .sub {
    font-size: 11px;
    color: var(--muted);
  }
  .btn-sec {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
    padding: 6px 10px;
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
  }
  .btn-del {
    background: transparent;
    border: 1px solid var(--ring);
    color: var(--danger);
    padding: 6px 10px;
    border-radius: 8px;
    font-size: 12px;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn-del:disabled,
  .btn-sec:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
