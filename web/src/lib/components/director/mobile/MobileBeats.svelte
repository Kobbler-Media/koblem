<script lang="ts">
  import { parseH3Block, sectionLabel } from '$lib/director/h3-block';
  import { h3MainSectionFor, h3StripCutVerb, writeH3MainSection } from '$lib/director/h3-document';
  import { H3_CUT_VERBS, formatCutTime, h3ShotIsCut } from '$lib/director/h3-prompt';
  import { MOBILE_BEAT_GAP, nextBeatSeconds } from '$lib/director/mobile';

  /**
   * The beats of one render unit, as a list — because without a time axis to drag on, that is what
   * they are. A row still answers the question the ribbon answers on a desktop: when it lands, what
   * happens, and whether it CUTS.
   *
   * 🔴 A `[Shot N]` with no transition verb produces no cut at all — measured at a 0.71 max scene
   * score over 362 frames, where a hard cut scores above 10. The verb is the whole difference, so the
   * row says which it is and underlines the verb that made it one.
   */
  let {
    value,
    seconds,
    subUnit,
    frameFor = undefined,
    onprompt
  }: {
    value: string;
    /** This render unit's own length — what a new beat's time is clamped into. */
    seconds: number;
    subUnit: string;
    /** A still from the rendered take at this beat's time, when one could be had. */
    frameFor?: ((index: number, atSeconds: number) => string | undefined) | undefined;
    onprompt: (next: string) => void;
  } = $props();

  const parsed = $derived(parseH3Block(value));
  const rows = $derived(parsed.shots);
  const task = $derived(parsed.task ?? 'ref2va');
  const styleOpening = $derived(parsed.styleOpening);

  let open = $state<number | null>(null);

  const RIBBON_CELLS = 4;

  /** Two words at most in the picker; the sentence the verb actually writes is in the hint below. */
  const VERB_LABEL: Record<string, string> = {
    'the camera cuts to': 'Camera cut',
    'the shot cuts to': 'Cut',
    'the shot transitions to': 'Transition',
    'the shot changes to': 'Change',
    'the shot switches to': 'Switch'
  };

  /** Rewrite the main section from the rows, which is the only path a beat edit takes. */
  function write(next: typeof rows) {
    onprompt(
      writeH3MainSection(
        value,
        task,
        next.map((r) => ({
          index: r.index,
          atSeconds: r.atSeconds,
          body: r.body,
          ...(r.cutVerb ? { cutVerb: r.cutVerb } : {}),
          ...(r.scenetrans ? { scenetrans: true } : {})
        })),
        styleOpening
      )
    );
  }

  const patch = (i: number, next: Partial<(typeof rows)[number]>) =>
    write(rows.map((r, k) => (k === i ? { ...r, ...next } : r)));

  function setVerb(i: number, verb: string) {
    const row = rows[i];
    // Strip whatever verb the body already opens with, or the prose reads "the shot cuts to the
    // shot cross-dissolves to …" the second time this is touched.
    const body = h3StripCutVerb(row.body);
    patch(i, verb ? { cutVerb: verb, body } : { cutVerb: undefined, body });
  }

  const nextAt = $derived(
    nextBeatSeconds(
      rows.map((r) => r.atSeconds ?? 0),
      seconds
    )
  );

  function addBeat() {
    if (nextAt === null) return;
    write([...rows, { index: rows.length, atSeconds: nextAt, body: '' } as (typeof rows)[number]]);
    open = rows.length;
  }

  function removeBeat(i: number) {
    // Index 0 opens the render and is untimed — dropping it would retime everything after it.
    if (i === 0) return;
    write(rows.filter((_, k) => k !== i).map((r, k) => ({ ...r, index: k })));
    open = null;
  }

  const mainSection = $derived(h3MainSectionFor(task));
</script>

<p class="eye">Beats in this {subUnit === 'shot' ? 'render' : subUnit}</p>

{#if rows.length === 0}
  <p class="hint">
    Nothing in <b>{sectionLabel(mainSection)}</b> yet. Draft the prompt and the beats it writes appear
    here.
  </p>
{/if}

{#each rows as row, i (i)}
  {@const cut = h3ShotIsCut(row)}
  {@const at = i === 0 ? 0 : (row.atSeconds ?? 0)}
  <button class="beat" type="button" onclick={() => (open = open === i ? null : i)}>
    <span class="tc">{formatCutTime(at)}</span>
    <span class="bd">
      <span class="pill" class:cut><i></i>{cut ? `Cut · ${row.cutVerb}` : 'Beat'}</span>
      <span class="tx">
        {#if cut && row.cutVerb}<u>{row.cutVerb}</u>&nbsp;{/if}{h3StripCutVerb(row.body) ||
          'Nothing written for this beat yet.'}
      </span>
      {#if frameFor}
        {@const strip = Array.from({ length: RIBBON_CELLS }, (_, k) =>
          frameFor(i, at + (k / RIBBON_CELLS) * 0.8)
        )}
        <!-- Four frames wide is enough to answer "did that beat land where I wrote it". Drawn only
             when the take's stills are already in the cache — four empty boxes answer nothing. -->
        {#if strip.some(Boolean)}
          <span class="ribbon" aria-hidden="true">
            {#each strip as src, k (k)}
              <span
                >{#if src}<img {src} alt="" />{/if}</span
              >
            {/each}
          </span>
        {/if}
      {/if}
    </span>
    <span class="gr" aria-hidden="true">{open === i ? '⌄' : '›'}</span>
  </button>

  {#if open === i}
    <div class="grp">
      <div class="field">
        <span>What happens</span>
        <textarea
          class="inp"
          rows="3"
          value={h3StripCutVerb(row.body)}
          oninput={(e) => patch(i, { body: e.currentTarget.value })}></textarea>
      </div>
      {#if i > 0}
        <div class="field">
          <span>Lands at</span>
          <input
            class="inp"
            inputmode="decimal"
            value={(row.atSeconds ?? 0).toFixed(3)}
            onchange={(e) => {
              const v = Number(e.currentTarget.value);
              // Strictly INSIDE the unit: a beat at exactly `seconds` has no time to play, which
              // the render-time validator refuses in the same breath as a non-increasing one.
              if (Number.isFinite(v))
                patch(i, {
                  atSeconds: Math.max(0, Math.min(seconds - MOBILE_BEAT_GAP, v))
                });
            }}
          />
        </div>
        <div class="field">
          <span>Transition</span>
          <select
            class="inp"
            value={row.cutVerb ?? ''}
            onchange={(e) => setVerb(i, e.currentTarget.value)}
          >
            <option value="">Beat</option>
            {#each H3_CUT_VERBS as verb (verb)}
              <option value={verb}>{VERB_LABEL[verb] ?? 'Cut'}</option>
            {/each}
          </select>
        </div>
        <p class="hint">
          A beat keeps the camera rolling and lands the action at the stated time. Anything else
          writes a transition verb — <b>{row.cutVerb ?? 'the shot cuts to'}</b> — in front of the body,
          and that verb is the only thing that makes it a cut.
        </p>
        <button class="btn full danger" type="button" onclick={() => removeBeat(i)}
          >✕ Remove this beat</button
        >
      {:else}
        <p class="hint">
          The first beat opens the render, so it carries no time and nothing to cut from.
        </p>
      {/if}
    </div>
  {/if}
{/each}

<div class="addrow">
  <button class="btn full" type="button" disabled={nextAt === null} onclick={addBeat}
    >＋ Add beat</button
  >
</div>
{#if nextAt === null}
  <p class="hint warn">
    No room for another beat: cut times have to strictly increase and land before this {subUnit ===
    'shot'
      ? 'render'
      : subUnit} ends at {seconds.toFixed(2)}s. Move the last beat earlier, or make this one longer.
  </p>
{:else if rows.length}
  <p class="hint">The next one would land at {formatCutTime(nextAt)}.</p>
{/if}
