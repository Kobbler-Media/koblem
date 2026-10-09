<script lang="ts">
  import { onDestroy } from 'svelte';
  import StepOutput from './StepOutput.svelte';
  import type { AgentStep } from '$lib/api/client';

  let {
    step,
    onOpenImage
  }: {
    step: AgentStep;
    onOpenImage: (src: string) => void;
  } = $props();

  // The wire format carries no per-step timestamps, so a running step is timed
  // from when this client first saw it running — enough to tell a stuck GPU
  // wait from a step that just started.
  let elapsed = $state(0);
  let timer: ReturnType<typeof setInterval> | null = null;
  let startedAt = 0;

  $effect(() => {
    const running = step.state === 'running';
    if (running && timer === null) {
      startedAt = Date.now();
      elapsed = 0;
      timer = setInterval(() => {
        elapsed = Math.round((Date.now() - startedAt) / 1000);
      }, 1000);
    } else if (!running && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  const clock = $derived(
    elapsed < 60
      ? `${elapsed}s`
      : `${Math.floor(elapsed / 60)}m ${String(elapsed % 60).padStart(2, '0')}s`
  );
</script>

<li class="step" data-state={step.state}>
  <div class="step-head">
    <span class="marker" aria-hidden="true">
      {#if step.state === 'running'}
        <span class="spinner"></span>
      {:else if step.state === 'done'}
        <svg viewBox="0 0 24 24" width="13" height="13" fill="currentColor">
          <path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z"></path>
        </svg>
      {:else if step.state === 'failed'}
        <svg viewBox="0 0 24 24" width="13" height="13" fill="currentColor">
          <path
            d="M19 6.4 17.6 5 12 10.6 6.4 5 5 6.4 10.6 12 5 17.6 6.4 19l5.6-5.6 5.6 5.6 1.4-1.4-5.6-5.6z"
          ></path>
        </svg>
      {:else}
        <span class="dot"></span>
      {/if}
    </span>
    <span class="tool">{step.tool}</span>
    {#if step.args_summary}
      <span class="args" title={step.args_summary}>{step.args_summary}</span>
    {/if}
    {#if step.state === 'running' && elapsed > 0}
      <span class="clock">{clock}</span>
    {/if}
    <span class="state-tag">{step.state}</span>
  </div>

  {#if step.detail}
    <p class="detail">{step.detail}</p>
  {/if}

  {#if step.outputs.length > 0}
    <div class="outputs">
      {#each step.outputs as out, i (i)}
        <StepOutput output={out} tool={step.tool} {onOpenImage} />
      {/each}
    </div>
  {/if}
</li>

<style>
  .step {
    list-style: none;
    border: 1px solid var(--ring);
    border-radius: 12px;
    padding: 10px 12px;
    background: rgba(0, 0, 0, 0.14);
  }

  :global([data-theme='light']) .step {
    background: rgba(0, 0, 0, 0.03);
  }

  .step[data-state='running'] {
    border-color: var(--accent);
  }

  .step[data-state='failed'] {
    border-color: rgba(239, 68, 68, 0.45);
  }

  .step-head {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }

  .marker {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    flex-shrink: 0;
    color: var(--muted);
  }

  .step[data-state='running'] .marker {
    color: var(--accent);
  }
  .step[data-state='done'] .marker {
    color: var(--success);
  }
  .step[data-state='failed'] .marker {
    color: var(--danger);
  }

  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    border: 1.5px solid currentColor;
  }

  .spinner {
    width: 12px;
    height: 12px;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 0.75s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .tool {
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    font-size: 12px;
    font-weight: 600;
    color: var(--ink);
    flex-shrink: 0;
  }

  .args {
    font-size: 12px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
    flex: 1;
  }

  .clock {
    font-size: 11px;
    color: var(--accent);
    font-variant-numeric: tabular-nums;
    flex-shrink: 0;
  }

  .state-tag {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 2px 7px;
    border-radius: 999px;
    border: 1px solid var(--ring);
    color: var(--muted);
    flex-shrink: 0;
    margin-left: auto;
  }

  .step[data-state='running'] .state-tag {
    color: var(--accent);
    border-color: var(--accent);
  }
  .step[data-state='done'] .state-tag {
    color: var(--success);
    border-color: rgba(34, 197, 94, 0.4);
  }
  .step[data-state='failed'] .state-tag {
    color: var(--danger);
    border-color: rgba(239, 68, 68, 0.4);
  }

  /* `detail` is the honest progress text ("waiting for GPU, LLM evicted by an
     image render") — it is the whole point of a step row, so it gets weight. */
  .detail {
    margin: 8px 0 0 24px;
    font-size: 13px;
    line-height: 1.45;
    color: var(--ink);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .step[data-state='failed'] .detail {
    color: var(--danger);
  }

  .outputs {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    gap: 10px;
    margin: 10px 0 0 24px;
  }

  .outputs > :global(*) {
    max-width: 100%;
  }
</style>
