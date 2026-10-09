import { describe, expect, it } from 'vitest';
import {
  ENGINE_OPTIONS,
  beatsAvailable,
  H3_DEFAULT_CANVAS,
  UNAVAILABLE,
  UNAVAILABLE_LTX25,
  capabilities,
  historyServiceFor,
  readEngine,
  reframeForEngine,
  resumeAvailable,
  retakeTailNote,
  unavailableFor
} from './engine';

describe('readEngine', () => {
  it('reads both engines back', () => {
    expect(readEngine('h3')).toBe('h3');
    expect(readEngine('ltx25')).toBe('ltx25');
  });

  /**
   * A document written by a newer koblem has to OPEN on an older one. Rendering on the wrong engine
   * is visible; refusing to open the project loses the work.
   *
   * 🔴 `'ltx'` is in here on purpose. It is LTX-2.3, retired 2026-08-14, and a saved timeline can
   * still name it — so it has to resolve to a LIVE engine rather than leaving the project
   * unopenable. That is safe HERE, where the value only decides which controls to draw, and is not
   * safe for a bank or job id; `renders.test.ts` holds that half.
   */
  it('falls back to LTX-2.5 for anything it does not know, including nothing', () => {
    expect(readEngine('ltx')).toBe('ltx25');
    expect(readEngine(undefined)).toBe('ltx25');
    expect(readEngine(null)).toBe('ltx25');
    expect(readEngine('')).toBe('ltx25');
    expect(readEngine('ltx-2.5')).toBe('ltx25');
    expect(readEngine(3)).toBe('ltx25');
  });
});

describe('LTX-2.5 capabilities', () => {
  const ltx25 = capabilities('ltx25');

  /**
   * 🔴 The one that costs money if it is wrong, and it inverted when the bank landed (kobbler
   * `6ca0b6f2`). There IS a latent bank now, keyed per segment, so a resume restores the banked
   * prefix and renders only the tail. What did NOT arrive is the tail splice: a retake re-renders
   * its own segment and the ones after it are restored as they were, never re-pinned to the new
   * take's final frame — `retake.splices_tail` is `false` in the engine's own capabilities.
   */
  it('has a bank and a resume, but no tail splice', () => {
    expect(ltx25.resume).toBe(true);
    expect(ltx25.retakeSplicesTail).toBe(false);
  });

  /** Every control the distilled checkpoint accepts and ignores is hidden, not merely inert. */
  it('hides the sampling knobs the distilled checkpoint does not honour', () => {
    expect(ltx25.negativePrompt).toBe(false);
    expect(ltx25.cfg).toBe(false);
    expect(ltx25.customRecipe).toBe(false);
    // One transformer: the Echo variants were 2.3 checkpoints and are a 501 here.
    expect(ltx25.modelVariants).toBe(false);
  });

  /**
   * 🔴 A FLAG IS A SWITCH ON A CONTROL, NOT A DESCRIPTION OF THE ENGINE — the three assertions
   * below were flipped on 2026-08-13 to match what the plugin can do and broke three working
   * controls doing it. `compile.test.ts` holds the other half of each pair: that the field the
   * control writes is one `compileTimeline` reads FOR THIS ENGINE.
   *
   *  - `loraIdentity` picks the PICKER. `path` writes `[{path, multiplier}]`, which is exactly the
   *    shape `parse_loras` wants and the names are already bare (2.5's listing is always `auto`).
   *    `filename` swaps in the H3 knobs, which write `h3Loras`/`h3Steps` — H3-only document fields
   *    — and leaves `ShotDetailsPane` with no per-shot LoRA control at all.
   *  - `endFrame` draws H3's `fl2va` Last-frame dropzone bound to `clip.h3_end_image`. 2.5 can pin
   *    a closing KEYFRAME, but not through that field, so the dropzone renders nothing.
   *  - `upscaleChain` also owns the canvas: `true` re-enables `snapTargetToProfile` (which rewrites
   *    width/height onto a ladder 2.5 lacks) and suppresses `checkCanvas`'s ×32 error, which is the
   *    one rule the plugin hard-400s on.
   */
  it('gates H3-shaped controls off, whatever the plugin can do', () => {
    expect(ltx25.loraIdentity).toBe('path');
    expect(ltx25.endFrame).toBe(false);
    expect(ltx25.upscaleChain).toBe(false);
  });

  /**
   * The engine fact that IS a flag, because it gates nothing H3-shaped: 2.5 denoises a soundtrack
   * of its own. The soundtrack-mode dial it used to draw moved to `soundtrackRoute`, which is the
   * property that makes the question askable — so this may stay true without lying to a control.
   */
  it('says it writes its own soundtrack without claiming H3s soundtrack dial', () => {
    expect(ltx25.generatesAudio).toBe(true);
    expect(ltx25.soundtrackRoute).toBe('audio_source');
    expect(capabilities('h3').soundtrackRoute).toBe('audio_reference');
  });

  /**
   * 🔴 BOTH went true on 2026-08-14, and both had been false on a reason that turned out to be
   * wrong. v2v was never a control stack on the C++ engine either — it is plain SDEdit, the source
   * VAE-encoded over the init latent with the schedule entered partway — and the plugin implements
   * both `v2v_mode`s through that one path, so the two Director buttons keep meaning what they
   * meant. Beats are a first-party node (`custom_nodes/ltx25_prompt_relay`): ONE text encode with
   * an additive Gaussian bias on text cross-attention, not the mid-sample re-encode this file used
   * to claim.
   *
   * `compile.test.ts` holds the other half of each, as the flag rule demands: `compileSegment`
   * writes `mode` / `v2v_strength` / `v2v_source` and `beats` onto an LTX segment, and it is the
   * only compiler an `ltx25` project reaches.
   */
  it('has video-to-video and Prompt Relay beats', () => {
    expect(ltx25.v2v).toBe(true);
    expect(ltx25.beats).toBe(true);
  });

  /**
   * 🔴 The NARROW half of v2v, and the reason it is its own flag rather than a paragraph: a
   * BANKED-LATENT source is still a 501. It becomes the sampler's `latent_image` and
   * `Ltx25LatentBankLoad` does not restore `downscale_ratio_spacial`. Hiding it by turning `v2v`
   * off would take the upload dropzone — which works — away with it.
   */
  it('takes an uploaded source but not a prior render', () => {
    expect(ltx25.v2vPriorRenderSource).toBe(false);
    // H3 has no per-clip source field at all, so there is no narrower question to ask there.
    expect(capabilities('h3').v2v).toBe(false);
    expect(capabilities('h3').v2vPriorRenderSource).toBe(false);
  });

  /**
   * The LTX shape the editor is built around: the 8k+1 frame grid, the ×32 canvas rule and
   * chained-shot continuation. These are pinned rather than merely inherited — when LTX-2.3 was
   * retired the two records were merged into one, and a merge is exactly where a value gets
   * quietly dropped.
   */
  it('keeps the LTX geometry and continuation model', () => {
    // 8k+1.
    expect(ltx25.framePeriod).toBe(8);
    expect(ltx25.frameBase).toBe(1);
    expect(ltx25.dimMultiple).toBe(32);
    expect(ltx25.variableFps).toBe(true);
    expect(ltx25.fixedFps).toBeNull();
    expect(ltx25.chainedShots).toBe(true);
    expect(ltx25.loras).toBe(true);
    expect(ltx25.characterRefs).toBe(true);
    expect(ltx25.multiShotPerGeneration).toBe(false);
    expect(ltx25.structuredPrompt).toBe(false);
    // 🔴 A retake here re-renders one SEGMENT against the banked prefix — it is not a whole render
    // unit re-rolled the way H3's is, and a surface reading this as `true` would offer the H3
    // retake affordance on a chained engine. Deleted once; restored because deleting an assertion
    // is not how a capability changes.
    expect(ltx25.generationIsRetakeUnit).toBe(false);
    // The server answers `render-plan` with `supported: false` here, so no scope dialog and no
    // "these shots keep their takes" promise may be drawn.
    expect(ltx25.scopedFullRender).toBe(false);
    // A shot IS a render on LTX, so there is no sub-unit to mark up.
    expect(ltx25.vocab.subUnit).toBeNull();
    expect(ltx25.label).toContain('2.5');
  });

  /**
   * 🔴 The mechanism, pinned as a NEGATIVE. Every user-visible sentence about Prompt Relay in this
   * file said it "re-encodes the prompt register mid-sample"; the comfy side says flatly that it
   * does not — there is ONE text encode and the relay is an additive Gaussian bias on text
   * cross-attention. The claim was wrong in the direction that matters, because it was the whole
   * argument for why no ComfyUI node could do it.
   */
  it('never describes Prompt Relay as re-encoding the prompt mid-sample', () => {
    const prose = [
      ltx25.blurb,
      ...Object.values(UNAVAILABLE),
      ...Object.values(UNAVAILABLE_LTX25)
    ].join(' ');
    expect(prose).not.toMatch(/re-encod/i);
    expect(prose).not.toMatch(/prompt register/i);
  });

  it('leaves H3 exactly as it was', () => {
    expect(capabilities('h3').scopedFullRender).toBe(true);
    expect(capabilities('h3').generatesAudio).toBe(true);
    expect(capabilities('h3').resume).toBe(false);
  });
});

/**
 * The per-shot override card in `ShotDetailsPane`, whose four boxes write `clip.seed`,
 * `clip.steps`, `clip.cfg` and `clip.negative_prompt`.
 *
 * 🔴 TWO gates, and both are load-bearing in opposite directions. `perShotSampling` is the outer
 * one and is false on H3, where `compileSegment` never runs and the same numbers live on the
 * generation instead — `compile.test.ts` holds the other half. The three sampling flags are the
 * inner ones and are false on LTX-2.5, where the distilled checkpoint takes the fields on the wire
 * and ignores them. Drop either and a visible control writes a field that compiles to nothing.
 */
describe('per-shot sampling overrides', () => {
  it('offers the clip fields only where a compiler reads them', () => {
    expect(capabilities('ltx25').perShotSampling).toBe(true);
    expect(capabilities('h3').perShotSampling).toBe(false);
  });

  it('still hides the knobs the distilled checkpoint ignores', () => {
    const ltx25 = capabilities('ltx25');
    expect(ltx25.perShotSampling).toBe(true);
    for (const key of ['cfg', 'negativePrompt', 'customRecipe'] as const) {
      expect(ltx25[key]).toBe(false);
    }
  });
});

/**
 * 🔴 Two engines in one history bucket cannot be told apart, and asking for a bucket nothing was
 * written to shows an empty history with NO error — which is exactly how the Director's H3 history
 * came back blank. Mirrors `LtxVideoClient::service_tag()` in koblem.
 */
describe('historyServiceFor', () => {
  it('gives each engine its own bucket', () => {
    expect(historyServiceFor('h3')).toBe('minimax-h3');
    expect(historyServiceFor('ltx25')).toBe('ltx25');
  });
});

/**
 * 🔴 The capability is a CEILING and the deployment has the last word — the whole point of the
 * split. LTX-2.5 implements resume and a single-shot retake and answers 501 to both when
 * `COMFY_API_LTX_BANK_DIR` is unset, so a control drawn off `caps.resume` alone dispatches a render
 * that fails after the queue wait. And the silence has to mean different things on different
 * engines: koblem asks only an engine that could plausibly answer, so an absent flag is a NO on 2.5
 * — unreachable means there is nothing to resume from either.
 */
describe('resumeAvailable', () => {
  it('needs the live bank on LTX-2.5, and treats silence there as no', () => {
    expect(resumeAvailable('ltx25', true)).toBe(true);
    expect(resumeAvailable('ltx25', false)).toBe(false);
    expect(resumeAvailable('ltx25', undefined)).toBe(false);
    expect(resumeAvailable('ltx25', null)).toBe(false);
  });

  /** No capability, so no deployment fact can conjure one. */
  it('stays false on H3 whatever the flag says', () => {
    expect(resumeAvailable('h3', true)).toBe(false);
    expect(resumeAvailable('h3', undefined)).toBe(false);
  });
});

/**
 * The sentence every retake affordance prints, on both shells. It has to invert with
 * `retakeSplicesTail` — the same flag `take-continuity.ts` runs its cascade off — or the seed box
 * and the Joins panel make opposite claims about the same render.
 */
/**
 * 🔴 The same CEILING-vs-DEPLOYMENT split as `resumeAvailable`, and it exists for the same bill.
 * LTX-2.5 implements Prompt Relay, but the relay is a custom node ComfyUI only lists after its
 * container restarts — so a control drawn off `caps.beats` alone dispatches a shot that is refused
 * at graph build, AFTER the GPU gate has been waited on. Mirrors `beats_enabled` on the props body,
 * which koblem derives from the engine's own `/object_info` probe.
 */
describe('beatsAvailable', () => {
  it('needs the mounted relay node on LTX-2.5, and treats silence there as no', () => {
    expect(beatsAvailable('ltx25', true)).toBe(true);
    expect(beatsAvailable('ltx25', false)).toBe(false);
    expect(beatsAvailable('ltx25', undefined)).toBe(false);
    expect(beatsAvailable('ltx25', null)).toBe(false);
  });

  /** No capability, so no deployment fact can conjure one — H3's beats live in the prompt. */
  it('stays false on H3 whatever the flag says', () => {
    expect(beatsAvailable('h3', true)).toBe(false);
    expect(beatsAvailable('h3', undefined)).toBe(false);
  });

  /** The copy for the hidden case has to exist, or the control vanishes with no reason given. */
  it('explains the deployment absence rather than the engine', () => {
    const note = unavailableFor('ltx25', 'beats');
    expect(note).toContain('ltx25_prompt_relay');
    expect(note).toContain('restarts');
  });
});

describe('retakeTailNote', () => {
  it('says the tail is left alone on the engines that leave it alone', () => {
    for (const engine of ['ltx25', 'h3'] as const) {
      const note = retakeTailNote(capabilities(engine));
      expect(note).toContain('left as they were');
      expect(note).toContain('drift until the next hard cut');
    }
  });
});

describe('the engine picker', () => {
  it('offers the two live engines, LTX-2.5 first and the default', () => {
    expect(ENGINE_OPTIONS.map((o) => o.value)).toEqual(['ltx25', 'h3']);
    expect(ENGINE_OPTIONS[0].label).toBe(capabilities('ltx25').label);
  });

  /** The retired engine is not merely unlisted — it cannot be named. */
  it('has no entry for the retired LTX-2.3', () => {
    expect(ENGINE_OPTIONS.some((o) => (o.value as string) === 'ltx')).toBe(false);
    expect(ENGINE_OPTIONS.every((o) => !o.label.includes('2.3'))).toBe(true);
  });
});

/**
 * The absence has to be explained by the model the project is actually on. Printing H3's copy on
 * an LTX-2.5 project answers "where did my negative prompt go" by naming an engine that is not
 * rendering it.
 */
describe('unavailableFor', () => {
  it('explains an LTX-2.5 absence without naming MiniMax-H3', () => {
    // 🔴 `resume` is back on this list and it is the odd one out: every other key explains the
    // CHECKPOINT, this one explains a missing bank directory. It left when the bank landed (the
    // capability went true, so the control renders) and returned when the gate moved to the live
    // `bank_enabled` — a deployment without a bank dir needs the same explanation the absent
    // capability used to earn, and H3's wording would answer it by naming the wrong model.
    //
    // `upscaleChain` is on it because the control is hidden again: every Director shot carries
    // conditioning, and the plugin declines the two-stage upscaler for all of them.
    //
    // 🔴 `v2v` and `beats` LEFT this list on 2026-08-14, because both capabilities landed and the
    // controls render — a key whose control is visible is a paragraph nothing prints. What
    // replaces them is the narrower `v2vPriorRenderSource`, which is the only half still refused.
    for (const key of ['negativePrompt', 'v2vPriorRenderSource', 'resume', 'upscaleChain']) {
      const note = unavailableFor('ltx25', key);
      expect(note).not.toBe('');
      expect(note).not.toContain('MiniMax-H3');
      expect(note).toContain('LTX-2.5');
    }
  });

  it('keeps H3 on H3 copy, and never returns undefined', () => {
    expect(unavailableFor('h3', 'negativePrompt')).toContain('MiniMax-H3');
    expect(unavailableFor('ltx25', 'nothing-by-that-name')).toBe('');
  });
});

/**
 * The reframe is a fact about ARRIVING on an engine, not about one control: it used to live in the
 * picker's `onchange`, so an undo that crossed an engine change landed an LTX canvas on H3 and only
 * the settings sheet's warning ever said so.
 */
describe('reframeForEngine', () => {
  it('moves an LTX canvas onto H3s measured shape', () => {
    // 1920×1088 is legal ×32 and 2.02× the area every H3 number was measured at.
    expect(reframeForEngine('h3', 1920, 1088)).toEqual(H3_DEFAULT_CANVAS);
    // Off-grid counts too, and on its own: 720 is not a multiple of 32.
    expect(reframeForEngine('h3', 1280, 720)).toEqual(H3_DEFAULT_CANVAS);
  });

  it('leaves a canvas H3 is happy with alone', () => {
    expect(reframeForEngine('h3', H3_DEFAULT_CANVAS.w, H3_DEFAULT_CANVAS.h)).toBeNull();
    // On the grid and inside the budget — the cheap 16:9 rung out of H3's own preset list.
    expect(reframeForEngine('h3', 1152, 640)).toBeNull();
  });

  it('never reframes an engine whose canvas comes from a ladder', () => {
    expect(reframeForEngine('ltx25', 1344, 768)).toBeNull();
    expect(reframeForEngine('ltx25', 1280, 720)).toBeNull();
  });
});
