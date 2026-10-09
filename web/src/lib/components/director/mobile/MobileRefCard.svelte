<script lang="ts">
  import { MOBILE_REF_GLYPH, type MobileRef } from '$lib/director/mobile';

  // What an ordinal points at, as one card. Three surfaces open it — the ordinal inside the composed
  // prompt, the thumbnail in the reference strip, and the ⓘ on the block editor's caret bar — and it
  // is one component so those three can never come to disagree about what `<Picture 2>` is.
  let {
    ref,
    body = null,
    usage = null,
    warn = null,
    onclose,
    onedit = undefined
  }: {
    /** `null` closes the card — the caller's own selection state is the open/closed flag. */
    ref: MobileRef | null;
    /** The prose that defines it: a subject definition, or the author's note on the reference. */
    body?: string | null;
    /** Where it is used, in words — `"whole project"` / `"this shot only"`. */
    usage?: string | null;
    /** Amber line. An ordinal naming nothing is the DESIGNED outcome of taking a reference out of
        scope, so it is stated rather than left as an empty card. */
    warn?: string | null;
    onclose: () => void;
    onedit?: (() => void) | undefined;
  } = $props();

  let cardEl = $state<HTMLDivElement | null>(null);

  /**
   * 🔴 Opening the card focuses it, which is what makes its Escape handler reachable at all — a
   * handler on an element nothing ever focuses is dead code, and the phone shell has no global
   * branch for this card. Focus goes back to the ordinal or thumbnail that opened it on close, so a
   * keyboard walk through the reference strip is not thrown to the top of the document.
   */
  $effect(() => {
    if (!ref) return;
    const from = document.activeElement as HTMLElement | null;
    cardEl?.focus();
    return () => {
      if (from?.isConnected) from.focus();
    };
  });
</script>

{#if ref}
  <!-- Closes only on the BACKDROP itself, so a tap inside the card needs no stopPropagation — which
       would also have swallowed the Escape the card handles. -->
  <div class="pop" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
    <div
      class="popcard"
      bind:this={cardEl}
      role="dialog"
      aria-modal="true"
      aria-label={ref.label}
      tabindex="-1"
      onkeydown={(e) => {
        if (e.key === 'Escape') onclose();
      }}
    >
      <div class="pi">
        {#if ref.image}<img src={ref.image} alt="" />{:else}<span class="gl" aria-hidden="true"
            >{MOBILE_REF_GLYPH[ref.kind]}</span
          >{/if}
        <span class="ord">{ref.label}</span>
      </div>
      <div class="pb">
        <b>{ref.name}</b>
        {#if body}<p>{body}</p>{/if}
        {#if warn}<p class="hint warn">{warn}</p>{/if}
        <div class="pf">
          <span class="pill">{ref.role}</span>
          {#if ref.scoped}<span class="pill cut"><i></i>scoped</span>{/if}
          {#if usage}<span>{usage}</span>{/if}
        </div>
      </div>
      <div class="pact">
        <button class="btn sm" type="button" onclick={onclose}>Close</button>
        {#if onedit}
          <button class="btn sm" type="button" onclick={onedit}>Edit</button>
        {/if}
      </div>
    </div>
  </div>
{/if}
