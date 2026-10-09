<script lang="ts">
  import type {
    DirectorClip,
    DirectorH3Generation,
    H3ContinuationMode,
    H3LoraListing,
    LtxLoraInfo,
    LtxRuntimeLora
  } from '$lib/api/client';
  import type { EngineCapabilities, EngineVocab } from '$lib/director/engine';
  import { parseH3Block } from '$lib/director/h3-block';
  import { H3_PROMPT_MAX_CHARS } from '$lib/director/h3-prompt';
  import {
    MOBILE_REF_GLYPH,
    deadOrdinalNote,
    type MobilePromptTab,
    type MobileRef
  } from '$lib/director/mobile';
  import MobilePromptView from './MobilePromptView.svelte';
  import MobileSections from './MobileSections.svelte';
  import MobileBeats from './MobileBeats.svelte';
  import MobileDetails from './MobileDetails.svelte';
  import MobileRefCard from './MobileRefCard.svelte';

  /**
   * One render unit, in the hand: who it is, what it has rendered, what it may name, what it says,
   * and the two buttons that turn that into pixels.
   *
   * 🔴 Presentation only. Every value here is owned by `DirectorWorkspace`, which is the single
   * component that holds a project — so this file can be mounted, unmounted and re-mounted as the
   * author walks the cut without a byte of the document living inside it.
   */
  let {
    clip,
    sub = $bindable('prompt'),
    section = $bindable(null),
    expanded = $bindable(false),
    card = $bindable(null),
    deadLabel = $bindable(null),
    gen = null,
    doc = null,
    caps,
    vocab,
    index,
    count,
    stateLabel,
    prompt = '',
    idea = '',
    refs = [],
    resolvedLabels,
    takeSrc = null,
    takeCount = 0,
    takeNote = '',
    rendered = false,
    finding = null,
    findingBusy = false,
    frames,
    renderSeconds = null,
    fps,
    maxFrames = null,
    songNote = null,
    reachableNote = null,
    promptBusy = false,
    canReroll = false,
    retakeNote = '',
    resumeNote = '',
    tier,
    opensOnImage = false,
    frameMode = 'pin',
    frameStill = '',
    joinMode = null,
    joinDepth = 7,
    joinNote = null,
    loraCatalogue = null,
    projectLoras = [],
    ltxLoras = [],
    projectLtxLoras = [],
    loraNote = null,
    projectSteps = undefined,
    projectWidth = 0,
    projectHeight = 0,
    frameFor = undefined,
    onprev,
    onnext,
    onprompt,
    onidea,
    ondraft,
    onreroll,
    onsummarise,
    onrework,
    oneditblock,
    onopentakes,
    onopenrefs,
    onrender,
    onretake,
    onapplyfinding,
    ondismissfinding,
    onseconds,
    onframeimage,
    onframemode,
    onremoveframe,
    onjoin,
    onjoindepth,
    onupdate,
    onchange
  }: {
    clip: DirectorClip;
    /**
     * The screen's own view state, owned upward.
     *
     * 🔴 Bindable rather than local because the shell TEARS THIS SCREEN DOWN to push a sheet: from
     * Sections, "Rework this section" and Back would otherwise land on Prompt with the section lost,
     * and the expanded take player would collapse and refetch its video.
     */
    sub?: MobilePromptTab;
    section?: string | null;
    expanded?: boolean;
    card?: MobileRef | null;
    /** The `<Label N>` the open card is about when nothing in scope carries it. */
    deadLabel?: string | null;
    /** `null` on engines with no structured prompt — the whole H3 half then draws nothing. */
    gen?: { id: string; seconds: number } | null;
    doc?: DirectorH3Generation | null;
    caps: EngineCapabilities;
    vocab: EngineVocab;
    index: number;
    count: number;
    stateLabel: string;
    prompt?: string;
    idea?: string;
    refs?: MobileRef[];
    resolvedLabels?: ReadonlySet<string>;
    /** The take this client would play at the current tier, or `null`. */
    takeSrc?: string | null;
    /** `0` where the take list has not been read for this unit — the sheet reads it on open. */
    takeCount?: number;
    takeNote?: string;
    /** This unit has a take at the current tier — the exact test `renderCurrent()` routes on. */
    rendered?: boolean;
    /** A review finding for THIS unit, surfaced where the fix happens. */
    finding?: { title: string; body: string } | null;
    findingBusy?: boolean;
    frames: number;
    /** The render unit's delivered length when it packs more than this clip; `null` otherwise. */
    renderSeconds?: number | null;
    fps: number;
    /**
     * The frame ceiling in force for THIS project, or `null` where the engine has none.
     *
     * 🔴 Not `caps.maxFrames`. A project carrying a song is capped lower (345 vs 362) so the audio
     * reference can be as long as the picture — see `h3MaxFrames`. A budget bar drawn off the spec
     * cap tells the author they have room the packer will not give them.
     */
    maxFrames?: number | null;
    songNote?: string | null;
    reachableNote?: string | null;
    promptBusy?: boolean;
    /**
     * This DEPLOYMENT can re-roll one unit on its own, which is what makes the primary button a
     * retake.
     *
     * 🔴 Deployment, not engine: LTX-2.5 implements a single-shot retake and answers 501 to it when
     * the box has no bank directory, so a phone offered the button off the capability alone spends
     * the queue wait to be told no.
     */
    canReroll?: boolean;
    /** What a reroll does to the units after it. See `retakeTailNote`. */
    retakeNote?: string;
    /**
     * Why a reroll is unavailable, when it is — `unavailableFor(engine, 'resume')`.
     *
     * 🔴 Rendered as the disabled button's title. Without it this shell is the one place that
     * message never appears, so a phone user meets a greyed-out Retake with no account of why,
     * which reads as a bug rather than as a deployment fact.
     */
    resumeNote?: string;
    tier: 'proxy' | 'final';
    opensOnImage?: boolean;
    frameMode?: 'pin' | 'keyframe';
    frameStill?: string;
    joinMode?: H3ContinuationMode | null;
    joinDepth?: number;
    joinNote?: string | null;
    loraCatalogue?: H3LoraListing | null;
    projectLoras?: string[];
    /** The `loraIdentity: 'path'` checklist and its project defaults. See `MobileDetails`. */
    ltxLoras?: LtxLoraInfo[];
    projectLtxLoras?: LtxRuntimeLora[];
    loraNote?: string | null;
    projectSteps?: number | undefined;
    projectWidth?: number;
    projectHeight?: number;
    frameFor?: ((index: number, atSeconds: number) => string | undefined) | undefined;
    onprev: () => void;
    onnext: () => void;
    onprompt: (next: string) => void;
    onidea: (next: string) => void;
    ondraft: () => void;
    onreroll: () => void;
    onsummarise: () => void;
    /** Opens the rework sheet, optionally pre-aimed at one section. */
    onrework: (section?: string) => void;
    oneditblock: () => void;
    onopentakes: () => void;
    onopenrefs: () => void;
    onrender: () => void;
    onretake: () => void;
    onapplyfinding: () => void;
    ondismissfinding: () => void;
    onseconds: (next: number) => void;
    onframeimage: () => void;
    onframemode: (mode: 'pin' | 'keyframe') => void;
    onremoveframe: () => void;
    onjoin: (mode: H3ContinuationMode) => void;
    onjoindepth: (k: number) => void;
    onupdate: (patch: Partial<DirectorH3Generation>) => void;
    onchange: () => void;
  } = $props();

  const structured = $derived(caps.structuredPrompt);
  const hasPrompt = $derived(!!prompt.trim());
  /** `<Picture 1>` → the prose in `subject_definitions` that says what it IS — the card's body. */
  const definitions = $derived(
    new Map(parseH3Block(prompt).subjectDefinitions.map((d) => [d.label.trim(), d.text]))
  );
  const TABS: { id: MobilePromptTab; label: string }[] = [
    { id: 'prompt', label: 'Prompt' },
    { id: 'sections', label: 'Sections' },
    { id: 'beats', label: 'Beats' },
    { id: 'details', label: 'Details' }
  ];
  const tabs = $derived(
    structured ? TABS : TABS.filter((t) => t.id === 'prompt' || t.id === 'details')
  );
  // Switching to an engine with no structured prompt takes Sections and Beats away under whoever
  // was standing on one, so the view falls back rather than drawing an empty panel.
  const view = $derived(tabs.some((t) => t.id === sub) ? sub : 'prompt');

  // The two numbers that refuse a render, as bars — a length past what one call can hold, and a
  // block past MiniMax's serving limit. Both are cheap, and both are otherwise discovered after the
  // queue wait.
  const budgets = $derived.by(() => {
    if (!structured || !gen) return [];
    // The ceiling in force for THIS project (345 frames rather than 362 under a song), over the
    // rate the engine actually renders at — not the document's `fps`, which a fixed-rate engine
    // ignores.
    const maxSeconds = maxFrames !== null ? maxFrames / (caps.fixedFps ?? fps) : gen.seconds;
    return [
      {
        key: 'length',
        label: 'Length',
        pct: Math.min(100, (gen.seconds / maxSeconds) * 100),
        over: gen.seconds > maxSeconds,
        text: `${gen.seconds.toFixed(2)} / ${maxSeconds.toFixed(2)}s`
      },
      {
        key: 'prompt',
        label: 'Prompt',
        pct: Math.min(100, (prompt.length / H3_PROMPT_MAX_CHARS) * 100),
        over: prompt.length > H3_PROMPT_MAX_CHARS,
        text: `${prompt.length.toLocaleString()} / ${H3_PROMPT_MAX_CHARS.toLocaleString()}`
      }
    ];
  });

  // 🔴 The prompt only. A render in flight and an offline engine are queue states — the submit
  // enqueues past both — and a label that read them would change under the thumb.
  const blocked = $derived(!hasPrompt);
  // 🔴 Routed exactly as `renderCurrent()` routes: an unrendered unit appends a render, a rendered
  // one on an engine that can re-roll a single unit becomes a retake. The label has to say which,
  // because on a phone there is no tooltip and no confirm dialog to say it afterwards.
  const isReroll = $derived(canReroll && rendered);
  const primaryLabel = $derived(
    !hasPrompt
      ? `Write a prompt first`
      : isReroll
        ? `▶ Reroll this ${vocab.renderUnit}`
        : `▶ Render this ${vocab.renderUnit}`
  );

  function openOrdinal(label: string) {
    const hit = refs.find((r) => r.label === label);
    if (hit) {
      deadLabel = null;
      card = hit;
      return;
    }
    // A number naming nothing still opens a card — silently doing nothing is how it stays invisible.
    deadLabel = label;
    card = {
      id: label,
      kind: 'image',
      label,
      badge: label,
      name: 'Nothing defines this',
      role: 'unresolved',
      image: '',
      scoped: false,
      ownAudio: false
    };
  }
</script>

<div class="shotnav">
  <button
    type="button"
    aria-label="Previous {vocab.renderUnit}"
    disabled={index <= 0}
    onclick={onprev}>‹</button
  >
  <span class="who"
    ><b
      >{vocab.renderUnit.charAt(0).toUpperCase() + vocab.renderUnit.slice(1)}
      {index + 1} of {count}</b
    ><span>{stateLabel}</span></span
  >
  <button
    type="button"
    aria-label="Next {vocab.renderUnit}"
    disabled={index >= count - 1}
    onclick={onnext}>›</button
  >
</div>

<!-- `expanded` survives a walk to the next unit, so the player collapses itself when that one has
     never rendered rather than staying open over nothing. -->
{#if !expanded || !takeSrc}
  <button class="take" class:none={!takeSrc} type="button" onclick={() => (expanded = !!takeSrc)}>
    <span class="tk-thumb"
      >{#if takeSrc}<span class="pv" aria-hidden="true">▶</span>{/if}</span
    >
    <span class="tk-meta"
      ><b>{takeSrc ? 'The take in the cut' : 'No take yet'}</b><span
        >{takeNote || `Nothing rendered at ${tier}`}</span
      ></span
    >
    {#if takeSrc}<span class="tk-exp" aria-hidden="true">⤢</span>{/if}
  </button>
{:else}
  <div class="takebig">
    <div class="viewer">
      <!-- svelte-ignore a11y_media_has_caption -->
      <video src={takeSrc} controls playsinline preload="metadata"></video>
      <span class="tag">{tier} · {vocab.renderUnit} {index + 1}</span>
    </div>
    <div class="tkrow">
      <span class="lb">{takeNote}</span>
      <button class="btn sm" type="button" onclick={onopentakes}
        >Takes{takeCount ? ` · ${takeCount}` : ''}</button
      >
      <button class="btn sm" type="button" onclick={() => (expanded = false)}>Collapse</button>
    </div>
  </div>
{/if}

{#if finding}
  <div class="alert">
    <span class="gl" aria-hidden="true">◈</span>
    <span class="ab">
      <b>{finding.title}:</b>
      {finding.body}
      <span class="aact">
        <button class="btn sm" type="button" disabled={findingBusy} onclick={onapplyfinding}
          >{findingBusy ? 'Fixing…' : 'Apply'}</button
        >
        <button class="btn sm ghost" type="button" onclick={ondismissfinding}>Dismiss</button>
      </span>
    </span>
  </div>
{/if}

{#if budgets.length}
  <div class="budget">
    {#each budgets as b (b.key)}
      <div class="brow">
        <span>{b.label}</span>
        <span class="btrack"
          ><span
            class="bfill"
            class:warn={b.pct > 80 && !b.over}
            class:bad={b.over}
            style="width:{b.pct}%"
          ></span></span
        >
        <span class="bv">{b.text}</span>
      </div>
    {/each}
  </div>
{/if}

{#if caps.omniReferences}
  <div class="refstrip">
    {#each refs as r (r.id)}
      <button class="ref" type="button" onclick={() => (card = r)}>
        <span class="rv" class:audio={r.kind === 'audio'} class:scoped={r.scoped}>
          {#if r.image}<img src={r.image} alt="" />{:else}<span class="gl" aria-hidden="true"
              >{MOBILE_REF_GLYPH[r.kind]}</span
            >{/if}
          <span class="ord">{r.badge}</span>
        </span>
        <span class="rn">{r.scoped ? `this ${vocab.renderUnit}` : r.name}</span>
      </button>
    {/each}
    <button class="ref add" type="button" onclick={onopenrefs}>
      <span class="rv"><span class="gl" aria-hidden="true">＋</span></span>
      <span class="rn">{refs.length ? 'Add' : 'References'}</span>
    </button>
  </div>
{/if}

<div class="subseg" role="group" aria-label="Prompt view">
  {#each tabs as t (t.id)}
    <button type="button" aria-pressed={view === t.id} onclick={() => (sub = t.id)}
      >{t.label}</button
    >
  {/each}
</div>

{#if view === 'prompt'}
  <MobilePromptView
    value={prompt}
    {idea}
    {structured}
    {hasPrompt}
    busy={promptBusy}
    {resolvedLabels}
    unit={vocab.renderUnit}
    {onprompt}
    {onidea}
    {ondraft}
    {onreroll}
    {onsummarise}
    onedit={oneditblock}
    onrework={() => onrework()}
    onsection={(name) => {
      section = name;
      sub = 'sections';
    }}
    onordinal={openOrdinal}
  />
{:else if view === 'sections'}
  <MobileSections
    value={prompt}
    bind:section
    unit={vocab.renderUnit}
    {onprompt}
    onrework={(name) => onrework(name)}
  />
{:else if view === 'beats'}
  <MobileBeats
    value={prompt}
    seconds={gen?.seconds ?? clip.seconds}
    subUnit={vocab.subUnit ?? vocab.renderUnit}
    {frameFor}
    {onprompt}
  />
{:else}
  <MobileDetails
    {clip}
    {doc}
    {caps}
    {fps}
    {maxFrames}
    {frames}
    {renderSeconds}
    partUnit={vocab.subUnit ?? vocab.renderUnit}
    unit={vocab.renderUnit}
    {songNote}
    {reachableNote}
    {opensOnImage}
    {frameMode}
    {frameStill}
    refsInScope={refs.length}
    {joinMode}
    {joinDepth}
    {joinNote}
    {loraCatalogue}
    {projectLoras}
    {ltxLoras}
    {projectLtxLoras}
    {loraNote}
    {projectSteps}
    {projectWidth}
    {projectHeight}
    {onseconds}
    {onframeimage}
    {onframemode}
    {onremoveframe}
    {onjoin}
    {onjoindepth}
    {onupdate}
    {onchange}
  />
{/if}

<div class="acts">
  <button class="btn primary full" type="button" disabled={blocked} onclick={onrender}
    >{primaryLabel}</button
  >
  <div class="arow">
    <button
      class="btn"
      type="button"
      disabled={blocked || !canReroll}
      title={canReroll ? undefined : resumeNote || undefined}
      onclick={onretake}>↻ Retake</button
    >
    <button class="btn" type="button" disabled={!rendered && !takeSrc} onclick={onopentakes}
      >▣ Takes{takeCount ? ` · ${takeCount}` : ''}</button
    >
  </div>
  <p class="hint">
    {#if !hasPrompt}
      Nothing to render yet — this {vocab.renderUnit} has no prompt.
    {:else if isReroll}
      Rerolling adds a take; the one in the cut stays until you pick the new one. It {retakeNote} The
      tier is a cut-wide choice and lives on the Render tab.
    {:else}
      Renders this {vocab.renderUnit} alone at <b>{tier}</b>. The whole cut is on the Render tab.
    {/if}
  </p>
</div>

<MobileRefCard
  ref={card}
  body={card ? (definitions.get(card.label) ?? null) : null}
  usage={card?.scoped ? `this ${vocab.renderUnit} only` : 'whole project'}
  warn={deadLabel
    ? deadOrdinalNote(
        deadLabel,
        refs.map((r) => r.badge)
      )
    : null}
  onclose={() => {
    card = null;
    deadLabel = null;
  }}
  onedit={caps.omniReferences && !deadLabel ? onopenrefs : undefined}
/>
