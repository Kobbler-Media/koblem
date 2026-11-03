<script lang="ts">
  import { checkCanvas } from '#lib/director/engine.ts';
  import type { EngineVocab } from '#lib/director/engine.ts';
  import type { H3LoraListing } from '#lib/api/client.ts';

  /**
   * **The H3 render knobs — steps, LoRAs and canvas — in ONE place for both scopes.**
   *
   * 🔴 **Why this exists.** Until 2026-08-08 these three knobs were written TWICE, at roughly 110
   * lines each: once in `SettingsSheet.svelte` as the project default and once in
   * `H3PromptComposer.svelte` as the per-shot override. Same labels, same turbo shortcut, same
   * −39%/8-steps paragraph, and — the part that actually cost something — the same `loraProblem()`
   * validator implemented twice from the same four rules. Two copies of a validator is one copy that
   * is free to drift, and the drift would be invisible: the settings sheet would keep accepting a
   * name the shot card refuses, or the reverse, and either way the author finds out at submit.
   *
   * ## The two scopes, and the one rule that separates them
   *
   * `scope` is not styling. It decides what an EMPTY field means:
   *
   * | | `project` | `shot` |
   * |---|---|---|
   * | empty LoRA list | the engine's own (none) | **inherit the project's** |
   * | empty steps | the engine's default (20) | inherit the project's, then the engine's |
   * | empty canvas | not offered — the project canvas lives with the other size settings | inherit the project's |
   *
   * ⚠️ That inheritance rule is `compileH3Timeline`'s, not this component's: **a generation's own
   * non-empty list wins; empty means inherit.** It is restated in the placeholders rather than
   * re-decided here, because a card that describes a different render from the one being submitted
   * is the exact class of bug this file keeps guarding against.
   *
   * ## What this component does NOT own
   *
   * It never writes. Every change goes out through a callback so the two callers keep their own
   * storage — `SettingsSheet` binds project state, the composer patches one `DirectorH3Generation`
   * — and neither has to pretend to be the other.
   */
  let {
    scope,
    vocab,
    loras = [],
    multipliers = [],
    steps = undefined,
    width = 0,
    height = 0,
    inheritLoras = [],
    inheritSteps = undefined,
    inheritWidth = 0,
    inheritHeight = 0,
    catalogue = null,
    onloras,
    onsteps,
    oncanvas = undefined
  }: {
    /** Whose knobs these are. See the table above — it decides what EMPTY means. */
    scope: 'project' | 'shot';
    /** What this engine calls its units, so the copy never names an engine. See `EngineVocab`. */
    vocab: EngineVocab;
    loras?: string[];
    multipliers?: number[];
    steps?: number | undefined;
    /** `shot` scope only. `0` on either side ⇒ no override, and the pair moves together. */
    width?: number;
    height?: number;
    /** `shot` scope only: what an empty field falls back to, for the placeholders. */
    inheritLoras?: string[];
    inheritSteps?: number | undefined;
    inheritWidth?: number;
    inheritHeight?: number;
    /**
     * What the engine actually has in its `minimax_h3/` LoRA directory, or `null`.
     *
     * 🔴 `null` and an empty listing are DIFFERENT and are shown differently. `null` = the engine is
     * down or predates the route, so the free-text box stands alone and a remembered name is still
     * the user's best move. An empty listing = the directory is there and holds nothing, which on
     * this service means the `/loras` volume is not mounted — and then every `loras[]` request is
     * refused however well-formed, so saying so is the whole value.
     */
    catalogue?: H3LoraListing | null;
    /** Write both lists TOGETHER — see {@link writeLoras} for why they cannot be written apart. */
    onloras: (names: string[], multipliers: number[]) => void;
    /** `undefined` = clear the override. Never `0`: zero steps is not a thing to ask an engine for. */
    onsteps: (steps: number | undefined) => void;
    /** Absent ⇒ the canvas block is not drawn at all. The project canvas lives with the sizes. */
    oncanvas?: ((width: number, height: number) => void) | undefined;
  } = $props();

  const v = $derived(vocab);
  /** `contract.py: Generation.loras` is `max_length=8`, and each adapter costs ~5%/step to carry. */
  const MAX_LORAS = 8;
  /**
   * Used ONLY when the engine has not answered `/h3/v1/loras`. When it has, the shortcut names
   * whatever turbo adapter is actually on disk — see {@link turboLora}.
   *
   * 🔴 It was a bare constant, and that is why this control could offer a LoRA the engine did not
   * have (and keep offering the superseded `h3-turbo-4step-ema-pruned.safetensors` after v4 landed
   * beside it). A hardcoded name is a second, silent copy of the engine's directory listing.
   */
  const TURBO_FALLBACK = 'h3-turbo-v4-step600-ema-pruned.safetensors';
  /** Measured 2026-08-07: 465.4 s at 8 steps with it against the 20-step control's 762.8 s. */
  const TURBO_STEPS = 8;

  const entries = $derived(catalogue?.loras ?? null);
  /** `true` only when the engine ANSWERED. Absence is unknown, and unknown accuses nobody. */
  const listKnown = $derived(Array.isArray(entries));
  const options = $derived((entries ?? []).filter((e) => e.addressable !== false));
  const knownNames = $derived(new Set(options.map((e) => e.name)));
  /** Present on disk, unusable on the wire — listed so nobody hunts for a file they can see. */
  const nested = $derived((entries ?? []).filter((e) => e.addressable === false));
  /** The directory answered and is EMPTY: every `loras[]` request will 400. Almost always a mount. */
  const dirEmpty = $derived(listKnown && options.length === 0 && nested.length === 0);
  /**
   * The turbo adapter to offer, CHOSEN FROM WHAT THE ENGINE LISTS.
   *
   * Preference order among the turbo files on disk: a `v4` build first (upstream's recommended
   * checkpoint since 2026-08-09), then any other turbo, then — only when the engine has not
   * answered at all — {@link TURBO_FALLBACK}, so the shortcut still appears while `/props` is down
   * and a name typed then is still checked once the listing arrives.
   */
  const turboLora = $derived.by(() => {
    if (!listKnown) return TURBO_FALLBACK;
    const turbos = options.map((e) => e.name).filter((n) => /turbo/i.test(n));
    return turbos.find((n) => /\bv4\b|_v4|-v4/i.test(n)) ?? turbos[0] ?? null;
  });
  const turboAvailable = $derived(turboLora !== null);
  const turboOn = $derived(loras.some((n) => n.trim() === turboLora));
  /** Engine-listed adapters not already on this render, i.e. what the picker can still add. */
  const addable = $derived(options.filter((e) => !loras.some((n) => n.trim() === e.name)));

  /**
   * One datalist id per mounted instance.
   *
   * 🔴 The settings sheet and a shot card can be on screen at the same time, and two `<datalist>`
   * elements sharing an id is a silent mis-binding — the second input completes against the first
   * list. `$props.id()` is unique per component instance, which is exactly the scope needed.
   */
  // ⚠️ `$props.id()` must be a bare variable-declaration initialiser — it cannot be interpolated
  // inline, which is a compile error rather than a lint.
  const instanceId = $props.id();
  const listId = `h3-loras-${instanceId}`;

  const inheritsLoras = $derived(scope === 'shot' && !loras.length && inheritLoras.length > 0);
  const inheritsSteps = $derived(
    scope === 'shot' &&
      (steps === undefined || steps <= 0) &&
      inheritSteps !== undefined &&
      inheritSteps > 0
  );

  /**
   * Write the two lists TOGETHER.
   *
   * 🔴 A `lora_multipliers` list LONGER than `loras` is a koblem-side error, so the UI must never be
   * able to produce one — the slice is not defensive, it is the invariant. A SHORTER list is legal
   * and pads with 1.0, so the trailing defaults are dropped rather than sent: it keeps the common
   * case (every multiplier at 1.0) off the wire entirely.
   */
  function writeLoras(names: string[], mults: number[]) {
    const m = mults.slice(0, names.length);
    while (m.length && m[m.length - 1] === 1) m.pop();
    onloras(names, m);
  }
  const multAt = (i: number) => multipliers[i] ?? 1;
  const currentMults = () => loras.map((_, j) => multAt(j));

  /**
   * Why koblem will refuse this row. Shown rather than repaired: silently rewriting a filename is
   * how you end up rendering with a LoRA the author did not name.
   *
   * 🔴 **This was the duplicated one.** It existed at `SettingsSheet.svelte:271-290` and at
   * `H3PromptComposer.svelte:817-841` with the same four rules and two different wordings for the
   * nested-directory case — one of which did not mention it at all, so the project sheet accepted a
   * name the shot card explained. One copy, both callers.
   */
  function loraProblem(name: string, i: number): string | null {
    const n = name.trim();
    if (!n)
      return 'Blank — koblem refuses an empty LoRA name. Delete the row, or type the filename.';
    if (n.includes('/') || n.includes('\\') || n.startsWith('.')) {
      return 'Bare filenames only. A path separator or a leading dot is refused outright, because it could resolve outside the mounted LoRA directory.';
    }
    if (loras.findIndex((x) => x.trim() === n) !== i) {
      return 'Listed twice. koblem refuses the duplicate rather than applying the same adapter twice.';
    }
    // 🔴 Only when the engine ANSWERED. If the listing is unknown this says nothing — accusing a
    // name of not existing because the engine is down is worse than the typo it is hunting for.
    if (listKnown && !knownNames.has(n)) {
      const hit = nested.find((e) => e.name.endsWith(`/${n}`) || e.filename === n);
      if (hit) {
        return `On the engine at ${hit.name}, but inside a subdirectory — a LoRA name may not contain a path separator, so it cannot be requested. Move it to the top of the LoRA directory.`;
      }
      return dirEmpty
        ? 'The engine’s LoRA directory is empty, so this is refused at submit whatever it is called — see the note below.'
        : 'Not on the engine. The name is the only identifier there is, so this is refused at submit; pick one from the list.';
    }
    return null;
  }

  /** The ×32 grid is model law here — reported, never snapped. See `checkCanvas` in `./engine`. */
  const canvasProblem = $derived(width && height ? checkCanvas('h3', width, height).error : null);
  function setCanvas(w: number, h: number) {
    const ww = Number.isFinite(w) && w > 0 ? Math.round(w) : 0;
    const hh = Number.isFinite(h) && h > 0 ? Math.round(h) : 0;
    // 🔴 The wire takes them as a PAIR (`Generation.width` / `.height`, both optional) and koblem
    // refuses a half-set override rather than filling the other side in — which is right, because
    // inventing the missing side would silently change the aspect ratio of a render the author only
    // meant to make wider.
    oncanvas?.(ww && hh ? ww : 0, ww && hh ? hh : 0);
  }
</script>

<!--
  🔴 Steps and LoRAs are ONE decision and are drawn together for that reason. The turbo LoRA does not
  lower `steps` and is not free — it costs ~5%/step to carry — so loading it and leaving the count at
  20 buys nothing at all. 8 steps WITH it against 20 without is −39% wall at the SAME 6528 MiB peak
  (measured 2026-08-07). A UI that let you set one without seeing the other is how you collect the
  tax and not the win.
-->
<div class="knobs">
  {#if inheritsSteps || inheritsLoras}
    <p class="hint" style="margin:0 0 6px">
      Following the project default — {inheritsSteps
        ? `${inheritSteps} steps`
        : 'the project step count'}{inheritsLoras ? `, ${inheritLoras.join(', ')}` : ''}. Set
      anything below and this {v.renderUnit} stops following it.
    </p>
  {/if}

  <label class="field" style="margin:0"
    ><span>Steps</span>
    <input
      class="inp"
      type="number"
      min="1"
      max="60"
      step="1"
      placeholder={inheritsSteps ? `${inheritSteps} (project)` : '20 (engine default)'}
      value={steps ?? ''}
      oninput={(e) => {
        // Empty means ABSENT, not zero: the fallback is the project's, then the engine's own, and
        // there is no local default to invent. Anything unparseable clears it for the same reason.
        const n = Math.trunc(Number(e.currentTarget.value));
        onsteps(e.currentTarget.value === '' || !Number.isFinite(n) || n <= 0 ? undefined : n);
      }}
    />
  </label>
  <!-- The measured pair, said once. It used to be said twice, at length, in two files. -->
  <p class="hint" style="margin-top:5px">
    Blank leaves it to the {scope === 'shot' ? 'project, then to the engine' : 'engine'} (20).
    <b>8</b> is the number the turbo LoRA is for — <b>−39% wall at the same VRAM</b>. Without a
    turbo LoRA attached, 8 steps renders under-converged; with one, leaving this at 20 buys nothing
    but a ~5%/step tax.
  </p>

  {#if turboAvailable && !turboOn && loras.length < MAX_LORAS}
    <!-- A shortcut, not the picker — the datalist below is that. It survives the listing because it
         carries the STEP PAIRING, which no directory scan knows, and that is the half people forget. -->
    <button
      class="btn sm"
      style="margin-top:7px"
      onclick={() => {
        if (!turboLora) return;
        writeLoras([...loras, turboLora], [...currentMults(), 1]);
        onsteps(TURBO_STEPS);
      }}>＋ turbo LoRA at {TURBO_STEPS} steps</button
    >
  {/if}

  <!-- 🔴 The ENGINE'S OWN LIST, as a control rather than as autocomplete. The only way to add an
       adapter used to be the turbo shortcut (one hardcoded name) or typing into the text box below
       and knowing that a `<datalist>` was attached to it — so a directory the engine had already
       reported was, in practice, invisible. The text box stays for a file copied in a moment ago
       that the listing has not caught up with; this is for the ones it HAS reported. -->
  {#if addable.length && loras.length < MAX_LORAS}
    <label class="inline-row" style="gap:6px;margin-top:7px">
      <span class="tl-meta">On the engine</span>
      <select
        class="inp"
        value=""
        onchange={(e) => {
          const name = e.currentTarget.value;
          e.currentTarget.value = '';
          if (!name) return;
          writeLoras([...loras, name], [...currentMults(), 1]);
        }}
      >
        <option value="">Add a LoRA…</option>
        {#each addable as e (e.name)}<option value={e.name}>{e.name}</option>{/each}
      </select>
    </label>
  {/if}

  <!-- Keyed by INDEX and not by value: keying on the text destroys and recreates the input on every
       keystroke, which loses focus after one character. -->
  {#each loras as name, i (i)}
    {@const problem = loraProblem(name, i)}
    <div class="row">
      <!-- `list=` and not a <select>: completion when the engine has answered, and a name it has not
           listed still typeable, because the listing can lag a file copied in a moment ago. -->
      <input
        class="inp"
        list={listId}
        value={name}
        placeholder={turboLora ?? TURBO_FALLBACK}
        oninput={(e) =>
          writeLoras(
            loras.map((x, j) => (j === i ? e.currentTarget.value : x)),
            currentMults()
          )}
      />
      <input
        class="inp"
        type="number"
        min="0"
        max="2"
        step="0.05"
        style="flex:none;width:78px"
        title="Multiplier. Positionally matched to the list; 1.0 is the default and is not sent."
        value={multAt(i)}
        oninput={(e) => {
          const n = parseFloat(e.currentTarget.value);
          writeLoras(
            loras,
            loras.map((_, j) => (j === i ? (Number.isFinite(n) ? n : 1) : multAt(j)))
          );
        }}
      />
      <button
        class="btn sm danger"
        onclick={() =>
          writeLoras(
            loras.filter((_, j) => j !== i),
            currentMults().filter((_, j) => j !== i)
          )}>✕</button
      >
    </div>
    {#if problem}<p class="hint err" style="margin:-2px 0 6px">{problem}</p>{/if}
  {/each}
  <datalist id={listId}>
    {#each options as e (e.name)}<option value={e.name}></option>{/each}
  </datalist>

  <div class="inline-row" style="gap:6px;flex-wrap:wrap;margin-top:4px">
    <button
      class="btn sm"
      disabled={loras.length >= MAX_LORAS}
      title={loras.length >= MAX_LORAS
        ? `The engine takes at most ${MAX_LORAS} LoRAs per ${v.renderUnit}`
        : ''}
      onclick={() => writeLoras([...loras, ''], [...currentMults(), 1])}>＋ LoRA</button
    >
  </div>

  {#if dirEmpty}
    <p class="hint err" style="margin-top:6px">
      ⚠ The engine's LoRA directory{catalogue?.dir ? ` (${catalogue.dir})` : ''} is <b>empty</b>, so
      any LoRA here is refused at submit however it is spelled — this is normally a missing
      <code>/loras</code> mount rather than a missing file.{catalogue?.warning
        ? ` ${catalogue.warning}`
        : ''}
    </p>
  {:else if listKnown}
    <p class="hint" style="margin-top:6px">
      {options.length} on the engine{catalogue?.dir ? ` in ${catalogue.dir}` : ''} — start typing to complete
      one. The name is the only identifier there is, and each extra adapter costs about
      <b>5% per step</b> (the COUNT, not the rank — a rank-8 and a rank-128 LoRA cost the same).
      {#if nested.length}
        {nested.length} more {nested.length === 1 ? 'sits' : 'sit'} in a subdirectory and cannot be named
        on the wire ({nested.map((e) => e.name).join(', ')}); move them up a level to use them.
      {/if}
    </p>
  {/if}

  <details style="margin-top:6px">
    <summary class="tl-meta">Why <code>(N/N) applied</code> in the log proves nothing</summary>
    <p class="hint err" style="margin:6px 0 0">
      🔴 It proves the FILE LOADED and nothing more — not that an adapter attached to a single
      tensor. A LoRA that fails to attach renders 20-step quality in 8 steps' time, so it presents
      as
      <em>broken</em> rather than as fast. The only check that means anything is that the output actually
      CHANGED.
    </p>
  </details>

  {#if oncanvas}
    <!-- ── This shot's canvas ──────────────────────────────────────────────────────────────
         🔴 **Not a quality knob, and the copy has to say so.** H3's initial noise is
         `randn((1, 24, latent_t, latent_h, latent_w), manual_seed(seed))`, so a different width or
         height is a different latent SHAPE and therefore a different DRAW at the same seed —
         composition, camera path, subject placement, and (the audio noise comes off the same
         generator immediately afterwards) the music bed. Re-sizing a shot you liked does not give
         you that shot, larger; it gives you a different shot. -->
    <div class="lbl-row" style="margin-top:11px">
      <span>This {v.renderUnit}'s canvas</span><span class="tl-meta"
        >{width && height ? `${width}×${height}` : 'the project size'}</span
      >
    </div>
    <div class="inline-row" style="gap:6px;flex-wrap:wrap">
      <input
        class="inp"
        type="number"
        min="0"
        step="32"
        style="flex:none;width:92px"
        placeholder={inheritWidth ? String(inheritWidth) : 'width'}
        value={width || ''}
        oninput={(e) => setCanvas(parseInt(e.currentTarget.value, 10), height)}
      />
      <span class="tl-meta">×</span>
      <input
        class="inp"
        type="number"
        min="0"
        step="32"
        style="flex:none;width:92px"
        placeholder={inheritHeight ? String(inheritHeight) : 'height'}
        value={height || ''}
        oninput={(e) => setCanvas(width, parseInt(e.currentTarget.value, 10))}
      />
      {#if width || height}
        <button class="btn sm" onclick={() => oncanvas?.(0, 0)}>Use the project size</button>
      {/if}
    </div>
    {#if canvasProblem}
      <p class="hint err" style="margin-top:6px">{canvasProblem}</p>
    {:else if width && height}
      <p class="hint" style="margin-top:6px">
        ⚠ This is a <b>different render</b>, not the same one resized. The seed draws noise at the
        latent shape, so changing either side re-rolls the composition, the camera path and the
        music bed — at the same seed. Nothing survives but the prompt.
      </p>
    {/if}
  {/if}
</div>

<style>
  .knobs {
    display: block;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 6px 0;
  }
  .row .inp {
    flex: 1;
    min-width: 0;
  }
</style>
