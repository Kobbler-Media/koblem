//! Out-of-band GPU residency reconciliation.
//!
//! We sometimes call the C++ servers directly (bypassing koblem and the gate).
//! Those workers hold VRAM the budget scheduler can't see, which would let it
//! place another service on top and OOM. To stay honest, this loop polls each
//! registered server's announce endpoint `GET /v1/gpu/status` on an interval and
//! reconciles the gate's ledger:
//!
//! - `{loaded: true, gpu: "<uuid>"}` → [`GpuLock::observe`] the service on that
//!   card (reserves its peak in the budget; idempotent). The gate itself ignores
//!   an observe within a kind's eviction cooldown, so a worker mid-shutdown can't
//!   be resurrected onto the card the gate just handed to its evictor.
//! - `{loaded: false}` → [`GpuLock::unobserve`], which drops ONLY an out-of-band
//!   `Observed` placement — never a live guard-held or standing (LLM) reservation.
//!   This is what reclaims budget from a direct-API worker that has since idled or
//!   crashed (the old loop was observe-only and leaked those forever).
//! - `{loaded: true, busy: true}` → [`GpuLock::set_reported_busy`]: a direct-API
//!   caller is using the worker (a live STT stream), so the planner must not reclaim
//!   it as idle. Cleared when it reports `busy: false` or unloads.
//! - `{gpu, mem_free_mb}` → [`GpuLock::set_measured_free`]: a server's real free
//!   VRAM for its card clamps admission below the pure-peak budget, so an under-set
//!   peak or a non-gate VRAM consumer can't be scheduled over. Absent (servers that
//!   don't report it) → this secondary source contributes nothing.
//!
//! The **primary** measured-VRAM source is `nvidia-smi`, polled once per tick from
//! this process: it reports every card's real free VRAM directly (covering idle
//! cards and non-gate processes too), keyed by GPU UUID matching `gpu_cards`. When
//! `nvidia-smi` isn't available (koblem-api has no GPU access, or a non-NVIDIA
//! host) the poll disables itself after one failed attempt and the clamp degrades
//! to the announce `mem_free_mb` / pure-peak budget. In a topology where koblem-api
//! is NOT co-located with the GPUs, the UUIDs won't match `gpu_cards`, so stray
//! readings are simply ignored — never applied to the wrong card.
//!
//! Servers that don't expose the endpoint (404 / connection error / bad shape)
//! are silently skipped — the loop is best-effort and never blocks placement.

use std::time::Duration;

use kob_gpu_gate::GpuLock;
use serde::Deserialize;
use tracing::{debug, warn};

/// The announce-endpoint shape (`GET /v1/gpu/status`). All fields optional so a
/// partial / older server response degrades gracefully.
#[derive(Debug, Default, Deserialize)]
struct GpuStatus {
    #[serde(default)]
    loaded: bool,
    #[serde(default)]
    gpu: Option<String>,
    /// Real free VRAM (MiB) on the server's card, if the server reports it. Drives
    /// the measured-VRAM admission clamp; omitted → pure-peak budget only.
    #[serde(default)]
    mem_free_mb: Option<u32>,
    /// A caller koblem can't see is using the worker right now (a live STT stream,
    /// a file transcription posted straight at the server). It holds no guard, so
    /// without this the queue planner reclaims it as idle mid-use. Absent → false.
    #[serde(default)]
    busy: bool,
}

/// Poll interval. Short enough to catch a direct-API worker before the next
/// placement, long enough to be negligible load.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Run the reconcile loop forever. Spawned once at startup.
pub async fn run(gpu: GpuLock) {
    // Dedicated short-timeout client — a hung server must not stall reconcile.
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "gpu_reconcile: failed to build HTTP client; loop disabled");
            return;
        }
    };

    // nvidia-smi is the primary measured-VRAM source. Disabled after one failed
    // attempt (binary absent / no GPU access) so we don't respawn a doomed process
    // every tick — the clamp then leans on the announce `mem_free_mb` / pure budget.
    let mut nvidia_smi = true;

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        if nvidia_smi {
            match query_nvidia_smi().await {
                Ok(readings) => {
                    for (uuid, free_mb) in readings {
                        // set_measured_free ignores UUIDs not in gpu_cards, so a
                        // non-co-located host's GPUs are harmlessly skipped.
                        gpu.set_measured_free(&uuid, free_mb);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "gpu_reconcile: nvidia-smi unavailable; measured-VRAM clamp falls back to server mem_free_mb / pure budget");
                    nvidia_smi = false;
                }
            }
        }

        // The set of services + URLs can change at runtime (settings edits), so
        // re-read it every tick.
        for (kind, base_url) in gpu.reconcile_targets() {
            let url = format!("{}/v1/gpu/status", base_url.trim_end_matches('/'));
            let status: Option<GpuStatus> = match http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => resp.json().await.ok(),
                _ => None,
            };
            let Some(status) = status else {
                // No announce endpoint / unreachable — leave the ledger alone.
                continue;
            };

            // Feed the measured-VRAM clamp whenever the server reports both a card
            // and its real free memory (independent of loaded state — an unloaded
            // worker can still report the card's free VRAM).
            if let (Some(card), Some(free)) = (status.gpu.as_deref(), status.mem_free_mb)
                && !card.is_empty()
            {
                gpu.set_measured_free(card, free);
            }

            if status.loaded {
                gpu.set_reported_busy(kind, status.busy);
                if let Some(card) = status.gpu.as_deref().filter(|c| !c.is_empty()) {
                    if gpu.is_card_disabled(card) {
                        // Backstop: the worker landed on a card the operator
                        // disabled (a request path bypassed admission and fell
                        // back to WORKER_DEFAULT_GPU). Drain + SIGKILL it so it
                        // respawns on an enabled card next request — route-agnostic
                        // enforcement of "no work on a disabled card".
                        if gpu.evict_off_disabled_card(kind, card).await {
                            warn!(service = kind.as_str(), card = %card, "gpu_reconcile: worker on DISABLED card — evicting to relocate");
                        }
                    } else {
                        // Reserve the worker's peak (the gate skips this within the
                        // kind's eviction cooldown). Restores the LLM's standing seed
                        // after an eviction-cancel once it has actually reloaded.
                        gpu.observe(kind, card);
                        debug!(service = kind.as_str(), card = %card, "gpu_reconcile: observed resident worker");
                    }
                }
            } else {
                // Worker reports unloaded → reclaim any out-of-band reservation.
                gpu.unobserve(kind);
            }
        }

        // Reclaim the LLM's home card once the contention that evicted it clears
        // ("drop the LLM until home is clean, then bring it back").
        gpu.rehome_standing();
    }
}

/// Query every local NVIDIA GPU's real free VRAM via `nvidia-smi`. Returns
/// `(uuid, free_mb)` pairs; `Err` if the binary is missing or exits non-zero.
async fn query_nvidia_smi() -> Result<Vec<(String, u32)>, String> {
    let out = tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=uuid,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("nvidia-smi exited with {}", out.status));
    }
    Ok(parse_nvidia_smi(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `nvidia-smi --query-gpu=uuid,memory.free --format=csv,noheader,nounits`
/// output into `(uuid, free_mb)` pairs. Tolerant of blank lines / malformed rows.
fn parse_nvidia_smi(output: &str) -> Vec<(String, u32)> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(',');
            let uuid = parts.next()?.trim();
            let free = parts.next()?.trim().parse::<u32>().ok()?;
            (!uuid.is_empty()).then(|| (uuid.to_string(), free))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_smi_csv() {
        let out = "GPU-00000000-0000-0000-0000-000000000001, 8000\n\
                   GPU-00000000-0000-0000-0000-000000000002, 15000\n";
        let got = parse_nvidia_smi(out);
        assert_eq!(
            got,
            vec![
                ("GPU-00000000-0000-0000-0000-000000000001".to_string(), 8000),
                (
                    "GPU-00000000-0000-0000-0000-000000000002".to_string(),
                    15000
                ),
            ]
        );
    }

    #[test]
    fn skips_blank_and_malformed_rows() {
        let out = "\nGPU-aaa, 1234\ngarbage line\nGPU-bbb, notanumber\n, 500\n";
        assert_eq!(parse_nvidia_smi(out), vec![("GPU-aaa".to_string(), 1234)]);
    }
}
