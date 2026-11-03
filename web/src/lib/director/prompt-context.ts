/**
 * What the LLM is told about a shot before it writes or rewrites its prompt.
 *
 * Every draft, reroll, rework and summarise goes through the same grounding, which is the point:
 * the writer's answer changes with what it was told, so two paths that ground a shot differently
 * produce two different houses in the same scene. The judgements here are the ones that showed up
 * as bad renders — whether a shot continues out of the previous frame, whether anyone actually
 * speaks during it, which characters are in it — so they are pinned by tests rather than re-derived
 * at each call site.
 */

import type {
  DirectorCharacter,
  DirectorClip,
  DirectorEditorShot,
  DirectorEngine,
  DirectorH3Reference,
  H3VocalLane,
  H3VocalSpan,
  LtxPromptFromLlmRequest,
  LtxPromptImage,
  LyricWord,
  StoryContext
} from '#lib/api/client.ts';
import { h3RefBriefs, h3RefsForShot, type H3SoundtrackFacts } from '#lib/api/client.ts';
import { castForClip } from './characters';
import { closesScene, startsFresh } from './continuity';
import { H3_FPS, pinBlocksReferences } from './engine';
import { pinsAFrame, type PackedGeneration } from './h3-generations';
import {
  generationWantsSlice,
  planSoundtrack,
  type SoundtrackPlan,
  runUpSecondsFor
} from './h3-soundtrack';
import { layoutClips, ltxDeliveredSeconds, type TimelineLayout } from './timeline-layout';
import type { AudioKind } from './compile';

export const storyIsEmpty = (s: StoryContext): boolean =>
  !s.backstory.trim() &&
  !s.tone.trim() &&
  !s.rules.trim() &&
  !s.references.some((r) => r.label.trim() || r.description.trim() || r.image_b64);

/**
 * Blank clips are dropped from every LLM payload. Results come back keyed to a clip id, so no
 * filtered-position mapping is needed on the way back.
 */
export const clipHasContent = (c: DirectorClip) =>
  !!(c.prompt.trim() || c.idea?.trim() || c.gist?.trim());

/**
 * The text-only shot payload shared by summarise / rework / review / chat. `id` is load-bearing:
 * every per-shot result comes back keyed to it, so a shot sent without one can never be edited.
 */
export function editorShots(clips: DirectorClip[]): DirectorEditorShot[] {
  return clips.filter(clipHasContent).map((c) => ({
    id: c.id,
    idea: c.idea?.trim() || undefined,
    prompt: c.prompt.trim() || undefined,
    gist: c.gist?.trim() || undefined,
    mode: c.mode,
    seconds: c.seconds,
    new_scene: c.new_scene
  }));
}

/** The stage heading for a shot. Prefers the summary, then the idea, then the raw prompt. */
export const shotHeading = (c: DirectorClip): string =>
  c.gist?.trim() || c.idea?.trim() || c.prompt.trim() || 'Untitled shot';

/**
 * A compact one-liner, for feeding EARLIER shots to a draft or reroll as "story so far". Prefers the
 * plain-language gist, then the idea, then a truncated prompt.
 */
export function shotOneLiner(c: DirectorClip): string {
  const g = c.gist?.trim() || c.idea?.trim();
  if (g) return g;
  const p = c.prompt.trim();
  if (!p) return '';
  const words = p.split(/\s+/);
  return words.length > 16 ? words.slice(0, 16).join(' ') + '…' : p;
}

/**
 * Does the transcript put any word inside shot `idx`?
 *
 * The transcript is machine-heard and often wrong about WORDS, but its timing is good enough to
 * answer "is anyone making mouth-sounds here", which is all the writer needs to avoid lip-syncing
 * a silence.
 *
 * OVERLAP, not start: a word held across the cut is still being said in the shot it runs into, and
 * matching only its start called that shot silent. Both spans are half-open, as `beatWordsFor` is,
 * so a word that lands exactly ON a boundary belongs to one side of it and not to both.
 */
/**
 * Where each shot really lands, for windowing the transcript against.
 *
 * 🔴 The transcript is on the SONG clock and `clip.seconds` is not. Summing durations to find a
 * shot's span — which both readers below used to do — drifts from the render by the frame-grid
 * snap at every shot, so late in a cut the writer is told the wrong shot is singing and the beat
 * picker offers the wrong words. Neither fails; both are just quietly wrong.
 *
 * A caller that already has one — the workspace, which draws the timeline from it — passes it in
 * as {@link PromptContext.layout}, so the writer is briefed on the same geometry the author is
 * looking at rather than on a second copy that merely ought to match.
 *
 * `planSoundtrack(gens, 0)` in the fallback is deliberate: the plan's OFFSETS do not depend on the
 * song's length (only which slices get skipped does), so this needs no duration in the context.
 */
export function promptLayout(
  ctx: Pick<PromptContext, 'clips' | 'fps' | 'h3Packing' | 'layout'>
): TimelineLayout {
  if (ctx.layout) return ctx.layout;
  const gens = ctx.h3Packing ?? [];
  return layoutClips(ctx.clips, gens, planSoundtrack(gens, 0), ltxDeliveredSeconds(ctx.fps));
}

export function shotHasSpeech(
  layout: TimelineLayout,
  lyrics: LyricWord[],
  clipId: string
): boolean {
  const l = layout.byId.get(clipId);
  if (!l) return false;
  return lyrics.some((w) => w.end > l.start && w.start < l.start + l.width);
}

// ── The vocal lane ───────────────────────────────────────────────────────────
//
// 🔴 What `shotHasSpeech` above cannot say, and the defect that came of it.
//
// `speech_in_shot` is one bit for a whole ~14 s generation, and the server's `true` arm renders as
// *"a `<d>` block and a moving mouth are correct"* — a STANDING fact, which on H3 outranks anything
// a shot body says. Half a second of vocal and a ten-second guitar solo are the same `true`. On the
// 15-generation project reviewed 2026-08-14, three generations whose slice of the song is entirely
// instrumental (the vocal ends 37 s before the video does) were written with *"its mouth moves in
// perfect synchronization with the male vocals"* — ~40 s of a character's mouth flapping at nothing.
//
// The lane is the same measurement, unflattened: which LOCAL seconds of which generation carry a
// voice.

/**
 * How long a hole between two sung words has to be before it splits one vocal span into two.
 *
 * 0.45 s, and it is not arbitrary: it is the grouping threshold the measured lane in
 * `~/handoffs/koblem/h3-director/RESULTS-sealed-by-fate-clock-2026-08-14.md` was produced with, so
 * this reproduces that table's spans rather than a second opinion about them. Below it the singer
 * has not stopped singing — they have taken a breath, and telling the writer to close a mouth for
 * 300 ms is worse direction than saying nothing.
 */
const VOCAL_LINE_GAP = 0.45;

/**
 * How many transcript words ride along at a span's entry.
 *
 * Enough for the guide's §5.4 idiom to have a phrase to quote, short enough that a fifteen-lane
 * brief does not become the whole lyric sheet. The words are a POSITION cue first and a quotation
 * second — the recogniser was pointed at singing and mishears freely — which is why this is a
 * handful and not the line.
 */
const VOCAL_ENTRY_WORDS = 12;

/** Group the word-level transcript into sung phrases on the SONG clock. See {@link VOCAL_LINE_GAP}. */
function vocalLines(lyrics: LyricWord[]): { start: number; end: number; text: string }[] {
  const out: { start: number; end: number; text: string }[] = [];
  let cur: LyricWord[] = [];
  const flush = () => {
    if (!cur.length) return;
    out.push({
      start: cur[0].start,
      end: cur[cur.length - 1].end,
      text: cur
        .slice(0, VOCAL_ENTRY_WORDS)
        .map((w) => w.text.trim())
        .filter(Boolean)
        .join(' ')
    });
    cur = [];
  };
  // Sorted defensively: the lane's whole value is that its times are right, and a transcript that
  // arrived out of order would otherwise produce spans that run backwards.
  for (const w of [...lyrics].sort((a, b) => a.start - b.start)) {
    if (cur.length && w.start - cur[cur.length - 1].end > VOCAL_LINE_GAP) flush();
    cur.push(w);
  }
  flush();
  return out;
}

/**
 * Where the song's voice falls inside ONE generation, in that generation's LOCAL seconds.
 *
 * 🔴 **The clock conversion is the whole function and it is easy to get silently wrong.** A
 * generation is conditioned on its own slice of the song, so its clock — the one a
 * `[Shot N] At MM:SS.mmm` mark runs on — restarts at 0 where that slice starts:
 * `local = songTime - songOffsetSeconds`. It is NOT `deliveredAtSeconds`, which is one trim frame
 * later on every anchored generation and drifts a frame per seam along the cut, and it is not the
 * sum of the shot lengths either. The offset is taken from {@link planSoundtrack} rather than
 * re-derived; that arithmetic exists twice already and the two copies have drifted before.
 *
 * A generation the soundtrack is scoped away from returns an EMPTY lane rather than no lane: it
 * renders on the model's own bed, so there is genuinely no song vocal in it to perform, and an
 * empty lane is exactly how that is said.
 */
export function vocalLaneFor(
  lyrics: LyricWord[],
  gen: PackedGeneration,
  songOffsetSeconds: number,
  generationNumber?: number
): H3VocalLane {
  // The RENDERED length, not the authored sum — the local clock runs to the snapped frame count.
  const seconds = gen.frames / H3_FPS;
  //: 🔴 The run-up rides the lane so the WRITER knows where marks become unreachable. The spans
  //: below are clipped past it (see the clip in the loop), so the number is the lane's ONLY account
  //: of those frames — the server clause turns it into "that phrase is already sounding when the
  //: shot appears; describe it, do not time it".
  const runUpSeconds = runUpSecondsFor(gen);
  const base: H3VocalLane = {
    ...(generationNumber === undefined ? {} : { generation: generationNumber }),
    seconds,
    ...(runUpSeconds > 0 ? { run_up_seconds: runUpSeconds } : {}),
    spans: []
  };
  if (!generationWantsSlice(gen)) return base;
  const windowEnd = songOffsetSeconds + seconds;
  const spans: H3VocalSpan[] = [];
  for (const line of vocalLines(lyrics)) {
    if (line.end <= songOffsetSeconds || line.start >= windowEnd) continue;
    // Clipped to the window on both sides: a phrase that straddles a generation boundary is half in
    // each, and the half that is here is the half this render can perform.
    //
    // 🔴 …and clipped to the RUN-UP at the head, which is the owner's call and the right one: the
    // model does hear those frames, but they are dropped, and the prose has no say over them anyway
    // — an on-target carry supplies them as keyframes, so whatever the mouth was doing is already
    // fixed by the previous shot's tail. From the viewer's side that audio IS the previous shot.
    // Reporting it here only invites a sing-cue at a timestamp that gets trimmed away in silence.
    const start = Math.max(line.start, songOffsetSeconds + runUpSeconds) - songOffsetSeconds;
    const end = Math.min(line.end, windowEnd) - songOffsetSeconds;
    if (end <= start) continue;
    spans.push({ start, end, ...(line.text ? { words: line.text } : {}) });
  }
  return { ...base, spans };
}

/**
 * One lane per generation, render order — for the paths that write or rewrite the WHOLE project.
 *
 * Returns `[]` when there is no transcript to measure, which is the signal for "send nothing and
 * leave the server on the behaviour it had". Same gate `speech_in_shot` has always used.
 */
export function vocalLanes(
  ctx: Pick<PromptContext, 'lyrics' | 'h3Packing' | 'engine' | 'songSeconds'>
): H3VocalLane[] {
  const gens = ctx.h3Packing ?? [];
  if ((ctx.engine ?? 'ltx') !== 'h3' || !ctx.lyrics.length || !gens.length) return [];
  const plan = songSlices(ctx);
  return gens.map((gen, i) =>
    vocalLaneFor(ctx.lyrics, gen, plan.slices[i].songOffsetSeconds, i + 1)
  );
}

/**
 * The per-generation soundtrack slices this project's render will actually use.
 *
 * Straight through to {@link planSoundtrack} — never a second copy of the arithmetic. That
 * arithmetic already exists twice (here and `h3_plan_soundtrack` in koblibs) and the two copies HAVE
 * drifted silently, which is recorded at length in `h3-soundtrack.ts`'s header; a third copy whose
 * only symptom would be "the writer was briefed on the wrong fourteen seconds" is not worth having.
 *
 * ⚠️ `songSeconds` only decides which slices are SKIPPED and how long each one is; every offset is
 * correct at 0, which is why {@link promptLayout} can get away without one. The lane needs only the
 * offsets, so it works either way — the attachment path needs `sliceSeconds`, so it needs this.
 */
function songSlices(ctx: Pick<PromptContext, 'h3Packing' | 'songSeconds'>): SoundtrackPlan {
  return planSoundtrack(ctx.h3Packing ?? [], ctx.songSeconds ?? 0);
}

/**
 * Lyric words that land inside a shot, as SHOT-RELATIVE offsets — what "snap a beat to the word"
 * picks from.
 *
 * 🔴 The offset is the raw distance from the block's start, NOT rescaled into `clip.seconds`. A
 * generation's round-up onto the frame grid lands entirely on the tail of its LAST shot, so every
 * moment before that tail maps 1:1 between the drawn axis and the clock the render — and therefore
 * a `[Shot N] At …` mark — actually runs on. Rescaling would move every beat in the shot for
 * nothing.
 *
 * 🔴 …but the render's clock starts at `start - runUp`, so the RUN-UP is added. An anchored H3
 * continuation renders the previous shot's last 0.917 s as its own head and trims it, so a word
 * sitting 2.0 s into the block is 2.917 s into the render, and 2.917 is the number the prompt takes.
 * `runUp` is 0 on LTX and on the first shot, so this is the identity everywhere it always was.
 *
 * ⚠️ Selection is on DELIVERED time — the block the word is heard under — which is the attribution
 * rule the whole surface follows. A word inside the next shot's run-up therefore belongs to THIS
 * shot and appears once, not twice.
 *
 * ⚠️ Shot-relative, not generation-relative, on a generation holding more than one shot. That
 * predates the run-up and is unchanged by it; today one clip is one generation on H3, and the only
 * caller (`ShotPromptPane`) is LTX-only, where a generation is a shot by definition.
 */
export function beatWordsFor(
  layout: TimelineLayout,
  lyrics: LyricWord[],
  clipId: string
): (LyricWord & { offset: number })[] {
  const l = layout.byId.get(clipId);
  if (!l || lyrics.length === 0) return [];
  return lyrics
    .filter((w) => w.start >= l.start && w.start < l.start + l.width)
    .map((w) => ({ ...w, offset: w.start - l.start + l.runUp }));
}

/** Everything outside the shot itself that its grounding reads. Plain values — snapshot on the way in. */
export interface PromptContext {
  clips: DirectorClip[];
  characters: DirectorCharacter[];
  story: StoryContext;
  lyrics: LyricWord[];
  fps: number;
  audioKind: AudioKind;
  /**
   * Which engine the project renders on. Absent reads as `'ltx'`, which is what every caller
   * written before H3 existed means.
   */
  engine?: DirectorEngine;
  /** The project's H3 omni references, IN ORDER. Ignored on LTX. */
  h3References?: DirectorH3Reference[];
  /**
   * The project's soundtrack facts, attached to the reused-song reference in the roster.
   *
   * 🔴 Optional, and its absence is why every drafted block filed the song under
   * `non_diegetic_music`. The server's fallback is the guide's, and the guide says a reused score
   * is audience-only (`GUIDE-ref:296`) — correct for a film, fatal for a music video, where it
   * tells the model the performer cannot hear what it is singing. Omitting this keeps the old
   * behaviour rather than guessing, so a caller that does not have the timeline is unchanged.
   */
  h3Soundtrack?: H3SoundtrackFacts;
  /**
   * The live packing — which clips share a render, and what each render is actually conditioned on.
   *
   * 🔴 **Without it the writer is briefed on the wrong references.** Scope is a property of the
   * RENDER UNIT and is stored as `reference_ids` on the generation, because that is the only
   * granularity the conditioner has (`pipeline.py:311-340` builds one reference presentation per
   * call). Reading `segments` instead — which is what this did until 2026-08-08, and which nothing
   * has written since Stage 5 — makes a generation-level narrowing INVISIBLE: every reference in
   * the project looks in scope, so the writer over-includes and names `<Picture 4>` in prose for a
   * render that will only carry three pictures. That resolves against nothing and the render comes
   * back describing the wrong asset, silently.
   *
   * ⚠️ Optional, and the `segments` path survives as the fallback: a caller with no packing (a
   * test) still gets the old behaviour rather than an empty roster, which would be a different
   * wrong answer.
   */
  h3Packing?: PackedGeneration[];
  /**
   * Where each shot lands on the DELIVERED clock, when the caller already has it.
   *
   * Optional only so a caller without one still works; supply it wherever you can. It is what the
   * transcript is windowed against, and the workspace draws its timeline from the very same object
   * — so passing it makes "the writer was told shot 7 is singing" and "shot 7 is where the picture
   * says it is" the same statement instead of two that agree by luck.
   */
  layout?: TimelineLayout;
  /**
   * The project song, as the `asset:<hash>` the editor holds it under.
   *
   * H3 only, and used for ONE thing: telling the server which file to cut this generation's slice
   * out of so the prompt writer can hear it. It is never the bytes — the browser has only the ref,
   * which is the point (see `LtxPromptFromLlmRequest.h3_song_asset`).
   *
   * ⚠️ Sending it does not attach anything. The attachment is gated server-side on
   * `KOBLEM_H3_WRITER_SONG_AUDIO`, which is OFF by default because the context cost has never been
   * measured.
   */
  songAsset?: string | null;
  /**
   * The project song's duration, seconds — what the editor measured off the waveform.
   *
   * Only two things read it, and neither is the lane's clock: which generations get a slice at all
   * (one past the end of the song gets none), and how long each slice is. Every OFFSET is correct
   * without it, which is why {@link promptLayout} has never needed one.
   */
  songSeconds?: number;
  /** Output geometry, so an H3 prompt's spec line can state the aspect the corpus always states. */
  width?: number;
  height?: number;
}

/**
 * The `asset:<hash>` prefix stripped, or `undefined` for anything that is not a well-formed ref.
 *
 * 🔴 The BARE hash is what crosses the wire. koblem's request edge rewrites every `asset:<hash>`
 * string in a body into the base64 of its bytes, so a prefixed field would inline the whole song
 * into a request that wants fourteen seconds out of the middle of it. A legacy project whose track
 * is still literal base64 sends nothing at all: shipping megabytes up on every draft is precisely
 * what this avoids.
 */
export function bareAssetHash(value: string | null | undefined): string | undefined {
  const hash = value?.trim().replace(/^asset:/, '');
  return hash && /^[0-9a-f]{64}$/.test(hash) ? hash : undefined;
}

/**
 * The aspect as the corpus phrases it — "16:9 landscape", "9:16 vertical", "1:1 square".
 *
 * Named ratios only, and only the six the corpus actually uses; anything else is described by
 * orientation rather than invented as an odd ratio, because a made-up "1.87:1" in a prompt is a
 * number the model has no reason to have seen. This is presentation for the prompt's spec line —
 * the canvas itself is fixed by the request's width/height and the prompt cannot change it.
 */
export function aspectPhrase(width?: number, height?: number): string | undefined {
  if (!width || !height) return undefined;
  const r = width / height;
  const named: [number, string][] = [
    [21 / 9, '21:9 ultrawide'],
    [16 / 9, '16:9 landscape'],
    [4 / 3, '4:3 landscape'],
    [1, '1:1 square'],
    [3 / 4, '3:4 portrait'],
    [9 / 16, '9:16 vertical']
  ];
  for (const [ratio, phrase] of named) {
    if (Math.abs(r - ratio) / ratio < 0.04) return phrase;
  }
  return r > 1.05 ? 'landscape' : r < 0.95 ? 'vertical' : 'square';
}

/**
 * Does this shot put text on screen?
 *
 * Read off the shot's own text rather than asked of the user, because it only gates a paragraph of
 * direction and a false negative merely leaves the prompt as it would have been. It exists at all
 * because H3 REVERSES LTX here: LTX's writer is told "never put on-screen text or logos in frame"
 * (it renders text badly), while legible text and interfaces are one of the three things H3 is
 * specifically good at, and the corpus art-directs typography as its own block.
 */
export function shotWantsText(clip: DirectorClip): boolean {
  const hay = `${clip.prompt} ${clip.idea ?? ''} ${clip.gist ?? ''}`.toLowerCase();
  return /\b(title|titles|caption|subtitle|credits?|typograph|lettering|logo|headline|text|ui|hud|menu|interface|sign|signage|screen)\b/.test(
    hay
  );
}

/**
 * The grounding shared by draft / reroll / rework / summarise for one clip: its frames and
 * references, neighbouring prompts, the story-so-far arc, and project canon.
 */
export function shotContext(
  ctx: PromptContext,
  clip: DirectorClip,
  idx: number,
  withImages = true
): Partial<LtxPromptFromLlmRequest> {
  const { clips, story, lyrics } = ctx;
  // Built once for the whole shot rather than per branch: it is O(clips) and `shotContext` is
  // called for every shot of a draft-all.
  const layout = promptLayout(ctx);
  const images: LtxPromptImage[] = [];
  const references: LtxPromptImage[] = [];
  if (withImages) {
    if (clip.mode === 'i2v' && clip.image_b64) {
      images.push({ image_b64: clip.image_b64, role: idx === 0 ? 'opening' : 'scene' });
    }
    for (const kf of clip.keyframes) {
      if (kf.image_b64) {
        images.push({ image_b64: kf.image_b64, role: 'keyframe', seconds: kf.seconds });
      }
    }
    for (const r of clip.references ?? []) {
      if (r.image_b64?.trim()) references.push({ image_b64: r.image_b64, role: 'reference' });
    }
  }
  const prior = clips.slice(0, idx).map(shotOneLiner).filter(Boolean);
  // Only the characters IN this shot: the writer is being told which appearance sentences this
  // paragraph must carry verbatim, and listing the whole roster invites it to write in someone who
  // is not in the shot.
  const cast = castForClip(clip, ctx.characters);
  const engine = ctx.engine ?? 'ltx';

  // ── H3 ────────────────────────────────────────────────────────────────────
  //
  // A separate return rather than extra keys on the LTX one, and deliberately so: half the LTX
  // grounding is not merely irrelevant here, it is WRONG. `continuation` and `closes_scene` both
  // describe a carried final frame that does not exist on an engine whose shots share no latent
  // state; `audio_supplied` is meaningless where the model generates the soundtrack; `beats` is a
  // conditioning layout the H3 route refuses outright. Sending them anyway would have the writer
  // reason about a chain it is not part of.
  if (engine === 'h3') {
    // 🔴 A pinned shot renders on `fl2va`, which takes no omni references at all — so the writer
    // must not be told about references it cannot address. Routed through the capability flag
    // rather than re-deriving the rule here; it is one rule and it now has one home.
    const pinned = pinBlocksReferences('h3', pinsAFrame(clip));
    // 🔴 The packed render's OWN list when there is one — `reference_ids` narrowing included, and
    // the synthesised keyframe pictures included too, because those take real ordinals the prompt
    // has to be able to name. Falls back to the legacy `segments` scope only where no packing was
    // supplied. See `PromptContext.h3Packing`.
    const gens = ctx.h3Packing ?? [];
    const genIdx = gens.findIndex((g) => g.shots.some((s) => s.clip.id === clip.id));
    const gen = genIdx >= 0 ? gens[genIdx] : undefined;
    const refs = pinned
      ? []
      : gen
        ? h3RefBriefs(gen.references, ctx.h3Soundtrack)
        : h3RefsForShot(ctx.h3References ?? [], idx, ctx.h3Soundtrack);
    const aspect = aspectPhrase(ctx.width, ctx.height);
    // 🔴 THIS generation's slice of the song, and everything that follows from knowing where it
    // sits: which local seconds carry a voice, and (opt-in, server-gated) the audio itself.
    //
    // Needs the packing — without it there is no generation, so no offset, so no local clock, and a
    // lane on the wrong clock is worse than no lane. A caller with no packing keeps `speech_in_shot`.
    const slice = gen ? songSlices(ctx).slices[genIdx] : undefined;
    // No `generation` number: this path describes one generation, and numbering it would invite the
    // writer to address a neighbour it has not been told about.
    const lane =
      gen && slice && lyrics.length
        ? vocalLaneFor(lyrics, gen, slice.songOffsetSeconds)
        : undefined;
    const songAsset = bareAssetHash(ctx.songAsset);
    return {
      // The shot's own length, unsnapped.
      //
      // 🔴 It is a SHOT, not a render: on H3 several shots are packed into one ~15 s generation and
      // only the generation snaps onto the frame grid, absorbing the rounding into its last shot.
      // Snapping here (which this used to do) told the writer every shot was 0.7 s longer than it
      // is, and it placed its cues accordingly.
      seconds: clip.seconds,
      engine: 'h3',
      ...(refs.length ? { h3_references: refs } : {}),
      ...(pinned && withImages && clip.h3_end_image ? { h3_end_image: true } : {}),
      ...(clip.h3_audio?.trim() ? { h3_audio: clip.h3_audio.trim() } : {}),
      ...(aspect ? { aspect } : {}),
      ...(shotWantsText(clip) ? { on_screen_text: true } : {}),
      // 🔴 Both, and that is not belt-and-braces: the server IGNORES `speech_in_shot` whenever a
      // lane is present (they are read off the same transcript, so they can only agree, and the
      // boolean's wording is the standing permission the lane exists to bound). It is still sent so
      // a koblem that predates the lane keeps the behaviour it was written against.
      ...(lyrics.length ? { speech_in_shot: shotHasSpeech(layout, lyrics, clip.id) } : {}),
      ...(lane ? { h3_vocal_lanes: [lane] } : {}),
      // Opt-in and server-gated — see `PromptContext.songAsset`. Only meaningful together, so all
      // three go or none do.
      ...(songAsset && slice && !slice.skipped
        ? {
            h3_song_asset: songAsset,
            h3_song_offset_seconds: slice.songOffsetSeconds,
            h3_song_slice_seconds: slice.sliceSeconds
          }
        : {}),
      // The opener still rides as an image when the shot pins one and has no references — the VL
      // model reads it to write the motion out of it, exactly as on LTX.
      ...(refs.length === 0 && images.length ? { images } : {}),
      // "A shot like this" images. Unrelated to H3's omni references and safe on either engine —
      // they never reach the render, they only steer the writing.
      ...(references.length ? { references } : {}),
      ...(prior.length ? { prior_shots: prior } : {}),
      prev_prompt: clips[idx - 1]?.prompt?.trim() || undefined,
      next_prompt: clips[idx + 1]?.prompt?.trim() || undefined,
      ...(storyIsEmpty(story) ? {} : { story }),
      ...(cast.length ? { cast } : {})
    };
  }

  return {
    mode: clip.mode,
    seconds: clip.seconds,
    fps: ctx.fps,
    // A shot with its OWN clip has audio even when the project track is empty — the script LLM
    // needs to know so it writes to a soundtrack rather than for silence.
    audio_supplied: ctx.audioKind !== 'none' || !!clip.audio_b64,
    // Without it the writer reaches for "singing" as generic liveliness and the render lip-syncs
    // over an instrumental break. Undefined when there is no transcript at all — the writer is then
    // told nothing rather than guessing.
    ...(lyrics.length ? { speech_in_shot: shotHasSpeech(layout, lyrics, clip.id) } : {}),
    // A shot past the first that does not start fresh renders from the prior shot's final frame —
    // treat it as i2v (describe the change, not the standing scene) even with no attached image.
    continuation: idx > 0 && !startsFresh(clip),
    closes_scene: closesScene(clips, idx),
    ...(images.length ? { images } : {}),
    ...(references.length ? { references } : {}),
    ...(prior.length ? { prior_shots: prior } : {}),
    prev_prompt: clips[idx - 1]?.prompt?.trim() || undefined,
    next_prompt: clips[idx + 1]?.prompt?.trim() || undefined,
    // This shot's beats, so the writer knows those moments are conditioned SEPARATELY and writes
    // the setting only. Without it the two halves of authoring fight: the planner separates the
    // actions into beats and a beat-unaware draft narrates them all back into the prompt, which
    // re-averages exactly what the beats were added to separate. `label` is editor-only.
    beats: (clip.beats ?? [])
      .filter((b) => b.text.trim().length > 0)
      .map((b) => ({ seconds: b.seconds, text: b.text.trim() })),
    ...(storyIsEmpty(story) ? {} : { story }),
    ...(cast.length ? { cast } : {})
  };
}
