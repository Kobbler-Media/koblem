<script lang="ts">
  import { planIsEmpty, planNoteCount, type H3Plan } from '#lib/director/h3-plan.ts';
  import {
    H3_CHECKPOINT_FLIP_SECONDS as H3_FLIP_SECONDS,
    formatDuration,
    h3Conditioning,
    h3ReloadCost,
    h3RenderWall
  } from '#lib/director/h3-telemetry.ts';
  import { DEFAULT_VOCAB, type EngineVocab } from '#lib/director/engine.ts';

  /**
   * What the ENGINE said it would render — the plan echo, stamped onto the project at dispatch.
   *
   * 🔴 **The whole point is that "my setting did nothing" stops being invisible.** WanGP filters its
   * task dict against a function signature and drops unknown keys *silently* (`api_cli.py:121`), so
   * a setting the engine does not understand costs nothing, changes nothing, and reports nothing:
   * the default renders and the job succeeds. The plan echo is the only channel that says otherwise,
   * and a wall of undifferentiated text would defeat it — so the three lists are drawn apart and
   * labelled by what they MEAN rather than by their field names:
   *
   * | field | shown as | what it costs you |
   * |---|---|---|
   * | `warnings` | Happened differently | the render exists, but not as authored (a frame snap, a seam) |
   * | `ignored`  | The engine threw it away | you set it, it arrived, it was discarded |
   * | `dropped`  | koblem never sent it | a leftover LTX field the H3 allowlist declined |
   *
   * Collapsed by default: after a clean run there is nothing to act on, and a panel that demands
   * attention every time stops being read. The summary line carries the count so a run WITH notes
   * announces itself without being opened.
   *
   * ## It is also the render's TIMESHEET, and that is not a second job
   *
   * The plan echo outlives the job store's TTL — koblem stamps it onto the project row — so it is
   * the only place a finished render's real numbers survive. Three of them landed here on
   * 2026-08-08, all of which the server had all along and none of which anybody could see:
   *
   * 1. **`render_seconds` per generation, and the gate wait apart from the render.** The Director
   *    reported "~1 hr" for a 24.9-minute render because it was timing its own session — a failed
   *    submit, a 54.2 s wait for the card, a retry, and then the render. Those are four different
   *    numbers and only one of them is the engine's.
   * 2. **`task` and `references[]`, as the engine parsed them.** `"task": "t2va"` where `ref2va`
   *    was expected, or a `references[]` with no `<Audio 1>`, is the earliest possible signal that
   *    conditioning was dropped — and every one of those renders returns a happy 200.
   * 3. **The price of a checkpoint switch.** `reload_required` was already drawn per generation;
   *    what was missing is that 13 of them is 13 minutes, which is a decision rather than a fact.
   *
   * 🔴 All three are plan-level facts and belong here rather than in the composer: they describe a
   * render that already happened, and the composer describes one that has not.
   */
  let {
    plan,
    engineLabel = 'MiniMax-H3',
    vocab = DEFAULT_VOCAB
  }: {
    plan: H3Plan;
    engineLabel?: string;
    /**
     * What this engine calls its units. `H3Plan.generation_plans` keeps its wire name — this is a
     * DISPLAY vocabulary and the 202's JSON is not ours to rename. See `EngineVocab`.
     */
    vocab?: EngineVocab;
  } = $props();

  const reloadCost = $derived(h3ReloadCost(plan));
  const wall = $derived(h3RenderWall(plan));
  const notes = $derived(planNoteCount(plan));
  const eta = $derived(formatDuration(plan.total_eta_seconds));
  const stampedAt = $derived.by(() => {
    if (!plan.at) return null;
    const ms = Date.parse(plan.at);
    return Number.isFinite(ms) ? new Date(ms).toLocaleString() : null;
  });
  const empty = $derived(planIsEmpty(plan));
</script>

{#if !empty}
  <details class="plan-echo">
    <summary>
      <span class="chev">▶</span><b>What the engine actually rendered</b>
      {#if notes}
        <span class="tally" title="Things that did not happen as authored">{notes}</span>
      {/if}
      <span class="tl-meta"
        >{plan.generation_plans.length}
        {plan.generation_plans.length === 1 ? vocab.renderUnit : vocab.renderUnitPlural}{#if eta}
          · ~{eta}{/if}</span
      >
    </summary>

    <p class="hint">
      The 202 {engineLabel} answered the dispatch with — the geometry you did <em>not</em> choose.
      Frame counts here are <b>snapped</b>: the engine rounds up onto its 5+17k grid, so these are
      the lengths that rendered, not the ones that were asked for.{#if stampedAt}
        <span class="tl-meta">Stamped {stampedAt}.</span>{/if}
    </p>

    {#if wall.render}
      <!-- 🔴 The queue and the render are drawn APART and never summed. A 24.9-minute render was
           reported as "~1 hr" because the editor timed its own session — a failed submit, a 54.2 s
           wait for the card, a retry, then the render. Only the middle number here is the engine's,
           and the fix for a long queue (scheduling) is not the fix for a long render (geometry). -->
      <div class="wall">
        <span><b>{wall.render}</b> on the GPU</span>
        {#if wall.gateWait}
          <span class="tl-meta"
            >· {wall.gateWait} of that was waiting for the card, before anything rendered</span
          >
        {/if}
        {#if wall.generations}
          <span class="tl-meta"
            >· {wall.generations} of it inside the {vocab.renderUnitPlural} below</span
          >
        {/if}
      </div>
      <p class="hint">
        This is the render's own clock, not the editor's. The editor times a session — a failed
        submit, a queue and a retry all land inside it, which is how a 25-minute render gets
        reported as an hour.
      </p>
    {/if}

    {#if plan.generation_plans.length}
      <div class="gens">
        {#each plan.generation_plans as g, i (i)}
          {@const cond = h3Conditioning(g)}
          <div class="gen-row">
            <span class="idx">{(g.index ?? i) + 1}</span>
            <div class="gcol">
              <div class="gline">
                {#if g.frames !== undefined}<b>{g.frames}f</b>{/if}
                {#if g.seconds !== undefined}<span class="tl-meta">{g.seconds.toFixed(2)}s</span
                  >{/if}
                {#if g.task}<span class="task" class:bare={g.task === 't2va'}>{g.task}</span>{/if}
                {#if g.width && g.height}<span class="tl-meta">{g.width}×{g.height}</span>{/if}
              </div>
              {#if cond.labels.length}
                <!-- 🔴 What ACTUALLY conditioned this generation, in the ordinals the prompt had to
                     name. A missing <Audio 1> here is the song never attaching — the picture comes
                     back fine and the only other way to find out is to watch it. -->
                <div class="refs">
                  {#each cond.labels as l, li (li)}<span class="ref">{l}</span>{/each}
                </div>
              {/if}
              {#if cond.note}
                <div class="dropped-cond">🔴 {cond.note}</div>
              {/if}
              <div class="tl-meta gsub">
                {#if g.steps !== undefined}{g.steps} steps{/if}
                {#if g.seed !== undefined}
                  · seed {g.seed < 0 ? 'random' : g.seed}{/if}
                {#if g.prompt_chars !== undefined}
                  · {g.prompt_chars} prompt chars{/if}
                {#if g.checkpoint}
                  · <span class="ckpt">{g.checkpoint}</span>{/if}
              </div>
              {#if g.reload_required}
                <!-- 🔴 Named here so the minutes of silence between two generations have a cause.
                     A fl2va ↔ ref2va switch is a full release_model() + ~20 GB reload, never
                     incremental — it is already inside this generation's ETA. -->
                <div class="reload">
                  ⟳ Loads a different checkpoint first — ~20 GB of DiT and text encoder, minutes of
                  no visible progress before the first step.
                </div>
              {/if}
            </div>
            <!-- 🔴 The measured time REPLACES the estimate once there is one, rather than sitting
                 beside it. Two numbers a few pixels apart, one prefixed `~`, is how an ETA gets
                 quoted back as a measurement. The estimate has done its job by then. -->
            {#if g.render_seconds !== undefined}
              <span
                class="geta measured"
                title="Measured by the engine for this {vocab.renderUnit}."
                >{formatDuration(g.render_seconds)}</span
              >
            {:else if g.eta_seconds !== undefined}
              <span
                class="geta"
                title="The engine's estimate at submit — this render was not measured."
                >~{formatDuration(g.eta_seconds)}</span
              >
            {/if}
          </div>
        {/each}
      </div>
      {#if reloadCost.reloads > 0}
        <!-- 🔴 The PRICE, not just the count. "Switches checkpoint 13 times" is a fact nobody can
             act on; "13 minutes of this render is loading weights" is a decision — and the usual
             fix is free, because what forces the flip is almost always a frame pin, and a prose
             keyframe on ref2va holds the frame nearly as well while keeping the references and the
             song. 64 s is MEASURED (2026-08-07): 31.9 s reload + 32.5 s of slower first step. The
             second half is the one everyone forgets. -->
        <div class="lst warn">
          <div class="lst-h">
            Checkpoint switches<span class="tl-meta">{reloadCost.reloads}</span>
          </div>
          <p class="hint">
            {reloadCost.note} Each switch is a full <code>release_model()</code> plus a ~20 GB DiT
            and text-encoder reload — never incremental — and it costs
            <b>~{formatDuration(H3_FLIP_SECONDS)}</b>: 31.9s of reloading plus 32.5s of a slower
            first step on the fresh weights. It is already inside the ETAs above, and it is the
            pause between {vocab.renderUnitPlural}, not a hang.
          </p>
          <p class="hint">
            What forces one is a {vocab.renderUnit} that pins a frame (<b>fl2va</b>) sitting next to
            one that carries references (<b>ref2va</b>) — they are different weight sets. Ordering
            the timeline so the pinned {vocab.renderUnitPlural} sit together buys the difference back
            for nothing.
          </p>
        </div>
      {/if}
    {/if}

    <!-- ── Three lists, three different failures. Never merged. ─────────────────────────── -->
    {#if plan.warnings.length}
      <div class="lst warn">
        <div class="lst-h">
          Happened differently<span class="tl-meta">{plan.warnings.length}</span>
        </div>
        <p class="hint">
          It rendered — just not exactly as authored. A frame count snapped onto the grid, or a
          timeline split across {vocab.renderUnitPlural} so there is now a seam.
        </p>
        <ul>
          {#each plan.warnings as w, i (i)}<li>{w}</li>{/each}
        </ul>
      </div>
    {/if}

    {#if plan.ignored.length}
      <div class="lst ignored">
        <div class="lst-h">
          The engine threw it away<span class="tl-meta">{plan.ignored.length}</span>
        </div>
        <p class="hint">
          🔴 These reached the engine and were <b>discarded there</b>. The default rendered in their
          place and the render still succeeded — this list is the only place that fact appears. If
          one of them is a setting you deliberately changed, it did nothing.
        </p>
        <ul>
          {#each plan.ignored as f, i (i)}<li><code>{f}</code></li>{/each}
        </ul>
      </div>
    {/if}

    {#if plan.dropped.length}
      <div class="lst dropped">
        <div class="lst-h">
          koblem never sent it<span class="tl-meta">{plan.dropped.length}</span>
        </div>
        <p class="hint">
          Fields the saved project still carries that the {engineLabel} allowlist declined on the way
          out — usually leftovers from when this project rendered on LTX (LoRAs, beats, a negative prompt).
          Harmless, but they are not conditioning anything.
        </p>
        <ul>
          {#each plan.dropped as f, i (i)}<li><code>{f}</code></li>{/each}
        </ul>
      </div>
    {/if}
  </details>
{/if}

<style>
  .plan-echo {
    margin-top: 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--bg-2);
    padding: 8px 10px;
  }
  summary {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    list-style: none;
    font-size: 12px;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  .chev {
    font-size: 9px;
    color: var(--muted);
    transition: transform 0.12s;
  }
  .plan-echo[open] .chev {
    transform: rotate(90deg);
  }
  /* The count on the closed summary: a run WITH notes has to announce itself, or a collapsed
     panel is the same as no panel. */
  .tally {
    font-family: var(--mono);
    font-size: 10px;
    padding: 0 6px;
    border-radius: 999px;
    border: 1px solid var(--warning);
    color: var(--warning);
  }
  summary .tl-meta {
    margin-left: auto;
  }
  .gens {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin: 8px 0;
  }
  .gen-row {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    padding: 6px 0;
    border-bottom: 1px dashed var(--ring);
  }
  .gen-row:last-child {
    border-bottom: none;
  }
  .idx {
    font-family: var(--mono);
    font-size: 10px;
    color: var(--muted);
    border: 1px solid var(--ring);
    border-radius: 5px;
    padding: 1px 5px;
    flex: none;
  }
  .gcol {
    flex: 1;
    min-width: 0;
  }
  .gline {
    display: flex;
    gap: 7px;
    align-items: baseline;
    flex-wrap: wrap;
    font-size: 12px;
  }
  .gsub {
    margin-top: 2px;
    overflow-wrap: anywhere;
  }
  .task {
    font-family: var(--mono);
    font-size: 10px;
    padding: 1px 7px;
    border-radius: 999px;
    border: 1px solid var(--ring);
    color: var(--accent);
  }
  .ckpt {
    font-family: var(--mono);
  }
  /* The render's own clock, above the per-generation rows: it is the answer to "how long did that
     take", and it has to be findable without opening a row. */
  .wall {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 6px;
    margin: 8px 0 2px;
    font-size: 12.5px;
  }
  /* The ordinals a generation was actually conditioned with. Mono, because they are the literal
     strings the prompt had to name — `<Audio 1>` is a token, not a description. */
  .refs {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
    margin-top: 3px;
  }
  .ref {
    font-family: var(--mono);
    font-size: 10.5px;
    padding: 1px 6px;
    border-radius: 5px;
    border: 1px solid var(--ring);
    color: var(--muted);
  }
  /* `t2va` where conditioning was expected: the render succeeded and carried none of it. */
  .task.bare {
    border-color: var(--danger);
    color: var(--danger);
  }
  .dropped-cond {
    margin-top: 4px;
    font-size: 11px;
    line-height: 1.5;
    color: var(--danger);
  }
  /* Measured, so it does not wear the `~` and does not read as another estimate. */
  .geta.measured {
    color: var(--ink);
  }
  .reload {
    margin-top: 4px;
    font-size: 11px;
    line-height: 1.5;
    color: var(--warning);
  }
  .geta {
    font-family: var(--mono);
    font-size: 11px;
    color: var(--muted);
    flex: none;
  }
  /* Three lists, kept apart on purpose — the left rule is the fastest way to see WHICH kind of
     "it didn't do what I asked" you are looking at without reading the heading. */
  .lst {
    margin-top: 10px;
    padding: 7px 0 7px 10px;
    border-left: 2px solid var(--ring);
  }
  .lst.warn {
    border-left-color: var(--warning);
  }
  .lst.ignored {
    border-left-color: var(--danger);
  }
  .lst.dropped {
    border-left-color: var(--muted);
  }
  .lst-h {
    display: flex;
    align-items: baseline;
    gap: 8px;
    font-size: 12px;
    font-weight: 600;
    margin-bottom: 3px;
  }
  .lst-h .tl-meta {
    margin-left: auto;
  }
  .lst ul {
    margin: 5px 0 0;
    padding-left: 16px;
    font-size: 11.5px;
    line-height: 1.6;
    overflow-wrap: anywhere;
  }
  .lst code {
    font-family: var(--mono);
    font-size: 11px;
  }
</style>
