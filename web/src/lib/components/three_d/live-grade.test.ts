import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';
import * as THREE from 'three';
import { attachLiveGrade } from './live-grade';
import {
  CONTRAST_PIVOT,
  LUMA_B,
  LUMA_G,
  LUMA_R,
  TEMPERATURE_GAIN,
  adjustColour,
  type ColourAdjust
} from './texture-repair';

/**
 * This file is the guard on the one thing in the 3D editor that rots SILENTLY: `live-grade.ts`
 * compiles its GLSL from constants it imports out of `texture-repair.ts`, and if the two drift the
 * on-screen preview stops agreeing with what Apply bakes into the GLB. Nothing else in the tree
 * would notice — both halves keep compiling, both keep looking plausible.
 *
 * ⚠️ What it CANNOT do: run the shader. vitest has no GL context. So it checks the two halves that
 * are reachable from JS — the gain `set()` folds into the uniform, and the constants that were
 * literally compiled into the emitted shader source — and then walks the emitted GLSL through a
 * transcription of itself to compare the whole grade against the bake, byte for byte. A GLSL
 * compiler error, a wrong injection POINT, or a colour-space mistake in the sampler are all outside
 * what this can see.
 */

/** vitest runs in node, which has no `ImageData`. `adjustColour` only needs the three fields. */
class ImageDataShim {
  readonly data: Uint8ClampedArray;
  readonly width: number;
  readonly height: number;
  readonly colorSpace = 'srgb';
  constructor(data: Uint8ClampedArray, width: number, height: number) {
    this.data = data;
    this.width = width;
    this.height = height;
  }
}

let shimmed = false;
beforeAll(() => {
  if (typeof globalThis.ImageData === 'undefined') {
    (globalThis as { ImageData?: unknown }).ImageData = ImageDataShim;
    shimmed = true;
  }
});
// ⚠️ vitest workers are reused across files; put the global back so nothing else sees a fake one.
afterAll(() => {
  if (shimmed) Reflect.deleteProperty(globalThis, 'ImageData');
});

/** The shape of `meshphysical.glsl.js`'s `main()` that matters here: `diffuseColor` is declared,
 *  the map chunk multiplies the texel into it, and tone mapping happens far later. */
const FRAGMENT_SHADER = [
  'void main() {',
  '\tvec4 diffuseColor = vec4( diffuse, opacity );',
  '\t#include <map_fragment>',
  '\t#include <tonemapping_fragment>',
  '\t#include <colorspace_fragment>',
  '}'
].join('\n');

interface FakeParameters {
  uniforms: Record<string, THREE.IUniform>;
  fragmentShader: string;
}

/** Drive the material's `onBeforeCompile` the way `WebGLPrograms` would, and keep what it produced. */
function compile(material: THREE.MeshStandardMaterial, shader = FRAGMENT_SHADER): FakeParameters {
  const parameters: FakeParameters = { uniforms: {}, fragmentShader: shader };
  material.onBeforeCompile(
    parameters as unknown as THREE.WebGLProgramParametersWithUniforms,
    {} as THREE.WebGLRenderer
  );
  return parameters;
}

/** A freshly graded material, already through `onBeforeCompile`. */
function graded(shader = FRAGMENT_SHADER): FakeParameters {
  const material = new THREE.MeshStandardMaterial();
  attachLiveGrade(material);
  return compile(material, shader);
}

function probeImage(): ImageData {
  const texels = 256;
  const data = new Uint8ClampedArray(texels * 4);
  for (let i = 0; i < texels; i++) {
    data[i * 4] = i;
    data[i * 4 + 1] = (i * 7) % 256;
    data[i * 4 + 2] = (i * 13) % 256;
    data[i * 4 + 3] = 255 - i;
  }
  return new ImageData(data, 16, 16);
}

const decodeSrgb = (byte: number): number => {
  const c = byte / 255;
  return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
};

const encodeSrgb = (linear: number): number => {
  const c = linear <= 0 ? 0 : linear >= 1 ? 1 : linear;
  return Math.round((c <= 0.0031308 ? c * 12.92 : 1.055 * Math.pow(c, 1 / 2.4) - 0.055) * 255);
};

/**
 * The constants the shader was actually built with, read back OUT of the emitted GLSL rather than
 * imported. That is the point: if `live-grade.ts` ever stops importing them from `texture-repair.ts`
 * and grows its own copy, the numbers recovered here stop matching the exported ones and the
 * comparison against `adjustColour` below falls apart.
 */
function constantsInShader(source: string): { pivot: number; luma: [number, number, number] } {
  const pivot = source.match(/kbGraded\s*-\s*([0-9.eE+-]+)\s*\)/);
  const luma = source.match(/dot\(\s*kbGraded,\s*vec3\(([^)]*)\)\s*\)/);
  if (!pivot || !luma) throw new Error(`could not read the grade constants back out:\n${source}`);
  const parts = luma[1].split(',').map((p) => Number(p.trim()));
  return { pivot: Number(pivot[1]), luma: [parts[0], parts[1], parts[2]] };
}

/** A line-for-line transcription of the emitted `GRADE_GLSL`, driven by the uniforms `set()` wrote
 *  and the constants parsed out of the shader — no imported number appears in here. */
function runShader(
  rgb: [number, number, number],
  parameters: FakeParameters
): [number, number, number] {
  const { pivot, luma } = constantsInShader(parameters.fragmentShader);
  const gain = parameters.uniforms.kbGradeGain.value as THREE.Vector3;
  const contrast = parameters.uniforms.kbGradeContrast.value as number;
  const saturation = parameters.uniforms.kbGradeSaturation.value as number;

  let out: [number, number, number] = [rgb[0] * gain.x, rgb[1] * gain.y, rgb[2] * gain.z];
  if (contrast !== 1) {
    out = out.map((c) => (c - pivot) * contrast + pivot) as [number, number, number];
  }
  if (saturation !== 1) {
    const l = out[0] * luma[0] + out[1] * luma[1] + out[2] * luma[2];
    out = out.map((c) => l + (c - l) * saturation) as [number, number, number];
  }
  return out.map((c) => Math.min(1, Math.max(0, c))) as [number, number, number];
}

describe('attachLiveGrade — the constants compiled into the shader', () => {
  /**
   * 🔴 A second copy of the pivot is a second grade: the GPU preview would pivot somewhere the
   * commit does not, and the model would jump the instant the user pressed Apply. These are read
   * back out of the generated source and compared against the values `texture-repair.ts` exports.
   */
  it('bakes in the exported pivot and the exported Rec.709 luma weights, not a retype', () => {
    const { pivot, luma } = constantsInShader(graded().fragmentShader);
    expect(pivot).toBe(CONTRAST_PIVOT);
    expect(luma).toEqual([LUMA_R, LUMA_G, LUMA_B]);
  });

  /** 9 significant figures, so the GLSL literal is not a rounded version of the JS constant. */
  it('emits them at enough precision to survive the round trip', () => {
    const source = graded().fragmentShader;
    expect(source).toContain(CONTRAST_PIVOT.toPrecision(9));
    expect(source).toContain(LUMA_R.toPrecision(9));
  });

  it('injects the grade AFTER the map chunk, exactly once', () => {
    const source = graded().fragmentShader;
    const map = source.indexOf('#include <map_fragment>');
    expect(map).toBeGreaterThanOrEqual(0);
    expect(source.indexOf('#include <map_fragment>', map + 1)).toBe(-1);
    expect(source.indexOf('kbGraded')).toBeGreaterThan(map);
    // ...and before tone mapping and the output encode, or the grade is in the wrong space.
    expect(source.indexOf('kbGraded')).toBeLessThan(source.indexOf('tonemapping_fragment'));
  });

  it('declares its three uniforms and hands the same objects to three', () => {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    const parameters = compile(material);
    expect(parameters.fragmentShader).toContain('uniform vec3 kbGradeGain;');
    expect(parameters.uniforms.kbGradeGain).toBeDefined();

    // The uniform OBJECT is shared, which is what makes `set()` a write with no recompile.
    const gain = parameters.uniforms.kbGradeGain;
    grade.set({ exposure: 1 });
    expect(parameters.uniforms.kbGradeGain).toBe(gain);
    expect((gain.value as THREE.Vector3).y).toBe(2);
  });

  /** ⚠️ Only `.rgb`: `diffuseColor.a` carries the alpha map and grading it turns a transparency map
   *  into a fade. `adjustColour` passes alpha through for the same reason. */
  it('never writes diffuseColor.a', () => {
    const source = graded().fragmentShader;
    expect(source).toContain('diffuseColor.rgb = clamp(');
    expect(source).not.toMatch(/diffuseColor\.a\s*=/);
    expect(source).not.toMatch(/diffuseColor\s*=\s*clamp/);
  });
});

describe('attachLiveGrade — the gain fold matches the bake', () => {
  /**
   * 🔴 The exposure/white-balance fold is the one piece of the grade that lives in JS on BOTH sides:
   * `adjustColour` folds it into `gainR`/`gainG`/`gainB` before its texel loop, and `set()` folds it
   * into one `vec3` uniform. `TEMPERATURE_GAIN` is imported here from the same module both of them
   * import it from, so a change to the constant moves this assertion and the bake together — and a
   * change to either FORMULA breaks it.
   */
  it.each([
    { exposure: 0, temperature: 0 },
    { exposure: 1, temperature: 0 },
    { exposure: -2.5, temperature: 0 },
    { exposure: 0, temperature: 1 },
    { exposure: 0, temperature: -1 },
    { exposure: 0.7, temperature: 0.35 }
  ])('folds exposure %o and temperature into the same per-channel gain', (adjust) => {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    const parameters = compile(material);
    grade.set(adjust);

    const stops = Math.pow(2, adjust.exposure);
    const gain = parameters.uniforms.kbGradeGain.value as THREE.Vector3;
    expect(gain.x).toBe(stops * (1 + adjust.temperature * TEMPERATURE_GAIN));
    expect(gain.y).toBe(stops);
    expect(gain.z).toBe(stops * (1 - adjust.temperature * TEMPERATURE_GAIN));
  });

  /** ⚠️ Contrast and saturation are identity at 1, not 0. A caller that initialises its slider state
   *  to all-zeroes gets a flat grey — from the preview AND from the commit, which is the point. */
  it('defaults an omitted control the way the bake does, through the same resolver', () => {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    const parameters = compile(material);

    grade.set({});
    expect(parameters.uniforms.kbGradeContrast.value).toBe(1);
    expect(parameters.uniforms.kbGradeSaturation.value).toBe(1);
    expect((parameters.uniforms.kbGradeGain.value as THREE.Vector3).toArray()).toEqual([1, 1, 1]);

    grade.set({ contrast: 0, saturation: 0 });
    expect(parameters.uniforms.kbGradeContrast.value).toBe(0);
  });

  /** A NaN gain would black the model out with nothing in the console. */
  it('rejects a non-numeric control instead of writing NaN into a uniform', () => {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    const parameters = compile(material);
    expect(() => grade.set({ exposure: Number.NaN })).toThrow(/not a number/);
    expect((parameters.uniforms.kbGradeGain.value as THREE.Vector3).toArray()).toEqual([1, 1, 1]);
  });

  /** 🔴 One `set()` per pointermove on a 250k-vertex character; a recompile per frame is a visible
   *  stutter. `Material.needsUpdate` bumps `version`, so a stable version is the observable proof. */
  it('is a pure uniform write with no recompile', () => {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    compile(material);
    const version = material.version;
    for (let i = 0; i < 50; i++) grade.set({ exposure: i / 50 });
    expect(material.version).toBe(version);
  });
});

describe('live grade ↔ bake parity', () => {
  /**
   * The end-to-end check: walk the SAME texels through `adjustColour` (the version that gets saved)
   * and through a transcription of the emitted GLSL driven by the uniforms `set()` wrote, and
   * require the two to land on the same byte.
   *
   * ⚠️ Tolerance of 1 LSB, and it has to be: the commit rounds through a 65536-entry encode LUT
   * where this rounds in float. That is the documented, sub-LSB half of the difference. The mean is
   * asserted separately so a systematic offset — a wrong pivot, the operations in the wrong order,
   * a missing clamp — cannot hide inside the per-texel slack.
   */
  const cases: ColourAdjust[] = [
    { exposure: 1 },
    { exposure: -0.5, temperature: 0.8 },
    { contrast: 1.4 },
    { contrast: 0.6 },
    { saturation: 0.3 },
    { saturation: 1.8 },
    { temperature: -1 },
    { exposure: 0.7, contrast: 1.2, saturation: 1.6, temperature: -0.6 },
    // +3 EV: everything above the clamp has to go white in the preview too, rather than pushing the
    // extra energy on into the lighting.
    { exposure: 3 }
  ];

  /** Per-channel byte distance between the preview and the bake, over the whole probe image. */
  function drift(adjust: ColourAdjust, doctor?: (source: string) => string) {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    const parameters = compile(material);
    grade.set(adjust);
    if (doctor) parameters.fragmentShader = doctor(parameters.fragmentShader);

    const image = probeImage();
    const baked = adjustColour(image, adjust);

    let worst = 0;
    let total = 0;
    for (let p = 0; p < image.data.length; p += 4) {
      const linear: [number, number, number] = [
        decodeSrgb(image.data[p]),
        decodeSrgb(image.data[p + 1]),
        decodeSrgb(image.data[p + 2])
      ];
      const previewed = runShader(linear, parameters).map(encodeSrgb);
      for (let k = 0; k < 3; k++) {
        const delta = Math.abs(previewed[k] - baked.data[p + k]);
        worst = Math.max(worst, delta);
        total += delta;
      }
    }
    return { worst, mean: total / (image.data.length / 4) / 3, image, baked };
  }

  it.each(cases)('agrees with adjustColour for %o', (adjust) => {
    const { worst, mean, image, baked } = drift(adjust);
    expect(worst).toBeLessThanOrEqual(1);
    expect(mean).toBeLessThan(0.5);
    // Alpha is untouched on both sides.
    expect([...baked.data.filter((_, i) => i % 4 === 3)]).toEqual([
      ...image.data.filter((_, i) => i % 4 === 3)
    ]);
  });

  /**
   * The check above only means something if it would actually FAIL on a drift, so: doctor the
   * emitted shader source the way a second, retyped copy of a constant would, and require the
   * comparison to blow up. Without this the parity test could be passing vacuously.
   */
  it('would catch a pivot that drifted away from the exported one', () => {
    const adjust = { contrast: 1.6 };
    expect(drift(adjust).worst).toBeLessThanOrEqual(1);
    // 0.2140... → 0.5, the "pivot at linear mid-grey" mistake the source documents.
    const doctored = drift(adjust, (s) => s.replace(CONTRAST_PIVOT.toPrecision(9), '0.500000000'));
    expect(doctored.worst).toBeGreaterThan(10);
  });

  it('would catch luma weights that drifted away from the exported ones', () => {
    const adjust = { saturation: 0 };
    expect(drift(adjust).worst).toBeLessThanOrEqual(1);
    // Rec.709 → the flat 1/3 average a "luma is just the mean" rewrite would produce.
    const doctored = drift(adjust, (s) =>
      s.replace(
        `vec3( ${LUMA_R.toPrecision(9)}, ${LUMA_G.toPrecision(9)}, ${LUMA_B.toPrecision(9)} )`,
        'vec3( 0.333333333, 0.333333333, 0.333333333 )'
      )
    );
    expect(doctored.worst).toBeGreaterThan(10);
  });

  /**
   * 🔴 The two `!== 1` guards are not an optimisation: `(c - PIVOT) * 1.0 + PIVOT` is not exactly
   * `c` in floating point, so an unguarded contrast of 1 would shift every texel by an ulp and the
   * "reset" would not quite reset. The bake short-circuits the identity outright; the preview has to
   * be a no-op too, or letting go of the slider would move the picture.
   */
  it('is a bit-exact no-op at the identity, on both sides', () => {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    const parameters = compile(material);
    grade.set({ exposure: 0, contrast: 1, saturation: 1, temperature: 0 });

    const image = probeImage();
    expect([...adjustColour(image, {}).data]).toEqual([...image.data]);

    for (let p = 0; p < image.data.length; p += 4) {
      const linear: [number, number, number] = [
        decodeSrgb(image.data[p]),
        decodeSrgb(image.data[p + 1]),
        decodeSrgb(image.data[p + 2])
      ];
      expect(runShader(linear, parameters)).toEqual(linear);
    }
  });
});

describe('attachLiveGrade — attach and detach', () => {
  /**
   * ⚠️ `acquireProgram` looks programs up in a renderer-GLOBAL map keyed on the material's
   * parameters plus `customProgramCacheKey()` — never on the source `onBeforeCompile` produced. An
   * ungraded material with identical parameters that compiled first would hand us its program and
   * the injection would silently do nothing.
   */
  it('perturbs the program cache key', () => {
    const material = new THREE.MeshStandardMaterial();
    const before = material.customProgramCacheKey();
    attachLiveGrade(material);
    const after = material.customProgramCacheKey();
    expect(after).not.toBe(before);
    expect(after).toContain('kb-live-grade');
  });

  /**
   * 🔴 The previous key is snapshotted BEFORE the hook is swapped. `Material`'s default
   * `customProgramCacheKey` returns `this.onBeforeCompile.toString()`, so reading it afterwards
   * would stringify OUR hook and throw away whatever made the material's existing hook distinct —
   * two materials with different foreign hooks would then collide on the tag alone.
   */
  it('keeps a foreign hook distinguishable in the key', () => {
    const a = new THREE.MeshStandardMaterial();
    const b = new THREE.MeshStandardMaterial();
    a.onBeforeCompile = function hookA(): void {
      /* distinct source */
    };
    b.onBeforeCompile = function hookB(): void {
      /* a different distinct source */
    };
    attachLiveGrade(a);
    attachLiveGrade(b);
    expect(a.customProgramCacheKey()).not.toBe(b.customProgramCacheKey());
    expect(a.customProgramCacheKey()).toContain('hookA');
  });

  it('chains a pre-existing hook and runs it first, so it injects into their source', () => {
    const material = new THREE.MeshStandardMaterial();
    const order: string[] = [];
    material.onBeforeCompile = (parameters): void => {
      order.push('theirs');
      parameters.fragmentShader = parameters.fragmentShader.replace(
        '#include <map_fragment>',
        '#include <map_fragment>\n\t// theirs'
      );
    };

    attachLiveGrade(material);
    const source = compile(material).fragmentShader;
    expect(order).toEqual(['theirs']);
    expect(source).toContain('// theirs');
    expect(source).toContain('kbGraded');
  });

  it('restores the material exactly as it found it, and is idempotent', () => {
    const material = new THREE.MeshStandardMaterial();
    const key = material.customProgramCacheKey();
    const grade = attachLiveGrade(material);

    grade.detach();
    expect(Object.prototype.hasOwnProperty.call(material, 'onBeforeCompile')).toBe(false);
    expect(Object.prototype.hasOwnProperty.call(material, 'customProgramCacheKey')).toBe(false);
    expect(material.customProgramCacheKey()).toBe(key);
    expect(compile(material).fragmentShader).toBe(FRAGMENT_SHADER);

    const version = material.version;
    grade.detach();
    expect(material.version).toBe(version);
  });

  it('restores an own hook as an own property rather than exposing the prototype', () => {
    const material = new THREE.MeshStandardMaterial();
    const theirs = (): void => {};
    material.onBeforeCompile = theirs;
    attachLiveGrade(material).detach();
    expect(Object.prototype.hasOwnProperty.call(material, 'onBeforeCompile')).toBe(true);
    expect(material.onBeforeCompile).toBe(theirs);
  });

  it('recompiles once on detach, and once on attach', () => {
    const material = new THREE.MeshStandardMaterial();
    const start = material.version;
    const grade = attachLiveGrade(material);
    expect(material.version).toBe(start + 1);
    grade.detach();
    expect(material.version).toBe(start + 2);
  });

  /** Two grades on one material apply twice and each detach restores half the pair. */
  it('refuses a second grade on the same material, and allows one after detach', () => {
    const material = new THREE.MeshStandardMaterial();
    const grade = attachLiveGrade(material);
    expect(() => attachLiveGrade(material)).toThrow(/already has a live grade/);
    grade.detach();
    expect(() => attachLiveGrade(material)).not.toThrow();
  });

  /** An inert preview looks exactly like a broken slider, so it has to say so. */
  it('warns once and injects nothing when the map chunk is gone', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const material = new THREE.MeshStandardMaterial();
    attachLiveGrade(material);

    const stripped = 'void main() { vec4 diffuseColor = vec4( diffuse, opacity ); }';
    expect(compile(material, stripped).fragmentShader).toBe(stripped);
    compile(material, stripped);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn.mock.calls[0][0]).toContain('map_fragment');
    warn.mockRestore();
  });
});
