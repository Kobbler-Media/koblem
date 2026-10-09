<script lang="ts">
  /*
   * 3D → Pixal3D. Photo in, a rigged character out, then prompts against it forever.
   *
   * 🔴 THE TIMINGS ARE LOPSIDED AND THEY ARE THE WHOLE LAYOUT:
   *
   *     rig      ~450 s first tier, ~150 s each after   ONCE per character
   *     motion   ~11 s warm / ~33 s cold                per prompt
   *     animate  0.22 s                                 per take
   *
   * So this is a character LIBRARY plus a fast prompt loop, not a form with a submit button. The
   * animation prompt is deliberately not on the upload form: the rig is the artefact, and prompts
   * are what you do to it afterwards, repeatedly.
   *
   * ⚠️ THERE IS NO PROGRESS ON A RIG. The engine's job record publishes queued/running/completed
   * and nothing else — no phase, no percentage. Everything shown while one runs is elapsed time
   * against a measured expectation, and it is labelled as such. Do not add a progress bar.
   *
   * 🔴 GATES ARE ADVISORY. `accepted: false` is a normal outcome that still ships an asset; the
   * owner judged a rig failing the pose gate at 25.9 "decent". Show the verdict, never withhold the
   * model. A tier whose gate reports `draws: 0` inherited the shared skeleton and ran no gate at
   * all — rendering that block would report a failure that did not happen.
   *
   * 🔴 A "TRY AGAIN" MUST MOVE A SEED. The rig decode is deterministic in (mesh, seed); re-running
   * unchanged returns the draw you just rejected, 205 s later. The re-roll buttons here each pick a
   * fresh one, and the server refuses a repeat.
   */
  import { onDestroy, onMount } from 'svelte';
  import FileDropzone from '$lib/components/FileDropzone.svelte';
  import RigWorkbench from './RigWorkbench.svelte';
  import {
    animatePixal3dCharacter,
    cancelPixal3dCharacter,
    cancelPixal3dMotion,
    createPixal3dCharacter,
    createPixal3dMotion,
    deletePixal3dAnimation,
    deletePixal3dCharacter,
    deletePixal3dEdit,
    deletePixal3dMotion,
    getPixal3dCharacter,
    listPixal3dCharacters,
    listPixal3dMotions,
    mediaUrl,
    pixal3dActive,
    pixal3dAnimationPath,
    pixal3dBundlePath,
    pixal3dBundleUrl,
    pixal3dEditPath,
    pixal3dModelPath,
    pixal3dProps,
    pixal3dSourcePath,
    prefetchMediaSignatures,
    renamePixal3dCharacter,
    rerigPixal3dCharacter,
    savePixal3dEdit
  } from '$lib/api/client';
  import type { EnqueueLike, QueueSubmitOptions } from '$lib/queue/options';
  import QueueChip from '$lib/components/queue/QueueChip.svelte';
  import type {
    Pixal3dBusy,
    Pixal3dCharacter,
    Pixal3dMotion,
    Pixal3dProps,
    Pixal3dTier
  } from '$lib/types';

  const POLL_MS = 2500;

  let props = $state<Pixal3dProps | null>(null);
  let characters = $state<Pixal3dCharacter[]>([]);
  let motions = $state<Pixal3dMotion[]>([]);
  let busySlot = $state<Pixal3dBusy | null>(null);
  let selectedId = $state('');
  let error = $state('');
  let notice = $state('');
  let now = $state(Math.floor(Date.now() / 1000));

  // ── upload form ──────────────────────────────────────────────────────
  let files = $state<FileList | null>(null);
  let file = $derived(files?.length ? files[0] : null);
  let newName = $state('');
  let wantTiers = $state<string[]>(['hero', 'game']);
  let rigRetries = $state<number | null>(null);
  let resolution = $state(0);
  let submitting = $state(false);
  // Rig and motion are separate submits onto the same shared slot, so they carry
  // separate overrides rather than one set that silently applies to both.
  let rigQueue = $state<QueueSubmitOptions>({});
  let rigAccepted = $state<EnqueueLike | null>(null);

  // ── motion form ──────────────────────────────────────────────────────
  let prompt = $state('');
  let motionSeconds = $state(4);
  let motionSeed = $state<number | null>(null);
  let motionBusy = $state(false);
  let motionQueue = $state<QueueSubmitOptions>({});
  let motionAccepted = $state<EnqueueLike | null>(null);

  // ── animate form ─────────────────────────────────────────────────────
  let pickedMotions = $state<string[]>([]);
  let animateTier = $state('hero');
  let animateEdit = $state('');
  let variant = $state('rotbase');
  let exercise = $state(true);
  let takeLabel = $state('');

  // ── viewer ───────────────────────────────────────────────────────────
  type View =
    | { kind: 'tier'; tier: string }
    | { kind: 'edit'; id: string }
    | { kind: 'animation'; id: string };
  let view = $state<View>({ kind: 'tier', tier: 'hero' });
  /** Bumped to force the viewer to re-fetch the same path — the only honest undo for an in-place
   *  browser edit, and what a newly-baked take needs so it is not served from a stale blob. */
  let reloadNonce = $state(0);

  //: 🔴 EVERY PIECE OF EDITING STATE MOVED INTO `RigWorkbench`, and that is the point of the
  //: rebuild rather than an accident of it. The viewer and the editor used to be siblings here, so
  //: this panel held the state they had to agree on — which joint is selected, which triangle was
  //: last picked, whether the grab handles are up. The result was a control panel scrolled far away
  //: from the model it described, and a `selectedComponent` that highlighted a row in a list and
  //: nothing on the character. The workbench OWNS the canvas now, so the agreement is internal and
  //: this panel is back to what it is good at: the library, the forms, and which model is on screen.

  let timer: ReturnType<typeof setInterval> | null = null;

  const selected = $derived(characters.find((c) => c.id === selectedId) ?? null);
  const running = $derived(
    characters.filter((c) => !['done', 'failed', 'cancelled'].includes(c.state))
  );
  const runningMotions = $derived(
    motions.filter((m) => !['done', 'failed', 'cancelled'].includes(m.state))
  );
  const readyMotions = $derived(motions.filter((m) => m.state === 'done'));
  const engineDown = $derived(!!props && (!props.configured || !props.healthy));

  const viewPath = $derived.by(() => {
    const v = view;
    if (!selected) return '';
    // The nonce rides in the query so a re-fetch is a different URL to the browser cache without
    // changing which asset it names. Appended with the right separator: the tier route already
    // carries one parameter and the others carry none.
    const bust = (path: string) =>
      reloadNonce ? `${path}${path.includes('?') ? '&' : '?'}v=${reloadNonce}` : path;
    if (v.kind === 'tier') {
      const tier = selected.tiers.find((t) => t.tier === v.tier) ?? selected.tiers[0];
      return tier ? bust(pixal3dModelPath(selected.id, tier.tier)) : '';
    }
    if (v.kind === 'edit') {
      const edit = selected.edits.find((e) => e.id === v.id);
      return edit ? bust(pixal3dEditPath(selected.id, edit.id)) : '';
    }
    const animation = selected.animations.find((a) => a.id === v.id);
    return animation?.filename ? bust(pixal3dAnimationPath(selected.id, animation.id)) : '';
  });

  const viewLabel = $derived.by(() => {
    const v = view;
    if (!selected) return '';
    if (v.kind === 'tier') return `${v.tier} tier`;
    if (v.kind === 'edit') return selected.edits.find((e) => e.id === v.id)?.label ?? 'edited rig';
    return selected.animations.find((a) => a.id === v.id)?.label ?? 'take';
  });

  /** The tier a rig repair is based on. A saved repair is still a rig, so it stays editable against
   *  whatever tier it came from — chaining a weight prune onto a joint fix should not require
   *  starting over. A take is baked output and has no tier. */
  const editableTier = $derived.by(() => {
    const v = view;
    if (v.kind === 'tier') return v.tier;
    if (v.kind === 'edit') {
      const edit = selected?.edits.find((e) => e.id === v.id);
      return edit && edit.base_tier !== 'take' ? edit.base_tier : '';
    }
    return '';
  });

  // Keep the tier selections pointing at tiers this character actually has. A ladder is a
  // request, not a guarantee: `{tiers: [hero, game]}` can come back with only the hero if a later
  // rung failed, and a stale `hero` here would submit an animate the engine 400s.
  $effect(() => {
    const c = selected;
    const v = view;
    if (!c || !c.tiers.length) return;
    if (!c.tiers.some((t) => t.tier === animateTier)) animateTier = c.tiers[0].tier;
    if (v.kind === 'tier' && !c.tiers.some((t) => t.tier === v.tier)) {
      view = { kind: 'tier', tier: c.tiers[0].tier };
    }
  });

  onMount(async () => {
    try {
      props = await pixal3dProps();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
    await Promise.all([refreshCharacters(), refreshMotions(), refreshActive()]);
    timer = setInterval(tick, POLL_MS);
  });

  onDestroy(() => {
    if (timer) clearInterval(timer);
  });

  async function tick() {
    now = Math.floor(Date.now() / 1000);
    const live = running.length > 0 || runningMotions.length > 0;
    const selectedLive =
      selected &&
      (!['done', 'failed', 'cancelled'].includes(selected.state) ||
        selected.animations.some((a) => !['done', 'failed', 'cancelled'].includes(a.state)));
    if (!live && !selectedLive) return;
    await Promise.all([
      live ? refreshCharacters() : Promise.resolve(),
      runningMotions.length ? refreshMotions() : Promise.resolve(),
      selectedLive && !live ? refreshSelected() : Promise.resolve(),
      refreshActive()
    ]);
  }

  async function refreshCharacters() {
    try {
      const { characters: list } = await listPixal3dCharacters();
      characters = list;
      if (!selectedId && list.length) selectedId = list[0].id;
      // Sign the thumbnails rather than letting `mediaUrl` fall back to `?token=`, which would put
      // a real API key in browser history and access logs for every character in the library.
      const sources = list.filter((c) => c.source_filename).map((c) => pixal3dSourcePath(c.id));
      if (sources.length) await prefetchMediaSignatures(sources).catch(() => {});
    } catch {
      // A transient poll failure must not blank the library.
    }
  }

  async function refreshSelected() {
    if (!selectedId) return;
    try {
      const fresh = await getPixal3dCharacter(selectedId);
      characters = characters.map((c) => (c.id === fresh.id ? fresh : c));
    } catch {
      /* transient */
    }
  }

  async function refreshMotions() {
    try {
      const { motions: list } = await listPixal3dMotions();
      motions = list;
    } catch {
      /* transient */
    }
  }

  async function refreshActive() {
    try {
      const { active } = await pixal3dActive();
      busySlot = active;
    } catch {
      /* transient */
    }
  }

  function toggleTier(tier: string) {
    wantTiers = wantTiers.includes(tier)
      ? wantTiers.filter((t) => t !== tier)
      : [...wantTiers, tier];
  }

  async function submit() {
    if (!file) {
      error = 'Add a character image first.';
      return;
    }
    if (!wantTiers.length) {
      error = 'Pick at least one tier.';
      return;
    }
    submitting = true;
    error = '';
    notice = '';
    rigAccepted = null;
    try {
      const accepted = await createPixal3dCharacter(
        file,
        {
          name: newName.trim() || undefined,
          tiers: wantTiers,
          rig_retries: rigRetries ?? undefined,
          resolution: resolution || undefined
        },
        rigQueue
      );
      rigAccepted = accepted;
      files = null;
      newName = '';
      await refreshCharacters();
      selectedId = accepted.id;
      notice = queuedNote(accepted.position);
      view = { kind: 'tier', tier: wantTiers[0] };
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  /** Where a fresh submit landed. The rig/motion slot is shared, so "queued" is ordinary and worth
   *  saying with a number rather than leaving as silence. */
  function queuedNote(position: number | null): string {
    return position && position > 1 ? `Queued — ${position - 1} job(s) ahead of it.` : '';
  }

  /** A fresh seed, because a repeat is a paid no-op. */
  function newSeed(): number {
    return Math.floor(Math.random() * 4_000_000_000) + 1;
  }

  async function reroll(mode: 'rig_seed' | 'geo_seed') {
    if (!selected) return;
    error = '';
    notice = '';
    try {
      const request =
        mode === 'rig_seed'
          ? ({ mode, rig_seed: newSeed(), tiers: selected.tiers_requested } as const)
          : ({
              mode,
              geo_seed: newSeed() % 2_000_000_000,
              tiers: selected.tiers_requested
            } as const);
      const accepted = await rerigPixal3dCharacter(selected.id, request, rigQueue);
      rigAccepted = accepted;
      await refreshCharacters();
      selectedId = accepted.id;
      notice = [
        mode === 'rig_seed'
          ? 'Re-rigging on the cached geometry — about 205 s. Same body, a different skeleton draw.'
          : 'Redrawing the mesh — about 450 s. This is a genuinely different character, not a re-roll of the same one.',
        queuedNote(accepted.position)
      ]
        .filter(Boolean)
        .join(' ');
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function bakeTiers(extra: string[]) {
    if (!selected || !extra.length) return;
    error = '';
    try {
      const accepted = await rerigPixal3dCharacter(
        selected.id,
        { mode: 'tiers', tiers: extra },
        rigQueue
      );
      rigAccepted = accepted;
      await refreshCharacters();
      selectedId = accepted.id;
      notice = [
        'Baking onto the SAME skeleton — about 134 s a tier, no rig decode. A clip authored on the hero plays on these unchanged.',
        queuedNote(accepted.position)
      ]
        .filter(Boolean)
        .join(' ');
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function cancel(id: string, kind: 'rig' | 'motion') {
    try {
      if (kind === 'rig') await cancelPixal3dCharacter(id);
      else await cancelPixal3dMotion(id);
      await Promise.all([refreshCharacters(), refreshMotions()]);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function removeCharacter(id: string) {
    try {
      await deletePixal3dCharacter(id);
      if (selectedId === id) selectedId = '';
      await refreshCharacters();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function rename(id: string, name: string) {
    const trimmed = name.trim();
    if (!trimmed) return;
    try {
      await renamePixal3dCharacter(id, trimmed);
      characters = characters.map((c) => (c.id === id ? { ...c, name: trimmed } : c));
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function generateMotion() {
    const text = prompt.trim();
    if (!text) return;
    motionBusy = true;
    error = '';
    motionAccepted = null;
    try {
      const accepted = await createPixal3dMotion(
        {
          prompt: text,
          seconds: motionSeconds,
          seed: motionSeed ?? undefined
        },
        motionQueue
      );
      motionAccepted = accepted;
      notice = [
        accepted.dit_resident
          ? ''
          : 'First prompt of the session — this one pays ~21 s to load the motion model. The rest are ~11 s.',
        queuedNote(accepted.position)
      ]
        .filter(Boolean)
        .join(' ');
      await refreshMotions();
      pickedMotions = [...pickedMotions, accepted.id];
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      motionBusy = false;
    }
  }

  /** Same prompt, new seed — a genuinely different take, and ~11 s. The seed is a real RNG seed,
   *  which is what makes this worth a button rather than a re-type. */
  async function rerollMotion(m: Pixal3dMotion) {
    motionBusy = true;
    error = '';
    try {
      const accepted = await createPixal3dMotion(
        {
          prompt: m.prompt,
          seconds: m.seconds ?? motionSeconds,
          seed: newSeed() % 4_294_967_295
        },
        motionQueue
      );
      motionAccepted = accepted;
      await refreshMotions();
      pickedMotions = [...pickedMotions.filter((id) => id !== m.id), accepted.id];
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      motionBusy = false;
    }
  }

  function togglePicked(id: string) {
    pickedMotions = pickedMotions.includes(id)
      ? pickedMotions.filter((m) => m !== id)
      : [...pickedMotions, id];
  }

  async function bakeTake() {
    if (!selected) return;
    const ready = pickedMotions.filter((id) => readyMotions.some((m) => m.id === id));
    if (!ready.length) {
      error = 'Pick at least one finished motion.';
      return;
    }
    error = '';
    try {
      const accepted = await animatePixal3dCharacter(selected.id, {
        motion_ids: ready,
        rig_source: animateEdit
          ? { kind: 'edit', edit_id: animateEdit }
          : { kind: 'tier', tier: animateTier },
        label: takeLabel.trim() || undefined,
        variant,
        exercise
      });
      takeLabel = '';
      await refreshSelected();
      view = { kind: 'animation', id: accepted.id };
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function removeAnimation(id: string) {
    if (!selected) return;
    try {
      await deletePixal3dAnimation(selected.id, id);
      if (view.kind === 'animation' && view.id === id) view = { kind: 'tier', tier: animateTier };
      await refreshSelected();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function removeEdit(id: string) {
    if (!selected) return;
    try {
      await deletePixal3dEdit(selected.id, id);
      if (view.kind === 'edit' && view.id === id) view = { kind: 'tier', tier: animateTier };
      if (animateEdit === id) animateEdit = '';
      await refreshSelected();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function removeMotion(id: string) {
    try {
      await deletePixal3dMotion(id);
      pickedMotions = pickedMotions.filter((m) => m !== id);
      await refreshMotions();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function download(id: string) {
    // Sign first: a `download` anchor cannot set an Authorization header, and the fallback puts a
    // real API key in browser history and access logs.
    try {
      await prefetchMediaSignatures([pixal3dBundlePath(id)]);
    } catch {
      /* signing is an optimisation for the URL's shape, not a gate on the download */
    }
    window.location.href = pixal3dBundleUrl(id);
  }

  /**
   * Where a repair's bytes go. The workbench builds the patched GLB and knows nothing about how this
   * host stores things — which is what lets the same editor serve SPAR3D, whose assets have no record
   * to attach an edit to and which therefore takes the workbench's download fallback instead.
   *
   * 🔴 `base_tier` cannot be derived inside the workbench: a take has no tier at all, and a repair of
   * a repair has to inherit the tier its parent came from rather than name itself. `editableTier`
   * already resolves both cases — it is `''` for anything that is not an editable rig, which is
   * exactly when the right answer is `'take'`.
   */
  async function saveRepair(
    glb: Blob,
    meta: { label: string; changes: Record<string, unknown> }
  ): Promise<{ note?: string; animatable?: boolean }> {
    if (!selectedId) throw new Error('no character is selected');
    const result = await savePixal3dEdit(selectedId, glb, {
      label: meta.label,
      base_tier: editableTier || 'take',
      changes: meta.changes
    });
    return { note: result.note ?? undefined, animatable: result.animatable };
  }

  function onEditSaved() {
    void refreshSelected();
    reloadNonce++;
  }

  function sourceThumb(c: Pixal3dCharacter): string {
    return c.source_filename ? mediaUrl(pixal3dSourcePath(c.id)) : '';
  }

  function elapsed(c: Pixal3dCharacter): number {
    return Math.max(0, now - c.created_unix);
  }

  /** What a running rig should take, given what it was asked for. A range, because the measured
   *  numbers are a sample and not a bound. */
  function expectation(origin: string, tierCount: number): string {
    if (!props) return '';
    const [firstLow, firstHigh] = props.timings.rig_first_tier_seconds;
    const [extraLow, extraHigh] = props.timings.rig_extra_tier_seconds;
    if (origin === 'rig_seed') return `~${props.timings.rerig_from_geometry_seconds} s expected`;
    if (origin === 'tiers')
      return `~${props.timings.rebake_from_rig_cache_seconds} s a tier expected`;
    const extra = Math.max(0, (tierCount || 1) - 1);
    return `~${firstLow + extra * extraLow}–${firstHigh + extra * extraHigh} s expected`;
  }

  function gateWord(tier: Pixal3dTier): { text: string; tone: 'ok' | 'warn' | 'muted' } {
    // 🔴 `draws: 0` means this tier inherited the skeleton and ran no gate. Reporting its empty
    // verdict would say "failed" about work that never happened.
    if (tier.gate.draws === 0) return { text: 'inherited skeleton', tone: 'muted' };
    if (tier.gate.accepted) return { text: 'accepted', tone: 'ok' };
    return { text: 'shipped, gate not met', tone: 'warn' };
  }

  function fmtBytes(n: number): string {
    if (!n) return '—';
    const mb = n / 1024 ** 2;
    return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GB` : `${mb.toFixed(1)} MB`;
  }

  function fmtSeconds(s: number | null): string {
    if (s === null || s === undefined) return '—';
    return s >= 60 ? `${Math.floor(s / 60)}m ${Math.round(s % 60)}s` : `${s.toFixed(0)}s`;
  }
</script>

<div class="pixal">
  <p class="muted panel-status">
    longcat-rig · photo → rigged LOD ladder → prompted animation ·
    {#if props}
      {#if props.configured && props.healthy}
        <span class="ok">engine up</span>
        {#if props.health?.waiting_for_gpu_lock}
          · <span class="warn-inline"
            >another process on this box holds the card — the engine is idle and cannot start</span
          >
        {:else if busySlot}
          <!-- The slot is queue-backed, so the row may be WAITING rather than running — saying
               "busy" for a job that has not started sends someone looking at the GPU. -->
          ·
          <span class="warn-inline">
            {busySlot.state === 'running' || busySlot.state === 'collecting' ? 'busy' : 'queued'}:
            {busySlot.kind} “{busySlot.label}”
          </span>
        {:else if props.health && !props.health.loaded}
          · motion model unloaded, so the next prompt pays ~{props.timings.motion_cold_seconds} s
        {/if}
      {:else}
        <span class="down">engine offline</span>
      {/if}
    {/if}
  </p>

  {#if engineDown}
    <div class="error">
      Pixal3D is not available. Set <code>pixal3d_server_url</code> in Settings (normally
      <code>http://longcat-rig:8102</code>), then check it there.
    </div>
  {/if}
  {#if error}<div class="error">{error}</div>{/if}
  {#if notice}<div class="notice">{notice}</div>{/if}

  <div class="layout">
    <!-- ── the library ──────────────────────────────────────────────── -->
    <aside class="library">
      <section class="card">
        <h3>New character</h3>
        <p class="hint">
          A rig is <strong>~7½ minutes</strong> and happens once. Everything after it — prompts, takes,
          tier swaps — is seconds.
        </p>
        <div class="dz">
          <FileDropzone
            accept="image/*"
            disabled={submitting || engineDown}
            label={file ? 'Replace image' : 'Character image'}
            hint="PNG, JPEG or WebP — one clear subject"
            onFiles={(v) => (files = v)}
          />
        </div>
        {#if file}<p class="chosen">{file.name} · {fmtBytes(file.size)}</p>{/if}
        <label class="field">
          Name
          <input type="text" bind:value={newName} placeholder="from the filename" />
        </label>

        <div class="field">
          Tiers
          <div class="tier-picks">
            {#each props?.tiers ?? [] as tier (tier)}
              <label class="toggle">
                <input
                  type="checkbox"
                  checked={wantTiers.includes(tier)}
                  onchange={() => toggleTier(tier)}
                />
                {tier}
              </label>
            {/each}
          </div>
        </div>
        <p class="hint">
          All tiers share <strong>one skeleton</strong>, so a clip authored on the hero plays on the
          game tier unchanged. Only the first pays for geometry and the rig decode; the rest are a
          bake each.
        </p>

        <details class="advanced">
          <summary>Quality levers</summary>
          <p class="hint">
            Both of these are per-subject rather than upgrades. Leave them alone unless you have a
            specific complaint about a specific character.
          </p>
          <label class="field">
            Geometry lattice
            <select bind:value={resolution}>
              <option value={0}>1024 — the default</option>
              <option value={1536}>1536 — better hands on some, a worse rig on others</option>
            </select>
          </label>
          <label class="field">
            Best-of-N draws
            <input
              type="number"
              min="0"
              max="16"
              placeholder={String(props?.health?.rig_retries_default ?? 2)}
              value={rigRetries ?? ''}
              oninput={(e) => {
                const v = (e.currentTarget as HTMLInputElement).value;
                rigRetries = v === '' ? null : Number(v);
              }}
            />
          </label>
          <p class="hint">
            🔴 A rejected draw is usually a ~214 s runaway, not a quick retry — raising this can
            turn a 450 s rig into ~900 s. The engine's own default is
            {props?.health?.rig_retries_default ?? 2}.
          </p>
        </details>

        <button
          class="btn-primary full"
          onclick={submit}
          disabled={submitting || engineDown || !file}
        >
          {submitting ? 'Submitting…' : 'Rig this character'}
        </button>
        <p class="hint">
          <QueueChip kind="pixal3d_rig" bind:options={rigQueue} response={rigAccepted} />
        </p>
        <p class="hint">
          Runs server-side — safe to reload or close this page. {expectation(
            'upload',
            wantTiers.length
          )}.
        </p>
      </section>

      <section class="card">
        <h3>Characters</h3>
        {#if !characters.length}
          <p class="hint">Nothing rigged yet.</p>
        {/if}
        <ul class="char-list">
          {#each characters as c (c.id)}
            <li>
              <button
                class="char"
                class:on={c.id === selectedId}
                onclick={() => {
                  selectedId = c.id;
                  view = { kind: 'tier', tier: c.tiers[0]?.tier ?? 'hero' };
                }}
              >
                {#if sourceThumb(c)}
                  <img src={sourceThumb(c)} alt="" />
                {:else}
                  <span class="thumb-blank"></span>
                {/if}
                <span class="char-body">
                  <span class="char-name">{c.name}</span>
                  <span class="char-meta">
                    {#if c.state === 'done'}
                      {c.tiers.length} tier{c.tiers.length === 1 ? '' : 's'} ·
                      {c.tiers[0]?.named_core ?? 0}/22 named
                    {:else if c.state === 'failed'}
                      <span class="down">failed</span>
                    {:else if c.state === 'cancelled'}
                      cancelled
                    {:else}
                      <span class="live">{c.state}</span> · {elapsed(c)} s
                    {/if}
                  </span>
                  {#if c.parent_id}
                    <span class="char-lineage">re-roll · {c.origin}</span>
                  {/if}
                </span>
              </button>
            </li>
          {/each}
        </ul>
      </section>
    </aside>

    <!-- ── the selected character ───────────────────────────────────── -->
    <main class="detail">
      {#if !selected}
        <section class="card">
          <p class="hint">Pick a character, or rig a new one.</p>
        </section>
      {:else}
        <!-- 🔴 Bound once, so the narrowing survives into every event handler below. `c` is a
             const: TypeScript keeps a `let` narrowed only until a closure boundary, and every
             button in this branch is one. -->
        {@const c = selected}
        <section class="card">
          <div class="detail-head">
            <input
              class="name-input"
              type="text"
              value={c.name}
              onchange={(e) => rename(c.id, (e.currentTarget as HTMLInputElement).value)}
            />
            <div class="head-actions">
              {#if !['done', 'failed', 'cancelled'].includes(c.state)}
                <button class="btn-sec small" onclick={() => cancel(c.id, 'rig')}>Cancel</button>
              {:else}
                {#if c.tiers.length}
                  <button class="btn-sec small" onclick={() => download(c.id)}>
                    Download ZIP
                  </button>
                {/if}
                <button class="btn-sec small danger" onclick={() => removeCharacter(c.id)}>
                  Delete
                </button>
              {/if}
            </div>
          </div>

          {#if !['done', 'failed', 'cancelled'].includes(c.state)}
            <!-- Indeterminate, never a fill: the engine publishes NO progress on a job record. The
                 only honest thing to show is elapsed against a measured expectation. -->
            <div class="bar indeterminate"></div>
            <p class="ptext">
              <strong>{c.state}</strong>
              · {elapsed(c)} s elapsed · {expectation(c.origin, c.tiers_requested.length)}
              {#if c.state === 'collecting'}
                — the card is free; koblem is pulling the models across.
              {/if}
            </p>
            <p class="hint">
              No stage-by-stage progress exists: the engine reports queued, running and completed,
              and nothing in between. Cancelling lands in about 7 seconds and leaves no partial
              model.
            </p>
          {/if}

          {#if c.error}<div class="error">{c.error}</div>{/if}
          {#if c.warnings.length}
            <ul class="warnings">
              {#each c.warnings as w, i (i)}
                <li>{w}</li>
              {/each}
            </ul>
          {/if}

          {#if c.state === 'done'}
            <p class="stat">
              {fmtSeconds(c.seconds)} on the card
              {#if c.peak_vram_mib}· peak {c.peak_vram_mib} MiB{/if}
              {#if c.rig_seed !== null}· rig_seed {c.rig_seed}{/if}
              {#if c.geo_seed !== null}· geo_seed {c.geo_seed}{/if}
            </p>
          {/if}
        </section>

        {#if c.tiers.length}
          <!-- ── viewer ───────────────────────────────────────────────── -->
          <section class="card">
            <div class="view-picker">
              <span class="pick-label">Model</span>
              {#each c.tiers as tier (tier.tier)}
                <button
                  class="chip"
                  class:on={view.kind === 'tier' && view.tier === tier.tier}
                  onclick={() => (view = { kind: 'tier', tier: tier.tier })}
                >
                  {tier.tier}
                </button>
              {/each}
              {#each c.edits as edit (edit.id)}
                <button
                  class="chip edit"
                  class:on={view.kind === 'edit' && view.id === edit.id}
                  onclick={() => (view = { kind: 'edit', id: edit.id })}
                >
                  ✎ {edit.label}
                </button>
              {/each}
            </div>

            <!-- 🔴 The workbench decides RIG vs TAKE itself, from what actually loaded rather than
                 from which chip was clicked. A saved take-repair is reached through the edits row
                 but still carries animation tracks, and offering joint editing on it would re-bind
                 a posed skeleton — the one unrecoverable mistake in this panel. `baseTier` is ''
                 for anything that is not an editable rig, which closes every editing mode. -->
            <RigWorkbench
              src={viewPath}
              label={viewLabel}
              characterId={c.id}
              baseTier={editableTier}
              tiers={c.tiers.map((t) => t.tier)}
              stagingReady={props?.staging_ready ?? false}
              onSave={saveRepair}
              onSaved={onEditSaved}
              onReloadRequested={() => reloadNonce++}
              onError={(m) => (error = m)}
            />

            {#each c.tiers.filter((t) => view.kind === 'tier' && t.tier === view.tier) as tier (tier.tier)}
              {@const verdict = gateWord(tier)}
              <div class="verdict">
                <span class="stat">
                  {tier.joints} joints · <b>{tier.named_core}/22</b> core names ·
                  {fmtBytes(tier.bytes)} · {fmtSeconds(tier.seconds)}
                </span>
                <span class="badge {verdict.tone}">{verdict.text}</span>
              </div>
              {#if tier.gate.draws > 0}
                <p class="hint">
                  {tier.gate.summary}
                  {#if tier.gate.pose_gate_ran}
                    · deformation gate {tier.gate.pose_gate_pass ? 'passed' : 'FAILED'} at
                    {tier.gate.pose_gate_worst.toFixed(2)} (limit 6.0)
                  {/if}
                </p>
                {#if !tier.gate.accepted}
                  <p class="hint">
                    The verdict is <strong>advisory</strong>. No draw cleared the acceptance
                    predicate so the best-ranked one shipped — a usable model, not a broken one.
                    Look at it before re-rolling; a rig failing the pose gate at 25.9 was judged
                    decent by eye.
                  </p>
                {/if}
              {:else}
                <p class="hint">
                  This tier inherited the shared skeleton and decoded nothing, so it ran no gate of
                  its own. Its joint counts are the hero's.
                </p>
              {/if}
              {#if tier.skin_cleanup}
                <p class="hint">Automatic skin cleanup: {tier.skin_cleanup}</p>
              {/if}
            {/each}

            <!-- ── re-rolls ──────────────────────────────────────────── -->
            {#if c.state === 'done'}
              <details class="advanced">
                <summary>Not right? Try again</summary>
                <p class="hint">
                  🔴 Each of these moves a <strong>seed</strong>. Re-running the same request
                  returns the identical draw after the same wait, which is why there is no plain
                  "retry".
                </p>
                <div class="reroll">
                  <button class="btn-sec small" onclick={() => reroll('rig_seed')}>
                    New skeleton draw (~205 s)
                  </button>
                  <span class="hint"
                    >Same body, re-decoded from the cached geometry. The usual fix for a bad
                    skeleton.</span
                  >
                </div>
                <div class="reroll">
                  <button class="btn-sec small" onclick={() => reroll('geo_seed')}>
                    New mesh (~450 s)
                  </button>
                  <span class="hint"
                    >Redraws the body itself — a genuinely different character. Use it when the
                    geometry is the problem (fused hands, a lost limb), not the rig.</span
                  >
                </div>
                {#if (props?.tiers ?? []).some((t) => !c.tiers.some((x) => x.tier === t))}
                  <div class="reroll">
                    <button
                      class="btn-sec small"
                      onclick={() =>
                        bakeTiers(
                          (props?.tiers ?? []).filter((t) => !c.tiers.some((x) => x.tier === t))
                        )}
                    >
                      Bake the missing tiers (~134 s each)
                    </button>
                    <span class="hint"
                      >Onto the same skeleton, so every take you have already made still plays.</span
                    >
                  </div>
                {/if}
              </details>
            {/if}
          </section>

          <!-- ── motion + takes ─────────────────────────────────────── -->
          <section class="card">
            <h3>Animate</h3>
            <p class="hint">
              A clip is ~{props?.timings.motion_warm_seconds ?? 11} s and a retarget is
              {props?.timings.animate_seconds ?? 0.22} s, so this is the loop: prompt, look, prompt again.
              Clips are shared across every character — the retarget matches joints by name.
            </p>

            <div class="prompt-row">
              <input
                type="text"
                bind:value={prompt}
                placeholder="a person walks forward confidently, arms swinging"
                onkeydown={(e) => {
                  if (e.key === 'Enter') void generateMotion();
                }}
              />
              <input
                class="secs"
                type="number"
                min="0.5"
                max="60"
                step="0.5"
                bind:value={motionSeconds}
              />
              <button
                class="btn-sec"
                onclick={generateMotion}
                disabled={motionBusy || engineDown || !prompt.trim()}
              >
                {motionBusy ? 'Generating…' : 'Generate clip'}
              </button>
              <QueueChip
                kind="pixal3d_motion"
                bind:options={motionQueue}
                response={motionAccepted}
              />
            </div>
            <p class="hint">
              Nothing scores whether a motion is right — there is no quality signal for it at all.
              Keep the takes you like and compare them; the same prompt with a new seed is a
              genuinely different one.
            </p>

            {#if motions.length}
              <ul class="motions">
                {#each motions as m (m.id)}
                  <li class:picked={pickedMotions.includes(m.id)}>
                    <label class="toggle">
                      <input
                        type="checkbox"
                        checked={pickedMotions.includes(m.id)}
                        disabled={m.state !== 'done'}
                        onchange={() => togglePicked(m.id)}
                      />
                      <span class="mprompt">{m.prompt}</span>
                    </label>
                    <span class="mmeta">
                      {#if m.state === 'done'}
                        {m.frames} f @ {m.fps.toFixed(0)} fps
                        {#if m.seed !== null}· seed {m.seed}{/if}
                        {#if !m.dit_was_resident}· cold load{/if}
                      {:else if m.state === 'failed'}
                        <span class="down">{m.error ?? 'failed'}</span>
                      {:else}
                        <span class="live">{m.state}…</span>
                      {/if}
                    </span>
                    {#if ['queued', 'running'].includes(m.state)}
                      <button class="link" onclick={() => cancel(m.id, 'motion')}>cancel</button>
                    {:else}
                      {#if m.state === 'done'}
                        <button
                          class="link"
                          disabled={motionBusy || engineDown}
                          onclick={() => rerollMotion(m)}>re-roll</button
                        >
                      {/if}
                      <button class="link" onclick={() => removeMotion(m.id)}>delete</button>
                    {/if}
                  </li>
                {/each}
              </ul>
            {/if}

            <div class="knobs">
              <label>
                Onto
                <select
                  value={animateEdit ? `edit:${animateEdit}` : `tier:${animateTier}`}
                  onchange={(e) => {
                    const v = (e.currentTarget as HTMLSelectElement).value;
                    if (v.startsWith('edit:')) {
                      animateEdit = v.slice(5);
                    } else {
                      animateEdit = '';
                      animateTier = v.slice(5);
                    }
                  }}
                >
                  {#each c.tiers as tier (tier.tier)}
                    <option value={`tier:${tier.tier}`}>{tier.tier} tier</option>
                  {/each}
                  <!-- 🔴 A corrected TAKE is excluded, not just disabled: it already carries baked
                       tracks, so retargeting onto it would layer a second clip over the first. -->
                  {#each c.edits.filter((e) => e.base_tier !== 'take') as edit (edit.id)}
                    <option value={`edit:${edit.id}`} disabled={!edit.staged_path}>
                      {edit.label}{edit.staged_path ? '' : ' (not animatable)'}
                    </option>
                  {/each}
                </select>
              </label>
              <label>
                Retarget variant
                <select bind:value={variant}>
                  {#each props?.variants ?? ['rotbase'] as v (v)}
                    <option value={v}>{v}</option>
                  {/each}
                </select>
              </label>
              <label class="wide">
                Name this take
                <input type="text" bind:value={takeLabel} placeholder="from the first prompt" />
              </label>
            </div>
            <label class="toggle">
              <input type="checkbox" bind:checked={exercise} /> Include the skeleton-exercise track
            </label>
            <p class="hint">
              The exercise track swings every materially weighted joint in turn. It is the honest
              test of whether the rig deforms at all — worth keeping on until you trust a character.
            </p>
            <button
              class="btn-primary"
              onclick={bakeTake}
              disabled={engineDown || !pickedMotions.length}
            >
              Bake take ({pickedMotions.length} clip{pickedMotions.length === 1 ? '' : 's'})
            </button>

            {#if c.animations.length}
              <h4>Takes</h4>
              <ul class="takes">
                {#each c.animations as take (take.id)}
                  <li class:on={view.kind === 'animation' && view.id === take.id}>
                    <button
                      class="take"
                      disabled={take.state !== 'done'}
                      onclick={() => (view = { kind: 'animation', id: take.id })}
                    >
                      <span class="take-label">{take.label}</span>
                      <span class="take-meta">
                        {#if take.state === 'done'}
                          {take.animations} track{take.animations === 1 ? '' : 's'} ·
                          <b>{take.mapped}</b>
                          joints mapped
                          {#if take.map_kind}by {take.map_kind}{/if}
                        {:else if take.state === 'failed'}
                          <span class="down">{take.error ?? 'failed'}</span>
                        {:else}
                          <span class="live">{take.state}…</span>
                        {/if}
                      </span>
                    </button>
                    <button class="link" onclick={() => removeAnimation(take.id)}>delete</button>
                  </li>
                {/each}
              </ul>
              <p class="hint">
                A low <strong>joints mapped</strong> on a humanoid is the visible consequence of a low
                core-name count — the clip drives only the joints it can find by name.
              </p>
            {/if}
          </section>

          {#if c.edits.length}
            <section class="card">
              <h3>Repairs</h3>
              <ul class="edits">
                {#each c.edits as edit (edit.id)}
                  <li>
                    <span class="edit-label">{edit.label}</span>
                    <span class="stat">
                      from {edit.base_tier} · {fmtBytes(edit.bytes)}
                      {#if edit.changes?.pruned_influences}
                        · pruned {edit.changes.pruned_influences} influences
                      {/if}
                      {#if edit.changes?.renamed_joints?.length}
                        · renamed {edit.changes.renamed_joints.length}
                      {/if}
                      {#if !edit.staged_path}· <span class="warn-inline">not animatable</span>{/if}
                    </span>
                    <button class="link" onclick={() => removeEdit(edit.id)}>delete</button>
                  </li>
                {/each}
              </ul>
              <p class="hint">
                Deleting a repair also removes the takes retargeted onto it — their rig would no
                longer exist.
              </p>
            </section>
          {/if}
        {/if}
      {/if}
    </main>
  </div>
</div>

<style>
  .panel-status {
    margin: 0 0 16px;
  }
  /* The library rail is a fixed reading width; everything the rail does not need goes to the
     workbench, because the canvas is the tool now and the page no longer caps at 1600px. The rail
     is allowed to shrink to 240px before the layout collapses, since on a laptop the difference
     between a 240px and a 300px thumbnail list is nothing and the difference to the model is real. */
  .layout {
    display: grid;
    grid-template-columns: minmax(240px, 300px) minmax(0, 1fr);
    gap: 16px;
    align-items: start;
  }
  @media (max-width: 900px) {
    .layout {
      grid-template-columns: 1fr;
    }
  }
  .card {
    padding: 20px;
    margin-bottom: 16px;
  }
  .card h3 {
    margin: 0 0 12px;
    font-size: 15px;
  }
  .card h4 {
    margin: 16px 0 8px;
    font-size: 13px;
  }
  .hint {
    color: var(--muted);
    font-size: 11px;
    line-height: 1.45;
    margin: 8px 0 0;
  }
  .stat {
    color: var(--muted);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }
  .notice {
    margin: 0 0 12px;
    padding: 8px 12px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--muted);
    font-size: 12px;
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
  }
  .live {
    color: var(--accent);
  }
  .warn-inline {
    color: var(--warning, #d9952d);
  }
  .warnings {
    margin: 8px 0 0;
    padding-left: 18px;
    font-size: 12px;
    color: var(--warning, #d9952d);
  }

  input[type='text'],
  input[type='number'],
  select {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    color: var(--ink);
    font-family: inherit;
    font-size: 13px;
  }
  .field {
    display: block;
    font-size: 12px;
    color: var(--muted);
    margin-top: 10px;
  }
  .dz {
    margin-top: 6px;
  }
  .chosen {
    margin: 8px 0 0;
    font-size: 12px;
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  .tier-picks {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    margin-top: 6px;
  }
  .toggle {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    color: var(--ink);
    font-size: 13px;
    margin-top: 10px;
  }
  .toggle input {
    width: auto;
  }
  .advanced {
    margin-top: 14px;
    padding-top: 12px;
    border-top: 1px solid var(--ring);
  }
  .advanced summary {
    cursor: pointer;
    color: var(--ink);
    font-size: 13px;
    font-weight: 600;
  }
  .btn-primary.full {
    width: 100%;
    margin-top: 14px;
  }

  /* library */
  .char-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
    max-height: 460px;
    overflow-y: auto;
  }
  .char {
    display: flex;
    gap: 10px;
    align-items: center;
    width: 100%;
    padding: 6px;
    background: none;
    border: 1px solid transparent;
    border-radius: 8px;
    cursor: pointer;
    text-align: left;
    color: var(--ink);
  }
  .char.on {
    border-color: var(--accent);
    background: var(--bg-2);
  }
  .char img,
  .thumb-blank {
    width: 44px;
    height: 44px;
    object-fit: cover;
    border-radius: 6px;
    background: var(--bg-2);
    flex: none;
  }
  .char-body {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .char-name {
    font-size: 13px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .char-meta,
  .char-lineage {
    font-size: 11px;
    color: var(--muted);
  }

  /* detail */
  .detail-head {
    display: flex;
    gap: 12px;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
  }
  .name-input {
    flex: 1 1 240px;
    font-size: 15px;
    font-weight: 600;
  }
  .head-actions {
    display: flex;
    gap: 8px;
  }
  .btn-sec.small {
    padding: 4px 10px;
    font-size: 12px;
  }
  .btn-sec.danger {
    color: var(--danger);
  }
  .bar {
    height: 6px;
    border-radius: 3px;
    background: var(--bg-2);
    overflow: hidden;
    margin-top: 12px;
  }
  .bar.indeterminate {
    background: linear-gradient(90deg, var(--bg-2) 0%, var(--accent) 50%, var(--bg-2) 100%);
    background-size: 200% 100%;
    animation: slide 1.2s linear infinite;
  }
  @keyframes slide {
    from {
      background-position: 200% 0;
    }
    to {
      background-position: -200% 0;
    }
  }
  .ptext {
    color: var(--muted);
    font-size: 12px;
    margin: 8px 0 0;
  }

  .view-picker {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    align-items: center;
    margin-bottom: 12px;
  }
  .pick-label {
    font-size: 12px;
    color: var(--muted);
    margin-right: 4px;
  }
  .chip {
    padding: 4px 10px;
    border: 1px solid var(--ring);
    border-radius: 999px;
    background: none;
    color: var(--muted);
    font-size: 12px;
    cursor: pointer;
    font-family: inherit;
  }
  .chip.on {
    border-color: var(--accent);
    color: var(--ink);
    background: var(--bg-2);
  }
  .verdict {
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  .badge {
    padding: 2px 8px;
    border-radius: 999px;
    font-size: 11px;
    border: 1px solid var(--ring);
  }
  .badge.ok {
    color: var(--ok, #4ade80);
    border-color: var(--ok, #4ade80);
  }
  .badge.warn {
    color: var(--warning, #d9952d);
    border-color: var(--warning, #d9952d);
  }
  .badge.muted {
    color: var(--muted);
  }
  .reroll {
    display: flex;
    gap: 10px;
    align-items: baseline;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  .reroll .hint {
    flex: 1 1 240px;
    margin: 0;
  }

  /* motion + takes */
  .prompt-row {
    display: flex;
    gap: 8px;
    margin-top: 12px;
    flex-wrap: wrap;
  }
  .prompt-row input[type='text'] {
    flex: 1 1 260px;
    width: auto;
  }
  .prompt-row .secs {
    width: 80px;
  }
  .motions,
  .takes,
  .edits {
    list-style: none;
    margin: 12px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-height: 260px;
    overflow-y: auto;
  }
  .motions li,
  .edits li {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 12px;
    padding: 4px 6px;
    border-radius: 6px;
  }
  .motions li.picked {
    background: var(--bg-2);
  }
  .motions .toggle {
    margin: 0;
    flex: 1 1 auto;
    min-width: 0;
  }
  .mprompt,
  .edit-label,
  .take-label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .mmeta {
    color: var(--muted);
    font-size: 11px;
    white-space: nowrap;
  }
  .takes li {
    display: flex;
    align-items: center;
    gap: 8px;
    border-radius: 6px;
  }
  .takes li.on {
    background: var(--bg-2);
  }
  .take {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 2px;
    align-items: flex-start;
    background: none;
    border: none;
    color: var(--ink);
    font-family: inherit;
    font-size: 12px;
    cursor: pointer;
    padding: 6px;
    text-align: left;
    min-width: 0;
  }
  .take:disabled {
    cursor: default;
    opacity: 0.7;
  }
  .take-meta {
    color: var(--muted);
    font-size: 11px;
  }
  .link {
    background: none;
    border: none;
    color: var(--muted);
    font-size: 11px;
    cursor: pointer;
    font-family: inherit;
    text-decoration: underline;
    padding: 2px 4px;
  }
  .knobs {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
    gap: 8px 16px;
    margin-top: 14px;
  }
  .knobs label {
    display: block;
    font-size: 12px;
    color: var(--muted);
  }
  .knobs label.wide {
    grid-column: 1 / -1;
  }
</style>
