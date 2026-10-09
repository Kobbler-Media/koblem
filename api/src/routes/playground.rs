use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use kob_providers::releases::{
    InterpretInput, InterpretedQuality, InterpretedRelease, ReleaseSearchRequest,
};
use koblem_client::types::{ReviewCandidate, ReviewRequest, ReviewResponse, WantContext};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::AppState;
use crate::auth::require_auth;
use crate::context::assemble_context;
use crate::prompts::review_candidates_messages;
use crate::repositories::llm_logs::{self, CreateLog};
use crate::repositories::review_sessions::{self, StoreReviewParams};
use crate::review_helpers::{
    compute_diff, finish_reason, format_debug_prompt, parse_review_response, prepend_note,
    token_usage, truncation_note,
};
use crate::routes::chat::{grammar_tag, preset_tag};
use koblem_client::types::{ReviewAccept, ReviewDelta, SamplingConfig, SamplingPreset};

/// Flush interval for streaming progress UPDATEs into the running llm_logs row.
/// Mirrors review_pipeline::PROGRESS_FLUSH; kept inline so playground+testbed
/// don't have to import a private constant from the pipeline module.
const PROGRESS_FLUSH: std::time::Duration = std::time::Duration::from_millis(1500);

/// Resolve the effective sampling config for a playground review run:
/// the `Deterministic` baseline overlaid with the persisted
/// `sampling_review_candidates` DB default, then with any per-run override
/// from the request.
async fn resolve_review_sampling(
    pool: &sqlx::PgPool,
    request_override: Option<&SamplingConfig>,
) -> SamplingConfig {
    let baseline = SamplingConfig {
        preset: Some(SamplingPreset::Deterministic),
        enable_thinking: Some(false),
        ..SamplingConfig::default()
    };
    let db_default = crate::sampling::resolve_sampling(pool, "sampling_review_candidates").await;
    let request = request_override.cloned().unwrap_or_default();
    baseline.overlay(&db_default).overlay(&request)
}

/// Open an `llm_logs` row in `running` state for a playground review run so
/// the History page (joined to llm_logs) can surface its CoT trace and the
/// LLM Logs page can show it live. Returns nil-UUID on insert failure — the
/// streaming helper's UPDATEs no-op against nil so the review still runs.
async fn open_review_log(
    pool: &sqlx::PgPool,
    body: &serde_json::Value,
    sampling: &SamplingConfig,
    caller: &str,
) -> uuid::Uuid {
    let mut seed = CreateLog::new(
        Some(caller.to_string()),
        body.get("messages").cloned().unwrap_or_default(),
    );
    seed.set_sampling_from_body(body, preset_tag(sampling), grammar_tag(sampling).as_deref());
    match llm_logs::start_log(pool, &seed).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("failed to start {caller} llm log: {e}");
            uuid::Uuid::nil()
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/playground/search", post(search))
        .route("/api/v1/playground/interpret", post(interpret))
        .route("/api/v1/playground/compare", post(compare))
}

// ── Search ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PlaygroundSearchRequest {
    show_id: String,
    season_id: Option<String>,
    #[serde(default)]
    episode_ids: Vec<String>,
    /// Optional per-run sampling override. `None` = use DB default
    /// (`sampling_review_candidates`) or the hardcoded `Deterministic` baseline.
    #[serde(default)]
    sampling: Option<koblem_client::types::SamplingConfig>,
}

#[derive(Serialize)]
struct SearchAndReviewResponse {
    /// Raw kobload scoring results.
    search: kob_providers::releases::ReleaseSearchResults,
    /// LLM review of the results.
    review: ReviewResponse,
    /// Session ID for the decision log.
    session_id: Option<String>,
}

/// Search & Review: builds a MediaWant → searches via kobload → reviews via LLM.
/// Returns both kobload's raw scoring AND the LLM's verdicts.
async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<PlaygroundSearchRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let library = match state.library_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "library provider not configured",
            )
                .into_response();
        }
    };

    let releases = match state.release_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "release provider not configured",
            )
                .into_response();
        }
    };

    // Step 1: Build MediaWant from library (same as a real want)
    let build_req = kob_providers::types::BuildMediaWantRequest {
        show_id: payload.show_id.clone(),
        season_id: payload.season_id.clone(),
        episode_ids: payload.episode_ids.clone(),
    };

    let media_want = match library.build_media_want(&build_req).await {
        Ok(mw) => mw,
        Err(e) => {
            error!("build_media_want failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                format!("build_media_want failed: {e}"),
            )
                .into_response();
        }
    };

    // Step 2: Search releases via kobload
    let search_req = ReleaseSearchRequest {
        media_want,
        quality_profile_id: None,
        min_score: 0.0,
        limit: None,
    };

    let search_results = match releases.search_releases(&search_req).await {
        Ok(results) => results,
        Err(e) => {
            error!("release search failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                format!("release search failed: {e}"),
            )
                .into_response();
        }
    };

    // Step 3: Extract wanted context from the MediaWant JSON (same fields kobload uses)
    let mw = &search_req.media_want;
    let wanted_title = mw
        .get("show_name")
        .or_else(|| mw.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let wanted_year = mw.get("year").and_then(|v| v.as_i64()).map(|y| y as i32);
    let wanted_season = mw
        .get("season")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // For TvSeason, find the specific episode from episodes_needed
    let wanted_episode = payload.episode_ids.first().and_then(|_| {
        mw.get("episodes_needed")
            .and_then(|v| v.as_array())
            .and_then(|eps| eps.first())
            .and_then(|ep| ep.get("episode").and_then(|e| e.as_str()))
            .map(|s| s.to_string())
    });
    let wanted_episode_title = mw
        .get("episodes_needed")
        .and_then(|v| v.as_array())
        .and_then(|eps| eps.first())
        .and_then(|ep| ep.get("title").and_then(|t| t.as_str()))
        .map(|s| s.to_string());
    let wanted_air_date = mw
        .get("last_air_date")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let want = build_want_context(
        wanted_season.clone(),
        wanted_episode.clone(),
        wanted_year,
        wanted_episode_title.clone(),
        wanted_air_date.clone(),
    );

    // Step 4: Build ReviewCandidates for the LLM.
    // Dedup by release name (same release from multiple indexers is redundant for review).
    // Cap at 50 unique releases — enough for good coverage, fast enough for E4B inference.
    fn scored_to_candidate(sr: &kob_providers::releases::ScoredRelease) -> ReviewCandidate {
        ReviewCandidate {
            name: sr.name.clone(),
            kobload_accepted: sr.accepted,
            kobload_score: sr.total_score,
            parsed_quality: parsed_quality_from(&sr.quality, sr.release_group.as_deref()),
            parsed_season: sr.matched_season.clone(),
            parsed_episode: sr.matched_episode.clone(),
            parsed_title: None,
            parsed_year: None,
            reject_reasons: vec![],
            quality_rejected: !sr.accepted,
            quality_reject_reasons: vec![],
            structural_reject_reason: None,
            tier_bonus: None,
            publish_date: sr.publish_date.clone(),
            seeders: sr.seeders,
            votes: sr.votes,
            grabs: sr.grabs,
        }
    }

    // Dedup: same release name from same protocol → keep best scored version.
    // Same name on usenet vs torrent = different entries (different signals: seeders vs grabs).
    let mut seen = std::collections::HashSet::new();
    let mut review_candidates: Vec<ReviewCandidate> = Vec::new();

    // Accepted first (already sorted by score desc)
    for sr in &search_results.candidates {
        let protocol = sr.protocol.as_deref().unwrap_or("unknown");
        if seen.insert((sr.name.clone(), protocol.to_string())) {
            review_candidates.push(scored_to_candidate(sr));
        }
    }
    // Then rejected
    for sr in &search_results.rejected {
        let protocol = sr.protocol.as_deref().unwrap_or("unknown");
        if seen.insert((sr.name.clone(), protocol.to_string())) {
            review_candidates.push(scored_to_candidate(sr));
        }
    }

    let review_request = ReviewRequest {
        source_id: Some(payload.show_id.clone()),
        wanted_title,
        want,
        // Playground doesn't have access to a QualityProfileRow — leaving the
        // structured profile None means the prompt will render "QUALITY WANT:
        // (no preferences)", which is the truthful state for a dry-run search.
        quality_profile: None,
        candidates: review_candidates,
        sampling: payload.sampling.clone(),
    };

    // Step 5: Assemble context + run LLM review
    let ctx_start = std::time::Instant::now();
    let library_ref = state
        .library_provider
        .as_ref()
        .map(|p| p.as_ref() as &dyn kob_providers::LibraryProvider);
    let ctx = assemble_context(&review_request, library_ref, &state.db).await;
    let context_assembly_ms = ctx_start.elapsed().as_millis() as u64;

    let mut sorted_candidates = review_request.candidates.clone();
    let sort_tokens = crate::prompts::build_sort_token_bag(&review_request);
    crate::prompts::sort_candidates_by_relevance(&mut sorted_candidates, &sort_tokens);

    let sampling = resolve_review_sampling(&state.db, payload.sampling.as_ref()).await;
    let body = match review_candidates_messages(
        &state.db,
        &ctx,
        &review_request,
        &sorted_candidates,
        &sampling,
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            error!("prompt engine error in search-review: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {e}"),
            )
                .into_response();
        }
    };
    let debug_prompt = format_debug_prompt(&body);

    let log_id = open_review_log(&state.db, &body, &sampling, "playground:search").await;

    let llm_start = std::time::Instant::now();
    let parts = match state
        .llama
        .chat_completion_streaming_with_log(&body, state.db.clone(), log_id, PROGRESS_FLUSH)
        .await
    {
        Ok((_permit, response_body)) => run_review_parts(
            content_from(&response_body),
            &response_body,
            &sorted_candidates,
        ),
        Err(e) => {
            error!("llama-server error in search-review: {e}");
            error_review_parts(format!("LLM error: {e}"))
        }
    };
    let llm_inference_ms = llm_start.elapsed().as_millis() as u64;

    let session_id = review_sessions::store_review(
        &state.db,
        StoreReviewParams {
            request_json: serde_json::to_value(&review_request).unwrap_or_default(),
            ctx: &ctx,
            prompt_sent: &debug_prompt,
            llm_response: parts.raw_llm_response.as_deref(),
            verdicts_json: parts.verdicts_json.clone(),
            notes: parts.notes.clone(),
            context_assembly_ms,
            llm_inference_ms,
            llm_log_id: (!log_id.is_nil()).then_some(log_id),
        },
    )
    .await;

    let review = ReviewResponse {
        agreed: parts.agreed,
        vetoed: parts.vetoed,
        promoted: parts.promoted,
        accepts: parts.accepts,
        dropped_indices: parts.dropped_indices,
        context_used: ctx.context_used,
        notes: parts.notes,
        debug_prompt: Some(debug_prompt),
        raw_llm_response: parts.raw_llm_response,
        prompt_tokens: parts.prompt_tokens,
        completion_tokens: parts.completion_tokens,
        context_assembly_ms: Some(context_assembly_ms),
        llm_inference_ms: Some(llm_inference_ms),
        session_id: session_id.clone(),
    };

    Json(SearchAndReviewResponse {
        search: search_results,
        review,
        session_id,
    })
    .into_response()
}

// ── Interpret ────────────────────────────────────────────────────────

async fn interpret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Vec<InterpretInput>>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.release_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "release provider not configured",
            )
                .into_response();
        }
    };

    match provider.interpret_releases(&payload).await {
        Ok(results) => Json(results).into_response(),
        Err(e) => {
            error!("release provider interpret failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("release provider error: {e}"),
            )
                .into_response()
        }
    }
}

// ── Compare ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CompareRequest {
    source_id: Option<String>,
    /// Discriminator from older clients ("tv" / "movie" / "sport"). Ignored —
    /// koblem now branches on the `WantContext` variant inferred from the
    /// season/episode fields below. Tolerated for wire-compat.
    #[serde(default)]
    #[allow(dead_code)]
    media_type: Option<String>,
    wanted_title: String,
    wanted_year: Option<i32>,
    wanted_season: Option<String>,
    wanted_episode: Option<String>,
    wanted_episode_title: Option<String>,
    releases: Vec<CompareRelease>,
    /// Optional per-run sampling override.
    #[serde(default)]
    sampling: Option<koblem_client::types::SamplingConfig>,
}

#[derive(Deserialize)]
struct CompareRelease {
    name: String,
    media_kind: String,
    #[allow(dead_code)]
    publish_date: Option<String>,
}

#[derive(Serialize)]
struct CompareResponse {
    review: ReviewResponse,
    interpreted: Vec<InterpretedRelease>,
    session_id: Option<String>,
}

async fn compare(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CompareRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let provider = match state.release_provider.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "release provider not configured",
            )
                .into_response();
        }
    };

    // 1. Interpret releases
    let interpret_inputs: Vec<InterpretInput> = payload
        .releases
        .iter()
        .map(|r| InterpretInput {
            name: r.name.clone(),
            media_kind: r.media_kind.clone(),
        })
        .collect();

    let interpreted = match provider.interpret_releases(&interpret_inputs).await {
        Ok(r) => r,
        Err(e) => {
            error!("release provider interpret failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                format!("release provider error: {e}"),
            )
                .into_response();
        }
    };

    // 2. Build ReviewCandidates from interpreted releases.
    //
    // The compare flow has no upstream filter — the user pasted raw release
    // strings to ask "what does the LLM think of these?". For the diff UI,
    // we treat each pasted release as if the user implicitly accepted it,
    // so LLM picks land in `agreed` and LLM rejections land in `vetoed`.
    // (`promoted` will always be empty here, by construction.)
    let candidates: Vec<ReviewCandidate> = interpreted
        .iter()
        .enumerate()
        .map(|(i, ir)| ReviewCandidate {
            name: ir.input.clone(),
            kobload_accepted: true,
            kobload_score: 0.0,
            parsed_quality: parsed_quality_from(&ir.quality, ir.release_group.as_deref()),
            parsed_season: ir
                .media
                .get("season")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            parsed_episode: ir
                .media
                .get("episode")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            parsed_title: Some(ir.title.clone()),
            parsed_year: ir
                .media
                .get("year")
                .and_then(|v| v.as_i64())
                .map(|y| y as i32),
            reject_reasons: vec![],
            quality_rejected: false,
            quality_reject_reasons: vec![],
            structural_reject_reason: None,
            tier_bonus: None,
            publish_date: payload.releases.get(i).and_then(|r| r.publish_date.clone()),
            seeders: None,
            votes: None,
            grabs: None,
        })
        .collect();

    // 3. Build a ReviewRequest
    let review_request = ReviewRequest {
        source_id: payload.source_id.clone(),
        wanted_title: payload.wanted_title.clone(),
        want: build_want_context(
            payload.wanted_season.clone(),
            payload.wanted_episode.clone(),
            payload.wanted_year,
            payload.wanted_episode_title.clone(),
            None,
        ),
        quality_profile: None,
        candidates,
        sampling: payload.sampling.clone(),
    };

    // 4. Assemble context
    let ctx_start = std::time::Instant::now();
    let library_provider = state
        .library_provider
        .as_ref()
        .map(|p| p.as_ref() as &dyn kob_providers::LibraryProvider);
    let ctx = assemble_context(&review_request, library_provider, &state.db).await;
    let context_assembly_ms = ctx_start.elapsed().as_millis() as u64;

    // 5. Sort + build prompt
    let mut sorted_candidates = review_request.candidates.clone();
    let sort_tokens = crate::prompts::build_sort_token_bag(&review_request);
    crate::prompts::sort_candidates_by_relevance(&mut sorted_candidates, &sort_tokens);
    let sampling = resolve_review_sampling(&state.db, payload.sampling.as_ref()).await;
    let body = match review_candidates_messages(
        &state.db,
        &ctx,
        &review_request,
        &sorted_candidates,
        &sampling,
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            error!("prompt engine error in playground compare: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {e}"),
            )
                .into_response();
        }
    };
    let debug_prompt = format_debug_prompt(&body);

    let log_id = open_review_log(&state.db, &body, &sampling, "playground:compare").await;

    // 6. LLM inference
    let llm_start = std::time::Instant::now();
    let (_permit, response_body) = match state
        .llama
        .chat_completion_streaming_with_log(&body, state.db.clone(), log_id, PROGRESS_FLUSH)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("llama-server error in playground compare: {e}");
            return (StatusCode::BAD_GATEWAY, format!("llama-server error: {e}")).into_response();
        }
    };
    let llm_inference_ms = llm_start.elapsed().as_millis() as u64;

    let parts = run_review_parts(
        content_from(&response_body),
        &response_body,
        &sorted_candidates,
    );

    let session_id = review_sessions::store_review(
        &state.db,
        StoreReviewParams {
            request_json: serde_json::to_value(&review_request).unwrap_or_default(),
            ctx: &ctx,
            prompt_sent: &debug_prompt,
            llm_response: parts.raw_llm_response.as_deref(),
            verdicts_json: parts.verdicts_json.clone(),
            notes: parts.notes.clone(),
            context_assembly_ms,
            llm_inference_ms,
            llm_log_id: (!log_id.is_nil()).then_some(log_id),
        },
    )
    .await;

    let review = ReviewResponse {
        agreed: parts.agreed,
        vetoed: parts.vetoed,
        promoted: parts.promoted,
        accepts: parts.accepts,
        dropped_indices: parts.dropped_indices,
        context_used: ctx.context_used,
        notes: parts.notes,
        debug_prompt: Some(debug_prompt),
        raw_llm_response: parts.raw_llm_response,
        prompt_tokens: parts.prompt_tokens,
        completion_tokens: parts.completion_tokens,
        context_assembly_ms: Some(context_assembly_ms),
        llm_inference_ms: Some(llm_inference_ms),
        session_id: session_id.clone(),
    };

    Json(CompareResponse {
        review,
        interpreted,
        session_id,
    })
    .into_response()
}

// ── Shared review-result assembly helpers ──────────────────────────────────
//
// Both /playground/search and /playground/compare follow the same flow after
// the LLM call: extract content, parse, diff, layer warnings, build the
// response. These helpers factor that out so each endpoint stays readable.

/// Per-request review pipeline output, before being plugged into a
/// [`ReviewResponse`] or persisted as a session.
struct ReviewParts {
    agreed: Vec<ReviewDelta>,
    vetoed: Vec<ReviewDelta>,
    promoted: Vec<ReviewDelta>,
    accepts: Vec<ReviewAccept>,
    dropped_indices: Vec<usize>,
    notes: Option<String>,
    raw_llm_response: Option<String>,
    verdicts_json: Option<serde_json::Value>,
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
}

/// Pull `choices[0].message.content` out of a llama-server response body.
fn content_from(response_body: &serde_json::Value) -> &str {
    response_body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("{}")
}

/// Parse the LLM response into accepts, diff against kobload, layer
/// truncation/parse/dropped-index warnings into notes, and bundle everything
/// up for the caller to plug into a [`ReviewResponse`].
fn run_review_parts(
    content: &str,
    response_body: &serde_json::Value,
    candidates: &[ReviewCandidate],
) -> ReviewParts {
    let finish = finish_reason(response_body);
    let parsed = parse_review_response(content);
    let diff = compute_diff(&parsed.accepts, candidates);
    let (prompt_tokens, completion_tokens) = token_usage(response_body);

    let mut notes = parsed.notes;
    if let Some(err) = parsed.parse_error {
        notes = Some(prepend_note(&notes, &err));
    }
    if let Some(warning) = truncation_note(&finish, parsed.accepts.len(), candidates.len()) {
        notes = Some(prepend_note(&notes, &warning));
    }
    if !diff.dropped_indices.is_empty() {
        notes = Some(prepend_note(
            &notes,
            &format!(
                "⚠ LLM emitted {} out-of-range index/indices: {:?}",
                diff.dropped_indices.len(),
                diff.dropped_indices
            ),
        ));
    }

    let verdicts_json = serde_json::json!({
        "accepts": parsed.accepts,
        "agreed": diff.agreed,
        "vetoed": diff.vetoed,
        "promoted": diff.promoted,
        "dropped_indices": diff.dropped_indices,
    });

    ReviewParts {
        agreed: diff.agreed,
        vetoed: diff.vetoed,
        promoted: diff.promoted,
        accepts: parsed.accepts,
        dropped_indices: diff.dropped_indices,
        notes,
        raw_llm_response: Some(content.to_string()),
        verdicts_json: Some(verdicts_json),
        prompt_tokens,
        completion_tokens,
    }
}

/// `ReviewParts` for the LLM-error path: empty buckets, error text in notes,
/// no raw response or token counts.
fn error_review_parts(error_msg: String) -> ReviewParts {
    ReviewParts {
        agreed: vec![],
        vetoed: vec![],
        promoted: vec![],
        accepts: vec![],
        dropped_indices: vec![],
        notes: Some(error_msg),
        raw_llm_response: None,
        verdicts_json: None,
        prompt_tokens: None,
        completion_tokens: None,
    }
}

/// Map kob-providers' `InterpretedQuality` (a 11-field subset used in playground
/// flows) into the canonical `nats_jobs::ParsedQuality` carried on the wire.
/// Fields not present in `InterpretedQuality` default to false / None.
fn parsed_quality_from(
    iq: &InterpretedQuality,
    release_group: Option<&str>,
) -> nats_jobs::ParsedQuality {
    nats_jobs::ParsedQuality {
        resolution: iq.resolution.clone(),
        source: iq.source.clone(),
        streaming_service: iq.streaming_service.clone(),
        codec: iq.codec.clone(),
        audio: iq.audio.clone(),
        is_proper: iq.is_proper,
        is_repack: iq.is_repack,
        is_remux: iq.is_remux,
        is_hdr: iq.is_hdr,
        is_dolby_vision: iq.is_dolby_vision,
        is_hardcoded_subs: iq.is_hardcoded_subs,
        release_group: release_group.map(str::to_string),
        ..Default::default()
    }
}

/// Build a `WantContext` from the loose fields the playground extracts from
/// kobbler's `MediaWant` JSON / its `CompareRequest` payload. The mapping is
/// intentionally narrow — sport events and other variants aren't reachable
/// from the playground UI.
fn build_want_context(
    season: Option<String>,
    episode: Option<String>,
    year: Option<i32>,
    episode_title: Option<String>,
    air_date: Option<String>,
) -> WantContext {
    match (season, episode) {
        (Some(season), Some(episode)) => WantContext::TvEpisode {
            season,
            episode,
            episode_title,
            air_date,
            year,
        },
        (Some(season), None) => WantContext::TvSeason {
            season,
            year,
            episodes_needed: None,
        },
        (None, _) => {
            if year.is_some() {
                WantContext::Movie { year }
            } else {
                WantContext::Other
            }
        }
    }
}
