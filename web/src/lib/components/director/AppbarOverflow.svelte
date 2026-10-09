<script lang="ts">
  import OverflowSheet from './OverflowSheet.svelte';

  let {
    engineHealthy,
    saveStatus,
    shotCount,
    totalDuration,
    expectedSec,
    onsettings,
    onrenders,
    onprompts
  }: {
    engineHealthy: boolean;
    saveStatus: string;
    shotCount: number;
    totalDuration: number;
    expectedSec: number;
    onsettings: () => void;
    onrenders: () => void;
    /** The Prompts screen, or `null` on an engine without a structured prompt. See `AppBar`. */
    onprompts: (() => void) | null;
  } = $props();
</script>

<OverflowSheet
  btnClass="more-btn"
  label="More actions"
  title="Renders, settings and project status"
>
  {#snippet children(close: () => void)}
    <div class="more-status">
      <span class="engine" class:off={!engineHealthy}>
        <span class="edot"></span>{engineHealthy ? 'Engine ready' : 'Engine offline'}
      </span>
      <span class="runtime">
        {shotCount} shots · <b>{totalDuration.toFixed(1)}s</b>{#if expectedSec > 0}
          · full cut <b>~{expectedSec.toFixed(0)}s</b>{/if}
      </span>
      <span class="more-saved">{saveStatus || 'Ready'}</span>
    </div>
    <button
      class="btn ghost"
      onclick={() => {
        close();
        onrenders();
      }}>Renders</button
    >
    <button
      class="btn ghost"
      onclick={() => {
        close();
        onsettings();
      }}>Settings</button
    >
    {#if onprompts}
      <!-- `?.` rather than a bare call: the if-block narrows the read above, but TypeScript drops
           that narrowing inside a closure over a `let`-bound prop, and svelte-check reports it. -->
      <button
        class="btn ghost"
        onclick={() => {
          close();
          onprompts?.();
        }}>Prompts</button
      >
    {/if}
  {/snippet}
</OverflowSheet>
