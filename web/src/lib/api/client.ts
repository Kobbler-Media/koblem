import { clearAuth, getAuthToken } from '#lib/stores/auth.svelte.ts';
import { queueHeaders, type QueueSubmitOptions } from '#lib/queue/options.ts';
import type { SubscriptionBody } from '#lib/queue/push.ts';
import { parseTtsSse, type FaEvent } from '@koblibs/ui';

export type { QueueSubmitOptions };

const apiBase = import.meta.env.VITE_API_BASE_URL || '';

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    /** The decoded error body, when the server sent JSON. Carries fields like `current_version`. */
    public body?: unknown
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

type FetchOptions = Parameters<typeof fetch>[1];

async function request<T>(path: string, options: FetchOptions = {}): Promise<T> {
  const token = getAuthToken();
  const response = await fetch(`${apiBase}${path}`, {
    ...options,
    headers: {
      'Content-Type': 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...options.headers
    }
  });

  if (!response.ok) {
    if (response.status === 401) {
      clearAuth();
    }
    let message = `Request failed: ${response.status}`;
    let body: unknown;
    try {
      const error = await response.json();
      body = error;
      if (typeof error?.message === 'string' && error.message.trim()) {
        message = error.message.trim();
      } else if (typeof error?.error === 'string' && error.error.trim()) {
        message = error.error.trim();
      }
    } catch {
      try {
        const text = await response.text();
        if (text.trim()) {
          message = text.trim();
        } else if (response.statusText) {
          message = response.statusText;
        }
      } catch {
        if (response.statusText) {
          message = response.statusText;
        }
      }
    }
    throw new ApiError(response.status, message, body);
  }

  if (response.status === 204) {
    return undefined as T;
  }

  return response.json();
}

// ── Enqueueing submits ──────────────────────────────────────────────
//
// 🔴 The 409/503 that used to make a double-clicked Render harmless are gone by
// design: every render route enqueues now, and an enqueue succeeds twice. The
// server's unique index on `Idempotency-Key` is what replaces them, and only for
// a client that sends one — so every enqueueing submit goes through the helpers
// below rather than calling `request`/`authedFetch` directly.
//
// One key per engine, minted on first use and dropped once a submit is ACCEPTED:
// a retried click reuses it and dedupes, the next render mints a fresh one.

/// The house poll interval for `…/jobs/{id}`, shared by every panel so one
/// engine's render never polls faster than another's.
export const JOB_POLL_MS = 1000;

const idempotencyKeys = new Map<string, string>();

function idempotencyHeaders(scope: string): Record<string, string> {
  let key = idempotencyKeys.get(scope);
  if (!key) {
    key = crypto.randomUUID();
    idempotencyKeys.set(scope, key);
  }
  return { 'Idempotency-Key': key };
}

async function enqueueJson<T>(
  scope: string,
  path: string,
  body: unknown,
  queue?: QueueSubmitOptions
): Promise<T> {
  const result = await request<T>(path, {
    method: 'POST',
    headers: { ...idempotencyHeaders(scope), ...queueHeaders(queue) },
    body: JSON.stringify(body)
  });
  idempotencyKeys.delete(scope);
  return result;
}

async function enqueueForm<T>(
  scope: string,
  path: string,
  form: FormData,
  queue?: QueueSubmitOptions
): Promise<T> {
  const response = await authedFetch(path, {
    method: 'POST',
    headers: { ...idempotencyHeaders(scope), ...queueHeaders(queue) },
    body: form
  });
  idempotencyKeys.delete(scope);
  return response.json();
}

/// Tell a finished render apart from an accepted job on the sync-if-fast routes,
/// where one POST legitimately answers with either.
export function isEnqueued<T extends object>(body: T | EnqueueResponse): body is EnqueueResponse {
  return 'job_id' in body;
}

// ── Auth ────────────────────────────────────────────────────────────

export function login(email: string, password: string): Promise<{ token: string }> {
  return request('/api/v1/auth/login', {
    method: 'POST',
    body: JSON.stringify({ email, password })
  });
}

export function setupFirstUser(
  email: string,
  password: string
): Promise<{ user_id: string; token: string }> {
  return request('/api/v1/auth/setup', {
    method: 'POST',
    body: JSON.stringify({ email, password })
  });
}

export function getMe(): Promise<{ authenticated: boolean }> {
  return request('/api/v1/auth/me');
}

export interface ApiKeySummary {
  id: string;
  name: string;
  key: string | null;
  last_used_at: string | null;
  created_at: string;
  revoked: boolean;
}

export function listApiKeys(): Promise<ApiKeySummary[]> {
  return request('/api/v1/auth/api-keys');
}

export function createApiKey(name: string): Promise<{ id: string; key: string }> {
  return request('/api/v1/auth/api-keys', {
    method: 'POST',
    body: JSON.stringify({ name })
  });
}

export function revokeApiKey(id: string): Promise<void> {
  return request(`/api/v1/auth/api-keys/${id}`, { method: 'DELETE' });
}

// ── Health ──────────────────────────────────────────────────────────

export function getHealth(): Promise<{
  status: string;
  llama_server: boolean;
  vision: boolean;
  tts: boolean;
  stt: boolean;
  llama_model?: string;
  vision_model?: string;
  tts_model?: string;
  stt_model?: string;
  /// Does the LLM koblem is pointed at accept audio attachments?
  ///
  /// 🔴 DERIVED SERVER-SIDE, never probed — `/v1/models` says nothing about the
  /// mmproj. The resident gemma has a unified vision+audio encoder; Beefy mode's
  /// Qwen3.8 is VISION-ONLY. Images work either way.
  ///
  /// 🔴 May be absent on an older koblem, and absent means AUDIO WORKS (there was
  /// no Beefy mode before this field). Read it as `!== false`, never as truthy.
  llama_audio_input?: boolean;
}> {
  return request('/health');
}

/// Audio staging is allowed unless the server has actively said no. An older
/// koblem that omits the field, or a `/health` that failed, must not silently
/// disable a feature that works.
export function audioInputAllowed(health: { llama_audio_input?: boolean } | null): boolean {
  return health?.llama_audio_input !== false;
}

// ── Settings ────────────────────────────────────────────────────────

export function getSettings(): Promise<Record<string, unknown>> {
  return request('/api/v1/settings');
}

export function updateSettings(settings: Record<string, unknown>): Promise<void> {
  return request('/api/v1/settings', {
    method: 'PATCH',
    body: JSON.stringify(settings)
  });
}

export function testLibraryProvider(
  url?: string,
  token?: string
): Promise<{ success: boolean; message: string }> {
  return request('/api/v1/settings/test-library-provider', {
    method: 'POST',
    body: JSON.stringify({ url, token })
  });
}

export function testReleaseProvider(
  url?: string,
  token?: string
): Promise<{ success: boolean; message: string }> {
  return request('/api/v1/settings/test-release-provider', {
    method: 'POST',
    body: JSON.stringify({ url, token })
  });
}

/** Every service `POST /settings/test-gpu-service` resolves a URL for — mirrors the `setting_key`
 *  match in `api/src/routes/settings.rs`. A service missing here is untestable from the UI. */
export type GpuServiceKind =
  | 'llm'
  | 'vision'
  | 'text_embed'
  | 'tts'
  | 'stt'
  | 'music'
  | 'sa3'
  | 'music3'
  | 'songgen'
  | 'avatar'
  | 'flux2'
  | 'krea2'
  | 'ltx'
  | 'ltx_2b'
  | 'minimax_h3'
  | 'wan_vace'
  | 'matting'
  | 'spar3d'
  | 'pixal3d';

export function testGpuService(
  service: GpuServiceKind,
  url?: string
): Promise<{ success: boolean; message: string; model?: string }> {
  return request('/api/v1/settings/test-gpu-service', {
    method: 'POST',
    body: JSON.stringify({ service, url })
  });
}

// ── Corrections ─────────────────────────────────────────────────────

import type {
  GenericRule,
  EntityRule,
  DecisionCorrection,
  ReviewRequest,
  ReviewResponse,
  ShowSummary,
  ShowDetail,
  InterpretInput,
  InterpretedRelease,
  PlaygroundSearchRequest,
  SearchAndReviewResponse,
  CompareRequest,
  CompareResponse,
  ReviewSessionSummary,
  ReviewSessionDetail,
  PromptTemplatesOverview,
  PromptTemplateBody,
  PromptVariableSummary,
  PromptVariableListItem,
  PromptFixtureSummary,
  RenderPreviewRequest,
  RenderPreviewResponse,
  ContextSchemaField,
  MediaKindKey,
  PromptSlot,
  GrammarEntry,
  UpsertGrammarRequest,
  LlmLogSummary,
  LlmLogDetail,
  SamplingConfig,
  ChatRequest,
  ChatResponse,
  TestScenario,
  TestResult,
  GenerationList,
  UpscaleGenerateRequest,
  UpscaleJob,
  UpscalePlanResponse,
  UpscaleUploadResponse,
  Pixal3dAnimateRequest,
  Pixal3dBusy,
  Pixal3dCharacter,
  Pixal3dCreateRequest,
  Pixal3dMotion,
  Pixal3dMotionRequest,
  Pixal3dProps,
  Pixal3dRerigRequest
} from '#lib/types.ts';

export function getCorrections(): Promise<{
  generic_rules: GenericRule[];
  entity_rules: EntityRule[];
  decision_corrections: DecisionCorrection[];
}> {
  return request('/api/v1/corrections');
}

export function addGenericRule(rule: { scope: string; rule: string }): Promise<GenericRule> {
  return request('/api/v1/corrections/rules', {
    method: 'POST',
    body: JSON.stringify(rule)
  });
}

export function deleteGenericRule(id: string): Promise<void> {
  return request(`/api/v1/corrections/generic_rule/${id}`, {
    method: 'DELETE'
  });
}

export function addEntityRule(rule: {
  entity_id: string;
  entity_type: string;
  entity_name?: string;
  rule: string;
}): Promise<EntityRule> {
  return request('/api/v1/corrections/entity-rules', {
    method: 'POST',
    body: JSON.stringify(rule)
  });
}

export function deleteEntityRule(id: string): Promise<void> {
  return request(`/api/v1/corrections/entity_rule/${id}`, { method: 'DELETE' });
}

export function addDecisionCorrection(correction: {
  release_name: string;
  entity_id?: string;
  model_decision: string;
  correct_decision: string;
  reason: string;
}): Promise<DecisionCorrection> {
  return request('/api/v1/corrections/decisions', {
    method: 'POST',
    body: JSON.stringify(correction)
  });
}

export function deleteDecisionCorrection(id: string): Promise<void> {
  return request(`/api/v1/corrections/decision_correction/${id}`, {
    method: 'DELETE'
  });
}

// ── Review ──────────────────────────────────────────────────────────

export function submitReview(req: ReviewRequest): Promise<ReviewResponse> {
  return request('/api/v1/review-candidates', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

// ── Library ─────────────────────────────────────────────────────────

export function searchShows(query: string): Promise<ShowSummary[]> {
  return request(`/api/v1/library/search?q=${encodeURIComponent(query)}`);
}

export function getShowDetail(showId: string): Promise<ShowDetail> {
  return request(`/api/v1/library/shows/${encodeURIComponent(showId)}`);
}

export function getEpisodeTitle(
  showId: string,
  season: string,
  episode: string
): Promise<{ title: string }> {
  return request(
    `/api/v1/library/shows/${encodeURIComponent(showId)}/episodes/${encodeURIComponent(season)}/${encodeURIComponent(episode)}/title`
  );
}

export function buildMediaWant(
  showId: string,
  seasonId?: string,
  episodeIds?: string[]
): Promise<unknown> {
  return request('/api/v1/library/build-media-want', {
    method: 'POST',
    body: JSON.stringify({
      show_id: showId,
      season_id: seasonId,
      episode_ids: episodeIds || []
    })
  });
}

// ── Playground ──────────────────────────────────────────────────────

export function playgroundSearch(req: PlaygroundSearchRequest): Promise<SearchAndReviewResponse> {
  return request('/api/v1/playground/search', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

export function playgroundInterpret(releases: InterpretInput[]): Promise<InterpretedRelease[]> {
  return request('/api/v1/playground/interpret', {
    method: 'POST',
    body: JSON.stringify(releases)
  });
}

export function playgroundCompare(req: CompareRequest): Promise<CompareResponse> {
  return request('/api/v1/playground/compare', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

// ── Review Sessions ─────────────────────────────────────────────────

export function listReviewSessions(
  limit?: number,
  offset?: number
): Promise<ReviewSessionSummary[]> {
  const params = new URLSearchParams();
  if (limit != null) params.set('limit', String(limit));
  if (offset != null) params.set('offset', String(offset));
  const qs = params.toString();
  return request(`/api/v1/review-sessions${qs ? `?${qs}` : ''}`);
}

export function getReviewSession(id: string): Promise<ReviewSessionDetail> {
  return request(`/api/v1/review-sessions/${id}`);
}

/**
 * Replays a stored session through the current review pipeline (current prompt,
 * current grammar, current model). Returns both the original (frozen) request
 * + verdicts and the fresh result so the UI can diff them side-by-side.
 *
 * Replays are NOT persisted as new sessions — they're throwaway debug runs.
 */
export function replayReviewSession(id: string): Promise<{
  original_request: unknown;
  original_verdicts: unknown;
  original_notes: string | null;
  fresh: ReviewResponse;
}> {
  return request(`/api/v1/review-sessions/${id}/replay`, { method: 'POST' });
}

// ── Prompt Templates (per-media-kind, with default fallback) ───────

export function getPromptTemplatesOverview(): Promise<PromptTemplatesOverview> {
  return request('/api/v1/settings/prompt-templates');
}

export function getPromptTemplate(
  mediaKind: MediaKindKey,
  slot: PromptSlot
): Promise<PromptTemplateBody> {
  return request(`/api/v1/settings/prompt-templates/${mediaKind}/${slot}`);
}

export function upsertPromptTemplate(
  mediaKind: MediaKindKey,
  slot: PromptSlot,
  body: string
): Promise<void> {
  return request(`/api/v1/settings/prompt-templates/${mediaKind}/${slot}`, {
    method: 'PUT',
    body: JSON.stringify({ body })
  });
}

export function deletePromptTemplate(mediaKind: MediaKindKey, slot: PromptSlot): Promise<void> {
  return request(`/api/v1/settings/prompt-templates/${mediaKind}/${slot}`, {
    method: 'DELETE'
  });
}

export function listPromptVariables(mediaKind: MediaKindKey): Promise<PromptVariableListItem[]> {
  return request(`/api/v1/settings/prompt-variables/${mediaKind}`);
}

export function getPromptVariable(
  mediaKind: MediaKindKey,
  name: string
): Promise<PromptVariableSummary> {
  return request(`/api/v1/settings/prompt-variables/${mediaKind}/${encodeURIComponent(name)}`);
}

export function upsertPromptVariable(
  mediaKind: MediaKindKey,
  name: string,
  body: string
): Promise<void> {
  return request(`/api/v1/settings/prompt-variables/${mediaKind}/${encodeURIComponent(name)}`, {
    method: 'PUT',
    body: JSON.stringify({ body })
  });
}

export function deletePromptVariable(mediaKind: MediaKindKey, name: string): Promise<void> {
  return request(`/api/v1/settings/prompt-variables/${mediaKind}/${encodeURIComponent(name)}`, {
    method: 'DELETE'
  });
}

export function previewPromptRender(payload: RenderPreviewRequest): Promise<RenderPreviewResponse> {
  return request('/api/v1/settings/prompt-templates/preview', {
    method: 'POST',
    body: JSON.stringify(payload)
  });
}

export function listPromptFixtures(): Promise<PromptFixtureSummary[]> {
  return request('/api/v1/settings/prompt-templates/fixtures');
}

export function getPromptContextSchema(): Promise<ContextSchemaField[]> {
  return request('/api/v1/settings/prompt-templates/context-schema');
}

// ── Grammars ─────────────────────────────────────────────────────────

export function listGrammars(): Promise<GrammarEntry[]> {
  return request('/api/v1/grammars');
}

export function upsertGrammar(payload: UpsertGrammarRequest): Promise<void> {
  return request('/api/v1/grammars', {
    method: 'PUT',
    body: JSON.stringify(payload)
  });
}

export function deleteGrammar(name: string): Promise<void> {
  return request(`/api/v1/grammars/${encodeURIComponent(name)}`, {
    method: 'DELETE'
  });
}

// ── LLM Logs ────────────────────────────────────────────────────────

export function listLlmLogs(
  limit?: number,
  offset?: number,
  caller?: string
): Promise<LlmLogSummary[]> {
  const params = new URLSearchParams();
  if (limit != null) params.set('limit', String(limit));
  if (offset != null) params.set('offset', String(offset));
  if (caller) params.set('caller', caller);
  const qs = params.toString();
  return request(`/api/v1/llm-logs${qs ? `?${qs}` : ''}`);
}

export function getLlmLog(id: string): Promise<LlmLogDetail> {
  return request(`/api/v1/llm-logs/${id}`);
}

/**
 * Replay a past LLM request with a new `SamplingConfig`. Returns the new
 * log id + the llama-server response. Sending `{}` replays with whatever
 * the server-side defaults decide for fields the body doesn't carry.
 */
export function rerunLlmLog(
  id: string,
  sampling: SamplingConfig
): Promise<{ log_id: string; response: unknown }> {
  return request(`/api/v1/llm-logs/${id}/rerun`, {
    method: 'POST',
    body: JSON.stringify(sampling)
  });
}

// ── Raw Chat ────────────────────────────────────────────────────────

export function sendChat(req: ChatRequest): Promise<ChatResponse> {
  return request('/api/v1/chat', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

// ── Testbed ────────────────────────────────────────────────────────

export function listTestScenarios(): Promise<TestScenario[]> {
  return request('/api/v1/testbed/scenarios');
}

export function runTestScenario(scenarioId: string, promptOverride?: string): Promise<TestResult> {
  const body: Record<string, unknown> = { scenario_id: scenarioId };
  if (promptOverride) body.prompt_override = promptOverride;
  return request('/api/v1/testbed/run', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

// ── Audiobook picker (kobbler library via koblem's LibraryProvider) ────

export interface AudiobookSummary {
  id: string;
  title: string;
  author?: string;
  narrator?: string;
}

export interface AudiobookChapter {
  index: number;
  title: string;
  file_path: string;
  duration_seconds?: number;
}

export interface AudiobookDetail {
  id: string;
  title: string;
  author?: string;
  narrator?: string;
  chapters: AudiobookChapter[];
}

export function searchAudiobooks(q: string): Promise<AudiobookSummary[]> {
  return request(`/api/v1/library/audiobooks?q=${encodeURIComponent(q)}`);
}

export function getAudiobook(id: string): Promise<AudiobookDetail> {
  return request(`/api/v1/library/audiobooks/${encodeURIComponent(id)}`);
}

// ── TTS playground ─────────────────────────────────────────────────────

export interface TtsGenerateResult {
  audio: Blob;
  contentType: string;
  durationMs?: number;
  gateWaitMs?: number;
  sampleId?: number;
  /// The engine stopped at `max_new_frames` mid-sentence — the blob is valid
  /// but SHORT. Only reachable with long-form off; treat it as a failure.
  truncated?: boolean;
  /// Rate the render ACTUALLY ran at. A rate the engine can't do is refused and
  /// falls back to 1, so this is the only honest source for what was rendered.
  speedApplied?: number;
  /// Wall ms from fetch send to first response byte. Captures upstream
  /// time-to-first-audio plus gate/network overhead. Always measured client-
  /// side so it includes nginx + TCP + TLS hops the server can't see.
  ttfaMs?: number;
  /// Wall ms from fetch send to end-of-stream. End-to-end synth wall time.
  totalMs?: number;
}

async function authedFetch(path: string, init: FetchOptions = {}): Promise<Response> {
  const token = getAuthToken();
  const response = await fetch(`${apiBase}${path}`, {
    ...init,
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...init.headers
    }
  });
  if (!response.ok) {
    if (response.status === 401) clearAuth();
    let message = `Request failed: ${response.status}`;
    try {
      const err = await response.clone().json();
      if (typeof err?.error === 'string') message = err.error;
      else if (typeof err?.message === 'string') message = err.message;
    } catch {
      try {
        const text = await response.text();
        if (text.trim()) message = text.trim();
      } catch {
        // response body already consumed or unreadable — keep default message
      }
    }
    throw new ApiError(response.status, message);
  }
  return response;
}

function numberHeader(response: Response, name: string): number | undefined {
  const v = response.headers.get(name);
  return v != null ? Number(v) : undefined;
}

// ── TTS playground ─────────────────────────────────────────────────────

export interface TtsGenerateRequest {
  input: string;
  /** A cloned voice id. Omit to let the engine pick one and hold it. */
  voice?: string;
  /** Voice design, only meaningful without `voice`. */
  instructions?: string;
  /** Built-in speaker token S0..S9. For a stable narrator use `voice`. */
  speaker?: string;
  response_format?: 'wav' | 'pcm' | 'mp3';
  bitrate_kbps?: number;
  /** Render rate — a real re-render at that rate, not a player knob, so the
   *  alignment comes back in the rendered timebase. Omit at natural rate: an
   *  engine that can't do a rate rejects the request, and koblem then falls back
   *  to 1 and reports it as `speedApplied`. */
  speed?: number;
  temperature?: number;
  top_p?: number;
  top_k?: number;
  repetition_penalty?: number;
  seed?: number;
  /** Guidance strength for `instructions`; 4 makes the instruction land (~+30% render time). */
  cfg_scale?: number;
  /** 12.5 Hz FRAMES, not audio tokens. Raising it overflows the context
   *  window rather than producing longer audio — use long-form mode. */
  max_new_frames?: number;
  /** Prefer `sse`: it is the only transport that carries forced alignment, and
   *  the only one with a `speech.audio.done` event to report truncation on.
   *  `audio` is raw chunks with no envelope; `none` is one buffered blob. */
  stream_format?: 'none' | 'sse' | 'audio';
  stream_batch_size?: number;
  stream_first_batch_size?: number;
  /** Long-form mode. Defaults to TRUE server-side: it chunks internally with
   *  rolling context, which is both the lowest-VRAM path and what holds one
   *  voice across a whole passage. Never pre-chunk text yourself. */
  long?: boolean;
  chunk_words?: number;
  buffer?: number;
  save_sample?: boolean;
  align?: boolean;
  align_stream?: 'partial' | 'final-only';
}

export interface TtsVoice {
  id: string;
  name?: string;
  /** Shipped by the engine rather than cloned. `default` is the built-in — a
   *  real selectable value meaning "the model's own narrator". Built-ins have
   *  no reference clip or transcript and cannot be deleted. */
  builtin?: boolean;
  frames?: number;
  codebooks?: number;
  has_ref_text?: boolean;
  has_sample?: boolean;
}

export interface TtsVoicesResponse {
  voices: TtsVoice[];
  model_id?: string;
}

export interface TtsUploadResponse {
  id: string;
  name: string;
  mode?: string;
  ref_frames?: number;
  frames?: number;
  codebooks?: number;
}

/// Stream TTS SSE with forced-alignment word timings. Returns an async
/// iterable of normalized FaEvent values that drive a FaPlayback controller.
/// Server is expected to use `stream_format=sse`, `response_format=pcm`,
/// `align=true`; caller can override but those are the supported combo.
export async function ttsGenerateStreamingFa(
  req: TtsGenerateRequest,
  opts: { signal?: AbortSignal } = {}
): Promise<{
  events: AsyncIterable<FaEvent>;
  gateWaitMs?: number;
  sampleId?: number;
  speedApplied?: number;
}> {
  const body = {
    ...req,
    stream_format: 'sse' as const,
    response_format: 'pcm' as const,
    align: true,
    align_stream: req.align_stream ?? ('partial' as const)
  };
  const response = await authedFetch('/api/v1/tts/generate', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal: opts.signal
  });
  if (!response.body) throw new Error('No response body');
  return {
    events: parseTtsSse(response.body, { text: req.input }),
    gateWaitMs: numberHeader(response, 'x-koblem-gate-wait-ms'),
    sampleId: numberHeader(response, 'x-koblem-sample-id'),
    speedApplied: numberHeader(response, 'x-speed-applied')
  };
}

export async function ttsGenerate(
  req: TtsGenerateRequest,
  onFirstByte?: () => void
): Promise<TtsGenerateResult> {
  const fetchStart = performance.now();
  const response = await authedFetch('/api/v1/tts/generate', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(req)
  });

  const contentType = response.headers.get('content-type') ?? 'audio/wav';
  const gateWaitMs = numberHeader(response, 'x-koblem-gate-wait-ms');

  // Stream the body so we can clock real TTFA. response.body is null only
  // when the server sent no body — fall back to blob() in that case.
  let ttfaMs: number | undefined;
  let blob: Blob;
  if (response.body) {
    const reader = response.body.getReader();
    const chunks: Uint8Array[] = [];
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      if (value) {
        if (ttfaMs == null) {
          ttfaMs = performance.now() - fetchStart;
          onFirstByte?.();
        }
        chunks.push(value);
      }
    }
    // Copy each chunk into a fresh non-shared ArrayBuffer-backed Uint8Array
    // — fetch's Uint8Array<ArrayBufferLike> doesn't narrow to BlobPart since
    // ArrayBufferLike admits SharedArrayBuffer.
    const parts = chunks.map((c) => {
      const out = new Uint8Array(c.byteLength);
      out.set(c);
      return out;
    });
    blob = new Blob(parts, { type: contentType });
  } else {
    blob = await response.blob();
  }

  const totalMs = performance.now() - fetchStart;

  return {
    audio: blob,
    contentType,
    gateWaitMs,
    sampleId: numberHeader(response, 'x-koblem-sample-id'),
    truncated: response.headers.get('x-truncated') === '1',
    speedApplied: numberHeader(response, 'x-speed-applied'),
    ttfaMs,
    totalMs
  };
}

export function listTtsVoices(): Promise<TtsVoicesResponse> {
  return request('/api/v1/tts/voices');
}

export async function uploadTtsVoice(
  name: string,
  file: File,
  refText?: string
): Promise<TtsUploadResponse> {
  const form = new FormData();
  form.append('name', name);
  form.append('audio_sample', file);
  if (refText && refText.trim()) form.append('ref_text', refText.trim());
  const response = await authedFetch('/api/v1/tts/voices', {
    method: 'POST',
    body: form
  });
  return response.json();
}

export async function deleteTtsVoice(name: string): Promise<void> {
  await authedFetch(`/api/v1/tts/voices/${encodeURIComponent(name)}`, {
    method: 'DELETE'
  });
}

export interface TtsRegisterFromLibraryRequest {
  name: string;
  audiobook_id: string;
  chapter_index: number;
  offset_seconds: number;
  duration_seconds: number;
  /** When true, koblem STT-transcribes the slice for ICL ref_text. */
  transcribe?: boolean;
}

/// Fetch a mono-WAV preview of a chapter window for the playground trim UI.
/// Mirrors the koblem proxy `/api/v1/library/audiobooks/{id}/clip` route.
export async function fetchAudiobookClipWav(
  audiobookId: string,
  chapter: number,
  offsetSeconds: number,
  durationSeconds: number
): Promise<Blob> {
  const qs = new URLSearchParams({
    chapter: String(chapter),
    offset: String(offsetSeconds),
    duration: String(durationSeconds)
  });
  const resp = await authedFetch(
    `/api/v1/library/audiobooks/${encodeURIComponent(audiobookId)}/clip?${qs}`
  );
  return resp.blob();
}

/// Register a clone whose audio source is a sliced kobbler audiobook
/// chapter. koblem fetches the WAV via library_provider.fetch_audiobook_clip,
/// optionally STT-transcribes it for a reference transcript, then uploads it.
export async function registerTtsVoiceFromLibrary(
  req: TtsRegisterFromLibraryRequest
): Promise<TtsUploadResponse> {
  const response = await authedFetch('/api/v1/tts/voices/from-library', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(req)
  });
  return response.json();
}

/// Download the original ref.wav for a clone. Returns the Blob (saved by the
/// caller via createObjectURL + click). Throws ApiError(404) when the voice
/// has no archived sample (e.g. registered before the archive dir was set).
export async function downloadTtsVoiceSample(name: string): Promise<Blob> {
  const resp = await authedFetch(`/api/v1/tts/voices/${encodeURIComponent(name)}/sample.wav`);
  return resp.blob();
}

/// Download the optional ref_text.txt for a clone. Throws ApiError(404) when
/// the clone was registered without a ref_text.
export async function downloadTtsVoiceRefText(name: string): Promise<string> {
  const resp = await authedFetch(`/api/v1/tts/voices/${encodeURIComponent(name)}/ref_text`);
  return resp.text();
}

// ── STT playground ─────────────────────────────────────────────────────

export interface SttTranscribeParams {
  timestamp_granularity?: 'segment' | 'word' | 'char';
  beam_size?: number;
  language?: string;
  save_sample?: boolean;
}

export interface SttSegment {
  start: number;
  end: number;
  text: string;
  id?: number;
}

export interface SttResponse {
  text: string;
  segments: SttSegment[];
  words: SttSegment[];
  chars: SttSegment[];
  language?: string;
  preempted: boolean;
  duration_ms?: number;
  gate_wait_ms?: number;
}

export async function sttTranscribe(file: File, params: SttTranscribeParams): Promise<SttResponse> {
  const form = new FormData();
  form.append('file', file);
  if (params.timestamp_granularity)
    form.append('timestamp_granularity', params.timestamp_granularity);
  if (params.beam_size != null) form.append('beam_size', String(params.beam_size));
  if (params.language) form.append('language', params.language);
  if (params.save_sample) form.append('save_sample', 'true');
  const response = await authedFetch('/api/v1/stt/transcribe', {
    method: 'POST',
    body: form
  });
  return response.json();
}

/// Same as sttTranscribe but reports upload progress via the callback. Use
/// this from the STT playground so the user sees "uploading 42%" while a
/// multi-hundred-MB audiobook chunk is streaming to koblem, then "transcribing"
/// while parakeet is running. Large uploads otherwise look frozen.
export function sttTranscribeWithProgress(
  file: File,
  params: SttTranscribeParams,
  onProgress: (phase: 'uploading' | 'transcribing', loaded: number, total: number) => void
): Promise<SttResponse> {
  return new Promise((resolve, reject) => {
    const form = new FormData();
    form.append('file', file);
    if (params.timestamp_granularity)
      form.append('timestamp_granularity', params.timestamp_granularity);
    if (params.beam_size != null) form.append('beam_size', String(params.beam_size));
    if (params.language) form.append('language', params.language);
    if (params.save_sample) form.append('save_sample', 'true');

    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${apiBase}/api/v1/stt/transcribe`);
    const token = getAuthToken();
    if (token) xhr.setRequestHeader('Authorization', `Bearer ${token}`);

    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress('uploading', e.loaded, e.total);
    };
    xhr.upload.onload = () => {
      // Upload done; server is now transcribing.
      onProgress('transcribing', file.size, file.size);
    };

    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(JSON.parse(xhr.responseText) as SttResponse);
        } catch {
          reject(new ApiError(xhr.status, 'invalid response JSON'));
        }
      } else {
        if (xhr.status === 401) clearAuth();
        let msg = `Request failed: ${xhr.status}`;
        try {
          const err = JSON.parse(xhr.responseText);
          if (typeof err?.error === 'string') msg = err.error;
          else if (typeof err?.message === 'string') msg = err.message;
        } catch {
          if (xhr.responseText?.trim()) msg = xhr.responseText.trim();
        }
        reject(new ApiError(xhr.status, msg));
      }
    };
    xhr.onerror = () => reject(new ApiError(0, 'network error'));
    xhr.onabort = () => reject(new ApiError(0, 'aborted'));
    xhr.send(form);
  });
}

export interface SttLibraryRequest {
  library_audiobook_id: string;
  offset_seconds?: number;
  duration_seconds?: number;
  timestamp_granularity?: 'segment' | 'word' | 'char';
  beam_size?: number;
  language?: string;
  save_sample?: boolean;
}

export function sttTranscribeFromLibrary(req: SttLibraryRequest): Promise<SttResponse> {
  return request('/api/v1/stt/transcribe-from-library', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

// ── Embeddings playground (EmbeddingGemma-2) ───────────────────────────

export interface VisionEmbedParams {
  save_sample?: boolean;
}

export interface VisionEmbedResponse {
  embeddings: number[][];
  model?: string;
  duration_ms?: number;
  gate_wait_ms?: number;
}

export async function visionEmbed(
  files: File[],
  params: VisionEmbedParams
): Promise<VisionEmbedResponse> {
  const form = new FormData();
  for (const f of files) form.append('images', f);
  if (params.save_sample) form.append('save_sample', 'true');
  const response = await authedFetch('/api/v1/vision/embed', {
    method: 'POST',
    body: form
  });
  return response.json();
}

export interface VisionClassifyParams {
  prompts: string[];
  save_sample?: boolean;
}

/** `scores[image][prompt]` is cosine similarity: compare scores to each other, never read one as
 *  a probability. Differences between labels are typically a few hundredths. */
export interface VisionClassifyResponse {
  scores: number[][];
  logits?: number[][];
  model?: string;
  duration_ms?: number;
  gate_wait_ms?: number;
}

export async function visionClassify(
  files: File[],
  params: VisionClassifyParams
): Promise<VisionClassifyResponse> {
  const form = new FormData();
  for (const f of files) form.append('images', f);
  for (const p of params.prompts) form.append('prompts', p);
  if (params.save_sample) form.append('save_sample', 'true');
  const response = await authedFetch('/api/v1/vision/classify', {
    method: 'POST',
    body: form
  });
  return response.json();
}

export interface VisionTextEmbedResponse {
  embeddings: number[][];
  model?: string;
  duration_ms?: number;
  gate_wait_ms?: number;
}

/** EmbeddingGemma-2 task prefixes koblem applies. `raw` embeds the text as typed. */
export type EmbedTask =
  | 'raw'
  | 'query'
  | 'document'
  | 'question_answering'
  | 'fact_checking'
  | 'classification'
  | 'clustering'
  | 'similarity'
  | 'code_retrieval';

/** Embed text on the always-on text instance (no GPU gate). Same space as the image vectors. */
export async function visionTextEmbed(
  prompts: string[],
  task: EmbedTask = 'raw'
): Promise<VisionTextEmbedResponse> {
  const response = await authedFetch('/api/v1/vision/text-embed', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ prompts, task })
  });
  return response.json();
}

export interface VisionClassifyFromEmbeddingsParams {
  image_embeddings: number[][];
  prompts: string[];
}

/** Score pre-computed image embeddings (e.g. from a prior /vision/embed call) against text
 * prompts. No image upload and no GPU: cosine against cached prompt vectors. */
export async function visionClassifyFromEmbeddings(
  params: VisionClassifyFromEmbeddingsParams
): Promise<VisionClassifyResponse> {
  const response = await authedFetch('/api/v1/vision/classify-from-embeddings', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(params)
  });
  return response.json();
}

// ── GPU logs ───────────────────────────────────────────────────────────

export interface GpuLogEntry {
  id: number;
  service: string;
  endpoint: string;
  request_meta: unknown;
  response_meta: unknown;
  gate_wait_ms: number;
  duration_ms: number;
  status: number;
  caller?: string;
  sample_id?: number;
  created_at: string;
}

export interface GpuLogsListResponse {
  entries: GpuLogEntry[];
  total: number;
}

export interface GpuLogsStats {
  total_entries: number;
  total_samples: number;
  total_sample_bytes: number;
  oldest_created_at?: string;
  newest_created_at?: string;
}

export function listGpuLogs(
  service?: string,
  limit = 50,
  offset = 0
): Promise<GpuLogsListResponse> {
  const params = new URLSearchParams();
  if (service) params.set('service', service);
  params.set('limit', String(limit));
  params.set('offset', String(offset));
  return request(`/api/v1/gpu-logs?${params.toString()}`);
}

export function getGpuLogsStats(): Promise<GpuLogsStats> {
  return request('/api/v1/gpu-logs/stats');
}

export function getGpuLog(id: number): Promise<GpuLogEntry> {
  return request(`/api/v1/gpu-logs/${id}`);
}

export function gpuLogSampleUrl(id: number): string {
  const token = getAuthToken();
  const base = `${apiBase}/api/v1/gpu-logs/${id}/sample`;
  // `<img src>` / `<audio src>` tags can't carry an Authorization header;
  // the backend accepts ?token= as an auth fallback for this single endpoint.
  return token ? `${base}?token=${encodeURIComponent(token)}` : base;
}

export function deleteGpuLog(id: number): Promise<void> {
  return request(`/api/v1/gpu-logs/${id}`, { method: 'DELETE' });
}

export function clearGpuLogs(): Promise<void> {
  return request('/api/v1/gpu-logs', { method: 'DELETE' });
}

// ── Music (ACE-Step) ─────

export type MusicMode = 'generate' | 'cover' | 'repaint' | 'extract' | 'lego' | 'complete';

export type MusicLyricsSource = 'ace' | 'llm';

export interface MusicGenerateRequest {
  mode?: MusicMode;
  caption: string;
  lyrics?: string;
  /// When lyrics are blank on generate: who writes them. "ace" (default) =
  /// the engine's own LM; "llm" = the main 9B (uses lyrics_prompt for theme).
  lyrics_source?: MusicLyricsSource;
  /// Optional brief for what the 9B-written lyrics should be about (theme/story),
  /// separate from caption (the music style). Only used with lyrics_source "llm".
  lyrics_prompt?: string;
  /// draft-lyrics only: an existing lyric draft to improve in place (same lyric
  /// rules) rather than write fresh. Ignored by generate.
  refine_source?: string;
  /// draft-lyrics only: free-form change to apply to refine_source; empty = a
  /// general polish.
  refine_instruction?: string;
  /// Omit to let koblem auto-compute from the lyric word count.
  duration?: number;
  seed?: number;
  /// cover only: audio_cover_strength. High = faithful to source, low = follows new lyrics.
  cover_strength?: number;
  /// cover only: false (default) = cover-nofsq (faithful remix, needs matching lyrics);
  /// true = cover (FSQ, loose "in the style of" reinterpretation, tolerates new lyrics).
  cover_reference?: boolean;
  /// cover only: cover_noise_strength. 0 = pure noise (most freedom) … 1 = start from source.
  cover_noise_strength?: number;
  /// extract/lego/complete only: the stem to isolate/generate/add (vocals, drums, …).
  track?: string;
  repaint_start?: number;
  repaint_end?: number;
  /// Sung-vocal language (ISO code: "en", "es", "fr", "de", "ja", "zh", …, or
  /// "unknown" to let the model choose). Omit → koblem sends "en"; without it
  /// ace picks a language at random.
  vocal_language?: string;
  output_format?: 'mp3' | 'wav16' | 'wav24' | 'wav32';
  /// Generate only: number of distinct songs from one prompt (lm_batch_size).
  songs?: number;
  /// Seed-variations per song (synth_batch_size); cover/repaint = render count.
  takes?: number;
  synth_model?: string;
  inference_steps?: number;
  shift?: number;
  save_song?: boolean;
}

export interface MusicUnderstood {
  caption: string;
  lyrics?: string;
  bpm?: number;
  keyscale?: string;
  timesignature?: string;
  vocal_language?: string;
  duration?: number;
}

/// A pair of line endings that only look like a rhyme: same letters, different
/// vowel ("good"/"blood"). Fine on the page, falls apart when sung.
export interface RhymeFlag {
  a: string;
  b: string;
}

export interface MusicDraftLyricsResponse {
  lyrics: string;
  /// Advisory. Absent/empty when the lyrics are clean. Never auto-applied —
  /// perfect rhyme is not always wanted.
  rhyme_flags?: RhymeFlag[];
  /// The LLM's estimate of a good track length (seconds). A hint the UI offers
  /// for the duration; never auto-applied. Absent when not estimated.
  suggested_seconds?: number;
}

export interface MusicTrack {
  filename: string;
  url: string;
  seed: number;
  content_type: string;
  bytes: number;
}

export interface MusicGenerateResponse {
  tracks: MusicTrack[];
  duration_used: number;
  lyrics_used?: string;
  duration_ms?: number;
  gate_wait_ms?: number;
  gpu_log_id?: number;
}

export interface MusicProps {
  dit_models: string[];
  lm_models: string[];
  healthy: boolean;
}

export function getMusicProps(): Promise<MusicProps> {
  return request('/api/v1/music/props');
}

/// text2music. Long-running; the GPU is held for the whole render. A 503 means
/// TTS preempted — retry shortly. Sends JSON unless a `ref` (timbre reference)
/// is supplied, in which case it goes multipart.
export async function musicGenerate(
  req: MusicGenerateRequest,
  ref?: File | null
): Promise<MusicGenerateResponse> {
  const body = { ...req, mode: 'generate' as const };
  if (!ref) {
    return request('/api/v1/music/generate', {
      method: 'POST',
      body: JSON.stringify(body)
    });
  }
  const form = new FormData();
  form.append('request', JSON.stringify(body));
  form.append('ref_audio', ref);
  const response = await authedFetch('/api/v1/music/generate', {
    method: 'POST',
    body: form
  });
  return response.json();
}

/// cover / repaint — multipart with the source-audio upload (+ optional timbre ref).
export async function musicTransform(
  req: MusicGenerateRequest,
  source: File,
  ref?: File | null
): Promise<MusicGenerateResponse> {
  const form = new FormData();
  form.append('request', JSON.stringify(req));
  form.append('audio', source);
  if (ref) form.append('ref_audio', ref);
  const response = await authedFetch('/api/v1/music/transform', {
    method: 'POST',
    body: form
  });
  return response.json();
}

/// Describe a source song (caption + metadata) via ace-server's reverse pipeline.
export async function musicUnderstand(source: File): Promise<MusicUnderstood> {
  const form = new FormData();
  form.append('audio', source);
  const response = await authedFetch('/api/v1/music/understand', {
    method: 'POST',
    body: form
  });
  return response.json();
}

/// Draft lyrics with the main LLM (no render) — fast, for previewing/editing.
export function musicDraftLyrics(req: MusicGenerateRequest): Promise<MusicDraftLyricsResponse> {
  return request('/api/v1/music/draft-lyrics', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

/// Rewrite only the line endings that don't rhyme when sung. User-triggered.
/// Re-checks after, so a stubborn pair comes back flagged rather than looping.
export function musicFixRhymes(
  lyrics: string,
  caption?: string
): Promise<MusicDraftLyricsResponse> {
  return request('/api/v1/music/fix-rhymes', {
    method: 'POST',
    body: JSON.stringify({ lyrics, caption })
  });
}

export interface MusicSuggestion {
  caption: string;
  lyrics?: string;
}

/// Expand a casual one-liner idea via the main 9B, to pre-fill the form.
/// `captionOnly` → caption only (no lyrics), for letting ACE write the lyrics;
/// otherwise caption + matching lyrics in one pass.
export function musicSuggest(brief: string, captionOnly = false): Promise<MusicSuggestion> {
  return request('/api/v1/music/suggest', {
    method: 'POST',
    body: JSON.stringify({ brief, caption_only: captionOnly })
  });
}

/// Fetch a rendered track as an object URL for `<audio>` playback. The file
/// endpoint is Bearer-auth only, so we fetch the blob rather than linking src.
export async function fetchMusicTrack(url: string): Promise<string> {
  const response = await authedFetch(url);
  const blob = await response.blob();
  return URL.createObjectURL(blob);
}

// ── Songgen (songgen.cpp / LeVo2 — the heavier, co-equal music engine) ───────
// A second music engine offered as a tab beside acestep. acestep = lighter and
// quicker; songgen = LeVo2, longer, with clone/continue from an audio prompt.
// Async on the server; koblem polls and returns the finished track (same shape
// as music). Tracks save to the shared SONGS_DIR and show in the same gallery.

export type SonggenMode = 'generate' | 'clone' | 'continue';

export type SonggenGenType = 'mixed' | 'vocal' | 'bgm';

export interface SonggenGenerateRequest {
  mode?: SonggenMode;
  /// Sung lyrics with [verse]/[chorus] tags. songgen SINGS these — it can't
  /// write them; leave blank with lyrics_source = "llm" to have the 9B write
  /// them, or blank with "ace" for an instrumental.
  lyric?: string;
  /// Natural-language style/mood description. Required for generate.
  description: string;
  /// When lyric is blank on generate: "ace" (default) = leave blank
  /// (instrumental); "llm" = the main 9B writes them (uses lyrics_prompt for the
  /// theme). songgen's LeLM never writes lyrics.
  lyrics_source?: MusicLyricsSource;
  lyrics_prompt?: string;
  /// draft-lyrics only: an existing lyric draft to improve in place (songgen's
  /// structured lyric rules) rather than write fresh. Ignored by generate.
  refine_source?: string;
  /// draft-lyrics only: free-form change to apply to refine_source; empty = a
  /// general polish.
  refine_instruction?: string;
  /// Omit → songgen renders to its model-max and stops on EOS (lyric-driven).
  /// clone hard-caps at 15 s unless set.
  duration?: number;
  seed?: number;
  gen_type?: SonggenGenType;
  temp?: number;
  top_k?: number;
  cfg?: number;
  fade?: number;
  save_song?: boolean;
}

export interface SonggenGenerateResponse {
  tracks: MusicTrack[];
  duration_used?: number;
  lyrics_used?: string;
  duration_ms?: number;
  gate_wait_ms?: number;
  gpu_log_id?: number;
}

export type AudioJobState = 'queued' | 'rendering' | 'done' | 'failed' | 'cancelled';

/// A songgen queue row. 🔴 `duration_used` and `lyrics_used` are NOT on it — those were echoes of
/// the engine's own accept, and the row keeps no copy. What the render produced is `tracks`.
export interface SonggenJob {
  id: string;
  state: AudioJobState;
  server_job_id?: string | null;
  phase?: string | null;
  tracks: MusicTrack[];
  warnings: string[];
  error?: string | null;
  queued_ms?: number | null;
  gate_wait_ms?: number | null;
  duration_ms?: number | null;
  blocked_reason?: BlockedReason | null;
  eta_start?: string | null;
}

export interface SonggenProps {
  healthy: boolean;
  version?: string;
}

export interface SonggenSuggestion {
  description: string;
  lyrics?: string;
}

export interface SonggenDraftLyricsResponse {
  lyrics: string;
  /// Advisory — see MusicDraftLyricsResponse.rhyme_flags.
  rhyme_flags?: RhymeFlag[];
}

export function getSonggenProps(): Promise<SonggenProps> {
  return request('/api/v1/songgen/props');
}

/// Expand a casual one-liner idea via the main 9B into a songgen description
/// (comma tags) and — unless descriptionOnly — matching structured lyrics.
export function songgenSuggest(brief: string, descriptionOnly = false): Promise<SonggenSuggestion> {
  return request('/api/v1/songgen/suggest', {
    method: 'POST',
    body: JSON.stringify({ brief, description_only: descriptionOnly })
  });
}

/// Draft songgen-format lyrics from a description (+ optional theme) with the
/// main LLM — fast, no render.
export function songgenDraftLyrics(
  description: string,
  lyricsPrompt?: string,
  refine?: { source: string; instruction?: string }
): Promise<SonggenDraftLyricsResponse> {
  const body: {
    description: string;
    lyrics_prompt?: string;
    refine_source?: string;
    refine_instruction?: string;
  } = { description, lyrics_prompt: lyricsPrompt };
  if (refine?.source.trim()) {
    body.refine_source = refine.source;
    if (refine.instruction?.trim()) body.refine_instruction = refine.instruction;
  }
  return request('/api/v1/songgen/draft-lyrics', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Rewrite only the line endings that don't rhyme when sung. User-triggered.
export function songgenFixRhymes(
  lyrics: string,
  description?: string
): Promise<SonggenDraftLyricsResponse> {
  return request('/api/v1/songgen/fix-rhymes', {
    method: 'POST',
    body: JSON.stringify({ lyrics, description })
  });
}

/// text→song. Returns as soon as the render is ACCEPTED — poll `getSonggenJob(job_id)`.
///
/// 🔴 Only this mode queues. `/clone`, `/continue` and `/separate` still answer with the finished
/// audio on the request that asked for it.
export function songgenGenerate(
  req: SonggenGenerateRequest,
  queue?: QueueSubmitOptions
): Promise<EnqueueResponse> {
  return enqueueJson(
    'songgen',
    '/api/v1/songgen/generate',
    { ...req, mode: 'generate' as const },
    queue
  );
}

export function getSonggenJob(id: string): Promise<SonggenJob> {
  return request(`/api/v1/songgen/jobs/${encodeURIComponent(id)}`);
}

/// The queued or running render, so a reloaded page re-finds the job whose id it never had.
export function getSonggenActive(): Promise<{ job: SonggenJob | null }> {
  return request('/api/v1/songgen/active');
}

/// clone / continue — multipart with either the vocal + bgm stems, or just a
/// full mix that songgen-server separates itself (htdemucs) before cloning.
export async function songgenTransform(
  req: SonggenGenerateRequest,
  mode: 'clone' | 'continue',
  vocalStem: File | null,
  bgmStem: File | null,
  fullMix?: File | null
): Promise<SonggenGenerateResponse> {
  const form = new FormData();
  form.append('request', JSON.stringify({ ...req, mode }));
  if (vocalStem) form.append('vocal_stem', vocalStem);
  if (bgmStem) form.append('bgm_stem', bgmStem);
  if (fullMix) form.append('full_mix', fullMix);
  const response = await authedFetch(`/api/v1/songgen/${mode}`, {
    method: 'POST',
    body: form
  });
  return response.json();
}

export interface SonggenSeparateResponse {
  vocal: MusicTrack;
  bgm: MusicTrack;
  duration_ms?: number;
  gate_wait_ms?: number;
}

/// Debug: htdemucs-split a (trimmed) mix into vocal + bgm stems — exactly what a
/// clone of the same audio feeds songgen internally (htdemucs is deterministic).
/// Synchronous; a few seconds of GPU. Both stems save to SONGS_DIR and come back
/// as MusicTracks.
export async function songgenSeparate(mix: File): Promise<SonggenSeparateResponse> {
  const form = new FormData();
  form.append('full_mix', mix);
  const response = await authedFetch('/api/v1/songgen/separate', {
    method: 'POST',
    body: form
  });
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}

// ── SA3 (Stable Audio 3 — the LIGHT music engine) ────────────────────────────
// A third music engine beside acestep/songgen: text→audio (game SFX / short
// music) plus audio editing (a2a restyle / inpaint / continuation). Async on the
// server; koblem polls and returns the finished clip (same MusicTrack shape).
// Tracks save to the shared SONGS_DIR and show in the same gallery.

export interface Sa3Props {
  healthy: boolean;
  version?: string | null;
}

export type Sa3EditMode = 'a2a' | 'inpaint' | 'continuation';

export interface Sa3GenerateRequest {
  /// Text description of the audio to generate.
  prompt: string;
  /// Output length in seconds (default 10; SA3 max 380).
  seconds?: number;
  seed?: number;
  /// "pingpong" (default, reference) or "euler" (deterministic).
  sampler?: 'pingpong' | 'euler';
  /// Diffusion steps (default 8 — the SA3 reference; leave it).
  steps?: number;
  /// Weight quant: "q8" (default) or "q6".
  model?: 'q8' | 'q6';
  /// Persist to the gallery (default true).
  save?: boolean;
}

export interface Sa3GenerateResponse {
  tracks: MusicTrack[];
  seconds_used?: number | null;
  duration_ms?: number | null;
  gate_wait_ms?: number | null;
}

/// An sa3 queue row. 🔴 `seconds_used` is NOT on it — that was the engine's accept echo and the row
/// keeps no copy; only `/edit`, which is still synchronous, still reports it.
export interface Sa3Job {
  id: string;
  state: AudioJobState;
  server_job_id?: string | null;
  phase?: string | null;
  tracks: MusicTrack[];
  warnings: string[];
  error?: string | null;
  queued_ms?: number | null;
  gate_wait_ms?: number | null;
  duration_ms?: number | null;
  blocked_reason?: BlockedReason | null;
  eta_start?: string | null;
}

export function getSa3Props(): Promise<Sa3Props> {
  return request('/api/v1/sa3/props');
}

/// text→audio (t2a). Returns as soon as the render is ACCEPTED — poll `getSa3Job(job_id)`.
///
/// 🔴 Only this mode queues. `/edit` still answers with the finished clip inline.
export function sa3Generate(
  req: Sa3GenerateRequest,
  queue?: QueueSubmitOptions
): Promise<EnqueueResponse> {
  return enqueueJson('sa3', '/api/v1/sa3/generate', req, queue);
}

export function getSa3Job(id: string): Promise<Sa3Job> {
  return request(`/api/v1/sa3/jobs/${encodeURIComponent(id)}`);
}

/// The queued or running render, so a reloaded page re-finds the job whose id it never had.
export function getSa3Active(): Promise<{ job: Sa3Job | null }> {
  return request('/api/v1/sa3/active');
}

/// audio→audio edit (a2a / inpaint / continuation) — multipart with the input
/// wav plus the per-mode knobs (strength for a2a, mask for inpaint, seconds for
/// continuation).
export async function sa3Edit(
  mode: Sa3EditMode,
  prompt: string,
  input: File,
  opts: {
    strength?: number;
    mask?: string;
    seconds?: number;
    seed?: number;
    sampler?: string;
    model?: string;
  } = {}
): Promise<Sa3GenerateResponse> {
  const form = new FormData();
  form.append('mode', mode);
  form.append('prompt', prompt);
  form.append('input', input);
  if (opts.strength != null) form.append('strength', String(opts.strength));
  if (opts.mask) form.append('mask', opts.mask);
  if (opts.seconds != null) form.append('seconds', String(opts.seconds));
  if (opts.seed != null) form.append('seed', String(opts.seed));
  if (opts.sampler) form.append('sampler', opts.sampler);
  if (opts.model) form.append('model', opts.model);
  const response = await authedFetch('/api/v1/sa3/edit', {
    method: 'POST',
    body: form
  });
  return response.json();
}

// ── MiniMax Music 3 (the `music3` plugin on comfy-api) ───────────────────────
// A fourth music engine beside acestep/songgen/sa3. Text→song only: a structured
// CAPTION plus tagged LYRICS become a complete track (stereo, up to 360 s). Delivered files probe
// at 44100 Hz — the model card's "32 kHz" is not what arrives, so trust `Music3Track.sample_rate`.
// Tracks save to the shared SONGS_DIR and show in the same gallery.
//
// 🔴 There is NO audio input of any kind — no cover, no repaint, no continuation,
// no reference timbre. The model's DAV VAE cannot encode, so those requests are a
// 501 from the engine rather than a silently-ignored field. Do not add an upload.

export interface Music3Props {
  configured: boolean;
  healthy: boolean;
  /** `GET /music3/v1/capabilities` verbatim — the model law (duration ceiling, prompt-token cap,
   *  sampler/scheduler/format lists). Read this rather than keeping a second copy of the numbers. */
  capabilities?: Record<string, unknown> | null;
}

export interface Music3GenerateRequest {
  /** The three-section structured caption (Global Metadata / Vocal Details / Arrangement). */
  caption: string;
  /** Tagged lyrics. Absent or empty = instrumental. */
  lyrics?: string;
  /** 🔴 A CEILING, not a target: the model's AR stage may end the song earlier, and the
   *  response's `duration_seconds` is what actually came out. 0.04–360. */
  max_duration?: number;
  /** Omit to have the engine draw one per take and echo it back in `plan`. */
  seed?: number;
  steps?: number;
  /** KSampler cfg. */
  cfg?: number;
  /** The AR stage's own CFG (`MiniMaxMusic3TextEncode.cfg_scale`) — a different stage from `cfg`,
   *  not another name for it. */
  cfg_scale?: number;
  top_k?: number;
  sampler?: string;
  scheduler?: string;
  /** Tiled audio decode. 🔴 NOT the VRAM-for-quality trade ComfyUI's template describes — measured
   *  at 120 s it is **24% FASTER** (125.0 s vs 165.0 s wall), uses **MORE** VRAM (15730 vs
   *  14420 MiB peak), and is lossless (identical duration, byte counts within 153, audio metrics
   *  matching to 4 dp, no seams). Off by default only because the tiled peak is 96% of the card. */
  tiled_decode?: boolean;
  /** Independent takes (1–4), each its own render. */
  takes?: number;
  /** 🔴 `flac` is the LOSSLESS option — there is no `wav`. ComfyUI has no wav writer anywhere
   *  (`SaveAudioAdvanced` does flac/mp3/opus; `SaveAudio` is flac-only), so the engine 400s on it. */
  format?: 'flac' | 'mp3' | 'opus';
  /** mp3/opus only; reported in `warnings` when ignored. */
  quality?: string;
  /** Persist to the gallery (default true). */
  save?: boolean;
}

export interface Music3Track {
  url: string;
  filename: string;
  index: number;
  seed?: number | null;
  content_type: string;
  bytes: number;
  /** 🔴 The RESOLVED length from the AR stage. Show THIS, never the requested `max_duration` —
   *  they differ whenever the model chose to end the song early, which is normal. */
  duration_seconds?: number | null;
  sample_rate?: number | null;
  channels?: number | null;
}

export type Music3JobState = 'queued' | 'rendering' | 'done' | 'failed' | 'cancelled';

export interface Music3Job {
  id: string;
  state: Music3JobState;
  /** comfy-api's own job id, for finding the render in its queue. */
  server_job_id?: string | null;
  progress: Music3Progress;
  tracks: Music3Track[];
  /** The accept echo plus the completion's authoritative `plan` block. */
  plan: Record<string, unknown>;
  warnings: string[];
  /** Fields the engine accepted and discarded — surfaced so a setting that did nothing is
   *  visible rather than discovered later. */
  ignored: string[];
  error?: string | null;
  gate_wait_ms?: number | null;
  duration_ms?: number | null;
  blocked_reason?: BlockedReason | null;
  eta_start?: string | null;
}

export interface Music3Progress {
  progress?: number | null;
  current_step?: number | null;
  total_steps?: number | null;
  /** 🔴 `conditioning` = the autoregressive stage, which reports NO steps and can be a large
   *  share of wall clock; `denoising` = the DiT, which does; `decoding` = the audio VAE.
   *
   *  Do NOT hardcode those words. `capabilities.progress.stepped_phases` /
   *  `.unstepped_phases` publish the split — drive the UI off those, so a phase that reports no
   *  steps can never render as an unlabelled bar frozen at zero. */
  phase?: string | null;
  track_index?: number | null;
  track_count?: number | null;
  tracks_done?: number | null;
  /** The engine's ETA for this job, carried from the accept body.
   *
   *  🔴 An **upper** estimate — the model routinely ends songs early, so renders finish well under
   *  it. Show it as text; do NOT make it the denominator of a progress bar, which would hit 100%
   *  and stall. The engine publishes the fitted curve as `capabilities.eta_model`; never re-derive
   *  it here. */
  eta_seconds?: number | null;
}

export interface Music3SuggestRequest {
  /** The user's rant — however they described the song they want. */
  brief: string;
  /** Write only the caption and leave the lyrics to the user. */
  caption_only?: boolean;
  /** A caption the user already has. With an empty `brief`, only lyrics are written to fit it. */
  caption?: string;
  /** The duration ceiling, so the writer plans a sensible number of sections. */
  max_duration?: number;
}

export interface Music3SuggestResponse {
  caption: string;
  lyrics?: string | null;
  /** Which of MiniMax's 18 official style families the router picked. Shown so a bad route is
   *  visible rather than inferred from a caption that merely feels wrong. */
  family: string;
}

/** What a rework is allowed to touch. */
export type Music3ReworkScope = 'both' | 'caption' | 'lyrics';

export interface Music3ReworkRequest {
  caption: string;
  lyrics?: string;
  /** The user's feedback, verbatim — "chorus needs to hit harder", "lose the strings". */
  instruction: string;
  /** Defaults to 'both'. */
  scope?: Music3ReworkScope;
  max_duration?: number;
}

export interface Music3ReworkResponse {
  caption: string;
  lyrics?: string | null;
  family: string;
  /** True when the instruction moved the style family. */
  family_changed: boolean;
}

export function getMusic3Props(): Promise<Music3Props> {
  return request('/api/v1/music3/props');
}

/// text→song. Returns as soon as the render is ACCEPTED — poll `getMusic3Job(job_id)` for the
/// result.
export function music3Generate(
  req: Music3GenerateRequest,
  queue?: QueueSubmitOptions
): Promise<EnqueueResponse> {
  return enqueueJson('music3', '/api/v1/music3/generate', req, queue);
}

/// Poll a render. 404 once the record has expired (30 min after it finishes — the audio itself
/// stays in the gallery).
export function getMusic3Job(id: string): Promise<Music3Job> {
  return request(`/api/v1/music3/jobs/${encodeURIComponent(id)}`);
}

/// Apply one piece of feedback to an existing caption + lyrics. LLM only, no GPU.
///
/// 🔴 An EDIT, not a regeneration: the server prompt is explicit that anything the instruction does
/// not mention comes back unchanged, and it refuses rather than returning an empty half — so a
/// caller can safely overwrite its editor from the response.
export function music3Rework(req: Music3ReworkRequest): Promise<Music3ReworkResponse> {
  return request('/api/v1/music3/rework', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

/// The render currently holding the slot, if any — lets a reloaded page re-find the job whose id
/// it lost, which is the whole reason this engine has a job store.
export function getMusic3Active(): Promise<{ job: Music3Job | null }> {
  return request('/api/v1/music3/active');
}

/// Expand a rant into a caption (+ lyrics). LLM only — no GPU slot is held.
export function music3Suggest(req: Music3SuggestRequest): Promise<Music3SuggestResponse> {
  return request('/api/v1/music3/suggest', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

// ── Avatar (LongCat-Video-Avatar — see longcat-avatar.cpp/GO-LIVE-PLAN.md) ───

export type AvatarOffload = 'true' | 'false' | 'auto';

export type AvatarOrientation = 'portrait' | 'landscape';

export interface AvatarBsaConfig {
  enable: boolean;
  radius?: number;
  self_frame?: boolean;
  bookend?: number;
  cube_h?: number;
  cube_w?: number;
  bitmap?: boolean;
}

export type AvatarImageSource =
  { kind: 'upload' } | { kind: 'url'; url: string } | { kind: 'library_asset_id'; id: string };

export type AvatarAudioSource =
  | { kind: 'upload' }
  | { kind: 'url'; url: string }
  | { kind: 'library_clip_id'; id: string }
  | {
      kind: 'tts';
      script?: string;
      script_from_brief?: string;
      voice?: string;
      engine?: string;
    };

export interface AvatarGenerateRequest {
  image_source: AvatarImageSource;
  audio_source: AvatarAudioSource;
  prompt?: string;
  resolution?: '480p' | '720p';
  orientation?: AvatarOrientation;
  duration_sec?: number;
  segments?: number;
  segment_frames?: number;
  cont_cond_frames?: number;
  steps?: number;
  cfg_scale?: number;
  seed?: number;
  audio_mouth_scale?: number;
  audio_lowpass_hz?: number;
  offload?: AvatarOffload;
  vae_tiling?: boolean;
  bsa?: AvatarBsaConfig;
  debug_env?: Record<string, string>;
  save_clip?: boolean;
}

export interface AvatarClip {
  filename: string;
  url: string;
  content_type: string;
  bytes: number;
  duration_ms?: number;
}

/** 🔴 `POST /generate` answers with THIS — the render is accepted, not finished. Poll
 *  `getAvatarJob(job_id)`.
 *
 *  `script_used` rides back here and NOWHERE ELSE: the 9B wrote it a moment ago and the queue row
 *  keeps no copy the poll could read later. */
export interface AvatarAcceptResponse {
  job_id: string;
  state: JobState;
  position?: number | null;
  deduplicated: boolean;
  blocked_reason?: BlockedReason | null;
  script_used?: string | null;
}

export type AvatarJobState = 'queued' | 'rendering' | 'done' | 'failed' | 'cancelled';

/** An avatar queue row. 🔴 `segments_rendered` and `offload_used` are NOT on it — those were the
 *  supervisor's accept echo and the row keeps no copy. `clip` is what the old response carried. */
export interface AvatarJob {
  id: string;
  state: AvatarJobState;
  phase?: string | null;
  clip: AvatarClip | null;
  warnings: string[];
  error?: string | null;
  queued_ms?: number | null;
  gate_wait_ms?: number | null;
  render_ms?: number | null;
  blocked_reason?: BlockedReason | null;
  eta_start?: string | null;
}

export interface AvatarProps {
  healthy: boolean;
  default_segment_frames: number;
  default_cont_cond_frames: number;
  default_steps: number;
  default_resolution: string;
  last_render_sec?: number;
}

export interface AvatarScriptFromLlmResponse {
  script: string;
}

export function getAvatarProps(): Promise<AvatarProps> {
  return request('/api/v1/avatar/props');
}

/// Drive a full avatar render. Returns as soon as it is ACCEPTED — poll `getAvatarJob(job_id)`.
/// Multipart with optional `image` / `audio` parts when the request's *_source = "upload".
export async function avatarGenerate(
  req: AvatarGenerateRequest,
  image?: File | null,
  audio?: File | null,
  queue?: QueueSubmitOptions
): Promise<AvatarAcceptResponse> {
  if (!image && !audio) {
    return enqueueJson('avatar', '/api/v1/avatar/generate', req, queue);
  }
  const form = new FormData();
  form.append('request', JSON.stringify(req));
  if (image) form.append('image', image);
  if (audio) form.append('audio', audio);
  return enqueueForm('avatar', '/api/v1/avatar/generate', form, queue);
}

export function getAvatarJob(id: string): Promise<AvatarJob> {
  return request(`/api/v1/avatar/jobs/${encodeURIComponent(id)}`);
}

/// The queued or running render, so a reloaded page re-finds the job whose id it never had.
export function getAvatarActive(): Promise<{ job: AvatarJob | null }> {
  return request('/api/v1/avatar/active');
}

/// Have the main 9B write a short spoken script from a brief. Ungated.
export function avatarScriptFromLlm(
  brief: string,
  durationSec?: number,
  refine?: { source: string; instruction?: string }
): Promise<AvatarScriptFromLlmResponse> {
  const body: {
    brief: string;
    duration_sec?: number;
    refine_source?: string;
    refine_instruction?: string;
  } = { brief, duration_sec: durationSec };
  if (refine?.source.trim()) {
    body.refine_source = refine.source;
    if (refine.instruction?.trim()) body.refine_instruction = refine.instruction;
  }
  return request('/api/v1/avatar/script-from-llm', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Fetch a rendered clip as an object URL for `<video>` playback.
export async function fetchAvatarClip(url: string): Promise<string> {
  const response = await authedFetch(url);
  const blob = await response.blob();
  return URL.createObjectURL(blob);
}

// ── LTX multi-segment video (T2V/I2V/AT2V/AI2V — generate_video_chain) ───────

export type LtxImageSource =
  | { kind: 'none' }
  | { kind: 'upload' }
  | { kind: 'url'; url: string }
  | { kind: 'library_asset_id'; id: string };

export type LtxAudioSource =
  | { kind: 'none' }
  | { kind: 'voice_upload' }
  | { kind: 'bgm_upload' }
  | { kind: 'mixed_upload' }
  | { kind: 'tts'; voice?: string };

/** A frame-pinned image within a shot (identity injection / reveal / first+last frame). */
export interface LtxKeyframe {
  /** Bare (or data-URI) base64 of the pinned image. */
  image_b64: string;
  /** Time within the shot (seconds) where it's pinned; koblem maps to a frame index. */
  seconds: number;
}

/**
 * How the engine sizes a TASS reference image before encoding it.
 *  - `native_resolution` (default) — encode the sheet at its OWN resolution. Correct for a
 *    character sheet: the reference rides on the token axis, so it can carry its own spatial grid,
 *    and a native 1536×1024 sheet keeps the per-panel face detail the checkpoint relies on.
 *  - `match_target` — resize to the render's resolution. Squashes a sheet into the video bucket.
 */
export type LtxRefResizeMode = 'native_resolution' | 'match_target';

/**
 * One character-sheet identity reference attached to a shot (TASS overlap conditioning).
 *
 * A sheet is a single 1536×1024 image with four panels of ONE character — face close-up, then full
 * body front / side / back. Attaching it holds that character's identity in pure **t2v**: no i2v
 * opener, no continuation needed, and the shot is NOT made fresh by attaching one.
 *
 * `source_id` is the identity TAG the model keys off, not an array index. It must be the same
 * number for the same character in every shot of a scene, or identity changes shot to shot and the
 * mechanism is broken — so the Director allocates it once per character at PROJECT level
 * (`DirectorCharacter.source_id`) and never renumbers.
 *
 * Multi-character is architecturally supported (the tag generalises to 2, 3, 4…) but was NOT
 * trained: upstream training was single-subject and a 4-panel sheet depicts one person.
 */
export interface LtxCharacterRef {
  /** Bare (or data-URI) base64 of the sheet — an `asset:<hash>` ref inside a saved document. */
  image_b64: string;
  /** The TASS source-phase tag (>= 2). Absent → the engine assigns 2, 3, 4… in array order. */
  source_id?: number;
  /**
   * Absent → the backend fills it from the render's identity regime: `native_resolution` for a
   * 1536×1024 character sheet (the `faceid-sheet-rt` adapter), `match_target` for an Echo or MSR
   * memory slot (which encode their slots at render size). Set it only to override that.
   */
  resize_mode?: LtxRefResizeMode;
  /** Character name. Editor/log only — the backend never forwards it to the engine. */
  name?: string;
  /**
   * Identity attributes (skin tone, hair, eyes, facial hair, glasses, face shape). The backend
   * folds this into the rendered prompt after the `ref_t2v: ` prefix — the model card is explicit
   * that identity is strongly prompt-driven and that a bare prompt gives weak identity.
   */
  identity?: string;
}

/**
 * Per-shot generation mode. Absent → inferred from `image`/`keyframes` (legacy t2v/i2v). A
 * `v2v_*` mode renders the shot from `v2v_source` (video-to-video):
 *  - `v2v_restyle` — SDEdit restyle (engine v2v_mode=1): re-render under a new prompt; strength
 *    1 = ignore source, lower = keep more of it. A restyle, not structure-preserving.
 *  - `v2v_edit` — guide-edit (engine v2v_mode=2): hold the source scene as a partial-strength
 *    guide and add the prompt's change ("keep the scene, add an explosion"). strength 1 = hold
 *    the scene hard (subtle edit), ~0.5 = bigger edit. The Director-2 "edit a video" mechanism.
 */
export type LtxSegmentMode = 'auto' | 't2v' | 'i2v' | 'v2v_restyle' | 'v2v_edit';

/** Where a v2v shot's source clip comes from. */
export type LtxV2vSource =
  /** Foreign/uploaded clip — bytes ride as multipart `seg_v2v_<i>` or inline `assets.seg_v2v_b64`. */
  | { kind: 'upload' }
  /** A clip WE rendered: a prior koblem LTX job + its FILTERED (rendered) segment index. */
  | { kind: 'prior_render'; job_id: string; segment: number };

export interface LtxSegment {
  prompt: string;
  /**
   * Base DiT variant for THIS shot. Absent inherits the request's top-level `model`.
   *
   * The engine leases variants at segment boundaries, so a chain can mix them. As of 2026-07-30
   * this is an AUTHOR'S PIN only: the automatic prop routing sets `loras` instead, because the
   * `msr-v2` checkpoint it used to switch to is deleted and MSR is a runtime adapter now. See
   * `resolveClipVariants`.
   *
   * Not free: each CHANGE of variant down the shot order is a DiT swap on the worker, so grouping
   * the shots that need one is worth doing. Unchanged neighbours cost nothing.
   */
  model?: string;
  /**
   * Runtime LoRAs for THIS shot only. Empty/absent inherits the request's top-level `loras`;
   * a non-empty list REPLACES them for this shot (so a shot can also drop an inherited one).
   * Switched at the same segment boundary as a per-shot `model`, but much cheaper — re-applying
   * adapters is ~1s vs a whole DiT swap. Needs an engine that parses a per-segment `lora` key;
   * older engines ignore it silently and use the top-level set for every shot.
   */
  loras?: LtxRuntimeLora[];
  seconds?: number;
  /**
   * Frame-pinned images within this shot. A shot with keyframes renders fresh (i2v keyframe
   * path); frame 0 = first, shot-length = last, a middle time = a reveal. Wins over `image`.
   */
  keyframes?: LtxKeyframe[];
  /**
   * Optional per-shot scene image (i2v). When set, this shot starts a NEW scene from the
   * image instead of continuing the previous shot; audio stays continuous across the cut.
   * Shot 0's image is the opener (overrides top-level `image_source`); later shots = a
   * mid-chain scene change. For an uploaded file the bytes ride as multipart `seg_image_<i>`
   * and this is `{ kind: "upload" }`.
   */
  image?: LtxImageSource;
  /** Generation mode. Absent → inferred t2v/i2v. A `v2v_*` mode renders from `v2v_source`. */
  mode?: LtxSegmentMode;
  /** Per-segment v2v guide/edit strength (0..1). See `LtxSegmentMode`. */
  v2v_strength?: number;
  /** Source clip for a `v2v_*` shot. Ignored for non-v2v shots. */
  v2v_source?: LtxV2vSource;
  /**
   * Force a FRESH scene from this shot's prompt alone — a hard cut with no init image and no
   * continuation from the prior shot. Absent/false = ordinary continuation. Only meaningful for
   * shot index >= 1 (shot 0 is always fresh); redundant when the shot has an `image` or
   * `keyframes`, whose frame-0 guide clears the continuation inside the engine anyway.
   *
   * 🔴 **NOT redundant on a `v2v_source` shot — REQUIRED.** A v2v source is not a guide: it is
   * VAE-encoded over the init latent, which is the very thing a continuation carry writes into. The
   * engine refuses the pair outright rather than obliterating one of them (the retired C++ engine
   * obliterated the carry), so an unmarked v2v shot past index 0 is a 400. `compileSegment` emits
   * this for every sourced v2v shot at index >= 1.
   */
  scene_cut?: boolean;
  /**
   * Per-shot image-pin hold strength (this shot's keyframes / scene image), 0..1. 1 = the pinned
   * image is held exactly at its frame (crispest); lower blends it into the generated motion.
   * Absent → the request's top-level `pin_strength`, else the backend default. The backend folds
   * the first shot that sets it into the chain-global pin `strength`.
   */
  pin_strength?: number;
  /** What this shot's own audio clip is for. Absent → `voice`. Only read alongside `audio_b64`. */
  audio_kind?: LtxShotAudioKind;
  /**
   * This shot's own audio, base64 (or data-URI). Starts at THIS shot and is cut at the shot
   * boundary, so it can never bleed into the next; shorter than the shot, it's padded with
   * silence. Composited OVER the chain-level `audio_source` track, which therefore still works as
   * a bed each shot replaces its own span of (music bed + per-shot VO).
   */
  audio_b64?: string;
  /**
   * Prompt Relay beats for this shot, in VISIBLE clip seconds. Absent/empty → one prompt for the
   * whole shot (byte-identical to before). The backend shifts each beat onto the render timeline
   * by this shot's seam drop, so `seconds: 0` is the first frame the viewer sees.
   */
  beats?: LtxBeat[];
  /** Per-shot seed. Absent → the chain seed offset by shot index. */
  seed?: number;
  /** Per-shot base step count. Absent → the render's steps. */
  steps?: number;
  /** Per-shot text CFG. Absent → the render's cfg. */
  cfg?: number;
  /** Per-shot negative prompt. Absent/empty → the render's negative prompt. */
  negative_prompt?: string;
  /**
   * Character sheets conditioning this shot's identity. Absent/empty → today's behaviour exactly
   * (the engine keeps its untagged path). The backend prefixes this shot's rendered prompt with
   * `ref_t2v: ` whenever it's non-empty — do NOT type the prefix by hand as well.
   */
  character_refs?: LtxCharacterRef[];
  /**
   * Drop the reference-contaminated leading frame(s) of THIS shot. Absent inherits the request's
   * top-level `frame_drop`; a value here is authoritative for this shot, `"off"` included — which
   * is the only way to switch the trim off for one shot of a project that has it on.
   */
  frame_drop?: LtxFrameDrop;
}

/**
 * Drop the leading frame(s) a character reference contaminates on a bare t2v shot.
 *
 * A reference sits at the target's first-frame rotary address, and on some checkpoints the decoder
 * hands that address straight back: pixel frame 0 renders the reference verbatim. With a location
 * plate in the set, frame 0 is the empty plate (which reads as "the subject never appears"); with
 * only a character sheet, it is that sheet's own source photo, background and all.
 *
 * | value | meaning |
 * |---|---|
 * | `"auto"` | let the engine decide how many frames to drop (the default) |
 * | `"off"`  | never trim |
 * | a number | drop exactly that many frames — an escape hatch, effectively never needed |
 *
 * Whether it happens is a property of the CHECKPOINT: `echo-e50` leaks, `echo-full` and `base` do
 * not, and no adapter changes it either way. `"auto"` is safe as a project-wide default, because the
 * engine self-gates the
 * trim to a no-op on any shot that already pins frame 0 (i2v scene image, keyframe at 0, or a
 * continuation shot) and on any shot with no references. The shot comes back one frame SHORTER
 * rather than being re-rendered — LTX frame counts are `8k+1`, so the smallest legal re-render
 * would cost eight.
 */
export type LtxFrameDrop = 'auto' | 'off' | number;

/** One Prompt Relay beat: a timed sub-prompt inside a shot. See `DirectorBeat`. */
export interface LtxBeat {
  /** Seconds on the shot's VISIBLE clip timeline. */
  seconds: number;
  /** The beat clause — one action, short. */
  text: string;
  /** Penalty multiplier. Absent → 1.0. */
  strength?: number;
  /** Flat-top half-width in seconds. Absent → the paper default (~0.67s crossfade at 24fps). */
  window?: number;
}

/** What a per-shot audio clip is for: drives lip-sync + delivered, or delivered only. */
export type LtxShotAudioKind = 'voice' | 'bgm';

/** Spatial upscaler ratio for the LTX hires pass. */
export type LtxUpscalerRatio = 'none' | 'x1.5' | 'x2';

/** Named LTX quality profile. `standard` (default) is byte-identical to the historical
 * single-hires ver3 request; `high`/`upscale_4x` bake the multi-stage upscale recipe;
 * `custom` exposes every field. */
export type LtxQualityProfile = 'standard' | 'high' | 'upscale_4x' | 'custom';

/** One upsample + SDEdit-refine stage in a Custom-profile `hires_chain`. Any field left
 * undefined falls to koblem's contract default (×2 upscaler, `euler_cfg_pp`, cfg 1.0,
 * 3 steps, sigmas [0.85, 0.725, 0.421875, 0.0]). */
export interface LtxHiresStage {
  /** Full spatial ×2 upscaler name (e.g. "ltx-2.3-spatial-upscaler-x2-1.1"). */
  upscaler?: string;
  custom_sigmas?: number[];
  sample_method?: string;
  cfg?: number;
  steps?: number;
}

export interface LtxVideoGenerateRequest {
  segments: LtxSegment[];
  /** Base DiT variant name (server-registered, e.g. "base", "audioreactive"). Absent → "base". */
  model?: string;
  /**
   * Runtime LoRAs applied on top of `model`, each at its own strength. Absent/empty → none.
   * Orthogonal to `model`: a folded variant already contains its LoRA, so don't send the same
   * one again here. Each adapter costs VRAM and sampling time.
   */
  loras?: LtxRuntimeLora[];
  image_source?: LtxImageSource;
  audio_source?: LtxAudioSource;
  negative_prompt?: string;
  width?: number;
  height?: number;
  fps?: number;
  segment_frames?: number;
  cont_latent_frames?: number;
  steps?: number;
  cfg_scale?: number;
  /** How hard image pins (keyframes/scene images/opener) hold: 1.0 = exact, lower = softer blend. */
  pin_strength?: number;
  /**
   * Prompt Relay tail floor, chain-global. Absent → engine default 0.01. Lower = harder
   * separation between beats. Only meaningful when some shot carries `beats`.
   *
   * ⚠️ NOT portable. It feeds `sigma = (L - w) / sqrt(2 ln(1/eps))`, whose width scales with the
   * beat's own cell `L`; the published ComfyUI relay packs use `sigma = 1/log(1/eps)`, which does
   * not. A value copied from one of those means something else here.
   */
  relay_eps?: number;
  /**
   * Fraction of the base schedule that carries the relay mask, chain-global. Absent → 1.0.
   *
   * 🔴 **Accepted and NOT honoured on LTX-2.5**, where the relay is a block-level attention patch:
   * retiring the mask partway needs a per-step hook such a patch does not receive, so the mask
   * stays on for the whole sample whatever this says. The consequence is behavioural rather than
   * cosmetic — beats read HARDER there than they did on the C++ engine, which did retire it.
   */
  relay_steps_frac?: number;
  sampling_method?: string;
  scheduler?: string;
  seed?: number;
  /** Legacy ×2 toggle; prefer `upscaler_ratio`. `true` ≡ `upscaler_ratio: "x2"`. */
  upscale?: boolean;
  /** Spatial upscaler ratio. Absent → falls back to `upscale` (true → x2, false → none). */
  upscaler_ratio?: LtxUpscalerRatio;
  /** Refine (hires) pass sampler. Absent → backend ver3 default ("euler"). */
  refine_sampler?: string;
  /** Refine (hires) pass step count. Absent → backend ver3 default (2). */
  refine_steps?: number;
  /** Explicit base-pass sigma schedule. Absent → backend ver3 default array. */
  base_custom_sigmas?: number[];
  /** Explicit refine-pass sigma schedule. Absent → backend ver3 default array. */
  refine_custom_sigmas?: number[];
  /** Named quality profile. Absent → "standard" (byte-identical to the ver3 request). */
  profile?: LtxQualityProfile;
  /** Custom-profile multi-stage upscale+refine chain. Consulted only when `profile === "custom"`;
   * empty/absent → no upscale chain. High/4x build their own and ignore this. */
  hires_chain?: LtxHiresStage[];
  /** "Detail / upscale strength" for the FINAL refine stage of the baked High/4× recipes — the
   * renoise sigma0 (default ~0.85). Higher (→1) = sharper, more new detail (can drift); lower
   * (→0.5) = truer to the base. Absent → the exact baked schedule (byte-identical). Ignored by
   * Standard/Custom. */
  upscale_detail_strength?: number;
  /** Interpret `width`/`height` as the desired OUTPUT resolution rather than the engine base pass.
   * When true, the quality profile sizes its base DOWN by the total upscale factor so every profile
   * lands on the same output (a 4×-upscale render starts from a quarter-size base). Absent/false =
   * legacy: the dims ARE the base pass. The Director sets this true; other panels leave it unset. */
  dims_are_target?: boolean;
  /**
   * TASS source-phase scale, chain-global. Only sent when some shot carries `character_refs`.
   *
   * Absent → the backend supplies the resolved REGIME's default: nothing in the `sheet` regime
   * (leaving the engine on 1.0, which the sheet adapter's own metadata declares), and an explicit
   * **0** in `memory` — Echo and MSR are UNTAGGED, and 0 is not the same as omitting the field.
   * Setting it by hand is an experiment, not a setting.
   */
  tass_phase_scale?: number;
  save_clip?: boolean;
  /** Resume a failed/cancelled render from its banked segments (the server job id). */
  resume_job_id?: string;
  /**
   * Project-level default for `LtxSegment.frame_drop`. Absent = `"auto"`, which is safe as a
   * default because the engine self-gates the trim off every shot that already pins frame 0.
   */
  frame_drop?: LtxFrameDrop;
}

export interface LtxVideoClip {
  filename: string;
  url: string;
  content_type: string;
  bytes: number;
  duration_ms?: number;
}

export interface LtxVideoGenerateResponse {
  clip: LtxVideoClip;
  segments_rendered: number;
  render_ms: number;
  gate_wait_ms: number;
  scripts_used?: string[];
  gpu_log_id?: number;
}

export interface LtxVideoProps {
  healthy: boolean;
  default_segment_frames: number;
  default_cont_latent_frames: number;
  default_steps: number;
  default_fps: number;
  default_resolution: string;
  last_render_sec?: number;
  /** Server-registered base DiT variant names for the model selector (e.g. ["base", ...]). */
  variants?: string[];
  /**
   * Runtime LoRAs the engine offers. Each can be applied per render (and per shot) at its own
   * strength — the tunable counterpart to a folded variant, which bakes its LoRA in at a fixed
   * strength but costs nothing extra at render time.
   *
   * 🔴 [2026-07-30] WAS `string[]` of bare file names, and the picker guessed everything else.
   * The entries are annotated now, sourced from `loras.json` in the engine's own LoRA directory
   * (falling back to koblibs' compiled-in catalogue): see {@link LtxLoraInfo}.
   */
  loras?: LtxLoraInfo[];
  /**
   * Whether THIS engine build can resume a render and retake a single shot right now, read live
   * from its own capabilities (`bank.enabled` AND `resume.supported` AND `retake.supported`).
   *
   * 🔴 A DEPLOYMENT FACT, not a feature flag — which is why it overrides the compiled-in
   * capability rather than restating it. LTX-2.5 is fully capable of both and answers 501 to each
   * when its bank directory is unconfigured, so drawing the controls off the engine's NAME offers a
   * button that fails at submit.
   *
   * ⚠️ Absent means "no live fact", and what that costs depends on the engine — so read it through
   * `resumeAvailable` in `lib/director/engine.ts` rather than here. koblem asks only an engine that
   * could plausibly answer, so absence is a NO on LTX-2.5 (unreachable, and there is nothing to
   * resume from) and carries nothing at all for the C++ ltx-video build, whose bank is part of the
   * build and which is never asked.
   */
  bank_enabled?: boolean;
  /**
   * Is the Prompt Relay custom node mounted — i.e. will a shot carrying `beats` render?
   *
   * 🔴 The same shape as `bank_enabled`, and the same reason for existing. LTX-2.5 implements
   * Prompt Relay, but it is served by `custom_nodes/ltx25_prompt_relay`, which ComfyUI lists in
   * `/object_info` only after the **comfyui** container restarts — separately from the comfy-api
   * container that answers this endpoint. Until then `beats` is refused at graph build with a 501,
   * after the GPU-gate wait. `undefined` = the engine did not answer, or is old enough not to
   * publish it; `beatsAvailable()` in `#lib/director/engine` is what a control reads.
   */
  beats_enabled?: boolean;
}

/**
 * One adapter the engine offers, plus what is known about how to USE it.
 *
 * The three annotated fields are things a file name cannot carry and the UI was previously
 * guessing at 1.0 / no-trigger / raw-filename:
 *
 * - `default_multiplier` — the strength this adapter should be ADDED at. Not always 1.0. None of
 *   these files ship `.alpha` tensors, so the engine applies the multiplier raw, and an adapter
 *   trained at alpha/r = 2 rendered at 1.0 is simply half an adapter with nothing on screen or on
 *   the wire saying so.
 * - `trigger` — a phrase the prompt must contain before the adapter does anything. The BACKEND
 *   appends it (per shot, against that shot's effective set); this field is here so the UI can say
 *   that is happening rather than leaving it to look like magic.
 * - `label` / `blurb` — display only. The real file name is always shown alongside, because that
 *   is the string an operator needs when the engine 400s on it.
 *
 * `path` is the only field that goes back on the wire.
 */
export interface LtxLoraInfo {
  /** File name under the engine's LoRA directory, **including the extension**. The wire value. */
  path: string;
  /** Display name. The file name itself when the adapter is undescribed. */
  label: string;
  /** One-line description; absent/empty when undescribed. */
  blurb?: string;
  /** Strength to add this adapter at. */
  default_multiplier: number;
  /** Phrase auto-appended to the prompts of shots this adapter is on, if any. */
  trigger?: string;
  /** True when the adapter does nothing at all without its trigger. */
  trigger_required?: boolean;
  /**
   * Whether the engine that listed this adapter can actually use it. Absent ⇒ yes.
   *
   * 🔴 Informational only HERE — the backend already filters these out of `props.loras`, so a UI
   * that ignores this field still cannot offer one. Present because the answer is not derivable
   * from the file name and a picker that ever shows the unfiltered listing must not treat every
   * entry as usable: `h3` and `ltx25` are two plugins on ONE ComfyUI sharing one `models/loras`,
   * and an H3 adapter attached to the LTX DiT matches zero keys and renders the unmodified model.
   */
  for_this_model?: boolean;
}

/** One runtime LoRA applied to a render, with its strength. 1.0 = the LoRA's native scale. */
export interface LtxRuntimeLora {
  /** File name under the engine's LoRA directory, **including the extension**. */
  path: string;
  multiplier: number;
}

/**
 * One `[Shot N]` body as the H3 prompt writer returns it.
 *
 * `index` is 0-based within its GENERATION and `at_seconds` is `null` on index 0, which is the
 * untimed opening shot. Both are advisory: the editor re-times every cut from the live packing,
 * because a shot trimmed after the draft was written moves every cut after it.
 */
export interface H3DraftShot {
  index: number;
  at_seconds: number | null;
  body: string;
}

/**
 * What the H3 prompt writer returns per GENERATION — the structured object, not prose.
 *
 * Mirrors `PROMPT-CONTRACT.md` §2 and koblibs' `H3PromptDraft`. This is the typed carriage: the
 * same object used to arrive as JSON text inside `prompt` / `segments`, and
 * `lib/director/h3-llm.ts` still parses that form because an older koblem, an older project and
 * the chat surface all still produce strings. Prefer the typed field; keep the fallback.
 *
 * Every field is optional except `shots` — an omitted one means "the human or a later pass fills
 * it", never "clear what is there".
 */
export interface H3PromptDraft {
  task_types?: H3TaskType[];
  style_opening?: string;
  subject_definitions?: H3SubjectDefinition[];
  summary?: string;
  retention_analysis?: H3RetentionEntry[];
  shots: H3DraftShot[];
  overall_soundscape?: string;
  non_diegetic_music?: string;
}

export interface LtxScriptFromLlmResponse {
  /**
   * @deprecated The prose carrier. Kept so a koblem that predates the typed field still works, and
   * marked for deletion — on H3 it carries the draft as JSON text and `h3_generations` carries it
   * properly.
   */
  segments: string[];
  /**
   * The H3 drafts, one per GENERATION, in order.
   *
   * 🔴 Per generation, not per shot. The six sections are properties of a render unit, and a
   * generation holds 1–N shots whose bodies come back in `shots[]` — so this array is NOT aligned
   * to `segments`, and zipping the two is the mistake to avoid.
   */
  h3_generations?: H3PromptDraft[];
}

/** An image handed to the per-shot prompt writer, tagged with role + position. */
export interface LtxPromptImage {
  image_b64: string;
  /** "opening" | "scene" | "keyframe" | "end" (free-form). */
  role: string;
  /** Time within the shot (seconds) for positional keyframes. */
  seconds?: number;
}

/** One established character or mood-board entry the director LLM treats as canon. Steers the
 * writing (identity/look/palette); the image is LLM-only and never rendered as a scene frame. */
export interface StoryReference {
  label: string;
  description: string;
  /** Bare or data-URI base64 (optional). */
  image_b64?: string | null;
}

/** Project "story bible" grounding every director LLM call so shots don't hallucinate the
 * scenario or a character's details. Persisted per-project in the timeline's editor blob. */
export interface StoryContext {
  backstory: string;
  tone: string;
  rules: string;
  references: StoryReference[];
}

export interface LtxPromptFromLlmRequest {
  /** Seed idea / current field text to build on (optional). */
  idea?: string;
  /** "t2v" | "i2v" | "v2v_edit" | "v2v_restyle"; omit to infer from images. */
  mode?: string;
  seconds?: number;
  fps?: number;
  audio_supplied?: boolean;
  /** Whether anyone audibly speaks or sings DURING this shot, read off the transcript's word
   * onsets. false = the shot falls in a gap between lines or over instrumental/ambient audio, so
   * the prompt must not put a moving mouth on anyone. Omit when there is no transcript to judge
   * from — the writer is then told nothing rather than guessing. */
  speech_in_shot?: boolean;
  /** Whether THIS shot renders from the previous shot's final frame (a plain continuation): true =
   * treat as image-to-video even with no attached image — the prior frame is the opening frame, so
   * describe only what changes. Omit for shot 0, hard cuts, and i2v/v2v shots with their own source. */
  continuation?: boolean;
  /** Whether this shot explicitly CLOSES its scene at the end — true = nothing continues out of it
   * (next is a hard cut, or it's a deliberate final beat), so it may close on any framing; false =
   * the next shot continues from its final frame (or none exists yet while building), so it must end
   * on one that still shows the whole scene. Omit when unknown (no end-framing guidance either way). */
  closes_scene?: boolean;
  images?: LtxPromptImage[];
  /** Visual references to match (framing/composition/mood) — NOT frames of this shot. */
  references?: LtxPromptImage[];
  prev_prompt?: string;
  next_prompt?: string;
  /** Compact summaries of the shots before this one (earliest → latest) so a draft/reroll knows
   * the story it's continuing. The immediate neighbour's full text still rides in `prev_prompt`. */
  prior_shots?: string[];
  /** Reroll: an existing full prompt to rewrite (same scene/intent, fresh wording). Replaces `idea`. */
  rewrite?: string;
  /** Rework: a directed change to apply to the shot ("make her angry", a continuity note). */
  instruction?: string;
  /**
   * This shot's Prompt Relay beats — the timed moments conditioned SEPARATELY, which the prompt
   * being written must therefore leave out. Sent so the writer holds the shot's SETTING instead of
   * narrating the beats back into the anchor that conditions every frame.
   */
  beats?: LtxBeat[];
  /**
   * Which engine's PROMPT CONVENTIONS to write in — `'ltx'` (default) or `'h3'`. They are not
   * variations on each other: an H3 prompt is block-structured, carries its negatives inline (there
   * is no negative-prompt field), directs its own soundtrack (H3 generates one), and addresses
   * references by ordinal. See `api/src/routes/h3_prompting.rs`, which is the single place those
   * conventions live.
   */
  engine?: DirectorEngine;
  /**
   * H3 only: the references IN SCOPE FOR THIS SHOT, in project order.
   *
   * Scoped, not the whole roster, and that distinction is load-bearing: the engine assigns
   * `<Picture i>` / `<Video k>` / `<Audio j>` ordinals over the shot's own in-scope list, per kind,
   * starting at 1. A prompt written against project-wide positions addresses the wrong reference on
   * every scoped shot and still renders something plausible.
   */
  h3_references?: H3PromptReference[];
  /** H3 only: this shot pins a CLOSING frame as well as an opener, so it is an `fl2va` transition. */
  h3_end_image?: boolean;
  /** H3 only: the planner's audio intent for this shot, to be expanded rather than replaced. */
  h3_audio?: string;
  /** H3 only: the shot's aspect as a phrase for the prompt's spec line ("16:9 landscape"). */
  aspect?: string;
  /** H3 only: does anything appear on screen as text? Gates the typography direction — H3 renders
   *  legible text well, but a shot with none must not be invited to add any. */
  on_screen_text?: boolean;
  /**
   * H3 only: where the song's voice falls in THIS shot's GENERATION, in that generation's own local
   * seconds. Exactly one entry here — the route reads `[0]` and ignores the rest.
   *
   * 🔴 Supersedes {@link speech_in_shot}, which is the same measurement flattened to one bit. The
   * boolean's `true` arm emits *"a `<d>` block and a moving mouth are correct"* as a STANDING fact,
   * which is how a generation whose slice is instrumental came to be written with a mouth
   * flapping for its whole 14 s. Absent ⇒ the server keeps the boolean's behaviour exactly.
   */
  h3_vocal_lanes?: H3VocalLane[];
  /**
   * H3 only, OPT-IN: the project song's BARE asset hash (64 hex, no `asset:` prefix), so the server
   * can cut this generation's slice and let the writer HEAR it.
   *
   * 🔴 The prefix is dropped on purpose. koblem's `AssetJson` edge rewrites every `asset:<hash>`
   * string in a request body into the base64 of its bytes, which for a song means an 8 MB file
   * inlined into a request that wants 14 seconds out of the middle of it. Without the prefix the
   * field is invisible to that walk and the server reads the store directly.
   *
   * ⚠️ Always safe to send: the attachment itself is gated server-side on
   * `KOBLEM_H3_WRITER_SONG_AUDIO`, which is OFF by default because the context cost of ~14 s of
   * audio through the mmproj has never been measured.
   */
  h3_song_asset?: string;
  /**
   * H3 only: where this generation's slice starts IN THE SONG, seconds.
   *
   * 🔴 `GenerationSlice.songOffsetSeconds` from {@link planSoundtrack}, never `deliveredAtSeconds`
   * — the two are one trim frame apart and drift by a frame per seam along the cut.
   */
  h3_song_offset_seconds?: number;
  /** H3 only: how many seconds of song this generation is conditioned on. */
  h3_song_slice_seconds?: number;
  /** Also return a one-sentence plain-language `gist` of the shot alongside the prompt. */
  want_gist?: boolean;
  /** Let the writer PROPOSE Prompt Relay beats when the idea holds a moment rather than one
   * continuous action. Ignored server-side unless `beats` is empty, so a redraft never overwrites
   * timing placed by hand. */
  want_beats?: boolean;
  /** Produce ONLY the gist for the supplied prompt (`rewrite`/`idea`); don't (re)write the prompt. */
  gist_only?: boolean;
  /** Project canon so a lone draft describes the established subject instead of inventing it. */
  story?: StoryContext;
  /**
   * The recurring cast whose appearance sentences must come back verbatim. Absent/empty ⇒
   * byte-identical to before. See `LtxCastMember`.
   */
  cast?: LtxCastMember[];
}

export interface LtxPromptFromLlmResponse {
  /**
   * The written prompt.
   *
   * On H3 this is the LEGACY carrier and is marked for deletion: the writer now returns the
   * structured object in {@link LtxPromptFromLlmResponse.h3_draft}, and a koblem that predates
   * that field puts the same object here as JSON text. `lib/director/h3-llm.ts` tries the typed
   * field, then this one, then treats it as prose — so nothing is lost at any of the three stages.
   */
  prompt: string;
  /** One-sentence plain-language scene summary (no camera jargon). Present when requested. */
  gist?: string;
  /** Beats the writer proposed, in shot-relative seconds. Only populated when `want_beats` was set
   * AND the shot had none — treat them as suggestions to keep or delete, not as authoritative. */
  beats?: LtxBeat[];
  /**
   * H3 only: the structured draft for the GENERATION this shot belongs to.
   *
   * 🔴 Its `shots[]` covers every shot in that generation, not only the one that was asked for —
   * the six sections cannot be written for one shot in isolation, because they describe the whole
   * render unit. The editor applies the shot bodies positionally across the generation.
   */
  h3_draft?: H3PromptDraft;
}

/// Draft ONE detailed LTX shot prompt for a single prompt field. Image- and mode-aware — attach the
/// shot's scene image / keyframes (with roles + seconds) so the model writes the transition, and set
/// `mode` so i2v describes only "what happens next" while t2v defines the scene from scratch.
export function ltxPromptFromLlm(body: LtxPromptFromLlmRequest): Promise<LtxPromptFromLlmResponse> {
  return request('/api/v1/ltx-video/prompt-from-llm', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

export type LtxJobState = 'queued' | 'rendering' | 'completed' | 'failed' | 'cancelled';

/** Status of an async LTX render (poll `…/jobs/{id}`). */
export interface LtxJobStatus {
  job_id: string;
  status: LtxJobState;
  /**
   * RFC3339 submit time — what a client attaching to a render started on another device counts
   * elapsed from. Absent from a koblem predating the field.
   */
  created_at?: string | null;
  /** The ltx-video-server's own job id; pass as `resume_job_id` to continue a failed render. */
  server_job_id?: string | null;
  resume_job_id?: string | null;
  resumable: boolean;
  segments: number;
  scripts: string[];
  gate_wait_ms: number;
  render_ms: number;
  error?: string | null;
  /** Set once `completed`: koblem media URL to fetch the clip. */
  result_url?: string | null;
  /**
   * Progressive per-segment delivery: each finished shot the engine emitted so far, served at its
   * `url` (`…/jobs/{id}/segments/{n}`) before the stitched `result_url` lands. Grows while
   * rendering; empty for a render that produced none. Best-effort — a missing shot just isn't shown.
   */
  partials?: { segment_index: number; url: string }[];
  /**
   * 🔴 **`result_url` IS NOT THE TIMELINE.** Absent means `false`.
   *
   * Two unrelated situations set it, and the reader's action is the same for both — never adopt
   * these bytes as the project's stitched cut:
   *
   *  - a resume or a retake: the media is the rendered WINDOW. Predictable at dispatch from
   *    `retakeSplicesTail`, a poll before any job body exists.
   *  - a full render whose concatenation FAILED: the media is segment 0 alone. A runtime failure,
   *    so no capability predicts it — this flag is the only structured warning a client gets, and
   *    the prose account of it rides `last_render_warnings`.
   *
   * See `mediaIsFragment` in `#lib/director/renders`, which is where the two are combined.
   */
  job_is_partial?: boolean;

  // ── MiniMax-H3 render telemetry ────────────────────────────────────────────
  //
  // 🔴 **Every field here is optional and absence means UNKNOWN, never zero.** The LTX stack has no
  // numeric progress at all — state transitions plus `created_at` are its whole progress signal —
  // so on an LTX job these keys are simply not on the wire, and they never will be. A reader that
  // defaults them to 0 draws "step 0 of 0", which a progress bar renders as FINISHED, on the one
  // engine whose renders take ~830 s per generation.
  //
  // They ride the shared LTX job route on purpose: the Director polls ONE endpoint for both
  // engines (`routes/minimax_h3.rs` — H3 jobs live in the LTX job store), so a second store would
  // mean a second poll, a second TTL sweep and a second restart-reattach path.
  /**
   * The engine's own ETA for the WHOLE job, taken at submit time and INCLUDING any checkpoint
   * reload a mixed `fl2va`/`ref2va` chain forces. The one number worth showing before an hour-long
   * render — `activeExpectedSec`, the editor's own figure, is a division of the last render's wall
   * time and knows nothing about a reload.
   */
  total_eta_seconds?: number | null;
  /** 0.0–1.0 across the WHOLE job, not the current generation. */
  progress?: number | null;
  current_step?: number | null;
  total_steps?: number | null;
  /**
   * 🔴 `loading` is a CHECKPOINT RELOAD, not a stall.
   *
   * A chain that switches checkpoint family pays a full ~20 GB DiT + text-encoder reload between
   * generations (`wgp.py`'s `release_model()` is never incremental), which is minutes of silence
   * with the step counter frozen. Drawn as "stalled" — or not drawn at all — it looks exactly like
   * a hang, and the render gets cancelled. See {@link H3Phase} and `h3PhaseLabel`.
   */
  phase?: H3Phase | string | null;
  /**
   * Bumps when a NEW preview frame exists at `/api/v1/minimax-h3/jobs/{id}/preview`.
   *
   * A sequence rather than a URL because the URL never changes: the number is the only signal that
   * re-fetching would return different bytes. See {@link fetchH3Preview}.
   */
  preview_seq?: number | null;
  /** Which generation is rendering. `segments` holds how many there are. */
  generation_index?: number | null;
  generations_done?: number | null;
}

/**
 * What the engine is doing right now. Absent on LTX, and possibly a value this build has never
 * heard of — the shim owns the vocabulary, so consumers must treat an unknown string as "busy"
 * rather than as an error.
 */
export type H3Phase = 'denoising' | 'decoding' | 'loading' | 'encoding';

/**
 * Fetch the latest in-flight preview PNG for an H3 job, as an object URL.
 *
 * 🔴 **`null` is the normal answer, not a failure.** koblem answers 404 whenever the engine has no
 * preview to give — which is every render that did not ask for one (`preview: true` is opt-in) and
 * the whole window before the first emission. Showing an error there would put a red banner on
 * every render that simply left previews off.
 *
 * The caller owns the returned object URL and must revoke it. The endpoint is keyed by the KOBLEM
 * job id — the same one already being polled — so no second identifier is needed, and the URL is
 * constant: poll `preview_seq` on the job body and re-fetch only when the number changes.
 */
export async function fetchH3Preview(jobId: string): Promise<string | null> {
  try {
    const response = await authedFetch(
      `/api/v1/minimax-h3/jobs/${encodeURIComponent(jobId)}/preview`
    );
    return URL.createObjectURL(await response.blob());
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null;
    throw e;
  }
}

/**
 * UI bootstrap for an LTX engine: health, defaults and the LoRA picker's contents.
 *
 * `engine` selects which service answers. It is sent whenever it is given; koblem's own default for
 * an absent one is LTX-2.5, the only LTX that still exists.
 */
export function getLtxVideoProps(engine?: DirectorEngine): Promise<LtxVideoProps> {
  const q = engine ? `?engine=${encodeURIComponent(engine)}` : '';
  return request(`/api/v1/ltx-video/props${q}`);
}

/** One banked segment: what it holds, what it cost, and enough provenance to recognise it. */
export interface LtxBankEntry {
  /** `"<engine job id>:<segment index>"`. What a resume or a retake reaches through its job half. */
  key: string;
  width: number;
  height: number;
  frames: number;
  latent_t: number;
  tail_frames: number;
  /** Always false on LTX: its chain has no fixed frame-0 restoring force to propagate. */
  has_anchor: boolean;
  size_bytes: number;
  created: number;
  /** Whatever else the plugin chose to publish — `job`, `segment`, `template`, `seed`, `fps`, … */
  [extra: string]: unknown;
}

export interface LtxBankListing {
  model: string;
  enabled: boolean;
  dir: string | null;
  count: number;
  bytes: number;
  cap_bytes: number;
  tail_frames_written?: number | null;
  /** Newest first. */
  entries: LtxBankEntry[];
}

/**
 * The engine's latent bank — which segments are still continuable, and what the store costs.
 *
 * 🔴 A live question about the engine's disk, not a projection of what was rendered. An entry is
 * written when a segment COMPLETES and dropped by a byte-bounded newest-first-keep sweep, so a
 * render old enough to have been swept can no longer be resumed however recently it was watched.
 * 404 on the C++ ltx-video engine, which keeps its latents in a job dir rather than a swept store.
 */
export function getLtxBank(engine?: DirectorEngine): Promise<LtxBankListing> {
  const q = engine ? `?engine=${encodeURIComponent(engine)}` : '';
  return request(`/api/v1/ltx-video/bank${q}`);
}

/// Submit a chain render. Returns immediately (HTTP 202) with a koblem `job_id`; the render
/// runs in a background task that owns the GPU guard, so a disconnect/navigation can't drop
/// it. Poll `getLtxJob(job_id)` for status and fetch the clip from `result_url` when done.
/// Multipart with optional `image` (seg-0 i2v), `audio` (voice/mixed), `bgm`, and per-shot
/// scene images `seg_image_<i>` (raw segment index) for mid-chain scene changes.
/**
 * Optional Director-persistence routing for a render: tag the job so its output is harvested into
 * `projectId`'s owned store on completion. `tier` picks the stream ('proxy'|'final'); `wipe`
 * replaces that tier's prior set (a full run) vs updating in place (a through-K / retake render).
 * Carried as query params so the shared render body is untouched.
 */
export interface LtxDirectorTag {
  projectId: string;
  tier: 'proxy' | 'final';
  wipe?: boolean;
  /**
   * Run the vision critic once this render finishes, honouring the project's
   * `timeline.critique` settings. Set ONLY on a full run: it is an explicit flag rather than
   * something the server infers, so a retake — including one the critic itself dispatched — can
   * never chain back into another review.
   */
  critique?: boolean;
}

function ltxGenerateUrl(director?: LtxDirectorTag | null, engine?: DirectorEngine): string {
  const base = '/api/v1/ltx-video/generate';
  const q = new URLSearchParams();
  if (director) {
    q.set('director_project_id', director.projectId);
    q.set('director_tier', director.tier);
    q.set('director_wipe', String(!!director.wipe));
    if (director.critique) q.set('director_critique', 'true');
  }
  // 🔴 The engine rides in the QUERY, not the body: the render body is the koblibs
  // `LtxVideoGenerateRequest` and koblem parses it strictly (an unknown key is a 400), so which
  // service serves the request — koblem's own routing concern — is carried the same way the
  // Director tag is.
  if (engine) q.set('engine', engine);
  const query = q.toString();
  return query ? `${base}?${query}` : base;
}

/**
 * Submit a compiled Director render.
 *
 * Both engines ride this one transport — koblem routes on the project's engine, not on the URL —
 * which is why the body is the {@link DirectorRequest} union rather than the LTX shape. The two
 * BODIES are not variants of each other and the union keeps that visible; only the multipart
 * sidecars below are LTX-only (H3 carries its openers inline, has no v2v path and takes no audio).
 */
export async function submitLtxVideo(
  req: DirectorRequest,
  image?: File | null,
  audio?: File | null,
  bgm?: File | null,
  segImages?: (File | null | undefined)[],
  segV2v?: (File | null | undefined)[],
  director?: LtxDirectorTag | null,
  /**
   * Which LTX service renders this. Absent (and `'ltx'`) is the C++ ltx-video engine — what every
   * caller sent before LTX-2.5 existed. `'h3'` never reaches here: an H3 project dispatches from
   * its saved document through `/run`.
   */
  engine?: DirectorEngine,
  queue?: QueueSubmitOptions
): Promise<{ job_id: string; status: LtxJobState }> {
  const url = ltxGenerateUrl(director, engine);
  const hasSegImages = !!segImages?.some((f) => !!f);
  const hasSegV2v = !!segV2v?.some((f) => !!f);
  if (!image && !audio && !bgm && !hasSegImages && !hasSegV2v) {
    return request(url, {
      method: 'POST',
      headers: queueHeaders(queue),
      body: JSON.stringify(req)
    });
  }
  const form = new FormData();
  form.append('request', JSON.stringify(req));
  if (image) form.append('image', image);
  if (audio) form.append('audio', audio);
  if (bgm) form.append('bgm', bgm);
  // Per-shot scene images, keyed by RAW segment index so the server matches them to shots.
  segImages?.forEach((f, i) => {
    if (f) form.append(`seg_image_${i}`, f);
  });
  // Per-shot foreign v2v source clips, keyed by RAW segment index.
  segV2v?.forEach((f, i) => {
    if (f) form.append(`seg_v2v_${i}`, f);
  });
  const response = await authedFetch(url, {
    method: 'POST',
    headers: queueHeaders(queue),
    body: form
  });
  if (!response.ok) {
    const text = await response.text().catch(() => '');
    throw new Error(`ltx-video submit failed: ${response.status} ${text}`);
  }
  return response.json();
}

/// Poll one LTX job's status.
export function getLtxJob(jobId: string): Promise<LtxJobStatus> {
  return request(`/api/v1/ltx-video/jobs/${encodeURIComponent(jobId)}`);
}

/// List in-flight + recently-finished LTX jobs, newest first — how a client finds the render
/// another device started. Pass `standalone` to drop the Director renders that share this store;
/// nothing else can tell them apart, and they belong to a project this caller has no part in.
export function listLtxJobs(standalone = false): Promise<{ jobs: LtxJobStatus[] }> {
  return request(`/api/v1/ltx-video/jobs${standalone ? '?standalone=true' : ''}`);
}

/**
 * `POST /api/v1/ltx-video/script-from-llm`.
 *
 * 🔴 **An OBJECT, because the positional signature could not reach half the endpoint.** This was
 * `(brief, nSegments, secondsPerSegment?, image?, audioSupplied?)` with a body literal of exactly
 * those five keys, so it was structurally incapable of sending `engine`, `h3_references` or `cast`
 * — and the server has read all three the whole time. koblem's handler wraps the shared request in
 * `H3Envelope` (`ltx_video.rs:997-1041`), and on `engine: "h3"` it swaps in
 * `h3_prompting::script_system_prompt` + `block_contract_many` and parses the `===GENERATION===`
 * batch reply into `h3_generations`. None of that had a caller: not one UI could set the flag that
 * selects it. Mirrors {@link ltxPromptFromLlm}, which has taken a body object all along.
 *
 * ⚠️ `brief` and `n_segments` are the only required fields; `image` is BARE base64 (no `data:`
 * prefix) and makes it image-to-video. `audio_supplied` true means the caller brings its own track,
 * so the prompts must not describe sound.
 */
export interface LtxScriptFromLlmRequest {
  brief: string;
  /** How many units to write. Server clamps to 1..24. */
  n_segments: number;
  /**
   * Target seconds per unit. On H3 it is re-snapped onto the 17n+5 grid server-side before the
   * writer sees it — a writer given the authored number places cuts past the end of the generation.
   */
  seconds_per_segment?: number;
  /** Bare base64 opening frame. Present ⇒ image-to-video. */
  image?: string;
  audio_supplied?: boolean;
  /** Project canon, so a lone draft describes the established world instead of inventing one. */
  story?: StoryContext;
  /** The recurring cast whose appearance sentences must come back verbatim. */
  cast?: LtxCastMember[];
  /**
   * `"ltx"` (default) or `"h3"`. Absent = LTX, which is what every existing caller sends.
   *
   * 🔴 This is the switch for the whole H3 batch path — the block contract, the per-generation
   * schema and the structured `h3_generations` reply. Sending `"h3"` changes what comes back, not
   * merely how it is worded.
   */
  engine?: DirectorEngine;
  /**
   * H3 only: the references in scope, in the order the runner will add them.
   *
   * ⚠️ ORDER IS THE REQUEST. Position assigns `<Picture i>` / `<Video k>` / `<Audio j>`, so a
   * reordered list makes the writer address the wrong asset and still produce something plausible.
   * Scope is the GENERATION — one call builds one reference presentation for all of its shots.
   */
  h3_references?: H3PromptReference[];
  /** H3 only: the aspect as a phrase for the prompt's spec line ("16:9 landscape"). */
  aspect?: string;
  /**
   * H3 only: where the song's voice falls, ONE LANE PER GENERATION in render order, each on that
   * generation's own local clock.
   *
   * 🔴 This writer decides the standing `<Audio N>` sentence for every generation in one turn, and
   * until 2026-08-14 it was the one writer never told whether anybody sings — `script_system_prompt`
   * did not call the audio clause at all. A single boolean could never have served it; a lane per
   * generation can.
   */
  h3_vocal_lanes?: H3VocalLane[];
}

/// Ask the main VL model to expand a brief into N per-unit director prompts. See the request type
/// for why this takes an object and what the H3 arm unlocks.
export function ltxVideoScriptFromLlm(
  body: LtxScriptFromLlmRequest
): Promise<LtxScriptFromLlmResponse> {
  return request('/api/v1/ltx-video/script-from-llm', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Fetch a rendered clip as an object URL for `<video>` playback.
export async function fetchLtxClip(url: string): Promise<string> {
  const response = await authedFetch(url);
  if (!response.ok) {
    throw new Error(`fetch clip failed: ${response.status}`);
  }
  const blob = await response.blob();
  return URL.createObjectURL(blob);
}

/// Cancel the in-flight LTX render (SIGKILL the worker child via /v1/admin/unload).
export function cancelLtxRender(): Promise<unknown> {
  return request('/api/v1/ltx-video/cancel', { method: 'POST' });
}

// ── Director 2 (timeline editor: saved, re-runnable multi-shot video projects) ─
// The editor persists a timeline doc, then compiles it to an LtxVideoGenerateRequest
// + inline base64 assets and runs it through the SAME chain render as ltx-video. A
// director "run" creates a job in the shared ltx job store, so poll it with
// getLtxJob() and fetch the clip with fetchLtxClip() — no separate job API.

/**
 * How a shot is generated on the timeline. `v2v_restyle` re-renders a source clip under a new
 * prompt (SDEdit); `v2v_edit` holds the source scene and adds the prompt's change (guide-edit).
 * (Legacy projects may carry the old `"v2v"` value — compile() maps it to `v2v_restyle`.)
 */
export type DirectorClipMode = 't2v' | 'i2v' | 'v2v_restyle' | 'v2v_edit';

/** Where a v2v clip's source comes from: an uploaded file, or a segment of a prior render. */
export type DirectorV2vSourceKind = 'upload' | 'prior_render';

/** One frame-pinned image inside a clip (identity injection / reveal / first-last). */
export interface DirectorPin {
  /** Bare (or data-URI) base64 of the pinned image. */
  image_b64: string;
  /** Time within the clip (seconds) where it's pinned. */
  seconds: number;
  /** Optional label (e.g. the lyric it's aligned to). */
  label?: string;
}

/**
 * One Prompt Relay beat inside a clip: a moment where something specific happens. The clip's
 * `prompt` stays the SETTING and conditions the whole shot; each beat is a short clause that gets
 * cross-attention priority around its own time. Zero beats renders exactly as before.
 *
 * Copy rule: never repeat the setting in a beat — the setting is what anchors every frame.
 */
export interface DirectorBeat {
  /** Time within the clip (seconds) — VISIBLE clip time, i.e. what the viewer sees. */
  seconds: number;
  /** The beat clause. One action, not a whole prompt. */
  text: string;
  /** Optional label (e.g. the lyric word it snapped to). Editor-only, never sent to the engine. */
  label?: string;
}

/**
 * One character in the project's cast, and the identity tag it owns.
 *
 * This is the whole point of the character-sheet feature working across a scene rather than within
 * one shot. The model keys identity off `source_id`, so the SAME character must present the SAME
 * number in every shot; a per-shot "first ref is 2, second is 3" scheme silently swaps identities
 * the moment a shot's cast differs. So the tag is allocated ONCE here, at project level, ascending
 * from 2, and persisted with the project.
 *
 * `source_id` is never reassigned and never compacted: deleting the character holding 2 leaves the
 * next one at 3. A stale gap costs nothing; a reused tag re-labels a face.
 */
export interface DirectorCharacter {
  /** Stable client id (uuid). What a clip's `character_ids` reference. */
  id: string;
  /** Display name — shown on the shot chips, logged with the render, never sent to the engine. */
  name: string;
  /**
   * The character's reference image, as an `asset:<hash>` ref (or legacy inline base64).
   *
   * WHAT it must be depends on the render's identity regime (see `ltxIdentityRegime`), and the two
   * answers are not interchangeable:
   *  - `sheet` (the `faceid-sheet-rt` adapter) — a single 1536×1024 image of ONE person, four
   *    panels: face close-up, then full body front / side / back.
   *  - `memory` (either Echo dose, or the MSR adapter) — one ordinary FULL FRAME of the character,
   *    uncropped and framed like the shots it will condition.
   *
   * (Field name kept from when only sheets existed; every saved project stores it under this key.)
   */
  sheet_b64?: string | null;
  /**
   * The TASS source-phase tag. Allocated once, ascending from 2, and never changed for the life of
   * the project. See the type docs — this is the load-bearing field.
   *
   * Only carries meaning in the `sheet` regime; Echo and MSR run UNTAGGED (`tass_phase_scale: 0`).
   * It is still allocated and still stable either way, so changing the model or swapping the
   * identity adapter doesn't renumber the cast.
   */
  source_id: number;
  /**
   * The character's APPEARANCE SENTENCE — skin tone, hair, eyes, facial hair, glasses, face shape,
   * clothing. Stable looks only: never expression or mood.
   *
   * It is folded into every rendered prompt for a shot this character appears in, and the LLM
   * drafting paths are told to reproduce it character-for-character. That is not cosmetic: in the
   * Echo regime there is no per-character token at all, so this sentence is the ONLY thing that
   * routes a shot to the right anchor, and a paraphrase reads as a different person.
   */
  identity?: string;
  /**
   * The clip that INTRODUCED this character — the shot whose footage `sheet_b64` was promoted
   * from. Set by "use this frame as the reference"; absent for an uploaded reference.
   *
   * Load-bearing, not just provenance: that shot is skipped when compiling references, because
   * conditioning a shot on a still taken from its own render is circular.
   */
  introduced_by?: string | null;
  /**
   * WHAT this reference is — a person, a place, or a thing.
   *
   * The engine cannot know: an image is just pixels. Only the editor can say, and it matters
   * because the two memory-regime mechanisms differ on exactly one axis. Echo and MSR are
   * equivalent on characters and locations (measured, n=4 on identity parity), but only MSR
   * composes a PROP as a separate thing — Echo fuses multiple heterogeneous subjects into one
   * entity (a rider welded into a bike, a drone absorbed into its frame).
   *
   * So the role is what makes a shot ADVISE the MSR adapter — advice only, never applied. See
   * `resolveClipVariants`.
   *
   * Absent means `"character"`: every reference that existed before this field was one.
   *
   * NOTE this lives on the CHARACTER, not on the asset. `director_assets` is content-addressed by
   * hash, so the same bytes used as a character in one project and a prop in another would collide
   * on one row. Role is a property of the USAGE.
   */
  role?: DirectorReferenceRole;
}

/**
 * What a reference depicts. Drives per-shot DiT selection, nothing else.
 *
 * `"location"` is listed separately from `"character"` even though the two behave identically
 * today: it is what the author meant, it reads correctly in the UI, and if the checkpoints ever
 * diverge on places the rule has somewhere to attach.
 */
export type DirectorReferenceRole = 'character' | 'location' | 'prop';

/**
 * WHICH identity mechanism a render implements. Mirrors `LtxIdentityRegime` in koblem-client —
 * the backend decides for real (from `request.model` plus `request.loras`); this is here so the
 * editor can say what a reference image needs to BE before one is uploaded.
 *
 * The failure mode when they disagree is silent: the render succeeds, identity just doesn't hold.
 */
export type LtxIdentityRegime = 'sheet' | 'memory';

/**
 * Identity mechanisms that ship as runtime LoRAs. Mirrors the `LTX_LORA_*` constants in
 * koblem-client; values are file names as `LTX_VIDEO_LORAS` offers them, extension included.
 *
 * 🔴 [2026-07-30] These were DiT variants (`faceid-sheet`, `msr-v2`) until their folded checkpoints
 * were deleted. The reason inverted: a fold existed because the same LoRA applied at runtime cost
 * +43.8s and +686 MiB per render, and fold-at-load reclaims all of it (+0.0s, +0 MiB measured). So a
 * 16.3 GB checkpoint per effect buys nothing, while an adapter is tunable, stackable and per-shot.
 *
 * They are named here — unlike every other adapter, which is an ordinary effect — because identity
 * conditioning is a CONTRACT: which of these is attached decides what a reference must be, how it is
 * sized, whether it is rotary-tagged and whether the prompt carries a prefix.
 */
export const LTX_LORA_MSR_V2 = 'LTX-2.3-Licon-MSR-V2.gguf';
export const LTX_LORA_FACEID_SHEET = 'faceid-sheet-rt.gguf';
export const LTX_LORA_FACEID_CLOSEUP = 'faceid-closeup-rt.gguf';
export const LTX_LORA_LIPDUB = 'ltx-2.3-22b-ic-lora-lipdub-0.9.gguf';

/**
 * The regime a render implements, from its model AND its attached adapters.
 *
 * The model name alone is no longer enough and cannot be made enough: `base` is `sheet`, but `base`
 * with {@link LTX_LORA_MSR_V2} attached is a `memory` render, and nothing in the string "base" says
 * so. An adapter therefore WINS over the bare name — which is sound because the only checkpoints
 * that carry a regime in their name are the Echo doses, already `memory`, so the reachable
 * disagreement is a sheet-ish model with an MSR adapter, and there the adapter is what supplies the
 * trained response.
 *
 * MUST stay in step with `LtxIdentityRegime::for_render` in koblibs, or the editor advises one
 * contract while the backend sends another.
 */
export function ltxIdentityRegime(
  model: string | undefined,
  loras: LtxRuntimeLora[] | undefined = undefined
): LtxIdentityRegime {
  let sawSheet = false;
  for (const l of loras ?? []) {
    const p = l.path.toLowerCase();
    // MSR shares every knob with Echo (match_target, untagged, no ref_t2v prefix).
    if (p.includes('msr')) return 'memory';
    if (p.includes('faceid')) sawSheet = true;
  }
  // An explicit sheet adapter pins `sheet` even on an Echo checkpoint. Contradictory, but
  // author-chosen, so it is honoured rather than silently reinterpreted.
  if (sawSheet) return 'sheet';
  const m = (model ?? '').toLowerCase();
  return m.includes('echo') || m.includes('msr') ? 'memory' : 'sheet';
}

/**
 * True when this render has something that actually conditions on identity — an identity-trained
 * checkpoint or an identity adapter. Anything else renders, but holds nothing.
 *
 * Takes the LoRA set for the same reason {@link ltxIdentityRegime} does: `base` is identity-trained
 * exactly when one of the identity adapters is attached.
 */
export function ltxModelIsIdentityTrained(
  model: string | undefined,
  loras: LtxRuntimeLora[] | undefined = undefined
): boolean {
  const m = (model ?? '').toLowerCase();
  if (m.includes('echo') || m.includes('faceid') || m.includes('msr')) {
    return true;
  }
  return (loras ?? []).some((l) => {
    const p = l.path.toLowerCase();
    return p.includes('faceid') || p.includes('msr');
  });
}

/** What a reference image must BE in each regime — editor copy, kept next to the rule it states. */
export const LTX_REFERENCE_SPEC: Record<LtxIdentityRegime, string> = {
  sheet:
    'One 1536×1024 image, four panels: face close-up, then full body front, side and back — same ' +
    'person, same clothes in all four.',
  memory:
    'The subject CUT OUT on a plain white background, evenly lit, UNCROPPED — nothing touching ' +
    'the frame edge. Three measured failures, all invisible in a successful render: a crop ' +
    'severs the subject and the missing part is reinvented differently every shot (cutting the ' +
    'feet out changed the feet); an unmatted photo can leak its own background and hijack the ' +
    "location; and lighting with no visible source in frame is read as the subject's own colour " +
    'and travels with it (a neon-lit cutout stayed pink on a moonlit beach).'
};

/** One member of the cast, as the LLM prompt writers need to see them. */
export interface LtxCastMember {
  /** What every prompt calls this character. */
  name: string;
  /** The appearance sentence, to be reproduced verbatim in every shot they appear in. */
  appearance: string;
  /** Their identity tag, as writer context only. */
  source_id?: number;
}

/** The lowest tag a reference may take. 0 is the target itself (phase 0 = an exact no-op) and 1 is
 * reserved by the upstream layout, so the cast starts at 2. */
export const DIRECTOR_FIRST_SOURCE_ID = 2;

/** A single shot/clip on the timeline — the editor's source-of-truth per-shot model. */
export interface DirectorClip {
  /** Stable client id (for keys + drag). */
  id: string;
  /**
   * Short one-liner seed the ✨ Draft builds the full `prompt` from. Persisted separately so a
   * re-roll regenerates `prompt` without clobbering the user's original idea. Empty on
   * auto-directed clips (the LLM writes `prompt` straight from the brief).
   */
  idea?: string;
  prompt: string;
  /**
   * One-sentence plain-language summary of the shot (no camera jargon), written by the LLM on
   * Draft/Reroll/Summarise. Lets the timeline + inspector be scanned without reading the full
   * prompt. Falls back to `idea` for display when unset.
   */
  gist?: string;
  /**
   * Storyboard preview still (bare base64 JPEG) generated by FLUX from this shot's scene at its
   * story moment. A quick visual of the shot before rendering video; can be promoted to the i2v
   * scene image (`image_b64`) or added as a reference.
   */
  snapshot_b64?: string | null;
  /**
   * Per-shot visual references ("a shot like this") handed to the draft LLM to steer
   * framing/composition/mood. Distinct from `keyframes` (injected frames) and Story references
   * (project canon) — these are never rendered.
   */
  references?: LtxPromptImage[];
  mode: DirectorClipMode;
  /** Clip duration in seconds. */
  seconds: number;
  /** Per-shot base DiT variant override (falls back to the project model). */
  model?: string;
  /**
   * Per-shot runtime LoRA override. Sits next to `model` because it switches at the same chain
   * boundary — but re-applying adapters is ~1s against ~36s for a folded-DiT swap, which is why
   * this, not `model`, is the cheap way to vary an effect's STRENGTH shot to shot.
   *
   * TWO states, not three, and deliberately so: `undefined` inherits the project's set, a
   * non-empty list REPLACES it for this shot. An empty list is NOT "deliberately none" — the
   * engine (and koblibs' `LtxSegment.loras`, a `Vec` skipped when empty) reads empty and absent
   * identically as "inherit", so `normaliseLoraOverride` in `lib/director/loras.ts` collapses an
   * emptied override back to `undefined` rather than persisting a state the wire cannot express.
   */
  loras?: LtxRuntimeLora[];
  /**
   * Per-shot reference frame-drop override (falls back to the project `frame_drop`). Sits next to
   * `model` because it IS a property of the model this shot renders on: `echo-e50` leaks its
   * references into frame 0 and wants the trim, `echo-full` and `base` do not.
   *
   * Three states, the same shape as `character_ids`: `undefined` inherits the project, `"off"` is
   * an explicit no, `"auto"`/a number is an explicit yes. `undefined` is never "off".
   */
  frame_drop?: LtxFrameDrop;
  /**
   * i2v scene image (mode "i2v") or the v2v source's reference frame (mode "v2v"),
   * bare base64. For i2v this starts a fresh scene; audio stays continuous.
   */
  image_b64?: string | null;
  /**
   * v2v source: where the clip to edit/restyle comes from. `upload` uses `v2v_source_b64`;
   * `prior_render` references a previous render's segment (`v2v_prior_job_id` + segment). Absent
   * on a v2v clip defaults to `upload`.
   */
  v2v_source_kind?: DirectorV2vSourceKind;
  /** v2v source clip, base64 (webm/mp4) — for `v2v_source_kind = "upload"`. */
  v2v_source_b64?: string | null;
  /** Prior koblem LTX render job id — for `v2v_source_kind = "prior_render"`. */
  v2v_prior_job_id?: string | null;
  /** FILTERED (rendered) segment index within that prior render (matches its banked seg_<i>.bin). */
  v2v_prior_segment?: number;
  /**
   * v2v strength 0..1 — the SDEdit fraction of the schedule that is re-run, so higher = further
   * from the source and 1 = a full re-render, on both v2v modes.
   *
   * 🔴 It does not behave like SDEdit strength elsewhere on LTX-2.5, whose distilled schedule is
   * FRONT-LOADED: the first four of its eight steps sit between noise 1.0 and 0.975, so 0.5 starts
   * at 0.975 and is very nearly a fresh generation. Only nine outcomes exist (0–8 entered steps),
   * and useful editing lives below about 0.3. See `v2vStrengthDefault` / `V2V_STRENGTH_STEP` in
   * `lib/director/continuity.ts`, which is where the defaults and the slider grid come from.
   */
  v2v_strength?: number;
  /**
   * How hard this shot's image pins (keyframes / i2v scene image) are held, 0..1. 1 = exact (the
   * pinned image IS that frame, crispest), lower = the frame blends into the generated motion.
   * Absent → the backend default (1.0). Compiles to the segment's `pin_strength`.
   */
  pin_strength?: number;
  /**
   * MiniMax-H3 only: this shot's CLOSING anchor (`fl2va`'s last frame). LTX has no equivalent —
   * it pins an opener and interpolates through mid-shot keyframes, but nothing anchors its end.
   *
   * On H3 this shares the shot's opener (`image_b64`) in one respect that matters: both belong to
   * the `fl2va` transformer, and a shot conditioned that way CANNOT also carry omni references
   * (`ref2va` is a different model, not a flag). Setting either one takes the shot out of the
   * reference path.
   */
  h3_end_image?: string | null;
  /**
   * MiniMax-H3 only: **how** this shot's attached first/last images reach the model.
   *
   * There are two paths and they are not variants of one setting — they land on different
   * checkpoints and buy different things (measured against the source frame):
   *
   * | value | wire | frame-0 NCC | keeps references + song | cost |
   * |---|---|---|---|---|
   * | `pin` | `first_frame` / `last_frame` on `fl2va` | **0.968** | no — `fl2va` takes neither | a 64 s checkpoint flip, and the generation is isolated on BOTH sides |
   * | `keyframe` | the image joins `references` as a `<Picture N>` on `ref2va` | 0.776 | **yes** | free |
   *
   * 🔴 Absent means `pin`, which is what every document written before 2026-08-08 meant. The
   * default for a NEW attachment is chosen by the editor — `keyframe` whenever the project carries
   * a song, because `fl2va` throws the song slice away — and it is written explicitly rather than
   * derived, so `pinsAFrame` has exactly one answer at every call site. Deriving it from "is there
   * a song" would flip what an existing project renders the moment a track was attached, silently.
   *
   * ⚠️ One value covers BOTH of this shot's images. A shot that wire-pinned its opener while
   * prose-describing its closer would need two `fl2va`/`ref2va` checkpoints for one render, which
   * is not a thing the engine can do.
   */
  h3_frame_mode?: 'pin' | 'keyframe';
  /**
   * MiniMax-H3 only: what this shot SOUNDS like, in a few words ("rain on the awning, no music").
   *
   * A per-shot authoring field with no LTX equivalent, because on LTX the soundtrack is supplied
   * once for the whole timeline while H3 generates one per shot, from noise, inside each call.
   * Written by the planner and expanded by the per-shot writer into the prompt's audio block. Never
   * sent to the engine — it reaches the render only as part of the prompt text.
   */
  h3_audio?: string;
  /**
   * MiniMax-H3 only: the cut verb that opens this shot's `[Shot N]` body.
   *
   * One of the five the guide names (`the camera cuts to`, `the shot cuts to`, `the shot
   * transitions to`, `the shot changes to`, `the shot switches to`) — cross-dissolve, fade and
   * wipe exist but are user-requested only, so they are not offered. Unset on the first shot of a
   * generation, which has nothing to cut FROM: `[Shot 1]` carries no timestamp and no cut verb.
   */
  h3_cut_verb?: string;
  /**
   * MiniMax-H3 only: a line of dialogue runs ACROSS the cut into this shot.
   *
   * Emits `<scenetrans>` at both connecting points plus a continuity phrase, which is the only
   * mechanism that carries audio across a cut — and it exists ONLY inside a single generation, so
   * it is silently meaningless when the packer put a generation boundary here. The composer says
   * so rather than emitting a marker that cannot work.
   *
   * ⚠️ `<scenetrans>` has zero in-situ examples in the whole official corpus (`SPEC-h3-prompt-
   * emitter.md` §K, row 4) — it is described in prose only, so its exact placement is inferred.
   */
  h3_scenetrans?: boolean;
  /** Frame-pinned images within this clip. */
  keyframes: DirectorPin[];
  /**
   * Hard cut: for a plain t2v shot (not the first), render it FRESH from its prompt alone instead
   * of continuing the previous shot's motion. Ignored for shot 0 and for i2v/v2v/keyframe shots,
   * which start fresh whatever this says — a v2v shot is compiled as `scene_cut` unconditionally,
   * because the engine refuses a shot that is both a v2v and a continuation. Compiles to the
   * segment's `scene_cut`.
   */
  new_scene?: boolean;
  /**
   * This shot's own audio (bare base64), replacing the project track for its span. `voice` also
   * drives the shot's lip-sync; `bgm` is delivered only. Absent → the shot uses the project track.
   */
  audio_b64?: string | null;
  audio_kind?: LtxShotAudioKind;
  /**
   * Prompt Relay beats — timed sub-prompts inside this one continuous shot. Absent/empty = one
   * prompt for the whole shot (today's behaviour, no cross-attention mask, no cost). Two or three
   * is the practical ceiling for a 4–5s shot.
   */
  beats?: DirectorBeat[];
  /** Per-shot seed override. Absent → the project seed (offset by shot index). */
  seed?: number;
  /** Per-shot base step count. Absent → the project steps. */
  steps?: number;
  /** Per-shot CFG. Absent → the project cfg. Only the `dev` variant responds to > 1. */
  cfg?: number;
  /** Per-shot negative prompt. Absent/empty → the project negative prompt. */
  negative_prompt?: string;
  /**
   * Which of the project's characters are IN this shot, by `DirectorCharacter.id` — a per-shot
   * ENABLE list with an all-on default. Three distinct states:
   *
   * | value | meaning |
   * |---|---|
   * | `undefined` | every character in the project (what a new shot gets) |
   * | `[]`        | explicitly none — the user switched them all off |
   * | `[a, c]`    | exactly those |
   *
   * `undefined` must never be used for "none": it reads as all, and the editor materialises the
   * list on first toggle so anything explicit stays explicit. Absent → all also matches what the
   * engine did before per-shot scoping existed (one chain-level array applied to every segment), so
   * the default describes what actually rendered.
   *
   * Each enabled character contributes its reference + its stable `source_id` to this shot's
   * `character_refs`, and its appearance sentence to the cast sent to the LLM for this shot. The
   * backend collapses the per-shot selections into one top-level array whose entries carry a
   * `segments` scope, so a reference's image bytes are sent once however many shots use it.
   *
   * One character per shot is the trained case. Two is architecturally supported and expected to be
   * the shakiest part of the feature — which is what the per-shot switch is for.
   */
  character_ids?: string[];
  /**
   * Whether this shot RE-SENDS the references of the characters it carries.
   *
   * 🔴 Cast membership and reference scope are two different questions, and conflating them is a
   * measured quality bug. A character can be in a shot without that shot needing to be conditioned
   * on their sheet: on a plain continuation the 17-frame carry ALREADY holds the identity, and
   * appending the reference on top of it re-imposes the reference's own state.
   *
   * Measured 2026-08-15, two shots, one variable (a robot whose head comes off in shot 1 and is
   * asked to stay off in shot 2):
   *
   * | | head stays off | motion step at the cut | ΔRGB | Δluma |
   * |---|---|---|---|---|
   * | reference re-sent on the continuation | **no — the head comes back** | 15.20 | 8.49 | −8.78 |
   * | reference on the opener only | yes | **6.83** | **3.82** | **−3.86** |
   *
   * And the opposite case, from the same session: a shot whose camera tilts UP off the subject to
   * the moon carries frames with no subject in them at all, and with no reference the character
   * dissolves. So neither blanket answer is right, which is why this is per shot.
   *
   * - `always` — **the default, and what an absent value means.** Re-send the references on every
   *   shot that has them. This is what every project did before this field existed, so adding the
   *   field changed no existing render.
   * - `auto` — send them only where the carry cannot supply the identity: a fresh shot, a scene
   *   cut, an explicit frame-0 pin or scene image, or a shot whose previous shot did not carry
   *   that character.
   * - `never` — never send them here, whatever the carry looks like.
   *
   * 🔴 `auto` IS OPT-IN, AND THAT IS AN OWNER DECISION, NOT AN OVERSIGHT. The measurement above is
   * real, but it was taken on a shot that ENDS ON A WELL-FRAMED SUBJECT. When a shot does not —
   * the camera tilts off, the subject is small or turned away, the frame fills with dust — the
   * carry has no identity to give and withholding the reference loses the character outright. That
   * was measured too, in the same session: a shot that tilted up to the moon dissolved its
   * character into a domed blob when the reference was withheld. Losing the character is a worse
   * failure than a reference re-imposing a pose, so the safe answer is the default.
   *
   * ⚠️ A character whose look CHANGED (the head coming off, a costume change) needs a NEW reference
   * of the changed look, as its own cast member. Neither `auto` nor `never` fixes that — they only
   * stop the OLD sheet arguing with the new look.
   */
  reference_mode?: DirectorReferenceMode;
}

/**
 * Per-shot reference scope. See {@link DirectorClip.reference_mode}, which documents the
 * measurement this exists for.
 */
export type DirectorReferenceMode = 'auto' | 'always' | 'never';

/** A word (or segment) with start/end seconds — the lyric lane, from STT. */
export interface LyricWord {
  start: number;
  end: number;
  text: string;
}

/** Compiled render inputs derived from the clips on save/run. */
export interface DirectorAssets {
  opener_image_b64?: string | null;
  audio_b64?: string | null;
  bgm_b64?: string | null;
  /** Aligned to request.segments (RAW segment index). */
  seg_images_b64?: (string | null)[];
  /** Per-shot uploaded v2v source clips, base64, aligned to request.segments (RAW index). */
  seg_v2v_b64?: (string | null)[];
}

/**
 * Unattended dailies review: what should happen to the footage once a full render finishes.
 *
 * Persisted on the timeline so the server can read it without the editor being open — that is what
 * makes "kick off at midnight, wake up to judged options" work at all. Absent = off, so an existing
 * project renders exactly as it always did.
 */
export interface DirectorCritiqueSettings {
  /** Judge the footage after a full render. */
  enabled: boolean;
  /** Also re-roll shots that fail, each as a NEW take (nothing is overwritten). Costs GPU time. */
  auto_retake: boolean;
  /** Most extra takes to spend on any one shot. */
  max_retakes?: number;
  /** Score below which a take fails (0..1). */
  threshold?: number;
}

/** The full persisted timeline doc (editor state + compiled render spec). */
/**
 * Which engine a Director project renders on.
 *
 * `ltx25` is LTX-2.5 under ComfyUI: chained shots with banked latents, runtime LoRAs, one DiT, and
 * a soundtrack it writes itself. `h3` is MiniMax-H3: video and its synchronised stereo soundtrack
 * out of ONE denoise, guidance-distilled, and no chained continuation at all.
 * `capabilities()` in `#lib/director/engine` is where the differences are stated, and
 * `resumeAvailable()` there is what a control reads.
 *
 * ⚠️ A third value, `'ltx'`, was LTX-2.3 — a separate C++ deployment with its own job store and its
 * own adapter directory. It has been retired, and it is deliberately NOT a member of this union any
 * more: a value that cannot be constructed cannot be sent to a service that no longer exists.
 * `readEngine` maps a stored `"ltx"` onto `ltx25`, which is right for a timeline's engine field —
 * it only decides which controls to draw.
 *
 * 🔴 It is NOT right for a bank or job id, and that is why `DirectorShotRender.engine` is typed
 * `string` rather than this union. A row stamped `'ltx'` names a job in the retired engine's store;
 * handing one to 2.5 is a 404 after the full queue wait (migration `0039` exists for exactly this).
 * Those rows are matched as raw strings against the project's engine, so a project reading as
 * `ltx25` never adopts one.
 */
export type DirectorEngine = 'h3' | 'ltx25';

/** What an H3 omni reference IS. See {@link DirectorH3Reference}. */
export type H3ReferenceKind = 'image' | 'video' | 'audio';

/**
 * What a reference is FOR — an authoring decision, and the one control that decides the prompt's
 * task-type prefix.
 *
 * 🔴 Not derivable from the kind. "Is this video the thing we are continuing, or is it structural
 * guidance for the cutting rhythm?" changes `[video continuation]` into `[reference generation]`,
 * and the guide is explicit that the mere PRESENCE of a video creates neither
 * (`GUIDE-ref:143`). koblem's `h3_prompting.rs` derives the task types from this field; without it
 * the fallback takes the weakest claim available for each kind, which makes `video editing`,
 * `video continuation` and `audio reuse` unreachable from the UI — every prompt comes out
 * `[reference generation …]` no matter what the author meant.
 *
 * | role | kind | drives |
 * |---|---|---|
 * | `identity_source` | image, video | `reference generation` — a face/wardrobe/style to carry |
 * | `structure_source` | image, video | `reference generation` — camera, cutting rhythm, storyboard |
 * | `storyboard` | image | `reference generation` — a layout to follow, not a frame to match |
 * | `frame_anchor` | image | `keyframe completion` — a concrete frame inside the target |
 * | `first_frame` / `last_frame` | image | `keyframe completion` at a named end |
 * | `edit_source` | video | `video editing` — this clip is directly modified |
 * | `continuation_source` | video | `video continuation` — the target resumes from it |
 * | `audio_reuse` | audio | `audio reuse` — the signal itself is reused |
 * | `voice_timbre` | audio | `audio reference` — timbre/delivery only, not the signal |
 * | `audio_continuity` | audio | `audio reference` — the bed carries on, not copied |
 */
export type H3ReferenceRole =
  | 'identity_source'
  | 'edit_source'
  | 'continuation_source'
  | 'structure_source'
  | 'voice_timbre'
  | 'audio_reuse'
  | 'audio_continuity'
  | 'frame_anchor'
  | 'storyboard'
  | 'first_frame'
  | 'last_frame';

/**
 * One MiniMax-H3 omni reference — a cast member, a location, a voice.
 *
 * NOT the same thing as an LTX {@link DirectorCharacter}, and not convertible to one. An LTX
 * character ref is a rotary-tagged latent token: it injects an identity the prompt cannot name.
 * An H3 reference goes through the Qwen3-VL text encoder and is presented ordinal-labelled in
 * language-model space, so the prompt can ADDRESS it — `"<Picture 2> wears the red coat"`.
 *
 * ⚠️ **Order is semantic.** A reference's position in the array assigns its `<Picture i>` /
 * `<Video k>` / `<Audio j>` ordinal AND advances a shared rotary clock, so reordering the same
 * references is a different request. The editor must therefore offer explicit reordering and must
 * never sort or group this list for display.
 */
export interface DirectorH3Reference {
  /** Stable editor id. Never sent. */
  id: string;
  kind: H3ReferenceKind;
  /**
   * What this reference is FOR. See {@link H3ReferenceRole} — it decides the prompt's task-type
   * prefix and is a genuine authoring choice, not a derived value.
   *
   * Absent on every reference added before roles existed. The backend then falls back to the
   * weakest claim for the kind, which is safe but flattens every prompt to
   * `[reference generation]` — so the editor defaults it on add rather than leaving it unset.
   */
  role?: H3ReferenceRole;
  /** `kind: 'image'` — an `asset:<hash>` ref (or literal base64 on a legacy doc). */
  image?: string | null;
  /**
   * `kind: 'video'` — the clip as a CONTAINER (`asset:<hash>` of the mp4/webm).
   *
   * 🔴 A container, not a frame array. `_add_video_reference` opens a file and does its own 2 fps
   * conditioner sampling and block timestamping; the shim's request model is `extra="forbid"`, so
   * a reference carrying `frames` is a 400 rather than a slow path. The editor never needed to
   * decode video in the first place — which is why the upload was blocked with an apology until
   * this was corrected.
   */
  video?: string | null;
  /**
   * @deprecated Legacy per-frame array. Read for old documents only and never emitted: the engine
   * takes a container (see {@link DirectorH3Reference.video}). Nothing ever successfully attached
   * one — the editor refused the upload — so this exists to keep an old doc parseable, not to keep
   * a working feature alive.
   */
  frames?: string[] | null;
  /** A soundtrack, for a `video` or `audio` reference. An `image` reference may not carry one. */
  audio?: string | null;
  /**
   * `kind: 'video'` — does this container's OWN soundtrack earn an `<Audio n>` ordinal?
   *
   * 🔴 A deliberate authoring choice, not an inference from the file having sound
   * (`GUIDE-ref:112`), which is why it is a field. It is also the only way to address a reference
   * video's audio at all: `_add_video_reference` appends the `{"type":"audio"}` row BEFORE the
   * `{"type":"video"}` row (`pipeline.py:249-254`), so turning this on allocates an `<Audio k>`
   * ahead of that clip's `<Video k>` and shifts every later audio ordinal by one.
   *
   * ⚠️ Do NOT reach for {@link DirectorH3Reference.audio} instead. A video reference carrying a
   * SEPARATE soundtrack is refused by koblem by name — *"the container's own audio is what the
   * engine reads"* — so that shape is a 400, not a second way to say this.
   */
  with_audio?: boolean;
  /**
   * `kind: 'video'` — how many frames of the clip to condition on.
   *
   * 🔴 Must be grid-legal: `5 + 17k` in `[56, 345]`, which `H3_LEGAL_TAIL_FRAMES` in
   * `lib/director/h3-generations.ts` generates. `_add_video_reference` truncates to the grid
   * keeping the **FIRST** frames (`pipeline.py:245-246`), so an off-grid tail is silently
   * shortened and the model is then told a lie about what moment it is looking at.
   */
  tail_frames?: number;
  /**
   * Which shots this is in scope for, in RENDERED-shot index space.
   *
   * **`undefined`/absent = every shot. An explicit `[]` = no shot.** Keep the two apart: collapsing
   * them turns "this reference is for nobody" into "this reference is for everybody".
   */
  segments?: number[];
  /** Editor-only label, so a list of thumbnails is readable. Never sent — the model addresses a
   *  reference by its ORDINAL, not by a name. */
  label?: string;
}

/**
 * One H3 reference as the PROMPT WRITER needs it: what kind it is, whether it carries a
 * soundtrack, and what the user called it. No pixels.
 *
 * Sent to the LLM endpoints, never to the engine. The writer is not being asked to look at the
 * reference — the render does that — it is being told which ordinal to address and what the user
 * meant by it, so it can give each one an explicit job the way the corpus does ("Use Picture 1 for
 * the overall mood; Picture 2 as the protagonist reference"). The label is included precisely
 * because it is the one thing the ENGINE never sees: it exists to help the writer decide what a
 * reference is FOR, and the prompt it writes then says so in the model's own ordinal terms.
 */
export interface H3PromptReference {
  kind: H3ReferenceKind;
  /**
   * What the author says this is for. The single input the backend derives the `summary:` task-type
   * prefix from — see {@link H3ReferenceRole}. Omitted only on a reference that predates roles.
   */
  role?: H3ReferenceRole;
  /**
   * Is there a soundtrack the prompt may address? Always true for `audio`; illegal on `image`.
   *
   * On a `video` this tracks {@link DirectorH3Reference.with_audio}, because that is what decides
   * whether the container's track is allocated an `<Audio n>` at all. Telling the writer a video
   * "has audio" when no ordinal was allocated invites it to name a label the conditioner never
   * packed, which is prose about nothing.
   */
  has_audio?: boolean;
  label?: string;
  /**
   * Can the characters HEAR this audio? `audio_reuse` only.
   *
   * 🔴 Omitted means "the guide decides", and the guide decides **audience-only** every time:
   * `GUIDE-ref:296` sends a reused score to `non_diegetic_music`, and that section is *defined* as
   * music the characters cannot hear. On a music video that tells the model the performer cannot
   * hear the track it is singing, and the singing stops locking to the vocal.
   */
  diegetic?: boolean;
  /** What the project does with this soundtrack at delivery. `audio_reuse` only. */
  soundtrack_mode?: 'replace' | 'keep' | 'mix';
}

/** The project-level soundtrack facts the writer needs, which live on the timeline, not the ref. */
export interface H3SoundtrackFacts {
  mode: 'replace' | 'keep' | 'mix';
  /** `true` when the characters hear the song and perform to it — a music video. */
  diegetic: boolean;
}

/**
 * One stretch of a generation's slice of the song where a voice is audible.
 *
 * 🔴 `start`/`end` are LOCAL seconds — measured from that generation's own frame 0, which is where
 * its slice of the song begins, and which is the clock a `[Shot N] At MM:SS.mmm` mark runs on.
 * `local = songTime - slice.songOffsetSeconds`. NOT `deliveredAtSeconds`, which is one trim frame
 * later. See `lib/director/h3-soundtrack.ts`.
 */
export interface H3VocalSpan {
  start: number;
  end: number;
  /**
   * A few of the transcript's own words at this span's ENTRY, for the guide's §5.4 idiom
   * (`When <Audio N> reaches <d>[English] …</d>, <Subject M> performs that vocal`).
   *
   * ⚠️ Machine-heard, from a recogniser pointed at SINGING, and often wrong about the words while
   * being right about the times. The server's clause says so to the writer in as many words.
   */
  words?: string;
}

/**
 * Where the song's voice falls inside ONE generation. Mirrors `h3_prompting::H3VocalLane`.
 *
 * 🔴 **An object rather than a bare span array, because the empty case is the whole point.** No
 * lane at all means "nobody measured" and leaves the server on its old `speech_in_shot` boolean; a
 * lane with `spans: []` means "measured, and this generation is entirely instrumental", which is
 * what stops a character miming a vocal that is not there. A bare array collapses those two.
 */
export interface H3VocalLane {
  /**
   * Seconds of RUN-UP at the head of this render that never reach the cut — the frames an
   * `on_target` carry overlays and the engine then trims. The model renders and hears them, so the
   * lane covers them; but a `[Shot N] At MM:SS.mmm` at or below this is DISCARDED in silence, so
   * the server turns it into "already sounding when the shot appears, describe it, do not time it".
   * Omitted when 0 (first generation, or an unanchored seam).
   */
  run_up_seconds?: number;
  /** 1-based generation number. Omit on the single-generation path, where it is unambiguous. */
  generation?: number;
  /** The generation's RENDERED length, seconds — so the clause can name the silent stretches. */
  seconds?: number;
  spans: H3VocalSpan[];
}

/**
 * The references in scope for shot `idx`, in project order, reduced to what the writer needs.
 *
 * **Scoping, not filtering-and-renumbering.** `undefined` scope = every shot, an explicit `[]` = no
 * shot; the two must never collapse into each other. Order is preserved exactly, because the
 * engine's ordinals are positional — it rebuilds this same in-scope list per shot and the
 * conditioner's per-kind counters restart at 1 for each one, so the Nth in-scope image IS
 * `<Picture N>` regardless of where it sits in the project roster.
 */
export function h3RefsForShot(
  refs: DirectorH3Reference[],
  idx: number,
  soundtrack?: H3SoundtrackFacts
): H3PromptReference[] {
  return h3RefBriefs(
    refs.filter((r) => r.segments === undefined || r.segments.includes(idx)),
    soundtrack
  );
}

/**
 * The same reduction, over a list somebody else has already scoped.
 *
 * 🔴 Split out of {@link h3RefsForShot} because `segments` is no longer how scope is decided. A
 * generation-level `reference_ids` wins outright (`referencesForGeneration`), and it is the
 * granularity the conditioner actually has — `pipeline.py:311-340` builds ONE reference
 * presentation per call. A caller that has the packing must therefore hand the packed list
 * straight in: filtering it AGAIN by `segments` would re-widen a narrowed scope, and the writer
 * would be briefed on assets the render was never going to carry and would name them in the prose.
 *
 * ⚠️ ORDER IS PRESERVED and must be. The engine's ordinals are positional, so the Nth in-scope
 * image IS `<Picture N>`; sorting or grouping here re-points every label in the drafted prompt.
 */
export function h3RefBriefs(
  refs: DirectorH3Reference[],
  soundtrack?: H3SoundtrackFacts
): H3PromptReference[] {
  return refs.map((r) => ({
    kind: r.kind,
    ...(r.role ? { role: r.role } : {}),
    // 🔴 Only onto the REUSED soundtrack, and only when the caller actually knows. A
    // `voice_timbre` or `audio_continuity` reference is not the project's song, and putting the
    // project's delivery mode on it would brief the writer about the wrong asset. Absent stays
    // absent so the server keeps its old behaviour rather than defaulting to a guess.
    ...(soundtrack && r.kind === 'audio' && r.role === 'audio_reuse'
      ? { diegetic: soundtrack.diegetic, soundtrack_mode: soundtrack.mode }
      : {}),
    // 🔴 Per kind, and for a VIDEO strictly `with_audio` — never "the reference has an audio
    // payload". A video carrying a separate soundtrack is a request koblem refuses by name (the
    // container's own track is what the engine reads), so counting it here would put an
    // `<Audio k>` in the roster the writer is told to address for a render that cannot happen —
    // and shift every later audio ordinal in the prompt while it did so.
    has_audio: r.kind === 'audio' ? true : r.kind === 'video' ? !!r.with_audio : false,
    ...(r.label?.trim() ? { label: r.label.trim() } : {})
  }));
}

/**
 * Which MiniMax-H3 CHECKPOINT a generation renders on. Derived, never chosen.
 *
 * 🔴 `fl2va` and `ref2va` are different weight sets served from different directories, not a flag
 * on one model — so a generation carries EITHER keyframes (`init_image` / `end_image`) or omni
 * references, never both, and switching between them inside one job costs a full DiT +
 * text-encoder reload that the plan echo reports as `reload_required`.
 *
 * `t2va` is the same checkpoint as `fl2va` with nothing pinned; it is a distinct value only
 * because it selects a different prompt layout (no alignment instruction line).
 */
export type H3Task = 't2va' | 'fl2va' | 'ref2va';

/** How a generation is joined to the one before it. See {@link DirectorH3Continuation}. */
export type H3ContinuationMode = 'latents' | 'frame' | 'none';

/**
 * How one generation continues out of the previous one — the ONLY place a seam can appear.
 *
 * 🔴 **None of these is frame-exact and the UI must not imply otherwise.** MiniMax's own words
 * (via SGLang) are that Ref2VA *"treats the input video as reference material, not as a
 * pixel-aligned edit source. It can resynthesize or reorder motion and cuts, and it does not
 * expose a denoising-strength control."* Ref2VA has no `frame_index` at all. Our latent pin is an
 * out-of-distribution patch of our own — the best known path, not the model's contract.
 *
 * ## Two wire fields are deliberately NOT here, and were not forgotten
 *
 * - `trim_join` is coupled to `anchor`: it drops the frame the anchor duplicates, so with no
 *   anchor there is no duplicate to drop. The shim already zeroes it (with a warning) in that
 *   case, and a second copy of that coupling in the editor is a second place for it to go stale.
 * - `chain_anchor` is per-CHAIN **server** state — frame 0 of the chain's FIRST generation,
 *   carried forward through the latent bank so shot 4 anchors to shot 1 exactly as shot 2 does. A
 *   stateless caller has nowhere to keep it: a chain outlives the 600 s job TTL, outlives a retake
 *   (which is a new job) and outlives a container restart. Left null — which is what a caller
 *   should do — the shim carries it. Sending our own would re-root the chain, and getting that
 *   wrong is a random walk that still returns 200 (−23% luma over four shots against −2.5%).
 *
 * ## Three more are GONE, because they were WanGP's and the backend is ComfyUI
 *
 * `tail_frames`, `drop_guide` and `posterior_noise` used to live here. On the ComfyUI backend
 * the carry's depth is `k` and only `k` (k=7 IS 22 pixel frames — there is no second length to
 * set), there is no video guide to drop, and the noise knob is a MIX FACTOR rather than an
 * additive sigma, so the units never converted. koblem filled all three from defaults and put
 * them on the wire regardless, and the shim answered every continued shot with warnings the
 * Director then showed the author as if the render had failed. Old documents still load —
 * the fields are ignored, not rejected.
 */
export interface DirectorH3Continuation {
  /**
   * `latents` keeps velocity and motion through the join but shifts contrast and grade across it,
   * and costs render time. `frame` keeps framing, identity and grade but freezes some velocity.
   * `none` is a hard cut — correct whenever the boundary was placed ON a cut, which is what the
   * timeline steers toward.
   */
  mode: H3ContinuationMode;
  /**
   * Which latent bank to continue from. Absent ⇒ `"prev"`, the generation immediately before.
   *
   * 🔴 `"prev"` is resolved by koblem/the shim AT RUN TIME, which is why it stays the default: it
   * survives a retake that re-renders an earlier generation, whereas a baked key would pin to the
   * take that was current when it was written.
   *
   * An explicit key is now honoured end to end — it is stored here, `compileH3Timeline` no longer
   * overwrites it with `'prev'`, and `h3_single_generation_chain` forwards a non-`prev` key
   * verbatim ("not the shim's to resolve, and not ours to second-guess"). That is what makes
   * "continue from take 2" expressible; before, the compiler clobbered it and the capability was
   * unreachable rather than unimplemented.
   *
   * ⚠️ A stored key can be EVICTED — the engine's bank is size-capped and reclaims oldest-first —
   * and koblem cannot ask whether one still exists. A vanished key fails the render with the shim's
   * own message naming it, which is the intended outcome: falling back to a hard cut would deliver
   * exactly the silent seam an explicit pin was chosen to avoid.
   */
  from?: string;
  /**
   * Latent-token pin depth. `(T_prev − k) % 5 == 0` is the engine's guard, where `T_prev` is the
   * previous generation's LATENT TOKEN count (`((frames − 5) / 17) * 5 + 2`) and NOT its frame
   * count. That value is `≡ 2 (mod 5)` at every legal length, so the legal depths are
   * `{2, 7, 12, …}` for every shot and the default 7 always fits.
   */
  k?: number;
  /** Also pin a single anchor FRAME at the target origin, on top of the latent carry. */
  anchor?: boolean;
}

/**
 * One MiniMax-H3 **generation** — the render unit, the delivery unit and the retake unit.
 *
 * 🔴 This is the type that carries the whole reframe. A Director clip is a SHOT; a generation is
 * up to ~15 s holding one or more consecutive shots, whose cuts are `[Shot N] At MM:SS.mmm` lines
 * inside ONE prompt that the model renders itself. Everything the schema names once per prompt —
 * the subject definitions, the summary, the retention analysis, the soundscape, the score — is
 * therefore per GENERATION, and only the `[Shot N]` body text is per shot.
 *
 * ## What is stored here and what is not
 *
 * The PACKING is not stored: one clip is one render, so it is re-derived from the clip order every
 * time by `lib/director/h3-generations.ts`. `clip_ids` is kept anyway — not as the authority on the
 * packing, but so a re-derived generation can be matched back to the sections a human already wrote
 * for it, which is also what carries a block through a merge or a collapse.
 *
 * `task` is likewise derived (from the references and frame pins in scope) and recorded only so
 * the editor can show which checkpoint a generation lands on. 🔴 `compileH3Timeline` re-derives it
 * and never reads this field — a stored value that disagreed would otherwise pick the wrong
 * checkpoint silently.
 */
export interface DirectorH3Generation {
  /** Stable editor id. Survives re-packing so the authored sections follow their generation. */
  id: string;
  /**
   * The shots this generation packs, in order, by `DirectorClip.id`. A record of what the packing
   * was when the sections were written — see the type doc. Ids of deleted clips are pruned on read.
   */
  clip_ids: string[];

  // ── Ref2VA-only sections ────────────────────────────────────────────────────
  /**
   * Task types for the `summary:` bracket prefix, joined with ` + ` and never repeated.
   *
   * ⚠️ No ordering rule is stated anywhere in the corpus and the two official examples order them
   * differently, so the canonical order is ours (`SPEC-h3-prompt-emitter.md` §K, row 7).
   */
  task_types?: H3TaskType[];
  /**
   * `<Subject N>` / `<Video N>` / `<Audio N>` definitions, one per line in the output.
   *
   * An image used only for identity or style gets NO standalone `<Picture N>` retention entry — it
   * is cited inside the `<Subject N>` definition that uses it.
   */
  subject_definitions?: H3SubjectDefinition[];
  /** The summary paragraph. The composer prepends the `[task types]` bracket; this is prose only. */
  summary?: string;
  /** One entry per referenced label, each with a marker from the correct fixed vocabulary. */
  retention_analysis?: H3RetentionEntry[];

  // ── Both schemas ────────────────────────────────────────────────────────────
  /**
   * One or two sentences of style.
   *
   * 🔴 Where it goes FLIPS with the checkpoint: Ref2VA writes it on its own line BEFORE `[Shot 1]`
   * inside `detailed_description`; the FL2VA family writes it AFTER the `[Shot 1]` tag, inside the
   * shot. Stored once, placed by the composer.
   */
  style_opening?: string;
  /** 1–4 sentences: ambience, physical action sounds, non-verbal human sounds. No dialogue, no
   *  diegetic music. `N/A` only for requested total silence. */
  overall_soundscape?: string;
  /** 1–3 sentences of audience-only score: instrumentation, tempo, rhythm, dynamics. `N/A` if
   *  none. The guide forbids abstract mood words (its own corpus breaks that rule; follow it). */
  non_diegetic_music?: string;

  /**
   * The references in scope for THIS generation, by `DirectorH3Reference.id`, in project order.
   *
   * Absent = fall back to the union of the per-shot `segments` scopes on the references
   * themselves. Present, it wins — and per-generation is the granularity the engine actually has:
   * `pipeline.py:311-340` builds ONE reference presentation per call, so a scope that varies
   * shot-to-shot inside a generation is describing something the conditioner cannot do.
   *
   * ⚠️ Scoping NARROWS the project list; it never reorders it. Position assigns the `<Picture i>` /
   * `<Video k>` / `<Audio j>` ordinal AND advances a shared rotary clock, so a reordered scope is a
   * different request rather than a re-labelling.
   */
  reference_ids?: string[];
  /** How this generation joins the previous one. Absent on the first — there is nothing to join. */
  continuation?: DirectorH3Continuation;

  // ── Derived, but recorded so the editor can show them ────────────────────────
  /** The checkpoint this lands on. Recomputed by the compiler, which never reads this. */
  task?: H3Task;
  /**
   * A plain-language line saying what happens in this SHOT — the whole generation, whole prompt.
   *
   * 🔴 **DISPLAY ONLY. It is NEVER compiled into the request, and it is the only field on this type
   * that is not on the wire.** Every one of its neighbours — `seed`, `steps`, `width`, `loras`,
   * `prompt` — becomes part of what the engine is sent, so the assumption a reader arrives with is
   * that this one does too. It does not: `compileH3Timeline` builds `H3RequestGeneration` from an
   * explicit field list and this is absent from it, deliberately. Adding it there would put an
   * editor's caption into the conditioning of every render.
   *
   * What it IS: the label the timeline band and the shot list draw for this render — one call, one
   * delivery, one retake, one sentence. The prompt is the artifact; this is how you recognise it in
   * a row of eight without reading 7,000 characters of schema.
   *
   * Written by the "Summarise this shot" action on the Prompt tab (the engine-neutral `gist_only`
   * path) and freely editable by hand, exactly like the `summary:` section it is NOT — that one is
   * MiniMax schema text inside `prompt` and does render.
   *
   * ⚠️ Not the same thing as `DirectorClip.gist`, which survives untouched and is still the label
   * for one BEAT inside this shot. A beat still wants a short line in a list of beats; this is the
   * line for the thing that renders. On LTX, where a shot IS a clip, `clip.gist` remains the only
   * one and nothing here applies.
   */
  gist?: string;
  /**
   * Per-generation seed. Absent (or negative) → the engine rolls one, which is what a fresh take
   * wants; `compileH3Timeline` forwards it only when it is >= 0, so "random" is never pinned by
   * accident.
   */
  seed?: number;
  /**
   * Per-generation step count. Absent → the PROJECT's step count if it has one, else the engine's
   * own default (20). The one time it is worth setting is alongside a turbo LoRA in
   * {@link DirectorH3Generation.loras} — the LoRA does not lower the step count itself, and at 20
   * steps it buys nothing.
   */
  steps?: number;
  /**
   * Per-generation canvas override. Both or neither; absent → the project's size.
   *
   * 🔴 **A different size is a different DRAW, not the same shot bigger.** The initial noise is
   * `randn((1, 24, latent_t, latent_h, latent_w), manual_seed(seed))`, so changing either side
   * changes the latent SHAPE and therefore the sample the seed produces: at the same seed a taller
   * canvas gives fresh composition, camera path and subject placement — and because the audio noise
   * comes off the same generator immediately afterwards, a different music bed too. Nothing about
   * the previous render survives except the prompt. This is the same fact
   * {@link EngineCapabilities.previewIsFullRes} exists for, and it is why this is not a quality
   * knob: re-sizing a generation you liked will not give you that generation, larger.
   *
   * ⚠️ Both sides must be multiples of 32 (`checkCanvas`), and a much larger canvas may not fit the
   * 16 GB card — the area every H3 timing and VRAM number was measured at is 1344×768.
   */
  width?: number;
  height?: number;

  // ── Render knobs the author sets per generation ──────────────────────────────
  /**
   * LoRA files for this generation, as bare filenames in the engine's loras dir.
   *
   * 🔴 The headline is the turbo LoRA — `h3-turbo-4step-ema-pruned.safetensors` at 8 `steps` is
   * 465.4 s against the 20-step control's 762.8 s, **−39% wall for ZERO extra VRAM**. It does not
   * change `steps`: this field and that one are set together or the win is not collected.
   *
   * ⚠️ Verification is by EYE, not by log. `"(N/N) applied"` means the file loaded, not that an
   * adapter attached — a turbo LoRA that fails to attach renders 8 steps of 20-step quality, i.e.
   * it looks broken rather than fast. Cost scales with the COUNT of LoRAs (~5%/step each), not
   * with rank.
   */
  loras?: string[];
  /**
   * Positional multipliers for {@link DirectorH3Generation.loras}, padded with 1.0. A list longer
   * than `loras` is refused by koblem rather than truncated.
   */
  lora_multipliers?: number[];
  /**
   * 🔴 **The assembled block, and the STORED ARTIFACT of this generation.** This is what renders.
   *
   * Until 2026-08-08 the fields above were the document and the block was a derived preview, with
   * `prompt_override` bolted on as a dead-end escape hatch that dimmed every panel. That made the
   * composer's form the only legal way to produce a prompt, so the prompt LLM was made to fill our
   * editor's JSON shape, a chat edit that produced a real block was flagged as a defect, and a
   * hand-written block could never be read back. The direction is now reversed: **the block is the
   * document and the panels are a lens on it**, both ways live.
   *
   * - a panel edit rewrites ONE section's bytes via `replaceH3Section` and leaves every other byte
   *   alone — including bytes no field can represent;
   * - a block edit reparses via `parseH3Block` and the panels redraw;
   * - `composeH3Prompt` SEEDS this field (a fresh generation, or an LLM that answered with fields
   *   rather than text) and never rewrites it.
   *
   * ⚠️ **Empty or all-whitespace means "fall back to the composed block", not "render
   * unconditioned".** `compileH3Timeline` composes from the fields when this trims to nothing —
   * a stored `""` is far more likely to be a box somebody cleared, and an empty prompt does not
   * reach the engine anyway: koblem drops an empty-prompt generation from the chain, so the
   * generation would silently VANISH from the render rather than fail.
   *
   * 🔴 It does not follow a re-pack. Trim a shot and the `[Shot N] At MM:SS.mmm` cut times in this
   * text are stale — the editor shows the drift and offers to realign the line-up rather than
   * rewriting an author's prose behind them.
   *
   * ⚠️ The retired `prompt_override` is NOT migrated. It existed for about a day; a document still
   * carrying one has no `prompt` key, so it seeds a fresh block from its stored sections on first
   * open like any other, and the old text is dropped with the rest of the unknown fields.
   */
  prompt?: string;
}

/**
 * The six Ref2VA task types, verbatim. They compose with ` + ` in the `summary:` bracket prefix.
 *
 * A closed set on purpose: these are English literals the model was trained to see, not a
 * description a writer improvises. `GUIDE-ref:143` also warns that the mere PRESENCE of a video or
 * audio asset does not create the corresponding type — a clip that only supplies camera movement
 * or cutting rhythm is `reference generation`, not `video editing`.
 */
export type H3TaskType =
  | 'keyframe completion'
  | 'reference generation'
  | 'video editing'
  | 'video continuation'
  | 'audio reuse'
  | 'audio reference';

/** One `subject_definitions` line. `label` is assigned by the composer from position, never typed. */
export interface H3SubjectDefinition {
  /** `<Subject 1>`, `<Video 1>`, `<Audio 1>` — angle brackets included; they are prompt text. */
  label: string;
  text: string;
}

/**
 * One `retention_analysis` line: `<Label> (role note): marker - text`.
 *
 * 🔴 The marker vocabularies are DISJOINT by label kind. `<Audio N>: partially_preserved` and
 * `<Subject N>: reference` are both schema errors, not stylistic slips — the two tables are
 * `H3_VISIBLE_MARKERS` and `H3_AUDIO_MARKERS` in `lib/director/h3-prompt.ts`, which is also where
 * the check lives.
 */
export interface H3RetentionEntry {
  label: string;
  /** The parenthesised scope note — `appears in [Shot 1]`, `continuation starting point`. */
  role_note?: string;
  marker: string;
  text: string;
}

/** `GET /api/v1/minimax-h3/props`. `configured` false = the service URL is unset. */
export interface H3Props {
  configured: boolean;
  healthy: boolean;
  /** Always 24. The frame grid, the 4x temporal VAE and the 40 Hz audio latent rate are all
   *  DEFINED at it, so a non-24 request is refused rather than resampled. */
  fps: number;
  frame_period?: number;
  frame_base?: number;
  /**
   * The SPEC bounds on a generation: **22** frames (0.917 s) floor, 362 (15.083 s) cap.
   *
   * 🔴 The floor is the video VAE's own DECODABLE minimum, not a duration rule. It was 107 until
   * 2026-08-07 — that is the grid-snap of MiniMax's documented 4-second guideline, and enforcing
   * it silently lengthened every shorter generation. See `documented_min_frames`.
   *
   * What this BOX can currently reach is a different number and lives at
   * **`capabilities.frames.reachable_max`** — nested, because koblem forwards the shim's capability
   * document verbatim and the shim groups its frame facts under `frames`. It is NOT
   * `capabilities.max_frames`: that flat key is this spec cap one level up, and reading it there
   * found nothing on every real engine (see `reachableMaxFrames` in `lib/director/engine.ts`, which
   * had exactly that bug). Optional here because a koblem old enough to predate the constant
   * answers without them, in which case the editor falls back to the values in `capabilities()`.
   */
  min_frames?: number;
  max_frames?: number;
  /**
   * MiniMax's documented shortest output (107 frames = 4.458 s), as an ADVISORY figure.
   *
   * A generation under it renders at the length authored and is flagged once; it is not corrected.
   * ⚠️ The engine still needs `H3_SHORT_CLIPS=1` to accept it — without that the shim raises the
   * length to 107 and says so in the plan warnings, naming the switch.
   */
  documented_min_frames?: number;
  documented_min_seconds?: number;
  /** 🔴 HARD, and model law. 1280×720 and 1920×1080 are NOT legal H3 canvases. */
  dim_multiple?: number;
  /**
   * 🔴 ADVISORY — see `max_pixels_advisory`. The area 1344×768 was measured at, not a limit the
   * model has: `diffusers` calls this exact number a *"soft area cap"*, ComfyUI does not enforce
   * it, and no config in the weights tree carries a max resolution. Over budget warns about VRAM.
   */
  max_pixels?: number;
  max_pixels_advisory?: boolean;
  /**
   * The reference budget per generation.
   *
   * 🔴 RESOLVED FROM THE LIVE ENGINE when it is up, and only falls back to koblem's compiled-in
   * constants when it is down. `max_video_refs`/`max_audio_refs` are **3** — they read 2 until
   * 2026-08-21 because WanGP held exactly two named guide slots per kind, and WanGP was replaced
   * by ComfyUI on 2026-08-12. ComfyUI's reference inputs declare `max: 3` (images `max: 9`), which
   * is MiniMax's documented figure — so the editor withheld a real slot for nine days.
   */
  max_image_refs?: number;
  max_video_refs?: number;
  max_audio_refs?: number;
  max_total_refs?: number;
  audio_only_refs?: boolean;
  /**
   * The reference-video floor, in frames. **5**, and it is a HARD refusal — `MiniMaxH3ReferenceToVideo`
   * raises below it, which on the engine means a dead render minutes in, after the DiT has loaded.
   *
   * 🔴 It read **48** until 2026-08-21 (2.0 s at 24 fps, from WanGP's validator, which no longer
   * runs). The editor is the right place to enforce this: at UPLOAD time the browser knows the
   * clip's duration and the user can still choose another. koblem cannot — it sees a container.
   */
  min_ref_video_frames?: number;
  /**
   * **Always `null`.** There is no constant ceiling: a longer reference video is TRUNCATED to the
   * generation's own frame count, silently, so the model simply never sees the tail. Use the
   * shot's own frame count if you want to warn about it. See `ref_video_max_rule`.
   */
  max_ref_video_frames?: null;
  ref_video_max_rule?: string;
  /** `"5+17k"`. An off-grid reference video is snapped DOWN to this, silently, by the node. */
  ref_video_grid?: string;
  /**
   * Reference images have NO aspect requirement and are never cropped.
   *
   * 🔴 `ref_image_aspect_min`/`max` (0.25/4.0) used to be published here, out of WanGP's
   * `pipeline.py:63`. There is no aspect check anywhere in the ComfyUI node: it scales every
   * reference aspect-preserving and down-only — to the generation's pixel area, or to a 2048px
   * short edge under `ref_image_size: "max"` — with cropping disabled. Removed, not replaced.
   */
  ref_image_aspect_unrestricted?: boolean;
  /** A reference is only ever scaled DOWN. A small image is used at its own size. */
  ref_image_downscale_only?: boolean;
  /**
   * What koblem's compiled-in H3 constants and the LIVE engine disagree about. Empty is healthy.
   *
   * 🔴 The fields above already prefer the engine, so a UI reading this document is correct
   * even when this is non-empty. What is stale in that case is koblem's REQUEST VALIDATION, which
   * runs at submit with nothing fetched — so a non-empty `mismatched` predicts a request the
   * editor will let you build and koblem will then refuse. Worth surfacing to an operator.
   */
  capability_drift?: {
    /** A real disagreement. One of the two trees has to move, and the engine is the authority. */
    mismatched?: string[];
    /** The engine publishes no value at all, so the constant cannot be checked. Normal transiently
     *  when koblibs ships ahead of the engine image. */
    unpublished?: string[];
  };
  /** The engine's own capability document, and (with a query) the plan those values would render
   *  at. `null` when the engine is down — the editor stays usable, the render buttons go quiet. */
  capabilities?: Record<string, unknown> | null;
  health?: Record<string, unknown> | null;
  /**
   * The adapters actually on disk in the engine's `minimax_h3/` LoRA directory.
   *
   * 🔴 `null` and `{count: 0}` are different answers and the editor must not conflate them. `null`
   * is "the engine is down, or too old to serve the listing" — offer the free-text box, because a
   * name typed from memory is still the user's best move. `count: 0` is "the directory is there and
   * EMPTY", which on this service almost always means the `/loras` volume is not mounted — and then
   * every `loras[]` request is refused however well-formed it is, so offering a picker with nothing
   * in it would be the cruellest possible UI.
   */
  loras?: H3LoraListing | null;
}

/** `GET /h3/v1/loras`, forwarded verbatim through props. */
export interface H3LoraListing {
  /** The directory a bare name resolves against — `<loras root>/minimax_h3`, not the root. A file
   *  dropped in the root is invisible to the engine. */
  dir?: string;
  exists?: boolean;
  count?: number;
  loras?: H3LoraEntry[];
  /**
   * Present only when the listing is EMPTY: the shim's own diagnosis, worth showing verbatim
   * because it distinguishes the two cases the count cannot — the directory exists and holds
   * nothing, versus nothing is mounted there at all.
   */
  warning?: string;
  note?: string;
}

/** One adapter on the engine's disk. */
export interface H3LoraEntry {
  /** What goes in `loras[]` — a bare filename relative to `dir`, and the ONLY identifier there is. */
  name: string;
  filename?: string;
  bytes?: number;
  /** Unix seconds. The listing is newest-first, which is usually the one you just copied in. */
  mtime?: number;
  /**
   * False for a file in a SUBDIRECTORY. The engine's enumerator globs `**` so it is genuinely
   * there, but a wire name containing a separator is refused (it could escape the root) — so it is
   * listed, unpickable, and the UI says why rather than hiding a file the user can see on disk.
   */
  addressable?: boolean;
}

/**
 * `GET /api/v1/minimax-h3/props` — is the engine there, and what would these values render at.
 *
 * The optional geometry is answered by the engine's SUPERVISOR without starting a CUDA child, so
 * the editor can show a snapped duration while the user is still typing, without evicting the LLM
 * to ask.
 */
export async function getH3Props(geometry?: {
  width?: number;
  height?: number;
  frames?: number;
}): Promise<H3Props> {
  const q = new URLSearchParams();
  if (geometry?.width) q.set('width', String(geometry.width));
  if (geometry?.height) q.set('height', String(geometry.height));
  if (geometry?.frames) q.set('frames', String(geometry.frames));
  const qs = q.toString();
  return request(`/api/v1/minimax-h3/props${qs ? `?${qs}` : ''}`);
}

/** One reference as the ENGINE takes it, inside a generation. See {@link H3GenerateRequest}. */
export interface H3RequestReference {
  kind: H3ReferenceKind;
  /**
   * The FULL authoring role, sent through verbatim.
   *
   * 🔴 Not narrowed here, and that was a bug worth naming: mapping the eleven roles down to the
   * engine's own vocabulary in the frontend made `audio_reuse` unreachable — the editor could not
   * express "reuse this signal" at all — and collapsed the distinctions koblem needs to derive the
   * prompt's `[task type + task type]` prefix. koblem accepts the full set and does the mapping,
   * because it is the side that owns both the prompt and the engine call.
   */
  role?: H3ReferenceRole;
  /** `image` — an `asset:<hash>` ref (or literal base64 on a legacy doc). */
  image?: string;
  /** `video` — the CONTAINER. Not a frame array; the engine opens the file and samples it itself. */
  video?: string;
  /** A soundtrack, for a `video` or `audio` reference. */
  audio?: string;
  /**
   * `video` only — give this container's OWN soundtrack an `<Audio n>` ordinal.
   *
   * 🔴 The only way a reference video's audio becomes addressable. A video does NOT earn an
   * `<Audio N>` just because the file has sound (`GUIDE-ref:112`), and the ordinal it earns is
   * allocated BEFORE that clip's own `<Video k>` (`pipeline.py:249-254`), so setting it renumbers
   * every audio ordinal after it. Sent only when true — koblem rejects it on an image or an audio
   * reference by name, and honouring it there would shift the same ordinals for no asset.
   */
  with_audio?: boolean;
  /**
   * `video` only — how many frames of the clip to condition on. Grid-legal or nothing.
   *
   * 🔴 `5 + 17k` in `[56, 345]` (`H3_LEGAL_TAIL_FRAMES`). `_add_video_reference` truncates to the
   * grid keeping the **FIRST** frames, so an off-grid value is not refused by the engine — it is
   * silently shortened, and the model is told a lie about time. koblem checks it against the grid
   * so the refusal happens before the GPU is involved.
   */
  tail_frames?: number;
}

/** One shot marker inside a generation. ADVISORY — see {@link H3GenerateRequest}. */
export interface H3RequestShot {
  /** 0-based within the generation. `[Shot 1]` in the prompt is index 0. */
  index: number;
  /** Absolute cut time inside the generation. `null` on index 0, which is untimed. */
  at_seconds: number | null;
}

/** One generation in an {@link H3GenerateRequest}. */
export interface H3RequestGeneration {
  /** Derived by koblem from what is in scope, and stated rather than left to the engine. */
  task: H3Task;
  /** Already snapped onto `5 + 17k`, already clamped into 22..362. */
  frames: number;
  /**
   * `fl2va` keyframes — the ONLY frame-exact mechanism in the system. Asset refs, inline.
   *
   * 🔴 Mutually exclusive with `references`: they are served by different checkpoints, so a
   * generation carrying both is not a request the engine can satisfy with either weight set.
   */
  first_frame?: string;
  last_frame?: string;
  /** The whole composed schema block. The AUTHORITY on where the cuts are. */
  prompt: string;
  /**
   * What was packed, for the plan echo.
   *
   * 🔴 **Advisory only.** The authority on the cut times is the `[Shot N] At MM:SS.mmm` text inside
   * `prompt` — the model reads the prompt and nothing else. This array exists so the 202 echo can
   * show what the Director believed it asked for, which is how a packing bug becomes visible
   * instead of merely looking like a bad render.
   */
  shots: H3RequestShot[];
  /** In scope for this generation, IN ORDER — position assigns each one's ordinal. */
  references?: H3RequestReference[];
  /** Absent on the first generation: there is nothing before it to continue from. */
  continuation?: DirectorH3Continuation;
  seed?: number;
  steps?: number;
  /**
   * LoRA files to attach to THIS generation. Bare filenames inside the engine's mounted loras
   * dir — never a path, which would resolve against WanGP's loras root and could escape it.
   *
   * 🔴 The reason the field exists is `h3-turbo-4step-ema-pruned.safetensors`: at 8 `steps` it is
   * 465.4 s against the 20-step control's 762.8 s — **−39% wall for ZERO extra VRAM** (6528 MiB
   * either way). It does NOT change `steps`; the caller sets those, and a turbo LoRA left at 20
   * steps buys nothing at all.
   *
   * ⚠️ Cost scales with the NUMBER of LoRAs (~5% per step each), not with rank — runtime adapter
   * cost is activation volume, so a rank-8 costs what a rank-128 costs.
   *
   * 🔴 `"(N/N) applied"` in the engine log proves the FILE LOADED, not that any adapter attached
   * to anything. A LoRA that loads and fails to attach renders 20-step quality in 8 steps' time,
   * so it presents as BROKEN rather than as fast — which is the opposite of how a missing speedup
   * usually looks. Judge the output, not the log line.
   */
  loras?: string[];
  /**
   * Per-LoRA multiplier, matched POSITIONALLY to {@link H3RequestGeneration.loras}, padded with
   * 1.0.
   *
   * A SHORTER list is normal (the pad is the point). A longer one is refused by koblem rather than
   * truncated, because it means the caller had a different LoRA list in mind than the one it sent.
   */
  lora_multipliers?: number[];
  /**
   * Per-generation canvas override, replacing the request-level pair for THIS generation only.
   *
   * Emitted only as a PAIR — `compileH3Timeline` drops a half-set override rather than inventing
   * the missing side, which would change the aspect ratio of a render the author only meant to
   * widen.
   */
  width?: number;
  height?: number;
}

/**
 * `POST /h3/v1/generate` — the MiniMax-H3 request, as its own type.
 *
 * 🔴 It is NOT a filtered `LtxVideoGenerateRequest` and must never be typed as one. This shape was
 * previously built as a `Record<string, unknown>` and double-cast, which meant the frontend had
 * zero type safety on the entire H3 wire: a renamed field, a dropped `fps`, a `segments` array
 * left behind from the LTX shape — none of them was a compile error, and all of them fail minutes
 * later on a GPU.
 *
 * Built ADDITIVELY from what H3 has. The fields it lacks are not fields the engine 400s on: they
 * do not exist, and anything unrecognised comes back in `ignored[]` having changed nothing. So
 * `negative_prompt`, `cfg`, LoRAs, model variants, the hires chain, `resume_job_id`,
 * `retake_segment`, `pin_strength`, mid-shot `keyframes`, every `v2v_*` field, the LTX audio sheet
 * and both `sigma_shift_*` knobs are simply unnameable here.
 */
export interface H3GenerateRequest {
  /** Ordered. The engine runs them as a manifest and does NOT re-pack them. */
  generations: H3RequestGeneration[];
  width: number;
  height: number;
  /** 24 and nothing else. Stated rather than omitted so a request that somehow carried another
   *  rate fails at submit instead of rendering against arithmetic that assumed 24. */
  fps: 24;
  /** Ask the engine for in-flight preview frames. Off by default: `generate_preview` decodes
   *  latents through the RGB-factor path per emission and the cost is unmeasured. */
  preview?: boolean;
  /**
   * One song across the whole job — the music-video path.
   *
   * 🔴 **Only the DELIVERY half lives here.** The conditioning half is already in the document as
   * ordinary `kind: 'audio'` references carrying the `soundtrack:` sentinel, one per generation the
   * song is in scope for; koblem replaces each sentinel with that generation's sliced span before
   * staging. Authoring them as real references is what lets the prompt address `<Audio N>` at all,
   * and it makes taking a shot out of the song ordinary reference scoping rather than a second
   * opt-out mechanism.
   *
   * This field is what gets REMUXED over the finished cut. Not redundant: `fully_copy` is a marker
   * in `retention_analysis`, not a copy instruction, so the engine's audio is a reconstruction of
   * the song rather than the song.
   */
  soundtrack?: {
    /** `asset:<hash>` or base64. */
    audio: string;
    /** What happens to H3's own generated audio. Defaults to `replace`. */
    mode?: 'replace' | 'keep' | 'mix';
    /** Ducking under the song in `mix`, dB below unity. */
    mix_duck_db?: number;
  };
}

/**
 * What a Director project compiles to — one shape per engine, discriminated by structure.
 *
 * `generations` exists only on H3 and `segments` only on LTX, so {@link isH3Request} narrows
 * without a cast and every reader has to say which engine it means.
 */
export type DirectorRequest = LtxVideoGenerateRequest | H3GenerateRequest;

export function isH3Request(r: DirectorRequest | undefined | null): r is H3GenerateRequest {
  return !!r && 'generations' in r;
}

/**
 * The LTX request, or `undefined` when this project compiled for another engine.
 *
 * Every reader of `timeline.request` that wants an LTX field goes through here. Without it the
 * union would make `t.request?.upscaler_ratio` a type error at a dozen call sites, and the fix
 * people reach for is a cast — which is how the H3 wire lost its type safety the first time.
 */
export function ltxRequestOf(
  r: DirectorRequest | undefined | null
): LtxVideoGenerateRequest | undefined {
  return r && !isH3Request(r) ? r : undefined;
}

export interface DirectorTimeline {
  version: number;
  fps: number;
  segment_frames: number;
  width: number;
  height: number;
  /**
   * Which engine this project renders on. TOP-LEVEL, next to `model`, because it is render state
   * rather than editor state: a doc that lost it would compile its shots for an engine that
   * refuses half of them. Absent = `'ltx'`, which every existing document is.
   */
  engine?: DirectorEngine;
  /**
   * H3 omni references — the project's cast and locations, in an order that is PART OF THE
   * REQUEST (see {@link DirectorH3Reference}). Top-level for the same reason `characters` is: a
   * reference is a cast member, not a property of one shot.
   *
   * Deliberately a SEPARATE field from `characters` rather than a reinterpretation of it. The two
   * are different mechanisms and do not convert, so a project switched between engines keeps both
   * and uses whichever its engine understands.
   */
  h3_references?: DirectorH3Reference[];
  /**
   * MiniMax-H3 generations — the authored SECTIONS of each render unit, next to `h3_references`
   * and for the same reason: they are render state, not editor state.
   *
   * Absent = derive a default packing and leave every section empty, which is what every project
   * saved before generations existed is. The packing itself is always re-derived (see
   * {@link DirectorH3Generation}); this array only carries what a human wrote.
   */
  h3_generations?: DirectorH3Generation[];
  model?: string;
  audio_kind: 'none' | 'voice' | 'bgm' | 'mixed';
  /**
   * H3 only: what happens to the model's OWN generated audio once a song is supplied.
   *
   * Optional because every document written before H3 took a soundtrack lacks it; `replace` is the
   * only thing such a project could mean, and it is the live default. Persisted rather than left to
   * the editor because it decides what is DELIVERED — a project reopened at the wrong default ships
   * a different mix than its author chose, and nothing about the timeline would look wrong.
   */
  h3_soundtrack_mode?: 'replace' | 'keep' | 'mix';
  /** Can the characters hear the supplied song? Absent = true; see `h3SoundtrackDiegetic`. */
  h3_soundtrack_diegetic?: boolean;
  clips: DirectorClip[];
  /**
   * The project's cast and their identity tags. PROJECT-level on purpose: a `source_id` allocated
   * per shot would change what a face means from shot to shot. Absent = a project with no
   * character sheets (every existing document).
   */
  characters?: DirectorCharacter[];
  /** May the planner propose Prompt Relay beats? Absent = on, as every pre-beats document is. */
  beats_enabled?: boolean;
  /**
   * Project default for dropping the reference-contaminated first frame of a t2v shot. Absent =
   * `"auto"`, which is what a pre-existing document gets and what a new project starts on — the
   * engine self-gates it off every shot that cannot be affected, so it costs nothing to leave on.
   */
  frame_drop?: LtxFrameDrop;
  /** Unattended dailies review after a full render. Absent = off. */
  critique?: DirectorCritiqueSettings;
  /** Cached word-level lyric alignment for the audio track. */
  lyrics?: LyricWord[];
  /** Compiled on save/run — what the backend actually renders. Shape follows `engine`. */
  request: DirectorRequest;
  assets: DirectorAssets;
  /** Opaque editor UI state (zoom, scroll, pixels-per-second, selection). */
  editor?: Record<string, unknown>;
}

/** List entry (omits the heavy `timeline` blob). */
export interface DirectorProjectSummary {
  id: string;
  name: string;
  metadata: Record<string, unknown>;
  created_at: string;
  updated_at: string;
  /**
   * Optimistic-concurrency token. Every read hands one back; a full-document PUT must quote the one
   * it read or the server rejects it with 409 rather than letting a stale autosave revert the doc.
   */
  version: number;
}

/** A full saved project. */
export interface DirectorProject extends DirectorProjectSummary {
  timeline: DirectorTimeline;
}

export async function listDirectorProjects(
  limit = 100,
  offset = 0
): Promise<{ items: DirectorProjectSummary[]; total: number }> {
  return request(`/api/v1/ltx-director/projects?limit=${limit}&offset=${offset}`);
}

export function getDirectorProject(id: string): Promise<DirectorProject> {
  return request(`/api/v1/ltx-director/projects/${encodeURIComponent(id)}`);
}

export function createDirectorProject(
  name: string,
  timeline: DirectorTimeline,
  metadata?: Record<string, unknown>
): Promise<DirectorProject> {
  return request('/api/v1/ltx-director/projects', {
    method: 'POST',
    body: JSON.stringify({ name, timeline, metadata: metadata ?? {} })
  });
}

/**
 * Replace the whole document. `version` is the token the caller last read: the write applies only
 * if the row is still on it, so a stale tab's autosave can no longer revert somebody else's shots
 * (and resume ids) wholesale. A mismatch throws `ApiError` 409 — read the live version back out
 * with {@link conflictVersion}.
 */
export function updateDirectorProject(
  id: string,
  name: string,
  timeline: DirectorTimeline,
  metadata: Record<string, unknown> | undefined,
  version: number
): Promise<DirectorProject> {
  return request(`/api/v1/ltx-director/projects/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body: JSON.stringify({ name, timeline, metadata: metadata ?? {}, version })
  });
}

/**
 * Rename / re-tag a project without resending its timeline — a saved timeline carries inline
 * assets, so a PUT to change the name alone ships megabytes. `metadata` is shallow-merged, and the
 * reply is the summary (no timeline), so the rename is small in both directions.
 *
 * Quotes no version: it cannot touch the timeline, so it has no document to revert. It does bump
 * the version, and the caller must adopt the one it returns — that is what keeps a pending timeline
 * autosave on the SAME client from conflicting with the rename it just made.
 */
export function patchDirectorProject(
  id: string,
  patch: { name?: string; metadata?: Record<string, unknown> }
): Promise<DirectorProjectSummary> {
  return request(`/api/v1/ltx-director/projects/${encodeURIComponent(id)}`, {
    method: 'PATCH',
    body: JSON.stringify(patch)
  });
}

/** The live version off a 409 from {@link updateDirectorProject}; null for any other failure. */
export function conflictVersion(e: unknown): number | null {
  if (!(e instanceof ApiError) || e.status !== 409) return null;
  const v = (e.body as { current_version?: unknown } | undefined)?.current_version;
  return typeof v === 'number' ? v : null;
}

export async function deleteDirectorProject(id: string): Promise<void> {
  const response = await authedFetch(`/api/v1/ltx-director/projects/${encodeURIComponent(id)}`, {
    method: 'DELETE'
  });
  if (!response.ok && response.status !== 404) {
    throw new Error(`delete failed: ${response.status}`);
  }
}

/** One durable render clip persisted for a project (survives the job store's 1h sweep). */
export interface DirectorRenderItem {
  /** Which render stream: 'proxy' (cheap preview, with audio) or 'final' (full res). */
  tier: 'proxy' | 'final';
  /** -1 = stitched final; >=0 = prompt-filtered shot index (the Nth shot WITH a prompt). */
  shot_index: number;
  /** The shot prompt as rendered ("" for the stitched final). Lets the editor flag stale shots. */
  prompt: string;
  content_type: string;
  created_at: string;
  /** koblem URL to fetch the clip bytes (via fetchLtxClip). */
  url: string;
  /** Which take this is (0 = the first roll of that shot). */
  variant?: number;
  /** How many takes that shot has. > 1 means there is something to choose between. */
  takes?: number;
  /** The seed this take was rendered with, when known. */
  seed?: number | null;
  /** The vision critic's verdict on this take, once judged. */
  critic?: DirectorCritic | null;
  /**
   * The engine chain THIS take was banked out of. Per row, not per project: a continue banks only
   * the shots it rendered, under its own fresh job id, so a chain of continues leaves the tier's
   * shots spread over several banks. `lib/director/renders.ts:bankedResumeTarget` picks the one a
   * `resume_job_id` may name. Null on a row written before the column existed.
   */
  latent_job_id?: string | null;
  /**
   * Which engine's store `latent_job_id` addresses. The two LTX deployments keep separate job
   * stores, and a project's engine is a picker that can move after it has rendered — so without
   * this the editor re-adopts a chain the engine it is now on has never heard of. Null on a row
   * migration `0039` could not place, and on a rebuilt cut, which names no chain at all.
   */
  engine?: string | null;
  /**
   * The render that produced this clip. A client that watched that render already holds the
   * identical bytes, so this is what lets `planHydration` re-read the durable rows after a
   * completion without downloading the whole render set again.
   */
  source_job_id?: string | null;
}

/// A project's persisted renders (stitched final + per-shot), for rehydrating playback/status on
/// load — these are copied out of the TTL'd job store so they outlive it.
export async function listDirectorRenders(id: string): Promise<DirectorRenderItem[]> {
  const r = await request<{ items: DirectorRenderItem[] }>(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/renders`
  );
  return r.items ?? [];
}

/** One render in flight for a project, from whichever device started it. */
export interface DirectorActiveJob {
  job_id: string;
  project_id: string;
  /** Which stream it feeds: 'proxy' (cheap preview) or 'final'. */
  tier: 'proxy' | 'final';
  status: LtxJobState;
  /**
   * The render is over and only its persistence into the durable rows is outstanding. A job stays
   * listed here until that lands, so "no active jobs" is the signal that `/renders` is whole —
   * a full run's harvest replaces the tier's entire set, and reading it mid-swap shows a
   * half-populated project.
   */
  harvesting?: boolean;
  /** Shots this render covers (prompt-filtered). */
  segments: number;
  /** Shots delivered so far. */
  completed_segments: number;
  /** The prompts as submitted, prompt-filtered — the render's own provenance. */
  scripts: string[];
  server_job_id?: string | null;
  seed?: number | null;
  partials: { segment_index: number; url: string }[];
  /**
   * This render's `/media` is a FRAGMENT of the cut — one render unit, or (when the engine's
   * concatenation failed) segment 0 alone.
   *
   * 🔴 The only way an adopting client can know. A tab that dispatched the render predicts this
   * from the engine's capabilities; a tab that reloaded, or a second device, predicted nothing —
   * so without it the adoption starts out believing the render is the whole cut and would file a
   * fifteen-second retake over a four-minute one. Server-side it is `delivers_fragment()`, the
   * same expression the job status body answers with, because it is the same question.
   */
  job_is_partial?: boolean;
  created_at?: string | null;
}

/// What is rendering for this project RIGHT NOW, regardless of which device started it. Derived
/// from the in-memory job store (the sweep only drops terminal jobs), so it is never stale.
export async function listDirectorActiveJobs(id: string): Promise<DirectorActiveJob[]> {
  const r = await request<{ items: DirectorActiveJob[] }>(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/active`
  );
  return r.items ?? [];
}

/// Why one generation needs rendering again. See the Rust `H3Staleness`.
export type H3Staleness =
  'never_rendered' | 'prompt_changed' | 'continues_from_stale' | 'inside_the_span';

/// What a run would do, and what a full one would destroy. `supported: false` on LTX, which has its
/// own resume path — callers fall back to plain "render everything" there.
export interface DirectorRenderPlan {
  supported: boolean;
  engine: string;
  tier: 'proxy' | 'final';
  /** One entry per generation in render order; `null` = has a current take and would be skipped. */
  generations?: (H3Staleness | null)[];
  total?: number;
  /** Inclusive generation range a `stale` run renders; absent when nothing needs it. */
  span?: { first: number; last: number } | null;
  /** How many generations a `stale` run renders — 0 means everything is up to date. */
  continue_count?: number;
  /** Takes an `all` run DELETES. Every take of this tier, not merely the selected ones. */
  existing_takes?: number;
}

/// What a run of this project would render right now. Read-only — nothing is dispatched.
export function getDirectorRenderPlan(
  id: string,
  tier: 'proxy' | 'final' = 'final'
): Promise<DirectorRenderPlan> {
  return request(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/render-plan?tier=${encodeURIComponent(tier)}`
  );
}

/// Run a saved project in one of its two tiers ('final' by default, 'proxy' for a cheap preview).
/// Returns a koblem `job_id` in the shared ltx job store; poll with getLtxJob() and fetch the clip
/// via fetchLtxClip(result_url).
///
/// 🔴 `scope` decides whether this DESTROYS the tier's existing takes. `all` (the default, and what
/// `/run` has always meant) re-renders the timeline and replaces every take. `stale` renders only
/// what `getDirectorRenderPlan` reports and keeps every take — H3 projects only. Defaulting to
/// `all` is deliberate: existing callers must keep getting a full fresh render.
export function runDirectorProject(
  id: string,
  tier: 'proxy' | 'final' = 'final',
  scope: 'all' | 'stale' = 'all',
  queue?: QueueSubmitOptions
): Promise<{
  job_id?: string;
  status: LtxJobState | 'up_to_date';
  tier: 'proxy' | 'final';
  /** Generations this job renders. Present on H3. */
  rendered?: number;
  /** Project generation index its first partial is filed under. Present on H3. */
  shot_offset?: number;
}> {
  return request(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/run?tier=${encodeURIComponent(tier)}&scope=${scope}`,
    { method: 'POST', headers: queueHeaders(queue) }
  );
}

export interface DirectorRetakePayload {
  /** RAW index into the saved timeline's segments — the shot to re-render. */
  segment: number;
  /** Override the shot's prompt (absent ⇒ keep saved). */
  prompt?: string;
  /** New chain seed; -1 = random. Pass a fresh value for a different take. */
  seed?: number;
  /** New scene image (base64); "" clears it back to a continuation; absent ⇒ untouched. */
  image_b64?: string;
  /** Banked render to retake from; absent ⇒ derived from the project's last render. */
  resume_job_id?: string;
  /** Persist the overrides back into the saved timeline (default true). */
  persist?: boolean;
  /** Which stream to re-roll: 'final' (default) or 'proxy', the cheap preview tier. */
  tier?: 'proxy' | 'final';
}

/// Re-render ONE shot of a saved, previously-rendered project, reusing the banked prefix.
/// Returns a koblem `job_id`; poll with getLtxJob(). Works for any shot: the engine re-renders
/// only this shot and splices the banked tail back, end-pinned for a seamless seam.
export function retakeDirectorSegment(
  id: string,
  payload: DirectorRetakePayload,
  queue?: QueueSubmitOptions
): Promise<{ job_id: string; status: LtxJobState; version: number }> {
  return request(`/api/v1/ltx-director/projects/${encodeURIComponent(id)}/retake`, {
    method: 'POST',
    headers: queueHeaders(queue),
    body: JSON.stringify(payload)
  });
}

/**
 * One TAKE of one shot: a single roll, kept alongside its siblings. Retakes accumulate instead of
 * overwriting, so an overnight run leaves several options and the pick is made afterwards.
 */
export interface DirectorTake {
  /** Ordinal within the shot (0 = first take). */
  variant: number;
  /** True for the take the timeline plays and a rebuilt cut uses. Exactly one per shot. */
  selected: boolean;
  /** The shot prompt as rendered for this take. */
  prompt: string;
  /** The seed this take was rendered with, when known. */
  seed?: number | null;
  /** The vision critic's verdict, once judged. */
  critic?: DirectorCritic | null;
  content_type: string;
  created_at: string;
  /** koblem URL for this take's clip. */
  url: string;
  /**
   * The render that produced this take — and what `take-continuity.ts` reconstructs the JOIN from.
   *
   * 🔴 A take does not record the pin it rendered against; only its own. A full run writes one take
   * per shot all sharing this id (and wipes the tier's other takes as it lands), so two takes
   * sharing it came out of one submitted chain — where `continuation.from: "prev"` could only have
   * meant each other. That is what makes "shot 4 was rendered from take 1 of shot 3" computable
   * rather than guessed. A RETAKE's pin is not recorded anywhere per-take.
   *
   * Absent on rows written before takes existed.
   */
  source_job_id?: string | null;
  /**
   * H3 only: this take's own latent-bank entry, `"<engine job id>:<generation index>"` — what the
   * NEXT generation's `continuation.from` pins to. Null on LTX takes and on the stitched cut.
   */
  bank_key?: string | null;
}

/** The vision critic's verdict on one take: what it saw versus what the shot asked for. */
export interface DirectorCritic {
  /** 0..1 — 1 = the brief is fully on screen. */
  score: number;
  verdict: 'pass' | 'fail';
  note?: string | null;
  /** What the brief asked for that isn't in the footage. */
  missing?: string[] | null;
  /** Per-beat findings, when the shot had Prompt Relay beats. */
  beats?: { text: string; happened: boolean; at?: number | null }[] | null;
  threshold?: number;
  at?: string;
}

/** Every take of one shot, oldest first. `shot` is a shot index or "final". */
export async function listDirectorTakes(
  id: string,
  tier: 'proxy' | 'final',
  shot: number | 'final'
): Promise<DirectorTake[]> {
  const r = await request<{ items: DirectorTake[] }>(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/renders/${tier}/${shot}/takes`
  );
  return r.items ?? [];
}

/**
 * Freeze one frame of a rendered shot into the asset store and get back its `asset:<hash>`.
 *
 * This is how a character INTRODUCED in a text-to-video shot acquires a reference: there is no init
 * image to point at, so the shot that establishes the look becomes the reference after it has been
 * rendered. Extraction is server-side — the bytes are already there, and ffmpeg keeps the exact
 * decoded pixels where a browser canvas grab of a 10-bit clip would tone-map them to 8-bit first.
 *
 * `seconds` is a timecode within the take (default 0.5s). `variant` picks a specific take; omitted,
 * it uses the selected one. Content-addressed, so promoting the same frame twice writes nothing.
 *
 * The frame goes through the reference-preparation pipeline by DEFAULT (server-side `prepare`,
 * default true), because a raw video frame is close to the worst input for a reference: unmatted,
 * so it carries its own background, and a subject in a rendered shot almost always touches a frame
 * edge. **That makes a 422 reachable here** — it means this FRAME cannot serve as a reference and
 * the message names the offending edge. Show it and pick a different moment; retrying is pointless.
 * Pass `width`/`height` so the letterbox matches the project's render aspect rather than the
 * server's 1280×704 fallback.
 */
export async function promoteRenderStill(
  id: string,
  tier: 'proxy' | 'final',
  shot: number | 'final',
  body: {
    seconds?: number;
    variant?: number;
    /** Run the preparation pipeline. Omitted → true; `false` stores the raw frame. */
    prepare?: boolean;
    width?: number;
    height?: number;
  } = {}
): Promise<{
  ref: string;
  hash: string;
  bytes: number;
  width: number;
  height: number;
  seconds: number;
  variant: number;
  /** Whether the pipeline ran (echoes the request's `prepare`). */
  prepared?: boolean;
  /** Per-edge subject-alpha fractions when it did, for explaining a near-miss. */
  edges?: PreparedReference['edges'];
}> {
  return request(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/renders/${tier}/${shot}/still`,
    { method: 'POST', body: JSON.stringify(body) }
  );
}

/** Choose which take of a shot is the one. Call rebuildDirectorCut() to re-derive the stitched cut. */
export function selectDirectorTake(
  id: string,
  tier: 'proxy' | 'final',
  shot: number | 'final',
  variant: number
): Promise<{ ok: boolean; variant: number }> {
  return request(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/renders/${tier}/${shot}/select`,
    { method: 'POST', body: JSON.stringify({ variant }) }
  );
}

/**
 * Re-derive the stitched cut by joining the SELECTED take of every shot (stream-copy concat, so no
 * re-encode and no generation loss). This does not re-render: picking an older take cannot rewind
 * the engine's banked latents.
 */
export function rebuildDirectorCut(
  id: string,
  tier: 'proxy' | 'final'
): Promise<{ ok: boolean; shots: number; variant: number; url: string }> {
  return request(
    `/api/v1/ltx-director/projects/${encodeURIComponent(id)}/renders/${tier}/rebuild`,
    { method: 'POST' }
  );
}

/** Body for the vision critic. Everything is optional; the defaults judge the proxy tier read-only. */
export interface DirectorCritiquePayload {
  tier?: 'proxy' | 'final';
  /** Shots to judge (prompt-filtered indices). Empty/absent = all of them. */
  shots?: number[];
  /** Re-roll a failed shot as a new take. Off by default — this spends GPU time. */
  auto_retake?: boolean;
  /** Most extra takes to spend on one shot (default 2). */
  max_retakes?: number;
  /** Score below which a take fails (default 0.6). */
  threshold?: number;
  /** Re-judge takes that already have a verdict (default false). */
  rejudge?: boolean;
}

/**
 * Watch the rendered footage with the vision LLM and judge it against each shot's prompt (and its
 * beats). Returns 202 — verdicts appear on the takes as it works, so poll listDirectorTakes().
 */
export function critiqueDirectorProject(
  id: string,
  payload: DirectorCritiquePayload = {}
): Promise<{ status: string; tier: string }> {
  return request(`/api/v1/ltx-director/projects/${encodeURIComponent(id)}/critique`, {
    method: 'POST',
    body: JSON.stringify(payload)
  });
}

/** Reference image handed to the auto-director (VLM studies it; cited shots render i2v from it). */
export interface DirectorRefImage {
  /** Bare or data-URI base64. */
  image_b64: string;
  label?: string;
}

/** Body for the LLM auto-director. Only `brief` (or a ref image) is required. */
export interface DirectorPlanRequest {
  brief: string;
  fps?: number;
  width?: number;
  height?: number;
  /** Base DiT variant; the auto-director never picks the lipdub variant (native AV, not relip). */
  model?: string;
  /**
   * Project runtime LoRAs, passed straight through to the plan's compiled `request.loras`.
   *
   * Sent for the same reason `model` is: `adoptTimeline` rebuilds the editor's state FROM the
   * returned doc, so a field the plan request omits comes back empty and silently wipes what the
   * user had set before they hit auto-direct. The planner itself never chooses these.
   */
  loras?: LtxRuntimeLora[];
  audio_kind?: 'none' | 'voice' | 'bgm' | 'mixed';
  /** Song/voice track (base64). When set and `lyrics` is empty, koblem word-aligns it via STT. */
  audio_b64?: string;
  /** Pre-aligned word timings (skip STT if already transcribed). */
  lyrics?: LyricWord[];
  ref_images?: DirectorRefImage[];
  /** No-lyrics path only: distributes shot durations to this total. */
  target_seconds?: number;
  /** Shot-count cap (default 16, clamped 1..64). Long videos are planned in chunks. */
  max_shots?: number;
  language?: string;
  /** Project canon (backstory / characters / tone / rules) grounding every shot. */
  story?: StoryContext;
  /**
   * The recurring cast whose appearance sentences must come back verbatim. Absent/empty ⇒
   * byte-identical to before. See `LtxCastMember`.
   */
  cast?: LtxCastMember[];
  /** May the planner place Prompt Relay beats inside shots? Omit/true = yes. false = every shot is
   * a single continuous action, for a project that wants plain one-prompt shots. */
  beats?: boolean;
  /**
   * Which engine this plan is for — `'ltx'` (default) or `'h3'`. It changes the planner's JOB, not
   * just its wording: LTX composes continuations out of a carried final frame, H3 composes CUTS
   * (shots share no latent state and each generates its own soundtrack from noise), so boundaries
   * go on natural audio/action breaks and every shot carries its own audio intent.
   *
   * Also stamped onto the returned document. Without it a plan comes back reading as LTX and, since
   * the editor rebuilds its whole state from that document, accepting it would move the project
   * onto the other engine.
   */
  engine?: DirectorEngine;
  /** H3 only: the project's omni references, in order, so a shot's idea can name the ones it is
   *  about. Briefs only — the planner is never sent the pixels, and the editor re-attaches the real
   *  roster on commit exactly as it does for `characters`. */
  h3_references?: H3PromptReference[];
}

/**
 * LLM auto-director, STAGE 1 of two: compose a DirectorTimeline's STRUCTURE from a brief +
 * optional song (lyrics with word timings) + reference images. Shot cuts snap to lyric onsets so
 * the visuals track what's sung/said when. Dialogue/singing uses native LTX joint-AV (no lipdub).
 *
 * Each planned clip comes back with an `idea` (a light one-liner to yes/no at a glance), its
 * duration, mode, scene image and `new_scene` flag — but an EMPTY `prompt`. Stage 2 is the
 * per-shot Draft path, which expands each idea into a full prompt with that shot's own length and
 * continuation context in hand. `summary` is a prose synopsis of the whole planned story for the
 * user to review before any prompt is drafted (absent if the synopsis call failed).
 */
export async function planDirectorTimeline(
  body: DirectorPlanRequest
): Promise<{ timeline: DirectorTimeline; summary?: string }> {
  return request('/api/v1/ltx-director/plan', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/** One shot for the summariser (editor fields only — no inline assets). */
export interface DirectorSummaryShot {
  idea?: string;
  prompt?: string;
  mode?: DirectorClipMode;
  seconds?: number;
}

/// Read the whole shot list back as a short prose synopsis of the video it
/// describes. One LLM call, grounded in the project canon (story) when supplied.
export async function summarizeDirectorShots(body: {
  shots: DirectorSummaryShot[];
  story?: StoryContext;
}): Promise<{ summary: string }> {
  return request('/api/v1/ltx-director/summarize', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/**
 * One shot fed to rework/review/gist/chat (editor text fields, no assets).
 *
 * `id` is the clip's stable id and is REQUIRED: every per-shot LLM result comes back keyed to it,
 * never to a position. The backend hands the model a short token derived from this id, and maps
 * whatever it echoes back to this exact value — an id it can't map is dropped, so a shot sent
 * without one can never receive a result.
 */
export interface DirectorEditorShot {
  id: string;
  idea?: string;
  prompt?: string;
  mode?: DirectorClipMode;
  seconds?: number;
  gist?: string;
  /** Hard cut — drives whether the PREVIOUS shot is told to end on a continuation point. */
  new_scene?: boolean;
}

/** One rewritten shot from a whole-script rework, keyed to the clip `id` it belongs to. */
export interface DirectorReworkedShot {
  id: string;
  /** @deprecated on H3 — the legacy carrier, kept for a koblem that predates `h3_draft`. */
  prompt: string;
  gist?: string;
  /** H3 only: the structured draft for this shot's GENERATION. See {@link H3PromptDraft}. */
  h3_draft?: H3PromptDraft;
}

/// Apply one director-level change across the WHOLE script — rewrites each shot's prompt (+gist)
/// in place, preserving shot count/order. Each rewrite is keyed to its clip `id`. `warnings` lists
/// rewrites the backend dropped (an id the model invented or repeated) — those shots are untouched.
export async function reworkDirectorScript(body: {
  shots: DirectorEditorShot[];
  instruction: string;
  story?: StoryContext;
  cast?: LtxCastMember[];
  /** Which engine's conventions to rewrite INTO. A rework touches every prompt in the project at
   *  once, so an engine mismatch here converts a whole script in a single call. */
  engine?: DirectorEngine;
  h3_references?: H3PromptReference[];
  /**
   * H3 only: one vocal lane per GENERATION, render order. A rework rewrites every block in the
   * project in a single call, so without this it is the fastest way to put a mimed vocal back into
   * a generation somebody had already corrected. See {@link H3VocalLane}.
   */
  h3_vocal_lanes?: H3VocalLane[];
}): Promise<{ shots: DirectorReworkedShot[]; warnings?: string[] }> {
  return request('/api/v1/ltx-director/rework', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Batch-write a one-sentence plain-language gist for many shots in ONE LLM call. Each gist is
/// keyed to its clip `id` (only non-empty gists are returned); `warnings` lists dropped entries.
export async function gistDirectorShots(body: {
  shots: DirectorEditorShot[];
  story?: StoryContext;
}): Promise<{ gists: { id: string; gist: string }[]; warnings?: string[] }> {
  return request('/api/v1/ltx-director/gist-shots', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/** A newly-written shot to append when extending the story. */
export interface DirectorExtendShot {
  idea?: string;
  /** @deprecated on H3 — the legacy carrier, kept for a koblem that predates `h3_draft`. */
  prompt: string;
  gist?: string;
  seconds?: number;
  /** Hard cut: this new shot opens a fresh scene rather than continuing the previous shot. */
  new_scene?: boolean;
  /** H3 only: the structured draft for the GENERATION this new shot lands in. */
  h3_draft?: H3PromptDraft;
}

/// Continue the story: write `count` NEW shots that pick up where the timeline ends, consistent
/// with the shots so far + canon. Returns shots to append (they are not shown the whole doc).
export async function extendDirectorScript(body: {
  shots: DirectorEditorShot[];
  count?: number;
  direction?: string;
  story?: StoryContext;
  cast?: LtxCastMember[];
  engine?: DirectorEngine;
  h3_references?: H3PromptReference[];
}): Promise<{ shots: DirectorExtendShot[] }> {
  return request('/api/v1/ltx-director/extend', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Name the project from its brief + shots (a short, catchy title). Used to auto-title untitled
/// projects.
export async function titleDirectorProject(body: {
  shots: DirectorEditorShot[];
  brief?: string;
  story?: StoryContext;
}): Promise<{ title: string }> {
  return request('/api/v1/ltx-director/title', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/** Insert an `add_shot` at the very head of the timeline. Reserved sentinel — `@` never appears in
 * a shot id, so it cannot collide with one. */
export const DIRECTOR_ADD_AT_START = '@start';

/** One edit the chat assistant wants applied to the timeline. `shot` is a clip `id`, already
 * resolved by the backend (an action naming an unknown shot never reaches the client). `after` is a
 * clip `id`, `DIRECTOR_ADD_AT_START`, or absent to append. The client applies these behind the
 * revert net. */
export interface DirectorChatAction {
  type:
    | 'set_prompt'
    | 'set_gist'
    | 'set_idea'
    | 'set_seconds'
    | 'add_shot'
    | 'delete_shot'
    | 'set_story';
  shot?: string;
  after?: string;
  field?: 'backstory' | 'tone' | 'rules';
  value?: string | number;
  idea?: string;
  prompt?: string;
}

/// Context-grounded director chat: the full shot list + story (+ reference images) are sent every
/// turn so the model is always grounded. Returns a conversational `reply` and edit `actions` the
/// client applies to the timeline; `warnings` lists actions the backend dropped.
export async function directorChat(body: {
  messages: { role: 'user' | 'assistant'; content: string }[];
  shots: DirectorEditorShot[];
  story?: StoryContext;
  reference_images?: string[];
  /** A chat turn can rewrite a prompt, so it needs the same conventions the batch writers get. */
  engine?: DirectorEngine;
  h3_references?: H3PromptReference[];
  /** The project's cast, so a chat turn can name a character the same way the batch writers do. */
  cast?: LtxCastMember[];
  /**
   * The H3 packing, so the model sees the real blocks rather than a flat list of beat bodies.
   *
   * 🔴 Without this the server's H3 chat grounding is INERT — it has the branch and nothing to feed
   * it, so chat answers about a prompt it has never seen. Same spread `/review` uses.
   *
   * 🔴 Every brief's `id` is what moves the turn onto the GENERATION id space and lets chat rewrite
   * a block instead of only reading one — see {@link DirectorH3GenerationBrief.id} and `target`
   * below. It is all-or-nothing per request, server-side.
   */
  h3_generations?: DirectorH3GenerationBrief[];
}): Promise<{
  reply: string;
  actions: DirectorChatAction[];
  warnings?: string[];
  /**
   * What every `shot` / `after` above is an id OF.
   *
   * 🔴 `'generation'` ⇒ `DirectorH3Generation.id`, and each action means the generation-level thing:
   * `set_prompt` is that render's whole block, `set_gist` its display label, `set_seconds` its total
   * length, `add_shot`/`delete_shot` a whole render. **Absent ⇒ `DirectorClip.id`** — every LTX turn,
   * and every H3 turn whose briefs carried no ids.
   *
   * ⚠️ A value this client does not recognise must be REFUSED, not applied. Resolving a generation
   * id against the clip list happens to find nothing today, so applying is a silent no-op rather
   * than a wrong write — but that is an accident of two id spaces not colliding, not a guarantee.
   */
  target?: 'generation';
}> {
  return request('/api/v1/ltx-director/chat', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/** A script-supervisor finding. `shot` is the clip `id` it applies to, or null for a whole-story
 * note. A finding whose id couldn't be mapped is dropped rather than pinned on a guessed shot. */
export interface DirectorReviewFinding {
  shot: string | null;
  severity: 'high' | 'medium' | 'low';
  issue: string;
}

/**
 * One packed generation, as the script supervisor has to see it.
 *
 * 🔴 **The block, not the fields.** `DirectorH3Generation.prompt` is the artifact that renders, and
 * every schema question a reviewer can usefully ask — is this cut time inside the duration, is this
 * `<Subject 2>` ever defined, does this label have a retention line — is answerable only from those
 * bytes. Sending the per-section fields instead would hand the reviewer a second copy that can
 * disagree with the one that renders.
 *
 * ⚠️ `shots` is CUT ids (`DirectorClip.id`), because a finding becomes a one-click rework of one
 * clip. A brief with a generation id in it would produce findings with nothing to fix.
 */
export interface DirectorH3GenerationBrief {
  /**
   * The generation's own stable id (`DirectorH3Generation.id`) — **what a chat action addresses.**
   *
   * 🔴 **Required, and the requirement is the point.** The server moves the whole turn onto the
   * generation id space only when EVERY brief carries an id (`chat_generation_ids`); one missing id
   * silently drops the turn back to the cut-id contract, where `set_prompt` lands on
   * `DirectorClip.prompt` — a seed the compiler reads only for a generation that has no block yet.
   * Typing it non-optional is what stops a caller half-filling it.
   *
   * ⚠️ `/review` ignores it (and {@link gist}): a finding is a one-click rework of one CLIP, so that
   * path keeps naming cuts. Two surfaces, two id spaces, one brief.
   */
  id: string;
  /**
   * The author's one-line label for this render (`DirectorH3Generation.gist`).
   *
   * ⚠️ **Display only — never compiled into a request**, on either side of the wire. It is here so a
   * chat turn can recognise a render in a list of eight without reading 7,000 characters of schema,
   * and so `set_gist` has something to rewrite. It must never leak into a block: the schema's own
   * `summary:` section is a different thing that DOES render, and lives inside {@link prompt}.
   */
  gist?: string;
  /** The assembled block, verbatim. Omitted ⇒ nothing has been written or composed for it. */
  prompt?: string;
  /** What it renders for, AFTER the frame-grid snap — the window every cut time must fall inside. */
  seconds: number;
  task: H3Task;
  /** The clips packed into this render, in cut order, by id. */
  shots: string[];
  /** False ⇒ `prompt` is koblem's own seed, not somebody's work. See the reviewer's NOT AUTHORED. */
  authored: boolean;
}

/// Script-supervisor pass: flags continuity breaks, weak shots and pacing issues as actionable
/// findings tied to a shot (or the whole story). Empty `findings` ⇒ the script is clean.
export async function reviewDirectorShots(body: {
  shots: DirectorEditorShot[];
  story?: StoryContext;
  /** The review's own subject changes with the engine: on LTX it hunts for a shot that CONTRADICTS
   *  the frame it renders out of, on H3 for one that DEPENDS on a frame that will not be there. */
  engine?: DirectorEngine;
  /**
   * H3 only: the timeline packed into renders, each carrying its block.
   *
   * 🔴 Omitting it does not merely weaken the review — the backend then tells the reviewer the
   * blocks are absent and stops asking for schema findings at all, because a model asked to find a
   * cut-time error in a listing with no cut times in it can only invent one. `shots` alone is one
   * line per cut and cannot answer a single one of those checks.
   */
  h3_generations?: DirectorH3GenerationBrief[];
}): Promise<{ findings: DirectorReviewFinding[]; warnings?: string[] }> {
  return request('/api/v1/ltx-director/review', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Word-align an audio track for the lyric lane. Returns words (and segments)
/// with start/end seconds so the editor can snap cuts/keyframes to lyric onsets.
export async function transcribeDirectorAudio(
  file: File | Blob,
  opts?: { language?: string; granularity?: 'word' | 'segment' | 'char' }
): Promise<{
  text: string;
  words: LyricWord[];
  segments: LyricWord[];
  language?: string;
}> {
  const form = new FormData();
  form.append('file', file, 'audio.wav');
  form.append('granularity', opts?.granularity ?? 'word');
  if (opts?.language) form.append('language', opts.language);
  const response = await authedFetch('/api/v1/ltx-director/transcribe', {
    method: 'POST',
    body: form
  });
  if (!response.ok) {
    const text = await response.text().catch(() => '');
    throw new Error(`transcribe failed: ${response.status} ${text}`);
  }
  return response.json();
}

// ── LTX-2B (fast, video-only 2B distilled toy) ────────────────────────────────
// Same request/segment/keyframe/job/props shapes as ltx-video, but the 2B service is a
// SINGLE video-only model: the request omits `model`, `audio_source`, and `upscaler_ratio`,
// and there are no top-level image/audio/bgm parts — only per-shot scene images
// `seg_image_<i>` (raw segment index) for per-shot i2v / scene changes.

export function getLtx2bProps(): Promise<LtxVideoProps> {
  return request('/api/v1/ltx-2b/props');
}

/// Submit a 2B chain render. Returns immediately (HTTP 202) with a queue `job_id`, so a
/// disconnect/navigation can't drop it. Poll `getLtx2bJob(job_id)` for status and fetch the clip
/// from `result_url` when done.
export async function submitLtx2b(
  req: LtxVideoGenerateRequest,
  segImages?: (File | null | undefined)[],
  queue?: QueueSubmitOptions
): Promise<EnqueueResponse> {
  const hasSegImages = !!segImages?.some((f) => !!f);
  if (!hasSegImages) {
    return enqueueJson('ltx-2b', '/api/v1/ltx-2b/generate', req, queue);
  }
  const form = new FormData();
  form.append('request', JSON.stringify(req));
  // Per-shot scene images, keyed by RAW segment index so the server matches them to shots.
  segImages?.forEach((f, i) => {
    if (f) form.append(`seg_image_${i}`, f);
  });
  return enqueueForm('ltx-2b', '/api/v1/ltx-2b/generate', form, queue);
}

/// Poll one LTX-2B job's status.
export function getLtx2bJob(jobId: string): Promise<LtxJobStatus> {
  return request(`/api/v1/ltx-2b/jobs/${encodeURIComponent(jobId)}`);
}

/// The last 50 LTX-2B rows on the queue, newest first.
export function listLtx2bJobs(): Promise<{ jobs: LtxJobStatus[] }> {
  return request('/api/v1/ltx-2b/jobs');
}

/// Cancel the in-flight LTX-2B render.
export function cancelLtx2bRender(): Promise<unknown> {
  return request('/api/v1/ltx-2b/cancel', { method: 'POST' });
}

// ── LTX lip-dub / relip (attach drive-audio to a video → re-sync mouths) ──────

export interface LtxRelipRequest {
  /** Optional mouth-style / scene prompt for the relip pass. */
  prompt?: string;
  /** Audio→video guidance. 1 = distilled default; the owner pushes up to ~3. */
  a2v_guidance?: number;
  /**
   * Two-stage (latent upsampler between two samplers).
   *
   * 🔴 An identity switch rather than a quality dial: guides are cropped before the upsampler, so
   * with it on the frozen source conditions stage 1 only and the stage-2 refine has nothing holding
   * the face being dubbed.
   */
  two_stage?: boolean;
  width?: number;
  height?: number;
  fps?: number;
  seed?: number;
  save_clip?: boolean;
  /**
   * Strength of the DubIt IC-LoRA. Absent → 1.0, its trained scale.
   *
   * The adapter was trained against LTX-2.3 and its keys map onto the 2.5 DiT, which proves
   * nothing about whether it still means the same thing there — this is the knob for settling that
   * by eye.
   */
  lipdub_strength?: number;
}

/// Submit a relip / lip-dub: attach a drive-audio track to a source video → re-sync mouths.
/// Multipart with the source `video` + drive `audio`. Returns 202 + a koblem `job_id`; poll
/// it with `getLtxJob(job_id)` and fetch the clip from `result_url` (same job store as render).
///
/// No engine argument, unlike {@link submitLtxVideo}: relip's wire form is LTX-2.5's, so koblem
/// renders it there and files the job under `ltx25` without being asked.
export async function submitLtxRelip(
  req: LtxRelipRequest,
  video: File,
  audio: File,
  queue?: QueueSubmitOptions
): Promise<{ job_id: string; status: LtxJobState }> {
  const form = new FormData();
  form.append('request', JSON.stringify(req));
  form.append('video', video);
  form.append('audio', audio);
  const response = await authedFetch('/api/v1/ltx-video/relip', {
    method: 'POST',
    headers: queueHeaders(queue),
    body: form
  });
  if (!response.ok) {
    const text = await response.text().catch(() => '');
    throw new Error(`ltx relip submit failed: ${response.status} ${text}`);
  }
  return response.json();
}

// ── Wan2.2-VACE video (T2V / I2V / continuation) ─────────────────────────────

export type WanVaceMode = 't2v' | 'i2v' | 'continuation';

export type WanImageSource = { kind: 'none' } | { kind: 'upload' } | { kind: 'url'; url: string };

export interface WanSegment {
  prompt: string;
  seconds?: number;
}

export interface WanVaceGenerateRequest {
  mode?: WanVaceMode;
  segments: WanSegment[];
  init_source?: WanImageSource;
  reference_source?: WanImageSource;
  negative_prompt?: string;
  width?: number;
  height?: number;
  fps?: number;
  frames?: number;
  seed?: number;
  quant?: string;
  save_clip?: boolean;
  /** Resume a failed/cancelled render from its banked segments (the server job id). */
  resume_job_id?: string;
}

export interface WanVaceProps {
  healthy: boolean;
  default_frames: number;
  default_fps: number;
  default_resolution: string;
  default_quant: string;
  last_render_sec?: number;
}

export interface WanScriptFromLlmResponse {
  segments: string[];
}

export type WanJobState = 'queued' | 'rendering' | 'completed' | 'failed' | 'cancelled';

/** Status of an async Wan render (poll `…/jobs/{id}`). */
export interface WanJobStatus {
  job_id: string;
  status: WanJobState;
  /** RFC3339 submit time — see {@link LtxJobStatus.created_at}. */
  created_at?: string | null;
  server_job_id?: string | null;
  resume_job_id?: string | null;
  resumable: boolean;
  segments: number;
  scripts: string[];
  gate_wait_ms: number;
  render_ms: number;
  error?: string | null;
  result_url?: string | null;
}

export function getWanVaceProps(): Promise<WanVaceProps> {
  return request('/api/v1/wan-vace/props');
}

/// Submit a Wan render. Returns immediately (HTTP 202) with a queue `job_id`. Poll
/// `getWanJob(job_id)` and fetch the clip from `result_url` when done. Multipart with optional
/// `init_image` (i2v) + `ref_image`.
export async function submitWanVace(
  req: WanVaceGenerateRequest,
  initImage?: File | null,
  refImage?: File | null,
  queue?: QueueSubmitOptions
): Promise<EnqueueResponse> {
  if (!initImage && !refImage) {
    return enqueueJson('wan-vace', '/api/v1/wan-vace/generate', req, queue);
  }
  const form = new FormData();
  form.append('request', JSON.stringify(req));
  if (initImage) form.append('init_image', initImage);
  if (refImage) form.append('ref_image', refImage);
  return enqueueForm('wan-vace', '/api/v1/wan-vace/generate', form, queue);
}

/// Poll one Wan job's status.
export function getWanJob(jobId: string): Promise<WanJobStatus> {
  return request(`/api/v1/wan-vace/jobs/${encodeURIComponent(jobId)}`);
}

/// The last 50 Wan rows on the queue, newest first.
export function listWanJobs(): Promise<{ jobs: WanJobStatus[] }> {
  return request('/api/v1/wan-vace/jobs');
}

/// Ask the main VL model to expand a brief into N per-segment continuation prompts. Pass a bare
/// base64 `image` (i2v opening frame or identity-anchor reference) so the model keeps that exact
/// subject across every shot — the server's system prompt encodes Wan2.2-VACE identity-continuity
/// guidance to stop the render drifting to a new subject.
export function wanVaceScriptFromLlm(
  brief: string,
  nSegments: number,
  secondsPerSegment?: number,
  image?: string
): Promise<WanScriptFromLlmResponse> {
  return request('/api/v1/wan-vace/script-from-llm', {
    method: 'POST',
    body: JSON.stringify({
      brief,
      n_segments: nSegments,
      seconds_per_segment: secondsPerSegment,
      image
    })
  });
}

export interface WanPromptFromLlmRequest {
  /** Seed idea / current field text to build on (optional). */
  idea?: string;
  /** "t2v" | "i2v" | "continuation"; drives motion/identity guidance. */
  mode?: string;
  seconds?: number;
  /** Init frame / identity anchor (reuses the shared LtxPromptImage DTO). */
  images?: LtxPromptImage[];
  prev_prompt?: string;
  next_prompt?: string;
  /** Reroll: an existing full prompt to rewrite (same scene/intent, fresh wording). Replaces `idea`. */
  rewrite?: string;
  /** Rework: a directed change to apply to the shot ("make her angry", a continuity note). */
  instruction?: string;
}

/// Draft ONE detailed Wan 2.2 shot prompt for a single prompt/beat field. Motion-first and
/// mode-aware — i2v animates only what's visible in the attached frame; continuation keeps the
/// same subject across beats.
export function wanPromptFromLlm(body: WanPromptFromLlmRequest): Promise<LtxPromptFromLlmResponse> {
  return request('/api/v1/wan-vace/prompt-from-llm', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Fetch a rendered Wan clip as an object URL for `<video>` playback.
export async function fetchWanClip(url: string): Promise<string> {
  const response = await authedFetch(url);
  const blob = await response.blob();
  return URL.createObjectURL(blob);
}

/// Cancel the in-flight Wan render (SIGKILL the worker child via /v1/admin/unload).
export function cancelWanRender(): Promise<unknown> {
  return request('/api/v1/wan-vace/cancel', { method: 'POST' });
}

// ── Avatar saved-character library (§6.1 of GO-LIVE-PLAN) ────────────────────

export interface AvatarAsset {
  id: string;
  filename: string;
  url: string;
  /// Seconds since unix epoch (i64 from the server).
  created_at: number;
}

export interface AvatarAssetsResponse {
  assets: AvatarAsset[];
}

export function listAvatarAssets(): Promise<AvatarAssetsResponse> {
  return request('/api/v1/avatar/assets');
}

/// Upload a fresh reference portrait. Single `image` multipart part. Returns
/// the saved AvatarAsset (id + url for re-use).
export async function uploadAvatarAsset(image: File): Promise<AvatarAsset> {
  const form = new FormData();
  form.append('image', image);
  const response = await authedFetch('/api/v1/avatar/assets', {
    method: 'POST',
    body: form
  });
  return response.json();
}

export async function deleteAvatarAsset(id: string): Promise<void> {
  await authedFetch(`/api/v1/avatar/assets/${encodeURIComponent(id)}`, {
    method: 'DELETE'
  });
}

/// Fetch a saved character's bytes as an object URL (for `<img>` preview).
export async function fetchAvatarAssetUrl(id: string): Promise<string> {
  const response = await authedFetch(`/api/v1/avatar/assets/${encodeURIComponent(id)}/file`);
  const blob = await response.blob();
  return URL.createObjectURL(blob);
}

// ── GPU service toggles (§6.5 of GO-LIVE-PLAN) ───────────────────────────────

export interface GpuServiceState {
  name: string;
  enabled: boolean;
  busy: boolean;
}

export interface GpuServicesResponse {
  services: GpuServiceState[];
}

export function listGpuServices(): Promise<GpuServicesResponse> {
  return request('/api/v1/gpu/services');
}

export function toggleGpuService(
  name: string,
  enabled: boolean
): Promise<{ name: string; enabled: boolean }> {
  return request(`/api/v1/gpu/services/${encodeURIComponent(name)}`, {
    method: 'POST',
    body: JSON.stringify({ enabled })
  });
}

export function disableAllServices(except?: string[]): Promise<unknown> {
  return request('/api/v1/gpu/services/disable-all', {
    method: 'POST',
    body: JSON.stringify({ except: except ?? [] })
  });
}

export function enableAllServices(): Promise<unknown> {
  return request('/api/v1/gpu/services/enable-all', {
    method: 'POST'
  });
}

// ── Beefy LLM (api/src/beefy.rs) ─────────────────────────────────────────────
//
// Qwen3.8-27B across BOTH cards, with only `BEEFY_ALLOW` admitted beside it. The same idiom
// as "Disable all except LLM" above plus a model swap — and it is a whole-box
// eviction, so nothing here is ever wired straight to a click.

/// `GET|POST /api/v1/gpu/beefy`. Every field is snake_case off `BeefyStatus`.
export interface BeefyStatus {
  /// The persisted flag.
  on: boolean;
  /// What the koblem PROCESS is talking to right now — not what the setting says.
  llm_url: string;
  /// `null` ⇒ `BEEFY_LLM_SERVICE_URL` is unset and the routes answer 501.
  beefy_url?: string | null;
  restore_url?: string | null;
  /// The beefy server's `/health` says `{"loaded": true}`.
  /// 🔴 Reachability is NOT readiness: the worker-isolation parent answers
  /// `{"status":"ok"}` with no model loaded at all. This is the only proof the
  /// 25 GiB is actually resident.
  loaded: boolean;
  /// Leaf model id at `llm_url`, or null when unreachable.
  model?: string | null;
  /// RFC3339 wall clock of when the mode was entered. Present only while `on`.
  since?: string | null;
  /// The gate's exclusive holder, e.g. `"llm"`.
  exclusive_for?: string | null;
  /// Services still admitted beside the 27B (`BEEFY_ALLOW`), budgeted against the
  /// per-card reservation. Empty = the box is locked whole.
  allow?: string[];
  /// `BEEFY_ALLOW` as configured — what `allow` will be once on. Use this while off.
  configured_allow?: string[];
  /// `X-Worker-Gpu` suppression — must be true whenever `on`.
  multi_gpu: boolean;
  live_jobs: number;
  /// True only when `DIRECTOR_LLM_SERVICE_URL` is set, in which case Director
  /// traffic does NOT follow the flip.
  director_llm_pinned: boolean;
  /// comfy-api's `busy` (includes renders submitted to ComfyUI directly); null when
  /// comfy-api is not configured or did not answer. True while `on` = someone is
  /// rendering on ComfyUI around every gate.
  comfy_busy?: boolean | null;
  /// comfy-api's admission is closed — must be true whenever `on`.
  comfy_draining?: boolean | null;
  /// 🔴 The half-state detector: the flag disagrees with the live URL, the gate,
  /// the both-cards flag, or comfy-api's drain. Somebody changed one of them
  /// behind koblem's back. Never hide this.
  inconsistent: boolean;
}

export function getBeefyState(): Promise<BeefyStatus> {
  return request('/api/v1/gpu/beefy');
}

/**
 * Enter or leave Beefy mode. **Blocks for tens of seconds** (measured ~10–40 s):
 * the eviction sweep, the 27B load and the proof it is resident all happen inside
 * this one request, and it answers with the FINAL state. There are no progress
 * phases to poll — do not build a stepper around it.
 *
 * Throws {@link ApiError}, whose `status` is the whole story:
 * - **501** `BEEFY_LLM_SERVICE_URL` is unset — the feature is not deployed here.
 * - **409** GPU jobs are running (the message names them; retry with `force` to
 *   starve them), ComfyUI is busy (possibly with a prompt submitted to it
 *   directly; `force` proceeds but that render keeps its VRAM), or a service could
 *   not prove it released its VRAM (the message names those; **nothing was
 *   flipped**).
 * - **502** the 27B did not come up. Already rolled back.
 * - **500** a DB write failed.
 */
export function setBeefyMode(opts: { on: boolean; force?: boolean }): Promise<BeefyStatus> {
  return request('/api/v1/gpu/beefy', {
    method: 'POST',
    body: JSON.stringify({ on: opts.on, force: opts.force ?? false })
  });
}

// ── Multi-GPU scheduler ──────────────────────

/// A physical GPU the scheduler can place work on.
export interface GpuCard {
  uuid: string;
  label: string;
  total_mb: number;
  fast: boolean;
  enabled: boolean;
  sort_order: number;
  updated_at: string;
}

/// Per-service VRAM-budget placement config (peak / home / candidates / evictors).
export interface GpuEngineConfig {
  service: string;
  peak_mb: number;
  home_gpu: string;
  candidate_gpus: string[];
  /// Named, relational eviction rule: the service NAMES allowed to evict THIS
  /// service. "flux can evict the LLM" => llm.evictable_by includes "flux".
  /// Empty = never auto-evicted (replaces the old numeric `priority`/`pinned`).
  evictable_by: string[];
  relocatable: boolean;
  /// Reserved for future conditional eviction rules (stored + shown).
  rules_json: string;
  updated_at: string;
}

/// One resident service on a card (live occupancy).
export interface GpuResident {
  service: string;
  peak_mb: number;
}

/// Live occupancy of one card: budget + what's resident.
export interface GpuCardOccupancy {
  uuid: string;
  total_mb: number;
  used_mb: number;
  free_mb: number;
  /// Measured real free VRAM (MiB) when a server has reported it; null when no
  /// measurement source is available (admission falls back to the pure budget).
  measured_free_mb: number | null;
  /// Beefy's reservation on this card while it holds the box. It stands in for
  /// the LLM's ledger row, so `used_mb` is NOT the sum of `residents` then.
  /// Absent when Beefy is off.
  reserved_mb?: number | null;
  fast: boolean;
  enabled: boolean;
  residents: GpuResident[];
}

export function listGpuCards(): Promise<{ cards: GpuCard[] }> {
  return request('/api/v1/gpu/scheduler/cards');
}

export function toggleGpuCard(uuid: string, enabled: boolean): Promise<{ card: GpuCard }> {
  return request(`/api/v1/gpu/scheduler/cards/${encodeURIComponent(uuid)}`, {
    method: 'POST',
    body: JSON.stringify({ enabled })
  });
}

export function listGpuEngineConfigs(): Promise<{
  engines: GpuEngineConfig[];
}> {
  return request('/api/v1/gpu/scheduler/engines');
}

export function upsertGpuEngineConfig(
  service: string,
  cfg: {
    peak_mb: number;
    home_gpu: string;
    candidate_gpus: string[];
    evictable_by: string[];
    relocatable: boolean;
    rules_json: string;
  }
): Promise<{ engine: GpuEngineConfig }> {
  return request(`/api/v1/gpu/scheduler/engines/${encodeURIComponent(service)}`, {
    method: 'PUT',
    body: JSON.stringify(cfg)
  });
}

export function getGpuOccupancy(): Promise<{ cards: GpuCardOccupancy[] }> {
  return request('/api/v1/gpu/scheduler/occupancy');
}

// ── Images (FLUX.2 — see flux2.cpp) ──────────────────────────────────────────

export type ImageModel = 'auto' | 'base' | 'edit';

export interface ImageGenerateRequest {
  prompt: string;
  negative_prompt?: string;
  /// "auto" (default) picks edit when an init/mask/reference image is attached,
  /// else base. "base" = text2img, "edit" = the Kontext edit/img2img model.
  model?: ImageModel;
  /// Must be a multiple of 16 (default 1024).
  width?: number;
  height?: number;
  steps?: number;
  cfg?: number;
  sampler?: string;
  scheduler?: string;
  seed?: number;
  /// Number of images to render (distinct seeds), default 1.
  batch?: number;
  /// img2img denoise strength when an init_image is attached.
  strength?: number;
  vae_tiling?: boolean;
  /// JSON-path base64 fields (omit on the multipart path — send file parts).
  init_image?: string;
  mask?: string;
  reference_images?: string[];
  /// Optionally chain background removal onto every rendered image. The cutout
  /// rides back on each `ImageResult.cutout_url`.
  remove_background?: ImageBackgroundRemoval;
}

/// Background-removal step chained after the flux render. `enabled` is the
/// toggle; the rest mirror `MattingRemoveRequest`'s knobs.
export interface ImageBackgroundRemoval {
  enabled: boolean;
  backend?: MattingBackend;
  process_res?: number;
  sensitivity?: number;
  mask_blur?: number;
  mask_offset?: number;
  refine?: boolean;
  invert?: boolean;
  bg_mode?: MattingBgMode;
  bg_color?: string;
}

export interface ImageResult {
  /// Server path to the render in the gallery. Pass through `mediaUrl` for an
  /// `<img src>`; the bytes are never inlined in the response.
  url: string;
  seed: number;
  /// Server path to the background-removed cutout, present only when
  /// remove_background was requested and the matting step succeeded.
  cutout_url?: string;
}

export interface ImageGenerateResponse {
  images: ImageResult[];
  model_used: 'base' | 'edit';
  width: number;
  height: number;
  steps: number;
  cfg: number;
  render_ms: number;
  gate_wait_ms: number;
  /// Set when background removal was requested but failed; the render itself
  /// still succeeded.
  background_removal_error?: string;
  /// Matting backend + wall time, present when the chained step ran.
  background_removal_backend?: string;
  background_removal_seconds?: number;
  /// The gallery row every `images[].url` points into. Always present — the
  /// route fails rather than return renders with nowhere to fetch them from.
  generation_id: string;
}

/// 🔴 SYNC-IF-FAST. `POST /generate` waits inline for a short window and answers
/// with the finished render when it lands in time; past that it answers 202 and a
/// job id. Both are legal answers to one POST — {@link isEnqueued} tells them
/// apart, and the fast path must not be removed: it is what keeps the
/// one-quick-image loop feeling synchronous.
export type ImageGenerateResult = ImageGenerateResponse | EnqueueResponse;

export type ImageJobState = 'queued' | 'rendering' | 'done' | 'failed' | 'cancelled';

export interface ImageJobProgress {
  phase?: string | null;
  progress?: number | null;
  images_done?: number | null;
  image_count?: number | null;
}

/// A FLUX.2 queue row in this page's vocabulary. The terminal answer carries the
/// same fields `/generate` returns inline, so {@link imageJobResult} maps one to
/// the other rather than the page keeping two result shapes.
export interface ImageJob {
  id: string;
  state: ImageJobState;
  server_job_id?: string | null;
  progress: ImageJobProgress;
  images: ImageResult[];
  generation_id: string;
  model_used: 'base' | 'edit';
  width: number;
  height: number;
  steps: number;
  cfg: number;
  background_removal_error?: string | null;
  background_removal_backend?: string | null;
  background_removal_seconds?: number | null;
  warnings: string[];
  error?: string | null;
  queued_ms?: number | null;
  admit_ms?: number | null;
  duration_ms?: number | null;
  blocked_reason?: BlockedReason | null;
  eta_start?: string | null;
}

export interface ImagePropsLimits {
  [key: string]: number;
}

export interface ImageVariant {
  default_steps: number;
  default_cfg: number;
}

export interface ImageProps {
  healthy: boolean;
  samplers: string[];
  schedulers: string[];
  limits: ImagePropsLimits;
  supported_modes: string[];
  variants: { base?: ImageVariant; edit?: ImageVariant };
  default_width: number;
  default_height: number;
  max_dim?: number;
  min_dim?: number;
}

export interface ImageSuggestPromptResponse {
  prompt: string;
  negative_prompt?: string;
}

export function imageProps(): Promise<ImageProps> {
  return request('/api/v1/images/props');
}

/// Drive a FLUX.2 image render. Answers with the finished render when it lands
/// inside the server's sync window, and with an {@link EnqueueResponse} when it
/// does not — see {@link ImageGenerateResult}. Sends JSON unless any image file
/// is attached (init/mask/reference), in which case it goes multipart with a
/// `request` JSON part (sans the b64 fields) plus the file parts.
export async function submitImageGenerate(
  req: ImageGenerateRequest,
  files?: {
    initImage?: File | null;
    mask?: File | null;
    referenceImages?: File[] | null;
  },
  queue?: QueueSubmitOptions
): Promise<ImageGenerateResult> {
  const initImage = files?.initImage ?? null;
  const mask = files?.mask ?? null;
  const referenceImages = files?.referenceImages ?? [];
  const hasFiles = !!initImage || !!mask || referenceImages.length > 0;

  if (!hasFiles) {
    return enqueueJson('images', '/api/v1/images/generate', req, queue);
  }

  // Multipart: strip the b64 fields from the JSON part — the bytes ride as
  // file parts instead.
  const { init_image: _initB64, mask: _maskB64, reference_images: _refB64, ...jsonReq } = req;
  void _initB64;
  void _maskB64;
  void _refB64;

  const form = new FormData();
  form.append('request', JSON.stringify(jsonReq));
  if (initImage) form.append('init_image', initImage);
  if (mask) form.append('mask', mask);
  for (const ref of referenceImages) form.append('reference_images', ref);

  return enqueueForm('images', '/api/v1/images/generate', form, queue);
}

/// Submit and settle: resolves with the finished render however long it waits.
/// For callers that render images inside a loop and have nowhere to draw a queue
/// position — the storyboard pass. A panel with a progress line wants
/// {@link submitImageGenerate} plus its own poll instead.
export async function imageGenerate(
  req: ImageGenerateRequest,
  files?: {
    initImage?: File | null;
    mask?: File | null;
    referenceImages?: File[] | null;
  }
): Promise<ImageGenerateResponse> {
  const accepted = await submitImageGenerate(req, files);
  if (!isEnqueued(accepted)) return accepted;
  return imageJobResult(await awaitImageJob(accepted.job_id));
}

async function awaitImageJob(id: string): Promise<ImageJob> {
  for (;;) {
    await new Promise((r) => setTimeout(r, JOB_POLL_MS));
    const job = await getImageJob(id);
    if (job.state === 'queued' || job.state === 'rendering') continue;
    if (job.state === 'done') return job;
    throw new Error(job.error || `render ${job.state}`);
  }
}

/// The finished render carried by a terminal job row.
export function imageJobResult(job: ImageJob): ImageGenerateResponse {
  return {
    images: job.images,
    model_used: job.model_used,
    width: job.width,
    height: job.height,
    steps: job.steps,
    cfg: job.cfg,
    render_ms: job.duration_ms ?? 0,
    gate_wait_ms: job.admit_ms ?? 0,
    background_removal_error: job.background_removal_error ?? undefined,
    background_removal_backend: job.background_removal_backend ?? undefined,
    background_removal_seconds: job.background_removal_seconds ?? undefined,
    generation_id: job.generation_id
  };
}

export function getImageJob(id: string): Promise<ImageJob> {
  return request(`/api/v1/images/jobs/${encodeURIComponent(id)}`);
}

/// The render in flight or next in line, so a reloaded page (or a second device)
/// re-finds the job whose id it never had.
export function getImageActive(): Promise<{ job: ImageJob | null }> {
  return request('/api/v1/images/active');
}

/// Have the main 9B write an image prompt from a casual brief, optionally
/// steered by reference images (base64). Returns prompt (+ optional negative)
/// to pre-fill the form. Ungated.
export function imageSuggestPrompt(
  brief: string,
  referenceImages?: string[],
  refine?: { source: string; instruction?: string }
): Promise<ImageSuggestPromptResponse> {
  const body: {
    brief: string;
    reference_images?: string[];
    refine_source?: string;
    refine_instruction?: string;
  } = { brief };
  if (referenceImages && referenceImages.length) {
    body.reference_images = referenceImages;
  }
  if (refine?.source.trim()) {
    body.refine_source = refine.source;
    if (refine.instruction?.trim()) body.refine_instruction = refine.instruction;
  }
  return request('/api/v1/images/suggest-prompt', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Cancel the in-flight image render. The caller of `imageGenerate` sees a 499.
export function cancelImageRender(): Promise<unknown> {
  return request('/api/v1/images/cancel', { method: 'POST' });
}

// ── Images (Krea-2-Turbo — the `krea2` service) ──────────────────────────────
//
// Same sd.cpp fork as FLUX.2, different checkpoint. Text-to-image AND
// instruction-based, identity-preserving EDIT: Krea 2 still has no first-party
// edit model, but the engine serves edits through the `krea2-identity-edit` LoRA
// plus its `krea2_identity_edit` reference-image preset. So `mode` here plays the
// part `model` plays for FLUX.2 — `auto` picks `edit` when a reference image is
// attached. There is still no init-image / mask (inpaint) surface: editing is
// reference-conditioned, not denoise-from-source.

/// "auto" (default) resolves to "edit" iff at least one reference image is
/// present, else "t2i". "edit" and "restage" with no reference image are a 400.
///
/// "edit" and "restage" are the SAME adapter pointed at different jobs, and the
/// difference is what happens to the reference's geometry:
///
///  - **edit** — "change the coat". The reference IS the canvas, so the render
///    takes its aspect ratio from the reference and the engine's centre-crop is a
///    no-op. Ask for a different aspect ratio and the server fits it back to the
///    reference's, rather than cropping the reference to yours.
///  - **restage** — "put this person in a night market". The reference is an
///    identity source; its aspect ratio is irrelevant, the target canvas is
///    whatever you ask for, and the engine is told to read the reference at its
///    native geometry instead of cropping it.
///
/// "auto" with a reference and an explicit canvas at a DIFFERENT aspect ratio
/// resolves to "restage", because cropping the reference would be silent data
/// loss. `mode_used` in the response says what it actually did.
export type Krea2Mode = 'auto' | 't2i' | 'edit' | 'restage';

export interface Krea2GenerateRequest {
  prompt: string;
  mode?: Krea2Mode;
  /// Reference images as base64 (a `data:image/...;base64,` prefix is accepted
  /// and stripped server-side). **1-2 entries, and the order matters: scene is
  /// image 1, person is image 2** — swapping them sharply degrades the result.
  /// Two-person edits blend faces; chain two single-reference passes instead.
  /// Omit on the multipart path — send `reference_images` file parts instead.
  ///
  /// `width`/`height` should match the source aspect ratio: the engine
  /// centre-crops the reference to the requested AR, and generation should stay
  /// ≤2 MP or the source starts duplicating.
  reference_images?: string[];
  /// Edit mode only: longest side the reference is handed to the VLM at (the
  /// `grounding_px` dial). LOWER = stronger edit adherence and a more uniform
  /// scene change; HIGHER = stronger identity. Unset keeps the preset's trained
  /// 768; clamped server-side to `min_grounding_px`..`max_grounding_px`. Lower it
  /// if you see duplicated or split compositions.
  grounding_px?: number;
  /// Only has an effect at cfg > 1. The default is cfg 1.0 (the distilled
  /// checkpoint runs without CFG), where a negative prompt is inert.
  negative_prompt?: string;
  /// Must be a multiple of 16; 1024–2048 is the model's supported range.
  width?: number;
  height?: number;
  steps?: number;
  cfg?: number;
  sampler?: string;
  scheduler?: string;
  seed?: number;
  /// Number of images to render (distinct seeds), default 1.
  batch?: number;
  /// LEAVE UNSET to keep the server's own `--vae-tiling` default (ON in the
  /// shipped krea2 image, which is what makes a 2048² VAE decode fit). Sending
  /// `false` explicitly turns it off.
  vae_tiling?: boolean;
  /// Optionally chain background removal onto every rendered image.
  remove_background?: ImageBackgroundRemoval;
}

export interface Krea2GenerateResponse {
  images: ImageResult[];
  /// What `auto` resolved to — which mode actually ran.
  mode_used: 't2i' | 'edit' | 'restage';
  width: number;
  height: number;
  steps: number;
  cfg: number;
  render_ms: number;
  gate_wait_ms: number;
  background_removal_error?: string;
  background_removal_backend?: string;
  background_removal_seconds?: number;
  /// The gallery row every `images[].url` points into. Always present — the
  /// route fails rather than return renders with nowhere to fetch them from.
  generation_id: string;
}

/// 🔴 SYNC-IF-FAST, exactly as FLUX.2's — see {@link ImageGenerateResult}.
export type Krea2GenerateResult = Krea2GenerateResponse | EnqueueResponse;

export interface Krea2Job {
  id: string;
  state: ImageJobState;
  server_job_id?: string | null;
  progress: ImageJobProgress;
  images: ImageResult[];
  generation_id: string;
  mode_used: 't2i' | 'edit' | 'restage';
  width: number;
  height: number;
  steps: number;
  cfg: number;
  background_removal_error?: string | null;
  background_removal_backend?: string | null;
  background_removal_seconds?: number | null;
  warnings: string[];
  error?: string | null;
  queued_ms?: number | null;
  admit_ms?: number | null;
  duration_ms?: number | null;
  blocked_reason?: BlockedReason | null;
  eta_start?: string | null;
}

/// Per-mode prefills. t2i is the distilled checkpoint's published 8 / cfg 1.0;
/// edit is the identity-edit LoRA card's balanced 10 / cfg 1.0.
export interface Krea2Variant {
  default_steps: number;
  default_cfg: number;
  /// Edit only: the reference-grounding size and its accepted bounds.
  default_grounding_px?: number;
  min_grounding_px?: number;
  max_grounding_px?: number;
}

export interface Krea2Variants {
  t2i?: Krea2Variant;
  edit?: Krea2Variant;
  /// Runs the same adapter as edit and inherits its sampling; the two differ
  /// only in what they do with the reference's geometry.
  restage?: Krea2Variant;
}

/// koblem-side sampling defaults for the distilled Krea-2-Turbo checkpoint. The
/// flat `default_*` fields are t2i's; `variants` carries the per-mode set (also
/// mirrored at the top level of `Krea2Props`).
export interface Krea2Defaults {
  default_steps: number;
  default_cfg: number;
  default_scheduler: string;
  variants?: Krea2Variants;
}

export interface Krea2Props {
  healthy: boolean;
  /// False when no krea2 URL is set at all (vs. set-but-unreachable).
  configured: boolean;
  samplers: string[];
  schedulers: string[];
  limits: ImagePropsLimits;
  defaults: Partial<Krea2Defaults>;
  /// Per-mode prefills — same shape as `ImageProps.variants`.
  variants?: Krea2Variants;
  /// False when the identity-edit LoRA is not in the engine's resolvable
  /// `loras[]`. GREY OUT edit mode when false: the engine answers an
  /// unresolvable LoRA with a bare 400 and nothing in its log, so an attempt
  /// looks like a random failure.
  edit_available?: boolean;
  /// The LoRA filename the server sends, for diagnostics in the UI.
  edit_lora?: string;
  /// Reference-image ceiling (2). More is a 400.
  max_reference_images?: number;
  default_width?: number;
  default_height?: number;
  max_dim?: number;
  min_dim?: number;
  /// Bottom of the model's supported resolution range — a UI hint, not a clamp.
  recommended_min_dim?: number;
}

export function krea2Props(): Promise<Krea2Props> {
  return request('/api/v1/krea2/props');
}

/// Drive a Krea-2-Turbo render (t2i or edit). Answers with the finished render
/// when it lands inside the server's sync window (~11 s at 1024² on the 5060 Ti),
/// and with an {@link EnqueueResponse} when it does not.
///
/// Sends JSON unless reference-image files are attached, in which case it goes
/// multipart with a `request` JSON part (sans `reference_images`) plus one
/// `reference_images` file part per image — **appended in order, and the order is
/// load-bearing: scene first, person second.**
export async function krea2Generate(
  req: Krea2GenerateRequest,
  files?: { referenceImages?: File[] | null },
  queue?: QueueSubmitOptions
): Promise<Krea2GenerateResult> {
  const referenceImages = files?.referenceImages ?? [];

  if (referenceImages.length === 0) {
    return enqueueJson('krea2', '/api/v1/krea2/generate', req, queue);
  }

  // Multipart: strip the b64 field from the JSON part — the bytes ride as file
  // parts instead.
  const { reference_images: _refB64, ...jsonReq } = req;
  void _refB64;

  const form = new FormData();
  form.append('request', JSON.stringify(jsonReq));
  for (const ref of referenceImages) form.append('reference_images', ref);

  return enqueueForm('krea2', '/api/v1/krea2/generate', form, queue);
}

export function getKrea2Job(id: string): Promise<Krea2Job> {
  return request(`/api/v1/krea2/jobs/${encodeURIComponent(id)}`);
}

export function getKrea2Active(): Promise<{ job: Krea2Job | null }> {
  return request('/api/v1/krea2/active');
}

/// The finished render carried by a terminal job row.
export function krea2JobResult(job: Krea2Job): Krea2GenerateResponse {
  return {
    images: job.images,
    mode_used: job.mode_used,
    width: job.width,
    height: job.height,
    steps: job.steps,
    cfg: job.cfg,
    render_ms: job.duration_ms ?? 0,
    gate_wait_ms: job.admit_ms ?? 0,
    background_removal_error: job.background_removal_error ?? undefined,
    background_removal_backend: job.background_removal_backend ?? undefined,
    background_removal_seconds: job.background_removal_seconds ?? undefined,
    generation_id: job.generation_id
  };
}

/// Have the main model write a Krea 2 prompt from a casual brief, or improve an
/// existing one in place. No reference-image path even though `/generate` now
/// takes them: the assist writes a t2i CAPTION, which is the wrong shape for an
/// edit INSTRUCTION. Ungated.
export function krea2SuggestPrompt(
  brief: string,
  refine?: { source: string; instruction?: string }
): Promise<ImageSuggestPromptResponse> {
  const body: {
    brief: string;
    refine_source?: string;
    refine_instruction?: string;
  } = { brief };
  if (refine?.source.trim()) {
    body.refine_source = refine.source;
    if (refine.instruction?.trim()) body.refine_instruction = refine.instruction;
  }
  return request('/api/v1/krea2/suggest-prompt', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

/// Cancel the in-flight Krea2 render. The caller of `krea2Generate` sees a 499.
export function cancelKrea2Render(): Promise<unknown> {
  return request('/api/v1/krea2/cancel', { method: 'POST' });
}

// ── Matting / background removal (vision.cpp BiRefNet / RMBG-2.0) ─────────────

export type MattingBackend = 'cpu' | 'gpu';
export type MattingBgMode = 'alpha' | 'color';

/// Tuning knobs for a background-removal request. All optional — omitting a
/// field leaves the server default. These map 1:1 onto the `/remove` query
/// string forwarded to matting-server.
export interface MattingRemoveRequest {
  backend?: MattingBackend;
  /// Inference resolution, 256..2048 (model native ~1024 when unset).
  process_res?: number;
  /// Mask strength / threshold, 0..1.
  sensitivity?: number;
  /// Box-blur radius on the mask, px.
  mask_blur?: number;
  /// Dilate (+) / erode (-) the mask, px.
  mask_offset?: number;
  /// Edge decontamination.
  refine?: boolean;
  /// Invert the mask.
  invert?: boolean;
  /// "alpha" (RGBA cutout) | "color" (flatten onto bg_color).
  bg_mode?: MattingBgMode;
  /// "#rrggbb" background for bg_mode=color.
  bg_color?: string;
  /// Return the grayscale mask instead of the cutout.
  mask_only?: boolean;
}

export interface MattingResult {
  /// Server path to the cutout PNG in the gallery. Pass through `mediaUrl` for
  /// an `<img src>`; the bytes are never inlined in the response.
  url: string;
}

export interface MattingRemoveResponse {
  count: number;
  results: MattingResult[];
  backend?: string;
  elapsed_seconds?: number;
  /// The gallery row every `results[].url` points into. Always present — the
  /// route fails rather than return cutouts with nowhere to fetch them from.
  generation_id: string;
}

export interface MattingHealth {
  healthy: boolean;
  configured: boolean;
  worker_alive: boolean;
  status?: string;
  backend?: string;
  native_size?: number;
  in_flight?: number;
}

/// Build the `/remove` query string from the tuning knobs.
function mattingQuery(req: MattingRemoveRequest): string {
  const q = new URLSearchParams();
  if (req.backend) q.set('backend', req.backend);
  if (req.process_res != null) q.set('process_res', String(req.process_res));
  if (req.sensitivity != null) q.set('sensitivity', String(req.sensitivity));
  if (req.mask_blur != null) q.set('mask_blur', String(req.mask_blur));
  if (req.mask_offset != null) q.set('mask_offset', String(req.mask_offset));
  if (req.refine != null) q.set('refine', String(req.refine));
  if (req.invert != null) q.set('invert', String(req.invert));
  if (req.bg_mode) q.set('bg_mode', req.bg_mode);
  if (req.bg_color) q.set('bg_color', req.bg_color);
  if (req.mask_only != null) q.set('mask_only', String(req.mask_only));
  const s = q.toString();
  return s ? `?${s}` : '';
}

/// Liveness probe for the matting service. Ungated; safe to poll on mount.
export function mattingHealth(): Promise<MattingHealth> {
  return request('/api/v1/matting/health');
}

/// Remove the background from a single image. Sends the raw bytes as the request
/// body (Content-Type image/*); knobs ride the query string.
export async function mattingRemoveSingle(
  file: File,
  req: MattingRemoveRequest = {}
): Promise<MattingRemoveResponse> {
  const response = await authedFetch(`/api/v1/matting/remove${mattingQuery(req)}`, {
    method: 'POST',
    headers: { 'Content-Type': file.type || 'application/octet-stream' },
    body: file
  });
  return response.json();
}

/// Remove the background from a batch of images (multipart, repeated `images`).
export async function mattingRemoveBatch(
  files: File[],
  req: MattingRemoveRequest = {}
): Promise<MattingRemoveResponse> {
  const form = new FormData();
  for (const f of files) form.append('images', f);
  const response = await authedFetch(`/api/v1/matting/remove${mattingQuery(req)}`, {
    method: 'POST',
    body: form
  });
  return response.json();
}

// ── Image → 3D rig ──────────────────────────────────────────────────────

export type RigTextureMode = 'generated' | 'project_front' | 'two_sided_mirror';
export interface ImageToRigRequest {
  seed?: number;
  texture_mode?: RigTextureMode;
  source_texture_resolution?: 512 | 1024;
  remove_background?: boolean;
  /** Separate plain GLBs for application-selected distance LOD. */
  generate_lods?: boolean;
}
export interface ImageToRigProps {
  configured: boolean;
  healthy: boolean;
  supports_project_front: boolean;
  supports_two_sided_mirror: boolean;
  source_texture_resolutions: number[];
}
export interface ImageToRigResponse {
  generation_id: string;
  url: string;
  bundle_url: string;
  lod_urls: string[];
  generate_lods: boolean;
  texture_mode: RigTextureMode;
  source_texture_resolution?: number;
  matting_elapsed_seconds?: number;
  worker_wall_seconds?: number;
  metadata: Record<string, unknown>;
}
export function imageToRigProps(): Promise<ImageToRigProps> {
  return request('/api/v1/3d/props');
}
export async function imageToRig(file: File, req: ImageToRigRequest): Promise<ImageToRigResponse> {
  const form = new FormData();
  form.append('image', file);
  form.append('request', new Blob([JSON.stringify(req)], { type: 'application/json' }));
  const response = await authedFetch('/api/v1/3d/generate', { method: 'POST', body: form });
  return response.json();
}

// ── Generations (per-service history) ───────────────────────────────────

export function listGenerations(service: string, limit = 50, offset = 0): Promise<GenerationList> {
  const q = new URLSearchParams({
    service,
    limit: String(limit),
    offset: String(offset)
  });
  return request<GenerationList>(`/api/v1/generations?${q}`);
}

export function deleteGeneration(id: string): Promise<void> {
  return request<void>(`/api/v1/generations/${id}`, { method: 'DELETE' });
}

export function clearGenerations(service: string): Promise<void> {
  const q = new URLSearchParams({ service });
  return request<void>(`/api/v1/generations?${q}`, { method: 'DELETE' });
}

/** Rename a generation. Returns the persisted (trimmed/clamped) title. */
export function renameGeneration(id: string, title: string): Promise<{ title: string }> {
  return request<{ title: string }>(`/api/v1/generations/${id}`, {
    method: 'PATCH',
    body: JSON.stringify({ title })
  });
}

/** Regenerate a generation's LLM title from its stored inputs. */
export function retitleGeneration(id: string): Promise<{ title: string }> {
  return request<{ title: string }>(`/api/v1/generations/${id}/retitle`, {
    method: 'POST'
  });
}

/** Short-lived signed URLs, by bare path. Populated by `prefetchMediaSignatures` /
 * `signedMediaUrl`; read synchronously by `mediaUrl`. Dropped a minute before expiry so a URL
 * handed to an element is never about to go stale mid-load. */
const signedMediaCache = new Map<string, { url: string; expiresAt: number }>();

function cachedSignature(path: string): string | null {
  const hit = signedMediaCache.get(path);
  if (!hit) return null;
  if (hit.expiresAt - 60_000 < Date.now()) {
    signedMediaCache.delete(path);
    return null;
  }
  return hit.url;
}

function rememberSignature(signed: string): void {
  const [bare, query] = signed.split('?');
  const exp = Number(new URLSearchParams(query ?? '').get('exp') ?? 0);
  if (!bare || !exp) return;
  signedMediaCache.set(bare, { url: signed, expiresAt: exp * 1000 });
}

/** Ask the server to sign these media paths, and cache the results so `mediaUrl` can use them.
 *
 * Signed URLs are the credential-free way to point an `<img>`/`<video>` at a gallery file: an
 * HMAC over (path, expiry) that grants that one file for ~15 minutes. Call this once for the
 * paths a view is about to render — a list, a storyboard — and the subsequent `mediaUrl` calls
 * come back signed instead of carrying an API key in the query string. */
export async function prefetchMediaSignatures(paths: string[]): Promise<void> {
  const wanted = [...new Set(paths.map((p) => p.split('?')[0]).filter(Boolean))].filter(
    (p) => !cachedSignature(p)
  );
  if (wanted.length === 0) return;
  const { urls } = await request<{ urls: string[] }>('/api/v1/media/sign', {
    method: 'POST',
    body: JSON.stringify({ paths: wanted })
  });
  urls.forEach(rememberSignature);
}

/** One signed, absolute media URL. Use where an await is available; `mediaUrl` is the
 * synchronous form that reads the same cache. */
export async function signedMediaUrl(path: string): Promise<string> {
  const bare = path.split('?')[0];
  await prefetchMediaSignatures([bare]);
  const signed = cachedSignature(bare);
  return signed ? `${apiBase}${signed}` : mediaUrl(path);
}

/** Absolute URL for a server-supplied media path (`results[].url` and friends), usable directly
 * in `<img>`/`<audio>`/`<video>` src — which cannot set an Authorization header.
 *
 * Prefers a cached SIGNED url (see `prefetchMediaSignatures`). Falls back to `?token=`, which
 * puts a real API key in browser history, `Referer` headers and access logs.
 * Prefetching is what removes it. */
export function mediaUrl(path: string): string {
  const bare = path.split('?')[0];
  const signed = cachedSignature(bare);
  if (signed) return `${apiBase}${signed}`;
  const token = getAuthToken();
  if (!token) return `${apiBase}${path}`;
  const sep = path.includes('?') ? '&' : '?';
  return `${apiBase}${path}${sep}token=${encodeURIComponent(token)}`;
}

/** URL for a gallery output file. */
export function generationFileUrl(id: string, index: number): string {
  return mediaUrl(`/api/v1/generations/${id}/file/${index}`);
}

/** Read a server-supplied media path (`images[].url`, `results[].url` and friends) back as a Blob,
 * for code that needs the bytes rather than an element `src` — re-cutting a render, or re-encoding
 * one into a storyboard still. Header auth, so no token in the query. */
export async function fetchMediaBlob(path: string): Promise<Blob> {
  const response = await authedFetch(path);
  return response.blob();
}

/** Fetch a transcript output's text (authed JSON file) for inline display. */
export async function fetchTranscriptText(id: string, index: number): Promise<string> {
  const data = await request<{ text?: string }>(`/api/v1/generations/${id}/file/${index}`);
  return typeof data?.text === 'string' ? data.text : '';
}

// ── Director assets (content-addressed) ──────────────────────────────────────

/**
 * Reference to a stored editor asset: `asset:<sha256>`.
 *
 * The project document holds these instead of the base64 it used to inline. That is what keeps
 * autosave, undo snapshots, render requests and the localStorage revert copy small no matter how
 * many stills, clips and audio a project accumulates — the cost used to track a project's assets
 * rather than its size.
 */
export const ASSET_REF_PREFIX = 'asset:';

export function isAssetRef(v: string | null | undefined): boolean {
  return typeof v === 'string' && v.startsWith(ASSET_REF_PREFIX);
}

export function assetRefHash(v: string): string {
  return v.slice(ASSET_REF_PREFIX.length);
}

/**
 * Store bytes and return their reference. Content-addressed and therefore idempotent: uploading
 * bytes the server already holds costs a hash lookup and no write, so the same still reused across
 * shots is stored once.
 */
export async function uploadDirectorAsset(
  data: Blob
): Promise<{ ref: string; hash: string; bytes: number }> {
  const response = await authedFetch('/api/v1/ltx-director/assets', {
    method: 'POST',
    headers: { 'Content-Type': data.type || 'application/octet-stream' },
    body: data
  });
  if (!response.ok) {
    const text = await response.text().catch(() => '');
    throw new Error(text || `asset upload failed: ${response.status}`);
  }
  return response.json();
}

/** What the reference-preparation pipeline did, and where the prepared image landed. */
export interface PreparedReference {
  ref: string;
  hash: string;
  bytes: number;
  width: number;
  height: number;
  /** False when the caller asked to skip the matte (the image was already cut out on white). */
  matted: boolean;
  /** Per-edge subject-alpha fractions, for the editor to explain a near-miss. */
  edges?: {
    top: number;
    bottom: number;
    left: number;
    right: number;
    threshold: number;
  } | null;
}

/**
 * Turn a snapshot into a reference the memory-regime checkpoints can actually hold: matted, on
 * white, letterboxed to the render aspect, and REFUSED if the subject is cut off at a frame edge.
 *
 * A 422 is not a transient failure — it means the image is unusable as a reference, and the message
 * says which edge and why. Show it; do not retry it. Every failure this catches is otherwise
 * invisible: the render succeeds and the subject's missing part is reinvented differently in every
 * shot.
 */
export async function prepareDirectorReference(
  data: Blob,
  width: number,
  height: number,
  skipMatte = false
): Promise<PreparedReference> {
  const q = new URLSearchParams({
    width: String(Math.round(width)),
    height: String(Math.round(height))
  });
  if (skipMatte) q.set('skip_matte', 'true');
  const response = await authedFetch(`/api/v1/ltx-director/assets/prepare-reference?${q}`, {
    method: 'POST',
    headers: { 'Content-Type': data.type || 'application/octet-stream' },
    body: data
  });
  if (!response.ok) {
    const body = await response.json().catch(() => null);
    const error = new Error(
      (body as { error?: string } | null)?.error ||
        `reference preparation failed: ${response.status}`
    ) as Error & { status?: number };
    error.status = response.status;
    throw error;
  }
  return response.json();
}

/**
 * Fetch an asset's bytes. Assets are immutable, so the response is cacheable forever.
 *
 * The thrown error carries `status`, so a caller can tell "this asset is gone" (404 — retrying will
 * never help) from a transient network failure (worth retrying).
 */
export async function fetchDirectorAsset(ref: string): Promise<Blob> {
  const hash = assetRefHash(ref);
  const response = await authedFetch(`/api/v1/ltx-director/assets/${encodeURIComponent(hash)}`);
  if (!response.ok) {
    const error = new Error(`asset fetch failed: ${response.status}`) as Error & {
      status?: number;
    };
    error.status = response.status;
    throw error;
  }
  return response.blob();
}

// ── Agent ───────────────────────────────────────────────────────────

export type AgentAttachmentKind = 'image' | 'audio';
export type AgentOutputKind = 'image' | 'video' | 'audio' | 'text';
export type AgentStepState = 'pending' | 'running' | 'done' | 'failed';
export type AgentMessageStatus = 'running' | 'complete' | 'error' | 'cancelled';

export interface AgentSessionSummary {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
}

/** An attachment on the way up: inline base64, capped client-side. */
export interface AgentAttachmentUpload {
  kind: AgentAttachmentKind;
  data_url: string;
  mime: string;
}

/** An attachment coming back down: a URL that carries its own `?token=`. */
export interface AgentAttachment {
  kind: AgentAttachmentKind;
  url: string;
  mime: string;
}

export interface AgentStepOutput {
  kind: AgentOutputKind;
  url: string;
  generation_id: string | null;
  mime: string;
}

export interface AgentStep {
  index: number;
  tool: string;
  args_summary: string;
  state: AgentStepState;
  /** Honest progress text — GPU waits, queue position, engine chatter. */
  detail: string;
  outputs: AgentStepOutput[];
}

export interface AgentMessage {
  id: string;
  seq: number;
  role: 'user' | 'assistant';
  content: string;
  attachments: AgentAttachment[];
  steps: AgentStep[];
  status: AgentMessageStatus;
  created_at: string;
}

export interface AgentSession {
  id: string;
  title: string;
  messages: AgentMessage[];
}

export function createAgentSession(): Promise<{
  id: string;
  title: string;
  created_at: string;
}> {
  return request('/api/v1/agent/sessions', {
    method: 'POST',
    body: '{}'
  });
}

export async function listAgentSessions(): Promise<AgentSessionSummary[]> {
  const res = await request<{ sessions: AgentSessionSummary[] }>('/api/v1/agent/sessions');
  return res.sessions ?? [];
}

export function getAgentSession(id: string): Promise<AgentSession> {
  return request(`/api/v1/agent/sessions/${encodeURIComponent(id)}`);
}

export function sendAgentMessage(
  id: string,
  content: string,
  attachments: AgentAttachmentUpload[]
): Promise<{ turn_id: string }> {
  return request(`/api/v1/agent/sessions/${encodeURIComponent(id)}/messages`, {
    method: 'POST',
    body: JSON.stringify({ content, attachments })
  });
}

export function cancelAgentTurn(id: string): Promise<void> {
  return request(`/api/v1/agent/sessions/${encodeURIComponent(id)}/cancel`, {
    method: 'POST',
    body: '{}'
  });
}

export function deleteAgentSession(id: string): Promise<void> {
  return request(`/api/v1/agent/sessions/${encodeURIComponent(id)}`, {
    method: 'DELETE'
  });
}

// ── Video → Upscale (LTX-2.5) ──────────────────────────────────────────
//
// 🔴 The source is handed to the engine BY PATH, never as bytes: a clip is routinely gigabytes and
// the multipart route would buffer it in koblem, again in comfy-api's parser, and again in its
// probe. `uploadUpscaleSource` streams the file to koblem's shared staging directory and hands back
// a token; everything after that names the token.

/// Stream a source clip to koblem's shared staging directory.
///
/// The response carries the probed geometry, so a panel can show the target resolution and the
/// segment split before anything is submitted. A file ffprobe cannot read is refused HERE rather
/// than twenty minutes into a render.
export async function uploadUpscaleSource(file: File): Promise<UpscaleUploadResponse> {
  const form = new FormData();
  form.append('file', file);
  const response = await authedFetch('/api/v1/upscale/upload', {
    method: 'POST',
    body: form
  });
  return response.json();
}

/// The engine's model law: the noise ladder with its measurements, the geometry rules, and the
/// `ram_guard` block the split is computed from. No GPU is touched.
///
/// ⚠️ The panel does not read this — the server computes the split and the panel reads the plan.
/// It is here for a client that wants to explain the knobs without submitting anything.
export function getUpscaleCapabilities(): Promise<Record<string, unknown>> {
  return request('/api/v1/upscale/capabilities');
}

/// 🔴 THE PRE-FLIGHT, and the reason it exists is `seams`. A window past the engine's host-RAM
/// guard has to be split into segments, and each segment gets its own refine — so every join is a
/// place the model can choose different detail. It also resolves the geometry LTX will actually
/// deliver (multiples of 64, frame counts 8n+1), which is not what the caller asked for. Ask this
/// before enabling the button.
export function planUpscale(req: UpscaleGenerateRequest): Promise<UpscalePlanResponse> {
  return request('/api/v1/upscale/plan', {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

/// Start an upscale. Returns as soon as it is ACCEPTED — poll `getUpscaleJob(job_id)`. The source
/// geometry and the segment split are NOT echoed here; `planUpscale` is where they come from, and
/// the panel has already asked.
export function upscaleGenerate(
  req: UpscaleGenerateRequest,
  queue?: QueueSubmitOptions
): Promise<EnqueueResponse> {
  return enqueueJson('upscale', '/api/v1/upscale/generate', req, queue);
}

export function getUpscaleJob(id: string): Promise<UpscaleJob> {
  return request(`/api/v1/upscale/jobs/${encodeURIComponent(id)}`);
}

/// The job holding the slot, if any — lets a reloaded page re-find a run whose id it lost.
export function getUpscaleActive(): Promise<{ active: UpscaleJob | null }> {
  return request('/api/v1/upscale/active');
}

/// Cooperative. The engine's cancel stops the segment in flight; koblem's flag stops the SEQUENCE,
/// which is the part that matters for a multi-segment run.
export function cancelUpscale(id: string): Promise<{ cancelled: string }> {
  return request(`/api/v1/upscale/jobs/${encodeURIComponent(id)}/cancel`, {
    method: 'POST',
    body: JSON.stringify({})
  });
}

/// The joined output, as a URL for a `<video src>` — a large file the browser should range-request
/// rather than hold in memory.
///
/// 🔴 Goes through `mediaUrl`, so it prefers a SIGNED url and only falls back to `?token=`. Call
/// `prefetchMediaSignatures([upscaleOutputPath(id)])` first: the fallback puts a real API key in
/// browser history, `Referer` headers and access logs, and this
/// path is on the sign route's allowlist precisely so it does not have to.
export function upscaleOutputPath(id: string): string {
  return `/api/v1/upscale/jobs/${encodeURIComponent(id)}/file`;
}

export function upscaleOutputUrl(id: string): string {
  return mediaUrl(upscaleOutputPath(id));
}

/// One delivered segment, before the join. Worth having: when a seam looks wrong, the question is
/// always "which side of it", and that is answered by playing the two segments.
export function upscaleSegmentPath(id: string, index: number): string {
  return `/api/v1/upscale/jobs/${encodeURIComponent(id)}/segments/${index}`;
}

export function upscaleSegmentUrl(id: string, index: number): string {
  return mediaUrl(upscaleSegmentPath(id, index));
}

/// The Director → Upscale hand-off, as an href.
///
/// 🔴 COORDINATES, NOT A FILENAME — the server resolves `(project, tier, shot, variant)` to a row
/// the same way `GET …/takes/{variant}` does, so the on-disk name never reaches the browser and a
/// caller cannot name a file koblem did not write.
///
/// ⚠️ NOTHING CALLS THIS YET. The take-card button that should is the one piece of the Upscale
/// feature deliberately left unplaced: it belongs in `DirectorWorkspace.svelte`, and dropping a
/// button into an 8k-line component nobody can type-check on the deployment box is how a build
/// breaks. Wire it wherever a take's actions live — the panel on the other end is finished and
/// deep-linkable today.
export function directorUpscaleHref(
  projectId: string,
  tier: 'proxy' | 'final',
  shot: number,
  variant: number
): string {
  const q = new URLSearchParams({
    tab: 'upscale',
    project: projectId,
    tier,
    shot: String(shot),
    variant: String(variant)
  });
  return `/ltx-video?${q.toString()}`;
}

// ── 3D → Pixal3D (longcat-rig) ──────────────────────────────────────────
//
// 🔴 EVERY GPU CALL HERE IS 202-AND-POLL. A rig is ~450 s for the first tier; nothing in this
// section blocks on one. `getPixal3dCharacter` is the poll, and there is no progress on it beyond
// the state — the engine publishes none and koblem does not invent one.

export function pixal3dProps(): Promise<Pixal3dProps> {
  return request('/api/v1/pixal3d/props');
}

/// What is holding the card, if anything. Rigs and motions share ONE slot; animating does not take
/// it, because the retarget is CPU-only and must not queue behind a ~450 s rig.
export function pixal3dActive(): Promise<{ active: Pixal3dBusy | null }> {
  return request('/api/v1/pixal3d/active');
}

export function listPixal3dCharacters(): Promise<{ characters: Pixal3dCharacter[] }> {
  return request('/api/v1/pixal3d/characters');
}

export function getPixal3dCharacter(id: string): Promise<Pixal3dCharacter> {
  return request(`/api/v1/pixal3d/characters/${encodeURIComponent(id)}`);
}

/// Start a rig. 202 — poll `getPixal3dCharacter(id)`. `position` is where it sits in the pending
/// set; null once it is running.
export async function createPixal3dCharacter(
  file: File,
  req: Pixal3dCreateRequest,
  queue?: QueueSubmitOptions
): Promise<{
  id: string;
  job_id: string;
  position: number | null;
  status: string;
  eta_seconds_first_tier: [number, number];
}> {
  const form = new FormData();
  form.append('image', file);
  form.append('json', new Blob([JSON.stringify(req)], { type: 'application/json' }));
  return enqueueForm('pixal3d', '/api/v1/pixal3d/characters', form, queue);
}

/// A re-roll, which arrives as a NEW character with `parent_id` set — the take it came from is
/// never overwritten.
///
/// 🔴 `mode: 'rig_seed'` MUST carry a seed different from the parent's or the server refuses it:
/// the decode is deterministic in (mesh, seed), so the same seed returns the rejected draw ~205 s
/// later. `mode: 'geo_seed'` redraws the mesh (~450 s) and is a genuinely different character.
export function rerigPixal3dCharacter(
  id: string,
  req: Pixal3dRerigRequest,
  queue?: QueueSubmitOptions
): Promise<{
  id: string;
  job_id: string;
  position: number | null;
  status: string;
  parent_id: string;
  origin: string;
}> {
  return enqueueJson(
    'pixal3d',
    `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/rerig`,
    req,
    queue
  );
}

export function renamePixal3dCharacter(id: string, name: string): Promise<void> {
  return request(`/api/v1/pixal3d/characters/${encodeURIComponent(id)}/rename`, {
    method: 'POST',
    body: JSON.stringify({ name })
  });
}

export function deletePixal3dCharacter(id: string): Promise<void> {
  return request(`/api/v1/pixal3d/characters/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/// Cancel a running rig or motion. Lands in ≤7 s and leaves no partial artifact.
export function cancelPixal3dCharacter(id: string): Promise<{ cancelled: string }> {
  return request(`/api/v1/pixal3d/characters/${encodeURIComponent(id)}/cancel`, { method: 'POST' });
}

export function listPixal3dMotions(): Promise<{ motions: Pixal3dMotion[] }> {
  return request('/api/v1/pixal3d/motions');
}

export function getPixal3dMotion(id: string): Promise<Pixal3dMotion> {
  return request(`/api/v1/pixal3d/motions/${encodeURIComponent(id)}`);
}

/// Generate a clip. `dit_resident: false` on the accept means this one pays the ~21 s model load —
/// worth showing, because it is the whole reason a first prompt feels three times slower.
export function createPixal3dMotion(
  req: Pixal3dMotionRequest,
  queue?: QueueSubmitOptions
): Promise<{
  id: string;
  job_id: string;
  position: number | null;
  status: string;
  dit_resident: boolean;
  eta_seconds: number;
}> {
  return enqueueJson('pixal3d', '/api/v1/pixal3d/motions', req, queue);
}

export function deletePixal3dMotion(id: string): Promise<void> {
  return request(`/api/v1/pixal3d/motions/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

export function cancelPixal3dMotion(id: string): Promise<{ cancelled: string }> {
  return request(`/api/v1/pixal3d/motions/${encodeURIComponent(id)}/cancel`, { method: 'POST' });
}

/// Retarget clips onto a rig. ~0.22 s — this is the loop, not a job.
export function animatePixal3dCharacter(
  id: string,
  req: Pixal3dAnimateRequest
): Promise<{ id: string; status: string; tier: string }> {
  return request(`/api/v1/pixal3d/characters/${encodeURIComponent(id)}/animate`, {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

export function deletePixal3dAnimation(id: string, animationId: string): Promise<void> {
  return request(
    `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/animations/${encodeURIComponent(animationId)}`,
    { method: 'DELETE' }
  );
}

/// Save a rig the browser repaired. The server also copies it into the shared staging directory so
/// `/v1/animate` can open it; `animatable: false` in the reply means that mount is missing and this
/// edit can be previewed and downloaded but not animated.
export async function savePixal3dEdit(
  id: string,
  glb: Blob,
  meta: { label?: string; base_tier: string; changes?: unknown }
): Promise<{ id: string; animatable: boolean; note: string | null }> {
  const form = new FormData();
  form.append('glb', glb, 'edited.glb');
  form.append('json', new Blob([JSON.stringify(meta)], { type: 'application/json' }));
  const response = await authedFetch(`/api/v1/pixal3d/characters/${encodeURIComponent(id)}/edits`, {
    method: 'POST',
    body: form
  });
  return response.json();
}

export function deletePixal3dEdit(id: string, editId: string): Promise<void> {
  return request(
    `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/edits/${encodeURIComponent(editId)}`,
    { method: 'DELETE' }
  );
}

// ── Pixal3D media paths ─────────────────────────────────────────────────
//
// Bare paths, for `prefetchMediaSignatures` + `mediaUrl`. The GLBs are fetched as blobs by the
// viewer (header auth, no token in the URL); the signed forms exist for the download anchors,
// which cannot set a header.

export function pixal3dModelPath(id: string, tier?: string): string {
  const q = tier ? `?tier=${encodeURIComponent(tier)}` : '';
  return `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/model${q}`;
}

export function pixal3dEditPath(id: string, editId: string): string {
  return `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/edits/${encodeURIComponent(editId)}/model`;
}

export function pixal3dAnimationPath(id: string, animationId: string): string {
  return `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/animations/${encodeURIComponent(animationId)}/model`;
}

export function pixal3dSourcePath(id: string): string {
  return `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/source`;
}

export function pixal3dBundlePath(id: string): string {
  return `/api/v1/pixal3d/characters/${encodeURIComponent(id)}/bundle`;
}

/// A signed, credential-free URL for a download anchor. Falls back to `?token=` when signing is
/// unavailable, which is the thing prefetching exists to avoid.
export function pixal3dBundleUrl(id: string): string {
  return mediaUrl(pixal3dBundlePath(id));
}

// ── Queue ───────────────────────────────────────────────────────────
//
// Mirrors `koblibs/koblem-client/src/types.rs`. Every optional field is optional
// because the server omits it, and an absent field means UNKNOWN — never zero.

export type JobKind =
  | 'ltx25'
  | 'h3'
  | 'music3'
  | 'upscale'
  | 'krea2'
  | 'flux2'
  | 'avatar'
  | 'wan_vace'
  | 'ltx_2b'
  | 'songgen'
  | 'sa3'
  | 'pixal3d_rig'
  | 'pixal3d_motion'
  | 'ltx_relip';

export type JobState =
  'queued' | 'running' | 'collecting' | 'done' | 'failed' | 'cancelled' | 'held';

/// Tagged on `reason`.
export type BlockedReason =
  | { reason: 'service_disabled'; service: string }
  | { reason: 'no_card_fits'; detail: string }
  | { reason: 'lane_busy'; lane: string }
  | { reason: 'eviction_cooldown' }
  | { reason: 'dependency_pending'; parent: string }
  | { reason: 'engine_unreachable'; detail: string }
  | { reason: 'head_reserved'; head: string }
  | { reason: 'paused' }
  /// Beefy LLM mode: one service owns BOTH cards and only its allow-list is admitted
  /// beside it (no queued job kind is on that list).
  /// Like `service_disabled` this is an OPERATOR state — the job waits and says
  /// so, it is never failed, and only a human clears it.
  | { reason: 'box_locked'; service: string };

export interface JobProgress {
  phase?: string;
  progress?: number;
  current_step?: number;
  total_steps?: number;
  preview_seq?: number;
  queue_position?: number;
}

export interface Job {
  id: string;
  kind: JobKind;
  state: JobState;
  band: number;
  queue_rank: number;
  target_gpu?: string;
  force_pin: boolean;
  server_job_id?: string;
  progress: JobProgress;
  depends_on?: string;
  created_by: string;
  blocked_reason?: BlockedReason;
  blocked_since?: string;
  error?: string;
  warnings: string[];
  queued_ms?: number;
  admit_ms?: number;
  render_ms?: number;
  units_total?: number;
  units_done: number;
  shape_key?: string;
  label: string;
  created_at: string;
  started_at?: string;
  finished_at?: string;
  eta_start?: string;
  eta_finish?: string;
  /// 🔴 Zero means the server invented nothing and omitted both ETAs. Never
  /// render a time off a zero-sample row.
  estimate_samples: number;
}

/// What an enqueueing `POST .../generate` answers with (202). 🔴 It states what
/// actually happened rather than what will: whether a job runs now or waits is a
/// prediction, and a prediction goes stale between paint and click.
export interface EnqueueResponse {
  job_id: string;
  state: JobState;
  /// 1-based position within the pending set. Absent once it is running.
  position?: number | null;
  eta_start?: string | null;
  blocked_reason?: BlockedReason | null;
  /// True when this submit matched an existing row's idempotency key and created
  /// no new work.
  deduplicated: boolean;
}

export interface QueueSnapshot {
  jobs: Job[];
  paused: boolean;
  pending: number;
  running: number;
  eta_finish?: string;
  /// Blocked past the alert threshold on a reason an operator can clear. A busy
  /// lane is the queue working correctly and never appears here.
  stuck: string[];
  /// The service Beefy mode is held for (`"llm"`), or absent when the box is
  /// free.
  ///
  /// 🔴 This rides the snapshot ON PURPOSE. Every page already holds this feed
  /// open for the topbar indicator, so the whole-app "the box is handed to the
  /// 27B" banner costs zero new requests. Do not add a `/api/v1/gpu/beefy` poll
  /// to get the same bit.
  box_locked?: string;
}

export interface ReorderRequest {
  after?: string | null;
  band?: number;
  bump?: boolean;
}

export interface JobEvent {
  job_id: string;
  kind: JobKind;
  state: JobState;
  progress?: JobProgress;
  blocked_reason?: BlockedReason;
  error?: string;
  warnings: string[];
  at: string;
}

export interface JobStat {
  kind: string;
  shape_key: string;
  samples: number;
  p50_ms: number;
  p90_ms: number;
  unit_p50_ms: number;
  updated_at: string;
  confident: boolean;
}

export function getQueueSnapshot(
  opts: { history?: boolean; limit?: number } = {}
): Promise<QueueSnapshot> {
  const params = new URLSearchParams();
  if (opts.history) params.set('history', 'true');
  if (opts.limit != null) params.set('limit', String(opts.limit));
  const qs = params.toString();
  return request(`/api/v1/queue${qs ? `?${qs}` : ''}`);
}

export function getQueueJob(id: string): Promise<Job | null> {
  return request(`/api/v1/queue/jobs/${encodeURIComponent(id)}`);
}

/// 204 cancelled, 202 asked-to-stop. 🔴 409 on a `collecting` row, carrying the
/// server's explanation — surface it rather than swallowing it.
export async function cancelQueueJob(id: string): Promise<void> {
  await authedFetch(`/api/v1/queue/jobs/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

export function reorderQueueJob(id: string, req: ReorderRequest): Promise<void> {
  return request(`/api/v1/queue/jobs/${encodeURIComponent(id)}/reorder`, {
    method: 'POST',
    body: JSON.stringify(req)
  });
}

export function holdQueueJob(id: string, held: boolean): Promise<void> {
  return request(`/api/v1/queue/jobs/${encodeURIComponent(id)}/hold`, {
    method: 'POST',
    body: JSON.stringify({ held })
  });
}

export async function preemptQueueJob(id: string): Promise<void> {
  await authedFetch(`/api/v1/queue/jobs/${encodeURIComponent(id)}/preempt`, { method: 'POST' });
}

/// Re-enqueue a finished job's stored payload as a new job (same kind, band and
/// card pin). 409 while the job is still live; 404 once its payload is pruned.
export function retryQueueJob(id: string): Promise<EnqueueResponse> {
  return request(`/api/v1/queue/jobs/${encodeURIComponent(id)}/retry`, { method: 'POST' });
}

export function setQueuePaused(paused: boolean): Promise<{ paused: boolean }> {
  return request('/api/v1/queue/pause', {
    method: 'POST',
    body: JSON.stringify({ paused })
  });
}

export function getQueueStats(): Promise<JobStat[]> {
  return request('/api/v1/queue/stats');
}

// ── Web Push ────────────────────────────────────────────────────────

export interface PushKey {
  public_key: string;
  subscriptions: number;
}

export function getPushKey(): Promise<PushKey> {
  return request('/api/v1/push/key');
}

export function subscribePush(body: SubscriptionBody): Promise<void> {
  return request('/api/v1/push/subscribe', { method: 'POST', body: JSON.stringify(body) });
}

export function unsubscribePush(endpoint: string): Promise<void> {
  return request('/api/v1/push/unsubscribe', {
    method: 'POST',
    body: JSON.stringify({ endpoint })
  });
}

export interface QueueEventHandlers {
  onJob: (event: JobEvent) => void;
  /// The dispatcher dropped frames for this client. The cached rows are now a
  /// guess — re-read the snapshot instead of missing the transitions.
  onLagged: (skipped: number) => void;
  onOpen?: () => void;
  onClose?: (error?: string) => void;
}

function dispatchSseFrame(frame: string, handlers: QueueEventHandlers) {
  let name = 'message';
  const data: string[] = [];
  for (const line of frame.split('\n')) {
    if (line.startsWith(':')) continue;
    if (line.startsWith('event:')) name = line.slice(6).trim();
    else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
  }
  const body = data.join('\n');
  if (!body) return;
  if (name === 'lagged') {
    handlers.onLagged(Number(body) || 0);
    return;
  }
  if (name !== 'job') return;
  try {
    handlers.onJob(JSON.parse(body) as JobEvent);
  } catch {
    // A frame we cannot parse is not worth tearing the stream down for; the
    // snapshot backstop will re-read the truth either way.
  }
}

/// Subscribe to `/api/v1/queue/events`. Not `EventSource`: that cannot carry the
/// bearer header, and a token in the query string would land in access logs.
/// Returns the teardown.
export function subscribeQueueEvents(handlers: QueueEventHandlers): () => void {
  const controller = new AbortController();
  void (async () => {
    try {
      const response = await authedFetch('/api/v1/queue/events', {
        headers: { Accept: 'text/event-stream' },
        signal: controller.signal
      });
      if (!response.body) throw new Error('the event stream had no body');
      handlers.onOpen?.();
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true }).replace(/\r\n/g, '\n');
        let split = buffer.indexOf('\n\n');
        while (split >= 0) {
          dispatchSseFrame(buffer.slice(0, split), handlers);
          buffer = buffer.slice(split + 2);
          split = buffer.indexOf('\n\n');
        }
      }
      handlers.onClose?.();
    } catch (e) {
      if (controller.signal.aborted) return;
      handlers.onClose?.(e instanceof Error ? e.message : String(e));
    }
  })();
  return () => controller.abort();
}
