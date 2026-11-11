//! Wire-format lock for the koblibs types the routes serialise.
//!
//! Each route-local struct that was replaced by a `koblem_client::types` one is
//! pinned here: a FULLY-POPULATED instance serialised and compared against the
//! exact JSON the route emitted before the swap. A rename, a dropped field or a
//! changed tag representation in koblibs breaks these instead of a client.

use koblem_client::types::*;
use serde_json::{Value, json};

fn eq(actual: &impl serde::Serialize, expected: Value) {
    assert_eq!(serde_json::to_value(actual).unwrap(), expected);
}

// ── async video jobs (ltx-video) ──────────────────────────────────────────────

fn full_job_status() -> VideoJobStatus {
    VideoJobStatus {
        job_id: "kob-1".to_string(),
        status: VideoJobState::Rendering,
        created_at: time::OffsetDateTime::from_unix_timestamp(1_767_225_600).ok(),
        server_job_id: Some("srv-9".to_string()),
        resume_job_id: Some("srv-9".to_string()),
        resumable: true,
        segments: 3,
        scripts: vec!["a".to_string(), "b".to_string()],
        gate_wait_ms: 1200,
        render_ms: 34000,
        error: Some("boom".to_string()),
        result_url: Some("/api/v1/ltx-video/jobs/kob-1/media".to_string()),
        partials: vec![VideoJobPartial {
            segment_index: 0,
            url: "/api/v1/ltx-video/jobs/kob-1/segments/0".to_string(),
        }],
        job_is_partial: true,
        // The H3 progress block. `None` here, so the golden JSON below stays exactly the body an
        // LTX job produces — which is the point of every one of them being `skip_serializing_if`:
        // adding this block changed no existing wire response at all.
        total_eta_seconds: None,
        progress: None,
        current_step: None,
        total_steps: None,
        phase: None,
        preview_seq: None,
        generation_index: None,
        generations_done: None,
    }
}

#[test]
fn video_job_status_json() {
    eq(
        &full_job_status(),
        json!({
            "job_id": "kob-1",
            "status": "rendering",
            "created_at": "2026-01-01T00:00:00Z",
            "server_job_id": "srv-9",
            "resume_job_id": "srv-9",
            "resumable": true,
            "segments": 3,
            "scripts": ["a", "b"],
            "gate_wait_ms": 1200,
            "render_ms": 34000,
            "error": "boom",
            "result_url": "/api/v1/ltx-video/jobs/kob-1/media",
            "partials": [{ "segment_index": 0, "url": "/api/v1/ltx-video/jobs/kob-1/segments/0" }],
            "job_is_partial": true,
        }),
    );
}

/// `job_is_partial` is emitted ONLY when true, in both directions.
///
/// The absent form is what every job that is not a resume/retake or a failed concatenation sends,
/// and what every koblem predating the field sends for all of them — so a client must read absence
/// as `false` rather than as "unknown", and a normal job's body must not have grown a key.
#[test]
fn video_job_status_partial_flag_is_absent_when_false() {
    let full = full_job_status();
    let body = serde_json::to_value(VideoJobStatus {
        job_is_partial: false,
        ..full
    })
    .unwrap();
    assert!(body.get("job_is_partial").is_none());

    let older: VideoJobStatus = serde_json::from_value(json!({
        "job_id": "kob-1",
        "status": "completed",
        "result_url": "/api/v1/ltx-video/jobs/kob-1/media",
    }))
    .unwrap();
    assert!(!older.job_is_partial);
}

#[test]
fn video_job_state_wire_names() {
    for (state, wire) in [
        (VideoJobState::Queued, "queued"),
        (VideoJobState::Rendering, "rendering"),
        (VideoJobState::Completed, "completed"),
        (VideoJobState::Failed, "failed"),
        (VideoJobState::Cancelled, "cancelled"),
    ] {
        assert_eq!(serde_json::to_value(state).unwrap(), json!(wire));
        assert_eq!(state.as_str(), wire);
    }
}

#[test]
fn video_job_list_and_submit_json() {
    eq(
        &VideoJobListResponse {
            jobs: vec![full_job_status()],
        },
        json!({ "jobs": [serde_json::to_value(full_job_status()).unwrap()] }),
    );
    eq(
        &VideoJobSubmitResponse {
            job_id: "kob-1".to_string(),
            status: Some(VideoJobState::Queued),
        },
        json!({ "job_id": "kob-1", "status": "queued" }),
    );
}

/// The LtxJob/WanJob sidecars persist `state`/`partials` with these same names —
/// a rename would orphan every on-disk job across a restart. `created_at` is DERIVED
/// from the long-standing `created` epoch rather than stored, so the wire gained a start
/// time without the sidecar shape changing: `LtxJobStore::open` silently drops a sidecar
/// it cannot parse, and a new required field would have orphaned every in-flight render.
#[test]
fn video_job_sidecar_fields_round_trip() {
    let job: crate::ltx_jobs::LtxJob = serde_json::from_value(json!({
        "state": "rendering",
        "server_job_id": "srv-9",
        "segments": 2,
        "scripts": ["a"],
        "gate_wait_ms": 1,
        "render_ms": 2,
        "error": null,
        "content_type": null,
        "partials": [{ "segment_index": 1, "url": "/u" }],
        "created": 10,
        "updated": 11,
    }))
    .unwrap();
    assert_eq!(job.state, VideoJobState::Rendering);
    assert_eq!(job.partials[0].segment_index, 1);
    // Non-terminal + a server id is NOT resumable.
    assert!(!job.resumable(true));
    assert_eq!(
        job.created_at(),
        Some(time::OffsetDateTime::from_unix_timestamp(10).unwrap())
    );
}

/// The sidecar's `engine` tag is what a restart has to read back to know which service owns a
/// render, so the three values it can hold are pinned here alongside the two things they decide.
///
/// 🔴 `clip_ext` is not cosmetic. Both live engines are mp4 — the H3 shim delivers x265, and
/// `ltx25` has no choice because ComfyUI v0.32.0's only audio-carrying save node is `SaveVideo`,
/// whose format enum is `["auto","mp4"]` (`SaveWEBM` takes images and has no audio input at all).
/// mp4 bytes under a `.webm` name break the first `ffmpeg -c copy` concat that rebuilds a cut, at
/// a point with nothing to say about where the wrong name came from. The untagged `webm` reading
/// is the sd.cpp fork's: any sidecar the retired C++ `ltx-video` or LTX-2B engines left on disk.
#[test]
fn job_engine_tag_decides_the_container() {
    let job = |engine: Value| -> crate::ltx_jobs::LtxJob {
        serde_json::from_value(json!({
            "state": "completed",
            "server_job_id": "srv-9",
            "engine": engine,
            "segments": 1,
            "scripts": ["a"],
            "gate_wait_ms": 0,
            "render_ms": 0,
            "error": null,
            "content_type": null,
            "created": 10,
            "updated": 11,
        }))
        .unwrap()
    };
    // Absent and explicit-"ltx" are the same job: a sidecar a retired sd.cpp-fork engine left
    // behind.
    assert_eq!(job(Value::Null).engine_name(), "ltx");
    assert_eq!(job(Value::Null).clip_ext(), "webm");
    assert_eq!(job(json!("ltx")).clip_ext(), "webm");
    assert_eq!(job(json!("h3")).clip_ext(), "mp4");
    assert_eq!(job(json!("ltx25")).clip_ext(), "mp4");
    assert!(job(json!("ltx25")).is_comfy_ltx());
    assert!(!job(json!("ltx25")).is_h3());
    assert!(!job(json!("h3")).is_comfy_ltx());
}

/// Every LTX engine banks latents; H3 does not, so only H3's jobs are never resumable — and the
/// DEPLOYMENT has a veto on top of that.
///
/// 🔴 The failure this pins is not a 501. Offering "resume" for an engine with no bank means
/// re-rendering the whole timeline at full cost and reporting it to the user as a resume. `ltx25`
/// was on that side of the line until kobbler `6ca0b6f2` gave it a real per-segment bank, and the
/// assertion moved with the engine rather than the engine being special-cased.
///
/// 🔴 `bank_live` is the half the job record cannot know: 2.5's bank is a directory an operator
/// configures. This value rides the wire as `VideoJobStatus.resumable` and the `kob` CLI prints
/// "resume with --resume-job-id" off it, so an unqualified `true` hands the user a command that
/// cannot work.
#[test]
fn only_the_banked_engine_is_resumable() {
    let failed = |engine: Value| -> crate::ltx_jobs::LtxJob {
        serde_json::from_value(json!({
            "state": "failed",
            "server_job_id": "srv-9",
            "engine": engine,
            "segments": 2,
            "scripts": ["a"],
            "gate_wait_ms": 0,
            "render_ms": 0,
            "error": null,
            "content_type": null,
            "created": 10,
            "updated": 11,
        }))
        .unwrap()
    };
    assert!(failed(Value::Null).resumable(true));
    assert!(failed(json!("ltx")).resumable(true));
    assert!(!failed(json!("h3")).resumable(true));
    assert!(failed(json!("ltx25")).resumable(true));
    assert!(failed(Value::Null).banks_latents());
    assert!(failed(json!("ltx25")).banks_latents());
    assert!(!failed(json!("h3")).banks_latents());

    // A deployment with no bank directory. The engine implements resume fully and answers 501 to
    // it, so a job of ANY engine is unresumable there — including an untagged legacy one.
    assert!(!failed(json!("ltx25")).resumable(false));
    assert!(!failed(json!("ltx")).resumable(false));

    // A bank is only reachable once the engine has minted a job id, and only from a terminal
    // failure — the two conditions the engine tag is ANDed with, unchanged.
    let no_server_id: crate::ltx_jobs::LtxJob = serde_json::from_value(json!({
        "state": "failed",
        "server_job_id": null,
        "segments": 1,
        "scripts": ["a"],
        "gate_wait_ms": 0,
        "render_ms": 0,
        "error": null,
        "content_type": null,
        "created": 10,
        "updated": 11,
    }))
    .unwrap();
    assert!(!no_server_id.resumable(true));
}

/// A koblem predating `created_at` still parses in a client built after it — the field is
/// what a newer `kob` CLI reads, and a hard requirement would fail the whole poll response.
#[test]
fn video_job_status_without_created_at_parses() {
    let s: VideoJobStatus = serde_json::from_value(json!({
        "job_id": "kob-1",
        "status": "rendering",
    }))
    .unwrap();
    assert!(s.created_at.is_none());
}

// ── health ───────────────────────────────────────────────────────────────────

#[test]
fn health_response_json() {
    eq(
        &HealthResponse {
            status: "ok".to_string(),
            llama_server: true,
            vision: true,
            tts: true,
            stt: true,
            llama_model: Some("gemma".to_string()),
            vision_model: Some("embeddinggemma-2-Q8_0.gguf".to_string()),
            tts_model: Some("breeze-tts-2".to_string()),
            stt_model: Some("parakeet".to_string()),
            llama_audio_input: true,
        },
        json!({
            "status": "ok",
            "llama_server": true,
            "vision": true,
            "tts": true,
            "stt": true,
            "llama_model": "gemma",
            "vision_model": "embeddinggemma-2-Q8_0.gguf",
            "tts_model": "breeze-tts-2",
            "stt_model": "parakeet",
            "llama_audio_input": true,
        }),
    );
}

// ── auth ─────────────────────────────────────────────────────────────────────

#[test]
fn api_key_summary_json() {
    eq(
        &ApiKeySummary {
            id: uuid::Uuid::parse_str("019e80ed-e64a-7d41-9398-f61852e90784").unwrap(),
            name: "work-laptop".to_string(),
            key: Some("secret".to_string()),
            last_used_at: Some("2026-07-31T00:51:39.926533Z".to_string()),
            created_at: "2026-06-01T02:05:35.178999Z".to_string(),
            revoked: false,
        },
        json!({
            "id": "019e80ed-e64a-7d41-9398-f61852e90784",
            "name": "work-laptop",
            "key": "secret",
            "last_used_at": "2026-07-31T00:51:39.926533Z",
            "created_at": "2026-06-01T02:05:35.178999Z",
            "revoked": false,
        }),
    );
}

#[test]
fn auth_me_and_api_key_create_json() {
    eq(
        &AuthMeResponse {
            authenticated: true,
        },
        json!({ "authenticated": true }),
    );
    eq(
        &CreateApiKeyResponse {
            id: uuid::Uuid::parse_str("019e80ed-e64a-7d41-9398-f61852e90784").unwrap(),
            key: "plain".to_string(),
        },
        json!({ "id": "019e80ed-e64a-7d41-9398-f61852e90784", "key": "plain" }),
    );
    let req: CreateApiKeyRequest = serde_json::from_value(json!({ "name": "cli" })).unwrap();
    assert_eq!(req.name, "cli");
}

// ── GPU scheduler ────────────────────────────────────────────────────────────

#[test]
fn gpu_services_json() {
    eq(
        &GpuServicesResponse {
            services: vec![GpuServiceState {
                name: "llm".to_string(),
                enabled: true,
                busy: false,
            }],
        },
        json!({ "services": [{ "name": "llm", "enabled": true, "busy": false }] }),
    );
}

#[test]
fn gpu_cards_json() {
    eq(
        &GpuCardsResponse {
            cards: vec![GpuCard {
                uuid: "GPU-abc".to_string(),
                label: "RTX 3060 12GB".to_string(),
                total_mb: 12000,
                fast: false,
                enabled: true,
                sort_order: 0,
                updated_at: Some("2026-06-20T03:03:15.186854Z".to_string()),
            }],
        },
        json!({ "cards": [{
            "uuid": "GPU-abc",
            "label": "RTX 3060 12GB",
            "total_mb": 12000,
            "fast": false,
            "enabled": true,
            "sort_order": 0,
            "updated_at": "2026-06-20T03:03:15.186854Z",
        }]}),
    );
}

#[test]
fn gpu_engines_json() {
    eq(
        &GpuEnginesResponse {
            engines: vec![GpuEngineConfig {
                service: "songgen".to_string(),
                peak_mb: 9700,
                home_gpu: "GPU-abc".to_string(),
                candidate_gpus: vec!["GPU-abc".to_string()],
                evictable_by: vec!["flux".to_string()],
                relocatable: false,
                rules_json: "{}".to_string(),
                updated_at: Some("2026-07-29T05:15:14.753031Z".to_string()),
            }],
        },
        json!({ "engines": [{
            "service": "songgen",
            "peak_mb": 9700,
            "home_gpu": "GPU-abc",
            "candidate_gpus": ["GPU-abc"],
            "evictable_by": ["flux"],
            "relocatable": false,
            "rules_json": "{}",
            "updated_at": "2026-07-29T05:15:14.753031Z",
        }]}),
    );
}

#[test]
fn gpu_occupancy_json() {
    eq(
        &GpuOccupancyResponse {
            cards: vec![GpuCardOccupancy {
                uuid: "GPU-abc".to_string(),
                total_mb: 12000,
                used_mb: 9000,
                free_mb: 3000,
                measured_free_mb: Some(2500),
                reserved_mb: None,
                fast: false,
                enabled: true,
                residents: vec![GpuResident {
                    service: "llm".to_string(),
                    peak_mb: 9000,
                }],
            }],
        },
        json!({ "cards": [{
            "uuid": "GPU-abc",
            "total_mb": 12000,
            "used_mb": 9000,
            "free_mb": 3000,
            "measured_free_mb": 2500,
            "fast": false,
            "enabled": true,
            "residents": [{ "service": "llm", "peak_mb": 9000 }],
        }]}),
    );
}

// ── generation gallery ───────────────────────────────────────────────────────

fn full_generation_row() -> GenerationRow {
    GenerationRow {
        id: "6164eccb".to_string(),
        service: "krea2".to_string(),
        endpoint: "generate".to_string(),
        request_meta: json!({ "prompt": "a cat", "batch": 1 }),
        outputs: vec![GenerationOutput {
            filename: "7c28c0ff-0.png".to_string(),
            mime: "image/png".to_string(),
            size_bytes: 1795157,
            kind: "image".to_string(),
        }],
        title: Some("Cat".to_string()),
        created_at: "2026-07-29T05:11:53.54094Z".to_string(),
    }
}

#[test]
fn generation_row_json() {
    eq(
        &full_generation_row(),
        json!({
            "id": "6164eccb",
            "service": "krea2",
            "endpoint": "generate",
            "request_meta": { "prompt": "a cat", "batch": 1 },
            "outputs": [{
                "filename": "7c28c0ff-0.png",
                "mime": "image/png",
                "size_bytes": 1795157,
                "kind": "image",
            }],
            "title": "Cat",
            "created_at": "2026-07-29T05:11:53.54094Z",
        }),
    );
}

#[test]
fn generation_list_and_title_json() {
    eq(
        &GenerationListResponse {
            items: vec![full_generation_row()],
            total: 1,
        },
        json!({
            "items": [serde_json::to_value(full_generation_row()).unwrap()],
            "total": 1,
        }),
    );
    eq(
        &GenerationTitle {
            title: "Snowy cabin".to_string(),
        },
        json!({ "title": "Snowy cabin" }),
    );
    let body: GenerationTitle = serde_json::from_value(json!({ "title": "x" })).unwrap();
    assert_eq!(body.title, "x");
}

// ── music / songgen suggest ──────────────────────────────────────────────────

#[test]
fn music_suggest_json() {
    let req: MusicSuggestRequest = serde_json::from_value(json!({ "brief": "sad robot" })).unwrap();
    assert_eq!(req.brief, "sad robot");
    assert!(!req.caption_only);
    let req: MusicSuggestRequest =
        serde_json::from_value(json!({ "brief": "b", "caption_only": true })).unwrap();
    assert!(req.caption_only);

    eq(
        &MusicSuggestion {
            caption: "warm lo-fi".to_string(),
            lyrics: Some("la la".to_string()),
        },
        json!({ "caption": "warm lo-fi", "lyrics": "la la" }),
    );
    // The route relies on `lyrics` disappearing (not nulling) for caption-only.
    eq(
        &MusicSuggestion {
            caption: "warm lo-fi".to_string(),
            lyrics: None,
        },
        json!({ "caption": "warm lo-fi" }),
    );
}

#[test]
fn songgen_suggest_and_separate_json() {
    let req: SonggenSuggestRequest = serde_json::from_value(json!({ "brief": "b" })).unwrap();
    assert!(!req.description_only);
    let req: SonggenSuggestRequest =
        serde_json::from_value(json!({ "brief": "b", "description_only": true })).unwrap();
    assert!(req.description_only);

    let stem = |name: &str| MusicTrack {
        filename: name.to_string(),
        url: format!("/api/v1/songgen/file/{name}"),
        seed: -1,
        content_type: "audio/wav".to_string(),
        bytes: 42,
    };
    eq(
        &SonggenSeparateResponse {
            vocal: stem("v.wav"),
            bgm: stem("b.wav"),
            duration_ms: 1000,
            gate_wait_ms: 5,
        },
        json!({
            "vocal": { "filename": "v.wav", "url": "/api/v1/songgen/file/v.wav", "seed": -1,
                       "content_type": "audio/wav", "bytes": 42 },
            "bgm": { "filename": "b.wav", "url": "/api/v1/songgen/file/b.wav", "seed": -1,
                     "content_type": "audio/wav", "bytes": 42 },
            "duration_ms": 1000,
            "gate_wait_ms": 5,
        }),
    );
}

// ── images / krea2 ───────────────────────────────────────────────────────────

#[test]
fn image_props_json_flux2() {
    eq(
        &ImageProps {
            healthy: true,
            samplers: vec!["euler".to_string()],
            schedulers: vec!["karras".to_string()],
            limits: json!({ "max_width": 4096 }),
            supported_modes: vec!["base".to_string(), "edit".to_string()],
            variants: json!({ "base": { "default_steps": 50 } }),
            default_width: Some(512),
            default_height: Some(512),
            max_dim: Some(2048),
            min_dim: Some(16),
            ..Default::default()
        },
        json!({
            "healthy": true,
            "samplers": ["euler"],
            "schedulers": ["karras"],
            "limits": { "max_width": 4096 },
            "supported_modes": ["base", "edit"],
            "variants": { "base": { "default_steps": 50 } },
            "default_width": 512,
            "default_height": 512,
            "max_dim": 2048,
            "min_dim": 16,
        }),
    );
}

#[test]
fn image_props_json_krea2() {
    eq(
        &ImageProps {
            healthy: true,
            configured: Some(true),
            samplers: vec!["euler".to_string()],
            schedulers: vec!["discrete".to_string()],
            limits: json!({ "max_batch_count": 8 }),
            defaults: Some(json!({ "default_steps": 8 })),
            variants: json!({ "t2i": { "default_steps": 8 } }),
            edit_available: Some(true),
            edit_lora: Some("krea2-identity-edit-v1_2.gguf".to_string()),
            max_reference_images: Some(2),
            default_width: Some(1024),
            default_height: Some(1024),
            max_dim: Some(2048),
            min_dim: Some(16),
            recommended_min_dim: Some(1024),
            ..Default::default()
        },
        json!({
            "healthy": true,
            "configured": true,
            "samplers": ["euler"],
            "schedulers": ["discrete"],
            "limits": { "max_batch_count": 8 },
            "defaults": { "default_steps": 8 },
            "variants": { "t2i": { "default_steps": 8 } },
            "edit_available": true,
            "edit_lora": "krea2-identity-edit-v1_2.gguf",
            "max_reference_images": 2,
            "default_width": 1024,
            "default_height": 1024,
            "max_dim": 2048,
            "min_dim": 16,
            "recommended_min_dim": 1024,
        }),
    );
}

/// The `!is_configured()` bodies both lean on `..Default::default()`, so a new
/// koblibs field lands in them without either route being touched. `samplers` /
/// `schedulers` / `variants` / `limits` carry no `skip_serializing_if` and must
/// stay present-but-empty; everything optional must stay absent — the UI reads
/// `configured` to tell "not deployed" apart from "deployed but unreachable".
#[test]
fn image_props_json_unconfigured() {
    eq(
        &crate::routes::images::unconfigured_props(),
        json!({
            "healthy": false,
            "samplers": [],
            "schedulers": [],
            "limits": {},
            "variants": {
                "base": { "default_steps": 50, "default_cfg": 4.0, "default_sampler": null },
                "edit": { "default_steps": 4, "default_cfg": 1.0, "default_sampler": "euler" },
            },
        }),
    );

    // `restage` runs the SAME adapter as `edit` and therefore carries identical
    // prefills — the two differ only in what the engine does with the reference's
    // geometry, which is not a sampling parameter and so appears nowhere here.
    let krea2_with_ref = json!({
        "default_steps": 10,
        "default_cfg": 1.0,
        "default_grounding_px": 768,
        "min_grounding_px": 384,
        "max_grounding_px": 1024,
    });
    let krea2_variants = json!({
        "t2i": { "default_steps": 8, "default_cfg": 1.0 },
        "edit": krea2_with_ref,
        "restage": krea2_with_ref,
    });
    eq(
        &crate::routes::krea2::unconfigured_props(),
        json!({
            "healthy": false,
            "configured": false,
            "samplers": [],
            "schedulers": [],
            "limits": {},
            "defaults": {
                "default_steps": 8,
                "default_cfg": 1.0,
                "default_scheduler": "discrete",
                "variants": krea2_variants,
            },
            "variants": krea2_variants,
            "edit_available": false,
            "max_reference_images": 2,
        }),
    );
}

/// Both image engines deliver renders as gallery URLs. `generation_id` is a bare
/// `String`, not an option: a route that could not save has nowhere to serve the
/// bytes from, so it fails instead of handing back a dangling id.
#[test]
fn image_generate_json() {
    eq(
        &ImageGenerateResponse {
            images: vec![
                GeneratedImage {
                    url: "/api/v1/generations/gid/file/0".to_string(),
                    seed: 7,
                    cutout_url: Some("/api/v1/generations/gid/file/1".to_string()),
                },
                // No chained cutout for this one → the field is absent, not null.
                GeneratedImage {
                    url: "/api/v1/generations/gid/file/2".to_string(),
                    seed: 8,
                    cutout_url: None,
                },
            ],
            model_used: "edit".to_string(),
            width: 512,
            height: 512,
            steps: 4,
            cfg: 1.0,
            render_ms: 900,
            gate_wait_ms: 10,
            background_removal_error: Some("nope".to_string()),
            background_removal_backend: Some("rmbg".to_string()),
            background_removal_seconds: Some(0.5),
            generation_id: "gid".to_string(),
        },
        json!({
            "images": [
                {
                    "url": "/api/v1/generations/gid/file/0",
                    "seed": 7,
                    "cutout_url": "/api/v1/generations/gid/file/1",
                },
                { "url": "/api/v1/generations/gid/file/2", "seed": 8 },
            ],
            "model_used": "edit",
            "width": 512,
            "height": 512,
            "steps": 4,
            "cfg": 1.0,
            "render_ms": 900,
            "gate_wait_ms": 10,
            "background_removal_error": "nope",
            "background_removal_backend": "rmbg",
            "background_removal_seconds": 0.5,
            "generation_id": "gid",
        }),
    );
}

#[test]
fn krea2_generate_json() {
    let req: Krea2GenerateRequest = serde_json::from_value(json!({
        "prompt": "a cat",
        "mode": "edit",
        "reference_images": ["AAA"],
        "grounding_px": 768,
        "negative_prompt": "blur",
        "width": 1024, "height": 1024, "steps": 8, "cfg": 1.0,
        "sampler": "euler", "scheduler": "discrete", "seed": 7, "batch": 2,
        "vae_tiling": false,
        "remove_background": { "enabled": true, "bg_mode": "alpha" },
    }))
    .unwrap();
    assert_eq!(req.mode.as_deref(), Some("edit"));
    assert_eq!(req.grounding_px, Some(768));
    assert!(req.remove_background.as_ref().unwrap().enabled);
    assert!(!req.vae_tiling.unwrap());

    eq(
        &Krea2GenerateResponse {
            images: vec![GeneratedImage {
                url: "/api/v1/generations/gid/file/0".to_string(),
                seed: 7,
                cutout_url: Some("/api/v1/generations/gid/file/1".to_string()),
            }],
            mode_used: "edit".to_string(),
            width: 1024,
            height: 1024,
            steps: 8,
            cfg: 1.0,
            render_ms: 900,
            gate_wait_ms: 10,
            background_removal_error: Some("nope".to_string()),
            background_removal_backend: Some("rmbg".to_string()),
            background_removal_seconds: Some(0.5),
            generation_id: "gid".to_string(),
        },
        json!({
            "images": [{
                "url": "/api/v1/generations/gid/file/0",
                "seed": 7,
                "cutout_url": "/api/v1/generations/gid/file/1",
            }],
            "mode_used": "edit",
            "width": 1024,
            "height": 1024,
            "steps": 8,
            "cfg": 1.0,
            "render_ms": 900,
            "gate_wait_ms": 10,
            "background_removal_error": "nope",
            "background_removal_backend": "rmbg",
            "background_removal_seconds": 0.5,
            "generation_id": "gid",
        }),
    );

    let req: Krea2SuggestPromptRequest = serde_json::from_value(json!({
        "brief": "b", "refine_source": "s", "refine_instruction": "i",
    }))
    .unwrap();
    assert_eq!(req.refine_source.as_deref(), Some("s"));
}

// ── sa3 ──────────────────────────────────────────────────────────────────────

#[test]
fn sa3_generate_json() {
    // `save` defaults to TRUE on the wire — a derived Default would silently
    // disable the gallery write.
    let req: Sa3GenerateRequest = serde_json::from_value(json!({ "prompt": "rain" })).unwrap();
    assert!(req.save);
    assert!(Sa3GenerateRequest::default().save);
    let req: Sa3GenerateRequest = serde_json::from_value(json!({
        "prompt": "rain", "seconds": 12.0, "seed": 3, "sampler": "euler",
        "steps": 8, "model": "q6", "save": false,
    }))
    .unwrap();
    assert!(!req.save);
    assert_eq!(req.model.as_deref(), Some("q6"));

    eq(
        &Sa3GenerateResponse {
            tracks: vec![MusicTrack {
                filename: "a.wav".to_string(),
                url: "/api/v1/sa3/file/a.wav".to_string(),
                seed: 3,
                content_type: "audio/wav".to_string(),
                bytes: 10,
            }],
            seconds_used: Some(12.0),
            duration_ms: Some(500),
            gate_wait_ms: Some(5),
        },
        json!({
            "tracks": [{ "filename": "a.wav", "url": "/api/v1/sa3/file/a.wav", "seed": 3,
                         "content_type": "audio/wav", "bytes": 10 }],
            "seconds_used": 12.0,
            "duration_ms": 500,
            "gate_wait_ms": 5,
        }),
    );

    eq(
        &Sa3Props {
            healthy: true,
            version: Some("72bf558 (2026-07-14)".to_string()),
        },
        json!({ "healthy": true, "version": "72bf558 (2026-07-14)" }),
    );
    // `routes::sa3::props` serves this verbatim when no engine URL is set — the
    // UI disables the tab on `healthy: false`, so the key must not vanish.
    eq(&Sa3Props::default(), json!({ "healthy": false }));
}

// ── TTS ──────────────────────────────────────────────────────────────────────

#[test]
fn tts_voices_json() {
    eq(
        &TtsVoicesResponse {
            voices: vec![TtsVoice {
                id: "gaiman".to_string(),
                name: Some("gaiman".to_string()),
                builtin: false,
                frames: Some(150),
                codebooks: Some(16),
                has_ref_text: true,
                has_sample: true,
            }],
            model_id: Some("breeze-tts-2".to_string()),
        },
        json!({
            "voices": [{
                "id": "gaiman", "name": "gaiman", "builtin": false, "frames": 150,
                "codebooks": 16, "has_ref_text": true, "has_sample": true,
            }],
            "model_id": "breeze-tts-2",
        }),
    );
}

#[test]
fn tts_generate_request_json() {
    let body = json!({
        "input": "hello",
        "voice": "gaiman",
        "instructions": "a gruff sea captain",
        "speaker": "S0",
        "response_format": "pcm",
        "bitrate_kbps": 64,
        "temperature": 0.9,
        "top_p": 1.0,
        "top_k": 50,
        "repetition_penalty": 1.1,
        "seed": 11,
        "max_new_frames": 750,
        "stream_format": "sse",
        "stream_batch_size": 30,
        "stream_first_batch_size": 1,
        "long": true,
        "chunk_words": 120,
        "buffer": 1,
        "align": true,
        "align_stream": "partial",
        "save_sample": true,
    });
    let req: TtsGenerateRequest = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(req.input, "hello");
    assert_eq!(req.align_stream.as_deref(), Some("partial"));
    assert_eq!(req.long, Some(true));
    // Fully-populated round-trip: this is what lands in the gallery's
    // `request_meta`, so every knob name is load-bearing.
    eq(&req, body);

    // The realistic shape — a bare `input` — is what actually gets persisted into
    // `gpu_logs.request_meta` and `generations.request_meta`. Every optional
    // carries `skip_serializing_if`, so the record must stay a single key: losing
    // one of those attributes writes a wall of nulls into every row.
    let req: TtsGenerateRequest = serde_json::from_value(json!({ "input": "hello" })).unwrap();
    assert!(req.voice.is_none());
    assert!(
        req.long.is_none(),
        "unset long defers to the server default"
    );
    eq(&req, json!({ "input": "hello" }));
    eq(&TtsGenerateRequest::default(), json!({ "input": "" }));
}

/// `speed` is a real render parameter now, but it must stay OFF the wire unless
/// the caller asked for one — an unconditional `speed: 1.0` is one engine
/// validation change away from 400-ing every request. `language` has no engine
/// support at all and must never appear.
#[test]
fn tts_generate_request_sends_speed_only_when_set_and_never_language() {
    let bare = serde_json::to_value(TtsGenerateRequest {
        input: "hi".to_string(),
        ..Default::default()
    })
    .unwrap();
    let obj = bare.as_object().unwrap();
    assert!(!obj.contains_key("speed"));
    assert!(!obj.contains_key("language"));

    let with_speed = serde_json::to_value(TtsGenerateRequest {
        input: "hi".to_string(),
        speed: Some(1.25),
        ..Default::default()
    })
    .unwrap();
    let obj = with_speed.as_object().unwrap();
    assert_eq!(obj.get("speed").and_then(|v| v.as_f64()), Some(1.25));
    assert!(!obj.contains_key("language"));
}

// ── matting (already koblibs; pinned so the /remove reply can't drift) ───────

#[test]
fn matting_remove_json() {
    eq(
        &MattingRemoveResponse {
            count: 1,
            results: vec![MattingResultImage {
                url: "/api/v1/generations/gid/file/0".to_string(),
            }],
            backend: Some("rmbg".to_string()),
            elapsed_seconds: Some(0.25),
            generation_id: "gid".to_string(),
        },
        json!({
            "count": 1,
            "results": [{ "url": "/api/v1/generations/gid/file/0" }],
            "backend": "rmbg",
            "elapsed_seconds": 0.25,
            "generation_id": "gid",
        }),
    );
}

/// The cutout urls are positional against the gallery row: `results[i]` is
/// `outputs[i]`. A drift here silently hands every client the wrong image.
#[test]
fn matting_result_urls_index_the_generation_row() {
    use crate::routes::tier2_helpers::generation_file_url;
    assert_eq!(
        generation_file_url("gid", 0),
        "/api/v1/generations/gid/file/0"
    );
    assert_eq!(
        generation_file_url("gid", 3),
        "/api/v1/generations/gid/file/3"
    );
}

// ── depth (route-local; pinned so the /depth reply can't drift) ─────────────

/// The min/max are the ONLY way to denormalise the 16-bit PNGs, so dropping one
/// turns the reply into a picture rather than data. Pinned fully populated.
#[test]
fn depth_response_json() {
    use crate::routes::depth::{DepthMaps, DepthResultOut, DepthRouteResponse};
    eq(
        &DepthRouteResponse {
            count: 1,
            results: vec![DepthResultOut {
                width: 512,
                height: 512,
                maps: DepthMaps::Stored {
                    depth_url: "/api/v1/generations/gid/file/0".to_string(),
                    confidence_url: Some("/api/v1/generations/gid/file/1".to_string()),
                },
                depth_min: 0.7608,
                depth_max: 2.6982,
                confidence_min: Some(0.927),
                confidence_max: Some(4.5383),
            }],
            model: Some("depth-anything-3".to_string()),
            depth_polarity: Some("distance".to_string()),
            backend: Some("vulkan".to_string()),
            elapsed_seconds: Some(0.381),
            generation_id: Some("gid".to_string()),
        },
        json!({
            "count": 1,
            "results": [{
                "width": 512,
                "height": 512,
                "depth_url": "/api/v1/generations/gid/file/0",
                "confidence_url": "/api/v1/generations/gid/file/1",
                "depth_min": 0.7608,
                "depth_max": 2.6982,
                "confidence_min": 0.927,
                "confidence_max": 4.5383,
            }],
            "model": "depth-anything-3",
            "depth_polarity": "distance",
            "backend": "vulkan",
            "elapsed_seconds": 0.381,
            "generation_id": "gid",
        }),
    );
}

/// The DEFAULT model, Depth-Anything V2, has no confidence head. Its absence is
/// the shape — no url, no bounds, no fabricated constant — and `depth_polarity`
/// flips with it, because V2 emits disparity (larger is NEARER) where DA3 emits
/// distance.
#[test]
fn depth_response_without_confidence_json() {
    use crate::routes::depth::{DepthMaps, DepthResultOut, DepthRouteResponse};
    eq(
        &DepthRouteResponse {
            count: 1,
            results: vec![DepthResultOut {
                width: 512,
                height: 512,
                maps: DepthMaps::Stored {
                    depth_url: "/api/v1/generations/gid/file/0".to_string(),
                    confidence_url: None,
                },
                depth_min: 0.0,
                depth_max: 1.0,
                confidence_min: None,
                confidence_max: None,
            }],
            model: Some("depth-anything-v2".to_string()),
            depth_polarity: Some("disparity".to_string()),
            backend: Some("vulkan".to_string()),
            elapsed_seconds: Some(0.12),
            generation_id: Some("gid".to_string()),
        },
        json!({
            "count": 1,
            "results": [{
                "width": 512,
                "height": 512,
                "depth_url": "/api/v1/generations/gid/file/0",
                "depth_min": 0.0,
                "depth_max": 1.0,
            }],
            "model": "depth-anything-v2",
            "depth_polarity": "disparity",
            "backend": "vulkan",
            "elapsed_seconds": 0.12,
            "generation_id": "gid",
        }),
    );
}

/// `?inline=true`: the same bounds, the pixels instead of a place to find them,
/// and no `generation_id` — nothing was filed, so there is nothing to name.
#[test]
fn depth_inline_response_json() {
    use crate::routes::depth::{DepthMaps, DepthResultOut, DepthRouteResponse};
    eq(
        &DepthRouteResponse {
            count: 1,
            results: vec![DepthResultOut {
                width: 512,
                height: 512,
                maps: DepthMaps::Inline {
                    depth_png_base64: "iVBORw0=".to_string(),
                    confidence_png_base64: Some("iVBORw1=".to_string()),
                },
                depth_min: 0.7608,
                depth_max: 2.6982,
                confidence_min: Some(0.927),
                confidence_max: Some(4.5383),
            }],
            model: Some("depth-anything-3".to_string()),
            depth_polarity: Some("distance".to_string()),
            backend: Some("vulkan".to_string()),
            elapsed_seconds: Some(0.381),
            generation_id: None,
        },
        json!({
            "count": 1,
            "results": [{
                "width": 512,
                "height": 512,
                "depth_png_base64": "iVBORw0=",
                "confidence_png_base64": "iVBORw1=",
                "depth_min": 0.7608,
                "depth_max": 2.6982,
                "confidence_min": 0.927,
                "confidence_max": 4.5383,
            }],
            "model": "depth-anything-3",
            "depth_polarity": "distance",
            "backend": "vulkan",
            "elapsed_seconds": 0.381,
        }),
    );
}

/// The maps are filed consecutively in the order they are pushed — depth, then
/// confidence only when the model produced one. A drift here hands a client the
/// wrong image's map.
#[test]
fn depth_result_urls_index_the_generation_row_in_push_order() {
    use crate::routes::tier2_helpers::generation_file_url;
    for i in 0..6usize {
        assert_eq!(
            generation_file_url("gid", i),
            format!("/api/v1/generations/gid/file/{i}")
        );
    }
}

/// Pins the upstream `/depth` contract this client deserialises.
#[test]
fn depth_upstream_response_parses() {
    let raw = json!({
        "count": 1,
        "results": [{
            "width": 512, "height": 512,
            "depth_png_base64": "AA==", "confidence_png_base64": "AQ==",
            "depth_min": 0.7608, "depth_max": 2.6982,
            "confidence_min": 0.927, "confidence_max": 4.5383,
        }],
        "elapsed_seconds": 0.381,
        "backend": "vulkan",
    });
    let parsed: crate::depth::DepthResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(parsed.count, 1);
    assert_eq!(parsed.backend.as_deref(), Some("vulkan"));
    let r = &parsed.results[0];
    assert_eq!((r.width, r.height), (512, 512));
    assert_eq!(r.confidence_png_base64.as_deref(), Some("AQ=="));
    assert!((r.depth_max - 2.6982).abs() < f64::EPSILON);
}

// ── character sheet (route-local; paperworld deserialises it by hand) ────────

/// paperworld reads this reply with its own structs, so every name here is a contract with a
/// separate repo that nothing in this workspace type-checks. Pinned fully populated: a rename or a
/// re-tagged enum breaks the summon, not the build.
#[test]
fn character_sheet_json() {
    use crate::routes::character_sheet::{
        CharacterSheet, Death, Emits, Kind, Locomotion, Motion, Part, Skill,
    };
    let expected = json!({
        "name": "Heron",
        "subject": "a grey heron mid-stride in shallow water",
        "sound_desc": "a harsh croaking squawk",
        "height_m": 1.0,
        "speed_mps": 11.0,
        "mass_kg": 1.5,
        "hp": 8,
        "strength": 3,
        "kind": "fighter",
        "locomotion": ["walk", "fly"],
        "roaming": true,
        "skills": ["shoot"],
        "parts": [{ "name": "wings", "motion": "flap", "count": 2 }],
        "death": "ragdoll",
        "explosion_radius_m": 0.0,
        "mountable": false,
        "buoyant": true,
        "front_back_symmetric": false,
        "emits": "none",
    });
    let sheet = CharacterSheet {
        name: "Heron".to_string(),
        subject: "a grey heron mid-stride in shallow water".to_string(),
        sound_desc: "a harsh croaking squawk".to_string(),
        height_m: 1.0,
        speed_mps: 11.0,
        mass_kg: 1.5,
        hp: 8,
        strength: 3,
        kind: Kind::Fighter,
        locomotion: vec![Locomotion::Walk, Locomotion::Fly],
        roaming: true,
        skills: vec![Skill::Shoot],
        parts: vec![Part {
            name: "wings".to_string(),
            motion: Motion::Flap,
            count: 2,
        }],
        death: Death::Ragdoll,
        explosion_radius_m: 0.0,
        mountable: false,
        buoyant: true,
        front_back_symmetric: false,
        emits: Emits::None,
    };
    eq(&sheet, expected.clone());
    // The same shape is how the model's reply is read, so the wire pin covers both directions.
    assert_eq!(
        serde_json::from_value::<CharacterSheet>(expected).unwrap(),
        sheet
    );
}

/// The enum wire names the game switches on. Spelled out rather than derived, so a variant renamed
/// on either side of the wire has to be renamed here too.
#[test]
fn character_sheet_enum_wire_names() {
    use crate::routes::character_sheet::{Death, Emits, Kind, Locomotion, Motion};
    let pairs: Vec<(Value, &str)> = vec![
        (json!(Kind::Static), "static"),
        (json!(Kind::Fighter), "fighter"),
        (json!(Kind::Lover), "lover"),
        (json!(Kind::Medic), "medic"),
        (json!(Kind::Weapon), "weapon"),
        (json!(Kind::Mount), "mount"),
        (json!(Locomotion::Walk), "walk"),
        (json!(Locomotion::Fly), "fly"),
        (json!(Locomotion::Drive), "drive"),
        (json!(Locomotion::Swim), "swim"),
        (json!(Locomotion::Hop), "hop"),
        (json!(Locomotion::Slither), "slither"),
        (json!(Locomotion::Static), "static"),
        (json!(Motion::Spin), "spin"),
        (json!(Motion::Swing), "swing"),
        (json!(Motion::Flap), "flap"),
        (json!(Motion::Bob), "bob"),
        (json!(Motion::None), "none"),
        (json!(Death::None), "none"),
        (json!(Death::Explode), "explode"),
        (json!(Death::Shatter), "shatter"),
        (json!(Death::Deflate), "deflate"),
        (json!(Death::Ragdoll), "ragdoll"),
        (json!(Emits::None), "none"),
        (json!(Emits::Smoke), "smoke"),
        (json!(Emits::Sparks), "sparks"),
        (json!(Emits::Bubbles), "bubbles"),
    ];
    for (value, wire) in pairs {
        assert_eq!(value, json!(wire));
    }
}

// ── summon stream (route-local; paperworld reads the events by name) ─────────

/// The event names paperworld attaches its listeners to. Nothing in this
/// workspace type-checks them, so a rename here is a summon that streams into a
/// client that never hears it.
#[test]
fn summon_stage_event_names() {
    use crate::routes::summon::Stage;
    for (stage, name) in [
        (Stage::Sheet, "sheet"),
        (Stage::Image, "image"),
        (Stage::Sound, "sound"),
        (Stage::Depth, "depth"),
        (Stage::Parts, "parts"),
        (Stage::Ready, "ready"),
    ] {
        assert_eq!(stage.name(), name);
        assert_eq!(json!(stage), json!(name));
    }
}

/// `image` — `/oneshot`'s `image_b64` under the same name, so the caller's decode
/// path is the one it already has.
#[test]
fn summon_image_stage_json() {
    use crate::routes::summon::ImageStage;
    eq(
        &ImageStage {
            image_b64: "iVBORw0=",
            width: 1024,
            height: 1024,
            gate_wait_ms: 4200,
            render_ms: 11_000,
        },
        json!({
            "image_b64": "iVBORw0=",
            "width": 1024,
            "height": 1024,
            "gate_wait_ms": 4200,
            "render_ms": 11_000,
        }),
    );
}

/// `sound` — a null `sound_b64` on this event is a thing that makes no noise. A
/// failed render is an `error` event instead, so the key must survive
/// serialisation rather than being skipped.
#[test]
fn summon_sound_stage_json() {
    use crate::routes::summon::SoundStage;
    eq(
        &SoundStage {
            sound_b64: Some("UklGRg=="),
            gate_wait_ms: 900,
            render_ms: 3100,
        },
        json!({ "sound_b64": "UklGRg==", "gate_wait_ms": 900, "render_ms": 3100 }),
    );
    eq(
        &SoundStage {
            sound_b64: None,
            gate_wait_ms: 0,
            render_ms: 0,
        },
        json!({ "sound_b64": null, "gate_wait_ms": 0, "render_ms": 0 }),
    );
}

/// `depth` — the `?inline=true` result shape for the one sprite, flattened, plus
/// how long it took. The bounds are the only way to denormalise the 16-bit PNGs,
/// so they are pinned alongside the maps.
#[test]
fn summon_depth_stage_json() {
    use crate::routes::depth::{DepthMaps, DepthResultOut};
    use crate::routes::summon::DepthStage;
    eq(
        &DepthStage {
            maps: DepthResultOut {
                width: 1024,
                height: 1024,
                maps: DepthMaps::Inline {
                    depth_png_base64: "iVBORw0=".to_string(),
                    confidence_png_base64: Some("iVBORw1=".to_string()),
                },
                depth_min: 0.7608,
                depth_max: 2.6982,
                confidence_min: Some(0.927),
                confidence_max: Some(4.5383),
            },
            render_ms: 96,
        },
        json!({
            "width": 1024,
            "height": 1024,
            "depth_png_base64": "iVBORw0=",
            "confidence_png_base64": "iVBORw1=",
            "depth_min": 0.7608,
            "depth_max": 2.6982,
            "confidence_min": 0.927,
            "confidence_max": 4.5383,
            "render_ms": 96,
        }),
    );
}

/// `parts` — the masks for the sheet's moving parts, over the same pixels the
/// `image` event carried. `name` is the SHEET's own part name, not the singular
/// phrase that found it, so the consumer pairs masks onto parts by name.
#[test]
fn summon_parts_stage_json() {
    use crate::routes::parts::{PartMaskRef, PartOut};
    use crate::routes::summon::PartsStage;
    eq(
        &PartsStage {
            width: 1024,
            height: 1024,
            parts: vec![
                PartOut {
                    name: "wings".to_string(),
                    score: 0.6,
                    bbox: [10.0, 20.0, 110.0, 220.0],
                    area: 4096,
                    mask: PartMaskRef::Inline {
                        mask_png_base64: "iVBORw0=".to_string(),
                    },
                },
                PartOut {
                    name: "legs".to_string(),
                    score: 0.79,
                    bbox: [1.0, 2.0, 3.0, 4.0],
                    area: 512,
                    mask: PartMaskRef::Inline {
                        mask_png_base64: "iVBORw1=".to_string(),
                    },
                },
            ],
            render_ms: 6300,
        },
        json!({
            "width": 1024,
            "height": 1024,
            "parts": [
                {
                    "name": "wings",
                    "score": 0.6,
                    "box": [10.0, 20.0, 110.0, 220.0],
                    "area": 4096,
                    "mask_png_base64": "iVBORw0=",
                },
                {
                    "name": "legs",
                    "score": 0.79,
                    "box": [1.0, 2.0, 3.0, 4.0],
                    "area": 512,
                    "mask_png_base64": "iVBORw1=",
                },
            ],
            "render_ms": 6300,
        }),
    );
}

// ── parts (route-local; pinned so the /parts reply can't drift) ─────────────

/// A mask PNG is only usable against the pixels it indexes and the phrase that
/// found it, so the dimensions, the phrases and each box travel with the urls.
#[test]
fn parts_response_json() {
    use crate::routes::parts::{PartMaskRef, PartOut, PartsRouteResponse};
    eq(
        &PartsRouteResponse {
            width: 1024,
            height: 1024,
            prompts: vec!["arm".to_string(), "leg".to_string()],
            count: 2,
            parts: vec![
                PartOut {
                    name: "arm".to_string(),
                    score: 0.6,
                    bbox: [10.0, 20.0, 110.0, 220.0],
                    area: 4096,
                    mask: PartMaskRef::Stored {
                        mask_url: "/api/v1/generations/gid/file/0".to_string(),
                    },
                },
                PartOut {
                    name: "leg".to_string(),
                    score: 0.79,
                    bbox: [1.0, 2.0, 3.0, 4.0],
                    area: 512,
                    mask: PartMaskRef::Stored {
                        mask_url: "/api/v1/generations/gid/file/1".to_string(),
                    },
                },
            ],
            model: Some("sam3".to_string()),
            encode_seconds: Some(4.2),
            decode_seconds: vec![1.01, 0.98],
            elapsed_seconds: Some(6.3),
            backend: Some("vulkan".to_string()),
            generation_id: Some("gid".to_string()),
        },
        json!({
            "width": 1024,
            "height": 1024,
            "prompts": ["arm", "leg"],
            "count": 2,
            "parts": [
                {
                    "name": "arm",
                    "score": 0.6,
                    "box": [10.0, 20.0, 110.0, 220.0],
                    "area": 4096,
                    "mask_url": "/api/v1/generations/gid/file/0",
                },
                {
                    "name": "leg",
                    "score": 0.79,
                    "box": [1.0, 2.0, 3.0, 4.0],
                    "area": 512,
                    "mask_url": "/api/v1/generations/gid/file/1",
                },
            ],
            "model": "sam3",
            "encode_seconds": 4.2,
            "decode_seconds": [1.01, 0.98],
            "elapsed_seconds": 6.3,
            "backend": "vulkan",
            "generation_id": "gid",
        }),
    );
}

/// `?inline=true` — the pixels themselves, and no evidence they were ever filed.
#[test]
fn parts_inline_response_json() {
    use crate::routes::parts::{PartMaskRef, PartOut, PartsRouteResponse};
    eq(
        &PartsRouteResponse {
            width: 512,
            height: 512,
            prompts: vec!["arm".to_string()],
            count: 1,
            parts: vec![PartOut {
                name: "arm".to_string(),
                score: 0.41,
                bbox: [0.0, 0.0, 8.0, 8.0],
                area: 64,
                mask: PartMaskRef::Inline {
                    mask_png_base64: "iVBORw0=".to_string(),
                },
            }],
            model: Some("sam3".to_string()),
            encode_seconds: Some(4.2),
            decode_seconds: vec![1.01],
            elapsed_seconds: Some(5.3),
            backend: Some("cpu".to_string()),
            generation_id: None,
        },
        json!({
            "width": 512,
            "height": 512,
            "prompts": ["arm"],
            "count": 1,
            "parts": [{
                "name": "arm",
                "score": 0.41,
                "box": [0.0, 0.0, 8.0, 8.0],
                "area": 64,
                "mask_png_base64": "iVBORw0=",
            }],
            "model": "sam3",
            "encode_seconds": 4.2,
            "decode_seconds": [1.01],
            "elapsed_seconds": 5.3,
            "backend": "cpu",
        }),
    );
}

/// Pins the upstream `/parts` contract this client deserialises — `box` is the
/// wire name and a Rust keyword, so a rename here silently drops every box.
#[test]
fn parts_upstream_response_parses() {
    let raw = json!({
        "width": 1024, "height": 1024,
        "prompts": ["arm", "leg"],
        "count": 1,
        "parts": [{
            "name": "arm", "score": 0.48, "box": [10.0, 20.0, 110.0, 220.0],
            "area": 4096, "mask_png_base64": "iVBORw0=",
        }],
        "model": "sam3", "encode_seconds": 4.2, "decode_seconds": [1.01, 0.98],
        "elapsed_seconds": 6.3, "backend": "vulkan",
    });
    let parsed: crate::parts::PartsResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(parsed.parts[0].bbox, [10.0, 20.0, 110.0, 220.0]);
    assert_eq!(parsed.parts[0].area, 4096);
    assert_eq!(parsed.parts[0].name, "arm");
}

/// The masks are registered against the consumer's own alpha crop
/// PROPORTIONALLY, so an event without the frame they are at is one it cannot
/// use — and a missing key is invisible from this side of the stream. Pinned as
/// an exact field SET, not a subset.
#[test]
fn summon_parts_stage_carries_the_frame_the_masks_are_at() {
    use crate::routes::summon::PartsStage;
    let v = serde_json::to_value(PartsStage {
        width: 512,
        height: 288,
        parts: vec![],
        render_ms: 6300,
    })
    .unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["height", "parts", "render_ms", "width"]);
    assert_eq!(v["width"], 512);
    assert_eq!(v["height"], 288);
}

/// `error` — one stage failing is an event, not the end of the stream, so it has
/// to say which stage it was about in the same spelling as that stage's event.
#[test]
fn summon_error_event_json() {
    use crate::routes::summon::{Stage, StageError};
    eq(
        &StageError {
            stage: Stage::Sound,
            error: "sa3: connection refused".to_string(),
        },
        json!({ "stage": "sound", "error": "sa3: connection refused" }),
    );
}

/// `ready` — terminal, and the client's chance to reconcile anything it missed.
#[test]
fn summon_ready_stage_json() {
    use crate::routes::summon::ReadyStage;
    eq(
        &ReadyStage {
            sheet_ok: true,
            image_ok: true,
            sound_ok: false,
            depth_ok: true,
            parts_ok: true,
            render_ms: 12_400,
            gate_wait_ms: 4200,
        },
        json!({
            "sheet_ok": true,
            "image_ok": true,
            "sound_ok": false,
            "depth_ok": true,
            "parts_ok": true,
            "render_ms": 12_400,
            "gate_wait_ms": 4200,
        }),
    );
}

/// `sheet` carries the `/api/v1/character_sheet` body verbatim — the same struct,
/// so the caller parses one shape whichever route it came from.
#[test]
fn summon_sheet_stage_is_the_character_sheet_body() {
    use crate::routes::character_sheet::{
        CharacterSheet, Death, Emits, Kind, Locomotion, Motion, Skill,
    };
    let sheet = CharacterSheet {
        name: "Heron".to_string(),
        subject: "a grey heron".to_string(),
        sound_desc: "a harsh croaking squawk".to_string(),
        height_m: 1.0,
        speed_mps: 11.0,
        mass_kg: 1.5,
        hp: 8,
        strength: 3,
        kind: Kind::Fighter,
        locomotion: vec![Locomotion::Walk],
        roaming: false,
        skills: vec![Skill::Shoot],
        parts: vec![crate::routes::character_sheet::Part {
            name: "wings".to_string(),
            motion: Motion::Flap,
            count: 2,
        }],
        death: Death::Ragdoll,
        explosion_radius_m: 0.0,
        mountable: false,
        buoyant: true,
        front_back_symmetric: false,
        emits: Emits::None,
    };
    let v = serde_json::to_value(&sheet).unwrap();
    assert_eq!(v["height_m"], 1.0);
    assert_eq!(v["subject"], "a grey heron");
    assert_eq!(v["kind"], "fighter");
    // Round-trips: the event payload is the route body, not a projection of it.
    let back: CharacterSheet = serde_json::from_value(v).unwrap();
    assert_eq!(back, sheet);
}

// ── MiniMax-H3 reference roles ────────────────────────────────────────────────

/// The eleven role strings ARE the wire. The Director's editor sends them by name
/// (`web/src/lib/director/h3-roles.ts`), a saved timeline stores them verbatim, and the prompt
/// emitter and the render path now read the SAME enum out of koblibs — so a rename in one place is
/// a rename everywhere, which is exactly why it has to break here first.
///
/// 🔴 Pinned as strings and not as variants: the failure this guards is a `rename_all` change or a
/// variant spelling drifting, both of which compile perfectly and silently downgrade every saved
/// reference to its kind's default (`identity_source` / `structure_source` / `voice_timbre`) — a
/// weaker `summary:` prefix on a render that merely looks mediocre.
#[test]
fn h3_reference_role_wire_strings() {
    for (role, wire) in [
        (H3ReferenceRole::FirstFrame, "first_frame"),
        (H3ReferenceRole::LastFrame, "last_frame"),
        (H3ReferenceRole::IdentitySource, "identity_source"),
        (H3ReferenceRole::FrameAnchor, "frame_anchor"),
        (H3ReferenceRole::Storyboard, "storyboard"),
        (H3ReferenceRole::EditSource, "edit_source"),
        (H3ReferenceRole::ContinuationSource, "continuation_source"),
        (H3ReferenceRole::StructureSource, "structure_source"),
        (H3ReferenceRole::VoiceTimbre, "voice_timbre"),
        (H3ReferenceRole::AudioReuse, "audio_reuse"),
        (H3ReferenceRole::AudioContinuity, "audio_continuity"),
    ] {
        eq(&role, json!(wire));
        assert_eq!(role.as_str(), wire);
    }
    // `kind` shares its three strings with the prompt emitter's briefs, which now name this very
    // type — so `{"kind":"image"}` has to keep meaning what it meant.
    eq(&H3ReferenceKind::Image, json!("image"));
    eq(&H3ReferenceKind::Video, json!("video"));
    eq(&H3ReferenceKind::Audio, json!("audio"));
}

/// One reference, fully populated, as `routes::minimax_h3::build_chain` reads it back off a saved
/// Director document. An absent `role` must not serialise — the render path and the prompt path
/// both resolve absence through `default_for` the KIND, and a materialised default here would
/// freeze one of them into the document.
#[test]
fn h3_reference_json() {
    eq(
        &H3Reference {
            kind: H3ReferenceKind::Video,
            role: Some(H3ReferenceRole::ContinuationSource),
            bank: Some("h3_9f2c:0".to_string()),
            with_audio: true,
            tail_frames: Some(56),
            label: Some("Ines".to_string()),
            ..H3Reference::default()
        },
        json!({
            "kind": "video",
            "role": "continuation_source",
            "bank": "h3_9f2c:0",
            "with_audio": true,
            "tail_frames": 56,
            "label": "Ines",
        }),
    );
    eq(
        &H3Reference {
            kind: H3ReferenceKind::Image,
            image: Some("QUJD".to_string()),
            ..H3Reference::default()
        },
        json!({ "kind": "image", "image": "QUJD" }),
    );
}
