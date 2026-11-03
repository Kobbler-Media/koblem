<script lang="ts">
  /*
   * 🔴 The overnight failure mode is five jobs that never started because a
   * service was left disabled at 22:00. That has to be visible WITHOUT opening
   * the queue page, which is why this sits in the topbar on every screen.
   *
   * 🔴 It renders in every state, idle included — with nothing in flight this is
   * the only Queue affordance outside the sidebar, so it is a nav item that
   * happens to carry status, not a status that sometimes appears.
   */
  import { onDestroy, onMount } from 'svelte';
  import { getQueueState, startQueueFeed } from '#lib/stores/queue.svelte.ts';
  import { indicatorState } from '#lib/queue/format.ts';

  const queue = getQueueState();

  let now = $state(Date.now());
  let stop: (() => void) | null = null;
  let tick: ReturnType<typeof setInterval> | undefined;

  onMount(() => {
    stop = startQueueFeed();
    tick = setInterval(() => (now = Date.now()), 30000);
  });

  onDestroy(() => {
    stop?.();
    if (tick) clearInterval(tick);
  });

  const view = $derived(indicatorState(queue.snapshot, now));
  // 🔴 Beefy is the one state whose fix is not on the queue page. The queue can
  // only show you rows that will not start; the switch that unblocks them is on
  // GPU Services, so that is where the chip goes.
  const href = $derived(view.mode === 'beefy' ? '/gpu-services' : '/queue');
</script>

<a
  {href}
  class="queue-indicator {view.mode}"
  title={view.title}
  aria-label="GPU queue — {view.title}"
>
  {view.text}
</a>

<style>
  .queue-indicator {
    display: inline-flex;
    align-items: center;
    max-width: 42vw;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    padding: 0 12px;
    height: 32px;
    border-radius: 12px;
    border: 1px solid var(--ring);
    background: var(--card);
    color: var(--muted);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    text-decoration: none;
  }

  .queue-indicator:hover {
    color: var(--ink);
    border-color: var(--accent);
  }

  .queue-indicator.idle {
    background: transparent;
  }

  .queue-indicator.blocked {
    color: var(--danger);
    border-color: var(--danger);
    background: color-mix(in srgb, var(--danger) 14%, transparent);
    animation: queue-pulse 2.4s ease-in-out infinite;
  }

  .queue-indicator.blocked:hover {
    color: var(--danger);
    border-color: var(--danger);
  }

  /* 🔴 --warning, not --danger. Beefy is a deliberate operator state, not a
     fault: the box is doing exactly what someone asked it to. It still has to
     be unmissable — this chip is what survives a collapsed sidebar, where the
     banner is not on screen at all. */
  .queue-indicator.beefy {
    color: var(--warning);
    border-color: var(--warning);
    background: color-mix(in srgb, var(--warning) 14%, transparent);
    font-weight: 600;
    letter-spacing: 0.02em;
  }

  .queue-indicator.beefy:hover {
    color: var(--warning);
    border-color: var(--warning);
  }

  @keyframes queue-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 transparent;
    }
    50% {
      box-shadow: 0 0 0 3px color-mix(in srgb, var(--danger) 22%, transparent);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .queue-indicator.blocked {
      animation: none;
    }
  }

  @media (max-width: 600px) {
    .queue-indicator {
      max-width: 50vw;
      font-size: 11px;
      padding: 0 8px;
    }
  }
</style>
