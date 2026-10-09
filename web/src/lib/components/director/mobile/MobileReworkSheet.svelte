<script lang="ts">
  // Rework and Draft have very different blast radii and looked identical as two adjacent buttons,
  // so rework gets a sheet that asks for the instruction and states what it will not touch.
  let {
    instruction = $bindable(''),
    shotLabel,
    unit,
    busy,
    hasPrompt,
    onrework
  }: {
    instruction: string;
    shotLabel: string;
    unit: string;
    busy: boolean;
    hasPrompt: boolean;
    onrework: () => void;
  } = $props();
</script>

<div class="sheetbody">
  <p class="hint">
    Rework edits the prompt <b>in place</b> — it keeps what is there and changes only what you ask for.
    Unlike Draft, it does not start over.
  </p>
  <div class="grp">
    <p class="eye">What should change</p>
    <textarea
      class="inp"
      rows="4"
      bind:value={instruction}
      placeholder="Make the light colder and lose the crane."></textarea>
    <button
      class="btn primary full"
      type="button"
      onclick={onrework}
      disabled={busy || !instruction.trim()}>{busy ? 'Reworking…' : '⌁ Rework the prompt'}</button
    >
  </div>
  <p class="hint">
    {#if hasPrompt}
      Applies to {shotLabel} only. Use Chat to rework several {unit}s at once.
    {:else}
      This {unit} has no prompt yet, so there is nothing to edit in place — the instruction will be written
      from scratch. Draft it from an idea first if you want a starting point.
    {/if}
  </p>
</div>
