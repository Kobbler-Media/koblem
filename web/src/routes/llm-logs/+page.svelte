<script lang="ts">
  import { onMount } from 'svelte';
  import { listLlmLogs, getLlmLog, rerunLlmLog } from '$lib/api/client';
  import type { LlmLogSummary, LlmLogDetail, SamplingPreset } from '$lib/types';
  import JsonBlock from '$lib/components/JsonBlock.svelte';
  import SamplingControls, {
    emptySampling,
    fromSamplingConfig,
    toSamplingConfig,
    type SamplingState
  } from '$lib/components/SamplingControls.svelte';

  let logs = $state<LlmLogSummary[]>([]);
  let loading = $state(true);
  let error = $state('');
  let callerFilter = $state('');
  let expandedId = $state<string | null>(null);
  let detail = $state<LlmLogDetail | null>(null);
  let detailLoading = $state(false);
  let detailError = $state('');
  let showRequestMessages = $state(false);
  let showToolCalls = $state(false);
  let showThinking = $state(true);
  let rerunSampling: SamplingState = $state(emptySampling());
  let showRerun = $state(false);
  let rerunning = $state(false);
  let rerunError = $state('');
  // ms tick used to recompute "running for Xs" labels live without
  // refetching from the server. Bumped every second by setInterval below.
  let now = $state(Date.now());

  const POLL_MS = 2000;
  const hasRunningRow = $derived(logs.some((l) => l.status === 'running'));
  const detailIsRunning = $derived(detail?.status === 'running');

  onMount(() => {
    loadLogs();
    const tick = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(tick);
  });

  // Auto-refresh the list every POLL_MS as long as any row is running.
  // We use $effect so the polling starts/stops automatically as rows
  // transition between running and complete.
  $effect(() => {
    if (!hasRunningRow) return;
    const handle = setInterval(() => {
      // Quiet refresh — don't flip the loading flag, which would flicker
      // the table on every tick.
      listLlmLogs(50, undefined, callerFilter || undefined)
        .then((rows) => {
          logs = rows;
        })
        .catch(() => {
          /* ignore transient errors during background refresh */
        });
    }, POLL_MS);
    return () => clearInterval(handle);
  });

  // Auto-refetch the expanded row's detail while it's still running.
  $effect(() => {
    if (!expandedId || !detailIsRunning) return;
    const id = expandedId;
    const handle = setInterval(() => {
      getLlmLog(id)
        .then((d) => {
          // Drop the result if the user navigated to a different row mid-fetch.
          if (expandedId === id) detail = d;
        })
        .catch(() => {
          /* ignore */
        });
    }, POLL_MS);
    return () => clearInterval(handle);
  });

  const PRESET_NAMES: SamplingPreset[] = [
    'deterministic',
    'qwen_instruct_general',
    'qwen_instruct_reasoning'
  ];
  /** Narrow the log's free-text sampling tag to a real preset, or nothing.
   * The column also carries per-surface settings keys (see `LlmLogDetail`). */
  function asPreset(tag: string | undefined): SamplingPreset | undefined {
    return PRESET_NAMES.find((p) => p === tag);
  }

  async function loadLogs() {
    loading = true;
    error = '';
    try {
      logs = await listLlmLogs(50, undefined, callerFilter || undefined);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to load logs';
    }
    loading = false;
  }

  async function toggleRow(id: string) {
    if (expandedId === id) {
      expandedId = null;
      detail = null;
      showRerun = false;
      return;
    }
    expandedId = id;
    detail = null;
    detailLoading = true;
    detailError = '';
    showRequestMessages = false;
    showToolCalls = false;
    showThinking = true;
    showRerun = false;
    rerunError = '';
    try {
      detail = await getLlmLog(id);
      // Prefill the rerun form with the log's own sampling so the user can
      // tweak from there rather than from a blank slate.
      rerunSampling = fromSamplingConfig({
        // 🔴 `sampling_preset` is a free-text tag — for calls that had no preset
        // it holds the surface key (`sampling_director`, …). Feeding that back
        // would put a non-preset in the preset field and 422 the rerun.
        sampling_preset: asPreset(detail.sampling_preset),
        temperature: detail.request_temperature,
        top_p: detail.request_top_p,
        top_k: detail.request_top_k,
        min_p: detail.request_min_p,
        presence_penalty: detail.request_presence_penalty,
        repetition_penalty: detail.request_repetition_penalty,
        max_tokens: detail.request_max_tokens,
        enable_thinking: detail.request_enable_thinking,
        grammar_mode: detail.request_grammar
      });
    } catch (e) {
      detailError = e instanceof Error ? e.message : 'Failed to load log';
    }
    detailLoading = false;
  }

  async function handleRerun() {
    if (!detail) return;
    rerunning = true;
    rerunError = '';
    try {
      await rerunLlmLog(detail.id, toSamplingConfig(rerunSampling));
      await loadLogs();
      // Close the detail view so the new row surfaces at the top of the list.
      expandedId = null;
      detail = null;
      showRerun = false;
    } catch (e) {
      rerunError = e instanceof Error ? e.message : 'Rerun failed';
    }
    rerunning = false;
  }

  function formatDate(iso: string): string {
    const d = new Date(iso);
    return d.toLocaleString();
  }

  function formatRunning(createdAtIso: string): string {
    const elapsed = Math.max(0, now - new Date(createdAtIso).getTime());
    const total = Math.floor(elapsed / 1000);
    if (total < 60) return `${total}s`;
    const m = Math.floor(total / 60);
    const s = total % 60;
    if (m < 60) return `${m}m ${s}s`;
    const h = Math.floor(m / 60);
    return `${h}h ${m % 60}m`;
  }

  function handleFilterChange() {
    expandedId = null;
    detail = null;
    loadLogs();
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>LLM Logs</h2>
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}

  <div class="filter-bar">
    <label class="filter-group">
      <span class="filter-label">Filter by Caller</span>
      <input
        type="text"
        class="filter-input"
        bind:value={callerFilter}
        placeholder="e.g. review, chat"
        onchange={handleFilterChange}
      />
    </label>
    <button class="btn-sec compact" type="button" onclick={loadLogs}>Refresh</button>
  </div>

  {#if loading}
    <div class="loading-card">Loading logs...</div>
  {:else if logs.length === 0}
    <div class="empty-card">
      <p>No LLM logs found{callerFilter ? ` for caller "${callerFilter}"` : ''}.</p>
    </div>
  {:else}
    <div class="logs-card">
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Status</th>
              <th>Caller</th>
              <th>Preview</th>
              <th>Tokens</th>
              <th>Duration</th>
              <th>Created</th>
            </tr>
          </thead>
          <tbody>
            {#each logs as log (log.id)}
              <tr
                class="log-row"
                class:expanded={expandedId === log.id}
                class:row-running={log.status === 'running'}
                class:row-error={log.status === 'error'}
                class:row-cancelled={log.status === 'cancelled'}
                onclick={() => toggleRow(log.id)}
              >
                <td>
                  <span class="status-badge status-{log.status}">
                    {#if log.status === 'running'}
                      <span class="dot pulsing"></span> running
                    {:else}
                      {log.status}
                    {/if}
                  </span>
                </td>
                <td>
                  {#if log.caller}
                    <span class="caller-chip">{log.caller}</span>
                  {:else}
                    <span class="muted-text">-</span>
                  {/if}
                </td>
                <td class="preview-cell">{log.message_preview ?? ''}</td>
                <td class="mono">
                  {log.completion_tokens != null ? log.completion_tokens : '-'}
                </td>
                <td class="mono">
                  {#if log.status === 'running'}
                    <span class="running-elapsed">{formatRunning(log.created_at)}</span>
                  {:else if log.duration_ms != null}
                    {log.duration_ms}ms
                  {:else}
                    -
                  {/if}
                </td>
                <td class="date-cell">{formatDate(log.created_at)}</td>
              </tr>
              {#if expandedId === log.id}
                <tr class="detail-row">
                  <td colspan="6">
                    {#if detailLoading}
                      <div class="detail-loading">Loading details...</div>
                    {:else if detailError}
                      <div class="error-banner inner">{detailError}</div>
                    {:else if detail}
                      <div class="detail-content">
                        <!-- Live progress banner — only when row is running -->
                        {#if detail.status === 'running'}
                          <div class="live-banner">
                            <span class="dot pulsing"></span>
                            <span class="live-label">Running</span>
                            <span class="mono">{formatRunning(detail.created_at)}</span>
                            {#if detail.completion_tokens != null}
                              <span class="muted-text">·</span>
                              <span class="mono">{detail.completion_tokens} tokens</span>
                            {/if}
                            <span class="muted-text">· auto-refresh every 2s</span>
                          </div>
                        {/if}
                        {#if detail.status === 'error' && detail.error_message}
                          <div class="error-banner inner">{detail.error_message}</div>
                        {/if}

                        <!-- Badges row -->
                        <div class="badge-row">
                          <span class="status-badge status-{detail.status}">
                            {#if detail.status === 'running'}
                              <span class="dot pulsing"></span> running
                            {:else}
                              {detail.status}
                            {/if}
                          </span>
                          {#if detail.streaming}
                            <span class="mode-badge streaming">Streaming</span>
                          {:else}
                            <span class="mode-badge">Batch</span>
                          {/if}
                          {#if detail.tool_rewrite_applied}
                            <span class="rewrite-badge">Tool Rewrite</span>
                          {/if}
                          {#if detail.finish_reason}
                            <span class="finish-badge">{detail.finish_reason}</span>
                          {/if}
                          {#if detail.sampling_preset}
                            <span class="preset-badge">{detail.sampling_preset}</span>
                          {/if}
                          {#if detail.request_enable_thinking === true}
                            <span class="thinking-badge">Thinking ON</span>
                          {:else if detail.request_enable_thinking === false}
                            <span class="thinking-badge off">Thinking OFF</span>
                          {/if}
                          {#if detail.request_grammar && detail.request_grammar !== 'off'}
                            <span class="preset-badge">{`grammar:${detail.request_grammar}`}</span>
                          {/if}
                        </div>

                        <!-- Timing / Tokens -->
                        <div class="detail-section">
                          <h4>Metrics</h4>
                          <div class="metrics-row">
                            {#if detail.duration_ms != null}
                              <div class="metric">
                                <span class="metric-label">Duration</span>
                                <span class="metric-value">{detail.duration_ms}ms</span>
                              </div>
                            {/if}
                            {#if detail.prompt_tokens != null}
                              <div class="metric">
                                <span class="metric-label">Prompt Tokens</span>
                                <span class="metric-value">{detail.prompt_tokens}</span>
                              </div>
                            {/if}
                            {#if detail.completion_tokens != null}
                              <div class="metric">
                                <span class="metric-label">Completion Tokens</span>
                                <span class="metric-value">{detail.completion_tokens}</span>
                              </div>
                            {/if}
                            {#if detail.request_temperature != null}
                              <div class="metric">
                                <span class="metric-label">Temperature</span>
                                <span class="metric-value">{detail.request_temperature}</span>
                              </div>
                            {/if}
                            {#if detail.request_max_tokens != null}
                              <div class="metric">
                                <span class="metric-label">Max Tokens</span>
                                <span class="metric-value">{detail.request_max_tokens}</span>
                              </div>
                            {/if}
                            {#if detail.request_top_p != null}
                              <div class="metric">
                                <span class="metric-label">top_p</span>
                                <span class="metric-value">{detail.request_top_p}</span>
                              </div>
                            {/if}
                            {#if detail.request_top_k != null}
                              <div class="metric">
                                <span class="metric-label">top_k</span>
                                <span class="metric-value">{detail.request_top_k}</span>
                              </div>
                            {/if}
                            {#if detail.request_min_p != null}
                              <div class="metric">
                                <span class="metric-label">min_p</span>
                                <span class="metric-value">{detail.request_min_p}</span>
                              </div>
                            {/if}
                            {#if detail.request_presence_penalty != null}
                              <div class="metric">
                                <span class="metric-label">presence</span>
                                <span class="metric-value">{detail.request_presence_penalty}</span>
                              </div>
                            {/if}
                            {#if detail.request_repetition_penalty != null}
                              <div class="metric">
                                <span class="metric-label">repetition</span>
                                <span class="metric-value">{detail.request_repetition_penalty}</span
                                >
                              </div>
                            {/if}
                          </div>
                        </div>

                        <!-- Rerun with new sampling -->
                        <div class="detail-section">
                          <button
                            class="section-toggle"
                            type="button"
                            onclick={() => (showRerun = !showRerun)}
                          >
                            <h4>Rerun with new params</h4>
                            <svg
                              class="chevron"
                              class:expanded={showRerun}
                              viewBox="0 0 24 24"
                              width="14"
                              height="14"
                              fill="currentColor"
                            >
                              <path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6 1.41-1.41z"
                              ></path>
                            </svg>
                          </button>
                          {#if showRerun}
                            <div class="rerun-block">
                              <SamplingControls bind:value={rerunSampling} label="Sampling" />
                              {#if rerunError}
                                <div class="error-banner inner">{rerunError}</div>
                              {/if}
                              <div class="rerun-actions">
                                <button
                                  class="btn-primary"
                                  type="button"
                                  disabled={rerunning}
                                  onclick={handleRerun}
                                >
                                  {rerunning ? 'Running…' : 'Run'}
                                </button>
                              </div>
                            </div>
                          {/if}
                        </div>

                        <!-- Response Content -->
                        {#if detail.response_content}
                          <div class="detail-section">
                            <h4>Response</h4>
                            <JsonBlock content={detail.response_content} label="Response" />
                          </div>
                        {/if}

                        <!-- Thinking trace (chain-of-thought) -->
                        {#if detail.response_reasoning_content}
                          <div class="detail-section">
                            <button
                              class="section-toggle"
                              type="button"
                              onclick={() => (showThinking = !showThinking)}
                            >
                              <h4>
                                Thinking ({detail.response_reasoning_content.length.toLocaleString()}
                                chars)
                              </h4>
                              <svg
                                class="chevron"
                                class:expanded={showThinking}
                                viewBox="0 0 24 24"
                                width="14"
                                height="14"
                                fill="currentColor"
                              >
                                <path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6 1.41-1.41z"
                                ></path>
                              </svg>
                            </button>
                            {#if showThinking}
                              <pre class="thinking-block">{detail.response_reasoning_content}</pre>
                            {/if}
                          </div>
                        {/if}

                        <!-- Request Messages (collapsible) -->
                        {#if detail.request_messages}
                          <div class="detail-section">
                            <button
                              class="section-toggle"
                              type="button"
                              onclick={() => (showRequestMessages = !showRequestMessages)}
                            >
                              <h4>Request Messages</h4>
                              <svg
                                class="chevron"
                                class:expanded={showRequestMessages}
                                viewBox="0 0 24 24"
                                width="14"
                                height="14"
                                fill="currentColor"
                              >
                                <path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6 1.41-1.41z"
                                ></path>
                              </svg>
                            </button>
                            {#if showRequestMessages}
                              <JsonBlock
                                content={detail.request_messages}
                                label="Request Messages"
                              />
                            {/if}
                          </div>
                        {/if}

                        <!-- Response Tool Calls (collapsible) -->
                        {#if detail.response_tool_calls}
                          <div class="detail-section">
                            <button
                              class="section-toggle"
                              type="button"
                              onclick={() => (showToolCalls = !showToolCalls)}
                            >
                              <h4>Tool Calls</h4>
                              <svg
                                class="chevron"
                                class:expanded={showToolCalls}
                                viewBox="0 0 24 24"
                                width="14"
                                height="14"
                                fill="currentColor"
                              >
                                <path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6 1.41-1.41z"
                                ></path>
                              </svg>
                            </button>
                            {#if showToolCalls}
                              <JsonBlock content={detail.response_tool_calls} label="Tool Calls" />
                            {/if}
                          </div>
                        {/if}

                        <!-- Request Tools (collapsible) -->
                        {#if detail.request_tools}
                          <div class="detail-section">
                            <button class="section-toggle" type="button" onclick={() => {}}>
                              <h4>Request Tools</h4>
                            </button>
                            <JsonBlock content={detail.request_tools} label="Request Tools" />
                          </div>
                        {/if}
                      </div>
                    {/if}
                  </td>
                </tr>
              {/if}
            {/each}
          </tbody>
        </table>
      </div>
    </div>
  {/if}
</div>

<style>
  .filter-bar {
    display: flex;
    gap: 12px;
    align-items: flex-end;
    margin-bottom: 16px;
    flex-wrap: wrap;
  }

  .filter-group {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .filter-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-weight: 600;
  }

  .filter-input {
    padding: 8px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: transparent;
    color: var(--ink);
    font-size: 13px;
    width: 200px;
  }

  .logs-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 4px;
    overflow: hidden;
  }

  .table-wrap {
    overflow-x: auto;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  th {
    text-align: left;
    padding: 12px 16px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
    border-bottom: 1px solid var(--ring);
    font-weight: 600;
  }

  td {
    padding: 12px 16px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.04);
    vertical-align: top;
  }

  :global([data-theme='light']) td {
    border-bottom: 1px solid rgba(0, 0, 0, 0.04);
  }

  .log-row {
    cursor: pointer;
    transition: background 0.15s;
  }

  .log-row:hover {
    background: rgba(255, 255, 255, 0.03);
  }

  :global([data-theme='light']) .log-row:hover {
    background: rgba(0, 0, 0, 0.03);
  }

  .log-row.expanded {
    background: rgba(255, 255, 255, 0.05);
  }

  .caller-chip {
    font-size: 11px;
    font-weight: 600;
    padding: 3px 8px;
    border-radius: 6px;
    background: var(--accent-dim);
    color: var(--accent);
  }

  .muted-text {
    color: var(--muted);
  }

  .preview-cell {
    max-width: 400px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .mode-badge {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 3px 8px;
    border-radius: 6px;
    background: var(--tag-bg);
    color: var(--tag-text);
  }

  .mode-badge.streaming {
    background: rgba(59, 130, 246, 0.15);
    color: #3b82f6;
  }

  .mono {
    font-variant-numeric: tabular-nums;
    font-family: monospace;
    font-size: 12px;
  }

  .date-cell {
    font-size: 12px;
    color: var(--muted);
    white-space: nowrap;
  }

  .detail-row td {
    padding: 0;
    background: rgba(0, 0, 0, 0.1);
  }

  :global([data-theme='light']) .detail-row td {
    background: rgba(0, 0, 0, 0.02);
  }

  .detail-loading {
    padding: 24px;
    text-align: center;
    color: var(--muted);
  }

  .detail-content {
    padding: 20px 24px;
  }

  .badge-row {
    display: flex;
    gap: 8px;
    margin-bottom: 16px;
    flex-wrap: wrap;
  }

  .rewrite-badge {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 3px 8px;
    border-radius: 6px;
    background: rgba(249, 115, 22, 0.15);
    color: var(--warning);
  }

  .finish-badge {
    font-size: 10px;
    font-weight: 600;
    padding: 3px 8px;
    border-radius: 6px;
    background: var(--tag-bg);
    color: var(--tag-text);
  }

  .preset-badge {
    font-size: 10px;
    font-weight: 600;
    padding: 3px 8px;
    border-radius: 6px;
    background: var(--accent-dim);
    color: var(--accent);
    text-transform: lowercase;
  }

  .thinking-badge {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 3px 8px;
    border-radius: 6px;
    background: rgba(168, 85, 247, 0.18);
    color: rgb(196, 138, 255);
  }

  .status-badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 3px 8px;
    border-radius: 6px;
    background: var(--tag-bg);
    color: var(--tag-text);
  }
  .status-badge.status-running {
    background: rgba(34, 197, 94, 0.18);
    color: rgb(74, 222, 128);
  }
  .status-badge.status-error {
    background: rgba(239, 68, 68, 0.18);
    color: rgb(248, 113, 113);
  }
  .status-badge.status-cancelled {
    background: rgba(148, 163, 184, 0.2);
    color: rgb(148, 163, 184);
  }
  .status-badge.status-complete {
    background: var(--tag-bg);
    color: var(--muted);
  }

  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: currentColor;
    display: inline-block;
  }
  .dot.pulsing {
    animation: pulse 1.2s ease-in-out infinite;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 1;
      transform: scale(1);
    }
    50% {
      opacity: 0.35;
      transform: scale(0.7);
    }
  }

  .running-elapsed {
    color: rgb(74, 222, 128);
    font-variant-numeric: tabular-nums;
  }

  tr.row-running {
    background: rgba(34, 197, 94, 0.04);
  }
  tr.row-error {
    background: rgba(239, 68, 68, 0.05);
  }

  .live-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    margin-bottom: 12px;
    border-radius: 8px;
    background: rgba(34, 197, 94, 0.08);
    border: 1px solid rgba(34, 197, 94, 0.25);
    color: rgb(74, 222, 128);
    font-size: 12px;
    font-weight: 500;
  }
  .live-banner .live-label {
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .thinking-badge.off {
    background: var(--tag-bg);
    color: var(--muted);
  }

  .thinking-block {
    margin: 0;
    padding: 12px 14px;
    border-radius: 8px;
    background: var(--code-bg, rgba(0, 0, 0, 0.25));
    color: var(--text);
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    font-size: 12px;
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 70vh;
    overflow: auto;
    border: 1px solid var(--border, rgba(255, 255, 255, 0.08));
  }

  .rerun-block {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 8px;
  }

  .rerun-actions {
    display: flex;
    justify-content: flex-end;
  }

  .detail-section {
    margin-bottom: 16px;
  }

  .detail-section h4 {
    margin: 0 0 10px;
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
  }

  .metrics-row {
    display: flex;
    gap: 24px;
    flex-wrap: wrap;
  }

  .metric {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .metric-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.3px;
  }

  .metric-value {
    font-size: 18px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
  }

  .section-toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: none;
    color: var(--ink);
    cursor: pointer;
    padding: 0;
    width: 100%;
  }

  .section-toggle h4 {
    margin: 0;
  }

  .chevron {
    transition: transform 0.2s;
    color: var(--muted);
  }

  .chevron.expanded {
    transform: rotate(180deg);
  }

  .empty-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 40px 24px;
    text-align: center;
    color: var(--muted);
    font-size: 14px;
  }

  .error-banner {
    padding: 12px 16px;
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
    border-radius: 12px;
    color: var(--danger);
    font-size: 13px;
    margin-bottom: 16px;
  }

  .error-banner.inner {
    margin: 16px 24px;
  }

  .compact {
    padding: 8px 14px;
    font-size: 12px;
  }
</style>
