<script lang="ts">
  import { onMount } from 'svelte';
  import { getHealth, getCorrections, getSettings } from '#lib/api/client.ts';

  type GpuService = {
    key: 'vision' | 'tts' | 'stt';
    label: string;
    healthy: boolean;
    model: string;
  };

  let llmStatus = $state<string>('checking...');
  let llmModel = $state('');
  let libraryStatus = $state<string>('checking...');
  let correctionsCount = $state(0);
  let gpuServices = $state<GpuService[]>([
    { key: 'vision', label: 'Vision', healthy: false, model: '' },
    { key: 'tts', label: 'TTS', healthy: false, model: '' },
    { key: 'stt', label: 'STT', healthy: false, model: '' }
  ]);
  let loading = $state(true);

  onMount(async () => {
    const results = await Promise.allSettled([getHealth(), getCorrections(), getSettings()]);

    if (results[0].status === 'fulfilled') {
      const health = results[0].value;
      llmStatus = health.status || 'ok';
      llmModel = health.llama_model || '';
      gpuServices = [
        {
          key: 'vision',
          label: 'Vision',
          healthy: !!health.vision,
          model: health.vision_model || ''
        },
        {
          key: 'tts',
          label: 'TTS',
          healthy: !!health.tts,
          model: health.tts_model || ''
        },
        { key: 'stt', label: 'STT', healthy: !!health.stt, model: health.stt_model || '' }
      ];
    } else {
      llmStatus = 'unreachable';
    }

    if (results[1].status === 'fulfilled') {
      const c = results[1].value;
      correctionsCount =
        c.generic_rules.length + c.entity_rules.length + c.decision_corrections.length;
    }

    if (results[2].status === 'fulfilled') {
      const settings = results[2].value;
      libraryStatus = settings.library_provider_url ? 'configured' : 'not configured';
    } else {
      libraryStatus = 'not configured';
    }

    loading = false;
  });
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Dashboard</h2>
  </div>

  {#if loading}
    <div class="loading-card">Loading status...</div>
  {:else}
    <div class="status-grid">
      <div class="status-card">
        <div
          class="status-icon"
          class:ok={llmStatus === 'ok' || llmStatus === 'ready'}
          class:error={llmStatus === 'unreachable'}
        >
          <svg viewBox="0 0 24 24" width="24" height="24" fill="currentColor">
            <path
              d="M21 10.12h-6.78l2.74-2.82c-2.73-2.7-7.15-2.8-9.88-.1-2.73 2.71-2.73 7.08 0 9.79s7.15 2.71 9.88 0C18.32 15.65 19 14.08 19 12.1h2c0 1.98-.88 4.55-2.64 6.29-3.51 3.48-9.21 3.48-12.72 0-3.5-3.47-3.53-9.11-.02-12.58s9.14-3.49 12.65 0L21 3v7.12z"
            ></path>
          </svg>
        </div>
        <div class="status-info">
          <span class="status-label">LLM Status</span>
          <span
            class="status-value"
            class:ok={llmStatus === 'ok' || llmStatus === 'ready'}
            class:error={llmStatus === 'unreachable'}
          >
            {llmStatus}
          </span>
          {#if llmModel}<span class="status-sub">{llmModel}</span>{/if}
        </div>
      </div>

      <div class="status-card gpu-card">
        <div
          class="status-icon"
          class:ok={gpuServices.every((s) => s.healthy)}
          class:warn={gpuServices.some((s) => !s.healthy)}
        >
          <svg viewBox="0 0 24 24" width="24" height="24" fill="currentColor">
            <path
              d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16zM12 4.15 18.64 8 12 11.85 5.36 8 12 4.15zM5 9.73l6 3.48v7.62l-6-3.43V9.73zm8 11.1v-7.62l6-3.48v7.67L13 20.83z"
            ></path>
          </svg>
        </div>
        <div class="status-info gpu-info">
          <span class="status-label">GPU Services</span>
          <div class="gpu-rows">
            {#each gpuServices as svc (svc.key)}
              <div class="gpu-row">
                <span
                  class="gpu-dot"
                  class:ok={svc.healthy}
                  class:fail={!svc.healthy}
                  aria-hidden="true"
                ></span>
                <span class="gpu-label">{svc.label}</span>
                {#if svc.model}
                  <span class="gpu-model" title={svc.model}>{svc.model}</span>
                {:else}
                  <span class="gpu-model muted">unreachable</span>
                {/if}
              </div>
            {/each}
          </div>
        </div>
      </div>

      <div class="status-card">
        <div
          class="status-icon"
          class:ok={libraryStatus === 'configured'}
          class:warn={libraryStatus === 'not configured'}
        >
          <svg viewBox="0 0 24 24" width="24" height="24" fill="currentColor">
            <path
              d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm-2 15l-5-5 1.41-1.41L10 14.17l7.59-7.59L19 8l-9 9z"
            ></path>
          </svg>
        </div>
        <div class="status-info">
          <span class="status-label">Library Provider</span>
          <span
            class="status-value"
            class:ok={libraryStatus === 'configured'}
            class:warn={libraryStatus === 'not configured'}
          >
            {libraryStatus}
          </span>
        </div>
      </div>

      <div class="status-card">
        <div class="status-icon neutral">
          <svg viewBox="0 0 24 24" width="24" height="24" fill="currentColor">
            <path
              d="M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zM20.71 7.04a.996.996 0 0 0 0-1.41l-2.34-2.34a.996.996 0 0 0-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z"
            ></path>
          </svg>
        </div>
        <div class="status-info">
          <span class="status-label">Corrections</span>
          <span class="status-value neutral">{correctionsCount} total</span>
        </div>
      </div>
    </div>

    <div class="quick-links">
      <h3>Quick Links</h3>
      <div class="link-cards">
        <a href="/playground" class="link-card">
          <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
            <path
              d="M9.4 16.6L4.8 12l4.6-4.6L8 6l-6 6 6 6 1.4-1.4zm5.2 0l4.6-4.6-4.6-4.6L16 6l6 6-6 6-1.4-1.4z"
            ></path>
          </svg>
          <span>Open Playground</span>
          <span class="link-desc">Test review decisions with custom candidates</span>
        </a>
        <a href="/settings" class="link-card">
          <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
            <path
              d="M19.14 12.94c.04-.3.06-.61.06-.94 0-.32-.02-.64-.07-.94l2.03-1.58a.49.49 0 0 0 .12-.61l-1.92-3.32a.49.49 0 0 0-.59-.22l-2.39.96c-.5-.38-1.03-.7-1.62-.94l-.36-2.54a.484.484 0 0 0-.48-.41h-3.84c-.24 0-.43.17-.47.41l-.36 2.54c-.59.24-1.13.57-1.62.94l-2.39-.96a.49.49 0 0 0-.59.22L2.74 8.87c-.12.21-.08.47.12.61l2.03 1.58c-.05.3-.07.62-.07.94s.02.64.07.94l-2.03 1.58a.49.49 0 0 0-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32c.12-.22.07-.47-.12-.61l-2.01-1.58zM12 15.6c-1.98 0-3.6-1.62-3.6-3.6s1.62-3.6 3.6-3.6 3.6 1.62 3.6 3.6-1.62 3.6-3.6 3.6z"
            ></path>
          </svg>
          <span>Configure Settings</span>
          <span class="link-desc">Set up library provider connection</span>
        </a>
        <a href="/corrections" class="link-card">
          <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
            <path
              d="M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zM20.71 7.04a.996.996 0 0 0 0-1.41l-2.34-2.34a.996.996 0 0 0-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z"
            ></path>
          </svg>
          <span>Manage Corrections</span>
          <span class="link-desc">Add rules and fix model decisions</span>
        </a>
      </div>
    </div>
  {/if}
</div>

<style>
  .status-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
    gap: 16px;
    margin-bottom: 32px;
  }

  .status-card {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 20px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
  }

  .status-icon {
    width: 48px;
    height: 48px;
    border-radius: 12px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--tag-bg);
    color: var(--muted);
    flex-shrink: 0;
  }

  .status-icon.ok {
    background: rgba(34, 197, 94, 0.15);
    color: var(--success);
  }

  .status-icon.error {
    background: rgba(239, 68, 68, 0.15);
    color: var(--danger);
  }

  .status-icon.warn {
    background: rgba(249, 115, 22, 0.15);
    color: var(--warning);
  }

  .status-icon.neutral {
    background: var(--accent-dim);
    color: var(--accent);
  }

  .status-info {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .status-label {
    font-size: 12px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-weight: 600;
  }

  .status-value {
    font-size: 15px;
    font-weight: 600;
    text-transform: capitalize;
  }

  .status-value.ok {
    color: var(--success);
  }

  .status-value.error {
    color: var(--danger);
  }

  .status-value.warn {
    color: var(--warning);
  }

  .status-value.neutral {
    color: var(--ink);
  }

  .status-sub {
    font-size: 12px;
    color: var(--muted);
    font-family: monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .gpu-card {
    align-items: flex-start;
  }
  .gpu-info {
    min-width: 0;
    flex: 1;
  }
  .gpu-rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-top: 6px;
  }
  .gpu-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    min-width: 0;
  }
  .gpu-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .gpu-dot.ok {
    background: var(--success);
  }
  .gpu-dot.fail {
    background: var(--danger);
  }
  .gpu-label {
    font-weight: 600;
    color: var(--ink);
    width: 52px;
    flex-shrink: 0;
  }
  .gpu-model {
    color: var(--ink);
    font-family: monospace;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .gpu-model.muted {
    color: var(--muted);
    font-style: italic;
  }

  .quick-links h3 {
    font-size: 16px;
    margin: 0 0 12px;
  }

  .link-cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
    gap: 12px;
  }

  .link-card {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 20px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    text-decoration: none;
    color: var(--ink);
    transition:
      border-color 0.15s,
      transform 0.15s;
  }

  .link-card:hover {
    border-color: var(--accent);
    transform: translateY(-2px);
  }

  .link-card svg {
    color: var(--accent);
  }

  .link-card span:first-of-type {
    font-weight: 600;
    font-size: 14px;
  }

  .link-desc {
    font-size: 12px;
    color: var(--muted);
  }
</style>
