<script lang="ts">
  import { onMount } from 'svelte';
  import {
    listGpuLogs,
    getGpuLogsStats,
    deleteGpuLog,
    clearGpuLogs,
    gpuLogSampleUrl
  } from '#lib/api/client.ts';
  import type { GpuLogEntry, GpuLogsStats } from '#lib/api/client.ts';
  import JsonBlock from '#lib/components/JsonBlock.svelte';

  let entries = $state<GpuLogEntry[]>([]);
  let total = $state(0);
  let stats = $state<GpuLogsStats | null>(null);
  let selected = $state<GpuLogEntry | null>(null);
  let serviceFilter = $state<'' | 'tts' | 'stt' | 'vision'>('');
  let limit = $state(50);
  let offset = $state(0);
  let loading = $state(true);

  async function load() {
    loading = true;
    const [list, s] = await Promise.all([
      listGpuLogs(serviceFilter || undefined, limit, offset),
      getGpuLogsStats()
    ]);
    entries = list.entries;
    total = list.total;
    stats = s;
    loading = false;
  }

  onMount(load);

  async function remove(id: number) {
    await deleteGpuLog(id);
    if (selected?.id === id) selected = null;
    await load();
  }

  async function clearAll() {
    if (!confirm('Delete ALL GPU logs and samples?')) return;
    await clearGpuLogs();
    selected = null;
    await load();
  }

  function fmtBytes(b: number): string {
    if (!b) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB'];
    let i = 0;
    let v = b;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v.toFixed(v < 10 ? 2 : 1)} ${units[i]}`;
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>GPU Logs</h2>
    <p class="muted">Tier-2 playground history. Saved samples link to replay audio/image bytes.</p>
  </div>

  {#if stats}
    <div class="stats">
      <div><span>Entries:</span> {stats.total_entries}</div>
      <div><span>Samples:</span> {stats.total_samples}</div>
      <div><span>Disk:</span> {fmtBytes(stats.total_sample_bytes)}</div>
      {#if stats.oldest_created_at}<div><span>Oldest:</span> {stats.oldest_created_at}</div>{/if}
    </div>
  {/if}

  <div class="filters">
    <label>
      Service
      <select
        bind:value={serviceFilter}
        onchange={() => {
          offset = 0;
          load();
        }}
      >
        <option value="">(all)</option>
        <option value="tts">tts (legacy chatterbox rows)</option>
        <option value="stt">stt</option>
        <option value="vision">vision</option>
      </select>
    </label>
    <button class="btn-sec" onclick={load} disabled={loading}>Refresh</button>
    <button class="btn-sec btn-danger" onclick={clearAll}>Clear all</button>
  </div>

  <div class="grid">
    <div class="list">
      {#if loading}
        <div class="muted">Loading…</div>
      {:else if entries.length === 0}
        <div class="muted">No entries.</div>
      {:else}
        <table>
          <thead>
            <tr>
              <th>ID</th>
              <th>Service</th>
              <th>Endpoint</th>
              <th>Status</th>
              <th>Duration</th>
              <th>Gate wait</th>
              <th>Sample</th>
              <th>Created</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {#each entries as e (e.id)}
              <tr
                class:active={selected?.id === e.id}
                class:err={e.status >= 400}
                onclick={() => (selected = e)}
              >
                <td>#{e.id}</td>
                <td>{e.service}</td>
                <td>{e.endpoint}</td>
                <td>{e.status}</td>
                <td>{e.duration_ms} ms</td>
                <td>{e.gate_wait_ms} ms</td>
                <td class="sample-cell" onclick={(ev) => ev.stopPropagation()}>
                  {#if e.sample_id}
                    {#if e.service === 'vision'}
                      <a
                        href={gpuLogSampleUrl(e.id)}
                        target="_blank"
                        rel="noopener"
                        aria-label="open sample"
                      >
                        <img class="thumb" src={gpuLogSampleUrl(e.id)} alt="sample" />
                      </a>
                    {:else}
                      <audio class="mini-audio" controls preload="none" src={gpuLogSampleUrl(e.id)}
                      ></audio>
                    {/if}
                  {/if}
                </td>
                <td>{new Date(e.created_at).toLocaleString()}</td>
                <td>
                  <button
                    class="icon"
                    onclick={(ev) => {
                      ev.stopPropagation();
                      remove(e.id);
                    }}
                  >
                    ✕
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>

        <div class="pager">
          <button
            class="btn-sec"
            disabled={offset === 0}
            onclick={() => {
              offset = Math.max(0, offset - limit);
              load();
            }}
          >
            Prev
          </button>
          <span class="muted">
            {offset + 1}–{Math.min(offset + limit, total)} of {total}
          </span>
          <button
            class="btn-sec"
            disabled={offset + limit >= total}
            onclick={() => {
              offset += limit;
              load();
            }}
          >
            Next
          </button>
        </div>
      {/if}
    </div>

    <div class="detail">
      {#if selected}
        <h3>Entry #{selected.id}</h3>
        <div class="meta">
          <div><span>Service:</span> {selected.service}</div>
          <div><span>Endpoint:</span> {selected.endpoint}</div>
          <div><span>Status:</span> {selected.status}</div>
          <div><span>Duration:</span> {selected.duration_ms} ms</div>
          <div><span>Gate wait:</span> {selected.gate_wait_ms} ms</div>
          <div><span>Caller:</span> {selected.caller ?? '—'}</div>
          <div><span>Created:</span> {selected.created_at}</div>
        </div>
        <h4>Request</h4>
        <JsonBlock content={selected.request_meta} label="Request" />
        <h4>Response</h4>
        <JsonBlock content={selected.response_meta} label="Response" />
        {#if selected.sample_id}
          <div class="sample-actions">
            {#if selected.service === 'tts' || selected.service === 'stt'}
              <audio controls src={gpuLogSampleUrl(selected.id)}></audio>
            {:else}
              <img class="sample" src={gpuLogSampleUrl(selected.id)} alt="sample" />
            {/if}
            <a class="btn-sec" href={gpuLogSampleUrl(selected.id)} download>Download sample</a>
          </div>
        {/if}
      {:else}
        <div class="muted">Click a row to see details.</div>
      {/if}
    </div>
  </div>
</div>

<style>
  .stats {
    display: flex;
    gap: 24px;
    flex-wrap: wrap;
    padding: 12px 16px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 12px;
    margin-bottom: 16px;
    font-size: 13px;
  }
  .stats span {
    color: var(--muted);
    margin-right: 6px;
  }
  .filters {
    display: flex;
    gap: 12px;
    align-items: flex-end;
    margin-bottom: 16px;
  }
  .filters label {
    display: flex;
    flex-direction: column;
    font-size: 12px;
    color: var(--muted);
  }
  .filters select {
    padding: 6px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
  }
  .grid {
    display: grid;
    grid-template-columns: 2fr 1fr;
    gap: 16px;
  }
  @media (max-width: 900px) {
    .grid {
      grid-template-columns: 1fr;
    }
  }
  .list {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 16px;
    overflow-x: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }
  th,
  td {
    text-align: left;
    padding: 6px 8px;
    border-bottom: 1px solid var(--ring);
  }
  tbody tr {
    cursor: pointer;
  }
  tbody tr:hover {
    background: var(--bg-2);
  }
  tbody tr.active {
    background: var(--accent-dim);
  }
  tbody tr.err {
    color: var(--danger);
  }
  .icon {
    background: none;
    border: none;
    cursor: pointer;
    color: var(--muted);
  }
  .icon:hover {
    color: var(--danger);
  }
  .pager {
    display: flex;
    gap: 12px;
    align-items: center;
    margin-top: 12px;
    justify-content: flex-end;
  }
  .detail {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 16px;
  }
  .detail h3 {
    margin: 0 0 8px;
  }
  .detail h4 {
    margin: 12px 0 4px;
    font-size: 12px;
    text-transform: uppercase;
    color: var(--muted);
  }
  .meta {
    font-size: 12px;
  }
  .meta span {
    color: var(--muted);
    margin-right: 6px;
  }
  .sample-actions {
    margin-top: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .sample {
    max-width: 100%;
    border-radius: 8px;
  }
  .sample-cell {
    width: 220px;
    padding: 4px 8px;
  }
  .thumb {
    display: block;
    width: 64px;
    height: 64px;
    object-fit: cover;
    border-radius: 6px;
    border: 1px solid var(--ring);
    background: var(--bg-2);
  }
  .mini-audio {
    width: 100%;
    max-width: 220px;
    height: 32px;
  }
  tbody tr:not(.active) .mini-audio {
    opacity: 0.85;
  }
  .btn-danger {
    color: var(--danger);
    border-color: var(--danger);
  }
</style>
