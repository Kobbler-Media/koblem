/**
 * Frames of a rendered take, at arbitrary times, pulled out of the blob URL the page already holds.
 *
 * ## Why this has to exist
 *
 * The beat ribbon paints one time-proportional cell per timed beat, and each cell wants the frame
 * the shot is showing AT that beat. Nothing stores that: a render produces one video and a poster,
 * never a per-time thumbnail, and there is no endpoint that will seek one out. The only source of a
 * frame at 4.2 s is the take itself, which is already decoded in the page as the object URL behind
 * `<video src>`. So the frames are extracted client-side or the ribbon shows coloured rectangles.
 *
 * ## 🔴 Three things that make the naive version unusable
 *
 * 1. **One `<video>` per cell.** A ten-beat shot then holds ten decoders on the same blob, each
 *    demuxing from the top. {@link grabFrames} takes the whole batch on ONE element and walks the
 *    times in **ascending** order, because a forward seek continues from the decoder's current
 *    position while a backward one restarts it at the previous keyframe.
 * 2. **Re-extracting on every re-render.** The ribbon repaints on any beat edit, drag or selection
 *    change, and a repaint that re-decodes the take stutters the whole workspace. Results live in a
 *    module-level cache and {@link cachedFrame} is the synchronous peek the component paints from,
 *    so only the genuinely new times ever reach a decoder.
 * 3. **Throwing.** Every failure here is a thumbnail that is missing, never a ribbon that is
 *    missing — a stuck seek, metadata that never arrives, a tainted canvas and an abort all resolve
 *    with whatever was captured before it. Nothing in this module rejects.
 *
 * ⚠️ Times are keyed to millisecond precision ({@link roundFrameTime}). Beat times are produced by
 * summing float durations, so the same beat is `4.2` on one pass and `4.2000001` on the next; keyed
 * raw those are two cache entries, two seeks and — after enough edits — an unbounded cache of
 * near-duplicates of the same frame.
 */

/** Everything a browser needs is behind a `document` check, so the pure helpers run under node. */
const hasDom = (): boolean => typeof document !== 'undefined';

export interface GrabOptions {
  /** Longest dimension of the thumbnail, in px. Never upscales past the source. */
  width?: number;
  /** JPEG quality, 0–1. */
  quality?: number;
  signal?: AbortSignal;
}

const DEFAULT_WIDTH = 256;
const DEFAULT_QUALITY = 0.72;

/** A blob URL that never fires `loadedmetadata` is a revoked or truncated take, not a slow one. */
const METADATA_TIMEOUT_MS = 5000;
/** Per-frame. A seek this slow on a local blob is wedged; the remaining beats are still worth a go. */
const SEEK_TIMEOUT_MS = 3000;

/**
 * 🔴 The floor is deliberately not 0. Setting `currentTime` to the position the element is already
 * at aborts the seek algorithm without firing `seeked` — and a fresh element sits at exactly 0, so
 * a beat at t=0 would hang until {@link SEEK_TIMEOUT_MS} and then be dropped. One millisecond in is
 * the same frame everywhere above 1000 fps.
 */
const MIN_SEEK = 0.001;
/**
 * Seeking to exactly `duration` lands past the last frame and decodes nothing on most builds, so
 * the tail clamps a frame-and-a-bit short of the end (24 fps is the slowest thing we render).
 */
const END_GUARD = 0.04;
/**
 * After `seeked`, `currentTime` is the requested position and not the nearest keyframe. A real
 * mismatch means the event belonged to a seek we already gave up on, whose frame is the wrong one.
 */
const SEEK_TOLERANCE = 0.25;

/** ~512 × 256px JPEGs ≈ a few MB. Past this the oldest go, in insertion order. */
export const FRAME_CACHE_LIMIT = 512;

/**
 * 🔴 NUL, because the key is `url + separator + time` and the url is whatever
 * `createObjectURL` handed us. Any separator a URL can itself contain lets two takes share a key,
 * and a collision here is not a missing thumbnail — it is one shot's frame in another shot's row.
 */
const KEY_SEP = '\u0000';

const frameCache = new Map<string, string>();

/** Millisecond precision — see this module's header for why raw float times cannot be keys. */
export const roundFrameTime = (time: number): number => Math.round(time * 1000) / 1000;

/** Fixed 3 decimals so `4.2`, `4.2000001` and `4.2000` are one string rather than three. */
export const frameCacheKey = (url: string, time: number): string =>
  `${url}${KEY_SEP}${roundFrameTime(time).toFixed(3)}`;

/**
 * Where the element actually seeks for a requested time.
 *
 * `duration` is unknown until metadata lands, and beat times come from an edited timeline that can
 * outrun the take it was rendered from — a shot trimmed to 6 s still carries the beat someone wrote
 * at 8 s. Clamping inside the take gives that beat the last frame; leaving it alone gives it
 * nothing, since a seek past the end never completes.
 */
export function clampFrameTime(time: number, duration?: number): number {
  const t = Number.isFinite(time) ? Math.max(MIN_SEEK, time) : MIN_SEEK;
  if (duration === undefined || !Number.isFinite(duration) || duration <= 0)
    return roundFrameTime(t);
  return roundFrameTime(Math.min(t, Math.max(MIN_SEEK, duration - END_GUARD)));
}

/** One seek, and every requested time it answers. */
export interface FrameGrabStep {
  /** Clamped, rounded, and distinct from every other step's. */
  seekTo: number;
  /** The caller's own numbers, so the returned map is keyed by what they asked for. */
  times: number[];
  /** Cache keys to write on capture. */
  keys: string[];
}

/**
 * The seeks one batch needs, ascending.
 *
 * Grouped by the CLAMPED target — several beats past the end of a short take are one frame, and
 * seeking to it three times costs three decodes for three identical JPEGs. Keys are built from the
 * REQUESTED time instead, so the caller's synchronous {@link cachedFrame} peek hits on the number it
 * holds rather than on a clamped one it has no way to compute.
 *
 * A non-finite time is dropped rather than floored: it means the beat arithmetic upstream divided
 * by zero, and a frame from the head of the shot would present that as a real answer.
 */
export function planFrameGrabs(
  url: string,
  times: readonly number[],
  duration?: number
): FrameGrabStep[] {
  const steps = new Map<number, FrameGrabStep>();
  for (const time of times) {
    if (!Number.isFinite(time)) continue;
    const seekTo = clampFrameTime(time, duration);
    const step = steps.get(seekTo) ?? { seekTo, times: [], keys: [] };
    step.times.push(time);
    const key = frameCacheKey(url, time);
    if (!step.keys.includes(key)) step.keys.push(key);
    steps.set(seekTo, step);
  }
  return [...steps.values()].sort((a, b) => a.seekTo - b.seekTo);
}

/** Thumbnail dimensions for a source. Falls back to 16:9 before metadata reports a real size. */
export function frameSize(
  videoWidth: number,
  videoHeight: number,
  want: number
): { width: number; height: number } {
  const sw = videoWidth > 0 ? videoWidth : 16;
  const sh = videoHeight > 0 ? videoHeight : 9;
  const target = Number.isFinite(want) && want > 0 ? want : DEFAULT_WIDTH;
  // Never upscale: a 128px-wide proxy render re-encoded to 256 is twice the bytes and no more
  // detail, and the ribbon cell scales it to fit either way.
  const width = Math.max(1, Math.round(Math.min(target, sw)));
  return { width, height: Math.max(1, Math.round((width * sh) / sw)) };
}

/** Did `seeked` belong to the seek we asked for? See {@link SEEK_TOLERANCE}. */
export const seekLanded = (current: number, target: number): boolean =>
  Math.abs(current - target) <= SEEK_TOLERANCE;

/** The synchronous peek. Paint this first; anything it misses arrives from {@link grabFrames}. */
export const cachedFrame = (url: string, time: number): string | undefined =>
  frameCache.get(frameCacheKey(url, time));

/**
 * Drop cached frames — one take's, or all of them.
 *
 * Per-url matters on a re-render of the SAME shot: a fresh take is served from a new object URL, so
 * its frames are new keys and the old take's are dead weight the ribbon will never ask for again.
 */
export function clearFrameCache(url?: string): void {
  if (url === undefined) {
    frameCache.clear();
    return;
  }
  const prefix = `${url}${KEY_SEP}`;
  for (const key of [...frameCache.keys()]) {
    if (key.startsWith(prefix)) frameCache.delete(key);
  }
}

/** The cache-write seam: every key a step answers gets the one captured frame. */
export function rememberFrame(step: FrameGrabStep, dataUrl: string): void {
  for (const key of step.keys) frameCache.set(key, dataUrl);
  // Map iterates in insertion order, so the first key is the least recently WRITTEN. Good enough:
  // the ribbon re-extracts an evicted frame at the cost of one seek, and only when it scrolls back.
  while (frameCache.size > FRAME_CACHE_LIMIT) {
    const oldest = frameCache.keys().next();
    if (oldest.done) break;
    frameCache.delete(oldest.value);
  }
}

/**
 * Resolve `true` when `event` fires, `false` on error, abort or timeout — and remove every listener
 * on all four paths. An element that outlives a batch by one stray `seeked` handler is a decoder
 * the GC cannot take.
 */
function settleOn(
  el: HTMLVideoElement,
  event: 'loadedmetadata' | 'seeked',
  ms: number,
  signal?: AbortSignal
): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    let timer: ReturnType<typeof setTimeout>;
    let done = false;
    const finish = (ok: boolean) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      el.removeEventListener(event, onEvent);
      el.removeEventListener('error', onFail);
      signal?.removeEventListener('abort', onFail);
      resolve(ok);
    };
    const onEvent = () => finish(true);
    const onFail = () => finish(false);
    timer = setTimeout(onFail, ms);
    el.addEventListener(event, onEvent);
    el.addEventListener('error', onFail);
    signal?.addEventListener('abort', onFail);
  });
}

/**
 * 🔴 Clearing `src` is not enough to release the media resource — the element keeps its decoder and
 * any in-flight fetch until the load algorithm is re-run, which is what the bare `load()` does.
 */
function release(el: HTMLVideoElement): void {
  try {
    el.pause();
    el.removeAttribute('src');
    el.load();
  } catch {
    // Teardown is best-effort by definition: it runs on the paths where the element is already in
    // a state we did not expect.
  }
}

/**
 * Frames for `times`, keyed by the numbers the caller passed in.
 *
 * Missing keys are frames that could not be captured — never a reason to reject. A caller renders
 * `map.get(t) ?? cachedFrame(url, t)` and leaves the cell blank when both are empty.
 */
export async function grabFrames(
  url: string,
  times: readonly number[],
  opts: GrabOptions = {}
): Promise<Map<number, string>> {
  const out = new Map<number, string>();
  for (const time of times) {
    const hit = cachedFrame(url, time);
    if (hit !== undefined) out.set(time, hit);
  }
  const missing = times.filter((t) => !out.has(t));
  if (!missing.length || opts.signal?.aborted || !hasDom()) return out;

  const video = document.createElement('video');
  video.muted = true;
  video.playsInline = true;
  video.preload = 'metadata';
  // crossOrigin is deliberately unset: the source is a same-origin blob, and asking for CORS on a
  // blob URL is how you get a load error instead of a frame.
  try {
    video.src = url;
    video.load();
    if (!(await settleOn(video, 'loadedmetadata', METADATA_TIMEOUT_MS, opts.signal))) return out;

    const size = frameSize(video.videoWidth, video.videoHeight, opts.width ?? DEFAULT_WIDTH);
    const canvas = document.createElement('canvas');
    canvas.width = size.width;
    canvas.height = size.height;
    const ctx = canvas.getContext('2d');
    if (!ctx) return out;
    const quality = opts.quality ?? DEFAULT_QUALITY;

    for (const step of planFrameGrabs(url, missing, video.duration)) {
      if (opts.signal?.aborted) return out;
      video.currentTime = step.seekTo;
      if (!(await settleOn(video, 'seeked', SEEK_TIMEOUT_MS, opts.signal))) continue;
      if (!seekLanded(video.currentTime, step.seekTo)) continue;
      let data: string;
      try {
        ctx.drawImage(video, 0, 0, size.width, size.height);
        data = canvas.toDataURL('image/jpeg', quality);
      } catch {
        // A tainted canvas taints every later read of it too, so there is nothing to carry on for.
        return out;
      }
      rememberFrame(step, data);
      for (const time of step.times) out.set(time, data);
    }
    return out;
  } finally {
    release(video);
  }
}
