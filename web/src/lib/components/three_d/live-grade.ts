/*
 * The colour grade, previewed on the GPU while the slider is still moving.
 *
 * [`adjustColour`] is the grade of record: it is what gets baked into the texture and saved into
 * the GLB, and nothing here replaces it. But it is a 4.19 M-texel CPU loop, so it can only run on
 * commit — which left the user dragging blind, pressing Apply, looking, and undoing. This module
 * puts the same maths in the fragment shader so the model updates under the slider, and then the
 * commit repaints with the exact version.
 *
 * # 🔴 The whole job is matching the bake
 *
 * A preview that does not agree with the commit is worse than no preview: it teaches the user a
 * mapping from slider to colour that Apply then breaks. Three things have to line up.
 *
 * 1. **The order.** exposure → white balance → contrast → saturation, the same photographic order
 *    [`adjustColour`] documents, and it is not commutative — moving white balance after the
 *    contrast pivot makes the temperature slider stop behaving like white balance.
 * 2. **The constants.** [`CONTRAST_PIVOT`], [`LUMA_R`]/[`LUMA_G`]/[`LUMA_B`] and
 *    [`TEMPERATURE_GAIN`] are imported and compiled into the shader source. They are not retyped
 *    here, and they must never be: a second copy of the pivot is a second grade.
 * 3. **The space.** [`adjustColour`] decodes sRGB, works in linear, re-encodes. The shader has to
 *    work in the same linear space — see below, because this is the part that goes wrong silently.
 *
 * # 🔴 Why the injection point is `#include <map_fragment>` and nowhere else
 *
 * Verified against three r0.185.1 sources, `renderers/shaders/`:
 *
 *   - `ShaderLib/meshphysical.glsl.js` `main()` opens with `vec4 diffuseColor = vec4( diffuse,
 *     opacity );` then `#include <map_fragment>` (one occurrence), and only reaches
 *     `#include <tonemapping_fragment>` / `#include <colorspace_fragment>` at the very END of the
 *     function, after all lighting. So immediately after `map_fragment` we are pre-tonemap,
 *     pre-output-encode, and pre-lighting: `diffuseColor.rgb` is a plain albedo.
 *   - `ShaderChunk/map_fragment.glsl.js` is `diffuseColor *= texture2D( map, vMapUv )`. It does NOT
 *     decode sRGB — `WebGLTextures.getInternalFormat` uploads an `SRGBColorSpace` byte texture as
 *     `gl.SRGB8_ALPHA8`, so the sampler hardware applies the sRGB EOTF and the texel arrives
 *     **linear**. (The inline `sRGBTransferEOTF` in that chunk is `DECODE_VIDEO_TEXTURE` only.)
 *
 * That is the one point in the shader whose units are [`adjustColour`]'s units. Grading after tone
 * mapping grades a display-referred value; grading the texel before decode grades an sRGB-encoded
 * one. Both compile, both look plausible, and both disagree with the file that gets saved.
 *
 * ⚠️ Alpha is not touched, matching [`adjustColour`]. Grading it turns a transparency map into a
 * fade.
 *
 * # Where the preview still cannot match, and why
 *
 * - **A tinted `material.color`.** `map_fragment` multiplies the texel INTO `diffuseColor`, which
 *   already holds `diffuse`. So we grade `color × texel` where the bake grades `texel` alone, and
 *   contrast and saturation are both non-linear in their input. Exact when `material.color` is
 *   white, which is what GLTF's default `baseColorFactor` and the Pixal3D baker give us. If a
 *   tinted base colour ever becomes real, the fix is to divide `diffuse` out before grading and
 *   multiply it back after (`diffuse` is already a declared uniform in this shader) — not to move
 *   the injection point.
 * - **Texture filtering.** The GPU grades a bilinear/mip-filtered sample; the bake grades texels
 *   and mips afterwards. `grade(filter(x)) != filter(grade(x))` wherever contrast or saturation is
 *   away from 1. Sub-pixel, visible only as a hair of difference on minified detail.
 * - **8-bit quantisation.** The commit rounds through a 65536-entry encode LUT into a byte per
 *   channel; the preview stays in float. Sub-LSB.
 *
 * # ⚠️ WebGLRenderer only
 *
 * `onBeforeCompile` and `customProgramCacheKey` are WebGL-path hooks; three's own docs say so. This
 * module does nothing under WebGPURenderer.
 */

import * as THREE from 'three';

import {
  CONTRAST_PIVOT,
  LUMA_B,
  LUMA_G,
  LUMA_R,
  TEMPERATURE_GAIN,
  resolveAdjust,
  type ColourAdjust
} from './texture-repair';

export interface LiveGrade {
  /**
   * Update the preview. Uniform writes only — no shader recompile, so this is safe to call from
   * every `input` event on a slider.
   *
   * ⚠️ Takes the same [`ColourAdjust`] [`adjustColour`] takes, with the same identity:
   * `{ exposure: 0, contrast: 1, saturation: 1, temperature: 0 }`. Contrast and saturation are
   * **1** at rest, not 0. Passing zeroes for them asks for a flat grey and will render one —
   * correctly, because that is what the commit would bake.
   */
  set(adjust: ColourAdjust): void;
  /** Put the material back exactly as it was found. Idempotent. */
  detach(): void;
}

/**
 * The marker we inject after. Exactly one occurrence in the standard/physical fragment shader.
 */
const MAP_FRAGMENT = '#include <map_fragment>';

/**
 * ⚠️ Appended to the cache key of any material we attach to. `WebGLPrograms.acquireProgram` looks
 * compiled programs up in a renderer-GLOBAL `programsMap` keyed by
 * `getProgramCacheKey(parameters)`, and that key is built from the material's PARAMETERS plus
 * `material.customProgramCacheKey()` — never from the shader source `onBeforeCompile` produced. An
 * ungraded material with identical parameters that compiled first would therefore hand us its
 * program, and the injection would silently do nothing. Perturbing the key is the only thing
 * standing between this module and an intermittently dead preview.
 */
const CACHE_KEY_TAG = 'kb-live-grade';

/** Materials with a grade on them, so a second `attachLiveGrade` fails loudly instead of stacking. */
const attached = new WeakSet<THREE.Material>();

/**
 * A GLSL float literal. `toPrecision` always leaves a decimal point (or an exponent, which GLSL
 * also accepts), so an integral constant cannot silently compile as an `int` and change the type of
 * the expression around it.
 */
function glslFloat(value: number): string {
  return value.toPrecision(9);
}

/*
 * Three uniforms carry the four controls: exposure and temperature fold into one per-channel gain,
 * exactly as `adjustColour` folds them into its `gainR`/`gainG`/`gainB` before the loop. Doing that
 * fold in `set()` rather than in GLSL means the preview multiplies by the same JS-computed numbers
 * the commit will, not by an `exp2()` that agrees with `Math.pow(2, x)` only to within an ulp.
 *
 * ⚠️ Prepended to the fragment shader rather than spliced into a `_pars_fragment` chunk. three
 * concatenates its own `#version` / precision prefix in FRONT of this string when it builds the
 * program, so the top of the shader body is a legal place for a declaration and does not depend on
 * a second chunk name staying where it is.
 */
const UNIFORM_DECLARATIONS = `
uniform vec3 kbGradeGain;
uniform float kbGradeContrast;
uniform float kbGradeSaturation;
`;

/** The imported constants as shader literals — compiled in, never a uniform, never a retype. */
const PIVOT = glslFloat(CONTRAST_PIVOT);
const LUMA = `vec3( ${glslFloat(LUMA_R)}, ${glslFloat(LUMA_G)}, ${glslFloat(LUMA_B)} )`;

/*
 * A line-for-line transcription of `adjustColour`'s inner loop, including its two `!== 1` guards.
 * Those guards are not an optimisation here — they are what makes the identity grade a bit-exact
 * no-op. `(c - PIVOT) * 1.0 + PIVOT` is NOT exactly `c` in floating point, so an unguarded contrast
 * of 1 would shift every texel by an ulp and the "reset" would not quite reset.
 *
 * The trailing `clamp` is `encodeSrgb`'s clamp: the commit clips to the 0..1 byte range, so a +2 EV
 * preview has to go white here rather than push the extra energy on into the lighting.
 *
 * ⚠️ Only `.rgb`. `diffuseColor.a` carries the alpha map and `adjustColour` passes alpha through;
 * grading it would turn a transparency map into a fade.
 */
const GRADE_GLSL = `
	{
		vec3 kbGraded = diffuseColor.rgb * kbGradeGain;
		if ( kbGradeContrast != 1.0 ) {
			kbGraded = ( kbGraded - ${PIVOT} ) * kbGradeContrast + ${PIVOT};
		}
		if ( kbGradeSaturation != 1.0 ) {
			float kbLuma = dot( kbGraded, ${LUMA} );
			kbGraded = kbLuma + ( kbGraded - kbLuma ) * kbGradeSaturation;
		}
		diffuseColor.rgb = clamp( kbGraded, 0.0, 1.0 );
	}
`;

/**
 * Preview a colour grade on the GPU, matching what [`adjustColour`] will bake.
 *
 * Call [`LiveGrade.set`] on every slider frame; call [`LiveGrade.detach`] before committing the
 * grade to the texture (otherwise the preview grades the already-graded texels) and on teardown.
 * The typical Svelte shape is an `$effect` that attaches and returns `() => grade.detach()`.
 *
 * ⚠️ Throws if the material already has a live grade attached. Two grades on one material apply
 * twice and each `detach()` restores half of the pair; the caller wanted one handle.
 */
export function attachLiveGrade(material: THREE.MeshStandardMaterial): LiveGrade {
  if (attached.has(material)) {
    throw new Error('This material already has a live grade attached; detach the first one.');
  }

  const gain: THREE.IUniform<THREE.Vector3> = { value: new THREE.Vector3(1, 1, 1) };
  const contrast: THREE.IUniform<number> = { value: 1 };
  const saturation: THREE.IUniform<number> = { value: 1 };

  // Own property or inherited method — remembered separately so `detach` can restore the exact
  // shape it found. Deleting an own property the material never had would expose the prototype's
  // no-op, which is the same behaviour but not the same object graph; assigning the prototype's
  // method back as an own property would be the reverse mistake.
  const hadOwnHook = Object.prototype.hasOwnProperty.call(material, 'onBeforeCompile');
  const hadOwnCacheKey = Object.prototype.hasOwnProperty.call(material, 'customProgramCacheKey');
  const previousHook = material.onBeforeCompile;
  const previousCacheKey = material.customProgramCacheKey;

  // 🔴 Snapshot the previous key NOW, before `onBeforeCompile` is swapped. `Material`'s default
  // implementation returns `this.onBeforeCompile.toString()`, so calling it after the swap would
  // stringify OUR hook and throw away whatever made the material's existing hook distinct — two
  // materials with different foreign hooks would then collide on our tag alone.
  const cacheKey = `${previousCacheKey.call(material)}|${CACHE_KEY_TAG}`;

  let warnedMissingChunk = false;

  const hook = (
    parameters: THREE.WebGLProgramParametersWithUniforms,
    renderer: THREE.WebGLRenderer
  ): void => {
    // Chain, don't clobber — and run theirs first, so we inject into the source they produced.
    previousHook.call(material, parameters, renderer);

    // The uniform OBJECTS go in, not their values: three stores this map as
    // `materialProperties.uniforms` and reads `.value` off it at draw time, which is what makes
    // `set()` a pure write with no recompile.
    parameters.uniforms.kbGradeGain = gain;
    parameters.uniforms.kbGradeContrast = contrast;
    parameters.uniforms.kbGradeSaturation = saturation;

    if (!parameters.fragmentShader.includes(MAP_FRAGMENT)) {
      // Only reachable if something upstream rewrote the chunk away. Say so rather than render an
      // ungraded model that looks like a broken slider.
      if (!warnedMissingChunk) {
        warnedMissingChunk = true;
        console.warn(
          'attachLiveGrade: no map_fragment chunk in this fragment shader, so the colour ' +
            'preview is inert. Another onBeforeCompile hook replaced the chunk it injects after.'
        );
      }
      return;
    }

    // ⚠️ A replacer FUNCTION, not a replacement string: `$&`, `$'` and friends are substitution
    // patterns in a string replacement, and GLSL is not a language where you want to find that out
    // by accident. A string pattern also replaces only the first match, which is what we want.
    parameters.fragmentShader =
      UNIFORM_DECLARATIONS +
      parameters.fragmentShader.replace(MAP_FRAGMENT, () => MAP_FRAGMENT + '\n' + GRADE_GLSL);
  };

  material.onBeforeCompile = hook;
  material.customProgramCacheKey = () => cacheKey;
  material.needsUpdate = true;
  attached.add(material);

  let live = true;

  return {
    set(adjust: ColourAdjust): void {
      // Through the commit's own resolver: same identity defaults, same rejection of NaN — a NaN
      // gain would black the model out with no clue why.
      const resolved = resolveAdjust(adjust);
      // The exposure and white-balance fold, in the same three expressions `adjustColour` uses.
      const stops = Math.pow(2, resolved.exposure);
      gain.value.set(
        stops * (1 + resolved.temperature * TEMPERATURE_GAIN),
        stops,
        stops * (1 - resolved.temperature * TEMPERATURE_GAIN)
      );
      contrast.value = resolved.contrast;
      saturation.value = resolved.saturation;
      // 🔴 No `material.needsUpdate` here, deliberately. This runs once per pointermove on a 250k
      // vertex character; a recompile per frame is a visible stutter.
    },

    detach(): void {
      if (!live) return;
      live = false;
      if (hadOwnHook) {
        material.onBeforeCompile = previousHook;
      } else {
        Reflect.deleteProperty(material, 'onBeforeCompile');
      }
      if (hadOwnCacheKey) {
        material.customProgramCacheKey = previousCacheKey;
      } else {
        Reflect.deleteProperty(material, 'customProgramCacheKey');
      }
      attached.delete(material);
      // The one recompile this module allows itself. It is usually free: the pre-grade cache key
      // comes back, and the material's own program map still holds the program compiled under it.
      material.needsUpdate = true;
    }
  };
}
