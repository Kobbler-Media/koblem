<script lang="ts">
  // Chip-based songgen description builder. Two-way binds the comma-separated
  // `value` string songgen wants (canonical order: gender, genre, emotion,
  // instrument, then any open-vocab "other"). Backed by the model's predefined
  // vocabulary; ~12 common chips per dimension with "+ more" for the full list
  // and free custom (open-vocab) entry.
  import {
    SONGGEN_VOCAB,
    SONGGEN_COMMON,
    SONGGEN_DIMS,
    type SonggenDim
  } from '#lib/data/songgen-vocab.ts';

  let { value = $bindable('') }: { value?: string } = $props();

  const SETS: Record<SonggenDim, Set<string>> = {
    gender: new Set(SONGGEN_VOCAB.gender),
    genre: new Set(SONGGEN_VOCAB.genre),
    emotion: new Set(SONGGEN_VOCAB.emotion),
    instrument: new Set(SONGGEN_VOCAB.instrument)
  };
  const LABEL: Record<SonggenDim, string> = {
    gender: 'Gender',
    genre: 'Genre',
    emotion: 'Emotion',
    instrument: 'Instrument'
  };

  let sel = $state<Record<SonggenDim, string[]>>({
    gender: [],
    genre: [],
    emotion: [],
    instrument: []
  });
  let other = $state<string[]>([]); // open-vocab tags not in any list

  function classify(tag: string): SonggenDim | 'other' {
    for (const d of SONGGEN_DIMS) if (SETS[d].has(tag)) return d;
    return 'other';
  }

  function assemble(): string {
    return [...sel.gender, ...sel.genre, ...sel.emotion, ...sel.instrument, ...other].join(', ');
  }

  function parse(str: string) {
    const tags = str
      .split(',')
      .map((t) => t.trim().toLowerCase())
      .filter(Boolean);
    const next: Record<SonggenDim, string[]> = {
      gender: [],
      genre: [],
      emotion: [],
      instrument: []
    };
    const oth: string[] = [];
    for (const t of tags) {
      const d = classify(t);
      if (d === 'other') {
        if (!oth.includes(t)) oth.push(t);
      } else if (!next[d].includes(t)) next[d].push(t);
    }
    sel = next;
    other = oth;
  }

  // Keep internal selection in sync with external `value` changes (e.g. the LLM
  // "Expand" pre-fill or the free-form fallback), without echoing our own emits.
  let lastSynced = '';
  $effect(() => {
    if (value !== lastSynced) {
      parse(value);
      lastSynced = value;
    }
  });

  function emit() {
    const s = assemble();
    lastSynced = s;
    value = s;
  }

  function toggle(d: SonggenDim, tag: string) {
    sel[d] = sel[d].includes(tag) ? sel[d].filter((x) => x !== tag) : [...sel[d], tag];
    emit();
  }
  function removeOther(tag: string) {
    other = other.filter((x) => x !== tag);
    emit();
  }
  function addCustom(d: SonggenDim, raw: string) {
    const t = raw.trim().toLowerCase();
    if (!t) return;
    const cls = classify(t);
    if (cls === 'other') {
      if (!other.includes(t)) other = [...other, t];
    } else if (!sel[cls].includes(t)) {
      sel[cls] = [...sel[cls], t];
    }
    emit();
  }

  // The chips shown for a dimension = its common quick-picks ∪ anything selected
  // that isn't already common (so custom/searched picks stay visible).
  function chipsFor(d: SonggenDim): string[] {
    const common = SONGGEN_COMMON[d];
    const extra = sel[d].filter((t) => !common.includes(t));
    return [...common, ...extra];
  }
  const hasMore = (d: SonggenDim) => SONGGEN_VOCAB[d].length > SONGGEN_COMMON[d].length;

  let openMore = $state<SonggenDim | null>(null);
  let search = $state('');
  let customInput = $state('');
  const filtered = $derived.by(() => {
    if (!openMore) return [];
    const q = search.trim().toLowerCase();
    const all = SONGGEN_VOCAB[openMore];
    return q ? all.filter((t) => t.includes(q)) : all;
  });

  function openPanel(d: SonggenDim) {
    openMore = openMore === d ? null : d;
    search = '';
    customInput = '';
  }
  function customKey(e: KeyboardEvent, d: SonggenDim) {
    if (e.key === 'Enter') {
      e.preventDefault();
      addCustom(d, customInput);
      customInput = '';
    }
  }
</script>

<div class="desc-chips">
  {#each SONGGEN_DIMS as d (d)}
    <div class="group">
      <span class="glabel">{LABEL[d]}</span>
      <div class="chips">
        {#each chipsFor(d) as tag (tag)}
          <button
            type="button"
            class="chip"
            class:active={sel[d].includes(tag)}
            onclick={() => toggle(d, tag)}>{tag}</button
          >
        {/each}
        {#if hasMore(d)}
          <button
            type="button"
            class="chip more"
            class:open={openMore === d}
            onclick={() => openPanel(d)}>{openMore === d ? '× close' : '+ more'}</button
          >
        {/if}
      </div>
      {#if openMore === d}
        <div class="more-panel">
          <input
            type="text"
            class="search"
            placeholder={`search ${SONGGEN_VOCAB[d].length} ${d} tags…`}
            bind:value={search}
          />
          <div class="more-list">
            {#each filtered as tag (tag)}
              <button
                type="button"
                class="chip"
                class:active={sel[d].includes(tag)}
                onclick={() => toggle(d, tag)}>{tag}</button
              >
            {/each}
            {#if filtered.length === 0}<span class="hint">no match — add it as custom below</span
              >{/if}
          </div>
          <input
            type="text"
            class="custom"
            placeholder={`add a custom ${d} tag (open vocab) + Enter`}
            bind:value={customInput}
            onkeydown={(e) => customKey(e, d)}
          />
        </div>
      {/if}
    </div>
  {/each}

  {#if other.length}
    <div class="group">
      <span class="glabel">Other <span class="hint">(open-vocab / off-list)</span></span>
      <div class="chips">
        {#each other as tag (tag)}
          <button type="button" class="chip active" onclick={() => removeOther(tag)}>{tag} ×</button
          >
        {/each}
      </div>
    </div>
  {/if}

  <div class="preview">
    <span class="hint">songgen sees:</span>
    {#if value.trim()}<code>{value}</code>{:else}<span class="hint">— nothing selected yet —</span
      >{/if}
  </div>
</div>

<style>
  .desc-chips {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 6px;
  }
  .group {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .glabel {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .chip {
    width: auto;
    padding: 4px 10px;
    font-size: 12px;
    border-radius: 999px;
    border: 1px solid var(--ring);
    background: var(--bg-2);
    color: var(--ink);
    cursor: pointer;
    line-height: 1.4;
    transition:
      background 0.12s,
      border-color 0.12s;
  }
  .chip:hover {
    border-color: var(--accent);
  }
  .chip.active {
    background: var(--accent-dim);
    border-color: var(--accent);
    color: var(--accent);
    font-weight: 600;
  }
  .chip.more {
    color: var(--muted);
    border-style: dashed;
  }
  .chip.more.open {
    color: var(--ink);
  }
  .more-panel {
    border: 1px solid var(--ring);
    border-radius: 10px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    background: var(--bg-2);
  }
  .more-list {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    max-height: 180px;
    overflow-y: auto;
  }
  .search,
  .custom {
    font-size: 12px;
  }
  .preview {
    margin-top: 2px;
    font-size: 12px;
    display: flex;
    gap: 6px;
    align-items: baseline;
    flex-wrap: wrap;
  }
  .preview code {
    color: var(--ink);
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    padding: 3px 8px;
    font-size: 12px;
    word-break: break-word;
  }
  .hint {
    color: var(--muted);
    font-weight: normal;
    font-size: 11px;
  }
</style>
