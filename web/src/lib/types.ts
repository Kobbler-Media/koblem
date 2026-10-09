// Type-only, so the cycle with `client.ts` is erased at build: the queue's own
// vocabulary lives there and must not be copied here.
import type { BlockedReason, JobState } from '$lib/api/client';

export interface ReviewCandidate {
  name: string;
  kobload_accepted: boolean;
  kobload_score: number;
  /** Full parsed-quality block from kobload's filename parser. */
  parsed_quality?: ParsedQuality;
  parsed_season?: string;
  parsed_episode?: string;
  parsed_title?: string;
  parsed_year?: number;
  publish_date?: string;
  seeders?: number;
  votes?: number;
  grabs?: number;
}

/** Mirror of `nats_jobs::ParsedQuality` — only the fields the UI surfaces today. */
export interface ParsedQuality {
  resolution?: string;
  source?: string;
  streaming_service?: string;
  codec?: string;
  audio?: string;
  language?: string;
  release_group?: string;
  is_proper?: boolean;
  is_repack?: boolean;
  is_remux?: boolean;
  is_hdr?: boolean;
  is_hdr10?: boolean;
  is_hdr10plus?: boolean;
  is_dolby_vision?: boolean;
  is_3d?: boolean;
  is_hardcoded_subs?: boolean;
}

export interface QualityProfile {
  min_resolution?: string;
  max_resolution?: string;
  preferred_sources?: string[];
  preferred_codecs?: string[];
  preferred_audio?: string[];
  preferred_languages?: string[];
  rejected_sources?: string[];
  rejected_languages?: string[];
  rejected_terms?: string[];
  reject_hardcoded_subs?: boolean;
  reject_remux?: boolean;
}

export type WantContext =
  | {
      kind: 'tv_episode';
      season: string;
      episode: string;
      episode_title?: string;
      air_date?: string;
      year?: number;
    }
  | {
      kind: 'tv_season';
      season: string;
      year?: number;
      episodes_needed?: {
        season: string;
        episode: string;
        title?: string;
        air_date?: string;
      }[];
    }
  | { kind: 'movie'; year?: number }
  | {
      kind: 'sport_event';
      league_name: string;
      season: string;
      round?: number;
      location?: string;
      session_type?: string;
      event_title?: string;
      event_date?: string;
    }
  | { kind: 'other' };

export interface ReviewRequest {
  source_id?: string;
  wanted_title: string;
  want?: WantContext;
  quality_profile?: QualityProfile;
  candidates: ReviewCandidate[];
  sampling?: SamplingConfig;
}

/**
 * One entry in the LLM's raw accept list (before bucketing).
 */
export interface ReviewAccept {
  idx: number;
  reason: string;
}

/**
 * One row of the server-computed diff between kobload's accept set and the
 * LLM's accept set. Used in all three buckets (`agreed`, `vetoed`, `promoted`).
 */
export interface ReviewDelta {
  /** 1-based index into the candidate list. */
  idx: number;
  /** Release filename. */
  name: string;
  /** LLM's reason (always present for `agreed` / `promoted`, null for `vetoed`). */
  reason?: string | null;
}

export interface ReviewResponse {
  /** kobload accepted AND LLM accepted — high-confidence picks. */
  agreed: ReviewDelta[];
  /** kobload accepted but the LLM did NOT — false positives the LLM caught. */
  vetoed: ReviewDelta[];
  /** kobload rejected but the LLM accepted — rescued false negatives. */
  promoted: ReviewDelta[];
  /** The raw `accept` array as parsed from the LLM JSON, before bucketing. */
  accepts?: ReviewAccept[];
  /** 1-based indices the LLM emitted that fell outside the candidate range. */
  dropped_indices?: number[];
  context_used: string[];
  /** Batch-level observation from the LLM (and any truncation/parse warnings). */
  notes?: string;
  debug_prompt?: string;
  /** The verbatim LLM JSON response (for debugging schema drift / parse failures). */
  raw_llm_response?: string;
  /** llama-server's `usage.prompt_tokens` (input size). */
  prompt_tokens?: number;
  /** llama-server's `usage.completion_tokens` (output size). */
  completion_tokens?: number;
  context_assembly_ms?: number;
  llm_inference_ms?: number;
  session_id?: string;
}

export interface GenericRule {
  id: string;
  scope: string;
  rule: string;
  created_at: string;
}

export interface EntityRule {
  id: string;
  entity_id: string;
  entity_type: string;
  entity_name?: string;
  rule: string;
  created_at: string;
}

export interface DecisionCorrection {
  id: string;
  release_name: string;
  entity_id?: string;
  model_decision: string;
  correct_decision: string;
  reason: string;
  created_at: string;
}

export interface Setting {
  key: string;
  value: unknown;
}

export interface ShowSummary {
  id: string;
  title: string;
  year?: number;
  status?: string;
  episode_count?: number;
}

export interface ShowDetail {
  id: string;
  title: string;
  year?: number;
  status?: string;
  network?: string;
  content_category: string;
  quality_profile_id?: string;
  seasons: SeasonDetail[];
}

export interface SeasonDetail {
  id: string;
  season_identifier: string;
  episodes: EpisodeDetail[];
}

export interface EpisodeDetail {
  id: string;
  episode_identifier: string;
  title?: string;
  air_date?: string;
  want_status?: string;
}

// Playground types
export interface InterpretInput {
  name: string;
  media_kind: string;
}

export interface InterpretedQuality {
  resolution?: string;
  source?: string;
  codec?: string;
  audio?: string;
  streaming_service?: string;
  is_proper: boolean;
  is_repack: boolean;
  is_remux: boolean;
  is_hdr: boolean;
  is_dolby_vision: boolean;
  is_hardcoded_subs: boolean;
}

export interface InterpretedRelease {
  input: string;
  title: string;
  media: unknown;
  quality: InterpretedQuality;
  release_group?: string;
  confidence: number;
}

export interface ScoredRelease {
  guid: string;
  name: string;
  total_score: number;
  match_score: number;
  quality_score: number;
  accepted: boolean;
  reject_reason?: string;
  quality: InterpretedQuality;
  release_group?: string;
  indexer?: string;
  size?: number;
  matched_season?: string;
  matched_episode?: string;
}

export interface ReleaseSearchResults {
  candidates: ScoredRelease[];
  rejected: ScoredRelease[];
  best_pick?: ScoredRelease;
  total_found: number;
  total_accepted: number;
}

export interface SearchAndReviewResponse {
  search: ReleaseSearchResults;
  review: ReviewResponse;
  session_id?: string;
}

export interface CompareRequest {
  source_id?: string;
  /** Discriminator from older clients ("tv" / "movie" / "sport"). Ignored by
   * koblem — the backend infers the WantContext variant from the season/episode
   * fields below. Kept on the type for back-compat. */
  media_type?: string;
  wanted_title: string;
  wanted_year?: number;
  wanted_season?: string;
  wanted_episode?: string;
  wanted_episode_title?: string;
  releases: { name: string; media_kind: string; publish_date?: string }[];
  sampling?: SamplingConfig;
}

export interface CompareResponse {
  review: ReviewResponse;
  interpreted: InterpretedRelease[];
  session_id?: string;
}

// Review sessions / history
export interface ReviewSessionSummary {
  id: string;
  wanted_summary: string;
  agreed_count: number;
  vetoed_count: number;
  promoted_count: number;
  /** Non-zero only for legacy (pre-v3) sessions that stored verdicts as a
   * flat array rather than three buckets. */
  legacy_count: number;
  context_assembly_ms?: number;
  llm_inference_ms?: number;
  created_at: string;
}

export interface ReviewSessionDetail {
  id: string;
  request: unknown;
  wanted_summary: string;
  context_used: string[];
  prompt_sent: string;
  llm_response?: string;
  verdicts?: Partial<ReviewResponse>;
  notes?: string;
  context_assembly_ms?: number;
  llm_inference_ms?: number;
  created_at: string;
  /** FK to the underlying llm_logs row, when one exists. */
  llm_log_id?: string;
  /** CoT trace from llm_logs.response_reasoning_content. */
  thinking?: string;
}

// Prompt templates (per-media-kind, with default fallback)
export type MediaKindKey =
  'default' | 'tv_episode' | 'tv_season' | 'movie' | 'sport_event' | 'other';

export type PromptSlot = 'system' | 'user';

export interface PromptTemplatesOverview {
  media_kinds: {
    media_kind: MediaKindKey;
    label: string;
    template_overrides: number;
    variable_overrides: number;
  }[];
}

export interface PromptTemplateBody {
  media_kind: MediaKindKey;
  slot: PromptSlot;
  body: string;
  is_overridden: boolean;
  has_db_row: boolean;
  updated_at?: string;
  default_body: string;
}

export interface PromptVariableSummary {
  media_kind: MediaKindKey;
  name: string;
  body: string;
  is_overridden: boolean;
  has_db_row: boolean;
  is_seeded: boolean;
  updated_at?: string;
  default_body: string;
}

export interface PromptVariableListItem {
  name: string;
  /** Resolved body the engine would use right now: per-kind row → default
   * row → Rust seeded constant. The prompts page binds its edit textarea
   * straight to this so a kind-switch is one round-trip total. */
  body: string;
  is_seeded: boolean;
  has_per_kind_override: boolean;
  has_default_override: boolean;
  updated_at?: string;
  /** Rust-baked seed body so "reset to factory" is a no-fetch action. */
  default_body: string;
}

export interface PromptFixtureSummary {
  id: string;
  label: string;
  media_kind: MediaKindKey;
}

export interface RenderPreviewRequest {
  media_kind: MediaKindKey;
  draft_system?: string;
  draft_user?: string;
  draft_variables?: Record<string, string>;
  fixture_id?: string;
  fixture_payload?: unknown;
}

export interface RenderPreviewResponse {
  rendered_system?: string;
  rendered_user?: string;
  errors: string[];
}

export interface ContextSchemaField {
  path: string;
  kind: string;
  note: string;
}

// Grammars (DB-stored named GBNF grammars + built-in `off` and `review`)
export interface GrammarEntry {
  name: string;
  gbnf: string;
  description?: string;
  /** True for the synthetic built-in entries (`off`, `review`). */
  is_builtin: boolean;
  created_at?: string;
  updated_at?: string;
}

export interface UpsertGrammarRequest {
  name: string;
  gbnf: string;
  description?: string;
}

// Sampling (mirror of koblem-client's SamplingConfig + SamplingPreset).
//
// Thinking-mode presets are omitted because testing them was disappointing.
// 🔴 NOT because of any `--reasoning off` flag — this comment used to claim
// there was one and there is not, on either of koblem's llama-servers; both run
// `--jinja` and nothing else on this axis. Thinking is turned off PER REQUEST
// via `enable_thinking: false`, which koblem writes into
// `chat_template_kwargs.enable_thinking`.
export type SamplingPreset = 'deterministic' | 'qwen_instruct_general' | 'qwen_instruct_reasoning';

// Thinking budget for the chat template (`chat_template_kwargs.reasoning_effort`).
// 🔴 EXACTLY three values, and `'none'` is NOT one of them: Qwen3.8's chat template
// raises on `reasoning_effort: "none"` and llama-server answers HTTP 500 — measured.
// To disable thinking use `enable_thinking: false`, which is measured working.
export type ReasoningEffort = 'xhigh' | 'medium' | 'low';

export interface SamplingConfig {
  sampling_preset?: SamplingPreset;
  temperature?: number;
  top_p?: number;
  top_k?: number;
  min_p?: number;
  presence_penalty?: number;
  repetition_penalty?: number;
  max_tokens?: number;
  /** Toggle for the chat template's `<think>\n` auto-injector. Structured
   * endpoints baseline this to `false`; chat / proxy callers pick. */
  enable_thinking?: boolean;
  /** Thinking budget. Inert (and not sent) while thinking is off. `undefined`
   * = inherit the template's own default (`xhigh` on Qwen3.8). */
  reasoning_effort?: ReasoningEffort;
  /** Grammar mode: `"off"`, `"review"`, or any user-defined grammar name
   * from the `grammars` table. Wire key is `grammar_mode` to keep the
   * mode selector distinct from llama.cpp's raw-GBNF `grammar` field. */
  grammar_mode?: string;
}

// LLM logs
export type LlmLogStatus = 'running' | 'complete' | 'error' | 'cancelled';

export interface LlmLogSummary {
  id: string;
  caller?: string;
  message_preview: string;
  streaming: boolean;
  duration_ms?: number;
  status: LlmLogStatus;
  completion_tokens?: number;
  created_at: string;
  updated_at: string;
}

export interface LlmLogDetail {
  id: string;
  caller?: string;
  request_messages: unknown;
  request_tools?: unknown;
  request_temperature?: number;
  request_max_tokens?: number;
  request_top_p?: number;
  request_top_k?: number;
  request_min_p?: number;
  request_presence_penalty?: number;
  request_repetition_penalty?: number;
  request_enable_thinking?: boolean;
  /** Free-text sampling tag, NOT necessarily a `SamplingPreset`: it carries the
   * resolved preset name when the call had one, and otherwise the per-surface
   * settings key the call's sampling came from (`sampling_director`, …), so a
   * bad row is attributable to a knob. Narrow it before feeding it back into a
   * `SamplingConfig`. */
  sampling_preset?: string;
  /** Grammar mode tag in effect for this request: `"off"`, `"review"`, or
   * a user-defined grammar name. */
  request_grammar?: string;
  streaming: boolean;
  response_content?: string;
  response_reasoning_content?: string;
  response_tool_calls?: unknown;
  finish_reason?: string;
  tool_rewrite_applied: boolean;
  duration_ms?: number;
  prompt_tokens?: number;
  completion_tokens?: number;
  status: LlmLogStatus;
  error_message?: string;
  created_at: string;
  updated_at: string;
}

// Raw chat — sampling fields live at the top level so the wire shape matches
// OpenAI-style `temperature`/`top_p`/…/`sampling_preset` alongside `prompt`.
export interface ChatRequest {
  prompt: string;
  system?: string;
  /** Base64 data URLs (`data:image/<mime>;base64,...`) for vision-capable models. */
  images?: string[];
  /**
   * Base64 data URLs (`data:audio/<mime>;base64,...`) for audio-capable models.
   *
   * 🔴 DROPPED, not rejected, when the loaded model has no audio encoder (Beefy
   * LLM mode runs Qwen3.8, whose mmproj is vision-only). The turn still runs and
   * {@link ChatResponse.warnings} says what was lost. Pre-empt it with
   * `/health`'s `llama_audio_input` — see `audioInputAllowed` in the client.
   */
  audio?: string[];
  sampling_preset?: SamplingPreset;
  temperature?: number;
  top_p?: number;
  top_k?: number;
  min_p?: number;
  presence_penalty?: number;
  repetition_penalty?: number;
  max_tokens?: number;
  enable_thinking?: boolean;
  /** Grammar mode (see SamplingConfig.grammar_mode). */
  grammar_mode?: string;
}

export interface ChatResponse {
  response: string;
  prompt_tokens?: number;
  completion_tokens?: number;
  duration_ms?: number;
  /**
   * Non-fatal things the server did to the request. The turn still happened —
   * today's only entry is "your audio was dropped, the model has no audio
   * encoder". Absent on older koblem builds, so always guard the read.
   */
  warnings?: string[];
}

// Search request for playground
export interface PlaygroundSearchRequest {
  show_id: string;
  season_id?: string;
  episode_ids?: string[];
  sampling?: SamplingConfig;
}

// Testbed types
export interface TestScenario {
  id: string;
  name: string;
  description: string;
  category: string;
  candidate_count: number;
}

export interface ExpectedAction {
  release_name: string;
  expected: string;
  reason: string;
}

export interface WrongVerdict {
  release_name: string;
  expected: string;
  /** Where the LLM placed it: 'agreed' | 'vetoed' | 'promoted' | 'silent'. */
  actual_bucket: string;
  /** 'missed' (expected accept, LLM rejected) or 'spurious' (expected reject, LLM accepted). */
  sub_reason: string;
}

/**
 * Four-bucket scoring breakdown for a testbed scenario run.
 * Replaces the old binary passed/failed counts.
 */
export interface ScoringBreakdown {
  agreed_correct: number;
  promoted_correct: number;
  vetoed_correct: number;
  silent_correct: number;
  wrong: WrongVerdict[];
}

export interface TestResult {
  scenario_id: string;
  scenario_name: string;
  review: ReviewResponse;
  expected: ExpectedAction[];
  scoring: ScoringBreakdown;
  session_id?: string;
}

/** One output file of a generation. `kind` drives the history preview. */
export interface GenerationOutput {
  filename: string;
  mime: string;
  size_bytes: number;
  kind: 'video' | 'audio' | 'image' | 'transcript';
}

/** A user-facing generation gallery entry (per-service history row). */
export interface Generation {
  id: string;
  service: string;
  endpoint: string;
  request_meta: Record<string, unknown>;
  outputs: GenerationOutput[];
  title?: string | null;
  created_at: string;
}

export interface GenerationList {
  items: Generation[];
  total: number;
}

// ── Video → Upscale (LTX-2.5) ─────────────────────────────────────────────
//
// 🔴 A RESTORER, not a generator: an mp4 in, a bigger mp4 out. Nothing is prompted — both text
// encodes in the graph are empty and both CFGs are 1.0 — so every generative field is a 501.
//
// 🔴 TWO KNOBS. `noise` is how much of the picture the refine may redraw and `detailer` toggles an
// IC-LoRA that is measurably free. This replaced SeedVR2 (six knobs, 230 s, visible boiling) on
// measurements: 48 s and 0.84x the temporal flicker of a bicubic control.

/** How the output size is chosen. 🔴 The upsampler is a FIXED 2×, so the target is reached by
 *  resizing the source to half of it — which is why deliverable sizes are multiples of 64. */
export type UpscaleScaleMode = 'multiplier' | 'shorter_side' | 'longer_side' | 'dimensions';

/** Where the source comes from. Both arms end up staged in the shared hand-off directory. */
export type UpscaleSource =
  | { kind: 'upload'; token: string }
  /** 🔴 COORDINATES, not a filename — the same way the rest of the Director API addresses a take. */
  | {
      kind: 'director_take';
      project_id: string;
      tier: 'proxy' | 'final';
      shot: number;
      variant: number;
    };

export interface UpscaleGenerateRequest {
  source: UpscaleSource;
  scale_mode?: UpscaleScaleMode;
  multiplier?: number;
  shorter_size?: number;
  longer_size?: number;
  width?: number;
  height?: number;
  /** 🔴 0.0–1.0, default 0.4. The refine's starting sigma: how much the model may redraw.
   *  0 skips the refine entirely (no transformer, nothing that can invent). 0.85 is where the
   *  official two-stage workflow's SECOND stage starts and where it grew a subject a pair of
   *  glasses; 0.909375 is an ordinary step in Lightricks' own stage-1 schedule, which is why the
   *  scale does not stop at 0.85; 1.0 is pure noise with the input gone entirely. */
  noise?: number;
  /** The detailer IC-LoRA. Free, and refused at `noise: 0` where there is no transformer. */
  detailer?: boolean;
  /** `nearest-exact` | `bilinear` | `area` | `bicubic` | `lanczos`. Absent = the engine's lanczos. */
  scale_method?: string;
  start_time?: number;
  duration?: number;
  /** Normally omitted — the server computes the split from the source and the engine's RAM guard. */
  segment_seconds?: number;
}

export interface UpscaleProbedGeometry {
  width: number;
  height: number;
  fps: number;
  duration_seconds: number;
  has_audio: boolean;
}

/** The pre-flight split. 🔴 `seams` is the number that matters: each segment is its own refine, so
 *  every join is a place the model can choose different texture. `segments: 1` is the case to aim
 *  for. */
export interface UpscaleSegmentPlan {
  segment_seconds: number | null;
  segments: number;
  output_width: number;
  output_height: number;
  window_seconds: number;
  frames_per_prompt: number;
  seams: number;
}

export interface UpscalePlanResponse {
  source: UpscaleProbedGeometry;
  plan: UpscaleSegmentPlan;
  guard: { max_estimate_bytes: number; copies: number };
}

export interface UpscaleUploadResponse {
  token: string;
  filename: string;
  bytes: number;
  geometry: UpscaleProbedGeometry | null;
}

/** `joining` is its own state on purpose: the card is already free and a UI that still says
 *  "rendering" is telling the user to wait for something that finished. */
export type UpscaleJobState = 'queued' | 'rendering' | 'joining' | 'done' | 'failed' | 'cancelled';

export interface UpscaleSegmentOut {
  index: number;
  filename: string;
  bytes: number;
  start_time: number | null;
  duration: number | null;
  width: number | null;
  height: number | null;
  duration_seconds: number | null;
}

export interface UpscaleProgress {
  phase: string | null;
  current_step: number | null;
  /** Three, when there is a refine — the schedule is three sigmas down to zero and `noise` scales
   *  them rather than adding steps. `null` on the `noise: 0` arm, where there is no sampler and a
   *  0/3 bar would be a lie. ⚠️ Also a small share of the wall clock: the tiled VAE decode
   *  dominates and reports nothing. */
  total_steps: number | null;
  progress: number | null;
  queue_position: number | null;
  /** 🔴 Only set while the row is actually running. `segments_done` / `segment_count` on the job
   *  are the queue row's own unit counters and are always there — count from those. */
  segment: number | null;
  segment_count: number | null;
}

export interface UpscaleJob {
  id: string;
  state: UpscaleJobState;
  server_job_id: string | null;
  segments_done: number;
  segment_count: number;
  progress: UpscaleProgress;
  segments: UpscaleSegmentOut[];
  output_filename: string | null;
  output_bytes: number | null;
  plan: UpscaleSegmentPlan | null;
  source_label: string | null;
  warnings: string[];
  error: string | null;
  gate_wait_ms: number | null;
  duration_ms: number | null;
  cancel_requested: boolean;
  blocked_reason: BlockedReason | null;
  eta_start: string | null;
}

// ── 3D → Pixal3D (longcat-rig) ────────────────────────────────────────────
//
// 🔴 The timings decide the whole panel: a rig is ~450 s and happens ONCE per character; a motion
// is ~11 s; a retarget is 0.22 s. So the UI is a character library plus a fast prompt loop, not a
// form with a submit button.
//
// ⚠️ There is NO progress on a rig. The engine's job record publishes queued/running/completed and
// nothing else — no phase, no percentage. Anything resembling a progress bar here must be elapsed
// time against `Pixal3dProps['timings']`, never a fill.

export type Pixal3dJobState =
  | 'queued'
  | 'running'
  /** koblem is pulling the delivered GLBs onto its own disk. The card is already free. */
  | 'collecting'
  | 'done'
  | 'failed'
  | 'cancelled';

export type Pixal3dTierName = 'hero' | 'high' | 'medium' | 'game';

/** The advisory QC verdict for the draw a tier decoded.
 *
 * 🔴 ONLY MEANINGFUL WHEN `draws > 0`. A tier that inherited the shared skeleton ran no gate and
 * reports every field empty — `skin_ok: false`, `named_core: 0` — which reads exactly like a
 * failure and is not one. */
export interface Pixal3dGate {
  draws: number;
  /** `false` means no draw cleared the acceptance predicate and the best-ranked one shipped anyway.
   *  That is a normal, usable outcome. Show the verdict; never withhold the asset. */
  accepted: boolean;
  skin_ok: boolean;
  humanoid_gate_ok: boolean;
  generic_gate_ok: boolean;
  named_core: number;
  pose_gate_ran: boolean;
  pose_gate_pass: boolean;
  pose_gate_worst: number;
  summary: string;
}

export interface Pixal3dTier {
  tier: string;
  decimate: number;
  joints: number;
  /** Of 22 Mixamo core slots. This is the number that decides whether a humanoid clip retargets,
   *  and it is SHARED across the ladder — a `game` tier reports the hero's. */
  named_core: number;
  seconds: number;
  /** `''` when the automatic cleanup did not run; otherwise its verdict, including "found nothing
   *  to improve and kept the original skin". */
  skin_cleanup: string;
  gate: Pixal3dGate;
  filename: string;
  bytes: number;
  /** The GLB's path inside the engine container — how an animate names it. */
  glb_path: string;
}

/** Which GLB an animation was retargeted onto. */
export type Pixal3dRigSource = { kind: 'tier'; tier: string } | { kind: 'edit'; edit_id: string };

/** A rig repaired in the browser. A new file, never an overwrite of the tier it came from. */
export interface Pixal3dEdit {
  id: string;
  label: string;
  base_tier: string;
  filename: string;
  bytes: number;
  /** Present only when the shared staging directory exists. Without it the edit is downloadable
   *  but not animatable — the engine container cannot open a file it does not mount. */
  staged_path: string | null;
  changes: {
    pruned_influences?: number;
    vertices_touched?: number;
    renamed_joints?: { from: string; to: string }[];
    [key: string]: unknown;
  } | null;
  created_unix: number;
}

export interface Pixal3dAnimation {
  id: string;
  state: Pixal3dJobState;
  engine_job_id: string | null;
  label: string;
  motion_ids: string[];
  rig_source: Pixal3dRigSource;
  variant: string | null;
  smooth: number | null;
  exercise: boolean;
  /** glTF animation tracks baked into the delivered GLB. */
  animations: number;
  /** Joints the retarget actually mapped, and how. A low number on a humanoid is the visible
   *  consequence of a low `named_core`. */
  mapped: number;
  map_kind: string;
  exercise_joints: number;
  seconds: number;
  filename: string | null;
  bytes: number;
  error: string | null;
  created_unix: number;
}

export interface Pixal3dCharacter {
  id: string;
  name: string;
  state: Pixal3dJobState;
  engine_job_id: string | null;
  source_filename: string | null;
  source_bytes: number;
  engine_image: string | null;
  tiers: Pixal3dTier[];
  /** Written for a human by the engine. Surface these — they say which tier shipped a rejected
   *  draw and why. */
  warnings: string[];
  stage_dir: string | null;
  rig_cache: string | null;
  request: Record<string, unknown> | null;
  tiers_requested: string[];
  rig_seed: number | null;
  geo_seed: number | null;
  /** The character this is a re-roll of, so takes of one subject group together. */
  parent_id: string | null;
  /** `upload` | `rig_seed` | `geo_seed` | `tiers`. */
  origin: string;
  peak_vram_mib: number | null;
  seconds: number | null;
  gate_wait_ms: number | null;
  duration_ms: number | null;
  error: string | null;
  cancel_requested: boolean;
  edits: Pixal3dEdit[];
  animations: Pixal3dAnimation[];
  created_unix: number;
  finished_unix: number | null;
}

/** A generated clip. GLOBAL, not per-character: the retarget matches joints by NAME, so one clip
 *  plays on every humanoid rig here. */
export interface Pixal3dMotion {
  id: string;
  state: Pixal3dJobState;
  engine_job_id: string | null;
  prompt: string;
  seed: number | null;
  seconds: number | null;
  cfg: number | null;
  steps: number | null;
  frames: number;
  fps: number;
  clip_path: string | null;
  /** False ⇒ this generation paid the ~21 s DiT load. It is the entire explanation for a first
   *  prompt that felt three times slower than the rest of the session. */
  dit_was_resident: boolean;
  error: string | null;
  gate_wait_ms: number | null;
  duration_ms: number | null;
  cancel_requested: boolean;
  created_unix: number;
}

export interface Pixal3dHealth {
  status: string;
  busy: boolean;
  draining: boolean;
  in_flight: number;
  queued: number;
  jobs_queued: number;
  jobs_running: number;
  /** 🔴 We hold our slot but ANOTHER process on that box owns the card. Show this distinctly from
   *  "queued": the service looks idle and is not able to start. */
  waiting_for_gpu_lock: boolean;
  /** Whether the motion DiT is resident — the difference between an ~11 s and a ~33 s prompt. */
  loaded: boolean;
  idle_unload_seconds: number;
  min_free_mib: number;
  rig_retries_default: number;
  worker_isolation: boolean;
  fatal: string;
}

export interface Pixal3dProps {
  configured: boolean;
  healthy: boolean;
  health: Pixal3dHealth | null;
  tiers: Pixal3dTierName[];
  default_tiers: Pixal3dTierName[];
  variants: string[];
  staging_ready: boolean;
  staging_dir: string;
  timings: {
    rig_first_tier_seconds: [number, number];
    rig_extra_tier_seconds: [number, number];
    rerig_from_geometry_seconds: number;
    rebake_from_rig_cache_seconds: number;
    motion_warm_seconds: number;
    motion_cold_seconds: number;
    animate_seconds: number;
    note: string;
  };
}

export interface Pixal3dCreateRequest {
  name?: string;
  tiers?: string[];
  rig_seed?: number;
  rig_retries?: number;
  /** 0/absent = the default 1024 lattice. 1536 is the only other measured value and it is
   *  per-subject: better hands on some characters, a worse rig on others. */
  resolution?: number;
  texsize?: number;
  skin_cleanup?: boolean;
}

export type Pixal3dRerigRequest =
  | { mode: 'rig_seed'; rig_seed: number; tiers?: string[]; rig_retries?: number }
  | { mode: 'geo_seed'; geo_seed: number; rig_seed?: number; tiers?: string[]; resolution?: number }
  | { mode: 'tiers'; tiers: string[] };

export interface Pixal3dMotionRequest {
  prompt: string;
  seconds?: number;
  seed?: number;
  cfg?: number;
  steps?: number;
}

export interface Pixal3dAnimateRequest {
  motion_ids: string[];
  rig_source?: Pixal3dRigSource;
  label?: string;
  variant?: string;
  smooth?: number;
  exercise?: boolean;
}

/** What is holding — or waiting for — the shared rig/motion slot. Queue-backed, so `state` is the
 *  queue row's own, not the catalogue record's. */
export interface Pixal3dBusy {
  kind: string;
  id: string;
  label: string;
  job_id: string;
  state: JobState;
  eta_start: string | null;
}
