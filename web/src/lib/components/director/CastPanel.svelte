<script lang="ts">
  import { assetSrc } from '#lib/director/assets.svelte.ts';
  import {
    MULTI_CHARACTER_CAVEAT,
    REFERENCE_ROLES,
    REFERENCE_ROLE_HINT,
    REFERENCE_ROLE_LABEL,
    characterRole,
    referenceSpec
  } from '#lib/director/characters.ts';
  import { snap32 } from '#lib/director/resolution.ts';
  import type {
    DirectorCharacter,
    DirectorReferenceRole,
    LtxRuntimeLora
  } from '#lib/api/client.ts';

  // Character sheets and the identity tags they carry. Project-level, NOT per shot: LTX keys
  // identity off `source_id`, so a character has to present the same tag in every shot it appears
  // in. Which shot each one is IN is edited in the shot panel.
  let {
    characters = $bindable(),
    model,
    loras,
    width,
    height,
    identityRegime,
    identityNote,
    roleNote,
    referencePreparing,
    referencePrepError,
    hasStartFrame,
    introducedByShotNo,
    onchange,
    onadd,
    onremove,
    onsheet,
    onstartframe
  }: {
    characters: DirectorCharacter[];
    model: string | undefined;
    loras: LtxRuntimeLora[];
    width: number;
    height: number;
    identityRegime: string;
    /** "This will not do what you think", or null when the setup is sound. */
    identityNote: string | null;
    roleNote: string | null;
    /** Character id whose reference is being prepared, so its row can say so. */
    referencePreparing: string | null;
    referencePrepError: string | null;
    /** Whether the inspected shot has an i2v start frame to point a character at. */
    hasStartFrame: boolean;
    introducedByShotNo: (ch: DirectorCharacter) => number;
    onchange: () => void;
    onadd: () => void;
    onremove: (id: string) => void;
    onsheet: (id: string, files: FileList | null) => void;
    onstartframe: (id: string) => void;
  } = $props();
</script>

<!--
  🔴 This panel used to open with six permanent paragraphs of prose. What survives above the fold is
  either DYNAMIC (it appears only when it applies to this project) or one line long; the measured
  warnings were DEMOTED into the <details> below and into title= attributes, not deleted — every one
  of them describes a failure that is invisible in a successful render, so losing them would cost
  somebody the day it cost to find them.

  The one block deleted outright is the Echo-fuses-props / MSR-composes-props paragraph. It is not
  gone from the UI: `REFERENCE_ROLE_HINT.prop` says it as the title= on the very role picker where
  the choice is made, and `roleAdvisory` (→ `roleNote` below) says it again, with this project's own
  shot counts, the moment a prop reference actually exists. A third static copy was the one that
  could drift out of step with the other two.
-->
<p class="hint" style="margin:0 0 8px">
  On <b>{model || 'base'}</b> — the
  <b>{identityRegime === 'memory' ? 'Echo memory bank' : 'character-sheet'}</b> path.
</p>

<!-- Collapsed, not cut: an author who has done this once needs none of it, and one who has not
     needs all of it. Matches the disclosure pattern the composer already uses. -->
<details style="margin:0 0 8px">
  <summary class="tl-meta">What holds a face, and what the image has to be</summary>
  <p class="hint" style="margin:6px 0">
    Holding a face across <b>pure text-to-video</b> — no i2v opener, no continuation — needs
    something that conditions identity: the <b>Licon-MSR-V2</b> LoRA. The <b>faceid-sheet-rt</b>
    LoRA is the other mechanism and wants a 4-panel sheet instead. With neither of them the references
    are still applied and nothing holds.
  </p>
  <p class="hint" style="margin:6px 0">{referenceSpec(model, loras)}</p>
  {#if identityRegime === 'memory'}
    <p class="hint" style="margin:6px 0">
      Uploads are prepared for you — matted, flattened on white, letterboxed to {snap32(
        width
      )}×{snap32(height)} — and one whose subject is CUT OFF at a frame edge is refused outright.
    </p>
  {/if}
  <p class="hint" style="margin:6px 0">
    A new character is <b>on in every shot</b>; switch them off under <b>Cast in this shot</b> in
    the shot panel, which drops both their reference and their description from it. For a character
    with no reference yet, render the shot that introduces them and promote a frame from
    <b>Takes</b>.
    {MULTI_CHARACTER_CAVEAT}
  </p>
</details>

<!-- Everything below is conditional — it costs no vertical space on a project it does not apply to,
     which is the whole reason it stays above the fold while the prose did not. -->
{#if roleNote}
  <p class="hint" style="margin:0 0 8px">ℹ {roleNote}</p>
{/if}
{#if identityNote}
  <p class="hint err" style="margin:0 0 8px">⚠ {identityNote}</p>
{/if}
{#if referencePrepError}
  <p class="hint err" style="margin:0 0 8px">⚠ {referencePrepError}</p>
{/if}
{#each characters as ch (ch.id)}
  <div class="refrow">
    {#if ch.sheet_b64}
      <img
        class="refimg sheet"
        src={assetSrc(ch.sheet_b64)}
        alt={ch.name || 'character reference'}
      />
    {:else if referencePreparing === ch.id}
      <div class="refimg empty"><span class="plus">…</span> Preparing</div>
    {:else}
      <label class="refimg empty" title={referenceSpec(model, loras)}
        ><span class="plus">＋</span> Reference
        <input
          type="file"
          accept="image/*"
          hidden
          onchange={(e) => onsheet(ch.id, e.currentTarget.files)}
        />
      </label>
    {/if}
    <div class="refmeta">
      <div class="inline-row" style="gap:6px;align-items:center">
        <input bind:value={ch.name} oninput={onchange} placeholder="Name — e.g. Mia" />
        <select
          class="inp"
          style="width:auto;flex:0 0 auto"
          value={characterRole(ch)}
          onchange={(e) => {
            ch.role = e.currentTarget.value as DirectorReferenceRole;
            onchange();
          }}
          title={REFERENCE_ROLE_HINT[characterRole(ch)]}
        >
          {#each REFERENCE_ROLES as r (r)}
            <option value={r}>{REFERENCE_ROLE_LABEL[r]}</option>
          {/each}
        </select>
        <span
          class="tl-meta"
          title="This character's identity tag (source_id) — allocated once, never changes. Read only by the character-sheet regime; the memory regime runs untagged."
          >#{ch.source_id}</span
        >
      </div>
      <textarea
        rows="2"
        bind:value={ch.identity}
        oninput={onchange}
        placeholder="Appearance sentence — age, build, hair, eyes, face, clothing. No mood: every shot repeats this word for word."
      ></textarea>
      <div class="inline-row" style="gap:6px">
        {#if ch.sheet_b64}
          <label class="miniai" title={referenceSpec(model, loras)}
            >Replace
            <input
              type="file"
              accept="image/*"
              hidden
              onchange={(e) => onsheet(ch.id, e.currentTarget.files)}
            />
          </label>
        {/if}
        {#if hasStartFrame}
          <button
            class="miniai"
            disabled={referencePreparing === ch.id}
            title={identityRegime === 'memory'
              ? "Use the inspected shot's start frame as this character's reference — matted, flattened on white and letterboxed first, and refused if the subject is cut off at an edge"
              : "Use the inspected shot's start frame, uncropped, as this character's reference"}
            onclick={() => onstartframe(ch.id)}
            >{referencePreparing === ch.id ? 'Preparing…' : "From shot's start frame"}</button
          >
        {/if}
        <button class="miniai" onclick={() => onremove(ch.id)}>Delete</button>
      </div>
      {#if ch.introduced_by}
        <p class="hint" style="margin:4px 0 0">
          Introduced by shot {introducedByShotNo(ch)}, and taken from its footage — that shot
          renders WITHOUT the reference (it is what defined the look); every later shot holds it.
        </p>
      {/if}
    </div>
  </div>
{/each}
<!-- The "on in every shot" surprise is stated on the button that causes it, which is the moment it
     matters; the long form is in the panel's <details>. The identity-tag sentence that used to be
     here is gone — the #source_id chip's own title= already says the tag never changes, and that
     title sits on the number itself. -->
<button
  class="btn sm"
  style="width:100%;justify-content:center"
  title="Added to EVERY shot. Switch them off per shot under “Cast in this shot” in the shot panel."
  onclick={onadd}>＋ Add character</button
>
