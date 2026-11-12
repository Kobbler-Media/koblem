mod agent;
mod auth;
mod beefy;
mod config;
mod context;
mod depth;
mod director_assets;
mod director_think;
mod fixtures;
mod flux2;
mod jobs;
mod krea2;
mod llama;
mod ltx_jobs;
mod ltx_video;
mod matting;
mod media_sign;
mod minimax_h3;
mod music;
mod music3;
mod music3_prompt;
mod nats;
mod parts;
mod pixal3d;
mod pixal3d_jobs;
mod prompt_engine;
mod prompts;
pub(crate) mod queue;
mod repositories;
mod review_helpers;
pub(crate) mod review_pipeline;
mod rhyme;
mod routes;
mod sa3;
mod sampling;
mod songgen;
mod spar3d;
mod strict_json;
mod stt;
mod think;
mod tool_rewrite;
mod tts_breeze;
mod upscale;
mod vision;
mod wire;
#[cfg(test)]
mod wire_json_tests;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use kob_gpu_gate::{GpuLock, HeavyKind, HeavyService, TtsService};
use kob_providers::{LibraryProvider, ReleaseProvider};
use sqlx::postgres::PgPoolOptions;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// A late-bound handle to the queue dispatcher. See [`AppState::queue`].
#[derive(Clone, Default)]
pub struct QueueHandle(Arc<std::sync::OnceLock<queue::Dispatcher>>);

impl QueueHandle {
    pub fn set(&self, d: queue::Dispatcher) {
        let _ = self.0.set(d);
    }

    pub fn get(&self) -> Option<&queue::Dispatcher> {
        self.0.get()
    }

    /// Nudge the dispatcher after an enqueue, reorder or toggle. A no-op before
    /// the dispatcher is wired, which is only ever true during boot.
    pub fn wake(&self) {
        if let Some(d) = self.0.get() {
            d.wake();
        }
    }

    /// Announce a state transition on the queue's event spine, if there is one to announce it on.
    pub fn announce(
        &self,
        job_id: &str,
        kind: koblem_client::types::JobKind,
        state: koblem_client::types::JobState,
    ) {
        if let Some(d) = self.0.get() {
            d.announce(job_id, kind, state);
        }
    }

    /// Cancel a row that has not started, and its dependents with it.
    ///
    /// 🔴 The ONE way to do that. Through the dispatcher when it is on this process, so each
    /// adapter can retire the engine-side record it keeps — see `JobRunner::abandon`, and the day
    /// the Director spent re-adopting a dead render because nothing did. Without a dispatcher this
    /// falls back to the plain row writes, which is the honest answer rather than a silent skip:
    /// the records live on whichever process holds them, so a cancel issued here could not reach
    /// them anyway, and the readers' queue-row overlay is what keeps that case correct.
    pub async fn abandon_pending(&self, db: &sqlx::PgPool, id: &str) -> bool {
        if let Some(d) = self.0.get() {
            return d.abandon_pending(id).await;
        }
        let cancelled = repositories::gpu_jobs::cancel_pending(db, id)
            .await
            .unwrap_or(false);
        let _ = repositories::gpu_jobs::cascade_dependents(db, id).await;
        cancelled
    }
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub llama: llama::LlamaClient,
    /// Optional auto-director planning LLM (gemma-3-12b-it on its own
    /// llama-server). `None` → the `/ltx-director/plan` route falls back to the
    /// main `llama` (Qwen). See `config.director_llm_url`.
    pub director_llm: Option<llama::LlamaClient>,
    pub config: Arc<config::Config>,
    pub db: sqlx::PgPool,
    pub library_provider: Option<Arc<dyn LibraryProvider>>,
    pub release_provider: Option<Arc<dyn ReleaseProvider>>,
    pub api_key: Option<String>,
    /// Signs short-lived gallery URLs so an `<img src>` never has to carry a real credential
    /// (§6.1). See `media_sign.rs`.
    pub media_signer: Arc<media_sign::MediaSigner>,
    /// VRAM-budget GPU scheduler shared across every GPU service (the LLM
    /// included). Each `acquire` picks a physical card by budget and the chosen
    /// UUID is threaded into the C++ server's per-request `gpu` field.
    pub gpu: GpuLock,
    /// The global GPU job queue's dispatcher.
    ///
    /// Behind a `OnceLock` because the two need each other: the adapters hold an
    /// `AppState` (they persist artifacts, write gallery rows and read engine
    /// clients), and the enqueue routes hold the dispatcher to wake it. Both are
    /// process-lifetime singletons, so the resulting Arc cycle never needs to
    /// break — but it IS a cycle, and that is why this is a handle rather than a
    /// plain field.
    pub queue: QueueHandle,
    /// Where a run's working files live: segment directories, engine clips on their way to the
    /// gallery, and the sources `/upscale/upload` captures at enqueue. Keyed by queue job id and
    /// swept by `jobs::gpu_retention`; nothing in it is a deliverable. See `queue::files`.
    pub queue_files: queue::files::QueueFiles,
    pub vision: vision::VisionClient,
    pub tts: tts_breeze::BreezeTtsClient,
    pub stt: stt::SttClient,
    /// ACE-Step music engine (acestep.cpp ace-server), gated as `music`.
    pub music: music::MusicClient,
    /// songgen.cpp (LeVo2) — the heavier music engine offered as a tab alongside
    /// acestep. Gated as an on-demand HEAVY (like Flux): it drains the LLM
    /// and serialises against the other heavies, because its ~7 GB render won't
    /// fit beside the resident LLM in acestep's light `music` lane.
    pub songgen: songgen::SonggenClient,
    /// Stable Audio 3 (sa3-server) — the LIGHT music engine: text→audio + a2a/
    /// inpaint/continuation editing. Shares acestep's light `music` gate (all-Q8
    /// weights ≈ 2.67 GB co-reside with the LLM), self-registering on the music
    /// registry under the `sa3` tag.
    pub sa3: sa3::Sa3Client,
    /// MiniMax Music 3 — the `music3` plugin on the `comfy-api` shim. Text→song only (the DAV VAE
    /// cannot encode, so there is no audio input of any kind).
    ///
    /// 🔴 It takes the **`minimax_h3`** gate line and registers no drain/unload URL, for exactly
    /// the reason `ltx25` does: `music3`, `h3` and `ltx25` are three plugins on ONE comfy-api
    /// process in front of ONE ComfyUI with a single-slot queue. They cannot render at once and
    /// they share the resident-weights budget, so a line of its own would let the gate admit it
    /// beside an H3 render and double-count the card. See `music3.rs`'s module header.
    pub music3: music3::Music3Client,
    /// The LTX-2.5 upscaler — the `upscale` plugin on the same `comfy-api` shim. A RESTORER: an
    /// mp4 in, a bigger mp4 out. Nothing is prompted; two knobs, `noise` and `detailer`.
    ///
    /// 🔴 It replaced SeedVR2 on measurements (2026-08-14): 48 s against 230 s for the same clip,
    /// and 0.84x the temporal flicker of a bicubic control against SeedVR2's 1.25x.
    ///
    /// 🔴 It takes the **`minimax_h3`** gate line and registers no drain/unload URL, for exactly
    /// the reason `music3` and `ltx25` do — four plugins, one comfy-api process, one ComfyUI, one
    /// single-slot queue. A consequence worth knowing: an upscale cannot overlap a render, and an
    /// H3 chain is ONE prompt for its whole job, so an upscale submitted mid-render queues behind
    /// it. That is why Upscale is its own tab rather than a post-phase on a render.
    pub upscale: upscale::UpscaleClient,
    /// The directory a source clip is staged into so comfy-api can read it BY PATH.
    ///
    /// 🔴 Both containers mount it at the SAME absolute path. A source video is routinely
    /// gigabytes and handing over a path is what keeps those bytes off the HTTP hop entirely.
    pub upscale_staging_dir: Arc<std::path::PathBuf>,
    /// FLUX.2-Klein image-gen engine (flux2.cpp sd-server), gated as the `flux`
    /// heavy slot. Drains the LLM + evicts light peers on acquire.
    pub flux2: flux2::Flux2Client,
    /// Krea-2-Turbo text→image engine (the `krea2` sd-server — same sd.cpp fork
    /// as flux2, different checkpoint), gated on its OWN `krea2` budget line: the
    /// ~7.75 GiB nvfp4 DiT can't co-reside with FLUX.2, so the two image engines
    /// serialise by VRAM budget. Text-to-image only — Krea 2 has no edit model.
    pub krea2: krea2::Krea2Client,
    /// LTX-2.5 — the `ltx25` plugin on the `comfy-api` shim, and the ONLY LTX engine. It renders
    /// T2V/I2V/AT2V/AI2V chains over the wire the retired C++ `ltx-video` service defined, so the
    /// client keeps that module's name. Gated on the `minimax_h3` budget line it shares with its
    /// co-tenant, and it registers no drain/unload URL of its own — see `ltx_video.rs`.
    ///
    /// 🔴 Its renders land in the SHARED `ltx_jobs` store alongside H3's, and `LtxJob::engine`
    /// (`"ltx25"`) is what picks the client for the poll, the cancel and the restart-reattach —
    /// and the container the clip is written under (mp4, not webm).
    pub ltx25: ltx_video::LtxVideoClient,
    /// koblem-side store of in-flight + recently-finished LTX renders. A render runs in a
    /// background task (holding the GPU guard); the browser polls this store, so a
    /// disconnect/navigation no longer drops the guard or loses the result.
    pub ltx_jobs: ltx_jobs::LtxJobStore,
    /// Owned, non-swept dir where a Director run's rendered webms are copied out of the TTL'd
    /// `ltx_jobs` store so a saved project's shots survive the 1h sweep (`director-renders/
    /// <project_id>/`, bind-mounted in compose). See `routes::ltx_director` + `director_shot_renders`.
    pub director_renders_dir: Arc<std::path::PathBuf>,
    /// Content-addressed Director editor assets (see `director_assets`). Bind-mounted like the
    /// render store; a project's timeline references these instead of inlining their bytes.
    pub director_assets_dir: Arc<std::path::PathBuf>,
    /// MiniMax-H3 joint audio+video engine — the `h3-wangp` container, gated as the `minimax_h3`
    /// slot. The Director's SECOND engine, and the only one in the fleet that is NOT the sd.cpp
    /// fork: it is a WanGP runtime with its own wire contract.
    ///
    /// 🔴 Its unit is a GENERATION (up to ~15 s holding 1-N shots, whose cuts are `[Shot N]`
    /// markers in one prompt), not a segment. It generates its own soundtrack in the same denoise
    /// as the picture, takes no audio input, and has no LoRAs / CFG / variants / bank splice.
    ///
    /// Its renders land in the SHARED `ltx_jobs` store, not one of their own, because everything
    /// downstream of submit — poll, progressive partials, media, the Director harvest — is the
    /// same code path. `LtxJob::engine` says which client owns a job, and is also what decides the
    /// clip's container (mp4 here, webm on LTX).
    pub minimax_h3: minimax_h3::MinimaxH3Client,
    /// Background-removal (matting) engine — vision.cpp BiRefNet / RMBG-2.0.
    /// Gated under the budget scheduler as `matting` (~3.5 GB peak at res 2048).
    pub matting: matting::MattingClient,
    /// Depth-estimation engine — the SAME vision-server binary/worker as
    /// `matting`, so it acquires the `matting` budget line rather than one of its
    /// own (see `depth.rs`).
    pub depth: depth::DepthClient,
    /// SAM 3 part segmentation — the SAME vision-server container, worker and
    /// budget line as `matting`/`depth`, so it is built from the depth URL and
    /// shares that gate line (see `parts.rs`).
    pub parts: parts::PartsClient,
    /// SPAR3D + RigAnything image-to-rig service. Its worker is isolated and
    /// scheduled under its own tested VRAM budget line.
    pub spar3d: spar3d::Spar3dClient,
    /// **Pixal3D** — the longcat-rig `avatar_server`. Its own client, its own store and its own
    /// budget line beside `spar3d`: the two solve the same problem, share no implementation, and a
    /// rig here peaks near 8.7 GB against spar3d's 2.6 GB reservation.
    pub pixal3d: pixal3d::Pixal3dClient,
    /// The character catalogue: rigs, the motion-clip library, animation takes and browser edits.
    /// 🔴 No TTL — a rigged character is ~10 minutes of GPU and the only copy. See
    /// `pixal3d_jobs.rs`.
    pub pixal3d_jobs: pixal3d_jobs::Pixal3dStore,
    /// Where a browser-edited rig is written so `/v1/animate` can open it. Both containers mount
    /// this at the SAME absolute path; without it an edit is saved but not animatable.
    pub pixal3d_staging_dir: Arc<std::path::PathBuf>,
    /// Where tier-2 saved audio/image samples live on disk.
    pub samples_dir: Arc<std::path::PathBuf>,
    /// Where rendered music tracks are saved (bind-mounted to a host directory).
    pub songs_dir: Arc<std::path::PathBuf>,
    /// Where the retired avatar engine's clips were saved; still the gallery root for those rows.
    pub avatars_dir: Arc<std::path::PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = config::Config::from_env();
    info!(
        llama_url = %config.llama_server_url,
        bind = %config.bind_address,
        context_size = config.context_size,
        "Starting koblem"
    );

    // Connect to PostgreSQL
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await
        .expect("failed to connect to database");

    // Run migrations
    sqlx::migrate!("./migrations")
        .run(&db)
        .await
        .expect("failed to run migrations");
    info!("database migrations complete");

    // Browser push signs every request with a VAPID keypair the instance owns. Minted here rather
    // than on first use so there is nothing to configure by hand and no chance of two requests
    // racing to persist different private keys.
    if queue::push::ensure_vapid(&db).await.is_none() {
        warn!("push: no VAPID keypair; browser push notifications are unavailable");
    }

    // Sweep any llm_logs rows left as `running` by a prior crashed process —
    // they'd otherwise tick "running" forever in the UI.
    match repositories::llm_logs::cancel_stale_running(&db).await {
        Ok(0) => {}
        Ok(n) => info!(rows = n, "cancelled stale running llm_logs from prior run"),
        Err(e) => warn!(error = %e, "failed to sweep stale running llm_logs"),
    }

    // Same sweep for agent turns whose background task died with the process —
    // the browser polls a `running` turn forever otherwise.
    match agent::store::cancel_stale_running(&db).await {
        Ok(0) => {}
        Ok(n) => info!(
            rows = n,
            "cancelled stale running agent turns from prior run"
        ),
        Err(e) => warn!(error = %e, "failed to sweep stale running agent turns"),
    }

    // Create initial admin user if env vars are set and no users exist
    if let Some(ref email) = config.admin_email
        && let Some(ref password) = config.admin_password
    {
        match repositories::auth::user_count(&db).await {
            Ok(0) => {
                let email = email.trim().to_lowercase();
                match auth::hash_password(password) {
                    Ok(hash) => match repositories::auth::create_user(&db, &email, &hash).await {
                        Ok(id) => info!(%id, %email, "created initial admin user"),
                        Err(e) => warn!(error = %e, "failed to create initial admin user"),
                    },
                    Err(e) => warn!(error = %e, "failed to hash admin password"),
                }
            }
            Ok(count) => {
                info!(count, "users already exist, skipping admin bootstrap");
            }
            Err(e) => warn!(error = %e, "failed to check user count during bootstrap"),
        }
    }

    // Initialize providers from settings
    let library_provider = init_library_provider(&db).await;
    let release_provider = init_release_provider(&db).await;

    let api_key = config.api_key.clone();

    // Shared VRAM-budget GPU scheduler. There is no heavy/light lane: every GPU
    // service is admitted onto a physical card iff its tested peak fits the card's
    // remaining budget, possibly after evicting/relocating lower-priority
    // residents. Service URLs (DB settings override env fallbacks) are registered
    // so the scheduler can SIGKILL a worker to free its VRAM on eviction.
    let gpu = GpuLock::new();
    // 🔴 DB-backed like every other engine URL (`resolve_gpu_url` below), NOT read
    // straight off the env. Beefy mode rewrites this settings row to point at the
    // both-cards Qwen3.8 container; without the DB read the flip silently
    // evaporates on the next koblem restart and this process talks to a gemma
    // that Beefy has DRAINED — 503 on every chat, while the settings page still
    // shows the beefy URL. The env value stays the fallback for a fresh install.
    let llama_url = resolve_gpu_url(
        &db,
        "llama_server_url",
        &Some(config.llama_server_url.clone()),
    )
    .await
    .filter(|s| !s.trim().is_empty())
    .unwrap_or_else(|| config.llama_server_url.clone());
    info!(llama_url = %llama_url, "LLM endpoint resolved (DB setting overrides env)");
    let llama = llama::LlamaClient::new(llama_url.clone(), gpu.clone());
    // Auto-director planning LLM (gemma-3-12b-it on its own llama-server). A
    // plain second client on the shared GpuLock budget — it does NOT register a
    // heavy service (only the main LLM owns the resident-drain dance), so this
    // is a call-path wire only; standing gemma up as an on-demand heavy is a
    // GPU-gated deploy follow-up. None → the
    // /plan route falls back to the main `llama` (Qwen).
    let director_llm = config
        .director_llm_url
        .as_ref()
        .map(|u| llama::LlamaClient::new(u.clone(), gpu.clone()));
    let stt_url = resolve_gpu_url(&db, "stt_service_url", &config.stt_url).await;
    let tts_breeze_url =
        resolve_gpu_url(&db, "tts_breeze_service_url", &config.tts_breeze_url).await;
    let vision_url = resolve_gpu_url(&db, "vision_service_url", &config.vision_url).await;
    let text_embed_url =
        resolve_gpu_url(&db, "text_embed_service_url", &config.text_embed_url).await;
    // STT + Vision keep their single-URL setter; TTS goes through the engine
    // registry so the gate can unload its worker during eviction. The tag MUST
    // appear in `ServiceKind::Tts.light_tags()` or eviction silently no-ops.
    gpu.set_service_urls(stt_url.clone(), None, vision_url.clone());
    gpu.set_tts_services(build_tts_services(&tts_breeze_url));
    gpu.set_text_embed_url(text_embed_url.clone());

    // gpu_lock_enabled defaults to true; setting stored as JSON bool.
    let enabled = repositories::settings::get_setting(&db, "gpu_lock_enabled")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    gpu.set_enabled(enabled);

    // TTS hold-after-drop: keeps the GPU reserved for sentence-by-sentence
    // streaming without losing it to a queued background job. 0 disables.
    let hold = repositories::settings::get_setting(&db, "gpu_tts_hold_seconds")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .unwrap_or(30);
    gpu.set_tts_hold_seconds(hold);

    // Register the LLM heavy service on the gate. Without this,
    // a heavy acquire silently skips the drain dance and the render fights the
    // always-resident LLM for VRAM. The koblem-llama fork
    // ships `/v1/admin/{drain,unload}` (GO-LIVE-PLAN §3.4.b); the gate POSTs
    // to drain_path first, waits up to llm_drain_seconds for in-flight chats
    // to finish naturally, then POSTs unload_path to SIGKILL the worker child.
    if !llama_url.trim().is_empty() {
        gpu.set_llm_service(HeavyService {
            kind: HeavyKind::Llm,
            url: llama_url.clone(),
            unload_path: "/v1/admin/unload".to_string(),
            drain_path: "/v1/admin/drain".to_string(),
        });
    }

    // Multi-GPU scheduler: load the per-card budgets + per-service placement
    // config (gpu_cards / gpu_engine_config) into the gate's placement layer.
    // The gate stays a no-op single-card arbiter until these tables are seeded
    // (the migration seeds the 3060 + 5060 Ti and the §5 config), after which
    // each acquire chooses a physical card and threads it into the C++ `gpu`
    // request field. Edits via the settings page re-call `load_into_gate`.
    if let Err(e) = repositories::gpu_scheduler::load_into_gate(&db, &gpu).await {
        warn!(error = %e, "failed to load GPU scheduler config into the gate");
    }

    // 🔴 AFTER `load_into_gate`: that call seeds the LLM's standing reservation
    // and pushes card/engine config, so this is the only order where the ledger
    // is already correct when exclusive mode is re-armed. Restores the two
    // in-memory flags Beefy owns (the gate's exclusive lock and the client's
    // both-cards flag); the persisted URL was already restored above by
    // `resolve_gpu_url`.
    beefy::apply_at_boot(&db, &gpu, &llama, &config).await;

    // Beefy weights watcher: while the box is held for the 27B and beefy mode
    // is on, keep the weights resident in the background. A beefy container
    // restart (a runtime update) self-heals within one interval instead of
    // making the next request pay the ~40 s load inline; the request path is
    // the backstop for the request that arrives first.
    tokio::spawn(beefy::watch_weights(
        db.clone(),
        gpu.clone(),
        config.beefy_llm_url.clone(),
    ));

    // ace-server (music) URL. STRICT-evict, and with worker isolation
    // (ACE_WORKER_ISOLATION=1) it ships /v1/admin/unload that SIGKILLs its GPU
    // worker → true-0 VRAM. The MusicClient self-registers the URL on the gate
    // (set_music_service), so TTS preemption / §6.5 disable force-unload acestep
    // instead of waiting out its idle window. It still also acquires (evicting
    // STT/Vision peers from its own side).
    let ace_server_url = resolve_gpu_url(&db, "ace_server_url", &config.ace_server_url).await;

    // songgen-server (the LeVo2 music engine) URL. The SonggenClient self-registers
    // on the gate's HEAVY registry (set_songgen_service), so acquire_for_songgen
    // drains+unloads the LLM and evicts light peers, it serialises against
    // Flux, and a peer-heavy acquire or operator disable force-unloads its
    // render child to true-0 VRAM via /unload. (acestep stays the light `music`
    // engine; songgen's ~7 GB render is too big to co-reside with the LLM there.)
    let songgen_server_url =
        resolve_gpu_url(&db, "songgen_server_url", &config.songgen_server_url).await;

    // sa3-server (Stable Audio 3 — the LIGHT music engine) URL. The Sa3Client
    // self-registers on the gate's MUSIC registry under tag "sa3" (set_music_service)
    // so an eviction / operator disable can force-unload its warm worker via /unload
    // (true-0 VRAM). It is its OWN gate budget line (ServiceKind::Sa3, ~2.8 GB peak) —
    // no longer sharing acestep's slot — so it co-resides with other engines whenever
    // its small peak fits (unlike the ~7 GB songgen).
    let sa3_server_url = resolve_gpu_url(&db, "sa3_server_url", &config.sa3_server_url).await;

    // flux2.cpp sd-server (FLUX.2-Klein image gen) URL. The Flux2Client
    // self-registers on the gate's heavy slot when configured (set_flux_service),
    // so acquire_for_flux can drain the LLM + evict light peers and the §6.5
    // disable can SIGKILL renders.
    let flux2_url = resolve_gpu_url(&db, "flux2_server_url", &config.flux2_url).await;

    // krea2 sd-server (Krea-2-Turbo text→image) URL. Same self-registration shape
    // as flux2 (set_krea2_service), but its own gate budget line so the two image
    // engines serialise on VRAM instead of clobbering one shared slot.
    let krea2_url = resolve_gpu_url(&db, "krea2_server_url", &config.krea2_url).await;

    // matting-server (vision.cpp background removal) URL. Now gated under the
    // budget scheduler as `matting`; the MattingClient self-registers the URL so
    // the gate can SIGKILL its worker on eviction/disable.
    let matting_url = resolve_gpu_url(&db, "matting_service_url", &config.matting_url).await;

    // GPU-service clients. They all share the same GpuLock.
    let vision = vision::VisionClient::new(
        vision_url,
        text_embed_url,
        config.text_embed_max_tokens,
        gpu.clone(),
    );
    let tts = tts_breeze::BreezeTtsClient::new(tts_breeze_url, gpu.clone());
    let stt = stt::SttClient::new(stt_url, gpu.clone());
    let music = music::MusicClient::new(ace_server_url, gpu.clone());
    let songgen = songgen::SonggenClient::new(songgen_server_url.clone(), gpu.clone());
    let sa3 = sa3::Sa3Client::new(sa3_server_url.clone(), gpu.clone());
    // MiniMax Music 3 (`music3` on comfy-api). Resolved at boot like every other service — the DB
    // `settings` row wins over the env fallback — and a Settings edit reaches the running client
    // through the `music3_server_url` arm in `routes::settings`. It registers nothing on the gate:
    // the comfy-api URL is already registered under HeavyKind::MinimaxH3 by the H3 client below.
    let music3_url = resolve_gpu_url(&db, "music3_server_url", &config.music3_url).await;
    let music3 = music3::Music3Client::new(music3_url.clone(), gpu.clone());
    // Sidecar-persisted like the LTX store, so a restart mid-render can re-attach rather than
    // orphan a job that is still burning the card. Same dir convention and same env override.
    // The upscaler (`upscale` on the SAME comfy-api). Same resolution order, same gate reasoning, and
    // the same non-registration: the comfy-api URL is already registered under
    // HeavyKind::MinimaxH3 by the H3 client below.
    let upscale_url = resolve_gpu_url(&db, "upscale_server_url", &config.upscale_url).await;
    let upscale = upscale::UpscaleClient::new(upscale_url, gpu.clone());
    // 🔴 The hand-off directory, and the one thing about it that matters is that comfy-api mounts
    // the SAME host path at the SAME absolute path. Created here so a fresh deployment does not
    // discover the mount is missing halfway through a caller's first upload.
    let upscale_staging_dir = std::path::PathBuf::from(&config.upscale_staging_dir);
    if let Err(e) = std::fs::create_dir_all(&upscale_staging_dir) {
        tracing::warn!(
            path = %upscale_staging_dir.display(), error = %e,
            "🔴 the upscale staging directory is not writable — Video → Upscale will refuse every \
             request until it is. Both koblem and comfy-api must mount it at this absolute path.",
        );
    }
    let flux2 = flux2::Flux2Client::new(flux2_url.clone(), gpu.clone());
    let krea2 = krea2::Krea2Client::new(krea2_url.clone(), gpu.clone());
    // LTX-2.5 (`ltx25` on comfy-api). Resolved here, at BOOT, like every other service: the DB
    // `settings` row wins over the env fallback, and a Settings edit reaches the running client
    // only through the `ltx25_server_url` arm in `routes::settings`.
    let ltx25_url = resolve_gpu_url(&db, "ltx25_server_url", &config.ltx25_url).await;
    let ltx25 = ltx_video::LtxVideoClient::new(ltx25_url, gpu.clone());
    // MiniMax-H3 joint audio+video — the `h3-wangp` shim (a WanGP runtime, not the sd.cpp fork),
    // speaking its own `/h3/v1/*` contract. Self-registers on the gate under HeavyKind::MinimaxH3
    // — the budget line LTX-2.5 also queues on, since the two are plugins on one comfy-api process
    // in front of one ComfyUI. The registration is the H3 client's alone: two entries for one
    // container would only give the gate a second name for the process it already knows how to
    // drain.
    //
    // 🔴 One WanGP process per box, enforced shim-side with a flock: mmgp pins ~31 GB of HOST RAM
    // independent of VRAM, so a free GPU is not sufficient evidence that a second one can start.
    let minimax_h3_url =
        resolve_gpu_url(&db, "minimax_h3_server_url", &config.minimax_h3_url).await;
    let minimax_h3 = minimax_h3::MinimaxH3Client::new(minimax_h3_url, gpu.clone());
    // Gated under the budget scheduler as `matting` (self-registers its URL).
    let matting = matting::MattingClient::new(matting_url.clone(), gpu.clone());
    let spar3d_url = resolve_gpu_url(&db, "spar3d_service_url", &config.spar3d_url).await;
    let spar3d = spar3d::Spar3dClient::new(spar3d_url, gpu.clone());
    // Pixal3D (longcat-rig). Same resolution order as every other engine — the DB `settings` row
    // wins, the env var is the fallback — and an acquire-only gate line: the service idle-unloads
    // its own weights after 300 s and a gate-driven unload racing a job it cannot see would kill a
    // render it had already admitted.
    let pixal3d_url = resolve_gpu_url(&db, "pixal3d_server_url", &config.pixal3d_url).await;
    let pixal3d = pixal3d::Pixal3dClient::new(pixal3d_url.clone(), gpu.clone());
    let pixal3d_dir = std::path::PathBuf::from(
        std::env::var("PIXAL3D_DIR").unwrap_or_else(|_| "/var/lib/koblem/pixal3d".to_string()),
    );
    let pixal3d_jobs = pixal3d_jobs::Pixal3dStore::open(pixal3d_dir);
    let pixal3d_staging_dir = std::path::PathBuf::from(&config.pixal3d_staging_dir);
    if let Err(e) = std::fs::create_dir_all(&pixal3d_staging_dir) {
        tracing::warn!(
            path = %pixal3d_staging_dir.display(), error = %e,
            "the pixal3d staging directory is not writable — a rig repaired in the browser can \
             still be saved and downloaded, but not animated. Both koblem and longcat-rig must \
             mount it at this absolute path.",
        );
    }

    // vision-server `/depth` (Depth-Anything V2 by default). Same container as matting today,
    // its own URL so it can move later. Shares matting's gate budget line and,
    // with it, matting's gate URL registration — so if the two ever point at
    // different hosts the gate can no longer SIGKILL the depth worker.
    let depth_url = resolve_gpu_url(&db, "depth_service_url", &config.depth_url).await;
    let trimmed = |s: &str| s.trim().trim_end_matches('/').to_string();
    if let Some(d) = depth_url.as_deref()
        && matting_url.as_deref().map(trimmed) != Some(trimmed(d))
    {
        warn!(
            depth_url = %d,
            matting_url = matting_url.as_deref().unwrap_or("<unset>"),
            "depth and matting point at different hosts; depth shares matting's gate budget \
             line and the gate can only unload the worker reachable via MATTING_SERVICE_URL"
        );
    }
    let depth = depth::DepthClient::new(depth_url.clone(), gpu.clone());
    // `/parts` is a third endpoint on that same container, so it takes the same
    // URL rather than a settings key of its own.
    let parts = parts::PartsClient::new(depth_url, gpu.clone());

    // Samples directory for tier-2 saved audio/image files. Configurable via
    // SAMPLES_DIR env (defaults to /var/lib/koblem/gpu_samples in container,
    // ./target/gpu_samples locally). Created at startup.
    let samples_dir = std::path::PathBuf::from(
        std::env::var("SAMPLES_DIR").unwrap_or_else(|_| "/var/lib/koblem/gpu_samples".to_string()),
    );
    if let Err(e) = std::fs::create_dir_all(&samples_dir) {
        warn!(path = %samples_dir.display(), error = %e, "failed to create samples dir");
    }
    let samples_dir = Arc::new(samples_dir);

    // Rendered-music output directory. Bind-mounted to a host directory in
    // compose; defaults under /var/lib/koblem locally. Created at startup.
    let songs_dir = std::path::PathBuf::from(
        std::env::var("SONGS_DIR").unwrap_or_else(|_| "/var/lib/koblem/songs".to_string()),
    );
    if let Err(e) = std::fs::create_dir_all(&songs_dir) {
        warn!(path = %songs_dir.display(), error = %e, "failed to create songs dir");
    }
    let songs_dir = Arc::new(songs_dir);

    // The retired avatar engine's output directory, kept as the gallery root for
    // its existing rows. Bind-mounted in compose.
    let avatars_dir = std::path::PathBuf::from(
        std::env::var("AVATARS_DIR").unwrap_or_else(|_| "/var/lib/koblem/avatars".to_string()),
    );
    if let Err(e) = std::fs::create_dir_all(&avatars_dir) {
        warn!(path = %avatars_dir.display(), error = %e, "failed to create avatars dir");
    }
    let avatars_dir = Arc::new(avatars_dir);

    // LTX render job store: metadata sidecars + finished clips. Disk-backed + bind-mounted
    // (compose) so in-flight renders can be re-attached and clips re-fetched across a koblem
    // restart, not just a browser disconnect.
    let ltx_jobs_dir = std::path::PathBuf::from(
        std::env::var("LTX_JOBS_DIR").unwrap_or_else(|_| "/var/lib/koblem/ltx-jobs".to_string()),
    );
    let ltx_jobs = ltx_jobs::LtxJobStore::open(ltx_jobs_dir);

    // Owned Director render store: rendered webms copied out of the TTL'd `ltx_jobs` store so a
    // saved project's shots outlive the 1h sweep. Bind-mounted (compose) like the job store, but
    // never swept — a project owns its dir until the project is deleted.
    let director_renders_dir = std::path::PathBuf::from(
        std::env::var("DIRECTOR_RENDERS_DIR")
            .unwrap_or_else(|_| "/var/lib/koblem/director-renders".to_string()),
    );
    if let Err(e) = std::fs::create_dir_all(&director_renders_dir) {
        warn!(path = %director_renders_dir.display(), error = %e, "failed to create director renders dir");
    }

    // Content-addressed editor assets (stills, scene images, keyframe pins, uploaded v2v clips,
    // per-shot audio). Kept out of the project document so autosave/undo/render stay small.
    let director_assets_dir = std::path::PathBuf::from(
        std::env::var("DIRECTOR_ASSETS_DIR")
            .unwrap_or_else(|_| "/var/lib/koblem/director-assets".to_string()),
    );
    if let Err(e) = std::fs::create_dir_all(&director_assets_dir) {
        warn!(path = %director_assets_dir.display(), error = %e, "failed to create director assets dir");
    }

    // The queue's working files. 🔴 Bind-mount `QUEUE_FILES_DIR` in compose: an upscale resumes
    // from the segments already under it, so a root inside the container image re-renders finished
    // GPU work after every restart.
    let queue_files = queue::files::QueueFiles::open(
        std::path::Path::new(&config.queue_files_dir),
        std::path::Path::new(&config.queue_capture_dir),
    );

    // Derived from the API key so signed gallery URLs survive a restart and are invalidated by a
    // key rotation (§6.1).
    let media_signer = Arc::new(media_sign::MediaSigner::new(api_key.as_deref()));

    // Lane → base URL for the queue's health probe, captured while the resolved
    // URLs are still in scope. An unconfigured lane is simply absent, which reads
    // as healthy rather than down — the dispatcher must not refuse every job on a
    // box where an engine was never deployed.
    //
    // 🔴 ONE url for the whole comfy lane: h3, ltx25, music3 and upscale are four
    // plugins on one process, and its `/health` is what says both whether ComfyUI
    // is reachable and whether a render koblem did not schedule is in flight.
    let queue_health_lanes: Vec<(queue::lane::Lane, String)> = [
        (queue::lane::Lane::ComfyApi, music3_url.clone()),
        (queue::lane::Lane::Flux2, flux2_url.clone()),
        (queue::lane::Lane::Krea2, krea2_url.clone()),
        (queue::lane::Lane::Songgen, songgen_server_url.clone()),
        (queue::lane::Lane::Sa3, sa3_server_url.clone()),
        (queue::lane::Lane::LongcatRig, pixal3d_url.clone()),
    ]
    .into_iter()
    .filter_map(|(lane, url)| url.filter(|u| !u.trim().is_empty()).map(|u| (lane, u)))
    .collect();

    let state = AppState {
        llama,
        director_llm,
        config: Arc::new(config),
        db,
        library_provider,
        release_provider,
        api_key,
        media_signer,
        gpu,
        queue: QueueHandle::default(),
        queue_files,
        vision,
        tts,
        stt,
        music,
        songgen,
        sa3,
        music3,
        upscale,
        upscale_staging_dir: Arc::new(upscale_staging_dir),
        flux2,
        krea2,
        ltx25,
        ltx_jobs,
        director_renders_dir: Arc::new(director_renders_dir),
        director_assets_dir: Arc::new(director_assets_dir.clone()),
        minimax_h3,
        matting,
        depth,
        parts,
        spar3d,
        pixal3d,
        pixal3d_jobs,
        pixal3d_staging_dir: Arc::new(pixal3d_staging_dir),
        samples_dir,
        songs_dir,
        avatars_dir,
    };

    // Existing routes (no auth required — LLM proxy layer)
    let legacy_routes = Router::new()
        .route("/health", get(routes::health::health))
        .route("/v1/models", get(routes::models::list_models))
        .route(
            "/v1/chat/completions",
            post(routes::chat_completions::chat_completions),
        )
        .route(
            // Externally reachable mirror of the OpenAI proxy (auth-gated).
            // Same handler, same body, same response — just behind require_auth
            // so it can be hit through the public /api/ nginx prefix.
            "/api/v1/chat/completions",
            post(routes::chat_completions::chat_completions_authed),
        )
        .route(
            "/filter-releases",
            post(routes::filter_releases::filter_releases),
        )
        .route(
            "/validate-release",
            post(routes::validate_release::validate_release),
        )
        .route("/rank-releases", post(routes::rank_releases::rank_releases));

    // New API routes (auth required, handled per-handler)
    let app = legacy_routes
        .merge(routes::auth::routes())
        .merge(routes::settings::routes())
        .merge(routes::corrections::routes())
        .merge(routes::review::routes())
        .merge(routes::playground::routes())
        .merge(routes::llm_logs::routes())
        .merge(routes::chat::routes())
        .merge(routes::agent_chat::routes())
        .merge(routes::library::routes())
        .merge(routes::testbed::routes())
        .merge(routes::tier1_gpu::routes())
        .merge(routes::gpu_status::routes())
        .merge(routes::beefy_proxy::routes())
        .merge(routes::tts_playground::routes())
        .merge(routes::music::routes())
        .merge(routes::songgen::routes())
        .merge(routes::sa3::routes())
        .merge(routes::music3::routes())
        .merge(routes::upscale::routes())
        .merge(routes::images::routes())
        .merge(routes::krea2::routes())
        .merge(routes::oneshot::routes())
        .merge(routes::character_sheet::routes())
        .merge(routes::summon::routes())
        .merge(routes::ltx_video::routes())
        .merge(routes::minimax_h3::routes())
        .merge(routes::ltx_director::routes())
        .merge(routes::matting::routes())
        .merge(routes::depth::routes())
        .merge(routes::parts::routes())
        .merge(routes::three_d::routes())
        .merge(routes::pixal3d::routes())
        .merge(routes::stt_playground::routes())
        .merge(routes::vision_playground::routes())
        .merge(routes::gpu_logs::routes())
        .merge(routes::generations::routes())
        .merge(routes::grammars::routes())
        .merge(routes::prompt_templates::routes())
        .merge(routes::queue::routes())
        .merge(routes::push::routes())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&state.config.bind_address)
        .await
        .expect("failed to bind");

    // Start NATS review consumer if configured
    if state.config.nats_url.is_some() {
        let nats_state = state.clone();
        tokio::spawn(async move {
            nats::start_review_consumer(nats_state).await;
        });
    }

    // One-shot: bring pre-content-addressing gallery files onto `<sha256>.<ext>` names (§6.2).
    // Idempotent and best-effort — a boot where it cannot run leaves the legacy names working,
    // because the serve path never stopped accepting them.
    {
        let pool = state.db.clone();
        let roots = jobs::gallery_migration::Roots {
            songs_dir: (*state.songs_dir).clone(),
            avatars_dir: (*state.avatars_dir).clone(),
        };
        tokio::spawn(async move {
            jobs::gallery_migration::run(pool, roots).await;
        });
    }

    // Rolling retention: gpu_logs/samples telemetry + (opt-in) generation assets + the orphan
    // blob sweep that content addressing needs in place of refcounting.
    {
        let pool = state.db.clone();
        let roots = jobs::gpu_retention::Roots {
            samples_dir: (*state.samples_dir).clone(),
            songs_dir: (*state.songs_dir).clone(),
            avatars_dir: (*state.avatars_dir).clone(),
            queue_files: state.queue_files.clone(),
        };
        tokio::spawn(async move {
            jobs::gpu_retention::run(pool, roots).await;
        });
    }

    // Out-of-band GPU residency reconcile: poll each server's announce endpoint
    // (`GET /v1/gpu/status`) so a worker spawned by a direct API call (bypassing
    // koblem) still occupies budget at its service's peak. Best-effort.
    {
        let gpu = state.gpu.clone();
        tokio::spawn(async move {
            jobs::gpu_reconcile::run(gpu).await;
        });
    }

    // Retire content-addressed editor assets no saved project references any more. Uploads are
    // deduplicated by content, so this only ever collects genuinely abandoned bytes (a still that
    // was replaced, a project deleted) — hence the unhurried cadence and the 7-day grace.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            loop {
                tick.tick().await;
                routes::ltx_director::sweep_assets(&state, 7).await;
            }
        });
    }

    // ── The global GPU job queue ──────────────────────────────────────────────
    //
    // 🔴 ORDER IS LOAD-BEARING. Reattach has to fully settle before the first
    // dispatch: a re-attaching render still owns its card, and if the dispatcher
    // scans before that is on the ledger it admits a job onto occupied VRAM.
    // `bootstrap()` re-takes the guard for every reattached job synchronously
    // rather than waiting for a reconcile tick to observe it.
    {
        // Every kind, or the enqueue refuses it: a row nothing can dispatch is
        // worse than an error, and `Registry::missing()` is what says so out loud.
        let mut registry = queue::Registry::new();
        {
            use queue::adapters as a;
            let s = &state;
            registry.register(Arc::new(a::ltx25::Ltx25Adapter::new(s.clone())));
            registry.register(Arc::new(a::h3::H3Adapter::new(s.clone())));
            registry.register(Arc::new(a::music3::Music3Adapter::new(s.clone())));
            registry.register(Arc::new(a::upscale::UpscaleAdapter::new(s.clone())));
            registry.register(Arc::new(a::krea2::Krea2Adapter::new(s.clone())));
            registry.register(Arc::new(a::flux2::Flux2Adapter::new(s.clone())));
            registry.register(Arc::new(a::songgen::SonggenAdapter::new(s.clone())));
            registry.register(Arc::new(a::sa3::Sa3Adapter::new(s.clone())));
            registry.register(Arc::new(a::pixal3d_rig::Pixal3dRigAdapter::new(s.clone())));
            registry.register(Arc::new(a::pixal3d_motion::Pixal3dMotionAdapter::new(
                s.clone(),
            )));
            registry.register(Arc::new(a::ltx_relip::LtxRelipAdapter::new(s.clone())));
        }
        let missing = registry.missing();
        if !missing.is_empty() {
            warn!(
                kinds = ?missing.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
                "queue: no adapter registered; these kinds will be refused at enqueue"
            );
        }

        let health = Arc::new(queue::health::EngineHealth::new(queue_health_lanes));
        {
            let health = Arc::clone(&health);
            tokio::spawn(async move { health.run().await });
        }

        let events = queue::events::EventBus::new();
        let dispatcher = queue::Dispatcher::new(
            state.db.clone(),
            state.gpu.clone(),
            queue::clock::Clock::system(),
            Arc::new(registry),
            health,
            events.clone(),
        );
        state.queue.set(dispatcher.clone());

        // The two out-of-process sinks on the event spine. The SSE stream feeds
        // the UI from the same bus; this half is what reaches someone asleep.
        {
            let notifier = queue::notify::Notifier::new(
                state.db.clone(),
                events,
                state.config.nats_url.clone(),
            )
            .await;
            tokio::spawn(async move { notifier.run().await });
        }

        // 🔴 Single instance. The row claim is safe under FOR UPDATE SKIP LOCKED,
        // but the unload planner is not: two replicas would fight over drains, and
        // a drain issued by one and abandoned by the other leaves an engine
        // answering 503 to every render until the container restarts.
        if dispatcher.claim_singleton().await {
            if let Err(e) = dispatcher.bootstrap().await {
                warn!(error = %e, "queue: bootstrap failed; the dispatcher will not start");
            } else {
                tokio::spawn(async move { dispatcher.run().await });
            }
        } else {
            warn!(
                "queue: another koblem holds the dispatcher lock; this process serves the API only"
            );
        }
    }

    info!("koblem listening on {}", state.config.bind_address);
    axum::serve(listener, app).await.expect("server error");
}

/// Build the TTS-service registry from the configured engine URL. The
/// resulting Vec is what GpuLock unloads from on eviction. The tag must match
/// an entry in `ServiceKind::Tts.light_tags()`, or `reconcile_targets` and the
/// unload arm find nothing, return false, and the GPU never frees — silently.
/// Empty when the URL is unconfigured.
pub fn build_tts_services(breeze: &Option<String>) -> Vec<TtsService> {
    let mut out = Vec::new();
    if let Some(url) = breeze.as_deref().filter(|s| !s.is_empty()) {
        out.push(TtsService {
            tag: tts_breeze::TAG.to_string(),
            url: url.to_string(),
            unload_path: tts_breeze::UNLOAD_PATH.to_string(),
        });
    }
    out
}

/// Resolve a GPU-service URL: DB settings key overrides env-var fallback.
async fn resolve_gpu_url(
    db: &sqlx::PgPool,
    key: &str,
    fallback: &Option<String>,
) -> Option<String> {
    let from_db = repositories::settings::get_setting(db, key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .filter(|s| !s.is_empty());
    if from_db.is_some() {
        return from_db;
    }
    fallback.clone()
}

/// Try to initialize a release provider from DB settings.
async fn init_release_provider(db: &sqlx::PgPool) -> Option<Arc<dyn ReleaseProvider>> {
    let url = repositories::settings::get_setting(db, "release_provider_url")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.to_string()))?;

    let token = repositories::settings::get_setting(db, "release_provider_token")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    if url.is_empty() {
        return None;
    }

    info!(url = %url, "initializing release provider");
    Some(Arc::new(kob_providers::HttpReleaseProvider::new(
        url, token,
    )))
}

/// Try to initialize a library provider from DB settings.
async fn init_library_provider(db: &sqlx::PgPool) -> Option<Arc<dyn LibraryProvider>> {
    let url = repositories::settings::get_setting(db, "library_provider_url")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.to_string()))?;

    let token = repositories::settings::get_setting(db, "library_provider_token")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    if url.is_empty() {
        return None;
    }

    info!(url = %url, "initializing library provider");
    Some(Arc::new(kob_providers::HttpLibraryProvider::new(
        url, token,
    )))
}
