import { describe, expect, it } from 'vitest';
import { capabilities } from '#lib/director/engine.ts';
import {
  LTX_VIDEO_ENGINE,
  LTX_VIDEO_HISTORY_SERVICE,
  VIDEO_TABS,
  isVideoTab,
  type VideoTab
} from './videoTabs';

describe('LTX_VIDEO_ENGINE', () => {
  /** Lip-dub moved here once the ComfyUI plugin implemented `relip`; nothing is on 2.3 any more. */
  it('renders both LTX tabs on LTX-2.5', () => {
    expect(LTX_VIDEO_ENGINE).toBe('ltx25');
  });
});

describe('LTX_VIDEO_HISTORY_SERVICE', () => {
  /** `service_tag()`'s bucket — asking for the wrong one empties the panel with no error. */
  it('reads from the bucket the engine writes to', () => {
    expect(LTX_VIDEO_HISTORY_SERVICE).toBe('ltx25');
  });
});

describe('isVideoTab', () => {
  it.each(VIDEO_TABS)('accepts %s', (tab) => {
    expect(isVideoTab(tab)).toBe(true);
  });

  /** The pre-2.5 deep link. Rejected, so it falls back to the default — which is the same tab. */
  it.each(['ltx23', '', null, undefined, 'nope'])('rejects %s', (v) => {
    expect(isVideoTab(v)).toBe(false);
  });
});

/**
 * 🔴 The page draws its advanced knobs off these flags, not off the engine's name. Flip one and a
 * live control writes a field the distilled checkpoint accepts and ignores — the defect this
 * repoint existed to avoid. Both tabs read the same set now, so there is one answer to check.
 */
describe('the flags the LTX tabs gate their controls on', () => {
  it('hides everything LTX-2.5 takes and does not honour', () => {
    const caps = capabilities(LTX_VIDEO_ENGINE);
    for (const key of ['cfg', 'negativePrompt', 'customRecipe', 'upscaleChain'] as const) {
      expect(caps[key]).toBe(false);
    }
    expect(caps.modelVariants).toBe(false);
    expect(caps.resume).toBe(true);
  });
});

describe('VIDEO_TABS', () => {
  it('is the full tab set', () => {
    const tabs: VideoTab[] = ['ltx25', 'lipdub', 'upscale'];
    expect(VIDEO_TABS).toEqual(tabs);
  });
});
