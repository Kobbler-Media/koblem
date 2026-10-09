import { describe, expect, it } from 'vitest';
import {
  MAX_AUDIO_BYTES,
  MAX_IMAGE_BYTES,
  MAX_TURN_BYTES,
  MAX_TURN_FILES,
  attachmentKind,
  budgetOf,
  classify,
  type FileFacts,
  type PendingAttachment
} from './attachments';

const MiB = 1024 * 1024;
const empty = { bytes: 0, count: 0 };

function file(size: number, type = 'image/png', name = 'shot.png'): FileFacts {
  return { name, type, size };
}

function staged(...sizes: number[]): PendingAttachment[] {
  return sizes.map((size, i) => ({
    id: String(i),
    kind: 'image' as const,
    name: `a${i}.png`,
    size,
    mime: 'image/png',
    dataUrl: 'data:image/png;base64,'
  }));
}

function accepted(v: ReturnType<typeof classify>): boolean {
  return 'kind' in v;
}

describe('attachmentKind', () => {
  it('takes images and audio and nothing else', () => {
    expect(attachmentKind('image/webp')).toBe('image');
    expect(attachmentKind('audio/mpeg')).toBe('audio');
    expect(attachmentKind('video/mp4')).toBeNull();
    expect(attachmentKind('')).toBeNull();
  });
});

describe('budgetOf', () => {
  it('sums what is already staged', () => {
    expect(budgetOf(staged(3, 4, 5))).toEqual({ bytes: 12, count: 3 });
    expect(budgetOf([])).toEqual(empty);
  });
});

describe('classify audio capability', () => {
  // 🔴 The gate is at STAGING, not at send. The server would happily accept the
  // upload, store it, and then not send it to a model that cannot read it — a 25
  // MB round trip to learn nothing.
  it('refuses audio when the loaded model has no audio encoder', () => {
    const wav = file(1024, 'audio/wav', 'take.wav');
    expect(accepted(classify(wav, empty, true))).toBe(true);
    const verdict = classify(wav, empty, false);
    expect(accepted(verdict)).toBe(false);
    expect('reason' in verdict && verdict.reason).toContain('no audio encoder');
  });

  it('still takes images on an audio-less model', () => {
    expect(accepted(classify(file(1024), empty, false))).toBe(true);
  });

  // Absent means "audio works" — that is the pre-Beefy world, and every existing
  // caller relies on it.
  it('defaults to allowing audio', () => {
    expect(accepted(classify(file(1024, 'audio/wav', 'take.wav'), empty))).toBe(true);
  });
});

describe('classify per-file caps', () => {
  it('accepts a file inside its own cap', () => {
    expect(accepted(classify(file(MAX_IMAGE_BYTES), empty))).toBe(true);
    expect(accepted(classify(file(MAX_AUDIO_BYTES, 'audio/wav', 'v.wav'), empty))).toBe(true);
  });

  it('rejects a file over its own cap', () => {
    expect(accepted(classify(file(MAX_IMAGE_BYTES + 1), empty))).toBe(false);
    expect(accepted(classify(file(MAX_AUDIO_BYTES + 1, 'audio/wav', 'v.wav'), empty))).toBe(false);
  });

  it('rejects a kind the agent cannot take', () => {
    expect(accepted(classify(file(1, 'video/mp4', 'clip.mp4'), empty))).toBe(false);
  });
});

describe('classify per-turn budget', () => {
  it('keeps a turn that would blow the 40 MiB server body limit out of the request', () => {
    // Four 10 MiB images used to sail past the per-file caps and 413 server-side.
    const budget = budgetOf(staged(10 * MiB, 10 * MiB, 10 * MiB));
    expect(accepted(classify(file(10 * MiB), budget))).toBe(false);
  });

  it('rejects a second max-size audio file', () => {
    const first = { bytes: MAX_AUDIO_BYTES, count: 1 };
    expect(accepted(classify(file(MAX_AUDIO_BYTES, 'audio/wav', 'b.wav'), first))).toBe(false);
  });

  it('still fits a big audio file alongside a small image', () => {
    const budget = { bytes: MAX_AUDIO_BYTES, count: 1 };
    expect(accepted(classify(file(2 * MiB), budget))).toBe(true);
  });

  it('accepts a file that lands exactly on the budget', () => {
    const budget = { bytes: MAX_TURN_BYTES - 5 * MiB, count: 1 };
    expect(accepted(classify(file(5 * MiB), budget))).toBe(true);
    expect(accepted(classify(file(5 * MiB + 1), budget))).toBe(false);
  });

  it('names the budget and what is already staged so the user can act', () => {
    const v = classify(file(9 * MiB), { bytes: 20 * MiB, count: 1 });
    expect('reason' in v && v.reason).toMatch(/already staged/);
  });

  it('stays under the 40 MiB server body limit once base64 inflates it', () => {
    expect(Math.ceil(MAX_TURN_BYTES / 3) * 4).toBeLessThan(40 * MiB);
  });
});

describe('classify file count', () => {
  it('caps the number of files even when they are tiny', () => {
    const full = { bytes: 1024, count: MAX_TURN_FILES };
    expect(accepted(classify(file(16), full))).toBe(false);
    expect(accepted(classify(file(16), { bytes: 1024, count: MAX_TURN_FILES - 1 }))).toBe(true);
  });
});
