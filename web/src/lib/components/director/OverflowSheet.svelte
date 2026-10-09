<script lang="ts">
  import type { Snippet } from 'svelte';

  // A ⋯ button that opens a bottom-anchored sheet of the actions a narrow bar had to shed. The
  // sheet's own chrome lives in workspace.css (`.more-menu`), because the body is a snippet the
  // PARENT owns and component-scoped styles would not reach it.
  let {
    btnClass,
    label,
    title,
    children
  }: {
    /** Which bar this belongs to — workspace.css decides at what width the button appears. */
    btnClass: string;
    label: string;
    title: string;
    children: Snippet<[() => void]>;
  } = $props();

  let open = $state(false);
  const close = () => (open = false);

  function onKeydown(e: KeyboardEvent) {
    if (open && e.key === 'Escape') open = false;
  }
</script>

<svelte:window onkeydown={onKeydown} />

<button
  class="btn ghost {btnClass}"
  aria-haspopup="dialog"
  aria-expanded={open}
  aria-label={label}
  {title}
  onclick={() => (open = !open)}>⋯</button
>

{#if open}
  <div class="overlay" role="presentation" onclick={(e) => e.target === e.currentTarget && close()}>
    <div class="more-menu">
      {@render children(close)}
      <button class="btn ghost" onclick={close}>Close</button>
    </div>
  </div>
{/if}
