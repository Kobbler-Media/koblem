//! Which engines are actually usable right now. Filtered at request time from
//! `is_configured()` + the gate's per-service enable flag + a live health probe,
//! so the model is never offered an engine that would 503 the moment it picks
//! it — and the `engine` enum it sees is the truth about the box.

use std::time::Duration;

use kob_gpu_gate::ServiceKind;

use crate::AppState;

use super::tools::{LiveTool, Tool, catalogue};

/// A health probe that hangs must not hold up the turn. The engine clients set
/// a 5 s connect timeout and no request timeout by design (a render legitimately
/// takes minutes); `/health` is not a render, so it gets its own short leash.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// The tools to advertise on this turn, engine lists already filtered. A tool
/// with no live engine is dropped entirely.
pub async fn live_tools(state: &AppState) -> Vec<LiveTool> {
    let probes = catalogue()
        .iter()
        .flat_map(|d| d.engines.iter().filter_map(|e| e.service))
        .map(|kind| async move { (kind, engine_live(state, kind).await) });
    let results: Vec<(ServiceKind, bool)> = futures::future::join_all(probes).await;

    let mut out = Vec::new();
    for d in catalogue() {
        let engines: Vec<_> = d
            .engines
            .iter()
            .filter(|e| match e.service {
                // No GPU service behind it (the director library is a database
                // read), so there is nothing that could be down.
                None => true,
                Some(kind) => results
                    .iter()
                    .find(|(k, _)| *k == kind)
                    .map(|(_, ok)| *ok)
                    .unwrap_or(false),
            })
            .collect();
        if engines.is_empty() {
            continue;
        }
        out.push(LiveTool {
            tool: d.tool,
            engines,
        });
    }
    out
}

pub fn find(tools: &[LiveTool], tool: Tool) -> Option<&LiveTool> {
    tools.iter().find(|t| t.tool == tool)
}

async fn engine_live(state: &AppState, kind: ServiceKind) -> bool {
    if !state.gpu.is_service_enabled(kind) {
        return false;
    }
    // ⚠️ `ServiceKind::Ltx` is the CATALOGUE key for the LTX tool, not the budget line the render
    // takes — LTX-2.5 queues as `minimax_h3`, sharing one comfy-api process with H3. So the gate's
    // per-service switch for `Ltx` hides the tool from the model without stopping a render
    // dispatched another way, and disabling `minimax_h3` stops the render without hiding the tool.
    let configured = match kind {
        ServiceKind::Krea2 => state.krea2.is_configured(),
        ServiceKind::Flux => state.flux2.is_configured(),
        ServiceKind::Ltx => state.ltx25.is_configured(),
        ServiceKind::Music => state.music.is_configured(),
        ServiceKind::Sa3 => state.sa3.is_configured(),
        ServiceKind::Songgen => state.songgen.is_configured(),
        ServiceKind::Matting => state.matting.is_configured(),
        ServiceKind::Spar3d => state.spar3d.is_configured(),
        ServiceKind::Pixal3d => state.pixal3d.is_configured(),
        // TTS/STT/Vision clients carry their URL in the gate's service table
        // rather than an is_configured() flag; the health probe is the whole
        // test.
        ServiceKind::Tts | ServiceKind::Stt | ServiceKind::Vision => true,
        _ => false,
    };
    if !configured {
        return false;
    }
    tokio::time::timeout(HEALTH_TIMEOUT, healthy(state, kind))
        .await
        .unwrap_or(false)
}

async fn healthy(state: &AppState, kind: ServiceKind) -> bool {
    match kind {
        ServiceKind::Krea2 => state.krea2.healthy().await,
        ServiceKind::Flux => state.flux2.healthy().await,
        ServiceKind::Ltx => state.ltx25.healthy().await,
        ServiceKind::Music => state.music.healthy().await,
        ServiceKind::Sa3 => state.sa3.healthy().await,
        ServiceKind::Songgen => state.songgen.healthy().await,
        ServiceKind::Matting => state.matting.health().await.is_ok(),
        // The supervisor answers /v1/health without initialising CUDA, so this costs nothing
        // and is honest about whether a render could actually start.
        ServiceKind::Spar3d => state
            .spar3d
            .health()
            .await
            .map(|h| h.status == "ok")
            .unwrap_or(false),
        // Same property as spar3d's: longcat-rig answers /health without initialising CUDA and
        // while its weights are unloaded, so this is honest about whether a rig could start.
        ServiceKind::Pixal3d => state.pixal3d.healthy().await,
        ServiceKind::Vision => state.vision.health().await.unwrap_or(false),
        ServiceKind::Tts => state.tts.health_info().await.is_ok(),
        ServiceKind::Stt => state.stt.health().await.unwrap_or(false),
        _ => false,
    }
}
