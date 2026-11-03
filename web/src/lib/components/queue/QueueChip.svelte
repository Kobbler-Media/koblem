<script lang="ts">
  /*
   * What pressing the render button will do to the queue, in one word, next to the button — and the
   * knobs that change it, behind that word.
   *
   * 🔴 It replaced a sentence and a disclosure BAR on every render form. Both were honest and both
   * were wrong for the space: "queues at #3 · starts ~14:20" beside a button is a paragraph in a
   * toolbar, and a full-width `Queue options` fold pushed the button down the page on eleven screens
   * to offer knobs that are turned on maybe one render in fifty. The sentence is still the authority
   * — it is the title, and it is the first line inside the popover — it just no longer costs a row.
   *
   * 🔴 Still never ON the button. Whether a job runs now is a prediction, and a verb that flipped
   * "Render" → "Queue" under the cursor would race the snapshot it was computed from.
   */
  import { onDestroy, onMount } from 'svelte';
  import { type GpuCard, type JobKind } from '#lib/api/client.ts';
  import { getQueueState, startQueueFeed } from '#lib/stores/queue.svelte.ts';
  import { BANDS, bandLabel, isLive, isPending } from '#lib/queue/format.ts';
  import {
    enqueueReceipt,
    submitConsequence,
    type EnqueueLike,
    type QueueSubmitOptions
  } from '#lib/queue/options.ts';
  import { cardOption, gpuCards } from '#lib/queue/cards.ts';

  let {
    kind,
    options = $bindable(),
    response = null
  }: {
    kind: JobKind;
    options: QueueSubmitOptions;
    /** The last 202, or null before one. What actually happened, which the prediction is not. */
    response?: EnqueueLike | null;
  } = $props();

  const queue = getQueueState();

  let open = $state(false);

  /**
   * Every control writes straight into {@link options}; the chip holds no copy of what is set.
   *
   * 🔴 The Director mounts three of these against ONE options object — the app bar, the Renders
   * drawer and the phone's render sheet — and the render they modify is the same render. A chip
   * with its own `runNow`/`band`/`gpu` state would show "server default" for a band another chip
   * had set, and put it back the moment anything else on that chip moved.
   *
   * An unset field has no KEY, not an empty value: `queueHeaders` sends no header for an absent
   * one, and the server is what owns the default.
   */
  function setOpt(patch: Partial<QueueSubmitOptions>) {
    const next: QueueSubmitOptions = { ...options, ...patch };
    if (!next.mode) delete next.mode;
    if (next.band == null) delete next.band;
    // Force has nothing to force without a pin, and the server reads it as a pin modifier.
    if (!next.gpu) {
      delete next.gpu;
      delete next.force;
    }
    if (!next.force) delete next.force;
    if (!next.after) delete next.after;
    options = next;
  }

  const runNow = $derived(options.mode === 'now');
  const bandChoice = $derived(options.band != null ? String(options.band) : '');
  const gpuChoice = $derived(options.gpu ?? '');
  const force = $derived(options.force ?? false);
  const afterChoice = $derived(options.after ?? '');

  let cards = $state<GpuCard[]>([]);
  let cardsError = $state('');
  let stopFeed: (() => void) | null = null;
  let host = $state<HTMLElement | null>(null);
  let btn = $state<HTMLButtonElement | null>(null);
  /**
   * Where the panel sits, in VIEWPORT coordinates.
   *
   * 🔴 Fixed rather than absolute, and this is the reason it is worth the arithmetic: the chip is
   * mounted in twelve different places, and three of them clip — the Renders drawer scrolls, the
   * pixal3d rig column scrolls, and the app bar wraps. An absolutely-positioned panel is a panel
   * whose bottom half is missing on exactly the screens with the least room for it.
   */
  const PANEL_W = 340;
  const MARGIN = 12;
  let at = $state({ left: 0, edge: 'top' as 'top' | 'bottom', offset: 0 });

  function place() {
    const r = btn?.getBoundingClientRect();
    if (!r) return;
    const width = Math.min(PANEL_W, window.innerWidth - MARGIN * 2);
    const below = window.innerHeight - r.bottom;
    // Flip up only when there is genuinely more room there — a panel pinned above a chip near the
    // top of the window is the same problem upside down.
    const above = below < 320 && r.top > below;
    at = {
      left: Math.max(MARGIN, Math.min(r.right - width, window.innerWidth - width - MARGIN)),
      edge: above ? 'bottom' : 'top',
      offset: above ? window.innerHeight - r.top + 6 : r.bottom + 6
    };
  }

  function toggle() {
    if (!open) place();
    open = !open;
  }

  onMount(() => {
    stopFeed = startQueueFeed();
  });
  onDestroy(() => stopFeed?.());

  async function loadCards() {
    if (cards.length > 0) return;
    cardsError = '';
    try {
      cards = await gpuCards();
    } catch (e) {
      cardsError = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    if (open) void loadCards();
  });

  const view = $derived(
    response ? enqueueReceipt(response) : submitConsequence(queue.snapshot, kind, options)
  );

  const waiting = $derived(
    queue.snapshot.jobs.filter((j) => isPending(j.state) || isLive(j.state))
  );

  const summary = $derived.by(() => {
    const parts: string[] = [];
    if (runNow) parts.push('run now');
    if (bandChoice) parts.push(bandLabel(Number(bandChoice)));
    if (gpuChoice) parts.push(force ? 'pinned (forced)' : 'pinned');
    if (afterChoice) parts.push('chained');
    return parts.join(' · ');
  });

  function clear() {
    options = {};
  }

  // Pointerdown rather than click: a click that lands on a control INSIDE the popover must not be
  // preceded by a close that unmounts the control out from under it.
  function onPointerDown(e: PointerEvent) {
    if (!open || !host) return;
    if (!host.contains(e.target as Node)) open = false;
  }
</script>

<svelte:window
  onpointerdown={onPointerDown}
  onresize={() => open && place()}
  onkeydown={(e) => {
    if (e.key === 'Escape' && open) open = false;
  }}
/>
<!-- Capture phase: the chip lives inside two scrolling panels, and a scroll event on either of them
     never bubbles to the window. -->
<svelte:document onscrollcapture={() => open && place()} />

<span class="qchip" bind:this={host}>
  <button
    type="button"
    class="qc-btn"
    class:attention={view.attention}
    class:said={!!response}
    class:tuned={!!summary}
    aria-expanded={open}
    aria-label="Queue: {view.text}. Open queue options."
    title={summary ? `${view.text} — ${summary}` : view.text}
    onclick={toggle}
    bind:this={btn}
  >
    <span class="qc-glyph" aria-hidden="true">{view.immediate ? '⚡' : '▤'}</span>
    <span class="qc-short">{view.short}</span>
  </button>

  {#if open}
    <div class="qc-pop" style="left:{at.left}px; {at.edge}:{at.offset}px">
      <!-- The sentence the chip abbreviates, in full, at the top of the thing you opened to read
           about it. -->
      <p class="qc-said" class:attention={view.attention}>{view.text}</p>

      <div class="qc-grid">
        <label>
          Priority band
          <select bind:value={() => bandChoice, (v) => setOpt({ band: v ? Number(v) : undefined })}>
            <option value="">server default</option>
            {#each BANDS as band (band)}
              <option value={String(band)}>{bandLabel(band)}</option>
            {/each}
          </select>
        </label>

        <label>
          Pin to card
          <select bind:value={() => gpuChoice, (v) => setOpt({ gpu: v })}>
            <option value="">any card the scheduler picks</option>
            {#each cards as card (card.uuid)}
              <option value={card.uuid}>{cardOption(card)}</option>
            {/each}
          </select>
        </label>

        <label>
          Start after
          <select bind:value={() => afterChoice, (v) => setOpt({ after: v })}>
            <option value="">nothing — start as soon as it can</option>
            {#each waiting as job (job.id)}
              <option value={job.id}>{job.label}</option>
            {/each}
          </select>
        </label>
      </div>

      <label class="qc-check">
        <input
          type="checkbox"
          bind:checked={() => runNow, (v) => setOpt({ mode: v ? 'now' : undefined })}
        />
        <span>
          Run now
          <span class="qc-hint">
            front of the order rather than the tail — it still waits for whatever is rendering
          </span>
        </span>
      </label>

      <label class="qc-check">
        <input
          type="checkbox"
          bind:checked={() => force, (v) => setOpt({ force: v })}
          disabled={!gpuChoice}
        />
        <span>
          Force the pin
          <span class="qc-hint">
            evicts whatever is resident on that card, including a service the eviction rules would
            otherwise protect
          </span>
        </span>
      </label>

      {#if cardsError}
        <p class="qc-err">card list unavailable — {cardsError}</p>
      {/if}
      {#if summary}
        <button class="qc-clear" type="button" onclick={clear}>Reset to defaults</button>
      {/if}
    </div>
  {/if}
</span>

<style>
  .qchip {
    position: relative;
    display: inline-flex;
    flex: 0 0 auto;
  }
  .qc-btn {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    height: 26px;
    padding: 0 8px;
    border: 1px solid var(--ring);
    border-radius: 999px;
    background: var(--bg-2);
    color: var(--muted);
    font-family: inherit;
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    line-height: 1;
    white-space: nowrap;
    cursor: pointer;
  }
  .qc-btn:hover,
  .qc-btn[aria-expanded='true'] {
    color: var(--ink);
    border-color: var(--accent);
  }
  .qc-glyph {
    font-size: 10px;
    opacity: 0.75;
  }
  /* The 202 is a statement, the prediction is a guess — so the receipt is the one drawn in ink. */
  .qc-btn.said {
    color: var(--ink);
  }
  .qc-btn.attention {
    color: var(--warning);
    border-color: var(--warning);
  }
  /* A knob is turned. Small enough to ignore, present enough that "why did that go to band 3"
     is answerable without opening anything. */
  .qc-btn.tuned::after {
    content: '';
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--accent);
  }

  .qc-pop {
    position: fixed;
    z-index: 60;
    width: min(340px, calc(100vw - 24px));
    max-height: min(70vh, 520px);
    overflow-y: auto;
    box-sizing: border-box;
    padding: 12px 14px;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: var(--card);
    box-shadow: var(--shadow);
    font-size: 12px;
    text-align: left;
    white-space: normal;
  }
  .qc-said {
    margin: 0 0 10px;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .qc-said.attention {
    color: var(--warning);
  }
  .qc-grid {
    display: grid;
    gap: 10px;
    padding-bottom: 12px;
  }
  .qc-grid label {
    display: block;
    color: var(--muted);
  }
  select {
    width: 100%;
    box-sizing: border-box;
    margin-top: 4px;
    padding: 7px 9px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font-family: inherit;
    font-size: 12px;
  }
  .qc-check {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    color: var(--ink);
    padding-bottom: 10px;
  }
  .qc-check input {
    margin-top: 2px;
  }
  .qc-check input:disabled + span {
    color: var(--muted);
  }
  .qc-hint {
    display: block;
    color: var(--muted);
    font-size: 11px;
    line-height: 1.4;
    margin-top: 2px;
  }
  .qc-err {
    margin: 0 0 10px;
    color: var(--danger);
    font-size: 11px;
  }
  .qc-clear {
    padding: 5px 10px;
    background: transparent;
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--muted);
    font-family: inherit;
    font-size: 11px;
    cursor: pointer;
  }
  .qc-clear:hover {
    color: var(--ink);
    border-color: var(--accent);
  }
</style>
