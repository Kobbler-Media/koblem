<script lang="ts">
  // Multi-GPU scheduler config — edit each service's VRAM peak / home GPU /
  // candidate cards / who-can-evict-it (named, relational), toggle each GPU
  // on/off, and watch live occupancy (which service is on which card, free
  // budget). Live-editable so tuning the load-bearing peak table never needs a
  // recompile (handoff §8b).
  import { onMount, onDestroy } from 'svelte';
  import {
    listGpuCards,
    toggleGpuCard,
    listGpuEngineConfigs,
    upsertGpuEngineConfig,
    getGpuOccupancy,
    type GpuCard,
    type GpuEngineConfig,
    type GpuCardOccupancy
  } from '#lib/api/client.ts';

  let cards = $state<GpuCard[]>([]);
  let engines = $state<GpuEngineConfig[]>([]);
  let occupancy = $state<GpuCardOccupancy[]>([]);
  let loading = $state(true);
  let error = $state('');
  let actionInFlight = $state(false);
  let savedService = $state('');

  // Editable working copies of each engine config, keyed by service.
  let drafts = $state<Record<string, GpuEngineConfig>>({});

  let pollTimer: ReturnType<typeof setInterval> | undefined;

  async function load() {
    error = '';
    try {
      const [c, e, o] = await Promise.all([
        listGpuCards(),
        listGpuEngineConfigs(),
        getGpuOccupancy()
      ]);
      cards = c.cards;
      engines = e.engines;
      occupancy = o.cards;
      // Seed drafts (deep-ish copy so editing doesn't mutate the source list).
      const next: Record<string, GpuEngineConfig> = {};
      for (const cfg of engines) {
        next[cfg.service] = {
          ...cfg,
          candidate_gpus: [...cfg.candidate_gpus],
          evictable_by: [...(cfg.evictable_by ?? [])]
        };
      }
      drafts = next;
    } catch (err) {
      error = err instanceof Error ? err.message : 'failed to load scheduler config';
    }
    loading = false;
  }

  async function refreshOccupancy() {
    try {
      const o = await getGpuOccupancy();
      occupancy = o.cards;
    } catch {
      // Non-fatal — occupancy is informational; keep the last good snapshot.
    }
  }

  onMount(() => {
    load();
    // Live occupancy refresh while the page is open.
    pollTimer = setInterval(refreshOccupancy, 3000);
  });
  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
  });

  async function handleToggleCard(uuid: string, currentlyEnabled: boolean) {
    actionInFlight = true;
    try {
      await toggleGpuCard(uuid, !currentlyEnabled);
      await load();
    } catch (err) {
      error = err instanceof Error ? err.message : `toggle failed for ${uuid}`;
    } finally {
      actionInFlight = false;
    }
  }

  function toggleCandidate(service: string, uuid: string) {
    const d = drafts[service];
    if (!d) return;
    const idx = d.candidate_gpus.indexOf(uuid);
    if (idx >= 0) {
      d.candidate_gpus = d.candidate_gpus.filter((u) => u !== uuid);
    } else {
      d.candidate_gpus = [...d.candidate_gpus, uuid];
    }
  }

  // Named, relational eviction: toggle whether `evictor` may evict `service`
  // (i.e. whether evictor ∈ service.evictable_by). A service never lists itself.
  function toggleEvictor(service: string, evictor: string) {
    const d = drafts[service];
    if (!d) return;
    if (d.evictable_by.includes(evictor)) {
      d.evictable_by = d.evictable_by.filter((s) => s !== evictor);
    } else {
      d.evictable_by = [...d.evictable_by, evictor];
    }
  }

  // The list of services that may appear as evictors (every service except the
  // one being edited). Derived from the loaded config so it stays in sync.
  const allServiceIds = $derived(engines.map((e) => e.service));

  async function saveEngine(service: string) {
    const d = drafts[service];
    if (!d) return;
    actionInFlight = true;
    savedService = '';
    try {
      await upsertGpuEngineConfig(service, {
        peak_mb: d.peak_mb,
        home_gpu: d.home_gpu,
        candidate_gpus: d.candidate_gpus,
        evictable_by: d.evictable_by,
        relocatable: d.relocatable,
        rules_json: d.rules_json ?? ''
      });
      savedService = service;
      await refreshOccupancy();
      setTimeout(() => {
        if (savedService === service) savedService = '';
      }, 2000);
    } catch (err) {
      error = err instanceof Error ? err.message : `save failed for ${service}`;
    } finally {
      actionInFlight = false;
    }
  }

  const SERVICE_LABELS: Record<string, string> = {
    llm: 'LLM',
    tts: 'TTS (BreezeTTS 2)',
    music: 'Music (ACE-Step)',
    sa3: 'Music (SA3)',
    vision: 'Embeddings (EmbeddingGemma-2)',
    text_embed: 'Text embeddings (EmbeddingGemma-2)',
    stt: 'STT (parakeet)',
    flux: 'Images (FLUX.2)',
    krea2: 'Images (Krea 2)',
    songgen: 'Music: SongGen (LeVo2)',
    ltx: 'Video (LTX-2.5)',
    matting: 'Matting (RMBG-2.0)',
    spar3d: '3D (SPAR3D + RigAnything)'
  };
  function serviceLabel(name: string): string {
    return SERVICE_LABELS[name] ?? name;
  }
  function cardLabel(uuid: string): string {
    return cards.find((c) => c.uuid === uuid)?.label ?? uuid.slice(0, 12);
  }
  function pct(used: number, total: number): number {
    return total > 0 ? Math.min(100, Math.round((used / total) * 100)) : 0;
  }
</script>

<div class="page">
  <header class="header">
    <div>
      <h1>GPU Scheduler</h1>
      <p class="muted">
        Pure VRAM-budget placement across both cards — no lanes. A service is admitted to a card iff
        its tested peak fits the remaining budget, else it evicts the residents that NAME it as an
        evictor (relocating the ones that fit elsewhere), else it queues. Eviction is named &amp;
        relational — each service declares who may evict it; a service with no evictors is never
        auto-evicted. Edit each service's peak, home GPU, candidates, who-can-evict-it and the
        relocatable flag; toggle a card off to drain it. Changes apply live — no restart.
      </p>
    </div>
  </header>

  {#if error}
    <div class="card error">{error}</div>
  {/if}

  {#if loading}
    <div class="card muted">Loading…</div>
  {:else}
    <!-- Live occupancy -->
    <div class="card">
      <h3>Live occupancy</h3>
      <div class="cards-grid">
        {#each occupancy as occ (occ.uuid)}
          <div class="gpu-card" class:disabled={!occ.enabled}>
            <div class="gpu-head">
              <span class="gpu-name">
                {cardLabel(occ.uuid)}
                {#if occ.fast}<span class="badge">fast</span>{/if}
                {#if !occ.enabled}<span class="badge off">disabled</span>{/if}
              </span>
              <button
                class="btn-sec sm"
                disabled={actionInFlight}
                onclick={() => handleToggleCard(occ.uuid, occ.enabled)}
              >
                {occ.enabled ? 'Disable' : 'Enable'}
              </button>
            </div>
            <div class="bar">
              <div class="bar-fill" style={`width:${pct(occ.used_mb, occ.total_mb)}%`}></div>
            </div>
            <div class="gpu-meta">
              {occ.used_mb} / {occ.total_mb} MiB used · {occ.free_mb} MiB free
              {#if occ.measured_free_mb != null}
                · <span class:drift={occ.measured_free_mb < occ.free_mb}
                  >{occ.measured_free_mb} MiB measured free</span
                >
              {/if}
              {#if occ.reserved_mb != null}
                · {occ.reserved_mb} MiB held by Beefy
              {/if}
            </div>
            <ul class="residents">
              {#each occ.residents as r (r.service)}
                <li>
                  {serviceLabel(r.service)}
                  <span class="muted">({r.peak_mb} MiB)</span>
                </li>
              {:else}
                <li class="muted">idle</li>
              {/each}
            </ul>
          </div>
        {/each}
      </div>
    </div>

    <!-- Per-service config -->
    <div class="card">
      <h3>Service placement config</h3>
      <p class="muted small">
        Peak = tested worst-case VRAM (+margin) reserved while resident. Home = where
        un-targeted/direct requests land. Candidates = cards the gate may place on (home first, then
        spill). "Can be evicted by" = the named services allowed to boot this one to reclaim its
        card; none selected = never auto-evicted.
      </p>
      <div class="engines">
        {#each engines as eng (eng.service)}
          {@const d = drafts[eng.service]}
          {#if d}
            <div class="engine-row">
              <div class="engine-name">{serviceLabel(eng.service)}</div>
              <label class="fld">
                <span>Peak (MiB)</span>
                <input type="number" min="0" step="100" bind:value={d.peak_mb} />
              </label>
              <label class="fld">
                <span>Home GPU</span>
                <select bind:value={d.home_gpu}>
                  <option value="">(none)</option>
                  {#each cards as c (c.uuid)}
                    <option value={c.uuid}>{c.label}</option>
                  {/each}
                </select>
              </label>
              <div class="fld candidates">
                <span>Candidates</span>
                <div class="chips">
                  {#each cards as c (c.uuid)}
                    <button
                      type="button"
                      class="chip"
                      class:on={d.candidate_gpus.includes(c.uuid)}
                      onclick={() => toggleCandidate(eng.service, c.uuid)}
                    >
                      {c.label}
                    </button>
                  {/each}
                </div>
              </div>
              <div class="fld candidates evictors">
                <span>Can be evicted by</span>
                <div class="chips">
                  {#each allServiceIds.filter((s) => s !== eng.service) as svc (svc)}
                    <button
                      type="button"
                      class="chip"
                      class:on={d.evictable_by.includes(svc)}
                      onclick={() => toggleEvictor(eng.service, svc)}
                    >
                      {serviceLabel(svc)}
                    </button>
                  {/each}
                </div>
                {#if d.evictable_by.length === 0}
                  <span class="never">never auto-evicted</span>
                {/if}
              </div>
              <div class="flags">
                <label class="fld reloc">
                  <input type="checkbox" bind:checked={d.relocatable} />
                  <span>Relocatable</span>
                </label>
              </div>
              <label class="fld rules">
                <span>Rules (JSON)</span>
                <input type="text" placeholder="(reserved)" bind:value={d.rules_json} />
              </label>
              <button
                class="btn-pri sm"
                disabled={actionInFlight}
                onclick={() => saveEngine(eng.service)}
              >
                {savedService === eng.service ? 'Saved ✓' : 'Save'}
              </button>
            </div>
          {/if}
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .page {
    max-width: 980px;
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
  .cards-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
    gap: 14px;
  }
  .gpu-card {
    border: 1px solid var(--ring);
    border-radius: 12px;
    padding: 14px;
  }
  .gpu-card.disabled {
    opacity: 0.6;
  }
  .gpu-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    margin-bottom: 8px;
  }
  .gpu-name {
    font-weight: 600;
    font-size: 14px;
  }
  .badge {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    padding: 1px 5px;
    margin-left: 6px;
    color: var(--accent);
  }
  .badge.off {
    color: var(--danger);
  }
  .bar {
    height: 8px;
    border-radius: 6px;
    background: var(--bg-2);
    overflow: hidden;
    margin-bottom: 6px;
  }
  .bar-fill {
    height: 100%;
    background: var(--accent);
  }
  .gpu-meta {
    font-size: 12px;
    color: var(--muted);
    margin-bottom: 8px;
  }
  .gpu-meta .drift {
    color: var(--danger);
    font-weight: 600;
  }
  .residents {
    list-style: none;
    margin: 0;
    padding: 0;
    font-size: 13px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .engines {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .engine-row {
    display: grid;
    grid-template-columns: 1.3fr 0.8fr 1fr 1.6fr 2fr 0.9fr 1.1fr 0.7fr;
    align-items: end;
    gap: 10px;
    padding: 10px 0;
    border-bottom: 1px solid var(--ring);
  }
  .flags {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .engine-row:last-child {
    border-bottom: none;
  }
  .engine-name {
    font-size: 14px;
    font-weight: 500;
    align-self: center;
  }
  .fld {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 11px;
    color: var(--muted);
  }
  .fld input[type='number'],
  .fld input[type='text'],
  .fld select {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
    border-radius: 6px;
    padding: 5px 8px;
    font-size: 13px;
    width: 100%;
    box-sizing: border-box;
  }
  .fld.reloc {
    flex-direction: row;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: var(--ink);
  }
  .candidates .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
  }
  .evictors .never {
    font-size: 10px;
    color: var(--muted);
    font-style: italic;
    margin-top: 2px;
  }
  .chip {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--muted);
    border-radius: 6px;
    padding: 4px 8px;
    font-size: 12px;
    cursor: pointer;
  }
  .chip.on {
    color: var(--ink);
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 15%, transparent);
  }
  .btn-sec,
  .btn-pri {
    border-radius: 8px;
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
    padding: 6px 14px;
  }
  .btn-sec {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    color: var(--ink);
  }
  .btn-pri {
    background: var(--accent);
    border: 1px solid var(--accent);
    color: var(--bg);
  }
  .sm {
    padding: 4px 10px;
    font-size: 12px;
  }
  .btn-sec:disabled,
  .btn-pri:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
