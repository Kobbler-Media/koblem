<script lang="ts">
  /*
   * "The box is handed to the 27B" on every page.
   *
   * 🔴 ZERO new network traffic. It reads `box_locked` off the queue snapshot,
   * which `QueueIndicator` already holds open for the whole session — so this is
   * a render of a bit that was arriving anyway, not a second poller. If you are
   * tempted to add a `/api/v1/gpu/beefy` fetch here, read the note on
   * `QueueSnapshot.box_locked` first: the full status (model, elapsed, the
   * inconsistency detector) is fetched ON DEMAND by /gpu-services, which is
   * where an operator can act on it.
   *
   * The allow-list (what still runs beside the 27B) is NOT on the snapshot —
   * `QueueSnapshot` lives in koblibs — so this says it exists and /gpu-services
   * names it.
   *
   * 🔴 --warning, not --danger. Nothing is broken. Someone deliberately handed
   * the machine to the big model, and every render request between now and the
   * moment they hand it back will WAIT rather than fail. The failure this exists
   * to prevent is nobody remembering that at 23:00.
   */
  import { onDestroy, onMount } from 'svelte';
  import { getQueueState, startQueueFeed } from '#lib/stores/queue.svelte.ts';
  import { isLive } from '#lib/queue/format.ts';

  const queue = getQueueState();

  let stop: (() => void) | null = null;

  // Ref-counted: joining the same feed the topbar already opened, not a new one.
  onMount(() => {
    stop = startQueueFeed();
  });
  onDestroy(() => stop?.());

  const lockedFor = $derived(queue.snapshot.box_locked ?? null);
  // Everything that is NOT the beefy holder and therefore cannot start. A live
  // row here means the mode was forced over it and it is being starved.
  const waiting = $derived(queue.snapshot.jobs.filter((j) => !isLive(j.state)).length);
  const starved = $derived(queue.snapshot.jobs.filter((j) => isLive(j.state)).length);
</script>

{#if lockedFor}
  <div class="beefy-banner" role="status">
    <span class="bolt" aria-hidden="true">⚡</span>
    <div class="body">
      <strong>BEEFY LLM IS ON</strong>
      <span class="detail">
        The box is held for <code>{lockedFor}</code> — queued video, image, music and 3D work can't
        start; only the services on its allow-list run beside it.
        {#if waiting > 0}
          {waiting} job{waiting === 1 ? '' : 's'} waiting (not cancelled).
        {/if}
        {#if starved > 0}
          <span class="starved"
            >{starved} job{starved === 1 ? '' : 's'} still marked running — forced over, and being starved.</span
          >
        {/if}
        Audio input to the LLM is unavailable; images still work.
      </span>
    </div>
    <a class="fix" href="/gpu-services">Turn it off</a>
  </div>
{/if}

<style>
  .beefy-banner {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    margin: 12px 16px 0;
    padding: 10px 14px;
    border: 1px solid var(--warning);
    border-radius: 12px;
    background: color-mix(in srgb, var(--warning) 10%, var(--card));
    color: var(--ink);
    font-size: 13px;
    line-height: 1.45;
  }

  .bolt {
    font-size: 16px;
    line-height: 1.3;
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
    flex: 1;
  }

  .body strong {
    color: var(--warning);
    font-size: 13px;
    font-weight: 700;
    letter-spacing: 0.04em;
  }

  .detail {
    color: var(--muted);
  }

  .detail code {
    font-size: 12px;
    padding: 0 4px;
    border-radius: 4px;
    background: var(--tag-bg);
    color: var(--tag-text);
  }

  .starved {
    color: var(--danger);
  }

  .fix {
    flex: none;
    align-self: center;
    padding: 5px 12px;
    border: 1px solid var(--warning);
    border-radius: 8px;
    color: var(--warning);
    background: transparent;
    font-size: 12px;
    text-decoration: none;
    white-space: nowrap;
  }

  .fix:hover {
    background: color-mix(in srgb, var(--warning) 18%, transparent);
  }

  @media (max-width: 600px) {
    .beefy-banner {
      flex-wrap: wrap;
      margin: 8px 10px 0;
    }
    .fix {
      align-self: flex-start;
    }
  }
</style>
