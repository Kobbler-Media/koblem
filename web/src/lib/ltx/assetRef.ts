import { uploadDirectorAsset } from '$lib/api/client';

// Refs already minted for a Blob identity. Prompt-assist is a button the user presses repeatedly
// against the same attachments, and every press used to re-read and re-base64 the whole file; the
// ref it resolves to is a property of the bytes, so it is minted once per attachment.
const minted = new WeakMap<Blob, string>();

/**
 * The `asset:<hash>` reference for `data`, storing the bytes on first use.
 *
 * Every koblem route that deserialises a document-derived body resolves refs back to bytes at the
 * edge, so a ref is accepted anywhere base64 was — at ~70 bytes instead of 1.33× the file.
 */
export async function assetRefFor(data: Blob): Promise<string> {
  const hit = minted.get(data);
  if (hit) return hit;
  const { ref } = await uploadDirectorAsset(data);
  minted.set(data, ref);
  return ref;
}
