<script lang="ts">
  import { assetSrc } from '#lib/director/assets.svelte.ts';
  import {
    DEFAULT_VOCAB,
    UNAVAILABLE,
    capitalise,
    type EngineVocab
  } from '#lib/director/engine.ts';
  import { H3_LEGAL_TAIL_FRAMES } from '#lib/director/h3-generations.ts';
  import { h3Ordinals } from '#lib/director/h3-prompt.ts';
  import { H3_SOUNDTRACK_REF_ID } from '#lib/director/h3-soundtrack.ts';
  import type {
    DirectorClip,
    DirectorH3Reference,
    H3ReferenceKind,
    H3ReferenceRole
  } from '#lib/api/client.ts';
  import { H3_ROLES, defaultRoleFor } from '#lib/director/h3-roles.ts';

  /**
   * MiniMax-H3 omni references — the H3 counterpart of `CastPanel`, and deliberately a separate
   * component rather than a mode of it.
   *
   * The two mechanisms are not variants of one idea. An LTX character sheet is a rotary-tagged
   * latent identity: it is injected, and the prompt cannot refer to it. An H3 reference goes
   * through the Qwen3-VL text encoder and arrives ORDINAL-LABELLED in language-model space, so a
   * prompt can address it — "&lt;Picture 2&gt; wears the red coat". That is the whole reason this panel
   * numbers every row and refuses to sort them.
   *
   * Three rules the UI has to carry, because each one is invisible in the output when broken:
   *
   * 1. **Order is the request.** A row's position assigns its ordinal AND advances a shared rotary
   *    clock, so reordering the same references is a different render. Hence explicit ↑/↓ controls
   *    and no sorting, grouping or filtering anywhere in this file.
   * 2. 🔴 **Scope is NOT set here any more.** The numbered chip row that used to live on each row
   *    scoped by RENDERED-SHOT index, and the conditioner has no such granularity —
   *    `pipeline.py:311-340` builds ONE reference presentation per call, and a call is a whole
   *    generation. So it described something that could not happen, while `reference_ids` (which
   *    describes something that can) had no writer at all. Scope moved to the generation card in
   *    `H3PromptComposer`.
   *
   *    ⚠️ The read-only ROW that used to narrate the retired `segments` list is gone too — it was a
   *    second description of scope sitting beside the live one. What survives is a one-line MIGRATION
   *    affordance, shown only on a document that still carries the field, because
   *    `referencesForGeneration` still READS it as a fallback (`h3-generations.ts:400`) and that read
   *    is the only thing keeping an old project rendering what it always did.
   * 3. **Absent scope ≠ empty scope.** Absent means "every shot"; an explicit empty list means "no
   *    shot". They are opposite claims and neither is ever collapsed into the other.
   * 4. **A shot uses references OR an EXACT frame pin, never both** — `ref2va` and `fl2va` are
   *    different transformers. That is no longer a dead end: a shot's frame can instead be declared
   *    as a `ref2va` keyframe (its Details tab), which keeps every reference here in play.
   */
  let {
    references = $bindable(),
    clips,
    vocab = DEFAULT_VOCAB,
    maxImages,
    maxVideos,
    maxAudios,
    maxTotal,
    keyframePictures = 0,
    onchange,
    onadd,
    onremove,
    onmove,
    onfile
  }: {
    references: DirectorH3Reference[];
    /**
     * The timeline's shots — used ONLY to count how many render, so a legacy `segments` list can be
     * read back as "shots 1, 3 of 5". Scope itself is set on the generation card now.
     */
    clips: DirectorClip[];
    /** What this engine calls its units. See `EngineVocab` — display only. */
    vocab?: EngineVocab;
    maxImages: number;
    maxVideos: number;
    maxAudios: number;
    maxTotal: number;
    /**
     * Picture slots the WORST single render spends on frames it opens or closes on.
     *
     * 🔴 These are not in `references` and never can be: a keyframe belongs to exactly one cut in
     * exactly one render, so it is re-derived every time rather than stored. But it IS a real
     * `<Picture N>` on the wire — appended to that render's reference list by the packer — so it
     * spends one of the engine's per-request picture slots. Without it this panel counted a roster
     * and called it the engine's budget, and a project on the cap with one keyframe cut got a 400
     * at submit, after the queue wait, naming a limit the panel had just said it was inside.
     *
     * ⚠️ The MAXIMUM across renders, not the sum: the cap is per request, and summing would refuse
     * a timeline that is nowhere near it.
     */
    keyframePictures?: number;
    onchange: () => void;
    onadd: (kind: H3ReferenceKind) => void;
    onremove: (id: string) => void;
    /** Move a reference one place. Renumbers the ordinals, which is the point. */
    onmove: (id: string, delta: -1 | 1) => void;
    onfile: (id: string, slot: 'image' | 'audio' | 'video', files: FileList | null) => void;
  } = $props();

  /** Shots that actually render, in the index space `segments` scopes are expressed in. */
  const rendered = $derived(clips.filter((c) => c.prompt.trim().length > 0));

  const counts = $derived({
    image: references.filter((r) => r.kind === 'image').length,
    video: references.filter((r) => r.kind === 'video').length,
    audio: references.filter((r) => r.kind === 'audio').length
  });

  /**
   * The ordinals the model will actually use, computed by the same function the composer and the
   * compiler use.
   *
   * 🔴 This panel used to count position among references OF THE SAME KIND, which is right for
   * pictures and videos and **wrong for audio**: a clip whose `with_audio` is set takes its
   * `<Audio k>` number BEFORE its `<Video k>`, so every standalone audio reference sitting after
   * one was displayed one number lower than the engine assigns it. The failure is silent — the
   * prompt still renders, addressing the wrong asset. Which is also why the `with_audio` toggle
   * below carries a renumbering warning: it moves ordinals the prompt text already names.
   */
  const ordinals = $derived(h3Ordinals(references));

  /**
   * The budget as the ENGINE counts it: the roster plus the keyframe pictures the packer will
   * append to the busiest render. Adding a tenth picture to a nine-picture cap is refused whether
   * the tenth came from this list or from a cut's Details tab.
   */
  const engineImages = $derived(counts.image + keyframePictures);
  const engineTotal = $derived(references.length + keyframePictures);
  const atCap = $derived({
    image: engineImages >= maxImages,
    video: counts.video >= maxVideos,
    audio: counts.audio >= maxAudios,
    total: engineTotal >= maxTotal
  });

  /**
   * 🔴 "This shot has audio in scope but no picture" used to be checked here, per rendered shot.
   * It is gone rather than moved by accident: the rule is per GENERATION (an audio reference cannot
   * drive a `ref2va` call on its own), the generation is the render unit, and `validateH3Prompt`
   * already raises it as a hard error on the generation card. Two copies of a rule keyed on two
   * different units is how they end up disagreeing about which shots are in trouble.
   */
</script>

<p class="hint" style="margin:0 0 8px">
  A reference is a cast member, a location or a voice, held across the whole timeline. MiniMax-H3
  presents them to its text encoder <b>numbered</b>, so a prompt can name one directly — write
  <b>&lt;Picture 1&gt;</b>, <b>&lt;Video 1&gt;</b> or <b>&lt;Audio 1&gt;</b> in the prompt and the
  model knows which one you mean. <b>The angle brackets are part of the text</b> — they are what the encoder
  itself writes in front of each asset, so a label without them resolves to nothing.
</p>
<!-- Rendered rather than merely implied: "where did my Cast tab go" is the first thing anyone asks
     after switching a project to H3, and the answer is a property of the model. -->
<p class="hint" style="margin:0 0 8px">{UNAVAILABLE.characterRefs}</p>
<p class="hint" style="margin:0 0 8px">
  ⚠ <b>The order matters.</b> A reference's position sets its number and shifts the model's internal clock,
  so moving a row is a different render — not a re-labelling. Use ↑ / ↓ rather than deleting and re-adding.
</p>

{#each references as r, i (r.id)}
  {@const role = r.role ?? defaultRoleFor(r.kind)}
  {@const opts = H3_ROLES.filter((o) => o.kinds.includes(r.kind))}
  <div class="refrow">
    <!-- ── The tile CARRIES ITS ORDINAL ────────────────────────────────────────────────────────
         🔴 The ordinal is the only string the model resolves, and "which one is `<Picture 1>`" has
         to be answerable by looking rather than by counting rows. It was a muted `tl-meta` sharing
         a line with the name box, the ↑ ↓ pair and Delete — present, but the last thing on the row
         you read. It is now stamped on the picture itself, and it is still ALSO in the meta row for
         the audio-plus-video case, which needs two labels and does not fit a badge.
         ⚠️ The wrapper is what `position: absolute` needs, and it is the reason for a new class
         rather than styling `.refimg`: `.dw .refimg` is the img/label itself and two other rules in
         `workspace.css` (`.dw .sheet:not(.refimg)`, `.dw .empty:not(.refimg)`) key off that exact
         element. Wrapping leaves both intact. -->
    <div class="refslot">
      <!-- Guarded: an empty plate floating on a thumbnail says less than nothing. `h3Ordinals`
           always labels every reference today, so this is a belt rather than a case. -->
      {#if ordinals[i]?.label}
        <span
          class="reford"
          title="How the prompt refers to this reference. Derived from its POSITION in this list — move it and this changes. A clip whose soundtrack is turned into an Audio reference takes that number BEFORE its Video one, which is why these are not simply 1, 2, 3 per kind."
          >{ordinals[i].label}</span
        >
      {/if}
      {#if r.kind === 'image'}
        {#if r.image}
          <!-- 🔴 A LABEL, not a bare <img>. It used to be the image alone, which made a picture
               reference the one asset in the editor that could be created and deleted but never
               CHANGED — the only way to correct a wrong upload was to delete the row and re-add it,
               which renumbers every `<Picture N>` the prompts already name. -->
          <!-- 🔴 `display:contents` — the label must generate NO BOX. It carried `refimg sheet`
               itself and wrapped an img carrying them too, which is two sized boxes where the
               stylesheet expects exactly one: `.dw .refimg` targets the img/label itself and
               `.dw .sheet:not(.refimg)` keys off that same element, so the nesting broke the
               thumbnail's layout. Contents-display leaves the img as the only box, styled exactly
               as it was before the label existed, and the label still forwards a click to its
               input. -->
          <label style="display:contents;cursor:pointer" title="Replace this image">
            <img class="refimg sheet" src={assetSrc(r.image)} alt={r.label || 'reference'} />
            <input
              type="file"
              accept="image/*"
              hidden
              onchange={(e) => onfile(r.id, 'image', e.currentTarget.files)}
            />
          </label>
        {:else}
          <label class="refimg empty"
            ><span class="plus">＋</span> Picture
            <input
              type="file"
              accept="image/*"
              hidden
              onchange={(e) => onfile(r.id, 'image', e.currentTarget.files)}
            />
          </label>
        {/if}
      {:else if r.kind === 'video'}
        <!-- The clip goes up WHOLE. The engine opens the container and does its own 2 fps sampling,
             so there is nothing for the browser to decode — an earlier attempt to send one entry per
             frame was both a needless main-thread decode and a shape the shim rejects outright. -->
        <label class="refimg empty"
          ><span class="plus">{r.video ? '🎞' : '＋'}</span>
          {r.video ? 'Clip set' : 'Clip'}
          <input
            type="file"
            accept="video/*"
            hidden
            onchange={(e) => onfile(r.id, 'video', e.currentTarget.files)}
          />
        </label>
      {:else if r.id === H3_SOUNDTRACK_REF_ID}
        <!-- No file picker: this reference's payload is not a file the author chooses, it is THIS
             generation's slice of the project song, cut server-side at render time. Offering an
             upload here would accept a file and then silently overwrite it. -->
        <div class="refimg empty" style="cursor:default">
          <span class="plus">♪</span>
          Project song
        </div>
      {:else}
        <label class="refimg empty"
          ><span class="plus">{r.audio ? '♪' : '＋'}</span>
          {r.audio ? 'Audio set' : 'Audio'}
          <input
            type="file"
            accept="audio/*"
            hidden
            onchange={(e) => onfile(r.id, 'audio', e.currentTarget.files)}
          />
        </label>
      {/if}
    </div>
    <div class="refmeta">
      <div class="inline-row" style="gap:6px;align-items:center">
        <span
          class="tl-meta"
          title="How the prompt refers to this reference. Derived from its position in the list — move it and this changes. A clip whose soundtrack is turned into an Audio reference takes that number BEFORE its Video one, which is why these are not simply 1, 2, 3 per kind."
          >{#if ordinals[i]?.audioLabel}{ordinals[i].audioLabel} +
          {/if}{ordinals[i]?.label ?? ''}</span
        >
        <input
          bind:value={r.label}
          oninput={onchange}
          placeholder="Note to self — e.g. Mia, or the rooftop"
        />
        <button
          class="miniai"
          disabled={i === 0}
          title="Move earlier — renumbers this reference and the one above it"
          onclick={() => onmove(r.id, -1)}>↑</button
        >
        <button
          class="miniai"
          disabled={i === references.length - 1}
          title="Move later — renumbers this reference and the one below it"
          onclick={() => onmove(r.id, 1)}>↓</button
        >
        <!-- 🔴 The project soundtrack is AUTO-MANAGED and cannot be deleted here. It exists because
             a song is attached and it disappears when the song is removed, so a Delete on this row
             would be undone by the next re-derive — a button that visibly does nothing. Take a shot
             out of the song with the scope control below instead, which is the same gesture as for
             any other reference.
             It is a real reference rather than something injected at render time for a reason worth
             knowing: <Audio N> ordinals come from position in this list, so a reference the prompt
             cannot see is one it cannot address — and an unaddressed audio reference conditions far
             more weakly, silently. -->
        {#if r.id === H3_SOUNDTRACK_REF_ID}
          <span class="tl-meta" title="Auto-managed: remove the project song to remove this"
            >from the project song</span
          >
        {:else}
          <button class="miniai" onclick={() => onremove(r.id)}>Delete</button>
        {/if}
      </div>

      <!-- The task-type control. Not cosmetic: koblem derives the prompt's `[…]` prefix from this
           and nothing else, so a reference left on the default can only ever produce
           `[reference generation]` however it is actually being used. -->
      <label class="inline-row" style="gap:6px;margin-top:6px">
        <span class="tl-meta">Used as</span>
        <select
          class="inp"
          value={role}
          onchange={(e) => {
            r.role = e.currentTarget.value as H3ReferenceRole;
            onchange();
          }}
        >
          {#each opts as o (o.value)}<option value={o.value}>{o.label}</option>{/each}
        </select>
      </label>
      <p class="hint" style="margin:4px 0 0">
        {opts.find((o) => o.value === role)?.blurb ?? ''}
      </p>

      <!-- 🔴 Video only, and STRUCTURALLY so rather than by disabling: koblem rejects `with_audio`
           and `tail_frames` by name on an image or audio reference
           (`contract.py:501-507`), so offering them there would be offering a 400. -->
      {#if r.kind === 'video'}
        <p class="hint" style="margin:4px 0 0">
          Conditions on MOTION as well as appearance, and on its soundtrack when the file has one —
          so a clip is also the only way to hand H3 an existing voice or room tone.
        </p>
        <label class="inline-row" style="gap:6px;margin-top:4px">
          <span class="tl-meta">Soundtrack</span>
          <input
            type="file"
            accept="audio/*"
            onchange={(e) => onfile(r.id, 'audio', e.currentTarget.files)}
          />
        </label>
        <!-- A reference video does NOT create an `<Audio N>` just because the file has sound
             (`GUIDE-ref:112`) — it is a deliberate choice, which is why this is a field on the
             reference and not an inference from the container. -->
        <label class="inline-row" style="gap:6px;margin-top:6px">
          <input
            type="checkbox"
            checked={!!r.with_audio}
            onchange={(e) => {
              r.with_audio = e.currentTarget.checked;
              onchange();
            }}
            style="accent-color:var(--accent)"
          />
          <span class="tl-meta">Its soundtrack becomes an &lt;Audio&gt; of its own</span>
        </label>
        <p class="hint" style="margin:4px 0 0">
          ⚠ This <b>renumbers</b> the prompt. A clip that carries an &lt;Audio&gt; takes that number
          <em>before</em>
          its own &lt;Video&gt; one, so every later audio ordinal the prompt addresses shifts by one.
          Check the numbers in the composer after turning this on.
        </p>
        <label class="inline-row" style="gap:6px;margin-top:6px">
          <span class="tl-meta">Frames used</span>
          <select
            class="inp"
            value={r.tail_frames === undefined ? '' : String(r.tail_frames)}
            onchange={(e) => {
              const v = e.currentTarget.value;
              r.tail_frames = v === '' ? undefined : Number(v);
              onchange();
            }}
          >
            <option value="">whole clip</option>
            <!-- Generated from `H3_LEGAL_TAIL_FRAMES`, never a literal list. It is 5+17k inside
                 56..345, and 39 is deliberately absent: on-grid but 1.63s, and
                 `validate_generative_settings` rejects any reference video outside 2-15s. -->
            {#each H3_LEGAL_TAIL_FRAMES as f (f)}
              <option value={String(f)}>{f} frames · {(f / 24).toFixed(2)}s</option>
            {/each}
          </select>
        </label>
        <p class="hint" style="margin:4px 0 0">
          Only these lengths are handled honestly. An off-grid tail is not refused — it is silently
          shortened keeping the <em>first</em> frames, so the model is handed a different moment than
          the one you asked for and told it is the one you meant.
        </p>
      {/if}

      {#if r.segments !== undefined}
        <!-- ── A MIGRATION affordance, and nothing else ────────────────────────────────────────
             🔴 **This used to be a read-only scope ROW, and that was the defect.** It described the
             retired per-rendered-shot `segments` list in full — which shots, of how many — right
             beside the live `reference_ids` control on the generation card. Two controls narrating
             the same thing, one of which describes a granularity the conditioner does not have:
             `pipeline.py:311-340` builds ONE reference presentation per CALL, and a call is a whole
             generation, which is why `segments` was retired.

             What is left is the only thing that still needs a surface: the one click that drops it.
             It appears ONLY on a document that actually still carries the field, so a project
             written after the retirement never sees it at all.

             🔴 **The fallback READ stays** — `h3-generations.ts:400`,
             `r.segments === undefined || shots.some(...)`. It is the only thing keeping an old
             document rendering what it always did, and deleting it would silently re-scope every
             reference in every project written before 2026-08-08.

             ⚠️ `[]` is not a synonym for absent: an empty list means NO {vocab.renderUnit}, which is
             the one case worth spelling out because dropping it WIDENS the scope rather than
             narrowing it.

             For the record, since it is the question this row used to invite: *"a global reference
             on every shot except shot 0, plus a shot-0-specific one"* is expressible TODAY through
             `reference_ids` — generation 0's explicit list omits the global and includes its own,
             every other generation's includes the global. Nothing was lost with `segments`. -->
        <p class="hint" style="margin:7px 0 0">
          Carries a retired per-{vocab.subUnit ?? vocab.renderUnit} scope
          {#if r.segments.length === 0}
            <span style="color:var(--warning)">reaching nothing at all</span>
          {:else}
            ({r.segments.map((n) => n + 1).join(', ')} of {rendered.length})
          {/if}
          — still honoured by any {vocab.renderUnit} that has not set its own.
          <button
            class="miniai"
            title="Drops the stored per-shot list. This reference then follows the scope set on each {vocab.renderUnit}'s own {capitalise(
              vocab.renderUnit
            )} tab, or the whole reference list where none is set. ⚠ An EMPTY stored list means it currently reaches nothing, so dropping it widens the scope rather than narrowing it."
            onclick={() => {
              r.segments = undefined;
              onchange();
            }}>Drop it</button
          >
        </p>
      {/if}
    </div>
  </div>
{/each}

<div class="inline-row" style="gap:6px">
  <button
    class="btn sm"
    style="flex:1;justify-content:center"
    disabled={atCap.image || atCap.total}
    title={atCap.total
      ? `H3 takes at most ${maxTotal} references per render${keyframePictures ? `, and ${keyframePictures} of this timeline's busiest render is already spent on frames a cut opens on` : ''}`
      : atCap.image
        ? `H3 takes at most ${maxImages} pictures per render${keyframePictures ? `, and ${keyframePictures} of them ${keyframePictures === 1 ? 'is a frame a cut opens on' : 'are frames cuts open on'}` : ''}`
        : ''}
    onclick={() => onadd('image')}>＋ Picture</button
  >
  <button
    class="btn sm"
    style="flex:1;justify-content:center"
    disabled={atCap.video || atCap.total}
    title={atCap.total
      ? `H3 takes at most ${maxTotal} references total`
      : atCap.video
        ? `H3 takes at most ${maxVideos} clips`
        : ''}
    onclick={() => onadd('video')}>＋ Clip</button
  >
  <button
    class="btn sm"
    style="flex:1;justify-content:center"
    disabled={atCap.audio || atCap.total}
    title={atCap.total
      ? `H3 takes at most ${maxTotal} references total`
      : atCap.audio
        ? `H3 takes at most ${maxAudios} audio references`
        : ''}
    onclick={() => onadd('audio')}>＋ Audio</button
  >
</div>
<p class="hint" style="margin-top:8px">
  Up to {maxImages} pictures, {maxVideos} clips, {maxAudios} audio references and {maxTotal} total
  <b>per render</b> ({counts.image}/{counts.video}/{counts.audio} here; {engineTotal}/{maxTotal} on the
  busiest render). A reference is decoded once for the whole render, so scoping one to a few {vocab.renderUnitPlural}
  costs nothing extra — it only narrows which of them can name it.
</p>
{#if keyframePictures}
  <!-- 🔴 Said out loud, because it is the difference between the number above and the number the
       engine checks. A keyframe picture is a real <Picture N> on the wire and it is not in this
       list, so a roster that looks like it has room can still be refused at submit. -->
  <p class="hint" style="margin-top:8px">
    ⚠ {keyframePictures}
    {keyframePictures === 1 ? 'picture slot is' : 'picture slots are'} also spent on
    {keyframePictures === 1 ? 'a frame' : 'frames'} a {vocab.subUnit ?? vocab.renderUnit}
    opens or closes on, in this timeline's busiest render. Those are real
    <b>&lt;Picture N&gt;</b> assets and they are counted above — they are not in this list because a
    keyframe belongs to one {vocab.subUnit ?? vocab.renderUnit} in one render and is rebuilt every time.
  </p>
{/if}
<p class="hint" style="margin-top:8px">
  <b>Which {vocab.renderUnitPlural} each of these reaches is set on the {vocab.renderUnit} itself</b
  >
  — open a {vocab.renderUnit}'s <b>{capitalise(vocab.renderUnit)}</b>
  tab. That is the granularity the model has: it builds one reference presentation per render, and a render
  is a whole {vocab.renderUnit}. Nothing here narrows scope.
</p>
<p class="hint" style="margin-top:8px">
  A reference is <b>not</b> a frame the render has to match. It is encoded at its own size and
  steers the picture. If you want something to open on an exact image, attach it on that {vocab.subUnit ??
    vocab.renderUnit}'s Details tab — and pick between the exact <b>fl2va</b> pin (which costs every
  reference here, and the song) and the <b>ref2va</b> keyframe, which keeps them.
</p>

<!--
  ⚠️ **A scoped block, on purpose.** Everything else in this file comes from `workspace.css`, which
  is 2,400 lines of GLOBAL `.dw …` rules and has already had two class-name collisions that threw
  elements to the top of the viewport (`.dw .empty` and `.dw .sheet` against the `.refimg`
  modifiers). Two new names would be two new chances at a third. Svelte scopes these to this
  component, so they cannot collide with anything — and `.refimg` itself is deliberately NOT styled
  here, so the global rules that key off it keep working untouched.
-->
<style>
  /* The positioning context the ordinal badge needs. `.refimg` cannot be it: it is the `<img>` /
     `<label>` itself, and two global rules select that exact element by name. */
  .refslot {
    position: relative;
    display: block;
  }
  /* The ordinal, ON the thumbnail. It is the only string the model resolves, so it has to be
     readable over a photograph of anything — hence its own plate rather than a text colour. */
  .reford {
    position: absolute;
    left: 5px;
    top: 5px;
    z-index: 1;
    padding: 1px 7px;
    border-radius: 999px;
    border: 1px solid var(--accent);
    background: var(--bg-2);
    color: var(--accent);
    font-family: var(--mono);
    font-size: 10px;
    line-height: 1.6;
    font-weight: 700;
    pointer-events: none;
  }
</style>
