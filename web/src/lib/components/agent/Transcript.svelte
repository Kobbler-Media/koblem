<script lang="ts">
  import MessageCard from './MessageCard.svelte';
  import type { AgentMessage } from '#lib/api/client.ts';

  let {
    messages,
    loading = false,
    onOpenImage
  }: {
    messages: AgentMessage[];
    loading?: boolean;
    onOpenImage: (src: string) => void;
  } = $props();

  let el: HTMLDivElement | undefined = $state();
  let pinned = true;

  function onScroll() {
    if (!el) return;
    pinned = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  }

  // Steps mutate in place as a turn runs, so the signature has to reach into
  // them — message count alone never changes during a long tool call.
  const signature = $derived(
    messages
      .map(
        (m) =>
          `${m.id}:${m.status}:${m.content.length}:${m.steps
            .map((s) => `${s.state}/${s.detail.length}/${s.outputs.length}`)
            .join(',')}`
      )
      .join('|')
  );

  $effect(() => {
    void signature;
    if (!el || !pinned) return;
    const node = el;
    requestAnimationFrame(() => {
      node.scrollTop = node.scrollHeight;
    });
  });
</script>

<div class="transcript" bind:this={el} onscroll={onScroll}>
  {#if messages.length === 0}
    <div class="empty">
      <svg viewBox="0 0 24 24" width="44" height="44" fill="currentColor" class="empty-icon">
        <path
          d="M20 2H4c-1.1 0-2 .9-2 2v18l4-4h14c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2zm0 14H6l-2 2V4h16v12z"
        ></path>
      </svg>
      {#if loading}
        <p>Loading conversation…</p>
      {:else}
        <p>
          Say what you want and the agent picks the engine. Ask for an image, a voice line, a song,
          a video, or drop in a picture and ask about it.
        </p>
      {/if}
    </div>
  {:else}
    <div class="messages">
      {#each messages as message (message.id)}
        <MessageCard {message} {onOpenImage} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .transcript {
    flex: 1 1 0;
    min-height: 220px;
    overflow-y: auto;
    padding-bottom: 16px;
  }

  .messages {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 14px;
    padding: 56px 24px;
    text-align: center;
    color: var(--muted);
    height: 100%;
  }

  .empty-icon {
    opacity: 0.2;
  }

  .empty p {
    max-width: 420px;
    margin: 0;
    font-size: 14px;
    line-height: 1.55;
  }
</style>
