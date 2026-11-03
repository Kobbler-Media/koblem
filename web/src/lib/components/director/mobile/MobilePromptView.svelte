<script lang="ts">
  import { parseH3Block } from '#lib/director/h3-block.ts';
  import { h3Tokens } from '#lib/director/h3-highlight.ts';

  /**
   * The prompt as it is stored, read-only, with the two things worth tapping made tappable: a
   * section header (which lands on Sections with that one loaded) and an ordinal (which says what it
   * points at). Writing happens in the full-screen editor or in the panels — a phone-sized
   * contenteditable inside a scrolling page is where caret handling goes wrong.
   */
  let {
    value = '',
    idea = '',
    structured = true,
    hasPrompt,
    busy = false,
    resolvedLabels,
    unit,
    onprompt,
    onidea,
    ondraft,
    onreroll,
    onsummarise,
    onedit,
    onrework,
    onsection,
    onordinal
  }: {
    value?: string;
    /** The one-line brief the draft is written FROM. Stored on the anchor beat; see the composer. */
    idea?: string;
    /** This engine's prompt is a schema with named sections rather than prose. */
    structured?: boolean;
    hasPrompt: boolean;
    busy?: boolean;
    resolvedLabels?: ReadonlySet<string>;
    unit: string;
    onprompt: (next: string) => void;
    onidea: (next: string) => void;
    ondraft: () => void;
    onreroll: () => void;
    onsummarise: () => void;
    onedit: () => void;
    onrework: () => void;
    onsection: (name: string) => void;
    onordinal: (label: string) => void;
  } = $props();

  const tokens = $derived(structured ? h3Tokens(value, parseH3Block(value)) : []);
  const oneLine = $derived(idea.trim().split('\n')[0] || `Nothing written yet`);

  const sectionOf = (header: string) => header.trim().replace(/:$/, '');
  const isDead = (label: string) => (resolvedLabels ? !resolvedLabels.has(label) : false);
</script>

<!-- 🔴 Open only when there is no prompt, because there it is the only thing to do. Once one exists
     the fold closes and keeps the idea's first line in the summary, so what it was written from is
     still readable without the box that rewrites it being one mis-tap away. -->
<details class="origin" open={!hasPrompt}>
  <summary>
    <span class="ol">Written from</span>
    <span class="oq">{oneLine}</span>
    <span class="chev" aria-hidden="true">›</span>
  </summary>
  <div class="obody">
    <textarea
      class="inp"
      rows="2"
      value={idea}
      oninput={(e) => onidea(e.currentTarget.value)}
      placeholder="One line: what happens in this {unit}."></textarea>
    <div class="orow">
      <button class="btn" type="button" disabled={busy || !idea.trim()} onclick={ondraft}
        >✦ Draft the prompt</button
      >
      <button
        class="btn icn"
        type="button"
        aria-label="Reroll the prompt"
        disabled={busy || !hasPrompt}
        onclick={onreroll}>↻</button
      >
    </div>
    <!-- The idea drafts the PROMPT. Sections are how it is stored, not what the button's scope is. -->
    <p class="hint warn">Replaces the whole prompt, including hand edits.</p>
  </div>
</details>

<p class="eye">Prompt</p>

{#if !structured}
  <textarea
    class="inp"
    rows="9"
    {value}
    oninput={(e) => onprompt(e.currentTarget.value)}
    placeholder="What this {unit} shows."></textarea>
{:else if !value.trim()}
  <div class="block">Nothing written yet — draft it above, or write the block by hand.</div>
{:else}
  <!-- ⚠️ One line, and `prettier-ignore` is what keeps it one. `.block` is `white-space: pre-wrap`,
       and Svelte collapses template whitespace to a single space rather than removing it — so every
       newline between these tags would be rendered as a space INSIDE the monospace block. -->
  <!-- ⚠️ `foreign` is a header line the parser does NOT recognise, so `h3SectionSpans` has no span
       for it and Sections would silently load the FIRST section instead. Marked, not tappable. -->
  <!-- prettier-ignore -->
  <div class="block">{#each tokens as t (t.start)}{#if t.kind === 'header'}<button class="sec" type="button" onclick={() => onsection(sectionOf(t.text))}>{t.text.trim()}</button>{:else if t.kind === 'foreign'}<span class="sec foreign">{t.text.trim()}</span>{:else if t.kind === 'label'}<button class="ord" class:dead={isDead(t.text)} type="button" onclick={() => onordinal(t.text)}>{t.text}</button>{:else if t.kind === 'shot' || t.kind === 'tasktypes'}<span class="tag">{t.text}</span>{:else if t.kind === 'time' || t.kind === 'marker'}<span class="tc">{t.text}</span>{:else}{t.text}{/if}{/each}</div>
  <p class="hint" style="margin-top:6px">
    Tap a <b>section</b> to edit it, or an ordinal to see what it points at.
  </p>
{/if}

<div class="aibar">
  {#if structured}
    <button class="btn sm" type="button" onclick={onedit}>✎ Edit block</button>
  {/if}
  <button class="btn sm" type="button" disabled={busy || !hasPrompt} onclick={onsummarise}
    >≡ Summarise</button
  >
  <button class="btn sm" type="button" disabled={!hasPrompt} onclick={onrework}>⌁ Rework…</button>
</div>
