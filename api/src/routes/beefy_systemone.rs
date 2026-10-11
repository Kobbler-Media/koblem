//! `POST /api/v1/beefy/systemone` — llama.cpp's TypeSafe-compatible System One API,
//! answered by Beefy behind the same gate as `/api/v1/beefy/chat/completions`.
//!
//! Native first: the body goes to `{beefy}/v1/systemone`. Until the engine has that
//! route it answers 404, which is remembered per URL for [`NATIVE_RECHECK`] while this
//! module emulates it: one canonical prompt per question on `/v1/completions` with
//! `max_tokens:1` and the top-20 logprobs, softmaxed over the label tokens.
//!
//! 🔴 The prompt and the maths are a shared contract with the engine's native route
//! (`CONTRACT-systemone.md`): change one side and the two paths stop agreeing on the
//! same request.
//!
//! The request is validated here BEFORE anything reaches Beefy, native or not — a
//! request tabby rejects can take the whole service down with it.

use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::beefy_proxy::{authenticate, hold, oai_error, upstream};
use crate::AppState;

const MAX_CHOICE_OPTIONS: usize = 26;
const SCORE_LEVELS: std::ops::RangeInclusive<usize> = 2..=10;
const TOP_LOGPROBS: u32 = 20;
/// Questions after the first run at most this many at once — exl3's `max_batch_size`.
const CONCURRENCY: usize = 2;
/// How long a 404 from the native route is believed before it is probed again. The
/// engine is about to grow the route, and a koblem restart must not be what notices.
const NATIVE_RECHECK: Duration = Duration::from_secs(60);

const SYSTEM_PROMPT: &str = "You are a decision model. Read the state and answer the question with the label of one option only.";

pub(super) async fn systemone(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    if let Err(resp) = authenticate(&state.db, state.api_key.as_deref(), &headers).await {
        return resp;
    }
    // Before `hold`: a bad request must neither wake the 27B nor reach tabby.
    let request = match parse_request(&body) {
        Ok(r) => r,
        Err(rejection) => return rejection.into_response(),
    };
    let (base, _gpu) = match hold(&state).await {
        Ok(admitted) => admitted,
        Err(resp) => return resp,
    };
    let (resp, path) = answer(&base, &body, &request, state.config.systemone_temperature).await;
    info!(
        questions = request.questions.len(),
        total_ms = started.elapsed().as_millis() as u64,
        path = path.as_str(),
        status = resp.status().as_u16(),
        "beefy systemone: request finished"
    );
    resp
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    Native,
    Emulated,
}

impl Path {
    fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Emulated => "emulated",
        }
    }
}

async fn answer(base: &str, body: &Bytes, request: &Request, temperature: f64) -> (Response, Path) {
    let base = base.trim_end_matches('/');
    if !native_known_missing(base) {
        match forward_native(base, body).await {
            Ok(Some(resp)) => return (resp, Path::Native),
            Ok(None) => mark_native_missing(base),
            Err(resp) => return (resp, Path::Native),
        }
    }
    let resp = match emulate(base, request, temperature).await {
        Ok(json) => json_response(StatusCode::OK, &json),
        Err(e) => {
            warn!(error = %e, "beefy systemone: emulation failed");
            oai_error(StatusCode::BAD_GATEWAY, "upstream_error", &e)
        }
    };
    (resp, Path::Emulated)
}

static NATIVE_MISSING: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn native_known_missing(base: &str) -> bool {
    let map = NATIVE_MISSING.lock().unwrap_or_else(|e| e.into_inner());
    map.get(base)
        .is_some_and(|since| since.elapsed() < NATIVE_RECHECK)
}

fn mark_native_missing(base: &str) {
    let mut map = NATIVE_MISSING.lock().unwrap_or_else(|e| e.into_inner());
    map.insert(base.to_string(), Instant::now());
}

/// `Ok(None)` is "the engine has no such route" — the cue to emulate.
async fn forward_native(base: &str, body: &Bytes) -> Result<Option<Response>, Response> {
    let resp = upstream()
        .post(format!("{base}/v1/systemone"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.clone())
        .send()
        .await
        .map_err(|e| unreachable_error(&e))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = resp.bytes().await.map_err(|e| unreachable_error(&e))?;
    Ok(Some(
        (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
    ))
}

fn unreachable_error(e: &reqwest::Error) -> Response {
    oai_error(
        StatusCode::BAD_GATEWAY,
        "upstream_unreachable",
        &format!("beefy did not answer: {e}"),
    )
}

fn json_response(status: StatusCode, body: &Json) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(e) => oai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &e.to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Emulation

struct Asked {
    top: Vec<(String, f64)>,
    prompt_tokens: Option<u64>,
    model: Option<String>,
}

async fn emulate(base: &str, request: &Request, temperature: f64) -> Result<Json, String> {
    let prompts: Vec<String> = request
        .questions
        .iter()
        .map(|q| build_prompt(&request.state, q))
        .collect();
    // Question 1 alone first: it writes the shared state prefix into the cache that
    // the others then reuse, instead of every one of them prefilling it at once.
    let mut asked = vec![ask(base, &prompts[0]).await?];
    // Owned prompts: a closure over `&String` trips the async-fn HRTB check on the
    // handler's `Send` bound.
    let rest: Vec<Result<Asked, String>> = futures::stream::iter(prompts.into_iter().skip(1))
        .map(|p| async move { ask(base, &p).await })
        .buffered(CONCURRENCY)
        .collect()
        .await;
    for r in rest {
        asked.push(r?);
    }

    let mut answers = Vec::with_capacity(asked.len());
    for (q, a) in request.questions.iter().zip(&asked) {
        let labels = q.labels();
        let p = label_probabilities(&a.top, &labels, temperature);
        answers.push((q.id.clone(), q.answer(&p)));
    }
    let model = asked
        .iter()
        .find_map(|a| a.model.clone())
        .map_or(Json::Null, Json::String);
    let mut usage = Vec::new();
    // All or nothing: a partial sum would be a wrong number rather than a missing one.
    if let Some(total) = asked.iter().map(|a| a.prompt_tokens).sum::<Option<u64>>() {
        usage.push(("input_tokens".to_string(), Json::Number(total.into())));
    }
    usage.push(("output_tokens".to_string(), Json::Number(0.into())));
    Ok(Json::Object(vec![
        ("model".to_string(), model),
        ("answers".to_string(), Json::Object(answers)),
        ("usage".to_string(), Json::Object(usage)),
    ]))
}

async fn ask(base: &str, prompt: &str) -> Result<Asked, String> {
    let (completion, encoded) = tokio::join!(complete(base, prompt), count_tokens(base, prompt));
    let completion = completion?;
    let top = completion
        .pointer("/choices/0/logprobs/top_logprobs/0")
        .and_then(Value::as_object)
        .ok_or("beefy's completion carried no top_logprobs")?
        .iter()
        .filter_map(|(tok, lp)| lp.as_f64().map(|lp| (tok.clone(), lp)))
        .collect::<Vec<_>>();
    if top.is_empty() {
        return Err("beefy's completion carried an empty top_logprobs".to_string());
    }
    let prompt_tokens = completion
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .or(encoded);
    let model = completion
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Asked {
        top,
        prompt_tokens,
        model,
    })
}

async fn complete(base: &str, prompt: &str) -> Result<Value, String> {
    // `add_bos_token:false`: the prompt carries its own chat tokens, and Qwen has no BOS.
    let body = json!({
        "prompt": prompt,
        "max_tokens": 1,
        "temperature": 0,
        "logprobs": TOP_LOGPROBS,
        "add_bos_token": false,
    });
    let resp = upstream()
        .post(format!("{base}/v1/completions"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("beefy did not answer: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("beefy's completion did not arrive: {e}"))?;
    if !status.is_success() {
        return Err(format!("beefy's /v1/completions answered {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("beefy's completion is not JSON: {e}"))
}

/// Tabby's completions answer `usage: null`, so the count comes from its tokenizer.
/// Best effort: `None` only drops `usage.input_tokens`.
async fn count_tokens(base: &str, prompt: &str) -> Option<u64> {
    let resp = upstream()
        .post(format!("{base}/v1/token/encode"))
        .json(&json!({ "text": prompt, "add_bos_token": false }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v.get("tokens")
        .and_then(Value::as_array)
        .map(|t| t.len() as u64)
}

// ---------------------------------------------------------------------------
// Probabilities

/// `softmax(logprob[label] / T)` over the labels. A label outside the top-K gets the
/// lowest returned logprob minus ln 10. Logprobs and logits differ by one constant
/// per position, so this is the contract's softmax over label logits.
pub fn label_probabilities(top: &[(String, f64)], labels: &[String], temperature: f64) -> Vec<f64> {
    let floor = top
        .iter()
        .map(|(_, lp)| *lp)
        .filter(|lp| lp.is_finite())
        .fold(f64::INFINITY, f64::min)
        - std::f64::consts::LN_10;
    let scaled: Vec<f64> = labels
        .iter()
        .map(|label| {
            let lp = top
                .iter()
                .find(|(tok, _)| tok == label)
                .map_or(floor, |(_, lp)| *lp);
            lp / temperature
        })
        .collect();
    let max = scaled
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .fold(f64::NEG_INFINITY, f64::max);
    if !max.is_finite() {
        return vec![1.0 / labels.len() as f64; labels.len()];
    }
    let exp: Vec<f64> = scaled.iter().map(|x| (x - max).exp()).collect();
    let sum: f64 = exp.iter().sum();
    exp.iter().map(|e| e / sum).collect()
}

/// `1 − H(p)/ln(n)`: 0 for uniform, 1 for one-hot.
pub fn confidence(p: &[f64]) -> f64 {
    if p.len() < 2 {
        return 1.0;
    }
    let entropy: f64 = p.iter().filter(|x| **x > 0.0).map(|x| -x * x.ln()).sum();
    (1.0 - entropy / (p.len() as f64).ln()).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Request

#[derive(Debug)]
struct Request {
    state: String,
    questions: Vec<Question>,
}

#[derive(Debug)]
struct Question {
    id: String,
    instructions: String,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    /// `(option, description)` in the request's key order.
    Choice(Vec<(String, Option<String>)>),
    Score(Vec<String>),
    Noul {
        yes: Option<String>,
        no: Option<String>,
    },
}

#[derive(Debug)]
enum Rejection {
    Invalid(String),
    Unsupported(String),
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        match self {
            Self::Invalid(m) => oai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &m),
            Self::Unsupported(m) => {
                oai_error(StatusCode::NOT_IMPLEMENTED, "not_supported_error", &m)
            }
        }
    }
}

fn invalid(msg: impl Into<String>) -> Rejection {
    Rejection::Invalid(msg.into())
}

fn parse_request(body: &[u8]) -> Result<Request, Rejection> {
    let root: Json =
        serde_json::from_slice(body).map_err(|e| invalid(format!("body is not JSON: {e}")))?;
    let Json::Object(fields) = root else {
        return Err(invalid("body must be a JSON object"));
    };
    let field = |k: &str| fields.iter().find(|(name, _)| name == k).map(|(_, v)| v);

    for key in ["files", "images"] {
        match field(key) {
            None | Some(Json::Null) => {}
            Some(Json::Array(items)) if items.is_empty() => {}
            Some(Json::Array(_)) => {
                return Err(Rejection::Unsupported(
                    "image input not supported yet".to_string(),
                ));
            }
            Some(_) => return Err(invalid(format!("`{key}` must be an array"))),
        }
    }

    let state = match field("state") {
        Some(v @ (Json::String(_) | Json::Object(_) | Json::Array(_))) => as_text(v),
        Some(_) => return Err(invalid("`state` must be a string, an object or an array")),
        None => return Err(invalid("`state` is required")),
    };

    let Some(Json::Object(raw_questions)) = field("questions") else {
        return Err(invalid(
            "`questions` must be an object of question id → question",
        ));
    };
    if raw_questions.is_empty() {
        return Err(invalid("`questions` is empty"));
    }
    let mut questions = Vec::with_capacity(raw_questions.len());
    for (id, raw) in raw_questions {
        if questions.iter().any(|q: &Question| &q.id == id) {
            return Err(invalid(format!("question id `{id}` appears twice")));
        }
        questions.push(parse_question(id, raw).map_err(|e| match e {
            Rejection::Invalid(m) => invalid(format!("question `{id}`: {m}")),
            other => other,
        })?);
    }
    Ok(Request { state, questions })
}

fn parse_question(id: &str, raw: &Json) -> Result<Question, Rejection> {
    let Json::Object(fields) = raw else {
        return Err(invalid("must be an object"));
    };
    let field = |k: &str| fields.iter().find(|(name, _)| name == k).map(|(_, v)| v);
    let instructions = match field("instructions") {
        Some(v @ (Json::String(_) | Json::Object(_) | Json::Array(_))) => as_text(v),
        _ => {
            return Err(invalid(
                "`instructions` must be a string, an object or an array",
            ));
        }
    };
    let criteria = field("criteria").filter(|v| !matches!(v, Json::Null));
    let kind = match field("type") {
        Some(Json::String(t)) if t == "choice" => {
            let Some(Json::Object(options)) = criteria else {
                return Err(invalid(
                    "a choice question needs `criteria`: an object of option → description",
                ));
            };
            if options.len() < 2 || options.len() > MAX_CHOICE_OPTIONS {
                return Err(invalid(format!(
                    "a choice question takes 2 to {MAX_CHOICE_OPTIONS} options, got {}",
                    options.len()
                )));
            }
            let mut parsed: Vec<(String, Option<String>)> = Vec::with_capacity(options.len());
            for (option, desc) in options {
                if parsed.iter().any(|(o, _)| o == option) {
                    return Err(invalid(format!("option `{option}` appears twice")));
                }
                parsed.push((option.clone(), description(desc, option)?));
            }
            Kind::Choice(parsed)
        }
        Some(Json::String(t)) if t == "score" => {
            let Some(Json::Array(levels)) = criteria else {
                return Err(invalid(
                    "a score question needs `criteria`: an array of level descriptions",
                ));
            };
            if !SCORE_LEVELS.contains(&levels.len()) {
                return Err(invalid(format!(
                    "a score question takes 2 to 10 levels, got {}",
                    levels.len()
                )));
            }
            let levels = levels
                .iter()
                .map(|l| match l {
                    Json::String(s) => Ok(s.clone()),
                    _ => Err(invalid("score levels must be strings")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Kind::Score(levels)
        }
        Some(Json::String(t)) if t == "noul" => {
            let (mut yes, mut no) = (None, None);
            match criteria {
                None => {}
                Some(Json::Object(pairs)) => {
                    for (k, v) in pairs {
                        match k.as_str() {
                            "true" => yes = description(v, k)?,
                            "false" => no = description(v, k)?,
                            _ => {
                                return Err(invalid(format!(
                                    "noul `criteria` takes only `true` and `false`, not `{k}`"
                                )));
                            }
                        }
                    }
                }
                Some(_) => return Err(invalid("noul `criteria` must be an object")),
            }
            Kind::Noul { yes, no }
        }
        _ => return Err(invalid("`type` must be one of choice, score, noul")),
    };
    Ok(Question {
        id: id.to_string(),
        instructions,
        kind,
    })
}

/// An empty description reads as none: `"A) billing: "` would only be noise.
fn description(v: &Json, option: &str) -> Result<Option<String>, Rejection> {
    match v {
        Json::Null => Ok(None),
        Json::String(s) if s.is_empty() => Ok(None),
        Json::String(s) => Ok(Some(s.clone())),
        _ => Err(invalid(format!(
            "the description of `{option}` must be a string or null"
        ))),
    }
}

/// A string as-is; anything else as compact JSON in the request's own key order.
fn as_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

impl Question {
    fn labels(&self) -> Vec<String> {
        match &self.kind {
            Kind::Choice(options) => (b'A'..)
                .take(options.len())
                .map(|c| (c as char).to_string())
                .collect(),
            Kind::Score(levels) => (0..levels.len()).map(|i| i.to_string()).collect(),
            Kind::Noul { .. } => vec!["A".to_string(), "B".to_string()],
        }
    }

    /// `(option, description)` per label, as the prompt lists them.
    fn options(&self) -> Vec<(String, Option<String>)> {
        match &self.kind {
            Kind::Choice(options) => options.clone(),
            Kind::Score(levels) => levels.iter().map(|l| (l.clone(), None)).collect(),
            Kind::Noul { yes, no } => vec![
                ("Yes".to_string(), yes.clone()),
                ("No".to_string(), no.clone()),
            ],
        }
    }

    fn answer(&self, p: &[f64]) -> Json {
        let ty = |t: &str| ("type".to_string(), Json::String(t.to_string()));
        match &self.kind {
            Kind::Choice(options) => {
                let best = argmax(p);
                Json::Object(vec![
                    ty("choice"),
                    ("choice".to_string(), Json::String(options[best].0.clone())),
                    (
                        "probabilities".to_string(),
                        Json::Object(
                            options
                                .iter()
                                .zip(p)
                                .map(|((o, _), p)| (o.clone(), num(*p)))
                                .collect(),
                        ),
                    ),
                    ("confidence".to_string(), num(confidence(p))),
                ])
            }
            Kind::Score(levels) => {
                let score: f64 = p.iter().enumerate().map(|(i, p)| i as f64 * p).sum();
                Json::Object(vec![
                    ty("score"),
                    ("score".to_string(), num(score)),
                    (
                        "legend".to_string(),
                        Json::Object(
                            levels
                                .iter()
                                .enumerate()
                                .map(|(i, l)| (i.to_string(), Json::String(l.clone())))
                                .collect(),
                        ),
                    ),
                    (
                        "probabilities".to_string(),
                        Json::Object(
                            p.iter()
                                .enumerate()
                                .map(|(i, p)| (i.to_string(), num(*p)))
                                .collect(),
                        ),
                    ),
                    ("confidence".to_string(), num(confidence(p))),
                ])
            }
            Kind::Noul { .. } => Json::Object(vec![ty("noul"), ("noul".to_string(), num(p[0]))]),
        }
    }
}

/// The first of equals wins, so a tie resolves to the earlier option.
fn argmax(p: &[f64]) -> usize {
    p.iter()
        .enumerate()
        .fold((0, f64::NEG_INFINITY), |(bi, bp), (i, &x)| {
            if x > bp { (i, x) } else { (bi, bp) }
        })
        .0
}

fn num(x: f64) -> Json {
    serde_json::Number::from_f64(x).map_or(Json::Null, Json::Number)
}

/// The canonical prompt, byte for byte. The state comes first so every question of a
/// request shares it as a cached prefix.
fn build_prompt(state: &str, q: &Question) -> String {
    let mut options = String::new();
    for (label, (option, desc)) in q.labels().iter().zip(q.options()) {
        options.push_str(label);
        options.push_str(") ");
        options.push_str(&option);
        if let Some(desc) = desc {
            options.push_str(": ");
            options.push_str(&desc);
        }
        options.push('\n');
    }
    format!(
        "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n<|im_start|>user\nState:\n{state}\n\nQuestion: {instructions}\nOptions:\n{options}Answer with the label only.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n",
        instructions = q.instructions,
    )
}

// ---------------------------------------------------------------------------
// Order-preserving JSON

/// `serde_json::Value` sorts object keys (koblem builds serde_json without
/// `preserve_order`), but option labels follow the request's key order, and a
/// non-string state goes into the prompt as compact JSON that the native route must
/// reproduce byte for byte. This keeps both in the order the caller wrote them.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => s.serialize_unit(),
            Self::Bool(b) => s.serialize_bool(*b),
            Self::Number(n) => n.serialize(s),
            Self::String(v) => s.serialize_str(v),
            Self::Array(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Object(pairs) => {
                let mut map = s.serialize_map(Some(pairs.len()))?;
                for (k, v) in pairs {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
        Json::deserialize(d)
    }
    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(v)
            .map(Json::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::String(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::String(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut pairs = Vec::new();
        while let Some((k, v)) = map.next_entry::<String, Json>()? {
            pairs.push((k, v));
        }
        Ok(Json::Object(pairs))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{Json as AxJson, Router, routing::post};

    use super::*;

    fn top(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(t, lp)| (t.to_string(), *lp)).collect()
    }

    fn labels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn probabilities_are_a_softmax_over_the_labels_only() {
        let p = label_probabilities(
            &top(&[("A", -0.1), ("billing", -2.0), ("B", -3.0), ("C", -4.0)]),
            &labels(&["A", "B", "C"]),
            1.0,
        );
        let z = (-0.1f64).exp() + (-3.0f64).exp() + (-4.0f64).exp();
        assert!(close(p[0], (-0.1f64).exp() / z));
        assert!(close(p[1], (-3.0f64).exp() / z));
        assert!(close(p.iter().sum::<f64>(), 1.0));
    }

    #[test]
    fn a_label_outside_top_k_sits_one_decade_below_the_lowest() {
        let p = label_probabilities(&top(&[("A", -1.0), ("x", -5.0)]), &labels(&["A", "B"]), 1.0);
        let missing = -5.0 - std::f64::consts::LN_10;
        let z = (-1.0f64).exp() + missing.exp();
        assert!(close(p[1], missing.exp() / z));
    }

    #[test]
    fn temperature_flattens_or_sharpens() {
        let t = top(&[("A", -0.5), ("B", -1.5)]);
        let l = labels(&["A", "B"]);
        let p1 = label_probabilities(&t, &l, 1.0);
        let p2 = label_probabilities(&t, &l, 2.0);
        let p05 = label_probabilities(&t, &l, 0.5);
        assert!(p2[0] < p1[0] && p1[0] < p05[0]);
        assert!(close(p2[0], 1.0 / (1.0 + (-0.5f64).exp())));
    }

    #[test]
    fn confidence_is_zero_uniform_and_one_one_hot() {
        assert!(close(confidence(&[0.25; 4]), 0.0));
        assert!(close(confidence(&[1.0, 0.0, 0.0]), 1.0));
        let c = confidence(&[0.9, 0.1]);
        let h = -(0.9f64 * 0.9f64.ln() + 0.1 * 0.1f64.ln());
        assert!(close(c, 1.0 - h / 2f64.ln()));
    }

    #[test]
    fn the_prompt_is_the_contract_byte_for_byte() {
        let req = parse_request(
            br#"{"state":{"b":1,"a":"\u00e9"},"questions":{"route":{"type":"choice","instructions":"Which team?","criteria":{"billing":"money","shipping":null}}}}"#,
        )
        .unwrap();
        assert_eq!(req.state, r#"{"b":1,"a":"é"}"#);
        assert_eq!(
            build_prompt(&req.state, &req.questions[0]),
            "<|im_start|>system\nYou are a decision model. Read the state and answer the question with the label of one option only.<|im_end|>\n<|im_start|>user\nState:\n{\"b\":1,\"a\":\"é\"}\n\nQuestion: Which team?\nOptions:\nA) billing: money\nB) shipping\nAnswer with the label only.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
    }

    #[test]
    fn score_and_noul_prompts_list_their_labels() {
        let req = parse_request(
            br#"{"state":"s","questions":{"u":{"type":"score","instructions":"How urgent?","criteria":["low","high"]},"n":{"type":"noul","instructions":["is","it"],"criteria":{"true":"it is"}}}}"#,
        )
        .unwrap();
        let score = build_prompt(&req.state, &req.questions[0]);
        assert!(
            score.contains("Options:\n0) low\n1) high\nAnswer"),
            "{score}"
        );
        let noul = build_prompt(&req.state, &req.questions[1]);
        assert!(
            noul.contains("Question: [\"is\",\"it\"]\nOptions:\nA) Yes: it is\nB) No\nAnswer"),
            "{noul}"
        );
    }

    fn status_of(body: &str) -> StatusCode {
        match parse_request(body.as_bytes()) {
            Ok(_) => StatusCode::OK,
            Err(r) => r.into_response().status(),
        }
    }

    #[test]
    fn invalid_requests_are_400_and_images_are_501() {
        for bad in [
            "not json",
            r#"{"questions":{"q":{"type":"noul","instructions":"x"}}}"#,
            r#"{"state":1,"questions":{"q":{"type":"noul","instructions":"x"}}}"#,
            r#"{"state":"s","questions":{}}"#,
            r#"{"state":"s","questions":{"q":{"type":"vote","instructions":"x"}}}"#,
            r#"{"state":"s","questions":{"q":{"type":"noul"}}}"#,
            r#"{"state":"s","questions":{"q":{"type":"choice","instructions":"x","criteria":{"only":null}}}}"#,
            r#"{"state":"s","questions":{"q":{"type":"score","instructions":"x","criteria":["a"]}}}"#,
            r#"{"state":"s","questions":{"q":{"type":"noul","instructions":"x","criteria":{"maybe":"?"}}}}"#,
            r#"{"state":"s","questions":{"q":{"type":"noul","instructions":"x"},"q":{"type":"noul","instructions":"y"}}}"#,
        ] {
            assert_eq!(status_of(bad), StatusCode::BAD_REQUEST, "{bad}");
        }
        let many: String = (0..27)
            .map(|i| format!("\"o{i}\":null"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            status_of(&format!(
                r#"{{"state":"s","questions":{{"q":{{"type":"choice","instructions":"x","criteria":{{{many}}}}}}}}}"#
            )),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                r#"{"state":"s","images":["data:image/png;base64,AA=="],"questions":{"q":{"type":"noul","instructions":"x"}}}"#
            ),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            status_of(
                r#"{"state":"s","files":[],"questions":{"q":{"type":"noul","instructions":"x"}}}"#
            ),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_request_without_a_bearer_token_is_401() {
        let db = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        let resp = authenticate(&db, Some("secret"), &HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[derive(Default)]
    struct Hits {
        native: AtomicUsize,
        completions: AtomicUsize,
        encodes: AtomicUsize,
    }

    /// Tabby as it is today: no `/v1/systemone`, `usage: null` on completions. The
    /// logprobs depend on which question the prompt asks.
    async fn fake_tabby(native: Option<Value>) -> (String, Arc<Hits>) {
        let hits = Arc::new(Hits::default());
        let (h1, h2, h3) = (hits.clone(), hits.clone(), hits.clone());
        let app = Router::new()
            .route(
                "/v1/systemone",
                post(move || {
                    let hits = h1.clone();
                    let native = native.clone();
                    async move {
                        hits.native.fetch_add(1, Ordering::Relaxed);
                        match native {
                            Some(v) => (StatusCode::OK, AxJson(v)),
                            None => (StatusCode::NOT_FOUND, AxJson(json!({"detail": "Not Found"}))),
                        }
                    }
                }),
            )
            .route(
                "/v1/completions",
                post(move |AxJson(body): AxJson<Value>| {
                    let hits = h2.clone();
                    async move {
                        hits.completions.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(body["max_tokens"], 1);
                        assert_eq!(body["add_bos_token"], false);
                        let prompt = body["prompt"].as_str().unwrap();
                        let top = if prompt.contains("Which team") {
                            json!({"A": -0.05, "billing": -3.0, "B": -4.0})
                        } else if prompt.contains("urgent") {
                            json!({"2": -0.4, "1": -1.4, "3": -2.4, "x": -6.0})
                        } else {
                            json!({"B": -0.2, "A": -1.8})
                        };
                        AxJson(json!({
                            "model": "Swift-test",
                            "choices": [{"text": "A", "logprobs": {"tokens": ["A"], "top_logprobs": [top]}}],
                            "usage": null
                        }))
                    }
                }),
            )
            .route(
                "/v1/token/encode",
                post(move || {
                    let hits = h3.clone();
                    async move {
                        hits.encodes.fetch_add(1, Ordering::Relaxed);
                        AxJson(json!({"tokens": [1, 2, 3, 4, 5], "length": 5}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    const EXAMPLE: &str = r#"{
        "state": "Customer message: I was charged twice.",
        "questions": {
            "route": {"type": "choice", "instructions": "Which team should handle this?",
                      "criteria": {"billing": null, "shipping": null, "technical": null}},
            "angry": {"type": "noul", "instructions": "Is the customer angry?"},
            "urgency": {"type": "score", "instructions": "How urgent is this?",
                        "criteria": ["can wait", "this week", "today", "right now"]}
        }
    }"#;

    async fn call(base: &str, body: &str) -> (StatusCode, Value, Path) {
        let bytes = Bytes::from(body.to_string());
        let req = parse_request(&bytes).unwrap();
        let (resp, path) = answer(base, &bytes, &req, 1.0).await;
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap(), path)
    }

    #[tokio::test]
    async fn emulates_when_the_engine_lacks_the_route_and_remembers_that() {
        let (base, hits) = fake_tabby(None).await;
        let (status, v, path) = call(&base, EXAMPLE).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(path, Path::Emulated);
        assert_eq!(v["model"], "Swift-test");
        assert_eq!(v["usage"]["input_tokens"], 15);
        assert_eq!(v["usage"]["output_tokens"], 0);

        let route = &v["answers"]["route"];
        assert_eq!(route["type"], "choice");
        assert_eq!(route["choice"], "billing");
        let p = &route["probabilities"];
        let missing = -4.0 - std::f64::consts::LN_10;
        let z = (-0.05f64).exp() + (-4.0f64).exp() + missing.exp();
        assert!(close(p["billing"].as_f64().unwrap(), (-0.05f64).exp() / z));
        assert!(close(p["technical"].as_f64().unwrap(), missing.exp() / z));

        let angry = &v["answers"]["angry"];
        assert_eq!(angry["type"], "noul");
        let z = (-0.2f64).exp() + (-1.8f64).exp();
        assert!(close(angry["noul"].as_f64().unwrap(), (-1.8f64).exp() / z));

        let urgency = &v["answers"]["urgency"];
        assert_eq!(urgency["legend"]["3"], "right now");
        let ps: Vec<f64> = (0..4)
            .map(|i| urgency["probabilities"][i.to_string()].as_f64().unwrap())
            .collect();
        let expected: f64 = ps.iter().enumerate().map(|(i, p)| i as f64 * p).sum();
        assert!(close(urgency["score"].as_f64().unwrap(), expected));
        assert!(ps[2] > ps[1] && ps[1] > ps[3]);

        assert_eq!(hits.native.load(Ordering::Relaxed), 1);
        assert_eq!(hits.completions.load(Ordering::Relaxed), 3);
        assert_eq!(hits.encodes.load(Ordering::Relaxed), 3);

        // Within the recheck window the 404 is believed: no second native probe.
        let (status, _, path) = call(&base, EXAMPLE).await;
        assert_eq!((status, path), (StatusCode::OK, Path::Emulated));
        assert_eq!(hits.native.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn the_native_route_is_relayed_when_the_engine_has_it() {
        let native = json!({"model": "native", "answers": {}, "usage": {"input_tokens": 1, "output_tokens": 0}});
        let (base, hits) = fake_tabby(Some(native.clone())).await;
        let (status, v, path) = call(&base, EXAMPLE).await;
        assert_eq!((status, path), (StatusCode::OK, Path::Native));
        assert_eq!(v, native);
        assert_eq!(hits.completions.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn option_order_survives_into_the_answer() {
        let req = parse_request(
            br#"{"state":"s","questions":{"q":{"type":"choice","instructions":"x","criteria":{"zeta":null,"alpha":null}}}}"#,
        )
        .unwrap();
        let wire = serde_json::to_string(&req.questions[0].answer(&[0.5, 0.5])).unwrap();
        assert!(
            wire.starts_with(
                r#"{"type":"choice","choice":"zeta","probabilities":{"zeta":0.5,"alpha":0.5}"#
            ),
            "{wire}"
        );
    }
}
