// LTX frame math — mirror of the server's `frames_from_seconds` (api/src/routes/ltx_video.rs).
// A shot never renders at exactly its nominal length: LTX rounds every shot to a VAE-valid
// `8k+1` frame count, so `segment_frames` and keyframe pins must be computed in real frames or
// the engine rejects them (e.g. an 8s@24fps shot is 193 frames = 8.04s, not 192).

/**
 * Hard ceiling on a shot's RENDER frame count — mirror of `LTX_MAX_RENDER_FRAMES`
 * (api/src/routes/ltx_video.rs), which is the authority. Keep the two equal.
 *
 * This is the measured VRAM budget at 1920x1088, NOT the engine's tiling threshold — the two are
 * different numbers and conflating them produces a false explanation. A longer shot does not fail;
 * it simply does not fit the card, and only above 248 does the DiT additionally tile the temporal
 * axis (slower, softer across the tile seam). The server refuses either way, so the UI's job is to
 * say so before the user commits minutes of GPU time to a request that will be rejected.
 */
export const MAX_RENDER_FRAMES = 241;

/** Seam headroom a CONTINUATION shot renders on top of its visible length (`8 * K`, K=2). */
export const SEAM_OVERLAP_FRAMES = 16;

/**
 * The frames a shot actually asks the DiT to sample. A continuation shot (one that flows on from
 * the previous shot rather than cutting to a new scene) pays `SEAM_OVERLAP_FRAMES` on top of its
 * visible length, so it reaches the window sooner than a scene-cut shot of the same duration.
 */
export function renderFrames(seconds: number, fps: number, isSceneCut: boolean): number {
  return framesFromSeconds(seconds, fps) + (isSceneCut ? 0 : SEAM_OVERLAP_FRAMES);
}

/**
 * Longest a shot may be, in seconds, before the server refuses it — floored to the 0.5s grid the
 * duration inputs step on, so the number shown is one the user can actually dial in. That gives up
 * a fraction of a second at the top of the range; the server, not this, is the authority.
 */
export function maxShotSeconds(fps: number, isSceneCut: boolean): number {
  const budget = MAX_RENDER_FRAMES - (isSceneCut ? 0 : SEAM_OVERLAP_FRAMES);
  return Math.floor((budget / fps) * 2) / 2;
}

/**
 * `null` when the shot fits; otherwise a ready-to-show explanation of why it will be refused.
 * Assumes the worst case (a continuation shot) unless told otherwise, so the warning appears
 * before the user discovers it by rendering.
 */
export function shotLengthError(seconds: number, fps: number, isSceneCut = false): string | null {
  const frames = renderFrames(seconds, fps, isSceneCut);
  if (frames <= MAX_RENDER_FRAMES) return null;
  const max = maxShotSeconds(fps, isSceneCut);
  return `${frames} frames exceeds the ${MAX_RENDER_FRAMES}-frame VRAM budget for this resolution — it would not fit the card. Max ${max.toFixed(1)}s at ${fps}fps; split it into two shots to go longer.`;
}

/** Round a shot's seconds up to the nearest VAE-valid `8k+1` frame count (floored at 9). */
export function framesFromSeconds(seconds: number, fps: number): number {
  const raw = Math.max(1, Math.round(seconds * fps));
  const k = Math.max(1, Math.round((raw - 1) / 8));
  return k * 8 + 1;
}

/** Index of the last frame of a shot (`framesFromSeconds - 1`). */
export function lastFrameIndex(seconds: number, fps: number): number {
  return framesFromSeconds(seconds, fps) - 1;
}

/** The shot's real rendered duration in seconds (frame count / fps). */
export function actualDuration(seconds: number, fps: number): number {
  return framesFromSeconds(seconds, fps) / fps;
}

export function frameToSeconds(frame: number, fps: number): number {
  return frame / fps;
}

/** Snap a pin's seconds to a valid frame index in `[0, lastFrame]` for a shot of `shotSeconds`. */
export function secondsToFrame(seconds: number, fps: number, shotSeconds: number): number {
  const last = lastFrameIndex(shotSeconds, fps);
  return Math.min(last, Math.max(0, Math.round(seconds * fps)));
}

export type PinPreset = 'start' | 'middle' | 'end';

/** Frame index for a Start / Middle / End preset within a shot. */
export function presetFrame(preset: PinPreset, shotSeconds: number, fps: number): number {
  const last = lastFrameIndex(shotSeconds, fps);
  if (preset === 'start') return 0;
  if (preset === 'end') return last;
  return Math.round(last / 2);
}
