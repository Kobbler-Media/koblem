//! ACE-Step music generation routes (the GPU "music" engine). Auth-gated.
//!
//! Three modes:
//!   - `generate` (text2music): runs the ace LM code-sketch then the turbo DiT.
//!     JSON body. Empty lyrics → the LM writes them.
//!   - `cover` (lyric-swap via ace `cover-nofsq`) and `repaint` (regenerate a
//!     time window): multipart with a source-audio upload, no LM.
//!
//! koblem owns the whole render: it acquires the GPU as the `music` consumer
//! (TTS preempts, STT/Vision get evicted), drives the ace-server job queue,
//! polls with a TTS-yield check, saves the audio under `SONGS_DIR`, then
//! releases the GPU. A preempt mid-render returns 503 so the client can retry.

use std::time::Instant;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use koblem_client::types::{
    GenerationOutput, MusicDraftLyricsResponse, MusicFixRhymesRequest, MusicGenerateRequest,
    MusicGenerateResponse, MusicLyricsSource, MusicMode, MusicProps, MusicSuggestRequest,
    MusicSuggestion, MusicTrack, MusicUnderstood, RhymeFlag,
};
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::auth::require_auth;
use crate::music::{AudioPart, Error as MusicError};
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::tier2_helpers::{extract_caller, log_async, record_generation};

/// 256 MB matches ace-server's own body cap (≤10 min WAV source audio).
const MUSIC_BODY_LIMIT: usize = 256 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/generate", post(generate))
        .route("/api/v1/music/transform", post(transform))
        .route("/api/v1/music/understand", post(understand))
        .route("/api/v1/music/draft-lyrics", post(draft_lyrics))
        .route("/api/v1/music/fix-rhymes", post(fix_rhymes))
        .route("/api/v1/music/suggest", post(suggest))
        .route("/api/v1/music/props", get(props))
        .route("/api/v1/music/file/{name}", get(serve_file))
        .layer(DefaultBodyLimit::max(MUSIC_BODY_LIMIT))
}

// ── handlers ──────────────────────────────────────────────────────────

/// `POST /api/v1/music/generate` — text2music. Accepts a JSON
/// `MusicGenerateRequest`, or multipart/form-data with a `request` JSON part and
/// an optional `ref_audio` file (timbre reference).
async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/"));

    let (req, ref_audio) = if is_multipart {
        let mp = match Multipart::from_request(request, &state).await {
            Ok(mp) => mp,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        };
        match parse_music_multipart(mp).await {
            Ok(parsed) => match parsed.request {
                Some(r) => (r, parsed.ref_audio),
                None => return err(StatusCode::BAD_REQUEST, "missing `request` part"),
            },
            Err(resp) => return resp,
        }
    } else {
        let bytes = match to_bytes(request.into_body(), MUSIC_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
        };
        match serde_json::from_slice::<MusicGenerateRequest>(&bytes) {
            Ok(r) => (r, None),
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid request JSON: {e}"),
                );
            }
        }
    };

    if req.mode != MusicMode::Generate {
        return err(
            StatusCode::BAD_REQUEST,
            "use POST /api/v1/music/transform for cover/repaint/extract/lego/complete (needs source audio)",
        );
    }
    if req.caption.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "caption is required");
    }
    run_pipeline(&state, &headers, req, None, ref_audio).await
}

/// `POST /api/v1/music/transform` — cover/repaint. Multipart: a `request` JSON
/// part + an `audio` source file + an optional `ref_audio` timbre reference.
async fn transform(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let parsed = match parse_music_multipart(multipart).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some(req) = parsed.request else {
        return err(StatusCode::BAD_REQUEST, "missing `request` part");
    };
    // ace-server requires a non-empty caption for cover/repaint too (only
    // lego/extract/complete skip it — see ace-server.cpp:1003). Catch it here with
    // a useful message instead of a downstream 502.
    let stem_task = matches!(
        req.mode,
        MusicMode::Extract | MusicMode::Lego | MusicMode::Complete
    );
    if !stem_task && req.caption.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "caption is required — describe the music style; for a faithful cover, describe the source's own style",
        );
    }
    if req.mode == MusicMode::Generate {
        return err(
            StatusCode::BAD_REQUEST,
            "mode must be cover/repaint/extract/lego/complete on /transform; use /generate for text2music",
        );
    }
    let Some(audio) = parsed.source else {
        return err(
            StatusCode::BAD_REQUEST,
            "cover/repaint/extract/lego/complete require an `audio` part",
        );
    };
    run_pipeline(&state, &headers, req, Some(audio), parsed.ref_audio).await
}

/// `POST /api/v1/music/understand` — describe a source song (caption + metadata)
/// via ace-server's reverse pipeline. Multipart with an `audio` file part.
/// GPU-gated like a render (runs VAE + FSQ + LM); yields to TTS.
async fn understand(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.music.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "music service not configured",
        );
    }
    let parsed = match parse_music_multipart(multipart).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Some((bytes, filename)) = parsed.source else {
        return err(
            StatusCode::BAD_REQUEST,
            "understand requires an `audio` part",
        );
    };
    let Some((guard, _wait)) = state.music.acquire().await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "GPU busy with higher-priority TTS — retry shortly",
        );
    };
    let gpu = guard.chosen_gpu().map(str::to_owned);
    let result = async {
        let job = state
            .music
            .submit_understand(bytes, &filename, gpu.as_deref())
            .await?;
        state.music.await_job(&job).await?;
        state.music.fetch_understand(&job).await
    }
    .await;
    drop(guard);
    match result {
        Ok(v) => {
            let understood: MusicUnderstood = serde_json::from_value(v).unwrap_or_default();
            Json(understood).into_response()
        }
        Err(MusicError::Preempted) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "understand preempted by TTS — retry shortly",
        ),
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("ace-server: {e}")),
    }
}

/// `POST /api/v1/music/draft-lyrics` — write lyrics with the main 9B (no render)
/// for previewing/editing. JSON `MusicGenerateRequest` (uses caption +
/// lyrics_prompt). Ungated — the LLM is the always-resident model.
async fn draft_lyrics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MusicGenerateRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let refine = crate::routes::refine::Refine::from_opts(
        req.refine_source.as_deref(),
        req.refine_instruction.as_deref(),
    );
    // A refine carries its own source, so the caption (style context) is optional
    // there; a fresh draft still needs it.
    if req.caption.trim().is_empty() && refine.is_none() {
        return err(StatusCode::BAD_REQUEST, "caption is required");
    }
    match write_lyrics_with_llm(
        &state,
        &req.caption,
        req.lyrics_prompt.as_deref(),
        refine,
        req.vocal_language.as_deref(),
    )
    .await
    {
        Ok(lyrics) => {
            let suggested = suggest_duration_from_lyrics(&state, &req.caption, &lyrics).await;
            let mut resp = draft_response(lyrics);
            resp.suggested_seconds = suggested;
            Json(resp).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("lyric draft failed: {e}")),
    }
}

/// Attach the advisory rhyme flags. Reporting only — the fix is the user's call.
fn draft_response(lyrics: String) -> MusicDraftLyricsResponse {
    let rhyme_flags = to_flags(&crate::rhyme::eye_rhymes(&lyrics));
    MusicDraftLyricsResponse {
        lyrics,
        rhyme_flags,
        suggested_seconds: None,
    }
}

pub(crate) fn to_flags(flags: &[crate::rhyme::EyeRhyme]) -> Vec<RhymeFlag> {
    flags
        .iter()
        .map(|f| RhymeFlag {
            a: f.a.clone(),
            b: f.b.clone(),
        })
        .collect()
}

/// `POST /api/v1/music/fix-rhymes` — rewrite only the non-rhyming line endings.
/// Driven by the UI's Fix button; re-checks and returns whatever is left, so a
/// stubborn pair shows up again rather than looping the model.
async fn fix_rhymes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MusicFixRhymesRequest>,
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
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        Some(c) => format!("{}\n\nMusic / style: {c}", lyrics_system_prompt()),
        None => lyrics_system_prompt(),
    };
    match fix_lyric_rhymes(&state, &system, &req.lyrics, &flags).await {
        Ok(lyrics) => Json(draft_response(lyrics)).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("rhyme fix failed: {e}")),
    }
}

// ── ACE-Step prompt building ──────────────────────────────────────────────
//
// The caption and lyric rules (ACE-Step 1.5 Tutorial, distilled for a 12B) live
// as two shared fragments, composed into three system prompts so draft,
// fix-rhymes, improve and expand all follow one source of truth — edit a rule
// once and every prompt that uses it updates, with no drift between them.

/// Caption-writing rules. The caption is ACE's single most important input.
const CAPTION_RULES: &str = "The CAPTION is the most important input — it \
describes the music itself. Write a few comma-separated tags or a short \
description covering the useful dimensions: genre/style, mood, key instruments, \
vocal type (e.g. female vocal, breathy), timbre/production (warm, lo-fi, \
polished) and era/reference (80s synth-pop, like Bon Iver). Be specific — \
specific beats vague. No numeric BPM or key (a tempo feel like \"driving\" or \
\"laid-back\" is fine).";

/// Lyric-writing rules. Lyrics are a temporal script, not just words: they carry
/// structure, vocal-delivery, energy and instrumental direction via bracket tags.
/// The rhyme clause is load-bearing — without it the 12B eye-rhymes (good/wood);
/// the parentheses-vs-brackets clause stops it writing (whispered) as a delivery
/// hint when parentheses mean backing vocals.
const LYRIC_RULES: &str = "The LYRICS are the song's temporal script: beyond the \
words they direct how the vocal and music unfold — structure, vocal delivery, \
energy shifts and instrumental moments — through square-bracket tags placed where \
each change happens. They must match the caption (same instruments, vocals, \
mood).\n\
- Square-bracket tags [ ] direct the performance, each on its own line where it \
takes effect: structure ([Intro], [Verse], [Pre-Chorus], [Chorus], [Bridge], \
[Outro]); vocal delivery ([whispered], [raspy vocal], [falsetto], [spoken word], \
[harmonies]); energy shifts ([building energy], [explosive drop], [high energy], \
[breakdown]); instrumental moments with no words after ([Instrumental], [Guitar \
Solo], [Drum Break], [Piano Interlude], [Fade Out]). Add at most ONE mood word to \
a section tag ([Chorus - anthemic]); don't stack. Blank line between sections.\n\
- Parentheses ( ) are ONLY backing vocals — real words sung underneath, e.g. \
\"We rise (we rise)\". A delivery style is a tag, never a parenthesis: whispered \
singing is [whispered], not (whispered).\n\
- CAPS raise vocal intensity — use sparingly.\n\
- Keep lines singable: roughly 6-10 syllables, short enough to sing in one \
breath.\n\
- Rhyme by sound, not spelling — matching letters is not a rhyme. Never force a \
rhyme that bends a word or breaks the meaning; a natural near-rhyme beats a \
forced one.\n\
- Stay on one core image and develop it; avoid vague stacked buzzwords like \
\"neon skies, electric hearts, endless dreams\".\n\
- For a purely instrumental idea, use only instrumental tags; for a fully \
instrumental track write exactly [Instrumental].";

/// System prompt for the lyric-writing call (draft / fix-rhymes / improve): write
/// lyrics to fit a caption the caller already has.
fn lyrics_system_prompt() -> String {
    format!(
        "You write song lyrics for the ACE-Step text-to-music model. Fit the \
         caption's style, mood, instruments and vocals, plus any theme named. \
         Length and arrangement are yours; don't pad or trim to a target.\n\n\
         {LYRIC_RULES}\n\n\
         Output ONLY the lyrics and their tags: no title, commentary, markdown or \
         explanation."
    )
}

// Delimiter format below, not JSON: full lyrics (newlines, quotes, [tags]) break
// JSON escaping too often — the model double-encodes or mis-escapes and the whole
// reply lands in `caption`. Markers are trivial to split and impossible to
// mis-escape.

/// System prompt for the one-pass expander: idea → caption + matching lyrics.
fn suggest_system_prompt() -> String {
    format!(
        "You turn the user's music idea into two inputs for the ACE-Step \
         text-to-music model: a CAPTION describing the music and the song's \
         LYRICS. Capture the user's idea — only add what they asked for or clearly \
         implied; leave the rest open.\n\n\
         {CAPTION_RULES}\n\n\
         {LYRIC_RULES}\n\n\
         Output EXACTLY this layout and nothing else — no JSON, no markdown, no \
         commentary:\n\
         ###CAPTION###\n\
         <the music description; no lyrics here>\n\
         ###LYRICS###\n\
         <the song lyrics>"
    )
}

/// Caption-only variant of the expander — for users who write/tweak a caption and
/// let the ACE engine write the lyrics.
fn suggest_caption_only_prompt() -> String {
    format!(
        "You turn the user's music idea into a CAPTION for the ACE-Step \
         text-to-music model — a description of the music itself, no lyrics.\n\n\
         {CAPTION_RULES}\n\n\
         Output EXACTLY this and nothing else — no JSON, no markdown, no \
         commentary:\n\
         ###CAPTION###\n\
         <the music description>"
    )
}

/// `POST /api/v1/music/suggest` — expand a casual idea into a detailed caption
/// AND matching lyrics with the main 9B, in one pass, returned as JSON to
/// pre-fill the form. Ungated — LLM only.
async fn suggest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MusicSuggestRequest>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if req.brief.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "brief is required");
    }
    let (system, max_tokens) = if req.caption_only {
        (suggest_caption_only_prompt(), 400)
    } else {
        // Headroom for a full set of lyrics + the caption in one pass.
        (suggest_system_prompt(), 1500)
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": req.brief},
        ],
        "max_tokens": max_tokens,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_music", &mut body).await;
    let resp = match state.llama.chat_completion_json(&body).await {
        Ok((_permit, r)) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("suggest failed: {e}")),
    };
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let suggestion = parse_suggestion(content);
    if suggestion.caption.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "model returned no caption");
    }
    Json(suggestion).into_response()
}

/// Split a `###CAPTION### … ###LYRICS### …` reply into the two fields. Tolerant:
/// no LYRICS marker → the whole reply (sans a CAPTION marker) is the caption.
fn parse_suggestion(content: &str) -> MusicSuggestion {
    const CAP: &str = "###CAPTION###";
    const LYR: &str = "###LYRICS###";
    match content.find(LYR) {
        Some(li) => {
            let caption = content[..li].replacen(CAP, "", 1).trim().to_string();
            let lyrics = content[li + LYR.len()..].trim().to_string();
            MusicSuggestion {
                caption,
                lyrics: (!lyrics.is_empty()).then_some(lyrics),
            }
        }
        None => MusicSuggestion {
            caption: content.replacen(CAP, "", 1).trim().to_string(),
            lyrics: None,
        },
    }
}

/// `GET /api/v1/music/props` — DiT/LM model lists for the UI dropdown.
async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.music.is_configured() {
        return Json(MusicProps::default()).into_response();
    }
    let healthy = state.music.healthy().await;
    let mut out = MusicProps {
        healthy,
        ..Default::default()
    };
    if let Ok(props) = state.music.props().await
        && let Some(models) = props.get("models")
    {
        out.dit_models = string_list(models.get("dit"));
        out.lm_models = string_list(models.get("lm"));
    }
    Json(out).into_response()
}

/// `GET /api/v1/music/file/{name}` — serve a rendered song from `SONGS_DIR`.
async fn serve_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    // Reject anything that could escape SONGS_DIR — accept a bare filename only.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "invalid filename");
    }
    let path = state.songs_dir.join(&name);
    match crate::routes::byte_serve::serve_file(&headers, &path, content_type_for(&name)).await {
        Ok(resp) => resp,
        Err(_) => err(StatusCode::NOT_FOUND, "song not found"),
    }
}

// ── pipeline ──────────────────────────────────────────────────────────

async fn run_pipeline(
    state: &AppState,
    headers: &HeaderMap,
    req: MusicGenerateRequest,
    src_audio: Option<(Bytes, String)>,
    ref_audio: Option<(Bytes, String)>,
) -> Response {
    if !state.music.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "music service not configured (set ACE_SERVER_URL or the DB setting)",
        );
    }

    let caller = extract_caller(headers);
    let mut lyrics = req.lyrics.clone().unwrap_or_default();

    // Optionally have the main 9B write the lyrics (generate mode, blank lyrics,
    // lyrics_source = llm). Done BEFORE acquiring the music GPU lock — the LLM is
    // the ungated always-resident model, so we don't hold the music guard (and
    // block TTS) while it writes. Falls back to the ACE engine on any failure.
    if req.mode == MusicMode::Generate
        && lyrics.trim().is_empty()
        && req.lyrics_source == MusicLyricsSource::Llm
    {
        match write_lyrics_with_llm(
            state,
            &req.caption,
            req.lyrics_prompt.as_deref(),
            None,
            req.vocal_language.as_deref(),
        )
        .await
        {
            Ok(written) if !written.trim().is_empty() => lyrics = written,
            Ok(_) => warn!("9B returned empty lyrics; falling back to the ACE engine"),
            Err(e) => {
                warn!(error = %e, "9B lyric generation failed; falling back to the ACE engine")
            }
        }
    }

    let duration_used = resolve_duration(&req, &lyrics);
    let mut ace_body = build_ace_request(&req, &lyrics, duration_used);

    // DiT selection. Stem tasks (extract/lego/complete) REQUIRE the base model —
    // turbo output is incoherent for them. Everything else uses turbo. Because the
    // base GGUF sorts before turbo in the engine's registry, its mere presence would
    // otherwise make the engine default normal renders to the slow 50-step base, so
    // we pin the DiT explicitly here. A user's Advanced `synth_model` always wins.
    if ace_body
        .get("synth_model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty()
    {
        let want_base = matches!(
            req.mode,
            MusicMode::Extract | MusicMode::Lego | MusicMode::Complete
        );
        match resolve_dit(state, want_base).await {
            Some(name) => {
                ace_body["synth_model"] = json!(name);
            }
            None if want_base => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "stem tasks (extract/lego/complete) need the base DiT — install \
                     acestep-v15-base-*.gguf in the music models dir",
                );
            }
            None => {} // single-DiT install / props unreachable → leave to engine default
        }
    }

    // Acquire the GPU as the music consumer; None = TTS preempted us first.
    let Some((guard, gate_wait)) = state.music.acquire().await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "GPU busy with higher-priority TTS — retry shortly",
        );
    };
    // Thread the gate-chosen card into the C++ `gpu` field (no-op if unconfigured).
    crate::routes::tier2_helpers::inject_gpu(&mut ace_body, guard.chosen_gpu());
    let started = Instant::now();

    // Any uploaded audio (cover/repaint source OR a generate-mode timbre ref) can't
    // be replayed into the form on restore — capture before `render` consumes them.
    let had_audio_inputs = src_audio.is_some() || ref_audio.is_some();
    let outcome = render(state, &req, &ace_body, src_audio, ref_audio).await;

    // Always release the GPU before doing disk/DB work.
    drop(guard);

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let gate_wait_ms = gate_wait.as_millis() as u64;

    let (audio_parts, lyrics_used) = match outcome {
        Ok(v) => v,
        Err(MusicError::Preempted) => {
            log_music(
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
                "render preempted by TTS — retry shortly",
            );
        }
        Err(e) => {
            warn!(error = %e, "music render failed");
            log_music(
                state,
                caller,
                &req,
                gate_wait_ms,
                elapsed_ms,
                502,
                json!({"error": e.to_string()}),
            );
            return err(StatusCode::BAD_GATEWAY, &format!("ace-server: {e}"));
        }
    };

    // Persist each take under SONGS_DIR.
    let base_seed = req.seed.unwrap_or(-1);
    let stamp = file_stamp();
    let mut tracks: Vec<MusicTrack> = Vec::new();
    for (i, part) in audio_parts.iter().enumerate() {
        let seed = if base_seed >= 0 {
            base_seed + i as i64
        } else {
            -1
        };
        let ext = ext_for_content_type(&part.content_type);
        let filename = format!("{stamp}-{}-{}.{ext}", short_seed(seed), i);
        if let Err(e) = save_song(state, &filename, &part.bytes).await {
            warn!(error = %e, %filename, "failed to save song");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to save rendered song",
            );
        }
        tracks.push(MusicTrack {
            url: format!("/api/v1/music/file/{filename}"),
            filename,
            seed,
            content_type: part.content_type.clone(),
            bytes: part.bytes.len() as u64,
        });
    }

    let resp = MusicGenerateResponse {
        tracks,
        duration_used,
        lyrics_used: if lyrics_used.is_empty() {
            None
        } else {
            Some(lyrics_used)
        },
        duration_ms: Some(elapsed_ms),
        gate_wait_ms: Some(gate_wait_ms),
        gpu_log_id: None,
    };
    log_music(
        state,
        caller,
        &req,
        gate_wait_ms,
        elapsed_ms,
        200,
        json!({"tracks": resp.tracks.len(), "duration": duration_used}),
    );

    // Gallery record — tracks are always persisted to SONGS_DIR.
    let outputs: Vec<GenerationOutput> = resp
        .tracks
        .iter()
        .map(|t| GenerationOutput {
            filename: t.filename.clone(),
            mime: t.content_type.clone(),
            size_bytes: t.bytes as i64,
            kind: "audio".to_string(),
        })
        .collect();
    if !outputs.is_empty() {
        record_generation(
            state,
            "music",
            endpoint_label(req.mode),
            crate::routes::tier2_helpers::meta_from_request(
                &req,
                &[
                    ("engine", json!("acestep")),
                    ("lyrics_used", json!(resp.lyrics_used)),
                    ("had_audio_inputs", json!(had_audio_inputs)),
                ],
            ),
            outputs,
        )
        .await;
    }

    Json(resp).into_response()
}

/// Stable endpoint label for the music mode (also used by `log_music`).
fn endpoint_label(mode: MusicMode) -> &'static str {
    match mode {
        MusicMode::Generate => "generate",
        MusicMode::Cover => "cover",
        MusicMode::Repaint => "repaint",
        MusicMode::Extract => "extract",
        MusicMode::Lego => "lego",
        MusicMode::Complete => "complete",
    }
}

/// Drive the ace-server pipeline to audio. Returns `(audio_parts, lyrics_used)`.
/// `ref_audio` is an optional timbre reference (any mode); `src_audio` is the
/// cover/repaint source.
async fn render(
    state: &AppState,
    req: &MusicGenerateRequest,
    ace_body: &Value,
    src_audio: Option<(Bytes, String)>,
    ref_audio: Option<(Bytes, String)>,
) -> Result<(Vec<AudioPart>, String), MusicError> {
    let client = &state.music;
    let takes = req.takes.unwrap_or(1).clamp(1, 9);

    match req.mode {
        MusicMode::Generate => {
            // Cap matches ace-server's --max-batch (compose: 4) — the LM KV
            // cache is pre-allocated for max_batch × max_seq.
            let songs = req.songs.unwrap_or(1).clamp(1, 4);
            // Phase 1+2 LM: produces `songs` enriched requests, each carrying its
            // own lyrics/metadata/audio_codes.
            let mut lm_body = ace_body.clone();
            lm_body["lm_batch_size"] = json!(songs);
            let lm_job = client.submit_lm(&lm_body).await?;
            client.await_job(&lm_job).await?;
            let enriched = client.fetch_lm_result(&lm_job).await?;
            if enriched.is_empty() {
                return Err(MusicError::JobFailed("empty LM result".into()));
            }
            let lyrics_used = enriched[0]
                .get("lyrics")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // The DiT synth is the heavy VRAM step; the ace-server's enriched LM
            // result doesn't echo the transport `gpu` field, so re-inject the
            // gate-chosen card (carried in `ace_body`) into every synth body or
            // the synth falls back to the worker's WORKER_DEFAULT_GPU.
            let gpu = ace_body.get("gpu").and_then(|v| v.as_str());
            // Each song renders `takes` DiT seed-variations → songs*takes tracks.
            let synth_bodies: Vec<Value> = enriched
                .into_iter()
                .map(|mut s| {
                    s["synth_batch_size"] = json!(takes);
                    if let Some(fmt) = req.output_format.as_deref() {
                        s["output_format"] = json!(fmt);
                    }
                    crate::routes::tier2_helpers::inject_gpu(&mut s, gpu);
                    s
                })
                .collect();
            let audio = if ref_audio.is_some() {
                // ace's multipart /synth parses `request` as a single AceRequest
                // (not an array), so with a timbre ref we submit each song as its
                // own multipart job and gather the audio.
                let mut all = Vec::new();
                for body in &synth_bodies {
                    let job = client.submit_synth(body, None, ref_audio.clone()).await?;
                    client.await_job(&job).await?;
                    all.extend(client.fetch_synth_audio(&job).await?);
                }
                all
            } else {
                // No files → one JSON /synth batch for all songs (most efficient).
                let job = client
                    .submit_synth(&json!(synth_bodies), None, None)
                    .await?;
                client.await_job(&job).await?;
                client.fetch_synth_audio(&job).await?
            };
            Ok((audio, lyrics_used))
        }
        MusicMode::Cover
        | MusicMode::Repaint
        | MusicMode::Extract
        | MusicMode::Lego
        | MusicMode::Complete => {
            let source =
                src_audio.ok_or_else(|| MusicError::JobFailed("missing source audio".into()))?;
            // cover-nofsq wants a timbre ref = the source for best results, unless
            // the caller supplied an explicit reference clip. repaint ignores it.
            let effective_ref = match (req.mode, ref_audio) {
                (_, Some(r)) => Some(r),
                (MusicMode::Cover, None) => Some(source.clone()),
                _ => None,
            };
            let synth_job = client
                .submit_synth(ace_body, Some(source), effective_ref)
                .await?;
            client.await_job(&synth_job).await?;
            let audio = client.fetch_synth_audio(&synth_job).await?;
            let lyrics = req.lyrics.clone().unwrap_or_default();
            Ok((audio, lyrics))
        }
    }
}

// ── request building ────────────────────────────────────────────────────

/// Human-readable language to instruct the lyric writer in, or None when the
/// lyrics should just follow the caption/theme. Returns None for English, the
/// `unknown`/blank "let the model choose" values, so only a deliberate
/// non-English choice pins the language. Maps the common ISO codes to a name the
/// 12B follows better than a bare code; an unmapped non-English code is passed
/// through as-is (the model can usually resolve it).
fn vocal_language_name(code: Option<&str>) -> Option<String> {
    let c = code
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_lowercase();
    let name = match c.as_str() {
        "en" | "english" | "unknown" => return None,
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "pt" => "Portuguese",
        "nl" => "Dutch",
        "pl" => "Polish",
        "ru" => "Russian",
        "tr" => "Turkish",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "ar" => "Arabic",
        "hi" => "Hindi",
        other => return Some(other.to_string()),
    };
    Some(name.to_string())
}

/// Ask the LLM to estimate a good track length in seconds for the drafted lyrics.
/// Experimental hint the UI shows under the lyrics for the user to accept or
/// ignore — it can weigh section count, repeated choruses, the tempo the style
/// implies and instrumental sections in a way the plain word-count heuristic
/// can't. Cheap (a single integer out); returns None on any failure or an
/// out-of-range answer so the UI simply shows nothing.
async fn suggest_duration_from_lyrics(
    state: &AppState,
    caption: &str,
    lyrics: &str,
) -> Option<f64> {
    let l = lyrics.trim();
    if l.is_empty() || l.eq_ignore_ascii_case("[instrumental]") {
        return None;
    }
    let body = json!({
        "messages": [
            {"role": "system", "content": "You estimate how long a finished song should be. \
             Given its style and lyrics, weigh the section count, repeated choruses, the tempo the \
             style implies and any instrumental sections. Reply with ONLY an integer number of \
             seconds — nothing else."},
            {"role": "user", "content": format!("Style: {caption}\n\nLyrics:\n{l}")},
        ],
        "max_tokens": 12,
        // 🔴 Deliberately NOT on the `sampling_music` knob. That knob is creative on
        // purpose (temp 1.0, for captions and lyrics); this call wants one integer and
        // nothing else, so it keeps its own low temperature. Point it at a creative
        // setting and the song length starts wandering.
        "temperature": 0.3,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    let (_permit, resp) = state.llama.chat_completion_json(&body).await.ok()?;
    let text = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())?;
    let n: i64 = text
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    (30..=600).contains(&n).then_some(n as f64)
}

/// Ask the main LLM (Gemma 4 12B via llama-server) to write lyrics. `caption`
/// describes the music; `lyrics_prompt` (optional) describes what the words
/// should be about. Returns the lyrics text (the llama client strips any
/// `<think>` block). Called before the music GPU lock is taken — the LLM is the
/// ungated always-resident model.
pub(crate) async fn write_lyrics_with_llm(
    state: &AppState,
    caption: &str,
    lyrics_prompt: Option<&str>,
    refine: Option<crate::routes::refine::Refine<'_>>,
    vocal_language: Option<&str>,
) -> Result<String, crate::llama::Error> {
    let theme = lyrics_prompt.map(str::trim).filter(|s| !s.is_empty());
    let mut user = match &refine {
        // Improve an existing draft under the same lyric rules — the caption/theme
        // ride along as context so the revision stays in style.
        Some(r) => {
            let mut context = format!("Music / style: {caption}");
            if let Some(brief) = theme {
                context.push_str(&format!("\nTheme: {brief}"));
            }
            crate::routes::refine::refine_user_msg("lyric draft", &context, r)
        }
        None => match theme {
            Some(brief) => format!("Music / style: {caption}\n\nWrite lyrics about: {brief}"),
            None => format!("Write lyrics for this track:\n\n{caption}"),
        },
    };
    // Pin the language only when it isn't the English default — ace sings whatever
    // language the caption's set to, so English lyrics under a non-English vocal
    // language come out garbled. English/unknown/blank hands the choice to the
    // caption and theme, so we say nothing.
    if let Some(lang) = vocal_language_name(vocal_language) {
        user.push_str(&format!("\n\nWrite the lyrics in {lang}."));
    }
    let mut body = json!({
        "messages": [
            {"role": "system", "content": lyrics_system_prompt()},
            {"role": "user", "content": user},
        ],
        // A full song reproduced with every structure/vocal/energy/instrumental
        // tag on its own line runs long; dense lyrics (~550-word hyperpop) alone
        // approach 700, so budget for the whole song plus tags without truncating.
        "max_tokens": 1024,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_music", &mut body).await;
    lyric_call(state, &body).await
}

/// One LLM call, content or empty string.
async fn lyric_call(state: &AppState, body: &Value) -> Result<String, crate::llama::Error> {
    let (_permit, resp) = state.llama.chat_completion_json(body).await?;
    Ok(resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string())
}

/// Rewrite only the lines whose endings do not rhyme when sung. Driven by the
/// user's Fix button, never automatically: perfect rhyme is not always wanted, and
/// a model that cannot hear the difference will not converge on a retry loop.
pub(crate) async fn fix_lyric_rhymes(
    state: &AppState,
    system: &str,
    lyrics: &str,
    flags: &[crate::rhyme::EyeRhyme],
) -> Result<String, crate::llama::Error> {
    let Some(fix) = crate::rhyme::fix_instruction(flags) else {
        return Ok(lyrics.to_string());
    };
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "assistant", "content": lyrics},
            {"role": "user", "content": fix},
        ],
        // Reproduces the whole song with fixed endings — same budget as a draft.
        "max_tokens": 1024,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_music", &mut body).await;
    let out = lyric_call(state, &body).await?;
    Ok(if out.is_empty() {
        lyrics.to_string()
    } else {
        out
    })
}

/// Compute the duration to request. User value wins; else, when sung lyrics
/// are known, `max(30, round(words)) + 12` (the LM's own guess causes
/// lyric-skipping). When lyrics are empty/instrumental, a sane default — NOT 0
/// — because `duration=0` lets the LM free-run to its 600s FSM cap, producing
/// 10-minute "two songs in one" tracks (and the near-max code-sketch is what
/// overran the KV). The user can override via the duration field.
const DEFAULT_AUTO_DURATION: f64 = 150.0;
/// Sung words per second used to estimate a song's length from its lyrics. The
/// handoff's original 1.0 was calibrated on slow kids' songs and massively
/// over-estimates dense material (a ~550-word hyperpop song is ~3.5 min, not
/// 9 min) — which made the LM stretch the lyrics over a 9-minute track. ~2.0
/// gives the words a touch more room than a strict sung pace; section tags
/// ([verse] etc.) count as words but the +12s tail margin absorbs that. Users
/// can always override the duration.
const WORDS_PER_SEC: f64 = 2.0;

fn resolve_duration(req: &MusicGenerateRequest, lyrics: &str) -> f64 {
    if let Some(d) = req.duration
        && d > 0.0
    {
        return d;
    }
    let trimmed = lyrics.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("[instrumental]") {
        return DEFAULT_AUTO_DURATION;
    }
    let words = sung_word_count(trimmed) as f64;
    (words / WORDS_PER_SEC).max(30.0) + 12.0
}

/// Count sung words, ignoring `[bracket]` performance tags. Structure, vocal,
/// energy and instrumental tags are directions, not sung words, so counting them
/// (e.g. `[Chorus - anthemic]` as three words) inflates the duration estimate —
/// the more so now the lyric prompt encourages many multi-word tags. Backing
/// vocals in `(parentheses)` ARE sung, so they stay counted.
fn sung_word_count(lyrics: &str) -> usize {
    let mut sung = String::with_capacity(lyrics.len());
    let mut depth: u32 = 0;
    for ch in lyrics.chars() {
        match ch {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => sung.push(ch),
            _ => {}
        }
    }
    sung.split_whitespace().count()
}

/// Build the ace-server AceRequest JSON from the koblem request.
/// Resolve the DiT filename to pin: the base model for stem tasks, otherwise turbo.
/// Reads the engine's `/props` DiT list and matches by name. Returns None when no
/// suitable DiT is installed (or `/props` is unreachable) — the caller decides
/// whether that's fatal (stem tasks) or falls back to the engine default.
async fn resolve_dit(state: &AppState, want_base: bool) -> Option<String> {
    let props = state.music.props().await.ok()?;
    let names: Vec<String> = props
        .pointer("/models/dit")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    if want_base {
        names.into_iter().find(|n| n.contains("base"))
    } else if let Some(turbo) = names.iter().find(|n| n.contains("turbo")) {
        Some(turbo.clone())
    } else {
        // No explicit turbo (unusual) — any non-base DiT beats defaulting to base.
        names.into_iter().find(|n| !n.contains("base"))
    }
}

fn build_ace_request(req: &MusicGenerateRequest, lyrics: &str, duration: f64) -> Value {
    let task_type = match req.mode {
        MusicMode::Generate => "text2music",
        // Two backend cover paths: `cover-nofsq` (clean latents → faithful remix,
        // needs matching lyrics) vs `cover` (FSQ roundtrip → loose "in the style of"
        // reinterpretation that tolerates new lyrics). See MusicGenerateRequest.
        MusicMode::Cover if req.cover_reference => "cover",
        MusicMode::Cover => "cover-nofsq",
        MusicMode::Repaint => "repaint",
        // Stem tasks — base DiT only, no LM. `track` selects the stem.
        MusicMode::Extract => "extract",
        MusicMode::Lego => "lego",
        MusicMode::Complete => "complete",
    };
    // Repaint regenerates a region; with no lyrics provided, regenerate it as
    // instrumental rather than feeding the DiT an empty lyric string.
    let lyrics = if req.mode == MusicMode::Repaint && lyrics.trim().is_empty() {
        "[Instrumental]"
    } else {
        lyrics
    };
    let mut body = json!({
        "caption": req.caption,
        "lyrics": lyrics,
        "task_type": task_type,
        // `seed` is the DiT (Philox) noise; `lm_seed` seeds the code-sketch LM's
        // mt19937 (default -1 = random_device). Tie both to the one user seed so
        // a fixed seed makes generation fully reproducible (same inputs → same
        // song); -1 leaves both random.
        "seed": req.seed.unwrap_or(-1),
        "lm_seed": req.seed.unwrap_or(-1),
        "duration": duration,
        "output_format": req.output_format.clone().unwrap_or_else(|| "mp3".into()),
        // ace defaults mp3 to 128 kbps — too lossy for 48 kHz stereo music
        // ("wishy-washy" highs), and a cover re-synth makes it worse. Ship the
        // near-transparent max. Ignored for wav output.
        "mp3_bitrate": 320,
    });
    let obj = body.as_object_mut().unwrap();
    // Seed-variations per render. cover/repaint read this straight from the
    // body; generate re-sets it per song after the LM (same value). Clamp to
    // ace's 1..9.
    obj.insert(
        "synth_batch_size".into(),
        json!(req.takes.unwrap_or(1).clamp(1, 9)),
    );
    // Pin the sung-vocal language. Without this ace's metadata FSM picks one at
    // random from its 51-language set, so English captions/lyrics still come
    // out sung in random languages. Default to English; `unknown` (or empty)
    // hands the choice back to the model. ace forces `unknown` for
    // `[Instrumental]` regardless, so the vocals-off path is unaffected.
    let vocal_language = req
        .vocal_language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("en");
    obj.insert("vocal_language".into(), json!(vocal_language));
    if let Some(m) = req.synth_model.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("synth_model".into(), json!(m));
    }
    // Never force a step count on the stem tasks: they run on the base DiT, whose
    // step schedule (50) the engine auto-detects from the model. A turbo-era
    // override (8) would produce incoherent output. Non-stem modes honor it.
    let is_stem = matches!(
        req.mode,
        MusicMode::Extract | MusicMode::Lego | MusicMode::Complete
    );
    if !is_stem && let Some(steps) = req.inference_steps.filter(|s| *s > 0) {
        obj.insert("inference_steps".into(), json!(steps));
    }
    if let Some(shift) = req.shift.filter(|s| *s > 0.0) {
        obj.insert("shift".into(), json!(shift));
    }
    match req.mode {
        MusicMode::Cover => {
            obj.insert(
                "audio_cover_strength".into(),
                json!(req.cover_strength.unwrap_or(0.7)),
            );
            // SDEdit init blend (0 = pure noise / most freedom). Only meaningful
            // for cover; omit when unset so the engine keeps its 0.0 default.
            if let Some(ns) = req.cover_noise_strength {
                obj.insert("cover_noise_strength".into(), json!(ns));
            }
        }
        MusicMode::Repaint => {
            obj.insert(
                "repainting_start".into(),
                json!(req.repaint_start.unwrap_or(0.0)),
            );
            obj.insert(
                "repainting_end".into(),
                json!(req.repaint_end.unwrap_or(-1.0)),
            );
        }
        // Stem tasks carry the target stem in `track`; the engine uses the full
        // source context (no cover-strength / repaint region).
        MusicMode::Extract | MusicMode::Lego | MusicMode::Complete => {
            if let Some(t) = req
                .track
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                obj.insert("track".into(), json!(t));
            }
        }
        MusicMode::Generate => {}
    }
    body
}

// ── helpers ───────────────────────────────────────────────────────────

/// Fields pulled from a music multipart body.
struct ParsedMusic {
    request: Option<MusicGenerateRequest>,
    /// `audio` part — the cover/repaint source.
    source: Option<(Bytes, String)>,
    /// `ref_audio` part — optional timbre reference.
    ref_audio: Option<(Bytes, String)>,
}

/// Parse a music multipart body into its `request` JSON + `audio`/`ref_audio`
/// file parts. Returns an error `Response` on a malformed part.
async fn parse_music_multipart(mut mp: Multipart) -> Result<ParsedMusic, Response> {
    let mut out = ParsedMusic {
        request: None,
        source: None,
        ref_audio: None,
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
                        let r = serde_json::from_str::<MusicGenerateRequest>(&t).map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("invalid request JSON: {e}"),
                            )
                        })?;
                        out.request = Some(r);
                    }
                    "audio" => {
                        let b = field.bytes().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading audio field: {e}"),
                            )
                        })?;
                        out.source = Some((b, filename.unwrap_or_else(|| "source.wav".into())));
                    }
                    "ref_audio" => {
                        let b = field.bytes().await.map_err(|e| {
                            err(
                                StatusCode::BAD_REQUEST,
                                &format!("reading ref_audio field: {e}"),
                            )
                        })?;
                        out.ref_audio = Some((b, filename.unwrap_or_else(|| "ref.wav".into())));
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
fn log_music(
    state: &AppState,
    caller: Option<String>,
    req: &MusicGenerateRequest,
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
                "mode": endpoint,
                "caption": req.caption,
                "takes": req.takes.unwrap_or(1),
                "output_format": req.output_format.clone().unwrap_or_else(|| "mp3".into()),
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

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn ext_for_content_type(ct: &str) -> &'static str {
    if ct.contains("mpeg") { "mp3" } else { "wav" }
}

fn content_type_for(name: &str) -> &'static str {
    if name.ends_with(".mp3") {
        "audio/mpeg"
    } else {
        "audio/wav"
    }
}

/// Compact UTC timestamp for filenames: `20260522-134501`.
fn file_stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn short_seed(seed: i64) -> String {
    if seed < 0 {
        "rnd".into()
    } else {
        seed.to_string()
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sung_word_count_ignores_bracket_tags() {
        // Tags (including multi-word ones) don't count; backing vocals do.
        let lyrics =
            "[Verse]\nHold on to the light\n[Chorus - anthemic]\nWe rise (we rise)\n[Guitar Solo]";
        // "Hold on to the light" (5) + "We rise" (2) + "(we rise)" (2) = 9
        assert_eq!(sung_word_count(lyrics), 9);
    }

    #[test]
    fn resolve_duration_ignores_tag_density() {
        // Same sung content, wildly different tag density → identical estimate.
        let req = MusicGenerateRequest::default();
        let light = "[Verse]\none two three four five six seven eight nine ten";
        let heavy = "[Intro - ambient]\n[Verse]\none two three four five six seven eight nine ten\n\
                     [Pre-Chorus]\n[Chorus - anthemic]\n[Guitar Solo]\n[Drum Break]\n[Outro - fade out]";
        assert_eq!(resolve_duration(&req, light), resolve_duration(&req, heavy));
    }

    #[test]
    fn vocal_language_name_only_pins_non_english() {
        assert_eq!(vocal_language_name(None), None);
        assert_eq!(vocal_language_name(Some("")), None);
        assert_eq!(vocal_language_name(Some("en")), None);
        assert_eq!(vocal_language_name(Some("English")), None);
        assert_eq!(vocal_language_name(Some("unknown")), None);
        assert_eq!(vocal_language_name(Some("es")).as_deref(), Some("Spanish"));
        assert_eq!(vocal_language_name(Some("JA")).as_deref(), Some("Japanese"));
        // Unmapped non-English code passes through for the model to resolve.
        assert_eq!(vocal_language_name(Some("sv")).as_deref(), Some("sv"));
    }
}
