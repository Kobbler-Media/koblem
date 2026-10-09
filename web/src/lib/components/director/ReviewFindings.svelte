<script lang="ts">
  import type { ReviewItem } from '$lib/director/review';

  // Continuity findings from the script review. Each is keyed to its clip's stable id, so a finding
  // survives a reorder or a delete between review and fix — a removed shot says so rather than
  // silently pointing at whatever took its place.
  let {
    findings,
    fixingAll,
    shotIndexOf,
    onclose,
    ongoto,
    onapply,
    onfixall,
    ondismiss
  }: {
    findings: ReviewItem[];
    fixingAll: boolean;
    /** Raw timeline position of a clip, or -1 when it is gone. */
    shotIndexOf: (clipId: string) => number;
    onclose: () => void;
    ongoto: (clipId: string) => void;
    onapply: (item: ReviewItem) => void;
    onfixall: () => void;
    ondismiss: (item: ReviewItem) => void;
  } = $props();
</script>

<div class="overlay" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="modal review-modal">
    <div class="chat-h">
      <div class="chat-badge">⚑</div>
      <div style="flex:1">
        <b>Review findings</b><small
          >{findings.length} issue{findings.length === 1 ? '' : 's'}</small
        >
      </div>
      {#if findings.filter((f) => f.clipId !== null).length > 1}<button
          class="btn sm"
          onclick={onfixall}
          disabled={fixingAll}
          >{fixingAll
            ? 'Fixing all…'
            : `Auto-fix all (${findings.filter((f) => f.clipId !== null).length})`}</button
        >{/if}
      <button class="btn icon-only ghost" onclick={onclose}>✕</button>
    </div>
    <div class="review-list">
      {#if findings.length === 0}
        <p class="hint">No issues found — the script reads clean.</p>
      {/if}
      {#each findings as f (f)}
        {@const shotPos = f.clipId ? shotIndexOf(f.clipId) : -1}
        <div class="rev-card">
          <div class="top">
            <span class="sev {f.severity === 'low' ? 'info' : 'warn'}">{f.severity}</span>
            {#if f.clipId && shotPos >= 0}<button
                class="shotlink"
                onclick={() => f.clipId && ongoto(f.clipId)}>Shot {shotPos + 1} →</button
              >{:else}<span class="tl-meta">{f.clipId ? 'Shot (removed)' : 'Whole story'}</span
              >{/if}
          </div>
          <p class="issue">{f.issue}</p>
          {#if f.clipId !== null && shotPos >= 0}
            <div class="fixrow">
              <input bind:value={f.notes} placeholder="add direction for the fix — optional…" />
              <button class="btn sm primary" onclick={() => onapply(f)} disabled={f.fixing}
                >{f.fixing ? 'Fixing…' : 'Apply fix'}</button
              >
              <button class="btn sm ghost" onclick={() => ondismiss(f)}>Dismiss</button>
            </div>
          {:else}
            <div class="fixrow">
              <button class="btn sm ghost" onclick={() => ondismiss(f)}>Dismiss</button>
            </div>
          {/if}
        </div>
      {/each}
    </div>
  </div>
</div>
