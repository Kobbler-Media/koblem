<script lang="ts">
  import { formatBytes } from '@koblibs/ui';
  import { MAX_TURN_BYTES, MAX_TURN_FILES, budgetOf, type PendingAttachment } from './attachments';

  let {
    attachments,
    onRemove
  }: {
    attachments: PendingAttachment[];
    onRemove: (id: string) => void;
  } = $props();

  const images = $derived(attachments.filter((a) => a.kind === 'image'));
  const audio = $derived(attachments.filter((a) => a.kind === 'audio'));
  const budget = $derived(budgetOf(attachments));
  const tight = $derived(budget.bytes > MAX_TURN_BYTES * 0.8 || budget.count >= MAX_TURN_FILES - 2);
</script>

{#if images.length > 0}
  <div class="image-strip">
    {#each images as img (img.id)}
      <div class="image-chip">
        <img class="image-thumb" src={img.dataUrl} alt={img.name} />
        <button
          class="chip-remove"
          type="button"
          title="Remove {img.name}"
          aria-label="Remove {img.name}"
          onclick={() => onRemove(img.id)}>×</button
        >
        <div class="chip-meta">{formatBytes(img.size)}</div>
      </div>
    {/each}
  </div>
{/if}

{#if audio.length > 0}
  <div class="audio-strip">
    {#each audio as a (a.id)}
      <div class="audio-chip">
        <div class="audio-chip-head">
          <span class="audio-name" title={a.name}>{a.name}</span>
          <button
            class="chip-remove inline"
            type="button"
            title="Remove {a.name}"
            aria-label="Remove {a.name}"
            onclick={() => onRemove(a.id)}>×</button
          >
        </div>
        <audio class="audio-preview" controls src={a.dataUrl}></audio>
        <div class="chip-meta right">{formatBytes(a.size)}</div>
      </div>
    {/each}
  </div>
{/if}

{#if attachments.length > 0}
  <div class="budget" class:tight>
    <div class="budget-bar">
      <div
        class="budget-fill"
        style="width: {Math.min(100, (budget.bytes / MAX_TURN_BYTES) * 100)}%"
      ></div>
    </div>
    <span>
      {budget.count} of {MAX_TURN_FILES} files · {formatBytes(budget.bytes)} of {formatBytes(
        MAX_TURN_BYTES
      )}
    </span>
  </div>
{/if}

<style>
  .image-strip,
  .audio-strip {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-bottom: 10px;
  }

  .image-chip {
    position: relative;
    width: 96px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    overflow: hidden;
    background: rgba(0, 0, 0, 0.1);
  }

  .image-thumb {
    width: 96px;
    height: 96px;
    object-fit: cover;
    display: block;
  }

  .chip-remove {
    position: absolute;
    top: 4px;
    right: 4px;
    width: 22px;
    height: 22px;
    border-radius: 50%;
    border: none;
    background: rgba(0, 0, 0, 0.65);
    color: #fff;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
  }

  .chip-remove.inline {
    position: static;
    width: 20px;
    height: 20px;
    font-size: 15px;
    background: rgba(0, 0, 0, 0.45);
    flex-shrink: 0;
  }

  .chip-remove:hover {
    background: rgba(239, 68, 68, 0.85);
  }

  .chip-meta {
    padding: 3px 6px;
    font-size: 10px;
    color: var(--muted);
    text-align: center;
    font-variant-numeric: tabular-nums;
  }

  .chip-meta.right {
    text-align: right;
    padding: 0;
  }

  .audio-chip {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 10px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: rgba(0, 0, 0, 0.1);
    min-width: 240px;
    max-width: 320px;
  }

  :global([data-theme='light']) .audio-chip {
    background: rgba(0, 0, 0, 0.03);
  }

  .audio-chip-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }

  .audio-name {
    font-size: 12px;
    color: var(--ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .audio-preview {
    width: 100%;
    height: 34px;
  }

  .budget {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
    font-size: 11px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .budget-bar {
    flex: 0 0 120px;
    height: 4px;
    border-radius: 999px;
    background: var(--ring);
    overflow: hidden;
  }

  .budget-fill {
    height: 100%;
    background: var(--accent);
  }

  .budget.tight {
    color: var(--warning);
  }

  .budget.tight .budget-fill {
    background: var(--warning);
  }
</style>
