/**
 * Which LTX engine the `/ltx-video` tabs talk to, and where their renders are filed.
 *
 * Both LTX tabs are on LTX-2.5. Lip-dub was the last holdout — `relip` was a 501 on the ComfyUI
 * plugin, so it stayed on the C++ 2.3 build and carried 2.3 in its name — and it moved once that
 * plugin implemented the frozen in-context reference the C++ engine's relip does. So this module is
 * one engine and one history bucket, not a per-tab lookup.
 *
 * 🔴 The bucket follows from the engine and nothing else: `LtxVideoClient::service_tag()` files an
 * `ltx25` render under `ltx25`, so a page that asks for a bucket its renders were not written to
 * shows an empty list with no error at all. Change this only with `historyServiceFor` in hand.
 */
import type { DirectorEngine } from '#lib/api/client.ts';
import { historyServiceFor } from '#lib/director/engine.ts';

export type VideoTab = 'ltx25' | 'lipdub' | 'upscale';

export const VIDEO_TABS: VideoTab[] = ['ltx25', 'lipdub', 'upscale'];

/**
 * The engine behind both `LtxVideoClient` tabs — the generate tab and Lip-dub.
 *
 * ⚠️ Not every tab on this page is one. `upscale` is LTX-2.5's own upsampler — a restorer that takes a finished mp4 and
 * gives a bigger one back, so it has no render engine and no generations bucket at all.
 */
export const LTX_VIDEO_ENGINE: DirectorEngine = 'ltx25';

export const LTX_VIDEO_HISTORY_SERVICE = historyServiceFor(LTX_VIDEO_ENGINE);

export function isVideoTab(value: string | null | undefined): value is VideoTab {
  return !!value && (VIDEO_TABS as string[]).includes(value);
}
