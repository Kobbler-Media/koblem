<script lang="ts">
  import type { DirectorCharacter, DirectorTake } from '#lib/api/client.ts';

  // Every retake — yours or the critic's — is kept as its own take, so this is where the choice
  // actually gets made. Picking one re-derives the stitched cut from the selections; promoting a
  // frame of one is how a character introduced in a text-to-video shot gets a reference at all.
  let {
    takesTier = $bindable(),
    promoteTargetId = $bindable(),
    takeVideoEls = $bindable(),
    takes,
    takeUrls,
    takesBusy,
    criticNote,
    characters,
    promoteTarget,
    promoteBusy,
    onload,
    onpick,
    onreview,
    onpromote
  }: {
    takesTier: 'final' | 'proxy';
    /** Which character a promoted frame lands on. Null follows the shot's first cast member. */
    promoteTargetId: string | null;
    /** The take previews, so a promotion can take the frame the user actually scrubbed to. */
    takeVideoEls: (HTMLVideoElement | null)[];
    takes: DirectorTake[];
    takeUrls: string[];
    takesBusy: boolean;
    criticNote: string;
    characters: DirectorCharacter[];
    promoteTarget: DirectorCharacter | null;
    promoteBusy: boolean;
    onload: () => void;
    onpick: (variant: number) => void;
    onreview: (autoRetake: boolean) => void;
    onpromote: (variant: number, ti: number) => void;
  } = $props();
</script>

<!-- Takes. Every retake (yours or the critic's) is kept, so this is where the choice
     actually gets made. Picking one re-derives the stitched cut from the selections. -->
<div class="detail-block">
  <div class="lbl-row">
    <span>Takes</span>
    <span class="tl-meta">
      <select class="inp sm" bind:value={takesTier} onchange={onload}>
        <option value="final">final</option>
        <option value="proxy">proxy</option>
      </select>
    </span>
  </div>
  <div class="inline-row">
    <button class="btn sm" onclick={onload} disabled={takesBusy}
      >{takesBusy ? 'Loading…' : 'Show takes'}</button
    >
    <button
      class="btn sm"
      title="Gemma watches the rendered frames and judges them against each shot's prompt"
      onclick={() => onreview(false)}>Dailies review</button
    >
    <button
      class="btn sm"
      title="Judge, and re-roll failed shots as NEW takes (spends GPU time; nothing is overwritten)"
      onclick={() => onreview(true)}>Review + re-roll</button
    >
  </div>
  {#if criticNote}
    <p class="hint" style="margin-top:6px">{criticNote}</p>
  {/if}
  {#each takes as take, ti (take.variant)}
    <div class="take-row" class:chosen={take.selected}>
      {#if takeUrls[ti]}
        <video
          class="take-vid"
          bind:this={takeVideoEls[ti]}
          src={takeUrls[ti]}
          muted
          loop
          playsinline
          onmouseenter={(e) => e.currentTarget.play()}
          onmouseleave={(e) => e.currentTarget.pause()}
        ></video>
      {:else}
        <div class="take-vid"></div>
      {/if}
      <div class="take-body">
        <span class="take-head">
          <!-- Numbered by POSITION, not by `variant`: a variant is a stable id (it is
               in the take's URL) and a full run that replaces the tier continues the
               count, so the raw number drifts away from what is on screen. -->
          take {ti + 1}{take.selected ? ' · in the cut' : ''}
          {#if take.seed != null && take.seed >= 0}
            <span class="tl-meta">seed {take.seed}</span>
          {/if}
        </span>
        {#if take.critic}
          <span class="take-verdict" class:fail={take.critic.verdict === 'fail'}
            >{take.critic.verdict} · {take.critic.score.toFixed(2)}</span
          >
          {#if take.critic.note}
            <span class="hint">{take.critic.note}</span>
          {/if}
          {#if take.critic.beats?.length}
            <span class="hint">
              beats: {take.critic.beats
                .map((b) => `${b.happened ? '✓' : '✕'} ${b.text}`)
                .join(' · ')}
            </span>
          {/if}
        {:else}
          <span class="hint">not judged</span>
        {/if}
      </div>
      <div class="take-actions">
        {#if !take.selected}
          <button class="btn sm" disabled={takesBusy} onclick={() => onpick(take.variant)}
            >Use</button
          >
        {/if}
        {#if characters.length}
          <button
            class="btn sm"
            disabled={promoteBusy || !promoteTarget}
            title={promoteTarget
              ? `Freeze the frame this preview is paused on and make it ${promoteTarget.name || 'that character'}'s reference`
              : 'Pick a character below first'}
            onclick={() => onpromote(take.variant, ti)}>→ reference</button
          >
        {/if}
      </div>
    </div>
  {/each}
  {#if takes.length && characters.length}
    <div class="inline-row" style="margin-top:6px">
      <label class="field" style="flex:1"
        ><span>Promote frames to</span>
        <select
          class="inp sm"
          value={promoteTarget?.id ?? ''}
          onchange={(e) => (promoteTargetId = e.currentTarget.value || null)}
        >
          <option value="">— pick a character —</option>
          {#each characters as ch (ch.id)}<option value={ch.id}
              >{ch.name || 'unnamed'} #{ch.source_id}</option
            >{/each}
        </select>
      </label>
    </div>
    <p class="hint">
      Scrub a take to the frame you want, then <b>→ reference</b>. This is how a character
      introduced in a text-to-video shot gets a reference at all: the shot that establishes the look
      becomes the reference for every shot after it. Promote an UNCROPPED frame — a reference only
      carries what it shows, and cropping it changes the body, not just the crop.
    </p>
  {/if}
  {#if takes.length === 0 && !takesBusy}
    <p class="hint">
      No takes loaded. Every retake of this shot is kept as a separate take — nothing overwrites
      anything.
    </p>
  {/if}
</div>
