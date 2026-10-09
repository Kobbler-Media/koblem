<script lang="ts">
  import {
    parseH3Block,
    replaceH3Section,
    sectionLabel,
    type H3SectionName
  } from '$lib/director/h3-block';
  import { h3SectionSpans } from '$lib/director/h3-highlight';

  // One section at a time, with a way through. The desktop's lens rail becomes a `<select>` plus
  // ‹ ›, and tapping a header in the composed prompt lands here with that one already loaded.
  let {
    value,
    section = $bindable(null),
    unit,
    onprompt,
    onrework
  }: {
    /** The whole block. Edits here are a surgical rewrite of one section's bytes, never a recompose. */
    value: string;
    /** Which section is loaded. Bindable so a tap on a header in the prompt view can set it. */
    section?: string | null;
    unit: string;
    onprompt: (next: string) => void;
    /** Ask for a change to THIS section by name, through the same rework the prompt view uses. */
    onrework: (section: string) => void;
  } = $props();

  const spans = $derived(h3SectionSpans(parseH3Block(value)));
  const names = $derived(spans.map((s) => s.name));
  const index = $derived(section ? names.indexOf(section as H3SectionName) : -1);
  const current = $derived(index >= 0 ? names[index] : (names[0] ?? null));
  const body = $derived.by(() => {
    const s = spans.find((x) => x.name === current);
    return s ? value.slice(s.bodyStart, s.end) : '';
  });

  function step(delta: number) {
    const at = current ? names.indexOf(current as H3SectionName) : -1;
    const next = names[Math.min(names.length - 1, Math.max(0, at + delta))];
    if (next) section = next;
  }

  function write(next: string) {
    if (!current) return;
    onprompt(replaceH3Section(value, current as H3SectionName, next));
  }
</script>

{#if names.length === 0}
  <p class="hint">
    This {unit} has no block yet, so there are no sections to walk. Draft it on the Prompt view first.
  </p>
{:else}
  <div class="secnav">
    <button
      type="button"
      aria-label="Previous section"
      disabled={(current ? names.indexOf(current as H3SectionName) : 0) <= 0}
      onclick={() => step(-1)}>‹</button
    >
    <select
      class="inp"
      aria-label="Section"
      value={current}
      onchange={(e) => (section = e.currentTarget.value)}
    >
      {#each names as n (n)}
        <option value={n}>{sectionLabel(n)}</option>
      {/each}
    </select>
    <button
      type="button"
      aria-label="Next section"
      disabled={(current ? names.indexOf(current as H3SectionName) : 0) >= names.length - 1}
      onclick={() => step(1)}>›</button
    >
  </div>

  <textarea
    class="inp mono"
    rows="11"
    aria-label={current ? sectionLabel(current) : 'section'}
    value={body}
    oninput={(e) => write(e.currentTarget.value)}></textarea>

  <div class="seccount">
    <span>{(current ? names.indexOf(current as H3SectionName) : 0) + 1} of {names.length}</span>
    <span>{body.length.toLocaleString()} chars</span>
  </div>

  <div class="aibar">
    <button class="btn sm" type="button" onclick={() => current && onrework(current)}
      >⌁ Rework this section…</button
    >
  </div>
  <!-- 🔴 There is no section-scoped draft on the wire: the LLM entry points rewrite the whole block.
       So the one AI action here is a rework whose INSTRUCTION names the section, which is the only
       honest way to say "change this and leave the rest". -->
  <p class="hint" style="margin-top:8px">
    Typing here touches only <b>{current ? sectionLabel(current) : ''}</b>. Nothing else in the
    block changes.
  </p>
{/if}
