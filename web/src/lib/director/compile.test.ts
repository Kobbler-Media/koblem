import { describe, expect, it } from 'vitest';
import type { DirectorClip, H3GenerateRequest, LtxVideoGenerateRequest } from '#lib/api/client.ts';
import { isH3Request, ltxRequestOf } from '#lib/api/client.ts';
import { audioSource, compileForDelivery, compileTimeline, type CompileInput } from './compile';
import { V2V_STRENGTH_STEP, v2vSourceOf, v2vStrengthDefault } from './continuity';
import { DEFAULT_BASE_SAMPLER, DEFAULT_BASE_SIGMAS, newStage } from './resolution';

function clip(over: Partial<DirectorClip> = {}): DirectorClip {
  return { id: over.id ?? 'a', prompt: 'a shot', mode: 't2v', seconds: 4, keyframes: [], ...over };
}

function input(over: Partial<CompileInput> = {}): CompileInput {
  return {
    engine: 'ltx25',
    clips: [clip()],
    characters: [],
    h3References: [],
    h3Generations: [],
    model: undefined,
    loras: [],
    width: 1920,
    height: 1088,
    fps: 24,
    shotSeconds: 4,
    upscalerRatio: 'x2',
    qualityProfile: 'standard',
    audioKind: 'none',
    steps: undefined,
    cfgScale: undefined,
    negativePrompt: '',
    seed: undefined,
    frameDrop: 'auto',
    detailStrength: 0.85,
    customBaseSampler: DEFAULT_BASE_SAMPLER,
    customBaseSigmasText: DEFAULT_BASE_SIGMAS,
    customStages: [newStage()],
    resumeJobId: null,
    ...over
  };
}

/**
 * The compiled request, narrowed to the engine the test is about.
 *
 * `CompiledTimeline.request` is a union now — `generations` on H3, `segments` on LTX — so every
 * reader has to say which one it means. That is the whole point of giving H3 a real type: the
 * shape used to be a `Record<string, unknown>` double-cast, which meant nothing in the frontend
 * could tell the two apart and a field emitted into the wrong one was not a compile error.
 */
const ltx = (over: Partial<CompileInput> = {}): LtxVideoGenerateRequest => {
  const { request } = compileTimeline(input(over));
  if (isH3Request(request)) throw new Error('expected an LTX request');
  return request;
};
const h3req = (over: Partial<CompileInput> = {}): H3GenerateRequest => {
  const { request } = compileTimeline(input({ engine: 'h3', width: 1344, height: 768, ...over }));
  if (!isH3Request(request)) throw new Error('expected an H3 request');
  return request;
};

const seg = (over: Partial<CompileInput> = {}) => ltx(over).segments[0];

describe('audioSource', () => {
  it.each([
    ['none', 'none'],
    ['voice', 'voice_upload'],
    ['bgm', 'bgm_upload'],
    ['mixed', 'mixed_upload']
  ] as const)('%s → %s', (kind, want) => {
    expect(audioSource(kind)).toEqual({ kind: want });
  });
});

describe('compileTimeline — the untouched-project contract', () => {
  /** The whole reason every rule below is an "only when it differs" rule: a project that opened no
   *  advanced control must produce exactly the request it always did. */
  it('emits nothing optional for a plain t2v project', () => {
    expect(seg()).toEqual({
      prompt: 'a shot',
      seconds: 4,
      character_refs: undefined
    });
  });

  it('sends a character reference on every shot unless the shot opts into auto', () => {
    // 🔴 MEASURED 2026-08-15. Re-sending a reference into a join that already holds the identity
    // re-imposes the REFERENCE's state: a robot whose head came off in shot 1 got it back in shot
    // 2, and the motion step at the cut doubled (15.20 against 6.83). Cast membership and
    // reference scope are therefore two different questions.
    const cast = [
      { id: 'rob', name: 'Robot', source_id: 2, sheet_b64: 'asset:sheet' }
    ] as CompileInput['characters'];
    // 🔴 THE DEFAULT IS UNCHANGED BEHAVIOUR. Suppressing a reference is opt-in per shot, because a
    // shot that does NOT end on a well-framed subject has no identity in its carry and withholding
    // it there loses the character — measured on a shot that tilted up to the moon.
    const plain = ltx({
      characters: cast,
      clips: [clip({ id: 's0', mode: 'i2v', image_b64: 'asset:opener' }), clip({ id: 's1' })]
    });
    expect(plain.segments[0].character_refs).toHaveLength(1);
    expect(plain.segments[1].character_refs).toHaveLength(1);

    const { segments } = ltx({
      characters: cast,
      clips: [
        clip({ id: 's0', mode: 'i2v', image_b64: 'asset:opener', reference_mode: 'auto' }),
        clip({ id: 's1', reference_mode: 'auto' }),
        clip({ id: 's2', new_scene: true, reference_mode: 'auto' }),
        clip({ id: 's3', reference_mode: 'auto' })
      ]
    });
    expect(segments[0].character_refs).toHaveLength(1); // shot 0 opens the film
    expect(segments[1].character_refs).toBeUndefined(); // plain continuation — the carry holds it
    expect(segments[2].character_refs).toHaveLength(1); // a cut carries nothing in
    expect(segments[3].character_refs).toBeUndefined(); // continues the cut's shot
  });

  it('honours the per-shot reference override in both directions', () => {
    // `always` is the camera-left-the-subject case: the carry is sky, so the reference is the only
    // thing holding identity and withholding it dissolves the character. `never` is the escape
    // hatch for a shot that must not be conditioned at all.
    const cast = [
      { id: 'rob', name: 'Robot', source_id: 2, sheet_b64: 'asset:sheet' }
    ] as CompileInput['characters'];
    const { segments } = ltx({
      characters: cast,
      clips: [
        clip({ id: 's0', mode: 'i2v', image_b64: 'asset:opener' }),
        clip({ id: 's1', reference_mode: 'auto' }),
        clip({ id: 's2', reference_mode: 'never' })
      ]
    });
    // `auto` on a shot whose previous shot opened a scene and carried the character: suppressed.
    expect(segments[1].character_refs).toBeUndefined();
    expect(segments[2].character_refs).toBeUndefined();
  });

  it('sends the target dims, on-grid, flagged as the target', () => {
    expect(ltx({ width: 1919, height: 1081 })).toMatchObject({
      width: 1920,
      height: 1088,
      dims_are_target: true
    });
  });

  /** `frame_drop` and `profile` state the contract rather than relying on koblem's own defaults. */
  it('always states the profile and the project frame-drop', () => {
    expect(ltx().profile).toBe('standard');
    expect(ltx().frame_drop).toBe('auto');
  });

  it('omits project LoRAs entirely when there are none', () => {
    expect('loras' in ltx()).toBe(false);
    expect(ltx({ loras: [{ path: 'x.safetensors', multiplier: 1 }] }).loras).toHaveLength(1);
  });

  it('rounds the shot length to a VAE-valid frame count', () => {
    expect(ltx({ shotSeconds: 8, fps: 24 }).segment_frames).toBe(193);
  });
});

describe('compileTimeline — per-shot overrides', () => {
  it('emits a per-shot model only when it differs from the project one', () => {
    expect(seg({ clips: [clip({ model: 'echo-e50' })], model: undefined }).model).toBe('echo-e50');
    expect(seg({ clips: [clip({ model: 'echo-e50' })], model: 'echo-e50' }).model).toBeUndefined();
  });

  /** `undefined` is INHERIT. Conflating it with "off" would silently switch the trim off on every
   *  shot of a project whose default is auto. */
  it('keeps inherit, off and auto as three distinct frame-drop states', () => {
    expect('frame_drop' in seg({ clips: [clip()] })).toBe(false);
    expect(seg({ clips: [clip({ frame_drop: 'off' })] }).frame_drop).toBe('off');
    expect(seg({ clips: [clip({ frame_drop: 'auto' })] }).frame_drop).toBe('auto');
  });

  it('emits a per-shot LoRA set only when it is non-empty', () => {
    expect('loras' in seg({ clips: [clip({ loras: [] })] })).toBe(false);
    expect(seg({ clips: [clip({ loras: [{ path: 'l', multiplier: 1 }] })] }).loras).toHaveLength(1);
  });

  it('emits sampling overrides only when they are set and in range', () => {
    expect(seg({ clips: [clip({ seed: -1, steps: 0, cfg: -1 })] })).not.toHaveProperty('seed');
    const s = seg({ clips: [clip({ seed: 7, steps: 12, cfg: 1.5, negative_prompt: ' blur ' })] });
    expect(s).toMatchObject({ seed: 7, steps: 12, cfg: 1.5, negative_prompt: 'blur' });
  });

  it('carries this shot own audio with a defaulted kind', () => {
    expect(seg({ clips: [clip({ audio_b64: 'ref' })] })).toMatchObject({
      audio_b64: 'ref',
      audio_kind: 'voice'
    });
  });
});

describe('compileTimeline — scene images, pins and continuity', () => {
  it('ships an i2v scene image as an upload reference', () => {
    expect(seg({ clips: [clip({ mode: 'i2v', image_b64: 'img' })] }).image).toEqual({
      kind: 'upload'
    });
    expect(
      compileTimeline(input({ clips: [clip({ mode: 'i2v', image_b64: 'img' })] })).segImageRefs
    ).toEqual(['img']);
  });

  /** Without the injected frame-0 pin the keyframes win and the scene image is silently dropped,
   *  leaving a model-invented opening. */
  it('injects the i2v scene image as the frame-0 pin when the shot also has keyframes', () => {
    const c = clip({
      mode: 'i2v',
      image_b64: 'img',
      keyframes: [{ image_b64: 'k', seconds: 2 }]
    });
    expect(seg({ clips: [c] }).keyframes).toEqual([
      { image_b64: 'img', seconds: 0 },
      { image_b64: 'k', seconds: 2 }
    ]);
    // The image rides inline as the pin, so it must NOT also be shipped as a separate file.
    expect(compileTimeline(input({ clips: [c] })).segImageRefs).toEqual([null]);
  });

  it('does not double up when a pin already sits at frame 0', () => {
    const c = clip({ mode: 'i2v', image_b64: 'img', keyframes: [{ image_b64: 'k', seconds: 0 }] });
    expect(seg({ clips: [c] }).keyframes).toEqual([{ image_b64: 'k', seconds: 0 }]);
  });

  it('clamps a pin left behind by a shortened shot', () => {
    const c = clip({ seconds: 2, keyframes: [{ image_b64: 'k', seconds: 9 }] });
    expect(seg({ clips: [c] }).keyframes).toEqual([{ image_b64: 'k', seconds: 2 }]);
  });

  it('emits pin strength only when it is off exact', () => {
    const pinned = { mode: 'i2v' as const, image_b64: 'img' };
    expect('pin_strength' in seg({ clips: [clip({ ...pinned, pin_strength: 1 })] })).toBe(false);
    expect(seg({ clips: [clip({ ...pinned, pin_strength: 0.8 })] }).pin_strength).toBe(0.8);
  });

  /** Shot 0 is always fresh, so flagging it would be noise in the request. */
  it('flags a hard cut only past the first shot', () => {
    const cut = [clip({ id: 'a', new_scene: true }), clip({ id: 'b', new_scene: true })];
    const segs = ltx({ clips: cut }).segments;
    expect('scene_cut' in segs[0]).toBe(false);
    expect(segs[1].scene_cut).toBe(true);
  });
});

describe('compileTimeline — v2v', () => {
  it('falls back to plain t2v when a v2v shot has no source', () => {
    expect('mode' in seg({ clips: [clip({ mode: 'v2v_edit' })] })).toBe(false);
  });

  /** The compiler and both shot inspectors read ONE default, or the slider reads out a strength the
   *  render does not use — which is exactly what an untouched v2v_edit shot used to do (0.45 shown,
   *  0.50 rendered). Pinned by value as well as by identity so the shared constant cannot drift. */
  it('defaults edit and restyle strengths to the value the inspectors read out', () => {
    expect(seg({ clips: [clip({ mode: 'v2v_edit', v2v_source_b64: 's' })] }).v2v_strength).toBe(
      v2vStrengthDefault('v2v_edit')
    );
    expect(seg({ clips: [clip({ mode: 'v2v_restyle', v2v_source_b64: 's' })] }).v2v_strength).toBe(
      v2vStrengthDefault('v2v_restyle')
    );
    /**
     * 🔴 Pinned by VALUE, and the values changed on 2026-08-14. The distilled schedule is
     * front-loaded — 0.5 resolves to sigma 0.975, very nearly a fresh generation — so the old
     * 0.45 / 0.60 pair, tuned against LTX-2.3, was two positions that both meant "re-render it".
     * Both defaults now sit in the band that still edits, and both land on the 1/8 grid the
     * sampler can actually reach.
     */
    expect([v2vStrengthDefault('v2v_edit'), v2vStrengthDefault('v2v_restyle')]).toEqual([
      0.25, 0.375
    ]);
    for (const s of [v2vStrengthDefault('v2v_edit'), v2vStrengthDefault('v2v_restyle')]) {
      expect(Number.isInteger(s / V2V_STRENGTH_STEP)).toBe(true);
    }
  });

  it('an authored strength always wins over the default', () => {
    expect(
      seg({ clips: [clip({ mode: 'v2v_edit', v2v_source_b64: 's', v2v_strength: 0.9 })] })
        .v2v_strength
    ).toBe(0.9);
  });

  /**
   * 🔴 A v2v shot CANNOT also be a continuation, and only the compiler can say so on the wire.
   * The source is VAE-encoded over the init latent, which is exactly what the carry writes into;
   * the plugin refuses the pair with a 400 rather than obliterating one of them the way the
   * retired C++ engine did. Unlike an i2v or keyframed shot — whose frame-0 guide clears the
   * continuation inside the engine — a v2v source is not a guide, so nothing downstream clears
   * it and an unmarked v2v shot past the first fails EVERY time.
   *
   * `startsFresh` has said a v2v shot starts fresh since long before this; the compiler's
   * `else if` chain simply never reached the branch that emits it.
   */
  it('cuts a sourced v2v shot out of the continuation chain', () => {
    const segs = ltx({
      clips: [clip({ id: 'a' }), clip({ id: 'b', mode: 'v2v_edit', v2v_source_b64: 's' })]
    }).segments;
    expect(segs[1].mode).toBe('v2v_edit');
    expect(segs[1].scene_cut).toBe(true);
    // Shot 0 is fresh by definition, so the marker there would be noise on the wire.
    expect('scene_cut' in seg({ clips: [clip({ mode: 'v2v_edit', v2v_source_b64: 's' })] })).toBe(
      false
    );
  });

  /**
   * 🔴 The one v2v source LTX-2.5 still answers 501 to: a banked latent becomes the sampler's
   * `latent_image` and `Ltx25LatentBankLoad` does not restore `downscale_ratio_spacial`. So the
   * shot has no source it can use and falls through to plain t2v — the same landing as a v2v shot
   * nobody attached anything to, which is what `v2vMissingSource` already warns about.
   */
  it('never compiles a prior-render source on an engine that refuses one', () => {
    const c = clip({
      mode: 'v2v_edit',
      v2v_source_kind: 'prior_render',
      v2v_prior_job_id: 'engine-1',
      v2v_prior_segment: 2
    });
    const s = seg({ clips: [c] });
    expect('mode' in s).toBe(false);
    expect('v2v_source' in s).toBe(false);
    expect(compileTimeline(input({ clips: [c] })).segV2vRefs).toEqual([null]);
  });

  it('ships an uploaded source as a per-shot file', () => {
    const c = clip({ mode: 'v2v_restyle', v2v_source_b64: 'vid' });
    expect(compileTimeline(input({ clips: [c] })).segV2vRefs).toEqual(['vid']);
  });
});

/**
 * The one answer to "what does this shot render FROM", shared by the compiler, the inspector and
 * the pre-render warning — so a shot the editor calls sourced cannot be one the request calls bare.
 *
 * Both sides of the capability are exercised here because the compiler can only ever see one of
 * them: `v2vPriorRenderSource` is false on every live engine, and the branch it gates is what comes
 * back the day `Ltx25LatentBankLoad` restores `downscale_ratio_spacial`.
 */
describe('v2vSourceOf', () => {
  const prior = clip({
    mode: 'v2v_edit',
    v2v_source_kind: 'prior_render',
    v2v_prior_job_id: 'engine-1',
    v2v_prior_segment: 2,
    v2v_source_b64: 'stale-upload'
  });

  it('resolves a prior render server-side where the engine can read one', () => {
    expect(v2vSourceOf(prior, true)).toEqual({
      kind: 'prior_render',
      job_id: 'engine-1',
      segment: 2
    });
  });

  /** 🔴 NOT a silent fall back to the stale upload beside it — that renders different footage. */
  it('reports no source at all where it cannot', () => {
    expect(v2vSourceOf(prior, false)).toBeNull();
  });

  it('takes an uploaded clip on either', () => {
    const up = clip({ mode: 'v2v_restyle', v2v_source_b64: 'vid' });
    expect(v2vSourceOf(up, true)).toEqual({ kind: 'upload' });
    expect(v2vSourceOf(up, false)).toEqual({ kind: 'upload' });
  });

  it('is null for a v2v shot nobody attached anything to, and for a shot that is not v2v', () => {
    expect(v2vSourceOf(clip({ mode: 'v2v_edit' }), true)).toBeNull();
    expect(v2vSourceOf(clip({ mode: 't2v', v2v_source_b64: 'vid' }), true)).toBeNull();
  });
});

describe('compileTimeline — beats', () => {
  it('drops empty beats and trims the rest', () => {
    const c = clip({
      beats: [
        { seconds: 1, text: '  he stands  ' },
        { seconds: 2, text: '   ' }
      ]
    });
    expect(seg({ clips: [c] }).beats).toEqual([{ seconds: 1, text: 'he stands' }]);
  });

  it('emits nothing when a shot has no beats with text', () => {
    expect('beats' in seg({ clips: [clip({ beats: [{ seconds: 1, text: '' }] })] })).toBe(false);
  });

  /** `label` is editor-only. Sending it would put an author's timing note into the conditioning. */
  it('never sends the editor-only beat label', () => {
    const c = clip({ beats: [{ seconds: 1, text: 'x', label: 'chorus' }] });
    expect(seg({ clips: [c] }).beats).toEqual([{ seconds: 1, text: 'x' }]);
  });

  it('clamps a beat into the shot it belongs to', () => {
    const c = clip({ seconds: 3, beats: [{ seconds: 99, text: 'x' }] });
    expect(seg({ clips: [c] }).beats).toEqual([{ seconds: 3, text: 'x' }]);
  });
});

describe('compileTimeline — quality profiles', () => {
  it('sends detail strength only for the baked recipes, and only when moved', () => {
    expect(ltx({ qualityProfile: 'high' }).upscale_detail_strength).toBeUndefined();
    expect(ltx({ qualityProfile: 'high', detailStrength: 0.7 }).upscale_detail_strength).toBe(0.7);
    // Standard has no upscale refine and Custom edits its sigmas directly.
    expect(
      ltx({ qualityProfile: 'standard', detailStrength: 0.7 }).upscale_detail_strength
    ).toBeUndefined();
  });

  /** An emptied Custom stage list is base-only. Leaving the ratio on would make the backend's
   *  single-hires path apply a ×2 the editor no longer shows. */
  it('forces the single-hires ratio off under Custom', () => {
    const request = ltx({ qualityProfile: 'custom', upscalerRatio: 'x2', customStages: [] });
    expect(request.upscaler_ratio).toBe('none');
    expect(request.hires_chain).toEqual([]);
  });

  it('maps a Custom stage ratio key onto the engine upscaler name', () => {
    const request = ltx({
      qualityProfile: 'custom',
      customStages: [{ ...newStage(), upscaler: 'x1.5' }]
    });
    expect(request.hires_chain?.[0].upscaler).toBe('ltx-2.3-spatial-upscaler-x1.5-1.0');
  });

  it('leaves the recipe to the backend outside Custom', () => {
    const request = ltx({ qualityProfile: 'high' });
    expect(request.hires_chain).toBeUndefined();
    expect(request.base_custom_sigmas).toBeUndefined();
    expect(request.upscaler_ratio).toBe('x2');
  });
});

describe('compileTimeline — resume', () => {
  it('quotes the engine dir id only when one is supplied', () => {
    expect('resume_job_id' in ltx()).toBe(false);
    expect(ltx({ resumeJobId: 'engine-7' }).resume_job_id).toBe('engine-7');
  });
});

/**
 * 🔴 THE OTHER HALF OF EVERY LTX-2.5 CAPABILITY FLAG.
 *
 * A flag in `engine.ts` is a switch on a CONTROL. Three of them were flipped to match what the
 * ltx25 plugin can genuinely do, and each one swapped a working control for an H3 one that writes
 * an H3-only document field — a field `compileTimeline` reads inside `compileH3Timeline` and
 * nowhere else. `engine.test.ts` asserts the flag values; these assert what the flags decide, so
 * the pair fails rather than the UI silently compiling a field to nothing.
 */
describe('what an LTX-2.5 request is actually compiled from', () => {
  const ltx25 = (over: Partial<CompileInput> = {}) => ltx({ engine: 'ltx25', ...over });

  /** `loraIdentity: 'path'` — the picker that works. Its `[{path, multiplier}]` is what the plugin
   *  parses, and the offered names are already bare because 2.5's listing is always `auto`. */
  it('carries the LTX LoRA list, which is the only LoRA field it reads', () => {
    const runtime = [{ path: 'ltx25-detail.safetensors', multiplier: 0.8 }];
    expect(ltx25({ loras: runtime }).loras).toEqual(runtime);
  });

  /** `loraIdentity: 'filename'` would draw `H3RenderKnobs`, whose `h3Loras` / `h3LoraMultipliers` /
   *  `h3Steps` are read by `compileH3Timeline` alone — so a 2.5 project would compile without the
   *  adapters the user attached, and with an H3 steps knob on an engine whose `steps` is false. */
  it('reads none of the H3 LoRA or step fields', () => {
    const request = ltx25({
      h3Loras: ['h3-turbo.safetensors'],
      h3LoraMultipliers: [1],
      h3Steps: 8
    });
    expect(request.loras).toBeUndefined();
    expect(request.steps).toBeUndefined();
    expect(JSON.stringify(request)).not.toContain('h3-turbo');
  });

  /** `endFrame: true` would draw H3's Last-frame dropzone, bound to `clip.h3_end_image`. 2.5 can
   *  pin a closing frame as a KEYFRAME at the final index — but not through that field, so a
   *  closing still attached there is dropped on the floor at compile time. */
  it('drops h3_end_image entirely, so the H3 last-frame dropzone would render nothing', () => {
    const request = ltx25({
      clips: [clip({ h3_end_image: 'data:image/png;base64,AAAA' })]
    });
    expect(JSON.stringify(request)).not.toContain('AAAA');
  });

  /** `generatesAudio: true` is a fact about the engine and gates nothing here — the mode it used to
   *  draw is `h3SoundtrackMode`, read only by `compileH3Timeline`. */
  it('reads no soundtrack mode, which is why generatesAudio cannot gate that dial', () => {
    const request = ltx25({ audioKind: 'bgm', audioB64: 'AUDIOBYTES', h3SoundtrackMode: 'keep' });
    expect(JSON.stringify(request)).not.toContain('keep');
  });

  /**
   * 🔴 `upscaleChain: false` has to reach the WIRE, or the flag only hides the picker.
   *
   * `dims_are_target` makes koblem size the base pass at target ÷ the profile's factor, so a 1080p
   * project on `standard` + `x2` submits 960×544 and relies on the refine to put it back. LTX-2.5
   * declines that refine for any shot carrying conditioning — nearly all of them — and the base
   * pass becomes the delivery, so the user picks 1080p and is handed 544p, with the picker hidden
   * so the stale value is not theirs to change.
   */
  it('asks for one pass at the target where there is no upscale chain', () => {
    expect(
      compileForDelivery('ltx25', ltx25({ qualityProfile: 'high', upscalerRatio: 'x2' }))
    ).toMatchObject({
      width: 1920,
      height: 1088,
      dims_are_target: true,
      profile: 'standard',
      upscaler_ratio: 'none'
    });
  });
});

/**
 * 🔴 THE SAVED DOCUMENT IS NOT THE DISPATCH, and one `compile()` feeds both.
 *
 * `buildTimelineDoc` embeds this request and `readTimelineDoc` reads the profile and the upscaler
 * back OUT of it — that request is where those two settings are stored. So coercing them inside the
 * compile meant one autosave on LTX-2.5 rewrote an LTX-2.3 project's High ×2 ladder to
 * `standard` / `none`, and switching back found it gone with nothing having said so.
 */
describe('the engine ladder is imposed on the dispatch, not on the document', () => {
  const authored = { qualityProfile: 'high', upscalerRatio: 'x2' } as const;

  it('compiles the ladder the author chose, whatever engine is selected', () => {
    expect(compileTimeline(input({ engine: 'ltx25', ...authored })).request).toMatchObject({
      profile: 'high',
      upscaler_ratio: 'x2'
    });
  });

  it('drops it only on the way to an engine that cannot honour it', () => {
    const doc = compileTimeline(input({ engine: 'ltx25', ...authored })).request;
    expect(compileForDelivery('ltx25', doc)).toMatchObject({
      profile: 'standard',
      upscaler_ratio: 'none'
    });
    // …and does not reach back into the document it was derived from.
    expect(doc).toMatchObject({ profile: 'high', upscaler_ratio: 'x2' });
  });

  /**
   * The THIRD field, and the one `toMatchObject` cannot speak for: `apply_engine_delivery_limits`
   * on the server clears `hires_chain` as well, and only a Custom profile ever emits one — so a
   * test on `high` (which emits none) proves nothing about the field either side clears. The two
   * paths dispatch the same document; a rule stated on one of them is half a rule.
   */
  it('clears the custom ladder too, exactly as the server path does', () => {
    const doc = compileTimeline(input({ engine: 'ltx25', qualityProfile: 'custom' })).request;
    expect(ltxRequestOf(doc)?.hires_chain).toHaveLength(1);
    const sent = ltxRequestOf(compileForDelivery('ltx25', doc));
    expect(sent?.hires_chain).toBeUndefined();
    // Not by rewriting the document it came from — the ladder is the author's, on the way back.
    expect(ltxRequestOf(doc)?.hires_chain).toHaveLength(1);
  });

  /** H3 compiles a different request shape entirely and has no ladder fields to coerce. */
  it('leaves an H3 request untouched', () => {
    const doc = compileTimeline(input({ engine: 'h3', width: 1344, height: 768 })).request;
    expect(compileForDelivery('h3', doc)).toBe(doc);
  });
});

describe('compileH3Timeline — the generation contract', () => {
  const gen = (over: Partial<CompileInput> = {}, i = 0) => h3req(over).generations[i];

  /**
   * 🔴 The reframe, pinned. One clip is one generation is one render — and a render holding two
   * moments says so in its own prompt, as two `[Shot N]` lines inside a single block, which the
   * model renders itself with `<scenetrans>` carrying the audio across the cut. The cut time is
   * written in that text and nowhere else; the compiler ships the block verbatim rather than
   * re-deriving anything from `clip.seconds`.
   */
  it('sends one generation per shot, with the cuts stated inside its own prompt', () => {
    const block =
      'integrated_multimodal_description:\n[Shot 1] a harbour at dawn\n\n' +
      '[Shot 2] At 00:04.000, the camera cuts to the captain turning.';
    const req = h3req({
      clips: [clip({ id: 'a', prompt: 'a harbour at dawn', seconds: 8 })],
      h3Generations: [{ id: 'g1', clip_ids: ['a'], prompt: block }]
    });
    expect(req.generations).toHaveLength(1);
    expect(req.generations[0].prompt).toBe(block);
    // Advisory, and now trivially so: a generation packs one shot, and the authority on where its
    // cuts fall is the `[Shot N] At MM:SS.mmm` text above.
    expect(req.generations[0].shots).toEqual([{ index: 0, at_seconds: null }]);
  });

  /**
   * 24 fps is a model fact, and a generation's frames snap UP onto `17n+5`.
   *
   * 4 s = 96 frames, which is off the grid and rounds up to 107. That is the GRID doing it, not a
   * floor: 2 s stays 2 s (48 → 56), where it used to be pushed to 107 in silence.
   */
  it('states 24 fps and snaps the generation onto 17n+5', () => {
    const req = h3req({ clips: [clip({ seconds: 4 })], fps: 30 });
    expect(req.fps).toBe(24);
    expect(req.generations[0].frames).toBe(107);
    expect(req.width).toBe(1344);

    const short = h3req({ clips: [clip({ seconds: 2 })], fps: 30 });
    expect(short.generations[0].frames).toBe(56);
  });

  /**
   * The engine does not 400 on these — it drops unrecognised keys SILENTLY and renders the
   * default, so nobody finds out. The type makes them unnameable; this test makes sure no
   * configuration smuggles one back in.
   */
  it('never emits a field H3 does not have, however the project is configured', () => {
    const req = h3req({
      negativePrompt: 'blurry',
      cfgScale: 3.5,
      loras: [{ path: 'x.gguf', multiplier: 1 }],
      model: 'echo-full',
      qualityProfile: 'custom',
      upscalerRatio: 'x2',
      audioKind: 'voice',
      frameDrop: 'auto',
      resumeJobId: 'job_ltx',
      clips: [
        clip({ mode: 'v2v_restyle', beats: [{ seconds: 1, text: 'cut' }], pin_strength: 0.7 })
      ]
    }) as unknown as Record<string, unknown>;
    for (const forbidden of [
      'segments',
      'negative_prompt',
      'cfg_scale',
      'loras',
      'model',
      'hires_chain',
      'upscaler_ratio',
      'audio_source',
      'frame_drop',
      'resume_job_id',
      'profile',
      // Flow shift is FIXED at 12.0 on the WanGP contract. The pair used to be written into the
      // request and never read back on load, so reopening a project lost them — deleted rather
      // than round-tripped, because the field no longer exists on the wire at all.
      'sigma_shift_video',
      'sigma_shift_audio'
    ]) {
      expect(forbidden in req).toBe(false);
    }
  });

  /**
   * The other half of `engine.test.ts`'s `perShotSampling` pair: the four per-clip override fields
   * are `compileSegment`'s and nothing carries them onto an H3 generation. `packGenerations` does
   * not copy them, so a per-shot seed box drawn on H3 would write a field no compiler reads.
   */
  it('carries a clips own sampling overrides on LTX and drops them on H3', () => {
    const over = clip({ seed: 7, steps: 12, cfg: 3, negative_prompt: 'blurry' });

    const seg = ltx({ clips: [over] }).segments[0];
    expect(seg.seed).toBe(7);
    expect(seg.steps).toBe(12);
    expect(seg.cfg).toBe(3);
    expect(seg.negative_prompt).toBe('blurry');

    const gen = h3req({ clips: [over] }).generations[0] as unknown as Record<string, unknown>;
    for (const forbidden of ['seed', 'steps', 'cfg', 'negative_prompt']) {
      expect(forbidden in gen).toBe(false);
    }
  });

  /** `fl2va` and `ref2va` are different weight sets, so the packer isolates a pinned shot — and
   *  the pinned generation carries keyframes instead of references, with its alignment line. */
  it('splits a frame-pinned shot into its own fl2va generation', () => {
    const req = h3req({
      clips: [clip({ id: 'a' }), clip({ id: 'b', mode: 'i2v', image_b64: 'still' })],
      h3References: [{ id: 'r', kind: 'image', image: 'p' }]
    });
    expect(req.generations.map((g) => g.task)).toEqual(['ref2va', 'fl2va']);
    expect(req.generations[1].first_frame).toBe('still');
    expect('references' in req.generations[1]).toBe(false);
    expect(req.generations[1].prompt.startsWith('For the target video, at 0.00 seconds')).toBe(
      true
    );
  });

  /**
   * Order assigns each reference's ordinal AND advances a shared rotary clock, so nothing may sort
   * or group the array. Scoping narrows it per generation instead — which is the granularity the
   * conditioner has, since it builds one presentation per call.
   */
  it('narrows references per generation without reordering them', () => {
    const req = h3req({
      clips: [clip({ id: 'a' }), clip({ id: 'b' })],
      h3References: [
        { id: 'a1', kind: 'audio', audio: 'wav' },
        { id: 'i1', kind: 'image', image: 'img', segments: [1] },
        { id: 'i2', kind: 'image', image: 'img2' }
      ]
    });
    expect(req.generations[0].references?.map((r) => r.kind)).toEqual(['audio', 'image']);
    expect(req.generations[1].references?.map((r) => r.kind)).toEqual(['audio', 'image', 'image']);
    // The editor-only fields are not the model's business — and `label` in particular would imply
    // a reference can be addressed by name when only its ordinal can. `segments` is gone too: the
    // scope is now expressed by WHICH generation carries the reference.
    const first = req.generations[0].references![0] as unknown as Record<string, unknown>;
    for (const gone of ['id', 'label', 'segments']) expect(gone in first).toBe(false);
  });

  /** A boundary is the only place a seam can appear, so only a boundary carries a continuation —
   *  and the first generation omits the field rather than claiming `mode: "none"`. */
  it('attaches a continuation to every generation but the first', () => {
    const req = h3req({
      clips: [clip({ id: 'a' }), clip({ id: 'b' })],
      h3Generations: [
        { id: 'g0', clip_ids: ['a'] },
        { id: 'g1', clip_ids: ['b'], continuation: { mode: 'frame', k: 3 } }
      ]
    });
    expect('continuation' in req.generations[0]).toBe(false);
    expect(req.generations[1].continuation).toEqual({
      mode: 'frame',
      k: 3,
      anchor: true,
      from: 'prev'
    });
  });

  /** The authored six sections are per GENERATION and reach the model through the prompt only. */
  it('composes the stored sections into the block that is sent', () => {
    const g = gen({
      clips: [clip({ id: 'a', prompt: 'the captain at the rail' })],
      h3References: [{ id: 'r', kind: 'image', image: 'p', label: 'Ines' }],
      h3Generations: [
        {
          id: 'g0',
          clip_ids: ['a'],
          task_types: ['reference generation'],
          style_opening: 'The target video is in realistic photographic style.',
          subject_definitions: [{ label: '<Subject 1>', text: '<Subject 1> is the captain.' }],
          summary: 'The captain waits at the rail.',
          retention_analysis: [
            { label: '<Subject 1>', marker: 'fully_preserved', text: 'identity retained' }
          ],
          overall_soundscape: 'Gulls and water.',
          non_diegetic_music: 'N/A'
        }
      ]
    });
    expect(g.task).toBe('ref2va');
    expect(g.prompt).toContain('summary:\n[reference generation] The captain waits at the rail.');
    expect(g.prompt).toContain(
      'detailed_description:\nThe target video is in realistic photographic style.\n[Shot 1] the captain at the rail'
    );
    expect(g.prompt).toContain('non_diegetic_music:\nN/A');
  });

  /** Off by default: `generate_preview` decodes latents per emission and the cost is unmeasured,
   *  so it must not silently tax a production render. */
  it('asks for preview frames only when the project opted in', () => {
    expect('preview' in h3req()).toBe(false);
    expect(h3req({ h3Preview: true }).preview).toBe(true);
  });

  /**
   * 🔴 `with_audio` is what earns a reference clip's own soundtrack an `<Audio k>` ordinal — and it
   * takes that number BEFORE the clip's `<Video k>` (`pipeline.py:249-254`), which renumbers every
   * later audio label the prompt addresses. It is a CHOICE, not an inference: a reference video
   * does not create an `<Audio N>` just because the file has sound (`GUIDE-ref:112`). Both this and
   * `tail_frames` are video-only, and koblem refuses either on an image or audio reference by name.
   */
  it('forwards with_audio and tail_frames, and only on a video reference', () => {
    const refs = gen({
      clips: [clip({ id: 'a' })],
      h3References: [
        { id: 'v', kind: 'video', video: 'mp4', with_audio: true, tail_frames: 73 },
        // Nonsense on an image, and silently harmful if it were honoured — every later `<Audio n>`
        // would shift by one and the prompt would be describing a different asset.
        { id: 'i', kind: 'image', image: 'png', with_audio: true, tail_frames: 56 }
      ]
    }).references!;
    expect(refs[0]).toMatchObject({ kind: 'video', with_audio: true, tail_frames: 73 });
    const image = refs[1] as unknown as Record<string, unknown>;
    expect('with_audio' in image).toBe(false);
    expect('tail_frames' in image).toBe(false);
  });

  /**
   * The turbo LoRA is worth −39% wall at 8 steps for zero extra VRAM, so the field has to survive
   * compilation — but the multipliers are matched POSITIONALLY, and a shorter list is legal and
   * pads with 1.0. Absent lists emit no key at all: on this engine a key nobody set is the
   * difference between "the default" and "somebody chose the default".
   */
  it('forwards the generation LoRAs and their multipliers, and omits both when unset', () => {
    const g = gen({
      clips: [clip({ id: 'a' })],
      h3Generations: [
        {
          id: 'g0',
          clip_ids: ['a'],
          steps: 8,
          loras: ['h3-turbo-4step-ema-pruned.safetensors'],
          lora_multipliers: [0.9]
        }
      ]
    });
    expect(g.loras).toEqual(['h3-turbo-4step-ema-pruned.safetensors']);
    expect(g.lora_multipliers).toEqual([0.9]);
    // The LoRA does not lower the step count itself — that is the caller's, and it is where the
    // whole win lives.
    expect(g.steps).toBe(8);
    const bare = gen({ clips: [clip({ id: 'a' })] }) as unknown as Record<string, unknown>;
    for (const gone of ['loras', 'lora_multipliers', 'steps']) expect(gone in bare).toBe(false);
  });

  /**
   * The stored block IS the request's prompt. Sent VERBATIM — no section set, no ordinals, no
   * cut-time arithmetic, no length check — because the panels already edited these exact bytes.
   *
   * 🔴 The blank case is the one worth pinning, and it is not tidiness: an empty or all-whitespace
   * `prompt` falls back to the composed block rather than shipping nothing, because **koblem drops
   * an empty-prompt generation from the chain**. Honouring a cleared box would make this generation
   * VANISH from the render instead of failing, which is the least debuggable outcome available.
   * The composer says the same thing on screen.
   */
  it('sends the stored prompt verbatim, and falls back to the composed block when it is blank', () => {
    const authored = {
      id: 'g0',
      clip_ids: ['a'],
      overall_soundscape: 'Gulls and water.',
      non_diegetic_music: 'N/A'
    };
    const hand = gen({
      clips: [clip({ id: 'a', prompt: 'the captain at the rail' })],
      h3Generations: [{ ...authored, prompt: 'integrated_multimodal_description: mine' }]
    });
    expect(hand.prompt).toBe('integrated_multimodal_description: mine');
    // …and nothing the panels hold leaks into it.
    expect(hand.prompt).not.toContain('Gulls and water');

    for (const blank of ['', '   \n  ']) {
      const composed = gen({
        clips: [clip({ id: 'a', prompt: 'the captain at the rail' })],
        h3Generations: [{ ...authored, prompt: blank }]
      });
      expect(composed.prompt).toContain('the captain at the rail');
      expect(composed.prompt).toContain('overall_soundscape: Gulls and water.');
    }
  });

  /** A generation nobody has opened has no `prompt` at all, and must compile to what it always did. */
  it('composes from the stored fields when the generation has no prompt yet', () => {
    const fresh = gen({
      clips: [clip({ id: 'a', prompt: 'the captain at the rail' })],
      h3Generations: [
        {
          id: 'g0',
          clip_ids: ['a'],
          overall_soundscape: 'Gulls and water.',
          non_diegetic_music: 'N/A'
        }
      ]
    });
    expect(fresh.prompt).toContain('[Shot 1] the captain at the rail');
    expect(fresh.prompt).toContain('overall_soundscape: Gulls and water.');
  });

  /**
   * ## Project defaults for LoRAs and steps
   *
   * 🔴 There is no project level ON THE WIRE — `contract.py: Generation` carries `loras`,
   * `lora_multipliers` and `steps`, and the request around them carries none of the three. So "the
   * whole project renders at 8 steps with the turbo LoRA" exists only as an expansion the compiler
   * performs, and these tests are the only thing that pins it. A regression here is silent in the
   * worst way: the render succeeds, at 20 steps, costing double.
   */
  describe('project LoRA/step defaults', () => {
    const TURBO = 'h3-turbo-4step-ema-pruned.safetensors';

    it('writes the project default into a generation that names none of its own', () => {
      const g = gen({ h3Loras: [TURBO], h3Steps: 8 });
      expect(g.loras).toEqual([TURBO]);
      expect(g.steps).toBe(8);
    });

    /** The whole point of it being a DEFAULT: what an author typed on the card outranks it. */
    it("does not overwrite a generation's own LoRAs or steps", () => {
      const g = gen({
        h3Loras: [TURBO],
        h3Steps: 8,
        h3Generations: [{ id: 'g0', clip_ids: ['a'], loras: ['other.safetensors'], steps: 20 }]
      });
      expect(g.loras).toEqual(['other.safetensors']);
      expect(g.steps).toBe(20);
    });

    /**
     * 🔴 The two halves are independent. A project that sets only the step count must not have an
     * empty `loras` key appear, and vice versa — an empty list is not the same as an absent one to
     * the shim's coherence check.
     */
    it('applies each half independently', () => {
      const stepsOnly = gen({ h3Steps: 8 });
      expect(stepsOnly.steps).toBe(8);
      expect(stepsOnly.loras).toBeUndefined();
      const lorasOnly = gen({ h3Loras: [TURBO] });
      expect(lorasOnly.loras).toEqual([TURBO]);
      expect(lorasOnly.steps).toBeUndefined();
    });

    /**
     * 🔴 Multipliers follow whichever LIST won. Inheriting the project's multipliers onto a
     * generation's own adapters would apply them positionally to different files — the same rank of
     * silent corruption as a mismatched-length list, and it renders rather than failing.
     */
    it('never lands the project multipliers on a generation’s own LoRAs', () => {
      const g = gen({
        h3Loras: [TURBO],
        h3LoraMultipliers: [0.6],
        h3Generations: [{ id: 'g0', clip_ids: ['a'], loras: ['other.safetensors'] }]
      });
      expect(g.loras).toEqual(['other.safetensors']);
      expect(g.lora_multipliers).toBeUndefined();
    });

    it('carries the project multipliers with the project LoRAs, truncated to them', () => {
      const g = gen({ h3Loras: [TURBO], h3LoraMultipliers: [0.6, 0.4] });
      expect(g.loras).toEqual([TURBO]);
      expect(g.lora_multipliers).toEqual([0.6]);
    });

    /** A project that never opened the picker compiles to the request it always did. */
    it('emits neither field when the project sets neither', () => {
      const g = gen();
      expect(g.loras).toBeUndefined();
      expect(g.lora_multipliers).toBeUndefined();
      expect(g.steps).toBeUndefined();
    });
  });

  /**
   * ## There is no per-generation canvas
   *
   * 🔴 There was, and it was a footgun rather than a feature. One shot at 1280×720 beside another
   * at 1920×1080 is a cut that cannot be concatenated — and because the noise is drawn at the
   * latent SHAPE, an override also silently changed what a PINNED SEED produced, which is the one
   * thing pinning a seed is for.
   *
   * The field still exists on the document, so old projects carry values. These pin that they are
   * dead data: the request-level `width`/`height` govern every generation.
   */
  describe('per-generation canvas', () => {
    it('is never emitted', () => {
      expect(gen().width).toBeUndefined();
      expect(gen().height).toBeUndefined();
    });

    it('ignores a value left on an older document', () => {
      const g = gen({
        h3Generations: [{ id: 'g0', clip_ids: ['a'], width: 1536, height: 672 }]
      });
      expect(g.width).toBeUndefined();
      expect(g.height).toBeUndefined();
    });
  });

  /**
   * ## `continuation.from`
   *
   * 🔴 The compiler used to append `from: 'prev'` to the spread unconditionally, which clobbered an
   * explicit latent-bank key on its way to a backend that goes to some trouble to honour one
   * (`h3_single_generation_chain` forwards a non-`prev` key verbatim). "Continue from take 2" was
   * therefore unreachable by construction. These pin both halves: the default still lands, and an
   * explicit key survives.
   */
  describe('continuation.from', () => {
    /**
     * 🔴 Two 4 s shots pack into ONE generation and there is then no continuation to test at all —
     * a vacuously-passing test here would be worse than none, because the field it is guarding is
     * emitted only on a boundary. 14 s each forces two generations.
     */
    const two = () => [clip({ id: 'a', seconds: 14 }), clip({ id: 'b', seconds: 14 })];
    const chained = () => h3req({ clips: two() }).generations;

    it('defaults to prev when the document names no bank', () => {
      const gens = chained();
      expect(gens.length).toBeGreaterThan(1);
      expect(gens[1].continuation?.from).toBe('prev');
    });

    it('carries an explicit bank key through instead of overwriting it', () => {
      const { request } = compileTimeline(
        input({
          engine: 'h3',
          width: 1344,
          height: 768,
          clips: two(),
          h3Generations: [
            { id: 'g1', clip_ids: ['b'], continuation: { mode: 'latents', from: 'bank:xyz' } }
          ]
        })
      );
      if (!isH3Request(request)) throw new Error('expected an H3 request');
      const withKey = request.generations.find((g) => g.continuation?.from === 'bank:xyz');
      expect(withKey).toBeDefined();
    });

    /** Whitespace is not a key. A blank string must read as "unset", not as a bank named "". */
    it('treats a blank key as unset', () => {
      const { request } = compileTimeline(
        input({
          engine: 'h3',
          width: 1344,
          height: 768,
          clips: two(),
          h3Generations: [
            { id: 'g1', clip_ids: ['b'], continuation: { mode: 'latents', from: '   ' } }
          ]
        })
      );
      if (!isH3Request(request)) throw new Error('expected an H3 request');
      const joined = request.generations.filter((g) => g.continuation);
      expect(joined.length).toBeGreaterThan(0);
      for (const g of joined) expect(g.continuation!.from).toBe('prev');
    });
  });
});
