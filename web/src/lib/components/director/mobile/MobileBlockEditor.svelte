<script lang="ts">
  import H3BlockEditor from '../H3BlockEditor.svelte';
  import { parseH3Block, sectionLabel } from '#lib/director/h3-block.ts';
  import {
    h3SectionSpans,
    h3Tokens,
    type H3Token,
    type H3TokenCard
  } from '#lib/director/h3-highlight.ts';
  import { formatCutTime, H3_PROMPT_MAX_CHARS } from '#lib/director/h3-prompt.ts';
  import { deadOrdinalNote, type MobileRef } from '#lib/director/mobile.ts';
  import MobileRefCard from './MobileRefCard.svelte';

  /**
   * The desktop's CodeMirror block editor, full screen, with phone chrome around it.
   *
   * 🔴 `H3BlockEditor` is mounted UNCHANGED — same decorations, same `docId`-scoped undo, same
   * `resolvedLabels` marking. What is mobile is the chrome: a section rail, a context bar docked
   * where the keyboard will be (hover does not exist on touch, so the desktop's token card has to
   * become somewhere it can stay), and a row of insert chips, because typing `<` on a soft keyboard
   * is two layers deep.
   */
  let {
    value,
    docId,
    refs = [],
    resolvedLabels,
    unit,
    subUnit = 'shot',
    oninput,
    onclose
  }: {
    value: string;
    /** The generation's id. Passed straight through — it is what scopes undo to one render unit. */
    docId: string;
    refs?: MobileRef[];
    resolvedLabels?: ReadonlySet<string>;
    unit: string;
    subUnit?: string;
    oninput: (next: string) => void;
    onclose: () => void;
  } = $props();

  const parsed = $derived(parseH3Block(value));
  const spans = $derived(h3SectionSpans(parsed));
  const tokens = $derived(h3Tokens(value, parsed));

  /** Both numbers that will refuse a render, and both are cheap enough to keep live. */
  const dead = $derived(
    resolvedLabels
      ? [
          ...new Set(
            tokens
              .filter((t) => t.kind === 'label' && !resolvedLabels.has(t.text))
              .map((t) => t.text)
          )
        ]
      : []
  );

  const byLabel = $derived(new Map(refs.map((r) => [r.label, r])));
  /** `<Picture 1>` → the prose in `subject_definitions` that says what it IS. */
  const definitions = $derived(
    new Map(parsed.subjectDefinitions.map((d) => [d.label.trim(), d.text]))
  );

  let caretSection = $state<string | null>(null);
  let caretShot = $state<number | null>(null);
  let token = $state<H3Token | null>(null);
  let card = $state<MobileRef | null>(null);
  let claim = $state<{ insert: (label: string) => void } | null>(null);
  let railEl = $state<HTMLDivElement | null>(null);

  function lookup(t: H3Token): H3TokenCard | null {
    if (t.kind !== 'label') return null;
    const r = byLabel.get(t.text);
    return r
      ? { title: r.label, meta: `${r.name} · ${r.role}`, image: r.image || null }
      : {
          title: t.text,
          meta: 'nothing defines this',
          warn: 'the model reads it as ordinary words'
        };
  }

  /** What the bar says when the caret is not standing on a token — which is most of the time. */
  const ctx = $derived.by(() => {
    if (token?.kind === 'label') {
      const r = byLabel.get(token.text);
      return r
        ? { name: token.text, desc: `${r.name} · ${r.role}`, image: r.image, bad: false, ref: r }
        : {
            name: token.text,
            desc: deadOrdinalNote(
              token.text,
              refs.map((x) => x.badge)
            ),
            image: '',
            bad: true,
            ref: null
          };
    }
    if (token?.kind === 'time' && token.seconds !== undefined)
      return {
        name: formatCutTime(token.seconds),
        desc: `${token.seconds.toFixed(3)}s into this ${unit}`,
        image: '',
        bad: false,
        ref: null
      };
    if (caretSection)
      return {
        name: sectionLabel(caretSection),
        desc: `Caret is in ${sectionLabel(caretSection)}${caretShot ? ` · [Shot ${caretShot}]` : ''}`,
        image: '',
        bad: false,
        ref: null
      };
    return {
      name: 'the block',
      desc: `Tap a ${subUnit} label to see what it points at`,
      image: '',
      bad: false,
      ref: null
    };
  });

  const insert = (text: string) => claim?.insert(text);

  /**
   * ⚠️ Jumping the caret is the wrapped editor's own move — it owns the CodeMirror view and exposes
   * no way in — so the chip forwards its tap to the matching chip of the editor's built-in rail,
   * which mobile.css hides. A no-op if that rail has not rendered yet, which is the frame before the
   * document parses.
   */
  function jump(name: string) {
    const chips = railEl?.parentElement?.querySelectorAll<HTMLButtonElement>('.ed-doc .pe-chip');
    if (!chips) return;
    for (const c of chips) if (c.textContent?.trim() === sectionLabel(name)) c.click();
  }
</script>

<div class="ed">
  <div class="ed-rail" bind:this={railEl}>
    {#each spans as s (s.start)}
      <button type="button" aria-current={caretSection === s.name} onclick={() => jump(s.name)}
        >{sectionLabel(s.name)}</button
      >
    {/each}
  </div>

  <div class="ed-doc">
    <H3BlockEditor
      {value}
      {docId}
      {resolvedLabels}
      placeholder="The block this {unit} renders from."
      {oninput}
      onclaim={(t) => (claim = t)}
      onsection={(name) => (caretSection = name)}
      onshot={(n) => (caretShot = n)}
      onlookup={lookup}
      ontoken={(t) => (token = t)}
    />
  </div>

  <div class="ed-dock">
    <div class="ed-ctx">
      <span class="cx-th" class:warn={ctx.bad} aria-hidden="true">
        {#if ctx.image}<img src={ctx.image} alt="" />{:else}{ctx.bad ? '⚠' : '◧'}{/if}
      </span>
      <span class="cx-tx">
        <b>{ctx.name}</b>
        <span class:warn={ctx.bad}>{ctx.desc}</span>
      </span>
      <button
        class="cx-go"
        type="button"
        aria-label="Open this reference"
        disabled={!ctx.ref}
        onclick={() => (card = ctx.ref)}>ⓘ</button
      >
    </div>

    <div class="ed-ins">
      {#each refs as r (r.id)}
        <button class="ord" type="button" onclick={() => insert(r.label)}>{r.label}</button>
      {/each}
      {#if refs.length}<span class="sep"></span>{/if}
      <button type="button" onclick={() => insert(`[Shot ${parsed.shots.length + 1}]`)}
        >[Shot N]</button
      >
      <button type="button" onclick={() => insert('At 00:00.000,')}>At 00:00.000,</button>
      <button type="button" onclick={() => insert('<scenetrans>')}>&lt;scenetrans&gt;</button>
      <button type="button" onclick={() => insert('fully_copy')}>fully_copy</button>
    </div>

    <div class="ed-foot">
      <span class:over={value.length > H3_PROMPT_MAX_CHARS}
        >{value.length.toLocaleString()} / {H3_PROMPT_MAX_CHARS.toLocaleString()}</span
      >
      <span class="grow"></span>
      {#if dead.length}
        <span class="over"
          >{dead.length} {dead.length === 1 ? 'label names' : 'labels name'} nothing</span
        >
      {/if}
      <button class="btn sm" type="button" onclick={onclose}>Done</button>
    </div>
  </div>
</div>

<MobileRefCard
  ref={card}
  body={card ? (definitions.get(card.label) ?? null) : null}
  usage={card?.scoped ? `this ${unit} only` : 'whole project'}
  onclose={() => (card = null)}
/>
