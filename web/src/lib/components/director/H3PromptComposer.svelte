<script lang="ts">
  import { tick, untrack } from 'svelte';
  import { assetSrc } from '#lib/director/assets.svelte.ts';
  import {
    H3_SOUNDTRACK_REF_ID,
    H3_TRIM_JOIN,
    markIsTrimmed,
    runUpSecondsFor
  } from '#lib/director/h3-soundtrack.ts';
  import { roleLabel } from '#lib/director/h3-roles.ts';
  import {
    h3KeyframesFor,
    h3ReferenceIssues,
    h3UndeclaredKeyframes,
    type H3ReferenceCaps
  } from '#lib/director/h3-keyframes.ts';
  import {
    H3_AUDIO_MARKERS,
    H3_CAMERA_AMPLITUDE,
    H3_CAMERA_MOTION,
    H3_CAMERA_SPEED,
    H3_CUT_VERBS,
    H3_TASK_TYPES,
    H3_VISIBLE_MARKERS,
    composeH3Prompt,
    formatCutTime,
    h3Ordinals,
    h3ShotIsCut,
    validateH3Prompt,
    type H3ComposeInput,
    type H3Ordinal,
    type H3PromptIssue
  } from '#lib/director/h3-prompt.ts';
  import {
    H3_CHECKPOINT_FLIP_SECONDS,
    H3_DOCUMENTED_MIN_SECONDS,
    H3_MAX_SECONDS,
    h3FrameMode,
    h3GenerationGistFallback,
    h3HasFrameImage,
    h3ScopeAfterToggle,
    mergeBlockedReason,
    type PackedGeneration
  } from '#lib/director/h3-generations.ts';
  // `checkCanvas` moved with the canvas control into `H3RenderKnobs`.
  import { DEFAULT_VOCAB, capitalise, type EngineVocab } from '#lib/director/engine.ts';
  import { parseH3Block, replaceH3Section, type H3SectionName } from '#lib/director/h3-block.ts';
  import {
    h3DefinitionsBody,
    h3RetentionBody,
    h3SeedInput,
    h3StripCutVerb,
    h3SummaryBody,
    h3WriteKeyframes,
    needsH3PromptSeed,
    seedH3Prompt,
    writeH3MainSection,
    type H3ShotPlacement
  } from '#lib/director/h3-document.ts';
  import {
    h3SectionBadges,
    h3SectionSpans,
    type H3Token,
    type H3TokenCard
  } from '#lib/director/h3-highlight.ts';
  import { h3OrdinalRenames, h3Renumber, h3SubjectRenames } from '#lib/director/h3-renumber.ts';
  import { cachedFrame, grabFrames } from '#lib/director/frame-grab.ts';
  import BeatRibbon from './BeatRibbon.svelte';
  import H3BlockEditor from './H3BlockEditor.svelte';
  import H3RenderKnobs from './H3RenderKnobs.svelte';
  import type {
    DirectorClip,
    DirectorH3Continuation,
    DirectorH3Generation,
    DirectorH3Reference,
    H3ContinuationMode,
    H3LoraListing,
    H3ReferenceKind,
    H3RetentionEntry,
    H3SubjectDefinition,
    H3TaskType
  } from '#lib/api/client.ts';

  /**
   * The MiniMax-H3 prompt composer — **a lens over one generation's stored block.**
   *
   * 🔴 **Why the panels exist at all.** An H3 prompt is a schema the model was trained on, not
   * prose. Typing prose into it is out-of-distribution conditioning, and — the part that actually
   * costs picture quality — it supplies no `overall_soundscape` and no `non_diegetic_music`, so on
   * the engine whose headline feature is joint video+audio the model invents the entire audio bed
   * unguided. Every panel below exists to stop that.
   *
   * ## 🔴 The block is the document. The panels are a view of it. Both directions are live.
   *
   * This is the reverse of how this file was built. Until 2026-08-08 the FIELDS were the document,
   * the block was a read-only preview, and a hand-written block (`prompt_override`) was a dead end
   * that dimmed the whole panel stack. That made this form the only legal way to produce a prompt,
   * so the prompt LLM had to fill our editor's JSON shape, a chat edit that produced a real block
   * was flagged as a defect, and text an author typed could never come back into a field.
   *
   * Now `gen.doc.prompt` is the artifact and every panel edit is a **surgical rewrite of one
   * section's bytes**:
   *
   * ```
   *   panel edit  →  replaceH3Section(prompt, section, next)  →  onprompt(text)
   *   block edit  →  onprompt(text)  →  parseH3Block  →  panels redraw
   * ```
   *
   * 🔴 **Nothing here recomposes the whole block from the parsed fields, and that is the load-
   * bearing rule.** If it did, anything the parser could not represent — a malformed retention
   * line, a section MiniMax adds next month, a paragraph an author left between two headers — would
   * be destroyed the first time they touched an *unrelated* text box, silently, while they were
   * editing something else. `composeH3Prompt` therefore only SEEDS a block that does not exist yet.
   * Whatever the parser could not place is shown verbatim under "In the block, but in no field"
   * rather than hidden: it is in the block and it will be sent.
   *
   * ## One clip, one shot, one render, one store
   *
   * 🔴 The `[Shot N]` lines inside the block are BEATS — timed events within this one render — and
   * the block is the only place their prose and their cut times live. They used to be mirrored onto
   * a clip apiece, which stored both facts twice and produced a permanent "the timeline has not
   * caught up with the prompt" box; `collapseH3Beats` migrated that shape away.
   *
   * ## Which panels appear
   *
   * The section set is decided by the CHECKPOINT, which is decided by what is in scope, which is
   * decided by the packer. Ref2VA gets six sections keyed on `detailed_description`; the FL2VA
   * family gets three keyed on `integrated_multimodal_description`. Using the wrong main field is a
   * schema error and loses every reference section, so the branch is structural rather than a
   * "show advanced" toggle.
   */
  let {
    gen,
    genIndex,
    genCount,
    prev,
    joinPinNote = null,
    activeTab = $bindable('prompt'),
    anchorClip = undefined,
    shotAudio = [],
    promptBusy = false,
    draftFailed = false,
    draftTries = 0,
    draftAllRunning = false,
    reworkText = $bindable(''),
    references = [],
    refCaps = null,
    vocab = DEFAULT_VOCAB,
    reachableNote = null,
    loraCatalogue = null,
    projectLoras = [],
    projectSteps = undefined,
    projectWidth = 0,
    projectHeight = 0,
    onaddscopedreference,
    takeUrl = null,
    revealIssues = false,
    onupdate,
    onclip,
    onprompt,
    onmerge,
    ondraft,
    onreroll,
    onretrydraft,
    onrework,
    onsummarise,
    onhovertime
  }: {
    /** The packed generation — derived every render, never stored. See `h3-generations.ts`. */
    gen: PackedGeneration;
    genIndex: number;
    genCount: number;
    /**
     * Which of the two tabs this instance is drawing.
     *
     * 🔴 **One mounted component, two panes — not two mounts.** `touched` (below) is session state
     * about the AUTHOR's attention, and it is deliberately not persisted; mounting the composer
     * separately per tab would reset it every time somebody looked at the LoRA list and came back.
     * The component therefore stays mounted across the tab switch and only its markup branches.
     *
     * The split follows the owner's 2026-08-08 direction: `prompt` carries the references, the LLM
     * entry, the block itself and the exploded fields; `details` carries the configuration of the
     * render — join, seed, steps, LoRAs, canvas.
     */
    activeTab?: 'prompt' | 'details';
    /**
     * The BEAT the author has selected on the timeline, if it is one of this shot's.
     *
     * 🔴 It exists for exactly two fields — `idea` and `gist` — and only because they have nowhere
     * else to live: `DirectorH3Generation` has no `idea` and no `gist`, and adding one is a wire
     * change this stage does not need. So the shot's one-liner is stored on the beat the author was
     * standing on, which is where it was stored before the restructure too (`ShotPromptPane` bound
     * `clip.idea`). Nothing about it is per-beat in MEANING — the LLM drafts the whole shot from it.
     *
     * ⚠️ `undefined` while the selection is in another shot. The entry then falls back to the first
     * beat, so the box is never dead.
     */
    anchorClip?: DirectorClip | undefined;
    /**
     * The audio references this whole render will actually be conditioned by.
     *
     * Moved here from the per-beat pane, where it was an echo of something that is not per beat:
     * audio arrives through the ordered reference list, where POSITION assigns the `<Audio N>`
     * ordinal the prompt addresses, so a file attached to one beat would have no number the prose
     * could name. It belongs with the references, above the prompt that has to describe it.
     */
    shotAudio?: { ordinal: string; name: string; fromSong: boolean }[];
    /** An LLM call is in flight. Every entry-point button is a gated call the editor serialises. */
    promptBusy?: boolean;
    draftFailed?: boolean;
    draftTries?: number;
    draftAllRunning?: boolean;
    /** The instruction box for "send this prompt back through the LLM". Bound by the workspace. */
    reworkText?: string;
    /**
     * The PROJECT's references, in order — everything the scope picker below can turn on.
     *
     * 🔴 Not the same list as `gen.references`, and the difference is the whole point of Stage 5.
     * `gen.references` is what is IN SCOPE (and it also carries the keyframe pictures the packer
     * synthesised, which are not project references at all). This is the full roster, so a
     * reference that is currently out of scope is visible and reachable rather than absent.
     *
     * ⚠️ ORDER IS THE REQUEST. Position assigns each ordinal and advances a shared rotary clock, so
     * this array is never sorted, grouped or filtered for display here — reordering lives in the
     * References panel, behind explicit ↑/↓, because it is a different render rather than a
     * re-labelling.
     */
    references?: DirectorH3Reference[];
    /**
     * What the ENGINE will accept in one request, or `null` when props were never fetched.
     *
     * 🔴 Here rather than only in `ReferencesPanel`, because the panel counts the PROJECT's roster
     * and the engine counts ONE REQUEST — and the two differ by the keyframe pictures the packer
     * synthesises. A `keyframe`-mode image is a real `<Picture N>` on the wire, appended to this
     * render's reference list, and it occupies an image slot that no roster count has ever seen. A
     * project sitting at `max_image_refs` plus one keyframe cut is a 400 at submit, after the queue
     * wait, with nothing anywhere having said so. `null` accuses nobody: an unfetched cap is
     * unknown, not infinite, and inventing one would refuse legal requests while the engine is down.
     */
    refCaps?: H3ReferenceCaps | null;
    /**
     * What this engine calls its units — `shot` for what renders, `cut` for the pieces inside it.
     *
     * Passed in rather than read off `capabilities('h3')` so this component never learns which
     * engine it is on. See `EngineVocab`: the block below still writes `[Shot N]`, and the types are
     * still `DirectorH3Generation` / `DirectorClip`; only the labels move.
     */
    vocab?: EngineVocab;
    /**
     * This generation is longer than what the box can currently render, as a sentence — or `null`.
     *
     * 🔴 It is a WARNING and nothing else. The reachable ceiling is a RUNTIME property of a busy
     * box (VRAM, the loaded checkpoint, whatever reachability work is in flight), so it never
     * reaches the packer or `H3_MAX_SECONDS`: a saved timeline that silently re-packed itself
     * because the GPU was warm — and re-packed back an hour later — would change what renders with
     * nobody touching it. `null` when the engine reports no reduction, is down, or never sent the
     * field; an older shim does not, and inventing a limit out of silence is worse than saying
     * nothing. See `reachableCeilingNote` in `lib/director/engine.ts`.
     */
    reachableNote?: string | null;
    /**
     * What the engine actually has in its `minimax_h3/` LoRA directory, or `null`.
     *
     * 🔴 `null` and an empty listing are DIFFERENT and are shown differently. `null` = the engine
     * is down or predates the route, so the free-text box stands alone and a remembered name is
     * still the user's best move. An empty listing = the directory is there and holds nothing,
     * which on this service means the `/loras` volume is not mounted — and then every `loras[]`
     * request is refused however well-formed, so saying so is the whole value.
     */
    loraCatalogue?: H3LoraListing | null;
    /**
     * The PROJECT's LoRA/step defaults, for display only.
     *
     * 🔴 Shown here because `compile.ts` fills them into this generation when it names none of its
     * own, and a default that renders but is invisible on the card that renders it is the exact
     * shape of bug this file keeps guarding against. The empty field says "20 (engine default)"
     * however the project is configured, so an empty field on an 8-step project reads as a lie.
     *
     * Never written to from here — the project value is the settings sheet's. Typing on this card
     * OVERRIDES it for this {@link EngineVocab.renderUnit} and leaves the project alone.
     */
    projectLoras?: string[];
    projectSteps?: number | undefined;
    /**
     * The project's canvas, shown as the placeholder for the per-generation override below. `0`
     * where the caller has not supplied it, which simply prints no placeholder.
     */
    projectWidth?: number;
    projectHeight?: number;
    /**
     * Add a reference that ONLY this {@link EngineVocab.renderUnit} carries. Returns its id.
     *
     * 🔴 There is no per-shot reference on the wire and there cannot be one — the conditioner builds
     * one reference presentation per CALL, and a call is a whole generation. So this creates an
     * ordinary project reference and confines it here, which means pinning every other generation's
     * scope to what it already has. The editor owns that sweep (`addScopedReference`); this card
     * only asks for it, because it can see one generation and the sweep touches all of them.
     *
     * Absent ⇒ the control is not drawn. The old answer was "add it in the References panel, then
     * untick it on every other card", which is n-1 manual edits where missing one is invisible.
     */
    /** Opens a file picker and stores nothing if it is dismissed, so this resolves with no id. */
    onaddscopedreference?: (kind: H3ReferenceKind) => void;
    /**
     * Say everything, including the amber notes about fields nobody has touched yet.
     *
     * The workspace turns this on when the author asks for a RENDER — at that point "you have not
     * written a soundscape" stops being nagging and starts being the last chance to hear it. See
     * the `touched` note below for the rule this is the escape hatch from.
     */
    /**
     * The rendered take being shown for this render unit, if there is one.
     *
     * Only used to pull a still per beat — nothing here plays it. `null` while a shot has never
     * been rendered, which is most of the time a prompt is being written.
     */
    takeUrl?: string | null;
    revealIssues?: boolean;
    /** The generation before this one, for the merge check. Absent on the first. */
    prev: PackedGeneration | undefined;
    /**
     * Which TAKE of the previous render unit this join is pinned to, as one sentence.
     *
     * 🔴 Forward-looking. It names what the NEXT render of this generation will join — always the
     * predecessor's currently SELECTED take, because that is what koblem resolves
     * `continuation.from: "prev"` to. It does NOT describe the footage already on disk, which keeps
     * whatever join it was rendered with; the Joins panel on the Details tab is what reads that.
     *
     * `null` until the workspace has read the takes once. A take number invented from an unread
     * project would be worse than saying nothing, since this is the one number the author would use
     * to decide whether a re-render is needed.
     */
    joinPinNote?: string | null;
    /**
     * Patch this generation's NON-BLOCK state — steps, LoRAs, the continuation, the seed.
     *
     * ⚠️ Not the prompt. Everything the model reads goes through {@link onprompt}, which is a
     * separate callback precisely so a stray `onupdate({ prompt })` is a type error rather than a
     * write that skips the shot-body mirror.
     */
    onupdate: (patch: Partial<DirectorH3Generation>) => void;
    /**
     * Write this generation's block. **The single funnel for everything the model is sent.**
     *
     * ⚠️ Not `onupdate({ prompt })`. Keeping the two apart is what makes a stray prompt write a
     * type error rather than one that skips this funnel.
     */
    onprompt: (block: string) => void;
    /** Patch one of this generation's shots — the cut verb and the scenetrans flag live on the clip. */
    onclip: (clipId: string, patch: Partial<DirectorClip>) => void;
    /**
     * Dissolve the boundary before this {@link EngineVocab.renderUnit} — i.e. join it to the one
     * before, so the two render as a single call.
     *
     * ⚠️ **Not the timeline's "Merge".** That one merges two CLIPS into one beat. This one removes a
     * GENERATION boundary and changes nothing about the beats themselves. The two used to share the
     * word and they are different operations on different objects, so the label here now says which.
     */
    onmerge: () => void;
    /** Force a boundary before a beat inside this render — it starts a new render unit. */
    /** Draft the whole shot's prompt from the idea. Absent ⇒ the entry point is not drawn. */
    ondraft?: () => void;
    /** Rewrite the existing prompt — same intent, fresh wording. */
    onreroll?: () => void;
    /** Retry after auto-draft gave up on this shot. */
    onretrydraft?: () => void;
    /** Send the prompt back through the LLM with {@link reworkText} as the direction. */
    onrework?: () => void;
    /**
     * Write this {@link EngineVocab.renderUnit}'s one-line gist from its WHOLE prompt.
     *
     * 🔴 Not `summariseClipGist`, and not wired to it. That one reads `selectedClip` and summarises
     * `clip.prompt` — which under the reversal is a MIRROR of one `[Shot N]` body, so it would
     * describe a single beat and store the answer as the whole shot's label. This summarises the
     * block, which is what the shot IS. Absent ⇒ the button is not drawn.
     */
    onsummarise?: () => void;
    /**
     * The cut time the cursor is resting on in the prompt, or `null` when it has left one.
     *
     * 🔴 Seconds on the PROMPT's clock — measured from this render unit's frame 0, which restarts at
     * every generation boundary. The workspace converts to song time through the same
     * `planSoundtrack` offsets the render uses; doing the arithmetic here would be a second copy of
     * it, and this file cannot see the generations before this one to do it correctly anyway.
     */
    onhovertime?: (localSeconds: number | null) => void;
  } = $props();

  const doc = $derived(gen.doc);
  const isRef = $derived(gen.task === 'ref2va');

  /**
   * The **run-up**: seconds this render covers before its first delivered frame, and never delivers.
   *
   * 🔴 It is why the times in this panel are not the times on the timeline. An anchored continuation
   * is handed the previous shot's last {@link H3_TRIM_JOIN} frames as keyframes, re-renders them as
   * its own head to get itself moving into the join, and delivery trims them. So this render's clock
   * — which is the clock every `At MM:SS.mmm` below is measured on — starts 0.917 s before its block
   * appears in the cut. Everything here is already on that clock and stays on it: **the numbers in
   * the prompt are correct as written, and nothing in this file adds or subtracts the run-up.** What
   * it earns us is the ability to say when a time is UNREACHABLE.
   *
   * Derived from the packed generation, not passed in: it is a function of `continuation.anchor`
   * alone, which this component already holds.
   */
  const runUp = $derived(runUpSecondsFor(gen));

  /**
   * One line per task type, on the chip's own `title`.
   *
   * 🔴 **Two of the six keys did not exist, and two real types had no help at all.** It was written
   * against `'audio driven'` and `'style transfer'` — neither is in {@link H3_TASK_TYPES} and
   * neither appears anywhere in MiniMax's guide; they look like a memory of some other model's
   * vocabulary. The chips are rendered from `H3_TASK_TYPES`, so `audio reuse` and `audio reference`
   * — the two that decide whether the model COPIES the supplied signal or merely imitates it — had
   * an empty tooltip, while two entries here described chips nobody could ever hover.
   *
   * Now taken from `VIDEO_PROMPT_WRITING_GUIDE_ref_en.md`'s own table (`GUIDE-ref:130-145`), which
   * is vendored byte-for-byte at `api/assets/minimax-h3/`. The keys are the guide's and are the
   * literal tokens emitted into the summary bracket, so a typo here is a silent empty tooltip rather
   * than a type error — hence the exact-match test on the table below.
   */
  const TASK_TYPE_HELP: Record<H3TaskType, string> = {
    'keyframe completion':
      'An image IS a concrete frame of the target — first, last, or a keyframe anchor. Never claimed for the same asset as reference generation, which is defined as guidance that does NOT serve as a frame (GUIDE-ref:134-135).',
    'reference generation':
      'An asset guides character, scene, style, action, camera movement or storyboard, without being a frame of the target or the video being edited or continued. A clip that only lends its camera move or cutting rhythm is this (GUIDE-ref:135, 143).',
    'video editing':
      'An existing source video is directly MODIFIED. Editing an image, or generating between still keyframes, is not this (GUIDE-ref:136). If the original audio stays audible, add audio reuse.',
    'video continuation':
      'New content continues, extends, resumes or transitions from an existing source video (GUIDE-ref:137). If the new audio only carries on the original track’s character rather than copying it, add audio reference.',
    'audio reuse':
      'The same audio signal is reused, in full or in part (GUIDE-ref:138). This is the claim the project song makes — it pairs with the fully_copy retention marker.',
    'audio reference':
      'The signal is NOT copied: only its music style, timbre, dialogue or lyric content, sound-effect texture, beat or continuity is referenced (GUIDE-ref:139).'
  };
  const ordinals = $derived(h3Ordinals(gen.references));
  /** Short, because it is interpolated into roughly forty labels below. */
  const v = $derived(vocab);

  /**
   * MiniMax's serving limit, in characters. A hard REFUSAL and never a trim — see the counter's
   * title in the artifact section for why truncating would be worse than refusing.
   */
  const PROMPT_CHAR_LIMIT = 7000;

  /**
   * The SEED input — the one place a whole block is still composed, and only when there is none.
   *
   * Shared with `compileH3Timeline`'s fallback (`h3-document.ts`) rather than rebuilt here, so a
   * generation nobody has opened compiles to exactly what opening it would have stored. The two
   * used to be separate and drifted on one field — the FL2VA alignment line, which the preview
   * omitted and the compiler added.
   */
  const seedInput = $derived<H3ComposeInput>(h3SeedInput(gen));

  /**
   * Seed the block, ONCE, on first open.
   *
   * 🔴 The test is `typeof prompt !== 'string'`, not "is it empty". Once the author has a block
   * they are allowed to empty it, and re-filling an emptied box under their cursor is exactly the
   * behaviour the old override had — the reason it needed a confirm dialog to leave.
   *
   * ⚠️ This marks the project dirty just by opening a generation nobody has written. That is the
   * cost of the block being the document, and it is paid deliberately: composing lazily at compile
   * time is what made the block a preview of something rather than the thing itself.
   */
  $effect(() => {
    if (needsH3PromptSeed(doc)) onprompt(seedH3Prompt(gen));
  });

  /** The stored artifact. Empty only in the frame before the seed effect lands, or if emptied. */
  const prompt = $derived(doc.prompt ?? '');

  // ── Keeping the prompt's numbers pointing at what they pointed at ────────────

  /**
   * The block before the last renumber, so it can be put back.
   *
   * 🔴 Not the project's `aiRestore` slot, which is a whole-project revert point for an LLM pass and
   * holds exactly one entry: spending it on a reference toggle would throw away a restore point the
   * author was keeping for something else. Scoped to this generation and dropped on the way out.
   */
  let renumberUndo = $state<{ id: string; text: string; note: string } | null>(null);

  /**
   * Rewrite the prompt when the numbering moves under it.
   *
   * 🔴 **Reactive, and deliberately not wired into the four buttons that can cause it.** The ordinal
   * a label carries is allocated by POSITION in `gen.references`, so it moves for scoping a
   * reference in or out, following the project list again, adding one from this panel, attaching or
   * clearing a beat image (which mints or drops a keyframe picture), and reordering the roster in a
   * panel this component cannot see. Watching the OUTCOME covers all of them, including the ones
   * that do not exist yet; patching call sites would cover four and quietly miss the rest.
   *
   * ⚠️ Guarded on `doc.id`. A different generation has different references, so the first read after
   * switching shots is not a renumber — treating it as one would rewrite the incoming block's labels
   * to match the outgoing one's.
   */
  let lastOrdinals: { id: string; ordinals: H3Ordinal[] } | null = null;
  $effect(() => {
    const id = doc.id;
    const now = ordinals;
    const was = untrack(() => lastOrdinals);
    untrack(() => (lastOrdinals = { id, ordinals: now }));
    if (!was || was.id !== id) {
      untrack(() => (renumberUndo = null));
      return;
    }
    const renames = h3OrdinalRenames(was.ordinals, now);
    if (!renames.length) return;
    const before = untrack(() => prompt);
    const next = h3Renumber(before, renames);
    if (next === before) return;
    // Departures are the ones worth naming: a survivor shifting down is bookkeeping, but a mention
    // that now resolves to nothing is a sentence the author has to decide about.
    const orphaned = renames.filter(
      (r) => !now.some((o) => o.label === r.to || o.audioLabel === r.to)
    );
    untrack(() => {
      renumberUndo = {
        id,
        text: before,
        note: orphaned.length
          ? `${orphaned.map((r) => r.from).join(', ')} ${orphaned.length === 1 ? 'names' : 'name'} nothing now — the rest were renumbered.`
          : `${renames.length} ${renames.length === 1 ? 'number' : 'numbers'} moved with the reference list.`
      };
    });
    onprompt(next);
  });
  /** The lens. Never throws, never drops — see `h3-block.ts`. */
  const parsed = $derived(parseH3Block(prompt));
  /**
   * 🔴 Blank means the COMPOSED block ships, not silence. koblem drops an empty-prompt generation
   * from the chain, so honouring a cleared box would make this generation vanish from the render
   * rather than fail. `compileH3Timeline` has the same rule and the two have to agree, or this
   * panel is counting characters in something the compiler will not send.
   */
  const blockBlank = $derived(!prompt.trim());
  const sent = $derived(blockBlank ? composeH3Prompt(seedInput) : prompt);

  /**
   * What the lens reads, in the shape the validator wants — so the checks below describe the text
   * that will actually be sent, including a block that was written by hand or by the LLM. The old
   * override bypassed validation entirely; this one cannot.
   */
  const lensInput = $derived<H3ComposeInput>({
    task: gen.task,
    durationSeconds: gen.seconds,
    shots: parsed.shots,
    ordinals,
    ...(parsed.styleOpening ? { styleOpening: parsed.styleOpening } : {}),
    ...(parsed.taskTypes.length ? { taskTypes: parsed.taskTypes } : {}),
    ...(parsed.subjectDefinitions.length ? { subjectDefinitions: parsed.subjectDefinitions } : {}),
    ...(parsed.summary ? { summary: parsed.summary } : {}),
    ...(parsed.retentionAnalysis.length ? { retentionAnalysis: parsed.retentionAnalysis } : {}),
    ...(parsed.overallSoundscape ? { overallSoundscape: parsed.overallSoundscape } : {}),
    ...(parsed.nonDiegeticMusic ? { nonDiegeticMusic: parsed.nonDiegeticMusic } : {}),
    alignment: parsed.alignment
  });
  // ── References in scope, and the keyframes the packer synthesised ───────────

  /**
   * The keyframe images this generation hands `ref2va` as standalone `<Picture N>` pictures.
   *
   * Empty unless some shot's frame is on the "Open on this image" path. Derived from `gen`, never
   * stored: the ordinal comes from where the packer put the synthesised reference, so counting it
   * here separately is how a panel ends up naming `<Picture 2>` for an asset the engine packed as
   * `<Picture 3>` — which renders fine and talks about the wrong image.
   */
  const keyframes = $derived(h3KeyframesFor(gen));
  /** Keyframes whose four declarations are not all in the block yet. See `h3-keyframes.ts`. */
  const undeclaredKeyframes = $derived(h3UndeclaredKeyframes(parsed, keyframes));

  /**
   * The PROJECT references currently in scope, as ids.
   *
   * 🔴 Filtered against `references` on purpose: `gen.references` also carries the synthesised
   * keyframe pictures, and those must never reach a stored `reference_ids` — they belong to one
   * shot in one generation and are re-derived every render.
   */
  const scopedIds = $derived(
    gen.references.filter((r) => references.some((p) => p.id === r.id)).map((r) => r.id)
  );
  /**
   * Has anybody set this generation's scope explicitly?
   *
   * ⚠️ `undefined` and `[]` are DIFFERENT STATES meaning opposite things — absent is "whatever the
   * reference list says", empty is "no references at all, render this as t2va". Both are reachable
   * below and neither is ever collapsed into the other.
   */
  const scopeExplicit = $derived(doc.reference_ids !== undefined);
  const inScope = (id: string) => scopedIds.includes(id);

  /**
   * Turn one reference on or off for THIS generation.
   *
   * Per generation because per generation is the granularity the conditioner has:
   * `pipeline.py:311-340` builds ONE reference presentation per call, so the old per-rendered-shot
   * chip row in `ReferencesPanel` was describing something the engine cannot do. The first toggle
   * materialises the effective scope so the other references stay exactly where they were.
   */
  const toggleScope = (id: string) => {
    touch('references');
    onupdate({ reference_ids: h3ScopeAfterToggle(references, scopedIds, id) });
  };
  /** Back to "whatever the reference list says" — the `undefined` state, not the empty one. */
  const followProject = () => {
    touch('references');
    onupdate({ reference_ids: undefined });
  };

  /**
   * 🔴 Reference and keyframe checks live in `h3-keyframes.ts`, not in `validateH3Prompt`, and it is
   * structural rather than taste: that module imports `h3-prompt.ts`, so a check needing a
   * reference's ROLE cannot live in `h3-prompt.ts` without an import cycle. Concatenated here so
   * both tiers flow through the same `notes()` renderer and the same render gate.
   */
  const issues = $derived([
    ...validateH3Prompt(blockBlank ? seedInput : lensInput, sent),
    // 🔴 `gen.references` and not the project roster: it is the list the ENGINE receives for this
    // one call, keyframe pictures included, which is the only list the caps are checked against.
    ...h3ReferenceIssues(parsed, gen.references, keyframes, refCaps)
  ]);

  // ── Clicking an ordinal into whatever prose field has focus ─────────────────

  /**
   * The prose field the author was last in, and how to write it back.
   *
   * 🔴 **Never cleared on blur, and the pills call `preventDefault` on mousedown.** Both are needed
   * and for the same reason: clicking a pill would otherwise move focus out of the textarea before
   * the click handler ran, so the insertion would have nowhere to go — the control would appear to
   * do nothing, intermittently, which is worse than not existing. `preventDefault` keeps focus AND
   * the selection range; keeping the last target is the belt to that pair of braces.
   */
  /**
   * ⚠️ A CAPABILITY, not the element. The block editor is CodeMirror — its caret lives in editor
   * state, not on a DOM node with `selectionStart` — while every other field here is a real
   * `<textarea>`/`<input>`. Both can answer "write this label where the cursor is"; only one of them
   * can answer "what is your selectionStart". So the two register the same shape and the pills stay
   * indifferent to which kind of box they are writing into.
   */
  type ProseTarget = { insert: (label: string) => void };
  let proseTarget = $state<ProseTarget | null>(null);

  /** A plain prose field — the summary, a beat body, a retention line. */
  const claimProse = (el: HTMLTextAreaElement | HTMLInputElement, write: (v: string) => void) => {
    proseTarget = { insert: (label) => insertIntoField(el, write, label) };
  };

  /**
   * Insert an ordinal at the caret of a plain prose field.
   *
   * The ordinal stops being something to memorise, which is the whole request: a reference is shown
   * as its own thumbnail and the author's own name for it, and clicking it writes the only string
   * the model actually resolves.
   *
   * ⚠️ The caret is restored after `tick()`. Every field here is `value={…}` bound to the parsed
   * block rather than `bind:`, so Svelte replaces the DOM value on the next flush and the caret
   * would otherwise jump to the end of the paragraph on every insert. (The editor needs none of
   * this — a CodeMirror transaction carries the resulting selection with it.)
   */
  async function insertIntoField(
    el: HTMLTextAreaElement | HTMLInputElement,
    write: (v: string) => void,
    label: string
  ) {
    const v = el.value;
    const start = el.selectionStart ?? v.length;
    const end = el.selectionEnd ?? start;
    const before = v.slice(0, start);
    const after = v.slice(end);
    // Spaces only where one is missing: `<Picture 1>` fused to the previous word is a label the
    // conditioner does not match, and a space before a full stop is just untidy.
    const lead = before && !/\s$/.test(before) ? ' ' : '';
    const trail = after && !/^[\s.,;:!?)]/.test(after) ? ' ' : '';
    const caret = start + lead.length + label.length;
    write(before + lead + label + trail + after);
    await tick();
    el.focus();
    el.setSelectionRange(caret, caret);
  }

  const insertLabel = (label: string) => proseTarget?.insert(label);

  /**
   * ## Which sections this author has actually put a hand on, this session.
   *
   * 🔴 **The rule: nothing gets a VERDICT until it has been touched or the author hits render.** A
   * freshly-opened shot, and every LLM draft, is a document that is merely young — and until
   * 2026-08-08 the editor met both with six red errors, which is the complaint this whole stage
   * exists to answer. Amber notes are held back the same way, because "48 words, the guide asks for
   * 350" is true and worthless about a paragraph nobody has started writing.
   *
   * What is NOT gated, and deliberately:
   *
   * - `unwritten` — it is a ghost placeholder, not a verdict. It is the thing a fresh shot is
   *   supposed to show, so gating it would leave the section blank and silent, which is how an
   *   empty `overall_soundscape` reached the GPU in the first place.
   * - `error` — every remaining error is about text that IS PRESENT and IS WRONG: a `<Picture 4>`
   *   nobody defined, a marker from the other vocabulary, a cut past the end, 7000+ characters.
   *   None of them can fire on an untouched empty section, so gating them would buy nothing and
   *   would hide a refusal until submit. (The retier is what made this true — before it, half the
   *   "errors" were absences.)
   *
   * ⚠️ Session-scoped and intentionally not persisted: it tracks the AUTHOR's attention, not the
   * document's state, and a flag on disk saying "somebody once typed here" answers a different
   * question. Reopening a shot starts quiet again, which is the same promise a fresh one makes.
   *
   * The escape hatch is `revealIssues`, which the workspace turns on when a render is dispatched —
   * at that point every held-back note is said at once, because it is the last moment saying it is
   * still useful.
   */
  /**
   * 🔴 **Stamped with the generation it belongs to, and NOT reset by an effect.** The composer is
   * not remounted when the author switches generation tab, so the marks have to be scoped somehow —
   * but an `$effect` watching `doc.id` would wipe them on every keystroke instead: `gen` is derived
   * from the packer and every prompt edit produces a NEW `doc` object with the same id, which is a
   * change as far as an effect is concerned. Comparing the id at read time has no such problem and
   * needs no effect at all.
   */
  let touchedFor = $state<{ id: string; fields: Record<string, boolean> }>({ id: '', fields: {} });
  const touch = (field: string) => {
    if (touchedFor.id !== doc.id) touchedFor = { id: doc.id, fields: {} };
    if (!touchedFor.fields[field]) touchedFor.fields[field] = true;
  };
  const isTouched = (field: string) =>
    touchedFor.id === doc.id && touchedFor.fields[field] === true;

  const issueClass = (level: H3PromptIssue['level']) =>
    level === 'error' ? 'err' : level === 'warn' ? 'warnmsg' : 'ghost';
  /** The issues on one field that this author has earned the right to be told about. See above. */
  const issuesFor = (field: string) =>
    issues.filter(
      (i) => i.field === field && (i.level !== 'warn' || revealIssues || isTouched(field))
    );

  // ── Writing: one section's bytes at a time ──────────────────────────────────

  /**
   * Rewrite ONE section and leave every other byte of the block alone.
   *
   * Also the single place a section is marked TOUCHED, which is why every panel edit below routes
   * through it rather than calling `replaceH3Section` directly — a panel that wrote its own bytes
   * would edit a field and stay silent about it forever.
   */
  const setSection = (name: H3SectionName, body: string) => {
    touch(name);
    onprompt(replaceH3Section(prompt, name, body));
  };

  /**
   * The block's shots, as a rewrite of the main section needs them.
   *
   * ⚠️ The cut verb stays IN the body here (the parser leaves it there on purpose) and rides along
   * in `cutVerb` too. That is not a duplicate: `shotText` prefixes the verb only when the body does
   * not already open with it, which is what keeps a rewrite byte-identical to what was parsed.
   */
  const placements = $derived<H3ShotPlacement[]>(
    parsed.shots.map((s) => ({
      index: s.index,
      atSeconds: s.atSeconds,
      body: s.body,
      ...(s.cutVerb ? { cutVerb: s.cutVerb } : {}),
      ...(s.scenetrans ? { scenetrans: true } : {})
    }))
  );
  /** Ref2VA keeps the style on its own line; the FL2VA family folds it inside `[Shot 1]`. */
  const blockStyle = $derived(isRef ? parsed.styleOpening : null);
  /**
   * The main section this checkpoint keys on. It is where the word-envelope warning lands, and it
   * is what the block textarea and the shot controls both edit — so both mark it touched.
   */
  const bodyField = $derived(isRef ? 'detailed_description' : 'integrated_multimodal_description');
  const writeShots = (next: H3ShotPlacement[], style: string | null = blockStyle) => {
    touch('shots');
    touch(bodyField);
    onprompt(writeH3MainSection(prompt, gen.task, next, style));
  };

  // ── Adding a beat: a line in the prompt first, a clip second ─────────────────

  /**
   * One row per `[Shot N]` the prompt writes. **That is the entire definition of a beat.**
   *
   * 🔴 This used to pair the block's shots against the timeline's clips, because a beat WAS a clip
   * and either side could be missing — which is what produced "the prompt writes a beat the
   * timeline has no clip for" and its mirror. One clip is one shot now, so a beat has no timeline
   * side to disagree with: it is a timed event inside this shot's prose and nothing else.
   */
  const beatRows = $derived(parsed.shots);

  /**
   * The single clip this shot IS.
   *
   * One clip is one shot is one render on H3, so `gen.shots` is a one-element list and this is its
   * member. It carries what the prose cannot: the frames, the takes, the length.
   */
  const shotClip = $derived(gen.shots[0]?.clip ?? null);

  /**
   * Where each beat starts and how long it runs, off the block's own cut times.
   *
   * The same arithmetic the ribbon draws with — one derivation so the budget bar, the ribbon and a
   * beat's hover card can never disagree about how long a beat is.
   */
  const beatSpans = $derived(
    beatRows.map((row, i) => {
      const start = i === 0 ? 0 : (row.atSeconds ?? 0);
      const later = beatRows.map((x) => x.atSeconds ?? 0).filter((t) => t > start);
      const end = later.length ? Math.min(...later) : gen.seconds;
      return { row, start, seconds: Math.max(0.2, end - start) };
    })
  );

  /**
   * A still from the take at each beat's own time, for the ribbon's cells.
   *
   * 🔴 Best-effort by construction. There is no stored per-time thumbnail — the only source is the
   * take already decoded in the page — so a shot that has never rendered, a seek that does not
   * land, or a browser that refuses the canvas all end the same way: no frame, and a cell that
   * still shows its time, its duration and whether it cuts. The ribbon is useful without pictures,
   * which is what lets this fail quietly instead of blocking.
   */
  let frames = $state<Map<number, string>>(new Map());
  $effect(() => {
    const url = takeUrl;
    const times = beatRows.map((s) => s.atSeconds ?? 0);
    if (!url || !times.length) {
      untrack(() => (frames = new Map()));
      return;
    }
    // Paint whatever the cache already answers for before going near the decoder, so switching back
    // to a shot is instant rather than a second of empty cells.
    untrack(() => {
      const known = new Map<number, string>();
      for (const t of times) {
        const hit = cachedFrame(url, t);
        if (hit !== undefined) known.set(t, hit);
      }
      frames = known;
    });
    const stop = new AbortController();
    grabFrames(url, times, { signal: stop.signal, width: 220 }).then((got) => {
      if (!stop.signal.aborted) frames = got;
    });
    return () => stop.abort();
  });

  /**
   * Which beat the list below the ribbon is showing, or `null` for all of them.
   *
   * ⚠️ Reset when the render unit changes, or beat 3 of the shot you just left stays selected in a
   * shot that has two — `$derived` on `doc.id` rather than an effect, so it costs nothing per
   * keystroke and cannot land a frame late.
   */
  let beatFocusFor = $state<{ id: string; index: number | null }>({ id: '', index: null });
  /**
   * ⚠️ Also checked against the CURRENT rows, not just the id. A redraft that writes fewer
   * `[Shot N]` lines, or removing the selected beat, leaves an index nothing matches — and the list
   * below filters to it, so the panel would empty out with only the "show every one" button as a way
   * back. An index that no longer names a row means "all of them".
   */
  const beatFocus = $derived(
    beatFocusFor.id === doc.id &&
      beatFocusFor.index !== null &&
      beatRows.some((r) => r.index === beatFocusFor.index)
      ? beatFocusFor.index
      : null
  );
  const setBeatFocus = (index: number | null) => (beatFocusFor = { id: doc.id, index });

  /** The moment the "something changes here" entry is pointing at, render-unit relative. */
  let changeAt = $state(0);
  let changeText = $state('');
  /** Which beat that moment falls inside. `null` past the end of the render. */
  const changeTarget = $derived.by<{ index: number } | null>(() => {
    if (changeAt < 0 || changeAt >= gen.seconds) return null;
    let at = -1;
    for (let i = 0; i < beatRows.length; i++) {
      if ((beatRows[i].atSeconds ?? 0) <= changeAt) at = i;
    }
    return at < 0 ? null : { index: at };
  });

  /**
   * Add a beat: **write the `[Shot N]` line. That is the whole operation.**
   *
   * 🔴 It used to also split a clip, because a beat WAS a clip — and the two writes had to be
   * ordered just so, or the positional mirror landed the new beat's text on its neighbour. There is
   * no clip to split now and no mirror to race: a beat is a line in this shot's prose.
   */
  function addBeat() {
    const t = changeText.trim();
    const target = changeTarget;
    if (!t || !target) return;
    const next: H3ShotPlacement[] = [
      ...placements.slice(0, target.index + 1),
      { index: 0, atSeconds: Math.round(changeAt * 10) / 10, body: t },
      ...placements.slice(target.index + 1)
    ].map((p, i) => ({ ...p, index: i, ...(i === 0 ? { atSeconds: null } : {}) }));
    writeShots(next);
    changeText = '';
    changeAt = 0;
  }

  /**
   * "Keep it in this beat": append the sentence to that beat's body in the prompt.
   *
   * The body is one description the model reads in order, so a change written into it happens where
   * the sentence puts it. Nothing about the timeline moves, because nothing about the timeline is
   * involved — this is a prompt edit and only a prompt edit.
   */
  function foldChangeIn() {
    const t = changeText.trim();
    const i = changeTarget?.index ?? 0;
    if (!t || !placements[i]) return;
    writeShots(
      placements.map((p, j) =>
        j === i ? { ...p, body: `${p.body.replace(/\s*$/, '')} ${t}`.trim() } : p
      )
    );
    changeText = '';
    changeAt = 0;
  }

  /**
   * Write the four keyframe-completion declarations for every keyframe the block has not declared.
   *
   * Offered, never automatic. It writes into four sections at once, which is the largest edit any
   * button here makes — and the author is the one who knows whether the shot really opens on that
   * frame or merely happens to have a picture attached. Each of the four is idempotent, so a second
   * click does nothing and a half-written declaration is completed rather than duplicated.
   */
  const declareKeyframes = () => {
    for (const f of [
      'keyframes',
      'subject_definitions',
      'summary',
      'retention_analysis',
      bodyField
    ]) {
      touch(f);
    }
    onprompt(h3WriteKeyframes(prompt, gen.task, parsed, undeclaredKeyframes));
  };

  /**
   * Every label this generation's prompt is allowed to name, for the pickers below.
   *
   * ⚠️ De-duplicated, and not cosmetically: the "＋ &lt;Picture 1&gt;" button defines a subject
   * UNDER an ordinal's own label, so the two sources genuinely overlap — and the `<select>`s below
   * are keyed on the label, where a repeat is a runtime error rather than a doubled row.
   */
  const availableLabels = $derived([
    ...new Set([
      ...ordinals.flatMap((o) => (o.audioLabel ? [o.audioLabel, o.label] : [o.label])),
      ...parsed.subjectDefinitions.map((d) => d.label).filter(Boolean)
    ])
  ]);

  /**
   * The next free `<Subject N>`.
   *
   * 🔴 Deliberately NOT a renumber-on-delete. The number is written into prose across four other
   * sections, and shifting `<Subject 3>` down to `<Subject 2>` would silently re-point every
   * sentence that names it at a different person. A gap in the numbering is harmless; a
   * re-pointed one is invisible until the render comes back with the wrong face.
   */
  function nextSubjectLabel(): string {
    const used = parsed.subjectDefinitions
      .map((d) => /^<Subject (\d+)>$/.exec(d.label)?.[1])
      .filter(Boolean)
      .map(Number);
    return `<Subject ${(used.length ? Math.max(...used) : 0) + 1}>`;
  }

  const setDefs = (next: H3SubjectDefinition[]) =>
    setSection('subject_definitions', h3DefinitionsBody(next));

  /**
   * Remove one subject, and rename every later one across the whole block.
   *
   * 🔴 Explicit rather than reactive, unlike the ordinals: a subject is declared IN the document, so
   * nothing outside it changes when one goes and there is no derived value to watch. Both edits are
   * one action — dropping the line and renaming what followed it — so they are one call and one
   * undo, or the prompt spends a frame naming a subject it has just deleted.
   *
   * There is no dead slot here. A reference leaving scope still exists and its mentions have to stop
   * claiming somebody else's number; a deleted subject is simply gone, and the sentences that named
   * it are the author's to rewrite.
   */
  function removeSubject(i: number) {
    const defs = parsed.subjectDefinitions;
    const renames = h3SubjectRenames(i, defs.length);
    const before = prompt;
    const dropped = replaceH3Section(
      before,
      'subject_definitions',
      h3DefinitionsBody(defs.filter((_, j) => j !== i))
    );
    const next = h3Renumber(dropped, renames);
    touch('subject_definitions');
    if (renames.length) {
      renumberUndo = {
        id: doc.id,
        text: before,
        note: `${defs[i]?.label ?? 'That subject'} was removed, and ${renames.length} later ${renames.length === 1 ? 'subject was' : 'subjects were'} renumbered through the whole block.`
      };
    }
    onprompt(next);
  }
  const setRetention = (next: H3RetentionEntry[]) =>
    setSection('retention_analysis', h3RetentionBody(next));
  const setSummary = (text: string) => setSection('summary', h3SummaryBody(parsed.taskTypes, text));

  function toggleTaskType(t: H3TaskType) {
    const have = parsed.taskTypes;
    const next = have.includes(t) ? have.filter((x) => x !== t) : [...have, t];
    setSection('summary', h3SummaryBody(next, parsed.summary ?? ''));
  }

  /**
   * The beats whose `[Shot N]` line carries no transition verb — i.e. the camera keeps rolling.
   *
   * Counted for the header note, which is the finding surfacing on existing projects rather than a
   * regression: the picker that used to describe these read `cutVerb ?? clip.h3_cut_verb ??
   * H3_CUT_VERBS[0]` and so claimed a cut on every beat nobody had touched. `[Shot 1]` is excluded
   * because it opens the render and has nothing to cut FROM.
   */
  const silentBeats = $derived(
    parsed.shots.filter((s, i) => i > 0 && !h3ShotIsCut(s)).map((s) => s.index + 1)
  );

  const continuation = $derived(gen.continuation);
  function setContinuation(patch: Partial<DirectorH3Continuation>) {
    onupdate({ continuation: { ...(continuation ?? { mode: 'latents' }), ...patch } });
  }
  const mergeBlocked = $derived(mergeBlockedReason(prev, gen));

  /**
   * What the planner said each shot should SOUND like (`clip.h3_audio`), surfaced here.
   *
   * It has been written by the planner and read by the prompt grounding since H3 landed, with no
   * editor anywhere — so it was invisible state steering every draft. It belongs next to the
   * soundscape because that is what it feeds: the soundscape is per generation, and these are the
   * per-shot intents it has to reconcile.
   */
  const audioNote = $derived(shotClip?.h3_audio?.trim() ? shotClip : null);

  /**
   * Listed in the script rather than inline in the `{#each}`: an `as const` array literal sitting
   * where Svelte's each-parser is looking for its own ` as ` separator is asking for trouble.
   */
  const CONTINUATION_MODES: H3ContinuationMode[] = ['latents', 'frame', 'none'];
  /**
   * Markers are drawn from the table for the LABEL's kind — the two vocabularies are disjoint.
   *
   * 🔴 A value the block already carries is prepended when it is not in the table, and the same
   * goes for {@link labelOptions}. A `<select>` whose `value` is not among its options renders as
   * the FIRST option, so a hand-written block with an unknown marker — or a label whose reference
   * has gone out of scope — would silently read back as something the author never wrote, and the
   * next edit would commit it. The validator already says the value is wrong; the picker must not
   * pretend it is something else in the meantime.
   */
  const markersFor = (label: string, current: string): readonly string[] => {
    const table = label.startsWith('<Audio') ? H3_AUDIO_MARKERS : H3_VISIBLE_MARKERS;
    return (table as readonly string[]).includes(current) ? table : [current, ...table];
  };
  const labelOptions = (label: string): string[] =>
    availableLabels.includes(label) ? availableLabels : [label, ...availableLabels];

  // `$derived` rather than a plain const: it interpolates the vocabulary, and a const would capture
  // whatever the prop was at mount and then quietly stop tracking it.
  const MODE_BLURB = $derived<Record<H3ContinuationMode, string>>({
    latents:
      'Keeps velocity and motion through the join. Slower, and it shifts contrast and grade across the seam.',
    frame: `Keeps framing, identity and grade. Freezes some of the motion the previous ${v.renderUnit} ended on.`,
    none: 'A hard cut. The right answer whenever this boundary is a cut you wanted anyway.'
  });

  /**
   * ## Steps, LoRAs and canvas — now ONE component, shared with the project settings
   *
   * 🔴 The controls and their four-rule name validator moved to `H3RenderKnobs.svelte`, which the
   * settings sheet renders at `scope="project"` and this card renders at `scope="shot"`. They were
   * ~110 lines here and ~110 there, with `loraProblem` written out twice from the same rules — and
   * the two copies had already drifted on the nested-subdirectory case. What is left here is only
   * the wiring: what this generation stores, and what it inherits when it stores nothing.
   *
   * The measured pair is stated once, inside that component: the turbo LoRA at 8 steps is 465.4 s
   * against the 20-step control's 762.8 s at the SAME 6528 MiB peak (2026-08-07). The LoRA does not
   * lower the step count — load it and leave `steps` at 20 and you have bought a ~5%/step tax and
   * nothing else.
   */
  const loras = $derived(doc.loras ?? []);
  const loraMults = $derived(doc.lora_multipliers ?? []);

  /**
   * Which edges of this shot carry an attached frame.
   *
   * A frame is what can move the shot to `fl2va`, so it is the only thing the checkpoint panel has
   * anything to say about.
   */
  const framedClips = $derived(
    [shotClip]
      .filter((c): c is DirectorClip => !!c && h3HasFrameImage(c))
      .map((clip) => ({
        clip,
        edges: [
          ...(clip.mode === 'i2v' && clip.image_b64 ? ['first frame'] : []),
          ...(clip.h3_end_image ? ['last frame'] : [])
        ]
      }))
      .filter((f) => f.edges.length)
  );
  /** A pin on a project with a song is the one silently destructive combination — see the panel. */
  const projectHasSong = $derived(references.some((r) => r.id === H3_SOUNDTRACK_REF_ID));
  /**
   * Is this generation running on the PROJECT's LoRAs / step count rather than its own?
   *
   * The test matches `compileH3Timeline` exactly — an empty own-list inherits, a non-empty one wins
   * — because two places deciding what "overridden" means is how a card ends up describing a render
   * that is not the one being submitted. Passed DOWN to the knobs rather than re-derived there, so
   * the component never has to know which document shape it is looking at.
   */
  const inheritsLoras = $derived(!doc.loras?.length && (projectLoras ?? []).length > 0);
  const inheritsSteps = $derived(
    (doc.steps === undefined || doc.steps <= 0) && projectSteps !== undefined && projectSteps > 0
  );

  // ── The LLM entry point ─────────────────────────────────────────────────────

  /**
   * The beat whose `idea` / `gist` the entry point reads and writes.
   *
   * 🔴 The one-liner is a property of the SHOT — the LLM drafts the whole block from it — but there
   * is nowhere on `DirectorH3Generation` to put it, and inventing a wire field for a text box is
   * not what this stage is for. So it lives where it already lived: on a clip. The selected beat
   * when the selection is inside this shot, and the first beat otherwise, so the box is never dead
   * and never silently writes into a shot the author is not looking at.
   */
  const ideaClip = $derived(
    (anchorClip && gen.shots.some((s) => s.clip.id === anchorClip.id)
      ? anchorClip
      : gen.shots[0]?.clip) ?? null
  );

  /**
   * What the timeline shows for this {@link EngineVocab.renderUnit} while its gist is unwritten.
   *
   * Shared with `Timeline` through `h3GenerationGistFallback` rather than spelled
   * out here, so the box can say exactly what the band is currently drawing instead of a plausible
   * guess at it.
   */
  const gistFallback = $derived(h3GenerationGistFallback(gen));

  // ── What the prompt's own tokens mean, on hover ──────────────────────────────

  /**
   * Every `<Label N>` the block may name, with the thing it names.
   *
   * 🔴 Three sources, and all three are needed for the card to be true: the project references in
   * scope (which own the thumbnails), the keyframe pictures the PACKER synthesised (which take real
   * ordinals and are in no roster), and the block's own `subject_definitions`. A label in none of
   * them resolves to nothing on the wire — the model reads it as ordinary words — which is the one
   * case worth an amber line, because it looks completely normal in a wall of monospace.
   */
  /** The picture a subject's own definition cites — `…appearing in <Picture 1>`. First one wins. */
  const CITED_PICTURE_RE = /<(?:Picture|Video) \d+>/;

  const labelCards = $derived.by(() => {
    const out = new Map<string, H3TokenCard>();
    for (const k of keyframes) {
      // The picture itself lives on the reference the packer synthesised for it, not on the
      // declaration — the declaration is what the prompt has to SAY about the frame.
      const img = gen.references.find((r) => r.id === k.refId)?.image;
      out.set(k.label, {
        title: k.label,
        meta: `${capitalise(v.subUnit ?? '')} ${k.shotNumber} · ${k.edge} frame`,
        ...(img ? { image: assetSrc(img) } : {}),
        body: `A frame this ${v.subUnit} opens or closes on. The packer adds it to this render's reference list — it is not in the References panel and cannot be scoped.`
      });
    }
    for (const r of references) {
      const o = ordinals.find((x) => x.id === r.id);
      const name = r.label?.trim() || (r.id === H3_SOUNDTRACK_REF_ID ? 'project song' : 'unnamed');
      const card: H3TokenCard = {
        title: name,
        meta: `${r.kind} · used as: ${roleLabel(r.role)}`,
        ...(r.kind === 'image' && r.image ? { image: assetSrc(r.image) } : {}),
        ...(inScope(r.id)
          ? {}
          : { warn: `Out of scope for this ${v.renderUnit}, so this number resolves to nothing.` })
      };
      if (o?.label) out.set(o.label, card);
      if (o?.audioLabel)
        out.set(o.audioLabel, { ...card, meta: `${card.meta} · its own soundtrack` });
    }
    // 🔴 Subjects LAST, so a definition can borrow the picture it cites. A subject is a name the
    // block declares for itself, and its whole job is to tie that name to an image — so the card
    // that answers "who is <Subject 1>" should show the face, not just repeat the sentence the
    // caret is already sitting next to. Nothing collides: `<Subject N>` is its own label space.
    for (const d of parsed.subjectDefinitions) {
      const label = d.label.trim();
      if (!label) continue;
      const cited = CITED_PICTURE_RE.exec(d.text)?.[0];
      const from = cited ? out.get(cited) : undefined;
      out.set(label, {
        title: label,
        meta: cited ? `subject definition · ${cited}` : 'subject definition',
        ...(from?.image ? { image: from.image } : {}),
        body: d.text,
        // A definition naming a picture that is out of scope is worth saying plainly: the subject
        // reads as defined, and the thing it was defined AGAINST is not in the render.
        ...(cited && !from
          ? { warn: `${cited} is not in scope, so this subject has no picture.` }
          : {})
      });
    }
    return out;
  });

  /**
   * Every label that means something on the wire.
   *
   * 🔴 Derived from `labelCards` rather than rebuilt beside it, so the paint and the card can never
   * disagree: a label the block marks as naming nothing is exactly one `cardFor` will also call
   * nothing. `ordinals` is already scope-filtered (`h3Ordinals(gen.references)`), so an out-of-scope
   * reference contributes no key here and its number reads as dead — which is what it is.
   */
  const resolvedLabels = $derived(new Set(labelCards.keys()));

  /**
   * The card for one token of the block, or `null` where there is nothing worth saying.
   *
   * Everything it needs is already derived above — this is the lookup, not a second derivation.
   */
  function cardFor(t: H3Token): H3TokenCard | null {
    if (t.kind === 'label') {
      return (
        labelCards.get(t.text) ?? {
          title: t.text,
          meta: 'nothing defines this',
          warn: `No reference in scope carries this number and no subject_definitions line declares it, so the model reads it as ordinary words.`
        }
      );
    }
    if (t.kind === 'shot' && t.shot) {
      const span = beatSpans[t.shot - 1];
      if (!span) return null;
      const at = t.shot === 1 ? null : span.row.atSeconds;
      return {
        title: `${capitalise(v.subUnit ?? '')} ${t.shot}`,
        meta: [
          at === null ? `opens the ${v.renderUnit}` : `at ${formatCutTime(at)}`,
          `runs ${span.seconds.toFixed(2)}s`,
          h3ShotIsCut(span.row) ? 'cuts to it' : 'the camera keeps rolling'
        ].join(' · '),
        ...(span.row.body ? { body: span.row.body } : {})
      };
    }
    if (t.kind === 'time' && t.seconds !== undefined) {
      return {
        title: formatCutTime(t.seconds),
        meta: `${t.seconds.toFixed(3)}s into this ${v.renderUnit} · shown on the timeline`,
        ...(t.seconds >= gen.seconds
          ? {
              warn: `Past the end of this ${gen.seconds.toFixed(2)}s ${v.renderUnit}, so it is not a cue at all — the moment never arrives.`
            }
          : {})
      };
    }
    if (t.kind === 'header') {
      const name = t.text.trim().replace(/:$/, '');
      return SECTION_BLURB[name] ? { title: name, body: SECTION_BLURB[name] } : null;
    }
    if (t.kind === 'foreign') {
      return {
        title: t.text.trim(),
        meta: 'not a section we know',
        warn: 'It is in the block and it will be sent. No panel edits it, and no panel will overwrite it.'
      };
    }
    if (t.kind === 'marker') {
      return {
        title: t.text,
        meta: 'retention marker',
        body: 'A fixed English value the model was trained on. The picture/video table and the audio table are disjoint.'
      };
    }
    return null;
  }

  /** One line each, for the header hover. What the section is FOR, not what it contains. */
  const SECTION_BLURB: Record<string, string> = {
    subject_definitions:
      'Who and what the prompt may name, one per line, each under its own label.',
    summary: 'The task-type bracket and one paragraph saying what this render is.',
    retention_analysis: 'How much of each reference survives into the target.',
    detailed_description: 'The shots themselves. This is the section the model renders from.',
    integrated_multimodal_description:
      'The shots themselves, with the style folded into [Shot 1]. This is the section the model renders from.',
    overall_soundscape:
      'Ambience, action sounds, non-verbal human sounds. Not dialogue and not music.',
    non_diegetic_music: 'The score. `N/A` is a real answer and says so deliberately.'
  };

  /**
   * Hovering a cut time asks the timeline where it lands. Anything else clears the marker.
   *
   * A `[Shot N]` tag answers with its own cut time when it has one, because the tag is what the eye
   * lands on — the time beside it is the part you have to already know to look for.
   */
  const onEditorToken = (t: H3Token | null) => {
    if (!onhovertime) return;
    if (t?.kind === 'time' && t.seconds !== undefined) onhovertime(t.seconds);
    else if (t?.kind === 'shot' && t.shot) onhovertime(parsed.shots[t.shot - 1]?.atSeconds ?? null);
    else onhovertime(null);
  };

  // ══ The lens: ONE section's editor at a time, beside the block ═══════════════
  //
  // 🔴 **This replaced a `<details class="fold">` labelled "Break it down", shut by default, holding
  // all six section editors stacked.** The fold was the reason the panels could not do their job: a
  // section editor is a lens over a run of bytes in the block, and a lens you have to open — and
  // then scroll away from what it magnifies — is a second document. Nothing here is new machinery.
  // The same six snippets render, with the same handlers and the same `notes()` calls; only WHERE
  // they render moved, from a column under the prompt to a column beside it.

  /**
   * What each section currently holds — the dots and the counts in the index.
   *
   * 🔴 `h3SectionBadges` only measures headers that are PRESENT, which is the opposite of what the
   * index needs: a section nobody has written is precisely the one worth a row. So the row list is
   * built from the CHECKPOINT (below) and the badges only supply numbers where there are any.
   */
  const sectionBadges = $derived(h3SectionBadges(prompt, parsed));
  /**
   * Header + body bytes per section, for the budget bar.
   *
   * ⚠️ Spans and not badges: a badge's `chars` is the BODY alone, and every header is on the wire
   * too. A bar built from bodies would leave the schema itself unaccounted for and the segments
   * would never sum to the number printed above them.
   */
  const sectionSpans = $derived(h3SectionSpans(parsed));

  type LensRow = {
    /** The `H3SectionName` where there is one, or `style` — which is not a header. See below. */
    key: string;
    /** Drawn in mono, because with one exception these are the schema's own tokens. */
    name: string;
    gloss: string;
    /** Characters, except on the main body where the useful number is how many beats are in it. */
    count: string;
    empty: boolean;
  };

  /**
   * One row per editor this checkpoint has, **in the order the block writes them**.
   *
   * ⚠️ `style` sits directly before the main body rather than at the top, because that is where its
   * bytes are: on ref2va the style opening is the first line INSIDE `detailed_description`, ahead of
   * `[Shot 1]` — see `writeH3MainSection`, which takes it as an argument to that section's rewrite.
   * On the FL2VA family it has no line of its own at all, so the row leads to the explanation of
   * where it went rather than to a box.
   */
  const lensRows = $derived.by<LensRow[]>(() => {
    const badge = (n: H3SectionName) => sectionBadges.find((b) => b.name === n);
    const row = (n: H3SectionName, name = n as string): LensRow => ({
      key: n,
      name,
      gloss: SECTION_BLURB[n] ?? '',
      count: `${badge(n)?.chars ?? 0}`,
      empty: badge(n)?.empty ?? true
    });
    const rows: LensRow[] = [];
    if (isRef) {
      rows.push(row('subject_definitions'), row('summary'), row('retention_analysis'));
    }
    rows.push(
      isRef
        ? {
            key: 'style',
            name: 'style opening',
            gloss: 'One or two sentences on its own line, before [Shot 1].',
            count: `${(parsed.styleOpening ?? '').length}`,
            empty: !parsed.styleOpening?.trim()
          }
        : {
            key: 'style',
            name: 'style opening',
            gloss: 'Folded inside [Shot 1] on this checkpoint, with nothing marking where it ends.',
            count: 'in [Shot 1]',
            empty: true
          }
    );
    rows.push({
      ...row(bodyField),
      count: `${parsed.shots.length} ${parsed.shots.length === 1 ? v.subUnit : v.subUnitPlural}`
    });
    rows.push(row('overall_soundscape'), row('non_diegetic_music'));
    return rows;
  });

  /**
   * Which section the lens is showing.
   *
   * Two signals, and the caret is the one that matters:
   *
   * - **The caret.** `H3BlockEditor` reports the section it is standing in (`onsection`) and, inside
   *   the main body, the `[Shot N]` as well (`onshot`) — so putting the cursor in
   *   `retention_analysis` makes this the retention editor, and putting it in a shot makes it that
   *   beat's. Clicking a header in the block is the same gesture: the click places the caret, which
   *   is what reports.
   * - **A click** on the index, for reaching a section without going through the text.
   *
   * ⚠️ Stamped with the generation and validated against the CURRENT rows, for the same two reasons
   * `beatFocus` is: the composer is not remounted on a shot switch, and the row set changes with the
   * checkpoint — a `retention_analysis` pick carried into an fl2va generation would show an editor
   * that renders nothing at all.
   */
  let lensFor = $state<{ id: string; key: string }>({ id: '', key: '' });
  const lensKey = $derived(
    lensFor.id === doc.id && lensRows.some((r) => r.key === lensFor.key) ? lensFor.key : bodyField
  );
  const setLens = (key: string) => (lensFor = { id: doc.id, key });
  const lensRow = $derived(lensRows.find((r) => r.key === lensKey) ?? null);

  // ── The two budgets in the header ────────────────────────────────────────────

  /**
   * One segment per beat, measured against the MODEL's ceiling rather than against this render.
   *
   * The slack is the point: `gen.seconds / H3_MAX_SECONDS` as a number says how full this render is,
   * but only a bar says how much room a new beat has to land in.
   */
  const lengthSegs = $derived(
    beatSpans.map((b, i) => ({
      key: i,
      seconds: b.seconds,
      cut: i > 0 && h3ShotIsCut(b.row)
    }))
  );
  const lengthSlack = $derived(Math.max(0, H3_MAX_SECONDS - gen.seconds));
  const cutCount = $derived(parsed.shots.filter((s, i) => i > 0 && h3ShotIsCut(s)).length);

  /**
   * One segment per SECTION, header included, in document order.
   *
   * 🔴 Measured on `sent` and not on `prompt`, so the blank-block case is honest: with the box empty
   * the composed seed is what would render, it has no parsed sections, and the whole bar is
   * therefore the one "not in any section" segment. That is exactly what is happening.
   */
  const promptSegs = $derived(
    sectionSpans.map((s) => ({
      name: s.name,
      chars: Math.max(0, s.end - s.start),
      main: s.name === bodyField
    }))
  );
  /**
   * Bytes no section covers — the alignment line, the gaps between headers, an unparsed run, or a
   * blank block's composed stand-in. Drawn, because they are sent.
   */
  const promptOther = $derived(
    Math.max(0, sent.length - promptSegs.reduce((t, s) => t + s.chars, 0))
  );
  const promptLeft = $derived(PROMPT_CHAR_LIMIT - sent.length);
  const promptOver = $derived(sent.length > PROMPT_CHAR_LIMIT);
  const bodyShare = $derived(
    sent.length ? Math.round(((promptSegs.find((s) => s.main)?.chars ?? 0) / sent.length) * 100) : 0
  );

  // ── How wide THIS COMPONENT is ───────────────────────────────────────────────

  /**
   * 🔴 A `ResizeObserver` and deliberately not a viewport media query. This pane lives inside a
   * column whose width the workspace decides — rail open, inspector open, a second column beside it
   * — so the same window holds it at 1400px and at 380px. A media query would stack a wide pane and
   * leave a narrow one in two columns, which is the failure in both directions at once.
   */
  let paneEl = $state<HTMLElement | null>(null);
  let paneWidth = $state(0);
  $effect(() => {
    const el = paneEl;
    if (!el) return;
    const ro = new ResizeObserver(([entry]) => {
      if (entry) paneWidth = entry.contentRect.width;
    });
    ro.observe(el);
    return () => ro.disconnect();
  });
  /** ⚠️ Unmeasured reads as WIDE — `0 < 1000` would stack the pane for the first frame every time. */
  const narrow = $derived(paneWidth > 0 && paneWidth < 1000);

  // ── The idea / rework zone ───────────────────────────────────────────────────

  /**
   * Is the idea zone open?
   *
   * Shut once a prompt exists, and shut to ONE LINE rather than to a chevron: the idea is what this
   * prompt was written FROM, so it stays legible as provenance while its two boxes — both of which
   * REPLACE the whole prompt — stop occupying the top of the pane under a cursor.
   *
   * ⚠️ Stamped with the generation, like `touched` and `beatFocus`: the composer is not remounted
   * when the author switches shots, so an unstamped flag would carry one shot's state into another.
   */
  let ideaFor = $state<{ id: string; open: boolean } | null>(null);
  const ideaOpen = $derived(ideaFor?.id === doc.id ? ideaFor.open : !prompt.trim());
  const setIdeaOpen = (open: boolean) => (ideaFor = { id: doc.id, open });
  const ideaText = $derived(ideaClip?.idea?.trim() ?? '');
</script>

<!--
  Every section's notes, drawn the same way in one place — because the TIER is the whole message
  and three hand-rolled `{#each}` blocks with three different class expressions is how a red one
  ends up looking amber. `unwritten` is a ghost line under the empty control: it says what the
  section is for and what leaving it empty costs, in muted text, with no icon and no red. `warn` is
  amber. `error` is the only red left, and it is the only tier that blocks the render.
-->
<!--
  ＋ for a reference only THIS render unit carries.

  🔴 The kind is asked for up front because it is not a display detail — `defaultRoleFor(kind)`
  sets the role, the role is what koblem derives the prompt's task-type bracket from, and the three
  kinds have separate engine budgets (2 audio refs at 2-15 s each summing to <=15 s, a budget
  SEPARATE from the reference-video one). A single "＋ Reference" that guessed image would quietly
  make the audio case unreachable from here.

  The FILE is asked for FIRST — the picker opens on click and a dismissed dialog stores nothing.
  This used to create the row and send the author to the References panel to fill it, which is how a
  payload-less reference ended up sitting in a production document (2026-08-09): unfillable from
  where it was made, and refused by `build_references` if it was ever scoped in.
-->
{#snippet addScoped()}
  {#if onaddscopedreference}
    <span class="tl-meta">＋ for this {v.renderUnit} only:</span>
    {#each [['image', 'picture'], ['video', 'clip'], ['audio', 'sound']] as [kind, word] (kind)}
      <button
        class="miniai"
        title="Pick a file, then it is added to the project list and scoped to this {v.renderUnit} alone — every other {v.renderUnit} is pinned to what it already carries, so this one cannot drift into them. Cancelling the picker adds nothing."
        onclick={() => onaddscopedreference?.(kind as H3ReferenceKind)}>{word}</button
      >
    {/each}
  {/if}
{/snippet}

{#snippet notes(field: string)}
  {#each issuesFor(field) as issue (issue.message)}
    <p class="hint {issueClass(issue.level)}" style="margin-top:6px">{issue.message}</p>
  {/each}
{/snippet}

<!--
  ══ The header: who this is, and the two budgets it is spending ═══════════════════════
  🔴 **The budgets are BARS because the numbers alone were being read as decoration.** Both are
  hard ceilings with very different textures — `H3_MAX_SECONDS` is what the model renders in one
  call, `PROMPT_CHAR_LIMIT` is a refusal — and both used to be a fragment of grey text: the length
  at the end of the identity line, the character count in the prompt's own corner where nothing
  about it was visible until it was already over. Segmented, they answer the question the number
  cannot: WHICH beat is eating the render, and WHICH section is eating the 7,000.
-->
{#snippet secHeader()}
  <div class="hgrid" class:stack={narrow}>
    <div class="hid">
      <div class="gen-h">
        <!-- Siblings, not a span nested in the `<b>`: nested, "of 15" inherited the bold and the
             mono face and its only separator was a literal space, so it read as part of the number. -->
        <span class="unit">{capitalise(v.renderUnit)} {genIndex + 1}</span>
        <span class="of">of {genCount}</span>
        <span
          class="task"
          title="The checkpoint this renders on. Derived from what is in scope — fl2va and ref2va are different weight sets, not a flag."
          >{gen.task}</span
        >
        <!-- Counted off the PROMPT, which is the only place beats exist. -->
        <span class="tl-meta"
          >{parsed.shots.length}
          {parsed.shots.length === 1 ? v.subUnit : v.subUnitPlural}</span
        >
      </div>
      <!-- The gist is the identity of this render unit — the one line the timeline draws for it —
           so it sits with the name rather than three sections away beside the LLM buttons. -->
      <div class="hgist">{@render secGist()}</div>
    </div>

    <div class="budget">
      <div class="blab">Length</div>
      <div class="bnum" class:over={gen.overBudget}>
        <b>{gen.seconds.toFixed(2)}</b> / {H3_MAX_SECONDS.toFixed(2)} s
      </div>
      <div
        class="bbar"
        class:over={gen.overBudget}
        title="Each {v.subUnit} at the length it renders, against the {H3_MAX_SECONDS.toFixed(
          2
        )}s a single {v.renderUnit} can render. A hard edge is a {v.subUnitCut}; a soft one is a {v.subUnit} the camera rolls through."
      >
        {#each lengthSegs as seg (seg.key)}
          <span class="bseg" class:cut={seg.cut} style="flex:{Math.max(seg.seconds, 0.01)}"></span>
        {/each}
        {#if lengthSlack > 0}<span class="bslack" style="flex:{lengthSlack}"></span>{/if}
      </div>
      <div class="blegend">
        {beatRows.length}
        {beatRows.length === 1 ? v.subUnit : v.subUnitPlural} · {cutCount}
        {cutCount === 1 ? v.subUnitCut : v.subUnitCutPlural}
      </div>
    </div>

    <div class="budget">
      <div class="blab">Prompt</div>
      <div class="bnum" class:over={promptOver}>
        <b>{sent.length.toLocaleString()}</b> / {PROMPT_CHAR_LIMIT.toLocaleString()} chars
      </div>
      <!-- ⚠️ Segments are HEADER + BODY, so they sum to the number above them. MiniMax refuse over
           the limit rather than trimming — truncating would silently drop `non_diegetic_music`,
           which is the last section — so the whole bar goes red rather than one segment. -->
      <div class="bbar" class:over={promptOver} title="Each section's share, header included.">
        <!-- ⚠️ Keyed by POSITION. A hand-written or pasted block can carry the same header twice —
             the parser reports both, because both are in the block and both are sent — and a keyed
             each on the name would throw on the duplicate and take the whole pane down. -->
        {#each promptSegs as seg, i (i)}
          <span
            class="bseg"
            class:main={seg.main}
            style="flex:{Math.max(seg.chars, 1)}"
            title="{seg.name} · {seg.chars.toLocaleString()}"
          ></span>
        {/each}
        {#if promptOther > 0}
          <span
            class="bseg other"
            style="flex:{promptOther}"
            title="{promptOther.toLocaleString()} characters in no section — the alignment line, the gaps, or a block the fields have not parsed"
          ></span>
        {/if}
        {#if promptLeft > 0}<span class="bslack" style="flex:{promptLeft}"></span>{/if}
      </div>
      <div class="blegend">
        {bodyShare}% is the {v.subUnitPlural} ·
        {#if promptOver}{(-promptLeft).toLocaleString()} over{:else}{promptLeft.toLocaleString()}
          left{/if}
      </div>
    </div>
  </div>
  {#if silentBeats.length}
    <!-- ── The finding surfacing, NOT a regression ────────────────────────────────────────
         🔴 Measured 2026-08-08: three `[Shot N] At MM:SS.mmm` marks with no transition verb produced
         NO scene change at all across 362 frames (ffmpeg `scdet` max 0.71, where a hard cut scores
         >10) and still landed their actions within ~0.5 s. So a verbless mark is a BEAT.
         Every H3 project built before that date was described by a `<select>` that read
         `cutVerb ?? clip.h3_cut_verb ?? H3_CUT_VERBS[0]` and therefore said "the camera cuts to" on
         every untouched beat, while the block held no verb and the render was a beat. This line is
         the correction arriving, not something that changed underneath the author. -->
    <p class="hint" style="margin:0 0 8px">
      {silentBeats.length} of these {v.subUnitPlural} do not cut — <b>the camera keeps rolling</b>
      through {silentBeats.length === 1 ? 'it' : 'them'}.
    </p>
  {/if}
{/snippet}

{#snippet secNotices()}
  <!-- ⚠️ No over-budget paragraph. The Length bar in the header already goes red with the number in
       red beside it, and the timeline block and shot list say it too — a fourth copy in prose was
       the grey-text-nobody-reads pattern this pane exists to remove. -->
  {#if gen.belowDocumentedFloor}
    <!-- 🔴 Said once, not corrected. Until 2026-08-07 this generation was silently LENGTHENED to
         4.46s and the last shot absorbed the difference. 107 frames is only the grid-snap of
         MiniMax's documented 4-second duration guideline — the model renders far shorter clips,
         and the video VAE's own floor is 22 frames (0.92s). -->
    <p class="hint" style="margin:0 0 8px">
      This renders as <b>{gen.seconds.toFixed(2)}s</b>, under the {H3_DOCUMENTED_MIN_SECONDS.toFixed(
        2
      )}s MiniMax document as their shortest output. It is rendered at the length you wrote — not
      lengthened — but quality below 4s is outside the range they validated.
    </p>
  {/if}

  {#if reachableNote}
    <!-- 🔴 Separate from `overBudget` above, and deliberately not merged with it: that one is the
         MODEL's ceiling and is a property of the timeline. This one is what the box can run TODAY,
         which moves with VRAM and the loaded checkpoint. The timeline has not been re-packed and
         will not be — see the prop doc. -->
    <p class="hint err" style="margin:0 0 8px">⚠ {reachableNote}</p>
  {/if}

  <!-- ── The prompt was rewritten under you ──────────────────────────────────────────────
       🔴 At the TOP of the pane rather than beside the references, because the cause and the effect
       are in different places: the numbering moves when a reference is scoped, a beat image is
       attached, or the roster is reordered in a panel this component cannot see — and the thing
       that changed is the block. A note in a fold that happened to be shut would be a rewrite
       nobody was told about.

       ⚠️ Dropped on a shot switch (see the effect), so this always describes the block on screen. -->
  {#if renumberUndo && renumberUndo.id === doc.id}
    <div class="warn-bar" style="margin:0 0 8px">
      <span class="sev warn">renumbered</span>
      <span style="flex:1">{renumberUndo.note}</span>
      <button
        class="btn sm"
        title="Put the block back exactly as it was before the numbers moved. The reference list is not touched — undoing the text without undoing the cause is the point, so you can decide what the sentences should say."
        onclick={() => {
          const back = renumberUndo;
          renumberUndo = null;
          if (back) onprompt(back.text);
        }}>Undo</button
      >
      <button class="btn sm ghost" onclick={() => (renumberUndo = null)}>Dismiss</button>
    </div>
  {/if}
{/snippet}

<!-- ── The checkpoint, and the frames that decide it ────────────────────────────────────
     🔴 **On the RENDER UNIT, because that is what a checkpoint is a property of.** The pin-vs-
     keyframe control already existed with these numbers on `ShotDetailsPane` — but that pane edits
     one BEAT, and one beat pinning a frame moves the whole generation to `fl2va` and strips every
     reference from all of them. The decision was being made where its consequence was invisible.

     ⚠️ Derived and not offered as a choice: `ref2va` and `fl2va` are disjoint weight sets, so there
     is no request that is a bit of both and a picker would be offering a state that cannot render.
     What IS a choice is how an attached frame is handed over, which is the control below. -->
{#snippet secCheckpoint()}
  <div class="sec">
    <div class="lbl-row">
      <span>Checkpoint</span><span class="tl-meta">derived from what is in scope</span>
    </div>
    <p class="hint" style="margin:0 0 8px">
      This {v.renderUnit} renders on <b>{gen.task}</b> —
      {#if gen.task === 'ref2va'}
        {scopedIds.length}
        {scopedIds.length === 1 ? 'reference is' : 'references are'} in scope. References win over everything
        else.
      {:else if gen.task === 'fl2va'}
        a {v.subUnit} pins a frame on the wire, which takes precedence over references — and takes
        <b>no references at all</b>.
      {:else}
        nothing is in scope and no frame is pinned, so it is plain text.
      {/if}
    </p>

    {#if framedClips.length}
      {#each framedClips as f (f.clip.id)}
        {@const mode = h3FrameMode(f.clip)}
        <div class="entry" style="border-top:none;padding-top:0">
          <div class="lbl-row">
            <span>{capitalise(v.renderUnit)} frame</span>
            <span class="tl-meta">{f.edges.join(' + ')}</span>
          </div>
          <div class="modes" style="margin-bottom:6px">
            <button
              class="mode"
              class:on={mode === 'keyframe'}
              onclick={() => onclip(f.clip.id, { h3_frame_mode: 'keyframe' })}
              ><b>Open on this image</b><small
                >Stays on <b>ref2va</b>. The image reaches the model as a numbered &lt;Picture N&gt;
                with prose, so every reference{projectHasSong ? ' and the song slice' : ''} survives and
                there is no checkpoint flip. Measures <b>0.776</b> frame-0 similarity.</small
              ></button
            >
            <button
              class="mode"
              class:on={mode === 'pin'}
              onclick={() => onclip(f.clip.id, { h3_frame_mode: 'pin' })}
              ><b>Match this frame exactly</b><small
                ><b>0.968</b> frame-0 similarity — but it renders on <b>fl2va</b>, which drops every
                reference{projectHasSong ? ' and this shot’s slice of the song' : ''}, isolates this
                {v.renderUnit} on both sides, and costs a ~{H3_CHECKPOINT_FLIP_SECONDS}s checkpoint
                flip.</small
              ></button
            >
          </div>
          {#if mode === 'pin' && projectHasSong}
            <!-- 🔴 The one combination that is silently destructive: `fl2va` has no audio-reference
                 path at all, so the slice is not weakened, it is ABSENT — and the render comes back
                 looking fine, scored by something the model invented. -->
            <p class="hint err" style="margin:0">
              ⚠ This project has a song, and an exact pin renders on <b>fl2va</b>, which takes no
              audio reference. This {v.renderUnit} would be scored by whatever the model invents, not
              by your track. "Open on this image" keeps both.
            </p>
          {/if}
        </div>
      {/each}
    {:else}
      <p class="hint" style="margin:0">
        Nothing here opens or closes on an attached image, so nothing can move it off <b>ref2va</b>.
        Frames are attached on a {v.subUnit}'s own Details tab.
      </p>
    {/if}
  </div>
{/snippet}

<!-- ── The join with the previous render unit: only at a boundary, the only seam there is ──── -->
{#snippet secJoin()}
  {#if continuation}
    <div class="sec">
      <div class="lbl-row"><span>Joins the previous {v.renderUnit}</span></div>
      <details class="why" style="margin-bottom:8px">
        <summary class="tl-meta">None of these is frame-exact</summary>
        <p class="hint" style="margin:6px 0 0">
          MiniMax describe Ref2VA as treating the incoming video as reference material rather than a
          pixel-aligned edit source — it can resynthesize motion, and there is no denoising-strength
          control. Expect a join, not a splice; the cleanest result is still to put this boundary
          where you wanted a cut.
        </p>
      </details>
      <div class="modes" style="margin-bottom:8px">
        {#each CONTINUATION_MODES as m (m)}
          <button
            class="mode"
            class:on={continuation.mode === m}
            onclick={() => setContinuation({ mode: m })}
            ><b
              >{m === 'latents' ? 'Carry latents' : m === 'frame' ? 'Carry a frame' : 'Hard cut'}</b
            ><small>{MODE_BLURB[m]}</small></button
          >
        {/each}
      </div>
      {#if continuation.mode !== 'none' && joinPinNote}
        <!-- 🔴 WHICH take this joins, not just that it joins. The seam is made against ONE take of
             the previous render unit — the selected one — and until this line existed the author
             could pick a different take there with nothing on this panel changing, and no way to
             tell from here what the next render would actually pick up from.

             ⚠️ It describes the NEXT render, not the footage on disk. Existing frames keep the join
             they were rendered with; the Joins panel on the Details tab is what says when those two
             have come apart. -->
        <p class="hint" style="margin:0 0 8px">🔗 {joinPinNote}</p>
      {/if}
      {#if continuation.mode !== 'none'}
        <!-- 🔴 ONE KNOB, not two. There used to be a "Tail handed over" dropdown beside this,
             offering 56..345 frames. It was a WanGP field: the engine sets the carry's depth from
             k and nothing else, and k=7 IS 22 pixel frames. Whatever the dropdown said went out on
             the wire and came back as a warning saying it did not apply. -->
        <div class="rowgrid">
          <label class="field" style="margin:0"
            ><span>Pin depth (k)</span><input
              class="inp"
              type="number"
              min="1"
              max="17"
              step="1"
              value={continuation.k}
              oninput={(e) => setContinuation({ k: parseInt(e.currentTarget.value, 10) || 7 })}
            /></label
          >
        </div>
        <p class="hint" style="margin-top:5px">
          7 latent frames is 22 pixel frames and the measured saturation point — k=2 scores 6%, k=7
          91%, k=12 91%.
        </p>
        <label class="inline-row" style="gap:6px;margin-top:6px">
          <input
            type="checkbox"
            checked={continuation.anchor}
            onchange={(e) => setContinuation({ anchor: e.currentTarget.checked })}
            style="accent-color:var(--accent)"
          />
          <span class="tl-meta">Also pin an anchor frame on top of the tail</span>
        </label>
      {/if}
      <!-- ⚠️ "Merge" named TWO different operations two panels apart, and they act on different
           objects: the timeline's Merge joins two BEATS into one, this one dissolves a RENDER
           BOUNDARY and leaves every beat exactly where it is. Renamed rather than reworded, because
           the confusion was the verb. -->
      <div class="inline-row" style="margin-top:8px">
        <button
          class="btn sm"
          disabled={!!mergeBlocked}
          title="Removes the boundary so this {v.renderUnit} and the one before it render as a single call. It does not join any {v.subUnitPlural} together — the timeline's Merge is the one that does that."
          onclick={onmerge}>Dissolve this boundary into the previous {v.renderUnit}</button
        >
        {#if mergeBlocked}<span class="hint">{mergeBlocked}</span>{/if}
      </div>
    </div>
  {/if}
{/snippet}

<!-- ── Style ─────────────────────────────────────────────────────────────────────────── -->
{#snippet secStyle()}
  <div class="sec">
    <!-- Every section header carries a ✓ or a dash, so an unwritten one is visible from the fold's
         own summary without opening it. It is a STATE marker, not a verdict — an unwritten section
         is the normal look of a fresh shot, which is why it is a dash and not a red note. -->
    <div class="lbl-row">
      <span>Style opening</span><span class="tl-meta"
        >{isRef ? (parsed.styleOpening ? '✓' : '— not written') : 'inside [Shot 1]'}</span
      >
    </div>
    {#if isRef}
      <textarea
        class="inp"
        rows="2"
        value={parsed.styleOpening ?? ''}
        onfocus={(e) => claimProse(e.currentTarget, (v) => writeShots(placements, v))}
        oninput={(e) => writeShots(placements, e.currentTarget.value)}
        placeholder="The target video is in realistic photographic style."></textarea>
    {:else}
      <!-- 🔴 Not a box on this checkpoint, and not an oversight. The FL2VA family folds the style
           INSIDE [Shot 1] with nothing marking where it ends, so reading it back means guessing
           where the style stops and the shot begins — and guessing wrong silently moves a sentence
           of the author's prose from one to the other on every edit. `parseH3Block` refuses to
           guess for the same reason, so the honest surface is to point at where it lives. -->
      <p class="hint" style="margin:0">
        On this checkpoint the style is written <b>inside [Shot 1]</b> with nothing marking where it
        ends, so there is no box for it: write it as the first sentence of that {v.subUnit}'s body.
      </p>
    {/if}
  </div>
{/snippet}

{#snippet secDefinitions()}
  {#if isRef}
    <!-- ── subject_definitions ────────────────────────────────────────────────────────── -->
    <div class="sec">
      <div class="lbl-row">
        <span>Subject definitions</span><span class="tl-meta"
          >{parsed.subjectDefinitions.length || '— none'}</span
        >
      </div>
      <details class="why" style="margin-bottom:8px">
        <summary class="tl-meta">When a picture needs its own line</summary>
        <p class="hint" style="margin:6px 0 0">
          Every label the prompt names has to be defined here first. An image used only for identity
          or style gets <b>no</b> standalone retention entry — cite it inside the subject that uses it
          ("…appearing in &lt;Picture 1&gt;").
        </p>
      </details>
      {#each parsed.subjectDefinitions as d, i (i)}
        <!-- `{@const}` has to be an immediate child of the block, not of the row div below. -->
        {@const writeDef = (v: string) => {
          const next = [...parsed.subjectDefinitions];
          next[i] = { ...d, text: v };
          setDefs(next);
        }}
        <div class="defrow">
          <div class="rowhead">
            <!-- Blank when the line carried no recognisable label. Kept and shown rather than
                 discarded: dropping an author's sentence over a missing pair of angle brackets is
                 how a definition disappears without anybody noticing. -->
            <span class="lab">{d.label || '—'}</span>
            <button
              class="btn sm danger"
              title="Remove this subject. Every later <Subject N> is renumbered through the whole block."
              onclick={() => removeSubject(i)}>✕</button
            >
          </div>
          <textarea
            class="inp prosebox"
            rows="2"
            value={d.text}
            onfocus={(e) => claimProse(e.currentTarget, writeDef)}
            oninput={(e) => writeDef(e.currentTarget.value)}
            placeholder="is the harbour captain: weathered face, grey beard, navy peacoat, appearing in <Picture 1>."
          ></textarea>
        </div>
      {/each}
      <div class="inline-row" style="gap:6px;flex-wrap:wrap">
        <button
          class="btn sm"
          onclick={() =>
            setDefs([...parsed.subjectDefinitions, { label: nextSubjectLabel(), text: '' }])}
          >＋ Subject</button
        >
        {#each ordinals as o (o.id)}
          {@const defined = parsed.subjectDefinitions.some((d) => d.label === o.label)}
          <button
            class="btn sm"
            disabled={defined}
            title={o.labelText ? `your note: ${o.labelText}` : undefined}
            onclick={() => setDefs([...parsed.subjectDefinitions, { label: o.label, text: '' }])}
            >＋ {o.label}</button
          >
          {#if o.audioLabel}
            {@const audioDefined = parsed.subjectDefinitions.some((d) => d.label === o.audioLabel)}
            <button
              class="btn sm"
              disabled={audioDefined}
              onclick={() =>
                setDefs([...parsed.subjectDefinitions, { label: o.audioLabel!, text: '' }])}
              >＋ {o.audioLabel}</button
            >
          {/if}
        {/each}
      </div>
      {@render notes('subject_definitions')}
    </div>
  {/if}
{/snippet}

<!-- ── summary ──────────────────────────────────────────────────────────────────────────
     🔴 **This is the one thing called "Summary" on this engine, and that took a decision.** The
     word named two different objects two tabs apart: this, the MiniMax `summary:` schema section
     with its task-type bracket, and `clip.gist`, the plain-language line the timeline draws. The
     schema section is the one the owner asked for beside the LLM entry — it is LLM-written and
     human-editable, and it is the piece of the prompt that says what this shot IS. `gist` keeps
     working (the LLM still writes it, the shot list still draws it) but it is no longer offered
     here under a name that collides.

     ⚠️ Ref2VA only: the FL2VA family's three-section schema has no `summary:` at all. It used to be
     rendered inside the LLM entry, beside Draft and Rework — it moved into the lens when that zone
     collapsed to one line, because a section editor that only exists while a collapsible zone
     happens to be open is the failure this restructure removed one level up. -->
{#snippet secSummary()}
  {#if isRef}
    <div class="sec">
      <div class="lbl-row">
        <span>Summary</span><span class="tl-meta"
          >the prompt's own <code>summary:</code>
          · {parsed.summary ? '✓' : '— not written'}</span
        >
      </div>
      <div class="chiprow" style="margin-bottom:8px">
        {#each H3_TASK_TYPES as t (t)}
          <button
            class="chip"
            class:on={parsed.taskTypes.includes(t)}
            title={TASK_TYPE_HELP[t]}
            onclick={() => toggleTaskType(t)}>{t}</button
          >
        {/each}
      </div>
      <details class="why" style="margin-bottom:8px">
        <summary class="tl-meta">What the bracket means</summary>
        <p class="hint" style="margin:6px 0 0">
          These become the bracket the summary opens with. Pick every relationship that applies —
          but the mere presence of a clip does not make it one: a reference video that only supplies
          camera movement or cutting rhythm is <b>reference generation</b>, not video editing.
          <br />🔴 <b>The audio ones pair with the video ones</b> (GUIDE-ref:145): editing a source
          video whose original audio stays audible is
          <code>[video editing + audio reuse]</code>, and continuing one where the new audio only
          carries on the old track's character rather than copying it is
          <code>[video continuation + audio reference]</code>. Neither is added for you. ⚠️ The
          order is normalised, so a hand-written bracket comes back sorted.
        </p>
      </details>
      <textarea
        class="inp"
        rows="3"
        value={parsed.summary ?? ''}
        onfocus={(e) => claimProse(e.currentTarget, setSummary)}
        oninput={(e) => setSummary(e.currentTarget.value)}
        placeholder="The target video continues directly from the end of <Video 1>. <Subject 1> …"
      ></textarea>
      {@render notes('summary')}
    </div>
  {/if}
{/snippet}

<!-- ── The shot's own one-liner ────────────────────────────────────────────────────────
     🔴 **`DirectorH3Generation.gist` — DISPLAY ONLY. It is never compiled into the request.** Every
     other field on that type is on the wire, so the loud note is earned: see the field's own doc in
     `client.ts`, and `compile.ts`, which builds `H3RequestGeneration` from an explicit list this is
     absent from.

     ⚠️ **Not the `summary:` section, which is a lens panel and IS sent.** The word named two objects
     two tabs apart before the restructure and the collision cost a decision; the distinction that
     survives it is *what renders* versus *what the timeline draws*. This is the second, which is why
     it is in the HEADER — it is this render unit's name, the line the timeline itself draws — while
     `summary:` is in the lens with the rest of the schema.

     🔴 It is per SHOT — the owner's words, 2026-08-08: *"gist is the gist of the ENTIRE PROMPT, just
     some display text so you can understand what happens in this 'shot' (generation)"*. `clip.gist`
     is untouched and still labels one BEAT, which is what a list of beats wants.

     The action reuses the engine-neutral `gist_only` path (`ltx_video.rs:1379` short-circuits to a
     plain summariser before any H3 branch), so it takes the block as arbitrary prompt text and needs
     no scope fields at all. -->
{#snippet secGist()}
  <div class="sec">
    <div class="lbl-row">
      <span>What happens in this {v.renderUnit}</span><span class="tl-meta"
        >the timeline's label · <b>not sent to the model</b></span
      >
    </div>
    <div class="inline-row">
      <input
        class="inp"
        value={doc.gist ?? ''}
        oninput={(e) => onupdate({ gist: e.currentTarget.value })}
        placeholder={gistFallback || `one line: what a viewer would say happens here`}
      />
      {#if onsummarise}
        <button
          class="btn sm"
          disabled={promptBusy || !sent.trim()}
          title="Reads this {v.renderUnit}'s WHOLE prompt — every {v.subUnit} in it — and writes one plain-language sentence. It does not touch the prompt, and the sentence is never sent to the engine."
          onclick={() => onsummarise?.()}
          >{promptBusy
            ? '…'
            : doc.gist?.trim()
              ? '↻ Re-summarise'
              : `Summarise this ${v.renderUnit}`}</button
        >
      {/if}
    </div>
    {#if !doc.gist?.trim() && gistFallback}
      <p class="hint" style="margin-top:5px">
        Until you write one the timeline shows
        <em>“{gistFallback.slice(0, 60)}{gistFallback.length > 60 ? '…' : ''}”</em>, taken from the
        first {v.subUnit}.
      </p>
    {/if}
  </div>
{/snippet}

<!-- ── retention_analysis ─────────────────────────────────────────────────────────── -->
{#snippet secRetention()}
  {#if isRef}
    <div class="sec">
      <div class="lbl-row">
        <span>Retention analysis</span><span class="tl-meta"
          >{parsed.retentionAnalysis.length || '— none'}</span
        >
      </div>
      <details class="why" style="margin-bottom:8px">
        <summary class="tl-meta">What a marker claims</summary>
        <p class="hint" style="margin:6px 0 0">
          How much of each reference survives into the target. The markers are fixed English values
          the model was trained on, and the two vocabularies are <b>disjoint</b> — picture and video labels
          take one set, audio labels the other.
        </p>
      </details>
      {#each parsed.retentionAnalysis as e, i (i)}
        {@const patch = (p: Partial<H3RetentionEntry>) => {
          const next = [...parsed.retentionAnalysis];
          next[i] = { ...e, ...p };
          setRetention(next);
        }}
        <!-- The line reads `<Label N> (role): marker - text`, and the row is laid out in that order:
             the two pickers that decide WHICH claim this is on the head line with the delete, and
             the two things the author writes below it, each on the full width. -->
        <div class="ret">
          <div class="rowhead">
            <!-- The entry's own label is always an option, even when it names something no longer
                 in scope: a select that silently re-pointed the row at the first label in the list
                 would rewrite a retention claim about one asset into a claim about another. -->
            <select
              class="inp"
              value={e.label}
              onchange={(ev) => patch({ label: ev.currentTarget.value })}
            >
              {#each labelOptions(e.label) as l (l)}<option value={l}>{l}</option>{/each}
            </select>
            <select
              class="inp"
              value={e.marker}
              onchange={(ev) => patch({ marker: ev.currentTarget.value })}
            >
              {#each markersFor(e.label, e.marker) as m (m)}
                <option value={m}>{m}</option>
              {/each}
            </select>
            <button
              class="btn sm danger"
              onclick={() => setRetention(parsed.retentionAnalysis.filter((_, j) => j !== i))}
              >✕</button
            >
          </div>
          <label class="field">
            <span>role</span>
            <input
              class="inp"
              value={e.role_note ?? ''}
              oninput={(ev) => patch({ role_note: ev.currentTarget.value })}
              placeholder="appears in the video"
            />
          </label>
          <label class="field">
            <span>what survives</span>
            <textarea
              class="inp prosebox"
              rows="2"
              value={e.text}
              onfocus={(ev) => claimProse(ev.currentTarget, (v) => patch({ text: v }))}
              oninput={(ev) => patch({ text: ev.currentTarget.value })}
              placeholder="identity, face, hair, wardrobe and props are retained"></textarea>
          </label>
        </div>
      {/each}
      <button
        class="btn sm"
        disabled={availableLabels.length === 0}
        onclick={() =>
          setRetention([
            ...parsed.retentionAnalysis,
            {
              label: availableLabels[0],
              marker: availableLabels[0].startsWith('<Audio') ? 'reference' : 'fully_preserved',
              text: ''
            }
          ])}>＋ Retention line</button
      >
      {@render notes('retention_analysis')}
    </div>
  {/if}
{/snippet}

<!-- ── The shape of the render ──────────────────────────────────────
     🔴 **Directly under the block, in the same column.** It shipped for one commit inside the
     break-it-down fold with `secBeats` — which put the one control that shows what the render looks
     like behind a `<details>` that was shut by default. The fold is gone entirely now; this
     describes the whole render unit, so it stays with the prompt while the per-beat editor it
     selects into lives in the lens beside it.

     Picking a cell also points the lens at the main body, because that is where the beat's own
     editor is and a selector that selects something you cannot then edit is half a control. -->
{#snippet secShape()}
  <div class="sec">
    <div class="lbl-row">
      <span>{capitalise(v.subUnitPlural ?? '')} in this {v.renderUnit}</span>
      <span class="tl-meta">{beatRows.length} · written as <code>[Shot N]</code></span>
    </div>
    <BeatRibbon
      rows={beatRows}
      totalSeconds={gen.seconds}
      runUpSeconds={runUp}
      selected={beatFocus}
      vocab={v}
      opensOnImage={!!shotClip?.image_b64}
      closesOnImage={!!shotClip?.h3_end_image}
      frameFor={(r) => frames.get(r.atSeconds ?? 0)}
      onselect={(i) => {
        const next = beatFocus === i ? null : i;
        setBeatFocus(next);
        if (next !== null) setLens(bodyField);
      }}
    />
  </div>
{/snippet}

<!-- ── The beats ────────────────────────────────────────────────────────────────────────
     🔴 **These text boxes edit the PROMPT, one `[Shot N]` line at a time.** They used to be
     read-only echoes here and editable on each beat's own Prompt tab — and that tab was a DECOY:
     it bound `clip.prompt` with `oninput={markDirty}` and nothing ever pushed it into the block, so
     typing there changed the shot list and never reached the render. Worse, the Draft button
     standing beside it DID work, because `absorbH3Reply` replaces the whole block. One control that
     rendered and one that did not, six inches apart.
     Now there is one store. `writeShots` rewrites just the main section's bytes through
     `replaceH3Section`, so every other section survives untouched. -->
{#snippet secBeats()}
  <div class="sec">
    <div class="lbl-row">
      <span>{capitalise(v.subUnitPlural ?? '')} in this {v.renderUnit}</span><span
        class="tl-meta"
        title="[Shot N] is the model's own tag for a beat — the vocabulary it was trained on, written verbatim in the prompt."
        >written as <code>[Shot N]</code> in the prompt</span
      >
    </div>
    <details class="why" style="margin-bottom:8px">
      <summary class="tl-meta">Beat or cut?</summary>
      <p class="hint" style="margin:6px 0 0">
        A {v.subUnit} with <b>no transition verb</b> is a timed moment inside a continuing take —
        measured 2026-08-08, three of them produced no scene change at all and still landed their
        actions within ~0.5 s. Add a verb and the same line becomes a {v.subUnitCut}. A {v.subUnitCut}
        should introduce new information about the subject, space, state, viewpoint or time — if only
        the distance or the angle changes, use camera motion and keep it one take.
      </p>
    </details>

    {#each beatRows.filter((r) => beatFocus === null || r.index === beatFocus) as row (row.index)}
      {@const si = row.index}
      {@const at = row.atSeconds}
      {@const isCut = h3ShotIsCut(row)}
      <div class="shot">
        <div class="inline-row" style="gap:6px;align-items:baseline">
          <b>{capitalise(v.subUnit ?? '')} {si + 1}</b>
          <span class="tl-meta"
            >{at === null ? `opens the ${v.renderUnit}` : `at ${formatCutTime(at)}`}</span
          >
          {#if at !== null && markIsTrimmed(at, runUp)}
            <span
              class="tl-warn"
              title={`This ${v.renderUnit} renders ${runUp.toFixed(3)}s of run-up before its first ` +
                `delivered frame — the previous ${v.renderUnit}'s last ${H3_TRIM_JOIN} frames, ` +
                `re-rendered as its own head and then trimmed. A time at or under that lands in ` +
                `the discarded head: the model will perform it and nobody will ever see it. Move ` +
                `it past ${formatCutTime(runUp)}, or make it the opening ${v.subUnit} instead.`}
              >never delivered — inside the {runUp.toFixed(2)}s run-up</span
            >
          {/if}
          <span class="grow"></span>
          {#if beatRows.length > 1}
            <button
              class="btn sm danger"
              title="Remove this {v.subUnit}'s line from the prompt. The {v.renderUnit} keeps its length."
              onclick={() => writeShots(placements.filter((_, j) => j !== si))}
              >Remove {v.subUnit} {si + 1}</button
            >
          {/if}
        </div>

        <textarea
          class="inp beatbody"
          rows="2"
          value={row.body}
          onfocus={(e) =>
            claimProse(e.currentTarget, (val) =>
              writeShots(placements.map((p, j) => (j === si ? { ...p, body: val } : p)))
            )}
          oninput={(e) => {
            const val = e.currentTarget.value;
            writeShots(placements.map((p, j) => (j === si ? { ...p, body: val } : p)));
          }}
          placeholder="what happens in {v.subUnit} {si + 1}"></textarea>

        {#if si > 0}
          <div class="inline-row" style="gap:6px;flex-wrap:wrap;align-items:center;margin-top:5px">
            <span class="tl-meta">at</span>
            <input
              class="inp"
              type="number"
              min="0"
              max={gen.seconds}
              step="0.1"
              style="flex:none;width:84px"
              value={(at ?? 0).toFixed(1)}
              onchange={(e) =>
                writeShots(
                  placements.map((p, j) =>
                    j === si ? { ...p, atSeconds: Number(e.currentTarget.value) } : p
                  )
                )}
            />
            <span class="tl-meta">s of {gen.seconds.toFixed(1)}s</span>
            {#if runUp > 0}
              <!-- The reachable range, said once beside the box that sets it. This clock starts at
                   the RENDER, not at the block — the first `runUp` seconds of it are re-rendered
                   from the previous shot and trimmed, so they are time the model spends and the
                   audience never gets. -->
              <span
                class="tl-meta"
                title={`This ${v.renderUnit}'s clock starts ${runUp.toFixed(3)}s before its block ` +
                  `appears in the cut: it re-renders the previous ${v.renderUnit}'s last ` +
                  `${H3_TRIM_JOIN} frames as a run-up and delivery trims them. Times at or under ` +
                  `${runUp.toFixed(2)}s are rendered but never delivered.`}
                >· delivered from {formatCutTime(runUp)}</span
              >
            {/if}
          </div>
          <!-- 🔴 Read from the BLOCK alone, via `h3ShotIsCut`. The `<select>` this replaced fell back
               to `clip.h3_cut_verb ?? H3_CUT_VERBS[0]`, so every untouched beat claimed a cut while
               the block held no verb and the render was a beat. -->
          <div class="inline-row" style="gap:6px;flex-wrap:wrap;align-items:center;margin-top:5px">
            <span class="tl-meta">camera</span>
            <button
              class="chip"
              class:on={!isCut}
              title="A {v.subUnit}: the camera keeps rolling and this happens at the stated time."
              onclick={() =>
                writeShots(
                  placements.map((p, j) =>
                    // The strip is mandatory: `readShot` leaves the verb IN the body, so dropping
                    // `cutVerb` without it leaves the text still opening "the camera cuts to".
                    j === si ? { ...p, cutVerb: undefined, body: h3StripCutVerb(p.body) } : p
                  )
                )}>keeps rolling</button
            >
            <button
              class="chip"
              class:on={isCut}
              title="A {v.subUnitCut}: a new setup starts here, in the same render."
              onclick={() =>
                writeShots(
                  placements.map((p, j) =>
                    j === si
                      ? {
                          ...p,
                          cutVerb: row.cutVerb ?? H3_CUT_VERBS[0],
                          body: h3StripCutVerb(p.body)
                        }
                      : p
                  )
                )}>cuts to it</button
            >
            {#if isCut}
              <select
                class="inp"
                style="flex:none;width:auto"
                value={row.cutVerb ?? H3_CUT_VERBS[0]}
                onchange={(e) => {
                  const verb = e.currentTarget.value;
                  writeShots(
                    placements.map((p, j) =>
                      j === si ? { ...p, cutVerb: verb, body: h3StripCutVerb(p.body) } : p
                    )
                  );
                }}
              >
                <!-- `verb`, not `v`: `v` is the display vocabulary here and an each-block that
                     shadowed it would silently swap the two in any label added inside. -->
                {#each H3_CUT_VERBS as verb (verb)}<option value={verb}>{verb}</option>{/each}
              </select>
              <!-- Gated on the CUT state: `<scenetrans>` carries a line of dialogue ACROSS a cut, so
                   on a rolling beat it is unread text eating the 7,000-character budget. -->
              <label class="inline-row" style="gap:6px">
                <input
                  type="checkbox"
                  checked={!!row.scenetrans}
                  onchange={(e) => {
                    const on = e.currentTarget.checked;
                    writeShots(placements.map((p, j) => (j === si ? { ...p, scenetrans: on } : p)));
                  }}
                  style="accent-color:var(--accent)"
                />
                <span class="tl-meta">a line of dialogue runs across this cut</span>
              </label>
            {/if}
          </div>
        {/if}
      </div>
    {/each}
    {@render notes('shots')}
    <details style="margin-top:8px">
      <summary class="tl-meta">Camera motion vocabulary</summary>
      <p class="hint" style="margin:6px 0">
        Natural English inside the {v.subUnit} — "the camera pushes in with small amplitude at slow speed
        toward the letter in her hands" — never labels stacked at the end of a sentence.
      </p>
      <div class="chiprow">
        {#each H3_CAMERA_MOTION as m (m)}<span class="chip static">{m}</span>{/each}
        {#each H3_CAMERA_AMPLITUDE as m (m)}<span class="chip static alt">{m}</span>{/each}
        {#each H3_CAMERA_SPEED as m (m)}<span class="chip static alt">{m}</span>{/each}
      </div>
    </details>

    <!-- ⚠️ The two buttons are genuinely different films — a sentence folded into a beat happens
         where the prose puts it, a new beat is a timed mark the model lands on — and the wording is
         what says which. -->
    <div class="entry" style="margin-top:10px">
      <div class="lbl-row">
        <span>Something changes here</span><span class="tl-meta"
          >a sentence in a {v.subUnit}, or a new one</span
        >
      </div>
      <input
        class="inp"
        bind:value={changeText}
        placeholder="she sets the glass down and looks up"
      />
      <!-- Seconds with a 0.1 step and not a frame picker, deliberately: a cut time is a sentence in
           the prompt rather than a VAE boundary, and `PackedShot.cutAtSeconds` is explicitly NOT
           snapped to the frame grid. Frame accuracy here would be a precision the tag cannot carry
           and the model does not read. -->
      <label class="inline-row" style="gap:6px;margin-top:6px">
        <span class="tl-meta">at</span>
        <input
          class="inp"
          type="number"
          min="0"
          max={gen.seconds}
          step="0.1"
          style="flex:none;width:84px"
          bind:value={changeAt}
        />
        <span class="tl-meta"
          >s of {gen.seconds.toFixed(1)}s{#if changeTarget}
            · inside {v.subUnit} {changeTarget.index + 1}{/if}</span
        >
      </label>
      <div class="inline-row" style="gap:6px;margin-top:6px;flex-wrap:wrap">
        <button
          class="btn sm"
          disabled={!changeText.trim() || !changeTarget}
          title="Adds this sentence to the end of that {v.subUnit}'s body. The model reads the whole body as one description, so the change happens where the sentence puts it — the time is NOT sent, because there is no timestamp inside a {v.subUnit}."
          onclick={foldChangeIn}>Fold it into that {v.subUnit}</button
        >
        <button
          class="btn sm"
          disabled={!changeText.trim() || !changeTarget}
          title={!changeTarget
            ? `Pick a moment inside a ${v.subUnit} — within 0.1s of a boundary there is nothing to split.`
            : `Writes a new [Shot N] line at this time, with this text, and gives the timeline a ${v.subUnit} for it. With no transition verb it is a BEAT — the camera keeps rolling.`}
          onclick={addBeat}>Make it a new {v.subUnit}</button
        >
      </div>
    </div>
  </div>
{/snippet}

<!-- ── Audio: the whole reason the schema is worth the trouble ─────────────────────── -->
{#snippet secSoundscape()}
  <div class="sec">
    <div class="lbl-row">
      <span>Soundscape</span><span class="tl-meta"
        >{parsed.overallSoundscape ? '✓' : '— not written'}</span
      >
    </div>
    <details class="why" style="margin-bottom:8px">
      <summary class="tl-meta">What belongs in it, and what an empty one costs</summary>
      <p class="hint" style="margin:6px 0 0">
        Ambience, physical action sounds, non-verbal human sounds — 1 to 4 sentences, one paragraph.
        <b>Not</b> dialogue (that lives in the {v.subUnit}'s body) and not diegetic music. Leaving
        this empty does not mean silence: H3 generates the soundtrack in the same pass as the
        picture, so an empty soundscape means it invents the whole bed unguided. Write
        <code>N/A</code> for deliberate silence.
      </p>
    </details>
    {#if audioNote}
      <div class="callout" style="margin-bottom:8px">
        <b>What the planner said this {v.renderUnit} should sound like</b>
        <input
          class="inp"
          style="margin-top:4px"
          value={audioNote.h3_audio ?? ''}
          oninput={(e) => onclip(audioNote.id, { h3_audio: e.currentTarget.value })}
        />
        <p class="hint" style="margin:6px 0 0">
          It never reaches the render on its own — the soundscape below is what is sent.
        </p>
      </div>
    {/if}
    <textarea
      class="inp"
      rows="3"
      value={parsed.overallSoundscape ?? ''}
      onfocus={(e) => claimProse(e.currentTarget, (v) => setSection('overall_soundscape', v))}
      oninput={(e) => setSection('overall_soundscape', e.currentTarget.value)}
      placeholder="Hull timbers groan under the swell; rigging ticks against the mast; gulls call somewhere off to port."
    ></textarea>
    {@render notes('overall_soundscape')}
  </div>
{/snippet}

{#snippet secScore()}
  <div class="sec">
    <div class="lbl-row">
      <span>Score</span><span class="tl-meta"
        >{parsed.nonDiegeticMusic ? '✓' : '— not written'}</span
      >
    </div>
    <details class="why" style="margin-bottom:8px">
      <summary class="tl-meta">How the guide wants it written</summary>
      <p class="hint" style="margin:6px 0 0">
        The audience-only music, 1 to 3 sentences: instrumentation, tempo, rhythm, how it develops.
        The guide asks for none of the abstract mood words ("tense", "melancholy") — describe what
        is playing. <code>N/A</code> if there is no score.
      </p>
    </details>
    <textarea
      class="inp"
      rows="2"
      value={parsed.nonDiegeticMusic ?? ''}
      onfocus={(e) => claimProse(e.currentTarget, (v) => setSection('non_diegetic_music', v))}
      oninput={(e) => setSection('non_diegetic_music', e.currentTarget.value)}
      placeholder="Low sustained strings under a single struck piano note every four bars, building in dynamic toward the cut."
    ></textarea>
    {@render notes('non_diegetic_music')}
  </div>
{/snippet}

<!-- ── References ────────────────────────────────────────────────────────────────────
     🔴 **At the TOP of the Prompt tab, above the prompt, and that is the point of the ordering.**
     The LLM has to know what it is allowed to name BEFORE it writes anything — a draft that
     invents `<Picture 2>` for a roster of one is unrecoverable from the text — so the references
     come first and the entry point comes second.

     Collapsed once a prompt exists, because at that stage they are context rather than a task, and
     the counter in the summary is what keeps them honest while closed.

     The scope control lives here, on the render unit, and writes `reference_ids`. Until 2026-08-08
     the only shipped control was a numbered chip row in `ReferencesPanel` that scoped by
     RENDERED-SHOT index — a granularity the conditioner does not have, since `pipeline.py:311-340`
     builds one reference presentation per CALL. Meanwhile `reference_ids`, which is the granularity
     it does have, had no writer at all. -->
{#snippet secReferences()}
  <div class="sec">
    <div class="lbl-row">
      <span>References</span><span class="tl-meta"
        >{#if gen.task === 'fl2va'}none — this {v.renderUnit} pins a frame{:else}{scopedIds.length}/{references.length}
          in scope{#if keyframes.length}
            · {gen.references.length} sent{/if}{#if shotAudio.length}
            · {shotAudio.length} audio{/if}{/if}</span
      >
    </div>
    {#if gen.task === 'fl2va'}
      <!-- 🔴 Not "none selected" — none is POSSIBLE. A wire-pinned shot renders on `fl2va`, a
           different weight set, and the second path out of that is on the shot's own Details tab. -->
      <p class="hint">
        None, and none is possible: a {v.subUnit} here <b>pins a frame exactly</b>, so this {v.renderUnit}
        renders on the <b>fl2va</b> checkpoint. References run on <b>ref2va</b> — a different weight
        set, not a flag — and the flip costs ~{H3_CHECKPOINT_FLIP_SECONDS}s on top.
        <br />Switch that {v.subUnit}'s frame to <b>“Open on this image”</b> in its Details tab and
        this
        {v.renderUnit} stays on ref2va with every reference, the song slice, and a 0.776 frame-0 match
        against the picture instead of 0.968.
      </p>
    {:else if references.length}
      <div class="pills">
        {#each references as r (r.id)}
          {@const on = inScope(r.id)}
          {@const o = ordinals.find((x) => x.id === r.id)}
          {@const name = r.label?.trim() || (r.id === H3_SOUNDTRACK_REF_ID ? 'project song' : '—')}
          <div class="pillwrap">
            <!-- 🔴 Two controls in one pill, and they are deliberately separate gestures. The BODY
                 writes the ordinal into the prose (the number stops being something to remember);
                 the ✓/＋ at the end takes the reference in or out of this shot's scope. One button
                 doing both would make "I want to mention Mia here" and "Mia is not in this shot"
                 the same click.
                 `preventDefault` on mousedown so the textarea keeps focus AND its caret — without
                 it the insert has nowhere to go and the control silently does nothing. -->
            <button
              class="pill"
              class:on
              disabled={!on}
              onmousedown={(e) => e.preventDefault()}
              onclick={() => o && insertLabel(o.label)}
              title={on
                ? `Write ${o?.label ?? ''} into the text box you were last in. Used as: ${roleLabel(r.role)}.`
                : 'Out of scope for this shot, so it has no number here and the prompt cannot name it. Bring it in with the ＋.'}
            >
              {#if r.kind === 'image' && r.image}
                <img class="thumb" src={assetSrc(r.image)} alt={name} />
              {:else}
                <span class="thumb glyph"
                  >{r.kind === 'video' ? '🎞' : r.id === H3_SOUNDTRACK_REF_ID ? '♪' : '🔊'}</span
                >
              {/if}
              <span class="pilltext">
                <b>{name}</b>
                <small
                  >{#if on}{#if o?.audioLabel}{o.audioLabel} +
                    {/if}{o?.label ?? ''}{:else}not in this shot{/if}</small
                >
              </span>
            </button>
            <button
              class="scope"
              class:on
              title={on
                ? 'In scope for this shot. Click to take it out — it keeps its place in the References list, so nothing else is renumbered.'
                : 'Out of scope. Click to bring it into this shot; it takes its number from its position in the References list.'}
              onclick={() => toggleScope(r.id)}>{on ? '✓' : '＋'}</button
            >
            <!-- The hover card. Deliberately the FULL image and the DECLARED ROLE together: the
                 role is what the summary's task-type bracket is derived from, so "which picture is
                 this and what did I say it was for" is one question, not two. -->
            <div class="pop">
              {#if r.kind === 'image' && r.image}
                <img src={assetSrc(r.image)} alt={name} />
              {/if}
              <b>{name}</b>
              <small>{on ? (o?.label ?? '') : 'out of scope'} · used as: {roleLabel(r.role)}</small>
            </div>
          </div>
        {/each}
      </div>
      <div class="inline-row" style="gap:8px;margin-top:7px;flex-wrap:wrap">
        <span class="tl-meta">
          {#if !scopeExplicit}
            following the reference list
          {:else if scopedIds.length === 0}
            <span style="color:var(--warning)">none — this shot renders unconditioned</span>
          {:else}
            set for this shot
          {/if}
        </span>
        {#if scopeExplicit}
          <button
            class="miniai"
            title="Go back to letting the References panel decide — that is the ABSENT state, which is not the same as an empty one."
            onclick={followProject}>Follow the reference list</button
          >
        {/if}
        {@render addScoped()}
        <span class="grow"></span>
        <span class="tl-meta"
          >{proseTarget
            ? 'click a reference to write its number in'
            : 'click into a text box first'}</span
        >
      </div>
      <details style="margin-top:6px">
        <summary class="tl-meta">Where the numbers come from</summary>
        <p class="hint" style="margin:6px 0 0">
          Scoping <b>narrows</b>; it never reorders. Numbering follows the order the engine packs
          them in — pictures, then clips, then standalone audio, and a clip carrying a soundtrack
          takes its &lt;Audio&gt; number <em>before</em> its &lt;Video&gt; one. ⚠ Reordering the
          list in the References panel is a <b>different render</b>, not a re-labelling: position
          assigns the number and advances a shared rotary clock.
        </p>
      </details>
    {:else}
      <p class="hint">This project has no references yet.</p>
      <div class="inline-row" style="gap:8px;margin-top:7px;flex-wrap:wrap">
        {@render addScoped()}
      </div>
    {/if}

    {#if keyframes.length}
      <!-- ── The ref2va keyframes, which are references the packer made ───────────────────
           🔴 Not scopeable and not in the References panel: a keyframe belongs to exactly one shot
           in exactly one generation, so it is re-derived every render rather than stored. It is
           listed here because it TAKES AN ORDINAL — it is the reason a picture the author never
           added to the roster can be `<Picture 3>`. -->
      <div class="callout" style="margin-top:9px">
        <b>Frames a {v.subUnit} opens or closes on</b>
        <div class="pills" style="margin-top:6px">
          {#each keyframes as k (k.refId)}
            <button
              class="pill on solo"
              title="Click to write this ordinal into the focused text box. It is a reference the packer added for the image on that {v.subUnit}'s Details tab — it is not in the References list and cannot be scoped."
              onmousedown={(e) => e.preventDefault()}
              onclick={() => insertLabel(k.label)}
            >
              <span class="thumb glyph">🎬</span>
              <span class="pilltext"
                ><b>{capitalise(v.subUnit ?? '')} {k.shotNumber} · {k.edge} frame</b><small
                  >{k.label}</small
                ></span
              >
            </button>
          {/each}
        </div>
        {#if undeclaredKeyframes.length}
          <div class="inline-row" style="gap:6px;margin-top:7px;flex-wrap:wrap">
            <button
              class="btn sm"
              title="Writes the four declarations MiniMax's guide specifies — a subject_definitions line, the keyframe completion task type, a retention_analysis row, and the opening sentence of the {v.subUnit}'s body. Each is written only if it is missing."
              onclick={declareKeyframes}
              >Write the keyframe declarations ({undeclaredKeyframes.length})</button
            >
          </div>
        {/if}
      </div>
    {/if}
    <!-- ── The audio this render is actually scored by ──────────────────────────────────
         🔴 Moved here from the per-beat pane, where it echoed something that is not per beat. Audio
         arrives through this same ordered reference list — POSITION assigns the `<Audio N>` ordinal
         the prompt addresses — so a file attached to one beat would have no number the prose could
         name. There is no uploader here for the same reason; the file is attached in the References
         panel and takes its number from where it lands. -->
    <div class="lbl-row" style="margin-top:10px">
      <span>What this {v.renderUnit} will hear</span>
      <span class="tl-meta"
        >{shotAudio.length
          ? `${shotAudio.length} in scope`
          : 'none — the model invents the whole bed'}</span
      >
    </div>
    {#if shotAudio.length}
      <div class="audiorefs">
        {#each shotAudio as a (a.ordinal)}
          <span class="aref"
            ><b>{a.ordinal}</b>
            {a.name}{#if a.fromSong}<span class="tl-meta"> · project song slice</span>{/if}</span
          >
        {/each}
      </div>
    {:else}
      <p class="hint">
        H3 makes the soundtrack in the same pass as the picture. Attach the project song, or an
        audio reference, and every {v.renderUnit} gets its own slice automatically.
      </p>
    {/if}
    {@render notes('keyframes')}
    {@render notes('references')}
  </div>
{/snippet}

<!-- ── Steps, LoRAs and canvas — the shared component, at `scope="shot"` ────────────────
     🔴 The controls used to be written out here in full and AGAIN in `SettingsSheet.svelte`, ~110
     lines apiece, with the four-rule LoRA-name validator implemented twice. `H3RenderKnobs` is that
     code, once; what stays here is the wiring — what this generation stores, and what it falls back
     to when it stores nothing. -->
{#snippet secKnobs()}
  <div class="sec">
    <div class="lbl-row">
      <span>Steps &amp; LoRAs</span><span class="tl-meta"
        >{inheritsSteps || inheritsLoras
          ? `overrides the project default for this ${v.renderUnit}`
          : `this ${v.renderUnit} only`}</span
      >
    </div>
    <!-- 🔴 No `oncanvas`, and that is what removes the control: the shared component draws the
         canvas fields only for a caller that can take them, and a canvas is not a property of one
         shot. Eight shots at four resolutions do not concatenate into a cut, and the noise is drawn
         at the latent SHAPE — so a per-shot canvas also silently changes what a pinned seed draws.
         It lives on the project, in `SettingsSheet`, which still passes the handler. -->
    <H3RenderKnobs
      scope="shot"
      vocab={v}
      {loras}
      multipliers={loraMults}
      steps={doc.steps}
      inheritLoras={projectLoras ?? []}
      inheritSteps={projectSteps}
      catalogue={loraCatalogue}
      onloras={(names, mults) =>
        onupdate({ loras: names, lora_multipliers: mults.length ? mults : undefined })}
      onsteps={(n) => onupdate({ steps: n })}
    />
    <p class="hint" style="margin-top:6px">
      No canvas here: shots that render at different sizes cannot be joined into a cut, so it is one
      size — <b>{projectWidth}×{projectHeight}</b> — for the whole project, in Settings.
    </p>
  </div>
{/snippet}

<!-- ── The seed ────────────────────────────────────────────────────────────────────────
     🔴 `DirectorH3Generation.seed` has existed since H3 landed and had NO WRITER ANYWHERE — the
     field was read by `compileH3Timeline` (forwarded only when >= 0, so "random" is never pinned by
     accident) and set by nothing. Which meant every render rolled fresh and there was no way to ask
     for the one you just got, at all.
     ⚠️ Pinning a seed does NOT reproduce a render on its own. The noise is drawn at the latent
     SHAPE, so the same seed at a different canvas is a different sample — and the audio noise comes
     off the same generator immediately afterwards, so the music bed moves with it. -->
{#snippet secSeed()}
  <div class="sec">
    <div class="lbl-row">
      <span>Seed</span><span class="tl-meta"
        >{doc.seed === undefined || doc.seed < 0 ? 'rolled fresh each render' : 'pinned'}</span
      >
    </div>
    <div class="inline-row" style="gap:6px;flex-wrap:wrap">
      <input
        class="inp"
        type="number"
        step="1"
        style="flex:none;width:150px"
        placeholder="random"
        value={doc.seed === undefined || doc.seed < 0 ? '' : doc.seed}
        oninput={(e) => {
          // Empty and negative both mean RANDOM, and they are stored as the same thing: absent.
          // `compileH3Timeline` forwards the seed only when it is >= 0, so a negative left on the
          // document would read as pinned here while rolling fresh on the wire.
          const n = Math.trunc(Number(e.currentTarget.value));
          onupdate({
            seed: e.currentTarget.value === '' || !Number.isFinite(n) || n < 0 ? undefined : n
          });
        }}
      />
      {#if doc.seed !== undefined && doc.seed >= 0}
        <button class="btn sm" onclick={() => onupdate({ seed: undefined })}>Roll fresh</button>
      {/if}
    </div>
    <p class="hint" style="margin-top:6px">
      A pinned seed only holds at the <b>same canvas</b>: the noise is drawn at the latent shape,
      and the soundtrack's noise comes off the same generator immediately afterwards.
    </p>
  </div>
{/snippet}

<!-- ── THE PROMPT — the hero of the tab ────────────────────────────────────────────────
     🔴 It is the deliverable, so it looks like one: full width, tall, top of the tab under the
     entry point, and NOT a drawer at the floor. An earlier proposal had it collapsed at the bottom
     under the exploded fields; the owner rejected that outright — "prompt is the key deliverable
     and should be LOUD, 'fancy pants editing form' is the 'break it down'."
     This is also the document itself and not a preview of one. Typing here IS the edit; the fields
     below are a lens that rewrites one section's bytes at a time. -->
{#snippet secArtifact()}
  <div class="sec artifact hero">
    <!-- ⚠️ No character count here. It is the segmented prompt budget in the header, which is the
         only place that can say WHICH section is spending the 7,000 — and a second copy of the
         number in this corner was read as the budget itself. -->
    <div class="lbl-row">
      <span class="heroh">The prompt</span>
    </div>
    <p class="hint" style="margin-bottom:6px">
      Typing here is the edit. The lens beside it follows your caret.
    </p>
    <!-- `prompt`, never `sent`: while the box is blank it has to STAY blank, or clearing it would
         refill itself with the composed block under the author's cursor. What `sent` says is in the
         character counter, and while the box is blank it is printed in full under the warning below.
         🔴 A CodeMirror editor rather than a textarea — the colour is a decoration over the same
         document, not a second copy of it painted underneath. See `H3BlockEditor` for why the
         overlay it replaced could not be made to hold its alignment. It registers itself as the
         `proseTarget` on focus, which is how a reference pill's ordinal gets to the caret. -->

    <H3BlockEditor
      value={prompt}
      docId={doc.id}
      rows={22}
      onclaim={(target) => (proseTarget = target)}
      oninput={(next) => {
        // Typing in the block is touching the DOCUMENT, so it reveals the two notes that describe
        // the whole of it — the character cap and the main body's word envelope. It deliberately
        // does NOT mark the six named sections touched: an author who adds a sentence to a shot has
        // not thereby decided anything about the score, and lighting all of them up on the first
        // keystroke is the behaviour this stage removed.
        touch('prompt');
        touch(bodyField);
        onprompt(next);
      }}
      {resolvedLabels}
      onshot={(n) => {
        // `onshot` fires only from inside the main body, so it adds the BEAT to what `onsection` has
        // already said — the two together are what puts the field and the bytes it rewrites on
        // screen at once. Neither fires with `null`, so reading something else and coming back does
        // not deselect what was being edited.
        setBeatFocus(n - 1);
        setLens(bodyField);
      }}
      onsection={setLens}
      onlookup={cardFor}
      ontoken={onEditorToken}
    />
    {#if blockBlank}
      <p class="hint err" style="margin-top:6px">
        The box is empty, so the <b>block composed from the fields is what would render</b> — an
        empty prompt is not a thing this engine can be sent. koblem drops an empty-prompt {v.renderUnit}
        from the chain, so this one would vanish from the render rather than fail.
      </p>
      <!-- 🔴 Shown verbatim, because with the box blank NOTHING ELSE ON THE TAB DRAWS THIS TEXT.
           The panels below are the lens over `prompt`, which is `""`, so they are all empty — while
           `sent` is composed from `h3SeedInput`: the clip bodies, and on a document written before
           the reversal the stored `summary` / `subject_definitions` / soundscape sections that no
           panel has written or read since. That is the one path on which those dead fields still
           reach the engine. Without this the author gets empty fields, a character count of a few
           thousand, and a render conditioned on prose that is nowhere on screen. -->
      <pre class="block small">{sent}</pre>
    {/if}
    {@render notes('prompt')}
    {@render notes(bodyField)}

    {#if parsed.unparsed.length}
      <!-- 🔴 Shown, never swallowed. This is text that is IN the block and WILL be sent, and that
           no panel above can edit — a section MiniMax added after we last looked, a retention line
           that does not match the shape, a paragraph left between two headers. `replaceH3Section`
           leaves its bytes alone, so a field edit cannot destroy it; hiding it would just mean
           nobody knew it was conditioning the render. -->
      <div class="callout" style="margin-top:8px">
        <b>In the block, but in no field</b>
        <p class="hint" style="margin:4px 0">
          Sent with everything else. Nothing above edits or overwrites it — change it in the box, or
          leave it.
        </p>
        {#each parsed.unparsed as u (u.at)}
          <pre class="block small">{u.text}</pre>
        {/each}
      </div>
    {/if}
  </div>
{/snippet}

<!-- ── B. The LLM entry point — where you START ────────────────────────────────────────
     Owner, 2026-08-08: *"LLM is the starting entry point, prompt is the key deliverable and should
     be LOUD, 'fancy pants editing form' is the 'break it down'."* So this sits between the
     references (which the writer must know about first) and the prompt (which it produces).

     🔴 The idea box is MULTILINE, and that is a correction rather than a preference: it was a single
     `<input>`, which the owner reported "didn't cut it for 5s LTX clips, def won't cut it for 15s
     H3 shots". A render unit here is up to 15 seconds holding several {@link EngineVocab.subUnit}s;
     one line cannot hold the intent for that.

     ⚠️ Draft REPLACES the whole prompt, hand-written text included — `absorbH3Reply` always did, and
     saying so on the button is cheaper than a confirm dialog on the one action people press on
     purpose.

     🔴 **Collapsed to ONE LINE once a prompt exists, and that is the point of the zone.** Both boxes
     in here overwrite the whole deliverable, so leaving them open above it spends the top of the
     pane on the two controls the author is least likely to want next — while the one fact that is
     always worth having, *what this prompt was written from*, was buried in a textarea. Shut, the
     zone says exactly that fact and nothing else; open, it is a two-column grid so neither box is
     the narrow one. -->
{#snippet secIdea()}
  <div class="sec">
    {#if draftFailed}
      <div class="warn-bar" style="margin:0 0 8px">
        <span class="sev warn">draft failed</span>
        <span style="flex:1"
          >Auto-draft gave up on this {v.renderUnit} after {draftTries} tries.</span
        >
        <button class="btn sm" onclick={() => onretrydraft?.()} disabled={draftAllRunning}
          >Retry</button
        >
      </div>
    {/if}

    {#if !ideaOpen}
      <div class="ideabar">
        <span class="tl-meta">WRITTEN FROM</span>
        <em class="ideaecho"
          >{ideaText || `nothing yet — this ${v.renderUnit} has no idea written`}</em
        >
        <button
          class="miniai"
          title="Open the idea and the rework box. Both of them replace the whole prompt."
          onclick={() => setIdeaOpen(true)}>Redraft or rework</button
        >
      </div>
    {:else}
      <div class="ideahead">
        <span class="tl-meta">Where the prompt comes from</span>
        <span class="grow"></span>
        <button
          class="xclose"
          title="Collapse this back to one line. Nothing is lost — the idea stays on the {v.subUnit} it is stored on."
          onclick={() => setIdeaOpen(false)}>✕</button
        >
      </div>
      <div class="ideagrid" class:stack={narrow}>
        <div class="ideacell">
          <div class="lbl-row">
            <span>Your idea</span><span class="tl-meta">what you want, in your own words</span>
          </div>
          {#if ideaClip}
            <textarea
              class="inp ideabox"
              value={ideaClip.idea ?? ''}
              oninput={(e) => onclip(ideaClip.id, { idea: e.currentTarget.value })}
              placeholder="a woman at a bar counter watching the TV; at eight seconds she puts the glass down and looks at the door…"
            ></textarea>
          {/if}
          <div class="inline-row" style="gap:6px;margin-top:6px;flex-wrap:wrap">
            <button
              class="miniai"
              disabled={promptBusy || !ondraft}
              title="Hands MiniMax's own prompt-writing guide, this {v.renderUnit}'s length, and the references above to the writer, and REPLACES the prompt with what comes back."
              onclick={() => ondraft?.()}>✨ {promptBusy ? 'Working…' : 'Draft the prompt'}</button
            >
            {#if prompt.trim()}
              <button
                class="miniai"
                disabled={promptBusy || !onreroll}
                title="Same intent, fresh wording. Also replaces the prompt."
                onclick={() => onreroll?.()}>Reroll</button
              >
            {/if}
          </div>
          <p class="hint warnmsg" style="margin-top:6px">
            ⚠ Both of these <b>replace the whole prompt</b>, hand-written text included.
          </p>
        </div>

        <div class="ideacell">
          <div class="lbl-row">
            <span>Rework what is there</span><span class="tl-meta"
              >send the prompt back through with a direction</span
            >
          </div>
          <!-- ⚠️ A textarea now, and Enter still submits — `preventDefault` keeps it from also
               inserting the newline it would otherwise leave behind. Shift+Enter is the newline.
               The single-line `<input>` this replaced could not hold a direction longer than a
               phrase, which is the same complaint that made the idea box multiline. -->
          <textarea
            class="inp ideabox"
            bind:value={reworkText}
            onkeydown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                onrework?.();
              }
            }}
            placeholder="she looks scared, add rain, cut to a wider angle…"></textarea>
          <div class="inline-row" style="gap:6px;margin-top:6px">
            <button
              class="btn sm"
              onclick={() => onrework?.()}
              disabled={promptBusy || !reworkText.trim() || !onrework}
              >{promptBusy ? '…' : 'Rework'}</button
            >
            <span class="tl-meta">keeps what is there and edits it</span>
          </div>
        </div>
      </div>
    {/if}
  </div>
{/snippet}

<!-- ══ The lens ════════════════════════════════════════════════════════════════════════
     🔴 **One section's editor, beside the bytes it rewrites.** Every panel in here is a surgical
     rewrite of one run of the block — `replaceH3Section(prompt, section, next)` — and until this
     column existed they were stacked inside a `<details>` that was SHUT by default and, when
     opened, sat entirely below the prompt. So the field and the text it edits were never on screen
     together, which is the one thing a lens has to manage.

     ⚠️ The index is drawn from `h3SectionBadges` rather than from a second count: that function
     measures the same spans the highlighter paints and the same spans `replaceH3Section` writes, so
     a row can never claim a section is written while the block is empty at that offset. -->
{#snippet lens()}
  <div class="lens">
    <div class="lidx">
      {#each lensRows as r (r.key)}
        <button
          class="lrow"
          class:on={r.key === lensKey}
          class:bare={r.empty}
          onclick={() => setLens(r.key)}
        >
          <!-- A STATE marker, never a verdict. A hollow dot is the normal look of a fresh shot —
               several of them — so it is drawn as an outline rather than as anything coloured. -->
          <span class="dot" class:filled={!r.empty}></span>
          <span class="lname">{r.name}</span>
          <span class="lcount">{r.count}</span>
        </button>
      {/each}
    </div>

    {#if lensRow}
      <div class="lhead">
        <span class="lheadname">{lensRow.name}</span>
        <p class="lgloss">{lensRow.gloss}</p>
      </div>
    {/if}

    <!-- 🔴 The SAME six snippets, unchanged — same handlers, same `notes()` calls, same `touch()`
         marking. Only where they render moved. Each one's own long-form disclosure carries
         `class="why"` so it reads as the section's explanation rather than as another control. -->
    {#if lensKey === 'style'}
      {@render secStyle()}
    {:else if lensKey === 'subject_definitions'}
      {@render secDefinitions()}
    {:else if lensKey === 'summary'}
      {@render secSummary()}
    {:else if lensKey === 'retention_analysis'}
      {@render secRetention()}
    {:else if lensKey === 'overall_soundscape'}
      {@render secSoundscape()}
    {:else if lensKey === 'non_diegetic_music'}
      {@render secScore()}
    {:else}
      {@render secBeats()}
    {/if}
  </div>
{/snippet}

<!-- ══ The two panes ═══════════════════════════════════════════════════════════════════
     One mounted component, two layouts. `touched` and the caret target survive the tab switch
     because the component is not remounted — see the `pane` prop doc.

     ⚠️ `bind:this` is here and not on an inner wrapper: the split below is measured against THIS
     component's width, and the element that has it must therefore be the one the workspace sizes. -->
<div class="gen" bind:this={paneEl}>
  {@render secHeader()}
  {@render secNotices()}

  <!-- 🔴 References ABOVE the tabs, and never folded away. What this shot may name is not a step you
       finish — it is the vocabulary every panel under here writes in, and it belongs to the shot
       rather than to the Prompt tab. It used to be a `<details>` that shut itself the moment a
       prompt existed, i.e. permanently, so the one control that says which numbers resolve was
       hidden exactly when the prose started using them. -->
  {@render secReferences()}

  <!-- ⚠️ The tab strip lives HERE and not in the workspace, because everything above it describes
       the shot itself and both tabs need it on screen. Split across two components it could only
       sit below, which put the shot's identity, its two budgets and its references inside one of
       the two things they label. -->
  <div class="tabs">
    <button class="tab" class:on={activeTab === 'prompt'} onclick={() => (activeTab = 'prompt')}
      >Prompt</button
    >
    <button class="tab" class:on={activeTab === 'details'} onclick={() => (activeTab = 'details')}
      >{capitalise(v.renderUnit)} details</button
    >
  </div>

  {#if activeTab === 'prompt'}
    {@render secIdea()}

    <!-- C. The deliverable and its lens, side by side.
         🔴 **This replaced `<details class="fold">Break it down</details>`.** The fold held these
         same six editors stacked under the prompt and shut by default, so the field and the bytes
         it rewrites were never visible at once. `align-items:start` keeps the lens pinned to the top
         of the block rather than centred against a 22-row editor, and the stack threshold is the
         COMPONENT's width — see `narrow`. -->
    <div class="split" class:stack={narrow}>
      <div class="scol">
        {@render secArtifact()}
        {@render secShape()}
      </div>
      {@render lens()}
    </div>
  {:else}
    {@render secCheckpoint()}
    {@render secJoin()}
    {@render secKnobs()}
    {@render secSeed()}
  {/if}
</div>

<style>
  .gen {
    border: 1px solid var(--ring);
    border-radius: 12px;
    padding: 10px 11px 12px;
    background: var(--bg-2);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .gen-h {
    display: flex;
    align-items: baseline;
    gap: 9px;
    flex-wrap: wrap;
    margin-bottom: 4px;
  }
  .unit {
    font-size: 16px;
    font-weight: 650;
    letter-spacing: -0.015em;
  }
  .of {
    font-size: 11.5px;
    color: var(--faint, var(--muted));
  }
  .task {
    font-family: var(--mono);
    font-size: 10px;
    padding: 1px 7px;
    border-radius: 999px;
    border: 1px solid var(--ring);
    color: var(--accent);
  }
  .sec {
    padding-top: 10px;
    margin-top: 8px;
    border-top: 1px solid var(--ring);
  }
  /* A list row's head: what the row IS, and the button that removes it. The delete sits here rather
     than after the boxes so it never precedes the text it would destroy. */
  .rowhead {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 4px;
  }
  .rowhead .inp {
    flex: 1;
    min-width: 0;
  }
  .lab {
    font-family: var(--mono);
    font-size: 11px;
    color: var(--accent);
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .defrow {
    margin-bottom: 10px;
  }
  .ret {
    margin-bottom: 8px;
    padding-bottom: 8px;
    border-bottom: 1px dashed var(--ring);
  }
  .shot {
    padding: 7px 0;
    border-bottom: 1px dashed var(--ring);
  }
  .shot:last-of-type {
    border-bottom: none;
  }
  .grow {
    flex: 1;
  }
  /* A time that will be rendered and then thrown away. `--danger`, not `--warning`: it is not a
     caveat about a cue, it is a cue that does not exist in the delivered cut. */
  .tl-warn {
    font-family: var(--mono);
    font-size: 11px;
    color: var(--danger);
    cursor: help;
  }
  .chiprow {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .chip {
    width: auto;
    padding: 3px 9px;
    font-size: 11.5px;
    border-radius: 999px;
    border: 1px solid var(--ring);
    background: var(--bg-3, var(--bg-2));
    color: var(--ink);
    cursor: pointer;
    line-height: 1.4;
  }
  .chip:hover {
    border-color: var(--accent);
  }
  .chip.on {
    border-color: var(--accent);
    color: var(--accent);
    font-weight: 600;
  }
  /* ── Reference pills ──────────────────────────────────────────────────────────────────
     A reference is a picture with a name, not a number to memorise — so the pill leads with the
     thumbnail and the author's own label, and carries the ordinal as the small print underneath.
     The ordinal is still shown, always, because it is the only string the model resolves. */
  .pills {
    display: flex;
    flex-wrap: wrap;
    gap: 7px;
  }
  .pillwrap {
    position: relative;
    display: flex;
    align-items: stretch;
  }
  .pill {
    width: auto;
    display: flex;
    align-items: center;
    gap: 7px;
    padding: 3px 4px 3px 3px;
    border: 1px solid var(--ring);
    border-right: none;
    border-radius: 999px 0 0 999px;
    background: var(--bg-3, var(--bg-2));
    color: var(--ink);
    cursor: pointer;
    text-align: left;
    line-height: 1.25;
  }
  .pill.on {
    border-color: var(--accent);
  }
  /* A pill with no scope button beside it — the keyframes, which belong to one shot and cannot be
     scoped. It closes its own right-hand side rather than looking like a half-drawn control. */
  .pill.solo {
    border-right: 1px solid var(--ring);
    border-radius: 999px;
    padding-right: 11px;
  }
  .pill.solo.on {
    border-right-color: var(--accent);
  }
  /* Out of scope reads as absent rather than as broken: it has no ordinal, so there is nothing to
     insert, and the ＋ next to it is the way back in. */
  .pill:disabled {
    opacity: 0.55;
    cursor: default;
  }
  .thumb {
    width: 34px;
    height: 24px;
    border-radius: 999px 4px 4px 999px;
    object-fit: cover;
    flex: none;
    background: var(--bg-2);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 13px;
  }
  .pilltext {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .pilltext b {
    font-size: 11.5px;
    overflow-wrap: anywhere;
  }
  .pilltext small {
    font-family: var(--mono);
    font-size: 9.5px;
    color: var(--muted);
  }
  .scope {
    width: auto;
    flex: none;
    padding: 0 9px;
    border: 1px solid var(--ring);
    border-radius: 0 999px 999px 0;
    background: var(--bg-3, var(--bg-2));
    color: var(--muted);
    cursor: pointer;
    font-size: 11px;
  }
  .scope.on {
    border-color: var(--accent);
    color: var(--accent);
  }
  /* Hover card: the full picture and the DECLARED ROLE, because "which one is this" and "what did
     I say it was for" are one question — the role is what the task-type bracket derives from. */
  .pop {
    position: absolute;
    bottom: calc(100% + 6px);
    left: 0;
    z-index: 20;
    display: none;
    flex-direction: column;
    gap: 4px;
    width: 210px;
    padding: 7px 8px;
    border: 1px solid var(--accent);
    border-radius: 9px;
    background: var(--bg-2);
    box-shadow: 0 8px 22px rgba(0, 0, 0, 0.35);
  }
  .pop img {
    width: 100%;
    border-radius: 6px;
    display: block;
  }
  .pop b {
    font-size: 11.5px;
  }
  .pop small {
    font-size: 10.5px;
    color: var(--muted);
  }
  .pillwrap:hover .pop,
  .pillwrap:focus-within .pop {
    display: flex;
  }
  /* A vocabulary the author copies from, not a control — so it must not look clickable. */
  .chip.static {
    cursor: default;
    color: var(--muted);
  }
  .chip.static:hover {
    border-color: var(--ring);
  }
  .chip.static.alt {
    font-style: italic;
  }
  .block {
    margin: 0;
    padding: 9px 10px;
    border: 1px solid var(--ring);
    border-radius: 9px;
    background: var(--bg-3, var(--bg-2));
    font-family: var(--mono);
    font-size: 11px;
    line-height: 1.55;
    color: var(--ink);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 320px;
    overflow-y: auto;
  }
  .over {
    color: var(--danger);
    font-weight: 700;
  }
  /* ── The three tiers, and they must look like three different things ──────────────────────
     `err` (red, from the shared workspace stylesheet) is the only one that blocks a render.
     `warnmsg` is amber: outside the guide's envelope, renders anyway.
     🔴 `ghost` is a PLACEHOLDER, not a verdict. It is the normal appearance of a section nobody
     has written yet — every fresh shot shows several — so it has to read as instruction rather
     than as fault. Muted, italic, and set apart by a rule rather than by colour, because a fresh
     shot covered in coloured notices is exactly what this stage was built to stop. */
  .warnmsg {
    color: var(--warning);
  }
  .ghost {
    color: var(--muted);
    font-style: italic;
    opacity: 0.85;
    border-left: 2px solid var(--ring);
    padding-left: 8px;
  }
  /* ── The hero ────────────────────────────────────────────────────────────────────────────
     The prompt is the deliverable, so it is the loudest thing on the tab: an accent rule down its
     left edge, a heading a size up, and a box tall enough to read a whole 15-second shot in. Its
     neighbours are a fold and a form; this one is not. */
  .hero {
    border-left: 3px solid var(--accent);
    padding-left: 10px;
    margin-left: -2px;
  }
  .heroh {
    font-size: 13.5px;
    font-weight: 700;
  }
  /* A beat's own line in the prompt, and the section rows beside it. Same mono face as the block,
     because that IS what they edit — the boxes have to read as the same substance or the surgical
     edit looks like a copy. */
  .beatbody,
  .prosebox {
    font-family: var(--mono);
    font-size: 11px;
    line-height: 1.5;
  }
  .beatbody {
    margin-top: 4px;
  }
  /* The "something changes here" entry: a control cluster inside a section, set apart from the
     beat list above it without being a section of its own. */
  .entry {
    border-top: 1px dashed var(--ring);
    padding-top: 9px;
  }
  /* The unparsed runs: same face as the block, smaller, so they read as excerpts OF it. */
  .block.small {
    font-size: 10.5px;
    max-height: 160px;
    margin-top: 4px;
  }

  /* ══ The header: identity, then the two budgets ═══════════════════════════════════════════
     Three columns and not a flex row, because the two budgets have to keep their own width while
     the identity takes whatever is left — a bar that reflows to 40px says nothing at all. */
  .hgrid {
    display: grid;
    grid-template-columns: minmax(240px, 1fr) auto auto;
    align-items: start;
    gap: 10px 18px;
    margin-bottom: 6px;
  }
  .hgrid.stack {
    grid-template-columns: 1fr;
  }
  .hid {
    min-width: 0;
  }
  /* The gist arrives as an ordinary `.sec`, which is a rule and a top margin. Neither belongs
     inside the identity block — it IS the identity, not a section under it. */
  .hgist > .sec {
    border-top: none;
    margin-top: 2px;
    padding-top: 0;
  }
  .budget {
    min-width: 190px;
  }
  .hgrid.stack .budget {
    min-width: 0;
  }
  .blab {
    font-family: var(--mono);
    font-size: 9.5px;
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--faint, var(--muted));
  }
  .bnum {
    font-family: var(--mono);
    font-size: 12px;
    color: var(--muted);
    margin: 1px 0 4px;
  }
  .bnum b {
    color: var(--ink);
    font-weight: 600;
  }
  /* Over the ceiling the NUMBER is the alarm, so it takes the weight as well as the colour — a
     seven-thousand-and-thirty that looks like a seven-thousand is the whole failure mode. */
  .bnum.over,
  .bnum.over b {
    color: var(--danger);
    font-weight: 700;
  }
  .bbar {
    display: flex;
    gap: 1px;
    height: 9px;
    border-radius: 999px;
    overflow: hidden;
    background: var(--bg-3, var(--bg-2));
    border: 1px solid var(--ring);
  }
  /* ⚠️ `--accent` at reduced opacity rather than `--accent-dim`: that token is a 15% wash meant to
     sit BEHIND text, and a 9px bar painted in it is invisible against `--bg-3`. */
  .bseg {
    min-width: 2px;
    background: var(--accent);
    opacity: 0.45;
  }
  /* A cut reads as a harder edge than a beat, which is the same distinction the ribbon draws. */
  .bseg.cut {
    opacity: 1;
    background: var(--accent);
  }
  /* The section the model actually renders from. It is normally the largest share of the 7,000 and
     it is the one worth finding at a glance. */
  .bseg.main {
    opacity: 1;
    background: var(--accent);
  }
  /* Bytes in no section. Neutral rather than accented: they are sent, and nothing here edits them. */
  .bseg.other {
    background: var(--muted);
    opacity: 0.5;
  }
  .bslack {
    background: transparent;
  }
  /* The whole bar, not one segment: the request is refused rather than trimmed, so no single
     section is the culprit. */
  .bbar.over {
    border-color: var(--danger);
  }
  .bbar.over .bseg {
    background: var(--danger);
    opacity: 0.85;
  }
  .blegend {
    margin-top: 3px;
    font-size: 10.5px;
    color: var(--faint, var(--muted));
  }

  /* ══ The idea zone ════════════════════════════════════════════════════════════════════════ */
  .ideabar {
    display: flex;
    align-items: baseline;
    gap: 9px;
    min-width: 0;
  }
  .ideaecho {
    flex: 1;
    min-width: 0;
    font-size: 12px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ideahead {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 6px;
  }
  .xclose {
    width: auto;
    flex: none;
    padding: 0 7px;
    border: 1px solid var(--ring);
    border-radius: 6px;
    background: transparent;
    color: var(--muted);
    cursor: pointer;
    font-size: 11px;
    line-height: 1.7;
  }
  .xclose:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  .ideagrid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
    align-items: start;
  }
  .ideagrid.stack {
    grid-template-columns: 1fr;
  }
  .ideacell {
    min-width: 0;
  }
  /* Both boxes the same height, both full width of their column: one of them is the intent for a
     15-second render and the other is the direction for rewriting it, and neither is a phrase. */
  .ideabox {
    min-height: 110px;
    resize: vertical;
  }

  /* ══ The block and its lens ═══════════════════════════════════════════════════════════════ */
  .split {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 360px;
    align-items: start;
    gap: 0 16px;
  }
  .scol {
    min-width: 0;
  }
  /* A rule rather than a gap alone: the lens is a different KIND of surface from the document
     beside it — one is what is sent, the other is a set of instruments pointed at it. */
  .lens {
    min-width: 0;
    border-left: 1px solid var(--ring);
    padding-left: 16px;
  }
  .split.stack {
    grid-template-columns: 1fr;
  }
  .split.stack .lens {
    border-left: none;
    border-top: 1px solid var(--ring);
    padding-left: 0;
    padding-top: 12px;
    margin-top: 12px;
  }
  /* The index: every editor this checkpoint has, whether or not the block has written it yet. */
  .lidx {
    display: flex;
    flex-direction: column;
    gap: 1px;
    margin-bottom: 4px;
  }
  .lrow {
    display: flex;
    align-items: center;
    gap: 7px;
    width: 100%;
    padding: 3px 6px;
    border: 1px solid transparent;
    border-radius: 6px;
    background: transparent;
    color: var(--ink);
    font-family: var(--mono);
    font-size: 11.5px;
    line-height: 1.5;
    text-align: left;
    cursor: pointer;
  }
  .lrow:hover {
    border-color: var(--ring);
  }
  .lrow.on {
    border-color: var(--accent);
    background: var(--bg-3, var(--bg-2));
    color: var(--accent);
  }
  .lrow.bare .lname {
    color: var(--muted);
  }
  .lrow.on .lname {
    color: inherit;
  }
  .dot {
    flex: none;
    width: 7px;
    height: 7px;
    border-radius: 999px;
    border: 1px solid var(--muted);
  }
  .dot.filled {
    background: var(--accent);
    border-color: var(--accent);
  }
  .lname {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .lcount {
    flex: none;
    font-size: 10.5px;
    color: var(--faint, var(--muted));
  }
  .lhead {
    padding-top: 9px;
    margin-top: 6px;
    border-top: 1px solid var(--ring);
  }
  .lheadname {
    font-family: var(--mono);
    font-size: 12.5px;
    font-weight: 600;
    color: var(--accent);
    overflow-wrap: anywhere;
  }
  .lgloss {
    margin: 2px 0 0;
    font-size: 11.5px;
    line-height: 1.45;
    color: var(--muted);
  }
  /* The section's own long-form prose, which each snippet already carried. In the lens it is the
     "why" behind the controls under it, so it is set apart rather than left looking like a control
     of its own — but it stays SHUT, because the author who came here to type does not want it. */
  .lens details.why {
    border-left: 2px solid var(--ring);
    padding-left: 8px;
  }
  .lens details.why summary {
    cursor: pointer;
  }
  /* 360px is narrower than any of these control clusters was designed against. Wrapping is the whole
     accommodation; nothing is hidden and no control is dropped. */
  .lens .inline-row {
    flex-wrap: wrap;
  }
  /* A definition and a retention claim are whole sentences — one of them ends in a citation
     ("…appearing in <Picture 1>") — so they take the column's full width and stand a couple of lines
     tall. ⚠️ `.dw textarea.inp` sets `resize: none` for the app's one-line boxes at (0,2,1), so the
     exception needs three classes to outrank it, not a deeper-looking two. */
  .lens .sec .prosebox {
    min-height: 3.4rem;
    resize: vertical;
  }
  /* The two pickers shrink to share the head line rather than wrapping into a ragged stack. */
  .lens .rowhead .inp {
    flex: 1 1 7rem;
  }
  .lens .ret .field {
    margin-bottom: 6px;
  }
  .lens .ret .field:last-child {
    margin-bottom: 0;
  }
  /* The panel below the head is already the lens's subject, so it drops the rule that separates
     sections from each other in a stack. */
  .lens > .sec {
    border-top: none;
    margin-top: 6px;
    padding-top: 0;
  }
</style>
