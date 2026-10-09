<script lang="ts">
  import StepRow from './StepRow.svelte';
  import type { AgentMessage } from '$lib/api/client';

  let {
    message,
    onOpenImage
  }: {
    message: AgentMessage;
    onOpenImage: (src: string) => void;
  } = $props();

  const images = $derived(message.attachments.filter((a) => a.kind === 'image'));
  const audio = $derived(message.attachments.filter((a) => a.kind === 'audio'));
  const running = $derived(message.status === 'running');
  const idle = $derived(running && !message.content.trim() && message.steps.length === 0);

  function when(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleTimeString();
  }
</script>

<article class="msg" data-role={message.role} data-status={message.status}>
  <header class="msg-head">
    <span class="who">{message.role === 'user' ? 'You' : 'Agent'}</span>
    {#if running}
      <span class="live"><span class="spinner"></span> working</span>
    {/if}
    <span class="time">{when(message.created_at)}</span>
  </header>

  {#if images.length > 0}
    <div class="att-images">
      {#each images as att, i (i)}
        <button
          class="att-img-btn"
          type="button"
          title="View full size"
          onclick={() => onOpenImage(att.url)}
        >
          <img src={att.url} alt="Attachment {i + 1}" />
        </button>
      {/each}
    </div>
  {/if}

  {#if audio.length > 0}
    <div class="att-audio">
      {#each audio as att, i (i)}
        <audio controls preload="none" src={att.url}></audio>
      {/each}
    </div>
  {/if}

  {#if message.steps.length > 0}
    <ul class="steps">
      {#each message.steps as step (step.index)}
        <StepRow {step} {onOpenImage} />
      {/each}
    </ul>
  {/if}

  {#if message.content.trim()}
    <p class="body">{message.content}</p>
  {:else if idle}
    <p class="body muted-body">Thinking…</p>
  {/if}

  {#if message.status === 'error'}
    <p class="status-note error">
      This turn failed. {message.content.trim() ? '' : 'The agent did not report a reason.'}
    </p>
  {:else if message.status === 'cancelled'}
    <p class="status-note cancelled">Cancelled.</p>
  {/if}
</article>

<style>
  .msg {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 14px;
    padding: 12px 16px;
  }

  .msg[data-role='user'] {
    background: var(--bg-2);
    border-left: 3px solid var(--accent);
  }

  .msg[data-status='error'] {
    border-color: rgba(239, 68, 68, 0.45);
  }

  .msg-head {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 8px;
  }

  .who {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.6px;
    color: var(--muted);
  }

  .msg[data-role='assistant'] .who {
    color: var(--accent);
  }

  .time {
    margin-left: auto;
    font-size: 11px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .live {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    color: var(--accent);
  }

  .spinner {
    display: inline-block;
    width: 10px;
    height: 10px;
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

  .body {
    margin: 0;
    font-size: 14px;
    line-height: 1.55;
    color: var(--ink);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .steps + .body {
    margin-top: 12px;
  }

  .muted-body {
    color: var(--muted);
  }

  .steps {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin: 0;
    padding: 0;
  }

  .att-images {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 10px;
  }

  .att-img-btn {
    padding: 0;
    border: 1px solid var(--ring);
    border-radius: 8px;
    overflow: hidden;
    background: var(--bg-2);
    cursor: zoom-in;
    line-height: 0;
  }

  .att-img-btn:hover {
    border-color: var(--accent);
  }

  .att-img-btn img {
    display: block;
    max-width: 160px;
    max-height: 160px;
  }

  .att-audio {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 10px;
  }

  .att-audio audio {
    width: 100%;
    max-width: 360px;
    height: 34px;
  }

  .status-note {
    margin: 10px 0 0;
    padding: 8px 12px;
    border-radius: 10px;
    font-size: 13px;
  }

  .status-note.error {
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
    color: var(--danger);
  }

  .status-note.cancelled {
    background: rgba(0, 0, 0, 0.14);
    border: 1px solid var(--ring);
    color: var(--muted);
  }

  :global([data-theme='light']) .status-note.cancelled {
    background: rgba(0, 0, 0, 0.03);
  }
</style>
