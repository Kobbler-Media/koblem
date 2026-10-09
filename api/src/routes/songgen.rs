//! songgen.cpp (LeVo2) music routes — the heavier, co-equal "music" engine,
//! offered as a tab on the music page alongside acestep. Auth-gated.
//!
//! Three modes:
//!   - `generate` (text→song): JSON body. Lyrics are SUNG as given; with a blank
//!     `lyric` and `lyrics_source = llm` the always-resident 9B writes them first.
//!     (songgen's LeLM only sings the lyrics it's given — it can't write them; a
//!     blank lyric with no 9B writer renders instrumental.)
//!   - `clone` (audio prompt → cloned style) and `continue` (extend a clip):
//!     multipart with either `vocal_stem` + `bgm_stem` stems, or a lone
//!     `full_mix` that songgen-server separates itself (htdemucs) before cloning.
//!
//! User-pasted lyrics are normalised into songgen's strict structured format
//! ([verse]/[chorus] sections joined by " ; ", phrases by ". ") before render.
//!
//! 🔴 **`generate` is a queued job; `clone` and `continue` are not.** `POST /generate` builds the
//! songgen-server body and hands it to the global GPU queue (202 + a job id, polled at
//! `GET /api/v1/songgen/jobs/{id}`); `queue::adapters::songgen` owns the render from there. The two
//! transform modes take their vocal / bgm / full-mix stems as multipart audio parts, and a queue
//! payload is JSON that may sit for hours — there is nowhere for those bytes to live, so they keep
//! the synchronous path: acquire the `music` slot (TTS preempts; STT/Vision and the peer acestep
//! engine get evicted), poll, download, save under `SONGS_DIR`, release. A preempt mid-render
//! returns 503 so the client can retry.

use std::time::Instant;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use koblem_client::types::{
    GenerationOutput, Job as JobRow, JobKind, JobState, MusicLyricsSource, MusicTrack,
    SonggenGenType, SonggenGenerateRequest, SonggenGenerateResponse, SonggenMode, SonggenProps,
};
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::gpu_jobs as queue_store;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::engine_cancel;
use crate::routes::tier2_helpers::{extract_caller, log_async, record_generation};
use crate::songgen::Error as SonggenError;
use koblem_client::types::{
    SonggenDraftLyricsResponse, SonggenFixRhymesRequest, SonggenSeparateResponse,
    SonggenSuggestRequest, SonggenSuggestion,
};

/// 256 MB cap — clone/continue accept long stem uploads.
const SONGGEN_BODY_LIMIT: usize = 256 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/songgen/generate", post(generate))
        .route("/api/v1/songgen/jobs/{id}", get(job))
        .route("/api/v1/songgen/active", get(active))
        .route("/api/v1/songgen/cancel", post(cancel))
        .route("/api/v1/songgen/clone", post(clone))
        .route("/api/v1/songgen/continue", post(continue_))
        .route("/api/v1/songgen/separate", post(separate))
        .route("/api/v1/songgen/suggest", post(suggest))
        .route("/api/v1/songgen/draft-lyrics", post(draft_lyrics))
        .route("/api/v1/songgen/fix-rhymes", post(fix_rhymes))
        .route("/api/v1/songgen/props", get(props))
        .route("/api/v1/songgen/file/{name}", get(serve_file))
        .layer(DefaultBodyLimit::max(SONGGEN_BODY_LIMIT))
}

// ── handlers ──────────────────────────────────────────────────────────

/// `POST /api/v1/songgen/generate` — text→song. JSON `SonggenGenerateRequest`. 202 and a job id.
///
/// 🔴 The 503 is gone. "songgen disabled or GPU unavailable" was the gate refusing rather than
/// queueing, and refusing is the behaviour this change exists to remove; what is left refuses only
/// what the render could never do — an unconfigured service, or a body with nothing to sing.
async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let bytes = match to_bytes(request.into_body(), SONGGEN_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
    };
    let mut req = match serde_json::from_slice::<SonggenGenerateRequest>(&bytes) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid request JSON: {e}"),
            );
        }
    };
    req.mode = SonggenMode::Generate;
    if req.description.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "description is required");
    }
    if !state.songgen.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "songgen service not configured (set SONGGEN_SERVER_URL or the DB setting)",
        );
    }

    // Resolved HERE, before the row exists: the seed is what the payload carries, what names the
    // file and what a good result is reproduced from, and a random one drawn at render time hours
    // later would agree with none of them.
    if req.seed.is_none_or(|s| s < 0) {
        req.seed = Some(i64::from(rand::random::<u32>()));
    }
    let mut lyric = normalize_lyrics_to_songgen(&req.lyric);
    // The 9B writes the words before anything queues — it is the ungated always-resident model, and
    // a payload that sat overnight has no LLM step left to run. songgen's LeLM cannot write lyrics,
    // so a failure renders instrumental rather than "falling back".
    if lyric.trim().is_empty() && req.lyrics_source == MusicLyricsSource::Llm {
        match write_songgen_lyrics(&state, &req.description, req.lyrics_prompt.as_deref(), None)
            .await
        {
            Ok(written) if !written.trim().is_empty() => lyric = written,
            Ok(_) => warn!("9B returned empty lyrics; songgen will render instrumental"),
            Err(e) => {
                warn!(error = %e, "9B lyric generation failed; songgen will render instrumental")
            }
        }
    }

    // 🔴 The payload IS the songgen-server body, so nothing may be added to it — a stray key rides
    // straight into `POST /generate`. `save_song` therefore has nowhere to live: the adapter keeps
    // and files every queued take, because a render nobody kept is GPU minutes for nothing.
    let payload = build_generate_body(&req, &lyric);
    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let enqueued = crate::routes::queue::enqueue(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers)
            .request(JobKind::Songgen, payload),
    )
    .await;
    match enqueued {
        Ok(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// `GET /api/v1/songgen/jobs/{id}` — poll a queued render.
async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such songgen job");
    };
    if row.kind != JobKind::Songgen {
        return err(StatusCode::NOT_FOUND, "that job is not a songgen render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/songgen/active` — the queued or running render, so a reloaded page can find the
/// job it lost without having kept the id.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    let found = live
        .into_iter()
        .chain(pending)
        .find(|j| j.kind == JobKind::Songgen);
    match found {
        Some(row) => Json(json!({ "job": view(&state, &row).await })).into_response(),
        None => Json(json!({ "job": Value::Null })).into_response(),
    }
}

/// `POST /api/v1/songgen/cancel` — cancel the in-flight render, through the dispatcher.
///
/// 🔴 Not straight at the engine: the dispatcher owns the run, so it is the only thing that can
/// stop it AND file the terminal state. A job in `collecting` is deliberately unreachable — the
/// wav is landing and the GPU cost is already paid.
async fn cancel(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        JobKind::Songgen,
        "no songgen render is queued or running",
    )
    .await
}

/// Render a queue row in the shape this page already speaks.
async fn view(state: &AppState, row: &JobRow) -> Value {
    let result = queue_store::detail(&state.db, &row.id)
        .await
        .ok()
        .flatten()
        .and_then(|(_, _, result)| result)
        .unwrap_or(Value::Null);
    json!({
        "id": row.id,
        // `collecting` has no equivalent in this engine's vocabulary, and the song is not saved
        // yet — the card is free but the page still has nothing to play.
        "state": match row.state {
            JobState::Queued | JobState::Held => "queued",
            JobState::Running | JobState::Collecting => "rendering",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        },
        "server_job_id": row.server_job_id,
        "phase": row.progress.phase,
        "tracks": result.get("tracks").cloned().unwrap_or(json!([])),
        "warnings": row.warnings,
        "error": row.error,
        "queued_ms": row.queued_ms,
        "gate_wait_ms": row.admit_ms,
        "duration_ms": row.render_ms,
        "blocked_reason": row.blocked_reason,
        "eta_start": row.eta_start,
    })
}

/// `POST /api/v1/songgen/clone` — clone an audio prompt's style. Multipart:
/// `request` JSON part + `vocal_stem` + `bgm_stem` (+ optional `full_mix`).
async fn clone(State(state): State<AppState>, headers: HeaderMap, mp: Multipart) -> Response {
    transform(state, headers, mp, SonggenMode::Clone).await
}

/// `POST /api/v1/songgen/continue` — extend an audio prompt (output is the
/// continuation only). Same multipart shape as `clone`.
async fn continue_(State(state): State<AppState>, headers: HeaderMap, mp: Multipart) -> Response {
    transform(state, headers, mp, SonggenMode::Continue).await
}

async fn transform(
    state: AppState,
    headers: HeaderMap,
    multipart: Multipart,
    mode: SonggenMode,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let parsed = match parse_multipart(multipart).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(mut req) = parsed.request else {
        return err(StatusCode::BAD_REQUEST, "missing `request` part");
    };
    req.mode = mode;
    // Accept either pre-split stems (vocal + bgm) OR a full mix that songgen-server
    // separates itself (htdemucs) before cloning. A lone `full_mix` is enough.
    let have_stems = parsed.vocal_stem.is_some() && parsed.bgm_stem.is_some();
    if !have_stems && parsed.full_mix.is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "clone/continue need both `vocal_stem` and `bgm_stem`, or a `full_mix` to auto-separate",
        );
    }
    run_pipeline(
        &state,
        &headers,
        req,
        parsed.vocal_stem,
        parsed.bgm_stem,
        parsed.full_mix,
    )
    .await
}

/// A `MusicTrack` for a saved separated-stem file (seed is irrelevant → -1).
fn stem_track(filename: &str, bytes: u64) -> MusicTrack {
    MusicTrack {
        url: format!("/api/v1/songgen/file/{filename}"),
        filename: filename.to_string(),
        seed: -1,
        content_type: "audio/wav".to_string(),
        bytes,
    }
}

/// `POST /api/v1/songgen/separate` — htdemucs-split a (trimmed) full mix into
/// vocal + bgm stems and return both as saved files. htdemucs is deterministic,
/// so this is EXACTLY the separation a `/clone` of the same audio runs internally
/// — a debugging view of what songgen's clone actually receives. Multipart: a
/// single `full_mix` audio file (the UI sends the same trimmed snippet clone
/// would). Synchronous on the server (no job) — a few seconds of GPU.
async fn separate(State(state): State<AppState>, headers: HeaderMap, mp: Multipart) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.songgen.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "songgen service not configured (set SONGGEN_SERVER_URL or the DB setting)",
        );
    }
    let parsed = match parse_multipart(mp).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some((mix, name)) = parsed.full_mix else {
        return err(
            StatusCode::BAD_REQUEST,
            "separate needs a `full_mix` audio file",
        );
    };
    // Separation is a quick htdemucs pass, but it runs on the GPU under
    // songgen-server's compute lock, so take the heavy `music` slot like a render
    // (drains the LLM, serialises vs Avatar/Flux/acestep). None = disabled.
    let Some((guard, gate_wait)) = state.songgen.acquire().await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "songgen disabled or GPU unavailable — try again later",
        );
    };
    let started = Instant::now();
    let outcome = state.songgen.separate(mix, &name, guard.chosen_gpu()).await;
    drop(guard);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let gate_wait_ms = gate_wait.as_millis() as u64;

    let (vocal, bgm) = match outcome {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "songgen separate failed");
            return err(StatusCode::BAD_GATEWAY, &format!("songgen-server: {e}"));
        }
    };

    // Random tag so two separates in the same wall-clock second don't collide
    // (the render path disambiguates with the seed; separation has none).
    let stamp = file_stamp();
    let tag = format!("{:04x}", rand::random::<u16>());
    let vocal_name = format!("{stamp}-songgen-sep-{tag}-vocal.wav");
    let bgm_name = format!("{stamp}-songgen-sep-{tag}-bgm.wav");
    if let Err(e) = save_song(&state, &vocal_name, &vocal).await {
        warn!(error = %e, "failed to save separated vocal stem");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to save vocal stem",
        );
    }
    if let Err(e) = save_song(&state, &bgm_name, &bgm).await {
        warn!(error = %e, "failed to save separated bgm stem");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "failed to save bgm stem");
    }

    Json(SonggenSeparateResponse {
        vocal: stem_track(&vocal_name, vocal.len() as u64),
        bgm: stem_track(&bgm_name, bgm.len() as u64),
        duration_ms: elapsed_ms,
        gate_wait_ms,
    })
    .into_response()
}

/// `GET /api/v1/songgen/props` — reachability for the UI to enable/disable the
/// songgen tab.
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.songgen.is_configured() {
        return Json(SonggenProps::default()).into_response();
    }
    let healthy = state.songgen.healthy().await;
    let version = if healthy {
        state.songgen.version().await
    } else {
        None
    };
    Json(SonggenProps { healthy, version }).into_response()
}

/// `GET /api/v1/songgen/file/{name}` — serve a rendered song from `SONGS_DIR`.
async fn serve_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "invalid filename");
    }
    let path = state.songs_dir.join(&name);
    match crate::routes::byte_serve::serve_file(&headers, &path, content_type_for(&name)).await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "song not found"),
    }
}

// ── LLM assist (write the description and/or lyrics in songgen's format) ──────

/// songgen LYRICS = one line, sections split by " ; ", structural tags, phrases
/// split by ". ". Getting this wrong degrades quality AND stops EOS from firing.
const SONGGEN_LYRICS_PROMPT: &str = "You write song lyrics for the songgen \
text-to-music model in its REQUIRED structured format. Breaking the format degrades \
quality and stops the model from ending the song, so follow it exactly:\n\
- Output ONE line. Separate sections with \" ; \" (space, semicolon, space).\n\
- Structural tags you may use: [intro-short], [intro-medium], [inst-short], \
[inst-medium], [outro-short], [outro-medium] (instrumental); [verse], [chorus], \
[bridge] (lyrical). Use ONLY these tags.\n\
- Lyrical tags ([verse], [chorus], [bridge]) are FOLLOWED by lyrics: write phrases \
separated by \". \" and end the section's final phrase with \".\" before the \" ;\".\n\
- Instrumental tags stand ALONE with no lyrics after them.\n\
- Use normal sentence capitalization — capitalize the first word of each phrase. Do \
NOT lowercase everything (the model is trained on sentence-case lyrics).\n\
- English only. The ONLY punctuation allowed is the period (.) to end/pause a \
phrase and the apostrophe (') inside words. For ANY pause, use a period — never a \
comma. NEVER use commas, hyphens or dashes, parentheses, brackets, question marks, \
exclamation marks, colons, quotes or emoji: LeVo sings them aloud and garbles the \
word (e.g. \"hand-in-hand\" breaks — write \"hand in hand\"). These punctuation rules \
are for the sung words only — the structural tags keep their brackets and hyphens \
(write [intro-short] exactly, never [intro short]). There is no backing-vocal, \
ad-lib or second-singer syntax — write every word you want sung as a plain phrase.\n\
- Rhyme by sound, not spelling. Matching letters is not a rhyme.\n\
- Fit the given music description; reuse a chorus for structure. Output ONLY the lyric \
line: no title, no commentary, no markdown, no quotes.\n\
Example:\n\
[intro-medium] ; [verse] Trails wind through the forest. Trees stand tall and honest. \
Moss covers the logs. Sunlight starts to glow. ; [chorus] Hold on to the light. Hold on \
through the night. ; [bridge] Every step a little closer now. ; [chorus] Hold on to the \
light. Hold on through the night. ; [outro-medium]";

/// One-pass idea -> description + lyrics, emitted with split markers (full lyrics
/// with [tags] break JSON escaping too often — markers are trivial to split).
const SONGGEN_SUGGEST_PROMPT: &str = "You turn the user's music idea into inputs for \
the songgen text-to-music model: a DESCRIPTION (comma-separated tags) and the song \
LYRICS (structured format).\n\n\
DESCRIPTION rules: one line of comma-separated TAGS, never sentences. Use songgen's \
four trained dimensions as short lowercase keywords: gender (male/female/mixed), genre, \
emotion/mood, and instruments — e.g. \"female, synth-pop, nostalgic, warm synths, drum \
machine, bass\". Prefer common standard tag words; ALWAYS include a vocal gender and \
2-4 instruments (these pin the sound). 6-12 tags. No tempo/bpm (not a trained tag). \
Capture the user's idea without adding a vibe they didn't ask for.\n\n\
LYRICS rules: ONE line; sections separated by \" ; \"; structural tags \
[intro-short]/[intro-medium]/[inst-short]/[inst-medium]/[outro-short]/[outro-medium] \
(instrumental) and [verse]/[chorus]/[bridge] (lyrical) — use ONLY these; lyrical tags \
([verse]/[chorus]/[bridge]) are followed by phrases separated by \". \" ending with \
\".\"; instrumental tags stand alone; use normal sentence capitalization (capitalize \
the first word of each phrase, do not lowercase everything). In the LYRICS the ONLY \
punctuation is the period (.) and the apostrophe (') inside words — for any pause use \
a period, NEVER a comma, hyphen/dash, parenthesis, question/exclamation mark or quote \
(LeVo sings them aloud; write compound words like \"hand in hand\", not \
\"hand-in-hand\") — this is for the sung words only; the structural tags above keep \
their hyphens (write [intro-short], never [intro short]). For an instrumental idea, \
use only instrumental tags.\n\n\
Output EXACTLY this layout and nothing else — no commentary, no markdown:\n\
###DESCRIPTION###\n\
<the comma-separated tags>\n\
###LYRICS###\n\
<the structured lyric line>";

/// Description-only variant — write just the description; the user supplies the
/// lyrics (or leaves them blank for an instrumental).
const SONGGEN_SUGGEST_DESC_ONLY_PROMPT: &str = "You turn the user's music idea into a \
songgen DESCRIPTION — a single line of comma-separated TAGS (never sentences) using \
songgen's four trained dimensions as short lowercase keywords: gender (male/female/mixed), \
genre, emotion/mood, instruments — e.g. \"female, synth-pop, nostalgic, warm synths, \
drum machine, bass\". Prefer common standard tag words; ALWAYS include a vocal gender and \
2-4 instruments. 6-12 tags. No tempo/bpm (not a trained tag). Capture the idea without \
adding a vibe they didn't ask for.\n\n\
Output EXACTLY this and nothing else — no lyrics, no commentary, no markdown:\n\
###DESCRIPTION###\n\
<the comma-separated tags>";

/// `POST /api/v1/songgen/suggest` — expand a one-liner idea into a songgen
/// description (+ optionally lyrics) with the main 9B, to pre-fill the form.
/// Ungated — the LLM is the always-resident model.
async fn suggest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SonggenSuggestRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if req.brief.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "brief is required");
    }
    let (system, max_tokens) = if req.description_only {
        (SONGGEN_SUGGEST_DESC_ONLY_PROMPT, 300)
    } else {
        (SONGGEN_SUGGEST_PROMPT, 1200)
    };
    let mut llm = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": req.brief},
        ],
        "max_tokens": max_tokens,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_songgen", &mut llm).await;
    let resp = match state.llama.chat_completion_json(&llm).await {
        Ok((_permit, r)) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("suggest failed: {e}")),
    };
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let suggestion = parse_songgen_suggestion(content);
    if suggestion.description.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "model returned no description");
    }
    Json(suggestion).into_response()
}

/// `POST /api/v1/songgen/draft-lyrics` — write songgen-format lyrics for a given
/// description (+ optional theme) with the main 9B (no render). Ungated.
async fn draft_lyrics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SonggenGenerateRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let refine = crate::routes::refine::Refine::from_opts(
        req.refine_source.as_deref(),
        req.refine_instruction.as_deref(),
    );
    // Clone/continue hide the description (style comes from the audio prompt), so a
    // theme alone is enough; a refine carries its own source. Only reject when
    // there's nothing at all to write from.
    if refine.is_none()
        && lyric_draft_user_msg(&req.description, req.lyrics_prompt.as_deref()).is_none()
    {
        return err(
            StatusCode::BAD_REQUEST,
            "a description or a theme is required",
        );
    }
    match write_songgen_lyrics(
        &state,
        &req.description,
        req.lyrics_prompt.as_deref(),
        refine,
    )
    .await
    {
        Ok(lyrics) => Json(draft_response(lyrics)).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("lyric draft failed: {e}")),
    }
}

/// Attach the advisory rhyme flags. Reporting only — the fix is the user's call.
fn draft_response(lyrics: String) -> SonggenDraftLyricsResponse {
    let rhyme_flags = crate::routes::music::to_flags(&crate::rhyme::eye_rhymes(&lyrics));
    SonggenDraftLyricsResponse {
        lyrics,
        rhyme_flags,
    }
}

/// `POST /api/v1/songgen/fix-rhymes` — rewrite only the non-rhyming line endings.
/// Driven by the UI's Fix button; re-checks and returns whatever is left, so a
/// stubborn pair shows up again rather than looping the model.
async fn fix_rhymes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SonggenFixRhymesRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if req.lyrics.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "lyrics are required");
    }
    let flags = crate::rhyme::eye_rhymes(&req.lyrics);
    if flags.is_empty() {
        return Json(draft_response(req.lyrics)).into_response();
    }
    let system = match req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(d) => format!("{SONGGEN_LYRICS_PROMPT}\nMusic description: {d}"),
        None => SONGGEN_LYRICS_PROMPT.to_string(),
    };
    match crate::routes::music::fix_lyric_rhymes(&state, &system, &req.lyrics, &flags).await {
        // The fix pass can reflow sections; put it back in songgen's one-line format.
        Ok(lyrics) => Json(draft_response(sanitize_songgen_lyrics(&lyrics))).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("rhyme fix failed: {e}")),
    }
}

/// Build the user message for a lyric draft from a music `description` (comma
/// tags) and/or a `theme` (what the words are about). Clone/continue supply only
/// a theme — the audio prompt carries the style — so the description may be empty.
/// Returns `None` when both are blank (nothing to write from).
fn lyric_draft_user_msg(description: &str, theme: Option<&str>) -> Option<String> {
    let desc = description.trim();
    let theme = theme.map(str::trim).filter(|s| !s.is_empty());
    match (desc.is_empty(), theme) {
        (false, Some(t)) => Some(format!(
            "Music description: {desc}\n\nWrite lyrics about: {t}"
        )),
        (false, None) => Some(format!("Write lyrics for this music:\n\n{desc}")),
        (true, Some(t)) => Some(format!("Write song lyrics about: {t}")),
        (true, None) => None,
    }
}

/// Ask the main 9B to write songgen-format lyrics. `description` is the music
/// (comma tags); `theme` (optional) is what the words should be about. Returns
/// the single-line ` ; `-separated lyric string. Called before the GPU lock —
/// the LLM is the ungated always-resident model.
async fn write_songgen_lyrics(
    state: &AppState,
    description: &str,
    theme: Option<&str>,
    refine: Option<crate::routes::refine::Refine<'_>>,
) -> Result<String, crate::llama::Error> {
    let user = match &refine {
        // Improve an existing draft under songgen's structured lyric rules; the
        // description/theme ride along as context so the revision stays in style.
        Some(r) => {
            let desc = description.trim();
            let mut context = String::new();
            if !desc.is_empty() {
                context.push_str(&format!("Music description: {desc}"));
            }
            if let Some(t) = theme.map(str::trim).filter(|s| !s.is_empty()) {
                if !context.is_empty() {
                    context.push('\n');
                }
                context.push_str(&format!("Theme: {t}"));
            }
            crate::routes::refine::refine_user_msg("lyric draft", &context, r)
        }
        None => lyric_draft_user_msg(description, theme)
            .unwrap_or_else(|| "Write song lyrics.".to_string()),
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": SONGGEN_LYRICS_PROMPT},
            {"role": "user", "content": user},
        ],
        "max_tokens": 700,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_songgen", &mut body).await;
    let (_permit, resp) = state.llama.chat_completion_json(&body).await?;
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(sanitize_songgen_lyrics(content))
}

/// Collapse an LLM lyric reply to songgen's single-line format. Models sometimes
/// emit one section per line; join them with " ; " when a line starts a new
/// `[tag]`, otherwise a space. Idempotent on already-correct single-line output.
fn sanitize_songgen_lyrics(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if out.is_empty() {
            out.push_str(line);
        } else if line.starts_with('[') {
            out.push_str(" ; ");
            out.push_str(line);
        } else {
            out.push(' ');
            out.push_str(line);
        }
    }
    out.trim().trim_matches('"').trim().to_string()
}

/// Split a `###DESCRIPTION### … ###LYRICS### …` reply. Tolerant: no LYRICS marker
/// → the whole reply (sans the DESCRIPTION marker) is the description.
fn parse_songgen_suggestion(content: &str) -> SonggenSuggestion {
    const DESC: &str = "###DESCRIPTION###";
    const LYR: &str = "###LYRICS###";
    match content.find(LYR) {
        Some(li) => {
            let description = content[..li].replacen(DESC, "", 1).trim().to_string();
            let lyrics = sanitize_songgen_lyrics(content[li + LYR.len()..].trim());
            SonggenSuggestion {
                description,
                lyrics: (!lyrics.is_empty()).then_some(lyrics),
            }
        }
        None => SonggenSuggestion {
            description: content.replacen(DESC, "", 1).trim().to_string(),
            lyrics: None,
        },
    }
}

// ── lyrics normalisation (free-form → songgen's structured format) ────────────
//
// songgen (LeVo2) requires a strict lyric shape (upstream README "Lyrics Input
// Format"): sections tagged with structural markers and separated by " ; ";
// lyrical tags ([verse]/[chorus]/[bridge]) carry phrases separated by ". " whose
// final phrase MUST end with "."; instrumental tags ([intro-*]/[inst-*]/[outro-*],
// [silence]) stand alone; English half-width punctuation only. Getting it wrong
// degrades quality and can stop EOS from firing. `normalize_lyrics_to_songgen`
// converts loosely-formatted lyrics (headers like "[Verse 1]", "Chorus:",
// "Pre-Chorus", "Intro", plain line-broken verses) into that shape and is
// idempotent on already-correct input (parse → re-emit yields the same string).

/// Resolve a section keyword head + length hint to a songgen structural tag. The
/// tokenizer accepts the *-short/medium/long variants and [silence] (a superset
/// of the README's documented set).
fn songgen_tag_for(head: &str, long: bool, medium: bool) -> Option<&'static str> {
    Some(match head {
        "intro" => {
            if long {
                "intro-long"
            } else if medium {
                "intro-medium"
            } else {
                "intro-short"
            }
        }
        "outro" | "ending" | "end" => {
            if long {
                "outro-long"
            } else if medium {
                "outro-medium"
            } else {
                "outro-short"
            }
        }
        "inst" | "instr" | "instrumental" | "interlude" | "solo" | "break" => {
            if long {
                "inst-long"
            } else if medium {
                "inst-medium"
            } else {
                "inst-short"
            }
        }
        "verse" => "verse",
        "chorus" | "hook" | "refrain" => "chorus",
        "bridge" => "bridge",
        "pre" | "prechorus" => "bridge", // songgen has no pre-chorus; bridge is closest
        "silence" | "silent" => "silence",
        _ => return None,
    })
}

fn is_lyrical_tag(tag: &str) -> bool {
    matches!(tag, "verse" | "chorus" | "bridge")
}

/// The exact canonical songgen tags (lowercase), used to recognise input that is
/// already correctly formatted.
const SONGGEN_CANONICAL_TAGS: &[&str] = &[
    "intro-short",
    "intro-medium",
    "intro-long",
    "verse",
    "chorus",
    "bridge",
    "inst-short",
    "inst-medium",
    "inst-long",
    "outro-short",
    "outro-medium",
    "outro-long",
    "silence",
];

/// True when the input is already in songgen's structured format: a single line
/// whose every bracketed tag is an exact canonical tag (and there is at least
/// one). Such input — e.g. the 9B's own output, or hand-written correct lyrics,
/// including Chinese-lyric formatting — is accepted verbatim rather than reparsed
/// (reparsing could split an in-lyric "Mr." or force a trailing period).
fn is_already_songgen(input: &str) -> bool {
    let t = input.trim();
    if t.is_empty() || t.contains('\n') {
        return false;
    }
    let mut saw_tag = false;
    let mut rest = t;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            return false; // stray '[' with no closer → not clean songgen
        };
        if !SONGGEN_CANONICAL_TAGS.contains(&&after[..close]) {
            return false;
        }
        saw_tag = true;
        rest = &after[close + 1..];
    }
    saw_tag
}

/// Map a header string (bracket/colon-stripped, e.g. "verse", "Verse 1",
/// "intro-medium") to a songgen tag, honouring an explicit length word.
fn map_header_word(raw: &str) -> Option<&'static str> {
    let lowered = raw
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '/', '-'], " ");
    let norm = lowered.split_whitespace().collect::<Vec<_>>().join(" ");
    let head = norm.split_whitespace().next()?;
    let long = norm.split_whitespace().any(|w| w == "long");
    let medium = norm.split_whitespace().any(|w| w == "medium" || w == "med");
    songgen_tag_for(head, long, medium)
}

/// Strip the decoration around a bare header line ("(Chorus)", "** Verse 1 **",
/// "Chorus:") down to the section words.
fn strip_header_punct(line: &str) -> String {
    line.trim()
        .trim_matches(|c: char| {
            matches!(
                c,
                '[' | ']' | '(' | ')' | '{' | '}' | '*' | '#' | ':' | '.' | '"' | '\'' | ' '
            )
        })
        .trim()
        .to_string()
}

/// A bare word recognised as part of a section header.
fn is_section_word(t: &str) -> bool {
    matches!(
        t,
        "intro"
            | "outro"
            | "ending"
            | "end"
            | "inst"
            | "instr"
            | "instrumental"
            | "interlude"
            | "solo"
            | "break"
            | "verse"
            | "chorus"
            | "hook"
            | "refrain"
            | "bridge"
            | "pre"
            | "prechorus"
            | "silence"
            | "silent"
    )
}

/// A header qualifier: a section number ("1", "2)"), a roman numeral, or a length word.
fn is_qualifier_token(t: &str) -> bool {
    let t = t.trim_matches(|c: char| matches!(c, '.' | ')' | ':'));
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
        || matches!(t, "short" | "medium" | "med" | "long")
        || matches!(
            t,
            "i" | "ii" | "iii" | "iv" | "v" | "vi" | "vii" | "viii" | "ix" | "x"
        )
}

/// A bare header line names a section AND contains nothing but section words and
/// qualifiers — so "Verse 1"/"Pre-Chorus"/"Inst Long" are headers, but a lyric
/// line like "Bridge over troubled water" or "Break my heart" is not.
fn looks_like_bare_header(line: &str) -> bool {
    let s = strip_header_punct(line);
    if map_header_word(&s).is_none() {
        return false;
    }
    let lower = s.to_ascii_lowercase().replace(['_', '/', '-'], " ");
    let toks: Vec<&str> = lower.split_whitespace().collect();
    !toks.is_empty()
        && toks
            .iter()
            .all(|t| is_section_word(t) || is_qualifier_token(t))
}

/// Normalise full-width / Chinese punctuation to English half-width (songgen
/// rejects non-English marks).
fn to_english_punct(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '。' | '；' => '.', // full-width period / semicolon → phrase break
            '，' | '、' => ',',
            '：' => ':',
            '！' => '!',
            '？' => '?',
            '（' => '(',
            '）' => ')',
            '“' | '”' | '《' | '》' => '"',
            '‘' | '’' => '\'',
            '—' | '–' | '－' => '-',
            other => other,
        })
        .collect()
}

/// Fold punctuation LeVo can't voice onto its only in-line separator (the period
/// = a sung pause/breath) or onto spaces, WITHOUT touching structure (`;` section
/// separators, `[tags]`) or existing periods (so abbreviations like "Mr." and
/// "U.S.A." survive). LeVo was trained with the period as the sole phrase break;
/// a literal comma is sung aloud as a word, and parentheses/`?`/`!`/`:`/`"` are
/// out-of-distribution. So:
///   `,` `?` `!`              → ". "  (the pause the writer intended)
///   `(` `)` `:` `"` `-`/dash → " "   (asides / hyphen-joins — LeVo sings a hyphen
///                                      aloud then garbles, e.g. "hand-in-hand")
/// This runs on every path, including already-songgen input we'd otherwise pass
/// through verbatim, so hand-pasted lyrics with commas get fixed too.
fn scrub_inline_punct(input: &str) -> String {
    let mut s = String::with_capacity(input.len());
    let mut prev_space = false;
    let mut in_tag = false;
    for ch in input.chars() {
        // Structural tags ([intro-short], [inst-long], ...) are canonical and
        // legitimately contain hyphens — pass their contents through verbatim so
        // the de-hyphenation below only folds hyphens in lyric text.
        if in_tag {
            s.push(ch);
            prev_space = false;
            if ch == ']' {
                in_tag = false;
            }
            continue;
        }
        match ch {
            '[' => {
                s.push('[');
                prev_space = false;
                in_tag = true;
            }
            ',' | '?' | '!' => {
                while s.ends_with(' ') {
                    s.pop();
                }
                if !s.is_empty() && !s.ends_with(['.', ';', '[']) {
                    s.push('.');
                }
                s.push(' ');
                prev_space = true;
            }
            '(' | ')' | ':' | '"' | '-' | '\u{2010}' | '\u{2011}' | '\u{2013}' | '\u{2014}'
            | '\u{FF0D}' | '\u{201C}' | '\u{201D}' => {
                if !prev_space && !s.is_empty() {
                    s.push(' ');
                    prev_space = true;
                }
            }
            // Collapse runs of periods (ellipses, or a stray "." after a folded
            // comma) to a single phrase break — LeVo dislikes repeated marks.
            '.' => {
                if !s.ends_with('.') {
                    s.push('.');
                }
                prev_space = false;
            }
            c if c.is_whitespace() => {
                if !prev_space && !s.is_empty() {
                    s.push(' ');
                    prev_space = true;
                }
            }
            c => {
                s.push(c);
                prev_space = false;
            }
        }
    }
    s.trim().to_string()
}

/// Clean one lyric phrase: drop structure marks (`[`, `]`, `;`) that can't appear
/// inside a phrase, fold un-voiceable punctuation onto periods/spaces (see
/// [`scrub_inline_punct`]), and trim trailing separators (the ". " joins and the
/// final "." are re-added by the emitter).
fn sanitize_phrase(raw: &str) -> String {
    let no_struct: String = raw
        .chars()
        .map(|c| if matches!(c, '[' | ']' | ';') { ' ' } else { c })
        .collect();
    scrub_inline_punct(&no_struct)
        .trim_end_matches(['.', ' '])
        .trim()
        .to_string()
}

/// Convert loosely-formatted lyrics into songgen's required single-line format.
/// Empty in → empty out (the caller renders instrumental or has the 9B write them).
pub(crate) fn normalize_lyrics_to_songgen(input: &str) -> String {
    // Already correctly formatted (incl. the 9B's own output) → keep its structure
    // verbatim, but still scrub un-voiceable punctuation (a hand-pasted single-line
    // lyric can be canonically tagged yet riddled with commas/parens). This is
    // tag- and separator-preserving, so the structure is untouched.
    if is_already_songgen(input) {
        return scrub_inline_punct(input.trim());
    }
    let cleaned = to_english_punct(input.trim());
    if cleaned.is_empty() {
        return String::new();
    }

    struct Section {
        tag: &'static str,
        phrases: Vec<String>,
    }
    // One input line (or one bracket's inline tail) = one phrase. We deliberately
    // do NOT split on "." — internal periods ("Mr. Jones", "U.S.A.") are preserved
    // and read as phrase breaks in the joined output all the same, so splitting
    // would only corrupt abbreviations.
    fn push_phrase(sec: &mut Section, body: &str) {
        let p = sanitize_phrase(body);
        if !p.is_empty() {
            sec.phrases.push(p);
        }
    }

    // Put every bracketed tag at the start of its own line so single-line songgen
    // input ("... ; [chorus] ...") and multi-line input split identically.
    let exploded = cleaned.replace('[', "\n[");
    let mut sections: Vec<Section> = Vec::new();

    for line in exploded.lines() {
        let line = line.trim().trim_end_matches(';').trim(); // drop the " ; " separator
        if line.is_empty() {
            continue;
        }

        // Bracketed tag, optionally with inline lyrics ("[verse] sing this.").
        if let Some(rest) = line.strip_prefix('[') {
            let (inner, after) = rest.split_once(']').unwrap_or((rest, ""));
            if let Some(tag) = map_header_word(inner) {
                sections.push(Section {
                    tag,
                    phrases: Vec::new(),
                });
                if is_lyrical_tag(tag) {
                    push_phrase(sections.last_mut().unwrap(), after);
                }
                continue;
            }
            // unknown bracket → fall through and treat the line as lyric text
        }

        // Bare header line: "Verse 1", "Chorus:", "Pre-Chorus".
        if looks_like_bare_header(line) {
            let tag = map_header_word(&strip_header_punct(line)).unwrap();
            sections.push(Section {
                tag,
                phrases: Vec::new(),
            });
            continue;
        }

        // Lyric content. Open an implicit [verse] when words arrive with no open
        // lyrical section (song starts straight into lyrics, or after an intro).
        if !matches!(sections.last(), Some(s) if is_lyrical_tag(s.tag)) {
            sections.push(Section {
                tag: "verse",
                phrases: Vec::new(),
            });
        }
        push_phrase(sections.last_mut().unwrap(), line);
    }

    let mut out: Vec<String> = Vec::new();
    for sec in &sections {
        if is_lyrical_tag(sec.tag) {
            if sec.phrases.is_empty() {
                continue; // a lyrical tag with no lyrics is invalid — drop it
            }
            out.push(format!("[{}] {}.", sec.tag, sec.phrases.join(". ")));
        } else {
            out.push(format!("[{}]", sec.tag));
        }
    }
    out.join(" ; ")
}

// ── pipeline ──────────────────────────────────────────────────────────

/// The synchronous clone/continue path. 🔴 Not queued, and not queueable: the stems are multipart
/// audio parts and a queue payload is JSON that may sit for hours.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    state: &AppState,
    headers: &HeaderMap,
    mut req: SonggenGenerateRequest,
    vocal_stem: Option<(Bytes, String)>,
    bgm_stem: Option<(Bytes, String)>,
    full_mix: Option<(Bytes, String)>,
) -> Response {
    if !state.songgen.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "songgen service not configured (set SONGGEN_SERVER_URL or the DB setting)",
        );
    }

    // Resolve the seed ONCE, up front, so the value sent to the server, the
    // filename, and the recorded metadata all agree. An unset (or negative
    // "random") seed becomes a fresh concrete value — previously unset seeds were
    // hardcoded to 1234 server-side while being recorded as -1/"random", so every
    // default generation rendered the identical song yet was labelled random. We
    // persist the concrete seed so a good result can be reproduced.
    if req.seed.is_none_or(|s| s < 0) {
        req.seed = Some(i64::from(rand::random::<u32>()));
    }

    let caller = extract_caller(headers);
    // Normalise whatever lyrics the user pasted into songgen's required format
    // (idempotent on already-correct input); blank stays blank so the 9B / the
    // instrumental path below still apply.
    let lyric = normalize_lyrics_to_songgen(&req.lyric);

    // Acquire the heavy slot (drains the LLM, serialises vs Avatar/Flux); None =
    // songgen administratively disabled.
    let Some((guard, gate_wait)) = state.songgen.acquire().await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "songgen disabled or GPU unavailable — try again later",
        );
    };
    let started = Instant::now();

    let outcome = render(
        state,
        &req,
        &lyric,
        vocal_stem,
        bgm_stem,
        full_mix,
        guard.chosen_gpu(),
    )
    .await;

    // Always release the GPU before doing disk/DB work.
    drop(guard);

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let gate_wait_ms = gate_wait.as_millis() as u64;

    let (content_type, audio) = match outcome {
        Ok(v) => v,
        Err(SonggenError::Preempted) => {
            log_songgen(
                state,
                caller,
                &req,
                gate_wait_ms,
                elapsed_ms,
                503,
                json!({"error": "preempted"}),
            );
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "render preempted by a higher-priority GPU consumer — retry shortly",
            );
        }
        Err(e) => {
            warn!(error = %e, "songgen render failed");
            log_songgen(
                state,
                caller,
                &req,
                gate_wait_ms,
                elapsed_ms,
                502,
                json!({"error": e.to_string()}),
            );
            return err(StatusCode::BAD_GATEWAY, &format!("songgen-server: {e}"));
        }
    };

    // Persist the rendered song under SONGS_DIR (shared with acestep).
    let seed = req.seed.unwrap_or(-1);
    let ext = ext_for_content_type(&content_type);
    let filename = format!("{}-songgen-{}.{ext}", file_stamp(), short_seed(seed));
    if let Err(e) = save_song(state, &filename, &audio).await {
        warn!(error = %e, %filename, "failed to save song");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to save rendered song",
        );
    }

    let track = MusicTrack {
        url: format!("/api/v1/songgen/file/{filename}"),
        filename: filename.clone(),
        seed,
        content_type: content_type.clone(),
        bytes: audio.len() as u64,
    };

    let resp = SonggenGenerateResponse {
        tracks: vec![track.clone()],
        duration_used: req.duration,
        lyrics_used: (!lyric.trim().is_empty()).then(|| lyric.clone()),
        duration_ms: Some(elapsed_ms),
        gate_wait_ms: Some(gate_wait_ms),
        gpu_log_id: None,
    };
    log_songgen(
        state,
        caller,
        &req,
        gate_wait_ms,
        elapsed_ms,
        200,
        json!({"filename": filename, "bytes": track.bytes}),
    );

    // Gallery record — service "music" (shared gallery), endpoint marks engine+mode.
    if req.save_song {
        record_generation(
            state,
            "music",
            endpoint_label(req.mode),
            crate::routes::tier2_helpers::meta_from_request(
                &req,
                &[
                    ("engine", json!("songgen")),
                    ("lyrics_used", json!(resp.lyrics_used)),
                    ("had_audio_inputs", json!(req.mode != SonggenMode::Generate)),
                ],
            ),
            vec![GenerationOutput {
                filename: track.filename.clone(),
                mime: track.content_type.clone(),
                size_bytes: track.bytes as i64,
                kind: "audio".to_string(),
            }],
        )
        .await;
    }

    Json(resp).into_response()
}

/// Drive the songgen async job to a downloaded wav. Returns `(content_type, bytes)`.
#[allow(clippy::too_many_arguments)]
async fn render(
    state: &AppState,
    req: &SonggenGenerateRequest,
    lyric: &str,
    vocal_stem: Option<(Bytes, String)>,
    bgm_stem: Option<(Bytes, String)>,
    full_mix: Option<(Bytes, String)>,
    gpu: Option<&str>,
) -> Result<(String, Bytes), SonggenError> {
    use crate::routes::tier2_helpers::push_gpu_field;
    let client = &state.songgen;
    let path = if req.mode == SonggenMode::Clone {
        "/clone"
    } else {
        "/continue"
    };
    let mut fields = build_stem_fields(req, lyric);
    push_gpu_field(&mut fields, gpu);
    let job_id = client
        .submit_stems(path, &fields, vocal_stem, bgm_stem, full_mix)
        .await?;
    client.await_job(&job_id).await?;
    client.download(&job_id).await
}

// ── request building ────────────────────────────────────────────────────

fn gen_type_str(t: SonggenGenType) -> &'static str {
    match t {
        SonggenGenType::Mixed => "mixed",
        SonggenGenType::Vocal => "vocal",
        SonggenGenType::Bgm => "bgm",
    }
}

/// JSON body for songgen-server `POST /generate`. Omits unset optionals so the
/// server applies its own defaults; `duration` is omitted when unset so songgen
/// renders to its model-max and stops on EOS (length is lyric-driven).
fn build_generate_body(req: &SonggenGenerateRequest, lyric: &str) -> Value {
    let mut body = json!({
        "lyric": lyric,
        "description": req.description,
        "gen_type": gen_type_str(req.gen_type),
        "seed": req.seed.unwrap_or(1234),
    });
    let obj = body.as_object_mut().unwrap();
    if let Some(d) = req.duration.filter(|d| *d > 0.0) {
        obj.insert("duration".into(), json!(d));
    }
    if let Some(t) = req.temp {
        obj.insert("temp".into(), json!(t));
    }
    if let Some(k) = req.top_k {
        obj.insert("top_k".into(), json!(k));
    }
    if let Some(c) = req.cfg {
        obj.insert("cfg".into(), json!(c));
    }
    if let Some(f) = req.fade {
        obj.insert("fade".into(), json!(f));
    }
    body
}

/// Multipart text fields for `POST /clone|/continue` — same knobs as generate,
/// as form strings (the server reads them with `form_str`/`form_has`).
fn build_stem_fields(req: &SonggenGenerateRequest, lyric: &str) -> Vec<(String, String)> {
    let mut f: Vec<(String, String)> = vec![
        ("lyric".into(), lyric.to_string()),
        ("description".into(), req.description.clone()),
        ("gen_type".into(), gen_type_str(req.gen_type).into()),
        ("seed".into(), req.seed.unwrap_or(1234).to_string()),
    ];
    if let Some(d) = req.duration.filter(|d| *d > 0.0) {
        f.push(("duration".into(), d.to_string()));
    }
    if let Some(t) = req.temp {
        f.push(("temp".into(), t.to_string()));
    }
    if let Some(k) = req.top_k {
        f.push(("top_k".into(), k.to_string()));
    }
    if let Some(c) = req.cfg {
        f.push(("cfg".into(), c.to_string()));
    }
    if let Some(fade) = req.fade {
        f.push(("fade".into(), fade.to_string()));
    }
    f
}

// ── helpers ───────────────────────────────────────────────────────────

/// Fields pulled from a songgen multipart body.
struct ParsedSonggen {
    request: Option<SonggenGenerateRequest>,
    vocal_stem: Option<(Bytes, String)>,
    bgm_stem: Option<(Bytes, String)>,
    full_mix: Option<(Bytes, String)>,
}

async fn parse_multipart(mut mp: Multipart) -> Result<ParsedSonggen, Response> {
    let mut out = ParsedSonggen {
        request: None,
        vocal_stem: None,
        bgm_stem: None,
        full_mix: None,
    };
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                let filename = field.file_name().map(|s| s.to_string());
                match name.as_str() {
                    "request" => {
                        let t = field.text().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading request field: {e}"),
                            )
                        })?;
                        let r =
                            serde_json::from_str::<SonggenGenerateRequest>(&t).map_err(|e| {
                                err(
                                    StatusCode::BAD_REQUEST,
                                    &format!("invalid request JSON: {e}"),
                                )
                            })?;
                        out.request = Some(r);
                    }
                    "vocal_stem" => {
                        let b = field.bytes().await.map_err(|e| {
                            err(StatusCode::BAD_REQUEST, &format!("reading vocal_stem: {e}"))
                        })?;
                        out.vocal_stem = Some((b, filename.unwrap_or_else(|| "vocal.wav".into())));
                    }
                    "bgm_stem" => {
                        let b = field.bytes().await.map_err(|e| {
                            err(StatusCode::BAD_REQUEST, &format!("reading bgm_stem: {e}"))
                        })?;
                        out.bgm_stem = Some((b, filename.unwrap_or_else(|| "bgm.wav".into())));
                    }
                    "full_mix" => {
                        let b = field.bytes().await.map_err(|e| {
                            err(StatusCode::BAD_REQUEST, &format!("reading full_mix: {e}"))
                        })?;
                        out.full_mix = Some((b, filename.unwrap_or_else(|| "mix.wav".into())));
                    }
                    _ => {
                        let _ = field.bytes().await;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    &format!("malformed multipart: {e}"),
                ));
            }
        }
    }
    Ok(out)
}

/// Stable endpoint label for a songgen mode (gallery + gpu_logs).
fn endpoint_label(mode: SonggenMode) -> &'static str {
    match mode {
        SonggenMode::Generate => "songgen-generate",
        SonggenMode::Clone => "songgen-clone",
        SonggenMode::Continue => "songgen-continue",
    }
}

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

async fn save_song(state: &AppState, filename: &str, bytes: &Bytes) -> std::io::Result<()> {
    tokio::fs::create_dir_all(&*state.songs_dir).await?;
    let path = state.songs_dir.join(filename);
    tokio::fs::write(path, bytes).await
}

#[allow(clippy::too_many_arguments)]
fn log_songgen(
    state: &AppState,
    caller: Option<String>,
    req: &SonggenGenerateRequest,
    gate_wait_ms: u64,
    duration_ms: u64,
    status: i32,
    response_meta: Value,
) {
    let endpoint = endpoint_label(req.mode);
    log_async(
        state.db.clone(),
        CreateLog {
            service: "music".into(),
            endpoint: endpoint.into(),
            request_meta: json!({
                "engine": "songgen",
                "mode": endpoint,
                "description": req.description,
                "gen_type": gen_type_str(req.gen_type),
            }),
            response_meta,
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: duration_ms as i64,
            status,
            caller,
        },
    );
}

pub(crate) fn ext_for_content_type(ct: &str) -> &'static str {
    if ct.contains("mpeg") { "mp3" } else { "wav" }
}

fn content_type_for(name: &str) -> &'static str {
    if name.ends_with(".mp3") {
        "audio/mpeg"
    } else {
        "audio/wav"
    }
}

/// Compact UTC timestamp for filenames: `20260607-134501`.
fn file_stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Short, filename-safe seed tag (`r` for random/-1).
fn short_seed(seed: i64) -> String {
    if seed < 0 {
        "r".to_string()
    } else {
        seed.to_string()
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::lyric_draft_user_msg;
    use super::normalize_lyrics_to_songgen as norm;

    #[test]
    fn empty_stays_empty() {
        assert_eq!(norm(""), "");
        assert_eq!(norm("   \n  "), "");
    }

    #[test]
    fn idempotent_on_correct_format() {
        let s = "[intro-short] ; [verse] walking through the rain. neon calls my name. ; \
[chorus] hold on to the light. hold on through the night. ; [outro-short]";
        assert_eq!(norm(s), s);
        assert_eq!(norm(&norm(s)), s); // double-apply == single
    }

    #[test]
    fn already_formatted_passes_through_verbatim() {
        // exact-tag single-line input is accepted as-is, even with in-lyric periods
        let s = "[verse] Mr. Jones walks alone. he waits for dawn. ; [chorus] oh oh.";
        assert_eq!(norm(s), s);
        // leading/trailing whitespace is trimmed but content untouched
        assert_eq!(norm("  [inst-long]  "), "[inst-long]");
    }

    #[test]
    fn capitalised_or_multiline_tags_are_normalised() {
        assert_eq!(norm("[Verse]\nhi there"), "[verse] hi there.");
    }

    #[test]
    fn converts_freeform_headers() {
        let input = "[Verse 1]\nWalking through the city in the rain\nNeon signs they call my name\n\n\
[Chorus]\nHold on to the light\nHold on through the night";
        assert_eq!(
            norm(input),
            "[verse] Walking through the city in the rain. Neon signs they call my name. ; \
[chorus] Hold on to the light. Hold on through the night."
        );
    }

    #[test]
    fn maps_aliases_and_lengths() {
        assert_eq!(norm("Intro\nla la"), "[intro-short] ; [verse] la la.");
        assert_eq!(norm("Hook\nyeah"), "[chorus] yeah.");
        assert_eq!(norm("Pre-Chorus\nrising"), "[bridge] rising.");
        assert_eq!(
            norm("[inst-long]\n[Outro Medium]"),
            "[inst-long] ; [outro-medium]"
        );
    }

    #[test]
    fn bare_lyrics_become_a_verse() {
        assert_eq!(
            norm("just some words\nmore words"),
            "[verse] just some words. more words."
        );
    }

    #[test]
    fn final_phrase_gets_a_period_and_strips_chinese_punct() {
        // free-form (not already-songgen) path: full-width punctuation normalised
        // to half-width; the comma then folds to a period (LeVo's only pause).
        assert_eq!(norm("Verse 1\nhello，world。"), "[verse] hello. world.");
    }

    #[test]
    fn commas_become_periods_not_spoken_words() {
        // LeVo sings a literal comma aloud — fold it to the period (a sung pause).
        assert_eq!(
            norm("[Verse]\nOne, two, three, tap your feet"),
            "[verse] One. two. three. tap your feet."
        );
    }

    #[test]
    fn hyphens_become_spaces() {
        // LeVo garbles hyphenated words ("hand-in-hand" -> "hand" + noise); split
        // them into plain words. Periods/abbreviations ("Mr.") are untouched.
        assert_eq!(
            norm("[chorus]\nhand-in-hand. ass-hat. Mr. Cheesecake."),
            "[chorus] hand in hand. ass hat. Mr. Cheesecake."
        );
    }

    #[test]
    fn lyric_draft_msg_covers_desc_and_theme_combos() {
        // generate: description (+ optional theme) drives the draft.
        assert_eq!(
            lyric_draft_user_msg("synth-pop, warm", Some("rainy city")),
            Some("Music description: synth-pop, warm\n\nWrite lyrics about: rainy city".into())
        );
        assert_eq!(
            lyric_draft_user_msg("synth-pop, warm", None),
            Some("Write lyrics for this music:\n\nsynth-pop, warm".into())
        );
        // clone/continue: description is hidden, so the theme alone drives it.
        assert_eq!(
            lyric_draft_user_msg("   ", Some("rainy city")),
            Some("Write song lyrics about: rainy city".into())
        );
        // nothing to write from → None (handler rejects with 400).
        assert_eq!(lyric_draft_user_msg("", None), None);
        assert_eq!(lyric_draft_user_msg("  ", Some("   ")), None);
    }

    #[test]
    fn hyphen_scrub_spares_structural_tags() {
        // De-hyphenation must fold hyphens in lyric words but NOT inside canonical
        // tags ([inst-long]/[intro-short]/...), which legitimately carry a hyphen.
        assert_eq!(
            norm("[intro-short] ; [verse] hand-in-hand tonight. ; [inst-long]"),
            "[intro-short] ; [verse] hand in hand tonight. ; [inst-long]"
        );
    }

    #[test]
    fn parens_and_question_marks_are_folded() {
        // parentheses (no backing-vocal syntax exists) drop to spaces; `?`/`!`
        // become phrase breaks rather than spoken tokens.
        assert_eq!(
            norm("[Refrain]\nWhat's the nicest thing you see? (Hmm)\nChelsea!"),
            "[chorus] What's the nicest thing you see. Hmm. Chelsea."
        );
    }

    #[test]
    fn already_songgen_input_still_gets_commas_scrubbed() {
        // canonically-tagged single-line input is kept verbatim structurally, but
        // its commas/parens are still folded (the verbatim fast-path scrubs them).
        assert_eq!(
            norm("[verse] One, two, three. (backing) ; [chorus] oh, oh."),
            "[verse] One. two. three. backing ; [chorus] oh. oh."
        );
    }

    #[test]
    fn in_lyric_periods_are_preserved() {
        // abbreviations must not be split into separate phrases
        assert_eq!(
            norm("[Verse]\nMr. Jones walks alone\nDr. Smith waits for dawn"),
            "[verse] Mr. Jones walks alone. Dr. Smith waits for dawn."
        );
    }

    #[test]
    fn drops_empty_lyrical_sections() {
        assert_eq!(norm("[verse]\n[chorus]\nsing"), "[chorus] sing.");
    }

    #[test]
    fn lyric_lines_starting_with_section_words_are_not_headers() {
        // these must be sung lines, not mistaken for [bridge]/[chorus]/[inst]
        assert_eq!(
            norm("Verse 1\nBridge over troubled water\nBreak my heart in two"),
            "[verse] Bridge over troubled water. Break my heart in two."
        );
    }
}
