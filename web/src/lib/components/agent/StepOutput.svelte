<script lang="ts">
  import JsonBlock from '$lib/components/JsonBlock.svelte';
  import type { AgentStepOutput } from '$lib/api/client';

  let {
    output,
    tool,
    onOpenImage
  }: {
    output: AgentStepOutput;
    tool: string;
    onOpenImage: (src: string) => void;
  } = $props();

  // A text output is still a URL on the wire, so it has to be pulled down
  // before it can be shown. The token travels in the URL, hence a bare fetch.
  let text = $state<string | null>(null);
  let textError = $state('');
  let fetched = '';

  $effect(() => {
    if (output.kind !== 'text' || fetched === output.url) return;
    fetched = output.url;
    const url = output.url;
    void (async () => {
      try {
        const res = await fetch(url);
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const body = await res.text();
        if (fetched === url) text = body;
      } catch (e) {
        if (fetched === url) {
          textError = e instanceof Error ? e.message : 'failed to load';
        }
      }
    })();
  });
</script>

{#if output.kind === 'image'}
  <button
    class="img-btn"
    type="button"
    title="View full size"
    onclick={() => onOpenImage(output.url)}
  >
    <img class="out-image" src={output.url} alt="Result from {tool}" />
  </button>
{:else if output.kind === 'video'}
  <!-- svelte-ignore a11y_media_has_caption -->
  <video class="out-video" controls preload="metadata" src={output.url}></video>
{:else if output.kind === 'audio'}
  <audio class="out-audio" controls preload="none" src={output.url}></audio>
{:else if text !== null}
  <JsonBlock content={text} maxHeight="280px" label="Output" />
{:else if textError}
  <p class="out-text-error">Could not load text output ({textError}).</p>
{:else}
  <p class="out-text-loading">Loading text output…</p>
{/if}

<style>
  .img-btn {
    display: block;
    padding: 0;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--bg-2);
    cursor: zoom-in;
    overflow: hidden;
    line-height: 0;
  }

  .img-btn:hover {
    border-color: var(--accent);
  }

  .out-image {
    display: block;
    max-width: 100%;
    max-height: 340px;
    width: auto;
    height: auto;
  }

  .out-video {
    display: block;
    max-width: 100%;
    max-height: 340px;
    border-radius: 10px;
    border: 1px solid var(--ring);
    background: #000;
  }

  .out-audio {
    width: 100%;
    max-width: 420px;
    height: 34px;
  }

  .out-text-error,
  .out-text-loading {
    margin: 0;
    font-size: 12px;
    color: var(--muted);
  }

  .out-text-error {
    color: var(--danger);
  }
</style>
