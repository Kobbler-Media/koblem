<script lang="ts">
  import { untrack } from 'svelte';
  import { uid } from '@koblibs/ui';
  import type { DirectorClip } from '#lib/api/client.ts';
  import { DEFAULT_VOCAB, type EngineVocab } from '#lib/director/engine.ts';

  interface Props {
    summary?: string | null;
    clips: DirectorClip[];
    defaultSeconds: number;
    /**
     * What this engine calls its units. The planner emits the pieces the timeline is made of, which
     * are CUTS wherever a render holds several — see `EngineVocab`.
     */
    vocab?: EngineVocab;
    onGo: (clips: DirectorClip[]) => void;
    onCancel: () => void;
  }

  let { summary, clips, defaultSeconds, vocab = DEFAULT_VOCAB, onGo, onCancel }: Props = $props();
  const unit = $derived(vocab.subUnit ?? vocab.renderUnit);
  const units = $derived(vocab.subUnitPlural ?? vocab.renderUnitPlural);

  // The sheet edits its OWN copy of the plan — the timeline is untouched until Go, and Cancel
  // simply drops this. Deliberately a one-time capture of the incoming plan.
  let beats = $state<DirectorClip[]>(
    untrack(() => clips.map((c) => ({ ...$state.snapshot(c) }) as DirectorClip))
  );

  const total = $derived(beats.reduce((a, c) => a + c.seconds, 0));

  function move(i: number, delta: number) {
    const j = i + delta;
    if (j < 0 || j >= beats.length) return;
    const [c] = beats.splice(i, 1);
    beats.splice(j, 0, c);
  }

  function remove(i: number) {
    beats.splice(i, 1);
  }

  function add(at: number) {
    beats.splice(at, 0, {
      id: uid(),
      idea: '',
      prompt: '',
      mode: 't2v',
      seconds: defaultSeconds,
      keyframes: []
    });
  }
</script>

<div
  class="overlay"
  role="presentation"
  onclick={(e) => e.target === e.currentTarget && onCancel()}
>
  <div class="modal">
    <div class="chat-h">
      <div class="chat-badge">🎬</div>
      <div style="flex:1">
        <b>Review the plan</b>
        <small
          >{beats.length}
          {beats.length === 1 ? unit : units} · {total.toFixed(1)}s — nothing is on the timeline yet</small
        >
      </div>
      <button class="btn icon-only ghost" onclick={onCancel}>✕</button>
    </div>

    <div class="plan-list">
      {#if summary}
        <div class="callout"><b>The story</b><br />{summary}</div>
      {/if}

      {#if beats.length === 0}
        <p class="hint">No {units} left — add one, or cancel.</p>
      {/if}

      {#each beats as b, i (b.id)}
        <div class="plan-card">
          <div class="top">
            <span class="pno">{unit.toUpperCase()} {String(i + 1).padStart(2, '0')}</span>
            {#if b.new_scene && i > 0}
              <span class="sev warn">hard cut</span>
            {:else if i > 0}
              <span class="sev info">continues</span>
            {/if}
            <span class="grow"></span>
            <span class="tl-meta">{b.seconds.toFixed(1)}s</span>
            <button
              class="btn icon-only ghost"
              title="Move up"
              disabled={i === 0}
              onclick={() => move(i, -1)}>↑</button
            >
            <button
              class="btn icon-only ghost"
              title="Move down"
              disabled={i === beats.length - 1}
              onclick={() => move(i, 1)}>↓</button
            >
            <button class="btn icon-only ghost" title="Remove {unit}" onclick={() => remove(i)}
              >✕</button
            >
          </div>
          {#if b.gist?.trim()}
            <div class="hint">{b.gist}</div>
          {/if}
          <!-- An H3 plan entry is the render's whole shooting script, which is what the writer
               expands — so it gets room to be read and edited, not a two-line box. -->
          <textarea
            class="inp"
            rows={b.idea && b.idea.length > 160 ? 10 : 2}
            placeholder="what happens in this {unit}…"
            bind:value={b.idea}></textarea>
          <div class="plan-row">
            <label
              >Length
              <input
                class="inp"
                type="number"
                min="1"
                max="20"
                step="0.5"
                value={b.seconds}
                oninput={(e) => {
                  const v = parseFloat(e.currentTarget.value);
                  if (!Number.isNaN(v) && v > 0) b.seconds = v;
                }}
              />
            </label>
            {#if i > 0}
              <label class="chk">
                <input
                  type="checkbox"
                  checked={b.new_scene === true}
                  onchange={(e) => (b.new_scene = e.currentTarget.checked)}
                />
                Hard cut
              </label>
            {/if}
            <span class="grow"></span>
            <button class="btn sm ghost" onclick={() => add(i + 1)}>+ Add {unit} below</button>
          </div>
        </div>
      {/each}

      <button class="btn sm ghost plan-add" onclick={() => add(beats.length)}>+ Add {unit}</button>
    </div>

    <div class="plan-foot">
      <span class="hint">Go commits these beats, then drafts every prompt in the background.</span>
      <button class="btn ghost" onclick={onCancel}>Cancel</button>
      <button
        class="btn primary"
        disabled={beats.length === 0}
        onclick={() => onGo($state.snapshot(beats) as DirectorClip[])}
        >Go — draft {beats.length} {beats.length === 1 ? unit : units}</button
      >
    </div>
  </div>
</div>
