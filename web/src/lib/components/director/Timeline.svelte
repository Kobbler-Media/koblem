<script lang="ts">
  import { untrack } from 'svelte';
  import { waveformPeaks } from '@koblibs/ui';
  import { assetSrc } from '#lib/director/assets.svelte.ts';
  import type { DirectorClip, LyricWord } from '#lib/api/client.ts';
  import { secondsToFrame, frameToSeconds } from '#lib/ltx/frames.ts';
  import { reorderTarget, trimmedSeconds } from '#lib/director/timeline-drag.ts';
  import { shotGist } from '#lib/director/shot-list.ts';
  import { h3GenerationGist, type PackedGeneration } from '#lib/director/h3-generations.ts';
  import { parseH3Block } from '#lib/director/h3-block.ts';
  import { formatCutTime, h3ShotIsCut } from '#lib/director/h3-prompt.ts';
  import {
    H3_TRIM_JOIN,
    h3Timecode,
    markIsTrimmed,
    planSoundtrack,
    runUpAt,
    runUpSecondsFor,
    shotClockAt,
    type ShotClock
  } from '#lib/director/h3-soundtrack.ts';
  import {
    layoutClips,
    ltxDeliveredSeconds,
    MIN_CLIP_SECONDS
  } from '#lib/director/timeline-layout.ts';
  import {
    fitPhraseLabel,
    lyricPhrases,
    rulerTicks,
    tickLabel
  } from '#lib/director/timeline-ticks.ts';
  import { DEFAULT_VOCAB, capitalise, type EngineVocab } from '#lib/director/engine.ts';

  // Multi-lane, zoomable, scrollable timeline. The page owns the clip array; this
  // component mutates it in place (reorder / trim / move-pin) and calls back for
  // actions that need page context (select, seek). Time flows on X; three lanes:
  // video clips, keyframe pins, and the audio waveform + lyric chips.
  let {
    clips = $bindable([]),
    pxPerSec = $bindable(80),
    fitMode = $bindable(false),
    windowSeconds = $bindable(null),
    playhead = $bindable(0),
    playing = $bindable(false),
    audioSrc = null,
    selectedIds = $bindable([]),
    selectedPin = $bindable(null), // { clipId, index } | null
    lyrics = [],
    waveform = null,
    waveDuration = 0,
    snap = true,
    fps = 24,
    generations = [],
    audioLane = true,
    mark = null,
    onchange,
    shotStatus,
    proxyStatus,
    findingSeverity,
    draftFailed,
    onContinueScene,
    onopengeneration,
    vocab = DEFAULT_VOCAB
  }: {
    /**
     * What this engine calls its units. Every block in the video lane is one `renderUnit` — a shot,
     * on both engines — and `subUnit` describes only the beats drawn as ticks INSIDE a block, which
     * carry no label. Display only: the props are still `clips` and `generations`.
     */
    vocab?: EngineVocab;
    clips: DirectorClip[];
    pxPerSec?: number;
    fitMode?: boolean;
    windowSeconds?: number | null;
    playhead?: number;
    /**
     * Whether the project song is playing. Bindable so the page's keymap can drive it — the
     * `<audio>` lives here, next to the playhead it moves.
     */
    playing?: boolean;
    /**
     * The project song, as something an `<audio>` can load. `null` (or `''` while the asset is
     * still being fetched) leaves the transport out entirely.
     *
     * 🔴 On the SONG's clock, which since the layout moved to delivered time is also the timeline's
     * — that equality is the whole reason playback can drive the playhead directly.
     */
    audioSrc?: string | null;
    selectedIds?: string[];
    selectedPin?: { clipId: string; index: number } | null;
    lyrics?: LyricWord[];
    waveform?: Float32Array | null;
    waveDuration?: number;
    snap?: boolean;
    fps?: number;
    /**
     * MiniMax-H3 generations, one per clip — the block in the video lane IS the render.
     *
     * Empty on LTX. On H3 the authored block lives here rather than on the clip, so this is where
     * the `[Shot N]` beat ticks and the shot's own gist are read from.
     */
    generations?: PackedGeneration[];
    /**
     * Whether the audio + lyric lane is meaningful at all.
     *
     * True on both engines. It was false on H3 while `audioInput` conflated "the engine takes no
     * audio field" with "a song cannot be attached" — but the lane draws a waveform and a
     * transcript, neither of which the engine ever sees.
     */
    audioLane?: boolean;
    /**
     * A moment being pointed at somewhere ELSE — today, a `[Shot N] At MM:SS.mmm` mark the cursor is
     * resting on in the prompt editor.
     *
     * 🔴 In SONG seconds, converted by the caller. The prompt's clock restarts at every render
     * boundary (`shotClockAt`), so a mark handed over on the prompt's own clock would draw at the
     * right number and the wrong place — which is the exact confusion this line exists to remove.
     * `null` draws nothing.
     */
    mark?: { seconds: number; label: string } | null;
    onchange?: () => void;
    shotStatus?: (c: DirectorClip) => 'ok' | 'stale' | 'none' | 'busy';
    proxyStatus?: (c: DirectorClip) => 'ok' | 'stale' | 'none' | 'busy';
    findingSeverity?: (clipId: string) => 'warn' | 'info' | null;
    draftFailed?: (clipId: string) => boolean;
    onContinueScene?: () => void;
    /** Open a shot's composer — where its sections, and the seam before it, are edited. */
    onopengeneration?: (index: number) => void;
  } = $props();

  const LANE_H = 62;
  const OVERVIEW_H = 22;
  const PIN_LANE_H = 34;
  const SNAP_PX = 8;

  let selectedSet = $derived(new Set(selectedIds));

  /**
   * The transcript as PHRASES, which is the only unit that fits.
   *
   * 🔴 Per-word labels cannot be placed. Sung words are ~0.3 s apart and their text is several
   * times wider than that at any usable zoom, so the old greedy pass could only keep a scattered
   * minority and collapse the rest to anonymous ticks — a picture of the rhythm with the words
   * missing, which is confetti rather than a transcript. A phrase owns the whole span of its own
   * words, so its label has room to be read as language, and the words stay underneath as the
   * ticks you point at for an exact onset.
   */
  let phrases = $derived(lyricPhrases(lyrics));
  // Live feedback while dragging: a snap guide line + a reorder drop marker.
  let snapLineSec: number | null = $state(null);
  let dropIndex: number | null = $state(null);

  // ── The hover time readout ────────────────────────────────────────────────────────────────
  //
  // 🔴 Two clocks, and the prompt uses the one the ruler does NOT show. A `[Shot N] At MM:SS.mmm`
  // mark is measured from its GENERATION's frame 0, not from the start of the song — each
  // generation is conditioned on its own slice of the track, so the model's clock restarts at every
  // render boundary. An author reading a cue off the waveform reads a SONG time and writes it into
  // a prompt that means something else; the mark then lands early by the whole preceding timeline,
  // or falls outside the duration entirely and stops being a cue at all (`PROMPT-CONTRACT.md:176`).
  //
  // So the chip shows both, and shows the shot-local one in the exact `MM:SS.mmm` spelling the
  // prompt takes, ready to paste.
  //
  // 🔴 And the shot-local one carries the RUN-UP. An anchored continuation re-renders the previous
  // shot's last 22 frames as its own head and trims them, so the generation's clock is already
  // 0.917 s in when its block starts: `local = (song − deliveredAt) + runUp`. `shotClockAt` has
  // always measured from `songOffsetSeconds` and therefore says this correctly — what changed on
  // 2026-08-14 is that the number is 0.917 s rather than 0.042 s, which is the difference between a
  // rounding nobody notices and a whole beat. Every readout on this surface goes through it.
  let hoverSec: number | null = $state(null);

  /**
   * The generation offsets, from the SAME arithmetic the render uses — a generation's frames snap
   * up onto 17n+5 and it loses its anchor frame at delivery, so accumulating authored seconds
   * drifts from the render by its own rounding at every boundary.
   *
   * It is now the axis the whole component draws on (see `layout` below), not just the readout's
   * conversion: those were two different clocks, and that was the bug. `waveDuration` of 0 (no song
   * attached) still yields correct offsets; it only marks the slices skipped.
   */
  let songPlan = $derived(planSoundtrack(generations, waveDuration));

  /**
   * A lyric the cursor is actually on, which is a stronger answer than the cursor's own x.
   *
   * Pointing at a word and reading back "the pixel you are over is 22.79 s" is the wrong number by
   * however far the cursor is from the onset — and an onset is exactly the thing worth marking, so
   * it is the one time on this axis that should never be approximate.
   */
  let hoverLyric: LyricWord | null = $state(null);

  /** Song time under the cursor — or a hovered word's exact onset — on the prompt's own clock. */
  let hoverAt = $derived.by(() => {
    const at = hoverLyric ? hoverLyric.start : hoverSec;
    return at === null ? null : shotClockAt(songPlan, generations, at);
  });

  /**
   * The SECOND shot the cursor is inside, when it is inside two — or `null`, which is the usual case.
   *
   * 🔴 Near a seam a song moment genuinely belongs to two renders and they do not conflict: it is in
   * shot N's delivered block (the picture actually on screen there, which is what {@link hoverAt}
   * reports) and it is also inside shot N+1's RUN-UP, because shot N+1 re-renders that stretch to
   * get itself moving into the join and then throws it away. Saying so is the difference between a
   * hatched band that looks like a bug and one that reads as a mechanism.
   */
  let hoverRunUp = $derived.by(() => {
    const at = hoverLyric ? hoverLyric.start : hoverSec;
    return at === null ? null : runUpAt(songPlan, at);
  });

  /** How the unit is named in a sentence — `Shot`, capitalised, as the mark spells it. */
  let unitCap = $derived(capitalise(vocab.renderUnit));

  /**
   * `[Shot N] At MM:SS.mmm` for a delivered time — the exact bytes a prompt takes for that moment.
   *
   * 🔴 **The one conversion every readout on this surface goes through.** The owner's workflow is to
   * find a moment on the timeline — a lyric onset, a beat, the playhead — read its number, and type
   * it into that shot's prompt. That number is NEVER the song time and it is never the offset into
   * the block either: the render's clock starts `runUp` before the block, so a moment 2.0 s into an
   * anchored shot is `At 00:02.917`. Anything on screen that shows one of the other two numbers
   * beside a place you could type is an invitation to be 0.917 s wrong.
   */
  function markAt(at: ShotClock): string | null {
    return at.shot === null ? null : `[${unitCap} ${at.shot}] At ${h3Timecode(at.localSeconds)}`;
  }

  /**
   * One sentence naming a song moment on every clock that matters — for a `title`.
   *
   * Shot clock FIRST and song time in parentheses after it, deliberately: the shot clock is the one
   * that gets typed somewhere, and the song time is context.
   */
  function markNote(songSeconds: number): string {
    const at = shotClockAt(songPlan, generations, songSeconds);
    const song = `song ${h3Timecode(songSeconds)}`;
    const mark = markAt(at);
    if (!mark) return song;
    const alsoRunUp = runUpAt(songPlan, songSeconds);
    const also = alsoRunUp
      ? ` · ${vocab.renderUnit} ${alsoRunUp.shot} re-renders this instant as its run-up ` +
        `(${h3Timecode(alsoRunUp.localSeconds)} on its clock) and then throws it away, so it cannot ` +
        `be marked there`
      : '';
    return `${mark} — paste that into ${vocab.renderUnit} ${at.shot}'s prompt (${song})${also}`;
  }

  /**
   * The x-axis, in DELIVERED seconds — the same clock as the waveform, the lyric chips, the `mark`
   * line and the hover readout above.
   *
   * 🔴 Not `sum(clip.seconds)`, and that was the bug. Authored seconds and song seconds diverge by
   * the frame-grid snap-up plus a trimmed seam frame at every render boundary, always in the same
   * direction — so the blocks crept ahead of the waveform they sit under and a beat plainly inside
   * shot 8 read back "Shot 7". See `timeline-layout.ts`.
   */
  let layout = $derived(layoutClips(clips, generations, songPlan, ltxDeliveredSeconds(fps)));
  let offsets = $derived(layout.clips.map((l) => l.start));
  let widths = $derived(layout.clips.map((l) => l.width));
  /**
   * The run-up in front of each block — seconds the render covers before its first delivered frame.
   *
   * 🔴 It reaches BACKWARDS over the previous block, it is not a gap in front of this one. Nothing
   * in `offsets` or `widths` moves because of it; the delivered lane is end-to-end as it always was.
   * See `timeline-layout.ts`. 0 on LTX and on the opening shot, so every use of it is unconditional.
   */
  let runUps = $derived(layout.clips.map((l) => l.runUp));
  /** Where each shot's render — and therefore its own `At 00:00.000` — actually begins. */
  let renderStarts = $derived(layout.clips.map((l) => Math.max(0, l.start - l.runUp)));
  /** What each clip's drawn width owes to the frame grid — the delta a trim has to undo. */
  let absorbed = $derived(layout.clips.map((l) => l.width - l.authored));
  let clipsEnd = $derived(layout.end);
  let totalSeconds = $derived(Math.max(waveDuration, clipsEnd, 4));
  let ticks = $derived(rulerTicks(totalSeconds, pxPerSec));

  /**
   * Everything a block draws that lives on the RENDER rather than on the clip.
   *
   * `gist` above all: `collapseH3Beats` empties `clip.prompt`, so on H3 the only label the shot has
   * is the generation's. `rendered` is the second the timeline used to get wrong — a generation's
   * frames round UP onto the engine's grid, and that extra time is played but never authored.
   */
  const shotMeta = $derived.by(() => {
    const m = new Map<
      string,
      {
        gi: number;
        first: boolean;
        rendered: number;
        gist: string;
        over: boolean;
        join: string | null;
      }
    >();
    generations.forEach((g, gi) => {
      const gist = h3GenerationGist(g);
      g.shots.forEach((s, i) =>
        m.set(s.clip.id, {
          gi,
          first: i === 0,
          rendered: s.renderedSeconds,
          gist,
          over: g.overBudget,
          join: g.continuation?.mode ?? null
        })
      );
    });
    return m;
  });

  /**
   * The `[Shot N]` cut times inside each block, in seconds from the block's own left edge.
   *
   * 🔴 Read from the BLOCK, which is the document — `h3ShotIsCut` exists because a clip-side copy of
   * "is this a cut" was a mirror that had one writer and a lying fallback. `atSeconds` is on the
   * generation's clock, so a shot that is not the generation's first has its own offset subtracted.
   *
   * 🔴 **And the RUN-UP comes off too, which it did not until 2026-08-14.** `atSeconds` is on the
   * RENDER clock, which starts `runUp` before the block is delivered — so a mark drawn at `at - from`
   * lands `runUp` too far right. That was invisible at the old one-frame trim (0.042 s) and is
   * 22 frames now: every authored beat on every anchored shot drew most of a second late, under the
   * waveform it was written against. The prompt text is untouched — the picture moves to meet it.
   *
   * A mark at or under the run-up is **trimmed**: the model renders it and delivery discards those
   * frames, so it is a cue nobody ever sees. It is kept in the list rather than dropped, drawn at the
   * block's head in the danger colour, because a mark that silently vanishes from the timeline is
   * exactly how one came to sit at 0.592 s in the live project and do nothing.
   */
  const h3Beats = $derived.by(() => {
    type Mark = { at: number; trimmed: boolean; cut: boolean; label: string };
    const m = new Map<string, Mark[]>();
    for (const g of generations) {
      const block = g.doc.prompt?.trim();
      if (!block) continue;
      const parsed = parseH3Block(block).shots;
      const runUp = runUpSecondsFor(g);
      g.shots.forEach((s, si) => {
        const from = s.cutAtSeconds ?? 0;
        const to = from + Math.max(0, s.clip.seconds);
        // Only the generation's FIRST shot has anything in front of it that delivery drops; the
        // rest are already mid-render. Same rule `LaidOutClip.runUp` follows.
        const head = si === 0 ? runUp : 0;
        const marks: Mark[] = [];
        for (const b of parsed) {
          const at = b.atSeconds;
          if (at === null || at <= from || at >= to) continue;
          const cut = h3ShotIsCut(b);
          const drawn = at - from - head;
          // `at` is already on the GENERATION's clock, which is the clock the run-up is measured on,
          // so the reachability test is on `at` and not on `drawn`. Through `markIsTrimmed` rather
          // than `drawn < 0` so the boundary rule — inclusive, see its doc — lives in exactly one
          // place and this cannot quietly disagree with the composer's warning about the same mark.
          const trimmed = markIsTrimmed(at, head);
          const kind = cut ? 'a cut' : 'the camera keeps rolling';
          marks.push({
            at: drawn,
            trimmed,
            cut,
            label: trimmed
              ? `[${unitCap} ${b.index + 1}] At ${formatCutTime(at)} — ${kind}, but it lands in the ` +
                `${h3Timecode(head)} run-up this ${vocab.renderUnit} re-renders and then throws ` +
                `away. Nobody will see it. Move it past ${formatCutTime(head)}.`
              : `[${unitCap} ${b.index + 1}] At ${formatCutTime(at)} — ${kind}`
          });
        }
        if (marks.length) m.set(s.clip.id, marks);
      });
    }
    return m;
  });

  const CONTINUE_W = 108;
  let contentWidth = $derived(totalSeconds * pxPerSec + 40 + (onContinueScene ? CONTINUE_W : 0));

  // Fit-to-width: while fitMode is on, the shots stretch to fill the visible lane width
  // (no more tiny strips with dead space beside them). Any manual zoom turns fitMode off.
  let laneW = $state(0);
  $effect(() => {
    if (!laneEl) return;
    const ro = new ResizeObserver((entries) => {
      laneW = entries[0].contentRect.width;
    });
    ro.observe(laneEl);
    laneW = laneEl.clientWidth;
    return () => ro.disconnect();
  });
  $effect(() => {
    // Hold the scale steady while a trim/reorder drag is live, or the whole timeline
    // rescales under the cursor. Re-fits automatically once the drag ends.
    if (!fitMode || drag || laneW <= 0 || totalSeconds <= 0) return;
    const usable = laneW - 40 - (onContinueScene ? CONTINUE_W : 0) - 8;
    // Floor at a readable shot scale: a short cut stretches to fill the width, but a long one
    // keeps legible shots and overflows instead of shrinking to slivers — so it can be scrolled.
    pxPerSec = Math.min(320, Math.max(56, Math.round(usable / totalSeconds)));
  });
  $effect(() => {
    // "Seconds visible" presets: scale pxPerSec so exactly windowSeconds of song fills the
    // viewport, recomputed from the live lane width so the zoom reflows on browser resize.
    if (windowSeconds == null || fitMode || drag || laneW <= 0 || windowSeconds <= 0) return;
    pxPerSec = Math.min(320, Math.max(20, Math.round((laneW - 8) / windowSeconds)));
  });

  function xToSec(px: number): number {
    return Math.max(0, px / pxPerSec);
  }

  /**
   * A time on the drawn axis → seconds from the start of clip `i`, on the clip's own clock.
   *
   * 🔴 A plain subtraction, NOT a proportional rescale. A generation's round-up onto the frame grid
   * lands entirely on the TAIL of its last shot, so every moment before that tail maps 1:1 — the
   * shot really does begin where its block begins and really does play second-for-second from
   * there. Scaling by `authored / width` would move every pin and beat in the shot to correct for
   * time that is only ever added at the end. Same rule as `beatWordsFor`.
   */
  function withinClip(i: number, sec: number): number {
    const l = layout.clips[i];
    return Math.max(0, l ? sec - l.start : sec);
  }

  // Snap a candidate time to the nearest clip boundary / lyric onset / playhead. All on the drawn
  // axis, which is delivered time — snapping to an authored boundary would land the guide line
  // somewhere no edge is.
  function snapSec(sec: number, excludeClipId?: string): number {
    if (!snap) return sec;
    const targets: number[] = [playhead];
    for (const l of layout.clips) {
      if (l.id !== excludeClipId) {
        targets.push(l.start);
        targets.push(l.start + l.width);
        // Where the shot's own clock reads 00:00.000. It is not an edge of anything delivered, but
        // it IS the instant a loop over "what this render sees" has to open on, and the ruler drag
        // is the gesture that marks one by hand.
        if (l.runUp > 0) targets.push(Math.max(0, l.start - l.runUp));
      }
    }
    for (const w of lyrics) targets.push(w.start);
    // Bars, not beats — a cut wants the downbeat, and offering every beat makes the strongest
    // target in the bar no easier to hit than the weakest.
    for (const b of grid.bars) targets.push(b);
    let best = sec;
    let bestDist = SNAP_PX / pxPerSec;
    for (const tg of targets) {
      const d = Math.abs(tg - sec);
      if (d < bestDist) {
        bestDist = d;
        best = tg;
      }
    }
    snapLineSec = best !== sec ? best : null; // guide line when a snap is active
    return best;
  }

  function isAdditive(e: Event): boolean {
    const m = e as MouseEvent;
    return !!(m.shiftKey || m.metaKey || m.ctrlKey);
  }

  // Ctrl/⌘ + wheel = zoom around the cursor; plain vertical wheel scrolls the timeline
  // horizontally when it overflows (otherwise the wheel is swallowed and it looks unscrollable).
  function onWheel(e: WheelEvent) {
    if (e.ctrlKey || e.metaKey) {
      e.preventDefault();
      fitMode = false;
      windowSeconds = null;
      const before = xToSec(localX(e as unknown as PointerEvent));
      const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
      pxPerSec = Math.min(320, Math.max(20, Math.round(pxPerSec * factor)));
      // keep the point under the cursor stable
      laneEl.scrollLeft = before * pxPerSec - (e.clientX - laneEl.getBoundingClientRect().left);
      return;
    }
    const overflow = laneEl.scrollWidth - laneEl.clientWidth;
    if (overflow > 1) {
      const d = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      if (d !== 0) {
        laneEl.scrollLeft += d;
        e.preventDefault();
      }
    }
  }

  // ── Pointer interactions ────────────────────────────────────────────────────
  let laneEl: HTMLDivElement;
  type Drag =
    | { kind: 'seek' }
    | { kind: 'region'; from: number }
    | { kind: 'reorder'; id: string; grabDx: number; startIndex: number }
    | { kind: 'trim'; id: string; startSec: number }
    | { kind: 'pin'; clipId: string; index: number; startSec: number }
    | { kind: 'beat'; clipId: string; index: number; startSec: number; startLabel?: string }
    | null;
  let drag: Drag = $state(null);
  // Every drag records where it STARTED, and an edit is reported only if it ends somewhere else.
  // A touch pan over the lane cancels the pointer stream mid-drag, a plain tap opens and closes a
  // clip without moving it, and a clip dragged out and back lands on its own index — none of those
  // changed the cut, and each one reporting an edit means an autosave (and a possible conflict
  // banner) for doing nothing.

  function localX(e: PointerEvent): number {
    const rect = laneEl.getBoundingClientRect();
    return e.clientX - rect.left + laneEl.scrollLeft;
  }

  function startSeek(e: PointerEvent) {
    laneEl.setPointerCapture(e.pointerId);
    drag = { kind: 'seek' };
    seekTo(snapSec(xToSec(localX(e))));
    // A tap is a seek too, and a seek you cannot hear is the thing this exists to fix.
    scrubGrain(playhead);
  }

  /**
   * Mark a span to loop, by dragging on the ruler.
   *
   * The ruler rather than the lane: every pixel of the lane already belongs to a shot (seek,
   * reorder, trim), and a modifier-drag over the blocks would be a gesture you can only discover by
   * being told. The ruler is the one strip that is purely time, which is exactly what a loop is.
   */
  function startRegion(e: PointerEvent) {
    laneEl.setPointerCapture(e.pointerId);
    const at = snapSec(xToSec(localX(e)));
    drag = { kind: 'region', from: at };
    loop = null;
  }

  function startReorder(e: PointerEvent, c: DirectorClip, idx: number) {
    e.stopPropagation();
    laneEl.setPointerCapture(e.pointerId);
    // Keep an existing multi-selection if grabbing one of its members; else select just this.
    if (!selectedSet.has(c.id)) selectedIds = [c.id];
    selectedPin = null;
    const clipLeft = offsets[idx] * pxPerSec;
    drag = { kind: 'reorder', id: c.id, grabDx: localX(e) - clipLeft, startIndex: idx };
  }

  function startTrim(e: PointerEvent, c: DirectorClip) {
    e.stopPropagation();
    laneEl.setPointerCapture(e.pointerId);
    selectedIds = [c.id];
    drag = { kind: 'trim', id: c.id, startSec: c.seconds };
  }

  function startPinDrag(e: PointerEvent, clipId: string, index: number) {
    e.stopPropagation();
    laneEl.setPointerCapture(e.pointerId);
    selectedPin = { clipId, index };
    selectedIds = [clipId];
    drag = {
      kind: 'pin',
      clipId,
      index,
      startSec: clips.find((c) => c.id === clipId)?.keyframes[index]?.seconds ?? 0
    };
  }

  /** Drag a Prompt Relay beat tick along its clip. Beats are TIMES, not frames, so unlike a pin
   * this snaps to the same grid as everything else (lyrics/clip edges) and not to a frame index. */
  function startBeatDrag(e: PointerEvent, clipId: string, index: number) {
    e.stopPropagation();
    laneEl.setPointerCapture(e.pointerId);
    selectedIds = [clipId];
    const beat = clips.find((c) => c.id === clipId)?.beats?.[index];
    drag = {
      kind: 'beat',
      clipId,
      index,
      startSec: beat?.seconds ?? 0,
      startLabel: beat?.label
    };
  }

  function onMove(e: PointerEvent) {
    // Tracked on every move, not only while dragging — the readout is for reading a cue off the
    // waveform, which is something you do without pressing anything.
    hoverSec = xToSec(localX(e));
    if (!drag) return;
    const sec = xToSec(localX(e));
    if (drag.kind === 'seek') {
      seekTo(snapSec(sec));
      scrubGrain(playhead);
    } else if (drag.kind === 'region') {
      const to = snapSec(sec);
      loop = { start: Math.min(drag.from, to), end: Math.max(drag.from, to) };
    } else if (drag.kind === 'trim') {
      const i = clips.findIndex((c) => c.id === (drag as { id: string }).id);
      if (i >= 0) {
        // The handle is dragged on the DRAWN edge, but `seconds` is authored — so the grid time
        // this clip currently absorbs comes back off before it is written. On the last shot of a
        // generation that is up to 0.7 s, which is the difference between the edge landing under
        // the cursor and landing most of a beat away from it.
        clips[i].seconds = trimmedSeconds(offsets[i] + absorbed[i], snapSec(sec, clips[i].id));
      }
    } else if (drag.kind === 'reorder') {
      // Reorder when the dragged clip's grab-point crosses a neighbour midpoint.
      const grabSec = xToSec(localX(e) - drag.grabDx);
      const from = clips.findIndex((c) => c.id === (drag as { id: string }).id);
      if (from < 0) return;
      const target = reorderTarget(widths, from, grabSec);
      dropIndex = target;
      if (target !== from) {
        const [moved] = clips.splice(from, 1);
        clips.splice(target, 0, moved);
      }
    } else if (drag.kind === 'pin') {
      const i = clips.findIndex((c) => c.id === (drag as { clipId: string }).clipId);
      if (i >= 0) {
        const kf = clips[i].keyframes[(drag as { index: number }).index];
        if (kf) {
          const rel = withinClip(i, snapSec(sec, clips[i].id));
          // Snap the dragged pin to a valid frame + clamp to the shot's REAL last frame, matching
          // the inspector's PinTime control (a pin at nominal `seconds` would exceed the 8k+1 range).
          const next = frameToSeconds(secondsToFrame(rel, fps, clips[i].seconds), fps);
          kf.seconds = next;
        }
      }
    } else if (drag.kind === 'beat') {
      const i = clips.findIndex((c) => c.id === (drag as { clipId: string }).clipId);
      const beat = clips[i]?.beats?.[(drag as { index: number }).index];
      if (beat) {
        const rel = withinClip(i, snapSec(sec, clips[i].id));
        const next = Math.max(0, Math.min(rel, clips[i].seconds));
        if (next !== beat.seconds) {
          beat.seconds = next;
          // The label records which lyric a beat was snapped to; once dragged it is stale.
          beat.label = undefined;
        }
      }
    }
  }

  /// Did this drag actually leave the cut different from how it found it? A beat is the one that
  /// needs restoring rather than just comparing: dragging it clears the lyric label it was snapped
  /// to, so a beat put back where it started has to get its label back too or "no net change" would
  /// still be a change.
  function netChanged(d: NonNullable<Drag>): boolean {
    // Neither the playhead nor a loop span is part of the cut — they are where you are LOOKING.
    // Reporting them would autosave (and risk a conflict banner) for having listened to something.
    if (d.kind === 'seek' || d.kind === 'region') return false;
    if (d.kind === 'reorder') return clips.findIndex((c) => c.id === d.id) !== d.startIndex;
    if (d.kind === 'trim') return clips.find((c) => c.id === d.id)?.seconds !== d.startSec;
    const clip = clips.find((c) => c.id === d.clipId);
    if (d.kind === 'pin') return clip?.keyframes[d.index]?.seconds !== d.startSec;
    const beat = clip?.beats?.[d.index];
    if (!beat || beat.seconds !== d.startSec) return true;
    beat.label = d.startLabel;
    return false;
  }

  function endDrag(e: PointerEvent) {
    if (!drag) return;
    try {
      laneEl.releasePointerCapture(e.pointerId);
    } catch {
      /* capture may already be gone */
    }
    const edited = netChanged(drag);
    // A tap on the ruler is a CLEAR, not a zero-length loop: the gesture that makes a region and
    // the gesture that removes it are the same one, so there is nothing extra to find.
    if (drag.kind === 'region' && loop && loop.end - loop.start < MIN_LOOP_SECONDS) loop = null;
    drag = null;
    snapLineSec = null;
    dropIndex = null;
    if (edited) onchange?.();
  }

  function selectClip(e: Event, c: DirectorClip) {
    e.stopPropagation();
    selectedPin = null;
    if (isAdditive(e)) {
      // Toggle membership in a multi-selection.
      selectedIds = selectedSet.has(c.id)
        ? selectedIds.filter((id) => id !== c.id)
        : [...selectedIds, c.id];
    } else {
      selectedIds = [c.id];
    }
  }

  // ── Playback ────────────────────────────────────────────────────────────────
  //
  // The song plays against the cut, and the playhead IS the audio's clock. That only works because
  // the layout above is on delivered time: on the old authored layout the audio would have been
  // right and the block under the playhead wrong, which is worse than no playback at all.
  //
  // A plain `<audio>`, deliberately — no AudioContext. Nothing here needs sample access (the
  // waveform is drawn from samples the page already decoded), and a context taken on this element
  // would be permanent for the life of the page (`web-audio-mediaelementsource-permanent`).
  let audioEl: HTMLAudioElement | undefined = $state();
  let canPlay = $derived(!!audioSrc && audioLane);

  /**
   * A span to loop while writing against it, or `null`.
   *
   * Deliberately NOT persisted and not reported through `onchange`: it is a place you are looking,
   * like the playhead, and a project that autosaved because you listened to a bar twice would be
   * the same scaffolding-in-the-document mistake as storing a zoom level per shot.
   */
  let loop: { start: number; end: number } | null = $state(null);
  /** Shorter than this and a region drag was a click, which means "clear". */
  const MIN_LOOP_SECONDS = 0.15;

  /**
   * Loop exactly one unit's span — the gesture that dragging the ruler approximates by hand.
   *
   * Toggles, so the same control that sets it clears it: pressing loop on the unit you are already
   * looping means "stop", not "set it to what it already is".
   */
  function loopSpan(start: number, end: number) {
    if (loop && Math.abs(loop.start - start) < 1e-6 && Math.abs(loop.end - end) < 1e-6) {
      loop = null;
      return;
    }
    loop = { start, end };
    seekTo(start);
  }
  // ── Beat grid ───────────────────────────────────────────────────────────────
  //
  // A tempo the author states, not one anything detects. The transcript gives word onsets, which is
  // where the SINGING is — but a cut lands on the bar, and nothing in the project knows where a bar
  // is. Two numbers are enough to draw them, and the drawing is only half the point: the bar times
  // join the snap targets, so a shot boundary dragged near a downbeat lands ON it.
  //
  // Transient, like the loop: a grid is a way of looking at the song, not a property of the cut,
  // and persisting it would put a view preference in the render document.
  let gridOn = $state(false);
  let bpm = $state(120);
  let gridOffset = $state(0);
  const BEATS_PER_BAR = 4;

  /** Bar and beat times across the whole axis, or empty when the grid is off. */
  let grid = $derived.by(() => {
    if (!gridOn || bpm <= 0) return { bars: [] as number[], beats: [] as number[] };
    const beat = 60 / bpm;
    const bars: number[] = [];
    const beats: number[] = [];
    // Beats are drawn only while they are far enough apart to be lines rather than a hatch.
    const showBeats = beat * pxPerSec >= 14;
    // 🔴 Windowed to the VIEWPORT, not the song. At 300 BPM a three-minute track is a thousand
    // lines, every one a DOM node re-derived on each zoom and each scroll, and all but a handful
    // scrolled out of sight. The margin covers the gap between a scroll and the re-render.
    const margin = 400 / pxPerSec;
    const from = Math.max(0, scrollX / pxPerSec - margin);
    const to = Math.min(totalSeconds, (scrollX + Math.max(laneW, 400)) / pxPerSec + margin);
    for (let i = Math.max(0, Math.floor((from - gridOffset) / beat)); ; i++) {
      const t = gridOffset + i * beat;
      if (t > to) break;
      if (t < from) continue;
      if (i % BEATS_PER_BAR === 0) bars.push(t);
      else if (showBeats) beats.push(t);
    }
    return { bars, beats };
  });

  /** Nudge the grid so its nearest downbeat lands on the playhead — "the one is HERE". */
  function alignGridToPlayhead() {
    const beat = 60 / Math.max(1, bpm);
    const bar = beat * BEATS_PER_BAR;
    gridOffset = ((playhead % bar) + bar) % bar;
  }

  const loopsSpan = (start: number, end: number) =>
    !!loop && Math.abs(loop.start - start) < 1e-6 && Math.abs(loop.end - end) < 1e-6;

  /**
   * What the loop chip says it is looping, on both clocks.
   *
   * 🔴 Attributed through {@link runUpAt} FIRST. A loop opened on a render start sits one instant
   * before that shot's delivered block, so plain delivered attribution would name its PREDECESSOR —
   * and then report `00:13.458` for a span that is, on the clock the author is writing against,
   * exactly `00:00.000`. The run-up reading is the only one under which the loop begins where the
   * shot's own clock begins, which is the whole reason the per-shot loop reaches back.
   */
  let loopNote = $derived.by(() => {
    if (!loop) return '';
    const span = `Looping song ${h3Timecode(loop.start)}–${h3Timecode(loop.end)}`;
    const run = runUpAt(songPlan, loop.start);
    const at = shotClockAt(songPlan, generations, loop.start);
    const shot = run ? run.shot : at.shot;
    const local = run ? run.localSeconds : at.localSeconds;
    const tail = 'Click to clear, or drag the ruler again.';
    if (shot === null) return `${span}. ${tail}`;
    return (
      `${span} — it opens at ${h3Timecode(local)} on ${vocab.renderUnit} ${shot}'s own clock` +
      `${run ? ', inside the run-up it renders and then drops' : ''}. ${tail}`
    );
  });

  $effect(() => {
    const a = audioEl;
    if (!a) return;
    if (playing) {
      // Autoplay policy: a play() the browser refuses must put the button back, not leave it
      // showing a pause icon over silence.
      void a.play().catch(() => (playing = false));
      // The shot's own clip rides along, but only where there IS one — `shotAudioEl` holds
      // whatever the last shot with audio put in it, so playing it unconditionally would replay
      // the previous shot's voice line over a shot that has none.
      //
      // UNTRACKED: this effect is the transport, and it must answer to `playing` alone. Tracking
      // the shot id would re-run the whole body at every block boundary — racing the effect below,
      // which is the one that owns entering and leaving a shot.
      untrack(() => {
        if (shotAudioEl && shotAudioId) void shotAudioEl.play().catch(() => {});
      });
    } else {
      a.pause();
      shotAudioEl?.pause();
    }
  });

  $effect(() => {
    if (!playing) return;
    let live = true;
    let frame = 0;
    const tick = () => {
      if (!live) return;
      // Not while the playhead is being dragged: the drag is the authority then, and reading the
      // element back mid-scrub makes the line stutter between the two.
      if (audioEl && drag?.kind !== 'seek') {
        // Wrap BEFORE reading, so the playhead never draws past the out point on the way round.
        if (loop && audioEl.currentTime >= loop.end) audioEl.currentTime = loop.start;
        playhead = audioEl.currentTime;
        keepPlayheadVisible();
      }
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => {
      live = false;
      cancelAnimationFrame(frame);
    };
  });

  /** The block the playhead is inside, or null when it is past the end of the cut. */
  let playheadClipId = $derived(
    layout.clips.find((l) => playhead >= l.start && playhead < l.start + l.width)?.id ?? null
  );

  /**
   * While the song runs, the block under the playhead becomes the selection — so the prompt pane
   * for a moment is already open when the moment is heard, which is the whole point of listening.
   *
   * 🔴 Only while PLAYING, and the selection is read untracked. Following a paused playhead would
   * fight every click, and tracking the read would make this effect re-run on a manual selection
   * and snap it straight back — so a shot clicked mid-playback keeps the pane until the playhead
   * crosses into the next block, which is when following it is what was wanted again.
   */
  $effect(() => {
    if (!playing) return;
    const id = playheadClipId;
    if (!id) return;
    // ⚠️ Never while the author is typing. Swapping the selection swaps what the prompt pane is
    // editing, so following the playhead through a block boundary mid-sentence would move the caret
    // out from under them and into another shot's text — the one cost that would make listening
    // while writing worse than not listening at all.
    const el = document.activeElement as HTMLElement | null;
    if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable)) return;
    untrack(() => {
      if (selectedIds.length !== 1 || selectedIds[0] !== id) selectedIds = [id];
    });
  });

  // ── Per-shot audio ──────────────────────────────────────────────────────────
  //
  // A shot can carry its own clip, which the engine COMPOSITES over the project track for that
  // shot's span — so the preview layers it too rather than swapping the song out. Playing only the
  // song would let a voice line the render will contain go unheard until the render exists.
  let shotAudioEl: HTMLAudioElement | undefined = $state();
  let shotAudio = $derived.by(() => {
    if (!playheadClipId) return null;
    const c = clips.find((x) => x.id === playheadClipId);
    const l = layout.byId.get(playheadClipId);
    if (!c?.audio_b64 || !l) return null;
    return { id: c.id, src: assetSrc(c.audio_b64, 'audio/mpeg'), start: l.start };
  });
  /** Keyed on the ID so the effect below fires on ENTERING a shot, not on every animation frame. */
  let shotAudioId = $derived(shotAudio?.id ?? null);

  /**
   * Put the shot's own clip where the playhead is inside its block.
   *
   * 🔴 Refuses when the element is still holding the PREVIOUS shot's clip. `shotAudio` is a
   * `$derived` and updates the instant `playhead` moves, but the `src` attribute it feeds is
   * written on the next DOM flush — so a seek that crossed into a different shot would otherwise
   * scrub the outgoing clip to an offset in the incoming one's timeline, and where that element is
   * still sounding you hear it. The effect below re-runs after the flush and places it properly.
   */
  function placeShotAudio() {
    const a = shotAudioEl;
    if (!a) return;
    if (!shotAudio) {
      a.pause();
      return;
    }
    if (a.src !== shotAudio.src) return;
    a.currentTime = Math.max(0, playhead - shotAudio.start);
    if (playing) void a.play().catch(() => {});
  }

  $effect(() => {
    // Entering or leaving a shot that carries its own audio. Keyed on the id, so it fires at the
    // block boundary rather than on every animation frame.
    void shotAudioId;
    void shotAudioEl;
    untrack(placeShotAudio);
  });

  /** Move both clocks at once. Every seek gesture goes through here. */
  function seekTo(sec: number) {
    playhead = Math.max(0, sec);
    if (audioEl && Number.isFinite(audioEl.duration)) {
      audioEl.currentTime = Math.min(playhead, audioEl.duration);
    }
    // The shot's own clip is positioned RELATIVE to its block, so a seek has to re-place it — it
    // does not share the song's clock and nothing else would resync it within a block.
    placeShotAudio();
  }

  function togglePlay() {
    if (!canPlay) return;
    if (!playing) {
      // A loop marked while the playhead sits outside it means "play THAT" — starting where the
      // line happens to be would play up to the out point once and then jump, which reads as a bug.
      if (loop && (playhead < loop.start || playhead >= loop.end)) seekTo(loop.start);
      // Restarting from the top at the end is what a transport does; leaving it stuck at the tail
      // means the button appears to do nothing.
      else if (audioEl?.ended) seekTo(0);
    }
    playing = !playing;
  }

  // ── Scrub audio ─────────────────────────────────────────────────────────────
  //
  // Dragging the playhead plays short grains of the song at the cursor, the way an NLE does —
  // finding a downbeat by eye on a waveform is guesswork, and by ear it is immediate.
  //
  // 🔴 A plain AudioContext over the samples the page ALREADY decoded, never a MediaElementSource
  // on the `<audio>` above: `createMediaElementSource` captures that element permanently for the
  // life of the page (`web-audio-mediaelementsource-permanent`), which would put the transport
  // behind a graph it can never get back out of.
  //
  // 🔴 One small buffer PER GRAIN rather than one AudioBuffer for the song. A three-minute track at
  // 48 kHz is ~40 MB of float32, and `waveform` is already holding that once — copying it whole to
  // scrub would double a cost the editor pays on every project with a song, for a gesture most
  // sessions never make. A grain is ~19 KB and is collected as soon as it has sounded.
  const GRAIN_MS = 110;
  let scrubCtx: AudioContext | null = null;
  let lastGrainAt = 0;

  function scrubGrain(atSeconds: number) {
    // Silent while playing: the song is already sounding, and a grain on top of it is just a flam.
    if (playing || !waveform || waveform.length === 0 || waveDuration <= 0) return;
    const now = performance.now();
    // Overlap the grains slightly rather than gapping them — at 0.7 the tail of one is still
    // sounding as the next starts, which is what makes a drag sound continuous instead of stuttered.
    if (now - lastGrainAt < GRAIN_MS * 0.7) return;
    lastGrainAt = now;
    // Recovered rather than passed: `waveDuration` is defined as `samples.length / sampleRate`, so
    // this is that division inverted and is exact for any rate the decoder produced.
    const rate = waveform.length / waveDuration;
    const n = Math.max(1, Math.round((rate * GRAIN_MS) / 1000));
    const from = Math.max(0, Math.min(waveform.length - n, Math.round(atSeconds * rate)));
    if (n <= 1) return;
    try {
      // Created on the first scrub, inside the pointer gesture that asked for it — so it is never
      // built for a session that does not scrub, and it is never blocked by the autoplay policy.
      scrubCtx ??= new AudioContext();
      void scrubCtx.resume();
      const buf = scrubCtx.createBuffer(1, n, rate);
      const d = buf.getChannelData(0);
      d.set(waveform.subarray(from, from + n));
      // Ramp both edges. A grain cut at a non-zero sample starts and ends on a step, and a step is
      // a click — one per grain, sixty times a second, is all you would hear.
      const ramp = Math.min(64, Math.floor(n / 4));
      for (let i = 0; i < ramp; i++) {
        const g = i / ramp;
        d[i] *= g;
        d[n - 1 - i] *= g;
      }
      const src = scrubCtx.createBufferSource();
      src.buffer = buf;
      src.connect(scrubCtx.destination);
      src.start();
    } catch {
      /* no audio device, or a context the browser refused — scrubbing silently is still scrubbing */
    }
  }

  $effect(() => () => void scrubCtx?.close());

  /**
   * Is the mark the prompt's cursor is resting on inside a trimmed head?
   *
   * The caller hands it over already converted to song time, so this is the same question
   * {@link runUpAt} answers for every other readout: does this instant fall in the stretch some
   * render covers and delivery discards. A mark that does is a cue nobody will ever see.
   */
  let markTrimmed = $derived(mark ? runUpAt(songPlan, mark.seconds) !== null : false);

  /** Scroll the lane so the playhead stays on screen while the song runs. */
  function keepPlayheadVisible() {
    if (!laneEl) return;
    const x = playhead * pxPerSec;
    const left = laneEl.scrollLeft;
    const w = laneEl.clientWidth;
    // A margin rather than a hard edge, so the line does not ride the border of the viewport.
    if (x < left + 40 || x > left + w - 40) laneEl.scrollLeft = Math.max(0, x - w / 2);
  }

  /** The paste-ready mark for wherever the playhead is — the reason to listen at all. */
  let playAt = $derived(shotClockAt(songPlan, generations, playhead));
  let playMark = $derived(markAt(playAt) ?? h3Timecode(playAt.songSeconds));
  let copied = $state(false);
  async function copyMark() {
    try {
      await navigator.clipboard.writeText(playMark);
      copied = true;
      setTimeout(() => (copied = false), 1200);
    } catch {
      /* no clipboard permission — the text is on screen to read either way */
    }
  }

  /**
   * Put the playhead exactly on a moment and put its mark on the clipboard.
   *
   * 🔴 The owner's actual workflow, in one gesture: find the lyric the robot should sing on, and
   * write the cue for it. Doing it by hand means reading a song time off the axis and subtracting
   * the shot's offset AND its run-up — which is the arithmetic that put `At 00:22.000` into a
   * 14 s render, and the arithmetic that put a cue at 0.592 s inside a 0.917 s trimmed head.
   *
   * `playMark` is a `$derived` off `playhead`, and runes are pull-based, so it is already the new
   * moment's mark by the time this reads it.
   */
  async function seekAndCopyMark(sec: number) {
    seekTo(sec);
    await copyMark();
  }

  // ── Overview strip ──────────────────────────────────────────────────────────
  //
  // The whole song at a glance, with a box round the part the lanes are showing. At 320 px/s the
  // main lane holds about four seconds — enough to place a beat and nowhere near enough to know
  // which chorus you are in. This is the only thing on screen that answers "where am I".
  //
  // Its own width, not the content's: it never scrolls, so it maps the ENTIRE axis onto whatever
  // the timeline is wide, and that ratio is the whole point.
  let overviewEl: HTMLCanvasElement | undefined = $state();
  let overviewW = $state(0);
  let scrollX = $state(0);
  let showOverview = $derived(audioLane && totalSeconds > 0 && contentWidth > laneW + 8);

  let viewBox = $derived.by(() => {
    if (!showOverview || totalSeconds <= 0 || overviewW <= 0) return null;
    const perSec = overviewW / totalSeconds;
    return {
      left: (scrollX / pxPerSec) * perSec,
      width: Math.min(overviewW, (laneW / pxPerSec) * perSec),
      playhead: playhead * perSec
    };
  });

  /** Jump the lanes so `fraction` of the song sits in the middle of the viewport. */
  function overviewJump(e: PointerEvent) {
    if (!overviewEl || !laneEl) return;
    const rect = overviewEl.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width));
    laneEl.scrollLeft = Math.max(0, frac * totalSeconds * pxPerSec - laneEl.clientWidth / 2);
  }

  $effect(() => {
    void waveform;
    void overviewW;
    void totalSeconds;
    drawOverview();
  });

  function drawOverview() {
    const canvas = overviewEl;
    if (!canvas || overviewW <= 0) return;
    const dpr = window.devicePixelRatio || 1;
    const h = OVERVIEW_H;
    canvas.width = Math.floor(overviewW * dpr);
    canvas.height = Math.floor(h * dpr);
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, overviewW, h);
    if (!waveform || waveform.length === 0 || waveDuration <= 0) return;
    ctx.fillStyle = getComputedStyle(canvas).getPropertyValue('--accent-2').trim() || '#2d6b67';
    const mid = h / 2;
    // The song may be shorter than the cut, so the wave occupies its own share of the strip rather
    // than being stretched to fill it — otherwise the box and the picture disagree.
    const waveW = (waveDuration / totalSeconds) * overviewW;
    const peaks = waveformPeaks(waveform, waveW);
    for (let x = 0; x < peaks.length; x++) {
      const barH = Math.max(1, peaks[x] * (h - 2));
      ctx.fillRect(x, mid - barH / 2, 1, barH);
    }
  }

  // ── Waveform canvas ─────────────────────────────────────────────────────────
  let waveCanvas: HTMLCanvasElement | undefined = $state();
  $effect(() => {
    // Redraw when samples / zoom change.
    void waveform;
    void pxPerSec;
    void contentWidth;
    drawWave();
  });
  function drawWave() {
    const canvas = waveCanvas;
    if (!canvas) return;
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(1, Math.floor(contentWidth));
    const h = LANE_H;
    canvas.width = Math.floor(w * dpr);
    canvas.height = Math.floor(h * dpr);
    canvas.style.width = `${w}px`;
    canvas.style.height = `${h}px`;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, w, h);
    if (!waveform || waveform.length === 0 || waveDuration <= 0) return;
    const color = getComputedStyle(canvas).getPropertyValue('--accent-2').trim() || '#2d6b67';
    ctx.fillStyle = color;
    const mid = h / 2;
    const peaks = waveformPeaks(waveform, waveDuration * pxPerSec);
    for (let x = 0; x < peaks.length; x++) {
      const barH = Math.max(1, peaks[x] * (h - 4));
      ctx.fillRect(x, mid - barH / 2, 1, barH);
    }
  }
</script>

<div class="timeline">
  <!-- ── The head strip: transport on the left, time readout on the right ─────────────────────
       In FLOW, not floating over the ruler. Both of these are read while the cursor is somewhere
       else on the axis, so anything that covers the ticks — or that appears and shifts the lanes
       under the pointer — is the one thing they cannot afford to do. The row is always present for
       the same reason, even when it is empty.

       Transport and mark sit together deliberately: hearing the song is how a beat gets found, and
       the mark is what the prompt takes for it on the render unit's own clock. Reading a moment off
       the waveform and converting it by hand is exactly the arithmetic that put `At 00:22.000` into
       a 14 s render. -->
  <div class="tl-head">
    {#if canPlay}
      <button
        class="tp-play"
        onclick={togglePlay}
        title={playing ? 'Pause (space)' : 'Play the song against the cut (space)'}
        aria-label={playing ? 'Pause' : 'Play'}>{playing ? '❚❚' : '▶'}</button
      >
      <button
        class="tp-mark"
        class:copied
        onclick={copyMark}
        title="Copy this moment as a prompt mark, on the {vocab.renderUnit}'s own clock"
        >{copied ? 'copied' : playMark}</button
      >
      <audio
        bind:this={audioEl}
        src={audioSrc}
        preload="auto"
        onended={() => (playing = false)}
        onpause={() => (playing = false)}
        onseeked={() => audioEl && drag?.kind !== 'seek' && (playhead = audioEl.currentTime)}
      ></audio>
      <!-- The selected shot's OWN clip, layered over the song exactly as the engine composites it.
           No `onpause` handler: this one stopping (its clip is shorter than the shot, or the
           playhead left the block) must not stop the transport. -->
      <audio bind:this={shotAudioEl} src={shotAudio?.src || undefined} preload="auto"></audio>
    {/if}
    {#if audioLane}
      <button
        class="tp-grid"
        class:on={gridOn}
        onclick={() => (gridOn = !gridOn)}
        title="Bar lines at a tempo you set. Cuts snap to the downbeats."
        aria-pressed={gridOn}>♩</button
      >
    {/if}
    {#if gridOn}
      <span class="grid-fields">
        <input
          class="gnum"
          type="number"
          min="30"
          max="300"
          step="0.5"
          bind:value={bpm}
          title="Beats per minute"
        />
        <button onclick={alignGridToPlayhead} title="Put the nearest downbeat on the playhead"
          >set “1”</button
        >
      </span>
    {/if}
    {#if loop}
      <button class="tp-loop" onclick={() => (loop = null)} title={loopNote}
        >⟲ {h3Timecode(loop.start)}–{h3Timecode(loop.end)} ✕</button
      >
    {/if}
    {#if hoverAt}
      <div class="hover-readout" class:past={hoverAt.past} class:onset={!!hoverLyric}>
        {#if hoverLyric}
          <!-- Named, because this is an EXACT onset rather than wherever the cursor happens to be
               — and the two look identical once they are just a timecode. -->
          <span class="hr-word">“{hoverLyric.text}”</span>
        {/if}
        <span class="hr-song"><b>song</b> {h3Timecode(hoverAt.songSeconds)}</span>
        {#if hoverAt.shot !== null}
          <span class="hr-sep">·</span>
          <!-- The number that gets TYPED. It already carries the run-up — `shotClockAt` measures
               from the render's frame 0, not from the block's left edge — so it is paste-ready as
               spelled. The tag beside it says where the extra 0.9 s came from, in WORDS rather than
               a tooltip: this whole strip is `pointer-events:none` and disappears the moment the
               cursor leaves the lane, so nothing in it can be hovered for an explanation. -->
          <span class="hr-local">
            <b>{unitCap} {hoverAt.shot}</b>
            At {h3Timecode(hoverAt.localSeconds)}
          </span>
          {#if hoverAt.runUpSeconds > 0}
            <span class="hr-runup">incl. {hoverAt.runUpSeconds.toFixed(2)}s run-up</span>
          {/if}
          {#if hoverAt.past}
            <span class="hr-warn">past the end of this {vocab.renderUnit}</span>
          {/if}
        {/if}
        {#if hoverRunUp}
          <!-- The same instant, in the OTHER render that covers it. Not a contradiction: the next
               shot re-renders this stretch to get itself moving into the join, then trims it — so a
               cue written here would be discarded, which is worth saying where somebody is standing
               on it looking for a number.

               No timecode on purpose. Every other number on this strip exists to be typed into a
               prompt, and this is the one that must not be. -->
          <span class="hr-sep">·</span>
          <span class="hr-trim">{unitCap} {hoverRunUp.shot}’s run-up — trimmed, unmarkable</span>
        {/if}
      </div>
    {/if}
  </div>
  <!-- Only while the lanes actually overflow: with the whole cut already on screen, a second
       picture of the same thing is a strip that says nothing. -->
  {#if showOverview}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="overview"
      style="height:{OVERVIEW_H}px"
      bind:clientWidth={overviewW}
      onpointerdown={(e) => {
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
        overviewJump(e);
      }}
      onpointermove={(e) => e.buttons === 1 && overviewJump(e)}
    >
      <canvas bind:this={overviewEl} style="width:{overviewW}px; height:{OVERVIEW_H}px"></canvas>
      {#if viewBox}
        <div class="ov-view" style="left:{viewBox.left}px; width:{viewBox.width}px"></div>
        <div class="ov-head" style="left:{viewBox.playhead}px"></div>
      {/if}
    </div>
  {/if}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="lanes"
    bind:this={laneEl}
    onpointermove={onMove}
    onpointerup={endDrag}
    onpointercancel={endDrag}
    onpointerleave={() => (hoverSec = null)}
    onscroll={(e) => (scrollX = e.currentTarget.scrollLeft)}
    onwheel={onWheel}
  >
    <div class="content" style="width:{contentWidth}px">
      <!-- Ruler lives inside the scroll content so its ticks share the clips' x-origin
           (a 0s tick sits exactly at clip 0's left edge) and scroll together. Dragging it marks a
           loop: it is the one strip that is purely time, which is what a loop is. -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="ruler"
        style="width:{contentWidth}px"
        onpointerdown={startRegion}
        title="Drag to loop a span · click to clear"
      >
        {#each ticks.minors as t (t)}
          <div class="tick minor" style="left:{t * pxPerSec}px"></div>
        {/each}
        {#each ticks.major as t (t)}
          <div class="tick" style="left:{t * pxPerSec}px">
            <span>{tickLabel(t, ticks.step)}</span>
          </div>
        {/each}
      </div>
      <!-- Bars and beats. Under the lanes: this is the grid the cut is built ON, so it belongs
           behind the blocks rather than ruled across their faces. -->
      {#each grid.beats as t (t)}
        <div class="beat-line" style="left:{t * pxPerSec}px"></div>
      {/each}
      {#each grid.bars as t (t)}
        <div class="bar-line" style="left:{t * pxPerSec}px"></div>
      {/each}
      <!-- The loop span: a translucent wash ABOVE the lanes and below the playhead. It has to cross
           the clip blocks, and the blocks are opaque — drawn underneath, the span would be visible
           only in the gaps between shots, which is exactly where it says nothing. -->
      {#if loop}
        <div
          class="loop-band"
          style="left:{loop.start * pxPerSec}px; width:{Math.max(
            1,
            (loop.end - loop.start) * pxPerSec
          )}px"
        ></div>
      {/if}
      <!-- Snap guide (while dragging) -->
      {#if snapLineSec !== null}
        <div class="snap-guide" style="left:{snapLineSec * pxPerSec}px"></div>
      {/if}
      <!-- Where the hover readout is measuring. Drawn under the playhead so it never hides it. -->
      {#if hoverSec !== null && !drag}
        <div class="hover-guide" style="left:{hoverSec * pxPerSec}px"></div>
      {/if}
      <!-- ── Where the prompt's cursor is pointing ────────────────────────────────────────
           Above the playhead deliberately: it is transient and it is the thing being LOOKED FOR, so
           it must not be the line that gets hidden. It is drawn from the caller's song-time
           conversion, and the chip repeats the prompt's own spelling of the mark so the two can be
           read against each other without arithmetic. -->
      {#if mark}
        <!-- 🔴 A mark can legitimately land LEFT of its own block. `At 00:00.400` on a shot with a
             0.917 s run-up is inside the head the render throws away, so it draws over the previous
             shot — which is exactly where the model puts it, and exactly why nobody ever sees it.
             Drawn where it really lands, in the danger colour, rather than clamped to the block and
             made to look like a cue that works. -->
        <div
          class="mark-guide"
          class:trimmed={markTrimmed}
          style="left:{mark.seconds * pxPerSec}px"
        >
          <span class="mark-tag">{mark.label}{markTrimmed ? ' · trimmed' : ''}</span>
        </div>
      {/if}
      <!-- Reorder drop marker -->
      {#if dropIndex !== null}
        <div class="drop-marker" style="left:{(offsets[dropIndex] ?? clipsEnd) * pxPerSec}px"></div>
      {/if}
      <!-- Playhead. Two numbers, because the line is read for two different reasons: where you are
           in the song, and what to write in a prompt for the moment you are standing on. The second
           is the shot's own clock — run-up included — and it is the one the ruler underneath can
           never show.

           No `title` on either: the whole playhead is `pointer-events:none`, so a tooltip here
           could never be opened. The transport's mark chip carries the full spelling and copies it. -->
      <div class="playhead" class:live={playing} style="left:{playhead * pxPerSec}px">
        <span class="playhead-time"
          >{h3Timecode(playhead)}{#if playAt.shot !== null}<i class="ph-shot"
              >{unitCap.slice(0, 1)}{playAt.shot} {h3Timecode(playAt.localSeconds)}</i
            >{/if}</span
        >
      </div>

      <!-- Video lane -->
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div class="lane clips-lane" style="height:{LANE_H}px" onpointerdown={startSeek}>
        {#each clips as c, i (c.id)}
          {@const left = offsets[i] * pxPerSec}
          {@const width = Math.max(24, widths[i] * pxPerSec)}
          {@const status = shotStatus ? shotStatus(c) : null}
          {@const pstatus = proxyStatus ? proxyStatus(c) : null}
          {@const flag = findingSeverity ? findingSeverity(c.id) : null}
          {@const dfail = draftFailed ? draftFailed(c.id) : false}
          {@const meta = shotMeta.get(c.id)}
          <div
            class="clip"
            class:selected={selectedSet.has(c.id)}
            class:dragging={drag?.kind === 'reorder' && drag.id === c.id}
            class:none={status === 'none'}
            class:flagged={!!flag}
            class:draft-failed={dfail}
            class:over={meta?.over}
            style="left:{left}px; width:{width}px"
            onpointerdown={(e) => startReorder(e, c, i)}
            onclick={(e) => selectClip(e, c)}
            onkeydown={(e) => {
              // Enter only. Clicking a block focuses it, and SPACE is the transport everywhere on
              // this page — a block that swallowed it would make the play key dead exactly after
              // the gesture that most often precedes wanting to hear the shot.
              if (e.key === 'Enter') selectClip(e, c);
            }}
            role="button"
            tabindex="0"
          >
            {#if c.image_b64 || c.snapshot_b64}
              <img class="film" src={assetSrc(c.image_b64 || c.snapshot_b64)} alt="" />
            {:else}
              <div class="film film-{(i % 6) + 1}"></div>
            {/if}
            <div class="veil"></div>
            <!-- ── The run-up ──────────────────────────────────────────────────────────────────
                 🔴 Drawn on the PREVIOUS block, because that is the footage it covers. The next
                 shot's render opens `runUp` seconds EARLY, on top of this shot's tail, re-renders it
                 and throws the result away. It is an OVERLAP, not a gap — nothing is missing from
                 the delivered cut, nothing shifts, and the lane needs no hole cut into it.

                 A child of the block rather than a free band over the lanes, for two reasons: a
                 free band would have to eat pointer events to carry a tooltip, which would take the
                 last second of every shot away from reorder and trim; and drawn here it paints
                 above the film and under the labels, which is exactly the reading order — a hatch
                 over the picture, with the shot's own text still on top of it.

                 Immediately after `.veil` on purpose: paint order is what layers it, not z-index. -->
            {#if runUps[i + 1] > 0}
              <span
                class="runup-band"
                style="width:{Math.min(width, runUps[i + 1] * pxPerSec)}px"
                title={`${unitCap} ${i + 2}'s render starts here, ${runUps[i + 1].toFixed(2)}s before ` +
                  `its own block. It is handed this ${vocab.renderUnit}'s last ${H3_TRIM_JOIN} ` +
                  `frames as keyframes and re-renders them — that is how it gets itself moving into ` +
                  `the join — and delivery then trims them, because this ${vocab.renderUnit} has ` +
                  `already played them. So this stretch is rendered twice and delivered once: an ` +
                  `overlap, not a gap. Nothing in the cut moves.\n\n` +
                  `What it means for writing: ${unitCap} ${i + 2}'s own clock reads 00:00.000 HERE, ` +
                  `so every time in its prompt is ${runUps[i + 1].toFixed(2)}s earlier than the ` +
                  `block makes it look — and a mark under ${runUps[i + 1].toFixed(2)}s lands in ` +
                  `the discarded head and never appears.`}
              ></span>
            {/if}
            <span class="sn">{String(i + 1).padStart(2, '0')}</span>
            {#if meta && meta.first && i >= 1}
              <!-- Every block here is its own render, so the gap in front of it is a seam by
                   definition and there is nothing left to say about WHERE it is. What is still worth
                   a glyph is what crosses it, which is the continuation the composer sets. -->
              <span
                class="clip-cut"
                class:cut={meta.join === 'none'}
                title={meta.join === 'none'
                  ? 'Hard cut — nothing is carried across the seam from the previous shot'
                  : `Continues the previous ${vocab.renderUnit}, joined by ${meta.join}.` +
                    (runUps[i] > 0
                      ? ` It renders ${runUps[i].toFixed(2)}s of run-up first — the previous ` +
                        `${vocab.renderUnit}'s last ${H3_TRIM_JOIN} frames, re-rendered as its own ` +
                        `head and then trimmed. Its prompt's 00:00.000 is that much before this ` +
                        `block starts.`
                      : '')}>{meta.join === 'none' ? '⟂' : '→'}</span
              >
            {:else if c.mode === 't2v' && i >= 1}
              <span
                class="clip-cut"
                class:cut={c.new_scene === true}
                title={c.new_scene === true
                  ? 'New scene — a hard cut, rendered fresh from the prompt'
                  : 'Continues the previous shot'}>{c.new_scene === true ? '⟂' : '→'}</span
              >
            {/if}
            {#if status}<span class="rs {status}" title="Final render: {status}"></span>{/if}
            {#if pstatus && pstatus !== 'none'}<span
                class="rs proxy {pstatus}"
                title="Proxy preview: {pstatus}"
              ></span>{/if}
            {#if c.audio_b64}
              <span
                class="clip-aud"
                title={c.audio_kind === 'bgm'
                  ? 'This shot has its own audio (sound only — no lip-sync)'
                  : 'This shot has its own audio (voice — drives lip-sync)'}
                >{c.audio_kind === 'bgm' ? '♪' : '🗣'}</span
              >
            {/if}
            <!-- Prompt Relay beats: draggable ticks on the clip body, so the timing of the beats
                 INSIDE a shot is edited where the shot itself is. -->
            {#each c.beats ?? [] as beat, bi (bi)}
              <button
                class="beat-tick"
                class:dragging={drag?.kind === 'beat' && drag.clipId === c.id && drag.index === bi}
                style="left:{(Math.min(beat.seconds, c.seconds) /
                  Math.max(MIN_CLIP_SECONDS, widths[i])) *
                  100}%"
                onpointerdown={(e) => startBeatDrag(e, c.id, bi)}
                title={`beat @ ${beat.seconds.toFixed(2)}s${beat.label ? ` (on “${beat.label}”)` : ''}: ${beat.text || '—'}`}
              ></button>
            {/each}
            {#if (c.beats ?? []).length}
              <span class="clip-relay" title="Prompt relay: timed beats in this shot"
                >⟡{(c.beats ?? []).length}</span
              >
            {/if}
            <!-- The `[Shot N]` marks in this shot's own block. Read-only, and that is the point: a
                 beat is a sentence with a time in it, so it is written where the sentence is and the
                 timeline only says when it lands. A hard rule is a cut, a hairline is a moment the
                 camera rolls through. -->
            {#each h3Beats.get(c.id) ?? [] as b, bi (bi)}
              <!-- Clamped at 0 rather than allowed negative: `.clip` is `overflow:hidden`, so a
                   trimmed mark drawn where it really lands would be invisible — which is the failure
                   mode, not the fix. It sits on the block's head in the danger colour instead, and
                   its title says the frames it lands on are thrown away. -->
              <span
                class="beat-mark"
                class:cut={b.cut}
                class:trimmed={b.trimmed}
                style="left:{(Math.min(Math.max(b.at, 0), widths[i]) /
                  Math.max(MIN_CLIP_SECONDS, widths[i])) *
                  100}%"
                title={b.label}
              ></span>
            {/each}
            {#if flag}<span class="flag">!</span>{/if}
            {#if dfail}<span
                class="dfail"
                title="Auto-draft failed on this {vocab.renderUnit} — it has no prompt">✕</span
              >{/if}
            <!-- 🔴 The badge shows what the BLOCK is, which is the delivered length — it used to
                 show the rendered one, and with a 22-frame run-up that is a number 0.917 s longer
                 than the thing it is printed on. The full breakdown is in the title, because all
                 three numbers are real and the author needs different ones at different moments. -->
            {#if meta && Math.abs(widths[i] - c.seconds) > 0.005}
              <span
                class="clip-snap"
                title={`Authored ${c.seconds.toFixed(2)}s · renders ${meta.rendered.toFixed(2)}s ` +
                  `(the engine's frame grid only rounds up)` +
                  (runUps[i] > 0
                    ? ` · of which the first ${runUps[i].toFixed(2)}s is the run-up it re-renders ` +
                      `from the previous ${vocab.renderUnit} and delivery drops` +
                      ` · plays ${widths[i].toFixed(2)}s in the cut`
                    : ` · plays ${widths[i].toFixed(2)}s in the cut`)}>{widths[i].toFixed(2)}s</span
              >
            {/if}
            {#if canPlay}
              <!-- 🔴 The loop covers the RENDER window, run-up included — not the block.
                   Looping a shot is what you do while writing its prompt, and the prompt's clock
                   starts at the render, so this way the loop's first instant IS `At 00:00.000` and
                   every mark can be counted from something you can hear. It is also the only span
                   that plays what the model is actually shown. Identical to the block wherever the
                   run-up is 0 (the opening shot, a loose seam, all of LTX). -->
              <button
                class="clip-loop"
                class:on={loopsSpan(renderStarts[i], offsets[i] + widths[i])}
                title={runUps[i] > 0
                  ? `Loop everything this ${vocab.renderUnit} renders — including the ` +
                    `${runUps[i].toFixed(2)}s run-up before its block, which is where its own ` +
                    `00:00.000 is. Press ▶ or space to hear it round.`
                  : `Loop this ${vocab.renderUnit} while you write it — press ▶ or space to hear it round`}
                onpointerdown={(e) => e.stopPropagation()}
                onclick={(e) => {
                  e.stopPropagation();
                  loopSpan(renderStarts[i], offsets[i] + widths[i]);
                }}>⟲</button
              >
            {/if}
            {#if meta && onopengeneration}
              <button
                class="clip-open"
                title="Open this {vocab.renderUnit}'s prompt"
                onpointerdown={(e) => e.stopPropagation()}
                onclick={(e) => {
                  e.stopPropagation();
                  onopengeneration(meta.gi);
                }}>✎</button
              >
            {/if}
            <span class="lbl" title={meta?.gist || c.prompt || ''}>{meta?.gist || shotGist(c)}</span
            >
            <div
              class="trim-handle"
              onpointerdown={(e) => startTrim(e, c)}
              role="button"
              tabindex="-1"
              aria-label="Trim clip"
            ></div>
          </div>
        {/each}
        {#if onContinueScene && clips.length > 0}
          <button
            class="clip-add"
            style="left:{clipsEnd * pxPerSec + 6}px"
            onpointerdown={(e) => e.stopPropagation()}
            onclick={onContinueScene}
            title="LLM drafts the next {vocab.renderUnit} from the story so far — no render needed"
          >
            <span class="plus">＋</span>Continue<br />scene
          </button>
        {/if}
        {#if clips.length === 0}
          <div class="lane-empty">No clips yet — add one to start directing.</div>
        {/if}
      </div>

      <!-- Keyframe lane -->
      <div class="lane pin-lane" style="height:{PIN_LANE_H}px">
        {#each clips as c, i (c.id)}
          {#each c.keyframes as kf, ki (ki)}
            <!-- Placed 1:1 from the block's start, not as a fraction of its drawn width: a pin is
                 a FRAME INDEX into the shot, and the shot's frames run second-for-second from its
                 head. Only the round-up at the tail is extra. -->
            {@const abs = (offsets[i] + Math.min(kf.seconds, c.seconds)) * pxPerSec}
            <div
              class="pin"
              class:selected={selectedPin?.clipId === c.id && selectedPin?.index === ki}
              style="left:{abs}px"
              onpointerdown={(e) => startPinDrag(e, c.id, ki)}
              title={kf.label || `pin @ ${kf.seconds.toFixed(2)}s`}
              role="button"
              tabindex="0"
            >
              {#if kf.image_b64}
                <img src={assetSrc(kf.image_b64)} alt="" />
              {:else}
                <span class="pin-dot"></span>
              {/if}
            </div>
          {/each}
        {/each}
      </div>

      <!-- Audio + lyric lane. Drawn on both engines: it is the authoring spine cuts snap to, not
           a picture of anything the engine was sent. -->
      {#if audioLane}
        <div class="lane audio-lane" style="height:{LANE_H}px">
          <canvas bind:this={waveCanvas} class="wave"></canvas>
          <!-- Two rows: the phrase reads, the words are what you aim at.

               🔴 Both label themselves on the SHOT's clock, not the song's. "Find the lyric the
               robot should sing on, read its number, type it into that shot's prompt" is the
               workflow this lane exists for, and the song time is the one number that cannot be
               typed anywhere — it is a whole timeline's worth of offset plus a run-up away from the
               mark that lands where it points. -->
          <div class="lyrics">
            {#each phrases as p (p.start)}
              {@const w = Math.max(6, (p.end - p.start) * pxPerSec)}
              {@const label = fitPhraseLabel(p.text, w)}
              {@const endShot = shotClockAt(songPlan, generations, p.end).shot}
              {@const startShot = shotClockAt(songPlan, generations, p.start).shot}
              <button
                class="phrase"
                class:hovered={hoverLyric !== null && p.words.includes(hoverLyric)}
                style="left:{p.start * pxPerSec}px; width:{w}px"
                title={`“${p.text}” opens at ${markNote(p.start)}, and runs to song ` +
                  `${h3Timecode(p.end)}` +
                  (endShot !== null && endShot !== startShot
                    ? ` — which is over the seam, in ${vocab.renderUnit} ${endShot}`
                    : '') +
                  `. Click to put the playhead on the first word.`}
                onclick={() => seekTo(p.start)}
              >
                {label}
              </button>
            {/each}
            <!-- Keyed by INDEX, not by onset: a transcript can legitimately carry two words with
                 the same start (STT emitting an overlap), and a duplicate key is a hard error.

                 A click seeks AND copies. Two gestures used to be needed — seek here, then hit the
                 mark chip — and the whole point of an onset is that it is the one time on this axis
                 worth having exactly, so making the exact answer the one that takes an extra step
                 was backwards. The chip flashes "copied", so nothing happens to the clipboard
                 without saying so.

                 `.seam` is NOT "this word is unusable". Every onset is markable in the render unit
                 whose delivered block plays it. What it means is that the word falls in the stretch
                 the NEXT one re-renders and throws away — so if the intent is "the shot after this
                 seam should sing this line", it cannot be cued there: the mark would land in the
                 trimmed head and be discarded. That is the case that bit for real, so it is drawn. -->
            {#each lyrics as w, i (i)}
              <button
                class="lyric tick"
                class:hovered={hoverLyric === w}
                class:seam={runUpAt(songPlan, w.start) !== null}
                style="left:{w.start * pxPerSec}px"
                title={`“${w.text}” — ${markNote(w.start)}. Click to put the playhead exactly on ` +
                  `it and copy that mark.`}
                onpointerenter={() => (hoverLyric = w)}
                onpointerleave={() => (hoverLyric = null)}
                onclick={() => seekAndCopyMark(w.start)}
                aria-label={w.text}
              ></button>
            {/each}
          </div>
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  .timeline {
    position: relative;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: var(--bg-2);
    overflow: hidden;
    user-select: none;
    /* stay bounded to the parent so the wide .content scrolls inside .lanes instead of
       stretching the whole workspace when a project has shots */
    min-width: 0;
    max-width: 100%;
  }
  .ruler {
    position: relative;
    height: 22px;
    overflow: hidden;
    margin-bottom: 4px;
    border-bottom: 1px solid var(--ring);
    /* The ruler is draggable (it marks the loop span) and the cursor is the only thing that says so. */
    cursor: ew-resize;
    touch-action: none;
  }
  .tick {
    position: absolute;
    top: 0;
    height: 100%;
    border-left: 1px solid var(--ring);
    padding-left: 3px;
  }
  .tick span {
    font-size: 10px;
    color: var(--muted);
  }
  .lanes {
    overflow-x: auto;
    overflow-y: hidden;
    /* A finger anywhere on the lane pans the timeline; the browser wins that gesture and cancels
       our pointer stream. Only the precision handles below opt out with touch-action:none, so
       setPointerCapture and native panning stop fighting over the same drag. */
    touch-action: pan-x;
  }
  .content {
    position: relative;
    padding: 6px 0;
  }
  .tl-head {
    display: flex;
    align-items: center;
    gap: 4px;
    /* Reserved whether or not anything is in it, so the lanes never shift under the pointer when a
       readout appears — the readout exists to be aimed at, and an axis that moves defeats it. */
    min-height: 24px;
    padding: 2px 8px 0;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }
  .tl-head button {
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: var(--bg-1, #14161a);
    color: var(--fg-1);
    font: inherit;
    padding: 1px 8px;
    cursor: pointer;
  }
  .tl-head button:hover {
    border-color: var(--accent);
  }
  .tp-play {
    min-width: 30px;
    color: var(--accent);
  }
  .tp-mark.copied {
    color: var(--success);
  }
  .tp-loop {
    color: var(--accent-2, #2d6b67);
  }
  .tp-grid.on {
    color: var(--accent);
    border-color: var(--accent);
  }
  .grid-fields {
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .gnum {
    width: 54px;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: var(--bg-1, #14161a);
    color: var(--fg-1);
    font: inherit;
    font-variant-numeric: tabular-nums;
    padding: 1px 4px;
  }
  .overview {
    position: relative;
    margin: 0 8px 2px;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: var(--bg-2);
    overflow: hidden;
    cursor: pointer;
    touch-action: none;
  }
  .ov-view {
    position: absolute;
    top: 0;
    bottom: 0;
    min-width: 2px;
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    border-left: 1px solid var(--accent);
    border-right: 1px solid var(--accent);
    pointer-events: none;
  }
  .ov-head {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 1px;
    background: var(--accent);
    pointer-events: none;
  }
  .bar-line,
  .beat-line {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 1px;
    z-index: 0;
    pointer-events: none;
  }
  .bar-line {
    background: color-mix(in srgb, var(--accent) 42%, transparent);
  }
  .beat-line {
    background: color-mix(in srgb, var(--muted) 26%, transparent);
  }
  .hover-readout.onset {
    border-color: var(--accent);
  }
  .hr-word {
    color: var(--accent);
    max-width: 18ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* Stays out of the way until the block is hovered — except while it is ON, which is a state that
     must be visible without hunting for it. */
  .clip-loop {
    position: absolute;
    right: 4px;
    bottom: 4px;
    z-index: 6;
    border: 1px solid var(--ring);
    border-radius: 4px;
    background: var(--bg-1, #14161a);
    color: var(--muted);
    font-size: 9px;
    line-height: 1;
    padding: 2px 3px;
    cursor: pointer;
    opacity: 0;
  }
  .clip:hover .clip-loop,
  .clip-loop.on {
    opacity: 1;
  }
  .clip-loop.on {
    color: var(--accent);
    border-color: var(--accent);
  }
  .loop-band {
    position: absolute;
    top: 0;
    bottom: 0;
    z-index: 1;
    background: color-mix(in srgb, var(--accent) 16%, transparent);
    border-left: 2px solid var(--accent);
    border-right: 2px solid var(--accent);
    pointer-events: none;
  }
  .playhead {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    background: var(--accent);
    z-index: 20;
    pointer-events: none;
  }
  .playhead.live {
    box-shadow: 0 0 6px var(--accent);
  }
  .playhead-time {
    position: absolute;
    top: -1px;
    left: 3px;
    font-size: 9px;
    font-variant-numeric: tabular-nums;
    color: #1a1206;
    background: var(--accent);
    padding: 0 3px;
    border-radius: 0 3px 3px 0;
    white-space: nowrap;
  }
  /* The shot's own clock, on the same chip. Set apart rather than run together, because they are
     two different times and reading them as one 20-character number is worse than either alone. */
  .playhead-time .ph-shot {
    font-style: normal;
    margin-left: 5px;
    padding-left: 5px;
    border-left: 1px solid rgba(26, 18, 6, 0.45);
    opacity: 0.85;
  }
  .snap-guide {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 1px;
    background: var(--success, #6bd39a);
    z-index: 18;
    pointer-events: none;
    opacity: 0.8;
  }
  /* The prompt's cursor, landed on the time axis. Dashed rather than solid so it cannot be mistaken
     for the playhead at a glance, and above it in z so it is never the line that gets covered. */
  .mark-guide {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 0;
    border-left: 2px dashed var(--warning, #e0a458);
    z-index: 21;
    pointer-events: none;
  }
  /* The same mark, landing in a head the render throws away. Danger rather than warning: it is not
     a caveat about a cue, it is a cue that will not exist. */
  .mark-guide.trimmed {
    border-left-color: var(--danger, #e06c6c);
  }
  .mark-guide.trimmed .mark-tag {
    background: var(--danger, #e06c6c);
    color: #fff;
  }
  .mark-tag {
    position: absolute;
    top: 14px;
    left: 4px;
    font-size: 9px;
    font-variant-numeric: tabular-nums;
    color: #1a1206;
    background: var(--warning, #e0a458);
    padding: 0 4px;
    border-radius: 0 3px 3px 0;
    white-space: nowrap;
  }
  .hover-guide {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 1px;
    background: var(--fg-2, #8a8f98);
    z-index: 17;
    pointer-events: none;
    opacity: 0.55;
  }
  .hover-readout {
    /* Right end of the head strip, so it never lands under the cursor it is reporting on. */
    margin-left: auto;
    display: flex;
    align-items: baseline;
    gap: 6px;
    padding: 1px 8px;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: var(--bg-1, #14161a);
    font-size: 11px;
    font-variant-numeric: tabular-nums;
    color: var(--fg-1);
    pointer-events: none;
    white-space: nowrap;
    /* It has three or four parts now and the strip is a fixed-height flex row. Clipping the tail is
       the correct failure at a narrow width: the shot mark is FIRST after the song time, so what
       gets lost is the run-up annotation and not the number anyone is here to read. */
    min-width: 0;
    overflow: hidden;
  }
  .hover-readout b {
    font-weight: 600;
    color: var(--fg-2, #8a8f98);
  }
  .hover-readout .hr-local b {
    color: var(--accent);
  }
  .hover-readout .hr-sep {
    color: var(--fg-3, #5a5f68);
  }
  .hover-readout.past .hr-local {
    color: var(--danger, #e06c6c);
  }
  .hover-readout .hr-warn {
    color: var(--danger, #e06c6c);
  }
  /* Where the extra 0.9 s in the shot time came from. Quiet, because it is an explanation of a
     number rather than a number — the mark beside it is what gets read and typed. */
  .hover-readout .hr-runup {
    color: var(--fg-3, #5a5f68);
    font-size: 10px;
  }
  /* The one thing on this strip that must NOT be pasted anywhere. */
  .hover-readout .hr-trim {
    color: var(--warning, #e0a458);
    font-size: 10px;
  }
  .drop-marker {
    position: absolute;
    top: 6px;
    height: 62px;
    width: 3px;
    margin-left: -1px;
    background: var(--accent);
    border-radius: 2px;
    z-index: 19;
    pointer-events: none;
    box-shadow: 0 0 6px var(--accent);
  }
  .clip.dragging {
    opacity: 0.65;
    z-index: 25;
    cursor: grabbing;
  }
  .lane {
    position: relative;
    margin: 4px 0;
  }
  .clip-snap {
    position: absolute;
    right: 5px;
    bottom: 5px;
    font-family: var(--mono);
    font-size: 9px;
    color: var(--warning);
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.7);
    pointer-events: none;
  }
  .clip-open {
    position: absolute;
    left: 3px;
    top: 50%;
    transform: translateY(-50%);
    width: 15px;
    height: 22px;
    padding: 0;
    border: 1px solid var(--ring);
    border-radius: 4px;
    background: rgba(11, 13, 17, 0.65);
    color: var(--muted);
    font-size: 10px;
    line-height: 1;
    cursor: pointer;
    opacity: 0;
    z-index: 4;
  }
  .clip:hover .clip-open {
    opacity: 1;
  }
  .clip-open:hover {
    color: var(--accent);
    border-color: var(--accent);
  }
  .clips-lane {
    background: repeating-linear-gradient(
      90deg,
      transparent,
      transparent 39px,
      rgba(255, 255, 255, 0.03) 39px,
      rgba(255, 255, 255, 0.03) 40px
    );
  }
  .clip {
    position: absolute;
    top: 4px;
    bottom: 4px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    overflow: hidden;
    cursor: grab;
    transition: border-color 0.12s;
  }
  .clip:active {
    cursor: grabbing;
  }
  .clip:hover {
    border-color: var(--ring-2, var(--accent));
  }
  .clip.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .clip .film {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .film-1 {
    background: linear-gradient(150deg, #3a2740, #0e1117);
  }
  .film-2 {
    background: linear-gradient(150deg, #5c3a2a, #120d0b);
  }
  .film-3 {
    background: linear-gradient(150deg, #233e3b, #0b1110);
  }
  .film-4 {
    background: linear-gradient(150deg, #2a3350, #0c0f16);
  }
  .film-5 {
    background: linear-gradient(150deg, #4a2f38, #100b0d);
  }
  .film-6 {
    background: linear-gradient(150deg, #1f3346, #0a0e12);
  }
  .clip .veil {
    position: absolute;
    inset: 0;
    background: linear-gradient(180deg, transparent 40%, rgba(11, 13, 17, 0.85));
  }
  /* 🔴 The one span on the timeline that is rendered twice and delivered once. Hatched rather than
     tinted: a flat wash reads as a selection or a highlight, and diagonal rule reads as "this is not
     ordinary footage" in every NLE anyone has used. Pinned to the RIGHT edge because it always ends
     exactly where the next block starts — that boundary is the seam. */
  .runup-band {
    position: absolute;
    top: 0;
    bottom: 0;
    right: 0;
    background-image: repeating-linear-gradient(
      -45deg,
      rgba(11, 13, 17, 0.55) 0 4px,
      rgba(224, 164, 88, 0.28) 4px 8px
    );
    border-left: 1px dashed var(--warning, #e0a458);
  }
  .clip .sn {
    position: absolute;
    left: 6px;
    top: 5px;
    font-family: var(--mono);
    font-size: 10px;
    font-weight: 700;
    color: #fff;
  }
  .clip-cut {
    position: absolute;
    left: 25px;
    top: 5px;
    font-size: 9px;
    letter-spacing: 0.05em;
    color: rgba(243, 241, 238, 0.6);
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.6);
  }
  .clip-cut.cut {
    color: var(--accent);
    font-weight: 700;
  }
  /* Sits under the shot number, opposite the render dots, so it reads at a glance without
   * colliding with the cut arrow beside it. */
  .clip-aud {
    position: absolute;
    left: 6px;
    top: 20px;
    font-size: 9px;
    line-height: 1;
    color: var(--accent);
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.6);
    pointer-events: none;
  }
  .clip-relay {
    position: absolute;
    left: 6px;
    top: 32px;
    font-size: 9px;
    line-height: 1;
    color: var(--accent);
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.6);
    pointer-events: none;
  }
  /* A beat tick spans the clip's height so it reads as a moment IN the shot, not a marker on it. */
  .beat-tick {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 9px;
    margin-left: -4px;
    padding: 0;
    border: 0;
    background: none;
    cursor: ew-resize;
    touch-action: none;
    z-index: 3;
  }
  .beat-tick::before {
    content: '';
    position: absolute;
    top: 3px;
    bottom: 16px;
    left: 4px;
    width: 1px;
    background: var(--accent);
    box-shadow: 0 0 3px rgba(0, 0, 0, 0.7);
  }
  .beat-tick::after {
    content: '';
    position: absolute;
    top: 1px;
    left: 1.5px;
    width: 4px;
    height: 4px;
    border-radius: 50%;
    background: var(--accent);
  }
  .beat-tick.dragging::before,
  .beat-tick.dragging::after {
    background: #fff;
  }
  /* The block's own `[Shot N]` marks. No handle and no dot: these are not draggable, and anything
     that looks grabbable here would promise an edit the prose has to make. */
  .beat-mark {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 7px;
    margin-left: -3px;
    z-index: 3;
  }
  .beat-mark::after {
    content: '';
    position: absolute;
    inset: 12px auto 12px 3px;
    width: 1px;
    background: rgba(255, 255, 255, 0.75);
  }
  /* A cut gets the full height and the accent — it is a different picture on the other side of it,
     where a plain beat is the same shot still running. */
  .beat-mark.cut::after {
    inset: 6px auto 6px 3px;
    width: 2px;
    background: var(--accent);
  }
  /* 🔴 A mark that lands in the trimmed head. The model renders it and delivery throws those frames
     away, so it is a cue nobody will ever see — clamped to the block's left edge, in the danger
     colour, dashed so it does not read as a moment that happens. */
  .beat-mark.trimmed::after,
  .beat-mark.trimmed.cut::after {
    inset: 4px auto 4px 3px;
    width: 2px;
    background: repeating-linear-gradient(
      180deg,
      var(--danger, #e06c6c) 0 3px,
      transparent 3px 6px
    );
  }
  .clip .lbl {
    position: absolute;
    left: 6px;
    right: 6px;
    bottom: 5px;
    font-size: 10px;
    color: rgba(243, 241, 238, 0.9);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .rs {
    position: absolute;
    right: 5px;
    top: 5px;
    width: 9px;
    height: 9px;
    border-radius: 50%;
    border: 1.5px solid rgba(11, 13, 17, 0.55);
  }
  /* Proxy-preview status: a smaller square below the round final dot, so "previewed" reads
     distinctly from "finally rendered". Colour classes (.ok/.stale/.busy) cascade from .rs. */
  .rs.proxy {
    top: 17px;
    width: 7px;
    height: 7px;
    border-radius: 2px;
    opacity: 0.85;
  }
  .rs.ok {
    background: var(--success);
  }
  .rs.stale {
    background: var(--warning);
  }
  .rs.none {
    background: transparent;
    border-color: var(--muted);
  }
  .rs.busy {
    background: var(--accent);
    animation: rs-pulse 1.1s infinite;
  }
  @keyframes rs-pulse {
    0%,
    100% {
      opacity: 0.35;
    }
    50% {
      opacity: 1;
    }
  }
  .clip.none .film {
    filter: grayscale(0.5) brightness(0.6);
  }
  .clip.none {
    border-style: dashed;
  }
  /* Longer than the engine's 15.08s cap, so the tail will be truncated. The composer carries the
     meter and the explanation; this is only so the block is not silent about it. */
  .clip.over {
    border-color: var(--danger);
  }
  .clip.flagged {
    box-shadow: inset 0 -3px 0 rgba(249, 115, 22, 0.85);
  }
  .clip.flagged.selected {
    box-shadow:
      0 0 0 1px var(--accent),
      inset 0 -3px 0 rgba(249, 115, 22, 0.85);
  }
  .clip .flag {
    position: absolute;
    right: 5px;
    bottom: 5px;
    width: 15px;
    height: 15px;
    border-radius: 4px;
    background: var(--warning);
    color: #1a120b;
    font-size: 10px;
    font-weight: 800;
    display: grid;
    place-items: center;
  }
  /* A draft failure is not a continuity finding, so it reads differently: a red ring (outline, so
   * it never fights the box-shadow the selected/flagged states own) and a round ✕ badge. */
  .clip.draft-failed {
    outline: 2px solid var(--danger);
    outline-offset: -2px;
  }
  .clip .dfail {
    position: absolute;
    right: 5px;
    bottom: 5px;
    width: 15px;
    height: 15px;
    border-radius: 50%;
    background: var(--danger);
    color: #fff;
    font-size: 10px;
    font-weight: 800;
    display: grid;
    place-items: center;
  }
  .clip.draft-failed .flag {
    right: 24px;
  }
  .clip.draft-failed .lbl {
    right: 24px;
  }
  .clip.draft-failed.flagged .lbl {
    right: 43px;
  }
  .trim-handle {
    position: absolute;
    right: 0;
    top: 0;
    bottom: 0;
    width: 8px;
    cursor: ew-resize;
    touch-action: none;
    z-index: 3;
  }
  .clip:hover .trim-handle {
    background: linear-gradient(90deg, transparent, rgba(211, 140, 90, 0.5));
  }
  .clip-add {
    position: absolute;
    top: 4px;
    bottom: 4px;
    width: 96px;
    border: 1px dashed var(--ring-2, var(--ring));
    border-radius: 8px;
    background: none;
    color: var(--muted);
    font-size: 10.5px;
    font-weight: 600;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 3px;
    transition: 0.12s;
  }
  .clip-add:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  .clip-add .plus {
    font-size: 16px;
    line-height: 1;
  }
  .lane-empty {
    padding: 18px;
    color: var(--muted);
    font-size: 12px;
  }
  .pin-lane {
    background: rgba(255, 255, 255, 0.02);
    border-radius: 6px;
  }
  .pin {
    position: absolute;
    top: 3px;
    transform: translateX(-50%);
    cursor: grab;
    touch-action: none;
    width: 26px;
    height: 26px;
    border-radius: 5px;
    overflow: hidden;
    border: 1px solid var(--ring);
    background: var(--card);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .pin.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .pin img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .pin-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
  }
  .audio-lane {
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 8px;
    overflow: hidden;
  }
  .wave {
    position: absolute;
    inset: 0;
    opacity: 0.8;
  }
  /* Two rows: phrases on top where there is width to read them, word ticks along the bottom edge
     where they line up with the waveform they mark. */
  .lyrics {
    position: absolute;
    left: 0;
    bottom: 2px;
    height: 28px;
  }
  /* The phrase owns its own span, so its text has the whole line's width to be read in. */
  .phrase {
    position: absolute;
    top: 0;
    height: 16px;
    font-size: 10px;
    line-height: 14px;
    color: var(--muted);
    background: color-mix(in srgb, var(--accent-2, #2d6b67) 14%, transparent);
    border: 1px solid var(--ring);
    border-left: 2px solid var(--accent-2, #2d6b67);
    border-radius: 3px;
    padding: 0 4px;
    text-align: left;
    white-space: nowrap;
    overflow: hidden;
    cursor: pointer;
  }
  .phrase.hovered,
  .phrase:hover {
    color: var(--fg-1);
    border-color: var(--accent);
  }
  .lyric {
    position: absolute;
    top: 0;
    transform: translateX(-2px);
    font-size: 10px;
    color: var(--muted);
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    padding: 2px 7px;
    line-height: 14px;
    white-space: nowrap;
    cursor: pointer;
  }
  .lyric:hover {
    border-color: var(--accent);
    color: var(--ink);
    z-index: 2;
  }
  /* Collapsed label: a slim rhythm tick at the lane floor. */
  .lyric.tick {
    top: auto;
    bottom: 0;
    width: 2px;
    min-width: 2px;
    height: 7px;
    padding: 0;
    border: none;
    border-radius: 1px;
    background: var(--ring);
    opacity: 0.6;
  }
  /* A 2px target is not a target. Widen what the pointer can reach without moving the mark itself,
     so the words that COLLAPSED at this zoom — the ones whose onset you cannot otherwise read —
     are the ones you can still ask. */
  .lyric.tick::before {
    content: '';
    position: absolute;
    inset: -6px -4px;
  }
  .lyric.hovered {
    border-color: var(--accent);
    color: var(--accent);
  }
  .lyric.tick.hovered {
    background: var(--accent);
    opacity: 1;
  }
  .lyric.tick:hover {
    opacity: 1;
    background: var(--accent);
  }
  /* A word inside the NEXT render's run-up. Warned, not greyed: it is perfectly markable in the
     shot whose block plays it, and only unreachable in the one that re-renders and discards it. */
  .lyric.tick.seam {
    background: var(--warning, #e0a458);
    opacity: 0.75;
  }
  /* Touch only: grow what a finger has to hit, not what the handle occupies on the time axis.
     A mouse matches none of this, so desktop keeps its pixel precision. */
  @media (pointer: coarse) {
    .trim-handle {
      /* min() so a short shot keeps a draggable body — the handle can never eat the whole clip */
      width: min(20px, 35%);
      /* no hover on touch, so the handle has to show itself */
      background: linear-gradient(90deg, transparent, rgba(211, 140, 90, 0.45));
    }
    .pin {
      overflow: visible;
      width: 30px;
      height: 30px;
    }
    .pin img {
      border-radius: 4px;
    }
    .pin::before {
      content: '';
      position: absolute;
      top: 50%;
      left: 50%;
      width: 40px;
      height: 40px;
      transform: translate(-50%, -50%);
    }
    .lyric.tick::before {
      content: '';
      position: absolute;
      inset: -18px -9px;
    }
  }
</style>
