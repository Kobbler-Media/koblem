use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use kob_providers::LibraryProvider;
use kob_providers::releases::{InterpretInput, ReleaseProvider};

use crate::AppState;
use crate::auth::require_auth;
use crate::repositories::settings as settings_repo;

#[derive(Deserialize)]
struct TestProviderRequest {
    url: Option<String>,
    token: Option<String>,
}

#[derive(Serialize)]
pub struct TestProviderResponse {
    success: bool,
    message: String,
}

/// Resolve provider URL + token: prefer body params, fall back to saved settings.
async fn resolve_provider_params(
    pool: &sqlx::PgPool,
    body: Option<TestProviderRequest>,
    prefix: &str,
) -> (Option<String>, String) {
    let url_key = format!("{prefix}_url");
    let token_key = format!("{prefix}_token");

    let url = body
        .as_ref()
        .and_then(|b| b.url.clone())
        .filter(|u| !u.is_empty())
        .or(settings_repo::get_setting(pool, &url_key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string())));

    let token = body
        .as_ref()
        .and_then(|b| b.token.clone())
        .filter(|t| !t.is_empty());

    let token = match token {
        Some(t) => t,
        None => settings_repo::get_setting(pool, &token_key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default(),
    };

    (url, token)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/settings", get(get_settings).patch(update_settings))
        .route(
            "/api/v1/settings/test-library-provider",
            post(test_library_provider),
        )
        .route(
            "/api/v1/settings/test-release-provider",
            post(test_release_provider),
        )
        .route("/api/v1/settings/test-gpu-service", post(test_gpu_service))
}

async fn get_settings(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match settings_repo::get_all_settings(&state.db).await {
        Ok(all) => {
            // Return flat key-value object for easy frontend consumption
            let mut map = serde_json::Map::new();
            for (key, value) in all {
                // 🔴 Omitted rather than masked: the settings page saves the object it was handed
                // back, and a placeholder would overwrite the real key with the placeholder.
                if key == crate::queue::push::PRIVATE_KEY {
                    continue;
                }
                map.insert(key, value);
            }
            Json(Value::Object(map)).into_response()
        }
        Err(err) => {
            warn!(error = %err, "failed to get settings");
            (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response()
        }
    }
}

async fn update_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => {
            return (StatusCode::BAD_REQUEST, "expected JSON object").into_response();
        }
    };

    let mut gpu_urls_changed = false;
    for (key, value) in obj {
        if let Err(err) = settings_repo::set_setting(&state.db, key, value).await {
            warn!(error = %err, key = %key, "failed to set setting");
            return (StatusCode::INTERNAL_SERVER_ERROR, "database error").into_response();
        }

        // Hot-reload GPU-lock-affecting settings
        match key.as_str() {
            "gpu_lock_enabled" => {
                let enabled = value.as_bool().unwrap_or(true);
                state.gpu.set_enabled(enabled);
            }
            "gpu_tts_hold_seconds" => {
                let secs = value.as_u64().unwrap_or(30);
                state.gpu.set_tts_hold_seconds(secs);
            }
            "vision_service_url"
            | "text_embed_service_url"
            | "tts_breeze_service_url"
            | "stt_service_url" => {
                gpu_urls_changed = true;
            }
            // Music URL: propagate to the running client so a
            // settings edit doesn't require a koblem-api restart. The
            // MusicClient set_base_url method takes an
            // Option<String> — empty string clears the URL (lazy-fail on
            // next call), matching the existing STT/TTS/Vision shape.
            "ace_server_url" => {
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.music.set_base_url(url);
            }
            "flux2_server_url" => {
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.flux2.set_base_url(url);
            }
            "krea2_server_url" => {
                // krea2 self-registers on the gate's drain/unload registry
                // (set_base_url re-registers under HeavyKind::Krea2), same shape
                // as flux2 — but on its own `krea2` budget line.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.krea2.set_base_url(url);
            }
            "ltx25_server_url" => {
                // LTX-2.5 (the `ltx25` plugin on comfy-api). 🔴 Like `music3` and `upscale` below,
                // it deliberately does NOT re-register on the gate: its URL is the comfy-api
                // container, already registered under HeavyKind::MinimaxH3 by the H3 client, and a
                // second entry would only give the gate another name for one process.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.ltx25.set_base_url(url);
            }
            "music3_server_url" => {
                // MiniMax Music 3 (the `music3` plugin on comfy-api). 🔴 Like `ltx25` above and
                // for the same reason, it does NOT register on the gate: the URL is the comfy-api
                // container, already registered under HeavyKind::MinimaxH3 by the H3 client, and a
                // second registration would replace an entry the gate's eviction path needs.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.music3.set_base_url(url);
            }
            "upscale_server_url" => {
                // The upscaler (the `upscale` plugin on the SAME comfy-api). 🔴 Same
                // non-registration, same reason: four plugins, one container, one gate entry.
                //
                // ⚠️ Pointing this at a different host than `minimax_h3_server_url` does NOT give
                // the upscaler its own card — it would still take the `minimax_h3` budget line,
                // because that line is what this client acquires. It also would not see the shared
                // staging mount, so every hand-off would fail at `stage_path`. If the upscaler ever
                // moves hosts, both of those have to move with it.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.upscale.set_base_url(url);
            }
            "minimax_h3_server_url" => {
                // minimax_h3 self-registers as a HEAVY peer (set_base_url re-registers
                // on the gate under HeavyKind::MinimaxH3), same shape as ltx.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.minimax_h3.set_base_url(url);
            }
            "songgen_server_url" => {
                // songgen is a self-registering HEAVY peer (set_base_url
                // re-registers on the gate), same shape as flux2.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.songgen.set_base_url(url);
            }
            "sa3_server_url" => {
                // sa3 self-registers on the gate's light music registry (same
                // shape as acestep) — set_base_url re-registers under tag "sa3".
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.sa3.set_base_url(url);
            }
            "llama_server_url" => {
                // The always-resident LLM. set_base_url also re-registers the
                // gate's LLM heavy service so the drain/unload dance follows the
                // new host. Empty is ignored (the LLM must always have a URL).
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.llama.set_base_url(url);
            }
            "matting_service_url" => {
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.matting.set_base_url(url);
            }
            "spar3d_service_url" => {
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.spar3d.set_base_url(url);
            }
            "pixal3d_server_url" => {
                // Nothing to re-register on the gate: `pixal3d` is an acquire-only budget line
                // with no drain/unload endpoint registered, so only the client's own target moves.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.pixal3d.set_base_url(url);
            }
            "depth_service_url" => {
                // Shares matting's gate budget line, so nothing to re-register
                // here — only the client's own target changes. One container
                // serves `/depth` and `/parts`, so both clients move together.
                let url = value
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                state.depth.set_base_url(url.clone());
                state.parts.set_base_url(url);
            }
            _ => {}
        }
    }

    if gpu_urls_changed {
        let stt = resolve_service_url(&state, "stt_service_url").await;
        let tts_breeze = resolve_service_url(&state, "tts_breeze_service_url").await;
        let vision = resolve_service_url(&state, "vision_service_url").await;
        let text_embed = resolve_service_url(&state, "text_embed_service_url").await;
        // STT + Vision via the legacy single-URL setter; TTS via the engine
        // registry so the gate can unload its worker on eviction.
        state
            .gpu
            .set_service_urls(stt.clone(), None, vision.clone());
        state
            .gpu
            .set_tts_services(crate::build_tts_services(&tts_breeze));
        state.gpu.set_text_embed_url(text_embed.clone());
        // Also propagate into the live HTTP clients; otherwise they'd keep
        // the base_url they captured at startup and return NotConfigured.
        state.stt.set_base_url(stt);
        state.tts.set_base_url(tts_breeze);
        state.vision.set_base_url(vision);
        state.vision.set_text_url(text_embed);
    }

    StatusCode::NO_CONTENT.into_response()
}

/// Resolve a service URL for the GPU lock: DB settings override env-var fallback.
async fn resolve_service_url(state: &AppState, key: &str) -> Option<String> {
    let from_db = settings_repo::get_setting(&state.db, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .filter(|s| !s.is_empty());
    if from_db.is_some() {
        return from_db;
    }
    match key {
        "stt_service_url" => state.config.stt_url.clone(),
        "tts_breeze_service_url" => state.config.tts_breeze_url.clone(),
        "vision_service_url" => state.config.vision_url.clone(),
        "text_embed_service_url" => state.config.text_embed_url.clone(),
        "ace_server_url" => state.config.ace_server_url.clone(),
        "songgen_server_url" => state.config.songgen_server_url.clone(),
        "sa3_server_url" => state.config.sa3_server_url.clone(),
        "flux2_server_url" => state.config.flux2_url.clone(),
        "krea2_server_url" => state.config.krea2_url.clone(),
        "ltx25_server_url" => state.config.ltx25_url.clone(),
        "music3_server_url" => state.config.music3_url.clone(),
        "upscale_server_url" => state.config.upscale_url.clone(),
        "minimax_h3_server_url" => state.config.minimax_h3_url.clone(),
        "matting_service_url" => state.config.matting_url.clone(),
        "spar3d_service_url" => state.config.spar3d_url.clone(),
        "pixal3d_server_url" => state.config.pixal3d_url.clone(),
        "depth_service_url" => state.config.depth_url.clone(),
        "llama_server_url" => Some(state.config.llama_server_url.clone()),
        _ => None,
    }
}

async fn test_library_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<TestProviderRequest>>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Use body params if provided, fall back to saved settings
    let (url, token) =
        resolve_provider_params(&state.db, body.map(|b| b.0), "library_provider").await;

    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => {
            return Json(TestProviderResponse {
                success: false,
                message: "library provider URL not configured".to_string(),
            })
            .into_response();
        }
    };

    let provider = kob_providers::HttpLibraryProvider::new(url, token);
    match provider.search_shows("test").await {
        Ok(shows) => {
            let count = shows.len();
            Json(TestProviderResponse {
                success: true,
                message: format!("connected successfully, found {count} shows for test query"),
            })
            .into_response()
        }
        Err(e) => Json(TestProviderResponse {
            success: false,
            message: format!("connection failed: {e}"),
        })
        .into_response(),
    }
}

// ── GPU service connectivity probes ──────────────────────────────────

#[derive(Deserialize)]
struct TestGpuServiceRequest {
    /// "llm" | "vision" | "text_embed" | "tts" | "stt" | "music" | "sa3"
    /// | "music3" | "upscale" | "songgen" | "flux2" | "krea2" | "ltx"
    /// | "minimax_h3" | "matting" | "spar3d" | "depth"
    ///
    /// `"ltx"` is the request-side key the web settings page has always sent; it now resolves to
    /// `ltx25_server_url`, LTX-2.5 being the only LTX engine.
    service: String,
    /// Optional override URL. If empty/missing, falls back to saved setting
    /// or env-var default.
    url: Option<String>,
}

#[derive(Serialize)]
struct TestGpuServiceResponse {
    success: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

async fn test_gpu_service(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TestGpuServiceRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let setting_key = match req.service.as_str() {
        "vision" => "vision_service_url",
        "text_embed" => "text_embed_service_url",
        "tts" => "tts_breeze_service_url",
        "stt" => "stt_service_url",
        "music" => "ace_server_url",
        "songgen" => "songgen_server_url",
        "sa3" => "sa3_server_url",
        "music3" => "music3_server_url",
        "upscale" => "upscale_server_url",
        "flux2" => "flux2_server_url",
        "krea2" => "krea2_server_url",
        "ltx" => "ltx25_server_url",
        "minimax_h3" => "minimax_h3_server_url",
        "matting" => "matting_service_url",
        "spar3d" => "spar3d_service_url",
        // longcat-rig answers `/health` (which the probe below appends) without initialising CUDA
        // and while its weights are unloaded, so this is a real liveness check.
        "pixal3d" => "pixal3d_server_url",
        "depth" => "depth_service_url",
        "llm" => "llama_server_url",
        _ => {
            return Json(TestGpuServiceResponse {
                success: false,
                message:
                    "unknown service (expected llm/vision/text_embed/tts/stt/music/sa3/music3/upscale/songgen/flux2/krea2/ltx/minimax_h3/matting/spar3d/pixal3d/depth)"
                        .to_string(),
                model: None,
            })
            .into_response();
        }
    };

    // Prefer override from request body, fall back to saved setting, then env var.
    let url = match req.url {
        Some(u) if !u.is_empty() => Some(u),
        _ => resolve_service_url(&state, setting_key).await,
    };
    let Some(url) = url.filter(|s| !s.is_empty()) else {
        return Json(TestGpuServiceResponse {
            success: false,
            message: format!("{} service URL not configured", req.service),
            model: None,
        })
        .into_response();
    };

    // Build a one-shot client against the override URL (not the AppState client,
    // which always points at whatever the startup/resolved URL was).
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");
    let probe_url = format!("{}/health", url.trim_end_matches('/'));
    match http.get(&probe_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            // Best-effort model name extraction. tts-breeze puts the id under
            // `model_id`; parakeet/vision under `model`.
            let body: Value = resp.json().await.unwrap_or_default();
            let model = body
                .get("model_id")
                .or_else(|| body.get("model"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Json(TestGpuServiceResponse {
                success: true,
                message: model
                    .clone()
                    .map(|m| format!("connected — model: {m}"))
                    .unwrap_or_else(|| "connected".to_string()),
                model,
            })
            .into_response()
        }
        Ok(resp) => Json(TestGpuServiceResponse {
            success: false,
            message: format!("service returned {}", resp.status()),
            model: None,
        })
        .into_response(),
        Err(e) => Json(TestGpuServiceResponse {
            success: false,
            message: format!("connection failed: {e}"),
            model: None,
        })
        .into_response(),
    }
}

// ── Release provider ─────────────────────────────────────────────────

async fn test_release_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<TestProviderRequest>>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Use body params if provided, fall back to saved settings
    let (url, token) =
        resolve_provider_params(&state.db, body.map(|b| b.0), "release_provider").await;

    let url = match url {
        Some(u) if !u.is_empty() => u,
        _ => {
            return Json(TestProviderResponse {
                success: false,
                message: "release provider URL not configured".to_string(),
            })
            .into_response();
        }
    };

    let provider = kob_providers::HttpReleaseProvider::new(url, token);
    let test_input = InterpretInput {
        name: "Test.Release.720p.WEB-DL.x264".to_string(),
        media_kind: "tv".to_string(),
    };
    match provider.interpret_releases(&[test_input]).await {
        Ok(results) => {
            let count = results.len();
            Json(TestProviderResponse {
                success: true,
                message: format!("connected successfully, interpreted {count} test release(s)"),
            })
            .into_response()
        }
        Err(e) => Json(TestProviderResponse {
            success: false,
            message: format!("connection failed: {e}"),
        })
        .into_response(),
    }
}
