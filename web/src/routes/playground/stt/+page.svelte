<script lang="ts">
  import { onMount } from 'svelte';
  import {
    sttTranscribeWithProgress,
    sttTranscribeFromLibrary,
    searchAudiobooks,
    getAudiobook,
    getHealth,
    getSettings
  } from '$lib/api/client';
  import JsonBlock from '$lib/components/JsonBlock.svelte';
  import type {
    SttResponse,
    SttTranscribeParams,
    AudiobookSummary,
    AudiobookDetail
  } from '$lib/api/client';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import GenerationHistory from '$lib/components/GenerationHistory.svelte';
  import type { Generation } from '$lib/types';
  import { str, num, obj, reattachNote } from '$lib/restore';

  let historyKey = $state(0);
  let restoreNote = $state('');

  // Restore the transcription knobs from a saved generation. The audio file
  // itself can't be re-attached (reattachNote flags that).
  function restore(g: Generation) {
    const m = g.request_meta;
    const p = obj(m, 'params') ?? m;
    const gr = str(p, 'timestamp_granularity');
    if (gr === 'segment' || gr === 'word' || gr === 'char') granularity = gr;
    const bs = num(p, 'beam_size');
    if (bs !== undefined) beamSize = bs;
    const lang = str(p, 'language');
    if (lang !== undefined) language = lang;
    restoreNote = reattachNote(m);
    error = '';
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  let source = $state<'upload' | 'library'>('upload');
  let uploadFile = $state<FileList | null>(null);

  let librarySearch = $state('');
  let libraryResults = $state<AudiobookSummary[]>([]);
  let selectedBook = $state<AudiobookDetail | null>(null);
  let offsetSeconds = $state<number | null>(null);
  let durationSeconds = $state<number | null>(null);

  let granularity = $state<'segment' | 'word' | 'char'>('segment');
  let beamSize = $state<number | null>(null);
  let language = $state('en');
  let saveSample = $state(true);

  let transcribing = $state(false);
  let uploadPhase = $state<'idle' | 'uploading' | 'transcribing'>('idle');
  let uploadLoaded = $state(0);
  let uploadTotal = $state(0);
  let result = $state<SttResponse | null>(null);
  let error = $state('');
  let model = $state('');

  const uploadPct = $derived(uploadTotal > 0 ? Math.round((uploadLoaded / uploadTotal) * 100) : 0);

  onMount(async () => {
    const [healthRes, settingsRes] = await Promise.allSettled([getHealth(), getSettings()]);
    if (healthRes.status === 'fulfilled') model = healthRes.value.stt_model || '';
    if (settingsRes.status === 'fulfilled' && settingsRes.value.gpu_sample_save_default != null) {
      saveSample = Boolean(settingsRes.value.gpu_sample_save_default);
    }
  });

  async function handleSearch() {
    if (!librarySearch.trim()) return;
    libraryResults = await searchAudiobooks(librarySearch.trim());
  }

  async function pickBook(id: string) {
    selectedBook = await getAudiobook(id);
  }

  async function handleTranscribe() {
    transcribing = true;
    error = '';
    result = null;
    uploadPhase = 'idle';
    uploadLoaded = 0;
    uploadTotal = 0;
    const params: SttTranscribeParams = {
      timestamp_granularity: granularity,
      beam_size: beamSize ?? undefined,
      language: language || undefined,
      save_sample: saveSample
    };
    try {
      if (source === 'upload') {
        if (!uploadFile || !uploadFile.length) {
          error = 'pick a file';
          return;
        }
        uploadPhase = 'uploading';
        uploadTotal = uploadFile[0].size;
        result = await sttTranscribeWithProgress(uploadFile[0], params, (phase, loaded, total) => {
          uploadPhase = phase;
          uploadLoaded = loaded;
          uploadTotal = total;
        });
      } else {
        if (!selectedBook) {
          error = 'pick a library book';
          return;
        }
        result = await sttTranscribeFromLibrary({
          library_audiobook_id: selectedBook.id,
          offset_seconds: offsetSeconds ?? undefined,
          duration_seconds: durationSeconds ?? undefined,
          timestamp_granularity: granularity,
          beam_size: beamSize ?? undefined,
          language: language || undefined,
          save_sample: saveSample
        });
      }
      if (result) historyKey++;
    } catch (e) {
      error = e instanceof Error ? e.message : 'Transcribe failed';
    } finally {
      transcribing = false;
      uploadPhase = 'idle';
    }
  }

  function fmtBytes(n: number): string {
    if (!n) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB'];
    let i = 0;
    let v = n;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
  }

  function downloadJson() {
    if (!result) return;
    const blob = new Blob([JSON.stringify(result, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = 'transcription.json';
    a.click();
    URL.revokeObjectURL(url);
  }

  function downloadSrt() {
    if (!result || !result.segments.length) return;
    const lines: string[] = [];
    result.segments.forEach((s, i) => {
      lines.push(String(i + 1));
      lines.push(`${fmtSrt(s.start)} --> ${fmtSrt(s.end)}`);
      lines.push(s.text);
      lines.push('');
    });
    const blob = new Blob([lines.join('\n')], { type: 'text/plain' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = 'transcription.srt';
    a.click();
    URL.revokeObjectURL(url);
  }

  function fmtSrt(s: number): string {
    const hh = Math.floor(s / 3600);
    const mm = Math.floor((s % 3600) / 60);
    const ss = Math.floor(s % 60);
    const ms = Math.floor((s - Math.floor(s)) * 1000);
    return `${pad(hh)}:${pad(mm)}:${pad(ss)},${pad(ms, 3)}`;
  }
  function pad(n: number, w = 2): string {
    return String(n).padStart(w, '0');
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>STT Playground</h2>
    {#if model}
      <p class="muted">Model: <code>{model}</code></p>
    {/if}
    <p class="muted preempt-note">
      Preempts automatically when TTS or vision needs the GPU — a partial transcription is returned
      with <code>preempted: true</code>.
    </p>
  </div>

  <div class="card">
    <div class="row">
      <label class="toggle">
        <input type="radio" bind:group={source} value="upload" />
        Upload file
      </label>
      <label class="toggle">
        <input type="radio" bind:group={source} value="library" />
        Pick from kobbler library
      </label>
    </div>

    {#if source === 'upload'}
      <FileDropzone
        accept="audio/*"
        bind:files={uploadFile}
        label="Drop audio file"
        hint="or click to browse"
      />
    {:else}
      <div class="search-row">
        <input
          type="text"
          bind:value={librarySearch}
          placeholder="Search audiobooks…"
          onkeydown={(e) => e.key === 'Enter' && handleSearch()}
        />
        <button class="btn-sec" onclick={handleSearch}>Search</button>
      </div>
      {#if libraryResults.length > 0}
        <ul class="results">
          {#each libraryResults as book (book.id)}
            <li>
              <button class="link" onclick={() => pickBook(book.id)}>
                {book.title}{book.author ? ` — ${book.author}` : ''}
              </button>
            </li>
          {/each}
        </ul>
      {/if}
      {#if selectedBook}
        <div class="selected">{selectedBook.title} ({selectedBook.chapters.length} chapters)</div>
        <div class="slice-row">
          <label>
            Offset (s)
            <input type="number" min="0" bind:value={offsetSeconds} />
          </label>
          <label>
            Duration (s) — leave blank for full
            <input type="number" min="1" bind:value={durationSeconds} />
          </label>
        </div>
      {/if}
    {/if}
  </div>

  <div class="card">
    <h3>Knobs</h3>
    <div class="knobs">
      <label>
        Timestamp granularity
        <select bind:value={granularity}>
          <option value="segment">segment</option>
          <option value="word">word</option>
          <option value="char">char</option>
        </select>
      </label>
      <label>
        Beam size (blank = greedy)
        <input type="number" min="1" max="20" bind:value={beamSize} />
      </label>
      <label>
        Language
        <input type="text" bind:value={language} placeholder="en" />
      </label>
      <label class="toggle">
        <input type="checkbox" bind:checked={saveSample} />
        save sample for replay
      </label>
    </div>
  </div>

  <div class="actions">
    <button class="btn-primary" onclick={handleTranscribe} disabled={transcribing}>
      {transcribing ? 'Working…' : 'Transcribe'}
    </button>
    {#if error}<span class="error">{error}</span>{/if}
  </div>

  {#if restoreNote}<p class="restore-note">{restoreNote}</p>{/if}

  {#if transcribing && uploadPhase !== 'idle'}
    <div class="progress-card">
      {#if uploadPhase === 'uploading'}
        <div class="progress-label">
          Uploading — {uploadPct}% ({fmtBytes(uploadLoaded)} / {fmtBytes(uploadTotal)})
        </div>
        <div class="progress-bar">
          <div class="progress-fill" style="width: {uploadPct}%"></div>
        </div>
      {:else}
        <div class="progress-label">Upload complete — transcribing…</div>
        <div class="progress-bar">
          <div class="progress-fill indeterminate"></div>
        </div>
      {/if}
    </div>
  {/if}

  {#if result}
    <div class="card">
      <h3>
        Transcription {#if result.preempted}<span class="tag warn">preempted</span>{/if}
      </h3>
      <JsonBlock content={result.text} label="Transcription" />
      <div class="meta">
        {#if result.duration_ms != null}<span>{result.duration_ms} ms</span>{/if}
        {#if result.gate_wait_ms != null}<span>gate wait {result.gate_wait_ms} ms</span>{/if}
        <span
          >{result.segments.length} segments, {result.words.length} words, {result.chars.length} chars</span
        >
      </div>
      <div class="actions">
        <button class="btn-sec" onclick={downloadJson}>Download JSON</button>
        <button class="btn-sec" onclick={downloadSrt}>Download SRT</button>
      </div>
      {#if result.segments.length}
        <table>
          <thead>
            <tr>
              <th>#</th>
              <th>Start</th>
              <th>End</th>
              <th>Text</th>
            </tr>
          </thead>
          <tbody>
            {#each result.segments as seg, i (seg.id ?? i)}
              <tr>
                <td>{seg.id ?? ''}</td>
                <td>{seg.start.toFixed(2)}</td>
                <td>{seg.end.toFixed(2)}</td>
                <td>{seg.text}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>
  {/if}

  <GenerationHistory
    service="stt"
    refreshKey={historyKey}
    title="Transcript history"
    onRestore={restore}
  />
</div>

<style>
  .preempt-note {
    font-size: 12px;
    line-height: 1.45;
    margin-top: 4px;
  }
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
  .search-row {
    display: flex;
    gap: 8px;
  }
  .results {
    list-style: none;
    padding: 0;
    margin: 8px 0;
    max-height: 200px;
    overflow-y: auto;
  }
  .results li {
    padding: 4px 0;
  }
  .link {
    background: none;
    border: none;
    color: var(--accent);
    cursor: pointer;
    padding: 0;
    font: inherit;
  }
  .selected {
    margin: 12px 0;
  }
  .slice-row {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
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
    flex-wrap: wrap;
  }
  .meta {
    display: flex;
    gap: 12px;
    font-size: 12px;
    color: var(--muted);
    margin: 8px 0;
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
  table {
    width: 100%;
    border-collapse: collapse;
    margin-top: 12px;
    font-size: 13px;
  }
  th,
  td {
    text-align: left;
    padding: 6px 8px;
    border-bottom: 1px solid var(--ring);
  }
  .tag {
    font-size: 11px;
    padding: 2px 8px;
    border-radius: 8px;
    margin-left: 8px;
  }
  .tag.warn {
    background: var(--warning);
    color: white;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
  }
  .progress-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 12px;
    padding: 14px 16px;
    margin-bottom: 16px;
  }
  .progress-label {
    font-size: 13px;
    color: var(--muted);
    margin-bottom: 8px;
  }
  .progress-bar {
    height: 6px;
    background: var(--bg-2);
    border-radius: 3px;
    overflow: hidden;
    position: relative;
  }
  .progress-fill {
    height: 100%;
    background: var(--accent);
    transition: width 120ms linear;
  }
  .progress-fill.indeterminate {
    width: 40%;
    animation: indeterminate 1.6s ease-in-out infinite;
  }
  @keyframes indeterminate {
    0% {
      margin-left: -40%;
    }
    100% {
      margin-left: 100%;
    }
  }
</style>
