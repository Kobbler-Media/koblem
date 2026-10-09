//! Avatar — image + audio into a talking-head WebM.
//!
//! # 🔴 The one engine in the fleet with no job model
//!
//! `POST /generate` on the avatar deployment is **synchronous**: it blocks for minutes and returns
//! the `video/webm` body on the same request, and it **429s on a concurrent render**. Everything
//! unusual about this adapter follows from that single fact:
//!
//! * there is no `server_job_id` to publish and no poll loop — `run` is one blocking call;
//! * [`AvatarAdapter::reattach`] **cannot recover a render**. The bytes went with the connection, and
//!   the supervisor keeps no record to re-poll and writes no file to fetch. The row is failed with
//!   that in as many words rather than pretending, or silently re-rendering minutes of GPU the user
//!   never asked for twice;
//! * [`AvatarAdapter::cancel`] has nothing engine-side to call. The supervisor's
//!   `/v1/admin/unload {"force":true}` is a force-unload of *whatever it is rendering*, not a cancel
//!   of a named job — with no job model koblem cannot prove the in-flight render is this one, and
//!   SIGKILLing someone else's is worse than letting this request drop.
//!
//! And because there is no cancellation point inside `generate`, the request is left to finish
//! rather than dropped: abandoning it leaves the supervisor rendering on, 429ing the next submit
//! while the lane reads as free.
//!
//! # Payload
//!
//! The supervisor `/generate` body exactly as the route builds it — `width`/`height` resolved to
//! the model's sanctioned tier, `sample_params`, `seed` — except that `image` and `audio` are
//! `asset:<hash>` refs the route captured at enqueue and this adapter swaps for base64 at dispatch.
//! `gpu` is injected here from the card the dispatcher chose.
//!
//! ⚠️ A queued render has no HTTP response to stream bytes into, so the route's `save_clip: false`
//! (preview-only) mode has no meaning here: the clip is always written under `AVATARS_DIR` and
//! always filed in the gallery. A render nobody kept would be minutes of GPU for nothing.

use async_trait::async_trait;
use base64::Engine as _;
use kob_gpu_gate::ServiceKind;
use koblem_client::types::{GenerationOutput, Job, JobKind, JobProgress, Preempt};
use serde_json::{Value, json};
use tracing::warn;

use super::verdict;
use crate::AppState;
use crate::avatar::Error as AvatarError;
use crate::director_assets;
use crate::queue::runner::{Failure, Interrupt, JobCtx, JobOutcome, JobRunner, RunResult};
use crate::repositories::gpu_jobs as store;
use crate::routes::avatar::{file_stamp_filename, save_clip};
use crate::routes::tier2_helpers::{inject_gpu, record_generation};

pub struct AvatarAdapter {
    state: AppState,
}

fn verdict_of(e: &AvatarError) -> Failure {
    match e {
        AvatarError::Request(e) => verdict::transport(e),
        // 🔴 429 here is not a bad request: the supervisor renders synchronously
        // with no job model, so it means *someone else's* render owns it right now —
        // and a drain ends the same way. Both are "try again shortly", and both are
        // the one failure this engine produces that a second slot actually fixes.
        AvatarError::Upstream(status, _) => verdict::upstream(*status),
        AvatarError::Preempted => Failure::Transient,
        AvatarError::NotConfigured | AvatarError::MissingField(_) => Failure::Permanent,
    }
}

impl AvatarAdapter {
    pub fn new(state: AppState) -> Self {
        AvatarAdapter { state }
    }

    /// Swap one `asset:<hash>` ref for the base64 the supervisor's `_materialize` path reads.
    async fn inline_media(&self, body: &mut Value, key: &str, what: &str) -> Result<(), String> {
        let reference = require_ref(body, key, what)?.to_string();
        let dir = self.state.director_assets_dir.as_ref();
        let bytes = director_assets::resolve_media(dir, &reference)
            .await
            .map_err(|e| format!("{what}: {e}"))?
            .ok_or_else(|| format!("{what}: the asset store holds no bytes for {reference}"))?;
        body[key] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
        Ok(())
    }
}

/// The request as the gallery should remember it — **without** the two media fields. `request_meta`
/// exists to be read back into a form, and neither an asset ref nor the base64 it stands for is
/// something a form can repopulate.
fn meta_of(payload: &Value) -> Value {
    let mut meta = payload.clone();
    if let Some(obj) = meta.as_object_mut() {
        let had_image = obj.remove("image").is_some();
        let had_audio = obj.remove("audio").is_some();
        obj.insert("had_media".into(), json!(had_image && had_audio));
        obj.insert("engine".into(), json!("avatar"));
    }
    meta
}

/// Duration is the only term that moves this render, and the buckets are coarse on purpose. An
/// absent `duration_sec` means the audio decides the length; that is its own bucket rather than a
/// guessed number, because a made-up duration would corrupt the measured ones.
fn bucket(payload: &Value) -> &'static str {
    match payload.get("duration_sec").and_then(Value::as_f64) {
        None => "auto",
        Some(s) if s <= 10.0 => "10s",
        Some(s) if s <= 30.0 => "30s",
        Some(s) if s <= 60.0 => "60s",
        Some(_) => "long",
    }
}

fn require_ref<'a>(payload: &'a Value, key: &str, what: &str) -> Result<&'a str, String> {
    let s = payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if s.is_empty() {
        return Err(format!(
            "{what} is missing — `{key}` must carry the `asset:<hash>` ref the render is driven \
             from"
        ));
    }
    if director_assets::ref_hash(s).is_none() {
        return Err(format!(
            "{what} must be an `asset:<hash>` ref, not inline media — an enqueued render \
             dispatches hours after the upload, so the bytes live in the asset store and the \
             queue row carries only the reference"
        ));
    }
    Ok(s)
}

#[async_trait]
impl JobRunner for AvatarAdapter {
    fn kind(&self) -> JobKind {
        JobKind::Avatar
    }

    fn service(&self) -> ServiceKind {
        ServiceKind::Avatar
    }

    /// Whole-job retry only: the supervisor publishes no checkpoint, no bank and no partial file,
    /// so there is nothing a preempt could hand back.
    fn preempt(&self) -> Preempt {
        Preempt::Kill
    }

    fn classify(&self, error: &str) -> Failure {
        verdict::recall(error)
    }

    /// A render that reaches the supervisor without either ref is a 400 after the queue has
    /// already waited for a card.
    fn validate(&self, payload: &Value) -> Result<(), String> {
        require_ref(payload, "image", "the reference portrait")?;
        require_ref(payload, "audio", "the spoken audio")?;
        Ok(())
    }

    fn shape_key(&self, payload: &Value) -> String {
        format!("avatar/{}", bucket(payload))
    }

    fn units(&self, _payload: &Value) -> Option<i32> {
        Some(1)
    }

    fn label(&self, payload: &Value) -> String {
        let prompt = payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(48)
            .collect::<String>();
        let secs = bucket(payload);
        if prompt.is_empty() {
            format!("avatar · {secs}")
        } else {
            format!("avatar · {secs} · {prompt}")
        }
    }

    async fn run(&self, ctx: JobCtx) -> RunResult {
        let client = &self.state.avatar;
        if !client.is_configured() {
            return Err("the avatar service is not configured".to_string());
        }
        let mut body = ctx.payload.clone();
        self.inline_media(&mut body, "image", "the reference portrait")
            .await?;
        self.inline_media(&mut body, "audio", "the spoken audio")
            .await?;
        // 🔴 The card the dispatcher placed us on. A submit that omits it lands on the worker's own
        // WORKER_DEFAULT_GPU, which is how a worker ends up resident on a disabled card.
        inject_gpu(&mut body, ctx.gpu.as_deref());
        ctx.progress(
            JobProgress {
                phase: Some("rendering".into()),
                ..Default::default()
            },
            0,
        )
        .await;

        let outcome = client.generate(&body).await;
        // 🔴 THE ENGINE/DISK BOUNDARY. The bytes are in hand; the card must not idle through the
        // write and the gallery insert.
        ctx.release_gpu();

        let result = match outcome {
            Ok(r) => r,
            Err(e) => {
                let message = match &e {
                    AvatarError::Preempted => {
                        "the avatar supervisor was draining and refused the render — TTS or an \
                         operator drain took the slot"
                            .to_string()
                    }
                    AvatarError::Upstream(429, detail) => format!(
                        "the avatar supervisor is already rendering and refused a second one \
                         ({detail}). It renders synchronously with no job model, so a render \
                         koblem did not schedule closes it to the queue until that render ends."
                    ),
                    e => format!("avatar-server: {e}"),
                };
                return Err(verdict::note(message, verdict_of(&e)));
            }
        };

        let seed = ctx
            .payload
            .get("seed")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let filename = file_stamp_filename(seed);
        save_clip(&self.state, &filename, &result.bytes)
            .await
            .map_err(|e| format!("the render finished but the clip could not be saved: {e}"))?;

        record_generation(
            &self.state,
            "avatar",
            "generate",
            meta_of(&ctx.payload),
            vec![GenerationOutput {
                filename: filename.clone(),
                mime: "video/webm".to_string(),
                size_bytes: result.bytes.len() as i64,
                kind: "video".to_string(),
            }],
        )
        .await;

        let row = json!({
            "clip": {
                "filename": filename,
                "url": format!("/api/v1/avatar/file/{filename}"),
                "content_type": "video/webm",
                "bytes": result.bytes.len(),
            },
        });
        if let Err(e) = store::set_result(&self.state.db, &ctx.job.id, &row).await {
            warn!(job = %ctx.job.id, error = %e, "avatar: could not record the result");
        }
        ctx.progress(
            JobProgress {
                phase: Some("saved".into()),
                ..Default::default()
            },
            1,
        )
        .await;

        // A cancel that arrived mid-render had nothing to stop, and the clip is the only copy of
        // GPU minutes already paid for. Keeping it silently would be the dishonest half.
        let warnings = if ctx.cancel.get() == Interrupt::Cancel {
            vec![
                "the cancel arrived while the supervisor was already rendering — it has no job \
                 model to cancel into, so the render finished and the clip was kept"
                    .to_string(),
            ]
        } else {
            vec![]
        };
        Ok(JobOutcome::Done { warnings })
    }

    /// 🔴 Unrecoverable, and said so rather than guessed at.
    ///
    /// `/generate` streams the WebM back on the request that started it and keeps no record: there
    /// is no id to re-poll, no file on the engine to fetch, and the connection died with koblem. The
    /// alternative — silently re-rendering — would spend minutes of GPU the user never asked for
    /// twice and, on the 429 path, would fail anyway if the supervisor is still finishing the
    /// original.
    async fn reattach(&self, _ctx: JobCtx) -> RunResult {
        Err(
            "koblem restarted while this avatar render was in flight, and the render cannot be \
             recovered: the supervisor renders synchronously, returns the clip on the request that \
             started it and keeps no job record, so the bytes went with the connection. Run this \
             job again to re-render it."
                .to_string(),
        )
    }

    /// Nothing to call. See the module header: the only engine-side lever is a force-unload of
    /// whatever is rendering, which koblem cannot prove is this job.
    async fn cancel(&self, _job: &Job) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(c: char) -> String {
        format!("asset:{}", String::from_iter(std::iter::repeat_n(c, 64)))
    }

    /// 🔴 Neither media field reaches `request_meta`: it is a JSONB column meant to be read back
    /// into a form, and a ref is no more repopulatable than the base64 it replaced.
    #[test]
    fn the_gallery_meta_carries_no_media() {
        let meta = meta_of(&json!({
            "image": asset('a'), "audio": asset('b'), "seed": 7, "width": 480, "height": 832,
        }));
        assert!(meta.get("image").is_none());
        assert!(meta.get("audio").is_none());
        assert_eq!(meta["had_media"], json!(true));
        assert_eq!(meta["seed"], json!(7));
    }

    /// Coarse buckets, and "the audio decides" is its own bucket rather than an invented number.
    #[test]
    fn durations_bucket_coarsely() {
        assert_eq!(
            bucket(&json!({"duration_sec": 12})),
            bucket(&json!({"duration_sec": 29.5}))
        );
        assert_ne!(
            bucket(&json!({"duration_sec": 12})),
            bucket(&json!({"duration_sec": 300}))
        );
        assert_eq!(bucket(&json!({"seed": 1})), "auto");
    }

    /// A render missing either ref is a 400 minutes into a queue wait, so it never enters the
    /// queue at all — and inline base64 is refused with it, because a payload carrying two whole
    /// files is hashed into the job id and compared whole on every later enqueue.
    #[test]
    fn only_a_pair_of_asset_refs_enters_the_queue() {
        let ok = |v: Value| {
            require_ref(&v, "image", "x")?;
            require_ref(&v, "audio", "y")?;
            Ok::<(), String>(())
        };
        assert!(ok(json!({ "audio": asset('b') })).is_err());
        assert!(ok(json!({ "image": asset('a'), "audio": "" })).is_err());
        let inline = ok(json!({ "image": asset('a'), "audio": "QkJCQg==" })).unwrap_err();
        assert!(inline.contains("asset:<hash>"), "{inline}");
        assert!(ok(json!({ "image": asset('a'), "audio": asset('b') })).is_ok());
    }

    /// 🔴 The one engine whose ordinary failure IS "someone else has it". A 429 and
    /// a drain both mean the supervisor is busy right now, and the render they
    /// refused is worth the slot it did not get.
    #[test]
    fn a_busy_supervisor_is_retried_and_a_bad_request_is_not() {
        let t = |e: AvatarError| verdict_of(&e);
        assert_eq!(t(AvatarError::Preempted), Failure::Transient);
        assert_eq!(
            t(AvatarError::Upstream(429, "avatar busy".into())),
            Failure::Transient
        );
        assert_eq!(
            t(AvatarError::Upstream(503, "avatar draining".into())),
            Failure::Transient
        );
        assert_eq!(
            t(AvatarError::Upstream(500, "avatar boom".into())),
            Failure::Transient
        );
        assert_eq!(
            t(AvatarError::Upstream(400, "avatar bad tier".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(AvatarError::Upstream(404, "avatar no route".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(AvatarError::Upstream(409, "avatar held".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(AvatarError::Upstream(410, "avatar gone".into())),
            Failure::Permanent
        );
        assert_eq!(
            t(AvatarError::Upstream(422, "avatar bad audio".into())),
            Failure::Permanent
        );
        assert_eq!(t(AvatarError::NotConfigured), Failure::Permanent);
        assert_eq!(t(AvatarError::MissingField("video")), Failure::Permanent);
    }

    /// The queue only ever sees the flattened sentence, so the verdict has to
    /// survive the flattening.
    #[test]
    fn the_verdict_survives_the_flattening() {
        let e = AvatarError::Upstream(429, "avatar is already rendering".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Transient);
        let e = AvatarError::Upstream(400, "avatar refused that portrait".into());
        let msg = verdict::note(e.to_string(), verdict_of(&e));
        assert_eq!(verdict::recall(&msg), Failure::Permanent);
    }
}
