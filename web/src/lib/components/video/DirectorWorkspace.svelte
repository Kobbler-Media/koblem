<script lang="ts">
  import {
    assetSrc,
    fetchAssetBlob,
    putAsset,
    prefetchAssets
  } from '#lib/director/assets.svelte.ts';
  import { onWriteConflict, renameRoute, type StaleDoc } from '#lib/director/save.ts';
  import { cachedFrame, clearFrameCache } from '#lib/director/frame-grab.ts';
  import { onMount, onDestroy } from 'svelte';
  import { blobToMonoSamples, uid } from '@koblibs/ui';
  import { framesFromSeconds } from '#lib/ltx/frames.ts';
  import { adoptedElapsedSec } from '#lib/ltx/elapsed.ts';
  import { downscaleImageToBlob } from '#lib/ltx/downscaleImage.ts';
  import ChatModal from '#lib/components/director/ChatModal.svelte';
  import PlanReviewSheet from '#lib/components/director/PlanReviewSheet.svelte';
  import SettingsSheet from '#lib/components/director/SettingsSheet.svelte';
  import ShotDetailsPane from '#lib/components/director/ShotDetailsPane.svelte';
  import RendersDrawer from '#lib/components/director/RendersDrawer.svelte';
  import RenderScopeDialog from '#lib/components/director/RenderScopeDialog.svelte';
  import StoryPanel from '#lib/components/director/StoryPanel.svelte';
  import CastPanel from '#lib/components/director/CastPanel.svelte';
  import ReferencesPanel from '#lib/components/director/ReferencesPanel.svelte';
  import AutoDirectPanel from '#lib/components/director/AutoDirectPanel.svelte';
  import RevisePanel from '#lib/components/director/RevisePanel.svelte';
  import ShotPromptPane from '#lib/components/director/ShotPromptPane.svelte';
  import AppBar from '#lib/components/director/AppBar.svelte';
  import StageViewer from '#lib/components/director/StageViewer.svelte';
  import RailDock from '#lib/components/director/RailDock.svelte';
  import TimelineBar from '#lib/components/director/TimelineBar.svelte';
  import ProjectMenu from '#lib/components/director/ProjectMenu.svelte';
  import AudioSheet from '#lib/components/director/AudioSheet.svelte';
  import ReviewFindings from '#lib/components/director/ReviewFindings.svelte';
  import { h3ReviewGenerations, type ReviewItem } from '#lib/director/review.ts';
  import { DIRECTOR_COMPACT, mediaQuery } from '#lib/director/viewport.svelte.ts';
  import {
    parseTierPreference,
    preferTier,
    resolveTier,
    type RenderTier,
    type TierPreference
  } from '#lib/director/tier.ts';
  import {
    adoptCompletedBank,
    bankedResumeTarget,
    clipKey,
    dispatchIsProjectEngine,
    harvestSettled,
    jobClipKey,
    mediaIsFragment,
    planCutHydration,
    planHydration,
    type HeldClip
  } from '#lib/director/renders.ts';
  import {
    deleteClips,
    duplicateClips,
    insertClip,
    mergeClips,
    splitClipAt,
    type ClipSelection
  } from '#lib/director/clip-ops.ts';
  import {
    compileForDelivery,
    compileTimeline,
    type CompiledTimeline
  } from '#lib/director/compile.ts';
  import {
    H3_FPS,
    capabilities,
    capitalise,
    h3MaxFrames,
    historyServiceFor,
    reachableCeilingNote,
    reframeForEngine,
    beatsAvailable,
    resumeAvailable,
    retakeTailNote,
    unavailableFor,
    type DirectorEngine
  } from '#lib/director/engine.ts';
  import { ltxReceipt } from '#lib/queue/ltx-receipt.ts';
  import { watchLiveJobs, type LiveWatch } from '#lib/queue/live.ts';
  import type { EnqueueLike, QueueSubmitOptions } from '#lib/queue/options.ts';
  import { readH3Telemetry, type H3Telemetry } from '#lib/director/h3-telemetry.ts';
  import { readH3Plan, type H3Plan } from '#lib/director/h3-plan.ts';
  import H3PromptComposer from '#lib/components/director/H3PromptComposer.svelte';
  import H3PromptsScreen from '#lib/components/director/H3PromptsScreen.svelte';
  import H3PromptsPanel from '#lib/components/director/H3PromptsPanel.svelte';
  import {
    clipRenders,
    canonicalH3Store,
    collapseH3Beats,
    dedupeH3GenerationIds,
    h3BlockedClipIds,
    h3FrameMode,
    h3GenerationGist,
    h3KeyframeReferences,
    packGenerations,
    pinsAFrame,
    referencesForGeneration,
    type PackedGeneration
  } from '#lib/director/h3-generations.ts';
  import { looksLikeH3Block, parseH3Block, sectionLabel } from '#lib/director/h3-block.ts';
  import { h3Ordinals } from '#lib/director/h3-prompt.ts';
  // `realignH3Shots` is gone from here with `adoptTimelineCuts` — the workspace no longer pushes
  // clips into a block in any direction. The composer still uses it for its own section rewrites.
  import { needsH3PromptSeed, seedH3Prompt } from '#lib/director/h3-document.ts';
  import {
    H3_SOUNDTRACK_REF_ID,
    h3Timecode,
    planSoundtrack,
    soundtrackFitNote,
    withSoundtrackReference
  } from '#lib/director/h3-soundtrack.ts';
  import { applyH3Reply, h3DraftFrom } from '#lib/director/h3-llm.ts';
  import { defaultRoleFor } from '#lib/director/h3-roles.ts';
  import { isV2v, v2vSourceOf } from '#lib/director/continuity.ts';
  import { moveShot, shotStateLabel } from '#lib/director/shot-list.ts';
  import { emptyStory, readTimelineDoc } from '#lib/director/timeline-doc.ts';
  import {
    layoutClips,
    ltxDeliveredSeconds,
    toAuthoredSeconds
  } from '#lib/director/timeline-layout.ts';
  import {
    applyChatActions,
    applyH3ChatActions,
    chatHistory,
    revertChatEdit,
    revertH3ChatEdit,
    type ChatEdit,
    type ChatMsg,
    type ChatTarget,
    type H3ChatTarget
  } from '#lib/director/chat.ts';
  import {
    aspectPhrase,
    beatWordsFor,
    editorShots,
    shotContext,
    shotHeading,
    shotOneLiner,
    storyIsEmpty,
    vocalLanes,
    type PromptContext
  } from '#lib/director/prompt-context.ts';
  import {
    SCRIPT_PASS_MAX_UNITS,
    scriptPassBrief,
    scriptPassOutcome,
    scriptPassSeconds,
    type ScriptPassUnit
  } from '#lib/director/script-pass.ts';
  import {
    clipAtPromptedIndex,
    promptedClips,
    promptedIndexOf,
    shotNumberAtPromptedIndex,
    shotStatusIn
  } from '#lib/director/shot-status.ts';
  import ContinuityPanel from '#lib/components/director/ContinuityPanel.svelte';
  import {
    continuityOf,
    pendingPinNote,
    type UnitContinuity,
    type UnitTakes
  } from '#lib/director/take-continuity.ts';
  import { startsFresh } from '#lib/director/continuity.ts';
  import {
    DEFAULT_BASE_SAMPLER,
    DEFAULT_BASE_SIGMAS,
    DETAIL_STRENGTH_DEFAULT,
    PREVIEW_TIERS,
    RATIO_MULT,
    RES_PRESETS,
    baseStepsFromSigmas,
    newStage,
    planResolution,
    profileFactors,
    sizeKeyFor,
    sizeTiersFor,
    snap32,
    snapDivisorFor,
    snapTargetFor,
    type CustomStage,
    type ResPreset
  } from '#lib/director/resolution.ts';
  import '#lib/director/workspace.css';
  // ⚠️ AFTER workspace.css, always. `.dwm` mounts inside `.dw` to inherit its token layer, so the
  // two sheets share a namespace and a dozen class names; equal specificity means source order is
  // what decides. See the header comment in mobile.css.
  import '#lib/director/mobile.css';
  import MobileShell from '#lib/components/director/mobile/MobileShell.svelte';
  import MobileShots from '#lib/components/director/mobile/MobileShots.svelte';
  import MobileRender from '#lib/components/director/mobile/MobileRender.svelte';
  import MobileMore from '#lib/components/director/mobile/MobileMore.svelte';
  import MobileSongSheet from '#lib/components/director/mobile/MobileSongSheet.svelte';
  import MobileSettingsSheet from '#lib/components/director/mobile/MobileSettingsSheet.svelte';
  import MobileReworkSheet from '#lib/components/director/mobile/MobileReworkSheet.svelte';
  import MobilePrompt from '#lib/components/director/mobile/MobilePrompt.svelte';
  import MobileBlockEditor from '#lib/components/director/mobile/MobileBlockEditor.svelte';
  import MobileTakes from '#lib/components/director/mobile/MobileTakes.svelte';
  import {
    mobileRefRows,
    type MobilePromptTab,
    type MobileRef,
    type MobileTab
  } from '#lib/director/mobile.ts';
  import { tickLabel } from '#lib/director/timeline-ticks.ts';
  import {
    getLtxVideoProps,
    getLtxBank,
    getH3Props,
    isH3Request,
    ltxRequestOf,
    submitLtxVideo,
    getLtxJob,
    fetchLtxClip,
    fetchH3Preview,
    cancelLtxRender,
    transcribeDirectorAudio,
    listDirectorProjects,
    createDirectorProject,
    getDirectorProject,
    updateDirectorProject,
    patchDirectorProject,
    conflictVersion,
    deleteDirectorProject,
    listDirectorRenders,
    listDirectorActiveJobs,
    getDirectorRenderPlan,
    runDirectorProject,
    type DirectorRenderPlan,
    type DirectorActiveJob,
    type DirectorRenderItem,
    listDirectorTakes,
    selectDirectorTake,
    promoteRenderStill,
    rebuildDirectorCut,
    critiqueDirectorProject,
    type DirectorTake,
    retakeDirectorSegment,
    planDirectorTimeline,
    h3RefBriefs,
    summarizeDirectorShots,
    reworkDirectorScript,
    reviewDirectorShots,
    gistDirectorShots,
    extendDirectorScript,
    titleDirectorProject,
    directorChat,
    imageSuggestPrompt,
    imageGenerate,
    imageProps,
    fetchMediaBlob,
    ltxPromptFromLlm,
    ltxVideoScriptFromLlm,
    type DirectorEditorShot,
    type ImageProps,
    type DirectorClip,
    type DirectorClipMode,
    type DirectorCharacter,
    type DirectorH3Generation,
    type DirectorH3Reference,
    type H3PromptDraft,
    type H3Props,
    type H3ReferenceKind,
    type DirectorTimeline,
    type DirectorAssets,
    type DirectorProjectSummary,
    type DirectorPlanRequest,
    type StoryContext,
    type LyricWord,
    type LtxBankListing,
    type LtxJobStatus,
    type LtxVideoProps,
    type LtxVideoGenerateRequest,
    type LtxFrameDrop,
    type LtxRuntimeLora,
    type LtxUpscalerRatio,
    type LtxQualityProfile,
    type JobKind,
    ltxIdentityRegime,
    prepareDirectorReference
  } from '#lib/api/client.ts';
  import { effectiveClipLoras, loraSummary, loraTriggers } from '#lib/director/loras.ts';
  import {
    pushSnapshot,
    restore as restoreSnapshot,
    snapshotOf,
    type Snapshot
  } from '#lib/director/history.ts';
  import {
    addCharacterToAllClips,
    castForPrompting,
    clipCharacters,
    carriedCharacterIds,
    clipRefCount,
    frameDropAdvice,
    identityAdvisory,
    REF_SHARE_WARN,
    refTokenShare,
    newCharacter,
    pruneClipCharacters,
    resetClipCast,
    resolveClipVariants,
    roleAdvisory,
    toggleClipCharacter,
    worstClipRefCount
  } from '#lib/director/characters.ts';
  import type { Generation } from '#lib/types.ts';
  import { str, num, bool, arr, reattachNote } from '#lib/restore.ts';

  const ACTIVE_JOB_KEY = 'director-active-job';
  // Per-DEVICE, not per-project: which tier this client renders and streams is a property of the
  // screen and the connection in front of it, so it must never ride in the saved document (where it
  // would follow the project onto a desktop — and dirty it, and race the optimistic-concurrency
  // version — every time a phone opened it).
  const TIER_PREF_KEY = 'director-render-tier';

  // ── Project + global settings ────────────────────────────────────────────────
  let projectId = $state<string | null>(null);
  let projectName = $state('Untitled project');
  // The name as the server last confirmed it, so a debounced rename can tell a real edit from the
  // user typing and undoing — and so a timeline PUT (which carries the name) settles it too.
  let savedName = $state('Untitled project');
  // The row's optimistic-concurrency token. Held from the last read/write; every full-document PUT
  // quotes it, and the server rejects the write rather than let a stale copy revert the doc.
  let projectVersion = $state<number | null>(null);
  // Set when the server rejected a write because the project moved under us. Blocks all further
  // writes — including autosave — until the user picks reload or overwrite. The in-memory doc is
  // NEVER dropped on our own initiative.
  let staleDoc = $state<StaleDoc | null>(null);
  // Server-managed metadata (e.g. last_job_id/last_run_at stamped by the /run path). Round-tripped
  // on save so a save/autosave never wipes it — the PUT overwrites metadata wholesale.
  let projectMetadata = $state<Record<string, unknown>>({});
  let projects = $state<DirectorProjectSummary[]>([]);
  let dirty = $state(false);
  let saving = $state(false);
  let duplicating = $state(false);
  // Autosave: debounced background persistence to the SAME project record as the
  // manual Save button, so a manual Save is just another write to the row autosave
  // already created — nothing to reconcile. Guards against spawning empty rows.
  let autosaveTimer: ReturnType<typeof setTimeout> | null = null;
  let autosaveRetryTimer: ReturnType<typeof setTimeout> | null = null;
  let renameTimer: ReturnType<typeof setTimeout> | null = null;
  let lastSavedAt = $state<number | null>(null);
  let autosaveFailed = $state(false);
  const AUTOSAVE_MS = 2500;
  const AUTOSAVE_RETRY_MS = 12000;
  const RENAME_MS = 800;
  // Every write to the project row goes through one promise chain. Without it a rename PATCH and a
  // timeline PUT can overlap, and whichever started first quotes a version the other just bumped —
  // the editor would conflict with itself over a change the same user made.
  let writeChain: Promise<unknown> = Promise.resolve();
  let saveStatus = $derived(
    staleDoc
      ? 'Changed elsewhere — not saving'
      : saving
        ? 'Saving…'
        : autosaveFailed
          ? 'Autosave failed — will retry'
          : dirty
            ? 'Unsaved changes'
            : lastSavedAt
              ? 'All changes saved'
              : ''
  );

  let fps = $state(24);
  // Global shot length → uniform segment_frames (see note). 7s = 169 frames (8*21+1) at 24fps:
  // sized off the engine's measured 241-frame VRAM envelope minus the seam overlap, references
  // and the rest of the per-request budget. LTX holds coherence to ~20s, so this is a memory
  // ceiling, not a drift one.
  let shotSeconds = $state(7);
  // Unattended dailies review after a full render. Persisted on the timeline so the server can read
  // it with no editor open — that is what lets a render finishing at 3am review itself. Off by
  // default, so an existing project renders exactly as it always did.
  // Whether the LLM may propose Prompt Relay beats — for the planner and for per-shot drafts.
  // On by default; turn it off for a project that wants plain single-prompt shots, or when the
  // planner is being beat-happy. Hand-placed beats are unaffected either way: this only governs
  // what the model is allowed to SUGGEST, never what the editor already put on the timeline.
  let beatsEnabled = $state(true);
  // Project default for dropping the reference-contaminated opening frame of a t2v shot. "auto"
  // lets the ENGINE decide how many frames — and, more importantly, WHETHER: it self-gates the trim
  // off i2v / keyframe-at-0 / continuation shots and off shots with no references, so leaving this
  // on costs nothing on a project that isn't affected. Off is for checkpoints that don't leak.
  let frameDrop = $state<LtxFrameDrop>('auto');
  // Project-level runtime LoRAs (the set every shot inherits unless it names its own). Orthogonal
  // to `model`: a folded variant already CONTAINS its adapter, so sending the same one here would
  // apply it twice. Round-tripped through the compiled `request.loras`, like steps/cfg/seed —
  // there is no second copy on the timeline doc to drift out of sync with it.
  let loras = $state<LtxRuntimeLora[]>([]);
  let critiqueEnabled = $state(false);
  let critiqueAutoRetake = $state(false);
  let critiqueMaxRetakes = $state(2);
  let critiqueThreshold = $state(0.6);

  // width/height are the desired OUTPUT (target) resolution — the profile picks a smaller,
  // cheaper base pass that upscales to this target (backend `dims_are_target`; see resolve_quality_plan
  // in koblem/api ltx_video.rs). RES_PRESETS survives only to migrate OLD saved projects whose
  // editor blob stored a named preset (see adoptTimeline); the live UI uses the profile-driven
  // size tiers. `ratio` is the Standard profile's single upscale factor.
  //
  // resPreset persists only so OLD saved projects keep restoring their named preset (adoptTimeline).
  // The live UI drives width/height directly via the size-tier dropdown and always saves "custom",
  // so a reloaded project round-trips through its raw output dims.
  let resPreset = $state<ResPreset>('custom');
  let width = $state(RES_PRESETS['1080p'].w);
  let height = $state(RES_PRESETS['1080p'].h);
  let upscalerRatio = $state<LtxUpscalerRatio>(RES_PRESETS['1080p'].ratio);
  // Human-readable note when snapping coerced the output — the fix for the "why did 1088 become
  // 1152?" confusion. Null when nothing changed.
  let snapNote = $state<string | null>(null);
  // Snap the OUTPUT target to a multiple of the selected profile's `32 × upscale-factor`, so the
  // base pass (target ÷ factor) is a clean ×32 and every intermediate ×2 stage stays on-grid.
  // e.g. a 4× target must be ÷128 (1920×1088 → 1920×1152 for 4×; 1088 is fine for Standard/High ÷64).
  function snapTargetToProfile() {
    // 🔴 THE LTX UPSCALE LADDER ONLY. `snapDivisor` is `32 × the selected quality profile's total
    // upscale factor` — 48 for ×1.5, 64 for ×2, 128 for 4× — and it exists so the base pass
    // (target ÷ factor) lands back on ×32. H3 has no upscale chain and no quality profile, so
    // applying it there enforces a divisor derived from a ladder the project does not have, and
    // silently rewrites the canvas while doing it.
    //
    // H3's rule is the plain ×32 grid, and it is REPORTED rather than snapped: `checkCanvas` in
    // `director/engine.ts` names the offending side and offers the legal neighbours, because 720
    // and 1080 are the numbers people type and neither divides by 32.
    if (!capabilities(engine).upscaleChain) {
      snapNote = null;
      return;
    }
    const snapped = snapTargetFor(width, height, snapDivisor, resPlan.factors.length);
    width = snapped.width;
    height = snapped.height;
    snapNote = snapped.note;
    markDirty();
  }
  // Switch quality profile while keeping the same named output tier where possible (re-sized for
  // the new profile's factor) — so standard→4× stays on "1080p" instead of snapping into Custom.
  // A Custom size falls back to snapping the raw dims (with the explanatory note).
  function changeProfile(next: LtxQualityProfile) {
    const keepTier = sizeSel; // read against the OLD profile before mutating
    qualityProfile = next;
    if (keepTier !== 'custom') {
      const tier = sizeTiers.find((t) => t.key === keepTier); // now recomputed for the new profile
      if (tier) {
        width = tier.w;
        height = tier.h;
        snapNote = null;
        markDirty();
        return;
      }
    }
    snapTargetToProfile();
  }
  // Pick a canonical output tier (already snapped for the current profile — so nothing snaps
  // silently) or reveal Custom size inputs.
  function selectSize(key: string) {
    resPreset = 'custom';
    if (key === 'custom') {
      sizeCustom = true; // latch into custom so the W/H inputs show even if dims match a tier
      snapNote = null;
      markDirty();
      return;
    }
    const tier = sizeTiers.find((t) => t.key === key);
    if (tier) {
      sizeCustom = false;
      width = tier.w;
      height = tier.h;
      snapNote = null;
      markDirty();
    }
  }
  /**
   * `engine` has moved — drop everything the editor holds that names the one it moved AWAY from.
   *
   * 🔴 A resume target is an opaque id in another service's job store, and the two LTX deployments
   * share none of them: "a job id from one is a 404 on the other". These two are persisted in the
   * saved document and rehydrated on load, so without this the tick-box and "Render this shot" kept
   * quoting a 2.3 chain at LTX-2.5 — every shot of the render a 404, and `restore_selected_takes`
   * stamping the same foreign ids onto every segment beside it. The rows keep their own engine
   * (`0039`), so switching BACK finds the chain again; nothing is deleted here.
   *
   * 🔴 Called by EVERY mover of `engine` that leaves the editor's own state standing — the picker
   * below and `applySnapshot`. That is the whole invariant: what this holds always belongs to the
   * engine it is on. (`adoptTimeline` and `newProject` are not movers in that sense: they install a
   * whole document, ids included.) Five rounds of "clear it at the site that moved it" each left the
   * next path uncovered, so the rule is stated once and every mover takes it.
   */
  function dropEngineBankState() {
    finalResumeJobId = null;
    proxyResumeJobId = null;
    // 🔴 And the targets of the render that is STILL GOING, which is the half that made clearing the
    // two above insufficient: the completion re-installs a resume target from the job it watched,
    // and the id it installs is the dir of the engine that ran it. `adoptCompletedBank` refuses that
    // write outright (the picker is disabled mid-render, so this is the belt to its braces), and
    // these two are what it would otherwise be handed.
    pendingFinalResumeTarget = null;
    pendingProxyResumeTarget = null;
    // The banked prefixes those ids addressed. Left behind they would report a continue as aligned
    // against shots the new engine has not rendered.
    lastRenderPrompts = [];
    proxyLastPrompts = [];
    // Prior renders offered as v2v sources are named by the same engine dir id, and a shot pointed
    // at one compiles it into `v2v_source` — so they are foreign the moment the engine moves,
    // whether or not the new engine even has a v2v node. The shot keeps its mode and reports itself
    // as needing a source (`v2vMissingSource`) rather than shipping an id nothing can resolve.
    priorRenders = [];
  }
  /**
   * The picker moved the engine: take the drop above, and take the clips' own prior-render sources
   * with it — they are the same foreign dir id, and here the clips STAY (an undo step instead
   * replaces them wholesale with a list the restored engine minted, which is why the loop is here
   * rather than in `dropEngineBankState`).
   *
   * On the change event rather than in an `$effect`, exactly like the canvas reframe beside it in
   * `SettingsSheet`: loading a project also moves `engine`, and an effect could not tell that apart
   * from a switch — it would clear the ids `adoptTimeline` had just restored.
   */
  function changeEngine() {
    dropEngineBankState();
    for (const c of clips) c.v2v_prior_job_id = null;
    // 🔴 Switching INTO H3 is a third door into the generation store: a document loaded as LTX keeps
    // whatever `h3_generations` it carries, and `canonicaliseGenerationStore` is engine-gated (the
    // canonical id is minted from an H3 packing, which only exists on H3). Without this line a store
    // written by an older build would go straight from an LTX load to being written to — and a write
    // to a generation whose id the packer no longer mints appends a second entry that adoption never
    // picks, i.e. the edit comes back missing.
    canonicaliseGenerationStore();
    markDirty();
  }
  let model = $state<string | undefined>(undefined);
  /**
   * Which engine this PROJECT renders on. Next to `model` because it is the same category of
   * thing — a property of the saved document that decides what the compiled request may contain,
   * not a UI preference. Every control that only one engine has is gated on `caps` below.
   */
  let engine = $state<DirectorEngine>('ltx25');
  const caps = $derived(capabilities(engine));
  /**
   * Hold the document's `fps` to what a fixed-rate engine actually renders at.
   *
   * 🔴 Nothing else did. An LTX project at 30 fps switched to H3 kept the 30 — while the packer
   * computed every generation on `H3_FPS`, so the frame grid the length stepper walked and the one
   * the packer snapped to were different grids, and every "N frames at M fps" line on screen named
   * a rate the render would not use. Corrected in place rather than only on the picker, so a
   * document that arrives inconsistent is right the moment it opens.
   */
  $effect(() => {
    if (caps.fixedFps !== null && fps !== caps.fixedFps) fps = caps.fixedFps;
  });
  /**
   * What this engine's units are CALLED on screen — `shot` on both, plus `cut` where a render holds
   * several. Passed DOWN to every child that draws a label rather than each of them comparing
   * engines: see `EngineVocab` in `lib/director/engine.ts` for why the render unit is a "shot" on
   * H3 even though the type is `DirectorH3Generation`.
   */
  const vocab = $derived(caps.vocab);
  /**
   * Show the {@link EngineVocab.renderUnit} the selected beat sits in.
   *
   * 🔴 It no longer has to FIND anything. There used to be a third tab whose body was driven by its
   * own `genTab` index, so opening it meant computing "which generation holds the selection" and
   * writing that down — and the two could then disagree. `activeGenIndex` derives it from the
   * selection now, so the only thing left is to select the beat.
   */
  function showGeneration(i: number) {
    const first = packedGenerations[i]?.shots[0]?.clip.id;
    if (first) selectedIds = [first];
    activeTab = 'prompt';
  }
  /** H3 omni references, project-level and IN ORDER — position assigns each one's ordinal. */
  let h3References = $state<DirectorH3Reference[]>([]);
  /** `GET /api/v1/minimax-h3/props`. null until fetched, and when H3 is unconfigured. */
  let h3Props = $state<H3Props | null>(null);
  let audioKind = $state<'none' | 'voice' | 'bgm' | 'mixed'>('none');
  // Declared here rather than with the rest of the audio state: the H3 soundtrack derivations in
  // "Timeline state" read both, and a `$derived` above its source is a svelte-check error even
  // though the runes graph resolves it lazily at runtime.
  let audioB64 = $state<string | null>(null);
  let waveDuration = $state(0);

  // Advanced render controls (chain-global). undefined/empty ⇒ engine defaults
  // (steps 8, cfg 1.0, seed random, no negative prompt).
  let steps = $state<number | undefined>(undefined);
  let cfgScale = $state<number | undefined>(undefined);
  let negativePrompt = $state('');
  let seed = $state<number | undefined>(undefined);

  // Quality profile: Standard (default, byte-identical ver3), High, 4× Upscale, or Custom.
  // Custom reveals the base-pass overrides + a repeatable upscale-stage editor below.
  let qualityProfile = $state<LtxQualityProfile>('standard');
  // Custom base pass overrides (only sent when profile === "custom"; else backend bakes them).
  let customBaseSampler = $state(DEFAULT_BASE_SAMPLER);
  let customBaseSigmasText = $state(DEFAULT_BASE_SIGMAS);
  let customStages = $state<CustomStage[]>([newStage()]);
  // "Detail / upscale strength": the final refine stage's renoise sigma0 for the baked High / 4×
  // recipes (default 0.85 = the baked schedule). Only sent when moved off 0.85, so an untouched
  // render stays byte-identical. Not applied to Standard (no upscale refine) or Custom (its
  // per-stage sigmas are edited directly below).
  let detailStrength = $state(DETAIL_STRENGTH_DEFAULT);
  const customBaseSteps = $derived(baseStepsFromSigmas(customBaseSigmasText));
  function addStage() {
    customStages = [...customStages, newStage()];
    snapTargetToProfile(); // stage count changed → re-snap the target for the new base factor
  }
  function removeStage(i: number) {
    customStages = customStages.filter((_, j) => j !== i);
    snapTargetToProfile();
  }
  // The resolution ladder for the selected profile: base pass → each ×N upscale → target output.
  //
  // 🔴 No factors at all where the engine has no ladder, which is the same gate `snapTargetToProfile`
  // and every `SettingsSheet` row already take. Without it a fresh LTX-2.5 project — which starts on
  // the LTX default `upscalerRatio: 'x2'` with the picker hidden — drew the ×2 ladder in the size
  // dropdown ("1080p — 1920×1088 (base 960×544)") for a base pass that engine never renders, and
  // snapped every offered tier to the ladder's ÷64 instead of the engine's own ×32 grid. It
  // corrected itself only after a save and a reload, which is not a way to be right.
  const resPlan = $derived(
    planResolution(
      width,
      height,
      caps.upscaleChain ? profileFactors(qualityProfile, upscalerRatio, customStages) : []
    )
  );
  const snapDivisor = $derived(snapDivisorFor(resPlan.mult));
  // Output = the target the ladder resolves to (what actually hits VRAM at decode). Surfaced for
  // the OOM guard + readout.
  const outW = $derived(resPlan.targetW);
  const outH = $derived(resPlan.targetH);
  const sizeTiers = $derived(sizeTiersFor(snapDivisor, resPlan.mult));
  // Explicit "Custom size…" latch. Without it, picking Custom while the dims still match a tier
  // (the usual case — you start on a tier) would instantly re-derive back to that tier and the
  // manual W/H inputs would never show. Set by selectSize("custom"), cleared when a tier is picked.
  let sizeCustom = $state(false);
  // The tier the current output matches, or "custom" (shows the manual W/H inputs). The latch wins
  // so an explicit Custom choice sticks even when the current dims happen to equal a tier.
  const sizeSel = $derived(sizeCustom ? 'custom' : sizeKeyFor(sizeTiers, width, height));
  // Continue an earlier render: the engine reuses its banked shots and renders only the
  // newly-added tail shots (motion-continuous). Existing shots are NOT re-rendered — edits to
  // them are ignored on a continue pass. Source = most recent completed render (priorRenders[0]).
  let continueFromLast = $state(false);
  // The non-empty prompts that were banked by the last render, in order. Continue is only valid
  // when the current timeline still begins with this exact prefix (the engine lines new shots up
  // after the banked ones — reorder/delete/edit within the prefix would misalign).
  let lastRenderPrompts = $state<string[]>([]);
  let pendingRenderPrompts: string[] = [];
  // The ENGINE server_job_id whose bank holds SEGMENT 0 of the FINAL prefix — what `resume_job_id`
  // must be (the engine knows nothing of koblem's job ids; it resolves its own bank by this one,
  // starting at segment 0). Persisted in the doc so "Render new shots" / "Render this shot" stay
  // incremental across a reload. Mirrors proxyResumeJobId — see the resume-id note there. `null` →
  // next render is a full prefix.
  let finalResumeJobId = $state<string | null>(null);
  let pendingFinalResumeTarget = $state<string | null>(null);
  /**
   * The in-flight render's `/media` is a FRAGMENT of the cut, not the cut.
   *
   * 🔴 Two things follow, and both were wrong before it existed: the engine's container for such a
   * job is one render unit, NOT the cut — adopting it as `resultUrl` would replace a four-minute
   * cut in the drawer with a fifteen-second clip — and the stitched cut is left stale, because
   * `harvest_director_renders` skips the `shot_index = -1` row on a partial render.
   * `hydrateAfterHarvest` re-joins the selected takes when this is set.
   *
   * Written from three places, which is deliberate: the single-unit dispatchers set it before the
   * render starts, `adoptActiveJob` takes the server's answer in place of a prediction it never
   * made, and `watchJob` folds in the job's own `job_is_partial` — the only signal for a FULL
   * render that failed to concatenate. `mediaIsFragment` owns how they combine.
   */
  let pendingPartialRender = $state(false);

  // ── Auto-direct (LLM plans a full timeline from a brief) ─────────────────────
  let autoBrief = $state('');
  let autoMaxShots = $state(16);
  let autoDirecting = $state(false);

  // Stage 1: /plan's structural draft, held OFF the timeline until the owner hits Go.
  let planReview = $state<{
    timeline: DirectorTimeline;
    summary: string | null;
  } | null>(null);

  // Stage 2: draft-all. Runs in the background — the editor stays live while prompts land.
  let draftAllTotal = $state(0);
  let draftAllDone = $state(0);
  let draftAllLabel = $state('');
  let draftAllRunning = $state(false);
  let draftFailedIds = $state<string[]>([]);

  // Dropped-id / structural warnings the LLM endpoints report. Surfaced, never silently binned.
  let llmWarnings = $state<string[]>([]);
  function noteWarnings(w: string[] | undefined) {
    if (w?.length) llmWarnings = [...llmWarnings, ...w];
  }

  // Shot-list summary (LLM prose synopsis of the current timeline). `summaryStale`
  // flags that shots changed since the synopsis was generated, so it isn't trusted as current.
  let summarizing = $state(false);
  let summary = $state<string | null>(null);
  let summaryStale = $state(false);

  // Whole-script rework + continuity review.
  let reworkInstruction = $state('');
  let reworking = $state(false);
  let gistingAll = $state(false);
  let reviewing = $state(false);
  let reviewRan = $state(false);
  let reviewFindings = $state<ReviewItem[]>([]);
  let fixingAll = $state(false);
  function clearReview() {
    reviewFindings = [];
    reviewRan = false;
    reworkInstruction = '';
  }

  // Extend the story (+N shots).
  let extendCount = $state(3);
  let extendDirection = $state('');
  let extending = $state(false);

  // Auto-title.
  let titling = $state(false);

  // Storyboard (FLUX preview stills).
  let imgProps = $state<ImageProps | null>(null);
  let storyboarding = $state(false);
  let storyboardProgress = $state('');

  // Quick sanity preview (trimmed, low-res render).
  let previewShots = $state(3);

  // Revert net: a durable one-level snapshot taken BEFORE each destructive AI batch (rework-all,
  // extend, auto-fix-all, chat edits) so a bad LLM rewrite can be rolled back — even after reload.
  // Snapshots clips + story only (not the heavy audio); persisted best-effort to localStorage.
  /**
   * ⚠️ `h3` is OPTIONAL and must stay optional: a snapshot persisted before this field existed has
   * no key, and reverting from one has to leave the blocks alone rather than clear them.
   *
   * 🔴 It was added for the one-pass writer. On H3 the block is the document (`h3-document.ts`) and
   * `clip.prompt` is a mirror of it — so a restore that put the clips back and left the generations
   * holding the LLM's blocks would not be an undo at all: it would be the timeline and the render
   * disagreeing on every shot, reported as drift, with nothing saying why.
   */
  type AiRestore = {
    clips: string;
    story: string;
    h3?: string;
    /**
     * The engine those clips were captured ON. Recorded for the same reason an undo step records it
     * (`history.ts`): the restored clips carry `v2v_prior_job_id`s that only one engine can resolve,
     * and this restore point outlives a reload, so the two have to travel together.
     */
    engine?: DirectorEngine;
    label: string;
  };
  let aiRestore = $state<AiRestore | null>(null);
  function restoreKey(): string | null {
    return projectId ? `director-restore-${projectId}` : null;
  }
  function captureRestore(label: string) {
    aiRestore = {
      clips: JSON.stringify($state.snapshot(clips)),
      story: JSON.stringify($state.snapshot(story)),
      engine,
      // Only the authored sections are stored — the packing itself is re-derived, so restoring the
      // clips restores the generations they pack into and this puts back what was WRITTEN for them.
      h3: JSON.stringify($state.snapshot(h3Generations)),
      label
    };
    persistRestore();
  }
  function persistRestore() {
    const key = restoreKey();
    if (!key) return;
    try {
      if (aiRestore) localStorage.setItem(key, JSON.stringify(aiRestore));
      else localStorage.removeItem(key);
    } catch {
      /* over quota (large storyboard) — in-session revert still works */
    }
  }
  function loadRestore() {
    aiRestore = null;
    const key = restoreKey();
    if (!key) return;
    try {
      const raw = localStorage.getItem(key);
      if (raw) aiRestore = JSON.parse(raw) as AiRestore;
    } catch {
      /* ignore malformed */
    }
  }
  function revertAiChange() {
    if (!aiRestore) return;
    try {
      clips = JSON.parse(aiRestore.clips) as DirectorClip[];
      story = JSON.parse(aiRestore.story) as StoryContext;
      // ⚠️ Absent on a snapshot taken before the field existed — those revert the clips only, which
      // is exactly what they always did. `undefined` and `[]` are different states here too.
      //
      // 🔴 Repaired on the way in, exactly like `adoptTimeline`. This is the OTHER door into the
      // store, and it is the one that outlives a fix: a restore point is JSON in `localStorage`,
      // written by whichever build was running when it was captured, so a blob holding two entries
      // under one id can be re-installed into a session long after the store itself was repaired —
      // and `updateGeneration` resolves an id by `findIndex`.
      if (aiRestore.h3 !== undefined) {
        const restored = JSON.parse(aiRestore.h3) as DirectorH3Generation[];
        h3Generations = dedupeH3GenerationIds(restored) ?? restored;
      }
    } catch {
      renderError = "couldn't revert — the restore point was unreadable";
      return;
    }
    // The clips came back holding the ids they were captured with, so the engine comes back with
    // them — the same pairing an undo step keeps, on the other path that replaces the clip list
    // wholesale. ⚠️ Absent on a restore point written before the field existed: it names no engine,
    // so it moves none.
    if (aiRestore.engine && aiRestore.engine !== engine) {
      engine = aiRestore.engine;
      dropEngineBankState();
    }
    // 🔴 AFTER the engine swap above, not inside the parse: the helper is engine-gated and a restore
    // point is allowed to be the thing that brings the project back to H3. The clips are in place by
    // now, so the packing — and therefore the canonical id of every generation — is knowable. Without
    // this, a blob captured by an older build re-installs ids the packer no longer mints, and the next
    // write to one of those generations appends a second entry that adoption never picks.
    canonicaliseGenerationStore();
    selectedIds = clips[0] ? [clips[0].id] : [];
    resetHistory();
    aiRestore = null;
    persistRestore();
    markDirty();
  }

  // ── Director chat (context-grounded, can edit the timeline via JSON actions) ──
  let chatOpen = $state(false);
  let chatBusy = $state(false);
  let chatMessages = $state<ChatMsg[]>([]);

  // ── Workspace redesign: view-only UI state (data + handlers above are unchanged) ──
  /**
   * The stage's two tabs.
   *
   * 🔴 **Two, not three.** On H3 the stage used to carry `Prompt | Cut details | Shot`, where the
   * first two edited one CUT and the third edited the render they were packed into — which is the
   * confusion the whole reframe exists to remove, and it left the prompt (the deliverable) buried on
   * the third tab. Now both tabs are about the {@link EngineVocab.renderUnit}: `prompt` is
   * references → LLM entry → the prompt itself → the exploded fields, and `details` is how it
   * renders. On LTX a shot IS a render, so the strip reads exactly as it always did.
   */
  let activeTab = $state<'prompt' | 'details'>('prompt');
  let settingsOpen = $state(false);
  let rendersOpen = $state(false);
  let reviewOpen = $state(false);
  let projectMenuOpen = $state(false);
  let audioOpen = $state(false);
  let foldStory = $state(true);
  let foldCast = $state(false);
  let foldAuto = $state(false);
  let foldRevise = $state(false);
  // Rail panels stay compact inline; the ⤢ button pops the same fields into a roomy modal.
  let expandPanel = $state<null | 'story' | 'cast' | 'auto' | 'revise'>(null);
  /**
   * The Prompts screen — every H3 block at once, with one find-and-replace across all of them.
   *
   * 🔴 A screen rather than a rail panel, and it does NOT reuse `expandPanel`. Those four are one
   * generation's fields blown up to a roomy size; this is the whole project's text, it is the width
   * of the sheet rather than 640px, and its state (the filter, the query, the cursor) has to survive
   * being scrolled through — none of which the tabbed panel modal is shaped for.
   */
  let promptsOpen = $state(false);
  /**
   * The seeds the Prompts screen shows for units nobody has written, cached for as long as it is open.
   *
   * 🔴 Deliberately a plain `Map`, not `$state`: it is written from inside the `$derived` that reads
   * it, and a reactive one would re-invalidate that derivation from its own body. Nothing else reads
   * it, so there is nothing to keep in step.
   *
   * Cleared on close ({@link closePrompts}) rather than never, because the timeline can be edited
   * between two visits and a seed is a function of the clips. It cannot go stale WHILE open: the only
   * thing the screen can change is a block's text, and a block that has been edited has a stored
   * `prompt` and never consults its seed again.
   */
  let h3SeedCache = new Map<string, string>();
  function closePrompts() {
    promptsOpen = false;
  }
  // Which of the two postures the take is in. `false` is the WRITING one — a thumbnail in the rail
  // (`RailDock`) and the whole stage given to the prompt; `true` puts the full `StageViewer` across
  // the stage, which is the posture for actually watching a take. Escape comes back.
  let viewerExpanded = $state(false);
  // The stage is its own scroll container (`.work > *`), so opening a shot from the list below can
  // bring its panel back into view.
  let stageEl = $state<HTMLElement | null>(null);
  // Narrow-screen layouts that CSS can't express: which component owns the cut (timeline vs shot
  // list), and which render tier this client defaults to.
  const compact = mediaQuery(DIRECTOR_COMPACT);
  // ⚠️ Must match the breakpoint workspace.css hides `.rail` at. The rail is where the take lives
  // now, so below this the stage has to keep the full viewer — otherwise the shot has no picture
  // AND no way to ask for one, the expand control having gone with the rail.
  const railHidden = mediaQuery('(max-width: 1080px)');
  // Below `compact` the whole desktop layout is replaced by `MobileShell` rather than squeezed, so
  // these two are the phone's entire navigation state. They live here, with everything else, because
  // the shell is presentation: it renders what it is handed and owns no part of the document.
  let mobileTab = $state<MobileTab>('shots');
  let mobileSheet = $state<string | null>(null);
  // 🔴 The Prompt screen's own view state, held here for the same reason: `MobileShell`'s body is an
  // `{#if sheet}…{:else if tab}` ladder, so pushing ANY sheet destroys that screen. Left inside
  // `MobilePrompt` these came back reset — Sections → "Rework this section" → Back landed on Prompt
  // with the section lost, and the expanded take player collapsed and refetched its video.
  let mobilePromptSub = $state<MobilePromptTab>('prompt');
  let mobilePromptSection = $state<string | null>(null);
  let mobileTakeExpanded = $state(false);
  let mobileCard = $state<MobileRef | null>(null);
  let mobileDeadLabel = $state<string | null>(null);
  let tierPref = $state<TierPreference>('auto');
  let renderTier = $derived(resolveTier(tierPref, compact.current));
  function setTierPref(next: TierPreference) {
    tierPref = next;
    try {
      localStorage.setItem(TIER_PREF_KEY, next);
    } catch {
      /* a device that refuses storage just re-decides on every load */
    }
    // The tier governs which clips are DOWNLOADED, not merely which are shown, so choosing one is
    // what pulls its bytes. Only on an explicit choice: a viewport change (an `auto` client
    // rotating a phone) keeps playing what it already holds — `preferTier` falls back to the other
    // tier — rather than spending a 19 MB download on a resize.
    if (projectId && renderRows.length) {
      void hydrateTierClips(projectId, resolveTier(next, compact.current));
    }
  }
  // Timeline defaults to a fixed 15s window (reflows with lane width); Fit / presets / manual zoom override.
  let fitTimeline = $state(false);
  let windowSeconds = $state<number | null>(15);
  let zoomSel = $derived(
    fitTimeline ? 'fit' : windowSeconds != null ? String(windowSeconds) : 'custom'
  );
  let promptBusy = $state(false);
  let snapBusy = $state(false);
  let reworkShotText = $state('');
  let batchText = $state('');
  let retakeSeedInput = $state(-1);
  // The revert-last-AI toast rides on the existing `aiRestore` snapshot; dismissing pins its label.
  let aiToastDismissed = $state<string | null>(null);
  let showAiToast = $derived(!!aiRestore && aiRestore.label !== aiToastDismissed);
  function dismissAiToast() {
    if (aiRestore) aiToastDismissed = aiRestore.label;
  }

  const MODE_META: { value: DirectorClipMode; label: string; hint: string }[] = [
    { value: 't2v', label: 'Text to video', hint: 'Prompt only' },
    { value: 'i2v', label: 'Image to video', hint: 'Start from a still' },
    {
      value: 'v2v_edit',
      label: 'Edit a clip',
      hint: 'Keep the scene, add a change'
    },
    {
      value: 'v2v_restyle',
      label: 'Restyle a clip',
      hint: 'Re-render under a new prompt'
    }
  ];
  const modeLabel = (m: DirectorClipMode) => MODE_META.find((x) => x.value === m)?.label ?? m;

  function setClipMode(m: DirectorClipMode) {
    if (!selectedClip) return;
    selectedClip.mode = m;
    markDirty();
  }
  function gotoShot(clipId: string) {
    selectedIds = [clipId];
    reviewOpen = false;
    activeTab = 'prompt';
  }
  async function runReview() {
    await reviewShots();
    if (reviewFindings.length) reviewOpen = true;
  }

  /**
   * A clip's index in ENGINE space — the one every `partials[]` entry, every persisted render row
   * and every take is keyed by. THE single place that mapping is decided.
   *
   * 🔴 The two engines do not share a space. On LTX it is the PROMPT-FILTERED shot index (the API
   * drops empty-prompt segments before the engine sees them). On H3 the render unit is the
   * GENERATION: `retake_segment_h3` tags its job `shot_offset = generation_index`, the harvest
   * files that job's `seg_0` there, and the bank key is `"<engine job id>:<generation index>"`. A
   * prompt-filtered SHOT index therefore addresses a different generation's footage on any project
   * whose generations hold more than one shot — which is the packer's normal case, since its whole
   * job is to fit as many shots into a generation as the 15.083 s budget allows.
   *
   * -1 for "not in engine space at all" on both arms: `findIndex` already returns it for a clip
   * that packed into no generation (it has no prompt, so it never renders).
   */
  const engineIndexOf = (c: DirectorClip): number =>
    caps.generationIsRetakeUnit
      ? packedGenerations.findIndex((g) => g.shots.some((s) => s.clip.id === c.id))
      : promptedIndexOf(clips, c.id);

  // Per-shot render state, one tier each. A proxy render in flight must NOT flip the final dots to
  // busy, so only the tier the live render belongs to is given its partials.
  const shotStatus = (c: DirectorClip) =>
    shotStatusIn(
      clips,
      c,
      {
        banked: lastRenderPrompts,
        live: renderIsMine && activeRenderTier === 'final' ? partialShots : null,
        bankedIsUnitScoped: caps.generationIsRetakeUnit
      },
      engineIndexOf(c)
    );
  const proxyStatus = (c: DirectorClip) =>
    shotStatusIn(
      clips,
      c,
      {
        banked: proxyLastPrompts,
        live: renderIsMine && activeRenderTier === 'proxy' ? proxyShots : null,
        bankedIsUnitScoped: caps.generationIsRetakeUnit
      },
      engineIndexOf(c)
    );
  /** The clip an engine-space index belongs to — a generation's FIRST shot on H3. */
  const partialClip = (engineIdx: number) =>
    caps.generationIsRetakeUnit
      ? (packedGenerations[engineIdx]?.shots[0]?.clip ?? null)
      : clipAtPromptedIndex(clips, engineIdx);
  /** …and its RAW editor shot number, so a label matches the stage slate and the timeline. */
  const partialShotNo = (engineIdx: number) => {
    if (!caps.generationIsRetakeUnit) return shotNumberAtPromptedIndex(clips, engineIdx);
    const c = packedGenerations[engineIdx]?.shots[0]?.clip;
    return c ? clips.findIndex((x) => x.id === c.id) + 1 : engineIdx + 1;
  };

  async function runPromptLlm(fn: () => void | Promise<void>) {
    promptBusy = true;
    renderError = null;
    try {
      await fn();
    } catch (e) {
      renderError = (e as Error).message;
    } finally {
      promptBusy = false;
    }
  }
  async function reworkThisShot() {
    const t = reworkShotText.trim();
    if (!t) return;
    await runPromptLlm(() => reworkClip(t));
    if (!renderError) reworkShotText = '';
  }
  async function runSnapshot() {
    snapBusy = true;
    renderError = null;
    try {
      await snapshotSelectedClip();
    } catch (e) {
      renderError = (e as Error).message;
    } finally {
      snapBusy = false;
    }
  }

  // Per-shot media handlers — the stage owns them. All act on selectedClip.
  /**
   * The frame path a NEWLY attached H3 image starts on.
   *
   * 🔴 Written into the document, never derived at read time. An exact pin renders on `fl2va`,
   * which takes no audio reference at all — so on a project with a song the pin silently drops that
   * generation's slice of the track, and the prose keyframe (0.776 frame-0 NCC, free, `ref2va`) is
   * the right default. But resolving that default LAZILY, from "does the project have a song right
   * now", would change what an existing project renders the moment a track was attached, with
   * nothing on screen moving. So it is chosen once, here, and `h3FrameMode` reads only the field.
   *
   * ⚠️ Only ever set on a clip that has NO mode yet. Re-attaching an image must not overrule a
   * choice the author made in the shot's own frame picker.
   */
  function defaultH3FrameMode(c: DirectorClip) {
    if (engine !== 'h3' || c.h3_frame_mode !== undefined) return;
    c.h3_frame_mode = hasProjectSong ? 'keyframe' : 'pin';
  }
  /**
   * Attach the image a shot OPENS on — from either shell.
   *
   * 🔴 The mode flip is part of attaching, not a phone convenience. `compileSegment` and
   * `h3HasFrameImage` both read `image_b64` only on an `i2v` shot, so storing one on a `t2v` shot
   * puts bytes in the document that no render will ever see. The desktop dropzone happens to sit
   * behind a `mode === 'i2v'` guard and the phone's button does not; one definition means the two
   * cannot answer differently if either guard moves.
   */
  function setOpeningFrame(c: DirectorClip, image: string) {
    c.image_b64 = image;
    if (c.mode === 't2v') c.mode = 'i2v';
    defaultH3FrameMode(c);
    markDirty();
  }
  async function onSceneImageFiles(files: FileList | null) {
    if (!selectedClip || !files?.[0]) return;
    setOpeningFrame(selectedClip, await putAsset(files[0]));
  }
  async function onV2vSourceFiles(files: FileList | null) {
    if (!selectedClip || !files?.[0]) return;
    selectedClip.v2v_source_b64 = await putAsset(files[0]);
    // Dropping a file IS choosing the upload kind. Without this, a shot saved against a prior
    // render on an engine that cannot read one stays pointed at it, so the clip just attached would
    // be ignored by the resolver and the shot would still report itself as source-less.
    selectedClip.v2v_source_kind = 'upload';
    markDirty();
  }
  async function onPinFiles(files: FileList | null) {
    if (!selectedClip || !files?.[0]) return;
    const image_b64 = await putAsset(files[0]);
    selectedClip.keyframes = [...selectedClip.keyframes, { image_b64, seconds: 0 }];
    markDirty();
  }
  /**
   * H3 only: attach this shot's CLOSING anchor (`fl2va`'s last frame).
   *
   * Stored on the clip rather than as a keyframe at `seconds = duration`: a keyframe is a mid-shot
   * pin the LTX engine interpolates through, and H3 has no such thing — it anchors exactly the
   * first and last frame, and expresses them as two distinct request fields.
   */
  async function onEndFrameFiles(files: FileList | null) {
    if (!selectedClip || !files?.[0]) return;
    selectedClip.h3_end_image = await putAsset(files[0]);
    defaultH3FrameMode(selectedClip);
    markDirty();
  }
  function removePin(i: number) {
    if (!selectedClip) return;
    selectedClip.keyframes = selectedClip.keyframes.filter((_, k) => k !== i);
    markDirty();
  }

  // ---- Prompt Relay beats -------------------------------------------------
  // Timed sub-prompts inside ONE continuous shot. The clip prompt stays the setting and conditions
  // every frame with no penalty; each beat gets cross-attention priority around its own moment.
  // Zero beats keeps the engine on its null-mask fast path, so ordinary shots cost nothing.
  function addBeat() {
    if (!selectedClip) return;
    const beats = selectedClip.beats ?? [];
    if (beats.length >= 3) return;
    const last = beats.length ? Math.max(...beats.map((b) => b.seconds)) : -1;
    selectedClip.beats = [
      ...beats,
      {
        seconds: Math.min(
          Math.max(0, last + selectedClip.seconds / (beats.length + 2)),
          Math.max(0, selectedClip.seconds - 0.1)
        ),
        text: ''
      }
    ];
    markDirty();
  }
  function removeBeat(i: number) {
    if (!selectedClip) return;
    selectedClip.beats = (selectedClip.beats ?? []).filter((_, k) => k !== i);
    markDirty();
  }
  /** Snap a beat onto the nearest lyric word inside this shot, and label it with that word. */
  function snapBeat(i: number) {
    const beat = selectedClip?.beats?.[i];
    if (!beat || selectedBeatWords.length === 0) return;
    const nearest = selectedBeatWords.reduce((best, w) =>
      Math.abs(w.offset - beat.seconds) < Math.abs(best.offset - beat.seconds) ? w : best
    );
    beat.seconds = nearest.offset;
    beat.label = nearest.text.trim();
    markDirty();
  }
  async function onClipRefFiles(files: FileList | null) {
    if (!selectedClip || !files?.[0]) return;
    const image_b64 = await putAsset(await downscaleImageToBlob(files[0]));
    selectedClip.references = [
      ...(selectedClip.references ?? []),
      { image_b64, role: 'reference' }
    ];
    markDirty();
  }
  function removeClipRef(i: number) {
    if (!selectedClip) return;
    selectedClip.references = (selectedClip.references ?? []).filter((_, k) => k !== i);
    markDirty();
  }
  function snapshotToScene() {
    if (!selectedClip?.snapshot_b64) return;
    selectedClip.image_b64 = selectedClip.snapshot_b64;
    selectedClip.mode = 'i2v';
    markDirty();
  }
  function snapshotToReference() {
    if (!selectedClip?.snapshot_b64) return;
    selectedClip.references = [
      ...(selectedClip.references ?? []),
      { image_b64: selectedClip.snapshot_b64, role: 'reference' }
    ];
    markDirty();
  }
  async function retakeCurrent() {
    await retakeSelectedClip(Number.isFinite(retakeSeedInput) ? Math.trunc(retakeSeedInput) : -1);
  }

  // Story-context handlers, for the rail fold.
  function storyAddRef() {
    story.references = [...story.references, { label: '', description: '', image_b64: null }];
    markDirty();
  }
  function storyRemoveRef(i: number) {
    story.references = story.references.filter((_, k) => k !== i);
    markDirty();
  }
  async function storyRefImage(i: number, files: FileList | null) {
    if (!files?.[0]) return;
    story.references[i].image_b64 = await putAsset(await downscaleImageToBlob(files[0]));
    markDirty();
  }
  function storyClearRefImage(i: number) {
    story.references[i].image_b64 = null;
    markDirty();
  }

  // Continue scene: the LLM drafts the next shot from story + prior prompts (Extend-by-one; no render
  // dependency — reuses the extend path with count 1).
  async function continueScene() {
    const prevCount = extendCount;
    const prevDir = extendDirection;
    extendCount = 1;
    extendDirection = '';
    try {
      await extendStory();
    } finally {
      extendCount = prevCount;
      extendDirection = prevDir;
    }
  }

  // Batch ops over the multi-selection (timeline batch bar).
  async function reworkSelected() {
    const t = batchText.trim();
    if (!t || selectedIds.length === 0) return;
    captureRestore('Rework selected');
    promptBusy = true;
    renderError = null;
    try {
      for (const id of [...selectedIds]) {
        const clip = clips.find((c) => c.id === id);
        if (clip) await reworkClipCore(clip, t);
      }
      batchText = '';
    } catch (e) {
      renderError = (e as Error).message;
    } finally {
      promptBusy = false;
    }
  }
  async function storyboardSelected() {
    if (selectedIds.length === 0) return;
    storyboarding = true;
    renderError = null;
    try {
      for (const id of [...selectedIds]) {
        const clip = clips.find((c) => c.id === id);
        if (clip && !clip.snapshot_b64) {
          try {
            await snapshotClip(clip);
          } catch (e) {
            renderError = (e as Error).message;
            break;
          }
        }
      }
    } finally {
      storyboarding = false;
    }
  }

  // The editor state the chat's timeline edits reach into. `detach` unwraps a clip out of the
  // reactive proxy, so a deleted shot can be restored whole.
  const chatTarget = (): ChatTarget => ({
    clips,
    story,
    defaultSeconds: shotSeconds,
    // A getter, not a copy: an add earlier in the same batch REPLACES the selection array, and a
    // later delete has to see that rather than the one the batch started with.
    get selectedIds() {
      return selectedIds;
    },
    select: (ids) => (selectedIds = ids),
    detach: (c) => $state.snapshot(c) as DirectorClip
  });

  /**
   * The same editor state, addressed the way an H3 chat turn addresses it: by RENDER.
   *
   * 🔴 `generations` is a getter for the same reason `selectedIds` is one — `packedGenerations` is
   * `$derived`, and an `add_shot` re-packs the timeline halfway through its own action. A captured
   * array would name renders that no longer exist.
   */
  const h3ChatTarget = (): H3ChatTarget => ({
    ...chatTarget(),
    get selectedIds() {
      return selectedIds;
    },
    get generations() {
      return packedGenerations;
    },
    // The ceiling in force for THIS project — 345 frames rather than 362 while a song is attached.
    maxSeconds: unitMaxFrames / H3_FPS,
    setGenerationPrompt,
    updateGeneration,
    putGeneration,
    dropGeneration,
    warn: (m) => noteWarnings([m])
  });

  function undoChatEdit(edit: ChatEdit) {
    // ⚠️ By the space the edit was MADE in, never by the engine selected now: a conversation and its
    // revert cards survive an engine switch. `revertH3ChatEdit` falls through to the clip arm for
    // everything that is not generation-space, so this only has to route the H3 half.
    const reverted =
      edit.space === 'generation'
        ? revertH3ChatEdit(h3ChatTarget(), edit)
        : revertChatEdit(chatTarget(), edit);
    if (reverted) markDirty();
  }

  async function sendChat(text: string) {
    chatMessages = [...chatMessages, { role: 'user', content: text }];
    chatBusy = true;
    try {
      const refs = (story.references ?? [])
        .map((r) => r.image_b64)
        .filter((b): b is string => !!b?.trim())
        .slice(0, 6);
      const { reply, actions, warnings, target } = await directorChat({
        messages: chatHistory(chatMessages),
        shots: editorShots(clips),
        ...storyArg(),
        ...castArg(),
        ...engineArg(),
        ...h3RefsArg(),
        // 🔴 The same spread `/review` sends. Without it the server's H3 chat grounding has nothing
        // to ground on: it would be answering about a flat list of beat bodies while the prompt that
        // actually renders — the block, with its soundscape, its retention lines and its cut times —
        // was never in the conversation. Every brief carries an `id`, which is what moves the turn
        // onto the generation id space — see `h3ReviewGenerations`, and `target` below.
        ...(engine === 'h3' && packedGenerations.length
          ? { h3_generations: h3ReviewGenerations(packedGenerations) }
          : {}),
        ...(refs.length ? { reference_images: refs } : {})
      });
      noteWarnings(warnings);
      const valid = Array.isArray(actions) ? actions : [];
      // 🔴 **What the returned ids are ids OF, decided by the server and never guessed here.** The
      // action vocabulary is identical in both spaces, so nothing about an action's shape says
      // whether `shot` names a clip or a render.
      //
      // ⚠️ An unrecognised `target` is REFUSED, not applied. Resolving a generation id against the
      // clip list happens to find nothing today, so falling through would be a silent no-op rather
      // than a wrong write — but that is an accident of two id spaces not colliding, and the next
      // space the server adds is under no obligation to keep the accident.
      let edits: ChatEdit[] = [];
      if (valid.length) {
        if (target === 'generation') edits = applyH3ChatActions(h3ChatTarget(), valid);
        else if (target === undefined) edits = applyChatActions(chatTarget(), valid);
        else
          noteWarnings([
            `This editor does not understand "${target}" edits, so ${valid.length} change${valid.length === 1 ? '' : 's'} from that reply were not applied.`
          ]);
      }
      chatMessages = [
        ...chatMessages,
        {
          role: 'assistant',
          content: reply,
          ...(edits.length ? { edits } : {})
        }
      ];
      markDirty(); // persist the conversation itself, not just applied edits
    } catch (e) {
      chatMessages = [...chatMessages, { role: 'assistant', content: `⚠ ${(e as Error).message}` }];
      markDirty();
    } finally {
      chatBusy = false;
    }
  }

  function clearChat() {
    chatMessages = [];
    markDirty();
  }

  // Project-level "story bible" — canon the LLM grounds every shot in (auto-direct AND per-shot
  // draft). Persisted per-project in the timeline's editor blob; kept separate from generated
  // per-shot prompts so the user's direction is never overwritten.
  let story = $state<StoryContext>(emptyStory());

  // ── Cast (character sheets → stable identity tags) ───────────────────────────
  // Project-level, NOT per shot: LTX keys identity off `source_id`, so a character has to present
  // the same tag in every shot it appears in. See lib/director/characters.ts.
  let characters = $state<DirectorCharacter[]>([]);

  function addCharacter() {
    const added = newCharacter(characters);
    characters = [...characters, added];
    // A new cast member is enabled on every shot, matching the all-on default a new SHOT gets.
    // Shots still on the implicit default pick them up for free; explicit lists are extended.
    addCharacterToAllClips(clips, added.id);
    markDirty();
  }
  function removeCharacter(id: string) {
    characters = characters.filter((c) => c.id !== id);
    // A deleted cast member must not linger on shots. Its tag is NOT recycled — nextSourceId always
    // climbs — so a later character can never inherit this one's identity.
    for (const c of clips) pruneClipCharacters(c, characters);
    markDirty();
  }
  async function onCharacterSheet(id: string, files: FileList | null) {
    if (!files || !files[0]) return;
    const c = characters.find((x) => x.id === id);
    if (!c) return;
    // NOT downscaled, in either regime: a sheet is fed at native 1536×1024 and the per-panel face
    // detail is the whole payload, and an Echo still must keep the framing it was captured at.
    // (Story references ARE downscaled — they only steer the LLM.)
    //
    // In the MEMORY regime the upload also goes through preparation: matted, flattened on white and
    // letterboxed to the render aspect, and REFUSED outright if the subject is cut off at a frame
    // edge. That is not tidying — each of the three is a measured failure that a successful render
    // hides (a severed subject gets its missing part reinvented per shot; an unmatted photo leaks
    // its own background over the location; unsourced lighting travels with the subject). The sheet
    // regime is left alone: a 4-panel sheet is supposed to fill its frame, so edge contact is
    // correct there and matting it would destroy the layout.
    const file = files[0];
    const ref = await preparedReferenceRef(
      id,
      async () => file,
      () => putAsset(file)
    );
    if (ref === null) return;
    c.sheet_b64 = ref;
    // Uploaded, not promoted: clear any introduction shot so that shot stops being skipped.
    c.introduced_by = null;
    markDirty();
  }

  /** Character id whose reference is being prepared, so the row can say so. */
  let referencePreparing = $state<string | null>(null);
  /** Why the last reference upload was refused, or degraded. Cleared on the next attempt. */
  let referencePrepError = $state<string | null>(null);

  // ── H3 omni references ───────────────────────────────────────────────────────
  // The H3 counterpart of the cast above, and NOT a translation of it: an LTX character sheet is a
  // rotary-tagged latent identity the prompt cannot name, an H3 reference is text-encoder
  // conditioning it addresses by ordinal. A project carries both lists and renders whichever its
  // engine understands, so switching engine and back loses neither.

  /**
   * The OS file picker as a promise. Resolves to the chosen file, or `null` if the dialog was
   * dismissed.
   *
   * 🔴 `cancel` is listened for, not assumed away: without it a dismissed dialog leaves the promise
   * pending forever and the caller's `await` never returns — which would be a reference the author
   * can neither see nor undo, i.e. the bug this exists to prevent, in a new form.
   */
  function pickFile(kind: H3ReferenceKind): Promise<File | null> {
    return new Promise((resolve) => {
      const input = document.createElement('input');
      input.type = 'file';
      input.accept = kind === 'image' ? 'image/*' : kind === 'video' ? 'video/*' : 'audio/*';
      input.addEventListener('change', () => resolve(input.files?.[0] ?? null), { once: true });
      input.addEventListener('cancel', () => resolve(null), { once: true });
      input.click();
    });
  }

  /** Attach bytes to a reference that already exists. The shared half of both add paths. */
  async function attachToReference(
    id: string,
    slot: 'image' | 'audio' | 'video',
    file: File
  ): Promise<void> {
    const r = h3References.find((x) => x.id === id);
    if (!r) return;
    referencePrepError = null;
    const ref = await putAsset(file);
    // A video reference is stored WHOLE. The engine opens the container and does its own 2 fps
    // conditioner sampling and block timestamping, so the browser never needed to decode it.
    if (slot === 'video') r.video = ref;
    else if (slot === 'image') r.image = ref;
    else r.audio = ref;
    markDirty();
  }

  /**
   * 🔴 **The file is chosen FIRST, and a dismissed dialog stores nothing.**
   *
   * This used to create `{id, kind, role}` immediately and leave attaching the bytes to a second
   * gesture in another panel. What that produced was a reference with no payload, which the tile
   * renders as an empty plate, the generation card renders as a pill whose only live control is the
   * scope toggle, and `build_references` REFUSES if it is ever scoped in — a row the author could
   * neither fill from where they made it nor recognise as unfinished. One was found sitting in a
   * production document on 2026-08-09.
   *
   * Appended, never inserted: position assigns the ordinal, so a new reference has to be LAST or
   * every prompt naming a later one is renumbered. The role is DEFAULTED rather than left unset —
   * koblem derives the prompt's task-type prefix from it, and an unset role falls back to the
   * weakest claim for the kind, flattening every prompt to `[reference generation]`.
   */
  async function addReference(kind: H3ReferenceKind): Promise<void> {
    const file = await pickFile(kind);
    if (!file) return;
    const id = crypto.randomUUID();
    h3References = [...h3References, { id, kind, role: defaultRoleFor(kind) }];
    await attachToReference(id, kind, file);
  }

  function removeReference(id: string) {
    h3References = h3References.filter((r) => r.id !== id);
    markDirty();
  }

  /**
   * Add a reference that only ONE render unit uses — the answer to "extra references on this shot".
   *
   * 🔴 **There is no per-shot reference on the wire, and there cannot be one.** The conditioner
   * builds a single reference presentation per CALL (`pipeline.py:311-340`), and a call is a whole
   * generation — so a reference is a project-level asset and `reference_ids` on each generation says
   * which of them that render carries. That is not a UI limitation to route around; it is the
   * granularity the model has.
   *
   * What WAS a UI limitation is that confining a new reference to one shot cost a manual visit to
   * every other generation card. Adding to the project list lands the reference on every generation
   * whose scope is still ABSENT (absent = "follow the reference list" = everything), so the author
   * had to go and untick it n-1 times, and missing one is invisible: the reference conditions a
   * render it was never meant for and the only symptom is a shot that looks subtly like the wrong
   * character.
   *
   * So this does the n-1 writes itself, and does them BEFORE the reference exists in the list those
   * absent scopes follow — each other generation is pinned to exactly what it carries right now, so
   * the new one cannot inherit into it.
   *
   * Two generations are deliberately left alone:
   *
   * - one whose scope is ALREADY explicit — an explicit list never auto-includes anything, so it is
   *   already safe, and rewriting it would churn a document for no change;
   * - one that PINS A FRAME — it renders on `fl2va`, which takes no references at all, so its
   *   effective set is `[]`. Freezing that as an explicit empty list would outlive the pin: remove
   *   the pin later and the generation would render unconditioned, having silently been told "no
   *   references" by an action about a different shot.
   */
  async function addScopedReference(genId: string, kind: H3ReferenceKind): Promise<void> {
    // 🔴 FIRST, and nothing is stored if the dialog is dismissed — see `addReference`. This path is
    // where the empty reference found in production came from: its button's tooltip sent the author
    // to the References panel to attach the file, so the row existed, unfilled, in between.
    const file = await pickFile(kind);
    if (!file) return;
    const ref: DirectorH3Reference = {
      id: crypto.randomUUID(),
      kind,
      role: defaultRoleFor(kind)
    };
    // Snapshot first: `updateGeneration` writes `h3Generations`, which `packedGenerations` derives
    // from, so iterating the live derived list would be reading a value that moves underneath it.
    const gens = [...packedGenerations];
    for (const g of gens) {
      if (g.id === genId) continue;
      const stored = h3Generations.find((x) => x.id === g.id);
      if (stored?.reference_ids !== undefined) continue;
      if (g.shots.some((sh) => pinsAFrame(sh.clip))) continue;
      updateGeneration(g.id, {
        reference_ids: referencesForGeneration(h3References, g.shots, undefined).map((r) => r.id)
      });
    }
    const target = gens.find((g) => g.id === genId);
    if (target) {
      const stored = h3Generations.find((x) => x.id === genId);
      const effective = referencesForGeneration(
        h3References,
        target.shots,
        stored?.reference_ids
      ).map((r) => r.id);
      // Appended LAST, matching its position in the project list — `reference_ids` is read in
      // project order and position assigns the ordinal, so any other order is a different render.
      updateGeneration(genId, { reference_ids: [...effective, ref.id] });
    }
    // Last, so every read above saw the list WITHOUT it.
    h3References = [...h3References, ref];
    markDirty();
    // The row exists now, so the bytes can land on it. Attaching before this point would look the
    // reference up in a list it is not in yet and silently do nothing.
    await attachToReference(ref.id, kind, file);
  }

  /**
   * Move a reference one place, swapping with its neighbour.
   *
   * This is a RENDER change, not a display change: a reference's position assigns its
   * `<Picture i>` ordinal AND advances the model's shared rotary clock, so the same references in
   * a different order are a different request. Which is exactly why it is an explicit control
   * instead of drag-to-tidy — and why nothing anywhere sorts this list.
   */
  function moveReference(id: string, delta: -1 | 1) {
    const i = h3References.findIndex((r) => r.id === id);
    const j = i + delta;
    if (i < 0 || j < 0 || j >= h3References.length) return;
    const next = [...h3References];
    [next[i], next[j]] = [next[j], next[i]];
    h3References = next;
    markDirty();
  }

  /**
   * Attach a file to a reference.
   *
   * NOT put through the LTX reference-preparation pipeline (matte, flatten on white, letterbox,
   * refuse a cut-off subject). Every one of those steps exists because Echo reads a reference as
   * LATENT overlap at the render's own aspect, where a stray background leaks into the location
   * and a severed subject gets reinvented per shot. H3 encodes a reference at ITS OWN resolution
   * through a vision-language model and does not bind the canvas at all, so matting it would throw
   * away context the encoder can use and letterboxing would add borders it would then describe.
   */
  async function onReferenceFile(
    id: string,
    slot: 'image' | 'audio' | 'video',
    files: FileList | null
  ) {
    if (!files || !files[0]) return;
    await attachToReference(id, slot, files[0]);
  }

  /**
   * Run candidate reference bytes through the preparation pipeline and return the ref to store, or
   * `null` when the image is REFUSED and nothing should be stored.
   *
   * Shared by every path that can produce a reference (an uploaded sheet, a shot's own start
   * frame), because the three failures it catches are properties of the IMAGE, not of where the
   * image came from — and a second copy of this logic is how one of those paths quietly stops
   * being checked. `referencePreparing` / `referencePrepError` are the one pair of states the Cast
   * rows render, so every path reports the same way.
   *
   * Regime-gated exactly as the sheet upload is: only `memory` prepares. A 4-panel character sheet
   * is SUPPOSED to fill its frame, so edge contact is correct there and matting would destroy the
   * layout.
   *
   * `loadSource` is a thunk, not a Blob, so the sheet regime never pays to fetch bytes it is not
   * going to prepare — and so a failure fetching them lands in the same catch as a failure
   * preparing them, rather than escaping as an unhandled rejection.
   *
   * `fallback` supplies the unprepared ref for the degraded path — the pipeline itself failing
   * (503, network) is not a reason to refuse the image, only a 422 is.
   */
  async function preparedReferenceRef(
    id: string,
    loadSource: () => Promise<Blob>,
    fallback: () => Promise<string>
  ): Promise<string | null> {
    referencePrepError = null;
    if (identityRegime !== 'memory') return fallback();
    referencePreparing = id;
    try {
      const prepared = await prepareDirectorReference(
        await loadSource(),
        snap32(width),
        snap32(height)
      );
      return prepared.ref;
    } catch (e) {
      const status = (e as Error & { status?: number }).status;
      // 422 means the image cannot work as a reference; anything else means the pipeline itself
      // failed, and refusing the upload over that would be worse than taking it unprepared.
      if (status === 422) {
        referencePrepError = (e as Error).message;
        return null;
      }
      referencePrepError = `${(e as Error).message} — stored unprepared.`;
      return fallback();
    } finally {
      referencePreparing = null;
    }
  }

  /** Point a character at this shot's own i2v start frame — the no-render path to a reference. */
  async function useStartFrameAsReference(id: string) {
    const clip = selectedClip;
    const source = clip?.image_b64;
    if (!source) return;
    const c = characters.find((x) => x.id === id);
    if (!c) return;
    // NOT cropped: cropping a reference is not free — a 1.2× crop against an i2v opening frame
    // deformed the body, because both land on the same frame-0 grid at different scales.
    //
    // But it IS prepared, on the same terms an upload is, because an i2v scene image is a FULL
    // FRAME and that is close to the worst input the pipeline exists for. It carries its own
    // background (the measured location-hijack), and a subject in a full frame very often touches
    // an edge (the measured severed subject — the legless robot). Taking it verbatim skipped both
    // checks on the one path most likely to fail them.
    //
    // The fallback re-uses the EXISTING ref rather than re-uploading: the start frame is already
    // in the content-addressed store, so the degraded path costs nothing and stores nothing new.
    const ref = await preparedReferenceRef(
      id,
      () => fetchAssetBlob(source),
      async () => source
    );
    if (ref === null) return;
    c.sheet_b64 = ref;
    c.introduced_by = null;
    markDirty();
  }
  /** Enable/disable one character on the selected shot. The shared helper owns the three-state
   * encoding (absent = all, [] = none) so both Director UIs cannot disagree about it. */
  function toggleClipCast(clip: DirectorClip, id: string) {
    toggleClipCharacter(clip, characters, id);
    markDirty();
  }
  /** Put a shot back on the all-on default. */
  function resetSelectedClipCast() {
    if (!selectedClip) return;
    resetClipCast(selectedClip);
    markDirty();
  }

  // ── Timeline state ───────────────────────────────────────────────────────────
  let clips = $state<DirectorClip[]>([]);

  /**
   * H3 generations — the authored SECTIONS of each render unit.
   *
   * 🔴 Not a packing. The packing is re-derived below on every change, and this array only carries
   * what a human (or the writer) put into a generation's six sections. Storing the packing would
   * let it drift out of step with a timeline that has since been trimmed, and a stale generation
   * boundary is invisible until it renders as a seam.
   */
  let h3Generations = $state<DirectorH3Generation[]>([]);
  /**
   * Is there a song to condition on and deliver?
   *
   * `audioKind === 'none'` means the author attached a file and then told the project to ignore it,
   * so the bytes alone are not enough — checking only `audioB64` would put a soundtrack reference
   * into every generation of a project that had opted out of one.
   */
  const hasProjectSong = $derived(!!audioB64 && audioKind !== 'none');
  /**
   * The frame ceiling in force for THIS project — 345 rather than 362 while a song is attached, so
   * no render unit can be longer than the audio reference that has to score it.
   *
   * One derivation, so the chat target's own limit, the phone's budget bar and the packer cannot
   * disagree about how much room is left. Meaningless on an engine with no ceiling, which is why
   * every consumer reads it through `caps.maxFrames`.
   */
  const unitMaxFrames = $derived(h3MaxFrames(hasProjectSong));

  /**
   * Keep the project song's reference in `h3References` in step with whether a song is attached.
   *
   * 🔴 **A real, persisted entry — not a derived one**, and the deciding reason is SCOPE. The whole
   * per-shot override is "which shots is this reference in scope for", which lives in the
   * reference's own `segments`; a reference rebuilt from scratch on every read would throw that
   * away every time the author set it. Persisting it also means the panel can show it, the
   * composer numbers it, the save round-trips it and `<Audio N>` means the same thing everywhere —
   * all for free, because it is then simply a reference like any other.
   *
   * The effect only adds and removes. It never rewrites an existing entry, so the author's scoping
   * and role survive every re-run.
   */
  $effect(() => {
    const at = h3References.findIndex((r) => r.id === H3_SOUNDTRACK_REF_ID);
    if (hasProjectSong && at < 0) {
      h3References = withSoundtrackReference(h3References, true);
      markDirty();
    } else if (!hasProjectSong && at >= 0) {
      h3References = h3References.filter((r) => r.id !== H3_SOUNDTRACK_REF_ID);
      markDirty();
    }
  });

  /**
   * What the ENGINE accepts on one request, with the shipped defaults where props are unavailable.
   *
   * One object rather than four attributes at each call site: the References panel and the composer
   * both need them now, and two lists of four fallbacks are two lists that can disagree about what
   * the default is.
   */
  const h3RefCaps = $derived({
    maxImages: h3Props?.max_image_refs ?? 9,
    // 🔴 3, not 2. The 2 was WanGP's two named guide slots per kind; ComfyUI replaced WanGP
    // in production on 2026-08-12 and its reference inputs declare `max: 3`, which is also
    // MiniMax's documented figure. These fallbacks only apply when the engine is unreachable —
    // when it is up, `props` resolves them from the live capabilities document.
    maxVideos: h3Props?.max_video_refs ?? 3,
    maxAudios: h3Props?.max_audio_refs ?? 3,
    maxTotal: h3Props?.max_total_refs ?? 12
  });

  /**
   * The live packing: consecutive shots grouped into ≤15.083s render units.
   *
   * Derived, so it cannot be stale. Everything H3-shaped downstream — the timeline bands, the
   * composer, the compiler — reads this rather than re-deriving its own, because two packers that
   * agree today disagree the first time one of them learns a new rule.
   */
  const packedGenerations = $derived<PackedGeneration[]>(
    engine === 'h3'
      ? packGenerations({
          clips,
          references: h3References,
          stored: h3Generations,
          hasSoundtrack: hasProjectSong
        })
      : []
  );

  /**
   * The most keyframe pictures any single render will carry — the slots the roster cannot see.
   *
   * 🔴 The MAXIMUM over generations, not the total, and not zero. A `keyframe`-mode first/last frame
   * is synthesised into a real `<Picture N>` on the request it belongs to (`h3KeyframeReferences`),
   * so it spends one of the engine's per-request picture slots; but it lives in exactly one
   * generation, so summing across the timeline would refuse a project that is nowhere near the cap.
   * The worst single request is what the panel has to budget against, because it is what 400s.
   */
  const h3KeyframePictures = $derived(
    engine === 'h3'
      ? packedGenerations.reduce(
          (m, g) =>
            Math.max(m, h3KeyframeReferences(g.shots).filter((r) => r.kind === 'image').length),
          0
        )
      : 0
  );

  /**
   * What the author needs to know about the song-vs-timeline fit BEFORE spending a render.
   *
   * A preview only — koblem re-derives all of it authoritatively at submit, from the same rules.
   * It exists because the alternative is finding out after a fifteen-minute queue that the last
   * two generations were never scored.
   */
  /**
   * Where every generation lands on the delivered clock — the offsets the finished cut runs on.
   *
   * One derivation for the page, because it feeds three unrelated readers (the fit note, the
   * prompt's `mark` line, and the timeline's own geometry) and a second copy is how the editor and
   * koblem drifted apart the first time. Correct at `waveDuration` 0: a project with no song still
   * has boundaries, and the duration only decides which slices are skipped.
   */
  const songPlan = $derived(planSoundtrack(packedGenerations, waveDuration));

  /**
   * The cut's geometry on the DELIVERED clock — where each shot really lands once the frame grid
   * and the seam trims have had their say. The timeline draws on it; the page needs it wherever a
   * time crosses between the drawn axis and `clip.seconds`.
   */
  const timelineLayout = $derived(
    layoutClips(clips, packedGenerations, songPlan, ltxDeliveredSeconds(fps))
  );

  /**
   * How long the finished video is — not `sum(clip.seconds)`.
   *
   * The authored sum is what was typed; on H3 the render is longer by the round-up onto 17n+5 at
   * every generation, less a frame per anchored seam. Showing the typed number beside a timeline
   * drawn at the real one is the same two-clocks confusion the layout exists to end.
   */
  const totalDuration = $derived(timelineLayout.end);

  const soundtrackNote = $derived(
    engine === 'h3' && hasProjectSong && waveDuration > 0
      ? soundtrackFitNote(songPlan, waveDuration)
      : null
  );

  // ── The prompt's cursor, on the time axis ───────────────────────────────────

  /**
   * The cut time the author is pointing at in the prompt editor, on the PROMPT's own clock.
   *
   * `null` whenever the cursor is not on one, which is most of the time.
   */
  let promptHoverSeconds = $state<number | null>(null);

  /**
   * Write into a generation's stored sections, materialising the entry on first edit.
   *
   * The packer mints a stable id for a generation nobody has written for, so "materialise on first
   * edit" is safe: the id it invents is the id this looks up next time.
   */
  function updateGeneration(id: string, patch: Partial<DirectorH3Generation>) {
    const gen = packedGenerations.find((g) => g.id === id);
    const at = h3Generations.findIndex((g) => g.id === id);
    if (at >= 0) h3Generations[at] = { ...h3Generations[at], ...patch };
    else
      h3Generations = [
        ...h3Generations,
        { id, clip_ids: gen?.shots.map((s) => s.clip.id) ?? [], ...patch }
      ];
    markDirty();
  }

  /**
   * Forget a generation's stored sections entirely — used when the render itself goes away.
   *
   * 🔴 Returns what it removed, so an undo restores exactly the entry that was there rather than a
   * reconstruction. An entry left behind after its clips are deleted is an ORPHAN: `adoptStored`
   * matches stored entries to derived generations by clip-id overlap, and a stale entry whose ids
   * partially overlap somebody else's render can be adopted by it — a soundscape silently moving to
   * a different video.
   */
  function dropGeneration(id: string): DirectorH3Generation | undefined {
    const at = h3Generations.findIndex((g) => g.id === id);
    if (at < 0) return undefined;
    const [removed] = h3Generations.splice(at, 1);
    markDirty();
    return $state.snapshot(removed) as DirectorH3Generation;
  }

  /** Put a whole stored entry back, with its ORIGINAL `clip_ids` — `adoptStored` re-attaches it to
   *  whichever derived generation now holds those clips, so the id it lands under can differ. */
  /**
   * Bring the store into its canonical form — the documents this timeline's own packing produces.
   *
   * 🔴 Called at every door that installs a store from outside (a loaded project, a restore point),
   * AFTER the clips it belongs to are in place, because the packing is what decides the canonical
   * ids. `canonicalH3Store` is the same call the save makes, so what is held is what would be
   * written — see its doc for what drifting apart cost.
   *
   * ⚠️ Deliberately NOT `markDirty()`. Opening a project must not dirty it; the canonical form rides
   * out on the next save the author makes, and until then the in-memory store is already correct,
   * which is all the write path needs.
   */
  function canonicaliseGenerationStore() {
    if (engine !== 'h3') return;
    // Read the derived packing AFTER the assignments above — a `$derived` recomputes on read, so this
    // is the packing of the document that was just installed.
    h3Generations = canonicalH3Store(packedGenerations).map(
      (g) => $state.snapshot(g) as DirectorH3Generation
    );
  }

  function putGeneration(doc: DirectorH3Generation) {
    const at = h3Generations.findIndex((g) => g.id === doc.id);
    if (at >= 0) h3Generations[at] = doc;
    else h3Generations = [...h3Generations, doc];
    markDirty();
  }

  /**
   * Write a generation's BLOCK — and mirror its `[Shot N]` bodies onto the clips.
   *
   * 🔴 **The single funnel for everything the model is sent on H3**, and it is now only a store.
   *
   * It used to also MIRROR each `[Shot N]` body down onto a clip, because a beat was a clip and the
   * shot list, the gist and `clipRenders()` all read `clip.prompt`. That mirror is gone with the
   * beat-clips it copied into: one clip is one shot, the block is the only store of its prose and
   * its cut times, and everything that used to read the copy reads the block.
   */
  function setGenerationPrompt(id: string, block: string) {
    updateGeneration(id, { prompt: block });
  }

  /**
   * Dissolve the boundary before shot `i` — merge it into its predecessor so the two render as one
   * call.
   *
   * 🔴 It used to clear `h3_break_before` and let the packer re-group the two clips. There is no
   * re-grouping now: one clip is one render, so dissolving a boundary means merging the CLIPS.
   * `mergeClips` keeps the first member's id, which is what carries that shot's authored block
   * through the merge; the second's own block goes with it, which is why the control is gated by
   * `mergeBlockedReason`.
   */
  function mergeGeneration(i: number) {
    const a = packedGenerations[i - 1]?.shots[0]?.clip;
    const b = packedGenerations[i]?.shots[0]?.clip;
    if (!a || !b) return;
    applyClipOp(mergeClips(snapshotClips(), [a.id, b.id]));
  }

  /** Patch one clip from the composer, which owns the two shot fields that only mean anything
   *  relative to the cut (the cut verb, and whether dialogue runs across it). */
  function patchClip(clipId: string, patch: Partial<DirectorClip>) {
    const at = clips.findIndex((c) => c.id === clipId);
    if (at < 0) return;
    Object.assign(clips[at], patch);
    markDirty();
  }
  let selectedIds = $state<string[]>([]);
  // Plain clips for the pure transforms in lib/director. `$state.snapshot` shares the base64 assets
  // by reference (see the undo/redo note), so this is a walk of the object shells, not of the bytes.
  const snapshotClips = () => $state.snapshot(clips) as DirectorClip[];
  // Continue-guard: the current banked-eligible prompts, and whether they still match the prefix
  // that the last render banked (see lastRenderPrompts).
  const currentPrompts = $derived(clips.map((c) => c.prompt.trim()).filter(Boolean));
  const continueMisaligned = $derived(
    lastRenderPrompts.length === 0 ||
      lastRenderPrompts.length > currentPrompts.length ||
      lastRenderPrompts.some((p, i) => p !== currentPrompts[i])
  );
  let selectedPin = $state<{ clipId: string; index: number } | null>(null);
  let pxPerSec = $state(80);
  /**
   * Where the playhead is, in DELIVERED seconds — the song's clock, not the authored one.
   *
   * 🔴 The timeline draws on delivered time (`timeline-layout.ts`), so anything that reads this
   * back into the clip array has to convert. `splitAtPlayhead` is the one that does; adding a
   * second reader without `toAuthoredSeconds` puts the edit in the wrong shot on H3, silently, and
   * only once the cut is long enough for the drift to exceed a shot.
   */
  let playhead = $state(0);
  let playing = $state(false);
  /**
   * The project song as something an `<audio>` can load, so the timeline can play it against the
   * cut. `''` while the asset is still being fetched, which the transport reads as "not yet".
   */
  const audioSrc = $derived(audioB64 ? assetSrc(audioB64, 'audio/mpeg') : null);
  let snap = $state(true);

  // The inspector edits a single clip; when exactly one is selected it's that
  // clip, otherwise the most-recently-added-to-selection (primary).
  let primaryId = $derived(selectedIds[selectedIds.length - 1] ?? null);
  let selectedClip = $derived(
    selectedIds.length === 1 ? (clips.find((c) => c.id === primaryId) ?? null) : null
  );
  let selectedSet = $derived(new Set(selectedIds));
  /**
   * ⚠️ Declared here rather than with the other H3 derivations because it reads `selectedClip`:
   * a `$derived` above its source is a svelte-check error even though the runes graph resolves it
   * lazily at runtime. Same rule `audioB64` above already follows.
   *
   * Which {@link EngineVocab.renderUnit} the two tabs are showing: **the one holding the selected
   * beat**, always.
   *
   * 🔴 This was a separate `genTab` piece of state, and the badge and the body could point at
   * different generations. The badge computed "the generation containing `selectedClip`"; the body
   * rendered `packedGenerations[genTab]`, and `genTab` was written in exactly two places — the tab
   * click and the timeline's own "open generation" — never on a selection change. So clicking a beat
   * on the timeline moved the badge and left the panel showing somebody else's prompt.
   *
   * Deriving it from the selection removes the second source of truth entirely. It also has to be
   * clamped, because re-packing can remove the generation that was open.
   */
  const activeGenIndex = $derived.by(() => {
    const at = packedGenerations.findIndex((g) =>
      g.shots.some((sh) => sh.clip.id === selectedClip?.id)
    );
    return Math.min(Math.max(0, at), Math.max(0, packedGenerations.length - 1));
  });

  /**
   * Is the Prompts surface on screen — the desktop modal, or the phone's pushed sheet?
   *
   * One flag for both, because one derivation feeds both surfaces and because the seed cache has to
   * be dropped when the LAST of them closes rather than when either does.
   */
  const h3PromptsShown = $derived(promptsOpen || mobileSheet === 'prompts');
  $effect(() => {
    // Plain assignment to a plain Map: nothing reactive reads it, so this cannot re-enter.
    if (!h3PromptsShown) h3SeedCache = new Map();
  });

  /**
   * Every render unit's block, in timeline order — what the Prompts surface draws.
   *
   * 🔴 An unwritten unit is shown its SEED and is not stored. `H3PromptComposer` seeds on open and
   * accepts the dirty flag that costs, because opening one generation is a deliberate act; opening
   * this list would otherwise write a block into every unit in the project at once, on a glance.
   * The seed shown is `seedH3Prompt`, i.e. the same bytes `compileH3Timeline` falls back to, so what
   * is on screen is what would render either way — and the first edit is what stores it.
   *
   * ⚠️ Gated on {@link h3PromptsShown}. `seedH3Prompt` composes a whole block per generation, and
   * ungated this would re-run for the entire project on every keystroke in the composer.
   */
  const h3PromptDocs = $derived(
    h3PromptsShown
      ? packedGenerations.map((g, i) => ({
          id: g.id,
          no: i + 1,
          gist: h3GenerationGist(g),
          text: h3ShownPrompt(g),
          written: !needsH3PromptSeed(g.doc)
        }))
      : []
  );

  /** The stored block, or the cached seed for a unit that has none. See {@link h3SeedCache}. */
  function h3ShownPrompt(gen: PackedGeneration): string {
    if (typeof gen.doc.prompt === 'string') return gen.doc.prompt;
    const hit = h3SeedCache.get(gen.id);
    if (hit !== undefined) return hit;
    const seed = seedH3Prompt(gen);
    h3SeedCache.set(gen.id, seed);
    return seed;
  }

  /**
   * The cut time under the cursor in the prompt editor, in SONG seconds — what the timeline draws on.
   *
   * 🔴 The conversion is the whole reason this lives here rather than in the composer. A
   * `[Shot N] At MM:SS.mmm` mark is measured from its GENERATION's frame 0 — the model's clock
   * restarts at every render boundary — so the offset is the sum of every preceding generation's
   * DELIVERED length, less the frame each anchored seam eats. That arithmetic is `planSoundtrack`'s
   * and it is not re-implemented here: the plan is the same one the slices are cut with, so the
   * marker cannot drift from what actually renders.
   *
   * ⚠️ Below `activeGenIndex` for the reason stated on it — a `$derived` above its source is a
   * svelte-check error even where the runes graph resolves it lazily.
   *
   * The plan is computed with `waveDuration`, and every offset it reports is right even when that is
   * 0: a project with no song still has generation boundaries, and the duration only decides which
   * SLICES are skipped.
   */
  const promptMark = $derived.by(() => {
    if (promptHoverSeconds === null || !packedGenerations.length) return null;
    const slice = songPlan.slices[activeGenIndex];
    if (!slice) return null;
    return {
      seconds: slice.songOffsetSeconds + promptHoverSeconds,
      label: h3Timecode(promptHoverSeconds)
    };
  });

  /** The selected shot's cast, resolved through the project roster (so the tags shown are the ones
   * that will actually be sent). Empty for a shot with no character sheets. */
  let selectedClipCast = $derived(selectedClip ? clipCharacters(selectedClip, characters) : []);

  // ── Undo / redo (coalesced snapshots of the clip list + the engine) ──────────
  //
  // What one step holds, and what restoring it means, live in `lib/director/history.ts` — both are
  // correctness questions (see `Restored.dropBank`), and only the timers and the reactive writes
  // below are the component's.
  let histStack = $state<Snapshot[]>([]);
  let histIndex = $state(-1);
  let applyingHistory = false;
  let commitTimer: ReturnType<typeof setTimeout> | null = null;

  function snapNow(): Snapshot {
    return snapshotOf({ clips: $state.snapshot(clips) as DirectorClip[], engine });
  }
  /** Debounced: fold rapid edits (e.g. typing) into one undo step. */
  function scheduleCommit() {
    if (applyingHistory) return;
    if (commitTimer) clearTimeout(commitTimer);
    commitTimer = setTimeout(commitHistory, 450);
  }
  /** Push the current clip state as a new history entry (immediate). */
  function commitHistory() {
    if (commitTimer) {
      clearTimeout(commitTimer);
      commitTimer = null;
    }
    const next = pushSnapshot(histStack, histIndex, snapNow());
    histStack = next.stack;
    histIndex = next.index;
  }
  /** Reset the undo history to the current clip state (new/loaded project). */
  function resetHistory() {
    if (commitTimer) {
      clearTimeout(commitTimer);
      commitTimer = null;
    }
    histStack = [snapNow()];
    histIndex = 0;
  }
  function applySnapshot(snap: Snapshot) {
    applyingHistory = true;
    const back = restoreSnapshot(snap, engine);
    clips = back.clips;
    // The canvas is not in the snapshot, so a step that crosses an engine change lands the OTHER
    // engine's canvas on this one — the same reframe the picker takes, for the same reason, and
    // the reason it lives in `engine.ts` rather than in that control's handler.
    if (back.engine !== engine) {
      const home = reframeForEngine(back.engine, width, height);
      if (home) {
        width = home.w;
        height = home.h;
      }
    }
    engine = back.engine;
    // The step moved the engine, so the ids this editor holds that the step does NOT carry name the
    // engine it moved away from — the same drop the picker takes, for the same reason.
    if (back.dropBank) dropEngineBankState();
    if (!clips.some((c) => selectedSet.has(c.id))) {
      selectedIds = clips.length ? [clips[0].id] : [];
    }
    dirty = true;
    if (summary) summaryStale = true;
    scheduleAutosave();
    // release the guard after the reactive update settles
    setTimeout(() => (applyingHistory = false), 0);
  }
  function undo() {
    commitHistory(); // capture any pending edit first
    if (histIndex <= 0) return;
    histIndex -= 1;
    applySnapshot(histStack[histIndex]);
  }
  function redo() {
    if (histIndex >= histStack.length - 1) return;
    histIndex += 1;
    applySnapshot(histStack[histIndex]);
  }
  let canUndo = $derived(histIndex > 0);
  let canRedo = $derived(histIndex < histStack.length - 1);

  // ── Audio + lyrics ───────────────────────────────────────────────────────────
  let audioFile = $state<File | null>(null);
  /**
   * H3 only: what happens to the model's OWN generated audio once a song is attached.
   *
   * Defaults to `replace`, which is what a music video wants — H3's track is a reconstruction of the
   * same song (`fully_copy` is a prompt marker, not a copy instruction), so keeping it would ship
   * the cover version instead of the original. `keep` still conditions the render, so performers
   * still sing; it just delivers what the model rendered.
   */
  let h3SoundtrackMode = $state<'replace' | 'keep' | 'mix'>('replace');
  // 🔴 Default TRUE. The prompt writer's fallback (the guide's) is audience-only, which is the
  // wrong answer for every music video and silently breaks the singing sync.
  let h3SoundtrackDiegetic = $state(true);
  let waveform = $state<Float32Array | null>(null);
  let lyrics = $state<LyricWord[]>([]);
  let transcribing = $state(false);

  // ── Render ───────────────────────────────────────────────────────────────────
  /**
   * The LTX engine's bootstrap: its health, its defaults and its LoRA picker's contents.
   *
   * ⚠️ ONE variable. It was two — one per LTX service — while LTX-2.3 and 2.5 were both deployed,
   * because the LoRA list is a live scan of the answering engine's own adapter directory and the
   * two directories held different files, so a shared bootstrap offered names the other engine
   * rejects at submit. With 2.3 retired there is one directory and one answer. Fetched up front
   * (it costs no GPU: the supervisor answers while the worker is unloaded) so the picker can say
   * whether the engine is configured before it is chosen.
   */
  let props = $state<LtxVideoProps | null>(null);
  /**
   * Can this box resume a render and retake one shot — the capability AND the live bank.
   *
   * 🔴 Not `caps.resume`, and the difference is a render that fails after the queue wait. The
   * capability says the engine implements a bank; `props.bank_enabled` says this deployment gave it
   * a directory to keep one in, and LTX-2.5 answers 501 to both `resume_job_id` and `retake_from`
   * without one. Every resume tick-box, every retake button and every compiled `resume_job_id`
   * reads this, so the controls and the request cannot disagree about what the engine will accept.
   */
  const canResume = $derived(resumeAvailable(engine, props?.bank_enabled));
  /**
   * Can timed beats be authored here — the capability AND the mounted relay node.
   *
   * 🔴 Not `caps.beats`, and the difference is a render that fails after the queue wait. Same split
   * as `canResume`: the flag says LTX-2.5 implements Prompt Relay, `props.beats_enabled` says this
   * deployment's comfyui container has actually restarted since the node was mounted.
   */
  const canBeats = $derived(beatsAvailable(engine, props?.beats_enabled));
  /**
   * Can ONE render unit be re-rolled on its own here?
   *
   * Two unrelated routes to the same affordance, and every surface that offers it has to accept
   * both: a banked engine splices the unit back into the chain, and an engine whose retake IS the
   * whole render unit needs no bank at all. Written out at five call sites until 2026-08-14, which
   * is five chances for one shell to offer a re-roll another one hides.
   */
  const canRetakeUnit = $derived(canResume || caps.generationIsRetakeUnit);
  /**
   * What a retake does to the shots after it, as one sentence shared by both shells.
   *
   * The same fact `caps.retakeSplicesTail` gives the Joins panel — LTX-2.3 re-pins the tail, 2.5
   * and H3 leave it as it was — said where the retake is dispatched instead of only where the
   * damage is reported afterwards.
   */
  const retakeNote = $derived(retakeTailNote(caps));
  /**
   * A resumed or retaken render's `/media` is that WINDOW alone, not the project's cut.
   *
   * The delivery side of `retakeSplicesTail`: an engine that splices the banked tail back writes
   * the finished timeline, one that leaves the tail as it was writes only the shots it rendered.
   *
   * 🔴 Half the answer, and the earlier half. It is read off the CAPABILITY because it is needed at
   * DISPATCH, a poll before any job body exists — but a capability can only describe what the
   * engine does by design, so it says "not a fragment" for a full render whose concatenation blew
   * up. `watchJob` folds the job's own `job_is_partial` on top for that; see `mediaIsFragment`.
   */
  const partialRenderIsFragment = $derived(!caps.retakeSplicesTail);
  /**
   * The engine's latent bank as it is on disk right now, once somebody has asked.
   *
   * 🔴 A question about the ENGINE's disk, not a projection of what this project rendered. An entry
   * is written when a segment completes and dropped by a byte-bounded newest-first-keep sweep, so
   * "continue from last render" can be offered — the resume id is still in the document — while the
   * latents it names have already been swept. That resume then re-renders the whole prefix at full
   * cost and reports itself as a resume, which is the failure this readout exists to pre-empt.
   *
   * On demand, and only on demand: it is a live round trip to the engine and nothing on the load
   * path needs it.
   */
  let bank = $state<LtxBankListing | null>(null);
  let bankBusy = $state(false);
  let bankError = $state<string | null>(null);
  /**
   * Is there a swept bank STORE to list, as opposed to latents kept in a job dir?
   *
   * 🔴 Deliberately `bank_enabled` rather than `canResume`. The listing and the flag come out of the
   * same place — the plugin's `bank` block — so an engine that answers one has the other.
   *
   * 🔴 And gated on the engine as well, because `props` is the LTX bootstrap on an H3 project too.
   * Without the first clause an H3 project would draw the button off LTX-2.5's flag and then ask
   * `/ltx-video/bank` for a store H3 does not keep. `resumeNeedsLiveBank` is the property that means
   * "has a swept bank store, configured separately" and it is false everywhere else.
   */
  const bankListable = $derived(caps.resumeNeedsLiveBank && props?.bank_enabled === true);
  async function loadBank() {
    bankBusy = true;
    bankError = null;
    try {
      bank = await getLtxBank(engine);
    } catch (e) {
      bank = null;
      bankError = (e as Error).message;
    } finally {
      bankBusy = false;
    }
  }
  // 🔴 `running` no longer gates a submit — every render route enqueues, so a render in flight is
  // a reason to queue behind it, not a reason to refuse. It still drives the progress readout, the
  // partial strip and which project owns the poll.
  let running = $state(false);
  // The queue overrides the primary button carries, and the last 202 it got back. Shared by every
  // dispatch in this workspace: one author, one set of knobs, one statement of what happened.
  let queueOpts = $state<QueueSubmitOptions>({});
  let queueAccepted = $state<EnqueueLike | null>(null);
  const queueKind = $derived<JobKind>(engine === 'h3' ? 'h3' : 'ltx25');
  let elapsed = $state(0);
  // How many shots the in-flight render actually computes (a per-shot render is fewer than the
  // full cut), so the live ETA can scale to it instead of always showing the whole-timeline time.
  let renderShots = $state(0);
  let renderError = $state<string | null>(null);
  let resultUrl = $state<string | null>(null);
  // Progressive per-segment delivery: each finished shot previewed live (as an object URL) while
  // the rest of the chain still renders, before `resultUrl` lands. Cleared at the start of each
  // render. Best-effort — a shot the engine didn't emit (or that failed to fetch) just isn't shown.
  //
  // Each entry carries the identity of the clip it holds (producing render + shot), which is what
  // lets the post-render re-read of the durable rows keep these blobs instead of downloading the
  // whole render set a second time.
  let partialShots = $state<HeldClip[]>([]);
  let resultKey = $state<string | null>(null);
  // The project's persisted renders as ROWS — both tiers. Metadata is cheap and the far tier's
  // status has to stay truthful; only the tier this client renders in has its CLIPS downloaded.
  let renderRows = $state<DirectorRenderItem[]>([]);

  // ── Proxy render tier ────────────────────────────────────────────────────────
  // A second, independent render stream: tiny + few-steps but WITH audio + LoRAs, so you can eyeball
  // "is the prompt landing / is it reacting to the music / is the lipsync close" cheaply, shot by
  // shot, before committing to the full-res `final` render. The two tiers never share latents
  // (different resolution) — each is its own banked chain, and both persist per project.
  // Proxy render config (per-project, persisted in the editor doc). Small + few-steps by default;
  // 512×288 is exact 16:9. Bigger/more-steps = slower but a truer preview (e.g. to judge lipsync,
  // where tiny faces don't read). Kept independent of the full-render "Output size".
  let proxyW = $state(512);
  let proxyH = $state(288);
  let proxySteps = $state(8);
  let previewSizeSel = $derived(
    PREVIEW_TIERS.find((t) => t.w === proxyW && t.h === proxyH)?.key ?? 'custom'
  );
  function selectPreviewSize(key: string) {
    const t = PREVIEW_TIERS.find((x) => x.key === key);
    // `custom` is a readout of dims that match no tier, not a mode with anything to enter — so it
    // has nothing to apply, and dirtying the project for it would be a save with no change in it.
    if (!t) return;
    proxyW = t.w;
    proxyH = t.h;
    markDirty();
  }
  /**
   * The geometry a PREVIEW actually renders at — which is not `proxyW`×`proxyH` on every engine.
   *
   * 🔴 On MiniMax-H3 the preview tier is the SAME canvas at fewer steps. A smaller canvas there is
   * a different latent shape, and the initial noise is `randn(shape, manual_seed(seed))` — so a
   * different shape is a different DRAW, not a downsample: composition, camera path and subject
   * placement are a fresh roll at the same seed, and because the audio noise comes off the same
   * generator immediately afterwards the music bed differs too. A cheap preview that predicts none
   * of the render it precedes is worth less than no preview. Holding the canvas and dropping the
   * steps gives byte-identical noise and therefore the same framing, under-converged.
   *
   * The server enforces this (`apply_h3_preview_steps`); this is the readout agreeing with it, so
   * every "Preview · W×H" label states what will actually be rendered.
   */
  const previewW = $derived(caps.previewIsFullRes ? outW : snap32(proxyW));
  const previewH = $derived(caps.previewIsFullRes ? outH : snap32(proxyH));
  // Which tier the in-flight render belongs to — routes watchJob's output to the right tier's set.
  let activeRenderTier = $state<'proxy' | 'final'>('final');
  // Which PROJECT dispatched the in-flight render. The engine renders one chain at a time for the
  // whole app, so a render started in project A keeps streaming while you open project B — without
  // this, B's shots, resume ids and autosave would be overwritten by A's output.
  let renderProjectId = $state<string | null>(null);
  /**
   * Which ENGINE the in-flight render was dispatched to.
   *
   * 🔴 The project's engine is a picker, and it sits above a render that is still going. Every id a
   * completed job hands back — its banked dir, the v2v source that dir becomes — addresses the store
   * of the engine that RAN it, so a switch mid-render turns the completion into a write of foreign
   * ids onto a project that has moved. Recorded at dispatch and compared at completion
   * (`adoptCompletedBank`), because nothing on the job says it: koblem's job status carries a
   * `server_job_id` and no engine at all.
   *
   * A plain string, not `DirectorEngine`: the reload path reads it back out of `localStorage`, where
   * it is whatever is written there. `null` (adopted from an older session) matches nothing, which
   * is the honest answer for a render whose engine is unknown.
   */
  let renderEngine = $state<string | null>(null);
  // The in-flight render belongs to the project on screen: its partials, dots and ETA are ours.
  let renderIsMine = $derived(running && renderProjectId === projectId);
  // Something else is rendering. The engine is busy (so we can't start one) but nothing about it
  // describes this project.
  let renderElsewhere = $derived(running && renderProjectId !== projectId);
  let renderOwnerName = $derived(
    projects.find((p) => p.id === renderProjectId)?.name || 'another project'
  );
  // Proxy stream mirrors of resultUrl / partialShots (the stitched proxy cut + per-shot proxy clips).
  let proxyResultUrl = $state<string | null>(null);
  let proxyResultKey = $state<string | null>(null);
  let proxyShots = $state<HeldClip[]>([]);
  // Proxy banked lineage: the prompts the last proxy render banked (for prefix-align on resume) and
  // the koblem job to resume that banked prefix from. Session-only (the engine prefix can age out).
  let proxyLastPrompts = $state<string[]>([]);
  // The ENGINE server_job_id whose bank holds SEGMENT 0 of the proxy prefix — what `resume_job_id`
  // must be, because that is the segment the engine resolves the id against before anything else.
  //
  // 🔴 It stays the id we RESUMED FROM, never the fresh one a resume mints. LTX-2.3 tolerated
  // either (`resolve_ltx_bank_dir` follows an alias the new id writes back to the bank it
  // continued); LTX-2.5's bank is a flat `"<job>:<segment>"` store with no alias, so the fresh id
  // addresses only the shots that render banked and a second continue 400s on it. The shots those
  // later jobs DID bank are reached per-shot instead, server-side (`restore_selected_takes`).
  //
  // Persisted in the doc so proxy previewing stays incremental across a reload. `null` → the next
  // preview renders the whole prefix fresh.
  let proxyResumeJobId = $state<string | null>(null);
  // The resume target the in-flight proxy render is using (so completion keeps it stable rather than
  // adopting the new job's throwaway id). null when the render started a fresh proxy chain.
  let pendingProxyResumeTarget = $state<string | null>(null);

  // ── MiniMax-H3 render telemetry ──────────────────────────────────────────────
  //
  // 🔴 Everything here is `null`-when-unknown, never zero-when-unknown. An H3 job reports numeric
  // progress that the LTX stack has never had; an LTX job carries none of these keys at all and
  // every readout below stays absent for it, so the LTX path is untouched.
  /**
   * The last poll body of the render THIS project owns, kept only for its H3 telemetry.
   *
   * Cleared when a render finishes or when the in-flight one belongs to another project — a stale
   * "generation 3 of 4" under an idle stage is worse than no readout, because it looks live.
   */
  let jobTelemetryRaw = $state<LtxJobStatus | null>(null);
  /**
   * The author has asked for a render, so the H3 composer may stop being polite.
   *
   * 🔴 Until this flips, the composer withholds its AMBER notes about fields nobody has touched —
   * a fresh shot and an LLM draft are documents that are merely young, and meeting either with a
   * stack of warnings is the behaviour the 2026-08-08 retier removed. Hitting render is the moment
   * "you never wrote a soundscape" changes from nagging into the last chance to hear it, so it is
   * the moment everything gets said. Ghost placeholders and hard errors are NOT gated on this —
   * see `touched` in `H3PromptComposer.svelte` for why.
   *
   * One-way, and per session rather than per render: a warning the author has already been shown
   * and chosen to live with should not disappear again the moment the job finishes.
   */
  let revealH3Issues = $state(false);
  /**
   * Ask the engine for in-flight preview frames. Per project, persisted in the doc's editor bag.
   *
   * 🔴 OFF by default. `generate_preview` decodes latents through the RGB-factor path on every
   * emission and that cost is UNMEASURED against steps that already run 43–50 s at production
   * shape — so it must never silently tax a production render. `compile.ts` reads this and emits
   * `preview: true`; nothing else can turn it on.
   */
  let h3Preview = $state(false);
  /**
   * The LoRAs and step count every H3 render unit inherits unless its own card names its own.
   *
   * 🔴 A PROJECT DEFAULT, resolved in `compile.ts` — not written into the stored generations. The
   * wire has no project level (`contract.py: Generation.loras` / `.steps` are per generation), so
   * "the whole project uses the turbo LoRA at 8 steps" has to be filled in at compile time. Storing
   * it once here and expanding it there keeps ONE copy: change the number and every generation that
   * never overrode it follows, instead of drifting apart the moment one was edited.
   *
   * 🔴 Bare engine FILENAMES, not `LtxRuntimeLora` — H3 has no path, id or hash for an adapter, and
   * `loras` above is the LTX list. The two are not interchangeable in either direction.
   */
  let h3Loras = $state<string[]>([]);
  let h3LoraMultipliers = $state<number[]>([]);
  let h3Steps = $state<number | undefined>(undefined);
  /**
   * The preview frame currently on screen, as `{ seq, url }`.
   *
   * 🔴 The SEQUENCE is the signal, not the URL: `…/jobs/{id}/preview` never changes, so a plain
   * re-fetch loop would either hammer the endpoint or show a stale frame forever. We re-fetch only
   * when `preview_seq` on the job body moves, and hold the seq we last fetched so a poll that
   * repeats the same number costs nothing. `url` is an object URL this component owns and revokes.
   */
  let livePreview = $state<{ seq: number; url: string } | null>(null);
  // Guards against two overlapping fetches when a poll lands while the previous image is still on
  // the wire — the second would win the race arbitrarily and could install an OLDER frame.
  let previewFetching = false;

  let historyKey = $state(0);
  let restoreNote = $state('');
  // Set once the component is torn down. Not reactive: nothing renders it, and its only job is to
  // stop already-running async work from resurrecting timers and blobs after teardown.
  let destroyed = false;
  let progressTimer: ReturnType<typeof setInterval> | null = null;
  let watchTimer: ReturnType<typeof setTimeout> | null = null;
  let liveWatch: LiveWatch | null = null;
  // Every job id this tab has attached to. Not reactive: nothing renders it, and its only job is to
  // stop `adoptServerActive` re-attaching to a render it has already seen out. Bounded by the
  // renders one session dispatches or adopts.
  const watchedJobs = new Set<string>();

  // How often we ask the server what is rendering for the open project WHEN THE QUEUE STREAM IS
  // DOWN, and only then.
  //
  // 🔴 This used to be an unconditional 12 s interval, and it was the wrong shape for the question.
  // "Has a render started for my project" is a transition, the queue publishes transitions, and
  // asking on a timer instead meant the answer was no all night except for the handful of times it
  // was not — while every yes tore the stage down and rebuilt it, because attaching is destructive.
  // The stream is the normal path now; this is what covers the stream being gone.
  const ACTIVE_POLL_MS = 12000;
  // The harvest into director_shot_renders runs AFTER the job is marked terminal, so a client that
  // sees "completed" is racing it — but the server keeps the job in `/active` until it lands, so
  // this is a bounded wait on a real signal rather than a blind sleep. The cap matters more than the
  // interval: a harvest that never clears must not leave the editor waiting on it forever.
  const HARVEST_POLL_MS = 400;
  const HARVEST_WAIT_TRIES = 25;

  /**
   * How long a full render of this project took last time — the number every progress bar and ETA
   * on screen is scaled from.
   *
   * 🔴 It used to read `props.last_render_sec` unconditionally, and `props` is the **LTX** engine's
   * props. On an H3 project that meant the progress bar was pacing a MiniMax render against the
   * last LTX one — two engines whose per-shot costs differ by an order of magnitude — so the bar
   * either finished in the first minute and sat at 99%, or crawled while the render was already
   * done. H3's own figure comes back on its `/health` document, which `getH3Props` already carries.
   */
  let expectedSec = $derived.by(() => {
    if (engine !== 'h3') return props?.last_render_sec ?? 0;
    const health = h3Props?.health as Record<string, unknown> | null | undefined;
    const last = health?.last_render_sec;
    return typeof last === 'number' && last > 0 ? last : 0;
  });

  onMount(async () => {
    tierPref = parseTierPreference(localStorage.getItem(TIER_PREF_KEY));
    try {
      // Unconditional, like h3Props below and for the same reason: the picker must be able to warn
      // that the service is unset before the user selects it.
      props = await getLtxVideoProps('ltx25');
    } catch {
      /* props are best-effort — the picker just cannot pre-warn about an unset URL */
    }
    try {
      // Cheap and CUDA-free: answered by the engine's supervisor, so asking costs no GPU even when
      // the project is on LTX. Fetched unconditionally so the engine picker can say whether H3 is
      // configured before the user selects it.
      h3Props = await getH3Props();
    } catch {
      /* h3 props are best-effort — the picker just cannot pre-warn about an unset URL */
    }
    try {
      imgProps = await imageProps();
    } catch {
      /* image engine props are best-effort (storyboard degrades gracefully) */
    }
    await refreshProjects();
    // Reconnect to an in-flight render after a reload: `{jobId, tier, projectId}`. A record whose
    // projectId isn't the project we reopened still reconnects (to track engine busy-ness), but
    // watchJob keeps its output out of the editor.
    const active = localStorage.getItem(ACTIVE_JOB_KEY);
    if (active) {
      try {
        const rec = JSON.parse(active) as {
          jobId?: string;
          tier?: 'proxy' | 'final';
          projectId?: string | null;
          engine?: string | null;
        };
        if (rec.jobId) {
          running = true;
          activeRenderTier = rec.tier === 'proxy' ? 'proxy' : 'final';
          renderProjectId = rec.projectId ?? null;
          // Absent on a record written before this was tracked. It reads as "unknown engine", so
          // the completion banks nothing and the durable rows are the only source of the resume id
          // — which is where `bankedResumeTarget` reads it from anyway.
          renderEngine = rec.engine ?? null;
          // Nothing here says when the render began — no project is open yet, so the per-project
          // `/active` endpoint has nothing to be asked. The first poll carries `created_at`.
          rebaseElapsedOnPoll = true;
          startProgress();
          watchJob(rec.jobId, activeRenderTier);
        }
      } catch {
        localStorage.removeItem(ACTIVE_JOB_KEY);
      }
    }
    // The localStorage record above stays as the mount-time fallback: no project is open yet, so
    // the per-project server endpoint has nothing to be asked about — and that record is also the
    // only thing that reconnects a render belonging to ANOTHER project (engine busy-ness). From
    // here the server is the authority: loadProject asks it directly, and this poll re-asks while a
    // project sits open.
    startActivePoll();
    resetHistory();
    document.addEventListener('visibilitychange', flushOnHide);
    window.addEventListener('beforeunload', warnUnsaved);
  });

  // Ask the server what is rendering for `id` and attach to it. Only takes over when nothing is
  // being watched locally — a render already streaming into this tab is the same job or a foreign
  // one we must not interrupt, and two watchJob loops would fight over the shared timer.
  async function adoptServerActive(id: string) {
    if (destroyed || running) return;
    let items: DirectorActiveJob[];
    try {
      items = await listDirectorActiveJobs(id);
    } catch {
      return;
    }
    // Re-checked AFTER the await, `destroyed` included: onDestroy cannot cancel a call already in
    // flight, and adopting here would start a progress interval and a poll chain that nothing is
    // left to clear.
    if (destroyed || !items.length || projectId !== id || running) return;
    // A render that is over and merely persisting is not something to attach to — its output
    // arrives through the durable rows, and adopting it would show a render that is not happening.
    // Nor is one this tab has already watched to its end: attaching is DESTRUCTIVE (`clearPartials`
    // revokes the tier's clips and the finish re-downloads them), so a job the server keeps listing
    // after we have seen it finish would tear the stage down and rebuild it on every poll. That is
    // not hypothetical — a job cancelled at the queue left `ltx_jobs` saying `queued` forever, and
    // the Director reloaded its own video and stills every twelve seconds for a day.
    const live = items.find((j) => !j.harvesting && !watchedJobs.has(j.job_id));
    if (!live) return;
    adoptActiveJob(live);
  }

  // Attach to a render started elsewhere: its partials stream into this tab, and on completion the
  // normal finish path banks its resume id + re-reads the durable rows.
  function adoptActiveJob(job: DirectorActiveJob) {
    running = true;
    activeRenderTier = job.tier === 'proxy' ? 'proxy' : 'final';
    renderProjectId = job.project_id;
    // 🔴 UNKNOWN, and specifically not this tab's `engine`. This only ever attaches to a render
    // THIS tab did not dispatch, so the local value is not an account of what dispatched it — it is
    // an account of what this tab last read. Nothing re-reads the timeline while a project sits
    // open, so a client that has had the project open since before another device switched engine
    // and rendered holds a stale value that MATCHES its equally stale `engine`, and the completion
    // would then bank the other engine's chain id and save it.
    //
    // Withholding costs almost nothing, which is `adoptCompletedBank`'s own argument: the completion
    // runs `hydrateAfterHarvest`, and `hydratePersistedRenders` re-derives both resume ids
    // (`bankedResumeTarget`, engine-filtered) and the banked prompts from the durable rows moments
    // later, while the prior-render offer is rebuilt on the next project load.
    renderEngine = null;
    renderError = null;
    renderShots = job.segments;
    clearPartials(activeRenderTier);
    // 🔴 AFTER `clearPartials`, which zeroes it — and it is the server's answer standing in for a
    // dispatch-time prediction this tab never made. Both spellings arrive folded into one flag
    // (`delivers_fragment`), so an H3 generation retake adopted after a reload is known to be a
    // fragment before its first poll instead of one poll later.
    pendingPartialRender = mediaIsFragment(false, job.job_is_partial);
    // The render's own prompt list, so completion baselines the banked prefix against what is
    // ACTUALLY being rendered rather than against this tab's (empty) dispatch state.
    pendingRenderPrompts = [...job.scripts];
    pendingProxyResumeTarget = null;
    pendingFinalResumeTarget = null;
    startProgress(adoptedElapsedSec(job.created_at, Date.now()));
    rememberActiveJob(job.job_id, activeRenderTier);
    watchJob(job.job_id, activeRenderTier);
  }

  function startActivePoll() {
    liveWatch?.stop();
    liveWatch = watchLiveJobs({
      // The Director drives two engines and a render dispatched from the phone is the same render.
      kinds: ['ltx25', 'h3'],
      downMs: ACTIVE_POLL_MS,
      check: () => {
        if (!projectId || running) return;
        return adoptServerActive(projectId);
      }
    });
  }

  // Persist any pending edit when the tab is backgrounded — the common "closed the
  // laptop / switched away" path that a reload-based autosave would otherwise miss.
  function flushOnHide() {
    if (document.visibilityState === 'hidden') void autosaveNow();
  }

  // Last-ditch guard for the narrow window where a hard reload/close beats the
  // debounced autosave (async saves can't reliably finish during unload). With
  // autosave flushing within ~AUTOSAVE_MS this only ever fires mid-edit.
  function warnUnsaved(e: BeforeUnloadEvent) {
    if (dirty || renameTimer || staleDoc) {
      e.preventDefault();
      e.returnValue = '';
    }
  }

  onDestroy(() => {
    // Checked after every await by anything that re-arms a timer or mints an object URL: neither a
    // poll chain nor an in-flight fetch can be cancelled, so they have to notice on their own.
    destroyed = true;
    stopTimers();
    liveWatch?.stop();
    liveWatch = null;
    clearRenderPreviews();
    document.removeEventListener('visibilitychange', flushOnHide);
    window.removeEventListener('beforeunload', warnUnsaved);
    if (autosaveTimer) clearTimeout(autosaveTimer);
    if (autosaveRetryTimer) clearTimeout(autosaveRetryTimer);
    void commitRename();
    void autosaveNow();
  });

  function stopTimers() {
    if (progressTimer) clearInterval(progressTimer);
    if (watchTimer) clearTimeout(watchTimer);
    progressTimer = null;
    watchTimer = null;
  }

  // ── Clip ops ─────────────────────────────────────────────────────────────────
  function markDirty() {
    dirty = true;
    if (summary) summaryStale = true;
    scheduleCommit();
    scheduleAutosave();
  }

  /** Adopt the result of a clip-ops transform. `null` = the op declined; nothing changes. */
  function applyClipOp(out: ClipSelection | null): boolean {
    if (!out) return false;
    clips = out.clips;
    selectedIds = out.selectedIds;
    markDirty();
    return true;
  }

  function addClip() {
    applyClipOp(insertClip(snapshotClips(), primaryId, shotSeconds));
  }

  function deleteSelected() {
    if (selectedIds.length === 0) return;
    applyClipOp(deleteClips(snapshotClips(), selectedIds));
  }

  function duplicateSelected() {
    if (selectedIds.length === 0) return;
    applyClipOp(duplicateClips(snapshotClips(), selectedIds));
  }

  /** Merge the selected clips (must be contiguous) into one shot. */
  function mergeSelected() {
    if (selectedIds.length < 2) return;
    if (!applyClipOp(mergeClips(snapshotClips(), selectedIds))) {
      renderError = 'Select adjacent shots to merge.';
    }
  }

  function splitAtPlayhead() {
    // `playhead` is on the drawn (delivered) axis and `splitClipAt` walks `clip.seconds`, so the
    // two only agree while nothing has been snapped onto the frame grid. Convert, or a split aimed
    // at a beat late in the cut lands a shot or more early.
    applyClipOp(splitClipAt(snapshotClips(), toAuthoredSeconds(timelineLayout, playhead)));
  }

  /** Move the primary selected clip one slot left/right in the order. */
  function nudge(dir: -1 | 1) {
    if (!primaryId) return;
    const moved = moveShot(
      snapshotClips(),
      clips.findIndex((c) => c.id === primaryId),
      dir
    );
    if (!moved) return;
    clips = moved;
    markDirty();
  }

  function zoom(delta: number) {
    fitTimeline = false;
    windowSeconds = null;
    pxPerSec = Math.min(320, Math.max(20, pxPerSec + delta));
  }
  function setZoomWindow(v: string) {
    if (v === 'fit') {
      fitTimeline = true;
      windowSeconds = null;
    } else {
      windowSeconds = Number(v);
      fitTimeline = false;
    }
  }

  // ── Keyboard shortcuts ───────────────────────────────────────────────────────
  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && planReview) {
      planReview = null;
      return;
    }
    if (
      e.key === 'Escape' &&
      (settingsOpen || rendersOpen || reviewOpen || projectMenuOpen || audioOpen)
    ) {
      settingsOpen = rendersOpen = reviewOpen = projectMenuOpen = audioOpen = false;
      return;
    }
    if (e.key === 'Escape' && expandPanel) {
      expandPanel = null;
      return;
    }
    if (e.key === 'Escape' && promptsOpen) {
      closePrompts();
      return;
    }
    if (e.key === 'Escape' && viewerExpanded) {
      viewerExpanded = false;
      return;
    }
    const t = e.target as HTMLElement;
    const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable);
    const mod = e.metaKey || e.ctrlKey;
    // 🔴 **The `typing` guard has to come FIRST, and undo is the reason it exists.** These two
    // branches used to sit above it, so Ctrl+Z with the caret in a prompt box undid a TIMELINE edit
    // — a shot trim, a reorder, a delete from some minutes ago — while the sentence the author had
    // just typed stayed exactly where it was. Reported 2026-08-12 as "it does some global project
    // undo and I have no idea what gets undone", which is precisely the failure: the thing undone is
    // invisible, somewhere else on the page, and unrelated to what the hands were doing.
    //
    // Below the guard, an editing surface keeps its own history — CodeMirror's for the block editor,
    // the browser's for every plain field — and the timeline's undo still answers everywhere else.
    if (typing) return; // don't hijack keys while editing text
    if (mod && e.key.toLowerCase() === 'z') {
      e.preventDefault();
      e.shiftKey ? redo() : undo();
      return;
    }
    if (mod && e.key.toLowerCase() === 'y') {
      e.preventDefault();
      redo();
      return;
    }
    if (e.key === 'Delete' || e.key === 'Backspace') {
      e.preventDefault();
      deleteSelected();
    } else if (mod && e.key.toLowerCase() === 'd') {
      e.preventDefault();
      duplicateSelected();
    } else if (e.key === ' ') {
      // Transport, the way every editor spells it. Prevented because the browser's own default is
      // to scroll the page, which moves the timeline out from under the playhead it just started.
      //
      // ⚠️ Space ACTIVATES a focused control, so a real control under the caret keeps it —
      // otherwise the play button itself would toggle twice (once here, once on its own click) and
      // appear dead. Timeline blocks are `role="button"` DIVs and are deliberately NOT in this
      // list: they take focus on click, and swallowing space there would kill the transport right
      // after the gesture that most often precedes wanting to hear the shot. They answer to Enter.
      if (t?.closest('button, select, a, input, [role="checkbox"]')) return;
      e.preventDefault();
      if (audioSrc) playing = !playing;
    } else if (e.key.toLowerCase() === 's') {
      splitAtPlayhead();
    } else if (e.key.toLowerCase() === 'm') {
      mergeSelected();
    } else if (e.key === '+' || e.key === '=') {
      zoom(20);
    } else if (e.key === '-') {
      zoom(-20);
    } else if (e.key === '[') {
      nudge(-1);
    } else if (e.key === ']') {
      nudge(1);
    }
  }

  // ── Audio ────────────────────────────────────────────────────────────────────
  async function onAudioFiles(files: FileList | null) {
    if (!files || !files[0]) return;
    const file = files[0];
    audioFile = file;
    if (audioKind === 'none') audioKind = 'voice';
    try {
      const { samples, sampleRate } = await blobToMonoSamples(file);
      waveform = samples;
      waveDuration = samples.length / sampleRate;
    } catch {
      waveform = null;
      waveDuration = 0;
    }
    audioB64 = await putAsset(file);
    markDirty();
  }

  // Per-shot audio: the selected shot's own clip, which the engine composites over the project
  // track for exactly that shot's span. Stored base64 on the clip, so it persists with the project
  // and re-renders without a re-upload — same trade-off as the project track's own `audioB64`.
  async function pickShotAudio(e: Event) {
    const file = (e.currentTarget as HTMLInputElement).files?.[0];
    if (!file || !selectedClip) return;
    selectedClip.audio_b64 = await putAsset(file);
    selectedClip.audio_kind ??= 'voice';
    markDirty();
  }

  function clearShotAudio() {
    if (!selectedClip) return;
    selectedClip.audio_b64 = null;
    selectedClip.audio_kind = undefined;
    markDirty();
  }

  function clearAudio() {
    audioFile = null;
    audioB64 = null;
    waveform = null;
    waveDuration = 0;
    lyrics = [];
    audioKind = 'none';
    markDirty();
  }

  async function transcribe() {
    if (!audioFile) return;
    transcribing = true;
    try {
      const res = await transcribeDirectorAudio(audioFile, {
        granularity: 'word'
      });
      lyrics = res.words?.length ? res.words : (res.segments ?? []);
      markDirty();
    } catch (e) {
      renderError = `transcribe failed: ${(e as Error).message}`;
    } finally {
      transcribing = false;
    }
  }

  // ── Compile → render ─────────────────────────────────────────────────────────
  /** Compile the timeline into the shared LtxVideoGenerateRequest + per-shot asset refs. */
  const compile = (): CompiledTimeline =>
    compileTimeline({
      engine,
      clips: snapshotClips(),
      characters: $state.snapshot(characters) as DirectorCharacter[],
      h3References: $state.snapshot(h3References) as DirectorH3Reference[],
      h3Generations: $state.snapshot(h3Generations) as DirectorH3Generation[],
      // Emits `preview: true` on the H3 request and nothing at all on LTX. Off unless asked for —
      // the per-emission decode cost is unmeasured (see the state declaration).
      h3Preview,
      // Project DEFAULTS: `compileH3Timeline` writes them into any generation that named none of
      // its own. Ignored entirely on LTX, which has its own `loras` below and its own `steps`.
      h3Loras: $state.snapshot(h3Loras) as string[],
      h3LoraMultipliers: $state.snapshot(h3LoraMultipliers) as number[],
      h3Steps,
      model,
      loras: $state.snapshot(loras) as LtxRuntimeLora[],
      width,
      height,
      fps,
      shotSeconds,
      upscalerRatio,
      qualityProfile,
      audioKind,
      audioB64,
      h3SoundtrackMode,
      steps,
      cfgScale,
      negativePrompt,
      seed,
      frameDrop,
      detailStrength,
      customBaseSampler,
      customBaseSigmasText,
      customStages: $state.snapshot(customStages) as CustomStage[],
      // Continue: reuse the prior render's banked shots and render only the newly-added tail. Only
      // when the timeline still starts with the banked prefix — otherwise a full fresh render.
      //
      // 🔴 `canResume` as well as the tick-box, because the tick-box is STATE and the engine can
      // change under it. Hiding the control on an engine with no reachable bank (H3, or LTX-2.5 on
      // a deployment with no bank directory) leaves a `true` set on LTX-2.3 still set — and a
      // `resume_job_id` the engine 501s is a render the user asked for that never starts.
      resumeJobId: continueFromLast && !continueMisaligned && canResume ? finalResumeJobId : null
    });

  /**
   * `compile()` plus the per-shot bytes the multipart render path needs, fetched from the asset
   * store. `compile()` itself stays synchronous and byte-free — it runs on every save to build the
   * timeline doc, which must keep the references rather than the payloads.
   */
  async function compileForRender() {
    const { request, segImageRefs, segV2vRefs } = compile();
    const toFile = async (ref: string | null, name: string) =>
      ref ? new File([await fetchAssetBlob(ref)], name) : null;
    const [segImageFiles, segV2vFiles] = await Promise.all([
      Promise.all(segImageRefs.map((r) => toFile(r, 'seg.png'))),
      Promise.all(segV2vRefs.map((r) => toFile(r, 'seg_v2v.webm')))
    ]);
    // The ONE place the engine's own limits are imposed on a request. `buildTimelineDoc` shares
    // `compile()` and must not — see `compileForDelivery`.
    return { request: compileForDelivery(engine, request), segImageFiles, segV2vFiles };
  }

  /** The full persisted doc (editor state + compiled render spec + inline assets). */
  function buildTimelineDoc(): DirectorTimeline {
    const { request } = compile();
    return {
      // v2: width/height are the OUTPUT target (v1 stored the engine base pass). adoptTimeline
      // migrates v1 docs by scaling their base up by the saved upscaler ratio.
      version: 2,
      fps,
      // The LTX field. On H3 there is no per-shot frame count at the document level — a shot is
      // not a render there — so this falls back to the authored shot length, which is what a
      // reader that only understands the LTX shape would have inferred anyway.
      segment_frames: ltxRequestOf(request)?.segment_frames ?? framesFromSeconds(shotSeconds, fps),
      width,
      height,
      engine,
      // Top-level, like `characters` and for the same reason: order is part of the request, so
      // this is render state and not editor state.
      h3_references: $state.snapshot(h3References) as DirectorH3Reference[],
      // The authored sections, not the packing — see `h3-generations.ts`. Pruned to the
      // generations that still exist, so a doc cannot accumulate orphaned sections for shots that
      // were deleted three edits ago.
      //
      // 🔴 `canonicalH3Store` rather than an inline map, and the SAME call the load path makes: what
      // is written and what is held have to be the same shape, or the ids in memory drift from the
      // ids the packer mints and a write to a drifted generation lands in a second entry nothing
      // ever adopts. That drift is what produced two generations under one id.
      h3_generations: canonicalH3Store(packedGenerations).map(
        (g) => $state.snapshot(g) as DirectorH3Generation
      ),
      model,
      audio_kind: audioKind,
      h3_soundtrack_mode: h3SoundtrackMode,
      h3_soundtrack_diegetic: h3SoundtrackDiegetic,
      clips: $state.snapshot(clips) as DirectorClip[],
      // Top-level (not in `editor`): the cast owns the identity tags the compiled request already
      // references, so it is render state, not UI state. A doc that lost it would still carry
      // `source_id`s in `request.segments[].character_refs` with nothing to explain them.
      characters: $state.snapshot(characters) as DirectorCharacter[],
      beats_enabled: beatsEnabled,
      frame_drop: frameDrop,
      critique: {
        enabled: critiqueEnabled,
        auto_retake: critiqueAutoRetake,
        max_retakes: critiqueMaxRetakes,
        threshold: critiqueThreshold
      },
      lyrics: $state.snapshot(lyrics) as LyricWord[],
      request,
      assets: {
        seg_images_b64: clips.map((c) => (c.mode === 'i2v' ? (c.image_b64 ?? null) : null)),
        // Uploaded v2v source clips inline (for the /run path); prior_render sources need none.
        // Same resolver as `compileTimeline`'s `segV2vRefs`, so the persisted document cannot carry
        // bytes for a shot the compiled request has no source for.
        seg_v2v_b64: clips.map((c) =>
          v2vSourceOf(c, caps.v2vPriorRenderSource)?.kind === 'upload'
            ? (c.v2v_source_b64 ?? null)
            : null
        ),
        audio_b64: (audioKind === 'voice' || audioKind === 'mixed') && audioB64 ? audioB64 : null,
        bgm_b64: audioKind === 'bgm' && audioB64 ? audioB64 : null
      },
      editor: {
        pxPerSec,
        shotSeconds,
        resPreset,
        proxyW,
        proxyH,
        proxySteps,
        // Editor state, not render state: it changes what the engine EMITS during a render, not
        // what it renders, so it rides the editor bag beside the other per-project preferences.
        h3Preview,
        // The project's H3 LoRA/step defaults. They ride the editor bag rather than the compiled
        // request because the H3 request has no project level to round-trip through — see the state
        // declaration.
        h3Loras: $state.snapshot(h3Loras) as string[],
        h3LoraMultipliers: $state.snapshot(h3LoraMultipliers) as number[],
        h3Steps,
        proxyResumeJobId,
        finalResumeJobId,
        story: $state.snapshot(story) as StoryContext,
        autoBrief,
        summary,
        summaryStale,
        chatMessages: $state.snapshot(chatMessages) as ChatMsg[]
      }
    };
  }

  // Adopt a full DirectorTimeline as the working state (auto-direct plan or a loaded project).
  // Resolution ignores the timeline's raw width/height when it's the un-upscaled 720p base — a
  // reskinned LTX-2.3 timeline always renders through the staged ×2 path (960×544 → 1920×1088).
  async function adoptTimeline(t: DirectorTimeline) {
    const d = readTimelineDoc(t);
    fps = d.fps;
    shotSeconds = d.shotSeconds;
    upscalerRatio = d.upscalerRatio;
    resPreset = d.resPreset;
    width = d.width;
    height = d.height;
    proxyW = d.proxyW;
    proxyH = d.proxyH;
    proxySteps = d.proxySteps;
    h3Preview = d.h3Preview;
    h3Loras = d.h3Loras;
    h3LoraMultipliers = d.h3LoraMultipliers;
    h3Steps = d.h3Steps;
    proxyResumeJobId = d.proxyResumeJobId;
    finalResumeJobId = d.finalResumeJobId;
    engine = d.engine;
    h3References = d.h3References;
    // 🔴 Two entries under one id are repaired ON THE WAY IN, before anything can key on them —
    // `updateGeneration` resolves an id by `findIndex`, so until this runs one shot's edits are
    // written into another shot's document. Found in a real 16-shot project; see
    // `dedupeH3GenerationIds` for how a store gets there. A no-op (and not dirtying) for a healthy
    // one; the repair rides out on the next save the author makes.
    h3Generations = dedupeH3GenerationIds(d.h3Generations) ?? d.h3Generations;
    model = d.model;
    loras = d.loras;
    steps = d.steps;
    cfgScale = d.cfgScale;
    negativePrompt = d.negativePrompt;
    seed = d.seed;
    detailStrength = d.detailStrength;
    qualityProfile = d.qualityProfile;
    if (d.customBaseSampler !== undefined) customBaseSampler = d.customBaseSampler;
    if (d.customBaseSigmasText !== undefined) customBaseSigmasText = d.customBaseSigmasText;
    if (d.customStages !== undefined) customStages = d.customStages;
    // Ensure the (migrated / preset) target divides cleanly for the adopted profile. Must run after
    // the profile and its stages land, since both feed the divisor.
    snapTargetToProfile();
    // A loaded project starts fresh — continue is opt-in per render session.
    continueFromLast = false;
    audioKind = d.audioKind;
    // 🔴 SET HERE, in the same synchronous run as `h3References` above, and that ordering IS the
    // fix. `hasProjectSong` is `audioB64 && audioKind !== 'none'`, and the effect that keeps the
    // soundtrack reference in step with it REMOVES that reference — and `markDirty()`s — whenever
    // it sees a reference with no song. Setting `audioB64` later meant every project load spent
    // two awaits (`hydrateAfterHarvest`, `adoptServerActive`) in a state where the document said
    // "song attached" and the editor said "no song": the effect stripped the reference, autosave
    // wrote the absence, and the save path nulled `assets.audio_b64` with it. Merely OPENING a
    // project deleted its song, silently, and it took three occurrences to catch because a render
    // with no audio reference fails like a bad model rather than like a bug.
    //
    // ⚠️ Guarded on the doc CARRYING an assets block, not on the value being truthy. The
    // auto-direct path adopts a planner timeline that has no `assets` at all, and an unguarded
    // assignment would delete the song there — the same way the planner once deleted the cast and
    // then the reference list (see the two comments at its call site). A project with genuinely no
    // song still has the block, with a null inside it, and correctly clears.
    const adopted = t.assets as DirectorAssets | undefined;
    if (adopted) audioB64 = adopted.audio_b64 ?? adopted.bgm_b64 ?? null;
    h3SoundtrackMode = d.h3SoundtrackMode;
    h3SoundtrackDiegetic = d.h3SoundtrackDiegetic;
    // 🔴 One clip per shot, on the way in. H3 used to store every BEAT as its own clip and call the
    // pack of them a render, which duplicated each beat's text and its cut time — the two copies
    // that produced "the timeline has not caught up with the prompt". The block is the only store
    // of both now, so a project written under the old shape is folded as it opens. No-op (and so
    // not dirtying) for anything already collapsed; see `collapseH3Beats` for what it preserves.
    clips =
      (d.engine === 'h3' &&
        collapseH3Beats({
          clips: d.clips,
          references: d.h3References,
          // The repaired list from above, so the collapse and the packer agree on which entry is which.
          stored: h3Generations,
          hasSoundtrack: !!(adopted?.audio_b64 ?? adopted?.bgm_b64)
        })) ||
      d.clips;
    characters = d.characters;
    lyrics = d.lyrics;
    beatsEnabled = d.beatsEnabled;
    frameDrop = d.frameDrop;
    critiqueEnabled = d.critique.enabled;
    critiqueAutoRetake = d.critique.autoRetake;
    critiqueMaxRetakes = d.critique.maxRetakes;
    critiqueThreshold = d.critique.threshold;
    pxPerSec = d.pxPerSec;
    windowSeconds = null;
    fitTimeline = false;
    story = d.story;
    if (d.autoBrief !== undefined) autoBrief = d.autoBrief;
    summary = d.summary;
    summaryStale = d.summaryStale;
    chatMessages = d.chatMessages;
    selectedIds = clips[0] ? [clips[0].id] : [];
    // 🔴 LAST, after the clips and the references are in place: the canonical id of a generation is
    // minted from the clip it opens on, so the packing has to be the one this document produces. See
    // `canonicalH3Store` — what is held now is byte-identical to what the next save writes.
    canonicaliseGenerationStore();
  }

  // Repopulate the director timeline from a saved "generate" render's request_meta; the per-shot
  // prompts live under `scripts` — a dub/relip row has none, so it's not restorable here.
  function restore(g: Generation) {
    const m = g.request_meta;
    const scripts = arr(m, 'scripts');
    if (scripts === undefined) {
      restoreNote = 'This entry was a dub/relip render — nothing to restore here.';
      return;
    }
    const md = str(m, 'model');
    if (md !== undefined) model = md;
    const pr = str(m, 'profile');
    if (pr !== undefined) qualityProfile = pr as LtxQualityProfile;
    const np = str(m, 'negative_prompt');
    if (np !== undefined) negativePrompt = np;
    const ur = str(m, 'upscaler_ratio');
    if (ur !== undefined) upscalerRatio = ur as LtxUpscalerRatio;
    // Legacy renders stored width/height as the engine BASE pass and carry no `dims_are_target`
    // flag; migrate them to the OUTPUT target by scaling up by the upscaler ratio (the output the
    // old single-upscale pipeline produced). Target-native renders (flag set) restore verbatim.
    const migM = bool(m, 'dims_are_target') ? 1 : RATIO_MULT[upscalerRatio];
    const w = num(m, 'width');
    if (w !== undefined) width = snap32(w * migM);
    const h = num(m, 'height');
    if (h !== undefined) height = snap32(h * migM);
    const f = num(m, 'fps');
    if (f !== undefined) fps = f;
    const sf = num(m, 'segment_frames');
    if (sf !== undefined) shotSeconds = Math.max(1, Math.round(sf / (fps || 24)));
    const st = num(m, 'steps');
    if (st !== undefined) steps = st;
    const cf = num(m, 'cfg_scale');
    if (cf !== undefined) cfgScale = cf;
    const sd = num(m, 'seed');
    if (sd !== undefined) seed = sd;
    const ds = num(m, 'upscale_detail_strength');
    detailStrength = ds ?? DETAIL_STRENGTH_DEFAULT;
    if (w !== undefined || h !== undefined || ur !== undefined) resPreset = 'custom';
    snapTargetToProfile(); // keep the (migrated) target divisible for the restored profile

    clips = scripts.map((s): DirectorClip => ({
      id: uid(),
      prompt: typeof s === 'string' ? s : '',
      mode: 't2v',
      seconds: shotSeconds,
      keyframes: []
    }));
    selectedIds = clips[0] ? [clips[0].id] : [];
    continueFromLast = false;
    lastRenderPrompts = [];
    resetHistory();
    summary = null;
    summaryStale = false;
    clearReview();
    dirty = true;
    scheduleAutosave();
    renderError = null;
    restoreNote = reattachNote(m);
    window.scrollTo({ top: 0, behavior: 'smooth' });
  }

  async function autoDirect() {
    if (!autoBrief.trim() && !audioB64) {
      renderError = 'Add a brief (or a song) to auto-direct.';
      return;
    }
    captureRestore('Auto-direct');
    autoDirecting = true;
    renderError = null;
    try {
      const body: DirectorPlanRequest = {
        brief: autoBrief.trim(),
        max_shots: autoMaxShots,
        // Off ⇒ the planner is told to split a two-moment shot into two shots instead of
        // beating it. Only sent when off, so an untouched project plans exactly as before.
        //
        // 🔴 `canBeats`, not the tick-box alone. With the relay node unmounted a planned beat is a
        // shot that 501s at graph build, and the planner splitting the moment into two shots is
        // exactly the right fallback — so the deployment fact has to reach the PLANNER too, not
        // only the editor that hides the array.
        ...(canBeats && beatsEnabled ? {} : { beats: false }),
        fps,
        width,
        height,
        model,
        // Carried across for the same reason `characters` is re-attached in commitPlan:
        // adoptTimeline rebuilds state from the returned doc, so anything not sent comes back
        // empty and auto-directing would quietly clear the project's LoRA set.
        ...(loras.length ? { loras: $state.snapshot(loras) as LtxRuntimeLora[] } : {}),
        audio_kind: audioKind === 'none' && audioB64 ? 'voice' : audioKind,
        ...(audioB64 ? { audio_b64: audioB64 } : {}),
        ...(lyrics.length ? { lyrics: $state.snapshot(lyrics) as LyricWord[] } : {}),
        ...(storyIsEmpty(story) ? {} : { story: $state.snapshot(story) as StoryContext }),
        ...castArg(),
        ...engineArg(),
        // Project-wide, unscoped: the planner is deciding which shots a reference belongs to, so
        // narrowing the list to a shot's scope would be circular.
        ...h3RefsArg()
      };
      const res = await planDirectorTimeline(body);
      // Stage 1 stops here: the plan is a proposal, not an edit. Nothing touches the timeline
      // until the owner reviews the beats and hits Go.
      planReview = { timeline: res.timeline, summary: res.summary ?? null };
    } catch (e) {
      renderError = `auto-direct failed: ${(e as Error).message}`;
    } finally {
      autoDirecting = false;
    }
  }

  // Stage 1 → 2. Commit the reviewed beats (ideas only — prompts are still empty), then hand every
  // shot to the same per-shot Draft path the ✨ button uses.
  async function commitPlan(beats: DirectorClip[]) {
    const plan = planReview;
    if (!plan) return;
    planReview = null;
    projectId = null;
    projectName = autoBrief.trim().slice(0, 60) || 'Auto-directed plan';
    // The plan proposes SHOTS, never a cast — and adoptTimeline reads the roster out of the doc,
    // so carry the current one across or auto-directing silently deletes every character.
    await adoptTimeline({
      ...plan.timeline,
      clips: beats,
      characters: $state.snapshot(characters) as DirectorCharacter[],
      // Same reason as `characters`, one step further: the planner is sent reference BRIEFS (kind +
      // label) and never the pixels, so it cannot put a roster back in the document it returns.
      // Without this line, auto-directing an H3 project would delete every reference it has —
      // including the ordinals the freshly-planned shots were written against.
      h3_references: $state.snapshot(h3References) as DirectorH3Reference[],
      // The plan proposes a whole new shot list, so the sections written for the OLD packing
      // describe generations that no longer exist. Dropped deliberately rather than re-attached by
      // clip-id overlap — there is no overlap to find, and carrying them would leave the first new
      // generation wearing somebody else's soundscape.
      h3_generations: plan.timeline.h3_generations ?? []
    });
    // adoptTimeline reads the summary out of the doc's editor blob (absent on a planned timeline),
    // so re-apply the plan's synopsis after it lands.
    summary = plan.summary;
    summaryStale = false;
    resetHistory();
    dirty = true;
    scheduleAutosave();
    void autoTitle(true); // upgrade the crude brief-slice name to a real title, best-effort
    void draftAllShots(undefined, true);
  }

  const DRAFT_TRIES = 3;

  // Stage 2: fill every idea-only shot with a real prompt via the per-shot Draft path. Sequential
  // and in timeline order on purpose — `shotContext` feeds each shot the PREVIOUS shot's prompt, so
  // drafting in order means that context is already written. Never blocks the editor.
  async function draftAllShots(only?: string[], keepSummary = false) {
    if (draftAllRunning) return;
    const targets = clips.filter(
      (c) => (only ? only.includes(c.id) : !c.prompt.trim()) && c.idea?.trim()
    );
    if (targets.length === 0) {
      if (!only) renderError = 'No shots with an idea left to draft.';
      return;
    }
    captureRestore('Draft all shots');
    draftAllRunning = true;
    draftAllTotal = targets.length;
    draftAllDone = 0;
    draftFailedIds = draftFailedIds.filter((id) => !targets.some((t) => t.id === id));
    try {
      for (const target of targets) {
        // Re-resolve each pass: the editor is live, so the shot may have been deleted meanwhile.
        const clip = clips.find((c) => c.id === target.id);
        if (!clip) {
          draftAllTotal--;
          continue;
        }
        const shotNo = clips.findIndex((c) => c.id === clip.id) + 1;
        draftAllLabel = `shot ${String(shotNo).padStart(2, '0')}`;
        let ok = false;
        for (let attempt = 1; attempt <= DRAFT_TRIES && !ok; attempt++) {
          try {
            await draftClipCore(clip, keepSummary);
            ok = true;
          } catch (e) {
            if (attempt === DRAFT_TRIES) {
              // One bad shot never kills the batch — flag it and move on; it stays retryable.
              draftFailedIds = [...draftFailedIds, clip.id];
              renderError = `shot ${shotNo} draft failed: ${(e as Error).message}`;
            } else {
              await sleep(600 * attempt);
            }
          }
        }
        draftAllDone++;
      }
    } finally {
      draftAllRunning = false;
      draftAllLabel = '';
    }
  }

  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

  const retryFailedDrafts = () => draftAllShots([...draftFailedIds]);

  const storyArg = () =>
    storyIsEmpty(story) ? {} : { story: $state.snapshot(story) as StoryContext };
  /**
   * The cast, for every LLM path that WRITES or REWRITES a prompt. Identity is held by exact
   * repetition of each character's appearance sentence — in the Echo regime that sentence is the
   * only thing routing a shot to the right anchor — so a rewrite that paraphrases it swaps the
   * character out. Empty roster ⇒ nothing sent ⇒ every draft unchanged.
   */
  const castArg = () => {
    const cast = castForPrompting($state.snapshot(characters));
    return cast.length ? { cast } : {};
  };
  /**
   * Which engine's PROMPT CONVENTIONS every LLM path writes in.
   *
   * Sent only for H3, so an LTX project's requests stay byte-identical to what they were before H3
   * existed and the server default (`ltx`) keeps answering for them. The two are not variants of
   * one style: an H3 prompt is block-structured, states its negatives inline because there is no
   * negative-prompt field, directs a soundtrack the model generates, and addresses references by
   * ordinal. `api/src/routes/h3_prompting.rs` is the one place those conventions live.
   */
  const engineArg = () => (engine === 'h3' ? { engine } : {});
  /**
   * The project's H3 references as writer BRIEFS — kind, whether it carries sound, and the user's
   * label. Never the pixels: the writer is being told which ordinal to address and what the user
   * meant by it, not asked to look at the reference.
   *
   * Whole-project and unscoped. The per-shot paths narrow this to the shot's own in-scope list
   * (`shotContext`), because the engine's ordinals restart at 1 per shot — but the batch paths are
   * reasoning across the timeline and want the roster.
   */
  /**
   * The reference roster for the LLM endpoints that take a whole project rather than one shot.
   *
   * 🔴 Goes through `h3RefBriefs` — it must not build the brief itself. It did, and the second copy
   * had drifted on BOTH fields it shares:
   *
   * - **`role` was missing entirely**, so the backend fell back to `default_for(kind)`. An
   *   `edit_source` video was briefed to the writer as *"supplies camera movement, cuts and rhythm
   *   only"* — the opposite of what it is — and the project soundtrack was briefed as a
   *   voice-timbre clip, losing "this is a slice of your own song" altogether.
   * - **`has_audio` used `r.audio`**, which on a VIDEO is not the field that decides. `h3RefBriefs`
   *   reads `with_audio` there and explains why at length: a video's `<Audio k>` is a deliberate
   *   choice, and getting it wrong shifts every later audio ordinal in the drafted prose.
   *
   * The lesson is the copy, not either bug: `h3RefBriefs` carries the reasoning for both rules in
   * its comments, and a hand-rolled duplicate a few thousand lines away inherits none of it.
   */
  const h3RefsArg = () => {
    if (engine !== 'h3' || h3References.length === 0) return {};
    return {
      h3_references: h3RefBriefs(h3References, {
        mode: h3SoundtrackMode,
        diegetic: h3SoundtrackDiegetic
      })
    };
  };
  /**
   * Where the song's voice falls, one lane per GENERATION, for the LLM paths that write or rewrite
   * the whole project at once.
   *
   * 🔴 The per-shot paths get their own single lane through `shotContext`; these two do not go
   * through it, and until 2026-08-14 they were told nothing about the audio at all —
   * `script_system_prompt` and `rework_system` never called the audio clause. That is the writer
   * that decides the standing `<Audio N>` sentence for fifteen generations in one turn, and the
   * rewrite that can undo a hand-fixed one in a single click.
   *
   * Empty ⇒ nothing sent ⇒ the server keeps the prompt it had, which is what a project with no
   * transcript (or no song) should get.
   */
  const vocalLaneArg = () => {
    const lanes = vocalLanes({
      lyrics: $state.snapshot(lyrics) as LyricWord[],
      h3Packing: $state.snapshot(packedGenerations) as PackedGeneration[],
      engine,
      songSeconds: waveDuration
    });
    return lanes.length ? { h3_vocal_lanes: lanes } : {};
  };

  // ── One pass over the whole project ─────────────────────────────────────────
  //
  // Owner, 2026-08-08: *"yes whole project in one pass should exist like it used to for ltx, i
  // probably won't use it for any reasonable length thing but should work"*. The standalone LTX
  // pages have always had it (`ltxVideoScriptFromLlm`); the Director never did — its batch was
  // `draftAllShots`, which is N single-unit calls in a loop, and auto-direct, which lays a timeline
  // OUT rather than writing into one. See `lib/director/script-pass.ts` for the shape of the brief.
  let scriptingAll = $state(false);

  /**
   * The render units one pass writes, in order — and what each one is FOR.
   *
   * 🔴 **This is where the derived-generations problem is answered.** `packGenerations` re-derives
   * the packing from the clips on every read and nothing stores it, so there is no way for a reply
   * to bring a new generation into existence — a surplus block has no id to be written under. So the
   * request is SIZED FROM THE CURRENT PACKING: `n_segments` is `units.length`, the brief states each
   * unit's exact duration and beat count, and block `i` is applied to unit `i`. The pass re-prompts
   * the timeline it was given; it never lays one out.
   *
   * On LTX a render IS a clip, so the unit list is the clip list and each unit has one part.
   */
  function scriptPassUnits(): ScriptPassUnit[] {
    if (engine === 'h3') {
      return packedGenerations.map((g) => ({
        id: g.id,
        // The SNAPPED duration, not the authored sum — it is the clock every `MM:SS.mmm` in that
        // generation's block is measured against. Same number `h3GenerationScope` sends.
        seconds: g.seconds,
        // `shotOneLiner` is gist → idea → the first 16 words of the clip's own seed body. Intent,
        // not prose: the writer is being told what the beat is for, and is about to write the prose
        // itself.
        parts: g.shots.map((s) => shotOneLiner(s.clip))
      }));
    }
    return clips.map((c) => ({ id: c.id, seconds: c.seconds, parts: [shotOneLiner(c)] }));
  }

  /**
   * How many units the button says it will write.
   *
   * 🔴 Derived by CALLING the builder rather than counting `packedGenerations` / `clips` beside it.
   * The number on the button is the `n_segments` the request will carry, and a second expression
   * for it is a second thing to keep in step — which on this endpoint would show up as a reply
   * sized for a count nothing else agreed with.
   */
  const scriptPassUnitCount = $derived(scriptPassUnits().length);

  /**
   * Write EVERY render unit's prompt in one LLM turn.
   *
   * ⚠️ **An explicit replace, over the whole project.** The single-unit draft already replaces one
   * prompt and says so on its own button; this does it to all of them, so it asks first — and the
   * restore point it takes now carries the H3 blocks as well as the clips, or Revert would put the
   * shot list back while leaving every generation wearing the new block.
   *
   * 🔴 Applied through {@link absorbH3ReplyInto} — the same funnel the single-unit draft uses, so
   * the block is stored verbatim and a surplus or missing `[Shot N]` renders exactly as it would
   * from a hand edit. There is deliberately no second application path.
   */
  async function scriptWholeProject() {
    // Same guard `draftAllShots` carries: two whole-project writers running at once would interleave
    // their writes onto the same fields. The button is disabled too; this is the double-click.
    if (scriptingAll || draftAllRunning) return;
    const units = scriptPassUnits();
    if (units.length === 0) {
      renderError = `Add a ${vocab.renderUnit} before writing the whole project.`;
      return;
    }
    if (units.length > SCRIPT_PASS_MAX_UNITS) {
      // Refused rather than truncated: the endpoint clamps `n_segments` to 24 and would answer with
      // 24 blocks for a longer timeline, which lands on the first 24 units and leaves the rest
      // silently untouched. Saying the number is cheaper than explaining the silence afterwards.
      renderError =
        `One pass writes at most ${SCRIPT_PASS_MAX_UNITS} ${vocab.renderUnitPlural} and this project has ` +
        `${units.length}. Use Draft all, which writes them one at a time.`;
      return;
    }
    const brief = autoBrief.trim() || summary?.trim() || '';
    if (
      !confirm(
        `Write all ${units.length} ${vocab.renderUnitPlural} in one pass?\n\n` +
          `This REPLACES every ${vocab.renderUnit}'s prompt — hand-written text included. ` +
          `The timeline itself is untouched: same ${vocab.renderUnitPlural}, same order, same lengths.\n\n` +
          `Revert undoes it.`
      )
    ) {
      return;
    }
    captureRestore('Write every prompt');
    scriptingAll = true;
    renderError = null;
    try {
      const aspect = aspectPhrase(outW, outH);
      // The SHORTEST unit — the endpoint takes one scalar for the whole batch and cannot be told to
      // omit it. `scriptPassSeconds` carries the reasoning for why short is the safe direction and
      // why the per-unit lengths in the brief are what actually bind.
      const perUnitSeconds = scriptPassSeconds(units);
      const res = await ltxVideoScriptFromLlm({
        // 🔴 The author's brief AND the timeline listing. Without the listing this endpoint writes
        // N units from one line and every idea already on the timeline is thrown away — which is
        // auto-direct wearing this button. `script-pass.ts` explains the wording.
        brief: scriptPassBrief(
          brief,
          units,
          engine === 'h3' ? { unit: 'generation', part: 'shot' } : { unit: 'shot', part: null }
        ),
        n_segments: units.length,
        ...(perUnitSeconds ? { seconds_per_segment: perUnitSeconds } : {}),
        ...storyArg(),
        ...castArg(),
        ...engineArg(),
        ...h3RefsArg(),
        ...vocalLaneArg(),
        // H3 only — the spec line the corpus opens nearly every prompt with. The OUTPUT target,
        // which is the canvas the shots are composed for.
        ...(engine === 'h3' && aspect ? { aspect } : {}),
        // LTX only, and only when the project brings its own track: it tells the writer not to
        // describe sound. H3 generates its own soundtrack and its arm of the endpoint never reads
        // this flag, so sending it there would be noise.
        ...(engine === 'ltx25' && (audioKind === 'voice' || audioKind === 'mixed')
          ? { audio_supplied: true }
          : {})
      });
      // 🔴 NOT filtered. A blank entry is skipped BY INDEX below — compacting the array would slide
      // every later block one unit earlier, which is the silent mis-application this whole design
      // is built to avoid.
      const blocks = res.segments ?? [];
      const typedDrafts = res.h3_generations ?? [];
      const outcome = scriptPassOutcome(units.length, blocks.length, vocab.renderUnitPlural);
      noteWarnings(outcome.warnings);
      // Units whose reply was prose rather than a block — kept, and reported. See below.
      const prose: number[] = [];
      // Units that stopped existing between the request and the reply (a concurrent edit, an undo).
      // Vanishingly rare and reported anyway: a block that landed nowhere must never look like one
      // that landed somewhere.
      const lost: number[] = [];
      let wrote = 0;
      for (let i = 0; i < outcome.applied; i++) {
        const text = blocks[i]?.trim();
        // Nothing written rather than a unit blanked: an empty reply for one entry must not delete
        // the prompt that entry already had.
        if (!text) continue;
        if (engine === 'h3') {
          // 🔴 Re-resolved every iteration, never from the captured `units`: `packedGenerations` is
          // `$derived` and the previous write changed its inputs. Boundaries cannot move (they are
          // decided by lengths, forced breaks and frame pins, none of which a body edit touches),
          // but reading the live packing is what makes that a fact rather than an assumption.
          const gen = packedGenerations.find((g) => g.id === units[i].id);
          if (!gen) {
            lost.push(i + 1);
            continue;
          }
          // ⚠️ The typed carrier is index-aligned to `segments` ONLY when the server populated it
          // all-or-nothing (it parses every segment or returns none), so the lengths agreeing is
          // the test for whether zipping them is legal. Normally it is empty — a BLOCK does not
          // parse as an `H3PromptDraft` — and `h3DraftFrom` reads the block out of the text.
          const typed = typedDrafts.length === blocks.length ? typedDrafts[i] : undefined;
          if (absorbH3ReplyInto(gen.id, typed, text)) {
            wrote++;
            continue;
          }
          // The writer answered with prose. Nothing is lost: it lands on the unit's first beat,
          // which is where the single-unit path puts prose too — and if the unit has no block yet,
          // the seed composes it straight back into `[Shot 1]`. Where it does have one, the
          // composer's drift box reports the beat and the block disagreeing, which is what it is
          // for. Repairing it here would mean guessing which section each paragraph belongs to.
          const first = gen.shots[0]?.clip.id;
          if (first) {
            patchClip(first, { prompt: text });
            wrote++;
          }
          prose.push(i + 1);
          continue;
        }
        // LTX: a render IS a clip, so the unit id is the clip id.
        patchClip(units[i].id, { prompt: text });
        wrote++;
      }
      if (prose.length) {
        noteWarnings([
          `The writer answered with prose instead of a prompt block for ${prose.length === 1 ? vocab.renderUnit : vocab.renderUnitPlural} ${prose.join(', ')} — it was kept on the opening ${vocab.subUnit ?? vocab.renderUnit}, not written into the prompt. Check those before rendering.`
        ]);
      }
      if (lost.length) {
        noteWarnings([
          `The timeline changed while the writer was working, so the prompts for ${vocab.renderUnitPlural} ${lost.join(', ')} had nowhere to go and were dropped.`
        ]);
      }
      if (wrote) markDirty();
      else renderError = 'The one-pass writer returned nothing usable.';
    } catch (e) {
      renderError = `one-pass write failed: ${(e as Error).message}`;
    } finally {
      scriptingAll = false;
    }
  }

  // Send the whole shot list to the LLM for a prose synopsis of the video the timeline describes.
  async function summarizeShots() {
    const shots = editorShots(clips);
    if (shots.length === 0) {
      renderError = 'Add at least one shot with a prompt to summarise.';
      return;
    }
    summarizing = true;
    renderError = null;
    try {
      // `engineArg()` matters here and is not cosmetic: without it the backend numbers every
      // shot under LTX's tags, and the one that misleads is `carried` — it tells the reader the
      // shot continues out of the previous shot's final frame, which is never true on H3.
      const res = await summarizeDirectorShots({ shots, ...engineArg(), ...storyArg() });
      summary = res.summary;
      summaryStale = false;
    } catch (e) {
      renderError = `summarise failed: ${(e as Error).message}`;
    } finally {
      summarizing = false;
    }
  }

  // Whole-script rework: apply one change across every shot, rewriting prompts (+gists) in place.
  // The backend maps its results back to the ORIGINAL clip positions (it filters blank shots), so
  // we apply by the returned `index` into the current clips array.
  async function reworkScript() {
    const instruction = reworkInstruction.trim();
    if (!instruction) {
      renderError = 'Describe the change to apply across the script.';
      return;
    }
    const shots = editorShots(clips);
    if (shots.length === 0) {
      renderError = 'Add at least one shot with a prompt to rework.';
      return;
    }
    captureRestore('Rework script');
    reworking = true;
    renderError = null;
    try {
      const { shots: out, warnings } = await reworkDirectorScript({
        shots,
        instruction,
        ...storyArg(),
        ...castArg(),
        ...engineArg(),
        ...h3RefsArg(),
        // A rework touches every block at once, so it is the single fastest way to reintroduce a
        // mimed vocal into a generation somebody had already corrected by hand.
        ...vocalLaneArg()
      });
      noteWarnings(warnings);
      let applied = 0;
      for (const s of out) {
        const target = clips.find((c) => c.id === s.id);
        if (!target) continue;
        // On H3 a rewrite comes back as its generation's structured draft, which fills the
        // composer's sections and every shot body in that generation. Absorbing it per shot is
        // idempotent — the later shots of a generation simply re-write what the first one already
        // applied — and it keeps the prose path working for a koblem that predates the field.
        if (!absorbH3Reply(target, s.h3_draft, s.prompt)) target.prompt = s.prompt;
        if (s.gist) target.gist = s.gist;
        applied++;
      }
      if (applied) {
        reworkInstruction = '';
        markDirty();
      } else {
        renderError = 'Rework returned nothing applicable.';
      }
    } catch (e) {
      renderError = `rework failed: ${(e as Error).message}`;
    } finally {
      reworking = false;
    }
  }

  // Continuity review: ask the LLM for concrete issues, tie each to its clip (0-based index or a
  // whole-story note), and keep a per-finding notes box + fixing flag for the one-click rework.
  async function reviewShots() {
    const shots = editorShots(clips);
    if (shots.length === 0) {
      renderError = 'Add at least one shot with a prompt to review.';
      return;
    }
    reviewing = true;
    renderError = null;
    try {
      // 🔴 On H3 the shot list alone cannot be reviewed: the block is the document, and every
      // schema check the supervisor makes reads it. Send the packing — `h3ReviewGenerations` says
      // what that buys and what its absence costs.
      const { findings, warnings } = await reviewDirectorShots({
        shots,
        ...storyArg(),
        ...engineArg(),
        ...(engine === 'h3' && packedGenerations.length
          ? { h3_generations: h3ReviewGenerations(packedGenerations) }
          : {})
      });
      noteWarnings(warnings);
      // `shot` is already the clip id (null = a whole-story note); a finding the backend couldn't
      // tie to a real shot never arrives, so this only guards against a clip deleted mid-review.
      reviewFindings = findings.map((f) => ({
        severity: f.severity,
        issue: f.issue,
        clipId: clips.some((c) => c.id === f.shot) ? f.shot : null,
        notes: '',
        fixing: false
      }));
      reviewRan = true;
    } catch (e) {
      renderError = `review failed: ${(e as Error).message}`;
    } finally {
      reviewing = false;
    }
  }

  // Backfill a plain-language gist for every shot that has a prompt but no summary yet — one batch
  // LLM call. Leaves shots that already have a gist untouched (use the per-shot Summarise to redo).
  async function gistAllShots() {
    const targets = clips
      .map((c, i) => ({ c, i }))
      .filter(({ c }) => c.prompt.trim() && !c.gist?.trim());
    if (targets.length === 0) {
      renderError = 'Every shot already has a summary.';
      return;
    }
    const shots: DirectorEditorShot[] = targets.map(({ c }) => ({
      id: c.id,
      prompt: c.prompt.trim(),
      idea: c.idea?.trim() || undefined,
      mode: c.mode,
      seconds: c.seconds,
      new_scene: c.new_scene
    }));
    captureRestore('Gist all shots');
    gistingAll = true;
    renderError = null;
    try {
      const { gists, warnings } = await gistDirectorShots({
        shots,
        ...engineArg(),
        ...storyArg()
      });
      noteWarnings(warnings);
      let applied = 0;
      for (const g of gists) {
        const target = clips.find((c) => c.id === g.id);
        if (target) {
          target.gist = g.gist;
          applied++;
        }
      }
      if (applied) markDirty();
    } catch (e) {
      renderError = `gist failed: ${(e as Error).message}`;
    } finally {
      gistingAll = false;
    }
  }

  // Apply one review finding: rework the flagged shot, using the issue (+ the user's own notes) as
  // the change instruction. Dismisses the finding on success.
  async function applyFinding(item: ReviewItem) {
    if (item.clipId === null) return;
    const clip = clips.find((c) => c.id === item.clipId);
    if (!clip) {
      renderError = 'That shot no longer exists.';
      reviewFindings = reviewFindings.filter((f) => f !== item);
      return;
    }
    const extra = item.notes.trim();
    const instruction = extra ? `${item.issue}\n\nAdditional notes: ${extra}` : item.issue;
    captureRestore('Apply fix');
    item.fixing = true;
    renderError = null;
    try {
      await reworkClipCore(clip, instruction);
      reviewFindings = reviewFindings.filter((f) => f !== item);
    } catch (e) {
      renderError = `fix failed: ${(e as Error).message}`;
    } finally {
      item.fixing = false;
    }
  }

  // Auto-fix all: apply every shot-scoped finding in one go (sequential — each is a gated LLM call).
  async function fixAllFindings() {
    const items = reviewFindings.filter((f) => f.clipId !== null);
    if (items.length === 0) return;
    captureRestore('Auto-fix all');
    fixingAll = true;
    renderError = null;
    try {
      for (const item of items) {
        const clip = clips.find((c) => c.id === item.clipId);
        if (!clip) {
          reviewFindings = reviewFindings.filter((f) => f !== item);
          continue;
        }
        item.fixing = true;
        const extra = item.notes.trim();
        const instruction = extra ? `${item.issue}\n\nAdditional notes: ${extra}` : item.issue;
        try {
          await reworkClipCore(clip, instruction);
          reviewFindings = reviewFindings.filter((f) => f !== item);
        } catch (e) {
          item.fixing = false;
          renderError = `fix failed on a shot: ${(e as Error).message}`;
          break; // stop the batch on the first hard error
        }
      }
    } finally {
      fixingAll = false;
    }
  }

  // Extend the story: append N new shots that continue from where the timeline ends.
  async function extendStory() {
    const shots = editorShots(clips);
    if (shots.length === 0) {
      renderError = 'Write at least one shot before extending the story.';
      return;
    }
    captureRestore('Extend story');
    extending = true;
    renderError = null;
    try {
      const { shots: added } = await extendDirectorScript({
        shots,
        count: extendCount,
        ...(extendDirection.trim() ? { direction: extendDirection.trim() } : {}),
        ...storyArg(),
        ...castArg(),
        ...engineArg(),
        ...h3RefsArg()
      });
      const newClips: DirectorClip[] = added.map((s) => ({
        id: uid(),
        idea: s.idea?.trim() || undefined,
        prompt: s.prompt,
        gist: s.gist?.trim() || undefined,
        mode: 't2v',
        seconds: s.seconds && s.seconds > 0 ? s.seconds : shotSeconds,
        keyframes: [],
        ...(s.new_scene ? { new_scene: true } : {})
      }));
      if (newClips.length) {
        clips = [...clips, ...newClips];
        // Absorb AFTER the append, never before: a new shot has no generation to belong to until
        // the packer has seen it, and `packedGenerations` is pull-based so it re-packs on this
        // read. Absorbing first would silently drop every structured draft the extend produced.
        added.forEach((s, i) => absorbH3Reply(newClips[i], s.h3_draft, s.prompt));
        selectedIds = [newClips[0].id];
        extendDirection = '';
        markDirty();
      }
    } catch (e) {
      renderError = `extend failed: ${(e as Error).message}`;
    } finally {
      extending = false;
    }
  }

  // Auto-title: name the project from its brief + shots. `silent` swallows errors (used to auto-name
  // a fresh auto-directed plan without nagging on failure).
  async function autoTitle(silent = false) {
    const shots = editorShots(clips);
    if (shots.length === 0 && !autoBrief.trim()) {
      if (!silent) renderError = 'Add a brief or a shot before auto-titling.';
      return;
    }
    titling = true;
    if (!silent) renderError = null;
    try {
      const { title } = await titleDirectorProject({
        shots,
        ...engineArg(),
        ...(autoBrief.trim() ? { brief: autoBrief.trim() } : {}),
        ...storyArg()
      });
      if (title.trim()) {
        projectName = title.trim();
        await commitRename();
      }
    } catch (e) {
      if (!silent) renderError = `auto-title failed: ${(e as Error).message}`;
    } finally {
      titling = false;
    }
  }

  // ── Storyboard stills (FLUX) ─────────────────────────────────────────────────
  // Generate a preview still for one shot: the LLM writes a FLUX prompt from the shot's scene AT its
  // story moment (grounded in the story canon + earlier shots), matched to any reference images, then
  // FLUX renders it. Stored downscaled so saved projects don't balloon. Returns false on failure.
  async function snapshotClip(clip: DirectorClip): Promise<boolean> {
    const idx = clips.findIndex((c) => c.id === clip.id);
    const scene = clip.gist?.trim() || clip.prompt.trim() || clip.idea?.trim();
    if (!scene) return false;
    const prior = clips.slice(0, idx).map(shotOneLiner).filter(Boolean);
    const parts = [
      `A single cinematic still frame from a film — the moment of this shot: ${scene}.`
    ];
    if (!storyIsEmpty(story)) {
      if (story.backstory.trim()) parts.push(`Story so far: ${story.backstory.trim()}`);
      if (story.tone.trim()) parts.push(`Visual tone / look: ${story.tone.trim()}`);
    }
    if (prior.length) parts.push(`Earlier shots: ${prior.slice(-4).join('; ')}.`);
    const brief = parts.join('\n');
    // Reference images (story canon + per-shot refs) steer identity/look, both for the prompt writer
    // and the FLUX render itself.
    const refs = [
      ...(story.references ?? []).map((r) => r.image_b64),
      ...(clip.references ?? []).map((r) => r.image_b64)
    ].filter((b): b is string => !!b?.trim());

    const { prompt: fluxPrompt } = await imageSuggestPrompt(brief, refs.length ? refs : undefined);
    const { images } = await imageGenerate({
      prompt: fluxPrompt || scene,
      width: 1024,
      height: 576,
      ...(refs.length ? { reference_images: refs } : {})
    });
    if (!images[0]?.url) return false;
    // Re-encode as a compact JPEG so N stills don't bloat the timeline JSONB.
    const file = new File([await fetchMediaBlob(images[0].url)], 'snapshot.png');
    clip.snapshot_b64 = await putAsset(await downscaleImageToBlob(file, 1024, 0.85));
    markDirty();
    return true;
  }

  async function snapshotSelectedClip() {
    if (!selectedClip) return;
    await snapshotClip(selectedClip);
  }

  // Storyboard the whole timeline: a still for every shot with a prompt, sequential (FLUX is
  // GPU-gated — parallel would just queue). Only fills shots missing a snapshot.
  async function storyboardAll() {
    const targets = clips.filter((c) => (c.prompt.trim() || c.idea?.trim()) && !c.snapshot_b64);
    if (targets.length === 0) {
      renderError = 'Every shot already has a snapshot.';
      return;
    }
    captureRestore('Storyboard all');
    storyboarding = true;
    renderError = null;
    try {
      let done = 0;
      for (const clip of targets) {
        storyboardProgress = `${done + 1}/${targets.length}`;
        try {
          await snapshotClip(clip);
        } catch (e) {
          renderError = `storyboard stopped: ${(e as Error).message}`;
          break;
        }
        done++;
      }
    } finally {
      storyboarding = false;
      storyboardProgress = '';
    }
  }

  // Everything outside the shot itself that its grounding reads, snapshotted so the pure builder
  // never sees a reactive proxy.
  const promptContext = (): PromptContext => ({
    clips: snapshotClips(),
    characters: $state.snapshot(characters) as DirectorCharacter[],
    story: $state.snapshot(story) as StoryContext,
    lyrics: $state.snapshot(lyrics) as LyricWord[],
    fps,
    audioKind,
    // Which conventions every draft / reroll / rework in this project is written in. Read from the
    // project rather than a UI toggle: it is the same fact `caps` above is derived from, and a
    // prompt written for the wrong engine is not a preference, it is a prompt the render ignores
    // half of.
    engine,
    h3References: $state.snapshot(h3References) as DirectorH3Reference[],
    // The song's diegetic status and delivery mode, for the reference roster. Without
    // them the writer falls back to the guide, which files a reused score under
    // `non_diegetic_music` — music the characters cannot hear — on a project whose
    // character sings it.
    h3Soundtrack: { mode: h3SoundtrackMode, diegetic: h3SoundtrackDiegetic },
    // 🔴 The packing, so the writer's reference roster is the one THIS RENDER carries rather than
    // the whole project. Scope lives on the generation as `reference_ids`; without this the
    // grounding fell back to the retired per-shot `segments`, which nothing has written since
    // Stage 5 — so a narrowed scope was invisible and every draft over-included.
    h3Packing: $state.snapshot(packedGenerations) as PackedGeneration[],
    // The SAME geometry the timeline is drawn from, so what the writer is told about which shot is
    // singing cannot disagree with where the author can see that shot sitting.
    layout: timelineLayout,
    // 🔴 The song itself, so the writer can be told WHICH LOCAL SECONDS of its own slice carry a
    // vocal — the fix for a mouth miming a vocal that ends 37 s before the video does. The ref is
    // never the bytes; the server cuts the slice. `songSeconds` only decides which generations get
    // one and how long each is (the offsets are right without it).
    songAsset: audioB64,
    songSeconds: waveDuration,
    // For the H3 spec line, which the corpus opens nearly every prompt with. The OUTPUT target, not
    // the base pass — that is the canvas the shot is composed for.
    width: outW,
    height: outH
  });
  const shotContextFor = (clip: DirectorClip, idx: number, withImages = true) =>
    shotContext(promptContext(), clip, idx, withImages);

  /**
   * What the writer needs to know about the GENERATION this shot sits in.
   *
   * 🔴 These are what make the writer return one body per shot. `shotContextFor` sends the SHOT's
   * own `seconds`, and the server presents that as the whole budget — so on a three-shot generation
   * the writer is briefed on 4 s when the render is 15 s, and has no idea it is writing a
   * multi-shot block at all. `seconds` stays: it is still correct for its own purpose (how long
   * this one shot plays). These three describe the render unit around it, and `h3_shot_focus` says
   * which shot inside it the request is actually about.
   *
   * Returned as a typed object and SPREAD rather than written inline at each call site: a spread is
   * exempt from the object-literal excess-property check, so this compiles against a client.ts that
   * has not yet declared the fields. The server ignores them when absent, so an older koblem keeps
   * working either way.
   */
  interface H3GenerationScope {
    h3_generation_seconds?: number;
    h3_shot_count?: number;
    h3_shot_focus?: number;
  }
  function h3GenerationScope(clip: DirectorClip): H3GenerationScope {
    if (engine !== 'h3') return {};
    const gen = packedGenerations.find((g) => g.shots.some((s) => s.clip.id === clip.id));
    if (!gen) return {};
    const focus = gen.shots.findIndex((s) => s.clip.id === clip.id);
    return {
      // The SNAPPED duration, not the authored sum — it is what the generation actually renders at,
      // and therefore the span the cut times in the block have to fit inside.
      h3_generation_seconds: gen.seconds,
      h3_shot_count: gen.shots.length,
      h3_shot_focus: Math.max(0, focus)
    };
  }

  // Per-shot "Draft with LLM": expand the `idea` seed into a full LTX prompt, grounded in this
  // shot's frames/references, the earlier shots (story-so-far), neighbours + Story canon. Only the
  // full `prompt` (and a plain-language `gist`) is written, so the user's one-liner idea survives.
  // `keepSummary`: drafting a shot only expands the idea the summary already described, so the
  // pass that produces a plan must not immediately declare its own summary stale. Pinning is
  // synchronous around markDirty, so a concurrent user edit can never be swallowed by it.
  /**
   * Absorb a prompt-LLM reply that is a structured H3 generation rather than a shot's prose.
   *
   * koblem's `h3_prompting.rs` now makes the writer emit the object in PROMPT-CONTRACT §2 — the six
   * sections, the `[Shot N]` bodies, the task types — so a draft fills the composer's FIELDS. It
   * arrives on the typed `h3_draft` field, or as JSON text inside the legacy `prompt` string from a
   * koblem that predates it; `h3DraftFrom` tries both.
   *
   * 🔴 Returning false is not a failure path, it is the LEGACY path: an older koblem, an older
   * project or the chat surface all still answer with prose, and prose belongs on the shot it was
   * drafted for. Nothing is lost either way, which is the property that has to survive the typed
   * field landing.
   */
  function absorbH3Reply(
    clip: DirectorClip,
    typed: H3PromptDraft | undefined,
    text: string | undefined
  ): boolean {
    if (engine !== 'h3') return false;
    const gen = packedGenerations.find((g) => g.shots.some((s) => s.clip.id === clip.id));
    if (!gen) return false;
    return absorbH3ReplyInto(gen.id, typed, text);
  }

  /**
   * The same absorb, addressed by GENERATION rather than by one of its shots.
   *
   * 🔴 Split out for the one-pass writer (`scriptWholeProject`), which has a block per generation
   * and no shot to look one up from — and split rather than copied, because a second application
   * path is exactly how two blocks for one generation start disagreeing. Everything below is the
   * single funnel: seed record → `setGenerationPrompt`. See {@link absorbH3Reply} for what this
   * used to do as well, and why it stopped.
   */
  function absorbH3ReplyInto(
    genId: string,
    typed: H3PromptDraft | undefined,
    text: string | undefined
  ): boolean {
    if (engine !== 'h3') return false;
    const reply = h3DraftFrom(typed, text);
    if (!reply) return false;
    const gen = packedGenerations.find((g) => g.id === genId);
    if (!gen) return false;
    // 🔴 **No per-shot mapping any more, and no `focus` anchor.** `applyH3Reply` used to also map
    // `reply.shots` positionally onto this generation's clip ids and return body writes, applied
    // here onto bodies the compiler stops reading the moment the block below is stored.
    // ⚠️ Retiring it does NOT make the editor non-positional: the `[Shot N]` number IS the
    // position. It makes the block the one place that position is recorded.
    // What is left is the section SEED RECORD — read only while `prompt` is absent, kept so an
    // older project opens showing what it always rendered.
    const patch = applyH3Reply(reply);
    if (Object.keys(patch.generation).length) updateGeneration(gen.id, patch.generation);
    // 🔴 The block is what renders, so a draft that only filled the FIELDS would be stored and
    // never sent. `packedGenerations` is re-read rather than reusing `gen`: it is `$derived` and
    // the two writes above changed its inputs, so the captured value predates the draft.
    //
    // 🔴 **When the writer answered with a block, that text is stored VERBATIM** rather than
    // re-composed from the fields it was parsed into. `h3DraftFromBlock` reads a block into the
    // `H3PromptDraft` shape to fill the panels, and that shape has nowhere to put the block's
    // `unparsed` tail — so re-composing would quietly delete whatever the model wrote that we have
    // no field for. Storing the reply is the only lossless option, and it is also the whole point
    // of the reversal: a prompt the model wrote is a prompt, not an input to our emitter.
    //
    // ⚠️ Either way this REPLACES the block, hand-written text included. Drafting is an explicit
    // replace and always was — `applyH3Reply` already overwrote every section.
    const merged = packedGenerations.find((g) => g.id === gen.id);
    const written = text?.trim();
    if (merged) {
      setGenerationPrompt(
        gen.id,
        written && looksLikeH3Block(written) ? written : seedH3Prompt(merged)
      );
    }
    if (patch.warnings.length) llmWarnings = [...llmWarnings, ...patch.warnings];
    markDirty();
    return true;
  }

  async function draftClipCore(clip: DirectorClip, keepSummary = false) {
    const idx = clips.findIndex((c) => c.id === clip.id);
    const res = await ltxPromptFromLlm({
      idea: clip.idea?.trim() || clip.prompt.trim(),
      want_gist: true,
      // Let the writer propose beats when the idea holds a moment rather than one continuous
      // action. The server ignores this for a shot that already has beats, so a hand-placed
      // timing is never overwritten by a redraft.
      //
      // 🔴 ANDed with `canBeats` for the same reason as the planner above: a drafted beat on a box
      // with no relay node is a shot that cannot render, written by the machine into a field the
      // author never touched.
      want_beats: canBeats && beatsEnabled,
      ...shotContextFor(clip, idx),
      ...h3GenerationScope(clip)
    });
    // On H3 the writer answers with the whole GENERATION's structured draft, so it fills the
    // composer's sections and every shot body in that generation — not just this one's prompt.
    const structured = absorbH3Reply(clip, res.h3_draft, res.prompt);
    if (!structured && res.prompt) clip.prompt = res.prompt;
    if (res.gist) clip.gist = res.gist;
    if (res.beats?.length && !(clip.beats ?? []).length) {
      clip.beats = res.beats.map((b) => ({ seconds: b.seconds, text: b.text }));
    }
    if (res.prompt || res.gist) {
      const wasStale = summaryStale;
      markDirty();
      if (keepSummary) summaryStale = wasStale;
    }
    draftFailedIds = draftFailedIds.filter((id) => id !== clip.id);
  }

  async function draftClipPrompt() {
    const clip = selectedClip;
    if (!clip) return;
    await draftClipCore(clip);
  }

  // Reroll: rewrite the shot's EXISTING prompt — same scene/subject/action/intent, fresh wording —
  // keeping all the same grounding. Refreshes the gist too.
  async function rerollClipPrompt() {
    const clip = selectedClip;
    if (!clip || !clip.prompt.trim()) return;
    const idx = clips.findIndex((c) => c.id === clip.id);
    const res = await ltxPromptFromLlm({
      rewrite: clip.prompt.trim(),
      want_gist: true,
      ...shotContextFor(clip, idx),
      ...h3GenerationScope(clip)
    });
    const structured = absorbH3Reply(clip, res.h3_draft, res.prompt);
    if (!structured && res.prompt) clip.prompt = res.prompt;
    if (res.gist) clip.gist = res.gist;
    if (structured || res.prompt || res.gist) markDirty();
  }

  // Rework one shot toward a directed change (from the inspector's box, or a review finding).
  // Revises the existing prompt in place, keeping continuity; refreshes the gist.
  async function reworkClipCore(clip: DirectorClip, instruction: string) {
    const idx = clips.findIndex((c) => c.id === clip.id);
    const res = await ltxPromptFromLlm({
      instruction,
      ...(clip.prompt.trim() ? { rewrite: clip.prompt.trim() } : { idea: clip.idea?.trim() }),
      want_gist: true,
      ...shotContextFor(clip, idx),
      ...h3GenerationScope(clip)
    });
    const structured = absorbH3Reply(clip, res.h3_draft, res.prompt);
    if (!structured && res.prompt) clip.prompt = res.prompt;
    if (res.gist) clip.gist = res.gist;
    if (structured || res.prompt || res.gist) markDirty();
  }

  async function reworkClip(instruction: string) {
    const clip = selectedClip;
    if (!clip || !instruction.trim()) return;
    await reworkClipCore(clip, instruction.trim());
  }

  // Summarise: write a plain-language `gist` of the shot from its current prompt WITHOUT touching
  // the prompt — so a jargon-heavy or auto-directed shot becomes scannable.
  async function summariseClipGist() {
    const clip = selectedClip;
    if (!clip) return;
    const src = clip.prompt.trim() || clip.idea?.trim();
    if (!src) return;
    const idx = clips.findIndex((c) => c.id === clip.id);
    const res = await ltxPromptFromLlm({
      rewrite: src,
      gist_only: true,
      ...shotContextFor(clip, idx, false)
    });
    if (res.gist) {
      clip.gist = res.gist;
      markDirty();
    }
  }

  /**
   * Summarise a whole H3 GENERATION — its entire block — into `DirectorH3Generation.gist`.
   *
   * 🔴 **Deliberately not `summariseClipGist`, and it must not be folded into it.** That one reads
   * `selectedClip` and summarises `clip.prompt`, which under the reversal is a MIRROR of ONE
   * `[Shot N]` body — so it describes a single beat. The owner's model, 2026-08-08: a shot is one
   * prompt is one generation is one render, and *"gist is the gist of the ENTIRE PROMPT"*. The
   * source here is therefore the block, and the destination is the generation.
   *
   * 🔴 `gist_only: true` and NOTHING ELSE on the body. `ltx_video.rs:1379` short-circuits that flag
   * to an engine-neutral summariser before any H3 branch runs, so the call takes arbitrary prompt
   * text and needs no `engine`, no references and no shot scope — adding them would only route the
   * request through a writer that is not being asked to write.
   *
   * ⚠️ The result never reaches the engine. See the field's doc in `client.ts`: it is the only thing
   * on `DirectorH3Generation` that `compile.ts` does not send.
   */
  async function summariseGenerationGist() {
    const gen = packedGenerations[activeGenIndex];
    if (!gen) return;
    // The block if there is one, else the seed the compiler would fall back to — the same rule
    // `compileH3Timeline` uses, so the sentence describes what would actually render.
    const src = gen.doc.prompt?.trim() || seedH3Prompt(gen).trim();
    if (!src) return;
    const res = await ltxPromptFromLlm({ rewrite: src, gist_only: true });
    if (res.gist) updateGeneration(gen.id, { gist: res.gist.trim() });
  }

  // Guard the render behind a confirm when the decoded output is well past the 1080p production
  // ceiling — the full-size final decode is the usual OOM cause. Returns false if the user backs out.
  function confirmLargeOutput(): boolean {
    if (outW * outH <= 1920 * 1088) return true;
    return confirm(
      `Output is ${outW}×${outH} (base ${resPlan.baseW}×${resPlan.baseH} → ${resPlan.factors.length} upscale stage(s)). ` +
        `That's well past 1080p and may OOM the GPU. Lower the output resolution to be safe.\n\nRender anyway?`
    );
  }

  /**
   * Say out loud what a Render all is about to do beyond rendering, when the project has the
   * unattended review switched on. The Settings toggle is the default; this is the last chance to
   * see it, because the whole point of the feature is that nobody is watching afterwards.
   */
  function confirmDailiesReview(shots: number): boolean {
    if (!critiqueEnabled) return true;
    const judged = `Render ${shots} shot${shots === 1 ? '' : 's'}, then have the vision model watch the footage and score each shot against its prompt.`;
    if (!critiqueAutoRetake) {
      return confirm(
        `${judged}\n\nNo extra renders — it only writes verdicts you can read on each shot's takes.\n\nGo?`
      );
    }
    return confirm(
      `${judged}\n\nShots scoring below ${critiqueThreshold.toFixed(2)} will be RE-ROLLED, up to ${critiqueMaxRetakes} extra take${critiqueMaxRetakes === 1 ? '' : 's'} each. Every take is kept — nothing is overwritten — and each shot ends up on its best-scoring take for you to review.\n\nWorst case this adds ${shots * critiqueMaxRetakes} render${shots * critiqueMaxRetakes === 1 ? '' : 's'} to the run.\n\nGo?`
    );
  }

  /**
   * The live "Render all" choice: continue, or start the tier over. `null` while nothing is asking.
   *
   * Held as a plan plus its resolver so the dialog can be awaited inline in `render()`, the way the
   * `confirm()` guards above it already are — the alternative is threading the whole dispatch into
   * a callback for one question.
   */
  let scopePrompt = $state<{
    plan: DirectorRenderPlan;
    resolve: (scope: 'stale' | 'all' | 'rebuild' | null) => void;
  } | null>(null);

  function answerRenderScope(scope: 'stale' | 'all' | 'rebuild' | null) {
    scopePrompt?.resolve(scope);
    scopePrompt = null;
  }

  /**
   * Re-join the selected takes into the cut, rendering nothing.
   *
   * 🔴 This is offered from the render dialog because it is what people MEAN by "Render all" about
   * half the time — the button reads like "make the finished thing out of all my shots", and until
   * now the only ways to reach a rebuild were selecting a take and finishing a partial render.
   * Wanting the combined video and being given a destructive full re-render is the exact trap the
   * dialog exists to close, so the harmless action has to be reachable from the same place.
   */
  async function rebuildCutOnly(tier: 'proxy' | 'final') {
    if (!projectId) return;
    try {
      await rebuildDirectorCut(projectId, tier);
      // `refresh: true` because the cut's bytes changed under a URL the editor is already holding —
      // the same follow-up selecting a take does, and without it the player keeps the old one.
      await hydratePersistedRenders(projectId, { refresh: true });
    } catch (e) {
      renderError = `the cut could not be re-joined: ${(e as Error).message}`;
    }
  }

  /**
   * Which scope this Render all should use — asking only when the answer can matter.
   *
   * 🔴 Returns `'all'` without a dialog when there is nothing to lose: no takes yet, an engine that
   * cannot answer (LTX resumes a different way), or a plan the server refused. A prompt shown when
   * both options do the same thing is one the user learns to click through, which is exactly how
   * the destructive default got dangerous in the first place.
   *
   * ⚠️ Must run AFTER the save. Staleness is "does the SELECTED take's prompt still match the
   * document", and the document it compares against is the saved one — ask before saving and the
   * edit the user just made is invisible, so their changed shot is reported as up to date.
   */
  async function askRenderScope(
    tier: 'proxy' | 'final'
  ): Promise<'stale' | 'all' | 'rebuild' | null> {
    if (!caps.scopedFullRender || !projectId) return 'all';
    let plan: DirectorRenderPlan;
    try {
      plan = await getDirectorRenderPlan(projectId, tier);
    } catch {
      // Never block a render on the preview of it. The old behaviour is the safe fallback here in
      // the sense that matters — it is what the button has always done.
      return 'all';
    }
    if (!plan.supported || (plan.existing_takes ?? 0) === 0) return 'all';
    return await new Promise((resolve) => {
      scopePrompt = { plan, resolve };
    });
  }

  async function render() {
    revealH3Issues = true;
    if (staleBlocksRender()) return;
    const prompts = clips.map((c) => c.prompt.trim()).filter(Boolean);
    // `nothingToRender` rather than the prompt count: on a structured-prompt engine the prose is in
    // the block and this list is empty for a perfectly renderable project.
    if (clips.length === 0 || nothingToRender) {
      renderError = `Add at least one ${vocab.renderUnit} with a prompt.`;
      return;
    }
    if (!confirmLargeOutput()) return;
    if (!confirmDailiesReview(renderUnits)) return;

    // 🔴 H3 settles the scope BEFORE anything below is touched, and the ordering is forced from
    // both sides. It has to come AFTER the save, because staleness compares the document against
    // each take's stored prompt and an unsaved edit would read as up-to-date. It has to come BEFORE
    // the reset, because that clears the strip and the cut — and a user who picks Cancel must be
    // left exactly where they were, not looking at an emptied editor.
    let h3Scope: 'all' | 'stale' = 'all';
    let alreadySaved = false;
    if (caps.scopedFullRender) {
      try {
        await saveProject();
        alreadySaved = true;
      } catch (e) {
        renderError = (e as Error).message;
        return;
      }
      const chosen = await askRenderScope('final');
      if (!chosen) return;
      // "Combine, don't render" — the whole point of offering it here. Nothing is dispatched and
      // the editor never enters the rendering state.
      if (chosen === 'rebuild') {
        await rebuildCutOnly('final');
        return;
      }
      h3Scope = chosen;
    }

    renderShots = Math.max(1, renderUnits);
    renderError = null;
    queueAccepted = null;
    activeRenderTier = 'final';
    // 🔴 A continue keeps the strip and the cut, for the same reason a retake does: the generations
    // it is not re-rendering are still on disk and still correct, and blanking them would erase the
    // surviving work from the screen at the very moment the user chose to preserve it. It also
    // produces no stitched cut of its own — a partial run is not harvested as one — so clearing
    // `resultUrl` would leave nothing to put back.
    // 🔴 One definition, two call sites, because WHEN it is safe to run depends on the engine. It
    // blanks the cut and the strip, which is only correct for a render that replaces both.
    const wipeFinalTier = () => {
      resultUrl = null;
      clearPartials('final');
    };
    if (h3Scope === 'stale') {
      // 🔴 A continue is a PARTIAL render, and this flag is what makes the completion treat it as
      // one. Two things go wrong without it, both silently: the span's container gets adopted as
      // the finished cut — so "the cut" becomes the two shots that were re-rendered — and
      // `hydrateAfterHarvest` never re-joins the takes, leaving the real cut stale for good.
      // The harvest skips the stitched `shot_index = -1` row for a partial render, so a rebuild is
      // the only thing that produces one, and nothing else would call it.
      pendingPartialRender = true;
    } else if (caps.structuredPrompt) {
      // Dispatched from the saved document below, and this engine never resumes — so whether the
      // tier is replaced is already settled here.
      wipeFinalTier();
    }
    running = true;
    renderProjectId = projectId;
    renderEngine = engine;
    startProgress();
    try {
      // Persist the render into the project's `final` tier — save first so a project id exists to tag
      // it with (a full render replaces the tier's set: wipe = true).
      if (!alreadySaved) await saveProject();
      // 🔴 H3 dispatches from the SAVED document through `/run`, never through the compiled-body
      // transport below — see `runH3Project`. Branch BEFORE compiling: there is nothing to upload
      // (keyframes ride inline on the generation) and the server owns the packing.
      if (engine === 'h3') {
        await runH3Project('final', h3Scope);
        pendingRenderPrompts = [...prompts];
        return;
      }
      const { request, segImageFiles, segV2vFiles } = await compileForRender();
      // compile() sets resume_job_id when "continue from last" is on (append new tail shots). Keep
      // that as the stable resume target, and don't wipe the persisted set on a continue — only a
      // fresh full render replaces it.
      const resumeUsed = ltxRequestOf(request)?.resume_job_id ?? null;
      pendingFinalResumeTarget = resumeUsed;
      // 🔴 The wipe waits for THIS, and it is the only place the answer exists. A continue keeps
      // the shots it is not re-rendering — they are still on disk and still correct — exactly as a
      // retake does; blanking the cut before compiling erased them from the screen at the moment
      // the author chose to preserve them, and a continue produces no stitched cut of its own to
      // put back.
      if (resumeUsed) {
        // A continue on an engine that does not splice delivers the NEW TAIL, not the whole cut —
        // so the fragment must not be adopted as `resultUrl`, and the cut has to be re-joined from
        // the takes afterwards. On a splicing engine the job's container really is the timeline.
        if (partialRenderIsFragment) pendingPartialRender = true;
      } else {
        wipeFinalTier();
      }
      const audio = audioKind === 'voice' || audioKind === 'mixed' ? audioFile : null;
      const bgm = audioKind === 'bgm' ? audioFile : null;
      const { job_id, status } = await submitLtxVideo(
        request,
        null,
        audio,
        bgm,
        segImageFiles,
        segV2vFiles,
        projectId
          ? {
              projectId,
              tier: 'final',
              wipe: !resumeUsed,
              // Full run only. A retake must never carry this, or the critic's own re-rolls would
              // chain into another review.
              critique: critiqueEnabled
            }
          : null,
        // Which LTX service. Omitted for 2.3, so its request is unchanged.
        engine,
        queueOpts
      );
      queueAccepted = ltxReceipt(status);
      // On success this becomes the new banked prefix (all rendered non-empty prompts).
      pendingRenderPrompts = [...prompts];
      rememberActiveJob(job_id, 'final');
      watchJob(job_id, 'final');
    } catch (e) {
      renderError = (e as Error).message;
      // Nothing was dispatched, so nothing will clear the flag on completion.
      pendingPartialRender = false;
      running = false;
      stopTimers();
    }
  }

  /**
   * Render ONE generation on MiniMax-H3 — the engine's smallest unit — through `/retake`.
   *
   * 🔴 A generation cannot be rendered by halves. Its cuts are `[Shot N]` lines inside one prompt,
   * so the engine has no way to stop at shot 2 of 3: the request either contains that generation
   * or it does not. "Render this shot" therefore means rendering the generation that HOLDS it, and
   * the extra shots that come along with it are not a bug — they are the render unit.
   *
   * 🔴 And there is no prefix render either. The two units the service exposes are the whole
   * project (`/run`) and one generation (`/retake`); nothing renders generations 0..N. So this is
   * what "render up to this shot" collapses to, and it is cheaper than the alternative rather than
   * worse — the earlier generations are already on disk and are not re-rendered.
   *
   * 🔴 No `resume_job_id`: there is no banked prefix to splice on this engine, and
   * `retake_segment_h3` rejects a non-empty one with a 400 rather than ignoring it. The server
   * resolves this generation's continuation out of the latent bank itself, from the PREVIOUS
   * generation's selected take.
   *
   * `/retake` renders from the SAVED document, so the current edits are persisted first and the
   * request is a few hundred bytes — the same reason the proxy path uses `/run`.
   */
  async function runH3Generation(rawIdx: number, seed: number, tier: 'proxy' | 'final' = 'final') {
    revealH3Issues = true;
    if (staleBlocksRender()) return;
    if (rawIdx < 0 || !clips[rawIdx]?.prompt.trim()) {
      renderError = 'This shot has no prompt to render.';
      return;
    }
    if (tier === 'final' && !confirmLargeOutput()) return;
    // The queue readout counts render units, and on H3 that is generations — this is exactly one.
    renderShots = 1;
    renderError = null;
    queueAccepted = null;
    // 🔴 Neither the cut nor the other generations' clips are cleared. This render replaces ONE
    // generation and every other one stays exactly where it was on disk — clearing them would
    // erase the eleven surviving generations from the strip at the very moment the user is
    // rendering the twelfth, which is the recovery this path exists for. `pendingPartialRender`
    // then keeps the completion from adopting this generation's container as the cut, and makes
    // `hydrateAfterHarvest` re-join the takes instead.
    pendingPartialRender = true;
    activeRenderTier = tier;
    running = true;
    renderProjectId = projectId;
    renderEngine = engine;
    startProgress();
    // No bank on this engine, so nothing is resumed and completion adopts the job's own id.
    if (tier === 'proxy') pendingProxyResumeTarget = null;
    else pendingFinalResumeTarget = null;
    try {
      // The server re-compiles from the saved timeline, so persist the overrides first.
      await saveProject();
      if (!projectId) throw new Error('save the project before rendering');
      // The save may have discovered the conflict only now. The server re-rolls from the SAVED
      // timeline, so going on would render somebody else's shot.
      if (staleDoc) throw new Error('This project changed elsewhere — reload or overwrite first.');
      const { job_id, version, status } = await retakeDirectorSegment(
        projectId,
        {
          segment: rawIdx,
          seed,
          tier
        },
        queueOpts
      );
      queueAccepted = ltxReceipt(status);
      // A persisting retake rewrites the saved timeline (it stamps the seed), so it bumps the
      // version. Adopt it or the very next autosave would conflict with a write we asked for.
      projectVersion = version;
      pendingRenderPrompts = [...(tier === 'proxy' ? proxyLastPrompts : lastRenderPrompts)];
      rememberActiveJob(job_id, tier);
      watchJob(job_id, tier);
    } catch (e) {
      // Same CAS handling as the LTX retake: nothing was dispatched on a conflict, so surface it
      // as the conflict it is rather than as a render error.
      const serverVersion = conflictVersion(e);
      if (serverVersion !== null) {
        markStale(serverVersion, true);
        renderError = 'This project changed elsewhere — reload it or overwrite that copy first.';
      } else {
        renderError = (e as Error).message;
      }
      // Nothing was dispatched, so nothing will clear the flag on completion.
      pendingPartialRender = false;
      running = false;
      stopTimers();
    }
  }

  /**
   * Render the WHOLE project on MiniMax-H3, at either tier, through `/run`.
   *
   * 🔴 `/run` is the only server path that dispatches a full H3 chain.
   * `/api/v1/ltx-video/generate` parses its body STRICTLY as the LTX shape — `segments` is a
   * required field (`koblibs types.rs:3550`) and unknown keys are rejected — so an
   * `H3GenerateRequest` 400s there before `run_h3_job` is ever reached. `/run` renders from the
   * saved document instead, and `routes::ltx_director::run_project` branches on the project's
   * engine, which is the routing the client cannot do for itself.
   *
   * The caller owns `running` / `startProgress` / the tier bookkeeping; this is only the dispatch.
   */
  async function runH3Project(tier: 'proxy' | 'final', scope: 'all' | 'stale' = 'all') {
    if (!projectId) throw new Error('save the project before rendering');
    if (staleDoc) throw new Error('This project changed elsewhere — reload or overwrite first.');
    // The queue readout counts render units, and on H3 that is generations, not shots.
    renderShots = Math.max(1, packedGenerations.length);
    // A fresh full pass: nothing is resumed, so completion adopts the new job's own id.
    if (tier === 'proxy') pendingProxyResumeTarget = null;
    else pendingFinalResumeTarget = null;
    const { job_id, rendered, status } = await runDirectorProject(
      projectId,
      tier,
      scope,
      queueOpts
    );
    // 🔴 A continue can find nothing to do, and the server says so with a 200 and NO job id rather
    // than dispatching an empty render. Watching `undefined` would hang the editor on a job that
    // does not exist.
    if (!job_id) {
      running = false;
      stopTimers();
      renderError = 'Everything is already rendered — nothing to continue.';
      return;
    }
    queueAccepted = ltxReceipt(status);
    // A continue renders a span, so the queue readout is its length, not the whole timeline's.
    if (rendered && rendered > 0) renderShots = rendered;
    rememberActiveJob(job_id, tier);
    watchJob(job_id, tier);
    // ⚠️ No plan-echo read here, deliberately. koblem CAPTURES the echo from the engine's 202 but
    // STAMPS it only when the job's task finishes (`stamp_h3_plan` runs after `run_h3_job` returns,
    // on the failure path too) — so at dispatch the row still holds the PREVIOUS run's plan, and
    // re-reading it now would show a stale echo under a live render. `finishJob` is the first
    // moment the stamp exists.
  }

  /**
   * Fill ONE hole a cancelled or crashed run left — the recovery move for a long cut.
   *
   * 🔴 The whole reason this is worth a button: a four-minute H3 cut is ~16 generations in ONE
   * ~8-hour engine job, and the job is not resumable. What IS recoverable is everything that
   * finished: the shim reports each generation the moment it lands, koblem's poll loop drains
   * `partials[]` before it looks at the terminal status (so a `cancelled` job is harvested too),
   * the rows carry each generation's latent-bank key, and the stitch orders selected takes by shot
   * index across job ids. So "died at 12 of 16" is genuinely "render 12, 13, 14, 15" — four
   * retakes, each pinning to the previous generation's banked take, and then the cut re-joins.
   *
   * ⚠️ Sequential by necessity, not by choice: generation N's continuation is resolved from N-1's
   * SELECTED take, so N-1 must be on disk before N is dispatched. Rendering them out of order
   * costs the seam, and the engine runs one job at a time anyway.
   *
   * `null` on LTX, and passed as null rather than hidden: LTX renders a PREFIX, so a hole there is
   * filled by "render through" that shot, not by a unit render.
   */
  const renderMissingUnit = $derived(
    caps.generationIsRetakeUnit
      ? (unitIdx: number) => {
          const raw = packedGenerations[unitIdx]?.shots[0]?.rawIndex ?? -1;
          if (raw < 0) {
            renderError = `that ${vocab.renderUnit} is no longer in the timeline — re-open the project.`;
            return;
          }
          // -1 seed = engine-random, the same as any other fresh roll of a generation.
          void runH3Generation(raw, -1, 'final');
        }
      : null
  );

  // Incremental render: render shots [0..=rawIdx] and bank the result. When the prior render still
  // holds the shots before this one, resume from it so the engine SKIPS them and renders only this
  // new tail shot — the write→render→plan-next loop without a full-timeline render. Rerolling an
  // already-rendered shot goes through retake instead (see renderCurrent).
  async function renderThrough(rawIdx: number) {
    // 🔴 "Render through shot K" is an LTX move — it depends on a banked prefix to skip the shots
    // before K. H3 has no bank and no prefix render: the units are the whole project or ONE
    // generation. So this collapses to re-rendering the generation that CONTAINS K, which is both
    // the smallest thing the engine can express and the cheapest. Delegated whole, before any
    // render state is touched, so the guards and the confirm run exactly once.
    if (engine === 'h3') {
      await runH3Generation(rawIdx, -1, 'final');
      return;
    }
    if (staleBlocksRender()) return;
    if (rawIdx < 0 || !clips[rawIdx]?.prompt.trim()) {
      renderError = 'This shot has no prompt to render.';
      return;
    }
    if (!confirmLargeOutput()) return;
    renderError = null;
    queueAccepted = null;
    resultUrl = null;
    activeRenderTier = 'final';
    clearPartials('final');
    running = true;
    renderProjectId = projectId;
    renderEngine = engine;
    startProgress();
    try {
      await saveProject();
      const { request, segImageFiles, segV2vFiles } = await compileForRender();
      // Narrows the union for everything below, and is a real assertion: the H3 arm returned at
      // the top, so an H3 body still here means the engine flag and the compiled document disagree.
      if (isH3Request(request)) throw new Error('this project compiled for H3 — reload it');
      const segments = request.segments.slice(0, rawIdx + 1);
      const prompts = segments.map((s) => s.prompt.trim()).filter(Boolean);
      if (prompts.length === 0) throw new Error('No prompted shot at or before this one.');
      // Resume only when the banked prefix still matches the shots before this one (so it's a true
      // append). Otherwise render [0..=rawIdx] fresh. resume is the ENGINE dir id (finalResumeJobId).
      //
      // `canResume` first: with no reachable bank there is no prefix to reload and the whole
      // [0..=rawIdx] span renders. That is what this button means there — it just is not cheap.
      const resume =
        canResume &&
        finalResumeJobId &&
        !continueMisaligned &&
        lastRenderPrompts.length < prompts.length
          ? finalResumeJobId
          : undefined;
      pendingFinalResumeTarget = resume ?? null;
      // On an engine that does not splice, the job's container is the RESUMED TAIL and nothing
      // else — adopting it would put those two shots on screen as the whole cut. `hydrateAfterHarvest`
      // re-joins the takes instead, which is the only thing that produces a cut for such a render.
      if (resume && partialRenderIsFragment) pendingPartialRender = true;
      // Only the shots past the banked prefix actually render; the prefix is reloaded fast.
      renderShots = resume
        ? Math.max(1, prompts.length - lastRenderPrompts.length)
        : prompts.length;
      const req: LtxVideoGenerateRequest = {
        ...request,
        segments,
        resume_job_id: resume
      };
      const audio = audioKind === 'voice' || audioKind === 'mixed' ? audioFile : null;
      const bgm = audioKind === 'bgm' ? audioFile : null;
      const { job_id, status } = await submitLtxVideo(
        req,
        null,
        audio,
        bgm,
        segImageFiles.slice(0, rawIdx + 1),
        segV2vFiles.slice(0, rawIdx + 1),
        // Through-K render updates the final tier in place (wipe = false) so earlier locked shots stay.
        projectId ? { projectId, tier: 'final', wipe: false } : null,
        engine,
        queueOpts
      );
      queueAccepted = ltxReceipt(status);
      pendingRenderPrompts = [...prompts];
      rememberActiveJob(job_id, 'final');
      watchJob(job_id, 'final');
    } catch (e) {
      renderError = (e as Error).message;
      // Nothing was dispatched, so nothing will clear the flag on completion.
      pendingPartialRender = false;
      running = false;
      stopTimers();
    }
  }

  // "Render this shot": append-render up to the selected shot if it hasn't rendered yet, else reroll
  // it (retake, using the inspector seed). Powers the incremental write→render→plan-next→render loop.
  //
  // On the proxy tier the same two moves apply to the proxy chain — and the re-roll half costs the
  // client nothing to send, which is why it is tried first on a narrow connection.
  async function renderCurrent() {
    const clip = selectedClip;
    if (!clip) return;
    const rawIdx = clips.findIndex((c) => c.id === clip.id);
    // 🔴 Can this deployment re-roll ONE unit at all? Two different ways to be able to: an LTX
    // chain splices a re-render back into its banked one, H3 renders the one generation on its own.
    // With neither — LTX-2.5 whose bank directory is unset — the only move left is the
    // append-render: this shot's span rendered again, which IS the re-roll there, at the cost the
    // engine actually charges. Falling through to the retake instead would send a `retake_from` the
    // engine answers 501 to, leaving this button dead on every shot that has already rendered.
    if (renderTier === 'proxy') {
      if (proxyStatus(clip) === 'none' || !canRetakeUnit) await previewThrough(rawIdx);
      else await retakeSegment(rawIdx, -1, 'proxy');
      return;
    }
    if (shotStatus(clip) === 'none' || !canRetakeUnit) {
      await renderThrough(rawIdx);
    } else {
      await retakeCurrent();
    }
  }

  /**
   * The app bar's primary button. On the final tier this is the full render; on the proxy tier it is
   * a whole-timeline preview dispatched through `/run?tier=proxy`.
   *
   * `/run` is the tier's mobile path on purpose: the editor's own preview compiles client-side and
   * ships every scene still, v2v source and the audio track back up as multipart, which is exactly
   * the upload a phone cannot afford. `/run` renders from the saved document instead, so the request
   * is a few hundred bytes, and it bootstraps 512×288/8-step geometry for a project that has never
   * previewed. It is always a fresh full pass (no resume, wipes the tier) — which is what "preview
   * the whole thing" means anyway.
   */
  async function renderPrimary() {
    revealH3Issues = true;
    if (renderTier !== 'proxy') {
      await render();
      return;
    }
    if (staleBlocksRender()) return;
    const prompts = clips.map((c) => c.prompt.trim()).filter(Boolean);
    if (clips.length === 0 || nothingToRender) {
      renderError = `Add at least one ${vocab.renderUnit} with a prompt.`;
      return;
    }
    // 🔴 "Preview all" wipes the PROXY tier exactly as "Render all" wipes the final one — same
    // `/run`, same `wipe = true`, just a different tier — so it gets the same question. Skipping it
    // here because proxy takes are cheap would still mean a button silently deleting work the user
    // spent time selecting between.
    let proxyScope: 'all' | 'stale' = 'all';
    let alreadySaved = false;
    if (caps.scopedFullRender) {
      try {
        await saveProject();
        alreadySaved = true;
      } catch (e) {
        renderError = (e as Error).message;
        return;
      }
      const chosen = await askRenderScope('proxy');
      if (!chosen) return;
      if (chosen === 'rebuild') {
        await rebuildCutOnly('proxy');
        return;
      }
      proxyScope = chosen;
    }
    // The queue readout counts RENDER UNITS, which is not the prompted-shot count on every engine —
    // see `renderUnits`.
    renderShots = Math.max(1, renderUnits);
    renderError = null;
    queueAccepted = null;
    activeRenderTier = 'proxy';
    // A continue keeps the strip and the preview cut — see the matching branch in `render()`.
    if (proxyScope === 'stale') {
      pendingPartialRender = true;
    } else {
      proxyResultUrl = null;
      clearPartials('proxy');
    }
    running = true;
    renderProjectId = projectId;
    renderEngine = engine;
    startProgress();
    // A fresh chain: nothing is resumed, so completion adopts the new job's own banked dir.
    pendingProxyResumeTarget = null;
    try {
      // Already saved above where the scope question needed the persisted document. Keyed on what
      // actually happened rather than on the engine name — `render()` does the same, and the two
      // must not answer "have we saved?" differently.
      if (!alreadySaved) await saveProject();
      if (!projectId) throw new Error('save the project before previewing');
      if (staleDoc) throw new Error('This project changed elsewhere — reload or overwrite first.');
      const { job_id, rendered, status } = await runDirectorProject(
        projectId,
        'proxy',
        proxyScope,
        queueOpts
      );
      if (!job_id) {
        running = false;
        stopTimers();
        renderError = 'Everything is already previewed — nothing to continue.';
        return;
      }
      queueAccepted = ltxReceipt(status);
      if (rendered && rendered > 0) renderShots = rendered;
      pendingRenderPrompts = [...prompts];
      rememberActiveJob(job_id, 'proxy');
      watchJob(job_id, 'proxy');
    } catch (e) {
      renderError = (e as Error).message;
      running = false;
      stopTimers();
    }
  }

  // "Preview this shot" — proxy tier: render [0..=rawIdx] tiny + few-steps but WITH audio + LoRAs so
  // you can eyeball prompt-fidelity / music reactivity / lipsync cheaply, then commit to the full
  // `final` render. Resumes the proxy chain's own banked prefix (independent of the final chain), and
  // persists into the project's `proxy` tier so it survives a reload and increments shot by shot.
  async function previewThrough(rawIdx: number) {
    // No proxy CHAIN on H3: there is no latent bank to resume a preview from, and the render unit
    // is the generation. A preview of "up to this shot" is therefore the same single generation the
    // final render would do, just at the proxy tier's geometry — which `/run`'s server-side
    // `apply_h3_proxy_geometry` applies, and which the old client-compiled path silently did NOT.
    if (engine === 'h3') {
      await runH3Generation(rawIdx, -1, 'proxy');
      return;
    }
    if (staleBlocksRender()) return;
    if (rawIdx < 0 || !clips[rawIdx]?.prompt.trim()) {
      renderError = 'This shot has no prompt to preview.';
      return;
    }
    renderError = null;
    queueAccepted = null;
    proxyResultUrl = null;
    activeRenderTier = 'proxy';
    clearPartials('proxy');
    running = true;
    renderProjectId = projectId;
    renderEngine = engine;
    startProgress();
    try {
      await saveProject();
      const { request, segImageFiles, segV2vFiles } = await compileForRender();
      // Narrows the union for everything below, and is a real assertion: the H3 arm returned at
      // the top, so an H3 body still here means the engine flag and the compiled document disagree.
      if (isH3Request(request)) throw new Error('this project compiled for H3 — reload it');
      const segments = request.segments.slice(0, rawIdx + 1);
      const prompts = segments.map((s) => s.prompt.trim()).filter(Boolean);
      if (prompts.length === 0) throw new Error('No prompted shot at or before this one.');
      // Resume the PROXY banked prefix (not the final one) when it still matches the shots before this.
      // Require a NON-EMPTY known prefix: an empty proxyLastPrompts makes `.every()` vacuously true, so
      // without this guard a restored/stale proxyResumeJobId (rehydrated without its per-shot rows) would
      // be resumed from with no verified matching prefix — appending fresh prompts onto banked latents of
      // unknown content/resolution/model → a garbled preview. Mirrors the final path's continueMisaligned.
      const aligned =
        proxyLastPrompts.length > 0 &&
        proxyLastPrompts.length < prompts.length &&
        proxyLastPrompts.every((p, i) => p === prompts[i]);
      // `canResume` first — with no reachable bank a proxy prefix cannot be reloaded either.
      const resume = canResume && proxyResumeJobId && aligned ? proxyResumeJobId : undefined;
      // Keep the resume target stable across the chain: if we're resuming, the banked dir stays the
      // one we resumed from; only a fresh chain adopts the new job's server id (on completion).
      pendingProxyResumeTarget = resume ?? null;
      // Same fragment rule as the final tier: a resumed preview delivers its tail, not the cut.
      if (resume && partialRenderIsFragment) pendingPartialRender = true;
      const wipe = !resume; // a fresh proxy prefix replaces the tier's set
      renderShots = resume ? Math.max(1, prompts.length - proxyLastPrompts.length) : prompts.length;
      // Proxy config: fixed low res, single pass (no upscale), few steps — but the compiled audio +
      // LoRAs are inherited from `request` untouched, so reactivity/lipsync are exercised.
      const req: LtxVideoGenerateRequest = {
        ...request,
        segments,
        width: snap32(proxyW),
        height: snap32(proxyH),
        dims_are_target: false,
        profile: 'standard',
        upscaler_ratio: 'none',
        steps: proxySteps,
        hires_chain: undefined,
        resume_job_id: resume
      };
      const audio = audioKind === 'voice' || audioKind === 'mixed' ? audioFile : null;
      const bgm = audioKind === 'bgm' ? audioFile : null;
      const { job_id, status } = await submitLtxVideo(
        req,
        null,
        audio,
        bgm,
        segImageFiles.slice(0, rawIdx + 1),
        segV2vFiles.slice(0, rawIdx + 1),
        projectId ? { projectId, tier: 'proxy', wipe } : null,
        engine,
        queueOpts
      );
      queueAccepted = ltxReceipt(status);
      pendingRenderPrompts = [...prompts];
      rememberActiveJob(job_id, 'proxy');
      watchJob(job_id, 'proxy');
    } catch (e) {
      renderError = (e as Error).message;
      // The banked prefix is NOT dropped here. Most of what lands in this catch never reached the
      // engine — a save conflict, a 500 from the submit — and throwing the chain away turns a
      // recoverable error into a full re-render of every previewed shot. The one failure that really
      // does invalidate the target (the engine swept the banked dir) shows up as a failed RENDER,
      // and is handled where that is known, in watchJob.
      //
      // Nothing was dispatched, so nothing will clear the fragment flag on completion.
      pendingPartialRender = false;
      running = false;
      stopTimers();
    }
  }

  // "Preview this shot" button: proxy-render up to the selected shot.
  async function previewCurrent() {
    const clip = selectedClip;
    if (!clip) return;
    await previewThrough(clips.findIndex((c) => c.id === clip.id));
  }

  // The old "Quick preview · first N" button — now a proxy pass over the first N prompted shots (with
  // audio, persisted). Equivalent to previewing through the Nth prompted shot.
  async function quickPreview() {
    let end = -1;
    let count = 0;
    for (let i = 0; i < clips.length; i++) {
      if (clips[i].prompt.trim()) {
        count++;
        end = i;
        if (count >= previewShots) break;
      }
    }
    if (end < 0) {
      renderError = 'Add at least one shot with a prompt to preview.';
      return;
    }
    await previewThrough(end);
  }

  // Re-render ONE shot (by its RAW timeline index) with the given seed, reusing the banked prefix.
  // Shared by "Retake last shot" and the per-clip inspector retake. Requires a saved project + a
  // completed render OF THAT TIER. `seed = -1` rolls a fresh random seed.
  //
  // The two tiers are separate chains with separate banked dirs, so everything here is picked by
  // tier — the resume target, the stream the partials land in, and the prompt baseline. A proxy
  // retake is also the one re-roll a phone can start without uploading a byte: the server rebuilds
  // the request from the saved document and the tier's recorded geometry.
  async function retakeSegment(rawIdx: number, seed: number, tier: 'proxy' | 'final' = 'final') {
    // 🔴 H3 takes neither of this function's two LTX assumptions. There is no banked prefix, so
    // there is no `resume` to require — and `retake_segment_h3` 400s on a non-empty
    // `resume_job_id` rather than ignoring it, which is what made every H3 retake fail. The server
    // pins the generation to its predecessor's banked take on its own.
    if (engine === 'h3') {
      await runH3Generation(rawIdx, seed, tier);
      return;
    }
    // 🔴 A retake reaches the engine as `retake_from`, which needs a bank to reach INTO — and on
    // LTX-2.5 that bank is a directory the deployment configures, so the same build answers 501
    // without one. Unlike H3 there is no single-unit render to fall back to. Said here, before any
    // render state is touched, so the answer is the reason rather than a 501 arriving on the job
    // after the UI has already gone into "rendering".
    if (!canResume) {
      renderError = `${caps.label} has no latent bank to reach into here, so a single shot cannot be re-rolled into the existing cut — render the ${tier === 'proxy' ? 'preview' : 'timeline'} again instead.`;
      return;
    }
    if (staleBlocksRender()) return;
    if (rawIdx < 0) {
      renderError = 'No shot to retake.';
      return;
    }
    const resume = tier === 'proxy' ? proxyResumeJobId : finalResumeJobId;
    if (!resume) {
      renderError =
        tier === 'proxy'
          ? 'Preview the timeline once before re-rolling a preview shot.'
          : 'Render the timeline once before retaking a shot.';
      return;
    }
    if (tier === 'final' && !confirmLargeOutput()) return;
    renderShots = 1;
    renderError = null;
    queueAccepted = null;
    // 🔴 An engine that SPLICES delivers the whole timeline from a retake, so blanking the cut and
    // the strip costs nothing — the job replaces both. One that does not deliver only the shot it
    // re-rendered, and every other shot is still on disk and still correct: clearing them would
    // erase the surviving cut at the moment the user re-rolled one shot of it, and leave nothing
    // for `hydrateAfterHarvest` to put back but a fifteen-second fragment.
    if (partialRenderIsFragment) {
      pendingPartialRender = true;
    } else {
      if (tier === 'proxy') proxyResultUrl = null;
      else resultUrl = null;
      clearPartials(tier);
    }
    activeRenderTier = tier;
    running = true;
    renderProjectId = projectId;
    renderEngine = engine;
    startProgress();
    // A retake re-renders into the same banked dir, so the resume target stays put.
    if (tier === 'proxy') pendingProxyResumeTarget = resume;
    else pendingFinalResumeTarget = resume;
    try {
      // Retake reads the SAVED timeline — persist current edits first so the shot's prompt matches.
      await saveProject();
      if (!projectId) throw new Error('save the project before retaking');
      // The save may have discovered the conflict only now. The server re-rolls from the SAVED
      // timeline, so going on would retake somebody else's shot.
      if (staleDoc) throw new Error('This project changed elsewhere — reload or overwrite first.');
      const { job_id, version, status } = await retakeDirectorSegment(
        projectId,
        {
          segment: rawIdx,
          seed,
          // The ENGINE dir id — the backend forwards it as resume_job_id (it knows nothing of koblem ids).
          resume_job_id: resume,
          tier
        },
        queueOpts
      );
      queueAccepted = ltxReceipt(status);
      // A persisting retake rewrites the saved timeline (it stamps the seed), so it bumps the
      // version. Adopt it or the very next autosave would conflict with a write we asked for.
      projectVersion = version;
      pendingRenderPrompts = [...(tier === 'proxy' ? proxyLastPrompts : lastRenderPrompts)];
      rememberActiveJob(job_id, tier);
      watchJob(job_id, tier);
    } catch (e) {
      // A retake writes the overrides it is about to render, so it can lose the CAS like any other
      // write — and when it does, nothing was dispatched. Surface it as the conflict it is rather
      // than as a render error, or the user is left with a message and no way to resolve it.
      const serverVersion = conflictVersion(e);
      if (serverVersion !== null) {
        markStale(serverVersion, true);
        renderError = 'This project changed elsewhere — reload it or overwrite that copy first.';
      } else {
        renderError = (e as Error).message;
      }
      // Nothing was dispatched, so nothing will clear the flag on completion.
      pendingPartialRender = false;
      running = false;
      stopTimers();
    }
  }

  // Re-render only the last shot of the last render with a fresh seed, reusing the banked prefix.
  async function retakeLastShot() {
    // Through `unpromptedClips` rather than `clip.prompt`: on a structured-prompt engine the prose
    // is in the block, and reading the seed field left this answering "nothing to retake" on a
    // project whose every shot had rendered.
    const blank = new Set(unpromptedClips.map((c) => c.id));
    const rawIdx = clips.reduce((last, c, i) => (blank.has(c.id) ? last : i), -1);
    if (rawIdx < 0) {
      renderError = `No ${vocab.renderUnit} with a prompt to retake.`;
      return;
    }
    await retakeSegment(rawIdx, -1, renderTier);
  }

  // Re-render the currently-selected inspector clip by its raw timeline index, with the seed the
  // inspector supplied (-1 = fresh roll). Same single-segment splice as the last-shot retake.
  async function retakeSelectedClip(seed: number) {
    const clip = selectedClip;
    if (!clip) return;
    const rawIdx = clips.findIndex((c) => c.id === clip.id);
    await retakeSegment(rawIdx, seed, renderTier);
  }

  // Set when a render was adopted without knowing when it started, so the first poll's
  // `created_at` rebases the timer once instead of the readout counting from the reload.
  let rebaseElapsedOnPoll = false;

  // `baseSec` is how long the render has ALREADY been going — non-zero only when adopting one
  // started elsewhere, where counting from the attach would under-report for the rest of the run.
  // Counts off wall-clock rather than accumulating ticks, so a throttled background tab that
  // stops firing the interval still reports the render's true age when it comes back.
  function startProgress(baseSec = 0) {
    if (progressTimer) clearInterval(progressTimer);
    elapsed = baseSec;
    if (destroyed) return;
    const startedAt = Date.now();
    progressTimer = setInterval(() => {
      elapsed = baseSec + (Date.now() - startedAt) / 1000;
    }, 250);
  }

  // Revoke + drop the live per-shot previews for one tier (called when a new render of it starts).
  function clearPartials(tier: 'proxy' | 'final' = 'final') {
    // Clearing a tier's clips is what a WHOLE-CUT render does — its output replaces the set. So it
    // is also the honest place to drop the "this render covers one unit" flag: every full-cut
    // dispatcher calls this, and the single-generation path deliberately calls neither.
    pendingPartialRender = false;
    if (tier === 'proxy') {
      for (const p of proxyShots) URL.revokeObjectURL(p.url);
      proxyShots = [];
    } else {
      for (const p of partialShots) URL.revokeObjectURL(p.url);
      partialShots = [];
    }
  }

  // Poll a render job to completion, routing its progressive shots + stitched result into the tier
  // that dispatched it (proxy → the preview mirrors; final → the main result + banked v2v source).
  async function watchJob(id: string, tier: 'proxy' | 'final' = 'final') {
    watchedJobs.add(id);
    let fails = 0;
    const poll = async () => {
      // A poll chain cannot be cancelled mid-await, so it re-arms itself after teardown unless it
      // checks: the page is gone, and anything it did from here would mint object URLs nothing can
      // revoke and timers nothing can clear.
      if (destroyed) return;
      // The render outlives the project that started it: keep polling to completion (so the engine
      // frees up and the button unblocks) but route NOTHING into the editor while a different
      // project is open — its shots, resume ids and autosave are not ours to overwrite. The server
      // harvests the render into its own project's director_shot_renders regardless, so switching
      // back and re-hydrating recovers everything.
      const mine = renderProjectId === projectId;
      try {
        const st = await getLtxJob(id);
        fails = 0;
        // H3 render telemetry, for the render THIS project owns only. A foreign render's
        // "generation 3 of 4" under our stage would read as ours; absent is the honest answer.
        jobTelemetryRaw = mine ? st : null;
        if (mine) {
          // 🔴 The ENGINE's own account of what `/media` is, folded onto the dispatch-time guess
          // and unable to clear it — see `mediaIsFragment`. This is the only thing that catches a
          // FULL render whose concatenation failed: the guess says "not a fragment" there, because
          // that is a runtime failure and not a property of the engine. Latched on EVERY poll
          // rather than the terminal one, since ltx25 echoes its render window from the first.
          pendingPartialRender = mediaIsFragment(pendingPartialRender, st.job_is_partial);
          // 🔴 The SEQUENCE is the signal. The preview URL is constant for the life of the job, so
          // the only way to know the bytes changed is that this number moved. Gated on the field
          // being present rather than on our own toggle: a render adopted from another device may
          // have asked for previews when this tab did not, and a 404 costs nothing anyway.
          void syncLivePreview(id, st.preview_seq);
        }
        if (rebaseElapsedOnPoll) {
          rebaseElapsedOnPoll = false;
          const age = adoptedElapsedSec(st.created_at, Date.now());
          if (age > 0) startProgress(age);
        }
        // Progressive delivery: fetch each newly-finished shot and show it live, before the
        // stitched result. Best-effort — a fetch failure just skips that shot's preview.
        if (mine && st.partials?.length) {
          for (const p of st.partials) {
            const have = tier === 'proxy' ? proxyShots : partialShots;
            if (have.some((x) => x.index === p.segment_index)) continue;
            try {
              const url = await fetchLtxClip(p.url);
              if (destroyed) {
                URL.revokeObjectURL(url);
                return;
              }
              // Tagged with the render + shot it IS, so the re-read after this render keeps it
              // instead of downloading the identical bytes the harvest just persisted.
              const next = [
                ...(tier === 'proxy' ? proxyShots : partialShots),
                { index: p.segment_index, url, key: jobClipKey(id, p.segment_index) }
              ].sort((a, b) => a.index - b.index);
              if (tier === 'proxy') proxyShots = next;
              else partialShots = next;
            } catch {
              /* best-effort preview */
            }
          }
        }
        if (st.status === 'completed' && st.result_url) {
          if (!mine) {
            finishJob();
            return;
          }
          // This job's container is a FRAGMENT — one render unit, or (when the engine's
          // concatenation failed) segment 0 on its own. Either way it is not the cut and must not
          // be labelled as one, so leave the cut alone here: `hydrateAfterHarvest` re-joins it from
          // the selected takes and `hydratePersistedRenders` pulls the rebuilt one down.
          if (pendingPartialRender) {
            finishJob();
            historyKey += 1;
            return;
          }
          const url = await fetchLtxClip(st.result_url);
          if (destroyed) {
            URL.revokeObjectURL(url);
            return;
          }
          // 🔴 Whether this render's ids may be adopted AT ALL is decided by the engine it was
          // DISPATCHED to, not by the one the project is on now — the picker moves during a render.
          // See `adoptCompletedBank`; the bytes above are still ours either way (they are this
          // project's cut, and `resultKey` names a koblem job, not an engine store).
          const bank = adoptCompletedBank({
            dispatchEngine: renderEngine,
            projectEngine: engine,
            pending: tier === 'proxy' ? pendingProxyResumeTarget : pendingFinalResumeTarget,
            banked: st.server_job_id,
            held: tier === 'proxy' ? proxyResumeJobId : finalResumeJobId
          });
          if (tier === 'proxy') {
            proxyResultUrl = url;
            proxyResultKey = jobClipKey(id, -1);
            // Bank the ENGINE server_job_id (the dir the banked prefix lives in) as the resume
            // target — keeping the one we resumed from if this was a resume, else adopting the fresh
            // chain's id. Persist it (+ the aligned prompts) so previewing stays incremental across a
            // reload instead of re-rendering the whole prefix. Either id would work (see the
            // declaration of proxyResumeJobId); this one is the bank itself, so it stays readable.
            proxyResumeJobId = bank.resumeJobId;
            if (bank.adopt) {
              proxyLastPrompts = [...pendingRenderPrompts];
              if (projectId) void saveProject({ quiet: true });
            }
          } else {
            resultUrl = url;
            resultKey = jobClipKey(id, -1);
            // Bank the ENGINE dir id as the stable resume target (keep the one we resumed from on a
            // continue/retake, else adopt the fresh chain's id), and persist it + the banked prefix
            // so "Render new/this shot" stays incremental across a reload.
            finalResumeJobId = bank.resumeJobId;
            if (bank.adopt) {
              lastRenderPrompts = [...pendingRenderPrompts];
              // Offer this finished render as a v2v "edit a video" source. The pickable source id is
              // the engine DIR (finalResumeJobId) — the dir the latents live in — so v2v works even
              // for a continued render and outlives koblem's job TTL. job_id (koblem) is just the
              // dedupe key.
              addPriorRender(
                id,
                finalResumeJobId,
                st.segments ?? clips.filter((c) => c.prompt.trim()).length,
                projectName ? `${projectName} render` : 'last render'
              );
              if (projectId) void saveProject({ quiet: true });
            }
          }
          finishJob();
          historyKey += 1;
          return;
        }
        if (st.status === 'failed' || st.status === 'cancelled') {
          if (!mine) {
            finishJob();
            return;
          }
          renderError = st.error || `render ${st.status}`;
          // A failure on a render that RESUMED is often a stale resume target (the engine swept that
          // banked dir) — drop it so the next render cleanly re-renders the whole prefix instead of
          // 404ing. A fresh (non-resumed) render's failure leaves the target alone.
          //
          // 🔴 Behind the same gate as the completion above, and for the same reason: a render only
          // speaks for the engine it was dispatched to. The paths that move a project's engine
          // WITHOUT the picker — a doc reload or a stale-doc overwrite reinstalling both ids, a
          // harvest installing this engine's id while the previous engine's render still polls —
          // leave a foreign render running against a target that is now legitimately ours, and
          // `pendingFinalResumeTarget` is then truthy precisely when it must not authorise a write.
          if (dispatchIsProjectEngine(renderEngine, engine)) {
            if (tier === 'proxy') proxyResumeJobId = null;
            else if (pendingFinalResumeTarget) finalResumeJobId = null;
          }
          finishJob();
          return;
        }
      } catch {
        if (++fails > 20) {
          if (mine) renderError = 'lost contact with the render job';
          finishJob();
          return;
        }
      }
      if (destroyed) return;
      watchTimer = setTimeout(poll, 1500);
    };
    poll();
  }

  /**
   * Re-fetch the preview frame — but ONLY when the engine says there is a new one.
   *
   * 🔴 `/api/v1/minimax-h3/jobs/{id}/preview` has one URL for the whole render, so it is
   * `preview_seq` and nothing else that says the bytes changed. Fetching on every poll would pull
   * the same PNG at 1.5 s intervals off the box that is running the render; never re-fetching would
   * freeze the first frame for the rest of the hour.
   *
   * 🔴 **A 404 is not an error.** `fetchH3Preview` answers `null` for it, and that is the normal
   * reply from every render that did not ask for previews and from the window before the first
   * emission. Showing a failure there would put a red banner on the majority of renders.
   */
  async function syncLivePreview(jobId: string, seq: number | null | undefined) {
    if (typeof seq !== 'number' || !Number.isFinite(seq)) return;
    // Same frame we are already showing, or one already on the wire — either way, nothing to do.
    if (livePreview?.seq === seq || previewFetching) return;
    previewFetching = true;
    try {
      const url = await fetchH3Preview(jobId);
      if (!url) return;
      // Re-checked AFTER the await: teardown and a project switch both happen mid-flight, and
      // installing the frame then would leak an object URL nothing is left to revoke.
      if (destroyed || renderProjectId !== projectId) {
        URL.revokeObjectURL(url);
        return;
      }
      const stale = livePreview;
      livePreview = { seq, url };
      if (stale) URL.revokeObjectURL(stale.url);
    } catch {
      /* best-effort: a preview that failed to fetch is a missing picture, not a failed render */
    } finally {
      previewFetching = false;
    }
  }

  function clearLivePreview() {
    if (livePreview) URL.revokeObjectURL(livePreview.url);
    livePreview = null;
  }

  function finishJob() {
    const mine = renderProjectId === projectId;
    const id = projectId;
    running = false;
    renderProjectId = null;
    renderEngine = null;
    stopTimers();
    // The render is over: a frozen "denoising · step 14 of 20" beside an idle stage reads as live.
    jobTelemetryRaw = null;
    clearLivePreview();
    localStorage.removeItem(ACTIVE_JOB_KEY);
    // The server banks every Director render into director_shot_renders from the job's own tag, so
    // re-reading them here is what gives a tab that only WATCHED a render (rather than dispatching
    // it) the takes, the prompts as rendered and the tier's resume id — plus the project row's own
    // post-render stamps, which land on the same schedule.
    if (mine && id) void hydrateAfterHarvest(id, { refresh: true });
  }

  /**
   * Re-read this project's SERVER metadata (`h3_plan`, `last_job_id`) without pulling its timeline.
   *
   * The list endpoint's summary carries metadata and omits the heavy document, which is what makes
   * this cheap — `getDirectorProject` would ship every inline asset in the project to learn one
   * key. Best-effort: a missed refresh means the plan panel shows the previous run's echo until the
   * next one, which is visibly stamped with its own time.
   */
  async function refreshProjectMetadata(id: string) {
    try {
      const { items } = await listDirectorProjects();
      if (destroyed || projectId !== id) return;
      projects = items;
      const row = items.find((p) => p.id === id);
      if (row) projectMetadata = (row.metadata as Record<string, unknown>) ?? {};
    } catch {
      /* the panel keeps the previous echo */
    }
  }

  // Wait for a render's output to be PERSISTED, then read the durable rows.
  //
  // The server keeps a finished render listed in `/active` until its harvest lands, so that is the
  // actual guarantee that `/renders` is whole — a full run's harvest replaces the tier's entire set,
  // and reading it mid-swap showed a half-populated project. This used to be a flat 2s sleep after a
  // completion, which is both a guess and, when the harvest is quick, a wait for nothing — and it
  // did nothing at all for the OTHER moment this matters, opening a project on a second device just
  // as the first device's render lands.
  async function hydrateAfterHarvest(id: string, opts: { refresh?: boolean } = {}) {
    for (let i = 0; i < HARVEST_WAIT_TRIES; i++) {
      try {
        if (harvestSettled(await listDirectorActiveJobs(id), id)) break;
      } catch {
        break; // the endpoint is the only thing that could tell us; don't spin on it
      }
      if (destroyed || projectId !== id) return;
      await new Promise((r) => setTimeout(r, HARVEST_POLL_MS));
    }
    if (destroyed || projectId !== id) return;
    // The PLAN ECHO and the DELIVERY WARNINGS live on the project row's metadata, so the only way
    // to see either is to re-read the row.
    //
    // 🔴 Here, and not in `finishJob`, because both are stamped LATER than the render: the plan when
    // the render task ends, and `last_render_warnings` inside the harvest itself. Reading at
    // completion returns the PREVIOUS run's, silently and plausibly — and for a full render whose
    // concatenation failed those warnings are the only prose account of what `/media` actually was.
    //
    // ⚠️ On the TERMINAL path only. `openProject` calls this too, and there the row was just read
    // and `projectMetadata` set from it — so a second, un-awaited listing would race the rest of
    // the open and overwrite `projects` and `projectMetadata` from under it.
    if (opts.refresh) void refreshProjectMetadata(id);
    // 🔴 A render that delivered a FRAGMENT leaves the CUT stale. `harvest_director_renders` skips
    // the stitched `shot_index = -1` row for a partial render (there is only a fragment to file),
    // and `rebuildDirectorCut` had exactly one caller — picking a take. So filling a hole left by a
    // cancelled 16-generation run gave you the generation and never the finished cut, and a full
    // render whose concatenation failed gave you shot 1 called "the cut". Re-join here, after the
    // harvest has settled so the new takes are among the ones being joined.
    if (pendingPartialRender) {
      pendingPartialRender = false;
      try {
        await rebuildDirectorCut(id, activeRenderTier);
      } catch (e) {
        renderError = `rendered, but the cut could not be re-joined: ${(e as Error).message}`;
      }
      if (destroyed || projectId !== id) return;
    }
    await hydratePersistedRenders(id, opts);
    // 🔴 A render that just landed became the SELECTED take of its unit (`upsert` selects what it
    // writes), which is what the unit after it now has to be re-rendered against. Re-reading here is
    // what turns a repair into a sequence: finish shot 4, and the panel is already offering shot 5.
    // Only when the panel is open — an unopened one means nobody asked, and this costs a request per
    // render unit.
    if (joinsLoaded && !destroyed && projectId === id) await loadContinuity();
  }

  // Remember the in-flight job, its tier and the project that owns it, so a reload reconnects to the
  // right stream AND knows whether the project then on screen is entitled to its output.
  function rememberActiveJob(jobId: string, tier: 'proxy' | 'final') {
    // `renderEngine`, not `engine`: this records what the job was DISPATCHED to, and the picker can
    // have moved between the dispatch and the response that lands here.
    localStorage.setItem(
      ACTIVE_JOB_KEY,
      JSON.stringify({ jobId, tier, projectId, engine: renderEngine })
    );
  }

  async function cancelRender() {
    try {
      await cancelLtxRender();
    } catch {
      /* best-effort */
    }
    finishJob();
  }

  // ── Project persistence ──────────────────────────────────────────────────────
  async function refreshProjects() {
    try {
      const { items } = await listDirectorProjects();
      projects = items;
    } catch {
      /* list is best-effort */
    }
  }

  function newProject() {
    cancelPendingRename();
    projectId = null;
    projectName = 'Untitled project';
    savedName = 'Untitled project';
    projectVersion = null;
    staleDoc = null;
    projectMetadata = {};
    fps = 24;
    shotSeconds = 7;
    // A new project starts on LTX-2.5 — what the rest of the editor's defaults (7s shots, the audio
    // sheet) are tuned for. Switching is one control away in Settings.
    engine = 'ltx25';
    h3References = [];
    h3Generations = [];
    model = undefined;
    autoBrief = '';
    qualityProfile = 'standard';
    detailStrength = DETAIL_STRENGTH_DEFAULT;
    upscalerRatio = 'x2';
    proxyW = 512;
    proxyH = 288;
    proxySteps = 8;
    // Off, like every new project: an unmeasured per-emission decode cost is not something a fresh
    // project opts into by inheriting the last one's setting.
    h3Preview = false;
    // A fresh project attaches no adapter and states no step count: the engine's own default is the
    // only honest starting point, and inheriting the last project's turbo pairing would render a
    // new one at 8 steps with nothing on screen saying so.
    h3Loras = [];
    h3LoraMultipliers = [];
    h3Steps = undefined;
    resPreset = 'custom';
    width = RES_PRESETS['1080p'].w;
    height = RES_PRESETS['1080p'].h;
    snapNote = null;
    snapTargetToProfile();
    clips = [];
    characters = [];
    lyrics = [];
    clearAudio();
    resetHistory();
    summary = null;
    summaryStale = false;
    clearReview();
    aiRestore = null;
    // Drop the prior project's rendered previews (both tiers) so a blank project starts empty.
    clearRenderPreviews();
    // Everything naming a bank belongs to the project just closed — and its engine, which need not
    // be the `ltx` this one starts on. Same call the picker and undo take, so a fresh project cannot
    // inherit a chain, a banked prefix or a prior-render offer from the last one.
    dropEngineBankState();
    dirty = false;
  }

  // Revoke + drop every rendered preview blob (final + proxy). Called on a fresh load / new project.
  function clearRenderPreviews() {
    clearPartials('final');
    clearPartials('proxy');
    // The in-flight H3 frame is an object URL like the rest — and it belongs to the render, so
    // switching project or tearing down must drop it rather than show one project's frame in
    // another's stage.
    clearLivePreview();
    jobTelemetryRaw = null;
    if (resultUrl) URL.revokeObjectURL(resultUrl);
    if (proxyResultUrl) URL.revokeObjectURL(proxyResultUrl);
    resultUrl = null;
    resultKey = null;
    proxyResultUrl = null;
    proxyResultKey = null;
  }

  // Rehydrate a project's DURABLE renders: the row metadata for BOTH tiers (per-shot prompts as
  // rendered, and the tier's banked engine chain), then the CLIPS of the tier this client works in.
  // Best-effort: a project with no persisted renders (or a fetch failure) just shows nothing to play.
  //
  // Only one tier's bytes are pulled, because that is what the tier means (`lib/director/tier.ts`):
  // a `final` shot is 1.4–19.5 MB and a 12-shot project is a download a phone cannot afford, which
  // is the entire reason the proxy tier exists. The other tier's clips are fetched if and when the
  // client switches to it.
  //
  // `refresh` marks a re-read of the project already on screen (after a render finishes) rather than
  // a switch into it — the difference decides what a failed fetch is allowed to blank.
  async function hydratePersistedRenders(id: string, opts: { refresh?: boolean } = {}) {
    // Only an in-flight render OF THIS PROJECT outranks the persisted rows — its live partials are
    // fresher. A render belonging to some other project must not suppress our hydration (that left
    // the previous project's clips on screen). Even when it IS ours we drop the previews we arrived
    // with FOR THE TIER IT IS RENDERING: st.partials refills those within a poll, while the other
    // tier's clips would just be blanked with nothing to refill them.
    if (running && renderProjectId === id) {
      clearPartials(activeRenderTier);
      return;
    }
    let items;
    try {
      items = await listDirectorRenders(id);
    } catch {
      // Switching INTO a project must not leave the previous one's clips on screen — but a refresh
      // of the project already shown must not blank the render it just finished either.
      if (!opts.refresh) clearRenderPreviews();
      return;
    }
    if (destroyed || projectId !== id) return; // switched projects (or left) while awaiting
    // Switching INTO a project starts from nothing: the blobs on hand belong to the project we just
    // left, and they are matched by producing render, so nothing here would claim them anyway. A
    // refresh keeps what it holds — that is the point of it.
    if (!opts.refresh) clearRenderPreviews();
    renderRows = items;

    for (const tier of ['final', 'proxy'] as const) {
      const shotRows = items
        .filter((r) => r.tier === tier && r.shot_index >= 0)
        .sort((a, b) => a.shot_index - b.shot_index);

      // Rebuild the tier's banked prefix from its per-shot prompts (index-aligned; a shot whose
      // preview didn't persist leaves an empty slot, which reads as "stale" — acceptable).
      if (shotRows.length) {
        const maxIdx = shotRows[shotRows.length - 1].shot_index;
        const prompts: string[] = new Array(maxIdx + 1).fill('');
        for (const r of shotRows) prompts[r.shot_index] = r.prompt;
        if (tier === 'proxy') proxyLastPrompts = prompts;
        else lastRenderPrompts = prompts;
      }

      // Adopt the tier's banked engine chain from the render rows. Only the tab that dispatched a
      // render ever learned this id from the job status, which is why a retake on a SECOND device
      // used to demand a full re-render first. A null here means the row predates the column — keep
      // whatever we already had rather than downgrading a working resume to a full prefix.
      //
      // 🔴 Shot 0's chain, NOT the newest, and NOT another engine's — see `bankedResumeTarget`.
      // Adopting the newest made a second continue a hard 400 on LTX-2.5, whose bank has no id
      // aliasing; adopting a row banked before an engine switch made every shot of the next render
      // a 404 in a store that has never held it.
      const banked = bankedResumeTarget(items, tier, engine);
      if (tier === 'proxy') proxyResumeJobId = banked ?? proxyResumeJobId;
      else finalResumeJobId = banked ?? finalResumeJobId;
    }

    await hydrateTierClips(id, renderTier);
  }

  // Download whatever of ONE tier's persisted clips this client does not already hold, and revoke
  // whatever it holds that no row backs any more. Idempotent, so switching tiers (or re-reading
  // after a render) costs only the clips that actually changed — a tab that watched the render
  // already holds the exact bytes the harvest persisted.
  async function hydrateTierClips(id: string, tier: RenderTier) {
    const plan = planHydration(renderRows, tier, tier === 'proxy' ? proxyShots : partialShots);
    for (const url of plan.drop) URL.revokeObjectURL(url);
    let held = plan.keep;
    const commit = () => {
      if (tier === 'proxy') proxyShots = [...held].sort((a, b) => a.index - b.index);
      else partialShots = [...held].sort((a, b) => a.index - b.index);
    };
    commit();
    for (const r of plan.fetch) {
      try {
        const url = await fetchLtxClip(r.url);
        if (destroyed || projectId !== id) {
          URL.revokeObjectURL(url);
          return;
        }
        held = [...held, { index: r.shot_index, url, key: clipKey(r) }];
        commit();
      } catch {
        /* best-effort per-shot preview */
      }
    }

    const heldCut =
      tier === 'proxy'
        ? proxyResultUrl && proxyResultKey
          ? { url: proxyResultUrl, key: proxyResultKey }
          : null
        : resultUrl && resultKey
          ? { url: resultUrl, key: resultKey }
          : null;
    const cut = planCutHydration(renderRows, tier, heldCut);
    if (cut.keep) return;
    if (cut.drop) URL.revokeObjectURL(cut.drop);
    if (tier === 'proxy') {
      proxyResultUrl = null;
      proxyResultKey = null;
    } else {
      resultUrl = null;
      resultKey = null;
    }
    if (!cut.fetch) return;
    try {
      const url = await fetchLtxClip(cut.fetch.url);
      if (destroyed || projectId !== id) {
        URL.revokeObjectURL(url);
        return;
      }
      if (tier === 'proxy') {
        proxyResultUrl = url;
        proxyResultKey = clipKey(cut.fetch);
      } else {
        resultUrl = url;
        resultKey = clipKey(cut.fetch);
      }
    } catch {
      /* best-effort stitched playback */
    }
  }

  // Serialise every write to the project row (PUT and PATCH alike) so no two of them can be in
  // flight quoting the same version.
  function queueWrite<T>(run: () => Promise<T>): Promise<T> {
    const next = writeChain.then(run, run);
    writeChain = next.then(
      () => undefined,
      () => undefined
    );
    return next;
  }

  // A stale document cannot be saved, and every render path saves first — the retake path renders
  // the SAVED timeline server-side, so going ahead would re-roll the other device's shot.
  function staleBlocksRender(): boolean {
    if (!staleDoc) return false;
    renderError =
      'This project changed elsewhere — reload it or overwrite that copy before rendering.';
    return true;
  }

  function stopAutosave() {
    if (autosaveTimer) {
      clearTimeout(autosaveTimer);
      autosaveTimer = null;
    }
    if (autosaveRetryTimer) {
      clearTimeout(autosaveRetryTimer);
      autosaveRetryTimer = null;
    }
  }

  // The server refused a write because the row moved on. Nothing here touches the timeline in
  // memory: the user's work stays exactly as they left it, and reloading is a choice they make.
  function markStale(serverVersion: number, userInitiated: boolean) {
    const next = onWriteConflict(
      { dirty, autosaveArmed: !!autosaveTimer, stale: staleDoc },
      serverVersion,
      userInitiated
    );
    dirty = next.dirty;
    staleDoc = next.stale;
    autosaveFailed = false;
    if (!next.autosaveArmed) stopAutosave();
  }

  // Discard OUR copy and take the server's — only ever from an explicit click.
  async function reloadStale() {
    const id = projectId;
    if (!id) return;
    cancelPendingRename();
    staleDoc = null;
    await loadProject(id);
  }

  // Keep our copy and make it the truth, by adopting the version we were told about and writing
  // over it. Also an explicit click: it is the mirror image of reload, and one of them has to lose.
  async function overwriteStale() {
    const version = staleDoc?.serverVersion;
    if (version == null) return;
    projectVersion = version;
    staleDoc = null;
    dirty = true;
    await saveProject();
  }

  async function saveProject(opts: { quiet?: boolean } = {}) {
    if (autosaveTimer) {
      clearTimeout(autosaveTimer);
      autosaveTimer = null;
    }
    // A stale document has no version that can succeed; writing again would just re-conflict.
    if (staleDoc) return;
    saving = true;
    try {
      await queueWrite(async () => {
        const doc = buildTimelineDoc();
        const name = projectName;
        if (projectId) {
          const saved = await updateDirectorProject(
            projectId,
            name,
            doc,
            projectMetadata,
            projectVersion ?? 0
          );
          projectVersion = saved.version;
        } else {
          const created = await createDirectorProject(name, doc);
          projectId = created.id;
          projectVersion = created.version;
          // A first render saves the project to get an id to tag itself with, so the in-flight
          // render was tagged with the pre-save `null`. Adopt the fresh id or it would read as
          // belonging to "another project" and its own output would be discarded.
          if (running && renderProjectId === null) renderProjectId = projectId;
        }
        // The PUT body carries `name`, so a save settles any pending rename too.
        savedName = name;
        cancelPendingRename();
      });
      dirty = false;
      lastSavedAt = Date.now();
      autosaveFailed = false;
      if (autosaveRetryTimer) {
        clearTimeout(autosaveRetryTimer);
        autosaveRetryTimer = null;
      }
      await refreshProjects();
    } catch (e) {
      const serverVersion = conflictVersion(e);
      if (serverVersion !== null) {
        // Autosave conflicts stay passive; only a Save the user pressed earns the question.
        markStale(serverVersion, !opts.quiet);
        return;
      }
      // A failed autosave stays quiet (no error banner) and leaves `dirty` set; it retries on the
      // next edit AND on a standalone backoff timer so it recovers even if the user stops editing.
      if (opts.quiet) {
        autosaveFailed = true;
        if (autosaveRetryTimer) clearTimeout(autosaveRetryTimer);
        autosaveRetryTimer = setTimeout(autosaveNow, AUTOSAVE_RETRY_MS);
      } else {
        renderError = `save failed: ${(e as Error).message}`;
      }
    } finally {
      saving = false;
    }
  }

  // ── Rename ─────────────────────────────────────────────────────────────────
  // The project name is the one field worth persisting on its own: a saved timeline carries inline
  // assets, so routing a rename through the full-document PUT ships megabytes to change a string.
  function cancelPendingRename() {
    if (renameTimer) {
      clearTimeout(renameTimer);
      renameTimer = null;
    }
  }

  function onNameInput() {
    if (renameRoute({ projectId, timelineDirty: dirty }) === 'patch') {
      cancelPendingRename();
      renameTimer = setTimeout(() => void commitRename(), RENAME_MS);
      return;
    }
    // Either there is no row yet (the create carries the name) or a timeline PUT is already due and
    // its body carries the name for free. Both end at the normal save path.
    markDirty();
  }

  /// Persist the name NOW: on blur, on teardown, or when the debounce runs out. Re-checks the route
  /// rather than trusting the one that armed the timer — the timeline can have gone dirty since,
  /// and then the pending PUT carries the name and a PATCH would just be a second write.
  async function commitRename() {
    cancelPendingRename();
    const id = projectId;
    const name = projectName.trim();
    if (!name || name === savedName || staleDoc) return;
    if (!id || renameRoute({ projectId: id, timelineDirty: dirty }) !== 'patch') {
      markDirty();
      return;
    }
    try {
      const summary = await queueWrite(() => patchDirectorProject(id, { name }));
      if (projectId !== id) return; // switched projects while awaiting
      // PATCH bumps the version. Adopting it here is what lets a timeline save queued behind this
      // one still land instead of conflicting with a rename the same user just made.
      projectVersion = summary.version;
      savedName = summary.name;
      lastSavedAt = Date.now();
      await refreshProjects();
    } catch {
      // Fall back to the full save: its body carries the name, and it owns the error reporting.
      markDirty();
    }
  }

  // Duplicate: branch the current project into a fresh scratch copy and switch to it — the original
  // stays saved and untouched in the list. Handy for cutting a copy down to a quick sanity-check
  // render before committing to the full long render.
  async function duplicateProject() {
    if (!projectId || duplicating) return;
    duplicating = true;
    renderError = null;
    try {
      // Persist the original's pending edits first so the branch point is saved; abort if that fails.
      if (dirty) {
        await saveProject();
        if (dirty) return;
      }
      const doc = buildTimelineDoc();
      const copyName = `${projectName} (copy)`;
      const created = await createDirectorProject(copyName, doc);
      cancelPendingRename();
      projectId = created.id;
      projectName = copyName;
      savedName = copyName;
      projectVersion = created.version;
      staleDoc = null;
      projectMetadata = {};
      priorRenders = [];
      lastRenderPrompts = [];
      // The banked engine dirs belong to the original — a copy resumes nothing until it renders.
      finalResumeJobId = null;
      proxyResumeJobId = null;
      proxyLastPrompts = [];
      clearReview();
      dirty = false;
      lastSavedAt = Date.now();
      restoreNote = `Now editing a copy — “${copyName}”. The original is saved and untouched.`;
      await refreshProjects();
    } catch (e) {
      renderError = `duplicate failed: ${(e as Error).message}`;
    } finally {
      duplicating = false;
    }
  }

  // ── Autosave ───────────────────────────────────────────────────────────────
  // Only persist a brand-new (unsaved) project once it has real content, so idle
  // "Untitled project" tabs don't litter the project list with empty rows.
  function hasSaveableContent(): boolean {
    return (
      clips.some((c) => c.prompt.trim() || c.image_b64 || c.v2v_source_b64) ||
      !!audioB64 ||
      autoBrief.trim().length > 0 ||
      projectName.trim() !== 'Untitled project'
    );
  }

  async function autosaveNow() {
    if (autosaveTimer) {
      clearTimeout(autosaveTimer);
      autosaveTimer = null;
    }
    if (staleDoc) return;
    if (renameTimer) await commitRename();
    if (!dirty || saving) return;
    if (!projectId && !hasSaveableContent()) return;
    await saveProject({ quiet: true });
  }

  function scheduleAutosave() {
    if (staleDoc) return;
    if (autosaveTimer) clearTimeout(autosaveTimer);
    autosaveTimer = setTimeout(autosaveNow, AUTOSAVE_MS);
  }

  async function loadProject(id: string) {
    cancelPendingRename();
    try {
      const p = await getDirectorProject(id);
      const t = p.timeline;
      projectId = p.id;
      projectName = p.name;
      savedName = p.name;
      projectVersion = p.version;
      staleDoc = null;
      projectMetadata = (p.metadata as Record<string, unknown>) ?? {};
      summary = null;
      summaryStale = false;
      clearReview();
      loadRestore();
      await adoptTimeline(t);
      resetHistory();
      // Offer the project's last render as a v2v "edit a video" source. The source id is the engine
      // dir (finalResumeJobId, restored from the doc by adoptTimeline above) so it survives koblem's
      // job TTL.
      //
      // 🔴 Offered only while the document still holds that dir id, because the id IS the offer:
      // `ShotDetailsPane` picks `serverJobId ?? job_id`, and the koblem `last_job_id` underneath it
      // carries no engine at all. A switch nulls `finalResumeJobId` and saves, so falling back to
      // the koblem id re-offered the PREVIOUS engine's render one reload after the switch had
      // dropped it — the same foreign id, arrived at by a different road. What makes this a test
      // rather than a guess is that `finalResumeJobId` always names the engine the project is on
      // (`dropEngineBankState`); a project too old to have saved one names nothing, and its koblem
      // job aged out of the TTL long ago anyway.
      priorRenders = [];
      const lastJob = (p.metadata as Record<string, unknown>)?.last_job_id;
      if (typeof lastJob === 'string' && lastJob) {
        if (finalResumeJobId) {
          addPriorRender(
            lastJob,
            finalResumeJobId,
            clips.filter((c) => c.prompt.trim()).length,
            `${p.name} (last render)`
          );
        }
        // Baseline the banked prefix from the saved timeline (assumed to reflect that render), so a
        // later edit flips Continue to misaligned rather than silently reusing stale shots. When
        // durable per-shot renders exist (below) they REPLACE this with the render's own provenance.
        lastRenderPrompts = clips.map((c) => c.prompt.trim()).filter(Boolean);
      } else {
        lastRenderPrompts = [];
      }
      // Rehydrate DURABLE renders: the stitched final into the player, each per-shot clip into the
      // strip, and — when per-shot clips exist — rebuild lastRenderPrompts from the prompts AS
      // RENDERED so shot status reflects the actual render, not the timeline-matches assumption.
      await hydrateAfterHarvest(p.id);
      // A render of this project may be in flight on another device — the job store is the only
      // place that knows, so ask before assuming the persisted rows are the whole story.
      await adoptServerActive(p.id);
      // Hydrate the audio FILE and waveform. `audioB64` itself was already set by `adoptTimeline`
      // above, synchronously with the reference list — see the comment there. This block used to be
      // its first writer, and the awaits in between are what deleted the song.
      const ab = t.assets?.audio_b64 ?? t.assets?.bgm_b64 ?? null;
      if (ab) {
        audioB64 = ab;
        audioFile = new File([await fetchAssetBlob(ab)], 'audio.wav');
        try {
          const { samples, sampleRate } = await blobToMonoSamples(audioFile);
          waveform = samples;
          waveDuration = samples.length / sampleRate;
        } catch {
          waveform = null;
        }
      } else {
        clearAudioSilent();
      }
      // Warm every asset the project references in one pass, so its stills appear together rather
      // than trickling in as each <img> mounts and asks for its own.
      prefetchAssets([
        ...clips.flatMap((c) => [
          c.image_b64,
          c.snapshot_b64,
          ...(c.references ?? []).map((r) => r.image_b64),
          ...c.keyframes.map((k) => k.image_b64)
        ]),
        ...story.references.map((r) => r.image_b64),
        ...characters.map((c) => c.sheet_b64)
      ]);
      dirty = false;
    } catch (e) {
      renderError = `load failed: ${(e as Error).message}`;
    }
  }

  function clearAudioSilent() {
    audioFile = null;
    audioB64 = null;
    waveform = null;
    waveDuration = 0;
  }

  async function removeProject(id: string) {
    if (!confirm('Delete this project?')) return;
    try {
      await deleteDirectorProject(id);
      if (projectId === id) newProject();
      await refreshProjects();
    } catch (e) {
      renderError = `delete failed: ${(e as Error).message}`;
    }
  }

  // The API drops empty-prompt segments before the engine sees them, so prompt-less shots are
  // silently skipped — and a timeline where NONE has a prompt (a plan that was never drafted)
  // 400s with "at least one segment prompt is required". Both are UI states, not backend errors.
  //
  // 🔴 Through `clipRenders`, which already owns "where does this engine keep the prompt": on a
  // structured-prompt engine the prose is in the generation's BLOCK and `clip.prompt` is a seed
  // nothing writes back to. Testing the clip directly reported a project whose blocks were all
  // written as having no prompts at all, and disabled Render on both shells.
  let unpromptedClips = $derived.by(() => {
    const written = caps.structuredPrompt ? h3BlockedClipIds(h3Generations) : undefined;
    return clips.filter((c) => !clipRenders(c, written));
  });
  let nothingToRender = $derived(clips.length > 0 && unpromptedClips.length === clips.length);
  let draftableCount = $derived(unpromptedClips.filter((c) => c.idea?.trim()).length);
  let v2vClips = $derived(clips.filter((c) => isV2v(c.mode)));
  // A v2v shot that hasn't been given a source yet (nothing to edit/restyle) — through the same
  // resolver the compiler builds the request from, so a shot this strip stays quiet about cannot be
  // one that compiles to plain t2v. It also covers a shot pointed at a prior render on an engine
  // that cannot read one: that has no source either, and the strip is where it gets said.
  let v2vMissingSource = $derived(
    v2vClips.filter((c) => !v2vSourceOf(c, caps.v2vPriorRenderSource))
  );
  // Prior renders offered as v2v "edit a video" sources: this project's completed renders this
  // session (+ its last saved render), newest first. Each render's segment count = its shot count.
  //
  // ⚠️ The picker they feed is behind `caps.v2vPriorRenderSource`, which is false on both live
  // engines — LTX-2.5 cannot start its sampler from a banked latent. The list is still kept, and
  // deliberately: it costs one push per completed render, and it is what the control needs the day
  // that flag flips. Read `ShotDetailsPane`'s source card before deciding this is dead.
  let priorRenders = $state<
    {
      job_id: string;
      serverJobId: string | null;
      label: string;
      segments: number;
    }[]
  >([]);
  // `serverJobId` is the ENGINE dir id — what a v2v prior-render source must reference (survives
  // koblem's TTL + points at the resumed dir for a continued render). `job_id` (koblem) is kept only
  // as the dedupe/display key.
  function addPriorRender(
    job_id: string,
    serverJobId: string | null,
    segments: number,
    label?: string
  ) {
    if (!job_id || priorRenders.some((r) => r.job_id === job_id)) return;
    priorRenders = [
      {
        job_id,
        serverJobId,
        segments,
        label: label ?? `render ${priorRenders.length + 1}`
      },
      ...priorRenders
    ];
  }
  // Workspace derived views (declared here so they read already-initialised state).
  let selectedIndex = $derived(
    selectedClip ? clips.findIndex((c) => c.id === selectedClip!.id) : -1
  );
  let selectedBeatWords = $derived(
    selectedClip ? beatWordsFor(timelineLayout, lyrics, selectedClip.id) : []
  );

  /**
   * The audio references the selected clip's RENDER UNIT will actually be conditioned by.
   *
   * Read off `gen.references` through the same `h3Ordinals` the composer and the compiler use, and
   * never counted here: position in the reference list assigns the `<Audio N>` ordinal AND advances
   * a shared rotary clock, so a locally-counted number is a number the prompt would address at the
   * wrong asset. Empty on LTX, where a shot carries its own file instead.
   */
  const selectedShotAudio = $derived(
    engine === 'h3' && selectedClip
      ? (() => {
          const gen = packedGenerations.find((g) =>
            g.shots.some((s) => s.clip.id === selectedClip!.id)
          );
          if (!gen) return [];
          return h3Ordinals(gen.references).flatMap((o) => {
            // 🔴 A standalone audio reference carries its `<Audio N>` in `label`; a VIDEO whose
            // soundtrack was promoted carries it in `audioLabel` and keeps `<Video N>` in `label`.
            // Reading `audioLabel` alone misses every standalone track, which is every project song.
            const ordinal = o.kind === 'audio' ? o.label : o.audioLabel;
            if (!ordinal) return [];
            const r = gen.references.find((x) => x.id === o.id);
            return [
              {
                ordinal,
                name:
                  r?.label?.trim() ||
                  (o.id === H3_SOUNDTRACK_REF_ID
                    ? 'the project song'
                    : o.kind === 'video'
                      ? 'the reference clip’s soundtrack'
                      : 'unnamed'),
                fromSong: o.id === H3_SOUNDTRACK_REF_ID
              }
            ];
          });
        })()
      : []
  );

  // Off `unpromptedClips`, so "has something to render" has one definition per engine — on a
  // structured-prompt engine that is the generation's block, not the clip's seed field.
  let promptedCount = $derived(clips.length - unpromptedClips.length);
  /**
   * The cost of one RENDER UNIT, which is a shot on LTX and a generation on H3.
   *
   * Dividing the last full-render time by the shot count on H3 answers a question nobody asked:
   * four shots packed into one generation cost one render between them, so a per-shot figure
   * under-estimates every unit by the packing factor and the ETA runs out well before the picture
   * does.
   */
  let renderUnits = $derived(caps.structuredPrompt ? packedGenerations.length : promptedCount);
  let perShotEstimate = $derived(
    expectedSec > 0 && renderUnits > 0 ? expectedSec / renderUnits : 0
  );
  // ETA for the CURRENT run: a per-shot render (renderThrough / retake) computes fewer shots than
  // the full cut, so scale by how many shots this run renders. Falls back to the full-cut time.
  let activeExpectedSec = $derived(
    renderIsMine && renderShots > 0 && perShotEstimate > 0
      ? perShotEstimate * renderShots
      : expectedSec
  );
  let progressPct = $derived(
    activeExpectedSec > 0 ? Math.min(99, (elapsed / activeExpectedSec) * 100) : 0
  );

  // ── H3 telemetry, plan echo and the reachable ceiling ────────────────────────
  /**
   * The engine's own live progress for the render this project owns. `null` on LTX (which reports
   * none), on a foreign render, and on an H3 render whose shim has said nothing yet — all three are
   * "unknown", and every consumer draws nothing rather than drawing a zero.
   */
  let h3Telemetry = $derived.by<H3Telemetry | null>(() => {
    if (!renderIsMine) return null;
    const t = readH3Telemetry(jobTelemetryRaw);
    return t.known ? t : null;
  });
  /** Which generation is rendering, in packer space (0-based). `null` when the engine hasn't said. */
  let renderingGenIndex = $derived(
    h3Telemetry?.generation != null ? h3Telemetry.generation - 1 : null
  );
  /**
   * The last H3 dispatch's plan echo, off the project's SERVER metadata.
   *
   * Read from `projectMetadata` rather than fetched: koblem stamps it there at dispatch precisely
   * so the answer outlives the job, and `refreshProjectMetadata` re-reads the row when a run ends.
   */
  let h3Plan = $derived<H3Plan | null>(engine === 'h3' ? readH3Plan(projectMetadata) : null);
  /**
   * Delivery problems from the last render of this project — a generation whose bytes would not
   * decode and was dropped, a soundtrack mux that failed.
   *
   * 🔴 This banner exists because the failure it reports is INDISTINGUISHABLE from not having
   * rendered. koblem refuses a generation whose video does not decode rather than filing it as a
   * take (2026-08-10: one shipped as 344 packets of zeros and was delivered as a black clip), so
   * what the author sees afterwards is a shot with no take — exactly what they would see if they
   * had never pressed the button. It was reported only to a log that rotates within hours.
   *
   * Read from `projectMetadata` for the same reason `h3Plan` is: koblem stamps it on the row at
   * harvest so the answer outlives the job, and `refreshProjectMetadata` re-reads when a run ends.
   * An EMPTY list is written on every clean render, which is what dismisses a fixed problem — so
   * this must not treat absent and empty differently.
   *
   * ⚠️ Things the engine said BEFORE rendering do not belong in here and no longer arrive — they
   * are the plan echo's three lists ({@link H3PlanEcho}), which name a snap, a discard and a drop
   * separately instead of calling all of them a delivery failure. Until 2026-08-18 the queue
   * adapter copied them in as well, so a chained render — every multi-generation cut there is —
   * finished by announcing that one thing had not delivered when everything had.
   */
  let deliveryWarnings = $derived<string[]>(
    (Array.isArray(projectMetadata?.last_render_warnings)
      ? (projectMetadata.last_render_warnings as unknown[])
      : []
    )
      .filter((w): w is string => typeof w === 'string' && w.trim().length > 0)
      .map((w) => w.trim())
  );
  /** Dismissed for this session only — the next render's stamp decides whether it comes back. */
  let deliveryWarningsHidden = $state(false);
  $effect(() => {
    // Re-arm whenever the set CHANGES, so dismissing one run's problem cannot hide the next one's.
    void deliveryWarnings.join(' ');
    deliveryWarningsHidden = false;
  });
  /**
   * Does the inspected shot belong to the generation currently on the GPU?
   *
   * On H3 a generation holds several shots, so "is my shot rendering" is a question about its
   * generation. Unknown generation index ⇒ true: the render IS this project's, and withholding the
   * readout because the shim omitted one number is worse than attributing it to the open shot.
   */
  let selectedShotIsRendering = $derived.by(() => {
    if (!renderIsMine || !selectedClip) return false;
    if (renderingGenIndex === null) return true;
    return engineIndexOf(selectedClip) === renderingGenIndex;
  });
  /** Telemetry for the stage — only for a shot in the generation that is actually rendering. */
  let stageTelemetry = $derived(selectedShotIsRendering ? h3Telemetry : null);
  /**
   * The in-flight frame for the stage. Scoped the same way: the preview is of ONE generation, and
   * showing it under a shot from a different one would claim footage that is not being made.
   */
  let stageLivePreview = $derived(selectedShotIsRendering && livePreview ? livePreview.url : null);
  /**
   * 🔴 The reachable ceiling as a WARNING, per generation — never as a re-cap.
   *
   * `reachableCeilingNote` is silent unless the engine has actually reported a reduction AND this
   * generation is over it, so it says nothing when the engine is down, when the field is absent
   * (an older shim does not send it) and when the box reaches the full spec cap. The authoring
   * limits are untouched: feeding a runtime ceiling into the packer would re-pack a saved timeline
   * because the GPU happened to be warm, and re-pack it back when it cooled.
   */
  const reachableNoteFor = (gen: PackedGeneration): string | null =>
    engine === 'h3' ? reachableCeilingNote(h3Props, gen.frames) : null;
  /** The same warning for the render control: the FIRST generation that would be refused. */
  let renderReachableNote = $derived.by(() => {
    if (engine !== 'h3') return null;
    for (const g of packedGenerations) {
      const note = reachableCeilingNote(h3Props, g.frames);
      if (note) return note;
    }
    return null;
  });
  /**
   * Is the engine THIS project renders on reachable right now?
   *
   * 🔴 Asked of the selected engine, and it used to be asked of `props` — which is the LTX
   * bootstrap for every engine that is not `ltx25`. So an H3 project never consulted H3's health at
   * all; it reported a different engine's. That was invisible while both were up, and became
   * "Director says Engine offline and will not render" the moment LTX-2.3 was retired from the
   * deployment: its props answer `healthy: false` to everyone, and every H3 project inherited it.
   *
   * Absent props read as healthy — `null` means "not asked yet", not "down".
   */
  let engineHealthy = $derived(
    engine === 'h3' ? (h3Props?.healthy ?? true) : (props?.healthy ?? true)
  );
  let selectedFinding = $derived(
    selectedClip ? (reviewFindings.find((f) => f.clipId === selectedClip!.id) ?? null) : null
  );
  let selectedStatus = $derived(selectedClip ? shotStatus(selectedClip) : 'none');
  let selectedProxyStatus = $derived(selectedClip ? proxyStatus(selectedClip) : 'none');
  // The in-flight render's live preview for the inspected shot, if it has landed — plays in the
  // stage viewer in place of the pre-render placeholder.
  // `partials[].segment_index` is in ENGINE space, which is NOT the raw `selectedIndex` on either
  // engine — see `engineIndexOf`, the one place that mapping lives.
  let selectedPartial = $derived.by(() => {
    if (!selectedClip) return null;
    const idx = engineIndexOf(selectedClip!);
    return idx < 0 ? null : (partialShots.find((p) => p.index === idx) ?? null);
  });
  // The proxy-tier per-shot clip for the inspected shot, if one has been previewed (same engine
  // index space as selectedPartial).
  let selectedProxyShot = $derived.by(() => {
    if (!selectedClip) return null;
    const idx = engineIndexOf(selectedClip!);
    return idx < 0 ? null : (proxyShots.find((p) => p.index === idx) ?? null);
  });
  // Which of the two tiers' clips the stage plays. On `final` this is the order the stage has always
  // used (live partial first, proxy as the fallback); on `proxy` it inverts, because a 512×288 clip
  // is the one a phone can actually stream.
  let stageSource = $derived(
    preferTier(
      renderTier,
      selectedProxyShot ? { url: selectedProxyShot.url, proxy: true } : null,
      selectedPartial ? { url: selectedPartial.url, proxy: false } : null
    )
  );
  // Same choice for the stitched cut in the Renders drawer — but one-way. Only the proxy tier
  // substitutes; on `final` this is exactly the stitched render the drawer has always played, so a
  // client that never asked for the proxy tier sees no change at all.
  let cutSource = $derived(
    renderTier === 'proxy' && proxyResultUrl
      ? { url: proxyResultUrl, proxy: true }
      : resultUrl
        ? { url: resultUrl, proxy: false }
        : null
  );

  // ---- Takes: several rolls of the inspected shot, kept side by side ---------
  // The takes endpoints key by ENGINE space — the prompt-filtered shot on LTX, the generation on
  // H3 (`retake_segment_h3` tags its job `shot_offset = generation_index` and the harvest files
  // that job's `seg_0` there). `engineIndexOf` is where that choice is made and explained; -1 for
  // "no row" comes through it unchanged.
  let selectedFilteredIndex = $derived(selectedClip ? engineIndexOf(selectedClip) : -1);
  let takes = $state<DirectorTake[]>([]);
  /** One-line status from the last dailies-review kick-off. */
  let criticNote = $state('');
  /**
   * Which tier's takes the desktop's own picker is pointed at. The phone has no such control.
   *
   * 🔴 The two chains are independent, so the phone must follow `renderTier` rather than this: a
   * device that resolved to `proxy` would otherwise list, select and rebuild the FINAL cut it has
   * never seen — and a dailies review with `auto_retake` would start full-price renders from it.
   */
  let desktopTakesTier = $state<'final' | 'proxy'>('final');
  const takesTier = $derived(compact.current ? renderTier : desktopTakesTier);
  let takesBusy = $state(false);
  /** Object URLs for the take previews — revoked whenever the list is replaced. */
  let takeUrls = $state<string[]>([]);

  function clearTakeUrls() {
    for (const u of takeUrls) {
      URL.revokeObjectURL(u);
      // The stills the beat ribbon pulled out of this take are keyed by its object URL, and a
      // revoked URL is never asked for again — so they are unreachable bytes sitting in a bounded
      // cache, pushing live entries out of it.
      clearFrameCache(u);
    }
    takeUrls = [];
  }

  /**
   * Which shot the loaded `takes` describe, in ENGINE index space.
   *
   * The phone prints the count on a per-shot button, so a list left over from the shot before is a
   * wrong number rather than a stale panel nobody is reading.
   */
  let takesIndex = $state(-1);

  async function loadTakes() {
    if (!projectId || selectedFilteredIndex < 0) {
      clearTakeUrls();
      takes = [];
      takesIndex = -1;
      return;
    }
    takesBusy = true;
    try {
      const rows = await listDirectorTakes(projectId, takesTier, selectedFilteredIndex);
      clearTakeUrls();
      takes = rows;
      takesIndex = selectedFilteredIndex;
      // Fetch the clips through the authenticated helper the rest of the editor uses; a bare <video
      // src> would bypass the auth header and 401.
      const urls: string[] = [];
      for (const t of rows) {
        try {
          urls.push(await fetchLtxClip(t.url));
        } catch {
          urls.push('');
        }
      }
      takeUrls = urls;
    } catch {
      takes = [];
      takesIndex = -1;
    } finally {
      takesBusy = false;
    }
  }

  /** Make one take the selected one, then re-derive the stitched cut from the selections. */
  async function pickTake(variant: number) {
    if (!projectId || selectedFilteredIndex < 0) return;
    takesBusy = true;
    try {
      await selectDirectorTake(projectId, takesTier, selectedFilteredIndex, variant);
      await rebuildDirectorCut(projectId, takesTier);
      await loadTakes();
      await hydratePersistedRenders(projectId, { refresh: true });
      // 🔴 The selection just moved, so what the LATER shots were rendered against may have moved
      // with it. Re-read here rather than waiting to be asked: the whole defect this closes is that
      // picking a take used to be silent about the shots downstream of it. Only when the panel is
      // already open — an unopened panel means the user has not asked, and this costs one request
      // per render unit.
      if (joinsLoaded) await loadContinuity();
    } catch (e) {
      renderError = `could not pick that take: ${(e as Error).message}`;
    } finally {
      takesBusy = false;
    }
  }

  // ---- Joins: which take each rendered continuation was actually made against ----------------
  //
  // The gap this closes: selecting a take is already the golden-take picker (`rebuild_cut` joins
  // the selected takes; a retake resolves `continuation.from: "prev"` to the previous shot's
  // SELECTED take), but it left the shots AFTER the one you changed silently stale. They were
  // rendered continuing out of the take that WAS selected, and their opening frames now join a
  // version that no longer plays.
  //
  // 🔴 The repair is a re-render, never a rebuild. See `take-continuity.ts` and `rebuild_cut`'s own
  // doc comment: a rebuild re-concatenates the clips you already have, and the seam is baked into
  // their frames.
  let joinRows = $state<UnitContinuity[]>([]);
  let joinsBusy = $state(false);
  /** Every take of every unit, keyed by ENGINE index — also what the composer's join readout reads. */
  let joinTakes = $state<Map<number, DirectorTake[]>>(new Map());
  /**
   * The project + tier the rows above were read for, and how the panel knows they are still about
   * what is on screen.
   *
   * 🔴 Takes are per TIER — the proxy and final chains are independent, with their own takes and
   * their own joins — so a tier switch invalidates every row. Held as the scope it was read FOR
   * rather than cleared by an `$effect`: an effect that writes the same state it would have to read
   * to know whether to write is a loop waiting to happen, and this makes "loaded" mean what it says.
   */
  let joinScopeRead = $state('');
  const joinScope = $derived(`${projectId ?? ''}:${takesTier}`);
  const joinsLoaded = $derived(joinScopeRead !== '' && joinScopeRead === joinScope);

  /** How many render units this timeline has, in the space takes are keyed by. See `engineIndexOf`. */
  const unitCount = $derived(
    caps.generationIsRetakeUnit ? packedGenerations.length : promptedClips(clips).length
  );

  /**
   * What one unit's join DOES, per the document — the only engine-specific part of the analysis.
   *
   * H3 states it outright (`continuation.mode`, and an author-supplied `from` key that does not
   * follow anybody's selection). LTX has no such field: a shot there continues the previous one
   * unless it starts fresh, which is exactly what `startsFresh` already answers for the compiler.
   */
  function joinSpecFor(index: number): { carries: boolean; explicitFrom?: string | null } {
    if (index === 0) return { carries: false };
    if (caps.generationIsRetakeUnit) {
      const c = packedGenerations[index]?.continuation;
      return { carries: !!c && c.mode !== 'none', explicitFrom: c?.from ?? null };
    }
    const c = promptedClips(clips)[index];
    return { carries: !!c && !startsFresh(c) };
  }

  /** The name a unit goes by on screen — the composer's `Shot N` on H3, the raw shot number on LTX. */
  const unitLabel = (index: number): string => {
    const n = caps.generationIsRetakeUnit ? index + 1 : shotNumberAtPromptedIndex(clips, index);
    return `${vocab.renderUnit} ${n}`;
  };

  /**
   * Read every unit's takes and work the joins out.
   *
   * ⚠️ One request per render unit. `list_renders` returns only the SELECTED take of each shot,
   * which is not enough: telling a full-run take from a retake needs the SIBLINGS, and that is the
   * whole basis of the reconstruction. Kept on demand for that reason — it is not on the load path.
   */
  async function loadContinuity() {
    if (!projectId || unitCount === 0) {
      joinRows = [];
      joinTakes = new Map();
      joinScopeRead = '';
      return;
    }
    // Captured BEFORE the awaits: a tier switch mid-read must not stamp the new scope onto rows
    // read from the old one.
    const scope = joinScope;
    const tier = takesTier;
    joinsBusy = true;
    try {
      const lists = await Promise.all(
        Array.from({ length: unitCount }, (_, i) =>
          listDirectorTakes(projectId!, tier, i).catch(() => [] as DirectorTake[])
        )
      );
      joinTakes = new Map(lists.map((takes, i) => [i, takes]));
      const units: UnitTakes[] = lists.map((takes, i) => ({
        index: i,
        takes,
        join: joinSpecFor(i)
      }));
      joinRows = continuityOf(units, {
        labelOf: unitLabel,
        retakeSplicesTail: caps.retakeSplicesTail
      });
      joinScopeRead = scope;
    } catch (e) {
      renderError = `could not read the joins: ${(e as Error).message}`;
    } finally {
      joinsBusy = false;
    }
  }

  /**
   * Re-render ONE unit so its join lands on the take that plays now.
   *
   * Goes through the ordinary single-unit retake path — `retakeSegment`, the same call the shot's
   * own Retake button makes — because that path is what already resolves the pin server-side
   * against the CURRENT selection. A second dispatcher would be a second chance to get that wrong.
   *
   * ⚠️ `seed = -1` (a fresh roll). The point is a different render, not the same one: re-running the
   * recorded seed would not reproduce the take anyway (neither engine is bit-reproducible), and
   * asking for it would imply that it might.
   */
  async function repairJoin(index: number) {
    const clip = caps.generationIsRetakeUnit
      ? packedGenerations[index]?.shots[0]?.clip
      : promptedClips(clips)[index];
    if (!clip) {
      renderError = 'that shot is no longer in the timeline';
      return;
    }
    // The retake endpoints take a RAW clip index and map it to the render unit themselves.
    await retakeSegment(clips.indexOf(clip), -1, takesTier);
  }

  /**
   * What the composer's join panel says this boundary is pinned to.
   *
   * Forward-looking, and deliberately so: this is what the NEXT render of this unit will join, which
   * is always the predecessor's currently selected take. `null` until the joins have been read once
   * — inventing a take number from an unread project would be worse than saying nothing.
   */
  const composerPinNote = $derived.by(() => {
    if (!joinsLoaded || activeGenIndex <= 0) return null;
    const prev = joinTakes.get(activeGenIndex - 1);
    if (!prev) return null;
    return pendingPinNote(prev, unitLabel(activeGenIndex - 1));
  });

  /**
   * Which identity mechanism this render implements — decides what a reference must BE.
   *
   * Reads the adapter set as well as the model: since `faceid-sheet` and `msr-v2` stopped being
   * checkpoints, `base` alone and `base` with an identity LoRA attached are different regimes and
   * the model name cannot tell them apart.
   */
  let identityRegime = $derived(ltxIdentityRegime(model, loras));

  /**
   * The BUSIEST shot's reference count — what the token-share advisory is computed against.
   *
   * Not the project-wide union: cost is paid per shot (each segment's sequence carries only the
   * references scoped to it), so six characters spread one per shot is cheap and the one shot
   * holding four is the thing worth warning about.
   */
  let worstShotReferenceCount = $derived(worstClipRefCount(clips, characters));

  /** The one-line "this will not do what you think" note, or null when the setup is sound. */
  let identityNote = $derived(
    identityAdvisory(
      model,
      width,
      height,
      Math.round(shotSeconds * fps),
      worstShotReferenceCount,
      loras
    )
  );

  /**
   * What the reference ROLES imply about routing — which shots add an identity adapter, and what
   * the re-folds cost. Separate from `identityNote`, which is about reference cost and kind rather
   * than what runs.
   */
  let roleNote = $derived(roleAdvisory(clips, characters, model, loras));

  /**
   * The resolved per-shot plan: author pins, plus ADVICE on shots that carry a prop. Nothing in it
   * is applied — the only field a compiler reads is `model`, which is an author's pin. The prop
   * entries are `suggestedLora` + a reason, shown so someone can opt in deliberately.
   */
  let clipVariantPlan = $derived(resolveClipVariants(clips, characters, model, loras));

  /**
   * Frame-drop guidance. One sentence for the whole project now: the frame-0 leak was a property of
   * the Echo CHECKPOINTS, and the only LTX left ships one DiT, so there is no per-shot model for the
   * advice to follow. Kept as a value rather than inlined because two panels print it.
   */
  const frameDropNote = frameDropAdvice();

  /** What the inspected shot's runtime-LoRA set resolves to — its own override, or the project's.
   *
   * States the trigger phrases too, because the backend appends them to THIS shot's prompt and
   * nothing else in the editor would say so — an author who sees `zhuanchang` show up in a render
   * log and cannot find where it came from is the failure this line exists to prevent. */
  let selectedClipLoraNote = $derived.by(() => {
    const eff = effectiveClipLoras(selectedClip, loras);
    if (!eff.length) return 'no runtime LoRAs.';
    const triggers = loraTriggers(props?.loras, eff);
    const where = selectedClip?.loras?.length
      ? " (this shot's own set)"
      : ' (inherited from the project)';
    const trig = triggers.length
      ? ` Its prompt gets “${triggers.join(', ')}” appended automatically.`
      : '';
    return `${loraSummary(eff, true, props?.loras)}${where}.${trig}`;
  });

  /**
   * Per shot, the character ids its CARRY already supplies — the previous shot's cast, unless this
   * shot opens a scene. Derived over the whole list because it depends on the shot BEFORE each one.
   *
   * 🔴 This is what stops a reference being re-sent into a join that already holds the identity.
   * Measured 2026-08-15: re-sending it restored the reference's own state (a robot asked to stay
   * headless got its head back) and doubled the motion step at the cut.
   */
  let carriedByClip = $derived(carriedCharacterIds(clips, characters));

  /** References the INSPECTED shot will actually send (its cast, minus the ones with nothing to
   * send — no image yet, promoted out of this very shot, or already carried in by the join). */
  let selectedClipRefCount = $derived(
    selectedClip
      ? clipRefCount(selectedClip, characters, carriedByClip[selectedIndex] ?? new Set<string>())
      : 0
  );

  /** What this ONE shot spends on identity, as a share of its own sequence. Per shot, because that
   * is where the cost is paid: a shot with three references and a short duration is the one that
   * gets its framing captured, however cheap the rest of the timeline is. */
  let selectedClipShareNote = $derived.by(() => {
    if (!selectedClip || selectedClipRefCount === 0) return '';
    const share = refTokenShare(
      width,
      height,
      Math.round((selectedClip.seconds || shotSeconds) * fps),
      selectedClipRefCount,
      identityRegime
    );
    const pct = Math.round(share * 100);
    return share >= REF_SHARE_WARN
      ? `⚠ ${pct}% of this shot's sequence`
      : `${pct}% of this shot's sequence`;
  });

  // ---- Promoting a rendered frame into a character reference -----------------
  // The t2v half of the feature. An i2v shot already owns an image to point a character at; a t2v
  // shot owns nothing until it has been rendered, so the shot that INTRODUCES a character becomes
  // that character's reference afterwards, from its own footage.
  /** Which character a promoted frame lands on. Defaults to the inspected shot's first cast member. */
  let promoteTargetId = $state<string | null>(null);
  let promoteBusy = $state(false);
  /** The take previews, so a promotion can take the frame the user actually scrubbed to. */
  let takeVideoEls = $state<(HTMLVideoElement | null)[]>([]);

  let promoteTarget = $derived(
    characters.find((c) => c.id === (promoteTargetId ?? selectedClipCast[0]?.id)) ?? null
  );

  async function promoteTakeToReference(variant: number, ti: number) {
    // Captured before the await: the inspected shot is what records `introduced_by`, and the
    // selection can move while the extract is in flight.
    const target = promoteTarget;
    const clip = selectedClip;
    const shotIndex = selectedFilteredIndex;
    if (!projectId || shotIndex < 0 || !target || !clip) {
      renderError = 'Pick a character to promote this frame to (add one under Cast first).';
      return;
    }
    promoteBusy = true;
    renderError = null;
    try {
      const at = takeVideoEls[ti]?.currentTime;
      // Geometry is passed so the letterbox targets THIS project's aspect; without it the server
      // falls back to its own 1280×704 and a 1920×1088 project gets a reference the engine then has
      // to resize again. `prepare` is left to the server default (true) deliberately — it is one
      // default, in one place, for the path the editor's own copy recommends.
      const res = await promoteRenderStill(projectId, takesTier, shotIndex, {
        variant,
        width: snap32(width),
        height: snap32(height),
        ...(typeof at === 'number' && at >= 0 ? { seconds: at } : {})
      });
      target.sheet_b64 = res.ref;
      // This shot is where the look was ESTABLISHED, so it must not condition on a frame of
      // itself — characterRefsForClip skips it. Every LATER shot is what the reference is for.
      target.introduced_by = clip.id;
      markDirty();
      criticNote = `Frame at ${res.seconds.toFixed(2)}s (${res.width}×${res.height}) is now ${target.name || 'that character'}'s reference. This shot introduced them, so it renders without it; later shots hold it.`;
    } catch (e) {
      // 422 is the preparation pipeline REFUSING this frame — the subject is cut off at an edge,
      // and the message says which. Terminal: scrub to a different moment, don't retry the same
      // one. Distinguished from a transient failure so the copy doesn't invite a pointless retry.
      const status = (e as Error & { status?: number }).status;
      renderError =
        status === 422
          ? `that frame can't be a reference: ${(e as Error).message} — scrub to a moment where the subject is fully inside the frame and promote that instead.`
          : `could not promote that frame: ${(e as Error).message}`;
    } finally {
      promoteBusy = false;
    }
  }

  /** Kick off the vision critic over the whole cut. Verdicts land on the takes as it works. */
  async function runDailiesReview(autoRetake: boolean) {
    if (!projectId) return;
    try {
      await critiqueDirectorProject(projectId, {
        tier: takesTier,
        auto_retake: autoRetake
      });
      criticNote = autoRetake
        ? 'Dailies review started — failed shots will be re-rolled as NEW takes; nothing is overwritten.'
        : "Dailies review started — verdicts will appear on each shot's takes.";
    } catch (e) {
      renderError = `could not start the review: ${(e as Error).message}`;
    }
  }

  // Best available still to show for a not-yet-rendered shot instead of a blank gradient:
  // its i2v start frame, a storyboard preview, or a pinned keyframe.
  let notRenderedStill = $derived.by(() => {
    const c = selectedClip;
    if (!c) return null;
    if (c.mode === 'i2v' && c.image_b64) return { src: c.image_b64, label: 'start frame' };
    if (c.snapshot_b64) return { src: c.snapshot_b64, label: 'storyboard still' };
    if (c.keyframes[0]?.image_b64) return { src: c.keyframes[0].image_b64, label: 'pinned frame' };
    return null;
  });

  // ── Phone shell ──────────────────────────────────────────────────────────────
  // Declared last, like the block above, so every derivation they read is already initialised.
  const mobileSaveState = $derived<'saved' | 'saving' | 'dirty' | 'stale'>(
    staleDoc ? 'stale' : saving ? 'saving' : dirty || autosaveFailed ? 'dirty' : 'saved'
  );
  const mobileSubtitle = $derived(
    `${clips.length} ${clips.length === 1 ? vocab.renderUnit : vocab.renderUnitPlural} · ${tickLabel(totalDuration, 1)}${saveStatus ? ` · ${saveStatus.toLowerCase()}` : ''}`
  );
  const mobileJob = $derived(
    running && renderIsMine
      ? {
          label: `${h3Telemetry?.generationLabel ?? `${renderShots} ${renderShots === 1 ? vocab.renderUnit : vocab.renderUnitPlural}`} · ${activeRenderTier === 'proxy' ? 'preview' : 'final'}`,
          pct: progressPct
        }
      : null
  );
  /** The take this client would PLAY for a shot — `stageSource`'s rule, applied to any clip. */
  const shotSourceFor = (c: DirectorClip): string | null => {
    const idx = engineIndexOf(c);
    if (idx < 0) return null;
    return preferTier(
      renderTier,
      proxyShots.find((p) => p.index === idx)?.url ?? null,
      partialShots.find((p) => p.index === idx)?.url ?? null
    );
  };
  /**
   * Which shots a full render would redo, in RAW editor numbers — or NOTHING on an engine where it
   * would redo all of them.
   *
   * Read off the same per-shot status the timeline dots use, so the sentence above the button and
   * the dots beside the shots cannot disagree. It is a forecast, not the plan: the server still
   * answers the scope question through `RenderScopeDialog` after the button is pressed.
   *
   * 🔴 Empty unless the engine can actually be asked for a subset. `render()` only offers the
   * choice where `scopedFullRender` holds; everywhere else the button wipes the tier and redraws
   * every shot, and a phone has no dialog after the tap to correct a promise made before it. Which
   * also settles the count: the sentence is only drawn where one clip is one render unit, so its
   * raw numbers and `unitCount` are the same set.
   */
  const mobileScope = $derived.by(() => {
    const stale: number[] = [];
    const keep: number[] = [];
    if (!caps.scopedFullRender) return { stale, keep };
    const statusOf = renderTier === 'proxy' ? proxyStatus : shotStatus;
    clips.forEach((c, i) => (statusOf(c) === 'ok' ? keep : stale).push(i + 1));
    return { stale, keep };
  });
  const mobilePartials = $derived(
    renderTier === 'proxy'
      ? proxyShots.length
        ? proxyShots
        : partialShots
      : partialShots.length
        ? partialShots
        : proxyShots
  );
  const mobilePartialSeconds = (engineIdx: number): number | null => {
    if (caps.generationIsRetakeUnit) return packedGenerations[engineIdx]?.seconds ?? null;
    const c = clipAtPromptedIndex(clips, engineIdx);
    if (!c) return null;
    return timelineLayout.byId.get(c.id)?.width ?? c.seconds;
  };
  const mobileRefsSummary = $derived.by(() => {
    if (!caps.omniReferences) {
      const n = characters.length;
      return n === 0 ? 'No character sheets yet' : `${n} character sheet${n === 1 ? '' : 's'}`;
    }
    const pics = h3References.filter((r) => r.kind === 'image').length;
    const vids = h3References.filter((r) => r.kind === 'video').length;
    const auds = h3References.filter((r) => r.kind === 'audio').length;
    if (!pics && !vids && !auds) return 'None yet — ordinals are position';
    const parts = [
      pics ? `${pics} picture${pics === 1 ? '' : 's'}` : '',
      vids ? `${vids} video${vids === 1 ? '' : 's'}` : '',
      auds ? `${auds} audio` : ''
    ].filter(Boolean);
    return `${parts.join(' · ')} · ordinals matter`;
  });
  const mobileSongSummary = $derived(
    audioB64
      ? `${audioFile?.name ?? 'project track'}${waveDuration > 0 ? ` · ${tickLabel(waveDuration, 1)}` : ''} · ${lyrics.length} words aligned`
      : 'No track yet'
  );
  const mobileShotLabel = $derived(
    selectedIndex >= 0 ? `${vocab.renderUnit} ${selectedIndex + 1}` : `no ${vocab.renderUnit} open`
  );

  // ── The Prompt screen ────────────────────────────────────────────────────────
  // One render unit's worth of the state above, shaped for the phone. Derived here rather than in
  // `MobilePrompt` so the block editor sheet and the prompt screen read the same rows — an ordinal
  // that meant different things on two surfaces is the exact drift the shared type exists to stop.
  const mobileGen = $derived(
    caps.structuredPrompt ? (packedGenerations[activeGenIndex] ?? null) : null
  );
  const mobilePrompt = $derived(
    caps.structuredPrompt ? (mobileGen?.doc.prompt ?? '') : (selectedClip?.prompt ?? '')
  );
  const mobileRefs = $derived(
    mobileGen ? mobileRefRows(mobileGen.references, (v) => assetSrc(v)) : []
  );
  /**
   * Every `<Label N>` that means something on the wire — the in-scope ordinals plus whatever the
   * block declares for itself. Anything else is a number the model reads as ordinary words, which is
   * what `H3BlockEditor` paints red and what the editor's footer counts.
   */
  const mobileResolvedLabels = $derived(
    new Set([
      ...mobileRefs.map((r) => r.label),
      ...parseH3Block(mobilePrompt)
        .subjectDefinitions.map((d) => d.label.trim())
        .filter(Boolean)
    ])
  );
  const mobileTier = $derived(renderTier === 'proxy' ? 'proxy' : 'final');
  const mobileRendered = $derived(
    (renderTier === 'proxy' ? selectedProxyStatus : selectedStatus) !== 'none'
  );
  const mobileTakeSrc = $derived(selectedClip ? shotSourceFor(selectedClip) : null);
  const mobileFrames = $derived(
    mobileGen ? mobileGen.frames : framesFromSeconds(selectedClip?.seconds ?? 0, fps)
  );
  /**
   * The render unit's own length, but only where it is NOT this clip's.
   *
   * `mobileFrames` above is the generation's, and a generation can pack several clips — so the
   * Details stepper (which moves ONE clip's authored seconds) has to be able to say which number
   * belongs to which, instead of printing "4.20s" over "362 frames".
   */
  const mobileRenderSeconds = $derived(
    mobileGen && mobileGen.shots.length > 1 ? mobileGen.seconds : null
  );
  const mobileStateLabel = $derived(
    `${shotStateLabel(renderTier === 'proxy' ? selectedProxyStatus : selectedStatus)} · ${(
      mobileGen?.seconds ??
      selectedClip?.seconds ??
      0
    ).toFixed(2)}s`
  );
  const mobileTakeNote = $derived(
    mobileTakeSrc
      ? `${(mobileGen?.seconds ?? selectedClip?.seconds ?? 0).toFixed(2)}s at ${mobileTier}`
      : ''
  );

  /**
   * Seed a block nobody has written, on the phone.
   *
   * 🔴 The desktop's copy of this lives in `H3PromptComposer`, which is not mounted below
   * `DIRECTOR_COMPACT` — without it every unwritten render unit would open to an empty Prompt view
   * with no sections and no beats, while the compiler's own fallback quietly rendered a block the
   * author never saw. Same two functions, so the two paths cannot produce different bytes.
   */
  $effect(() => {
    if (!compact.current) return;
    const gen = mobileGen;
    if (gen && needsH3PromptSeed(gen.doc)) setGenerationPrompt(gen.id, seedH3Prompt(gen));
  });

  /** Attach the image this render unit opens on, through the same path the desktop picker uses. */
  async function mobilePickOpeningFrame() {
    const file = await pickFile('image');
    if (!file || !selectedClip) return;
    setOpeningFrame(selectedClip, await putAsset(file));
  }

  // ⚠️ The OPENING image only, which is what the button says and what the desktop's own frame slot
  // does. It used to drop `h3_end_image` with it — a closing anchor this shell has no control for,
  // so the phone silently deleted work it could not show and could not put back.
  function mobileClearOpeningFrame() {
    if (!selectedClip) return;
    selectedClip.image_b64 = undefined;
    markDirty();
  }

  /** Aim the rework sheet at one section, which is the only section-scoped LLM move that exists. */
  function mobileRework(section?: string) {
    if (section)
      reworkShotText = `Rewrite only the ${sectionLabel(section)} section. Leave every other section exactly as it is.`;
    mobileSheet = 'rework';
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="dw" class:dw-mobile={compact.current}>
  <!-- Rail panel bodies — one source of markup, rendered compact inline and roomy in a modal.
       Declared at .dw scope so both the rail folds and the expand modal can render them. -->
  {#snippet storyBody()}
    <StoryPanel
      bind:story
      onchange={markDirty}
      onaddref={storyAddRef}
      onremoveref={storyRemoveRef}
      onrefimage={storyRefImage}
      onclearrefimage={storyClearRefImage}
    />
  {/snippet}

  <!-- One tab, two mechanisms. An LTX character sheet is a rotary-tagged latent identity the
       prompt cannot name; an H3 reference is text-encoder conditioning it CAN name by ordinal.
       They are not modes of one panel, so the branch is here rather than inside either. Both
       mount sites (the rail fold and the expand modal) render this snippet, so gating it once
       covers both. -->
  {#snippet castBody()}
    {#if caps.omniReferences}
      <!-- 🔴 These fallbacks apply whenever `/props` has not answered — yet, or at all — so they are
           not decoration: they are the caps a user builds a timeline against while the engine is
           down. Video and audio are **2**, not 3. `validate_generative_settings:260-319` refuses a
           third, and the 3 they carried came from the retired C++ engine, so the editor was offering
           a slot whose refusal only arrives at submit, after the timeline was built around it. -->
      <ReferencesPanel
        bind:references={h3References}
        {clips}
        {vocab}
        maxImages={h3RefCaps.maxImages}
        maxVideos={h3RefCaps.maxVideos}
        maxAudios={h3RefCaps.maxAudios}
        maxTotal={h3RefCaps.maxTotal}
        keyframePictures={h3KeyframePictures}
        onchange={markDirty}
        onadd={addReference}
        onremove={removeReference}
        onmove={moveReference}
        onfile={onReferenceFile}
      />
    {:else}
      <CastPanel
        bind:characters
        {model}
        {loras}
        {width}
        {height}
        {identityRegime}
        {identityNote}
        {roleNote}
        {referencePreparing}
        {referencePrepError}
        hasStartFrame={!!selectedClip?.image_b64}
        introducedByShotNo={(ch) => clips.findIndex((c) => c.id === ch.introduced_by) + 1}
        onchange={markDirty}
        onadd={addCharacter}
        onremove={removeCharacter}
        onsheet={onCharacterSheet}
        onstartframe={useStartFrameAsReference}
      />
    {/if}
  {/snippet}

  {#snippet autoBody()}
    <AutoDirectPanel
      bind:autoBrief
      bind:autoMaxShots
      {autoDirecting}
      {audioFile}
      {vocab}
      hasAudio={!!audioB64}
      onaudio={onAudioFiles}
      onopenaudio={() => (audioOpen = true)}
      projectSoundtrack={caps.projectSoundtrack}
      ondirect={autoDirect}
    />
  {/snippet}

  {#snippet reviseBody()}
    <RevisePanel
      bind:reworkInstruction
      bind:extendDirection
      bind:extendCount
      {reworking}
      {reviewing}
      {gistingAll}
      {draftAllRunning}
      {storyboarding}
      {storyboardProgress}
      {extending}
      imageEngineReady={imgProps ? imgProps.healthy : true}
      {reviewRan}
      findingCount={reviewFindings.length}
      {scriptingAll}
      scriptUnits={scriptPassUnitCount}
      scriptUnitCap={SCRIPT_PASS_MAX_UNITS}
      unitPlural={vocab.renderUnitPlural}
      onrework={reworkScript}
      onreview={runReview}
      ongistall={gistAllShots}
      ondraftall={() => draftAllShots()}
      onscriptall={() => void scriptWholeProject()}
      onstoryboardall={storyboardAll}
      onextend={extendStory}
      onopenfindings={() => (reviewOpen = true)}
    />
  {/snippet}

  <!-- ── WORK ZONE ───────────────────────────────────────── -->
  <!-- ── The take, at the size you watch it ────────────────────────────────────────────────
     🔴 The WATCHING posture — mounted when `viewerExpanded` (Escape returns), and unconditionally
     on a screen too narrow to have a rail, where it is the only picture there is. The viewer used
     to hold a 780px grid column here permanently, which left the shot's own editor about 380px on a
     1500px window: a prompt forty characters wide with its panels folded underneath. While you are
     writing you are not watching, and the rail's `RailDock` is what the writing posture gets.

     Still a snippet rather than inlined at the one call site: `clip` is narrowed non-null here, and
     the thirty props stay in one block instead of nested inside the stage's `{#if}` ladder. -->
  {#snippet stageViewer(clip: DirectorClip)}
    <StageViewer
      {clip}
      shotIndex={selectedIndex}
      {fps}
      status={selectedStatus}
      proxyStatus={selectedProxyStatus}
      bind:expanded={viewerExpanded}
      heading={shotHeading(clip)}
      modeLabel={modeLabel(clip.mode)}
      {stageSource}
      {notRenderedStill}
      {elapsed}
      {activeExpectedSec}
      {perShotEstimate}
      telemetry={stageTelemetry}
      livePreview={stageLivePreview}
      {vocab}
      proxyW={previewW}
      proxyH={previewH}
      previewFullRes={caps.previewIsFullRes}
      {renderTier}
      {snapBusy}
      imageEngineReady={imgProps ? imgProps.healthy : true}
      canRetake={canRetakeUnit}
      {retakeNote}
      retakeUnavailableNote={unavailableFor(engine, 'resume')}
      onretake={retakeCurrent}
      onsnapshot={runSnapshot}
      onpreview={previewCurrent}
      onrender={renderCurrent}
      onopenrenders={() => (rendersOpen = true)}
    />
  {/snippet}

  <!-- ── PHONE SHELL ──────────────────────────────────────────────────────────────────
       Below `DIRECTOR_COMPACT` the app bar, the two-zone work grid and the timeline are replaced
       wholesale rather than squeezed. Every screen is a snippet rendered here, so all project
       state stays in this component and the mobile components stay pure presentation. -->
  {#if compact.current}
    {#snippet screenShots()}
      <MobileShots
        bind:clips
        bind:selectedIds
        bind:playhead
        bind:playing
        generations={packedGenerations}
        {vocab}
        {caps}
        tier={renderTier}
        layout={timelineLayout}
        {audioSrc}
        {waveform}
        songSeconds={waveDuration}
        {shotStatus}
        {proxyStatus}
        shotSource={shotSourceFor}
        findingSeverity={(id) => {
          const f = reviewFindings.find((x) => x.clipId === id);
          return f ? (f.severity === 'low' ? 'info' : 'warn') : null;
        }}
        draftFailed={(id) => draftFailedIds.includes(id)}
        onchange={markDirty}
        onopen={(id) => {
          gotoShot(id);
          mobileTab = 'prompt';
        }}
        onadd={addClip}
        oncontinuescene={continueScene}
        onduplicate={duplicateSelected}
        ondelete={deleteSelected}
      />
    {/snippet}

    {#snippet screenPrompt()}
      {#if selectedClip}
        <MobilePrompt
          clip={selectedClip}
          bind:sub={mobilePromptSub}
          bind:section={mobilePromptSection}
          bind:expanded={mobileTakeExpanded}
          bind:card={mobileCard}
          bind:deadLabel={mobileDeadLabel}
          gen={mobileGen ? { id: mobileGen.id, seconds: mobileGen.seconds } : null}
          doc={mobileGen?.doc ?? null}
          {caps}
          {vocab}
          index={selectedIndex}
          count={clips.length}
          stateLabel={mobileStateLabel}
          prompt={mobilePrompt}
          idea={selectedClip.idea ?? ''}
          refs={mobileRefs}
          resolvedLabels={mobileResolvedLabels}
          takeSrc={mobileTakeSrc}
          takeCount={takesIndex === selectedFilteredIndex ? takes.length : 0}
          takeNote={mobileTakeNote}
          rendered={mobileRendered}
          finding={selectedFinding
            ? { title: selectedFinding.issue, body: selectedFinding.notes }
            : null}
          findingBusy={selectedFinding?.fixing ?? false}
          frames={mobileFrames}
          renderSeconds={mobileRenderSeconds}
          {fps}
          maxFrames={caps.maxFrames === null ? null : unitMaxFrames}
          songNote={soundtrackNote}
          reachableNote={mobileGen ? reachableNoteFor(mobileGen) : null}
          {promptBusy}
          canReroll={canRetakeUnit}
          {retakeNote}
          resumeNote={unavailableFor(engine, 'resume')}
          tier={mobileTier}
          opensOnImage={selectedClip.mode === 'i2v' && !!selectedClip.image_b64}
          frameMode={h3FrameMode(selectedClip)}
          frameStill={assetSrc(selectedClip.image_b64)}
          joinMode={mobileGen?.continuation?.mode ?? null}
          joinDepth={mobileGen?.continuation?.k ?? 7}
          joinNote={composerPinNote}
          loraCatalogue={h3Props?.loras ?? null}
          projectLoras={h3Loras}
          ltxLoras={props?.loras ?? []}
          projectLtxLoras={loras}
          loraNote={selectedClipLoraNote}
          projectSteps={h3Steps}
          projectWidth={width}
          projectHeight={height}
          frameFor={mobileTakeSrc ? (_i, at) => cachedFrame(mobileTakeSrc!, at) : undefined}
          onprev={() => clips[selectedIndex - 1] && gotoShot(clips[selectedIndex - 1].id)}
          onnext={() => clips[selectedIndex + 1] && gotoShot(clips[selectedIndex + 1].id)}
          onprompt={(next) => {
            if (mobileGen) setGenerationPrompt(mobileGen.id, next);
            else if (selectedClip) {
              selectedClip.prompt = next;
              markDirty();
            }
          }}
          onidea={(next) => {
            if (!selectedClip) return;
            selectedClip.idea = next;
            markDirty();
          }}
          ondraft={() => runPromptLlm(draftClipPrompt)}
          onreroll={() => runPromptLlm(rerollClipPrompt)}
          onsummarise={() =>
            runPromptLlm(caps.structuredPrompt ? summariseGenerationGist : summariseClipGist)}
          onrework={mobileRework}
          oneditblock={() => (mobileSheet = 'editor')}
          onopentakes={() => (mobileSheet = 'takes')}
          onopenrefs={() => (mobileSheet = 'refs')}
          onrender={renderCurrent}
          onretake={retakeCurrent}
          onapplyfinding={() => selectedFinding && applyFinding(selectedFinding)}
          ondismissfinding={() =>
            (reviewFindings = reviewFindings.filter((x) => x !== selectedFinding))}
          onseconds={(next) => {
            if (!selectedClip) return;
            selectedClip.seconds = Math.max(0.1, Number(next.toFixed(3)));
            markDirty();
          }}
          onframeimage={mobilePickOpeningFrame}
          onframemode={(mode) => {
            if (!selectedClip) return;
            selectedClip.h3_frame_mode = mode;
            markDirty();
          }}
          onremoveframe={mobileClearOpeningFrame}
          onjoin={(mode) =>
            mobileGen &&
            updateGeneration(mobileGen.id, {
              continuation: { ...(mobileGen.continuation ?? {}), mode }
            })}
          onjoindepth={(k) =>
            mobileGen &&
            updateGeneration(mobileGen.id, {
              // 🔴 Spread, like its sibling above: `from` names a latent bank the author chose and
              // `anchor` says where in it to join. Rebuilding the object from `mode` alone drops
              // both, and `compile.ts` then silently falls back to the previous unit.
              continuation: { mode: 'latents', ...(mobileGen.continuation ?? {}), k }
            })}
          onupdate={(patch) => mobileGen && updateGeneration(mobileGen.id, patch)}
          onchange={markDirty}
        />
      {:else}
        <p class="hint">
          No {vocab.renderUnit} open. Pick one on the {vocab.renderUnitPlural} screen.
        </p>
      {/if}
    {/snippet}

    {#snippet screenRender()}
      <MobileRender
        {vocab}
        bind:queueOptions={queueOpts}
        {queueKind}
        {queueAccepted}
        {cutSource}
        {totalDuration}
        unitCount={renderUnits}
        staleNos={mobileScope.stale}
        keepNos={mobileScope.keep}
        {tierPref}
        tier={renderTier}
        previewFullRes={caps.previewIsFullRes}
        {previewW}
        {previewH}
        {running}
        {renderIsMine}
        {renderElsewhere}
        {renderOwnerName}
        {progressPct}
        {elapsed}
        {activeExpectedSec}
        telemetry={h3Telemetry}
        partials={mobilePartials}
        partialLabel={(i) => String(partialShotNo(i))}
        partialSeconds={mobilePartialSeconds}
        expectedUnits={renderUnits}
        {nothingToRender}
        {renderError}
        reachableNote={renderReachableNote}
        historyCount={renderRows.length}
        ontier={setTierPref}
        onrender={renderPrimary}
        oncancel={cancelRender}
        onopenhistory={() => (rendersOpen = true)}
        onopenshot={(i) => {
          const c = partialClip(i);
          if (c) {
            gotoShot(c.id);
            mobileTab = 'prompt';
          }
        }}
      />
    {/snippet}

    {#snippet screenMore()}
      <MobileMore
        storySummary={storyIsEmpty(story) ? 'Not written yet' : 'Backstory, tone and rules'}
        refsTitle={caps.omniReferences ? 'References' : 'Cast'}
        refsSummary={mobileRefsSummary}
        songSummary={mobileSongSummary}
        settingsSummary="{caps.label} · {outW}×{outH} · {fps} fps"
        reviseSummary={reviewFindings.length
          ? `${reviewFindings.length} review ${reviewFindings.length === 1 ? 'finding' : 'findings'}`
          : 'Rework, review, storyboard, extend'}
        autoSummary="Brief in, shot list out"
        projectSummary="{projects.length} {projects.length === 1 ? 'project' : 'projects'}"
        {duplicating}
        canDuplicate={!!projectId}
        promptsSummary={caps.structuredPrompt && packedGenerations.length
          ? `Every block as text · find & replace across ${packedGenerations.length} ${
              packedGenerations.length === 1 ? vocab.renderUnit : vocab.renderUnitPlural
            }`
          : null}
        onopen={(id) => (mobileSheet = id)}
        onchat={() => (chatOpen = true)}
        onprojects={() => (projectMenuOpen = true)}
        onduplicate={duplicateProject}
      />
    {/snippet}

    <!-- The rail panels are reused as sheet bodies unchanged — the phone needed a route to them,
         not a second copy of them. `castBody` is already the engine branch (references on H3,
         character sheets on LTX), so the row that opens it is engine-aware for free. -->
    {#snippet sheetStory()}
      <div class="sheetbody">{@render storyBody()}</div>
    {/snippet}
    {#snippet sheetRefs()}
      <div class="sheetbody">{@render castBody()}</div>
    {/snippet}
    {#snippet sheetAuto()}
      <div class="sheetbody">{@render autoBody()}</div>
    {/snippet}
    {#snippet sheetRevise()}
      <div class="sheetbody">{@render reviseBody()}</div>
    {/snippet}

    {#snippet sheetAudio()}
      <MobileSongSheet
        bind:h3SoundtrackMode
        bind:h3SoundtrackDiegetic
        {audioFile}
        audioName={audioB64 ? (audioFile?.name ?? 'project track') : null}
        {waveDuration}
        {lyrics}
        {transcribing}
        {audioKind}
        soundtrackIsReference={caps.soundtrackRoute === 'audio_reference'}
        fitNote={soundtrackNote}
        onopenfull={() => (audioOpen = true)}
        ontranscribe={transcribe}
        onchange={markDirty}
      />
    {/snippet}

    {#snippet sheetSettings()}
      <MobileSettingsSheet
        {engine}
        engineLabel={caps.label}
        engineBlurb={caps.blurb}
        bind:fps
        variableFps={caps.variableFps}
        fixedFpsNote="{caps.label} is defined only at {caps.fixedFps} fps — its frame grid, temporal VAE and audio latent rate are all fixed to it."
        {sizeSel}
        {sizeTiers}
        {outW}
        {outH}
        {snapNote}
        previewFullRes={caps.previewIsFullRes}
        previewSel={previewSizeSel}
        {previewW}
        {previewH}
        onsize={selectSize}
        onpreviewsize={selectPreviewSize}
        onchange={markDirty}
        onopenfull={() => (settingsOpen = true)}
      />
    {/snippet}

    <!-- The same CodeMirror the desktop uses, full screen. `bare` hands it the whole body so the
         document scrolls inside it and the dock stays put where the keyboard will be. -->
    {#snippet sheetEditor()}
      {#if mobileGen}
        <MobileBlockEditor
          value={mobilePrompt}
          docId={mobileGen.doc.id}
          refs={mobileRefs}
          resolvedLabels={mobileResolvedLabels}
          unit={vocab.renderUnit}
          subUnit={vocab.subUnit ?? vocab.renderUnit}
          oninput={(next) => setGenerationPrompt(mobileGen!.id, next)}
          onclose={() => (mobileSheet = null)}
        />
      {/if}
    {/snippet}

    <!-- The desktop's Prompts screen, minus its modal: the shell's back button is the way out, and
         `bare` gives the panel the whole body so its toolbar can stay put while the list scrolls. -->
    {#snippet sheetPrompts()}
      <H3PromptsPanel
        docs={h3PromptDocs}
        {vocab}
        compact
        onprompt={setGenerationPrompt}
        oncapture={captureRestore}
        ongoto={(id) => {
          const clip = packedGenerations.find((g) => g.id === id)?.shots[0]?.clip;
          if (clip) gotoShot(clip.id);
          mobileSheet = null;
          mobileTab = 'prompt';
        }}
      />
    {/snippet}

    <!-- `canPromote` reads `characterRefs`, not `!omniReferences`: promotion writes
         `DirectorCharacter.sheet_b64`, so the flag it needs is the one that says character sheets
         exist on this engine. -->
    {#snippet sheetTakes()}
      <MobileTakes
        bind:takeEls={takeVideoEls}
        takes={takesIndex === selectedFilteredIndex ? takes : []}
        takeUrls={takesIndex === selectedFilteredIndex ? takeUrls : []}
        busy={takesBusy}
        unit={vocab.renderUnit}
        tier={takesTier}
        {criticNote}
        canPromote={caps.characterRefs && !!promoteTarget}
        promoteLabel={promoteTarget?.name ?? null}
        {promoteBusy}
        onload={loadTakes}
        onpick={pickTake}
        onpromote={promoteTakeToReference}
        onreview={runDailiesReview}
      />
    {/snippet}

    {#snippet sheetRework()}
      <MobileReworkSheet
        bind:instruction={reworkShotText}
        shotLabel={mobileShotLabel}
        unit={vocab.renderUnit}
        busy={promptBusy}
        hasPrompt={!!selectedClip?.prompt?.trim()}
        onrework={async () => {
          await reworkThisShot();
          if (!renderError) mobileSheet = null;
        }}
      />
    {/snippet}

    <MobileShell
      bind:tab={mobileTab}
      bind:sheet={mobileSheet}
      title={projectName}
      subtitle={mobileSubtitle}
      ontitle={() => (projectMenuOpen = true)}
      saveState={mobileSaveState}
      job={mobileJob}
      toast={showAiToast && aiRestore
        ? { label: aiRestore.label, detail: 'Puts the shots and the story back as they were' }
        : null}
      sheets={[
        { id: 'story', title: 'Story context', subtitle: projectName, body: sheetStory },
        {
          id: 'refs',
          title: caps.omniReferences ? 'References' : 'Cast',
          subtitle: caps.omniReferences ? 'ordinals are position' : 'identity tags',
          body: sheetRefs
        },
        {
          id: 'audio',
          title: 'Song & lyrics',
          subtitle: audioFile?.name ?? projectName,
          body: sheetAudio
        },
        {
          id: 'settings',
          title: 'Render settings',
          subtitle: projectName,
          body: sheetSettings
        },
        { id: 'auto', title: 'Auto-direct', subtitle: 'Brief in, shot list out', body: sheetAuto },
        { id: 'revise', title: 'Revise & review', subtitle: projectName, body: sheetRevise },
        { id: 'rework', title: 'Rework', subtitle: mobileShotLabel, body: sheetRework },
        {
          id: 'takes',
          title: 'Takes',
          subtitle: `${mobileShotLabel} · ${takesTier}`,
          body: sheetTakes
        },
        // ⚠️ Only while there IS a block to edit — the shell closes a sheet whose definition has
        // gone, which is what takes the author off this one when they switch to an engine with no
        // structured prompt rather than stranding them on an empty body.
        ...(mobileGen
          ? [
              {
                id: 'editor',
                title: 'Edit block',
                subtitle: `${mobileShotLabel} · saved as you type`,
                body: sheetEditor,
                bare: true
              }
            ]
          : []),
        // ⚠️ Same discipline as the editor sheet above: registered only while there ARE blocks, so
        // switching to an engine without a structured prompt takes the author off it rather than
        // leaving them on an empty list.
        ...(caps.structuredPrompt && packedGenerations.length
          ? [
              {
                id: 'prompts',
                title: 'Prompts',
                subtitle: `${packedGenerations.length} ${
                  packedGenerations.length === 1 ? vocab.renderUnit : vocab.renderUnitPlural
                } · find & replace`,
                body: sheetPrompts,
                bare: true
              }
            ]
          : [])
      ]}
      shots={screenShots}
      prompt={screenPrompt}
      render={screenRender}
      more={screenMore}
      onchat={() => (chatOpen = true)}
      onundo={revertAiChange}
      ondismisstoast={dismissAiToast}
      onjob={() => {
        mobileSheet = null;
        mobileTab = 'render';
      }}
    />
  {:else}
    <AppBar
      bind:projectName
      bind:queueOptions={queueOpts}
      {queueKind}
      {queueAccepted}
      {saveStatus}
      {engineHealthy}
      engineLabel={caps.label}
      shotCount={clips.length}
      {totalDuration}
      {expectedSec}
      {draftAllRunning}
      {draftAllDone}
      {draftAllTotal}
      {draftAllLabel}
      draftFailedCount={draftFailedIds.length}
      {draftableCount}
      stale={!!staleDoc}
      warnings={llmWarnings}
      {nothingToRender}
      unpromptedCount={unpromptedClips.length}
      {saving}
      {renderTier}
      proxyW={previewW}
      proxyH={previewH}
      previewFullRes={caps.previewIsFullRes}
      {continueFromLast}
      {continueMisaligned}
      onnameinput={onNameInput}
      oncommitname={() => void commitRename()}
      onopenprojects={() => (projectMenuOpen = true)}
      onopenpanels={() => (expandPanel = 'story')}
      onopenprompts={caps.structuredPrompt ? () => (promptsOpen = true) : null}
      onopensettings={() => (settingsOpen = true)}
      onopenrenders={() => (rendersOpen = true)}
      onretrydrafts={retryFailedDrafts}
      onrender={renderPrimary}
      onreload={() => void reloadStale()}
      onoverwrite={() => void overwriteStale()}
      ondismisswarnings={() => (llmWarnings = [])}
      ondraftall={() => draftAllShots()}
    />

    <div class="work">
      <!-- LEFT RAIL -->
      <aside class="rail">
        <!-- The take, docked above the writing panels and sticky to the top of the rail: opening a
             fold used to scroll it away, and checking the picture against the line that made it is
             the whole loop this workspace is for. `RailDock` is drawn for 296px; `StageViewer` is
             not, which is why it is a second component and not this one squeezed. -->
        {#if selectedClip}
          <div class="rail-dock">
            <RailDock
              shotIndex={selectedIndex}
              heading={shotHeading(selectedClip)}
              {vocab}
              status={selectedStatus}
              proxyStatus={selectedProxyStatus}
              {stageSource}
              {notRenderedStill}
              livePreview={stageLivePreview}
              {takes}
              {elapsed}
              {activeExpectedSec}
              {perShotEstimate}
              {renderTier}
              hasPrompt={!!selectedClip.prompt?.trim()}
              canRetake={canRetakeUnit}
              {retakeNote}
              retakeUnavailableNote={unavailableFor(engine, 'resume')}
              bind:expanded={viewerExpanded}
              onretake={retakeCurrent}
              onpreview={previewCurrent}
              onrender={renderCurrent}
              onopenrenders={() => (rendersOpen = true)}
            />
          </div>
        {/if}
        <div class="sec-h">
          <span class="eye">The Director</span><span class="grow"></span>
        </div>
        <button class="chat-entry" onclick={() => (chatOpen = true)}>
          <span class="ic">💬</span>
          <span><b>Chat with the director</b><small>Talk it through — it edits for you</small></span
          >
        </button>

        <!-- Story context -->
        <details class="fold" bind:open={foldStory}>
          <summary
            ><span class="chev">▶</span><b>Story context</b>{#if !storyIsEmpty(story)}<span
                class="cdot"
              ></span>{/if}<button
              class="fold-expand"
              title="Expand into a roomy editor"
              aria-label="Expand Story context"
              onclick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                expandPanel = 'story';
              }}>⤢</button
            ></summary
          >
          <div class="body">{@render storyBody()}</div>
        </details>

        <!-- Cast — character sheets and their identity tags -->
        <details class="fold" bind:open={foldCast}>
          <summary
            ><span class="chev">▶</span><b>Cast</b>{#if characters.length}<span class="cnt"
                >{characters.length}
                {characters.length === 1 ? 'character' : 'characters'}</span
              >{/if}<button
              class="fold-expand"
              title="Expand into a roomy editor"
              aria-label="Expand Cast"
              onclick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                expandPanel = 'cast';
              }}>⤢</button
            ></summary
          >
          <div class="body">{@render castBody()}</div>
        </details>

        <!-- Auto-direct -->
        <details class="fold" bind:open={foldAuto}>
          <summary
            ><span class="chev">▶</span><b>Auto-direct</b><span class="cnt">from a brief</span
            ><button
              class="fold-expand"
              title="Expand into a roomy editor"
              aria-label="Expand Auto-direct"
              onclick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                expandPanel = 'auto';
              }}>⤢</button
            ></summary
          >
          <div class="body">{@render autoBody()}</div>
        </details>

        <!-- Revise & review -->
        <details class="fold" bind:open={foldRevise}>
          <summary
            ><span class="chev">▶</span><b>Revise &amp; review</b>{#if reviewFindings.length}<span
                class="cnt">{reviewFindings.length} findings</span
              >{/if}<button
              class="fold-expand"
              title="Expand into a roomy editor"
              aria-label="Expand Revise and review"
              onclick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                expandPanel = 'revise';
              }}>⤢</button
            ></summary
          >
          <div class="body">{@render reviseBody()}</div>
        </details>
      </aside>

      <!-- ── CENTER STAGE ─────────────────────────────────── -->
      <main class="stage" class:watching={viewerExpanded} bind:this={stageEl}>
        <!-- 🔴 Above the stage, not tucked in a panel: this reports a shot that is MISSING, and a
             missing shot reads as "I never rendered it". If it has to be gone looking for, it has
             already failed at the one job it has. -->
        {#if deliveryWarnings.length && !deliveryWarningsHidden}
          <div class="delivery-warn" role="alert">
            <div class="delivery-warn-head">
              <strong
                >The last render finished, but {deliveryWarnings.length}
                {deliveryWarnings.length === 1 ? 'thing' : 'things'} did not deliver</strong
              >
              <button
                type="button"
                class="delivery-warn-close"
                title="Hide until the next render"
                onclick={() => (deliveryWarningsHidden = true)}>✕</button
              >
            </div>
            <!-- 🔴 No standing footnote. It used to end with "a shot named here has no take
                 because its file was rejected — retake it", printed under EVERY banner, and it was
                 wrong twice over: most entries name no shot at all (a failed mux, a seam drift),
                 and the one entry that does already ends with "Retake this shot to get its own file
                 back". So the constant either contradicted the list above it or repeated it. An
                 alarm that says the same thing whatever happened teaches the reader that it does
                 not know what happened. Each warning carries its own remedy or it carries none. -->
            <ul>
              {#each deliveryWarnings as w, i (i)}<li>{w}</li>{/each}
            </ul>
          </div>
        {/if}
        {#if selectedClip}
          {#if viewerExpanded || railHidden.current}
            {@render stageViewer(selectedClip)}
          {/if}

          <div class="stage-detail">
            {#if selectedFinding}
              <div class="finding-banner">
                <span
                  class="sev {selectedFinding.severity === 'high'
                    ? 'warn'
                    : selectedFinding.severity === 'medium'
                      ? 'warn'
                      : 'info'}">{selectedFinding.severity}</span
                >
                <p>{selectedFinding.issue}</p>
                <input
                  class="fnotes"
                  bind:value={selectedFinding.notes}
                  placeholder="add direction for the fix (optional)"
                />
                <button
                  class="btn sm primary"
                  onclick={() => applyFinding(selectedFinding)}
                  disabled={selectedFinding.fixing}
                  >{selectedFinding.fixing ? 'Fixing…' : 'Apply fix'}</button
                >
                <button
                  class="btn sm ghost"
                  onclick={() =>
                    (reviewFindings = reviewFindings.filter((x) => x !== selectedFinding))}
                  >Dismiss</button
                >
              </div>
            {/if}

            <!-- ⚠️ LTX only. The strip moved INTO the composer on engines that have one, so the
                 shot's identity, its budgets, its gist and its references sit ABOVE it — all things
                 both tabs need. LTX has no composer, so it keeps its own. -->
            {#if !caps.structuredPrompt}
              <div class="tabs">
                <button
                  class="tab"
                  class:on={activeTab === 'prompt'}
                  onclick={() => (activeTab = 'prompt')}>Prompt</button
                >
                <button
                  class="tab"
                  class:on={activeTab === 'details'}
                  onclick={() => (activeTab = 'details')}
                  >{capitalise(vocab.renderUnit)} details</button
                >
              </div>
            {/if}

            {#if caps.structuredPrompt}
              <!-- 🔴 Mounted ONCE and told which pane to draw, rather than mounted per tab. The
                   composer holds session state that must survive the switch — which sections the
                   author has touched (so a fresh shot does not open to six red errors) and which prose
                   box last had the caret (so a reference pill has somewhere to insert its ordinal). A
                   second mount would reset both every time somebody looked at the LoRA list. -->
              {#if packedGenerations[activeGenIndex]}
                <H3PromptComposer
                  gen={packedGenerations[activeGenIndex]}
                  genIndex={activeGenIndex}
                  genCount={packedGenerations.length}
                  bind:activeTab
                  anchorClip={selectedClip}
                  shotAudio={selectedShotAudio}
                  {promptBusy}
                  draftFailed={selectedClip ? draftFailedIds.includes(selectedClip.id) : false}
                  draftTries={DRAFT_TRIES}
                  {draftAllRunning}
                  bind:reworkText={reworkShotText}
                  reachableNote={reachableNoteFor(packedGenerations[activeGenIndex])}
                  loraCatalogue={h3Props?.loras ?? null}
                  projectLoras={h3Loras}
                  projectSteps={h3Steps}
                  projectWidth={width}
                  projectHeight={height}
                  onaddscopedreference={(kind) =>
                    addScopedReference(packedGenerations[activeGenIndex].id, kind)}
                  takeUrl={stageSource?.url ?? null}
                  revealIssues={revealH3Issues}
                  prev={activeGenIndex > 0 ? packedGenerations[activeGenIndex - 1] : undefined}
                  joinPinNote={composerPinNote}
                  {vocab}
                  references={h3References}
                  refCaps={h3RefCaps}
                  onupdate={(patch) =>
                    updateGeneration(packedGenerations[activeGenIndex].id, patch)}
                  onprompt={(block) =>
                    setGenerationPrompt(packedGenerations[activeGenIndex].id, block)}
                  onclip={patchClip}
                  onmerge={() => mergeGeneration(activeGenIndex)}
                  ondraft={() => runPromptLlm(draftClipPrompt)}
                  onreroll={() => runPromptLlm(rerollClipPrompt)}
                  onretrydraft={() => selectedClip && draftAllShots([selectedClip.id])}
                  onrework={reworkThisShot}
                  onsummarise={() => runPromptLlm(summariseGenerationGist)}
                  onhovertime={(sec) => (promptHoverSeconds = sec)}
                />
              {:else}
                <p class="hint">
                  Add a {vocab.subUnit ?? vocab.renderUnit} with a prompt — a {vocab.renderUnit} is made
                  of {vocab.subUnitPlural ?? vocab.renderUnitPlural}.
                </p>
              {/if}
            {:else}
              <!-- ⚠️ LTX only. On an engine with a structured prompt this pane IS the decoy described
                   above — its clip.prompt box never reaches the render — so it is not mounted there at
                   all, and its two H3 branches ("Something changes here", the render unit's audio
                   echo) moved into the composer with their comments. -->
              <ShotPromptPane
                {engine}
                beatsNodeMounted={props?.beats_enabled}
                clip={selectedClip}
                active={activeTab === 'prompt'}
                {fps}
                {characters}
                cast={selectedClipCast}
                refCount={selectedClipRefCount}
                carried={carriedByClip[selectedIndex] ?? new Set<string>()}
                shareNote={selectedClipShareNote}
                {identityRegime}
                beatWords={selectedBeatWords}
                {promptBusy}
                draftFailed={draftFailedIds.includes(selectedClip.id)}
                draftTries={DRAFT_TRIES}
                {draftAllRunning}
                bind:reworkShotText
                onchange={markDirty}
                ondraft={() => runPromptLlm(draftClipPrompt)}
                onreroll={() => runPromptLlm(rerollClipPrompt)}
                onsummarise={() => runPromptLlm(summariseClipGist)}
                onretrydraft={() => selectedClip && draftAllShots([selectedClip.id])}
                onaddbeat={addBeat}
                onremovebeat={removeBeat}
                onsnapbeat={snapBeat}
                oncast={(id) => selectedClip && toggleClipCast(selectedClip, id)}
                onresetcast={resetSelectedClipCast}
                onshotaudio={pickShotAudio}
                onclearshotaudio={clearShotAudio}
                onrework={reworkThisShot}
              />
            {/if}

            <!-- The per-beat details sit UNDER the render unit's own configuration on the same tab.
                 They are still per-clip on the wire — length, the image a beat opens on, its takes —
                 and the owner's list for this tab ("length, LoRAs, steps, canvas, seed, delete,
                 duplicate, retake, review, takes") spans both objects, so both are here. -->
            <ShotDetailsPane
              clip={selectedClip}
              active={activeTab === 'details'}
              shotIndex={selectedIndex}
              {fps}
              modes={MODE_META}
              projectHasSong={hasProjectSong}
              {props}
              {imgProps}
              {loras}
              {frameDrop}
              {frameDropNote}
              routing={clipVariantPlan.get(selectedClip.id)}
              loraNote={selectedClipLoraNote}
              {priorRenders}
              {characters}
              {snapBusy}
              {retakeNote}
              canRetake={canRetakeUnit}
              resumeNote={unavailableFor(engine, 'resume')}
              bind:retakeSeedInput
              bind:takesTier={desktopTakesTier}
              bind:promoteTargetId
              bind:takeVideoEls
              {takes}
              {takeUrls}
              {takesBusy}
              {criticNote}
              {promoteTarget}
              {promoteBusy}
              onchange={markDirty}
              onmode={setClipMode}
              onsceneimage={onSceneImageFiles}
              onv2vsource={onV2vSourceFiles}
              onclipref={onClipRefFiles}
              onremoveclipref={removeClipRef}
              {engine}
              onpin={onPinFiles}
              onendframe={onEndFrameFiles}
              onremovepin={removePin}
              onsnapshot={runSnapshot}
              onsnapshottoscene={snapshotToScene}
              onsnapshottoreference={snapshotToReference}
              onduplicate={duplicateSelected}
              ondelete={deleteSelected}
              onloadtakes={loadTakes}
              onpicktake={pickTake}
              onreview={runDailiesReview}
              onpromote={promoteTakeToReference}
            />

            <!-- Directly under the takes, because it is about what PICKING one just did. Mounted here
                 rather than inside `ShotDetailsPane` on purpose: it is a whole-timeline readout, not a
                 property of the selected shot, and it needs the same `activeTab` gate the pane applies
                 to itself internally. -->
            {#if activeTab === 'details'}
              <ContinuityPanel
                rows={joinRows}
                labelOf={unitLabel}
                {vocab}
                busy={joinsBusy}
                loaded={joinsLoaded}
                {running}
                onload={loadContinuity}
                onrepair={repairJoin}
              />
            {/if}
          </div>
        {/if}

        {#if clips.length === 0}
          <div class="empty">
            <div class="big"></div>
            <h2>Start a new film</h2>
            <p>
              Describe what you want and the director writes the shot list — or drop in an audio
              track and cut to the lyrics. You can also build the timeline by hand.
            </p>
            <div class="acts">
              <button
                class="btn primary"
                onclick={() => {
                  foldAuto = true;
                }}>Auto-direct from a brief</button
              >
              <button class="btn" onclick={addClip}>Add the first shot</button>
              {#if caps.projectSoundtrack}
                <button class="btn ghost" onclick={() => (audioOpen = true)}
                  >Start from audio</button
                >
              {/if}
            </div>
          </div>
        {/if}
      </main>
    </div>

    <TimelineBar
      bind:clips
      bind:selectedIds
      bind:selectedPin
      bind:pxPerSec
      bind:windowSeconds
      bind:fitTimeline
      bind:playhead
      bind:playing
      bind:snap
      bind:batchText
      {audioSrc}
      {vocab}
      {lyrics}
      {waveform}
      {waveDuration}
      {fps}
      {zoomSel}
      {totalDuration}
      {selectedIndex}
      {canUndo}
      {canRedo}
      {promptBusy}
      {storyboarding}
      imageEngineReady={imgProps ? imgProps.healthy : true}
      {extending}
      anyDraftFailed={draftFailedIds.length > 0}
      {shotStatus}
      {proxyStatus}
      findingSeverity={(id) => {
        const f = reviewFindings.find((x) => x.clipId === id);
        return f ? (f.severity === 'low' ? 'info' : 'warn') : null;
      }}
      draftFailed={(id) => draftFailedIds.includes(id)}
      onchange={markDirty}
      onadd={addClip}
      onsplit={splitAtPlayhead}
      onmerge={mergeSelected}
      onduplicate={duplicateSelected}
      ondelete={deleteSelected}
      onundo={undo}
      onredo={redo}
      onzoom={zoom}
      onzoomwindow={setZoomWindow}
      onopenaudio={() => (audioOpen = true)}
      projectSoundtrack={caps.projectSoundtrack}
      generations={packedGenerations}
      mark={promptMark}
      onopengeneration={(i) => {
        // Selects that render unit's first beat, which is what `activeGenIndex` now derives from —
        // there is no separate index to keep in step, and therefore no way for the two to disagree.
        showGeneration(i);
        stageEl?.scrollTo({ top: 0 });
      }}
      onreworkselected={reworkSelected}
      onstoryboardselected={storyboardSelected}
      oncontinuescene={continueScene}
    />
  {/if}

  <!-- ── OVERLAYS ─────────────────────────────────────────── -->
  <!-- Only a Save the user pressed gets this far — a background autosave conflict stays in the
       warn-zone banner above. Dismissing leaves the banner and the unsaved work both in place. -->
  {#if staleDoc?.prompt}
    <div class="overlay" role="presentation">
      <div class="modal" style="height:auto;max-width:460px">
        <div class="chat-h">
          <div class="chat-badge">⚠️</div>
          <div style="flex:1">
            <b>This project changed elsewhere</b><small
              >Saved on another device while you were editing</small
            >
          </div>
        </div>
        <div class="sheet-body">
          <p class="hint">
            Your copy was not written — nothing here has been lost. Reload to take the other copy
            and discard the edits on this screen, or overwrite it with what you have.
          </p>
          <div class="inline-row" style="flex-wrap:wrap;gap:7px;margin-top:12px">
            <button class="btn sm" onclick={() => void reloadStale()} disabled={saving}
              >Reload theirs</button
            >
            <button class="btn sm danger" onclick={() => void overwriteStale()} disabled={saving}
              >Overwrite with mine</button
            >
            <button
              class="btn sm ghost"
              onclick={() => staleDoc && (staleDoc = { ...staleDoc, prompt: false })}
              >Keep editing</button
            >
          </div>
        </div>
      </div>
    </div>
  {/if}
  {#if projectMenuOpen}
    <ProjectMenu
      bind:projectName
      {projectId}
      {projects}
      {saveStatus}
      {saving}
      stale={!!staleDoc}
      {titling}
      {duplicating}
      onclose={() => (projectMenuOpen = false)}
      onopen={(id) => {
        loadProject(id);
        projectMenuOpen = false;
      }}
      onnew={() => {
        projectMenuOpen = false;
        newProject();
      }}
      oninput={onNameInput}
      oncommitname={() => void commitRename()}
      onautotitle={() => autoTitle()}
      onsave={() => saveProject()}
      onduplicate={duplicateProject}
      onremove={() => projectId && removeProject(projectId)}
    />
  {/if}

  {#if settingsOpen}
    <SettingsSheet
      bind:engine
      beatsNodeMounted={props?.beats_enabled}
      bind:shotSeconds
      bind:fps
      bind:model
      bind:loras
      bind:frameDrop
      bind:beatsEnabled
      bind:critiqueEnabled
      bind:critiqueAutoRetake
      bind:critiqueThreshold
      bind:critiqueMaxRetakes
      bind:upscalerRatio
      bind:detailStrength
      bind:resPreset
      bind:width
      bind:height
      bind:proxyW
      bind:proxyH
      bind:proxySteps
      bind:h3Preview
      bind:h3Loras
      bind:h3LoraMultipliers
      bind:h3Steps
      bind:cfgScale
      bind:seed
      bind:negativePrompt
      bind:customBaseSampler
      bind:customBaseSigmasText
      bind:customStages
      {props}
      {h3Props}
      hasRenders={renderRows.length > 0}
      priorSourceShots={clips.filter((c) => c.v2v_prior_job_id).length}
      rendering={renderIsMine}
      {qualityProfile}
      {snapNote}
      {snapDivisor}
      {sizeSel}
      {sizeTiers}
      {resPlan}
      {previewSizeSel}
      {customBaseSteps}
      {frameDropNote}
      onchange={markDirty}
      onclose={() => (settingsOpen = false)}
      onprofile={changeProfile}
      onengine={changeEngine}
      onsize={selectSize}
      onpreviewsize={selectPreviewSize}
      onsnap={snapTargetToProfile}
      onaddstage={addStage}
      onremovestage={removeStage}
    />
  {/if}

  <!-- Present on BOTH engines. This was gated off on H3 by `audioInput`, on the reasoning that a
       jointly-generated soundtrack leaves a supplied track nowhere to go — but the sheet's whole
       job is the authoring spine (waveform, STT lyric lane, cuts snapping to word onsets), none of
       which the engine ever sees. What differs is only how the track REACHES the engine
       (`caps.soundtrackRoute`), and on H3 it does reach it: as a per-generation `<Audio N>` slice.
       See the audio block in `lib/director/engine.ts`. -->
  {#if audioOpen && caps.projectSoundtrack}
    <AudioSheet
      bind:audioKind
      bind:h3SoundtrackMode
      bind:h3SoundtrackDiegetic
      soundtrackIsReference={caps.soundtrackRoute === 'audio_reference'}
      fitNote={soundtrackNote}
      {audioFile}
      {waveDuration}
      {lyrics}
      {transcribing}
      onclose={() => (audioOpen = false)}
      onfiles={onAudioFiles}
      onclear={clearAudio}
      ontranscribe={transcribe}
      onchange={markDirty}
    />
  {/if}

  {#if rendersOpen}
    <RendersDrawer
      bind:continueFromLast
      bind:previewShots
      {vocab}
      historyService={historyServiceFor(engine)}
      {cutSource}
      {resultUrl}
      {proxyResultUrl}
      {partialShots}
      {proxyShots}
      proxyW={previewW}
      proxyH={previewH}
      previewFullRes={caps.previewIsFullRes}
      expectedShots={renderUnits}
      {outW}
      {outH}
      {running}
      {renderIsMine}
      {renderElsewhere}
      {renderOwnerName}
      {activeRenderTier}
      {elapsed}
      {activeExpectedSec}
      {progressPct}
      telemetry={h3Telemetry}
      {h3Plan}
      reachableNote={renderReachableNote}
      {promptedCount}
      {queueKind}
      bind:queueOptions={queueOpts}
      {queueAccepted}
      {nothingToRender}
      {continueMisaligned}
      {v2vMissingSource}
      {priorRenders}
      {summary}
      {summaryStale}
      {summarizing}
      {renderError}
      {restoreNote}
      {historyKey}
      {tierPref}
      {renderTier}
      narrow={compact.current}
      resume={canResume}
      resumeNote={unavailableFor(engine, 'resume')}
      canRetake={canRetakeUnit}
      {retakeNote}
      {bank}
      {bankBusy}
      {bankError}
      onloadbank={bankListable ? loadBank : null}
      {partialShotNo}
      onclose={() => (rendersOpen = false)}
      onrenderunit={renderMissingUnit}
      onshot={(idx) => {
        const c = partialClip(idx);
        if (c) {
          selectedIds = [c.id];
          rendersOpen = false;
        }
      }}
      onrender={renderPrimary}
      onquickpreview={quickPreview}
      onretakelast={retakeLastShot}
      onsummarise={summarizeShots}
      oncancel={cancelRender}
      ontier={setTierPref}
      onrestore={restore}
    />
  {/if}

  {#if reviewOpen}
    <ReviewFindings
      findings={reviewFindings}
      {fixingAll}
      shotIndexOf={(id) => clips.findIndex((c) => c.id === id)}
      onclose={() => (reviewOpen = false)}
      ongoto={gotoShot}
      onapply={applyFinding}
      onfixall={fixAllFindings}
      ondismiss={(item) => (reviewFindings = reviewFindings.filter((x) => x !== item))}
    />
  {/if}

  <!-- Every block in the project, as text, with one find-and-replace across all of them. Mounted
       only while open: it holds one CodeMirror per render unit. -->
  {#if promptsOpen && caps.structuredPrompt}
    <H3PromptsScreen
      docs={h3PromptDocs}
      {vocab}
      onprompt={setGenerationPrompt}
      oncapture={captureRestore}
      ongoto={(id) => {
        const clip = packedGenerations.find((g) => g.id === id)?.shots[0]?.clip;
        if (clip) gotoShot(clip.id);
        closePrompts();
      }}
      onclose={closePrompts}
    />
  {/if}

  {#if expandPanel}
    <div
      class="overlay"
      role="presentation"
      onclick={(e) => e.target === e.currentTarget && (expandPanel = null)}
    >
      <div class="modal panel-modal">
        <div class="chat-h">
          <div class="chat-badge">
            {expandPanel === 'story'
              ? '📖'
              : expandPanel === 'cast'
                ? '🎭'
                : expandPanel === 'auto'
                  ? '🎬'
                  : '✎'}
          </div>
          <div style="flex:1">
            <b
              >{expandPanel === 'story'
                ? 'Story context'
                : expandPanel === 'cast'
                  ? 'Cast'
                  : expandPanel === 'auto'
                    ? 'Auto-direct'
                    : 'Revise & review'}</b
            ><small>Roomy editor — same fields as the rail</small>
          </div>
          <button class="btn icon-only ghost" onclick={() => (expandPanel = null)}>✕</button>
        </div>
        <div class="panel-tabs">
          <button
            class="tab"
            class:on={expandPanel === 'story'}
            onclick={() => (expandPanel = 'story')}>Story context</button
          >
          <button
            class="tab"
            class:on={expandPanel === 'cast'}
            onclick={() => (expandPanel = 'cast')}>Cast</button
          >
          <button
            class="tab"
            class:on={expandPanel === 'auto'}
            onclick={() => (expandPanel = 'auto')}>Auto-direct</button
          >
          <button
            class="tab"
            class:on={expandPanel === 'revise'}
            onclick={() => (expandPanel = 'revise')}>Revise &amp; review</button
          >
        </div>
        <div class="sheet-body panel-modal-body">
          {#if expandPanel === 'story'}{@render storyBody()}{:else if expandPanel === 'cast'}{@render castBody()}{:else if expandPanel === 'auto'}{@render autoBody()}{:else}{@render reviseBody()}{/if}
        </div>
      </div>
    </div>
  {/if}

  {#if planReview}
    <PlanReviewSheet
      summary={planReview.summary}
      clips={planReview.timeline.clips}
      defaultSeconds={shotSeconds}
      {vocab}
      onGo={commitPlan}
      onCancel={() => (planReview = null)}
    />
  {/if}

  <!-- The phone draws the same restore point in its own dock above the tab bar (`MobileShell`), so
       this one would be a second toast for one undo. -->
  {#if showAiToast && aiRestore && !compact.current}
    <div class="ai-toast">
      <span>↩ <b>{aiRestore.label}</b></span><button class="undo" onclick={revertAiChange}
        >Undo</button
      ><button class="x" onclick={dismissAiToast}>Dismiss</button>
    </div>
  {/if}
</div>

{#if scopePrompt}
  <RenderScopeDialog
    plan={scopePrompt.plan}
    {vocab}
    secondsPerGeneration={h3Plan?.total_eta_seconds && packedGenerations.length
      ? h3Plan.total_eta_seconds / packedGenerations.length
      : null}
    onchoose={answerRenderScope}
  />
{/if}

{#if chatOpen}
  <ChatModal
    messages={chatMessages}
    busy={chatBusy}
    onSend={sendChat}
    onClear={clearChat}
    onRevertEdit={undoChatEdit}
    onClose={() => {
      chatOpen = false;
    }}
  />
{/if}

<!--
  ⚠️ A SCOPED block, on purpose, and the first in this component. `workspace.css` is ~2,400 lines of
  global `.dw …` rules and has already had two class-name collisions that threw elements to the top
  of the viewport (see `ContinuityPanel.svelte`). Svelte scopes these four names here so they cannot
  become a third. Nothing else in this file is styled locally.
-->
<style>
  .delivery-warn {
    /* 🔴 Load-bearing. On >=1240px `.dw .stage.split` is a two-column GRID, and this is the stage's
       first child — without the span it would take column 1 and push the viewer into column 2,
       wrecking the layout at exactly the moment the user is being told something went wrong. */
    grid-column: 1 / -1;
    border: 1px solid var(--danger, #e06c6c);
    border-left-width: 3px;
    border-radius: 8px;
    background: color-mix(in srgb, var(--danger, #e06c6c) 9%, var(--bg-2, #14161a));
    padding: 10px 13px;
    font-size: 12px;
    line-height: 1.45;
  }
  .delivery-warn-head {
    display: flex;
    align-items: baseline;
    gap: 10px;
    justify-content: space-between;
  }
  .delivery-warn-head strong {
    color: var(--danger, #e06c6c);
  }
  .delivery-warn-close {
    flex: none;
    border: 0;
    background: none;
    color: var(--fg-2, #8a8f98);
    cursor: pointer;
    font-size: 13px;
    line-height: 1;
    padding: 2px 4px;
  }
  .delivery-warn-close:hover {
    color: var(--fg-1);
  }
  .delivery-warn ul {
    margin: 6px 0 0;
    padding-left: 18px;
  }
  .delivery-warn li + li {
    margin-top: 4px;
  }
</style>
