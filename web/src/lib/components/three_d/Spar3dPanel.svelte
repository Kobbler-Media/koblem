<script lang="ts">
  /*
   * 3D → SPAR3D. The original image-to-rig path: one synchronous call, tens of seconds, a textured
   * and rigged GLB out.
   *
   * ⚠️ It overlaps Pixal3D in purpose and shares NOTHING with it in implementation — different
   * service, different budget line, different appetite (2.6 GB against a rig that refuses to start
   * below 11). Neither replaces the other: this one is fast, synchronous and aimed at props and
   * game assets; Pixal3D is a ~450 s character pipeline with an animation loop on the far side.
   *
   * The viewer is `RigWorkbench`, the same editor Pixal3D uses. The two engines are unrelated but
   * their output has the SAME class of defect — stray shells, skin-weight leaks past a joint,
   * anonymous joints no retarget will drive, a texture that is whatever the source photo was — so
   * the repairs are the same repairs, and this panel used to have none of them.
   *
   * What differs is where the repaired bytes go. `POST /api/v1/3d/generate` returns URLs and
   * nothing else: there is no edit endpoint and no asset record to store one against. So no
   * `onSave` handler is passed and the workbench downloads the patched GLB itself, which is the
   * honest answer for a host with nowhere to put it.
   *
   * 🔴 NOTHING HERE MAY MOVE THE BONES. This panel used to run its own three.js loop that rotated
   * every third bone by `sin(t)` as a liveliness demo. The GLB carries no animation clips, and
   * every rest-pose guard in the workbench keys off `animationClips.length` — so a wiggling model
   * reads to all of them as a model AT REST. Joint editing re-binds the skeleton at the CURRENT
   * pose, so a repair saved from a wiggling model would freeze that wiggle into the bind
   * permanently, with nothing anywhere to detect it. `ModelStage` does not animate a clip-less rig.
   * Do not reintroduce a preview animation of any kind.
   *
   * ⚠️ The old viewer also centred the model with `root.position.sub(centre)`. `ModelStage`
   * deliberately never moves the model — a framing offset would be baked into every saved repair
   * and would break the re-bind maths — and aims the camera instead. Do not add the centring back.
   */
  import { onMount } from 'svelte';
  import FileDropzone from '#lib/components/FileDropzone.svelte';
  import RigWorkbench from './RigWorkbench.svelte';
  import {
    imageToRig,
    imageToRigProps,
    mediaUrl,
    type ImageToRigProps,
    type RigTextureMode
  } from '#lib/api/client.ts';

  let files = $state<FileList | null>(null);
  let file = $derived(files?.length ? files[0] : null);
  let props = $state<ImageToRigProps | null>(null);
  let running = $state(false);
  let error = $state('');
  /**
   * ⚠️ Two forms of the same file, and they are NOT interchangeable. `resultPath` is the bare
   * koblem media path; the workbench hands it to `fetchMediaBlob`, which authenticates with a
   * header. `resultUrl` is the same path signed into a query-string URL, because an `<a download>`
   * cannot send a header. Give the signed URL to the workbench and you get a viewer that silently
   * never loads.
   */
  let resultPath = $state('');
  let resultUrl = $state('');
  let bundleUrl = $state('');
  let lodUrls = $state<string[]>([]);
  /** The source image's name, used as the viewer caption and as the stem of a repair's filename. */
  let assetLabel = $state('');
  /** Bumped by the workbench's "Discard and reload" — the only honest undo for an in-place browser
   *  edit, since the deletions it makes are to the loaded three.js geometry. */
  let reloadNonce = $state(0);
  /** How many repaired copies this session has written to the browser's downloads, so the links
   *  below can say plainly that they are not one of them. */
  let repairsSaved = $state(0);
  let removeBackground = $state(true);
  // Whole-GLB LODs are directly usable by Bevy's normal glTF loader. Keep
  // them on for a game bundle; callers can opt out when latency matters.
  let generateLods = $state(true);
  // Adaptive source-camera projection preserves the visible character artwork
  // while depth-gating it against the reconstructed mesh.
  let textureMode = $state<RigTextureMode>('project_front');
  let sourceTextureResolution = $state<512 | 1024>(1024);
  let useSeed = $state(false);
  let seed = $state(777);

  /** The nonce rides in the query so a re-fetch is a different URL to the browser cache without
   *  naming a different asset. The media route is a path route, so a spare parameter is ignored. */
  const viewerSrc = $derived(
    resultPath && reloadNonce
      ? `${resultPath}${resultPath.includes('?') ? '&' : '?'}v=${reloadNonce}`
      : resultPath
  );

  onMount(() => {
    imageToRigProps()
      .then((p) => (props = p))
      .catch(() => {
        props = {
          configured: false,
          healthy: false,
          supports_project_front: false,
          supports_two_sided_mirror: false,
          source_texture_resolutions: []
        };
      });
  });

  async function submit() {
    if (!file) {
      error = 'Add one character or prop image first.';
      return;
    }
    const sourceName = file.name;
    running = true;
    error = '';
    // 🔴 Cleared before the call, which unmounts the workbench and takes its whole editing session
    // with it. A second generation must not arrive under a repair measured against the first one.
    resultPath = '';
    resultUrl = '';
    bundleUrl = '';
    lodUrls = [];
    reloadNonce = 0;
    repairsSaved = 0;
    try {
      const result = await imageToRig(file, {
        remove_background: removeBackground,
        texture_mode: textureMode,
        source_texture_resolution: sourceTextureResolution,
        generate_lods: generateLods,
        seed: useSeed ? seed : undefined
      });
      assetLabel = sourceName.replace(/\.[^.]+$/, '') || 'asset';
      resultPath = result.url;
      resultUrl = mediaUrl(result.url);
      bundleUrl = mediaUrl(result.bundle_url);
      lodUrls = result.lod_urls.map(mediaUrl);
    } catch (e) {
      error = e instanceof Error ? e.message : '3D generation failed';
    } finally {
      running = false;
    }
  }
</script>

<div class="spar3d">
  <p class="muted panel-status">
    SPAR3D + RigAnything · a self-contained textured, rigged GLB for Paperworld or another glTF
    runtime ·
    {#if props}
      {#if props.healthy}<span class="ok">engine up</span>{:else}<span class="down"
          >engine offline</span
        >{/if}
    {/if}
  </p>

  {#if props && (!props.configured || !props.healthy)}
    <div class="error">
      SPAR3D + RigAnything is not available. Configure <code>spar3d_service_url</code> in Settings, then
      test it there.
    </div>
  {/if}
  {#if error}<div class="error">{error}</div>{/if}

  <!-- Three short config cards. Stacked full-width they were three wide slivers above a small
       model; as an auto-fit grid they sit side by side on a desktop and fall back to one column
       under ~320px of track without a media query. -->
  <div class="config-grid">
    <section class="card">
      <h3>Source image</h3>
      <div class="dz">
        <FileDropzone
          accept="image/*"
          label={file ? 'Replace image' : 'Add image'}
          hint="PNG, JPEG or WebP — a clear, shaded 3D-looking subject works best"
          onFiles={(v) => (files = v)}
        />
      </div>
      {#if file}
        <p class="chosen">{file.name} · {(file.size / 1024 / 1024).toFixed(1)} MB</p>
      {/if}
      <div class="toggles">
        <label class="toggle">
          <input type="checkbox" bind:checked={removeBackground} /> Remove background with RMBG-2.0
        </label>
      </div>
      <p class="hint">Recommended — the matte is what keeps the background out of the mesh.</p>
    </section>

    <section class="card">
      <h3>Texture</h3>
      <div class="knobs">
        <label class="wide"
          >Mode
          <select bind:value={textureMode}>
            <option value="project_front"
              >Project source onto front — recommended for characters</option
            >
            <option value="generated">Generated material only</option>
            <option value="two_sided_mirror"
              >Two-sided mirror — explicitly symmetric props only</option
            >
          </select></label
        >
        <label
          >Source detail
          <select bind:value={sourceTextureResolution}>
            <option value={1024}>1024 px (recommended)</option>
            <option value={512}>512 px</option>
          </select></label
        >
        {#if useSeed}
          <label>Seed <input type="number" min="0" step="1" bind:value={seed} /></label>
        {/if}
      </div>
      {#if textureMode === 'project_front'}
        <p class="hint">
          Adaptive depth-safe projection keeps the source image sharp on the visible front. Small
          gaps or a little conservative overdraw near occlusion boundaries are expected.
        </p>
      {/if}
      {#if textureMode === 'two_sided_mirror'}
        <p class="warn">
          Use only for intentionally symmetric, side-on props. It mirrors text, faces, and
          asymmetric markings.
        </p>
      {/if}
      <div class="toggles">
        <label class="toggle"><input type="checkbox" bind:checked={useSeed} /> Fixed seed</label>
      </div>
    </section>

    <section class="card">
      <h3>Game bundle</h3>
      <div class="toggles">
        <label class="toggle">
          <input type="checkbox" bind:checked={generateLods} /> Generate LOD1 + LOD2 sidecar GLBs
        </label>
      </div>
      <p class="hint">
        Recommended for Paperworld; adds finishing time. The game selects a whole GLB by distance —
        no MeshOpt, Draco, mesh quantization, or glTF LOD extension is required.
      </p>
    </section>
  </div>

  <div class="actions">
    <button class="btn-primary" disabled={running || !props?.healthy} onclick={submit}>
      {running ? 'Building rig…' : 'Generate GLB'}
    </button>
    <span class="muted">
      {running
        ? 'This normally takes tens of seconds; keep this page open until the GLB is saved.'
        : 'The rig is built server-side, but this page has to stay open to receive it.'}
    </span>
  </div>

  {#if resultPath}
    <section class="card result">
      <h3>Game asset ready</h3>
      <p class="hint">
        Drag to orbit; pick a mode in the canvas to repair the asset — delete a stray shell, prune a
        skin-weight leak, name an anonymous joint, grade the texture. The blue skeleton is what
        RigAnything detected. This GLB carries <strong>no animation clips</strong>: nothing moves
        until a runtime drives it, and every repair tool here depends on it staying at rest.
      </p>
      <!-- No `onSave`, so the workbench downloads the patched GLB itself — right for a service that
           stores nothing. `characterId`, `tiers` and `baseTier` all stay empty: SPAR3D has no tier
           ladder and nothing to file a repair against, so tier propagation never appears and the
           workbench's copy calls the delivered model "source" rather than naming a tier. -->
      <RigWorkbench
        src={viewerSrc}
        label={assetLabel}
        onSaved={() => repairsSaved++}
        onReloadRequested={() => reloadNonce++}
        onError={(m) => (error = m)}
      />

      <h4>Downloads — the asset exactly as delivered</h4>
      <div class="downloads">
        <a class="btn-primary" href={resultUrl} download="asset.glb">Download GLB</a>
        {#each lodUrls as lodUrl, index (lodUrl)}
          <a class="btn-sec" href={lodUrl} download={`asset.lod${index + 1}.glb`}
            >Download LOD{index + 1} GLB</a
          >
        {/each}
        <a class="btn-sec" href={bundleUrl} download="image-to-rig-asset.zip"
          >Download Paperworld asset bundle (ZIP)</a
        >
      </div>
      <p class="hint">
        The GLB includes mesh, texture, skin weights and skeleton; runtime does not need the source
        image. These links always serve what the engine produced — this service stores no edits, so
        a repair can never overwrite them.
      </p>
      <p class="hint">
        Saving a repair writes a <strong>separate</strong> GLB straight to your browser's downloads,
        named after the repair rather than <code>asset.glb</code>. ⚠️ The LODs and the bundle above
        stay unrepaired: they are separate meshes and nothing here verifies that an edit would land
        the same way on them.
        {#if repairsSaved}
          <strong
            >{repairsSaved} repaired {repairsSaved === 1 ? 'copy' : 'copies'} downloaded this session.</strong
          >
        {/if}
      </p>
    </section>
  {/if}
</div>

<style>
  .panel-status {
    margin: 0 0 16px;
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
    margin: 20px 0 8px;
    font-size: 13px;
    color: var(--muted);
  }
  .config-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
    gap: 16px;
    /* Texture grows when a seed field or a mode warning appears; without this the other two
       stretch to match and their hints float in dead space. */
    align-items: start;
    margin-bottom: 16px;
  }
  /* ⚠️ `.card` carries its own stacking margin for the single-column case. Inside the grid the
     gap owns the spacing, so leaving it would double the gutter on every wrapped row. */
  .config-grid .card {
    margin-bottom: 0;
  }
  input,
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
  label {
    display: block;
    font-size: 12px;
    color: var(--muted);
    margin-top: 10px;
  }
  .hint {
    color: var(--muted);
    font-weight: normal;
    font-size: 11px;
    margin: 8px 0 0;
    line-height: 1.45;
  }
  .warn {
    margin: 8px 0 0;
    color: var(--warning, #d9952d);
    font-size: 11px;
    line-height: 1.45;
  }
  .ok {
    color: var(--ok, #4ade80);
  }
  .down {
    color: var(--danger);
  }
  .dz {
    margin-top: 6px;
  }
  .chosen {
    margin: 8px 0 0;
    font-size: 13px;
    color: var(--ink);
    overflow-wrap: anywhere;
  }

  .knobs {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
    gap: 8px 16px;
    align-items: end;
  }
  .knobs label {
    margin-top: 0;
  }
  .knobs label.wide {
    grid-column: 1 / -1;
  }
  .toggles {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
    margin-top: 14px;
  }
  label.toggle {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    margin-top: 0;
    color: var(--ink);
    font-size: 13px;
  }
  label.toggle input {
    width: auto;
  }

  .actions {
    margin: 16px 0;
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }

  .downloads {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 12px;
  }
  .downloads a {
    text-decoration: none;
  }
</style>
