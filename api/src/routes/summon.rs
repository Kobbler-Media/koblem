//! `POST /api/v1/summon` — the same summon `/oneshot` performs, streamed as the
//! stages land instead of held back until the last one does.
//!
//! **Why.** `/oneshot` answers once, after 5–15 s of gate queue and render, so
//! the game has nothing to show but a placeholder for the whole wait. The work
//! itself is not one thing: the sheet is a ~1 s LLM call, the sound is an
//! independent engine, and the depth pass is a rounding error after the image.
//! Emitting each as it lands lets the caller stand a correctly-sized, correctly-
//! named thing in the world within a second and dress it as the rest arrives.
//!
//! **The sheet leads, and nothing waits on the image.** `height_m` is what turns
//! a generic blob into a placeholder of the right size, so it goes out on its own
//! event the moment the LLM answers.
//!
//! **Stages fail independently.** A failed stage emits an `error` event naming
//! itself and the run carries on: a thing with no sound is silent, a thing with
//! no sprite is broken, and neither is a reason to lose the other. Only auth, a
//! malformed body and an unconfigured image engine are non-2xx — after the
//! stream opens, every outcome is an event.
//!
//! **Nothing here re-implements `/oneshot`.** The image and sound halves are
//! `oneshot::render_image` and `oneshot::render_sound` verbatim, the sheet is
//! `character_sheet::sheet_for`, and the depth and parts passes are
//! `depth::depth_inline_one` and `parts::parts_inline_one`. This module is the
//! ordering, the sheet-derived part prompts, and the wire format.
//!
//! **Prompts arrive as templates.** The caller owns its prompt wrapping (framing,
//! house style, the constraints that keep the matte usable) but cannot fill in
//! the subject until the sheet exists — which is the round trip this route
//! removes. So `image.positive` may contain `{subject}` and `sound.prompt`
//! `{sound_desc}`, and the sheet's values are substituted in here.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::post,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

use crate::AppState;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::character_sheet::{self, CharacterSheet, Motion};
use crate::routes::depth::{self, DepthResultOut};
use crate::routes::oneshot::{self, ImageSpec, SoundSpec};
use crate::routes::parts::{self, PartOut};
use crate::routes::tier2_helpers::{extract_caller, log_async};

/// Prompt templates only, same as `/oneshot`.
const SUMMON_BODY_LIMIT: usize = 256 * 1024;

const SERVICE: &str = "summon";
const ENDPOINT: &str = "summon";

/// The gate-queued render carries no request timeout by design, so a summon can
/// sit silent for minutes waiting for a card. Without traffic in that gap the
/// browser and any proxy in between are entitled to call the connection dead, so
/// axum's keep-alive comment goes out well inside the usual 60 s idle cut-offs.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Five events at most, and the receiver is the socket. Two is plenty; the
/// slack only matters if two legs land in the same instant.
const CHANNEL: usize = 8;

/// What the caller's prompt templates get filled with once the sheet lands. A
/// template that contains neither is sent verbatim — the escape hatch for a
/// caller that wants an exact prompt.
const SUBJECT: &str = "{subject}";
const SOUND_DESC: &str = "{sound_desc}";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/summon", post(summon))
        .layer(DefaultBodyLimit::max(SUMMON_BODY_LIMIT))
}

// ── wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SummonRequest {
    /// What the player typed, verbatim. The sheet is made from this.
    pub text: String,
    /// Omitted → the sheet's `subject` is the whole prompt. Otherwise the same
    /// spec `/oneshot` takes, with `{subject}` substituted in `positive`.
    #[serde(default)]
    pub image: Option<ImageSpec>,
    /// Omitted → the sheet's `sound_desc` is the whole prompt. A sheet whose
    /// `sound_desc` is empty is a silent thing: no engine is touched and the
    /// `sound` event carries a null.
    #[serde(default)]
    pub sound: Option<SoundSpec>,
    /// On by default — the maps are a stage of a summon, not an extra. `false`
    /// skips the pass and its event entirely.
    #[serde(default = "enabled")]
    pub depth: bool,
    /// On by default. The phrases come from the sheet's own moving parts, so a
    /// summon with no sheet and one with nothing that moves both skip the stage.
    #[serde(default = "enabled")]
    pub parts: bool,
}

fn enabled() -> bool {
    true
}

/// The event names, and the `stage` an `error` event names. One `name`, used for
/// both, so an error can never disagree with the event it is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Sheet,
    Image,
    Sound,
    Depth,
    Parts,
    Ready,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Sheet => "sheet",
            Stage::Image => "image",
            Stage::Sound => "sound",
            Stage::Depth => "depth",
            Stage::Parts => "parts",
            Stage::Ready => "ready",
        }
    }
}

impl Serialize for Stage {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

/// Its own event rather than a nullable field on each stage: a client listening
/// for `image` should only ever be handed an image.
const ERROR_EVENT: &str = "error";

#[derive(Debug, Serialize)]
pub struct StageError {
    pub stage: Stage,
    pub error: String,
}

/// Base64 PNG **with an alpha channel**, background removed — `/oneshot`'s
/// `image_b64` under the same name. Borrowed: the payload is megabytes and the
/// event is built from the render's own buffer.
#[derive(Debug, Serialize)]
pub struct ImageStage<'a> {
    pub image_b64: &'a str,
    pub width: u32,
    pub height: u32,
    pub gate_wait_ms: u64,
    pub render_ms: u64,
}

/// A null `sound_b64` on a successful stage is a deliberately silent thing, not
/// a failure — a failure is an `error` event. Same distinction `/oneshot` draws
/// between a null `sound_b64` with and without a `sound_error`.
#[derive(Debug, Serialize)]
pub struct SoundStage<'a> {
    pub sound_b64: Option<&'a str>,
    pub gate_wait_ms: u64,
    pub render_ms: u64,
}

/// The `?inline=true` shape of `/api/v1/depth`, for the one image this summon
/// rendered: the 16-bit depth PNG plus the min/max that denormalise it back to
/// raw units via `min + (px / 65535) * (max - min)`, and a confidence PNG only
/// if the model had one — the default, V2, does not.
#[derive(Debug, Serialize)]
pub struct DepthStage {
    #[serde(flatten)]
    pub maps: DepthResultOut,
    pub render_ms: u64,
}

/// The instance masks for the sheet's moving parts, over the same pixels the
/// `image` event carried. `name` is the sheet's own part name, not the phrase
/// that found it, so the consumer pairs masks onto parts without singularising
/// anything itself.
///
/// The frame leads, as in the `depth` stage: the consumer registers each mask
/// against its own alpha crop PROPORTIONALLY, which it cannot do without the
/// resolution the masks are at — and that need not be the sprite's own.
#[derive(Debug, Serialize)]
pub struct PartsStage {
    pub width: u32,
    pub height: u32,
    pub parts: Vec<PartOut>,
    pub render_ms: u64,
}

/// Terminal. Whatever the client missed, it can reconcile from here.
#[derive(Debug, Serialize)]
pub struct ReadyStage {
    pub sheet_ok: bool,
    pub image_ok: bool,
    /// True for a deliberately silent thing as well: nothing failed.
    pub sound_ok: bool,
    /// False when the pass was skipped as well as when it failed.
    pub depth_ok: bool,
    /// False when the pass was skipped as well as when it failed.
    pub parts_ok: bool,
    pub render_ms: u64,
    /// The longest single gate wait, not the sum — the legs are concurrent.
    pub gate_wait_ms: u64,
}

// ── handler ──────────────────────────────────────────────────────────────────

/// Everything that can reject a request before an engine is touched, minus auth
/// (which needs the db). Split out so the non-2xx branches are testable without
/// an [`AppState`].
fn preflight(body: &[u8]) -> Result<SummonRequest, (StatusCode, String)> {
    let value = serde_json::from_slice(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid request JSON: {e}"),
        )
    })?;
    let req: SummonRequest = crate::strict_json::from_value_strict(value, "summon")
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if req.text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text is required".to_string()));
    }
    if let Some(i) = &req.image
        && i.positive.trim().is_empty()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "image.positive must not be empty — omit `image` to render the sheet's subject on its own, or send a template containing {SUBJECT}"
            ),
        ));
    }
    Ok(req)
}

async fn summon(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(resp) = depth::auth_gate(&state, &headers).await {
        return resp;
    }
    let req = match preflight(&body) {
        Ok(r) => r,
        Err((status, msg)) => return err(status, &msg),
    };
    if let Err((status, msg)) = oneshot::image_engines_ready(&state) {
        return err(status, &msg);
    }

    let caller = extract_caller(&headers);
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(CHANNEL);
    tokio::spawn(run(state, req, caller, Emitter(tx)));

    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
        .into_response()
}

/// The stream, from the caller's side. Every `send` reports whether the client
/// is still attached, so a run whose listener has gone stops rather than
/// finishing a render nobody will read.
struct Emitter(mpsc::Sender<Result<Event, Infallible>>);

impl Emitter {
    async fn emit(&self, event: &str, data: &impl Serialize) -> bool {
        match Event::default().event(event).json_data(data) {
            Ok(e) => self.0.send(Ok(e)).await.is_ok(),
            Err(e) => {
                // Only reachable if one of this module's own payloads stopped
                // being serialisable, which is a build-time fact, not a runtime
                // one. Drop the event rather than the run.
                warn!(event, error = %e, "summon: unserialisable stage payload");
                true
            }
        }
    }

    async fn stage(&self, stage: Stage, data: &impl Serialize) -> bool {
        self.emit(stage.name(), data).await
    }

    async fn fail(&self, stage: Stage, error: String) -> bool {
        warn!(stage = stage.name(), %error, "summon stage failed");
        self.emit(ERROR_EVENT, &StageError { stage, error }).await
    }
}

async fn run(state: AppState, req: SummonRequest, caller: Option<String>, emit: Emitter) {
    let started = Instant::now();
    let text = req.text.trim().to_string();

    let sheet = character_sheet::sheet_for(&state, &text, caller.clone()).await;
    // A missing sheet costs the game its size and its name, and it must not also
    // cost it the summon: the player's own line stands in as the subject so the
    // render still happens. The caller has its own fallback sheet for the rest.
    let (subject, sound_desc) = match &sheet {
        Ok(s) => (s.subject.as_str(), s.sound_desc.as_str()),
        Err(_) => (text.as_str(), text.as_str()),
    };
    let image_spec = image_spec(req.image.as_ref(), subject);
    let sound_spec = sound_spec(req.sound.as_ref(), sound_desc);

    let alive = match &sheet {
        Ok(s) => emit.stage(Stage::Sheet, s).await,
        Err(e) => emit.fail(Stage::Sheet, e.clone()).await,
    };
    if !alive {
        return;
    }

    let plan = match (req.parts, sheet.as_ref()) {
        (true, Ok(s)) => parts_plan(s),
        _ => None,
    };

    let (image, sound) = tokio::join!(
        image_leg(&state, &emit, &image_spec, req.depth, plan.as_ref()),
        sound_leg(&state, &emit, sound_spec.as_ref()),
    );
    let ImageLeg {
        image,
        depth_ok,
        depth_error,
        parts_ok,
        parts_error,
    } = image;

    let render_ms = started.elapsed().as_millis() as u64;
    let gate_wait_ms = image.gate_wait_ms.max(sound.gate_wait_ms);

    emit.stage(
        Stage::Ready,
        &ReadyStage {
            sheet_ok: sheet.is_ok(),
            image_ok: image.b64.is_some(),
            sound_ok: sound.error.is_none(),
            depth_ok,
            parts_ok,
            render_ms,
            gate_wait_ms,
        },
    )
    .await;

    log_async(
        state.db.clone(),
        CreateLog {
            service: SERVICE.into(),
            endpoint: ENDPOINT.into(),
            request_meta: json!({
                "text": text,
                "prompt": image_spec.positive,
                "sound_prompt": sound_spec.as_ref().map(|s| &s.prompt),
                "depth": req.depth,
                "parts": req.parts,
                "width": image.width,
                "height": image.height,
            }),
            response_meta: json!({
                "sheet_ok": sheet.is_ok(),
                "sheet_error": sheet.err(),
                "image_ok": image.b64.is_some(),
                "image_error": image.error,
                "sound_ok": sound.b64.is_some(),
                "sound_error": sound.error,
                "depth_ok": depth_ok,
                "depth_error": depth_error,
                "parts_ok": parts_ok,
                "parts_error": parts_error,
                "matting_backend": image.matting_backend,
            }),
            sample_id: None,
            gate_wait_ms: gate_wait_ms as i64,
            duration_ms: render_ms as i64,
            // Same call as `/oneshot`: the image is what makes a summon worth
            // anything, and a lost sound is still a usable one.
            status: if image.b64.is_some() { 200 } else { 502 },
            caller,
        },
    );
}

struct ImageLeg {
    image: oneshot::ImageOutcome,
    /// Distinct from `depth_error.is_none()`: a pass that was never asked for,
    /// or was abandoned because the listener went away, did not succeed either.
    depth_ok: bool,
    depth_error: Option<String>,
    parts_ok: bool,
    parts_error: Option<String>,
}

/// krea2 → matting → the `image` event → depth, then parts. Both passes ride here
/// rather than in `run` so they start the instant the image lands, while the
/// sound is still rendering.
///
/// SEQUENTIAL, not concurrent: `/depth` and `/parts` are two endpoints on the
/// same vision-server worker sharing one gate budget line, so submitting them
/// together only thrashes it.
async fn image_leg(
    state: &AppState,
    emit: &Emitter,
    spec: &ImageSpec,
    want_depth: bool,
    plan: Option<&PartsPlan>,
) -> ImageLeg {
    let started = Instant::now();
    let out = oneshot::render_image(state, spec).await;
    let render_ms = started.elapsed().as_millis() as u64;

    let mut alive = match &out.b64 {
        Some(image_b64) => {
            emit.stage(
                Stage::Image,
                &ImageStage {
                    image_b64,
                    width: out.width,
                    height: out.height,
                    gate_wait_ms: out.gate_wait_ms,
                    render_ms,
                },
            )
            .await
        }
        None => {
            let e = out
                .error
                .clone()
                .unwrap_or_else(|| "krea2 produced no image".to_string());
            emit.fail(Stage::Image, e).await
        }
    };

    let mut leg = ImageLeg {
        image: out,
        depth_ok: false,
        depth_error: None,
        parts_ok: false,
        parts_error: None,
    };
    if !alive || (!want_depth && plan.is_none()) {
        return leg;
    }
    // Both passes run on the CUT-OUT, not krea2's raw render: it is the image the
    // caller was just handed, so the maps and the masks index the same pixels the
    // sprite is drawn from. Decoded once, for both.
    let png = leg.image.b64.as_deref().map(decode_png);

    if want_depth {
        let (error, still) = depth_pass(state, emit, png.as_ref()).await;
        leg.depth_ok = error.is_none();
        leg.depth_error = error;
        alive = still;
    }
    if let (true, Some(plan)) = (alive, plan) {
        let (error, _) = parts_pass(state, emit, png.as_ref(), plan).await;
        leg.parts_ok = error.is_none();
        leg.parts_error = error;
    }
    leg
}

/// `None` = the render produced nothing; `Some(Err)` = it produced something that
/// is not base64. Either way the pass has no pixels and reports itself failed.
type Sprite<'a> = Option<&'a Result<Bytes, String>>;

async fn depth_pass(state: &AppState, emit: &Emitter, png: Sprite<'_>) -> (Option<String>, bool) {
    let started = Instant::now();
    let outcome = match png {
        None => Err("no image to estimate depth from".to_string()),
        Some(Err(e)) => Err(e.clone()),
        Some(Ok(bytes)) => depth::depth_inline_one(state, "summon.png", bytes.clone()).await,
    };
    match outcome {
        Ok(maps) => {
            let alive = emit
                .stage(
                    Stage::Depth,
                    &DepthStage {
                        maps,
                        render_ms: started.elapsed().as_millis() as u64,
                    },
                )
                .await;
            (None, alive)
        }
        Err(e) => {
            let alive = emit.fail(Stage::Depth, e.clone()).await;
            (Some(e), alive)
        }
    }
}

async fn parts_pass(
    state: &AppState,
    emit: &Emitter,
    png: Sprite<'_>,
    plan: &PartsPlan,
) -> (Option<String>, bool) {
    let started = Instant::now();
    let outcome = match png {
        None => Err("no image to locate parts in".to_string()),
        Some(Err(e)) => Err(e.clone()),
        Some(Ok(bytes)) => {
            parts::parts_inline_one(
                state,
                "summon.png",
                bytes.clone(),
                &plan.prompts(),
                Some(plan.max_instances),
            )
            .await
        }
    };
    match outcome {
        Ok(located) => {
            let alive = emit
                .stage(
                    Stage::Parts,
                    &PartsStage {
                        width: located.width,
                        height: located.height,
                        parts: plan.rewrite(located.parts),
                        render_ms: started.elapsed().as_millis() as u64,
                    },
                )
                .await;
            (None, alive)
        }
        Err(e) => {
            let alive = emit.fail(Stage::Parts, e.clone()).await;
            (Some(e), alive)
        }
    }
}

/// sa3 → the `sound` event. A thing the sheet says makes no noise never reaches
/// an engine and is reported as a success carrying a null.
async fn sound_leg(
    state: &AppState,
    emit: &Emitter,
    spec: Option<&SoundSpec>,
) -> oneshot::SoundOutcome {
    let started = Instant::now();
    let out = oneshot::render_sound(state, spec).await;
    let render_ms = started.elapsed().as_millis() as u64;

    match &out.error {
        None => {
            emit.stage(
                Stage::Sound,
                &SoundStage {
                    sound_b64: out.b64.as_deref(),
                    gate_wait_ms: out.gate_wait_ms,
                    render_ms,
                },
            )
            .await;
        }
        Some(e) => {
            emit.fail(Stage::Sound, e.clone()).await;
        }
    }
    out
}

fn decode_png(b64: &str) -> Result<Bytes, String> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map(Bytes::from)
        .map_err(|e| format!("rendered sprite was not base64: {e}"))
}

// ── prompt templates ─────────────────────────────────────────────────────────

fn image_spec(spec: Option<&ImageSpec>, subject: &str) -> ImageSpec {
    let template = spec.map_or(SUBJECT, |s| s.positive.as_str());
    ImageSpec {
        positive: template.replace(SUBJECT, subject.trim()).trim().to_string(),
        negative: spec.and_then(|s| s.negative.clone()),
        width: spec.and_then(|s| s.width),
        height: spec.and_then(|s| s.height),
        seed: spec.and_then(|s| s.seed),
    }
}

/// `None` = silence, which is a valid answer rather than a failure: either the
/// sheet named no noise, or filling the template left nothing to render.
fn sound_spec(spec: Option<&SoundSpec>, sound_desc: &str) -> Option<SoundSpec> {
    let template = spec.map_or(SOUND_DESC, |s| s.prompt.as_str());
    // A template that never mentions the sheet is an explicit prompt and stands
    // whatever the sheet said; one that does is silent when the sheet is.
    if template.contains(SOUND_DESC) && sound_desc.trim().is_empty() {
        return None;
    }
    let prompt = template.replace(SOUND_DESC, sound_desc.trim());
    let prompt = prompt.trim();
    (!prompt.is_empty()).then(|| SoundSpec {
        prompt: prompt.to_string(),
        seconds: spec.and_then(|s| s.seconds),
        seed: spec.and_then(|s| s.seed),
    })
}

// ── parts prompts ────────────────────────────────────────────────────────────

/// Per-phrase cap. The sheet's own count is the filter that works, but a sheet
/// that claims twenty legs would spend the whole decode budget on one phrase.
const MAX_INSTANCES_CAP: i32 = 6;

/// What to ask SAM 3 for, and how to read the answer back. Each pair is the
/// phrase actually sent alongside the sheet part it came from.
struct PartsPlan {
    pairs: Vec<(String, String)>,
    max_instances: u32,
}

impl PartsPlan {
    fn prompts(&self) -> Vec<String> {
        self.pairs.iter().map(|(p, _)| p.clone()).collect()
    }

    /// Back to the sheet's own spelling: the consumer pairs masks onto the
    /// sheet's parts by name and must not have to singularise to do it. Two sheet
    /// parts that singularise to one phrase were sent once, so their masks all go
    /// to the first of them — a degenerate sheet, and the alternative is dropping
    /// the masks entirely.
    fn rewrite(&self, mut located: Vec<PartOut>) -> Vec<PartOut> {
        for part in &mut located {
            if let Some((_, sheet_name)) = self.pairs.iter().find(|(p, _)| *p == part.name) {
                part.name = sheet_name.clone();
            }
        }
        located
    }
}

/// The phrases for one sheet, or `None` when there is nothing that moves. Only
/// moving parts are asked for — nothing that stays still needs a mask.
fn parts_plan(sheet: &CharacterSheet) -> Option<PartsPlan> {
    let moving: Vec<&character_sheet::Part> = sheet
        .parts
        .iter()
        .filter(|p| p.motion != Motion::None)
        .collect();

    let mut pairs: Vec<(String, String)> = Vec::new();
    for p in &moving {
        let prompt = singularise(&p.name);
        if prompt.is_empty() || pairs.iter().any(|(q, _)| *q == prompt) {
            continue;
        }
        pairs.push((prompt, p.name.clone()));
    }
    if pairs.is_empty() {
        return None;
    }
    let max_instances = moving
        .iter()
        .map(|p| p.count)
        .max()
        .unwrap_or(0)
        .clamp(0, MAX_INSTANCES_CAP) as u32;
    Some(PartsPlan {
        pairs,
        max_instances,
    })
}

/// MEASURED against the live vision-server on `real-knight.png`: `arm,leg,head`
/// returns 2 arms (0.60, 0.46), 2 legs (0.79, 0.79) and a head (0.89), while the
/// plural `arms,legs,head` returns NO arms at all and the legs at a materially
/// lower 0.68/0.67. The sheet's own prompt template shows the model emitting
/// plurals (`{"name":"wings",...}`), so this is the common case, not a corner.
///
/// The last word only — "front legs" is a leg, not a "front leg" pluralised.
fn singularise(name: &str) -> String {
    let trimmed = name.trim();
    let Some(last) = trimmed.split_whitespace().next_back() else {
        return String::new();
    };
    let head = &trimmed[..trimmed.len() - last.len()];
    format!("{head}{}", singular_word(last))
}

fn singular_word(w: &str) -> String {
    let lower = w.to_ascii_lowercase();
    if lower.ends_with("ies") && w.len() > 4 {
        return format!("{}y", &w[..w.len() - 3]);
    }
    for suffix in ["sses", "shes", "ches", "xes"] {
        if lower.ends_with(suffix) {
            return w[..w.len() - 2].to_string();
        }
    }
    if lower.ends_with('s') && !lower.ends_with("ss") && w.len() > 3 {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse(v: Value) -> Result<SummonRequest, (StatusCode, String)> {
        preflight(v.to_string().as_bytes())
    }

    /// The minimum body: one line of text. Everything else is the route's
    /// business, and depth and parts are on unless they are turned off.
    #[test]
    fn text_is_the_only_required_field_and_depth_defaults_on() {
        let req = parse(json!({ "text": "a heron" })).unwrap();
        assert_eq!(req.text, "a heron");
        assert!(req.image.is_none());
        assert!(req.sound.is_none());
        assert!(req.depth);
        assert!(req.parts);

        let req = parse(json!({ "text": "a heron", "depth": false })).unwrap();
        assert!(!req.depth);
        assert!(req.parts);

        let req = parse(json!({ "text": "a heron", "parts": false })).unwrap();
        assert!(!req.parts);
        assert!(req.depth);

        let req = parse(json!({
            "text": "a heron",
            "image": { "positive": "{subject}, centred", "negative": "blurry", "width": 768 },
            "sound": { "prompt": "{sound_desc}, dry", "seconds": 2.0 },
        }))
        .unwrap();
        assert_eq!(req.image.as_ref().unwrap().width, Some(768));
        assert_eq!(req.sound.as_ref().unwrap().seconds, Some(2.0));
    }

    /// A typo in a template is a prompt the caller never sent and a summon that
    /// silently isn't the one it asked for, so unknown fields are a 400 at any
    /// depth rather than a dropped key.
    #[test]
    fn malformed_requests_are_rejected_before_any_engine_is_touched() {
        let cases = [
            ("not json", json!("{not json").to_string()),
            ("empty body", json!({}).to_string()),
            ("blank text", json!({ "text": "  " }).to_string()),
            (
                "blank positive",
                json!({ "text": "a heron", "image": { "positive": " " } }).to_string(),
            ),
            (
                "unknown top-level field",
                json!({ "text": "a heron", "prompt": "a goose" }).to_string(),
            ),
            (
                "unknown nested field",
                json!({ "text": "a heron", "image": { "positive": "x", "positve": "y" } })
                    .to_string(),
            ),
        ];
        for (label, body) in cases {
            let (status, msg) = preflight(body.as_bytes()).expect_err(label);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
            assert!(!msg.is_empty(), "{label}");
        }
        assert!(preflight(b"{not json").is_err());
    }

    /// The event names are a contract with a separate repo, so they are stated
    /// once and the `stage` an error names is the same string as the event it is
    /// about.
    #[test]
    fn stage_names_are_the_event_names() {
        for (stage, name) in [
            (Stage::Sheet, "sheet"),
            (Stage::Image, "image"),
            (Stage::Sound, "sound"),
            (Stage::Depth, "depth"),
            (Stage::Parts, "parts"),
            (Stage::Ready, "ready"),
        ] {
            assert_eq!(stage.name(), name);
            assert_eq!(serde_json::to_value(stage).unwrap(), json!(name));
        }
    }

    /// The bytes paperworld's SSE reader actually sees: a named event per stage,
    /// one JSON object per event. A failed stage is a frame among the others and
    /// does not end the stream — everything after it still arrives.
    #[tokio::test]
    async fn each_stage_is_a_named_frame_and_a_failure_is_just_one_of_them() {
        let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(CHANNEL);
        let emit = Emitter(tx);
        emit.fail(Stage::Sound, "sa3: connection refused".to_string())
            .await;
        emit.stage(
            Stage::Ready,
            &ReadyStage {
                sheet_ok: true,
                image_ok: true,
                sound_ok: false,
                depth_ok: true,
                parts_ok: true,
                render_ms: 12_400,
                gate_wait_ms: 4200,
            },
        )
        .await;
        drop(emit);

        let response = Sse::new(ReceiverStream::new(rx)).into_response();
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/event-stream"
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let wire = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            wire.contains(
                "event: error\ndata: {\"stage\":\"sound\",\"error\":\"sa3: connection refused\"}"
            ),
            "{wire}"
        );
        assert!(
            wire.contains("event: ready\ndata: {\"sheet_ok\":true"),
            "{wire}"
        );
    }

    /// The template is the caller's wrapping and the sheet is what goes inside
    /// it. A template with no placeholder is an explicit prompt and survives
    /// untouched.
    #[test]
    fn the_sheet_fills_the_callers_template() {
        let spec = |positive: &str| ImageSpec {
            positive: positive.to_string(),
            negative: Some("blurry".to_string()),
            width: Some(768),
            height: None,
            seed: Some(7),
        };
        let cases = [
            (
                "wrapped",
                Some(spec("{subject}, centred. No shadow.")),
                "a grey heron, centred. No shadow.",
            ),
            (
                "twice",
                Some(spec("{subject} beside {subject}")),
                "a grey heron beside a grey heron",
            ),
            ("verbatim", Some(spec("an exact prompt")), "an exact prompt"),
            ("no spec", None, "a grey heron"),
        ];
        for (label, spec, expected) in cases {
            let out = image_spec(spec.as_ref(), "  a grey heron  ");
            assert_eq!(out.positive, expected, "{label}");
        }
        // The knobs are the caller's and pass straight through.
        let out = image_spec(Some(&spec("{subject}")), "a heron");
        assert_eq!(
            (out.width, out.height, out.seed),
            (Some(768), None, Some(7))
        );
        assert_eq!(out.negative.as_deref(), Some("blurry"));
    }

    /// A sheet that names no noise is a silent thing, not a failed one — and an
    /// explicit prompt is rendered whatever the sheet said.
    #[test]
    fn a_soundless_sheet_renders_no_sound_but_an_explicit_prompt_still_does() {
        let spec = |prompt: &str| SoundSpec {
            prompt: prompt.to_string(),
            seconds: Some(2.0),
            seed: None,
        };
        let cases: [(&str, Option<SoundSpec>, &str, Option<&str>); 5] = [
            (
                "wrapped",
                Some(spec("{sound_desc}, dry, mono")),
                "a harsh squawk",
                Some("a harsh squawk, dry, mono"),
            ),
            ("no spec", None, "a harsh squawk", Some("a harsh squawk")),
            (
                "silent sheet",
                Some(spec("{sound_desc}, dry, mono")),
                "  ",
                None,
            ),
            ("silent, no spec", None, "", None),
            (
                "explicit prompt",
                Some(spec("a door slam")),
                "",
                Some("a door slam"),
            ),
        ];
        for (label, spec, desc, expected) in cases {
            let out = sound_spec(spec.as_ref(), desc);
            assert_eq!(out.as_ref().map(|s| s.prompt.as_str()), expected, "{label}");
        }
        // The knobs are the caller's; `/oneshot`'s own defaults apply to a null.
        let out = sound_spec(Some(&spec("{sound_desc}")), "a squawk").unwrap();
        assert_eq!(out.seconds, Some(2.0));
        assert!(sound_spec(None, "a squawk").unwrap().seconds.is_none());
    }

    // ── parts prompts ────────────────────────────────────────────────────────

    use crate::routes::character_sheet::{
        CharacterSheet, Death, Emits, Kind, Locomotion, Part as SheetPart,
    };
    use crate::routes::parts::{PartMaskRef, PartOut};

    fn sheet(parts: Vec<(&str, Motion, i32)>) -> CharacterSheet {
        CharacterSheet {
            name: "Heron".to_string(),
            subject: "a grey heron".to_string(),
            sound_desc: "a squawk".to_string(),
            height_m: 1.0,
            speed_mps: 11.0,
            mass_kg: 1.5,
            hp: 8,
            strength: 3,
            kind: Kind::Fighter,
            locomotion: vec![Locomotion::Walk],
            roaming: true,
            skills: Vec::new(),
            parts: parts
                .into_iter()
                .map(|(name, motion, count)| SheetPart {
                    name: name.to_string(),
                    motion,
                    count,
                })
                .collect(),
            death: Death::None,
            explosion_radius_m: 0.0,
            mountable: false,
            buoyant: false,
            front_back_symmetric: true,
            emits: Emits::None,
        }
    }

    /// The plural is not a cosmetic difference: `arms` finds nothing where `arm`
    /// finds two. Only the last word is touched, so a multi-word part keeps its
    /// qualifier.
    #[test]
    fn only_the_last_word_is_singularised() {
        let cases: &[(&str, &str)] = &[
            ("arms", "arm"),
            ("legs", "leg"),
            ("wings", "wing"),
            ("head", "head"),
            ("wheels", "wheel"),
            ("eyes", "eye"),
            ("bodies", "body"),
            ("ties", "tie"),
            ("glasses", "glass"),
            ("brushes", "brush"),
            ("torches", "torch"),
            ("boxes", "box"),
            ("moss", "moss"),
            ("gas", "gas"),
            ("front legs", "front leg"),
            ("rear wheels", "rear wheel"),
            ("Arms", "Arm"),
            ("  arms  ", "arm"),
            ("", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(singularise(input), *expected, "input: {input:?}");
        }
    }

    /// Nothing that stays still needs a mask, two parts that singularise to one
    /// phrase are asked once, and the cap is the sheet's own largest count.
    #[test]
    fn the_plan_asks_once_per_moving_part_in_the_singular() {
        let plan = parts_plan(&sheet(vec![
            ("wings", Motion::Flap, 2),
            ("legs", Motion::Swing, 4),
            ("shell", Motion::None, 1),
        ]))
        .expect("a sheet with moving parts has a plan");
        assert_eq!(plan.prompts(), ["wing", "leg"]);
        assert_eq!(plan.max_instances, 4);

        // The still part's count never raises the cap, because it is never asked.
        let plan = parts_plan(&sheet(vec![
            ("wings", Motion::Flap, 2),
            ("scales", Motion::None, 400),
        ]))
        .unwrap();
        assert_eq!(plan.prompts(), ["wing"]);
        assert_eq!(plan.max_instances, 2);

        // A sheet that claims twenty legs would spend the decode budget on one
        // phrase.
        assert_eq!(
            parts_plan(&sheet(vec![("legs", Motion::Swing, 20)]))
                .unwrap()
                .max_instances,
            6
        );

        // Deduped: `arm` and `arms` are one phrase.
        let plan = parts_plan(&sheet(vec![
            ("arms", Motion::Swing, 2),
            ("arm", Motion::Swing, 1),
        ]))
        .unwrap();
        assert_eq!(plan.prompts(), ["arm"]);
    }

    /// Nothing to ask for is no stage at all, not an empty request that pays the
    /// whole image encode to find nothing.
    #[test]
    fn a_sheet_with_nothing_that_moves_has_no_plan() {
        assert!(parts_plan(&sheet(vec![])).is_none());
        assert!(parts_plan(&sheet(vec![("shell", Motion::None, 1)])).is_none());
        assert!(parts_plan(&sheet(vec![("   ", Motion::Spin, 1)])).is_none());
    }

    /// The consumer pairs masks onto the sheet's parts by name, so what comes
    /// back must be the sheet's own spelling rather than the phrase that was
    /// sent. An unrecognised name is left alone rather than dropped.
    #[test]
    fn located_masks_are_renamed_back_to_the_sheets_own_part_names() {
        let plan = parts_plan(&sheet(vec![
            ("wings", Motion::Flap, 2),
            ("front legs", Motion::Swing, 2),
        ]))
        .unwrap();
        assert_eq!(plan.prompts(), ["wing", "front leg"]);

        let located = ["wing", "wing", "front leg", "beak"]
            .into_iter()
            .map(|name| PartOut {
                name: name.to_string(),
                score: 0.6,
                bbox: [0.0, 0.0, 1.0, 1.0],
                area: 1,
                mask: PartMaskRef::Inline {
                    mask_png_base64: "AA==".to_string(),
                },
            })
            .collect();
        let names: Vec<String> = plan.rewrite(located).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["wings", "wings", "front legs", "beak"]);
    }
}
