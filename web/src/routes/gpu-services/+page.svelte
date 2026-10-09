<script lang="ts">
  // Service Toggles UI — §6.5.4 of GO-LIVE-PLAN. One row per gate-tracked
  // service with an Enable/Disable button + three preset shortcuts at the
  // bottom. Each toggle re-fetches state so the row reflects the actual gate
  // state (the disable side-effect is a SIGKILL on heavy services, so VRAM
  // does clear when the row flips).
  //
  // Plus the Beefy LLM card at the top — Qwen3.8-27B across both cards, with
  // only the `BEEFY_ALLOW` services still admitted beside it. That is "Disable
  // all except LLM" plus a model swap, so it lives here rather than growing a
  // page of its own.
  import { onDestroy, onMount } from 'svelte';
  import { Modal } from '@koblibs/ui';
  import {
    ApiError,
    listGpuServices,
    toggleGpuService,
    disableAllServices,
    enableAllServices,
    getBeefyState,
    setBeefyMode,
    type BeefyStatus,
    type GpuServiceState,
    type Job
  } from '$lib/api/client';
  import { getQueueState, refreshQueue, startQueueFeed } from '$lib/stores/queue.svelte';
  import { elapsedMs, humanMs, isLive, jobEta } from '$lib/queue/format';

  let services = $state<GpuServiceState[]>([]);
  let loading = $state(true);
  let error = $state('');
  let actionInFlight = $state(false);

  async function load() {
    error = '';
    try {
      const resp = await listGpuServices();
      services = resp.services;
    } catch (e) {
      error = e instanceof Error ? e.message : 'failed to load services';
    }
    loading = false;
  }

  onMount(load);

  async function handleToggle(name: string, currentlyEnabled: boolean) {
    actionInFlight = true;
    try {
      await toggleGpuService(name, !currentlyEnabled);
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : `toggle failed for ${name}`;
    } finally {
      actionInFlight = false;
    }
  }

  async function handleDisableAll() {
    actionInFlight = true;
    try {
      await disableAllServices([]);
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : 'disable-all failed';
    } finally {
      actionInFlight = false;
    }
  }

  async function handleEnableAll() {
    actionInFlight = true;
    try {
      await enableAllServices();
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : 'enable-all failed';
    } finally {
      actionInFlight = false;
    }
  }

  async function handleDisableExceptLlm() {
    actionInFlight = true;
    try {
      await disableAllServices(['llm']);
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : 'disable-except-llm failed';
    } finally {
      actionInFlight = false;
    }
  }

  // Human-readable label per service kind. Covers all 18 of `ServiceKind::ALL`
  // (kob-gpu-gate), cross-checked against kobbler's docker-compose service
  // comments — the Beefy card and dialog build their "keeps running" / "stops
  // working" lists from these names. Falls back to the raw name if the backend
  // adds a service we haven't named.
  const LABELS: Record<string, string> = {
    llm: 'LLM',
    tts: 'TTS (BreezeTTS 2)',
    avatar: 'Avatar (longcat-avatar)',
    music: 'Music (ACE-Step)',
    sa3: 'Music (SA3)',
    vision: 'Vision (siglip2)',
    stt: 'STT (parakeet)',
    flux: 'Images (FLUX.2)',
    krea2: 'Images (Krea 2)',
    matting: 'Matting (RMBG)',
    songgen: 'Music: SongGen (LeVo2)',
    ltx: 'Video (LTX-2.5)',
    ltx_2b: 'Video (LTX-2B)',
    wan_vace: 'Video (Wan2.2-VACE)',
    minimax_h3: 'Video + audio (MiniMax-H3)',
    spar3d: '3D (SPAR3D + RigAnything)',
    pixal3d: '3D (Pixal3D / longcat-rig)'
  };
  function label(name: string): string {
    return LABELS[name] ?? name;
  }

  // ── Beefy LLM ──────────────────────────────────────────────────────────────
  //
  // 🔴 The POST is ONE blocking call of tens of seconds (measured ~10–40 s): the
  // eviction sweep, the 27B load and the proof it is resident all happen inside
  // it, and it answers with the FINAL state. There are no phases to poll and no
  // stepper to build — the backend does not publish one.

  const queue = getQueueState();
  let stopQueue: (() => void) | null = null;
  let now = $state(Date.now());
  let clock: ReturnType<typeof setInterval> | undefined;

  onMount(() => {
    // The queue feed is ref-counted and already open (the topbar indicator holds
    // it). This joins it for the pre-flight list rather than adding a poller.
    stopQueue = startQueueFeed();
    clock = setInterval(() => (now = Date.now()), 30000);
  });
  onDestroy(() => {
    stopQueue?.();
    if (clock) clearInterval(clock);
  });

  let beefy = $state<BeefyStatus | null>(null);
  let beefyError = $state('');
  /// 501 from the routes: `BEEFY_LLM_SERVICE_URL` is unset, so the feature is not
  /// deployed on this koblem at all. A different state from "failed to load".
  let beefyUnavailable = $state(false);
  let beefyBusy = $state(false);
  let confirmOpen = $state(false);
  let force = $state(false);

  async function loadBeefy() {
    try {
      beefy = await getBeefyState();
      beefyError = '';
      beefyUnavailable = false;
    } catch (e) {
      if (e instanceof ApiError && e.status === 501) {
        beefyUnavailable = true;
        beefyError = '';
        return;
      }
      beefyError = e instanceof Error ? e.message : 'failed to read Beefy state';
    }
  }

  onMount(loadBeefy);

  /// What Beefy still admits beside the 27B, from the SERVER's allow-list — it
  /// is `BEEFY_ALLOW` in the environment, so hardcoding it here would drift the
  /// first time someone edits compose. While on, the gate's live list; while off
  /// (the enter dialog) the gate holds none, so the configured one.
  const beefyAllow = $derived((beefy?.on ? beefy.allow : beefy?.configured_allow) ?? []);
  /// Everything else the gate knows about. Built from the live service list, so a
  /// service the backend adds is warned about without touching this page.
  const beefyStops = $derived(
    services.filter((s) => s.name !== 'llm' && !beefyAllow.includes(s.name)).map((s) => s.name)
  );

  /// Live GPU work, from the snapshot every page already holds. This is what the
  /// server's own pre-flight refuses on — it is showing you the 409 before you
  /// earn it.
  const liveJobs = $derived(queue.snapshot.jobs.filter((j) => isLive(j.state)));

  /// 🔴 `eta_finish` is an RFC3339 WALL CLOCK, not a duration, and a row the
  /// estimator has never measured has no ETA at all. `jobEta` enforces both —
  /// never render a time off a zero-sample row.
  function etaText(job: Job): string {
    const eta = jobEta(job);
    if (!eta?.finishes) return 'no estimate';
    return `~${eta.finishes}${eta.samples < 3 ? ` (${eta.samples} sample${eta.samples === 1 ? '' : 's'})` : ''}`;
  }

  const beefyOnSince = $derived.by(() => {
    const ms = elapsedMs(beefy?.since, now);
    return ms == null ? null : humanMs(ms);
  });

  /// The half-state detector, spelled out. `inconsistent` alone tells an operator
  /// nothing actionable; these say WHICH of the four things disagrees.
  const inconsistencies = $derived.by(() => {
    const b = beefy;
    if (!b?.inconsistent) return [] as string[];
    const out: string[] = [];
    const pointsAtBeefy =
      !!b.beefy_url && b.llm_url.replace(/\/$/, '') === b.beefy_url.replace(/\/$/, '');
    if (b.on !== pointsAtBeefy) {
      out.push(
        b.on
          ? `the flag says ON but koblem is talking to ${b.llm_url} — someone edited llama_server_url behind it`
          : `the flag says OFF but koblem is talking to the beefy server (${b.llm_url})`
      );
    }
    if (b.on !== b.multi_gpu) {
      out.push(
        b.on
          ? 'the flag says ON but the both-cards flag is off — requests will send X-Worker-Gpu and 503 against a tensor-split model'
          : 'the flag says OFF but the both-cards flag is still on'
      );
    }
    const exclusive = b.exclusive_for ?? null;
    if (b.on !== (exclusive === 'llm')) {
      out.push(
        b.on
          ? `the flag says ON but the GPU gate is not locked (exclusive: ${exclusive ?? 'none'}) — other services can admit on top of the 27B`
          : `the flag says OFF but the GPU gate is still locked for ${exclusive} — nothing else can run`
      );
    }
    return out;
  });

  function openConfirm() {
    force = false;
    beefyError = '';
    confirmOpen = true;
  }

  async function apply(on: boolean, withForce: boolean) {
    beefyBusy = true;
    beefyError = '';
    try {
      beefy = await setBeefyMode({ on, force: withForce });
      confirmOpen = false;
      // The gate's exclusive flag just moved, so every row's admissibility did.
      await load();
      // And so did `box_locked` on the snapshot. Without this the global banner
      // and the topbar chip lag by up to one 10 s backstop tick — on the ONE
      // page where the operator just changed it and is looking straight at it.
      await refreshQueue();
    } catch (e) {
      // 409 (jobs running / eviction unconfirmed), 502 (load failed, already
      // rolled back), 501, 500 — the server's message names the jobs or the
      // services, so it is shown verbatim rather than re-worded here.
      beefyError = e instanceof Error ? e.message : 'Beefy mode change failed';
      // Whatever happened, the truth is now on the server: a 502 rolled back, a
      // 409 flipped nothing, and a network drop may have succeeded anyway.
      await loadBeefy();
    } finally {
      beefyBusy = false;
    }
  }
</script>

<div class="page">
  <header class="header">
    <div>
      <h1>GPU Services</h1>
      <p class="muted">
        Administratively enable/disable each GPU consumer. Disabling fires an unload on the service
        so VRAM clears within seconds (heavy services SIGKILL their in-flight render). State is
        persisted in the gate; a new request lazy-loads.
      </p>
    </div>
  </header>

  {#if error}
    <div class="card error">{error}</div>
  {/if}

  <!-- ── Beefy LLM ──────────────────────────────────────────────────────── -->
  {#if beefyUnavailable}
    <div class="card muted small">
      Beefy LLM is not deployed on this koblem — <code>BEEFY_LLM_SERVICE_URL</code> is unset, so the route
      answers 501.
    </div>
  {:else if beefy}
    <div class="card beefy" class:on={beefy.on}>
      {#if beefy.on}
        <div class="beefy-head">
          <div>
            <div class="beefy-title">⚡ BEEFY LLM IS ON</div>
            <div class="beefy-sub">
              {beefy.model ?? 'model unknown'} across both cards
              {#if beefyOnSince}· on for {beefyOnSince}{/if}
              {#if !beefy.loaded}
                · <span class="warn-text"
                  >weights NOT resident — the next request loads them (~40 s)</span
                >
              {/if}
            </div>
          </div>
          <button class="btn-danger" disabled={beefyBusy} onclick={() => apply(false, false)}>
            {beefyBusy ? 'Handing the box back…' : 'Turn Beefy off'}
          </button>
        </div>
        <ul class="beefy-facts">
          {#if beefyAllow.length > 0}
            <li>
              <strong>Still running beside it:</strong>
              {beefyAllow.map(label).join(', ')}. They share what the 27B leaves on the 3060, so
              they can unload each other and take turns.
              {#if beefyAllow.includes('tts')}Only <em>streaming</em> TTS works — buffered TTS is refused
                while Beefy is on.{/if}
            </li>
          {/if}
          <li>
            {beefyAllow.length > 0 ? 'Everything else' : 'Every other GPU service'} is refused admission.
            Queued jobs <em>wait</em>, they were not cancelled — they resume the moment this is
            turned off.
          </li>
          <li>
            <strong>Audio input to the LLM does not work</strong> in this mode (Qwen3.8's mmproj is vision-only).
            Images still work.
          </li>
          <li class="dim">
            Talking to <code>{beefy.llm_url}</code>{#if beefy.restore_url}, restores to
              <code>{beefy.restore_url}</code>{/if}.
          </li>
        </ul>
      {:else}
        <div class="beefy-head">
          <div>
            <div class="beefy-title off">Beefy LLM</div>
            <div class="beefy-sub">
              Hand the box to Qwen3.8-27B across both cards — ~21 GB of the box's 28.6 GB, ctx
              200,704 (gemma is 73,728), vision working{#if beefyAllow.length > 0}, with
                {beefyAllow.map(label).join(', ')} still running beside it{/if}. Currently on
              <code>{beefy.model ?? beefy.llm_url}</code>.
            </div>
          </div>
          <button class="btn-warning" disabled={beefyBusy} onclick={openConfirm}>
            Turn Beefy on…
          </button>
        </div>
      {/if}

      {#if beefy.director_llm_pinned}
        <p class="muted small">
          <strong>DIRECTOR_LLM_SERVICE_URL is set</strong> — Director traffic is pinned to its own
          llama-server and does <em>not</em> follow this flip.
        </p>
      {/if}

      {#if inconsistencies.length > 0}
        <!-- 🔴 Never quiet. `inconsistent` means the persisted flag and the live
             process disagree, which is the state where every other reading on
             this page is a guess. Both buttons are repair tools: entering
             re-verifies and finishes a half-enter, leaving restores
             unconditionally. -->
        <div class="beefy-inconsistent">
          <strong>⚠ Beefy state is inconsistent</strong>
          <ul>
            {#each inconsistencies as line, i (i)}
              <li>{line}</li>
            {/each}
          </ul>
          <p class="muted small">
            Both directions repair: turning it <em>off</em> restores unconditionally (URL,
            both-cards flag, gate lock, and it re-arms the drained gemma), turning it <em>on</em> re-verifies
            the weights.
          </p>
        </div>
      {/if}

      {#if beefyError}
        <div class="beefy-error">{beefyError}</div>
      {/if}
    </div>
  {:else if beefyError}
    <div class="card error">{beefyError}</div>
  {/if}

  {#if loading}
    <div class="card muted">Loading…</div>
  {:else}
    <div class="card">
      <ul class="rows">
        {#each services as svc (svc.name)}
          <li class="row">
            <span class="dot" class:on={svc.enabled} title={svc.enabled ? 'enabled' : 'disabled'}>
              {svc.enabled ? '●' : '○'}
            </span>
            <span class="name">{label(svc.name)}</span>
            <span class="state-tag" class:on={svc.enabled} class:busy={svc.busy}>
              {#if svc.busy}busy{:else if svc.enabled}enabled{:else}disabled{/if}
            </span>
            <button
              class="btn-sec"
              disabled={actionInFlight}
              onclick={() => handleToggle(svc.name, svc.enabled)}
              title={svc.enabled
                ? 'Disable + fire unload (VRAM clears)'
                : 'Re-enable — next request lazy-loads'}
            >
              {svc.enabled ? 'Disable' : 'Enable'}
            </button>
          </li>
        {/each}
      </ul>
    </div>

    <div class="card">
      <h3>Presets</h3>
      <div class="presets">
        <button class="btn-sec" disabled={actionInFlight} onclick={handleDisableAll}>
          Disable all
        </button>
        <button class="btn-sec" disabled={actionInFlight} onclick={handleEnableAll}>
          Enable all
        </button>
        <button class="btn-sec" disabled={actionInFlight} onclick={handleDisableExceptLlm}>
          Disable all except LLM
        </button>
      </div>
      <p class="muted small">
        "Disable all except LLM" keeps chat alive while benching the other GPU consumers. "Disable
        all" gets out of the way for an LLM-only benchmark. Beefy LLM above is that preset plus a
        model swap — and it also locks the gate, so nothing outside its allow-list can admit behind
        the evictions.
      </p>
    </div>
  {/if}
</div>

{#if confirmOpen}
  <!-- 🔴 A dialog, never a direct click. This evicts the whole box; a misclick
       must not be able to kill a four-hour render. -->
  <Modal title="Hand the box to the big LLM?" width="620px" onClose={() => (confirmOpen = false)}>
    <div class="confirm">
      <p class="lead">
        Qwen3.8-27B loads across <strong>both cards</strong> (ExLlamaV3, tensor parallel), taking
        <strong>~21 GB of the box's 28.6 GB</strong>. While it is on, the GPU gate refuses every
        {beefyAllow.length > 0 ? 'service not listed under “What keeps running”' : 'other service'}.
      </p>

      {#if beefyAllow.length > 0}
        <h4>What keeps running</h4>
        <ul class="keep">
          {#each beefyAllow as name (name)}
            <li>{label(name)}</li>
          {/each}
        </ul>
        <p class="note">
          These share what the 27B leaves on the 3060 — the 5060 Ti is held whole — so they can't
          all be loaded at once and will unload each other to take turns.
          {#if beefyAllow.includes('tts')}TTS works <em>streaming</em> only; buffered TTS is refused while
            Beefy is on.{/if}
        </p>
      {/if}

      <h4>What stops working</h4>
      <ul class="loss">
        {#each beefyStops as name (name)}
          <li>{label(name)}</li>
        {/each}
      </ul>
      <p class="note">
        ComfyUI itself is <em>not</em> stopped — koblem unloads its models and it co-exists on a ~284
        MiB residual. Anything queued directly in ComfyUI's own UI still cannot run, because its models
        are gone.
      </p>

      <h4>What you give up on the LLM itself</h4>
      <ul class="loss">
        <li>
          <strong>Audio input is lost.</strong> Qwen3.8's mmproj is vision-only; gemma's is unified vision+audio.
          Images still work in chat and the agent.
        </li>
        <li>
          <strong>A cold long prompt is slow.</strong> The first turn of a ~195k-token context takes about
          ten minutes to prefill, and decode falls from ~85 t/s on short turns to ~31 t/s at that depth.
        </li>
      </ul>
      <p class="note">
        You gain ctx 200,704 (gemma: 73,728), prefix reuse — later turns of the same conversation
        answer in seconds rather than re-reading the whole window — and vision that still works.
      </p>

      <h4>The queue</h4>
      <p class="note">
        Queued jobs <strong>wait</strong> — they are not cancelled, and they resume when Beefy is turned
        off. They will show "the box is held by Beefy LLM".
      </p>

      <h4>Pre-flight</h4>
      {#if liveJobs.length === 0}
        <p class="preflight ok">Nothing is on the GPU right now.</p>
      {:else}
        <p class="preflight bad">
          {liveJobs.length} job{liveJobs.length === 1 ? ' is' : 's are'} running. The server will refuse
          this unless you force it.
        </p>
        <ul class="live-list">
          {#each liveJobs as j (j.id)}
            <li>
              <span class="live-label">{j.label || j.kind}</span>
              <span class="live-state">{j.state}</span>
              <span class="live-eta">{etaText(j)}</span>
            </li>
          {/each}
        </ul>
      {/if}

      <label class="force-row">
        <input type="checkbox" bind:checked={force} />
        <span>
          Force — proceed over running jobs and over any service whose unload could not be
          confirmed. <strong>Running jobs will be starved</strong>, not cancelled: they keep their
          row and stop making progress.
        </span>
      </label>

      <p class="note dim">
        This request blocks for roughly 10–40 seconds while the box is evicted and the 27B loads. It
        answers with the final state — there is nothing to watch in the meantime.
      </p>

      {#if beefyError}
        <div class="beefy-error">{beefyError}</div>
      {/if}
    </div>

    {#snippet footer()}
      <button class="btn-sec" disabled={beefyBusy} onclick={() => (confirmOpen = false)}>
        Cancel
      </button>
      <button
        class="btn-warning"
        disabled={beefyBusy || (liveJobs.length > 0 && !force)}
        onclick={() => apply(true, force)}
      >
        {#if beefyBusy}
          Evicting the box and loading 25 GB…
        {:else if force}
          Force Beefy on
        {:else}
          Turn Beefy on
        {/if}
      </button>
    {/snippet}
  </Modal>
{/if}

<style>
  .page {
    max-width: 760px;
    margin: 24px auto;
    padding: 0 16px;
    color: var(--ink);
  }
  .header {
    margin-bottom: 16px;
  }
  .header h1 {
    margin: 0;
    font-size: 22px;
  }
  .muted {
    color: var(--muted);
    font-size: 13px;
    margin: 6px 0 0;
    line-height: 1.5;
  }
  .small {
    font-size: 12px;
  }
  .card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 20px;
    margin-bottom: 16px;
  }
  .card h3 {
    margin: 0 0 12px;
    font-size: 15px;
  }
  .error {
    border-color: var(--danger);
    color: var(--danger);
  }
  .rows {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .row {
    display: grid;
    grid-template-columns: 24px 1fr 100px 96px;
    align-items: center;
    gap: 12px;
    padding: 8px 4px;
    border-bottom: 1px solid var(--ring);
  }
  .row:last-child {
    border-bottom: none;
  }
  .dot {
    font-size: 16px;
    color: var(--muted);
    text-align: center;
  }
  .dot.on {
    color: var(--accent);
  }
  .name {
    font-size: 14px;
    font-weight: 500;
  }
  .state-tag {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
    text-align: right;
  }
  .state-tag.on {
    color: var(--accent);
  }
  .state-tag.busy {
    color: var(--danger);
  }
  .btn-sec {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
    padding: 6px 14px;
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn-sec:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .presets {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }

  /* ── Beefy ──────────────────────────────────────────────────────────────
     🔴 --warning, not --danger. This is a deliberate operator state, not a
     fault: the machine is doing exactly what it was told. */
  .beefy.on {
    border-color: var(--warning);
    background: color-mix(in srgb, var(--warning) 10%, var(--card));
  }
  .beefy-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }
  .beefy-title {
    font-size: 15px;
    font-weight: 700;
    letter-spacing: 0.04em;
    color: var(--warning);
  }
  .beefy-title.off {
    color: var(--ink);
    letter-spacing: normal;
  }
  .beefy-sub {
    color: var(--muted);
    font-size: 13px;
    line-height: 1.5;
    margin-top: 4px;
  }
  .warn-text {
    color: var(--warning);
  }
  .beefy-facts {
    margin: 14px 0 0;
    padding-left: 18px;
    font-size: 13px;
    line-height: 1.55;
    color: var(--muted);
  }
  .beefy-facts li {
    margin-bottom: 6px;
  }
  .beefy-facts li.dim {
    opacity: 0.75;
    font-size: 12px;
  }
  .beefy-inconsistent {
    margin-top: 14px;
    padding: 12px 14px;
    border: 1px solid var(--danger);
    border-radius: 12px;
    background: color-mix(in srgb, var(--danger) 10%, transparent);
    font-size: 13px;
  }
  .beefy-inconsistent strong {
    color: var(--danger);
  }
  .beefy-inconsistent ul {
    margin: 8px 0 0;
    padding-left: 18px;
    line-height: 1.5;
  }
  .beefy-error {
    margin-top: 12px;
    padding: 10px 12px;
    border: 1px solid var(--danger);
    border-radius: 10px;
    color: var(--danger);
    font-size: 13px;
    line-height: 1.5;
  }
  .btn-warning {
    background: color-mix(in srgb, var(--warning) 18%, transparent);
    border: 1px solid var(--warning);
    color: var(--warning);
    padding: 7px 16px;
    border-radius: 8px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn-danger {
    background: color-mix(in srgb, var(--danger) 16%, transparent);
    border: 1px solid var(--danger);
    color: var(--danger);
    padding: 7px 16px;
    border-radius: 8px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn-warning:disabled,
  .btn-danger:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* ── Confirm dialog ──────────────────────────────────────────────────── */
  .confirm {
    font-size: 13px;
    line-height: 1.55;
    color: var(--ink);
  }
  .confirm .lead {
    margin: 0 0 14px;
  }
  .confirm h4 {
    margin: 16px 0 6px;
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .loss,
  .keep {
    margin: 0;
    padding-left: 18px;
  }
  .loss li,
  .keep li {
    margin-bottom: 4px;
  }
  .note {
    margin: 8px 0 0;
    color: var(--muted);
  }
  .note.dim {
    font-size: 12px;
    opacity: 0.8;
  }
  .preflight {
    margin: 4px 0 0;
    font-weight: 600;
  }
  .preflight.ok {
    color: var(--success);
  }
  .preflight.bad {
    color: var(--danger);
  }
  .live-list {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    font-size: 12px;
  }
  .live-list li {
    display: flex;
    gap: 10px;
    align-items: baseline;
    padding: 4px 0;
    border-bottom: 1px solid var(--ring);
  }
  .live-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .live-state {
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-size: 11px;
  }
  .live-eta {
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }
  .force-row {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    margin-top: 16px;
    padding: 10px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--bg-2);
    color: var(--muted);
    cursor: pointer;
  }
  .force-row input {
    margin-top: 2px;
    flex: none;
  }
  code {
    font-size: 12px;
    padding: 0 4px;
    border-radius: 4px;
    background: var(--tag-bg);
    color: var(--tag-text);
  }
</style>
