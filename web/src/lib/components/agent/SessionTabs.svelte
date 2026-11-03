<script lang="ts">
  import type { AgentSessionSummary } from '#lib/api/client.ts';

  let {
    sessions,
    activeId,
    creating = false,
    onSelect,
    onCreate,
    onDelete
  }: {
    sessions: AgentSessionSummary[];
    activeId: string | null;
    creating?: boolean;
    onSelect: (id: string) => void;
    onCreate: () => void;
    onDelete: (session: AgentSessionSummary) => void;
  } = $props();

  function label(s: AgentSessionSummary): string {
    return s.title?.trim() || 'Untitled';
  }
</script>

<div class="tabs">
  {#each sessions as s (s.id)}
    <div class="tab" class:on={s.id === activeId}>
      <button class="tab-label" type="button" title={label(s)} onclick={() => onSelect(s.id)}>
        {label(s)}
      </button>
      <button
        class="tab-close"
        type="button"
        title="Delete this chat"
        aria-label="Delete chat {label(s)}"
        onclick={() => onDelete(s)}>×</button
      >
    </div>
  {/each}
  <button class="tab new" type="button" title="New chat" onclick={onCreate} disabled={creating}>
    {creating ? '…' : '+ New'}
  </button>
</div>

<style>
  .tabs {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 16px;
  }

  .tab {
    display: flex;
    align-items: center;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-2);
    color: var(--muted);
    font-size: 13px;
    overflow: hidden;
    max-width: 220px;
  }

  .tab.on {
    color: var(--accent);
    border-color: var(--accent);
  }

  .tab-label {
    background: transparent;
    border: none;
    color: inherit;
    font-size: 13px;
    padding: 8px 4px 8px 14px;
    cursor: pointer;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }

  .tab-close {
    background: transparent;
    border: none;
    color: var(--muted);
    font-size: 16px;
    line-height: 1;
    padding: 0 10px 0 4px;
    cursor: pointer;
    align-self: stretch;
  }

  .tab-close:hover {
    color: var(--danger);
  }

  .tab.new {
    padding: 8px 16px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: var(--bg-2);
    color: var(--muted);
    font-size: 13px;
    cursor: pointer;
  }

  .tab.new:hover:not(:disabled) {
    color: var(--ink);
    border-color: var(--muted);
  }

  .tab.new:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
