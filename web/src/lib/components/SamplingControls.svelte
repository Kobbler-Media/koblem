<script lang="ts" module>
  import type { ReasoningEffort, SamplingConfig, SamplingPreset } from '#lib/types.ts';

  /**
   * Mirror of `SamplingConfig` where every knob is explicitly nullable so the
   * UI can represent "inherit" (null) separately from an explicit numeric
   * value. Serialise to `SamplingConfig` via `toSamplingConfig`.
   */
  export interface SamplingState {
    preset: SamplingPreset | null;
    temperature: number | null;
    top_p: number | null;
    top_k: number | null;
    min_p: number | null;
    presence_penalty: number | null;
    repetition_penalty: number | null;
    max_tokens: number | null;
    /** Tri-state: `null` = inherit, `false` = thinking off, `true` = on. */
    enable_thinking: boolean | null;
    /** Thinking budget. `null` = inherit (the template's own default, `xhigh`
     * on Qwen3.8). 🔴 There is no `'none'`: that value makes the template raise
     * and llama-server answers 500 — turn thinking off with `enable_thinking`
     * instead. Inert, and not sent, while thinking is off. */
    reasoning_effort: ReasoningEffort | null;
    /** Grammar mode tag. `null` = inherit. `'off'` and `'review'` are the
     * built-in entries; any other string references a user-defined grammar
     * stored in the `grammars` table. */
    grammar: string | null;
  }

  export function emptySampling(): SamplingState {
    return {
      preset: null,
      temperature: null,
      top_p: null,
      top_k: null,
      min_p: null,
      presence_penalty: null,
      repetition_penalty: null,
      max_tokens: null,
      enable_thinking: null,
      reasoning_effort: null,
      grammar: null
    };
  }

  /** Strip nulls so the wire payload only contains fields the user set.
   * `SamplingState.preset` is the internal (short) name used across the UI;
   * the wire / settings JSON uses `sampling_preset`, so we rename on emit.
   * Likewise `SamplingState.grammar` becomes `grammar_mode` on the wire. */
  export function toSamplingConfig(state: SamplingState): SamplingConfig {
    const out: SamplingConfig = {};
    if (state.preset) out.sampling_preset = state.preset;
    if (state.temperature != null) out.temperature = state.temperature;
    if (state.top_p != null) out.top_p = state.top_p;
    if (state.top_k != null) out.top_k = state.top_k;
    if (state.min_p != null) out.min_p = state.min_p;
    if (state.presence_penalty != null) out.presence_penalty = state.presence_penalty;
    if (state.repetition_penalty != null) out.repetition_penalty = state.repetition_penalty;
    if (state.max_tokens != null) out.max_tokens = state.max_tokens;
    if (state.enable_thinking != null) out.enable_thinking = state.enable_thinking;
    // Thinking off ⇒ the budget is meaningless; drop it rather than storing noise.
    if (state.reasoning_effort != null && state.enable_thinking !== false)
      out.reasoning_effort = state.reasoning_effort;
    if (state.grammar != null) out.grammar_mode = state.grammar;
    return out;
  }

  /** Build a `SamplingState` from a persisted `SamplingConfig` (e.g. from DB). */
  export function fromSamplingConfig(cfg: SamplingConfig | undefined | null): SamplingState {
    const base = emptySampling();
    if (!cfg) return base;
    return {
      preset: cfg.sampling_preset ?? null,
      temperature: cfg.temperature ?? null,
      top_p: cfg.top_p ?? null,
      top_k: cfg.top_k ?? null,
      min_p: cfg.min_p ?? null,
      presence_penalty: cfg.presence_penalty ?? null,
      repetition_penalty: cfg.repetition_penalty ?? null,
      max_tokens: cfg.max_tokens ?? null,
      enable_thinking: cfg.enable_thinking ?? null,
      reasoning_effort: cfg.reasoning_effort ?? null,
      grammar: cfg.grammar_mode ?? null
    };
  }

  export function loadSamplingFromStorage(key: string): SamplingState {
    if (typeof localStorage === 'undefined') return emptySampling();
    const raw = localStorage.getItem(key);
    if (!raw) return emptySampling();
    try {
      return { ...emptySampling(), ...(JSON.parse(raw) as Partial<SamplingState>) };
    } catch {
      return emptySampling();
    }
  }

  export function saveSamplingToStorage(key: string, state: SamplingState): void {
    if (typeof localStorage === 'undefined') return;
    localStorage.setItem(key, JSON.stringify(state));
  }

  /** Preset recommended values — purely for the UI "apply preset" button.
   * `enable_thinking`, `reasoning_effort` and `grammar` are intentionally
   * outside the preset payload: all three are orthogonal to the sampling
   * preset, so applying one preserves whatever the user picked in the
   * thinking + effort + grammar selectors. */
  const PRESET_VALUES: Record<
    SamplingPreset,
    Omit<SamplingState, 'preset' | 'enable_thinking' | 'reasoning_effort' | 'grammar'>
  > = {
    deterministic: {
      temperature: 0.0,
      top_p: null,
      top_k: null,
      min_p: null,
      presence_penalty: null,
      repetition_penalty: null,
      max_tokens: null
    },
    qwen_instruct_general: {
      temperature: 0.7,
      top_p: 0.8,
      top_k: 20,
      min_p: 0.0,
      presence_penalty: 1.5,
      repetition_penalty: 1.0,
      max_tokens: null
    },
    qwen_instruct_reasoning: {
      temperature: 1.0,
      top_p: 0.95,
      top_k: 20,
      min_p: 0.0,
      presence_penalty: 1.5,
      repetition_penalty: 1.0,
      max_tokens: null
    }
  };

  export function applyPresetValues(state: SamplingState): SamplingState {
    if (!state.preset) return state;
    return {
      preset: state.preset,
      enable_thinking: state.enable_thinking,
      reasoning_effort: state.reasoning_effort,
      grammar: state.grammar,
      ...PRESET_VALUES[state.preset]
    };
  }
</script>

<script lang="ts">
  import { onMount } from 'svelte';
  import { listGrammars } from '#lib/api/client.ts';
  import type { GrammarEntry } from '#lib/types.ts';

  interface Props {
    value: SamplingState;
    /** Optional label for the whole block; defaults to "Sampling". */
    label?: string;
    /** When true, render as a collapsed section with a toggle. */
    collapsible?: boolean;
    /** When collapsible, show expanded initially. */
    defaultExpanded?: boolean;
  }

  let {
    value = $bindable(),
    label = 'Sampling',
    collapsible = false,
    defaultExpanded = true
  }: Props = $props();

  // Grammar dropdown is populated from /api/v1/grammars (built-ins + DB
  // entries). Fetched lazily; failures fall back to the two built-ins so
  // the selector still works offline.
  let grammars: GrammarEntry[] = $state([
    { name: 'off', gbnf: '', is_builtin: true },
    { name: 'review', gbnf: '', is_builtin: true }
  ]);
  onMount(async () => {
    try {
      grammars = await listGrammars();
    } catch {
      // keep the fallback built-ins
    }
  });

  // Track whether the collapsible block is open. We intentionally only
  // capture the *initial* value of `defaultExpanded` — after mount the
  // block's open state is owned by the user via the toggle button.
  // svelte-ignore state_referenced_locally
  let expanded = $state(defaultExpanded);

  const PRESET_OPTIONS: { id: SamplingPreset | ''; name: string }[] = [
    { id: '', name: 'Custom / Inherit' },
    { id: 'deterministic', name: 'Deterministic (temp=0)' },
    { id: 'qwen_instruct_general', name: 'Qwen Instruct General' },
    { id: 'qwen_instruct_reasoning', name: 'Qwen Instruct Reasoning' }
  ];

  function onPresetChange(e: Event) {
    const next = (e.currentTarget as HTMLSelectElement).value as SamplingPreset | '';
    value = applyPresetValues({ ...value, preset: next || null });
  }

  function clearField(k: keyof SamplingState) {
    value = { ...value, [k]: null };
  }

  // `<input type="number">` `.value` is `''` for empty AND for partial input
  // like `0.` (HTML5 number-validity strips it). Parse to `null` on empty,
  // null on NaN, else the number — matches the inherit semantic on the wire.
  function parseKnobValue(raw: string): number | null {
    if (raw === '') return null;
    const n = Number(raw);
    return Number.isFinite(n) ? n : null;
  }

  function onThinkingChange(e: Event) {
    const raw = (e.currentTarget as HTMLSelectElement).value;
    value = {
      ...value,
      enable_thinking: raw === '' ? null : raw === 'true'
    };
  }

  const THINKING_OPTIONS: { id: '' | 'false' | 'true'; name: string }[] = [
    { id: '', name: 'Inherit (default off)' },
    { id: 'false', name: 'Off' },
    { id: 'true', name: 'On' }
  ];

  function onEffortChange(e: Event) {
    const raw = (e.currentTarget as HTMLSelectElement).value;
    value = { ...value, reasoning_effort: raw === '' ? null : (raw as ReasoningEffort) };
  }

  // 🔴 No 'none' entry, ever. `chat_template_kwargs.reasoning_effort = "none"`
  // makes Qwen3.8's chat template raise and llama-server answers HTTP 500
  // (measured). "No thinking" is the Thinking selector's Off, not an effort level.
  const EFFORT_OPTIONS: { id: '' | ReasoningEffort; name: string }[] = [
    { id: '', name: 'Inherit (xhigh)' },
    { id: 'low', name: 'Low' },
    { id: 'medium', name: 'Medium' },
    { id: 'xhigh', name: 'Xhigh' }
  ];

  // Only meaningful while a trace is actually produced.
  let effortDisabled = $derived(value.enable_thinking === false);

  function onGrammarChange(e: Event) {
    const raw = (e.currentTarget as HTMLSelectElement).value;
    value = { ...value, grammar: raw === '' ? null : raw };
  }
</script>

{#snippet knob(
  fieldKey: keyof SamplingState,
  fieldLabel: string,
  step: number,
  min: number | undefined,
  max: number | undefined
)}
  <label class="knob">
    <span class="knob-label">
      {fieldLabel}
      {#if value[fieldKey] != null}
        <button
          class="knob-clear"
          type="button"
          title="Clear (inherit)"
          aria-label="Clear {fieldLabel}"
          onclick={() => clearField(fieldKey)}>×</button
        >
      {/if}
    </span>
    <input
      type="number"
      class="knob-input"
      {step}
      {min}
      {max}
      value={value[fieldKey] ?? ''}
      placeholder="inherit"
      oninput={(e) => {
        const raw = (e.currentTarget as HTMLInputElement).value;
        value = { ...value, [fieldKey]: parseKnobValue(raw) };
      }}
    />
  </label>
{/snippet}

<div class="sampling-block">
  <div class="sampling-header">
    {#if collapsible}
      <button class="sampling-toggle" type="button" onclick={() => (expanded = !expanded)}>
        <svg
          class="chevron"
          class:expanded
          viewBox="0 0 24 24"
          width="12"
          height="12"
          fill="currentColor"
        >
          <path d="M7.41 8.59L12 13.17l4.59-4.58L18 10l-6 6-6-6 1.41-1.41z"></path>
        </svg>
        <span>{label}</span>
        {#if value.preset}
          <span class="preset-tag">{value.preset}</span>
        {/if}
        {#if value.enable_thinking === true}
          <span class="preset-tag">thinking</span>
        {/if}
        {#if value.reasoning_effort && value.enable_thinking !== false}
          <span class="preset-tag">{`effort:${value.reasoning_effort}`}</span>
        {/if}
        {#if value.grammar && value.grammar !== 'off'}
          <span class="preset-tag">{`grammar:${value.grammar}`}</span>
        {/if}
      </button>
    {:else}
      <span class="sampling-label">{label}</span>
    {/if}
  </div>

  {#if !collapsible || expanded}
    <div class="sampling-body">
      <div class="top-row">
        <label class="preset-row">
          <span class="knob-label">Preset</span>
          <select class="knob-input" value={value.preset ?? ''} onchange={onPresetChange}>
            {#each PRESET_OPTIONS as opt (opt.id)}
              <option value={opt.id}>{opt.name}</option>
            {/each}
          </select>
        </label>
        <label class="preset-row">
          <span class="knob-label">Thinking</span>
          <select
            class="knob-input"
            value={value.enable_thinking == null ? '' : String(value.enable_thinking)}
            onchange={onThinkingChange}
          >
            {#each THINKING_OPTIONS as opt (opt.id)}
              <option value={opt.id}>{opt.name}</option>
            {/each}
          </select>
        </label>
        <label class="preset-row">
          <span class="knob-label">Thinking effort</span>
          <select
            class="knob-input"
            value={value.reasoning_effort ?? ''}
            disabled={effortDisabled}
            title={effortDisabled
              ? 'Thinking is off for this route — the effort budget is not sent'
              : 'How long a reasoning trace the chat template should budget for'}
            onchange={onEffortChange}
          >
            {#each EFFORT_OPTIONS as opt (opt.id)}
              <option value={opt.id}>{opt.name}</option>
            {/each}
          </select>
        </label>
        <label class="preset-row">
          <span class="knob-label">Grammar</span>
          <select class="knob-input" value={value.grammar ?? ''} onchange={onGrammarChange}>
            <option value="">Inherit</option>
            {#each grammars as g (g.name)}
              <option value={g.name}>
                {g.name}{g.is_builtin ? ' (built-in)' : ''}
              </option>
            {/each}
          </select>
        </label>
      </div>
      <div class="knobs">
        {@render knob('temperature', 'temperature', 0.05, 0, 2)}
        {@render knob('top_p', 'top_p', 0.05, 0, 1)}
        {@render knob('top_k', 'top_k', 1, 0, undefined)}
        {@render knob('min_p', 'min_p', 0.01, 0, 1)}
        {@render knob('presence_penalty', 'presence_penalty', 0.1, -2, 2)}
        {@render knob('repetition_penalty', 'repetition_penalty', 0.05, 0, 2)}
        {@render knob('max_tokens', 'max_tokens', 1, 1, undefined)}
      </div>
    </div>
  {/if}
</div>

<style>
  .sampling-block {
    border: 1px solid var(--ring);
    border-radius: 10px;
    padding: 12px 14px;
    background: rgba(0, 0, 0, 0.06);
  }
  :global([data-theme='light']) .sampling-block {
    background: rgba(0, 0, 0, 0.02);
  }

  .sampling-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .sampling-toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: none;
    color: var(--ink);
    cursor: pointer;
    padding: 0;
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .sampling-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-weight: 600;
  }

  .chevron {
    transition: transform 0.2s;
  }
  .chevron.expanded {
    transform: rotate(180deg);
  }

  .preset-tag {
    font-size: 10px;
    padding: 2px 6px;
    border-radius: 5px;
    background: var(--accent-dim);
    color: var(--accent);
    margin-left: 6px;
    text-transform: lowercase;
    letter-spacing: normal;
  }

  .sampling-body {
    margin-top: 10px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .top-row {
    display: grid;
    /* preset + thinking + effort + grammar */
    grid-template-columns: repeat(4, 1fr);
    gap: 8px 12px;
  }

  @media (max-width: 980px) {
    .top-row {
      grid-template-columns: repeat(2, 1fr);
    }
  }

  @media (max-width: 768px) {
    .top-row {
      grid-template-columns: 1fr 1fr;
    }
  }
  @media (max-width: 600px) {
    .top-row {
      grid-template-columns: 1fr;
    }
  }

  .preset-row {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .knobs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(130px, 1fr));
    gap: 8px 12px;
  }

  .knob {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .knob-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.3px;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 4px;
  }

  .knob-input {
    padding: 6px 8px;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: transparent;
    color: var(--ink);
    font-size: 13px;
    font-variant-numeric: tabular-nums;
    width: 100%;
    box-sizing: border-box;
  }

  .knob-input:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .knob-input::placeholder {
    color: var(--muted);
    opacity: 0.6;
  }

  .knob-clear {
    background: transparent;
    border: none;
    color: var(--muted);
    cursor: pointer;
    font-size: 14px;
    line-height: 1;
    padding: 0 2px;
  }

  .knob-clear:hover {
    color: var(--danger);
  }
</style>
