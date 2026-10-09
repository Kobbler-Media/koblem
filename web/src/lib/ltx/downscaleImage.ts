// Downscale + JPEG-compress an image for LLM-ONLY images (per-shot references, story-context
// references) that ride inside the director timeline but are never rendered — a VLM only needs a
// small image, so this keeps saved projects from ballooning. Do NOT use this for images that get
// rendered (keyframe pins, i2v scene frames, v2v sources) — those need full resolution.

/** Longest-side cap (px) — plenty for a VLM to read composition/mood from. */
const DEFAULT_MAX_DIM = 1024;
const DEFAULT_QUALITY = 0.82;

/**
 * Decode `file`, scale it down so its longest side is ≤ `maxDim` (never upscales), and re-encode as
 * JPEG. Falls back to the raw file bytes when the canvas path is unavailable.
 */
export async function downscaleImageToBlob(
  file: File | Blob,
  maxDim = DEFAULT_MAX_DIM,
  quality = DEFAULT_QUALITY
): Promise<Blob> {
  const url = URL.createObjectURL(file);
  try {
    const img = await new Promise<HTMLImageElement>((resolve, reject) => {
      const el = new Image();
      el.onload = () => resolve(el);
      el.onerror = () => reject(new Error('could not decode image'));
      el.src = url;
    });
    const w = img.naturalWidth;
    const h = img.naturalHeight;
    const scale = Math.min(1, maxDim / Math.max(w, h || 1));
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(w * scale));
    canvas.height = Math.max(1, Math.round(h * scale));
    const ctx = canvas.getContext('2d');
    if (!ctx) return file;
    ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
    const blob = await new Promise<Blob | null>((resolve) =>
      canvas.toBlob((b) => resolve(b), 'image/jpeg', quality)
    );
    return blob ?? file;
  } catch {
    return file;
  } finally {
    URL.revokeObjectURL(url);
  }
}
