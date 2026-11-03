<script lang="ts">
  import { untrack } from 'svelte';
  import H3BlockEditor from './H3BlockEditor.svelte';
  import {
    filterDocs,
    indexOfMatch,
    replaceAllAcross,
    replaceMatch,
    searchMatches,
    stepMatch,
    type PromptMatch,
    type TextWindow
  } from '#lib/director/prompt-search.ts';
  import { DEFAULT_VOCAB, type EngineVocab } from '#lib/director/engine.ts';
  import { H3_PROMPT_MAX_CHARS } from '#lib/director/h3-prompt.ts';
  import {
    H3_SECTION_NAMES,
    looksLikeH3Block,
    parseH3Block,
    sectionLabel,
    type H3SectionName
  } from '#lib/director/h3-block.ts';
  import { h3SectionSpans } from '#lib/director/h3-highlight.ts';

  /**
   * **Every H3 prompt in the project, as text, with one find-and-replace across all of them.**
   *
   * Chrome-free on purpose: this is the body, and it is mounted in two different surfaces —
   * `H3PromptsScreen` puts it in a desktop modal, and the phone shell pushes it as a `bare` sheet
   * with its own back button. Everything below is identical in both, because the job is identical in
   * both and a second implementation is how the two would start disagreeing about what Replace all
   * means.
   *
   * ## What this is for, and why the composer cannot be it
   *
   * `H3PromptComposer` is a lens on ONE block: it holds the section panels, the reference pills, the
   * budgets and the beat editors, and it shows the render unit that is selected. That is the right
   * surface for writing a shot and the wrong one for the job an author actually has after a script
   * pass — *"the character is called Mia in eleven shots and I have renamed her"*, *"every block says
   * `handheld` and the whole cut should be locked off"*. Done through the composer that is fourteen
   * shot selections, fourteen scrolls to the right section, and no way to see whether the twelfth was
   * missed.
   *
   * So this strips everything that is not the text: one code box per render unit, in timeline order,
   * and a query that runs over all of them at once.
   *
   * ## 🔴 The filter is not a view, it is the SCOPE
   *
   * The filter box hides blocks that do not contain its text — and everything else here then means
   * "of the blocks shown". The hit count, the next/previous cursor and, the part that matters,
   * **Replace all** all operate on exactly the set left on screen. That is deliberate and it is the
   * useful half of the feature: *"in the shots that mention the harbour, change dawn to dusk"* is one
   * filter and one Replace all. It is also the dangerous half if it is implied rather than said, so
   * the button names its own scope and never reads a bare "Replace all".
   *
   * ## 🔴 Unwritten blocks are SHOWN seeded and STORED only if touched
   *
   * A render unit nobody has opened has no `prompt` key at all (`needsH3PromptSeed`), and the
   * composer's answer is to seed it on open — which marks the project dirty for a shot the author
   * only looked at. Doing that here would mean opening this wrote a block into every unit in the
   * project at once. So the parent hands down the seeded text to DISPLAY (the same `seedH3Prompt` the
   * compiler falls back to, so it is the block that would render either way), and nothing is stored
   * until an edit or a replacement actually rewrites it.
   */
  let {
    docs,
    vocab = DEFAULT_VOCAB,
    compact = false,
    onprompt,
    oncapture,
    ongoto
  }: {
    /**
     * One entry per render unit, IN TIMELINE ORDER — the order the cursor walks and the order the
     * cards are drawn.
     */
    docs: {
      /** The generation id: what `onprompt` writes against. */
      id: string;
      /** Its number on the timeline, 1-based. */
      no: number;
      /** The gist — what this shot is, in the words the timeline band uses. */
      gist: string;
      /** The block as it stands, or the seeded block for a unit nobody has written yet. */
      text: string;
      /** Whether that text is stored or is the seed being shown. Drives the "not written" chip. */
      written: boolean;
    }[];
    vocab?: EngineVocab;
    /**
     * Phone shell. Grows each code box to its document instead of giving it its own scrollbar — see
     * `H3BlockEditor`'s `grow`: a column of independently-scrolling boxes is a nested-scroll trap
     * under a thumb, where on the desktop a uniform row of them is what makes the list readable.
     */
    compact?: boolean;
    onprompt: (id: string, block: string) => void;
    /**
     * Take a whole-project restore point under this label, immediately before a bulk rewrite.
     *
     * 🔴 Replace all can rewrite every block in the project from one click, and each editor's own
     * undo history only reaches its own block — so without this the only way back from a mistyped
     * needle would be fourteen separate Ctrl+Zs, one per box, if the author noticed at all.
     */
    oncapture: (label: string) => void;
    /** Select this render unit in the editor behind, and leave. */
    ongoto: (id: string) => void;
  } = $props();

  /**
   * Why a block can be on screen without being stored. See the seeding note above — the parent hands
   * the seed down to display, and the first edit is what writes it.
   */
  const UNWRITTEN_HINT =
    'Nothing stored yet — this is the block that would render, shown so it can be edited. Editing it here, or replacing into it, is what stores it.';

  let filter = $state('');
  let query = $state('');
  let replacement = $state('');
  /**
   * One toggle for both boxes.
   *
   * A filter and a find that disagreed about case would be two rules to hold in mind for one
   * question ("which shots am I looking at, and what is highlighted in them"), and there is no
   * situation in this workflow that wants them to differ.
   */
  let caseSensitive = $state(false);
  /**
   * Match whole words only. Off by default, because half the useful needles here are fragments
   * (`handheld`, `dawn light`) — but a rename is the headline case and `Mia` → `Nell` also rewrites
   * the middle of `Miami` without it.
   */
  let wholeWord = $state(false);
  /**
   * Which section everything on this screen is scoped to, or `null` for the whole block.
   *
   * 🔴 This is the other half of the scope, and it composes with the filter: with `overall_soundscape`
   * chosen, the hit count, the cursor, Replace and Replace all all mean "in the soundscape lines" —
   * which is the difference between changing every score cue and changing the word *music* wherever it
   * happens to appear in the prose as well.
   */
  let section = $state<H3SectionName | null>(null);
  /** What Replace all last did. Cleared by the next edit to either box. */
  let note = $state('');

  /**
   * The hit the buttons act on.
   *
   * ⚠️ A pair of OFFSETS, not an index into {@link matches} — every block on this screen is a live
   * editor, so the list can change under the cursor between painting it and pressing Replace.
   * `replaceMatch` re-checks the bytes before it writes; see its refusal path.
   */
  let active = $state<PromptMatch | null>(null);

  /**
   * The card element per doc id, for bringing the active hit on screen.
   *
   * A plain object rather than `$state`: it is written by `bind:this` during render and read
   * imperatively from an effect, so making it reactive would only add a dependency that invalidates
   * the effect every time a card mounts.
   */
  const cardEls: Record<string, HTMLElement | undefined> = {};

  /**
   * Bring the card holding the active hit into the list's view.
   *
   * 🔴 Two scrollers have to agree for "Next" to actually show you the hit: the LIST (which card) and
   * the editor (which line inside it). The editor's half is `EditorView.scrollIntoView`, dispatched
   * by `H3BlockEditor` when the active range changes. This is the list's half, and it is done here
   * rather than left to CodeMirror because whether CM walks up to a scrollable ancestor is an
   * implementation detail of a dependency — and a Next button that sometimes does not move the page
   * is worse than no Next button. Doing both is idempotent: they scroll to the same place.
   *
   * `block: 'nearest'` so a hit already on screen does not jerk the list to re-centre it — the common
   * case is stepping through several hits inside one card, and nothing should move for those.
   */
  let scrolledTo: string | null = null;
  $effect(() => {
    const a = active;
    if (!a) {
      untrack(() => (scrolledTo = null));
      return;
    }
    const key = `${a.docId}|${a.from}`;
    if (untrack(() => scrolledTo) === key) return;
    untrack(() => (scrolledTo = key));
    cardEls[a.docId]?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  });

  const opts = $derived({ caseSensitive, wholeWord });

  /**
   * The scoped range for one block, in ABSOLUTE offsets.
   *
   * Two decisions worth naming:
   *
   * - It starts at the section's `bodyStart`, so a scoped Replace all **cannot rewrite the header**.
   *   `summary:` is a literal token the model was trained to see at that position; a needle that ran
   *   through it would change the schema rather than the writing.
   * - A block that has no such section gets an EMPTY window, never `null`. `null` means "the whole
   *   text" downstream, so returning it here would silently un-scope exactly the blocks the author
   *   cannot see the scope in. See the note in `prompt-search.ts`.
   */
  function sectionWindow(text: string, name: H3SectionName | null): TextWindow | null {
    if (!name) return null;
    const span = h3SectionSpans(parseH3Block(text)).find((sp) => sp.name === name);
    return span ? { from: span.bodyStart, to: span.end } : { from: 0, to: 0 };
  }

  /**
   * The docs with their scope attached. Re-derived per keystroke like everything else here; the parse
   * it costs is skipped entirely while nothing is scoped, which is the common case.
   */
  const windowed = $derived(docs.map((d) => ({ ...d, window: sectionWindow(d.text, section) })));
  /** Which sections exist anywhere in the project — the only ones worth offering to scope to. */
  const sectionsPresent = $derived.by(() => {
    const seen = new Set<string>();
    for (const d of docs) for (const sp of h3SectionSpans(parseH3Block(d.text))) seen.add(sp.name);
    return H3_SECTION_NAMES.filter((n) => seen.has(n));
  });
  /** `true` when a section is scoped and THIS block has not got it — nothing here can be searched. */
  const outOfScope = (d: { window?: TextWindow | null }) =>
    !!d.window && d.window.from === d.window.to;

  const shown = $derived(filterDocs(windowed, filter, opts));
  const matches = $derived(searchMatches(shown, query, opts));
  const at = $derived(indexOfMatch(matches, active));
  /**
   * Hits per block, folded out of the one search rather than re-run per card — the cards each need
   * their own count and a second pass over fourteen blocks per keystroke buys nothing.
   */
  const hitsByDoc = $derived.by(() => {
    const per = new Map<string, number>();
    for (const m of matches) per.set(m.docId, (per.get(m.docId) ?? 0) + 1);
    return per;
  });
  const hitDocs = $derived(hitsByDoc.size);

  /**
   * 🔴 The two ways a bulk rewrite breaks a block, reported where the rewrite is made.
   *
   * Every other prompt surface carries the 7,000-character budget (`MobilePrompt`, `MobileBlockEditor`,
   * the composer's own note) and this is the only one that can push a dozen blocks past it in one
   * click — so leaving it out here would put the counter on every screen except the one that needs
   * it. Past the cap is a **hard error**: `validateH3Prompt` refuses the render rather than trimming,
   * because a trim drops `non_diegetic_music` and the model invents a score.
   *
   * The second is subtler and only a global replace really does it: a needle that happens to run
   * through `detailed_description:` or `integrated_multimodal_description:` rewrites the header that
   * DECIDES THE SCHEMA, and the block stops being a block. It still renders — as prose, on a
   * checkpoint chosen without it — which is exactly the kind of failure nobody goes looking for.
   *
   * ⚠️ An EMPTY block is not one of these. Emptying a block is a legal state the author is allowed to
   * be in (see `needsH3PromptSeed`), and flagging it would put a red chip on every shot they are
   * halfway through clearing.
   */
  const overCap = $derived(shown.filter((d) => d.text.length > H3_PROMPT_MAX_CHARS));
  const notBlocks = $derived(
    shown.filter((d) => d.text.trim().length > 0 && !looksLikeH3Block(d.text))
  );

  /** The engine's own word for one render, singular or plural. `shot` on both engines. */
  const unitWord = (n: number) => (n === 1 ? vocab.renderUnit : vocab.renderUnitPlural);

  const step = (dir: 1 | -1) => (active = stepMatch(matches, active, dir));

  /**
   * Rewrite the hit under the cursor.
   *
   * With no cursor yet this takes the first hit rather than doing nothing — pressing Replace with a
   * query typed and no navigation is a clear enough instruction, and the highlight shows where it
   * landed. A refusal (the bytes moved) re-points the cursor instead of writing anything.
   */
  function replaceOne() {
    const target = active ?? matches[0];
    if (!target || !query) return;
    const r = replaceMatch(shown, target, query, replacement, opts);
    if (!r) {
      active = matches[0] ?? null;
      return;
    }
    note = '';
    onprompt(target.docId, r.text);
    active = r.active;
  }

  function replaceAll() {
    if (!query || matches.length === 0) return;
    const r = replaceAllAcross(shown, query, replacement, opts);
    if (r.count === 0) return;
    // Before the first write, so the label can name what is about to happen and the snapshot is of
    // the project as it was.
    oncapture(
      `Replaced “${query}”${section ? ` in ${sectionLabel(section)}` : ''} in ${r.changed.length} ${unitWord(r.changed.length)}`
    );
    for (const c of r.changed) onprompt(c.id, c.text);
    active = null;
    note = `${r.count} ${r.count === 1 ? 'hit' : 'hits'} replaced in ${r.changed.length} ${unitWord(
      r.changed.length
    )}.`;
  }

  /** Enter walks the hits; Shift+Enter walks them backwards. The find box is where both are typed. */
  function findKeys(e: KeyboardEvent) {
    if (e.key !== 'Enter') return;
    e.preventDefault();
    step(e.shiftKey ? -1 : 1);
  }

  /** Enter in the replacement box rewrites the hit under the cursor — the same as pressing Replace. */
  function replaceKeys(e: KeyboardEvent) {
    if (e.key !== 'Enter') return;
    e.preventDefault();
    replaceOne();
  }

  /**
   * Find-next from anywhere on the screen, including with the caret inside a block.
   *
   * The two bindings every editor has for this, and neither collides with CodeMirror's own keymap
   * (we do not load `@codemirror/search`, so nothing here binds them) or with typing. Without them,
   * walking a query means putting the caret back in the Find box after every edit.
   */
  function panelKeys(e: KeyboardEvent) {
    const findNext = e.key === 'F3' || ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'g');
    if (!findNext || !query) return;
    e.preventDefault();
    step(e.shiftKey ? -1 : 1);
  }
</script>

<!-- On the window, not on the panel: this screen is a modal and the keys should work wherever the
     focus sits in it — including the chrome outside this element — and a listener on a plain div
     needs an interactive role it has no business claiming. It is scoped by MOUNT: the panel only
     exists while the screen is open. -->
<svelte:window onkeydown={panelKeys} />

<div class="pf">
  <div class="pf-bar">
    <div class="pf-row">
      <label class="pf-field">
        <span>Filter</span>
        <input
          class="inp"
          placeholder="only {vocab.renderUnitPlural} whose block contains…"
          bind:value={filter}
          oninput={() => (note = '')}
        />
      </label>
      <label class="pf-field pf-in">
        <span>In</span>
        <select class="inp" bind:value={section}>
          <option value={null}>Whole block</option>
          {#each sectionsPresent as name (name)}
            <option value={name}>{sectionLabel(name)}</option>
          {/each}
        </select>
      </label>
      <span class="pf-count"
        >{shown.length} of {docs.length}
        {unitWord(docs.length)}</span
      >
    </div>

    <div class="pf-row">
      <label class="pf-field">
        <span>Find</span>
        <input
          class="inp"
          placeholder="text to find — literal, not a pattern"
          bind:value={query}
          oninput={() => {
            note = '';
            active = null;
          }}
          onkeydown={findKeys}
        />
      </label>
      <label class="pf-field">
        <span>Replace with</span>
        <input
          class="inp"
          placeholder="replacement"
          bind:value={replacement}
          oninput={() => (note = '')}
          onkeydown={replaceKeys}
        />
      </label>
      <button
        class="btn sm ghost pf-tog"
        class:on={caseSensitive}
        title="Match case — applies to the filter as well"
        aria-pressed={caseSensitive}
        onclick={() => {
          caseSensitive = !caseSensitive;
          active = null;
        }}>Aa</button
      >
      <button
        class="btn sm ghost pf-tog"
        class:on={wholeWord}
        title="Whole words only — “Mia” stops matching inside “Miami”. A needle that starts or ends in punctuation (‹Subject 1›, [Shot 2]) is unaffected."
        aria-pressed={wholeWord}
        onclick={() => {
          wholeWord = !wholeWord;
          active = null;
        }}>|ab|</button
      >
    </div>

    <div class="pf-row pf-actions">
      <span class="pf-count"
        >{#if !query}<i>nothing to find yet</i>{:else if matches.length === 0}<i
            >no hits{section ? ` in ${sectionLabel(section)}` : ''}{filter
              ? ` in the ${vocab.renderUnitPlural} shown`
              : ''}</i
          >{:else}<b>{matches.length}</b>
          {matches.length === 1 ? 'hit' : 'hits'} in {hitDocs}
          {unitWord(hitDocs)}{#if section}
            · <b>{sectionLabel(section)}</b> only{/if}{#if at >= 0}
            · on <b>{at + 1}</b>{/if}{/if}</span
      >
      <button
        class="btn sm ghost"
        disabled={matches.length === 0}
        title="Previous hit — Shift+Enter in the Find box, or Shift+F3 anywhere"
        onclick={() => step(-1)}>‹</button
      >
      <button
        class="btn sm ghost"
        disabled={matches.length === 0}
        title="Next hit — Enter in the Find box, F3 or ⌘G anywhere"
        onclick={() => step(1)}>›</button
      >
      <button class="btn sm" disabled={matches.length === 0} onclick={replaceOne}>Replace</button>
      <!-- 🔴 The button says its own scope. `shown` is the filter's output, and a bare "Replace
           all" beside a filter box would be a claim about the whole project that is false the
           moment anything is typed into that box. -->
      <button
        class="btn sm primary"
        disabled={matches.length === 0}
        title="Rewrite every hit {section ? `in ${sectionLabel(section)}` : 'in the block'} {filter
          ? `in the ${shown.length} ${unitWord(shown.length)} the filter leaves on screen`
          : 'in every shot in the project'}"
        onclick={replaceAll}
        >Replace all{#if section}
          in {sectionLabel(section)}{/if}{#if filter}
          · {shown.length} shown{/if}</button
      >
      {#if note}<span class="pf-note">{note}</span>{/if}
    </div>

    {#if overCap.length || notBlocks.length}
      <div class="pf-warn" role="alert">
        {#if overCap.length}
          <p>
            <b
              >{overCap.length}
              {overCap.length === 1 ? 'block is' : 'blocks are'} over the {H3_PROMPT_MAX_CHARS.toLocaleString()}-character
              serving limit</b
            >
            — {overCap.map((d) => d.no).join(', ')}. The render is refused rather than trimmed, so
            these have to come down before the cut will go.
          </p>
        {/if}
        {#if notBlocks.length}
          <p>
            <b
              >{notBlocks.length}
              {notBlocks.length === 1 ? 'block no' : 'blocks no'} longer reads as an H3 block</b
            >
            — {notBlocks.map((d) => d.no).join(', ')}. Neither <code>detailed_description</code> nor
            <code>integrated_multimodal_description</code> is present, so what is there is sent as prose
            and the checkpoint is chosen without it.
          </p>
        {/if}
      </div>
    {/if}
  </div>

  <div class="pf-list">
    {#if docs.length === 0}
      <p class="hint">
        No {vocab.renderUnitPlural} yet — a block appears here for each one on the timeline.
      </p>
    {:else if shown.length === 0}
      <p class="hint">
        No block contains “{filter}”. Clear the filter to see all {docs.length}
        {unitWord(docs.length)}.
      </p>
    {/if}

    <!-- One editor per block shown, all mounted at once: this screen's whole point is that the
         text is in front of you rather than behind a selection, and a card that had to be opened
         to show its prompt would be the composer again. Filtering unmounts what it hides, which is
         what keeps a long project's cost proportional to what is being worked on. -->
    {#each shown as d (d.id)}
      {@const hits = hitsByDoc.get(d.id) ?? 0}
      <section
        class="pf-card"
        class:hit={hits > 0}
        class:on={active?.docId === d.id}
        bind:this={cardEls[d.id]}
      >
        <header>
          <span class="pf-no">{d.no}</span>
          <b class="pf-gist">{d.gist || 'no gist'}</b>
          {#if !d.written}<span class="pf-chip" title={UNWRITTEN_HINT}>not written</span>{/if}
          {#if hits}<span class="pf-chip on">{hits} {hits === 1 ? 'hit' : 'hits'}</span>{/if}
          {#if section && outOfScope(d)}<span
              class="pf-chip"
              title="This block has no {sectionLabel(
                section
              )} section, so the scope in force matches nothing in it — Replace all will not touch it."
              >no {sectionLabel(section)}</span
            >{/if}
          {#if d.text.trim().length > 0 && !looksLikeH3Block(d.text)}<span
              class="pf-chip bad"
              title="No detailed_description and no integrated_multimodal_description — the two headers that decide the schema. This is sent as prose."
              >not a block</span
            >{/if}
          <span class="pf-grow"></span>
          <!-- The ratio only appears once it MATTERS. `12,004 ch` beside thirteen numbers that are
               fine is a column to scan; `12,004 / 7,000` on the one that is not is a sentence. -->
          {#if d.text.length > H3_PROMPT_MAX_CHARS}
            <span
              class="pf-chars over"
              title="Past MiniMax's serving limit — the render is refused, not trimmed"
              >{d.text.length.toLocaleString()} / {H3_PROMPT_MAX_CHARS.toLocaleString()}</span
            >
          {:else}
            <span class="pf-chars">{d.text.length.toLocaleString()} ch</span>
          {/if}
          <button
            class="btn sm ghost"
            title="Open this {vocab.renderUnit} in the composer"
            onclick={() => ongoto(d.id)}>Open ↗</button
          >
        </header>
        <!-- 🔴 `grow` on the phone: one scroller for the whole list instead of fourteen that each
             swallow a touch drag. See the prop's own note. -->
        <H3BlockEditor
          value={d.text}
          docId={d.id}
          grow={compact}
          rows={12}
          minHeight={200}
          placeholder="Nothing written for this {vocab.renderUnit} yet."
          search={query
            ? {
                query,
                caseSensitive,
                wholeWord,
                window: d.window,
                active:
                  active && active.docId === d.id ? { from: active.from, to: active.to } : null
              }
            : null}
          oninput={(next) => onprompt(d.id, next)}
        />
      </section>
    {/each}
  </div>
</div>

<style>
  /* Fills whatever it is put in — a desktop modal or the phone's whole body — and scrolls inside
     itself, so the toolbar stays put while the list moves. */
  .pf {
    flex: 1 1 auto;
    display: flex;
    flex-direction: column;
    min-height: 0;
    height: 100%;
  }

  /* ── The toolbar. Sticky at the top of the sheet, because the answer to "how many hits" has to
        stay on screen while the list under it is scrolled through. */
  .pf-bar {
    flex: none;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 11px 16px;
    border-bottom: 1px solid var(--ring);
    background: var(--bg-2);
  }
  .pf-row {
    display: flex;
    align-items: flex-end;
    gap: 8px;
  }
  /* Capped: the sheet is nearly the width of the screen so the CARDS can be, but a 900px-wide box
     for the word "Mia" reads as a form somebody forgot to lay out. The slack goes to the right of
     the row, where the counts sit. */
  .pf-field {
    flex: 1 1 0;
    min-width: 0;
    max-width: 380px;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .pf-field span {
    font-size: 9.5px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--muted);
    font-weight: 700;
  }
  /* Narrow and fixed: it is a scope, not a field to write in, and it must not steal width from the
     filter box beside it. */
  .pf-in {
    flex: 0 0 190px;
  }
  /* The two match toggles. Mono, because both labels are notation rather than words. */
  .pf-tog {
    font-family: var(--mono);
  }
  .pf-tog.on {
    border-color: var(--accent);
    color: var(--accent);
  }
  .pf-actions {
    align-items: center;
    flex-wrap: wrap;
  }
  .pf-count {
    font-family: var(--mono);
    font-size: 10.5px;
    color: var(--muted);
    white-space: nowrap;
  }
  .pf-count b {
    color: var(--ink);
  }
  .pf-actions .pf-count {
    flex: 1 1 auto;
  }
  .pf-note {
    font-size: 11px;
    color: var(--success, #6bbf6b);
  }

  /* ── The list ───────────────────────────────────────────────────────────────── */
  .pf-list {
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding: 12px 16px 20px;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .pf-card {
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: var(--bg-2);
    padding: 9px 10px 10px;
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  /* A block the query is IN. The border is the only cue that survives being scrolled past at speed,
     which is how a list of fourteen is read. */
  .pf-card.hit {
    border-color: var(--warning);
  }
  /* The card the buttons are ON, as opposed to the cards that merely contain hits. Without it,
     stepping through twelve hits across five shots moves a highlight somewhere off-screen and the
     list gives no clue which box it landed in. */
  .pf-card.on {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .pf-card header {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .pf-no {
    flex: none;
    width: 22px;
    height: 22px;
    border-radius: 7px;
    display: grid;
    place-items: center;
    background: var(--accent-dim);
    color: var(--accent);
    font-family: var(--mono);
    font-size: 11px;
    font-weight: 700;
  }
  .pf-gist {
    font-size: 12px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .pf-grow {
    flex: 1;
  }
  .pf-chars {
    font-family: var(--mono);
    font-size: 10px;
    color: var(--muted);
    white-space: nowrap;
  }
  .pf-chip {
    flex: none;
    padding: 1px 7px;
    border: 1px solid var(--ring);
    border-radius: 999px;
    font-family: var(--mono);
    font-size: 9.5px;
    color: var(--muted);
    white-space: nowrap;
  }
  .pf-chip.on {
    border-color: var(--warning);
    color: var(--warning);
  }
  .pf-chip.bad {
    border-color: var(--danger);
    color: var(--danger);
  }
  .pf-chars.over {
    color: var(--danger);
    font-weight: 700;
  }

  /* Under the toolbar rather than in it: this is about the DOCUMENTS, not about the query, and it has
     to be readable at two lines without pushing the list off the screen. */
  .pf-warn {
    border-top: 1px solid var(--ring);
    padding: 8px 16px;
    background: color-mix(in srgb, var(--danger) 9%, transparent);
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .pf-warn p {
    margin: 0;
    font-size: 11px;
    line-height: 1.5;
    color: var(--ink);
  }
  .pf-warn b {
    color: var(--danger);
  }
  .pf-warn code {
    font-family: var(--mono);
    font-size: 10.5px;
    color: var(--muted);
  }

  /* The phone, and a narrow desktop window: the three boxes stack, and the padding comes in to the
     phone's own 12px gutter. */
  @media (max-width: 700px) {
    .pf-row {
      flex-wrap: wrap;
    }
    .pf-field {
      flex-basis: 100%;
    }
    .pf-in {
      flex: 1 1 100%;
    }
    .pf-bar,
    .pf-list {
      padding-left: 12px;
      padding-right: 12px;
    }
    /* The gist and the char count fight for one 390px line; the count is the one to drop — unless it
       is the OVER-CAP one, which is the whole reason the count exists. */
    .pf-chars:not(.over) {
      display: none;
    }
    .pf-warn {
      padding-left: 12px;
      padding-right: 12px;
    }
  }
</style>
