<script lang="ts">
  /*
   * The UV atlas, drawn — a READ-ONLY diagnostic.
   *
   * # Why this exists
   *
   * These characters wear a texture that is the source photo PROJECTED onto a reconstructed mesh
   * and packed into an automatic 2048² atlas. When something looks wrong on the model — a seam, a
   * smear, a patch of the wrong colour — every explanation lives in that atlas, and until this
   * panel there was no way to look at it. The user was reasoning about 4.19 million texels they
   * could not see.
   *
   * 🔴 IT CHANGES NOTHING. No pixel of the texture, no vertex of the mesh, no material property.
   * That is what makes it safe to ship next to a pipeline whose baker is already correct — see the
   * header of `texture-repair.ts` for why re-dilating or re-inpainting in the browser is actively
   * harmful. Looking is the one operation that cannot reintroduce the seam glitch.
   *
   * # The division of labour, which is the whole design
   *
   * - **The UV attribute answers the LAYOUT**, on the CPU: islands, bounds, per-triangle area,
   *   degenerate triangles, and what is under the cursor. `analyseUvLayout` and `buildUvHitGrid`
   *   do it in one pass each over a few hundred thousand triangles, with no GPU state touched, so
   *   hovering costs about four point-in-triangle tests.
   * - **Only a rasteriser answers TEXELS**, so `buildCoverageCountRect` runs exactly ONE render
   *   pass and one readback, and both of the questions that need it — how much of the atlas the
   *   mesh reaches, and whether two islands land on the same texel — come out of that single
   *   buffer. Overlap is not guessable from UV bounding boxes: automatic packers interlock boxes
   *   constantly and a box test would cry wolf on every good layout.
   *
   * # Cost discipline
   *
   * 🔴 Nothing per-texel ever runs on hover. The analysis is a staged sequence of `setTimeout(…, 16)`
   * steps with progress text — the same deferral the colour commit uses — and everything it
   * produces is CACHED into offscreen canvases. A hover or a pan then costs a handful of
   * `drawImage` calls, coalesced into one `requestAnimationFrame`.
   *
   * 🔴 The orientation rule, inherited from `texture-repair.ts`: `GLTFLoader` sets
   * `texture.flipY = false`, so image row 0 is `v = 0`. Drawing the decoded image at canvas y = 0
   * therefore puts `v = 0` at the TOP, and a UV plots at `(u * W, v * H)` with no flip. The
   * coverage buffer agrees without a flip for the reason set out there (two opposite conventions
   * cancel), which is why the mask can be written straight into an `ImageData` alongside the atlas.
   *
   * # What this component cannot do alone
   *
   * Connecting an island to a PLACE ON THE CHARACTER needs the 3D canvas, which this does not own.
   * The honest half is here: a hover resolves to a face index and a click resolves an island to
   * its face set, both emitted through `onHoverFace` / `onHighlightFaces`. The owner routes the
   * set to `ModelStage`'s `previewFaces` and the island lights up on the body; routing the stage's
   * own picked face back in through `highlightFace` closes the loop the other way.
   */
  import { onDestroy } from 'svelte';
  import * as THREE from 'three';

  import {
    analyseUvLayout,
    buildCoverageCountRect,
    buildUvHitGrid,
    countSubTexelFaces,
    islandFaces,
    summariseCoverage,
    textureToImageData,
    uvHitFace,
    uvIslandOutline,
    type CoverageSummary,
    type UvHitGrid,
    type UvLayout,
    type UvOutline
  } from './texture-repair';

  let {
    renderer = null,
    mesh = null,
    texture = null,
    highlightFace = null,
    onHighlightFaces,
    onHoverFace,
    onFocusFace
  }: {
    /**
     * The live renderer, i.e. `stage.renderer`.
     *
     * 🔴 Required for the coverage pass and for nothing else. Without it the panel still draws the
     * atlas, the islands and every CPU finding; it simply cannot report covered or overlapping
     * texels, because those are facts about the fill rule and only a rasteriser knows them.
     */
    renderer?: THREE.WebGLRenderer | null;
    /** The body, i.e. `stage.skinnedMeshes[0]`. A `SkinnedMesh` is fine: only `uv` is read. */
    mesh?: THREE.Mesh | null;
    /**
     * The map to draw under the layout — the baseColor map (`material.map`) in practice.
     *
     * ⚠️ Its size sets the coverage resolution, so pass the map whose texels you are asking about.
     * A normal map packed to different dimensions would be measured against its own grid.
     */
    texture?: THREE.Texture | null;
    /**
     * A face the 3D view is pointing at, so the panel can show where it lands in the atlas. `-1`
     * or `null` for none. This is the model → UV direction; the owner feeds it from whatever the
     * stage reports (`onFacePicked`, or the hovered piece's representative face).
     */
    highlightFace?: number | null;
    /**
     * Faces the panel wants drawn ON THE MODEL — an island the user selected, or the degenerate
     * triangles. `null` clears it.
     *
     * 🔴 Route this straight to `ModelStage`'s `previewFaces`, which already draws an arbitrary
     * face set in every mode. The set is compared BY IDENTITY there, and a fresh set is emitted
     * each time, so no in-place mutation is involved.
     */
    onHighlightFaces?: (faces: ReadonlySet<number> | null) => void;
    /** Face under the cursor in the atlas, `-1` for none. Fired only when the value CHANGES. */
    onHoverFace?: (faceIndex: number) => void;
    /**
     * A double-click on the atlas, asking for the camera to be taken to that face. Supply it and
     * the panel offers the gesture; omit it and it does not advertise something that will not
     * happen.
     */
    onFocusFace?: (faceIndex: number) => void;
  } = $props();

  /** Display size of the atlas view, in CSS px. The backing store is this times the DPR. */
  const VIEW_PX = 296;

  /**
   * Above this triangle count the every-triangle wireframe is refused. It is one Path2D of three
   * segments per face; at 400k faces that is 1.2 M segments and a visible freeze, and the island
   * outlines say more anyway.
   */
  const WIREFRAME_LIMIT = 400_000;

  /** Enough degenerates to hand the 3D view a truthful set; the COUNT is always exact. */
  const DEGENERATE_LIST_CAP = 5000;

  type Overlay = 'none' | 'gutter' | 'overlap';
  type Edges = 'none' | 'islands' | 'all';

  let phase = $state<'empty' | 'working' | 'ready' | 'error'>('empty');
  let progress = $state('');
  let errorText = $state('');

  // 🔴 `$state.raw` throughout for the analysis outputs. They are typed arrays and canvases held
  // by reference and replaced wholesale; a deep proxy over them would buy nothing and cost a
  // wrapper on every element access in the draw loop.
  let layout = $state.raw<UvLayout | null>(null);
  let outline = $state.raw<UvOutline | null>(null);
  let grid = $state.raw<UvHitGrid | null>(null);
  let coverage = $state.raw<CoverageSummary | null>(null);
  let subTexelFaces = $state.raw(0);
  let texWidth = $state.raw(0);
  let texHeight = $state.raw(0);

  let showAtlas = $state(true);
  let overlay = $state<Overlay>('gutter');
  let edges = $state<Edges>('islands');
  let zoom = $state(1);
  let panU = $state(0);
  let panV = $state(0);

  let hoverFace = $state.raw(-1);
  let selectedIsland = $state.raw(-1);

  let view = $state.raw<HTMLCanvasElement | null>(null);

  /**
   * Cached layers, at the texture's own resolution. Each is 16 MB at 2048², which is why they are
   * built LAZILY — a session that never opens the overlap view never pays for it — and released
   * together whenever the analysis is thrown away.
   */
  let atlasLayer: HTMLCanvasElement | null = null;
  let gutterLayer: HTMLCanvasElement | null = null;
  let overlapLayer: HTMLCanvasElement | null = null;
  let islandLayer: HTMLCanvasElement | null = null;
  let wireLayer: HTMLCanvasElement | null = null;
  let selectionLayer: HTMLCanvasElement | null = null;
  /** Which island `selectionLayer` was drawn for; `-1` when it holds nothing. */
  let selectionFor = -1;
  /** The raw coverage counts, kept only as long as it takes to build the two overlay layers. */
  let counts: Uint8Array | null = null;

  // ⚠️ Plain `let`, not `$state`: these are the "have we already analysed this?" latch, and making
  // them reactive would re-run the effect that writes them.
  let lastMesh: THREE.Mesh | null = null;
  let lastTexture: THREE.Texture | null = null;
  let lastRenderer: THREE.WebGLRenderer | null = null;
  let pending = 0;
  let rafId = 0;

  const highlightIsland = $derived(
    layout && highlightFace != null && highlightFace >= 0 && highlightFace < layout.faceCount
      ? layout.faceIsland[highlightFace]
      : -1
  );
  const hoverIsland = $derived(layout && hoverFace >= 0 ? layout.faceIsland[hoverFace] : -1);
  const coveredPercent = $derived(
    coverage && coverage.total > 0 ? (coverage.covered / coverage.total) * 100 : 0
  );
  const overlapPercent = $derived(
    coverage && coverage.total > 0 ? (coverage.overlapping / coverage.total) * 100 : 0
  );
  const findings = $derived.by(() => {
    const list: { key: string; text: string; faces: number[] | null }[] = [];
    if (!layout) return list;
    if (coverage && coverage.overlapping > 0) {
      list.push({
        key: 'overlap',
        text:
          `${coverage.overlapping.toLocaleString()} texels are written by more than one triangle ` +
          `(${overlapPercent.toFixed(2)}% of the atlas, worst texel ${coverage.maxCount}). Two ` +
          'parts of the character are sharing colour there.',
        faces: null
      });
    }
    if (layout.outsideFaces > 0) {
      list.push({
        key: 'outside',
        text:
          `${layout.outsideFaces.toLocaleString()} triangles have a corner outside [0,1] ` +
          `(u ${layout.minU.toFixed(3)}…${layout.maxU.toFixed(3)}, v ${layout.minV.toFixed(3)}…` +
          `${layout.maxV.toFixed(3)}). On a wrapping map that tiles; on a projected atlas it means ` +
          'those faces sample the wrong island.',
        faces: null
      });
    }
    if (layout.degenerateCount > 0) {
      list.push({
        key: 'degenerate',
        text:
          `${layout.degenerateCount.toLocaleString()} UV triangles have zero area — all three ` +
          'corners land on one texel, so a whole face wears a single colour.',
        faces: layout.degenerateFaces
      });
    }
    return list;
  });

  // ── the analysis, staged ──────────────────────────────────────────────────────────────────
  //
  // Each step is deferred a frame so the progress line actually paints before the main thread is
  // taken. Same shape as the colour commit in the workbench, for the same reason.

  function defer(fn: () => void): void {
    pending = window.setTimeout(fn, 16);
  }

  function fail(e: unknown): void {
    errorText = e instanceof Error ? e.message : String(e);
    phase = 'error';
    progress = '';
  }

  function releaseLayers(): void {
    for (const canvas of [
      atlasLayer,
      gutterLayer,
      overlapLayer,
      islandLayer,
      wireLayer,
      selectionLayer
    ]) {
      // Zeroing the size drops the backing store now rather than at the GC's convenience — at
      // 16 MB apiece that difference is visible in a long editing session.
      if (canvas) {
        canvas.width = 0;
        canvas.height = 0;
      }
    }
    atlasLayer = null;
    gutterLayer = null;
    overlapLayer = null;
    islandLayer = null;
    wireLayer = null;
    selectionLayer = null;
    selectionFor = -1;
    counts = null;
  }

  function clearAll(): void {
    if (pending) window.clearTimeout(pending);
    pending = 0;
    releaseLayers();
    layout = null;
    outline = null;
    grid = null;
    coverage = null;
    subTexelFaces = 0;
    texWidth = 0;
    texHeight = 0;
    hoverFace = -1;
    selectedIsland = -1;
    zoom = 1;
    panU = 0;
    panV = 0;
    progress = '';
    errorText = '';
    phase = 'empty';
  }

  function analyse(): void {
    const body = mesh;
    const map = texture;
    if (!body || !map) {
      clearAll();
      return;
    }
    if (pending) window.clearTimeout(pending);
    releaseLayers();
    layout = null;
    outline = null;
    grid = null;
    coverage = null;
    hoverFace = -1;
    selectedIsland = -1;
    errorText = '';
    phase = 'working';
    progress = 'Reading the UVs…';
    onHighlightFaces?.(null);

    defer(() => {
      try {
        const read = analyseUvLayout(body.geometry, {
          maxDegenerateListed: DEGENERATE_LIST_CAP
        });
        layout = read;
        grid = buildUvHitGrid(body.geometry);
        progress = 'Drawing the atlas…';
      } catch (e) {
        fail(e);
        return;
      }
      defer(() => {
        try {
          const image = textureToImageData(map);
          texWidth = image.width;
          texHeight = image.height;
          subTexelFaces = layout ? countSubTexelFaces(layout, image.width, image.height) : 0;
          atlasLayer = layerFrom(image.width, image.height, (ctx) => ctx.putImageData(image, 0, 0));
          progress = 'Tracing the islands…';
        } catch (e) {
          fail(e);
          return;
        }
        defer(() => {
          try {
            if (layout) outline = uvIslandOutline(body.geometry, layout);
            progress = renderer ? 'Measuring coverage…' : '';
          } catch (e) {
            fail(e);
            return;
          }
          if (!renderer) {
            // No renderer, no coverage — but the layout is complete and worth showing, so fall
            // back to the edges the CPU already knows about rather than an empty view.
            if (overlay !== 'none') overlay = 'none';
            phase = 'ready';
            ensureLayers();
            return;
          }
          defer(() => {
            try {
              // 🔴 The one GPU pass. Both texel answers come out of this single buffer; running a
              // second pass for overlap would double a 16 MB readback for nothing.
              counts = buildCoverageCountRect(renderer, body, texWidth, texHeight);
              coverage = summariseCoverage(counts);
              if (coverage.covered === 0) {
                // `readRenderTargetPixels` logs and returns rather than throwing when the format
                // is unreadable, so an all-zero buffer is the only symptom — and it is impossible
                // for a real mesh, which is what makes it a safe tell.
                counts = null;
                coverage = null;
                throw new Error(
                  'The coverage pass came back empty. Either the UVs cover nothing or the GPU ' +
                    'refused the readback; the layout figures above are still good.'
                );
              }
            } catch (e) {
              fail(e);
              return;
            }
            progress = '';
            phase = 'ready';
            ensureLayers();
          });
        });
      });
    });
  }

  // ── layers ────────────────────────────────────────────────────────────────────────────────

  function layerFrom(
    width: number,
    height: number,
    paint: (ctx: CanvasRenderingContext2D) => void
  ): HTMLCanvasElement {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('The browser would not give this panel a 2D canvas to draw on.');
    paint(ctx);
    return canvas;
  }

  /** Texels the mesh never reaches (`gutter`) or reaches twice (`overlap`), as a wash. */
  function buildCountLayer(mode: 'gutter' | 'overlap'): HTMLCanvasElement | null {
    if (!counts || texWidth === 0) return null;
    const image = new ImageData(texWidth, texHeight);
    const data = image.data;
    const wash =
      mode === 'gutter' ? { r: 0, g: 170, b: 255, a: 120 } : { r: 255, g: 48, b: 48, a: 205 };
    for (let i = 0; i < counts.length; i++) {
      const hit = mode === 'gutter' ? counts[i] === 0 : counts[i] > 1;
      if (!hit) continue;
      const p = i * 4;
      data[p] = wash.r;
      data[p + 1] = wash.g;
      data[p + 2] = wash.b;
      data[p + 3] = wash.a;
    }
    return layerFrom(texWidth, texHeight, (ctx) => ctx.putImageData(image, 0, 0));
  }

  function strokeWidth(): number {
    return Math.max(1, Math.round(texWidth / 700));
  }

  function buildIslandLayer(): HTMLCanvasElement | null {
    const edgesOf = outline;
    if (!edgesOf || texWidth === 0) return null;
    return layerFrom(texWidth, texHeight, (ctx) => {
      ctx.strokeStyle = 'rgba(255, 214, 120, 0.95)';
      ctx.lineWidth = strokeWidth();
      ctx.beginPath();
      for (let e = 0; e < edgesOf.count; e++) {
        const p = e * 4;
        ctx.moveTo(edgesOf.segments[p] * texWidth, edgesOf.segments[p + 1] * texHeight);
        ctx.lineTo(edgesOf.segments[p + 2] * texWidth, edgesOf.segments[p + 3] * texHeight);
      }
      ctx.stroke();
    });
  }

  function buildWireLayer(): HTMLCanvasElement | null {
    const hit = grid;
    if (!hit || texWidth === 0) return null;
    return layerFrom(texWidth, texHeight, (ctx) => {
      ctx.strokeStyle = 'rgba(140, 220, 255, 0.4)';
      ctx.lineWidth = Math.max(1, strokeWidth() - 1);
      ctx.beginPath();
      for (let f = 0; f < hit.faceCount; f++) {
        const p = f * 6;
        ctx.moveTo(hit.corners[p] * texWidth, hit.corners[p + 1] * texHeight);
        ctx.lineTo(hit.corners[p + 2] * texWidth, hit.corners[p + 3] * texHeight);
        ctx.lineTo(hit.corners[p + 4] * texWidth, hit.corners[p + 5] * texHeight);
        ctx.closePath();
      }
      ctx.stroke();
    });
  }

  /**
   * The selected island's border, cached.
   *
   * 🔴 Cached rather than stroked per frame precisely because the selection must survive hovering:
   * a big island's border is tens of thousands of segments, and rebuilding that path inside the
   * pointer handler is the freeze this panel is written to avoid.
   */
  function buildSelectionLayer(island: number): HTMLCanvasElement | null {
    const edgesOf = outline;
    if (!edgesOf || island < 0 || texWidth === 0) return null;
    return layerFrom(texWidth, texHeight, (ctx) => {
      ctx.strokeStyle = 'rgba(74, 222, 128, 1)';
      ctx.lineWidth = strokeWidth() + 1;
      ctx.beginPath();
      for (let e = 0; e < edgesOf.count; e++) {
        if (edgesOf.island[e] !== island) continue;
        const p = e * 4;
        ctx.moveTo(edgesOf.segments[p] * texWidth, edgesOf.segments[p + 1] * texHeight);
        ctx.lineTo(edgesOf.segments[p + 2] * texWidth, edgesOf.segments[p + 3] * texHeight);
      }
      ctx.stroke();
    });
  }

  // ── drawing ───────────────────────────────────────────────────────────────────────────────

  function clamp(value: number, low: number, high: number): number {
    return value < low ? low : value > high ? high : value;
  }

  /** The visible window in UV space: origin plus edge length, always inside `[0,1]`. */
  function window0(): { u: number; v: number; size: number } {
    const size = 1 / zoom;
    return { u: clamp(panU, 0, 1 - size), v: clamp(panV, 0, 1 - size), size };
  }

  function requestDraw(): void {
    if (rafId) return;
    rafId = requestAnimationFrame(() => {
      rafId = 0;
      draw();
    });
  }

  function draw(): void {
    const canvas = view;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const w = canvas.width;
    const h = canvas.height;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = '#0d0f13';
    ctx.fillRect(0, 0, w, h);
    if (texWidth === 0) return;

    const win = window0();
    const sx = win.u * texWidth;
    const sy = win.v * texHeight;
    const sw = win.size * texWidth;
    const sh = win.size * texHeight;
    // Past 4× the point is to see individual texels, so stop interpolating them away.
    ctx.imageSmoothingEnabled = zoom < 4;

    if (showAtlas && atlasLayer) ctx.drawImage(atlasLayer, sx, sy, sw, sh, 0, 0, w, h);
    const wash = overlay === 'gutter' ? gutterLayer : overlay === 'overlap' ? overlapLayer : null;
    if (wash) ctx.drawImage(wash, sx, sy, sw, sh, 0, 0, w, h);
    const edgeLayer = edges === 'islands' ? islandLayer : edges === 'all' ? wireLayer : null;
    if (edgeLayer) ctx.drawImage(edgeLayer, sx, sy, sw, sh, 0, 0, w, h);
    if (selectionLayer && selectionFor === selectedIsland && selectedIsland >= 0) {
      ctx.drawImage(selectionLayer, sx, sy, sw, sh, 0, 0, w, h);
    }

    // Vector marks, cheap enough to redraw every frame: a box per highlighted island and the one
    // triangle under the cursor.
    const scale = w / sw;
    ctx.setTransform(scale, 0, 0, scale, -sx * scale, -sy * scale);
    ctx.lineWidth = 1.5 / scale;
    if (selectedIsland >= 0) strokeIslandBox(ctx, selectedIsland, 'rgba(74, 222, 128, 0.9)');
    if (highlightIsland >= 0 && highlightIsland !== selectedIsland) {
      strokeIslandBox(ctx, highlightIsland, 'rgba(96, 165, 250, 0.95)');
    }
    if (hoverIsland >= 0 && hoverIsland !== selectedIsland) {
      strokeIslandBox(ctx, hoverIsland, 'rgba(255, 255, 255, 0.55)');
    }
    if (hoverFace >= 0 && grid) {
      const p = hoverFace * 6;
      ctx.strokeStyle = 'rgba(255, 255, 255, 0.95)';
      ctx.beginPath();
      ctx.moveTo(grid.corners[p] * texWidth, grid.corners[p + 1] * texHeight);
      ctx.lineTo(grid.corners[p + 2] * texWidth, grid.corners[p + 3] * texHeight);
      ctx.lineTo(grid.corners[p + 4] * texWidth, grid.corners[p + 5] * texHeight);
      ctx.closePath();
      ctx.stroke();
    }
    ctx.setTransform(1, 0, 0, 1, 0, 0);
  }

  function strokeIslandBox(ctx: CanvasRenderingContext2D, id: number, colour: string): void {
    const island = layout?.islands[id];
    if (!island) return;
    ctx.strokeStyle = colour;
    ctx.strokeRect(
      island.minU * texWidth,
      island.minV * texHeight,
      (island.maxU - island.minU) * texWidth,
      (island.maxV - island.minV) * texHeight
    );
  }

  // ── controls ──────────────────────────────────────────────────────────────────────────────

  /**
   * Build whatever the current toggles need and nothing else.
   *
   * ⚠️ The layers are plain `let`s, not state: assigning one does NOT schedule a redraw, which is
   * deliberate (a redraw per layer would flash a half-composited view). Every path that builds one
   * asks for the redraw itself, and this is the path the analysis takes.
   */
  function ensureLayers(): void {
    if (overlay === 'gutter' && !gutterLayer) gutterLayer = buildCountLayer('gutter');
    if (overlay === 'overlap' && !overlapLayer) overlapLayer = buildCountLayer('overlap');
    if (edges === 'islands' && !islandLayer) islandLayer = buildIslandLayer();
    if (edges === 'all' && !wireLayer) wireLayer = buildWireLayer();
    requestDraw();
  }

  function setOverlay(next: Overlay): void {
    overlay = next;
    if (next === 'gutter' && !gutterLayer) gutterLayer = buildCountLayer('gutter');
    if (next === 'overlap' && !overlapLayer) overlapLayer = buildCountLayer('overlap');
    requestDraw();
  }

  function setEdges(next: Edges): void {
    edges = next;
    if (next === 'islands' && !islandLayer) islandLayer = buildIslandLayer();
    if (next === 'all' && !wireLayer) wireLayer = buildWireLayer();
    requestDraw();
  }

  function setZoom(next: number): void {
    const before = window0();
    const centreU = before.u + before.size / 2;
    const centreV = before.v + before.size / 2;
    zoom = next;
    const size = 1 / next;
    panU = clamp(centreU - size / 2, 0, 1 - size);
    panV = clamp(centreV - size / 2, 0, 1 - size);
    requestDraw();
  }

  /**
   * Select an island, or `-1` to clear.
   *
   * ⚠️ Deliberately NOT a toggle. A double-click delivers two clicks before `dblclick`, so a
   * toggle would leave the island the user just asked the camera to fly to deselected — and the
   * highlight on the model would go out at the moment it became useful. Clearing is a click on the
   * gutter, which is unambiguous.
   */
  function select(island: number): void {
    if (!layout) return;
    const next = island;
    if (next === selectedIsland) return;
    selectedIsland = next;
    if (selectionLayer) {
      selectionLayer.width = 0;
      selectionLayer.height = 0;
    }
    selectionLayer = next >= 0 ? buildSelectionLayer(next) : null;
    selectionFor = next;
    onHighlightFaces?.(next >= 0 ? islandFaces(layout, next) : null);
    requestDraw();
  }

  function showDegenerates(): void {
    if (!layout || layout.degenerateFaces.length === 0) return;
    selectedIsland = -1;
    if (selectionLayer) {
      selectionLayer.width = 0;
      selectionLayer.height = 0;
      selectionLayer = null;
    }
    selectionFor = -1;
    // ⚠️ `degenerateFaces` is capped at `DEGENERATE_LIST_CAP`; on an asset with more than that the
    // model shows the first N. The COUNT in the finding above is exact either way.
    onHighlightFaces?.(new Set(layout.degenerateFaces));
    requestDraw();
  }

  // ── pointer ───────────────────────────────────────────────────────────────────────────────

  let dragging = false;
  let dragX = 0;
  let dragY = 0;
  /**
   * Total pointer travel since the press, in px.
   *
   * ⚠️ Accumulated rather than measured against the press point: `dragX`/`dragY` advance on every
   * move so the pan is incremental, which means the distance from them is one step, not the
   * gesture. A drag that panned across the whole atlas and came back would otherwise read as a
   * click and change the selection out from under the user.
   */
  let dragTravel = 0;

  function uvAt(event: PointerEvent | MouseEvent): { u: number; v: number } | null {
    const canvas = view;
    if (!canvas) return null;
    const rect = canvas.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return null;
    const win = window0();
    // 🔴 No flip on v: the atlas is drawn with image row 0 at the top, which IS v = 0 because the
    // loader set `flipY = false`. See the module header.
    return {
      u: win.u + ((event.clientX - rect.left) / rect.width) * win.size,
      v: win.v + ((event.clientY - rect.top) / rect.height) * win.size
    };
  }

  function onPointerDown(event: PointerEvent): void {
    dragging = true;
    dragTravel = 0;
    dragX = event.clientX;
    dragY = event.clientY;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }

  function onPointerMove(event: PointerEvent): void {
    if (dragging) {
      dragTravel += Math.abs(event.clientX - dragX) + Math.abs(event.clientY - dragY);
    }
    if (dragging && zoom > 1) {
      const rect = view?.getBoundingClientRect();
      if (rect && rect.width > 0) {
        const win = window0();
        panU = clamp(win.u - ((event.clientX - dragX) / rect.width) * win.size, 0, 1 - win.size);
        panV = clamp(win.v - ((event.clientY - dragY) / rect.height) * win.size, 0, 1 - win.size);
        dragX = event.clientX;
        dragY = event.clientY;
        requestDraw();
        return;
      }
    }
    dragX = event.clientX;
    dragY = event.clientY;
    const hit = grid ? uvAt(event) : null;
    // ⚠️ One point-in-triangle sweep of one grid bucket. Nothing per-texel, nothing on the GPU,
    // and no layer rebuilt — that is the rule that keeps a 2048² atlas hoverable.
    const face = hit && grid ? uvHitFace(grid, hit.u, hit.v) : -1;
    if (face === hoverFace) return;
    hoverFace = face;
    onHoverFace?.(face);
    requestDraw();
  }

  function onPointerUp(event: PointerEvent): void {
    dragging = false;
    (event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId);
  }

  function onPointerLeave(): void {
    dragging = false;
    if (hoverFace === -1) return;
    hoverFace = -1;
    onHoverFace?.(-1);
    requestDraw();
  }

  /** A real click on the button — also what Enter and Space produce, which is the keyboard path. */
  function onActivate(): void {
    if (dragTravel > 3) {
      dragTravel = 0;
      return;
    }
    select(hoverIsland);
  }

  function onDouble(): void {
    if (hoverFace >= 0) onFocusFace?.(hoverFace);
  }

  // ── wiring ────────────────────────────────────────────────────────────────────────────────

  // Re-analyse when the thing being analysed changes — and the renderer counts, because a stage
  // that arrives a beat after the mesh would otherwise leave the coverage figures permanently
  // absent with no way to ask for them but the Re-measure button.
  $effect(() => {
    const body = mesh;
    const map = texture;
    const gl = renderer;
    if (body === lastMesh && map === lastTexture && gl === lastRenderer) return;
    lastMesh = body;
    lastTexture = map;
    lastRenderer = gl;
    analyse();
  });

  $effect(() => {
    const canvas = view;
    if (!canvas) return;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    canvas.width = Math.round(VIEW_PX * dpr);
    canvas.height = Math.round(VIEW_PX * dpr);
    requestDraw();
  });

  // Redraw on anything that changes what the composite looks like. Reading each value here is what
  // subscribes the effect to it.
  $effect(() => {
    void [phase, showAtlas, overlay, edges, zoom, panU, panV, hoverFace, highlightIsland];
    requestDraw();
  });

  onDestroy(() => {
    if (pending) window.clearTimeout(pending);
    if (rafId) cancelAnimationFrame(rafId);
    releaseLayers();
    // Leave nothing painted on the model on the way out.
    onHighlightFaces?.(null);
  });
</script>

<div class="hud">
  <div class="head">
    <span class="title">UV atlas</span>
    <span class="flag">read-only</span>
  </div>

  {#if !mesh || !texture}
    <p class="hint">
      Load a character with a baseColor map to see its atlas. Nothing here edits the texture — it
      draws the layout the baker produced so a seam or a smear on the model can be traced to the
      island it came from.
    </p>
  {:else}
    <button
      class="atlas"
      type="button"
      aria-label="UV atlas. Move the pointer over it to identify an island, click to select it."
      onpointerdown={onPointerDown}
      onpointermove={onPointerMove}
      onpointerup={onPointerUp}
      onpointerleave={onPointerLeave}
      onclick={onActivate}
      ondblclick={onDouble}
    >
      <canvas bind:this={view} style:width="{VIEW_PX}px" style:height="{VIEW_PX}px"></canvas>
    </button>

    {#if phase === 'working'}
      <p class="work">{progress}</p>
    {:else if phase === 'error'}
      <p class="bad">{errorText}</p>
    {/if}

    <div class="row">
      <button
        class="btn-sec small"
        class:on={showAtlas}
        onclick={() => {
          showAtlas = !showAtlas;
        }}
      >
        Texture
      </button>
      <button
        class="btn-sec small"
        class:on={overlay === 'gutter'}
        disabled={!coverage}
        title="Texels no triangle reaches"
        onclick={() => setOverlay(overlay === 'gutter' ? 'none' : 'gutter')}
      >
        Gutter
      </button>
      <button
        class="btn-sec small"
        class:on={overlay === 'overlap'}
        disabled={!coverage}
        onclick={() => setOverlay(overlay === 'overlap' ? 'none' : 'overlap')}
      >
        Overlap
      </button>
    </div>

    <div class="row">
      <button
        class="btn-sec small"
        class:on={edges === 'islands'}
        onclick={() => setEdges(edges === 'islands' ? 'none' : 'islands')}
      >
        Island edges
      </button>
      <button
        class="btn-sec small"
        class:on={edges === 'all'}
        disabled={(layout?.faceCount ?? 0) > WIREFRAME_LIMIT}
        onclick={() => setEdges(edges === 'all' ? 'none' : 'all')}
      >
        Every triangle
      </button>
      {#each [1, 2, 4, 8] as level (level)}
        <button class="btn-sec small" class:on={zoom === level} onclick={() => setZoom(level)}>
          {level}×
        </button>
      {/each}
    </div>

    {#if layout}
      <dl class="facts">
        <dt>Atlas</dt>
        <dd>{texWidth} × {texHeight}</dd>
        <dt>Islands</dt>
        <dd>{layout.islands.length.toLocaleString()}</dd>
        <dt>Triangles</dt>
        <dd>{layout.faceCount.toLocaleString()}</dd>
        <dt>Covered</dt>
        <dd>
          {#if coverage}
            <b class:warn={coveredPercent < 25}>{coveredPercent.toFixed(1)}%</b>
            of its texels
          {:else}
            <span class="dim">not measured</span>
          {/if}
        </dd>
        <dt>UV area</dt>
        <dd>{(layout.uvArea * 100).toFixed(1)}% laid out</dd>
        {#if subTexelFaces > 0}
          <dt>Sub-texel</dt>
          <dd>{subTexelFaces.toLocaleString()} faces under one texel</dd>
        {/if}
      </dl>
    {/if}

    {#if phase === 'ready'}
      {#if findings.length === 0}
        <p class="hint ok">
          No UVs outside [0,1], no overlapping texels and no zero-area triangles. Whatever is wrong
          on the model is not the layout.
        </p>
      {:else}
        <ul class="findings">
          {#each findings as finding (finding.key)}
            <li>
              {finding.text}
              {#if finding.key === 'degenerate' && onHighlightFaces}
                <button class="btn-sec small" onclick={showDegenerates}
                  >Show them on the model</button
                >
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    {/if}

    {#if layout && hoverIsland >= 0}
      {@const island = layout.islands[hoverIsland]}
      <p class="hint">
        Island {hoverIsland + 1} of {layout.islands.length} — {island.faces.toLocaleString()} triangles,
        {(island.area * 100).toFixed(2)}% of the atlas{island.degenerate > 0
          ? `, ${island.degenerate} of them degenerate`
          : ''}. Face {hoverFace}.
      </p>
    {:else if layout && selectedIsland >= 0}
      {@const island = layout.islands[selectedIsland]}
      <p class="hint">
        Island {selectedIsland + 1} selected — {island.faces.toLocaleString()} triangles,
        {(island.area * 100).toFixed(2)}% of the atlas. Click the gutter to clear it.
      </p>
    {:else if layout && highlightIsland >= 0}
      <p class="hint">The 3D view is pointing at island {highlightIsland + 1} (blue box).</p>
    {/if}

    <p class="hint">
      {#if onHighlightFaces}
        Click an island to light it up on the character{onFocusFace
          ? '; double-click to fly the camera there'
          : ''}.
      {:else}
        Hovering names the island under the pointer.
      {/if}
      Blue is atlas the mesh never reaches — gutter, and the resolution it costs. Red is two triangles
      fighting over one texel.
    </p>

    <div class="row">
      <button class="btn-sec small" disabled={phase === 'working'} onclick={analyse}>
        {phase === 'working' ? progress || 'Working…' : 'Re-measure'}
      </button>
    </div>
  {/if}
</div>

<style>
  .hud {
    width: 320px;
    box-sizing: border-box;
    padding: 10px 12px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 10px 30px rgb(0 0 0 / 45%);
    pointer-events: auto;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    justify-content: space-between;
    margin-bottom: 6px;
  }
  .title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .flag {
    font-size: 10px;
    color: var(--muted);
  }
  .atlas {
    display: block;
    width: 100%;
    padding: 0;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: #0d0f13;
    cursor: crosshair;
    overflow: hidden;
    line-height: 0;
    touch-action: none;
  }
  .atlas canvas {
    display: block;
    margin: 0 auto;
    max-width: 100%;
  }
  .row {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
    margin-top: 8px;
  }
  .btn-sec.small {
    padding: 3px 8px;
    font-size: 11px;
  }
  .btn-sec.small.on {
    border-color: var(--accent, #d9952d);
    color: var(--accent, #d9952d);
  }
  .facts {
    display: grid;
    grid-template-columns: 66px 1fr;
    gap: 2px 8px;
    margin: 10px 0 0;
    font-size: 11px;
    color: var(--muted);
  }
  .facts dt {
    white-space: nowrap;
  }
  .facts dd {
    margin: 0;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .facts b {
    font-weight: 600;
  }
  .facts b.warn {
    color: var(--warning, #d9952d);
  }
  .facts .dim {
    color: var(--muted);
  }
  .findings {
    margin: 8px 0 0;
    padding-left: 16px;
    font-size: 10.5px;
    line-height: 1.4;
    color: var(--warning, #d9952d);
  }
  .findings li + li {
    margin-top: 5px;
  }
  .findings button {
    margin-top: 3px;
  }
  .hint {
    color: var(--muted);
    font-size: 10.5px;
    line-height: 1.4;
    margin: 8px 0 0;
  }
  .hint.ok {
    color: var(--ok, #4ade80);
  }
  .work {
    color: var(--muted);
    font-size: 10.5px;
    margin: 6px 0 0;
  }
  .bad {
    color: var(--danger, #ef4444);
    font-size: 10.5px;
    line-height: 1.4;
    margin: 6px 0 0;
  }
</style>
