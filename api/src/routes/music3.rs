//! MiniMax Music 3 routes — the fourth engine on the music surface, beside acestep / songgen /
//! sa3. Auth-gated.
//!
//! One render mode and only one: **text→song**. There is no cover, no repaint, no continuation and
//! no reference audio, because the DAV VAE cannot encode (see [`crate::music3`]). Every route here
//! is JSON; nothing accepts an upload, and a body carrying one is refused by name rather than
//! quietly rendered without it.
//!
//! 🔴 **`POST /generate` returns 202 immediately; the render runs in a background task.** It was
//! synchronous until real renders were measured — a 120 s song is ~269 s untiled at defaults, which
//! crossed the threshold `music3.rs` documented in advance. The task owns the comfy-api slot (the
//! `minimax_h3` budget line — one container, one slot) and the poll loop, so a reload or a closed
//! tab no longer discards a four-minute render. The browser polls
//! `GET /api/v1/music3/jobs/{id}`, or `/active` to re-find a job it lost. See the queue's `gpu_jobs` table for
//! the decision and for what it deliberately does not implement.
//!
//! `POST /suggest` is the rant → caption + lyrics expander and is **ungated** — it is an LLM call
//! only and must never hold the GPU slot while the LLM writes.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use koblem_client::types::{Job, JobKind, JobState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::require_auth;
use crate::music3;
use crate::music3_prompt;
use crate::repositories::gpu_jobs as queue_store;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::engine_cancel;
use crate::routes::tier2_helpers::log_async;

/// 1 MB. Every body here is text — a caption, lyrics, a rant. Nothing is uploaded, so the acestep
/// path's 256 MB cap would only be an invitation.
const MUSIC3_BODY_LIMIT: usize = 1024 * 1024;

/// Takes per request. A cap rather than model law: each take is a full render on a single-slot
/// queue, so a typo'd `takes: 50` is half a day of GPU that nobody asked for.
const MAX_TAKES: u32 = 4;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music3/generate", post(generate))
        .route("/api/v1/music3/suggest", post(suggest))
        .route("/api/v1/music3/rework", post(rework))
        .route("/api/v1/music3/jobs/{id}", get(job))
        .route("/api/v1/music3/active", get(active))
        .route("/api/v1/music3/cancel", post(cancel))
        .route("/api/v1/music3/props", get(props))
        .route("/api/v1/music3/file/{name}", get(serve_file))
        .layer(DefaultBodyLimit::max(MUSIC3_BODY_LIMIT))
}

// ── wire types ────────────────────────────────────────────────────────────────────────────────
//
// Route-local, not promoted to `koblem-client`. koblibs is shared with every consumer, so a type
// there is a cross-repo contract, and CLAUDE.md's rule is to promote a type
// "when a client needs it" — no `kob` CLI command speaks Music 3 yet. Promote these the day one
// does, and not before.

#[derive(Debug, Clone, Deserialize)]
pub struct Music3GenerateRequest {
    /// The three-section structured caption. Required.
    pub caption: String,
    /// Tagged lyrics. Absent or empty = instrumental.
    #[serde(default)]
    pub lyrics: Option<String>,
    /// 🔴 A CEILING. The AR stage may end the song sooner, and the response's
    /// `duration_seconds` is what actually came out.
    #[serde(default)]
    pub max_duration: Option<f64>,
    /// `None` ⇒ the engine draws one per take and echoes it in the plan.
    #[serde(default)]
    pub seed: Option<i64>,
    #[serde(default)]
    pub steps: Option<u32>,
    /// KSampler cfg.
    #[serde(default)]
    pub cfg: Option<f64>,
    /// The AR stage's own CFG (`MiniMaxMusic3TextEncode.cfg_scale`). A separate knob from `cfg`
    /// and on a different stage — they are not two names for one thing.
    #[serde(default)]
    pub cfg_scale: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub sampler: Option<String>,
    #[serde(default)]
    pub scheduler: Option<String>,
    /// Tiled audio decode.
    ///
    /// 🔴 **NOT the tradeoff ComfyUI's own template describes.** That template calls tiling a
    /// VRAM-for-speed-and-quality trade — "drastically cuts VRAM", "slightly slower, with a small
    /// risk of seams". Measured here at 120 s, both cold, every part of that is wrong:
    ///
    /// ```text
    ///   untiled   165.0 s wall   peak 14420 MiB
    ///   tiled     125.0 s wall   peak 15730 MiB     ← 24% FASTER, and MORE VRAM
    /// ```
    ///
    /// Outputs were effectively identical: same 71.192 s of audio, byte counts within 153, and
    /// rms / spectral flatness / flux / stereo correlation matching to four decimals. No seams.
    ///
    /// So on this card it is a **free speedup**, not a quality compromise.
    ///
    /// 🔴 **DEFAULTS TO TILED since 2026-08-16**, reversing the earlier product call. That call
    /// rested entirely on the 15730 MiB peak above — and that peak belonged to the official
    /// template's `tile_size` of 1536, not to tiling. The engine now ships tile 256, swept
    /// against 3072/1536/768/512/256/128 on the same song:
    ///
    /// ```text
    ///   untiled            96.2 s   12.5 GiB
    ///   tiled @ 1536       81.6 s   15.6 GiB
    ///   tiled @ 256        71.9 s   10.2 GiB   ← shipped: fastest AND leanest
    /// ```
    ///
    /// Ear-tested at 320 kbps against an untiled render of the same seed: the ~5.7x boundary
    /// contrast the metric sees at 10 ms windows (worst join −52 dBFS against a −17 dBFS track)
    /// is not audible. `None` here means "take the engine's default", which is now tiled;
    /// `Some(false)` still selects the untiled topology, which is the reference every tile size
    /// was graded against.
    #[serde(default)]
    pub tiled_decode: Option<bool>,
    /// Independent takes, each its own prompt on the engine so they deliver progressively.
    #[serde(default)]
    pub takes: Option<u32>,
    /// `flac` (lossless) | `mp3` | `opus`. 🔴 **There is no `wav`** — ComfyUI has no wav writer.
    #[serde(default)]
    pub format: Option<String>,
    /// mp3/opus only. Reported in `warnings` when it is ignored, never dropped in silence.
    #[serde(default)]
    pub quality: Option<String>,
    /// Record the takes in the generation gallery.
    #[serde(default = "default_true")]
    pub save: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct Music3SuggestRequest {
    /// The user's rant: whatever they typed about how the song should sound.
    pub brief: String,
    /// Write only the caption and leave the lyrics to the user.
    #[serde(default)]
    pub caption_only: bool,
    /// A caption the user already has: when set (and `caption_only` is false) only lyrics are
    /// written, to fit it.
    #[serde(default)]
    pub caption: Option<String>,
    /// The duration ceiling, so the writer plans a sensible number of sections.
    #[serde(default)]
    pub max_duration: Option<f64>,
}

/// Apply one piece of feedback to a caption and lyrics the user already has.
///
/// Mirrors `ltx_director`'s `/rework` shape — the accepted material plus a free-text `instruction`
/// — rather than inventing a third convention for the same idea.
#[derive(Debug, Clone, Deserialize)]
pub struct Music3ReworkRequest {
    pub caption: String,
    #[serde(default)]
    pub lyrics: Option<String>,
    /// The user's feedback, verbatim: "chorus needs to hit harder", "less mopey", "lose the
    /// strings", "make it 3 verses".
    pub instruction: String,
    /// What the edit is allowed to touch. Defaults to both halves.
    #[serde(default)]
    pub scope: music3_prompt::ReworkScope,
    #[serde(default)]
    pub max_duration: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Music3ReworkResponse {
    pub caption: String,
    pub lyrics: Option<String>,
    pub family: String,
    /// True when the instruction moved the style family — the UI can say "now country" instead of
    /// leaving the user to spot it.
    pub family_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Music3SuggestResponse {
    pub caption: String,
    pub lyrics: Option<String>,
    /// Which of the 18 official style families the router picked. Echoed so a bad route is visible
    /// instead of being inferred from a caption that merely feels off.
    pub family: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Music3Props {
    pub configured: bool,
    pub healthy: bool,
    /// `GET /music3/v1/capabilities` verbatim — the model law the UI reads instead of keeping a
    /// second copy of the numbers.
    pub capabilities: Option<Value>,
}

// ── handlers ──────────────────────────────────────────────────────────────────────────────────

/// `POST /api/v1/music3/generate` — text→song.
async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let bytes = match to_bytes(request.into_body(), MUSIC3_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad json: {e}")),
    };
    // 🔴 Strict parse. This is the surface an sa3 or acestep body would land on by mistake — same
    // page, same words ("caption", "lyrics", "seed") — and serde would drop every field Music 3
    // does not share and render something nobody asked for. `reference_audio` is the one that
    // matters most: this model cannot take it at all, so a silent drop is a rendered lie.
    let req: Music3GenerateRequest =
        match crate::strict_json::from_value_strict(value, "music3 /generate") {
            Ok(r) => r,
            Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
        };

    if let Some(msg) = validate(&req) {
        return err(StatusCode::BAD_REQUEST, &msg);
    }
    if !state.music3.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "music3 not configured (set MUSIC3_SERVICE_URL or the DB setting)",
        );
    }

    // 🔴 The 409 is gone. "A Music 3 render is already in flight" was the single-slot store
    // refusing rather than queueing, and refusing is the behaviour this whole change exists to
    // remove: the button is never disabled and no submit path 503s or 409s any more.
    //
    // 🔴 And this is now the ONLY path to the engine. A direct route left alive beside the queue
    // is two writers to one engine — the two-schedulers bug in a new place.
    let who = match crate::routes::queue::principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let payload = build_request(&req);
    let enqueued = crate::routes::queue::enqueue(
        &state,
        &who,
        crate::routes::queue::QueueOptions::from_headers(&headers)
            .request(JobKind::Music3, payload),
    )
    .await;

    match enqueued {
        Ok(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
        Err((code, msg)) => err(code, &msg),
    }
}

/// `POST /api/v1/music3/rework` — apply one piece of feedback to existing material. Ungated: LLM
/// only, and it must never hold the GPU slot while the model writes.
///
/// 🔴 The untouched half is echoed back from the REQUEST, never from the model. On a scoped rework
/// the model is told not to return it, so trusting its output for that half would blank whichever
/// side the user was protecting — the precise thing the scope exists to prevent.
async fn rework(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Music3ReworkRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let instruction = req.instruction.trim();
    if instruction.is_empty() {
        return err(StatusCode::BAD_REQUEST, "instruction is required");
    }
    let caption = req.caption.trim();
    let lyrics = req.lyrics.as_deref().unwrap_or("").trim();
    if caption.is_empty() && lyrics.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "nothing to rework — write or generate a caption first",
        );
    }
    if req.scope == music3_prompt::ReworkScope::Lyrics && lyrics.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "no lyrics to rework — this piece is instrumental. Use scope \"both\" to add some.",
        );
    }

    // 🔴 Routed on the INSTRUCTION, with the caption only holding the line — see `route_rework`.
    // Deliberately NOT `route_family(instruction, Some(caption))`: that concatenates and scores
    // the pair, so a long caption's genre words drown a short instruction and "make it more
    // country" would never move.
    let routed = music3_prompt::route_rework(instruction, caption);

    // Budgets have to cover REPRODUCING the untouched material, not just the delta — the model is
    // asked for the full revised text, so a budget sized for an edit truncates the song.
    let max_tokens = match req.scope {
        music3_prompt::ReworkScope::Both => 3000,
        music3_prompt::ReworkScope::Caption => 1800,
        music3_prompt::ReworkScope::Lyrics => 1600,
    };
    let system = music3_prompt::rework_system_prompt(req.scope, routed.exemplar, req.max_duration);
    let user_turn = music3_prompt::rework_user_turn(caption, lyrics, instruction);

    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_turn},
        ],
        "max_tokens": max_tokens,
        // Load-bearing for the same reason as `/suggest` — see the comment there.
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_music3", &mut body).await;

    let resp = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, r)) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("rework failed: {e}")),
    };
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");

    let (new_caption, new_lyrics) = match req.scope {
        music3_prompt::ReworkScope::Both => {
            let s = music3_prompt::parse_suggestion(content, routed.family);
            // Lyrics may legitimately come back absent on an instrumental piece; keep whatever the
            // user had rather than blanking it.
            let lyrics_out = s
                .lyrics
                .or_else(|| (!lyrics.is_empty()).then(|| lyrics.to_string()));
            (s.caption, lyrics_out)
        }
        music3_prompt::ReworkScope::Caption => {
            let s = music3_prompt::parse_suggestion(content, routed.family);
            (s.caption, (!lyrics.is_empty()).then(|| lyrics.to_string()))
        }
        music3_prompt::ReworkScope::Lyrics => {
            // Raw content: this arm asked for lyrics alone, with no delimiters to strip.
            (caption.to_string(), Some(content.trim().to_string()))
        }
    };

    // 🔴 Refuse rather than return an empty half. The client overwrites its editor from this
    // response, so handing back "" for the side that was being reworked would wipe the user's
    // accepted work on a model hiccup — the exact destruction this endpoint is built to avoid.
    let reworked_is_empty = match req.scope {
        music3_prompt::ReworkScope::Lyrics => new_lyrics
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty(),
        _ => new_caption.trim().is_empty(),
    };
    if reworked_is_empty {
        return err(
            StatusCode::BAD_GATEWAY,
            "the model returned nothing for the reworked section — your caption and lyrics are \
             unchanged; try rephrasing the instruction",
        );
    }

    Json(Music3ReworkResponse {
        caption: new_caption,
        lyrics: new_lyrics,
        family: routed.family.to_string(),
        family_changed: routed.changed,
    })
    .into_response()
}

/// `GET /api/v1/music3/jobs/{id}` — poll a render.
///
/// Reads the queue row rather than a per-engine store. The shape the page polls is unchanged;
/// what changed is where it comes from — and that the record now outlives the night (14 days)
/// instead of 30 minutes, so the morning after can still say what ran.
async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let Ok(Some(row)) = queue_store::get(&state.db, &id).await else {
        return err(StatusCode::NOT_FOUND, "no such Music 3 job");
    };
    if row.kind != JobKind::Music3 {
        return err(StatusCode::NOT_FOUND, "that job is not a Music 3 render");
    }
    Json(view(&state, &row).await).into_response()
}

/// `GET /api/v1/music3/active` — the render currently in flight, if any. Lets a page that was
/// reloaded find the job it lost without having kept the id.
async fn active(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let live = queue_store::live(&state.db).await.unwrap_or_default();
    let pending = queue_store::pending(&state.db).await.unwrap_or_default();
    let found = live
        .into_iter()
        .chain(pending)
        .find(|j| j.kind == JobKind::Music3);
    match found {
        Some(row) => Json(json!({ "job": view(&state, &row).await })).into_response(),
        None => Json(json!({ "job": Value::Null })).into_response(),
    }
}

/// Render a queue row in the shape this engine's page already speaks.
///
/// Live step/phase is overlaid from the client rather than trusted from the row: the client keeps
/// the authoritative snapshot between the row's one-second writes.
async fn view(state: &AppState, row: &Job) -> Value {
    let result = queue_store::detail(&state.db, &row.id)
        .await
        .ok()
        .flatten()
        .and_then(|(_, _, result)| result)
        .unwrap_or(Value::Null);

    let mut progress = json!({
        "phase": row.progress.phase,
        "progress": row.progress.progress,
        "current_step": row.progress.current_step,
        "total_steps": row.progress.total_steps,
        "tracks_done": row.units_done,
        "track_count": row.units_total,
    });
    if !row.state.is_terminal() {
        let live = state.music3.progress();
        progress["phase"] = json!(live.phase);
        progress["current_step"] = json!(live.current_step);
        progress["total_steps"] = json!(live.total_steps);
        progress["eta_seconds"] = json!(live.eta_seconds);
    }

    json!({
        "id": row.id,
        // `collecting` has no equivalent in this engine's vocabulary, and it IS still rendering as
        // far as the page is concerned — the card is free but the song is not saved yet.
        "state": match row.state {
            JobState::Queued | JobState::Held => "queued",
            JobState::Running | JobState::Collecting => "rendering",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        },
        "server_job_id": row.server_job_id,
        "progress": progress,
        "tracks": result.get("tracks").cloned().unwrap_or(json!([])),
        "plan": result.get("plan").cloned().unwrap_or(Value::Null),
        "warnings": row.warnings,
        "ignored": result.get("ignored").cloned().unwrap_or(json!([])),
        "error": row.error,
        "queued_ms": row.queued_ms,
        "admit_ms": row.admit_ms,
        "duration_ms": row.render_ms,
        "blocked_reason": row.blocked_reason,
        "eta_start": row.eta_start,
    })
}

/// Everything that can be refused without the GPU. Refusing here turns "you queued, waited for the
/// card, then got a 400" into "you got a 400".
fn validate(req: &Music3GenerateRequest) -> Option<String> {
    if req.caption.trim().is_empty() {
        return Some("caption is required".into());
    }
    if let Some(d) = req.max_duration
        && (!d.is_finite()
            || !(music3::MIN_DURATION_SECONDS..=music3::MAX_DURATION_SECONDS).contains(&d))
    {
        return Some(format!(
            "max_duration must be between {} and {} seconds ({} audio frames at {} fps is the \
             model's hard ceiling)",
            music3::MIN_DURATION_SECONDS,
            music3::MAX_DURATION_SECONDS,
            music3::MAX_DURATION_SECONDS * music3::AUDIO_FRAMES_PER_SECOND,
            music3::AUDIO_FRAMES_PER_SECOND,
        ));
    }
    if let Some(k) = req.top_k
        && (k < 1 || k as usize > 16384)
    {
        return Some("top_k must be between 1 and 16384 (the C0 codebook size)".into());
    }
    if let Some(t) = req.takes
        && !(1..=MAX_TAKES).contains(&t)
    {
        return Some(format!(
            "takes must be between 1 and {MAX_TAKES} — each take is a full render on a \
             single-slot queue"
        ));
    }
    // 🔴 wav is not a format this engine can decline to produce — it is one ComfyUI cannot WRITE.
    // `SaveAudioAdvanced` offers flac/mp3/opus and `SaveAudio` is flac-only; there is no wav writer
    // anywhere in the graph. The engine 400s on it, but only after this request has queued for the
    // card, so catch it here — and point at flac by name, because someone asking for wav wants
    // "uncompressed" and would otherwise go hunting for a format that does not exist.
    if let Some(f) = req.format.as_deref().map(str::trim)
        && f.eq_ignore_ascii_case("wav")
    {
        return Some(
            "format \"wav\" does not exist for this engine — ComfyUI has no wav writer at all. \
             Use \"flac\" for lossless, or \"mp3\"/\"opus\" for compressed."
                .into(),
        );
    }
    if let Some(estimate) =
        music3_prompt::over_prompt_budget(&req.caption, req.lyrics.as_deref().unwrap_or(""))
    {
        return Some(format!(
            "caption + lyrics is roughly {estimate} tokens, over the model's {} limit (estimated \
             at {} chars/token — the engine's tokeniser is the real judge, this check only saves \
             you the queue)",
            music3::MAX_PROMPT_TOKENS,
            music3::CHARS_PER_TOKEN_ESTIMATE,
        ));
    }
    None
}

/// `POST /api/v1/music3/suggest` — the rant expander. Ungated: LLM only, no GPU slot held.
async fn suggest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Music3SuggestRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let brief = req.brief.trim();
    let existing = req
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    if brief.is_empty() && existing.is_none() {
        return err(StatusCode::BAD_REQUEST, "brief is required");
    }

    // Route on the rant AND any caption already in the box: a user who has half a caption written
    // has told us more about the genre than the one-line brief does.
    let routed = music3_prompt::route_family(brief, existing);

    // Token budgets. A Music 3 caption is 250-450 WORDS — several times an acestep caption — so the
    // acestep path's 1500 would truncate a full caption+lyrics pass mid-sentence.
    let (system, max_tokens) = match (existing, req.caption_only) {
        (_, true) => (
            music3_prompt::caption_only_prompt(&routed, req.max_duration),
            1600,
        ),
        (Some(caption), false) if brief.is_empty() => (
            music3_prompt::lyrics_only_prompt(caption, req.max_duration),
            1200,
        ),
        _ => (
            music3_prompt::system_prompt(&routed, req.max_duration),
            2600,
        ),
    };
    let lyrics_only = existing.is_some() && !req.caption_only && brief.is_empty();

    let user_turn = if brief.is_empty() {
        "Write the lyrics for the caption above.".to_string()
    } else {
        brief.to_string()
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_turn},
        ],
        "max_tokens": max_tokens,
        // 🔴 **`enable_thinking: false` IS LOAD-BEARING — the whole path depends on this one flag.**
        // Verified 2026-08-14: with thinking left ON, gemma-4-12b spent all 4000 tokens on
        // `reasoning_content` and returned an EMPTY `content`, so the caption never existed. With
        // it off the same prompt produced a valid caption + lyrics first try (686 tokens,
        // `finish_reason: stop`, 11/11 contract checks). Do not drop this key "because the model
        // seems fine without it" — the failure is silent at the HTTP layer: a 200 with nothing in
        // it. `llama.rs`'s empty-content handling is the backstop that turns it into an error
        // instead of an empty caption box.
        "chat_template_kwargs": {"enable_thinking": false},
    });
    // Still one knob for all four engines' writers: `sampling_music3` inherits from
    // `sampling_music`, so setting the music surface moves this too — its own row exists
    // only for when music3's writer needs to differ. See `crate::sampling`.
    crate::sampling::apply_sampling(&state.db, "sampling_music3", &mut body).await;

    let resp = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, r)) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("suggest failed: {e}")),
    };
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");

    if lyrics_only {
        let lyrics = content.trim();
        if lyrics.is_empty() {
            return err(StatusCode::BAD_GATEWAY, "model returned no lyrics");
        }
        return Json(Music3SuggestResponse {
            caption: existing.unwrap_or_default().to_string(),
            lyrics: Some(lyrics.to_string()),
            family: routed.family.to_string(),
        })
        .into_response();
    }

    let suggestion = music3_prompt::parse_suggestion(content, routed.family);
    if suggestion.caption.trim().is_empty() {
        return err(StatusCode::BAD_GATEWAY, "model returned no caption");
    }
    Json(Music3SuggestResponse {
        caption: suggestion.caption,
        lyrics: suggestion.lyrics,
        family: suggestion.family,
    })
    .into_response()
}

/// `POST /api/v1/music3/cancel` — cancel the render on the card, or the next one queued.
///
/// The record of a running job is left alone here: the render task owns it, sees `Error::Cancelled`
/// from its own poll loop, and files the terminal state itself. Marking it from this side too would
/// race the task and could park the slot in `cancelled` while the task is still holding the GPU
/// guard. A job that never started has no such task, so the row is closed out directly.
async fn cancel(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    engine_cancel::cancel_kind(
        &state,
        JobKind::Music3,
        "no Music 3 render is queued or running",
    )
    .await
}

/// `GET /api/v1/music3/props` — reachability + the engine's own capabilities, for the UI.
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.music3.is_configured() {
        return Json(Music3Props::default()).into_response();
    }
    let healthy = state.music3.healthy().await;
    let capabilities = if healthy {
        state.music3.capabilities().await.ok()
    } else {
        None
    };
    Json(Music3Props {
        configured: true,
        healthy,
        capabilities,
    })
    .into_response()
}

/// `GET /api/v1/music3/file/{name}` — serve a rendered song from `SONGS_DIR`.
async fn serve_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    // Bare filename only — anything that could escape SONGS_DIR is refused.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "invalid filename");
    }
    let path = state.songs_dir.join(&name);
    match crate::routes::byte_serve::serve_file(&headers, &path, content_type_for(&name)).await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "song not found"),
    }
}

// ── the engine body ───────────────────────────────────────────────────────────────────────────
//
// The render itself lives in `queue::adapters::music3` now. What stayed here is the one thing
// that is genuinely about this route: turning the form's request into the engine's body. That
// body becomes the queue row's payload verbatim, so it has to survive a deploy — every optional
// field is omitted rather than defaulted, exactly as before.

fn build_request(req: &Music3GenerateRequest) -> Value {
    let mut body = json!({ "caption": req.caption });
    let obj = body.as_object_mut().expect("just built an object");
    // Lyrics always go, even empty: "" is how this wire says INSTRUMENTAL, and omitting the key
    // would let the plugin's default (whatever it is) decide something the user chose.
    obj.insert(
        "lyrics".into(),
        json!(req.lyrics.clone().unwrap_or_default()),
    );
    if let Some(v) = req.max_duration {
        obj.insert("max_duration".into(), json!(v));
    }
    if let Some(v) = req.seed {
        obj.insert("seed".into(), json!(v));
    }
    if let Some(v) = req.steps {
        obj.insert("steps".into(), json!(v));
    }
    if let Some(v) = req.cfg {
        obj.insert("cfg".into(), json!(v));
    }
    if let Some(v) = req.cfg_scale {
        obj.insert("cfg_scale".into(), json!(v));
    }
    if let Some(v) = req.top_k {
        obj.insert("top_k".into(), json!(v));
    }
    if let Some(v) = &req.sampler {
        obj.insert("sampler".into(), json!(v));
    }
    if let Some(v) = &req.scheduler {
        obj.insert("scheduler".into(), json!(v));
    }
    if let Some(v) = req.tiled_decode {
        obj.insert("tiled_decode".into(), json!(v));
    }
    if let Some(v) = req.takes {
        obj.insert("takes".into(), json!(v));
    }
    if let Some(v) = &req.format {
        obj.insert("format".into(), json!(v));
    }
    if let Some(v) = &req.quality {
        obj.insert("quality".into(), json!(v));
    }
    // 🔴 Not an engine field. It rides in the payload because the payload IS the
    // queue row, and the adapter that files the gallery row runs hours later in a
    // different process — there is nowhere else for a request-time choice to live.
    obj.insert("save".into(), json!(req.save));
    body
}

// ── helpers ───────────────────────────────────────────────────────────────────────────────────

async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if require_auth(&state.db, state.api_key.as_deref(), headers)
        .await
        .is_err()
    {
        return Some(err(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    None
}

pub(crate) async fn save_clip(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    tokio::fs::create_dir_all(&*state.songs_dir).await?;
    tokio::fs::write(state.songs_dir.join(filename), bytes).await
}

fn content_type_for(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".mp3") {
        "audio/mpeg"
    } else if lower.ends_with(".flac") {
        "audio/flac"
    } else if lower.ends_with(".opus") || lower.ends_with(".ogg") {
        "audio/ogg"
    } else {
        "audio/wav"
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn log_music3(
    state: &AppState,
    caller: Option<String>,
    request_meta: &Value,
    gate_wait_ms: u64,
    duration_ms: u64,
    status: i32,
    response_meta: Value,
) {
    log_async(
        state.db.clone(),
        CreateLog {
            // Filed under the shared `music` service so all four engines land on one log page;
            // `endpoint` is what tells them apart.
            service: "music".into(),
            endpoint: "music3-generate".into(),
            request_meta: request_meta.clone(),
            response_meta,
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: duration_ms as i64,
            status,
            caller,
        },
    );
}

/// Compact UTC timestamp for filenames: `20260814-134501`.
pub(crate) fn file_stamp() -> String {
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

/// Short, filename-safe seed tag.
pub(crate) fn short_seed(seed: Option<i64>) -> String {
    match seed {
        Some(s) if s >= 0 => s.to_string(),
        _ => "r".to_string(),
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Music3GenerateRequest {
        Music3GenerateRequest {
            caption: "Global Metadata\nIndie folk.".into(),
            lyrics: None,
            max_duration: Some(120.0),
            seed: None,
            steps: None,
            cfg: None,
            cfg_scale: None,
            top_k: None,
            sampler: None,
            scheduler: None,
            tiled_decode: None,
            takes: None,
            format: None,
            quality: None,
            save: true,
        }
    }

    /// The pre-flight refuses what the engine would refuse, before the queue rather than after it.
    #[test]
    fn the_duration_ceiling_is_enforced_before_the_gate() {
        assert!(validate(&base()).is_none());

        let mut over = base();
        over.max_duration = Some(400.0);
        let msg = validate(&over).expect("400 s is past the 9000-frame ceiling");
        assert!(msg.contains("360"), "{msg}");

        let mut under = base();
        under.max_duration = Some(0.0);
        assert!(validate(&under).is_some());

        // NaN must not slip through the comparison chain into the engine.
        let mut nan = base();
        nan.max_duration = Some(f64::NAN);
        assert!(validate(&nan).is_some());

        let mut blank = base();
        blank.caption = "   ".into();
        assert!(validate(&blank).is_some());

        let mut takes = base();
        takes.takes = Some(9);
        assert!(validate(&takes).is_some());
        takes.takes = Some(MAX_TAKES);
        assert!(validate(&takes).is_none());
    }

    /// 🔴 Empty lyrics are how this wire says INSTRUMENTAL, so the key must be present even when
    /// the user wrote nothing. Omitting it hands the choice to a plugin default.
    #[test]
    fn an_instrumental_still_sends_an_explicit_empty_lyrics_field() {
        let body = build_request(&base());
        assert_eq!(body["lyrics"], json!(""));
        // Unset optionals are absent, not guessed — the engine owns its defaults.
        assert!(body.get("steps").is_none());
        assert!(body.get("cfg").is_none());
        assert!(body.get("seed").is_none());
        assert_eq!(body["max_duration"], json!(120.0));

        let mut sung = base();
        sung.lyrics = Some("[Verse]\nwords".into());
        sung.cfg = Some(1.7);
        sung.cfg_scale = Some(1.9);
        let body = build_request(&sung);
        assert_eq!(body["lyrics"], json!("[Verse]\nwords"));
        // Two different stages, two different knobs — never collapsed into one.
        assert_eq!(body["cfg"], json!(1.7));
        assert_eq!(body["cfg_scale"], json!(1.9));
    }

    /// The strict parse is the guard against an acestep/sa3 body landing here. `reference_audio` is
    /// the one that matters: this model cannot take audio at all, so a serde drop would render a
    /// song that silently ignored the user's reference.
    #[test]
    fn a_foreign_music_body_is_refused_by_name() {
        let msg = crate::strict_json::from_value_strict::<Music3GenerateRequest>(
            json!({"caption": "x", "reference_audio": "asset:abc", "prompt": "y"}),
            "music3 /generate",
        )
        .expect_err("neither field belongs to music3");
        assert!(msg.contains("music3 /generate"), "{msg}");
        assert!(msg.contains("reference_audio"), "{msg}");

        // …and a legitimate body still parses.
        crate::strict_json::from_value_strict::<Music3GenerateRequest>(
            json!({"caption": "x", "lyrics": "[Verse]\na", "takes": 2, "format": "flac"}),
            "music3 /generate",
        )
        .expect("a real music3 body parses");
    }
}
