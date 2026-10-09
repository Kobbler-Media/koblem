/*
 * 🔴 READ THIS BEFORE PUTTING `repairAtlas` / `dilateIslands` / `inpaintHoles` BEHIND A BUTTON.
 * DO NOT. The engine's baker already does both halves, and redoing one of them is ACTIVELY HARMFUL.
 *
 * Checked against `tex_atlas.hpp` in the longcat-rig source, 2026-08-15:
 *
 *   1. **Interior holes are already inpainted.** The baker marks a texel invalid when it is
 *      `covered AND all-channels-zero` — "the remeshed surface is too far from the sparse PBR shell
 *      in deep concavities (skirt folds, underarm)" — and fills it from valid chart neighbours. So
 *      the gaps this module's `interiorAlphaGaps` was written to catch have already been filled,
 *      and they were never alpha-0 anyway: they are opaque black, which is precisely the case that
 *      heuristic cannot see.
 *
 *   2. 🔴 **Dilating further REINTRODUCES a seam glitch the engine engineered around.** Its gutter
 *      fill is deliberately capped at `padding-1` in precluster mode, with this reason: "a big fill
 *      BLEEDS each chart's colour across the gutter into its neighbours -> teal 'peeking through
 *      the cuts' (the seam-glitch). Cap the fill at padding-1 so each chart only fills its OWN
 *      gutter and never reaches a neighbour's rendered texels (neighbours are `padding` apart)."
 *      A browser pass that dilates "4-8 texels" because that is the usual answer for a 2048 map
 *      does not know this atlas's padding, and will merge adjacent charts.
 *
 * What in this module IS worth exposing: `adjustColour` (the engine is deliberately faithful to a
 * source photo, so "it came out dark" is a real complaint it will never fix), the
 * `textureToImageData` / `applyToTexture` / `encodeTexture` plumbing every texture edit needs, and
 * `buildCoverageMask` as the substrate for projection painting. The repair functions stay because
 * they are correct and because a future edit that CREATES holes (a paint stroke, a UV change) would
 * want them — not because this pipeline's output needs them.
 */

/*
 * Repairing the baked atlas of an image-to-3D character, in texture space.
 *
 * # What is actually wrong with these textures
 *
 * The map is a PROJECTION BAKE: the source photo is projected onto the reconstructed mesh and
 * written into an automatic UV atlas. That produces exactly two defects, and they need two
 * different fixes:
 *
 * 1. **Seams.** Bilinear filtering and mip generation both sample OUTSIDE an island's texels. The
 *    gutter between islands holds whatever the baker left there — usually background or black — so
 *    a dark rim crawls onto the silhouette of every island. Fix: push island colour outward a few
 *    texels ([`dilateIslands`]) so every tap that lands in the gutter lands on island colour.
 * 2. **Interior gaps.** Where the projection had no data (occlusion shadows) the island is filled
 *    with nothing useful. Dilation cannot reach the middle of a hole; that needs an inpaint
 *    ([`inpaintHoles`]).
 *
 * The map being DARK is not a defect — it is dark because the source photo is. That is what
 * [`adjustColour`] is for, and it is a separate, user-driven operation. Do not fold it into the
 * automatic repair.
 *
 * # 🔴 The orientation rule, stated once
 *
 * `GLTFLoader` sets `texture.flipY = false`, so three uploads the decoded image rows in file
 * order and image row 0 is `v = 0`. Therefore the canvas/`ImageData` pixel for a UV is
 * `(uv.x * W, uv.y * H)` — **not** `(1 - uv.y) * H`. Every loop in this file assumes
 * `index = (y * W + x) * 4` with `y = uv.y * H`.
 *
 * ⚠️ The coverage mask agrees with that WITHOUT a flip, and it is worth knowing why, because it
 * looks like a bug until you check it. The UV-space pass writes `gl_Position.y = uv.y * 2 - 1`, so
 * `uv.y = 0` lands at clip `y = -1`, the BOTTOM of the framebuffer. `gl.readPixels` returns rows
 * bottom-up. So readback row 0 is `uv.y = 0` — and `ImageData` row 0 is `uv.y = 0` as well, by the
 * flipY rule above. Two opposite conventions cancel. If you "fix" one of them you break both.
 *
 * # 🔴 Mutate the live `THREE.Texture`, never replace it
 *
 * [`applyToTexture`] assigns `texture.image = canvas` and sets `needsUpdate`. Do not reach for
 * `new THREE.CanvasTexture(...)`: its constructor defaults are `flipY = true` and
 * `colorSpace = NoColorSpace`, so the model comes back upside down AND washed out (an sRGB map
 * sampled as linear), and you also lose `wrapS/wrapT`, the filters, the anisotropy and
 * `userData.mimeType` that `GLTFLoader` recorded from the source. Keeping the same Texture object
 * preserves all of it for free.
 *
 * # Cost model — read before wiring a slider to any of this
 *
 * 🔴 A 2048² map is 4.19 M texels. `getImageData` on it allocates and copies 16.8 MB; the pixel
 * functions here allocate one output of the same size each. That is fine ONCE, on commit. Called
 * from `pointermove` it is ~1 GB/s of garbage and the tab janks. That is why the live path is
 * `live-grade.ts`, which injects this module's maths into the material's fragment shader and grades
 * on the model itself with no readback at all, and the exact path is [`adjustColour`], for the
 * single commit.
 *
 * ⚠️ The live path is deliberately NOT a `ctx.filter` string, which looks cheaper still. CSS filter
 * functions operate on sRGB-ENCODED values, so `brightness()` is a gamma-space lift rather than an
 * exposure stop — it raises the shadows more and the highlights less than a stop does — and a
 * preview built on it disagrees with the commit at both ends of every slider.
 *
 * ⚠️ `generateMipmaps` is true on loader textures, so every `texture.needsUpdate = true` rebuilds
 * the whole mip chain for a 2048² map. Coalesce updates to one per commit; that is the caller's
 * job, this module cannot see your event loop.
 *
 * # What is deliberately NOT here
 *
 * A normal-map paint brush. Painting a normal map writes non-unit vectors, and non-unit normals
 * produce lighting that is wrong in a way nobody can diagnose from the image. [`setNormalStrength`]
 * is the safe operation on a normal map: a lerp toward flat, which stays on the same side of the
 * hemisphere and is renormalised by the shader anyway.
 */

import * as THREE from 'three';

// ---------------------------------------------------------------------------------------------
// sRGB transfer, as lookup tables
//
// Everything that AVERAGES or SCALES colour has to do it on linear values. Averaging sRGB-encoded
// bytes is the classic "why is my blur muddy" bug: the encoding is a ~2.2 power curve, so the mean
// of two encoded values is not the encoding of the mean. Dilation is exempt because it COPIES
// texels rather than mixing them across a wide range; the inpaint pyramid and the colour grade are
// not.
//
// LUTs rather than `Math.pow` per texel: at 4.19 M texels × 3 channels, `pow` is the whole runtime.
// The encode table is 65536 entries so the shadow end stays accurate — the sRGB curve is nearly
// vertical near zero, and a 4096-entry table quantises the darkest 20 code values into 1.
// ---------------------------------------------------------------------------------------------

const ENCODE_STEPS = 65536;

/** sRGB byte (0..255) → linear float (0..1). */
const SRGB_TO_LINEAR = /* @__PURE__ */ (() => {
  const lut = new Float32Array(256);
  for (let i = 0; i < 256; i++) {
    const c = i / 255;
    lut[i] = c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
  }
  return lut;
})();

/** linear float (0..1, quantised to `ENCODE_STEPS`) → sRGB byte. */
const LINEAR_TO_SRGB = /* @__PURE__ */ (() => {
  const lut = new Uint8Array(ENCODE_STEPS);
  for (let i = 0; i < ENCODE_STEPS; i++) {
    const c = i / (ENCODE_STEPS - 1);
    const s = c <= 0.0031308 ? c * 12.92 : 1.055 * Math.pow(c, 1 / 2.4) - 0.055;
    lut[i] = Math.round(s * 255);
  }
  return lut;
})();

function encodeSrgb(linear: number): number {
  // Clamp here rather than at every call site — exposure and contrast both overshoot 1 routinely.
  const c = linear <= 0 ? 0 : linear >= 1 ? 1 : linear;
  return LINEAR_TO_SRGB[(c * (ENCODE_STEPS - 1) + 0.5) | 0];
}

/**
 * Linear value of sRGB 0.5.
 *
 * Contrast has to pivot somewhere, and mid-grey is the choice that leaves a mid-tone alone. The
 * grade runs on linear values, so the pivot is the LINEAR equivalent of the encoded 0.5 the eye
 * reads as mid — pivoting at linear 0.5 instead sits 1.2 stops brighter than the tone it is meant
 * to hold still, so raising contrast would drag mid-grey itself down.
 *
 * 🔴 Exported because `live-grade.ts` bakes it into a GLSL literal. Two copies of this number is
 * two grades: the GPU preview would pivot somewhere the commit does not, and the model would shift
 * the instant the user pressed Apply. Import it, never retype it.
 */
export const CONTRAST_PIVOT = 0.21404114;

/**
 * Rec.709 luma, the same weights CSS `saturate()` uses.
 *
 * 🔴 Exported for the same single-definition reason as {@link CONTRAST_PIVOT} — `live-grade.ts`
 * compiles these three into the fragment shader's `dot()`.
 */
export const LUMA_R = 0.2126;
export const LUMA_G = 0.7152;
export const LUMA_B = 0.0722;

/**
 * How far `temperature: ±1` pushes the red/blue gains. Kept modest — this is a nudge, not a LUT.
 *
 * 🔴 Exported so the GPU preview folds the same channel gains; see {@link CONTRAST_PIVOT}.
 */
export const TEMPERATURE_GAIN = 0.25;

// ---------------------------------------------------------------------------------------------
// Canvas helpers
// ---------------------------------------------------------------------------------------------

/**
 * ⚠️ Every function in this module is browser-only. SvelteKit will happily import the module
 * during SSR, so fail with a sentence a human can act on rather than `document is not defined`
 * three frames deep in a loop.
 */
function requireDocument(): Document {
  if (typeof document === 'undefined') {
    throw new Error(
      'Texture repair runs in the browser only; call it from onMount, not during SSR.'
    );
  }
  return document;
}

/**
 * 🔴 `willReadFrequently: true` is not optional here. Without it Chrome keeps the backing store on
 * the GPU and every `getImageData` stalls the pipeline to read it back — tens of milliseconds per
 * call, on the main thread. three's own `GLTFExporter` sets it for exactly this reason.
 *
 * ⚠️ `HTMLCanvasElement`, not `OffscreenCanvas`: the canvas we hand to a texture may later be read
 * by `GLTFExporter`, which calls `toDataURL` — a method `OffscreenCanvas` does not have.
 */
function createCanvas(width: number, height: number): HTMLCanvasElement {
  const canvas = requireDocument().createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  return canvas;
}

function get2d(canvas: HTMLCanvasElement): CanvasRenderingContext2D {
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  if (!ctx)
    throw new Error(
      'Could not get a 2D canvas context; the browser may have lost it under memory pressure.'
    );
  return ctx;
}

// ---------------------------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------------------------

/**
 * The UV-space pass. `gl_Position = vec4( uv * 2 - 1, 0, 1 )` rasterises each triangle where it
 * LIVES IN THE ATLAS rather than where it lives in the world, so the framebuffer is the ground
 * truth of which texels the mesh samples.
 *
 * ⚠️ `uv` is declared for us. three's `ShaderMaterial` prefix always emits
 * `attribute vec3 position; attribute vec3 normal; attribute vec2 uv;` — redeclaring any of them
 * is a compile error, and the error text points at the wrong line.
 */
const COVERAGE_VERTEX_SHADER = /* glsl */ `
void main() {
  gl_Position = vec4( uv * 2.0 - 1.0, 0.0, 1.0 );
}
`;

/**
 * ⚠️ Writes 1.0 with no colour-space conversion, deliberately.
 *
 * Rendering into a `WebGLRenderTarget` encodes to the WORKING space, not the target's — r185 picks
 * `( _currentRenderTarget === null ) ? outputColorSpace : workingColorSpace`. But that conversion
 * lives in the `<colorspace_fragment>` chunk, which only a built-in material includes. A hand-written
 * `ShaderMaterial` writes exactly what it says, so 1.0 reads back as 255. If you ever port a
 * UV-space pass to a built-in material, expect the values to move.
 */
const COVERAGE_FRAGMENT_SHADER = /* glsl */ `
void main() {
  gl_FragColor = vec4( 1.0 );
}
`;

/**
 * Which texels any UV island actually covers, as one byte per texel (1 = covered).
 *
 * 🔴 Do NOT infer coverage from alpha. A projection bake fills its gutters with OPAQUE black, so
 * an alpha test marks the entire gutter as covered and the seam fix silently does nothing.
 *
 * ⚠️ This is a plain fill pass, so a texel counts as covered only if its CENTRE is inside a
 * triangle. Texels an island edge only partly covers come out UNCOVERED — which is what we want:
 * their colour is a blend of island and gutter, i.e. already contaminated, and letting dilation
 * overwrite them with clean island colour is the seam fix. Adding a wireframe pass to "catch" them
 * would preserve the contamination it is meant to remove.
 *
 * @param size Mask resolution. Match the texture, or the mask and the pixels disagree.
 */
export function buildCoverageMask(
  renderer: THREE.WebGLRenderer,
  mesh: THREE.Mesh,
  size: number
): Uint8Array {
  return buildCoverageMaskRect(renderer, mesh, size, size);
}

/**
 * [`buildCoverageMask`] for non-square maps. The square entry point exists because that is the
 * shape every one of these bakes has been so far; this is the one that does the work.
 */
export function buildCoverageMaskRect(
  renderer: THREE.WebGLRenderer,
  mesh: THREE.Mesh,
  width: number,
  height: number
): Uint8Array {
  if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1) {
    throw new Error(`Coverage mask size must be positive integers, got ${width}×${height}.`);
  }
  const maxSize = renderer.capabilities.maxTextureSize;
  if (width > maxSize || height > maxSize) {
    throw new Error(
      `This texture is ${width}×${height}, larger than the GPU's ${maxSize} limit; it cannot be repaired here.`
    );
  }

  const geometry = mesh.geometry;
  if (!geometry.getAttribute('uv')) {
    throw new Error('This mesh has no UV coordinates, so its texture cannot be repaired.');
  }

  const target = new THREE.WebGLRenderTarget(width, height, {
    format: THREE.RGBAFormat,
    type: THREE.UnsignedByteType,
    // Nearest + no mips: this buffer is only ever read back texel-for-texel. Mip generation on a
    // mask is pure waste and filtering it would blur the very edges we are measuring.
    minFilter: THREE.NearestFilter,
    magFilter: THREE.NearestFilter,
    generateMipmaps: false,
    // No geometry-space depth exists in a UV pass; overlapping islands should all write.
    depthBuffer: false,
    stencilBuffer: false,
    // ⚠️ samples: 0. MSAA would give partially-covered edge texels a fractional value, turning a
    // binary mask into a threshold argument for no benefit.
    samples: 0,
    colorSpace: THREE.NoColorSpace
  });

  const material = new THREE.ShaderMaterial({
    vertexShader: COVERAGE_VERTEX_SHADER,
    fragmentShader: COVERAGE_FRAGMENT_SHADER,
    // 🔴 DoubleSide is load-bearing. Automatic atlases contain MIRRORED islands, whose triangles
    // wind backwards once projected into UV space. With the default FrontSide those islands are
    // culled and come back as holes in the mask — which then reads as "the bake is missing half
    // the character" rather than "the material was wrong".
    side: THREE.DoubleSide,
    depthTest: false,
    depthWrite: false,
    blending: THREE.NoBlending
  });

  // A proxy so we never reparent the live mesh — `scene.add` would yank it out of the viewer.
  // Sharing `geometry` is intentional and free; see the dispose note below.
  const proxy = new THREE.Mesh(geometry, material);

  // 🔴 Without this the pass renders NOTHING for most meshes. The vertex shader writes clip space
  // directly, but three culls against the geometry's bounding sphere transformed by the CAMERA —
  // a sphere describing where the mesh sits in the WORLD, which has no relationship to where it
  // draws in the atlas. A character standing 3 m from the origin is simply outside the identity
  // frustum and never rasterises.
  proxy.frustumCulled = false;

  const scene = new THREE.Scene();
  scene.add(proxy);

  // A bare Camera: identity projection and identity view, so clip space is whatever the vertex
  // shader wrote. An OrthographicCamera would apply its own matrix on top and skew the atlas.
  const camera = new THREE.Camera();

  const prevTarget = renderer.getRenderTarget();
  const prevCubeFace = renderer.getActiveCubeFace();
  const prevMipLevel = renderer.getActiveMipmapLevel();
  const prevClearColor = new THREE.Color();
  renderer.getClearColor(prevClearColor);
  const prevClearAlpha = renderer.getClearAlpha();
  const prevAutoClear = renderer.autoClear;

  const buffer = new Uint8Array(width * height * 4);
  try {
    renderer.setRenderTarget(target);
    renderer.autoClear = false;
    renderer.setClearColor(0x000000, 1);
    renderer.clear(true, false, false);
    renderer.render(scene, camera);
    renderer.readRenderTargetPixels(target, 0, 0, width, height, buffer);
  } finally {
    // Restore before anything can throw its way out of here; leaving the app's renderer pointed at
    // a disposed target turns the next frame black with no error.
    renderer.setRenderTarget(prevTarget, prevCubeFace, prevMipLevel);
    renderer.setClearColor(prevClearColor, prevClearAlpha);
    renderer.autoClear = prevAutoClear;

    scene.remove(proxy);
    material.dispose();
    target.dispose();
    // 🔴 Never `geometry.dispose()` here — it is the LIVE mesh's geometry, shared not copied.
    // Disposing it drops the VBOs out from under the viewer on the next frame.
  }

  // ⚠️ `readRenderTargetPixels` logs and returns instead of throwing when the format is unreadable,
  // so an all-zero buffer is the only symptom. The caller-facing check for that is in `repairAtlas`,
  // which knows that zero coverage is impossible for a real mesh.
  const mask = new Uint8Array(width * height);
  for (let i = 0, p = 0; i < mask.length; i++, p += 4) {
    mask[i] = buffer[p] > 127 ? 1 : 0;
  }
  return mask;
}

// ---------------------------------------------------------------------------------------------
// Texture ↔ ImageData
// ---------------------------------------------------------------------------------------------

/**
 * Pull a texture's pixels into an `ImageData`, in the orientation described at the top of the file.
 *
 * ⚠️ `texture.image` is an `ImageBitmap` on every modern browser — `GLTFLoader` decodes through
 * `ImageBitmapLoader` with `{ premultiplyAlpha: 'none', colorSpaceConversion: 'none' }` — and an
 * `HTMLImageElement` on the fallback path (old Safari/Firefox). `drawImage` takes either, which is
 * why this is one function and not two, but code that assumes `.naturalWidth` or `.src` breaks on
 * the common case.
 *
 * 🔴 No flip. `drawImage` puts the image's first row at canvas y = 0, which IS `v = 0` because the
 * loader set `flipY = false`. Adding a flip here mirrors the entire repair.
 */
export function textureToImageData(texture: THREE.Texture): ImageData {
  const source: unknown = texture.image;
  if (!source) {
    throw new Error('This texture has no decoded image, so it cannot be edited.');
  }
  if ('isCompressedTexture' in texture) {
    throw new Error('This texture is GPU-compressed (KTX2/Basis); it has no editable pixels.');
  }

  // A DataTexture-style source: already the bytes we want, no canvas round trip needed.
  const dataSource = source as { data?: unknown; width?: unknown; height?: unknown };
  const rawData = dataSource.data;
  const rawWidth = dataSource.width;
  const rawHeight = dataSource.height;
  if (rawData && typeof rawWidth === 'number' && typeof rawHeight === 'number') {
    const bytes =
      rawData instanceof Uint8Array || rawData instanceof Uint8ClampedArray ? rawData : null;
    if (!bytes) {
      throw new Error(
        'This texture stores floating-point pixels, which this editor cannot modify.'
      );
    }
    if (bytes.length !== rawWidth * rawHeight * 4) {
      throw new Error('This texture is not 8-bit RGBA, which this editor cannot modify.');
    }
    return new ImageData(new Uint8ClampedArray(bytes), rawWidth, rawHeight);
  }

  if (source instanceof ImageData) {
    return new ImageData(new Uint8ClampedArray(source.data), source.width, source.height);
  }

  // ⚠️ Read the size structurally, not through `CanvasImageSource`: that union's `width` is an
  // `SVGAnimatedLength` for one member, so intersecting it with `number` collapses to `never`.
  const sized = source as {
    width?: number;
    height?: number;
    naturalWidth?: number;
    naturalHeight?: number;
  };
  const width = sized.naturalWidth || sized.width || 0;
  const height = sized.naturalHeight || sized.height || 0;
  if (!width || !height) {
    throw new Error('This texture reports no size; it may still be decoding.');
  }

  const canvas = createCanvas(width, height);
  const ctx = get2d(canvas);
  ctx.drawImage(source as CanvasImageSource, 0, 0);
  return ctx.getImageData(0, 0, width, height);
}

/**
 * Write an `ImageData` back onto the live texture, in place.
 *
 * 🔴 In place is the whole point — see the module header. The replacement source is a canvas, which
 * three uploads exactly like the `ImageBitmap` it replaces because `flipY` is still false.
 *
 * ⚠️ The previous `ImageBitmap` is left alive rather than `.close()`d, so an undo stack can put it
 * back. If you keep no undo, close it yourself: it is another 16.8 MB at 2048² that the GC cannot
 * reclaim on its own.
 *
 * ⚠️ One call rebuilds the whole mip chain. Do it once per commit, never per slider frame.
 */
export function applyToTexture(texture: THREE.Texture, image: ImageData): void {
  const canvas = createCanvas(image.width, image.height);
  // `putImageData` bypasses filter, globalAlpha and compositing by spec — the bytes land verbatim.
  get2d(canvas).putImageData(image, 0, 0);
  texture.image = canvas;
  texture.needsUpdate = true;
}

// ---------------------------------------------------------------------------------------------
// Dilation — the seam fix
// ---------------------------------------------------------------------------------------------

/** Growable index queue; a plain `number[]` of 4 M entries is a boxed-value disaster. */
class IndexQueue {
  data: Int32Array;
  length = 0;

  constructor(capacity = 1 << 16) {
    this.data = new Int32Array(capacity);
  }

  push(value: number): void {
    if (this.length === this.data.length) {
      const grown = new Int32Array(this.data.length * 2);
      grown.set(this.data);
      this.data = grown;
    }
    this.data[this.length++] = value;
  }
}

/**
 * Push island colours outward `texels` texels so bilinear taps and mip reduction stop pulling
 * gutter into the silhouette. This is the seam fix.
 *
 * 🔴 `mask` is UPDATED IN PLACE: every texel this fills becomes covered, because that is what makes
 * the next pass — and [`inpaintHoles`] after it — see the right frontier. Pass `mask.slice()` if
 * you need the original coverage afterwards. The image is NOT mutated; a new one is returned.
 *
 * How wide? Bilinear reaches one texel, but each mip level doubles the reach in texel terms, so
 * the usual rule is ~1 texel per 512 of atlas edge (4 at 2048²). More is harmless but slower.
 *
 * ⚠️ Colours are averaged in the ENCODED space here, unlike everywhere else in this file. The
 * average is over at most 8 immediate neighbours of a single island, so the values are close
 * together and the gamma error is a fraction of a code value — and these texels are only ever seen
 * as a partial filter tap. Doing it properly would mean two full LUT passes for nothing visible.
 */
export function dilateIslands(image: ImageData, mask: Uint8Array, texels: number): ImageData {
  const { width, height } = image;
  if (mask.length !== width * height) {
    throw new Error(
      `Coverage mask is ${mask.length} texels but the texture is ${width * height}; they must match.`
    );
  }

  const out = new ImageData(new Uint8ClampedArray(image.data), width, height);
  if (texels <= 0) return out;

  const data = out.data;
  const passes = Math.floor(texels);

  // Pass 1 has to scan everything to find the frontier. Every later pass only needs to look at the
  // neighbours of what the previous pass filled — a newly-reachable texel is by definition adjacent
  // to one of them — which is why this is O(N + filled) and not O(N × texels).
  let frontier = new IndexQueue();
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const i = y * width + x;
      if (mask[i] !== 0) continue;
      if (!hasCoveredNeighbour(mask, width, height, x, y)) continue;
      // 2 = queued for this pass. It has to be distinguishable from 1, because a texel filled in
      // THIS pass must not be read as source by another texel in the same pass — that cascades in
      // raster order and smears colour diagonally down-right across the whole gutter.
      mask[i] = 2;
      frontier.push(i);
    }
  }

  for (let pass = 0; pass < passes; pass++) {
    if (frontier.length === 0) break;

    const filled = frontier.data;
    const count = frontier.length;
    for (let n = 0; n < count; n++) {
      const i = filled[n];
      const x = i % width;
      const y = (i - x) / width;
      let r = 0;
      let g = 0;
      let b = 0;
      let a = 0;
      let total = 0;
      for (let dy = -1; dy <= 1; dy++) {
        const ny = y + dy;
        if (ny < 0 || ny >= height) continue;
        for (let dx = -1; dx <= 1; dx++) {
          const nx = x + dx;
          if (nx < 0 || nx >= width || (dx === 0 && dy === 0)) continue;
          const j = ny * width + nx;
          if (mask[j] !== 1) continue; // 1 only: never read a texel queued or filled this pass.
          const p = j * 4;
          r += data[p];
          g += data[p + 1];
          b += data[p + 2];
          a += data[p + 3];
          total++;
        }
      }
      if (total === 0) continue;
      const p = i * 4;
      data[p] = r / total;
      data[p + 1] = g / total;
      data[p + 2] = b / total;
      data[p + 3] = a / total;
    }

    // Promote this pass's fills to covered, then build the next frontier from their neighbours.
    for (let n = 0; n < count; n++) mask[filled[n]] = 1;
    if (pass === passes - 1) break;

    const next = new IndexQueue();
    for (let n = 0; n < count; n++) {
      const i = filled[n];
      const x = i % width;
      const y = (i - x) / width;
      for (let dy = -1; dy <= 1; dy++) {
        const ny = y + dy;
        if (ny < 0 || ny >= height) continue;
        for (let dx = -1; dx <= 1; dx++) {
          const nx = x + dx;
          if (nx < 0 || nx >= width) continue;
          const j = ny * width + nx;
          if (mask[j] !== 0) continue;
          mask[j] = 2;
          next.push(j);
        }
      }
    }
    frontier = next;
  }

  // No 2s can survive: the final pass breaks before queueing anything it will not fill, and every
  // queued texel had a covered neighbour by construction, so none is ever skipped.
  return out;
}

function hasCoveredNeighbour(
  mask: Uint8Array,
  width: number,
  height: number,
  x: number,
  y: number
): boolean {
  for (let dy = -1; dy <= 1; dy++) {
    const ny = y + dy;
    if (ny < 0 || ny >= height) continue;
    for (let dx = -1; dx <= 1; dx++) {
      const nx = x + dx;
      if (nx < 0 || nx >= width || (dx === 0 && dy === 0)) continue;
      if (mask[ny * width + nx] === 1) return true;
    }
  }
  return false;
}

// ---------------------------------------------------------------------------------------------
// Inpaint — the interior-gap fix
// ---------------------------------------------------------------------------------------------

interface PyramidLevel {
  width: number;
  height: number;
  /** Normalised RGBA, linear (except alpha, which is always linear). */
  colour: Float32Array;
  /** Fractional coverage, 0..1. */
  weight: Float32Array;
}

export interface InpaintOptions {
  /**
   * Whether the pixels are sRGB-encoded and must be linearised before averaging. Default true.
   *
   * 🔴 Set false for normal maps, ORM/metallic-roughness and anything else with
   * `colorSpace = NoColorSpace`. Linearising a normal map's XY treats a vector component as a
   * brightness and bends every filled normal toward the surface.
   */
  srgb?: boolean;
}

/**
 * Pyramid (push-pull) inpaint: fills every uncovered texel by repeatedly halving the masked image
 * and reading back from the coarser level on the way up. That is what reaches the MIDDLE of an
 * occlusion hole, which dilation structurally cannot: dilation only ever advances one texel per
 * pass from a known-good edge, so a 60-texel hole would need 30 passes and would still meet itself
 * in the centre as a hard ridge. The pyramid solves the same hole in log2(size) levels and lands a
 * smooth gradient across it.
 *
 * ⚠️ It fills the far gutter too, not only interior holes — anything with `mask === 0`. That is
 * deliberate and it is the second half of the seam fix: dilation stops after N texels, and the
 * black beyond it still bleeds into the SMALL mips, where one texel covers a whole island. A fully
 * populated atlas has no black left to bleed.
 *
 * 🔴 `mask` is updated in place; on return every texel is marked covered.
 */
export function inpaintHoles(
  image: ImageData,
  mask: Uint8Array,
  options?: InpaintOptions
): ImageData {
  const { width, height } = image;
  if (mask.length !== width * height) {
    throw new Error(
      `Coverage mask is ${mask.length} texels but the texture is ${width * height}; they must match.`
    );
  }
  const srgb = options?.srgb ?? true;

  const out = new ImageData(new Uint8ClampedArray(image.data), width, height);
  const src = image.data;

  let anyCovered = false;
  let anyHole = false;
  for (let i = 0; i < mask.length; i++) {
    if (mask[i] === 1) anyCovered = true;
    else anyHole = true;
    if (anyCovered && anyHole) break;
  }
  if (!anyHole) return out;
  if (!anyCovered) {
    throw new Error(
      'No texel of this texture is covered by the mesh; there is nothing to fill holes from.'
    );
  }

  // Level 0 stays as the ImageData + mask rather than becoming float arrays: at 2048² a float
  // level 0 is 84 MB, and it would only ever be read twice. Levels 1..n together are a third of
  // that. Build level 1 straight from the bytes.
  const levels: PyramidLevel[] = [];
  levels.push(reduceFromBytes(src, mask, width, height, srgb));
  while (levels[levels.length - 1].width > 1 || levels[levels.length - 1].height > 1) {
    levels.push(reduceLevel(levels[levels.length - 1]));
  }

  // Pull: coarsest downward. Each level takes what it is missing from the level above, weighted by
  // how much real data it already had. By the time we reach level 1 every texel is defined.
  for (let l = levels.length - 2; l >= 0; l--) {
    const fine = levels[l];
    const coarse = levels[l + 1];
    const sample = new Float32Array(4);
    for (let y = 0; y < fine.height; y++) {
      for (let x = 0; x < fine.width; x++) {
        const i = y * fine.width + x;
        const w = fine.weight[i];
        if (w >= 1) continue;
        bilinearFromCoarse(coarse, x, y, sample);
        const p = i * 4;
        fine.colour[p] = sample[0] + (fine.colour[p] - sample[0]) * w;
        fine.colour[p + 1] = sample[1] + (fine.colour[p + 1] - sample[1]) * w;
        fine.colour[p + 2] = sample[2] + (fine.colour[p + 2] - sample[2]) * w;
        fine.colour[p + 3] = sample[3] + (fine.colour[p + 3] - sample[3]) * w;
        fine.weight[i] = 1;
      }
    }
  }

  // Final pull into level 0. Covered texels are never touched — they must come out BIT-IDENTICAL,
  // and a linear round trip would shift them by a code value or two for no reason.
  const level1 = levels[0];
  const sample = new Float32Array(4);
  const data = out.data;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const i = y * width + x;
      if (mask[i] === 1) continue;
      bilinearFromCoarse(level1, x, y, sample);
      const p = i * 4;
      if (srgb) {
        data[p] = encodeSrgb(sample[0]);
        data[p + 1] = encodeSrgb(sample[1]);
        data[p + 2] = encodeSrgb(sample[2]);
      } else {
        // ⚠️ No `+ 0.5` anywhere here. `Uint8ClampedArray` already rounds on store (half-to-even),
        // so adding a half rounds twice and shifts every filled texel up by a code value.
        data[p] = sample[0] * 255;
        data[p + 1] = sample[1] * 255;
        data[p + 2] = sample[2] * 255;
      }
      data[p + 3] = sample[3] * 255;
      mask[i] = 1;
    }
  }
  return out;
}

function reduceFromBytes(
  src: Uint8ClampedArray,
  mask: Uint8Array,
  width: number,
  height: number,
  srgb: boolean
): PyramidLevel {
  const lw = Math.max(1, Math.ceil(width / 2));
  const lh = Math.max(1, Math.ceil(height / 2));
  const colour = new Float32Array(lw * lh * 4);
  const weight = new Float32Array(lw * lh);

  for (let y = 0; y < lh; y++) {
    for (let x = 0; x < lw; x++) {
      let r = 0;
      let g = 0;
      let b = 0;
      let a = 0;
      let w = 0;
      let children = 0;
      for (let dy = 0; dy < 2; dy++) {
        const sy = y * 2 + dy;
        if (sy >= height) continue;
        for (let dx = 0; dx < 2; dx++) {
          const sx = x * 2 + dx;
          if (sx >= width) continue;
          children++;
          const j = sy * width + sx;
          if (mask[j] !== 1) continue;
          const p = j * 4;
          if (srgb) {
            r += SRGB_TO_LINEAR[src[p]];
            g += SRGB_TO_LINEAR[src[p + 1]];
            b += SRGB_TO_LINEAR[src[p + 2]];
          } else {
            r += src[p] / 255;
            g += src[p + 1] / 255;
            b += src[p + 2] / 255;
          }
          // ⚠️ Alpha is never sRGB-encoded, in glTF or anywhere else. Linearising it would make
          // every partially transparent texel more opaque.
          a += src[p + 3] / 255;
          w++;
        }
      }
      const i = y * lw + x;
      if (w > 0) {
        const p = i * 4;
        colour[p] = r / w;
        colour[p + 1] = g / w;
        colour[p + 2] = b / w;
        colour[p + 3] = a / w;
      }
      // Weight is the FRACTION of children with data, so it stays in 0..1 at every level and can be
      // used directly as the blend factor on the way back up.
      weight[i] = children > 0 ? w / children : 0;
    }
  }
  return { width: lw, height: lh, colour, weight };
}

function reduceLevel(fine: PyramidLevel): PyramidLevel {
  const lw = Math.max(1, Math.ceil(fine.width / 2));
  const lh = Math.max(1, Math.ceil(fine.height / 2));
  const colour = new Float32Array(lw * lh * 4);
  const weight = new Float32Array(lw * lh);

  for (let y = 0; y < lh; y++) {
    for (let x = 0; x < lw; x++) {
      let r = 0;
      let g = 0;
      let b = 0;
      let a = 0;
      let w = 0;
      let children = 0;
      for (let dy = 0; dy < 2; dy++) {
        const sy = y * 2 + dy;
        if (sy >= fine.height) continue;
        for (let dx = 0; dx < 2; dx++) {
          const sx = x * 2 + dx;
          if (sx >= fine.width) continue;
          children++;
          const j = sy * fine.width + sx;
          const cw = fine.weight[j];
          if (cw <= 0) continue;
          const p = j * 4;
          r += fine.colour[p] * cw;
          g += fine.colour[p + 1] * cw;
          b += fine.colour[p + 2] * cw;
          a += fine.colour[p + 3] * cw;
          w += cw;
        }
      }
      const i = y * lw + x;
      if (w > 0) {
        const p = i * 4;
        colour[p] = r / w;
        colour[p + 1] = g / w;
        colour[p + 2] = b / w;
        colour[p + 3] = a / w;
      }
      weight[i] = children > 0 ? w / children : 0;
    }
  }
  return { width: lw, height: lh, colour, weight };
}

/**
 * Bilinear tap into the next-coarser level, for a texel at (x, y) of the finer one.
 *
 * ⚠️ Nearest sampling here is what makes push-pull look like blocky JPEG in the middle of a large
 * hole — the coarse level's texels show through as squares. The half-texel offsets below are the
 * usual centre alignment; with odd dimensions the parent is `ceil(n/2)` and the mapping is off by
 * up to half a coarse texel, which is invisible in a fill and not worth a per-level correction.
 */
function bilinearFromCoarse(coarse: PyramidLevel, x: number, y: number, out: Float32Array): void {
  const u = (x + 0.5) * 0.5 - 0.5;
  const v = (y + 0.5) * 0.5 - 0.5;
  let x0 = Math.floor(u);
  let y0 = Math.floor(v);
  const fx = u - x0;
  const fy = v - y0;
  let x1 = x0 + 1;
  let y1 = y0 + 1;
  const maxX = coarse.width - 1;
  const maxY = coarse.height - 1;
  x0 = x0 < 0 ? 0 : x0 > maxX ? maxX : x0;
  x1 = x1 < 0 ? 0 : x1 > maxX ? maxX : x1;
  y0 = y0 < 0 ? 0 : y0 > maxY ? maxY : y0;
  y1 = y1 < 0 ? 0 : y1 > maxY ? maxY : y1;

  const p00 = (y0 * coarse.width + x0) * 4;
  const p10 = (y0 * coarse.width + x1) * 4;
  const p01 = (y1 * coarse.width + x0) * 4;
  const p11 = (y1 * coarse.width + x1) * 4;
  const c = coarse.colour;
  for (let k = 0; k < 4; k++) {
    const top = c[p00 + k] + (c[p10 + k] - c[p00 + k]) * fx;
    const bottom = c[p01 + k] + (c[p11 + k] - c[p01 + k]) * fx;
    out[k] = top + (bottom - top) * fy;
  }
}

// ---------------------------------------------------------------------------------------------
// Colour grade
// ---------------------------------------------------------------------------------------------

export interface ColourAdjust {
  /**
   * Photographic stops, applied in LINEAR space: +1 doubles the light. This is why it is not the
   * same control as CSS `brightness()`, which scales the encoded value.
   */
  exposure?: number;
  /** 1 = unchanged. Pivots at mid-grey (linear {@link CONTRAST_PIVOT}, i.e. sRGB 0.5). */
  contrast?: number;
  /** 1 = unchanged, 0 = greyscale, >1 pushes away from Rec.709 luma. */
  saturation?: number;
  /** White balance as a per-channel gain: +1 warm (more red, less blue), −1 cool. */
  temperature?: number;
}

/**
 * ⚠️ Contrast and saturation are identity at **1**, not 0. A caller that initialises its slider
 * state to all-zeroes is asking for zero contrast and zero saturation — a flat grey — which is
 * exactly what it will get, from the commit and from the GPU preview alike.
 */
export const IDENTITY_ADJUST: Required<ColourAdjust> = {
  exposure: 0,
  contrast: 1,
  saturation: 1,
  temperature: 0
};

/**
 * Fill in the identity defaults and reject non-numbers.
 *
 * Exported so the GPU preview in `live-grade.ts` defaults and validates a `ColourAdjust` through
 * the same code the commit does — a preview that read `undefined` as 0 where the bake reads it as
 * 1 would disagree with the bake on every untouched slider.
 */
export function resolveAdjust(adjust: ColourAdjust): Required<ColourAdjust> {
  const resolved = {
    exposure: adjust.exposure ?? IDENTITY_ADJUST.exposure,
    contrast: adjust.contrast ?? IDENTITY_ADJUST.contrast,
    saturation: adjust.saturation ?? IDENTITY_ADJUST.saturation,
    temperature: adjust.temperature ?? IDENTITY_ADJUST.temperature
  };
  for (const [key, value] of Object.entries(resolved)) {
    if (!Number.isFinite(value)) throw new Error(`Colour adjustment "${key}" is not a number.`);
  }
  return resolved;
}

/**
 * The exact grade, for commit: decode sRGB → operate in linear → re-encode.
 *
 * ⚠️ The order is exposure → white balance → contrast → saturation, and it is not commutative.
 * That order is the photographic one — gain first, because it models the light reaching the sensor;
 * tone and colour shaping after, because they model the print. Reversing it makes the temperature
 * control interact with the contrast pivot and stop behaving like white balance.
 *
 * Alpha is passed through untouched; grading it would turn a transparency map into a fade.
 *
 * 🔴 Assumes the image is sRGB-encoded — it is a colour grade, so that is the only sensible input.
 * Never run it on a normal map.
 */
export function adjustColour(image: ImageData, adjust: ColourAdjust): ImageData {
  const { exposure, contrast, saturation, temperature } = resolveAdjust(adjust);
  const out = new ImageData(new Uint8ClampedArray(image.data), image.width, image.height);
  if (exposure === 0 && contrast === 1 && saturation === 1 && temperature === 0) return out;

  const gain = Math.pow(2, exposure);
  const gainR = gain * (1 + temperature * TEMPERATURE_GAIN);
  const gainG = gain;
  const gainB = gain * (1 - temperature * TEMPERATURE_GAIN);
  const data = out.data;

  for (let p = 0; p < data.length; p += 4) {
    let r = SRGB_TO_LINEAR[data[p]] * gainR;
    let g = SRGB_TO_LINEAR[data[p + 1]] * gainG;
    let b = SRGB_TO_LINEAR[data[p + 2]] * gainB;

    if (contrast !== 1) {
      r = (r - CONTRAST_PIVOT) * contrast + CONTRAST_PIVOT;
      g = (g - CONTRAST_PIVOT) * contrast + CONTRAST_PIVOT;
      b = (b - CONTRAST_PIVOT) * contrast + CONTRAST_PIVOT;
    }
    if (saturation !== 1) {
      const luma = r * LUMA_R + g * LUMA_G + b * LUMA_B;
      r = luma + (r - luma) * saturation;
      g = luma + (g - luma) * saturation;
      b = luma + (b - luma) * saturation;
    }

    data[p] = encodeSrgb(r);
    data[p + 1] = encodeSrgb(g);
    data[p + 2] = encodeSrgb(b);
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Normal map strength
// ---------------------------------------------------------------------------------------------

/**
 * Lerp every texel toward flat, i.e. weaken the baked normal detail. 1 = unchanged, 0 = perfectly
 * flat, >1 exaggerates.
 *
 * 🔴 Reach for `material.normalScale` FIRST. three's normal-map chunk does `mapN.xy *= normalScale`
 * then normalises, which is the same operation for free, non-destructively, at full precision, with
 * a live slider. This function exists only for the case where the weakening must be BAKED INTO THE
 * FILE — a viewer that ignores `normalScale`, or an export whose consumer only reads the image.
 *
 * ⚠️ Flat is (128, 128, 255) by convention but the exact encoding of zero is 127.5. Using 128 as
 * the pivot biases every normal by half a code value toward +X/+Y, which shows up as a faint
 * diagonal lighting tilt on a fully "flattened" map. Alpha is untouched; some pipelines pack data
 * there.
 */
export function setNormalStrength(image: ImageData, strength: number): ImageData {
  if (!Number.isFinite(strength)) throw new Error('Normal map strength is not a number.');
  const out = new ImageData(new Uint8ClampedArray(image.data), image.width, image.height);
  if (strength === 1) return out;

  const data = out.data;
  for (let p = 0; p < data.length; p += 4) {
    // Uint8ClampedArray rounds and clamps on store, so strength > 1 cannot produce a wrapped value.
    data[p] = 127.5 + (data[p] - 127.5) * strength;
    data[p + 1] = 127.5 + (data[p + 1] - 127.5) * strength;
    // Z stays on the +Z side of the hemisphere: lerp toward 255, never toward 127.5.
    data[p + 2] = 255 + (data[p + 2] - 255) * strength;
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Encoding for the file
// ---------------------------------------------------------------------------------------------

export interface EncodeOptions {
  /** Lossy-format quality, 0..1. Ignored for PNG. */
  quality?: number;
}

const ENCODABLE = new Set(['image/png', 'image/jpeg', 'image/webp']);
const mimeSupport = new Map<string, boolean>();

/**
 * ⚠️ Probe on a 1×1 canvas. The standard feature test is `toDataURL(mime).startsWith('data:' + mime)`
 * — on the real canvas that base64-encodes the entire 2048² image just to read its first 20
 * characters.
 */
function canEncode(mimeType: string): boolean {
  const cached = mimeSupport.get(mimeType);
  if (cached !== undefined) return cached;
  const probe = createCanvas(1, 1);
  const supported = probe.toDataURL(mimeType).startsWith(`data:${mimeType}`);
  mimeSupport.set(mimeType, supported);
  return supported;
}

function hasTransparency(image: ImageData): boolean {
  const data = image.data;
  for (let p = 3; p < data.length; p += 4) {
    if (data[p] !== 255) return true;
  }
  return false;
}

/**
 * Encode for writing back into the GLB. Returns the bytes and the mime actually used, which may
 * not be the one asked for — the caller must write the returned one into the image def or the file
 * claims a format it does not contain.
 *
 * Three reasons it substitutes:
 * - 🔴 The source was `image/ktx2` or another GPU format. A canvas cannot produce those, so the
 *   texture comes back as PNG and the file grows a lot. Surface that to the user.
 * - 🔴 The source was JPEG but the repaired image has transparency. JPEG has no alpha channel and
 *   would silently composite it onto black — every soft edge turns into a hard dark fringe.
 * - The browser does not support the format (WebP encode on older Safari).
 *
 * ⚠️ Re-encoding to JPEG is generational loss on every save. Nothing here can avoid that once the
 * pixels have changed; the point of the repair is that it happens once.
 */
export async function encodeTexture(
  image: ImageData,
  mimeType: string,
  options?: EncodeOptions
): Promise<{ bytes: Uint8Array; mimeType: string }> {
  let target = ENCODABLE.has(mimeType) && canEncode(mimeType) ? mimeType : 'image/png';
  if (target === 'image/jpeg' && hasTransparency(image)) target = 'image/png';

  const canvas = createCanvas(image.width, image.height);
  get2d(canvas).putImageData(image, 0, 0);

  const blob = await new Promise<Blob | null>((resolve) => {
    canvas.toBlob(resolve, target, options?.quality ?? 0.92);
  });
  if (!blob)
    throw new Error(
      `The browser could not encode this ${image.width}×${image.height} texture as ${target}.`
    );

  return { bytes: new Uint8Array(await blob.arrayBuffer()), mimeType: target };
}

// ---------------------------------------------------------------------------------------------
// The one-button repair
// ---------------------------------------------------------------------------------------------

export interface RepairReport {
  /** Texels the mesh's UVs actually land on. */
  coveredTexels: number;
  totalTexels: number;
  /** Texels dilation reached: the near gutter, plus any interior gap narrow enough to close. */
  dilatedTexels: number;
  /** Texels the pyramid filled: the rest of the gutter, plus the interiors dilation could not reach. */
  inpaintedTexels: number;
}

export interface RepairOptions {
  /** Dilation width in texels. Default ~1 per 512 of atlas edge, minimum 2. */
  dilate?: number;
  /**
   * Treat fully transparent texels INSIDE the UV coverage as interior projection gaps. Default true.
   *
   * ⚠️ This is not the alpha-coverage mistake the module warns about. That mistake is inferring the
   * GUTTER from alpha, which fails because bakers fill gutters with opaque black. Inside a covered
   * island, alpha 0 is a positive statement that the projector wrote nothing there — and it cannot
   * be confused with dark content, because dark content is opaque. The original alpha is restored
   * afterwards, so a genuinely transparent material keeps its transparency and only gains bled
   * colour underneath it, which is what you want at a soft edge anyway.
   */
  interiorAlphaGaps?: boolean;
}

const ALPHA_GAP_THRESHOLD = 8;

/**
 * Coverage mask → dilate → inpaint, applied to one texture in place. No user input, no options
 * that need explaining, and a report of what it actually changed so the UI can say something true
 * instead of "Done!".
 *
 * Call it once per map. `texture.colorSpace` decides whether the maths runs in linear or raw, so
 * the same call is correct for a base-colour map and for a normal map.
 *
 * ⚠️ If the mesh uses multiple materials, this marks every material group's islands as covered,
 * because a single override material draws the whole geometry. When the groups share one atlas that
 * is exactly right; when they do not, the other material's islands are treated as content to
 * preserve rather than gutter to fill — harmless, but the seam fix will not reach across.
 *
 * ⚠️ Skinning, morph targets and world transform are all irrelevant here: the pass reads only `uv`.
 * A `SkinnedMesh` can be passed straight in.
 *
 * 🔴 KNOWN LIMIT, and the one thing to check against a real asset before promising it in the UI: a
 * UV coverage mask answers "does the mesh sample this texel", not "did the projector write anything
 * useful here". An occlusion gap in the MIDDLE of an island is covered by the mesh, so the mask
 * calls it content. `interiorAlphaGaps` catches the case where the baker left those texels
 * transparent — but a baker that writes OPAQUE black into them leaves nothing to distinguish the
 * gap from dark hair, and this function will preserve it. Detecting that needs a content heuristic,
 * and every content heuristic that finds black holes also finds pupils. It is deliberately not here.
 */
export function repairAtlas(
  renderer: THREE.WebGLRenderer,
  mesh: THREE.Mesh,
  texture: THREE.Texture,
  options?: RepairOptions
): RepairReport {
  const image = textureToImageData(texture);
  const { width, height } = image;
  const totalTexels = width * height;

  const mask = buildCoverageMaskRect(renderer, mesh, width, height);
  let coveredTexels = 0;
  for (let i = 0; i < mask.length; i++) coveredTexels += mask[i];
  if (coveredTexels === 0) {
    // Either the UVs are degenerate or the readback failed; both are fatal and neither is something
    // the user can fix by clicking again, so say so rather than writing a black texture.
    throw new Error(
      'The mesh UVs do not cover any part of this texture, so there is nothing to repair.'
    );
  }

  // Interior projection gaps: clear their mask bits so dilate/inpaint treat them as holes, and
  // remember them so their alpha can be put back untouched. See `interiorAlphaGaps` above.
  const alphaGaps = new IndexQueue(1 << 12);
  if (options?.interiorAlphaGaps ?? true) {
    const data = image.data;
    for (let i = 0; i < mask.length; i++) {
      if (mask[i] === 1 && data[i * 4 + 3] < ALPHA_GAP_THRESHOLD) {
        mask[i] = 0;
        alphaGaps.push(i);
      }
    }
  }

  const dilate = options?.dilate ?? Math.max(2, Math.round(Math.max(width, height) / 512));
  const beforeDilate = countCovered(mask);
  const dilated = dilateIslands(image, mask, dilate);
  const afterDilate = countCovered(mask);

  const inpaintedTexels = totalTexels - afterDilate;
  const repaired = inpaintHoles(dilated, mask, {
    // 🔴 The colour space decides whether values may be averaged as-is. Base colour is sRGB and
    // must be linearised; a normal map is NoColorSpace and must not be.
    srgb: texture.colorSpace === THREE.SRGBColorSpace
  });

  // Restore alpha where we only ever wanted the colour underneath it.
  for (let n = 0; n < alphaGaps.length; n++) {
    const p = alphaGaps.data[n] * 4 + 3;
    repaired.data[p] = image.data[p];
  }

  applyToTexture(texture, repaired);

  return {
    coveredTexels,
    totalTexels,
    dilatedTexels: afterDilate - beforeDilate,
    inpaintedTexels
  };
}

function countCovered(mask: Uint8Array): number {
  let n = 0;
  for (let i = 0; i < mask.length; i++) n += mask[i] === 1 ? 1 : 0;
  return n;
}

// ---------------------------------------------------------------------------------------------
// Reading the UV layout — the diagnostic half
//
// Everything below this line is READ-ONLY. Nothing here touches a texture, a geometry or a
// material; it answers "what does this atlas look like and what is wrong with it", which is the
// question nobody could ask before because a 2048² atlas has no UI.
//
// 🔴 The split between CPU and GPU is deliberate and is the whole design:
//
//   - **The UV attribute answers everything about the LAYOUT.** Bounds, per-triangle area,
//     degenerate triangles, which triangles form one island, where an island sits in UV space,
//     what is under the cursor. All of it is O(triangles) over an attribute that is already in
//     RAM — a few hundred thousand iterations, no readback, no GPU state to save and restore.
//
//   - **Only a rasteriser answers TEXELS.** "How much of the atlas does the mesh actually reach"
//     and "do two islands land on the same texel" are questions about the fill rule, and the fill
//     rule is the GPU's. [`buildCoverageCountRect`] is the one pass that has to happen, and it is
//     one pass for both answers.
//
// ⚠️ Do not be tempted to answer overlap from UV bounding boxes on the CPU. Automatic atlases pack
// islands so their boxes interlock constantly; a box test reports overlap on a perfectly good
// layout and is worse than no check at all.
// ---------------------------------------------------------------------------------------------

/** One connected chart of the UV layout. */
export interface UvIsland {
  /** Index into {@link UvLayout.islands}; also the value stored in `faceIsland`. */
  id: number;
  /** Triangles in the island. */
  faces: number;
  /** Welded UV vertices in the island. */
  vertices: number;
  minU: number;
  minV: number;
  maxU: number;
  maxV: number;
  /** Σ|signed UV area| of its triangles, in UV units² — 1.0 would be the whole atlas. */
  area: number;
  /**
   * The island's lowest face index. A stable representative to point the 3D view at: the caller
   * hands it back to whatever maps a faceIndex to a place on the character.
   */
  face: number;
  /** Zero-area UV triangles inside it. */
  degenerate: number;
  /** True when any corner of any of its triangles falls outside `[0,1]`. */
  outside: boolean;
}

export interface UvLayout {
  faceCount: number;
  /** Vertices in the geometry, not in the atlas — seam splits mean this exceeds the welded count. */
  vertexCount: number;
  /** Sorted by area, largest first. `islands[i].id === i`. */
  islands: UvIsland[];
  /** Island id per face, indexed by faceIndex — what a raycast hit maps through. */
  faceIsland: Int32Array;
  /**
   * |UV area| per face, in UV units². Kept because "smaller than one texel" is a texture-size
   * question the layout cannot answer on its own; see {@link countSubTexelFaces}.
   */
  faceArea: Float32Array;
  /**
   * Weld root per vertex: two vertices carrying the same UV to within the tolerance share one.
   *
   * 🔴 THE WELD ROOT, NOT THE ISLAND ROOT. It is what makes an edge identifiable across the two
   * triangles that share it — [`uvIslandOutline`] keys its edge map on it — and if it ever became
   * the island id every edge inside an island would collapse onto one key and the outline would
   * come back empty. Island membership is `faceIsland`.
   *
   * 🔴 And the weld is why islands come out right at all. A GLB from this pipeline is seam-split:
   * the exporter duplicates a vertex at every UV seam, every hard normal crease and every
   * skin-influence boundary, so two triangles sitting side by side IN THE SAME CHART can share no
   * index value. Walking the raw index buffer reports each crease as a chart of its own.
   * `mesh-repair`'s [`labelComponents`] welds by POSITION for the same reason; this welds by UV,
   * which is the correct equivalence for charts — a position weld would fuse the two sides of a
   * seam and report one island for the whole body.
   */
  vertexRoot: Int32Array;
  minU: number;
  minV: number;
  maxU: number;
  maxV: number;
  /** Triangles with a corner outside `[0,1]`. */
  outsideFaces: number;
  /** Face indices of the zero-area triangles, capped at `maxDegenerateListed`. */
  degenerateFaces: number[];
  /** How many there really are, whether or not they were all listed. */
  degenerateCount: number;
  /**
   * Σ of `faceArea`. Compare it with measured coverage: much larger means islands overlap or the
   * UVs tile; much smaller means the atlas is mostly gutter.
   */
  uvArea: number;
}

export interface UvLayoutOptions {
  /**
   * UV-space grid the vertex weld quantises to. Default `1e-6` — a fiftieth of a texel on a 2048
   * map, so it merges the exporter's bit-identical duplicates and nothing else.
   *
   * ⚠️ Raising this to "help" merges genuinely separate charts that happen to be packed close
   * together, and the panel then claims the character has three islands.
   */
  weldTolerance?: number;
  /** How many degenerate faces to list individually. Default 64; the count is always exact. */
  maxDegenerateListed?: number;
  /** How far outside `[0,1]` a coordinate must sit to count as out of range. Default `1e-6`. */
  unitEpsilon?: number;
  /**
   * |UV area| at or below which a triangle is called degenerate. Default `1e-12`, i.e. zero within
   * float noise rather than merely small.
   *
   * A degenerate UV triangle is a real defect in a projection atlas and not a cosmetic one: all
   * three corners sample the same texel, so one texel's colour is smeared flat across a whole face.
   */
  degenerateArea?: number;
}

/** Read a UV attribute out of whatever container it arrived in, as a flat `[u,v,…]` array. */
function readUvs(geometry: THREE.BufferGeometry): Float32Array {
  const attribute = geometry.getAttribute('uv');
  if (!attribute) {
    throw new Error('This mesh has no UV coordinates, so it has no atlas layout to show.');
  }
  // ⚠️ Through `getX`/`getY` rather than `.array`: a GLB may deliver UVs interleaved with the
  // other vertex attributes, in which case `.array` is the whole interleaved buffer and a stride
  // of 2 reads garbage. The accessors cost a call per component and are always right.
  const count = attribute.count;
  const out = new Float32Array(count * 2);
  for (let i = 0; i < count; i++) {
    out[i * 2] = attribute.getX(i);
    out[i * 2 + 1] = attribute.getY(i);
  }
  return out;
}

/**
 * Face corner indices, whether or not the geometry is indexed.
 *
 * A non-indexed geometry has an implicit index `0,1,2,…`, and returning that rather than refusing
 * keeps every caller below free of the distinction.
 */
function readFaceIndex(geometry: THREE.BufferGeometry, vertexCount: number): Uint32Array {
  const index = geometry.getIndex();
  if (!index) {
    const implicit = new Uint32Array(vertexCount - (vertexCount % 3));
    for (let i = 0; i < implicit.length; i++) implicit[i] = i;
    return implicit;
  }
  const out = new Uint32Array(index.count - (index.count % 3));
  for (let i = 0; i < out.length; i++) {
    const v = index.getX(i);
    if (v >= vertexCount) {
      throw new Error(
        `Index buffer references vertex ${v} but the mesh only has ${vertexCount}. The file is corrupt.`
      );
    }
    out[i] = v;
  }
  return out;
}

/** Union-find over vertices, flattening as it goes. Local to this section on purpose — the one in
 *  `mesh-repair` is not exported and this file must not grow a dependency on it. */
function uvRoot(parent: Int32Array, start: number): number {
  let root = start;
  while (parent[root] !== root) root = parent[root];
  let walk = start;
  while (parent[walk] !== root) {
    const next = parent[walk];
    parent[walk] = root;
    walk = next;
  }
  return root;
}

function uvUnite(parent: Int32Array, rank: Uint8Array, a: number, b: number): void {
  const ra = uvRoot(parent, a);
  const rb = uvRoot(parent, b);
  if (ra === rb) return;
  if (rank[ra] < rank[rb]) parent[ra] = rb;
  else if (rank[rb] < rank[ra]) parent[rb] = ra;
  else {
    parent[rb] = ra;
    rank[ra]++;
  }
}

/**
 * Everything the UV attribute alone can tell you about the atlas: islands, bounds, areas, and the
 * two defects that are visible from the layout — out-of-range coordinates and zero-area triangles.
 *
 * One pass, on the CPU, no GPU state touched. At ~200k triangles this is tens of milliseconds; it
 * is still worth deferring a frame behind progress text, because it is not zero.
 */
export function analyseUvLayout(
  geometry: THREE.BufferGeometry,
  options: UvLayoutOptions = {}
): UvLayout {
  const uvs = readUvs(geometry);
  const vertexCount = uvs.length / 2;
  const idx = readFaceIndex(geometry, vertexCount);
  const faceCount = idx.length / 3;
  const tolerance = Math.max(options.weldTolerance ?? 1e-6, Number.MIN_VALUE);
  const unitEpsilon = options.unitEpsilon ?? 1e-6;
  const degenerateArea = options.degenerateArea ?? 1e-12;
  const maxDegenerateListed = options.maxDegenerateListed ?? 64;

  // ── weld by UV, then grow islands over the welded vertices ──────────────────────────────────
  //
  // TWO partitions, deliberately. The weld says "these two vertices are the same point of the
  // atlas"; the island says "these two points are the same chart". They are built from the same
  // union-find code but must stay separate arrays — see the note on `vertexRoot`.
  const weldParent = new Int32Array(vertexCount);
  const islandParent = new Int32Array(vertexCount);
  for (let v = 0; v < vertexCount; v++) {
    weldParent[v] = v;
    islandParent[v] = v;
  }
  const weldRank = new Uint8Array(vertexCount);
  const islandRank = new Uint8Array(vertexCount);

  // Two-level map keyed by the quantised u then the quantised v. Packing both into one number
  // would overflow `Number.MAX_SAFE_INTEGER` for the out-of-range UVs this function exists to
  // find, and a string key allocates once per vertex.
  const byU = new Map<number, Map<number, number>>();
  const inverse = 1 / tolerance;
  for (let v = 0; v < vertexCount; v++) {
    const qu = Math.round(uvs[v * 2] * inverse);
    const qv = Math.round(uvs[v * 2 + 1] * inverse);
    let row = byU.get(qu);
    if (!row) {
      row = new Map<number, number>();
      byU.set(qu, row);
    }
    const first = row.get(qv);
    if (first === undefined) row.set(qv, v);
    else {
      uvUnite(weldParent, weldRank, v, first);
      uvUnite(islandParent, islandRank, v, first);
    }
  }
  byU.clear();

  for (let f = 0; f < faceCount; f++) {
    const a = idx[f * 3];
    const b = idx[f * 3 + 1];
    const c = idx[f * 3 + 2];
    uvUnite(islandParent, islandRank, a, b);
    uvUnite(islandParent, islandRank, b, c);
  }

  const vertexRoot = new Int32Array(vertexCount);
  const islandRoot = new Int32Array(vertexCount);
  for (let v = 0; v < vertexCount; v++) {
    vertexRoot[v] = uvRoot(weldParent, v);
    islandRoot[v] = uvRoot(islandParent, v);
  }

  // ── walk the faces once, accumulating per island ────────────────────────────────────────────
  const faceIsland = new Int32Array(faceCount).fill(-1);
  const faceArea = new Float32Array(faceCount);
  const degenerateFaces: number[] = [];
  const rootToIsland = new Map<number, number>();
  const raw: UvIsland[] = [];
  let minU = Infinity;
  let minV = Infinity;
  let maxU = -Infinity;
  let maxV = -Infinity;
  let outsideFaces = 0;
  let degenerateCount = 0;
  let uvArea = 0;

  const low = -unitEpsilon;
  const high = 1 + unitEpsilon;

  for (let f = 0; f < faceCount; f++) {
    const a = idx[f * 3];
    const b = idx[f * 3 + 1];
    const c = idx[f * 3 + 2];
    const au = uvs[a * 2];
    const av = uvs[a * 2 + 1];
    const bu = uvs[b * 2];
    const bv = uvs[b * 2 + 1];
    const cu = uvs[c * 2];
    const cv = uvs[c * 2 + 1];

    // Twice the signed area; the factor of 2 is divided out once, below.
    const area = Math.abs((bu - au) * (cv - av) - (cu - au) * (bv - av)) * 0.5;
    faceArea[f] = area;
    uvArea += area;
    const degenerate = area <= degenerateArea;
    if (degenerate) {
      degenerateCount++;
      if (degenerateFaces.length < maxDegenerateListed) degenerateFaces.push(f);
    }

    const fMinU = Math.min(au, bu, cu);
    const fMaxU = Math.max(au, bu, cu);
    const fMinV = Math.min(av, bv, cv);
    const fMaxV = Math.max(av, bv, cv);
    if (fMinU < minU) minU = fMinU;
    if (fMaxU > maxU) maxU = fMaxU;
    if (fMinV < minV) minV = fMinV;
    if (fMaxV > maxV) maxV = fMaxV;
    const outside = fMinU < low || fMinV < low || fMaxU > high || fMaxV > high;
    if (outside) outsideFaces++;

    const root = islandRoot[a];
    let id = rootToIsland.get(root);
    if (id === undefined) {
      id = raw.length;
      rootToIsland.set(root, id);
      raw.push({
        id,
        faces: 0,
        vertices: 0,
        minU: Infinity,
        minV: Infinity,
        maxU: -Infinity,
        maxV: -Infinity,
        area: 0,
        face: f,
        degenerate: 0,
        outside: false
      });
    }
    const island = raw[id];
    island.faces++;
    island.area += area;
    if (fMinU < island.minU) island.minU = fMinU;
    if (fMaxU > island.maxU) island.maxU = fMaxU;
    if (fMinV < island.minV) island.minV = fMinV;
    if (fMaxV > island.maxV) island.maxV = fMaxV;
    if (degenerate) island.degenerate++;
    if (outside) island.outside = true;
    faceIsland[f] = id;
  }

  // Welded vertex counts: one per WELD group, counted through its island. Counting raw vertices
  // instead would report the seam splits as extra points of the atlas, which they are not — two
  // vertices at the same UV are one texel's worth of chart. An island id of `undefined` is a weld
  // group the index buffer never references: dead weight in the buffer, not a chart.
  for (let v = 0; v < vertexCount; v++) {
    if (vertexRoot[v] !== v) continue;
    const id = rootToIsland.get(islandRoot[v]);
    if (id !== undefined) raw[id].vertices++;
  }

  // Biggest first: the interesting island is almost always the largest, and a stable order lets
  // the UI number them in a way that survives a re-measure.
  const islands = raw.slice().sort((x, y) => y.area - x.area || x.face - y.face);
  const remap = new Int32Array(raw.length);
  for (let i = 0; i < islands.length; i++) {
    remap[islands[i].id] = i;
    islands[i].id = i;
  }
  for (let f = 0; f < faceCount; f++) faceIsland[f] = remap[faceIsland[f]];

  return {
    faceCount,
    vertexCount,
    islands,
    faceIsland,
    faceArea,
    vertexRoot,
    minU: faceCount > 0 ? minU : 0,
    minV: faceCount > 0 ? minV : 0,
    maxU: faceCount > 0 ? maxU : 0,
    maxV: faceCount > 0 ? maxV : 0,
    outsideFaces,
    degenerateFaces,
    degenerateCount,
    uvArea
  };
}

/**
 * Triangles whose UV footprint is smaller than one texel of a `width × height` map.
 *
 * Not the same defect as a degenerate triangle and not always a defect at all — a dense mesh on a
 * small map has plenty of them, and they simply sample one texel. It becomes a defect when the
 * count is a large fraction of the mesh, which says the atlas is too small for the geometry.
 */
export function countSubTexelFaces(layout: UvLayout, width: number, height: number): number {
  const texelArea = 1 / (width * height);
  let n = 0;
  for (let f = 0; f < layout.faceArea.length; f++) {
    if (layout.faceArea[f] < texelArea) n++;
  }
  return n;
}

/** Every face of one island, as the set `ModelStage.previewFaces` wants. */
export function islandFaces(layout: UvLayout, islandId: number): Set<number> {
  const faces = new Set<number>();
  if (islandId < 0) return faces;
  for (let f = 0; f < layout.faceIsland.length; f++) {
    if (layout.faceIsland[f] === islandId) faces.add(f);
  }
  return faces;
}

// ---------------------------------------------------------------------------------------------
// Island outlines
// ---------------------------------------------------------------------------------------------

/** Boundary edges of every island, as flat `[u0,v0,u1,v1]` quads plus the island each belongs to. */
export interface UvOutline {
  /** 4 numbers per edge. */
  segments: Float32Array;
  /** Island id per edge; parallel to `segments / 4`. */
  island: Int32Array;
  count: number;
}

/**
 * The edges that bound each chart: those used by exactly ONE triangle once vertices are welded by
 * UV. Drawing these instead of the full wireframe is what makes a 200k-triangle atlas legible —
 * every triangle edge is a black mess at any display size, the chart outlines are the map.
 *
 * ⚠️ A UV SEAM produces a boundary edge on BOTH sides, which is correct and is the point: the two
 * triangles either side of a seam carry different UVs, so neither shares an edge with the other in
 * atlas space. Mesh holes also show up here; from the atlas they are indistinguishable from a seam,
 * and `mesh-repair` is where that distinction lives.
 */
export function uvIslandOutline(geometry: THREE.BufferGeometry, layout: UvLayout): UvOutline {
  const uvs = readUvs(geometry);
  const idx = readFaceIndex(geometry, layout.vertexCount);
  const faceCount = idx.length / 3;
  const root = layout.vertexRoot;
  const vertexCount = layout.vertexCount;

  // At most 3 distinct edges per face, so the parallel arrays can be sized once. The key packs
  // two welded vertex ids, both < vertexCount, so it stays inside `Number.MAX_SAFE_INTEGER` for
  // any mesh with fewer than ~94 M vertices.
  const maxEdges = faceCount * 3;
  const seen = new Map<number, number>();
  const cornerA = new Int32Array(maxEdges);
  const cornerB = new Int32Array(maxEdges);
  const uses = new Int32Array(maxEdges);
  const island = new Int32Array(maxEdges);
  let edges = 0;

  for (let f = 0; f < faceCount; f++) {
    const a = idx[f * 3];
    const b = idx[f * 3 + 1];
    const c = idx[f * 3 + 2];
    const corners = [a, b, c, a];
    for (let e = 0; e < 3; e++) {
      const v0 = corners[e];
      const v1 = corners[e + 1];
      const r0 = root[v0];
      const r1 = root[v1];
      const lo = r0 < r1 ? r0 : r1;
      const hi = r0 < r1 ? r1 : r0;
      const key = lo * vertexCount + hi;
      const slot = seen.get(key);
      if (slot === undefined) {
        seen.set(key, edges);
        cornerA[edges] = v0;
        cornerB[edges] = v1;
        uses[edges] = 1;
        island[edges] = layout.faceIsland[f];
        edges++;
      } else {
        uses[slot]++;
      }
    }
  }

  let boundary = 0;
  for (let e = 0; e < edges; e++) if (uses[e] === 1) boundary++;

  const segments = new Float32Array(boundary * 4);
  const owner = new Int32Array(boundary);
  let out = 0;
  for (let e = 0; e < edges; e++) {
    if (uses[e] !== 1) continue;
    const a = cornerA[e];
    const b = cornerB[e];
    segments[out * 4] = uvs[a * 2];
    segments[out * 4 + 1] = uvs[a * 2 + 1];
    segments[out * 4 + 2] = uvs[b * 2];
    segments[out * 4 + 3] = uvs[b * 2 + 1];
    owner[out] = island[e];
    out++;
  }
  return { segments, island: owner, count: boundary };
}

// ---------------------------------------------------------------------------------------------
// Hit testing UV space
// ---------------------------------------------------------------------------------------------

/**
 * A uniform grid over `[0,1]²` holding the triangle ids whose UV box touches each cell, plus the
 * triangles' corner UVs so a query allocates nothing.
 *
 * 🔴 This is the CPU answer to "what is under the cursor", and it exists so that hovering does not
 * touch the GPU. Rasterising an island-id map would answer the same question per texel — at 4.19 M
 * texels, one readback and 16 MB per interaction. This is ~4 triangles tested per hover.
 */
export interface UvHitGrid {
  /** Cells per axis. */
  cells: number;
  /** CSR offsets into `items`, length `cells * cells + 1`. */
  start: Int32Array;
  /** Face indices, bucketed. */
  items: Int32Array;
  /**
   * Faces whose UV box covers so much of the grid that bucketing them is pointless — a stray
   * triangle with a UV in the thousands would otherwise be inserted into every cell. Tested on
   * every query instead.
   */
  oversize: Int32Array;
  /** 6 floats per face: `u0,v0,u1,v1,u2,v2`. */
  corners: Float32Array;
  faceCount: number;
}

export interface UvHitGridOptions {
  /** Cells per axis. Default scales with the triangle count, clamped to 16…256. */
  cells?: number;
  /** A face touching more than this fraction of the grid goes in `oversize`. Default 1/16. */
  oversizeFraction?: number;
}

export function buildUvHitGrid(
  geometry: THREE.BufferGeometry,
  options: UvHitGridOptions = {}
): UvHitGrid {
  const uvs = readUvs(geometry);
  const vertexCount = uvs.length / 2;
  const idx = readFaceIndex(geometry, vertexCount);
  const faceCount = idx.length / 3;
  const cells =
    options.cells ?? Math.max(16, Math.min(256, Math.round(Math.sqrt(Math.max(faceCount, 1) / 4))));
  const oversizeFraction = options.oversizeFraction ?? 1 / 16;
  const maxCells = Math.max(1, Math.floor(cells * cells * oversizeFraction));

  const corners = new Float32Array(faceCount * 6);
  const counts = new Int32Array(cells * cells + 1);
  const boxes = new Int32Array(faceCount * 4);
  const oversizeList: number[] = [];

  for (let f = 0; f < faceCount; f++) {
    const a = idx[f * 3];
    const b = idx[f * 3 + 1];
    const c = idx[f * 3 + 2];
    const au = uvs[a * 2];
    const av = uvs[a * 2 + 1];
    const bu = uvs[b * 2];
    const bv = uvs[b * 2 + 1];
    const cu = uvs[c * 2];
    const cv = uvs[c * 2 + 1];
    corners[f * 6] = au;
    corners[f * 6 + 1] = av;
    corners[f * 6 + 2] = bu;
    corners[f * 6 + 3] = bv;
    corners[f * 6 + 4] = cu;
    corners[f * 6 + 5] = cv;

    const x0 = cellOf(Math.min(au, bu, cu), cells);
    const x1 = cellOf(Math.max(au, bu, cu), cells);
    const y0 = cellOf(Math.min(av, bv, cv), cells);
    const y1 = cellOf(Math.max(av, bv, cv), cells);
    const span = (x1 - x0 + 1) * (y1 - y0 + 1);
    if (span > maxCells) {
      boxes[f * 4] = -1;
      oversizeList.push(f);
      continue;
    }
    boxes[f * 4] = x0;
    boxes[f * 4 + 1] = y0;
    boxes[f * 4 + 2] = x1;
    boxes[f * 4 + 3] = y1;
    for (let y = y0; y <= y1; y++) {
      for (let x = x0; x <= x1; x++) counts[y * cells + x + 1]++;
    }
  }

  for (let i = 1; i < counts.length; i++) counts[i] += counts[i - 1];
  const start = counts;
  const items = new Int32Array(start[start.length - 1]);
  const cursor = new Int32Array(cells * cells);
  for (let f = 0; f < faceCount; f++) {
    if (boxes[f * 4] < 0) continue;
    const x0 = boxes[f * 4];
    const y0 = boxes[f * 4 + 1];
    const x1 = boxes[f * 4 + 2];
    const y1 = boxes[f * 4 + 3];
    for (let y = y0; y <= y1; y++) {
      for (let x = x0; x <= x1; x++) {
        const cell = y * cells + x;
        items[start[cell] + cursor[cell]++] = f;
      }
    }
  }

  return {
    cells,
    start,
    items,
    oversize: Int32Array.from(oversizeList),
    corners,
    faceCount
  };
}

function cellOf(value: number, cells: number): number {
  if (!Number.isFinite(value)) return 0;
  const cell = Math.floor(value * cells);
  return cell < 0 ? 0 : cell >= cells ? cells - 1 : cell;
}

/**
 * The face whose UV triangle contains `(u, v)`, or `-1`.
 *
 * ⚠️ Where islands OVERLAP more than one face contains the point; this returns the first one the
 * bucket holds, which is stable for a given grid but arbitrary. That is honest for a diagnostic —
 * the overlap itself is reported separately by the coverage pass, which counts every triangle.
 *
 * ⚠️ A point outside `[0,1]` is looked up in the nearest edge cell. It still resolves correctly for
 * a triangle whose UVs run off the atlas, because the containment test uses the real coordinates.
 */
export function uvHitFace(grid: UvHitGrid, u: number, v: number): number {
  const x = cellOf(u, grid.cells);
  const y = cellOf(v, grid.cells);
  const cell = y * grid.cells + x;
  const from = grid.start[cell];
  const to = grid.start[cell + 1];
  for (let i = from; i < to; i++) {
    const f = grid.items[i];
    if (containsUv(grid.corners, f, u, v)) return f;
  }
  for (let i = 0; i < grid.oversize.length; i++) {
    const f = grid.oversize[i];
    if (containsUv(grid.corners, f, u, v)) return f;
  }
  return -1;
}

function containsUv(corners: Float32Array, face: number, u: number, v: number): boolean {
  const p = face * 6;
  const au = corners[p];
  const av = corners[p + 1];
  const bu = corners[p + 2];
  const bv = corners[p + 3];
  const cu = corners[p + 4];
  const cv = corners[p + 5];
  // Barycentric sign test. A zero-area triangle yields three zero cross products and is never a
  // hit — correct, since it covers no area to point at.
  const d1 = (u - bu) * (av - bv) - (au - bu) * (v - bv);
  const d2 = (u - cu) * (bv - cv) - (bu - cu) * (v - cv);
  const d3 = (u - au) * (cv - av) - (cu - au) * (v - av);
  const negative = d1 < 0 || d2 < 0 || d3 < 0;
  const positive = d1 > 0 || d2 > 0 || d3 > 0;
  if (negative && positive) return false;
  return d1 !== 0 || d2 !== 0 || d3 !== 0;
}

// ---------------------------------------------------------------------------------------------
// Coverage COUNTS — the one thing only a rasteriser can answer
// ---------------------------------------------------------------------------------------------

/**
 * ⚠️ Writes 1/255 per fragment so an additive blend accumulates a COUNT in the red channel. See
 * the note on [`COVERAGE_FRAGMENT_SHADER`] about why no colour-space conversion happens here: a
 * hand-written `ShaderMaterial` writes exactly what it says, so 1/255 reads back as 1.
 */
const COVERAGE_COUNT_FRAGMENT_SHADER = /* glsl */ `
void main() {
  gl_FragColor = vec4( 1.0 / 255.0 );
}
`;

/**
 * How many triangles cover each texel, saturating at 255.
 *
 * `> 0` is coverage — the same answer [`buildCoverageMaskRect`] gives, from the same pass. `> 1` is
 * an OVERLAP: two triangles of the atlas writing to one texel, so whichever part of the character
 * loses the fight wears the other one's colour. That is the defect a projected atlas produces when
 * the packer was told the layout was fine and it was not, and nothing on the CPU can see it —
 * bounding boxes interlock harmlessly all the time, and only the fill rule decides which texel
 * centre belongs to which triangle.
 *
 * 🔴 Shared edges do NOT double-count. GPU rasterisation fills each texel centre for exactly one of
 * two triangles meeting at an edge (the top-left rule), so a watertight island comes back all 1s.
 * A count of 2 is a real second triangle, not a seam artefact.
 *
 * ⚠️ Deliberately a SIBLING of [`buildCoverageMaskRect`] rather than a refactor of it. That
 * function is on the repair path, which is destructive; this one is on the diagnostic path, which
 * is not. Sharing an implementation would put a behaviour change to the read-only panel one edit
 * away from changing what the repair writes.
 *
 * ⚠️ If a driver ignored the additive blend the counts would saturate at 1 — coverage would still
 * be right and overlap would read as zero. A false negative, never a false positive.
 */
export function buildCoverageCountRect(
  renderer: THREE.WebGLRenderer,
  mesh: THREE.Mesh,
  width: number,
  height: number
): Uint8Array {
  if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1) {
    throw new Error(`Coverage size must be positive integers, got ${width}×${height}.`);
  }
  const maxSize = renderer.capabilities.maxTextureSize;
  if (width > maxSize || height > maxSize) {
    throw new Error(
      `This texture is ${width}×${height}, larger than the GPU's ${maxSize} limit; its coverage cannot be measured here.`
    );
  }
  const geometry = mesh.geometry;
  if (!geometry.getAttribute('uv')) {
    throw new Error('This mesh has no UV coordinates, so it has no atlas coverage to measure.');
  }

  const target = new THREE.WebGLRenderTarget(width, height, {
    format: THREE.RGBAFormat,
    type: THREE.UnsignedByteType,
    minFilter: THREE.NearestFilter,
    magFilter: THREE.NearestFilter,
    generateMipmaps: false,
    depthBuffer: false,
    stencilBuffer: false,
    // ⚠️ No MSAA, for the same reason as the mask pass and one more: a resolve would AVERAGE the
    // per-sample counts and a texel covered twice would read back as something other than 2.
    samples: 0,
    colorSpace: THREE.NoColorSpace
  });

  const material = new THREE.ShaderMaterial({
    vertexShader: COVERAGE_VERTEX_SHADER,
    fragmentShader: COVERAGE_COUNT_FRAGMENT_SHADER,
    // 🔴 DoubleSide, as in the mask pass: mirrored islands wind backwards in UV space and would be
    // culled, reading as "half the character is missing" rather than "the material was wrong".
    side: THREE.DoubleSide,
    depthTest: false,
    depthWrite: false,
    // 🔴 Explicit One/One rather than `AdditiveBlending`, whose colour source factor is SrcAlpha.
    // That happens to be equivalent while alpha is 1, and would silently stop counting the day
    // someone changed the fragment's alpha.
    blending: THREE.CustomBlending,
    blendEquation: THREE.AddEquation,
    blendSrc: THREE.OneFactor,
    blendDst: THREE.OneFactor,
    blendEquationAlpha: THREE.AddEquation,
    blendSrcAlpha: THREE.OneFactor,
    blendDstAlpha: THREE.OneFactor
  });

  const proxy = new THREE.Mesh(geometry, material);
  // 🔴 See the mask pass: the vertex shader writes clip space directly, but three culls against the
  // bounding sphere in WORLD space, and a character standing away from the origin never rasterises.
  proxy.frustumCulled = false;

  const scene = new THREE.Scene();
  scene.add(proxy);
  const camera = new THREE.Camera();

  const prevTarget = renderer.getRenderTarget();
  const prevCubeFace = renderer.getActiveCubeFace();
  const prevMipLevel = renderer.getActiveMipmapLevel();
  const prevClearColor = new THREE.Color();
  renderer.getClearColor(prevClearColor);
  const prevClearAlpha = renderer.getClearAlpha();
  const prevAutoClear = renderer.autoClear;

  const buffer = new Uint8Array(width * height * 4);
  try {
    renderer.setRenderTarget(target);
    renderer.autoClear = false;
    renderer.setClearColor(0x000000, 0);
    renderer.clear(true, false, false);
    renderer.render(scene, camera);
    renderer.readRenderTargetPixels(target, 0, 0, width, height, buffer);
  } finally {
    renderer.setRenderTarget(prevTarget, prevCubeFace, prevMipLevel);
    renderer.setClearColor(prevClearColor, prevClearAlpha);
    renderer.autoClear = prevAutoClear;
    scene.remove(proxy);
    material.dispose();
    target.dispose();
    // 🔴 Never dispose `geometry` — it is the live mesh's, shared not copied.
  }

  const counts = new Uint8Array(width * height);
  for (let i = 0, p = 0; i < counts.length; i++, p += 4) counts[i] = buffer[p];
  return counts;
}

export interface CoverageSummary {
  total: number;
  /** Texels at least one triangle reaches. */
  covered: number;
  /** Texels two or more triangles reach — islands landing on top of each other. */
  overlapping: number;
  /** The worst texel's count. 255 means it saturated and the real number is unknown. */
  maxCount: number;
}

/**
 * Reduce a {@link buildCoverageCountRect} buffer to the three numbers a UI can show.
 *
 * ⚠️ `covered` is the fraction of the ATLAS the mesh reaches, and a number well under 1 is normal,
 * not a bug: an atlas is charts plus the gutter that keeps their filter taps apart. It becomes a
 * complaint when it is very low — that is resolution spent on nothing.
 */
export function summariseCoverage(counts: Uint8Array): CoverageSummary {
  let covered = 0;
  let overlapping = 0;
  let maxCount = 0;
  for (let i = 0; i < counts.length; i++) {
    const c = counts[i];
    if (c === 0) continue;
    covered++;
    if (c > 1) overlapping++;
    if (c > maxCount) maxCount = c;
  }
  return { total: counts.length, covered, overlapping, maxCount };
}
