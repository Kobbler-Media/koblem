<script lang="ts">
  /*
   * What to do with the shell under the cursor, drawn at the shell.
   *
   * 🔴 THE SELECTION IS DRAWN BEFORE THE DELETE BUTTON IS PRESSED. That is the headline requirement
   * of this rebuild: "312 tris · 0.4% of the model" is either a floating speck of junk or the tip of
   * a finger, and no list of numbers can tell them apart. Hovering Delete escalates the highlight to
   * the danger tone, so the last thing you see before committing is exactly the geometry that goes.
   *
   * Deleting a piece rewrites only the index buffer — no vertex moves, so skin weights and UVs
   * cannot be damaged by it. That is why this is a button a user is allowed to press freely.
   */
  let {
    triangles,
    vertices,
    sizeRatio,
    largest = false,
    busy = false,
    onDelete,
    onPreview,
    onClose
  }: {
    triangles: number;
    vertices: number;
    /** Bounding-box diagonal as a fraction of the whole model's. */
    sizeRatio: number;
    /** The biggest connected component is the character itself and is never offered for deletion. */
    largest?: boolean;
    busy?: boolean;
    onDelete: () => void;
    /** Escalate/clear the danger-tone preview as the delete button takes and loses the pointer. */
    onPreview: (on: boolean) => void;
    onClose: () => void;
  } = $props();

  const share = $derived(sizeRatio < 0.001 ? '<0.1%' : `${(sizeRatio * 100).toFixed(1)}%`);
</script>

<div class="pop">
  <div class="head">
    <span class="count">{triangles.toLocaleString()} tris</span>
    <button class="x" onclick={onClose} aria-label="Close this piece's options">×</button>
  </div>
  <p class="stat">{vertices.toLocaleString()} vertices · {share} of the model</p>

  {#if largest}
    <p class="hint">
      This is the largest connected piece — the character. It is never offered for deletion; the
      strays are everything else.
    </p>
  {:else}
    <button
      class="btn-danger small"
      disabled={busy}
      onclick={onDelete}
      onpointerenter={() => onPreview(true)}
      onpointerleave={() => onPreview(false)}
      onfocus={() => onPreview(true)}
      onblur={() => onPreview(false)}
    >
      Delete this piece
    </button>
    <p class="hint">
      Index-only: the vertices stay in the file costing size and nothing else. Anything under a
      percent or so is almost certainly junk — but look at what is lit up before you press it.
    </p>
  {/if}
</div>

<style>
  .pop {
    width: 224px;
    box-sizing: border-box;
    padding: 10px 12px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 10px 30px rgb(0 0 0 / 45%);
    pointer-events: auto;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    justify-content: space-between;
  }
  .count {
    font-size: 13px;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .x {
    background: none;
    border: none;
    color: var(--muted);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    padding: 0 2px;
  }
  .x:hover {
    color: var(--ink);
  }
  .stat {
    margin: 4px 0 10px;
    color: var(--muted);
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }
  .hint {
    color: var(--muted);
    font-size: 10.5px;
    line-height: 1.4;
    margin: 8px 0 0;
  }
  .btn-danger.small {
    padding: 4px 10px;
    font-size: 11.5px;
  }
</style>
