<script lang="ts">
  /*
   * The 3D page. Two engines that solve the same problem and share no implementation, so they are
   * two tabs rather than one merged form:
   *
   *   SPAR3D   one synchronous call, tens of seconds, a textured rigged GLB. Props and game assets.
   *   Pixal3D  longcat-rig: a ~450 s character pipeline, an LOD ladder on one skeleton, and a ~12 s
   *            prompt→animation loop on the far side.
   *
   * ⚠️ They are also separate GPU budget lines and separate settings. SPAR3D reserves 2.6 GB;
   * Pixal3D refuses to start below 11 GB free and is pinned to the 3060. One being down says
   * nothing about the other, which is why each tab reports its own engine health.
   *
   * `?tab=pixal3d` deep-links the second tab.
   */
  import { onMount } from 'svelte';
  import Spar3dPanel from '#lib/components/three_d/Spar3dPanel.svelte';
  import Pixal3dPanel from '#lib/components/three_d/Pixal3dPanel.svelte';

  type Tab = 'spar3d' | 'pixal3d';

  let tab = $state<Tab>('pixal3d');
  /** Mounted lazily and then kept mounted: both panels hold real state (a WebGL context and a
   *  poll loop here, an in-flight upload there) that must survive a tab switch. */
  let visited = $state<Record<Tab, boolean>>({ spar3d: false, pixal3d: true });

  onMount(() => {
    const requested = new URLSearchParams(window.location.search).get('tab');
    if (requested === 'spar3d' || requested === 'pixal3d') select(requested);
  });

  function select(next: Tab) {
    tab = next;
    visited = { ...visited, [next]: true };
  }
</script>

<div class="page-container page-wide">
  <div class="section-header">
    <h2>Image to 3D</h2>
    <p class="muted">Two engines, two very different shapes of job</p>
  </div>

  <div class="tabs">
    <button class="tab" class:on={tab === 'pixal3d'} onclick={() => select('pixal3d')}>
      Pixal3D
    </button>
    <button class="tab" class:on={tab === 'spar3d'} onclick={() => select('spar3d')}>SPAR3D</button>
  </div>

  <!-- `hidden` rather than an {#if}: destroying a panel would drop a WebGL context and a poll loop
       every time the user looks at the other tab. -->
  {#if visited.pixal3d}
    <div hidden={tab !== 'pixal3d'}><Pixal3dPanel /></div>
  {/if}
  {#if visited.spar3d}
    <div hidden={tab !== 'spar3d'}><Spar3dPanel /></div>
  {/if}
</div>

<style>
  /* 🔴 `.page-container` lives in koblibs and is shared by every page in the app: it caps at
     1600px and pads up to 80px a side. Editing it there would re-lay-out the whole product, so
     widen it here only. Both tabs are 3D viewports where the horizontal pixels ARE the feature.
     Vertical padding is left alone — only the max-width and the side gutters give way. */
  .page-wide {
    max-width: none;
    padding-left: clamp(12px, 2vw, 28px);
    padding-right: clamp(12px, 2vw, 28px);
  }
  .tabs {
    display: flex;
    gap: 4px;
    margin-bottom: 16px;
    border-bottom: 1px solid var(--ring);
  }
  .tab {
    padding: 8px 14px;
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--muted);
    font-family: inherit;
    font-size: 13px;
    cursor: pointer;
  }
  .tab.on {
    color: var(--ink);
    border-bottom-color: var(--accent);
  }
</style>
