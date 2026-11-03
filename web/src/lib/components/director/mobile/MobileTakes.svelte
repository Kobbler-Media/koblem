<script lang="ts">
  import type { DirectorTake } from '#lib/api/client.ts';

  // 🔴 A retake ADDS; it never overwrites. So the phone needs somewhere to compare and choose, not
  // just a retake button — the selected take is what the stitched cut is built from, and without a
  // picker every retake past the first is work with no way to reach it.
  let {
    takeEls = $bindable([]),
    takes = [],
    takeUrls = [],
    busy = false,
    unit,
    tier,
    criticNote = '',
    canPromote = false,
    promoteLabel = null,
    promoteBusy = false,
    onload,
    onpick,
    onpromote,
    onreview
  }: {
    /** The preview elements, so a promotion can take the frame the author actually scrubbed to. */
    takeEls?: (HTMLVideoElement | null)[];
    takes?: DirectorTake[];
    takeUrls?: string[];
    busy?: boolean;
    unit: string;
    tier: 'final' | 'proxy';
    criticNote?: string;
    /** Somewhere for a promoted still to land. Absent ⇒ the control is not drawn at all. */
    canPromote?: boolean;
    promoteLabel?: string | null;
    promoteBusy?: boolean;
    onload: () => void;
    onpick: (variant: number) => void;
    onpromote: (variant: number, index: number) => void;
    onreview: (autoRetake: boolean) => void;
  } = $props();

  // The sheet is only mounted while it is open, so mounting IS the request. Reading on open rather
  // than keeping the list warm keeps a phone from spending a round trip per shot walked past.
  let asked = false;
  $effect(() => {
    if (asked) return;
    asked = true;
    onload();
  });

  const selected = $derived(takes.findIndex((t) => t.selected));
</script>

<div class="sheetbody">
  <p class="hint">
    Takes are non-destructive — a retake adds one, it does not overwrite. The selected take is what
    the stitched cut uses.
  </p>

  {#if busy && takes.length === 0}
    <p class="hint">Reading the {tier} takes…</p>
  {:else if takes.length === 0}
    <p class="hint">
      No {tier} takes for this {unit} yet. Render it once and every roll after that lands here.
    </p>
  {:else}
    <div class="mlist">
      {#each takes as take, ti (take.variant)}
        <button
          class="mrow"
          type="button"
          aria-current={take.selected}
          disabled={busy || take.selected}
          onclick={() => onpick(take.variant)}
        >
          <span class="gl" aria-hidden="true">▣</span>
          <span class="tx">
            <!-- Numbered by POSITION: `variant` is a stable id that a full run keeps counting from,
                 so it drifts away from what is on screen the moment a tier is replaced. -->
            <b>Take {ti + 1}{take.selected ? ' · in the cut' : ''}</b>
            <span>
              {#if take.seed != null && take.seed >= 0}seed {take.seed} ·
              {/if}{take.critic
                ? `critic ${take.critic.score.toFixed(2)} · ${take.critic.verdict}`
                : 'not judged'}
            </span>
          </span>
          <span class="ch" aria-hidden="true">{take.selected ? '✓' : '›'}</span>
        </button>
      {/each}
    </div>

    {#each takes as take, ti (take.variant)}
      {#if takeUrls[ti]}
        <div class="tile" style="width:100%">
          <!-- svelte-ignore a11y_media_has_caption -->
          <video
            class="tv"
            bind:this={takeEls[ti]}
            src={takeUrls[ti]}
            controls
            playsinline
            preload="metadata"
          ></video>
          <div class="cap">
            <span>take {ti + 1}</span>
            {#if canPromote}
              <button
                class="btn sm"
                type="button"
                disabled={promoteBusy}
                onclick={() => onpromote(take.variant, ti)}
                >{promoteBusy ? 'Promoting…' : 'Use this frame'}</button
              >
            {/if}
          </div>
        </div>
      {/if}
    {/each}

    {#if canPromote}
      <p class="hint">
        Pause on a frame, then promote it{promoteLabel ? ` to ${promoteLabel}` : ''} — the cheapest way
        there is to hold an identity across a cut, and it costs no render.
      </p>
    {/if}

    <!-- Inside the has-takes branch: the critic judges footage, and its re-roll arm SPENDS a render
         per failure. Offering it over an empty list is an expensive button with nothing to judge. -->
    <div class="grp">
      <p class="eye">The vision critic</p>
      <button class="btn full" type="button" onclick={() => onreview(false)}>Dailies review</button>
      <button class="btn full" type="button" onclick={() => onreview(true)}
        >Review and re-roll failures</button
      >
      <p class="hint">
        Re-rolling adds a {tier} take per failed {unit}; nothing already rendered is replaced.
        Verdicts land on the rows above as they arrive.
      </p>
    </div>
  {/if}

  {#if criticNote}
    <p class="hint">{criticNote}</p>
  {/if}

  <button class="btn full ghost" type="button" disabled={busy} onclick={onload}
    >{busy ? 'Reading…' : 'Refresh'}</button
  >
  {#if selected >= 0}
    <p class="hint">Take {selected + 1} is the one the cut plays at {tier}.</p>
  {/if}
</div>
