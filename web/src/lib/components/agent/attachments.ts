import { formatBytes } from '@koblibs/ui';

export type AttachmentKind = 'image' | 'audio';

export interface PendingAttachment {
  id: string;
  kind: AttachmentKind;
  name: string;
  size: number;
  mime: string;
  dataUrl: string;
}

export const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
export const MAX_AUDIO_BYTES = 25 * 1024 * 1024;
// The per-file caps above say nothing about a turn carrying several files, so
// this is the one that keeps the request legal: the server rejects bodies over
// 40 MiB and base64 inflates raw bytes 4/3, leaving 30 MiB of payload. 28 MiB
// keeps room for the JSON envelope and the message text.
export const MAX_TURN_BYTES = 28 * 1024 * 1024;
export const MAX_TURN_FILES = 12;

export function attachmentKind(mime: string): AttachmentKind | null {
  if (mime.startsWith('image/')) return 'image';
  if (mime.startsWith('audio/')) return 'audio';
  return null;
}

export interface TurnBudget {
  bytes: number;
  count: number;
}

export function budgetOf(staged: PendingAttachment[]): TurnBudget {
  return { bytes: staged.reduce((n, a) => n + a.size, 0), count: staged.length };
}

export interface FileFacts {
  name: string;
  type: string;
  size: number;
}

export type FileVerdict = { kind: AttachmentKind } | { reason: string };

/// One sentence for "this model cannot hear", shared by the composer's tooltip,
/// its rejection notice and its standing banner so the three never drift.
///
/// 🔴 The capability is DERIVED server-side from which llama-server koblem is
/// pointed at (`/health`'s `llama_audio_input`) — it cannot be probed, because
/// `/v1/models` reports nothing about the mmproj. Beefy mode's Qwen3.8 is
/// vision-only; the resident gemma is vision+audio.
export const NO_AUDIO_REASON =
  'the loaded model has no audio encoder (Beefy LLM runs Qwen3.8, whose mmproj is vision-only). Images still work.';

/**
 * @param audioSupported false when the loaded LLM has no audio encoder. Audio is
 *   then refused HERE, at staging — the server would accept the upload, store it
 *   and simply not send it, which is a 25 MB round trip to learn nothing.
 */
export function classify(file: FileFacts, budget: TurnBudget, audioSupported = true): FileVerdict {
  const kind = attachmentKind(file.type);
  if (!kind) {
    return {
      reason: `Skipped ${file.name}: not an image or audio file (${file.type || 'unknown'})`
    };
  }
  if (kind === 'audio' && !audioSupported) {
    return { reason: `Skipped ${file.name}: ${NO_AUDIO_REASON}` };
  }
  const cap = kind === 'audio' ? MAX_AUDIO_BYTES : MAX_IMAGE_BYTES;
  if (file.size > cap) {
    return { reason: `${file.name} is ${formatBytes(file.size)} — limit is ${formatBytes(cap)}` };
  }
  if (budget.count >= MAX_TURN_FILES) {
    return { reason: `Skipped ${file.name}: a message carries at most ${MAX_TURN_FILES} files` };
  }
  if (budget.bytes + file.size > MAX_TURN_BYTES) {
    return {
      reason: `Skipped ${file.name}: a message carries ${formatBytes(MAX_TURN_BYTES)} of attachments and ${formatBytes(budget.bytes)} is already staged`
    };
  }
  return { kind };
}

function readAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error('read failed'));
    reader.onload = () => resolve(String(reader.result));
    reader.readAsDataURL(file);
  });
}

export interface ReadAttachmentsResult {
  accepted: PendingAttachment[];
  errors: string[];
}

export async function readAttachments(
  files: FileList | File[] | null | undefined,
  staged: PendingAttachment[] = [],
  audioSupported = true
): Promise<ReadAttachmentsResult> {
  const accepted: PendingAttachment[] = [];
  const errors: string[] = [];
  const budget = budgetOf(staged);
  for (const file of Array.from(files ?? [])) {
    const verdict = classify(file, budget, audioSupported);
    if ('reason' in verdict) {
      errors.push(verdict.reason);
      continue;
    }
    try {
      accepted.push({
        id: crypto.randomUUID(),
        kind: verdict.kind,
        name: file.name,
        size: file.size,
        mime: file.type,
        dataUrl: await readAsDataUrl(file)
      });
      budget.bytes += file.size;
      budget.count += 1;
    } catch (e) {
      errors.push(`Failed to read ${file.name}: ${e instanceof Error ? e.message : e}`);
    }
  }
  return { accepted, errors };
}

export function clipboardAttachments(e: ClipboardEvent): File[] {
  const items = e.clipboardData?.items;
  if (!items) return [];
  const files: File[] = [];
  for (const item of items) {
    if (item.kind !== 'file') continue;
    const f = item.getAsFile();
    if (f && attachmentKind(f.type)) files.push(f);
  }
  return files;
}
