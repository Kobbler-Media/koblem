// Typed readers for a generation's `request_meta` (a Record<string, unknown>),
// used by each playground page to repopulate its form from a saved generation.
// Each returns `undefined` when the key is absent or the wrong type, so callers
// assign only what's actually present: `const p = str(m, 'prompt'); if (p !== undefined) prompt = p;`

type Meta = Record<string, unknown>;

export function str(m: Meta, k: string): string | undefined {
  const v = m[k];
  return typeof v === 'string' ? v : undefined;
}

export function num(m: Meta, k: string): number | undefined {
  const v = m[k];
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined;
}

export function bool(m: Meta, k: string): boolean | undefined {
  const v = m[k];
  return typeof v === 'boolean' ? v : undefined;
}

export function obj(m: Meta, k: string): Meta | undefined {
  const v = m[k];
  return v && typeof v === 'object' && !Array.isArray(v) ? (v as Meta) : undefined;
}

export function arr(m: Meta, k: string): unknown[] | undefined {
  const v = m[k];
  return Array.isArray(v) ? v : undefined;
}

// The stored `had_image_inputs` / `had_audio_inputs` / `had_uploads` flags mean the
// render used an uploaded file the browser can't re-attach on restore. Returns a
// user-facing note when any is set, else ''.
export function reattachNote(m: Meta): string {
  if (
    bool(m, 'had_image_inputs') ||
    bool(m, 'had_audio_inputs') ||
    bool(m, 'had_video_inputs') ||
    bool(m, 'had_uploads')
  ) {
    return 'Settings restored — re-attach any image/audio/video inputs before rendering.';
  }
  return '';
}
