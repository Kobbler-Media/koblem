<script lang="ts">
  /*
   * The global GPU queue. One page: what is running, what is waiting and WHY it
   * is waiting.
   *
   * 🔴 "queued" is never the answer to "why hasn't it started". Every pending row
   * carries its `blocked_reason` and how long it has held it, because the reason
   * is the only thing an operator can act on.
   */
  import { onDestroy, onMount } from 'svelte';
  import {
    cancelQueueJob,
    getQueueSnapshot,
    getQueueStats,
    holdQueueJob,
    preemptQueueJob,
    reorderQueueJob,
    setQueuePaused,
    type Job,
    type JobStat
  } from '$lib/api/client';
  import {
    getQueueState,
    onJobEvent,
    refreshQueue,
    startQueueFeed
  } from '$lib/stores/queue.svelte';
  import { getNotifyState, setNotifyEnabled } from '$lib/stores/notify.svelte';
  import { reorderTarget } from '$lib/director/timeline-drag';
  import {
    BANDS,
    bandLabel,
    batchEta,
    blockedSummary,
    elapsedMs,
    humanMs,
    isLive,
    isPending,
    isTerminal,
    jobEta,
    reorderAfterId,
    unitsLabel
  } from '$lib/queue/format';

  const queue = getQueueState();
  const notify = getNotifyState();

  let now = $state(Date.now());
  let actionError = $state('');
  let busyId = $state('');
  let showHistory = $state(true);
  let historyJobs = $state<Job[]>([]);
  let historyLoading = $state(false);
  let historyStale = false;
  let stats = $state<JobStat[]>([]);
  let showStats = $state(false);

  let stopFeed: (() => void) | null = null;
  let stopEvents: (() => void) | null = null;
  let tick: ReturnType<typeof setInterval> | undefined;

  onMount(() => {
    stopFeed = startQueueFeed();
    // 🔴 A finished render leaves the live list and would otherwise vanish until
    // a manual refresh. The frame that retires it is what reloads the tail.
    stopEvents = onJobEvent((event) => {
      if (isTerminal(event.state)) void loadHistory();
    });
    // The wait durations are the point of this page, so they count in real time
    // rather than only moving when the server says something.
    tick = setInterval(() => (now = Date.now()), 1000);
    void loadHistory();
  });

  onDestroy(() => {
    stopFeed?.();
    stopEvents?.();
    if (tick) clearInterval(tick);
  });

  const snapshot = $derived(queue.snapshot);
  const byId = $derived(new Map(snapshot.jobs.map((j) => [j.id, j])));
  const liveJobs = $derived(snapshot.jobs.filter((j) => isLive(j.state)));
  const pendingJobs = $derived(snapshot.jobs.filter((j) => isPending(j.state)));
  const bands = $derived([...new Set(pendingJobs.map((j) => j.band))].sort((a, b) => a - b));
  const stuckSet = $derived(new Set(snapshot.stuck));
  const stuckJobs = $derived(snapshot.jobs.filter((j) => stuckSet.has(j.id)));
  const eta = $derived(batchEta(snapshot));

  async function run(id: string, fn: () => Promise<unknown>) {
    busyId = id;
    actionError = '';
    try {
      await fn();
      await refreshQueue();
    } catch (e) {
      // 🔴 Includes the 409 a `collecting` cancel answers with. The server's
      // sentence explains itself better than any status code we could restate.
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      busyId = '';
    }
  }

  // ── Reorder ───────────────────────────────────────────────────────

  type Drag = { id: string; band: number; order: string[]; grabDy: number };

  let drag = $state<Drag | null>(null);
  let listEl: HTMLElement | null = null;
  const rowEls: Record<string, HTMLElement> = {};

  function bandIds(band: number): string[] {
    if (drag && drag.band === band) return drag.order;
    return pendingJobs.filter((j) => j.band === band).map((j) => j.id);
  }

  function bandRows(band: number): Job[] {
    return bandIds(band)
      .map((id) => byId.get(id))
      .filter((j): j is Job => !!j);
  }

  function startDrag(e: PointerEvent, job: Job) {
    const row = rowEls[job.id];
    if (!row?.parentElement) return;
    e.preventDefault();
    listEl = row.parentElement;
    listEl.setPointerCapture(e.pointerId);
    drag = {
      id: job.id,
      band: job.band,
      order: bandIds(job.band).slice(),
      grabDy: e.clientY - row.getBoundingClientRect().top
    };
  }

  function onDragMove(e: PointerEvent) {
    if (!drag || !listEl) return;
    const from = drag.order.indexOf(drag.id);
    if (from < 0) return;
    // `reorderTarget` is pure 1-D geometry over a list of extents — the timeline
    // feeds it clip widths in seconds, this feeds it row heights in pixels.
    const heights = drag.order.map((id) => rowEls[id]?.offsetHeight ?? 0);
    const grabY = e.clientY - listEl.getBoundingClientRect().top - drag.grabDy;
    const target = reorderTarget(heights, from, grabY);
    if (target === from) return;
    const order = drag.order.slice();
    const [moved] = order.splice(from, 1);
    order.splice(target, 0, moved);
    drag = { ...drag, order };
  }

  async function endDrag(e: PointerEvent) {
    const d = drag;
    if (!d) return;
    try {
      listEl?.releasePointerCapture(e.pointerId);
    } catch {
      // capture may already be gone
    }
    listEl = null;
    drag = null;
    const to = d.order.indexOf(d.id);
    const served = pendingJobs.filter((j) => j.band === d.band).map((j) => j.id);
    if (to < 0 || served[to] === d.id) return;
    await run(d.id, () => reorderQueueJob(d.id, { after: reorderAfterId(d.order, to) }));
  }

  /// The keyboard and touch path. Same call as the drag, so the two can never
  /// drift apart.
  async function nudge(job: Job, delta: number) {
    const ids = pendingJobs.filter((j) => j.band === job.band).map((j) => j.id);
    const from = ids.indexOf(job.id);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= ids.length) return;
    const order = ids.slice();
    order.splice(from, 1);
    order.splice(to, 0, job.id);
    await run(job.id, () => reorderQueueJob(job.id, { after: reorderAfterId(order, to) }));
  }

  function bandIndex(job: Job): number {
    return pendingJobs.filter((j) => j.band === job.band).findIndex((j) => j.id === job.id);
  }

  function bandSize(job: Job): number {
    return pendingJobs.filter((j) => j.band === job.band).length;
  }

  // ── Actions ───────────────────────────────────────────────────────

  const changeBand = (job: Job, band: number) =>
    band === job.band ? undefined : run(job.id, () => reorderQueueJob(job.id, { band }));
  const bump = (job: Job) => run(job.id, () => reorderQueueJob(job.id, { bump: true }));
  const hold = (job: Job) => run(job.id, () => holdQueueJob(job.id, job.state !== 'held'));
  const cancel = (job: Job) => run(job.id, () => cancelQueueJob(job.id));
  const preempt = (job: Job) => run(job.id, () => preemptQueueJob(job.id));
  const togglePause = () => run('queue', () => setQueuePaused(!snapshot.paused));

  async function toggleHistory() {
    showHistory = !showHistory;
    if (!showHistory || historyJobs.length > 0) return;
    await loadHistory();
  }

  // 🔴 The history tail is its own request, not a slice of the snapshot the feed
  // keeps warm — that one carries pending + live only. A burst of frames must not
  // become a burst of requests, but the last one cannot be DROPPED either: the job
  // that retired after the in-flight request left is exactly the row the reload was
  // for. So one runs, the rest collapse into a single trailing reload.
  async function loadHistory() {
    if (historyLoading) {
      historyStale = true;
      return;
    }
    historyLoading = true;
    try {
      do {
        historyStale = false;
        const snap = await getQueueSnapshot({ history: true, limit: 200 });
        historyJobs = snap.jobs.filter((j) => isTerminal(j.state));
      } while (historyStale);
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      historyStale = false;
      historyLoading = false;
    }
  }

  // 🔴 The only caller of setNotifyEnabled, and it is a click. Permission is
  // asked for here or not at all.
  const toggleNotify = () => setNotifyEnabled(!notify.enabled);

  // 🔴 A dead toggle teaches nothing. When the reason notifications are unavailable is iOS wanting
  // the app on the Home Screen, the button is replaced by that instruction instead.
  const notifyOffered = $derived(
    notify.availability.state === 'ready' || notify.availability.state === 'blocked'
  );
  const notifyLabel = $derived.by(() => {
    if (!notify.enabled) return 'Notify me';
    return notify.pushing ? 'Notifications on' : 'Notifications on (this tab)';
  });
  const notifyTitle = $derived(
    notify.pushing
      ? 'A notification when a batch finishes, a render fails or the queue gets stuck — with the browser closed'
      : 'A notification when a render finishes or the queue gets stuck, while this tab is in the background'
  );

  async function toggleStats() {
    showStats = !showStats;
    if (!showStats || stats.length > 0) return;
    try {
      stats = await getQueueStats();
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    }
  }
</script>

{#snippet row(job: Job, reorderable: boolean)}
  <div class="row" class:dragging={drag?.id === job.id} bind:this={rowEls[job.id]}>
    {#if reorderable}
      <div class="grip">
        <button
          class="handle"
          type="button"
          aria-label="Drag to reorder {job.label}"
          onpointerdown={(e) => startDrag(e, job)}>⠿</button
        >
        <span class="arrows">
          <button
            type="button"
            aria-label="Move {job.label} earlier"
            disabled={bandIndex(job) === 0 || busyId === job.id}
            onclick={() => nudge(job, -1)}>↑</button
          >
          <button
            type="button"
            aria-label="Move {job.label} later"
            disabled={bandIndex(job) >= bandSize(job) - 1 || busyId === job.id}
            onclick={() => nudge(job, 1)}>↓</button
          >
        </span>
      </div>
    {/if}

    <div class="body">
      <div class="headline">
        <span class="status-chip state {job.state}">{job.state}</span>
        <span class="label">{job.label || job.kind}</span>
        {#if unitsLabel(job)}
          <span class="units">{unitsLabel(job)}</span>
        {/if}
        {#if job.progress.phase}
          <span class="phase">{job.progress.phase}</span>
        {/if}
        {#if job.target_gpu}
          <span class="pin" title="Pinned at submit time to {job.target_gpu}">
            pinned {job.force_pin ? '(forced)' : ''}
          </span>
        {/if}
        {#if job.depends_on}
          <span class="pin">after {job.depends_on}</span>
        {/if}
      </div>

      {#if job.progress.progress != null}
        <div class="progress-track">
          <div
            class="progress-fill"
            style={`width:${Math.round(Math.min(1, Math.max(0, job.progress.progress)) * 100)}%`}
          ></div>
        </div>
      {/if}

      {#if job.blocked_reason}
        {@const waited = elapsedMs(job.blocked_since, now)}
        <div class="blocked" class:stuck={stuckSet.has(job.id)}>
          {#if stuckSet.has(job.id)}<span class="status-pill siren">BLOCKED</span>{/if}
          <span class="why">waiting: {blockedSummary(job.blocked_reason)}</span>
          {#if waited != null}<span class="waited">for {humanMs(waited)}</span>{/if}
        </div>
      {/if}

      {#if isPending(job.state)}
        {@const view = jobEta(job)}
        {#if view}
          <div class="eta">
            {#if view.starts}<span>starts ~{view.starts}</span>{/if}
            {#if view.finishes}<span>done ~{view.finishes}</span>{/if}
            <!-- 🔴 The sample count travels with every ETA. A confident-looking
                 clock built from three runs is worse than one that admits it. -->
            <span class="samples">{view.samples} measured run{view.samples === 1 ? '' : 's'}</span>
          </div>
        {:else}
          <div class="eta unknown">no estimate — this shape has never been measured</div>
        {/if}
      {/if}

      {#if job.error}
        <div class="job-error">{job.error}</div>
      {/if}
      {#each job.warnings as warning, i (i)}
        <div class="warning">⚠ {warning}</div>
      {/each}

      <div class="meta muted">
        {job.kind} · {job.id} · by {job.created_by}
        {#if job.queued_ms != null}· queued {humanMs(job.queued_ms)}{/if}
        {#if job.admit_ms != null}· admission {humanMs(job.admit_ms)}{/if}
        {#if job.render_ms != null}· render {humanMs(job.render_ms)}{/if}
      </div>
    </div>

    <div class="controls">
      {#if reorderable}
        <label class="band">
          <span class="muted">band</span>
          <select
            aria-label="Band for {job.label}"
            value={job.band}
            disabled={busyId === job.id}
            onchange={(e) => changeBand(job, Number(e.currentTarget.value))}
          >
            {#each BANDS as band (band)}
              <option value={band}>{bandLabel(band)}</option>
            {/each}
            {#if !BANDS.includes(job.band)}
              <option value={job.band}>{bandLabel(job.band)}</option>
            {/if}
          </select>
        </label>
        <button class="btn-sec sm" disabled={busyId === job.id} onclick={() => bump(job)}>
          Bump
        </button>
        <button class="btn-sec sm" disabled={busyId === job.id} onclick={() => hold(job)}>
          {job.state === 'held' ? 'Release' : 'Hold'}
        </button>
      {/if}

      {#if job.state === 'running'}
        <button class="btn-sec sm" disabled={busyId === job.id} onclick={() => preempt(job)}>
          Preempt
        </button>
      {/if}

      <!-- 🔴 A `collecting` job is NOT cancellable: the GPU cost is already paid
           and the artifact is landing, so killing it loses the only copy. The
           button is absent, not disabled — a disabled button still reads as a
           thing you could have done. -->
      {#if job.state === 'collecting'}
        <span class="uncancellable">saving output — cannot be cancelled</span>
      {:else if !isTerminal(job.state)}
        <button class="btn-sec sm danger" disabled={busyId === job.id} onclick={() => cancel(job)}>
          Cancel
        </button>
      {/if}
    </div>
  </div>
{/snippet}

<div class="page-container">
  <div class="section-header">
    <h2>GPU Queue</h2>
    <div class="header-actions">
      <span class="status-chip feed" class:offline={!queue.streaming}>
        {queue.streaming ? 'live' : 'polling'}
      </span>
      {#if notifyOffered}
        <button
          class={notify.enabled ? 'btn-primary' : 'btn-sec'}
          disabled={notify.availability.state === 'blocked' || notify.busy}
          title={notify.availability.hint || notifyTitle}
          onclick={toggleNotify}
        >
          {notifyLabel}
        </button>
      {:else if notify.availability.hint}
        <span class="notify-hint">{notify.availability.hint}</span>
      {/if}
      <button class="btn-sec" onclick={toggleStats}>
        {showStats ? 'Hide estimator' : 'Estimator'}
      </button>
      <button class="btn-sec" onclick={toggleHistory}>
        {showHistory ? 'Hide history' : 'History'}
      </button>
      <button
        class={snapshot.paused ? 'btn-primary' : 'btn-sec'}
        disabled={busyId === 'queue'}
        onclick={togglePause}
      >
        {snapshot.paused ? 'Resume queue' : 'Pause queue'}
      </button>
    </div>
  </div>

  {#if queue.error}
    <div class="card error">{queue.error}</div>
  {/if}
  {#if actionError}
    <div class="card error">{actionError}</div>
  {/if}
  {#if !queue.loaded && !queue.error}
    <div class="loading-card">Loading the queue…</div>
  {/if}

  {#if stuckJobs.length > 0}
    <!-- 🔴 The overnight failure mode, and the ONLY thing that gets this
         treatment. `snapshot.stuck` is already filtered to reasons an operator
         can clear — a busy comfy lane is the queue working correctly. -->
    <div class="card siren-card">
      <div class="siren-head">
        <strong
          >{stuckJobs.length} job{stuckJobs.length === 1 ? '' : 's'} blocked on something you can clear</strong
        >
        <a class="btn-sec sm" href="/gpu-services">GPU Services</a>
      </div>
      <ul>
        {#each stuckJobs as job (job.id)}
          {@const waited = elapsedMs(job.blocked_since, now)}
          <li>
            <span class="label">{job.label || job.kind}</span>
            <span class="why">{job.blocked_reason ? blockedSummary(job.blocked_reason) : ''}</span>
            {#if waited != null}<span class="waited">for {humanMs(waited)}</span>{/if}
          </li>
        {/each}
      </ul>
    </div>
  {/if}

  <div class="card summary">
    <div><strong>{snapshot.running}</strong> <span class="muted">running</span></div>
    <div><strong>{snapshot.pending}</strong> <span class="muted">queued</span></div>
    <div>
      {#if eta}
        <strong>~{eta.at}</strong>
        <span class="muted">batch done</span>
        {#if eta.unestimated > 0}
          <span class="samples">{eta.unestimated} unmeasured job(s) not counted</span>
        {/if}
      {:else}
        <span class="muted">no batch estimate — nothing measured yet</span>
      {/if}
    </div>
    {#if snapshot.paused}
      <div class="paused">PAUSED — the running job finishes, nothing new starts</div>
    {/if}
  </div>

  {#if showStats}
    <div class="card">
      <h3>Estimator</h3>
      {#if stats.length === 0}
        <p class="muted">No measured shapes yet.</p>
      {:else}
        <table class="stats">
          <thead>
            <tr
              ><th>kind</th><th>shape</th><th>runs</th><th>p50</th><th>p90</th><th>per unit</th></tr
            >
          </thead>
          <tbody>
            {#each stats as stat (`${stat.kind}:${stat.shape_key}`)}
              <tr class:guess={!stat.confident}>
                <td>{stat.kind}</td>
                <td class="shape">{stat.shape_key}</td>
                <td>{stat.samples}{stat.confident ? '' : ' (guess)'}</td>
                <td>{humanMs(stat.p50_ms)}</td>
                <td>{humanMs(stat.p90_ms)}</td>
                <td>{humanMs(stat.unit_p50_ms)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>
  {/if}

  <div class="card">
    <h3>Running</h3>
    {#each liveJobs as job (job.id)}
      {@render row(job, false)}
    {:else}
      <p class="muted">Nothing on the cards.</p>
    {/each}
  </div>

  <div class="card">
    <h3>Waiting</h3>
    {#if pendingJobs.length === 0}
      <p class="muted">Nothing queued.</p>
    {/if}
    {#each bands as band (band)}
      <div class="band-head muted">{bandLabel(band)}</div>
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="rows"
        onpointermove={onDragMove}
        onpointerup={endDrag}
        onpointercancel={endDrag}
        onlostpointercapture={() => {
          drag = null;
          listEl = null;
        }}
      >
        {#each bandRows(band) as job (job.id)}
          {@render row(job, true)}
        {/each}
      </div>
    {/each}
  </div>

  {#if showHistory}
    <div class="card">
      <div class="history-head">
        <h3>History</h3>
        <button class="btn-sec sm" disabled={historyLoading} onclick={loadHistory}>Refresh</button>
      </div>
      {#if historyLoading && historyJobs.length === 0}
        <p class="muted">Loading…</p>
      {/if}
      {#each historyJobs as job (job.id)}
        {@render row(job, false)}
      {:else}
        {#if !historyLoading}<p class="muted">No finished jobs.</p>{/if}
      {/each}
    </div>
  {/if}
</div>

<style>
  .header-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .notify-hint {
    max-width: 340px;
    color: var(--muted);
    font-size: 11px;
    line-height: 1.4;
  }

  .card {
    margin-bottom: 16px;
  }

  .card h3 {
    margin: 0 0 12px;
    font-size: 15px;
  }

  .summary {
    display: flex;
    align-items: baseline;
    gap: 28px;
    flex-wrap: wrap;
  }

  .summary strong {
    font-size: 16px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .summary .paused {
    color: var(--warning);
    font-size: 12px;
  }

  .siren-card {
    border-color: var(--danger);
    background: color-mix(in srgb, var(--danger) 10%, var(--card));
  }

  .siren-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    color: var(--danger);
    margin-bottom: 8px;
  }

  .siren-head strong {
    font-weight: 600;
    font-size: 14px;
  }

  .siren-card ul {
    margin: 0;
    padding-left: 18px;
    font-size: 13px;
  }

  .siren-card li {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    padding: 2px 0;
  }

  .siren-card .why {
    color: var(--danger);
  }

  .siren-card .waited {
    color: var(--danger);
    font-variant-numeric: tabular-nums;
  }

  .band-head {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    margin: 12px 0 6px;
  }

  .rows {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .row {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: var(--bg-2);
  }

  .row.dragging {
    border-color: var(--accent);
    box-shadow: var(--shadow);
  }

  .grip {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
  }

  .handle {
    background: none;
    border: none;
    color: var(--muted);
    font-size: 16px;
    line-height: 1;
    cursor: grab;
    padding: 2px 4px;
    touch-action: none;
  }

  .arrows {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .arrows button {
    background: var(--card);
    border: 1px solid var(--ring);
    color: var(--ink);
    border-radius: 6px;
    width: 22px;
    height: 18px;
    line-height: 1;
    font-size: 11px;
    cursor: pointer;
    padding: 0;
  }

  .arrows button:disabled {
    opacity: 0.35;
    cursor: not-allowed;
  }

  .body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .headline {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  .headline .label {
    font-size: 14px;
    font-weight: 600;
    overflow-wrap: anywhere;
  }

  .units,
  .phase,
  .pin {
    font-size: 11px;
    color: var(--muted);
    border: 1px solid var(--ring);
    border-radius: 6px;
    padding: 1px 6px;
    font-variant-numeric: tabular-nums;
  }

  .state {
    text-transform: uppercase;
  }

  .state.running,
  .state.collecting {
    color: var(--accent);
    border-color: var(--accent);
  }

  .state.held {
    color: var(--warning);
    border-color: var(--warning);
  }

  .state.failed {
    color: var(--danger);
    border-color: var(--danger);
  }

  .state.done {
    color: var(--success);
    border-color: var(--success);
  }

  .blocked {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 12px;
    color: var(--muted);
  }

  .blocked .waited {
    color: var(--warning);
    font-variant-numeric: tabular-nums;
  }

  .blocked.stuck {
    color: var(--danger);
  }

  .blocked.stuck .waited {
    color: var(--danger);
  }

  /* 🔴 The one place the heavy weight is kept, and it comes from the shared
     .status-pill rather than a local override — a solid danger badge reading
     BLOCKED is the single thing on this page that must survive a glance. */
  .siren {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--bg);
    letter-spacing: 0.08em;
  }

  .eta {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    font-size: 12px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .eta.unknown {
    font-style: italic;
  }

  .samples {
    font-size: 11px;
    font-style: italic;
    opacity: 0.8;
  }

  .job-error {
    font-size: 12px;
    color: var(--danger);
    overflow-wrap: anywhere;
  }

  .warning {
    font-size: 12px;
    color: var(--warning);
  }

  .meta {
    font-size: 11px;
    overflow-wrap: anywhere;
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
    justify-content: flex-end;
    max-width: 320px;
  }

  .band {
    display: flex;
    align-items: center;
    gap: 4px;
    font-size: 11px;
  }

  .band select {
    background: var(--card);
    border: 1px solid var(--ring);
    color: var(--ink);
    border-radius: 6px;
    padding: 3px 6px;
    font-size: 12px;
  }

  .sm {
    padding: 4px 10px;
    font-size: 12px;
  }

  .danger {
    color: var(--danger);
    border-color: var(--danger);
  }

  .uncancellable {
    font-size: 11px;
    color: var(--muted);
    font-style: italic;
    max-width: 160px;
    text-align: right;
  }

  .feed {
    color: var(--success);
    border-color: var(--success);
  }

  .feed.offline {
    color: var(--muted);
    border-color: var(--ring);
  }

  .history-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 12px;
  }

  .history-head h3 {
    margin: 0;
  }

  .stats {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }

  .stats th {
    text-align: left;
    color: var(--muted);
    font-weight: 500;
    padding: 4px 8px 4px 0;
  }

  .stats td {
    padding: 3px 8px 3px 0;
    font-variant-numeric: tabular-nums;
  }

  .stats .shape {
    color: var(--muted);
    overflow-wrap: anywhere;
  }

  .stats tr.guess td {
    color: var(--warning);
  }

  @media (max-width: 900px) {
    .row {
      flex-wrap: wrap;
    }

    .controls {
      max-width: none;
      width: 100%;
      justify-content: flex-start;
    }
  }
</style>
