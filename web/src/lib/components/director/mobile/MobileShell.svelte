<script lang="ts">
  import type { Snippet } from 'svelte';
  import { MOBILE_TABS, type MobileSheetDef, type MobileTab } from '#lib/director/mobile.ts';

  // Chrome and router only. Every screen arrives as a snippet rendered by `DirectorWorkspace`, so
  // the ~180 `$state` declarations that make up a project stay in the one component that owns them
  // and this file never sees a clip.
  let {
    tab = $bindable('shots'),
    sheet = $bindable(null),
    title,
    subtitle,
    saveState,
    job = null,
    toast = null,
    sheets = [],
    shots,
    prompt,
    render,
    more,
    onchat,
    ontitle,
    onundo,
    ondismisstoast,
    onjob
  }: {
    tab?: MobileTab;
    /** The pushed screen, or `null` for the tab body. Bindable so the workspace can push one too. */
    sheet?: string | null;
    title: string;
    subtitle: string;
    saveState: 'saved' | 'saving' | 'dirty' | 'stale';
    /** The render in flight, if this project owns one. Drives the strip above the tab bar. */
    job?: { label: string; pct: number } | null;
    /** The last undoable AI change, mirroring the desktop's `.ai-toast`. */
    toast?: { label: string; detail: string } | null;
    sheets?: MobileSheetDef[];
    shots: Snippet;
    prompt: Snippet;
    render: Snippet;
    more: Snippet;
    onchat: () => void;
    /**
     * Tapping the project name.
     *
     * 🔴 The switcher used to be reachable ONLY through More → Projects → Switch project, which is
     * three taps and two screens away from the thing it acts on — while the project's NAME sits in
     * the app bar on every screen, doing nothing. The name is where somebody reaches for "which
     * project am I in", so it is where the answer belongs; the More row stays, because that is where
     * Duplicate and the project list's other business live.
     *
     * Only offered on the tab bodies: inside a pushed sheet the app bar shows the SHEET's title, and
     * a tap there would open a switcher from a heading that does not name a project.
     */
    ontitle: () => void;
    onundo: () => void;
    ondismisstoast: () => void;
    onjob: () => void;
  } = $props();

  let bodyEl = $state<HTMLDivElement | null>(null);
  const open = $derived(sheet ? (sheets.find((s) => s.id === sheet) ?? null) : null);

  // A `sheet` that names nothing would strand the user on an empty body with a back button — the
  // workspace can close a sheet by removing its definition (no takes left, engine switched away).
  $effect(() => {
    if (sheet && !open) sheet = null;
  });

  // A pushed screen and a tab switch both start at the top. Without this, opening a sheet from the
  // bottom of a long Prompt screen lands mid-document with no indication anything moved.
  $effect(() => {
    void tab;
    void sheet;
    bodyEl?.scrollTo({ top: 0 });
  });

  const saveTitle = {
    saved: 'All changes saved',
    saving: 'Saving…',
    dirty: 'Unsaved changes',
    stale: 'Changed elsewhere — not saving'
  };
</script>

<div class="dwm">
  <div class="pbar">
    {#if open}
      <button class="back" type="button" aria-label="Back" onclick={() => (sheet = null)}>‹</button>
      <!-- A pushed sheet's own heading, and NOT a switcher: a tap on "Takes" or "Edit block" opening
           the project list would be a control that means something different on every screen. -->
      <div class="ttl">
        <b>{open.title}</b>
        <span>{open.subtitle ?? subtitle}</span>
      </div>
    {:else}
      <!-- The caret is a SIBLING of the text stack, not inside the name: the name ellipsizes, and a
           caret inside it would be the first thing clipped — losing the affordance exactly on the
           long project names that need it most. -->
      <button class="ttl" type="button" onclick={ontitle} aria-label="Switch or manage projects">
        <span class="tstack">
          <b>{title}</b>
          <span>{subtitle}</span>
        </span>
        <span class="tcar" aria-hidden="true">▾</span>
      </button>
    {/if}
    <span
      class="savedot"
      class:busy={saveState === 'saving'}
      class:dirty={saveState === 'dirty'}
      class:bad={saveState === 'stale'}
      title={saveTitle[saveState]}
      aria-label={saveTitle[saveState]}
    ></span>
    <button class="ico" type="button" aria-label="Chat with the director" onclick={onchat}>✦</button
    >
  </div>

  <div class="pbody" class:bare={open?.bare} bind:this={bodyEl}>
    {#if open}
      {@render open.body()}
    {:else if tab === 'shots'}
      {@render shots()}
    {:else if tab === 'prompt'}
      {@render prompt()}
    {:else if tab === 'render'}
      {@render render()}
    {:else}
      {@render more()}
    {/if}
  </div>

  <!-- One dock, so the toast floats above whatever the dock currently is — the job strip appears and
       disappears mid-render, and a toast pinned to a fixed offset would sit on top of it. -->
  <div class="pdock">
    {#if toast}
      <div class="toast" role="status">
        <span class="tt"><b>↩ {toast.label}</b><span>{toast.detail}</span></span>
        <button class="btn sm" type="button" onclick={onundo}>Undo</button>
        <button class="btn sm ghost" type="button" onclick={ondismisstoast} aria-label="Dismiss"
          >✕</button
        >
      </div>
    {/if}

    {#if job}
      <button class="pjob" type="button" onclick={onjob}>
        <span class="spin"></span>
        <span class="txt"><b>Rendering</b> · {job.label}</span>
        <span class="pct">{Math.round(job.pct)}%</span>
      </button>
    {/if}

    <nav class="ptabs" class:muted={!!open} aria-label="Director sections">
      {#each MOBILE_TABS as t (t.id)}
        <button
          type="button"
          aria-pressed={tab === t.id}
          onclick={() => {
            sheet = null;
            tab = t.id;
          }}
        >
          <span class="gl" aria-hidden="true">{t.glyph}</span><span class="lb">{t.label}</span>
        </button>
      {/each}
    </nav>
  </div>
</div>
