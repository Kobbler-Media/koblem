<script lang="ts">
  // `capitalise` went with the render-unit audio echo, which moved to `H3PromptComposer`.
  import {
    BEATS_RELAY_NOTE,
    beatsAvailable,
    capabilities,
    unavailableFor,
    type DirectorEngine
  } from '$lib/director/engine';
  import { assetSrc } from '$lib/director/assets.svelte';
  import PinTime from '$lib/components/video/PinTime.svelte';
  import {
    MULTI_CHARACTER_CAVEAT,
    clipHasCharacter,
    refExclusionReason
  } from '$lib/director/characters';
  import type {
    DirectorCharacter,
    DirectorClip,
    LtxShotAudioKind,
    LyricWord
  } from '$lib/api/client';

  // The clip's words: its idea, the prompt those become, the moments that change inside it, who is
  // in it, and its audio. Every LLM action is a callback because each one is a gated call the
  // editor has to serialise and report on.
  //
  // 🔴 **Not mounted on an engine with `structuredPrompt` any more** — i.e. not on H3. There the
  // prompt is stored on the GENERATION and this pane's box wrote `clip.prompt`, which only reaches
  // the render for a generation nobody has opened: a decoy standing next to a Draft button that
  // worked. `H3PromptComposer` is the whole surface there, and the two branches this file used to
  // carry for it ("Something changes here", the render unit's audio echo) moved with their comments.
  //
  // ⚠️ The gate is `caps.structuredPrompt` at the call site, not an engine name here — so the LTX
  // vocabulary below (`v.subUnit ?? v.renderUnit`) is still read from the capability record.
  let {
    engine = 'ltx25',
    beatsNodeMounted,
    clip,
    active,
    fps,
    characters,
    cast,
    refCount,
    carried,
    shareNote,
    identityRegime,
    beatWords,
    promptBusy,
    draftFailed,
    draftTries,
    draftAllRunning,
    reworkShotText = $bindable(),
    onchange,
    ondraft,
    onreroll,
    onsummarise,
    onretrydraft,
    onaddbeat,
    onremovebeat,
    onsnapbeat,
    oncast,
    onresetcast,
    onshotaudio,
    onclearshotaudio,
    onrework
  }: {
    /** The PROJECT's engine. Beats and LTX character refs are hidden on an engine without them. */
    engine?: DirectorEngine;
    /** `props.beats_enabled` — whether this deployment's comfyui has the relay node mounted.
     *  Passed raw rather than pre-resolved, so the capability half stays with `caps`. */
    beatsNodeMounted?: boolean;
    clip: DirectorClip;
    /** Whether this tab is the one on screen. Both panes stay mounted; CSS shows one. */
    active: boolean;
    fps: number;
    characters: DirectorCharacter[];
    /** This shot's cast, resolved through the project roster. */
    cast: DirectorCharacter[];
    /** References this shot will actually send — cast members with something to send. */
    refCount: number;
    /**
     * Character ids this shot's CARRY already supplies, from `carriedCharacterIds`. A cast member
     * in this set is listed as excluded-with-a-reason rather than as sending a reference, because
     * re-sending a sheet the join already holds re-imposes the reference's own state.
     *
     * Computed in the workspace, not here: it depends on the shot BEFORE this one, and this pane
     * only ever sees one clip.
     */
    carried?: Set<string>;
    shareNote: string;
    identityRegime: string;
    /** Lyric words inside this shot, as shot-relative offsets — what a beat snaps to. */
    beatWords: (LyricWord & { offset: number })[];
    promptBusy: boolean;
    draftFailed: boolean;
    draftTries: number;
    draftAllRunning: boolean;
    reworkShotText: string;
    onchange: () => void;
    ondraft: () => void;
    onreroll: () => void;
    onsummarise: () => void;
    onretrydraft: () => void;
    onaddbeat: () => void;
    onremovebeat: (i: number) => void;
    onsnapbeat: (i: number) => void;
    oncast: (characterId: string) => void;
    onresetcast: () => void;
    onshotaudio: (e: Event) => void;
    onclearshotaudio: () => void;
    onrework: () => void;
  } = $props();
  const caps = $derived(capabilities(engine));
  /** The capability AND the mounted node — see `beatsAvailable`. */
  const canBeats = $derived(beatsAvailable(engine, beatsNodeMounted));
  /**
   * Why there are no beats, where there is something to say — the same line `SettingsSheet` draws.
   *
   * 🔴 Drawn rather than left blank because THIS is the tab an author goes to when they want to
   * time something inside a shot, and a section that simply is not there reads as "this engine
   * cannot do it". On LTX-2.5 it can: the relay node is a mount away, and the difference between
   * "unsupported" and "restart the container" is the whole answer.
   */
  const beatsNote = $derived(caps.beats && !canBeats ? unavailableFor(engine, 'beats') : null);
  /** What this engine calls its units. `sub` is what THIS pane edits when a render has parts. */
  const v = $derived(caps.vocab);
  /** The word for the thing this pane edits: a cut where a render has cuts, else the render itself. */
  const unit = $derived(v.subUnit ?? v.renderUnit);

  // 🔴 The "Something changes here" entry and the render unit's audio echo MOVED to
  // `H3PromptComposer` on 2026-08-08, comments and all. Both were in the `{:else}` arm of a
  // capability gate that only H3 took, and on H3 this pane is no longer mounted at all — its prompt
  // box wrote `clip.prompt`, which never reached the render for a generation with a stored block.
  // What they do is a PROMPT edit, so they belong where the prompt is.
</script>

<div class="pane" class:on={active}>
  <div class="compose">
    <div>
      <div class="lbl-row">
        <span>Idea</span><button class="miniai" onclick={ondraft} disabled={promptBusy}
          >✨ {promptBusy ? 'Working…' : 'Draft prompt from idea'}</button
        >
      </div>
      <!-- 🔴 MULTILINE, and that is a correction rather than a preference. It was a single-line
           `<input>`, and the owner's verdict 2026-08-08 was that one line "didn't cut it for 5s LTX
           clips" — the idea for even a short shot is usually two or three clauses, and a box that
           shows one of them at a time makes people write less than they meant to. -->
      <textarea
        class="inp"
        rows="2"
        bind:value={clip.idea}
        oninput={onchange}
        placeholder="a robot walks into frame, rain starts…"></textarea>
    </div>
    <div>
      <div class="lbl-row"><span>Prompt</span></div>
      {#if draftFailed}
        <div class="warn-bar" style="margin:0 0 8px">
          <span class="sev warn">draft failed</span>
          <span style="flex:1">Auto-draft gave up on this {unit} after {draftTries} tries.</span>
          <button class="btn sm" onclick={onretrydraft} disabled={draftAllRunning}>Retry</button>
        </div>
      {/if}
      <div class="prompt-wrap">
        <textarea
          class="inp prompt-edit"
          rows="4"
          bind:value={clip.prompt}
          oninput={onchange}
          placeholder="Describe this {unit}… or write an idea above and hit Draft."></textarea>
        <div class="tools">
          <button class="miniai" onclick={ondraft} disabled={promptBusy}>✨ Draft</button>
          {#if clip.prompt.trim()}<button class="miniai" onclick={onreroll} disabled={promptBusy}
              >Reroll</button
            >{/if}
        </div>
      </div>
    </div>
    {#if beatsNote}
      <div>
        <div class="lbl-row"><span>Beats</span><span class="tl-meta">unavailable</span></div>
        <p class="hint">{beatsNote}</p>
      </div>
    {/if}
    {#if canBeats}
      <!-- Beats sit with the prompt, not in Shot details: a beat is a timed extension of
         the prompt, and writing one only makes sense against the prompt it modifies. -->
      <div>
        <div class="lbl-row">
          <span>Beats</span><span class="tl-meta"
            >{(clip.beats ?? []).length ? `relay ×${(clip.beats ?? []).length}` : 'off'}</span
          >
        </div>
        <p class="hint" style="margin-bottom:8px">
          The prompt is the SETTING for the whole shot; a beat names what
          <em>changes</em> at one moment inside it ("sets the glass down"). Don't repeat the setting in
          a beat. No beats = one prompt, no extra cost.
        </p>
        <p class="hint" style="margin-bottom:8px">{BEATS_RELAY_NOTE}</p>
        {#each clip.beats ?? [] as beat, i (i)}
          <div class="pinrow">
            <div style="flex:1;display:flex;flex-direction:column;gap:6px">
              <PinTime bind:seconds={beat.seconds} shotSeconds={clip.seconds} {fps} {onchange} />
              <input bind:value={beat.text} oninput={onchange} placeholder="what changes here" />
            </div>
            {#if beatWords.length}
              <button
                class="btn sm"
                title="Snap to the nearest lyric word"
                onclick={() => onsnapbeat(i)}>♪</button
              >
            {/if}
            <button class="btn sm danger" onclick={() => onremovebeat(i)}>✕</button>
          </div>
        {/each}
        {#if (clip.beats ?? []).length < 3}
          <button class="btn sm" onclick={onaddbeat}>Add beat</button>
        {:else}
          <p class="hint">3 beats is the ceiling for a shot this short.</p>
        {/if}
      </div>
    {/if}
    {#if caps.characterRefs}
      <!-- Cast in this shot. Every character in the project is listed, ON by default, and
         switching one off drops its reference from THIS shot's request and its appearance
         sentence from this shot's drafted prompt. A character whose reference came out of
         this very shot cannot be conditioned on it (circular) and says so rather than
         accepting a toggle that would do nothing. -->
      <div>
        <div class="lbl-row">
          <span>Cast in this shot</span><span class="tl-meta"
            >{refCount
              ? `${identityRegime === 'memory' ? 'memory' : 'ref_t2v'} · ${refCount} ref${refCount === 1 ? '' : 's'}`
              : 'no refs'}</span
          >
        </div>
        {#if characters.length === 0}
          <p class="hint">
            No characters yet — add one under <b>Cast</b> in the left rail. Every character you add is
            on by default in every shot.
          </p>
        {:else}
          <div class="castlist">
            {#each characters as ch (ch.id)}
              {@const on = clipHasCharacter(clip, ch.id)}
              {@const blocked = refExclusionReason(clip, ch, carried)}
              <div class="castrow" class:off={!on}>
                <button
                  class="casttoggle"
                  class:on
                  role="switch"
                  aria-checked={on}
                  aria-label={`${ch.name || 'unnamed'} in this shot`}
                  title={on
                    ? 'In this shot — click to leave them out'
                    : 'Not in this shot — click to put them in'}
                  onclick={() => oncast(ch.id)}>{on ? '✓' : ''}</button
                >
                <span class="castname"
                  >{ch.name || 'unnamed'}
                  <span class="tl-meta" title="identity tag (source_id)">#{ch.source_id}</span
                  ></span
                >
                <span class="caststate" class:warn={on && !!blocked}>
                  {#if !on}
                    not in this shot
                  {:else if blocked}
                    no reference
                  {:else}
                    reference on
                  {/if}
                </span>
              </div>
              {#if on && blocked}
                <p class="hint castwhy">{blocked}</p>
              {/if}
            {/each}
          </div>
          <div class="inline-row" style="margin-top:6px">
            <button
              class="miniai"
              title="Put every character back in this shot (the default)"
              onclick={onresetcast}>All characters</button
            >
            {#if shareNote}
              <span class="hint">{shareNote}</span>
            {/if}
          </div>
          {#if cast.length}
            <p class="hint" style="margin-top:6px">
              {#if identityRegime === 'memory'}Conditioned on the Echo memory bank (untagged) with{:else}Rendered
                as
                <code>ref_t2v:</code> with{/if}
              {cast.map((c) => `${c.name || 'unnamed'} → #${c.source_id}`).join(', ')}. Their
              appearance sentence is folded into this shot's prompt automatically, and must read
              identically in every shot — that wording is what tells the model which character a
              shot is about. Characters switched off are left out of the prompt too.
            </p>
          {/if}
          {#if cast.length > 1}
            <p class="hint" style="margin-top:6px">
              ⚠ {MULTI_CHARACTER_CAVEAT}
            </p>
          {/if}
        {/if}
      </div>
    {/if}
    {#if (clip.gist ?? clip.idea)?.trim() || clip.prompt.trim()}
      <div>
        <div class="lbl-row">
          <span>Scene summary</span><button
            class="miniai"
            onclick={onsummarise}
            disabled={promptBusy || !clip.prompt.trim()}>Re-summarise</button
          >
        </div>
        <textarea
          class="inp gist-edit"
          rows="2"
          value={clip.gist ?? clip.idea ?? ''}
          oninput={(e) => {
            if (!clip) return;
            clip.gist = e.currentTarget.value;
            onchange();
          }}
          placeholder="A plain-language line about this {unit} — write your own, or hit Re-summarise."
        ></textarea>
      </div>
    {/if}
    <!-- 🔴 Gated, not merely ignored: the uploader used to render, accept a file, and have the
         bytes dropped by the compiler — the user watched a clip attach and got a render that had
         never heard it.
         The gate is `perShotAudioUpload`, NOT "does this engine take audio". H3 takes audio; it
         just takes it per GENERATION, through the ordered reference list where position assigns the
         <Audio N> ordinal a prompt addresses. There is nowhere to put a per-SHOT file that the
         prompt could then name, which is why this one control stays engine-specific while the
         project soundtrack no longer is. -->
    {#if caps.perShotAudioUpload}
      <div>
        <div class="lbl-row">
          <span>Shot audio</span>
          {#if clip.audio_b64}
            <button class="miniai" onclick={onclearshotaudio}>Clear</button>
          {/if}
        </div>
        {#if clip.audio_b64}
          <div class="inline-row">
            <select
              class="inp"
              value={clip.audio_kind ?? 'voice'}
              onchange={(e) => {
                if (!clip) return;
                clip.audio_kind = e.currentTarget.value as LtxShotAudioKind;
                onchange();
              }}
            >
              <option value="voice">Voice — drives lip-sync</option>
              <option value="bgm">Sound only — no lip-sync</option>
            </select>
            <audio controls class="shot-audio" src={assetSrc(clip.audio_b64, 'audio/*')}></audio>
          </div>
          <small class="hint"
            >Replaces the project track for this shot only. Trimmed at the shot boundary; short
            clips are padded with silence.</small
          >
        {:else}
          <input class="inp" type="file" accept="audio/*" onchange={onshotaudio} />
          <small class="hint"
            >Optional — give this shot its own voice-over or effect. The project track plays
            underneath as a bed.</small
          >
        {/if}
      </div>
    {/if}
    <div>
      <div class="lbl-row"><span>Rework this {unit}</span></div>
      <div class="inline-row">
        <input
          class="inp"
          bind:value={reworkShotText}
          onkeydown={(e) => e.key === 'Enter' && onrework()}
          placeholder="she looks scared, add rain, cut to a wider angle…"
        />
        <button class="btn sm" onclick={onrework} disabled={promptBusy || !reworkShotText.trim()}
          >{promptBusy ? '…' : 'Rework'}</button
        >
      </div>
    </div>
  </div>
</div>
