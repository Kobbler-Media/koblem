<script lang="ts">
  import type { RenderTier, TierPreference } from '#lib/director/tier.ts';

  let {
    pref,
    tier,
    narrow,
    onchange
  }: {
    pref: TierPreference;
    /** What `pref` resolves to on this screen — what will actually render and play. */
    tier: RenderTier;
    narrow: boolean;
    onchange: (next: TierPreference) => void;
  } = $props();

  const OPTIONS: { value: TierPreference; label: string }[] = [
    { value: 'auto', label: 'Auto' },
    { value: 'proxy', label: 'Preview' },
    { value: 'final', label: 'Final' }
  ];

  let note = $derived(
    pref === 'proxy'
      ? 'Preview only — 512×288, cheap to render and small to stream. Final renders stay untouched.'
      : pref === 'final'
        ? 'Full resolution — a finished shot is megabytes, and every playback fetches all of it.'
        : narrow
          ? 'Auto: this screen is narrow, so shots render and play as 512×288 previews.'
          : 'Auto: this screen is wide, so shots render and play at full resolution.'
  );
</script>

<div class="tiersel">
  <span class="lbl">Quality</span>
  <div class="opts" role="group" aria-label="Render and playback quality">
    {#each OPTIONS as o (o.value)}
      <button
        class="opt"
        class:on={pref === o.value}
        aria-pressed={pref === o.value}
        onclick={() => onchange(o.value)}>{o.label}</button
      >
    {/each}
  </div>
  <span class="now">{tier}</span>
  <p class="note">{note}</p>
</div>

<style>
  .tiersel {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 7px;
  }
  .lbl {
    font-size: 10px;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    font-weight: 600;
    color: var(--muted);
  }
  .opts {
    display: inline-flex;
    gap: 2px;
    padding: 2px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 9px;
  }
  .opt {
    border: none;
    background: none;
    color: var(--ink);
    border-radius: 6px;
    font-size: 11.5px;
    font-weight: 600;
    padding: 6px 10px;
    cursor: pointer;
    transition: 0.12s;
  }
  .opt:hover {
    background: var(--ring);
  }
  .opt.on {
    background: var(--accent-dim);
    color: var(--accent);
  }
  .now {
    font-family: var(--mono);
    font-size: 10.5px;
    color: var(--muted);
    border: 1px solid var(--ring);
    border-radius: 999px;
    padding: 2px 8px;
  }
  .note {
    flex: 1 1 100%;
    font-size: 11px;
    line-height: 1.5;
    color: var(--muted);
    margin: 0;
  }
  @media (pointer: coarse) {
    .opt {
      min-height: 40px;
    }
  }
</style>
