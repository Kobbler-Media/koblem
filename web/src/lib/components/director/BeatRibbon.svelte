<script lang="ts">
  import { formatCutTime, h3ShotIsCut, type H3ShotBody } from '$lib/director/h3-prompt';
  import { capitalise, type EngineVocab } from '$lib/director/engine';

  /**
   * The beats of one shot, drawn to scale.
   *
   * ## 🔴 Why the shape is the point
   *
   * A `[Shot N]` mark with no transition verb is a **beat** — the camera keeps rolling and the
   * action lands at the stated time — and the same line with a verb in front is a **cut**. That one
   * distinction decides what the render looks like, and it was carried by a pair of chips inside a
   * textarea with a paragraph above explaining them. Here it is drawn: a cut is a hard rule down the
   * left of its cell, a beat is a dotted one. Cell width is what the beat renders for.
   *
   * ⚠️ Every cell is a line of THIS SHOT'S PROMPT and nothing else. The rows used to pair the
   * block's `[Shot N]` lines against timeline clips, so a cell could be a beat with no clip or a
   * clip with no beat — the two half-states that produced the drift box. A beat is a timed event
   * inside one shot now; there is no second side for it to disagree with.
   */
  let {
    rows,
    totalSeconds,
    runUpSeconds = 0,
    selected,
    vocab,
    opensOnImage = false,
    closesOnImage = false,
    frameFor,
    onselect
  }: {
    rows: H3ShotBody[];
    /** The shot's own length, which the cells are measured against. */
    totalSeconds: number;
    /**
     * The **run-up** — how much of the head this render covers and delivery then throws away.
     *
     * 🔴 It is not a cell and it is not subtracted from one. The ribbon is drawn on the RENDER's
     * clock, which is where `atSeconds` lives, so cell 0 genuinely starts at 0 and genuinely spans
     * the run-up; what the run-up changes is that the first `runUpSeconds` of that cell never reach
     * the cut. Drawn as a hatch OVER the head rather than as a cell of its own, because inserting a
     * cell would shift every other one and make the ribbon disagree with the times printed on it.
     *
     * 0 on the opening shot and on any unanchored seam, where the whole render is delivered.
     */
    runUpSeconds?: number;
    /** Index of the row being edited, or `null`. */
    selected: number | null;
    vocab: EngineVocab;
    /**
     * Whether the SHOT opens or closes on an attached image.
     *
     * ⚠️ Per shot, not per beat, because that is where the frames live: `compileH3Timeline` sends
     * exactly one `first_frame` and one `last_frame`. They are drawn on the first and last cells
     * because those are the moments they land on.
     */
    opensOnImage?: boolean;
    closesOnImage?: boolean;
    /** A still from the rendered take at this beat's time, when one could be had. */
    frameFor?: (row: H3ShotBody) => string | undefined;
    onselect: (index: number) => void;
  } = $props();

  const v = $derived(vocab);

  /** When each beat starts and how long it runs, off the block's own cut times. */
  const cells = $derived.by(() =>
    rows.map((r, i) => {
      const start = i === 0 ? 0 : (r.atSeconds ?? 0);
      const later = rows.map((x) => x.atSeconds ?? 0).filter((t) => t > start);
      const end = later.length ? Math.min(...later) : totalSeconds;
      return { row: r, index: i, start, seconds: Math.max(0.2, end - start) };
    })
  );

  const marksFor = (i: number) => {
    const out: { cls: string; title: string }[] = [];
    if (i === 0 && opensOnImage)
      out.push({ cls: 'img', title: 'this shot opens on an attached image' });
    if (i === rows.length - 1 && closesOnImage)
      out.push({ cls: 'end', title: 'this shot closes on an attached image' });
    return out;
  };

  const gistOf = (r: H3ShotBody) => r.body.replace(/\s+/g, ' ').trim();

  /**
   * Is any cell showing a frame?
   *
   * 🔴 The whole cell changes when it is, and it has to change TOGETHER: over a photograph the
   * muted-on-sunken text this was drawn in is unreadable, so framed cells switch to light text on a
   * scrim and grow tall enough for the picture to be worth having. Half-applying it — a frame behind
   * text still coloured for a flat panel — is exactly the state this fixes.
   */
  const framed = $derived(cells.some((c) => !!frameFor?.(c.row)));
  const cutCount = $derived(cells.filter((c) => c.index > 0 && h3ShotIsCut(c.row)).length);

  /**
   * The run-up as a share of the FIRST CELL, which is the cell it falls inside.
   *
   * 🔴 Not a share of the ribbon. The ribbon is a flex row with `overflow-x:auto` and per-cell
   * `min-width`, so its content is wider than its box whenever the shot has many beats — an absolute
   * band measured against the box would sit in the wrong place the moment it scrolled, and be the
   * wrong width the moment a `min-width` bound. Inside cell 0 it rides the cell, which is the only
   * element whose left edge is the render's 00:00.000 by construction.
   */
  const runUpPct = $derived.by(() => {
    const first = cells[0];
    if (!first || runUpSeconds <= 0 || first.seconds <= 0) return 0;
    return Math.min(100, (runUpSeconds / first.seconds) * 100);
  });
</script>

<div class="rib-sub">
  {rows.length}
  {rows.length === 1 ? 'timed moment' : 'timed moments'} inside one render · {cutCount}
  {cutCount === 1 ? (v.subUnitCut ?? 'cut') : (v.subUnitCutPlural ?? 'cuts')} ·
  {Math.max(0, rows.length - cutCount - 1)} rolling
  {#if opensOnImage || closesOnImage || runUpSeconds > 0}
    <span class="rib-legend">
      {#if runUpSeconds > 0}<span
          class="help"
          title="This {v.renderUnit} re-renders the previous one's tail as its own head, to get itself moving into the join, and delivery trims it. Those frames are rendered, the model hears their audio, and they never reach the cut — so a timed moment at or under {runUpSeconds.toFixed(
            2
          )}s is performed and then thrown away. Nothing is missing from the cut: the previous {v.renderUnit} already delivered that stretch."
          ><i class="mk run"></i>first {runUpSeconds.toFixed(2)}s is run-up</span
        >{/if}
      {#if opensOnImage}<span><i class="mk img"></i>opens on an image</span>{/if}
      {#if closesOnImage}<span><i class="mk end"></i>closes on one</span>{/if}
    </span>
  {/if}
</div>

<div
  class="ribbon"
  class:framed
  role="tablist"
  aria-label="{capitalise(v.subUnitPlural ?? 'beats')} in this {v.renderUnit}"
>
  {#each cells as c (c.index)}
    {@const cut = h3ShotIsCut(c.row)}
    {@const marks = marksFor(c.index)}
    {@const frame = frameFor?.(c.row)}
    <button
      class="beat"
      class:cut={c.index > 0 && cut}
      class:roll={c.index > 0 && !cut}
      class:sel={selected === c.index}
      style="flex:{c.seconds} 1 0"
      role="tab"
      aria-selected={selected === c.index}
      title={cut
        ? `Cuts here — ${c.row.cutVerb ?? 'a transition verb opens the line'}`
        : c.index === 0
          ? `Opens the ${v.renderUnit}`
          : 'The camera keeps rolling through this one'}
      onclick={() => onselect(c.index)}
    >
      {#if frame}
        <span class="film" style="background-image:url({frame})"></span>
        <span class="veil"></span>
      {/if}
      <!-- The head this render covers and delivery discards. Drawn INSIDE the opening cell rather
           than as a cell of its own: the ribbon is on the render's clock, cell 0 really does start
           at 0 and really does span the run-up, and inserting a cell would shift every other one
           away from the time printed on it.

           `pointer-events:none`, and therefore NO title of its own — the cell has to stay
           selectable, and cell 0 is usually the widest one. The explanation lives on the legend
           chip above, which is inert and can carry a tooltip for free. -->
      {#if c.index === 0 && runUpPct > 0}
        <span class="runup" style="width:{runUpPct}%"></span>
      {/if}
      <span class="meta">
        <span class="n">{capitalise(v.subUnit ?? 'beat')} {c.index + 1}</span>
        <span class="t">
          {c.index === 0 ? 'opens' : formatCutTime(c.start)} · {c.seconds.toFixed(2)}s
          {#if cut && c.index > 0}<b class="cutword">cut</b>{/if}
        </span>
        <span class="g">{gistOf(c.row)}</span>
        <span class="marks">
          {#each marks as m (m.cls)}<i class="mk {m.cls}" title={m.title}></i>{/each}
        </span>
      </span>
    </button>
  {/each}
</div>

<!-- 0.00s is the RENDER's zero, which on a continued shot is `runUpSeconds` before anything the
     audience sees. Both ends are labelled from the render, because that is the clock the `At` times
     on the cells above are written on. -->
<div class="rib-ruler">
  <span
    >0.00s{#if runUpSeconds > 0}<i> · delivered from {runUpSeconds.toFixed(2)}s</i>{/if}</span
  >
  <span>{totalSeconds.toFixed(2)}s</span>
</div>

<style>
  /* The sub-line and the legend: what the marks on the cells mean, said once, beside them. */
  .rib-sub {
    display: flex;
    align-items: center;
    gap: 11px;
    flex-wrap: wrap;
    margin-bottom: 5px;
    font-size: 11px;
    color: var(--faint, var(--muted));
  }
  .rib-legend {
    display: flex;
    gap: 11px;
    margin-left: auto;
  }
  .rib-legend span {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .rib-legend .help {
    color: var(--warning);
    cursor: help;
  }
  .rib-ruler {
    display: flex;
    justify-content: space-between;
    margin-top: 4px;
    font-family: var(--mono);
    font-size: 9px;
    color: var(--faint, var(--muted));
  }
  .rib-ruler i {
    font-style: normal;
    color: var(--warning);
  }
  .ribbon {
    display: flex;
    background: var(--bg-3, var(--bg-2));
    border: 1px solid var(--ring);
    border-radius: 10px;
    overflow-x: auto;
    min-height: 92px;
    scrollbar-width: thin;
  }
  /* 🔴 Framed cells are a contact sheet, so the cell has to read as a FRAME first: tall enough and
     wide enough for the picture to be worth looking at, with the caption sitting on it. */
  .ribbon.framed {
    min-height: 154px;
  }
  .beat {
    position: relative;
    min-width: 96px;
    padding: 0;
    border: 0;
    border-left: 1px solid var(--ring);
    background: none;
    color: inherit;
    text-align: left;
    cursor: pointer;
    overflow: hidden;
  }
  .beat:first-child {
    border-left: 0;
  }
  /* The distinction the whole panel exists to make legible. */
  .beat.cut {
    border-left: 3px solid var(--accent);
  }
  .beat.roll {
    border-left: 1px dotted var(--ring-2, var(--ring));
  }
  .beat:hover {
    background: var(--bg-2);
  }
  .beat.sel {
    background: var(--accent-dim);
    box-shadow: inset 0 -2px 0 var(--accent);
  }
  /* ⚠️ Over a picture the accent WASH is invisible and it dirties the frame. A ring reads as
     selection at any brightness, which a 15% tint over film does not. */
  .ribbon.framed .beat.sel {
    background: none;
    box-shadow: inset 0 0 0 2px var(--accent);
  }
  .ribbon.framed .beat {
    min-width: 132px;
  }
  .film {
    position: absolute;
    inset: 0;
    background-size: cover;
    background-position: center;
  }
  /* Four stops, not two: the caption sits at the foot and the cell number at the head, so both ends
     need ground under them while the middle — the actual picture — stays clear. */
  .veil {
    position: absolute;
    inset: 0;
    background: linear-gradient(
      180deg,
      rgba(0, 0, 0, 0.24) 0%,
      rgba(0, 0, 0, 0) 22%,
      rgba(0, 0, 0, 0.1) 48%,
      rgba(0, 0, 0, 0.82) 100%
    );
  }
  .beat:hover .veil {
    background: linear-gradient(
      180deg,
      rgba(0, 0, 0, 0.18) 0%,
      rgba(0, 0, 0, 0) 30%,
      rgba(0, 0, 0, 0.06) 55%,
      rgba(0, 0, 0, 0.7) 100%
    );
  }
  .meta {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 2px;
    height: 100%;
    padding: 7px 9px 8px;
  }
  /* Picture on top, caption at the foot — the cell has to read as a frame before it reads as a row
     of text about one. */
  .ribbon.framed .meta {
    position: absolute;
    inset: 0;
    justify-content: flex-end;
  }
  .ribbon.framed .n {
    position: absolute;
    top: 7px;
    left: 9px;
  }
  .ribbon.framed .marks {
    margin-top: 4px;
  }
  /* 🔴 The fix for the wash-out. `--muted` and `--faint` are picked to sit on a flat panel; on a
     photograph they disappear into whatever happens to be behind them. Light ink plus a shadow is
     legible over a bright sky and a dark hull alike. */
  .ribbon.framed .t,
  .ribbon.framed .g {
    color: #e2dcd3;
    text-shadow: 0 1px 3px rgba(0, 0, 0, 0.9);
  }
  .ribbon.framed .n {
    text-shadow: 0 1px 3px rgba(0, 0, 0, 0.9);
  }
  .n {
    font-family: var(--mono);
    font-size: 10px;
    font-weight: 650;
    color: var(--accent);
  }
  .t {
    font-family: var(--mono);
    font-size: 9.5px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }
  .cutword {
    color: var(--accent);
    font-weight: 700;
    letter-spacing: 0.09em;
    text-transform: uppercase;
    font-size: 8.5px;
    margin-left: 5px;
  }
  .g {
    font-size: 10.5px;
    color: var(--muted);
    line-height: 1.32;
    overflow: hidden;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
  }
  .marks {
    margin-top: auto;
    display: flex;
    align-items: center;
    gap: 5px;
    height: 10px;
  }
  /* Drawn rather than typed: a glyph that falls back to tofu on one machine is worse than no mark. */
  .mk {
    width: 8px;
    height: 8px;
    border-radius: 2px;
    flex: none;
  }
  /* Hatched, matching the timeline's own run-up band — one mechanism, one texture, so the strip on
     the block and the strip in this ribbon read as the same thing rather than two coincidences.
     No z-index: paint order alone puts it above `.film`/`.veil` and under `.meta`, which is the
     reading order — a hatch over the picture, with the cell's own caption still on top of it. */
  .runup {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 0;
    pointer-events: none;
    border-right: 1px dashed var(--warning);
    background-image: repeating-linear-gradient(
      -45deg,
      rgba(11, 13, 17, 0.4) 0 4px,
      rgba(224, 164, 88, 0.26) 4px 8px
    );
  }
  .mk.img {
    background: var(--accent);
  }
  .mk.run {
    background-image: repeating-linear-gradient(-45deg, transparent 0 2px, var(--warning) 2px 4px);
    box-shadow: inset 0 0 0 1px var(--warning);
  }
  .mk.end {
    background: linear-gradient(90deg, transparent 50%, var(--accent) 50%);
    box-shadow: inset 0 0 0 1px var(--accent);
  }
</style>
