//! Vision-in-the-loop dailies review for the LTX Director.
//!
//! `/review` reads the *prompts* — a script supervisor. This reads the FOOTAGE: frames are sampled
//! out of each rendered shot, handed to the vision LLM alongside the prompt (and the shot's Prompt
//! Relay beats, with the window each was supposed to land in), and it answers whether what was
//! asked for is actually on screen. A shot it fails can be re-rolled automatically, which is what
//! makes an unattended overnight run worth starting: every re-roll is a NEW take
//! ([`crate::repositories::director_shot_renders`]), so nothing is overwritten and the pick is made
//! in the morning by a human looking at the options.
//!
//! Two invariants this module is built around:
//!   * ONE GPU job at a time. A retake is dispatched and then AWAITED to a terminal state before
//!     the next shot is even judged — the vision model and the renderer share the card.
//!   * Never destroy a take. The critic only ever adds takes and annotates them; selection stays a
//!     human decision (`…/renders/{tier}/{shot}/select`).

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::repositories::director_projects as repo;
use crate::repositories::director_shot_renders as repo_renders;

/// Frames sampled per shot. Four reads the shape of a 4-5s take (start / two middles / end) without
/// turning one judgement into a long multi-image prompt the 12B model starts losing track of.
const FRAMES_PER_SHOT: usize = 4;

/// How long to wait for one auto-retake before giving up on that shot and moving on. A 5s shot at
/// the final tier is minutes, not seconds, and an overnight run must not wedge on a stuck job.
const RETAKE_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// Poll interval while waiting on a retake job.
const POLL_EVERY: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
pub struct CritiquePayload {
    /// Which render stream to judge. `proxy` is the point of the feature — cheap takes, judged and
    /// re-rolled overnight — but `final` works identically.
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Shots to judge (prompt-filtered indices). Empty = every rendered shot of that tier.
    #[serde(default)]
    pub shots: Vec<i32>,
    /// Re-roll a shot the critic fails, as a new take. Off by default: judging is read-only unless
    /// the caller asks for the GPU to be spent.
    #[serde(default)]
    pub auto_retake: bool,
    /// Most extra takes to spend on ONE shot before accepting the best of what it has.
    #[serde(default = "default_max_retakes")]
    pub max_retakes: u32,
    /// A take scoring below this fails. 0.6 is deliberately generous — the cost of a false fail is
    /// a wasted render, and the critic is advisory.
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    /// Re-judge takes that already carry a verdict (default: skip them, so a re-run is cheap).
    #[serde(default)]
    pub rejudge: bool,
}

fn default_tier() -> String {
    "proxy".into()
}
fn default_max_retakes() -> u32 {
    2
}
fn default_threshold() -> f64 {
    0.6
}

/// `POST /api/v1/ltx-director/projects/{id}/critique` — judge the rendered footage against the
/// script, and optionally re-roll what fails.
///
/// Returns 202 immediately: judging every shot (and re-rolling some of them) is a long GPU-bound
/// job. Progress shows up as `critic` verdicts on the takes, readable through
/// `…/renders/{tier}/{shot}/takes`.
pub async fn critique(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CritiquePayload>,
) -> Response {
    if let Some(resp) = super::ltx_director::auth_gate(&state, &headers).await {
        return resp;
    }
    let Some(tier) = super::ltx_director::normalize_tier(&body.tier) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad tier"}))).into_response();
    };
    if repo::get(&state.db, &id).await.ok().flatten().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "project not found"})),
        )
            .into_response();
    }
    let state2 = state.clone();
    let id2 = id.clone();
    tokio::spawn(async move {
        run_critique(state2, id2, tier, body).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "queued", "tier": tier })),
    )
        .into_response()
}

/// Run the dailies review immediately after a full render finishes, if the project asked for it.
///
/// This is what makes the feature unattended: the harvest only happens at the END of a run, so a
/// render that finishes at 3am can review (and re-roll) itself instead of waiting for someone to
/// press a button. Called from the two FULL-RUN paths only, and gated by an explicit
/// `director_critique` flag on the render rather than anything the server infers — a retake must
/// never chain into a review, or the critic's own re-rolls would recurse.
///
/// Settings live on the timeline doc (`timeline.critique`) so they are readable without the editor
/// open. Absent or `enabled: false` = no-op, so an existing project renders exactly as before.
pub(crate) async fn maybe_critique_after_render(state: AppState, job_id: String) {
    let Some(job) = state.ltx_jobs.get(&job_id) else {
        return;
    };
    // Only a render that actually produced something is worth judging.
    if job.state != koblem_client::types::VideoJobState::Completed {
        return;
    }
    let (Some(project_id), Some(tier)) = (job.director_project_id, job.director_tier) else {
        return;
    };
    let Some(tier) = super::ltx_director::normalize_tier(&tier) else {
        return;
    };
    let Ok(Some(project)) = repo::get(&state.db, &project_id).await else {
        return;
    };
    let Some(settings) = project.timeline.get("critique") else {
        return;
    };
    if !settings
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }
    let body = CritiquePayload {
        tier: tier.to_string(),
        shots: Vec::new(),
        auto_retake: settings
            .get("auto_retake")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_retakes: settings
            .get("max_retakes")
            .and_then(Value::as_u64)
            .map(|n| n.min(10) as u32)
            .unwrap_or_else(default_max_retakes),
        threshold: settings
            .get("threshold")
            .and_then(Value::as_f64)
            .filter(|t| (0.0..=1.0).contains(t))
            .unwrap_or_else(default_threshold),
        rejudge: false,
    };
    warn!(
        project = %project_id, tier, auto_retake = body.auto_retake, max_retakes = body.max_retakes,
        "critic: starting the post-render dailies review"
    );
    run_critique(state, project_id, tier, body).await;
}

/// One shot's judgement plus the retakes it triggered.
async fn run_critique(
    state: AppState,
    project_id: String,
    tier: &'static str,
    body: CritiquePayload,
) {
    let takes = match repo_renders::selected_shots(&state.db, &project_id, tier).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, project = %project_id, "critic: could not list shots");
            return;
        }
    };
    let wanted: Vec<repo_renders::ShotRenderRow> = takes
        .into_iter()
        .filter(|r| body.shots.is_empty() || body.shots.contains(&r.shot_index))
        .collect();
    if wanted.is_empty() {
        warn!(project = %project_id, tier, "critic: nothing rendered to judge");
        return;
    }

    // The saved timeline is where the beats live — the render request carries them, and a beat's
    // window is what makes "did it land?" measurable rather than a vibe.
    //
    // Empty on H3, and correctly so: that engine has no beat conditioning, and its own within-take
    // timing lives in the `[Shot N] At MM:SS.mmm` markers inside the brief the judge already reads.
    let beats_by_shot = load_beats(&state, &project_id).await;
    // Does a take here hold more than one cut? A property of the ENGINE, read once — see the system
    // prompt in `judge_shot`.
    let multi_cut = repo::get(&state.db, &project_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|p| {
            super::ltx_director::project_engine(&p.timeline) == super::ltx_director::ENGINE_H3
        });

    for row in wanted {
        if row.critic.is_some() && !body.rejudge {
            continue;
        }
        let mut attempts = 0u32;
        let mut shot = row;
        loop {
            let beats = beats_by_shot
                .get(&shot.shot_index)
                .cloned()
                .unwrap_or_default();
            let verdict =
                match judge_shot(&state, &project_id, tier, &shot, &beats, multi_cut).await {
                    Some(v) => v,
                    None => break,
                };
            let score = verdict.get("score").and_then(Value::as_f64).unwrap_or(1.0);
            let passed = score >= body.threshold;
            let stored = json!({
                "score": score,
                "verdict": if passed { "pass" } else { "fail" },
                "note": verdict.get("note").cloned().unwrap_or(Value::Null),
                "missing": verdict.get("missing").cloned().unwrap_or(Value::Null),
                "beats": verdict.get("beats").cloned().unwrap_or(Value::Null),
                "threshold": body.threshold,
                "at": now_rfc3339(),
            });
            if let Err(e) = repo_renders::set_critic(
                &state.db,
                &project_id,
                tier,
                shot.shot_index,
                shot.variant,
                &stored,
            )
            .await
            {
                warn!(error = %e, project = %project_id, shot = shot.shot_index, "critic: verdict write failed");
            }
            if passed || !body.auto_retake || attempts >= body.max_retakes {
                break;
            }
            attempts += 1;
            warn!(
                project = %project_id, tier, shot = shot.shot_index, score, attempt = attempts,
                "critic: shot failed, re-rolling as a new take"
            );
            // Re-roll with a fresh PER-SHOT seed so the other shots' banked latents are untouched.
            let Some(job_id) = retake_shot(&state, &project_id, tier, shot.shot_index).await else {
                break;
            };
            if !await_job(&state, &job_id).await {
                warn!(project = %project_id, shot = shot.shot_index, job = %job_id, "critic: retake did not finish");
                break;
            }
            // Judge whatever take the retake produced (the harvest selects it).
            match repo_renders::get(&state.db, &project_id, tier, shot.shot_index).await {
                Ok(Some(next)) if next.variant != shot.variant => shot = next,
                _ => break,
            }
        }
        // Leave the BEST-SCORING judged take selected, not merely the last one rolled.
        //
        // Every take is kept and the choice is still the user's — this only decides what they open
        // to. Harvesting selects whatever it just wrote, so without this a shot that got worse on
        // re-roll (0.55 then 0.40) would sit on its worst take, which is the opposite of what an
        // unattended run is for. Ties keep the earliest, so a re-roll has to actually beat the
        // take it was trying to improve on.
        if attempts > 0 {
            select_best_take(&state, &project_id, tier, shot.shot_index).await;
        }
    }
}

/// Point a shot at its highest-scoring judged take. Unjudged takes are ignored rather than treated
/// as zero — a take the critic never got to say anything about is not evidence of anything.
async fn select_best_take(state: &AppState, project_id: &str, tier: &str, shot_index: i32) {
    let Ok(takes) = repo_renders::list_takes(&state.db, project_id, tier, shot_index).await else {
        return;
    };
    let best = takes
        .iter()
        .filter_map(|t| {
            t.critic
                .as_ref()
                .and_then(|c| c.get("score"))
                .and_then(Value::as_f64)
                .map(|score| (t.variant, score))
        })
        .reduce(|best, next| if next.1 > best.1 { next } else { best });
    let Some((variant, score)) = best else {
        return;
    };
    if takes.iter().any(|t| t.variant == variant && t.selected) {
        return;
    }
    match repo_renders::select_take(&state.db, project_id, tier, shot_index, variant).await {
        Ok(true) => {
            warn!(
                project = %project_id, tier, shot = shot_index, variant, score,
                "critic: selected the best-scoring take (all takes kept)"
            );
        }
        Ok(false) => {}
        Err(e) => {
            warn!(error = %e, project = %project_id, shot = shot_index, "critic: best-take select failed");
        }
    }
}

/// Beats per prompt-filtered shot index, read out of the saved timeline's compiled request. The
/// request's `segments` are RAW (including empty-prompt shots), so they are filtered the same way
/// the render path filters them — otherwise a blank shot shifts every later beat onto the wrong clip.
async fn load_beats(
    state: &AppState,
    project_id: &str,
) -> std::collections::HashMap<i32, Vec<(f64, String)>> {
    let mut out = std::collections::HashMap::new();
    let Ok(Some(project)) = repo::get(&state.db, project_id).await else {
        return out;
    };
    let Some(segments) = project
        .timeline
        .pointer("/request/segments")
        .and_then(Value::as_array)
    else {
        return out;
    };
    let mut filtered = 0i32;
    for seg in segments {
        let prompt = seg
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if prompt.is_empty() {
            continue;
        }
        let beats: Vec<(f64, String)> = seg
            .get("beats")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        Some((
                            b.get("seconds").and_then(Value::as_f64)?,
                            b.get("text").and_then(Value::as_str)?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !beats.is_empty() {
            out.insert(filtered, beats);
        }
        filtered += 1;
    }
    out
}

/// Sample frames from one take and ask the vision model whether the footage matches the script.
/// `None` on any failure to sample or call — an unavailable critic must leave the take unjudged
/// rather than mark it failed and trigger a pointless re-roll.
async fn judge_shot(
    state: &AppState,
    project_id: &str,
    tier: &str,
    shot: &repo_renders::ShotRenderRow,
    beats: &[(f64, String)],
    // True when a take may legitimately contain CUTS — see the system prompt below.
    multi_cut: bool,
) -> Option<Value> {
    let dir = super::ltx_director::project_render_dir(state, project_id)?;
    let path = dir.join(&shot.file_name);
    let frames = sample_frames(&path).await;
    if frames.is_empty() {
        warn!(project = %project_id, tier, shot = shot.shot_index, "critic: no frames sampled");
        return None;
    }

    let duration = probe_duration(&path).await.unwrap_or(0.0);
    // 🔴 "one continuous shot" is an LTX fact, not a universal one, and asserting it on H3 would
    // make the critic mark the render's own cuts as defects. An H3 take is a GENERATION: the brief
    // it is judged against carries `[Shot N] At MM:SS.mmm` markers and the model renders those cuts
    // itself, so a change of framing at one of those times is the brief being MET.
    let mut system = String::from(if multi_cut {
        "You are a film editor reviewing dailies. You are shown frames sampled IN ORDER from one \
         rendered take, with the timecode of each. The take may contain several CUTS — the brief \
         marks them as `[Shot N] At MM:SS.mmm`, and a change of framing at one of those times is \
         the brief being met, not a fault. Judge ONLY what is visibly in the frames against the \
         brief — not whether it is pretty. Be concrete about what is missing."
    } else {
        "You are a film editor reviewing dailies. You are shown frames sampled IN ORDER from one \
         continuous shot, with the timecode of each. Judge ONLY what is visibly in the frames \
         against the shot's brief — not whether it is pretty. Be concrete about what is missing."
    });
    system.push_str(
        " Reply with STRICT JSON only, no prose: {\"score\":<0..1>,\"note\":\"<one sentence>\",\
         \"missing\":[\"<what the brief asked for that is not on screen>\"]",
    );
    if beats.is_empty() {
        system.push_str("}. ");
    } else {
        system.push_str(
            ",\"beats\":[{\"text\":\"<the beat>\",\"happened\":<true|false>,\"at\":<seconds or null>}]}. \
             A beat is something the shot was supposed to do AT A PARTICULAR MOMENT: mark it \
             happened only if you can see it in a frame at or near its time, and say which. ",
        );
    }
    system.push_str(
        "score 1.0 = the brief is fully on screen; 0.5 = the subject is right but an asked-for \
         action is absent; 0.0 = unrelated footage. Omit code fences.",
    );

    let unit = if multi_cut { "take" } else { "shot" };
    let mut brief = format!(
        "{} brief:\n{}\n\nThe {unit} is {duration:.1}s long. Frames follow in order at: {}.",
        if multi_cut { "Take" } else { "Shot" },
        shot.prompt.trim(),
        frames
            .iter()
            .map(|(t, _)| format!("{t:.2}s"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !beats.is_empty() {
        brief.push_str("\n\nTimed beats it was supposed to hit:");
        for (seconds, text) in beats {
            brief.push_str(&format!("\n  {seconds:.2}s — {text}"));
        }
    }

    let mut content: Vec<Value> = vec![json!({"type": "text", "text": brief})];
    for (_, jpeg_b64) in &frames {
        content.push(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:image/jpeg;base64,{jpeg_b64}")},
        }));
    }

    let mut request = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": content},
        ],
        "max_tokens": 512,
        "temperature": 0.2,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    crate::sampling::apply_sampling(&state.db, "sampling_ltx_critic", &mut request).await;
    // The director LLM is the vision-capable one (gemma-3-12b-it in prod); the main text LLM is the
    // fallback and will simply score poorly on images, so a missing director LLM is worth logging.
    let client = state.director_llm.as_ref().unwrap_or(&state.llama);
    let text = match client.chat_completion_json(&request).await {
        Ok((_permit, resp)) => resp
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Err(e) => {
            warn!(error = %e, project = %project_id, shot = shot.shot_index, "critic: vision call failed");
            return None;
        }
    };
    serde_json::from_str::<Value>(super::ltx_prompting::extract_json_object(&text)).ok()
}

/// Sample `FRAMES_PER_SHOT` JPEG stills spread across a clip, as `(timecode, base64)`. One ffmpeg
/// call per frame with an input seek: cheap on a 4-5s webm, and far more predictable than asking
/// ffmpeg for an fps-filtered stream and guessing which frames came back.
async fn sample_frames(path: &std::path::Path) -> Vec<(f64, String)> {
    use base64::Engine as _;
    let duration = probe_duration(path).await.unwrap_or(0.0);
    if duration <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(FRAMES_PER_SHOT);
    for i in 0..FRAMES_PER_SHOT {
        // Inset from both ends: the very first and last frames of a trimmed clip are the least
        // representative of what the shot does.
        let at = duration * (i as f64 + 0.5) / FRAMES_PER_SHOT as f64;
        let output = tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-ss"])
            .arg(format!("{at:.3}"))
            .arg("-i")
            .arg(path)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "scale=512:-2",
                "-f",
                "image2pipe",
                "-vcodec",
                "mjpeg",
                "-",
            ])
            .stderr(std::process::Stdio::null())
            .output()
            .await;
        match output {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                out.push((
                    at,
                    base64::engine::general_purpose::STANDARD.encode(&o.stdout),
                ));
            }
            other => {
                warn!(path = ?path, at, status = ?other.map(|o| o.status), "critic: frame sample failed");
            }
        }
    }
    out
}

/// Clip duration in seconds via ffprobe.
async fn probe_duration(path: &std::path::Path) -> Option<f64> {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Re-roll one shot with a fresh seed. Returns the koblem job id.
///
/// The seed is applied to the DISPATCHED request only — the saved document is not touched.
///
/// It used to be persisted, "so the timeline and the take agree". That was the wrong place for the
/// agreement and it cost more than it bought: the write bumped the version, so an overnight run
/// disarmed an open editor's autosave once per re-roll (the one server-side write that broke the
/// rule the version column was added for), and a critic that lost the CAS rendered a seed the
/// document then disagreed with anyway. A take's provenance belongs on the TAKE — `job.seed` is
/// recorded on the row the harvest writes — while the timeline stays the author's.
///
/// Both the chain seed and the shot's own are set: a per-segment seed overrides the chain one for
/// that shot, so setting only the chain seed would re-roll a seeded shot to the identical take.
///
/// `tier` is threaded all the way through: a proxy re-roll must resume the PROXY chain's bank and
/// render at the proxy geometry, or it is neither cheap nor a take of the thing being judged.
async fn retake_shot(
    state: &AppState,
    project_id: &str,
    tier: &'static str,
    shot_index: i32,
) -> Option<String> {
    let project = repo::get(&state.db, project_id).await.ok().flatten()?;
    // 🔴 The two engines re-roll different UNITS, and the index means different things in each.
    // On LTX a take is one segment, and `shot_index` is a prompt-filtered segment index. On H3 a
    // take is one GENERATION — which may hold several cuts — and `shot_index` is already the
    // generation index (`LtxJob::director_shot_offset`). Poking `/request/segments` below on an H3
    // document finds no such key and returns `None`, which reads as "the critic declined to
    // re-roll" and is exactly the silence this branch removes.
    let engine = super::ltx_director::project_engine(&project.timeline);
    if engine == super::ltx_director::ENGINE_H3 {
        return super::ltx_director::critic_retake_h3(
            state,
            project_id,
            tier,
            shot_index.max(0) as usize,
        )
        .await;
    }
    // 🔴 A DECLINE, not an error — the critic runs unattended and a re-roll this deployment cannot
    // make must not fail the review. LTX-2.5 answers 501 to `retake_from` with
    // `COMFY_API_LTX_BANK_DIR` unset, and without this an overnight dailies pass dispatches one
    // doomed job per low-scoring shot and waits out each one's queue. Cached on the client, so a
    // twelve-shot review asks once.
    // 🔴 The same gate every other resume/retake dispatch takes, not a second reading of half of it.
    // `bank_live()` alone says nothing about whether a base URL is even set, which let the critic
    // queue a retake against a service that is not configured. The refusal body is dropped — the
    // critic declines rather than answering — but the question has to be the one `bank_gate` asks
    // or the three dispatch sites drift again.
    if super::ltx_director::bank_gate(state, engine)
        .await
        .is_some()
    {
        tracing::info!(
            project = %project_id, shot = shot_index, engine,
            "critic: skipping auto-retake — this deployment has no reachable latent bank to splice \
             a single shot into"
        );
        return None;
    }
    let mut timeline = project.timeline.clone();
    let segments = timeline
        .pointer_mut("/request/segments")
        .and_then(Value::as_array_mut)?;

    // `shot_index` is prompt-FILTERED (that is what the engine and the render store use); the
    // timeline's segments are raw. Walk to the raw index of the Nth non-empty shot.
    let mut filtered = 0i32;
    let mut raw = None;
    for (i, seg) in segments.iter().enumerate() {
        let prompt = seg
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if prompt.is_empty() {
            continue;
        }
        if filtered == shot_index {
            raw = Some(i);
            break;
        }
        filtered += 1;
    }
    let raw = raw?;
    // A fresh seed from the clock. rem_euclid, not abs(): abs() panics on i64::MIN, and a seed is
    // not worth a reachable panic.
    let seed =
        (time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64).rem_euclid(2_000_000_000);
    segments[raw]["seed"] = json!(seed);
    let mut request_val = timeline.get("request").cloned()?;
    request_val["seed"] = json!(seed);

    let resume_job_id = super::ltx_director::resolve_resume_job_id_for_tier(
        state,
        project_id,
        &project.metadata,
        tier,
        engine,
        None,
    )
    .await
    .ok()?;
    let prepared = match super::ltx_director::prepare_retake(
        state,
        super::ltx_director::RetakeDispatch {
            project_id,
            timeline: &timeline,
            request_val,
            resume_job_id: &resume_job_id,
            segment: raw,
            tier,
            engine,
            proxy_overrides: project.metadata.get("proxy_render"),
        },
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(message) => {
            warn!(project = %project_id, shot = shot_index, message = %message, "critic: retake dispatch failed");
            return None;
        }
    };
    // 🔴 `EnvKey`: koblem itself, not a person. It also means the re-roll enqueues at band 0 rather
    // than behind the night's queue — the critic blocks on this job before it judges the next shot,
    // so parking it behind an unrelated cut would stall the review it is part of.
    super::ltx_director::spawn_retake(
        state,
        &crate::auth::Principal::EnvKey,
        // The critic has no request to read a pin or a band off — it IS koblem.
        crate::routes::queue::QueueOptions::default(),
        prepared,
    )
        .await
        .inspect_err(
            |message| warn!(project = %project_id, shot = shot_index, %message, "critic: retake dispatch failed"),
        )
        .ok()
}

/// Block until a koblem LTX job reaches a terminal state. True iff it completed. This is what keeps
/// the critic to one GPU job at a time — the next shot is not even judged until this render is done.
async fn await_job(state: &AppState, job_id: &str) -> bool {
    let deadline = tokio::time::Instant::now() + RETAKE_TIMEOUT;
    loop {
        match state.ltx_jobs.get(job_id) {
            Some(job) if job.state.is_terminal() => {
                return job.state == koblem_client::types::VideoJobState::Completed;
            }
            // A job that vanished from the store (TTL sweep) cannot be waited on any further.
            None => return false,
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
