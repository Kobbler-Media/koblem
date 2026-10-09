import { describe, expect, it } from 'vitest';
import type { DirectorEngine, DirectorTimeline } from '$lib/api/client';
import { readTimelineDoc } from './timeline-doc';

const doc = (over: Partial<DirectorTimeline> = {}): DirectorTimeline =>
  ({ version: 2, clips: [], ...over }) as DirectorTimeline;

describe('readTimelineDoc — resolution migration', () => {
  /** A v1 doc stored the BASE pass. Read verbatim it renders at half the size it was saved at, and
   *  only old projects are affected — the exact shape of a silent regression. */
  it('scales a v1 custom resolution up by its saved upscaler ratio', () => {
    const out = readTimelineDoc(
      doc({
        version: 1,
        width: 960,
        height: 544,
        request: { segments: [], upscaler_ratio: 'x2' },
        editor: { resPreset: 'custom' }
      })
    );
    expect(out).toMatchObject({ width: 1920, height: 1088 });
  });

  it('leaves a v2 custom resolution alone', () => {
    const out = readTimelineDoc(
      doc({
        version: 2,
        width: 1920,
        height: 1088,
        request: { segments: [], upscaler_ratio: 'x2' },
        editor: { resPreset: 'custom' }
      })
    );
    expect(out).toMatchObject({ width: 1920, height: 1088 });
  });

  it('treats a doc with no version at all as v1', () => {
    const out = readTimelineDoc(
      doc({
        version: undefined,
        width: 640,
        height: 352,
        request: { segments: [], upscaler_ratio: 'x2' },
        editor: { resPreset: 'custom' }
      })
    );
    expect(out).toMatchObject({ width: 1280, height: 704 });
  });

  /** A named preset carries its own ratio, so it self-migrates whatever the doc's raw dims say. */
  it('a named preset overrides both the dims and the ratio', () => {
    const out = readTimelineDoc(
      doc({
        version: 1,
        width: 111,
        height: 222,
        request: { segments: [], upscaler_ratio: 'none' },
        editor: { resPreset: '1080p' }
      })
    );
    expect(out).toMatchObject({ width: 1920, height: 1088, upscalerRatio: 'x2' });
  });
});

describe('readTimelineDoc — defaults for fields added later', () => {
  it('defaults beats ON and the frame drop to auto', () => {
    const out = readTimelineDoc(doc());
    expect(out.beatsEnabled).toBe(true);
    expect(out.frameDrop).toBe('auto');
  });

  /**
   * Every project saved before H3 existed carries no `engine` key, and a project saved before
   * LTX-2.3 was retired may carry `"ltx"`. Both have to open on the LTX that still exists — the
   * alternative is a timeline the editor refuses to load.
   */
  it('defaults the engine to LTX-2.5 and carries no references', () => {
    const out = readTimelineDoc(doc());
    expect(out.engine).toBe('ltx25');
    expect(out.h3References).toEqual([]);
  });

  /**
   * The retired engine's own name reads the same way. Cast because `'ltx'` was REMOVED from
   * `DirectorEngine` when LTX-2.3 went — which is the point: the editor can no longer produce the
   * value, but a document saved before then still holds it, and this is the reader that has to cope.
   */
  it('opens a saved LTX-2.3 project on LTX-2.5', () => {
    const saved = doc({ engine: 'ltx' as unknown as DirectorEngine });
    expect(readTimelineDoc(saved).engine).toBe('ltx25');
  });

  /** Order is part of the request — it assigns each reference's ordinal — so it is read verbatim. */
  it('reads an h3 project and keeps its references in document order', () => {
    const out = readTimelineDoc(
      doc({
        engine: 'h3',
        h3_references: [
          { id: 'b', kind: 'image', image: 'two' },
          { id: 'a', kind: 'audio', audio: 'one' }
        ]
      })
    );
    expect(out.engine).toBe('h3');
    expect(out.h3References.map((r) => r.id)).toEqual(['b', 'a']);
  });

  /**
   * Generations carry the AUTHORED sections and nothing else — the packing is re-derived from the
   * clips on every render of the editor. Absent (every project saved before they existed) reads as
   * an empty list, which the packer turns into a default packing with empty sections.
   */
  it('reads the authored generation sections, and absent as none', () => {
    expect(readTimelineDoc(doc({ engine: 'h3' })).h3Generations).toEqual([]);
    const out = readTimelineDoc(
      doc({
        engine: 'h3',
        h3_generations: [{ id: 'g0', clip_ids: ['a'], overall_soundscape: 'Rain on the awning.' }]
      })
    );
    expect(out.h3Generations[0].overall_soundscape).toBe('Rain on the awning.');
  });

  /**
   * `request` is a union now, and an H3 document has no upscaler ratio, no LoRA set and no hires
   * chain to recover. Reading it as LTX would hand the editor an `undefined` from a shape that was
   * never going to have those fields — narrowing makes it the documented default instead.
   */
  it('does not mine an H3 request for LTX settings', () => {
    const out = readTimelineDoc(
      doc({
        engine: 'h3',
        request: { generations: [], width: 1344, height: 768, fps: 24 }
      })
    );
    expect(out.loras).toEqual([]);
    expect(out.negativePrompt).toBe('');
    expect(out.qualityProfile).toBe('standard');
  });

  it('honours an explicit off', () => {
    const out = readTimelineDoc(doc({ beats_enabled: false, frame_drop: 'off' }));
    expect(out.beatsEnabled).toBe(false);
    expect(out.frameDrop).toBe('off');
  });

  it('defaults the dailies review off, so an existing project renders as it always did', () => {
    expect(readTimelineDoc(doc()).critique).toEqual({
      enabled: false,
      autoRetake: false,
      maxRetakes: 2,
      threshold: 0.6
    });
  });

  it('derives the shot length from the frame count when the editor blob has none', () => {
    expect(readTimelineDoc(doc({ fps: 24, segment_frames: 193 })).shotSeconds).toBe(8);
    expect(readTimelineDoc(doc()).shotSeconds).toBe(4);
  });
});

describe('readTimelineDoc — round-tripped request fields', () => {
  /** A doc hand-edited or written by an older, looser caller must not smuggle a multiplier past the
   *  range the picker enforces. */
  it('re-clamps LoRA strengths on the way in', () => {
    const out = readTimelineDoc(
      doc({ request: { segments: [], loras: [{ path: 'l', multiplier: 99 }] } })
    );
    expect(out.loras[0].multiplier).toBeLessThanOrEqual(2);
  });

  it('maps a Custom hires chain back onto ratio keys', () => {
    const out = readTimelineDoc(
      doc({
        request: {
          segments: [],
          hires_chain: [
            {
              upscaler: 'ltx-2.3-spatial-upscaler-x1.5-1.0',
              custom_sigmas: [0.8, 0],
              sample_method: 'euler',
              cfg: 2,
              steps: 4
            }
          ]
        }
      })
    );
    expect(out.customStages).toEqual([
      { upscaler: 'x1.5', sigmasText: '0.8, 0', sample_method: 'euler', cfg: 2, steps: 4 }
    ]);
  });

  /** Absent means "keep what the editor has" — writing a default here would silently reset the
   *  Custom recipe of a project that never used one. */
  it('says nothing about the Custom recipe when the doc carries none', () => {
    const out = readTimelineDoc(doc());
    expect('customStages' in out).toBe(false);
    expect('customBaseSampler' in out).toBe(false);
    expect('customBaseSigmasText' in out).toBe(false);
  });
});

describe('readTimelineDoc — editor blob', () => {
  /** An auto-direct plan echoes no brief, so the user's typed one has to survive adopting the plan
   *  it produced. */
  it('omits the brief entirely when the doc has none', () => {
    expect('autoBrief' in readTimelineDoc(doc())).toBe(false);
    expect(readTimelineDoc(doc({ editor: { autoBrief: 'a chase' } })).autoBrief).toBe('a chase');
  });

  it('fills in every story field a partial blob is missing', () => {
    const out = readTimelineDoc(doc({ editor: { story: { backstory: 'b' } } }));
    expect(out.story).toEqual({ backstory: 'b', tone: '', rules: '', references: [] });
  });

  it('ignores a chat log that is not a list', () => {
    expect(readTimelineDoc(doc({ editor: { chatMessages: 'nope' } })).chatMessages).toEqual([]);
  });

  it('only trusts a summary that is a string', () => {
    expect(readTimelineDoc(doc({ editor: { summary: 7 } })).summary).toBeNull();
    expect(readTimelineDoc(doc({ editor: { summary: 'a synopsis' } })).summary).toBe('a synopsis');
  });
});

describe('readTimelineDoc — cast', () => {
  /** A doc whose roster lost a character must not leave shots pointing at it. */
  it('prunes shots that name a character the roster no longer has', () => {
    const out = readTimelineDoc(
      doc({
        characters: [
          {
            id: 'keep',
            name: 'Mia',
            source_id: 1,
            sheet_b64: null,
            identity: '',
            introduced_by: null
          }
        ],
        clips: [
          {
            id: 'a',
            prompt: 'p',
            mode: 't2v',
            seconds: 4,
            keyframes: [],
            character_ids: ['keep', 'gone']
          }
        ]
      })
    );
    expect(out.clips[0].character_ids).toEqual(['keep']);
  });

  it('fills in missing keyframe arrays', () => {
    const out = readTimelineDoc(
      doc({ clips: [{ id: 'a', prompt: 'p', mode: 't2v', seconds: 4 }] as never })
    );
    expect(out.clips[0].keyframes).toEqual([]);
  });
});
