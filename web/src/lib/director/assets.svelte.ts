import { fetchDirectorAsset, isAssetRef, uploadDirectorAsset } from '#lib/api/client.ts';

/**
 * Display cache for content-addressed editor assets.
 *
 * Templates need a `src` synchronously, but an asset ref has to be fetched. So `assetSrc()` returns
 * whatever is known right now — an object URL once resolved, `""` until then — and kicks off the
 * fetch. The cache is `$state`, so the moment a fetch lands every `<img>` referencing it updates.
 *
 * Keyed by content hash, which makes the dedupe exact: a still reused across ten shots is fetched
 * once and shares one object URL, where the old inline-base64 model rebuilt a `data:` URI string at
 * every one of those ten render sites.
 *
 * Entries are never evicted. They are bounded by the number of DISTINCT assets a session opens, and
 * an object URL costs a handle plus the blob — the previous design held the same bytes inline in
 * the document, the undo stack and localStorage simultaneously.
 */
const urls = $state<Record<string, string>>({});
// Plain records, not Sets: this is bookkeeping no template reads, so it must not be reactive.
const inflight: Record<string, true> = {};
// Refs the store answered 404 for. `assetSrc` runs on every re-render, so without this a ref whose
// bytes are gone re-fetches for the life of the session. Only 404s land here — a transient network
// failure stays retryable.
const gone: Record<string, true> = {};

/**
 * A `src` for an asset ref, legacy inline base64, or an already-usable URL.
 *
 * Legacy documents still hold raw base64 in these fields; they render straight from a data URI as
 * before, so nothing needs migrating.
 */
export function assetSrc(value: string | null | undefined, mime = 'image/*'): string {
  if (!value) return '';
  if (!isAssetRef(value)) {
    // Legacy inline base64 (bare or already a data:/blob:/http: URL).
    return /^(data:|blob:|https?:|\/)/.test(value) ? value : `data:${mime};base64,${value}`;
  }
  const have = urls[value];
  if (have) return have;
  if (!inflight[value] && !gone[value]) {
    inflight[value] = true;
    void fetchDirectorAsset(value)
      .then((blob) => {
        urls[value] = URL.createObjectURL(blob);
      })
      .catch((e: Error & { status?: number }) => {
        // A missing asset renders as nothing here; the render / draft paths are the ones that
        // report it, with the hash and what to do about it.
        if (e?.status === 404) gone[value] = true;
      })
      .finally(() => delete inflight[value]);
  }
  return '';
}

/** Warm the cache for every ref in one pass, so a loaded project's stills appear together. */
export function prefetchAssets(values: (string | null | undefined)[]): void {
  for (const v of values) if (isAssetRef(v)) assetSrc(v);
}

/** Store a file/blob and return its `asset:<hash>` ref, pre-warming the display cache. */
export async function putAsset(data: Blob): Promise<string> {
  const { ref } = await uploadDirectorAsset(data);
  if (!urls[ref]) urls[ref] = URL.createObjectURL(data);
  return ref;
}

/**
 * An asset ref (or legacy inline base64) as a Blob — for the render path, which still ships the
 * per-shot bytes as multipart parts. Assets are served immutable, so the browser cache absorbs
 * repeat renders.
 */
export async function fetchAssetBlob(value: string): Promise<Blob> {
  if (isAssetRef(value)) return fetchDirectorAsset(value);
  const clean = value.includes('base64,') ? value.split('base64,')[1] : value;
  const bin = atob(clean);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new Blob([bytes]);
}
