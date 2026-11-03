//! Client for the two EmbeddingGemma-2 llama-servers.
//!
//! - `embed-vision` (`vision_service_url`): images, and text when `embed-text`
//!   is down. GPU-gated as `vision`; the gate's card goes in `X-Worker-Gpu`,
//!   which places (or relocates) the server's worker.
//! - `embed-text` (`text_embed_service_url`): the always-on text instance. Its
//!   reservation is a standing one (`text_embed`), so requests never acquire.
//!
//! Both speak OpenAI `/v1/embeddings`. EG2 embeds exactly the string it is
//! given, so task prefixes are applied here ([`koblem_client::embed::eg2_text`])
//! and callers send plain text.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use kob_gpu_gate::{GpuLock, GpuVisionGuard, ServiceKind};
use koblem_client::embed::{
    EG2_MODEL, EmbedTask, EmbeddingInput, EmbeddingsRequest, EmbeddingsResponse, cosine, eg2_text,
    image_data_url,
};
use koblem_client::types::{ChunkOptions, TextEmbeddingChunk};
use serde::Deserialize;
use serde_json::json;

/// Headroom between "fits by byte count" and the server's `-ub`. Every token
/// covers at least one byte of the prompt, so a prompt of `n` bytes is at most
/// `n` tokens plus the specials the server adds (BOS/EOS).
const SPECIAL_TOKENS: usize = 4;
/// Prefix and text are tokenized apart; joined, a merge across the seam can
/// shift the count by a token.
const SEAM_TOKENS: usize = 2;
/// Inputs per upstream request on the text path. Bounds the body and lets one
/// huge document set stream through rather than build one giant JSON.
const TEXT_BATCH: usize = 32;
const PROMPT_CACHE_CAP: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("embedding HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("embedding service returned {0}: {1}")]
    Upstream(u16, String),
    #[error("embedding service not configured")]
    NotConfigured,
    #[error("the vision service is disabled, or Beefy holds the GPU")]
    GateRefused,
    #[error("input is {tokens} tokens; the limit is {max_tokens}")]
    TooLong {
        input_index: Option<usize>,
        tokens: usize,
        max_tokens: usize,
    },
    #[error("{0}")]
    BadRequest(String),
}

/// Who served a request and what admission cost. `guard` is `Some` only when
/// the gated `embed-vision` instance did the work; hold it until the response
/// has been read.
pub struct Served {
    pub guard: Option<GpuVisionGuard>,
    pub gate_wait: Duration,
}

impl Served {
    fn ungated() -> Self {
        Served {
            guard: None,
            gate_wait: Duration::ZERO,
        }
    }

    pub fn gate_wait_ms(&self) -> Option<u64> {
        self.guard
            .as_ref()
            .map(|_| self.gate_wait.as_millis() as u64)
    }
}

/// `embed_text_inputs` output: one vector per input, plus every chunk when
/// chunking was asked for.
pub struct TextEmbeddings {
    pub embeddings: Vec<Vec<f32>>,
    pub chunks: Option<Vec<TextEmbeddingChunk>>,
    pub served: Served,
}

#[derive(Debug, Default, Deserialize)]
pub struct HealthInfo {
    pub healthy: bool,
    pub model: Option<String>,
}

#[derive(Clone)]
pub struct VisionClient {
    /// No timeout: a gated request can queue behind a long render.
    http: reqwest::Client,
    /// Short connect timeout so a dead `embed-text` falls back promptly.
    text_http: reqwest::Client,
    base_url: Arc<RwLock<Option<String>>>,
    text_url: Arc<RwLock<Option<String>>>,
    text_max_tokens: usize,
    gate: GpuLock,
    prompt_cache: Arc<Mutex<PromptCache>>,
}

impl VisionClient {
    pub fn new(
        base_url: Option<String>,
        text_url: Option<String>,
        text_max_tokens: usize,
        gate: GpuLock,
    ) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        let text_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            text_http,
            base_url: Arc::new(RwLock::new(normalise(base_url))),
            text_url: Arc::new(RwLock::new(normalise(text_url))),
            text_max_tokens,
            gate,
            prompt_cache: Arc::new(Mutex::new(PromptCache::default())),
        }
    }

    /// Hot-swap the `embed-vision` URL (Settings save). Clears the prompt cache:
    /// a different server may serve a different model.
    pub fn set_base_url(&self, base_url: Option<String>) {
        *self.base_url.write().expect("vision base_url poisoned") = normalise(base_url);
        self.prompt_cache.lock().unwrap().clear();
    }

    /// Hot-swap the `embed-text` URL (Settings save).
    pub fn set_text_url(&self, text_url: Option<String>) {
        *self.text_url.write().expect("text_url poisoned") = normalise(text_url);
        self.prompt_cache.lock().unwrap().clear();
    }

    fn vision_base(&self) -> Option<String> {
        self.base_url
            .read()
            .expect("vision base_url poisoned")
            .clone()
    }

    fn text_base(&self) -> Option<String> {
        self.text_url.read().expect("text_url poisoned").clone()
    }

    /// `embed-vision`'s parent `/health` (no gate, never spawns the worker).
    pub async fn health_info(&self) -> Result<HealthInfo, Error> {
        let base = self.vision_base().ok_or(Error::NotConfigured)?;
        let resp = self.http.get(format!("{base}/health")).send().await?;
        if !resp.status().is_success() {
            return Ok(HealthInfo::default());
        }
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            model: Option<String>,
        }
        let raw: Raw = resp.json().await.unwrap_or(Raw { model: None });
        Ok(HealthInfo {
            healthy: true,
            model: raw.model,
        })
    }

    pub async fn health(&self) -> Result<bool, Error> {
        Ok(self.health_info().await?.healthy)
    }

    /// OpenAI `/v1/embeddings`, routed by content: anything with an image goes
    /// to `embed-vision` under the gate, text-only to `embed-text` with no gate
    /// (falling back to `embed-vision`). `model` is always [`EG2_MODEL`].
    pub async fn embeddings(
        &self,
        req: &EmbeddingsRequest,
    ) -> Result<(EmbeddingsResponse, Served), Error> {
        if req.input.is_empty() {
            return Err(Error::BadRequest("input must be non-empty".into()));
        }
        let (mut resp, served) = if req.input.iter().any(EmbeddingInput::has_image) {
            let (guard, resp, gate_wait) = self.post_vision(req).await?;
            (
                resp,
                Served {
                    guard: Some(guard),
                    gate_wait,
                },
            )
        } else {
            self.post_text(req).await?
        };
        if resp.data.len() != req.input.len() {
            return Err(Error::Upstream(
                502,
                format!(
                    "asked for {} embeddings, got {}",
                    req.input.len(),
                    resp.data.len()
                ),
            ));
        }
        resp.model = Some(EG2_MODEL.to_string());
        Ok((resp, served))
    }

    /// Images (filename, bytes, mime) → one vector each, in one gated request.
    pub async fn embed_images(
        &self,
        images: &[ImagePart],
    ) -> Result<(GpuVisionGuard, Vec<Vec<f32>>, Duration), Error> {
        let req = EmbeddingsRequest {
            input: images.iter().map(ImagePart::to_input).collect(),
            model: None,
        };
        let (guard, resp, gate_wait) = self.post_vision(&req).await?;
        let vectors = resp.into_vectors();
        if vectors.len() != images.len() {
            return Err(Error::Upstream(
                502,
                format!(
                    "asked for {} embeddings, got {}",
                    images.len(),
                    vectors.len()
                ),
            ));
        }
        Ok((guard, vectors, gate_wait))
    }

    /// Prompt vectors for classification: embedded as given ([`EmbedTask::Raw`])
    /// on the text path, served from an LRU cache keyed by `(model, prompt)`.
    pub async fn prompt_vectors(&self, prompts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        let mut out: Vec<Option<Vec<f32>>> = {
            let mut cache = self.prompt_cache.lock().unwrap();
            prompts.iter().map(|p| cache.get(p)).collect()
        };
        let missing: Vec<String> = prompts
            .iter()
            .zip(&out)
            .filter(|(_, v)| v.is_none())
            .map(|(p, _)| p.clone())
            .collect();
        if !missing.is_empty() {
            let (vectors, _served) = self.embed_strings(&missing).await?;
            let fresh: HashMap<&str, &Vec<f32>> =
                missing.iter().map(String::as_str).zip(&vectors).collect();
            let mut cache = self.prompt_cache.lock().unwrap();
            for (p, slot) in prompts.iter().zip(out.iter_mut()) {
                if slot.is_none()
                    && let Some(v) = fresh.get(p.as_str())
                {
                    cache.put(p.clone(), (*v).clone());
                    *slot = Some((*v).clone());
                }
            }
        }
        Ok(out.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// `/v1/text_embeddings`: apply the task prefix, refuse (or chunk) inputs
    /// over the text instance's batch, embed on the text path.
    pub async fn embed_text_inputs(
        &self,
        texts: &[String],
        task: EmbedTask,
        titles: &[Option<String>],
        chunk: Option<&ChunkOptions>,
    ) -> Result<TextEmbeddings, Error> {
        let title = |i: usize| titles.get(i).and_then(|t| t.as_deref());
        let max = self.text_max_tokens;
        let Some(chunk) = chunk else {
            let prompts: Vec<String> = texts
                .iter()
                .enumerate()
                .map(|(i, t)| eg2_text(task, title(i), t))
                .collect();
            for (i, p) in prompts.iter().enumerate() {
                if let Some(tokens) = self.overflow(p, max).await? {
                    return Err(Error::TooLong {
                        input_index: Some(i),
                        tokens,
                        max_tokens: max,
                    });
                }
            }
            let (embeddings, served) = self.embed_strings(&prompts).await?;
            return Ok(TextEmbeddings {
                embeddings,
                chunks: None,
                served,
            });
        };

        let limit = chunk
            .max_tokens
            .map(|m| (m as usize).min(max))
            .unwrap_or(max);
        let overlap = chunk.overlap_tokens as usize;
        let mut plan: Vec<(usize, usize, Span)> = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            let whole = eg2_text(task, title(i), text);
            if self.overflow(&whole, limit).await?.is_none() {
                plan.push((i, 0, Span::whole(text)));
                continue;
            }
            let prefix = self
                .count_tokens(&eg2_text(task, title(i), ""), true)
                .await?;
            let budget = limit.saturating_sub(prefix + SEAM_TOKENS);
            if budget <= overlap {
                return Err(Error::BadRequest(format!(
                    "chunk budget is {budget} text tokens after the {prefix}-token prefix; \
                     overlap_tokens ({overlap}) must be smaller"
                )));
            }
            let pieces = self.tokenize_pieces(text).await?;
            for (j, span) in chunk_spans(&pieces, budget, overlap)
                .into_iter()
                .enumerate()
            {
                plan.push((i, j, span));
            }
        }

        let prompts: Vec<String> = plan
            .iter()
            .map(|(i, _, span)| eg2_text(task, title(*i), &span.text))
            .collect();
        let (vectors, served) = self.embed_strings(&prompts).await?;
        let mut embeddings = vec![Vec::new(); texts.len()];
        let mut chunks = Vec::with_capacity(plan.len());
        for ((i, j, span), vector) in plan.into_iter().zip(vectors) {
            if j == 0 {
                embeddings[i] = vector.clone();
            }
            chunks.push(TextEmbeddingChunk {
                input_index: i,
                chunk_index: j,
                char_start: span.char_start,
                char_end: span.char_end,
                embedding: vector,
            });
        }
        Ok(TextEmbeddings {
            embeddings,
            chunks: Some(chunks),
            served,
        })
    }

    /// `Some(tokens)` when `prompt` exceeds `limit` tokens. Skips the tokenize
    /// round-trip when the byte count alone proves it fits.
    async fn overflow(&self, prompt: &str, limit: usize) -> Result<Option<usize>, Error> {
        if prompt.len() + SPECIAL_TOKENS <= limit {
            return Ok(None);
        }
        let tokens = self.count_tokens(prompt, true).await?;
        Ok((tokens > limit).then_some(tokens))
    }

    /// Embed literal strings on the text path, batched.
    async fn embed_strings(&self, strings: &[String]) -> Result<(Vec<Vec<f32>>, Served), Error> {
        let mut vectors = Vec::with_capacity(strings.len());
        let mut served = Served::ungated();
        for batch in strings.chunks(TEXT_BATCH) {
            let req = EmbeddingsRequest {
                input: batch.iter().cloned().map(EmbeddingInput::Text).collect(),
                model: None,
            };
            let (resp, s) = self.post_text(&req).await?;
            let got = resp.into_vectors();
            if got.len() != batch.len() {
                return Err(Error::Upstream(
                    502,
                    format!("asked for {} embeddings, got {}", batch.len(), got.len()),
                ));
            }
            vectors.extend(got);
            if s.guard.is_some() {
                served = s;
            }
        }
        Ok((vectors, served))
    }

    /// Text-only request: `embed-text` with no gate; on a connection failure or
    /// a 503 (unloaded by the gate, draining), `embed-vision` under the gate.
    async fn post_text(
        &self,
        req: &EmbeddingsRequest,
    ) -> Result<(EmbeddingsResponse, Served), Error> {
        if let Some(base) = self.text_base().filter(|_| self.text_instance_admitted()) {
            match self.send(&self.text_http, &base, req, None).await {
                Ok(resp) => return Ok((resp, Served::ungated())),
                Err(Error::Request(e)) if e.is_connect() || e.is_timeout() => {
                    tracing::warn!(error = %e, "embed-text unreachable; falling back to embed-vision");
                }
                Err(Error::Upstream(503, body)) => {
                    tracing::warn!(
                        body,
                        "embed-text answered 503; falling back to embed-vision"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        let (guard, resp, gate_wait) = self.post_vision(req).await?;
        Ok((
            resp,
            Served {
                guard: Some(guard),
                gate_wait,
            },
        ))
    }

    /// Whether a request may wake `embed-text`. Its worker reloads on any request,
    /// and nothing on that path asks the gate — so while the gate has it disabled,
    /// is unloading it for a heavy job, or Beefy holds the box without it, text
    /// goes to the gated `embed-vision` instead of pulling it back onto the card.
    fn text_instance_admitted(&self) -> bool {
        let kind = ServiceKind::TextEmbed;
        let box_locked = self.gate.exclusive().is_some_and(|keep| keep != kind)
            && !self.gate.exclusive_allow().contains(&kind);
        self.gate.is_service_enabled(kind) && !self.gate.recently_evicted(kind) && !box_locked
    }

    async fn post_vision(
        &self,
        req: &EmbeddingsRequest,
    ) -> Result<(GpuVisionGuard, EmbeddingsResponse, Duration), Error> {
        let base = self.vision_base().ok_or(Error::NotConfigured)?;
        let start = Instant::now();
        let guard = self
            .gate
            .acquire_for_vision()
            .await
            .ok_or(Error::GateRefused)?;
        let gate_wait = start.elapsed();
        let resp = self
            .send(&self.http, &base, req, guard.chosen_gpu())
            .await?;
        Ok((guard, resp, gate_wait))
    }

    async fn send(
        &self,
        http: &reqwest::Client,
        base: &str,
        req: &EmbeddingsRequest,
        gpu: Option<&str>,
    ) -> Result<EmbeddingsResponse, Error> {
        let mut call = http.post(format!("{base}/v1/embeddings")).json(req);
        if let Some(card) = gpu {
            call = call.header("X-Worker-Gpu", card);
        }
        let resp = call.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            if let Some(tokens) = too_large_tokens(&body) {
                return Err(Error::TooLong {
                    input_index: None,
                    tokens,
                    max_tokens: self.text_max_tokens,
                });
            }
            return Err(Error::Upstream(status, body));
        }
        Ok(resp.json().await?)
    }

    /// `POST /tokenize` on `embed-text`, else `embed-vision` (whose parent
    /// answers 503 rather than spawning a worker when it is unloaded).
    async fn tokenize(&self, body: serde_json::Value) -> Result<Bytes, Error> {
        let bases: Vec<String> = [self.text_base(), self.vision_base()]
            .into_iter()
            .flatten()
            .collect();
        if bases.is_empty() {
            return Err(Error::NotConfigured);
        }
        let mut last = Error::NotConfigured;
        for base in bases {
            let sent = self
                .text_http
                .post(format!("{base}/tokenize"))
                .json(&body)
                .send()
                .await;
            match sent {
                Ok(resp) if resp.status().is_success() => return Ok(resp.bytes().await?),
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    last = Error::Upstream(status, resp.text().await.unwrap_or_default());
                }
                Err(e) => last = Error::Request(e),
            }
        }
        Err(last)
    }

    async fn count_tokens(&self, content: &str, add_special: bool) -> Result<usize, Error> {
        #[derive(Deserialize)]
        struct Ids {
            tokens: Vec<serde_json::Value>,
        }
        let raw = self
            .tokenize(json!({"content": content, "add_special": add_special}))
            .await?;
        let ids: Ids = serde_json::from_slice(&raw)
            .map_err(|e| Error::Upstream(502, format!("bad /tokenize answer: {e}")))?;
        Ok(ids.tokens.len())
    }

    async fn tokenize_pieces(&self, content: &str) -> Result<Vec<Vec<u8>>, Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Piece {
            Text(String),
            Bytes(Vec<u8>),
        }
        #[derive(Deserialize)]
        struct Token {
            piece: Piece,
        }
        #[derive(Deserialize)]
        struct Pieces {
            tokens: Vec<Token>,
        }
        let raw = self
            .tokenize(json!({"content": content, "add_special": false, "with_pieces": true}))
            .await?;
        let parsed: Pieces = serde_json::from_slice(&raw)
            .map_err(|e| Error::Upstream(502, format!("bad /tokenize answer: {e}")))?;
        Ok(parsed
            .tokens
            .into_iter()
            .map(|t| match t.piece {
                Piece::Text(s) => s.into_bytes(),
                Piece::Bytes(b) => b,
            })
            .collect())
    }
}

/// An uploaded image on its way to a data URL.
pub struct ImagePart {
    pub filename: String,
    pub mime: Option<String>,
    pub bytes: Bytes,
}

impl ImagePart {
    pub fn to_input(&self) -> EmbeddingInput {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.bytes);
        EmbeddingInput::image(image_data_url(&self.mime(), &b64))
    }

    fn mime(&self) -> String {
        if let Some(m) = self.mime.as_deref().filter(|m| m.starts_with("image/")) {
            return m.to_string();
        }
        let ext = self
            .filename
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "bmp" => "image/bmp",
            _ => "image/png",
        }
        .to_string()
    }
}

/// `scores[image][prompt]` = cosine. Errors when the widths differ: a stored
/// SigLIP2 vector (1152-d) must be re-embedded, not scored.
pub fn cosine_matrix(images: &[Vec<f32>], prompts: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, Error> {
    let dim = prompts.first().map(Vec::len).unwrap_or(0);
    if let Some((i, row)) = images.iter().enumerate().find(|(_, r)| r.len() != dim) {
        return Err(Error::BadRequest(format!(
            "image embedding {i} has {} dims; {EG2_MODEL} vectors have {dim} — \
             re-embed vectors from an older model",
            row.len()
        )));
    }
    Ok(images
        .iter()
        .map(|img| prompts.iter().map(|p| cosine(img, p)).collect())
        .collect())
}

/// llama-server's over-`-ub` refusal: `input (N tokens) is too large to process`.
fn too_large_tokens(body: &str) -> Option<usize> {
    let at = body.find("tokens) is too large")?;
    let head = &body[..at];
    let open = head.rfind('(')?;
    head[open + 1..].trim().parse().ok()
}

fn normalise(url: Option<String>) -> Option<String> {
    url.map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// One chunk of an input: the text to embed and where it sits in the input.
#[derive(Debug, PartialEq)]
struct Span {
    text: String,
    char_start: usize,
    char_end: usize,
}

impl Span {
    fn whole(text: &str) -> Self {
        Span {
            text: text.to_string(),
            char_start: 0,
            char_end: text.chars().count(),
        }
    }
}

/// Windows of `budget` tokens, consecutive windows sharing `overlap`, over the
/// text the token pieces spell out. Offsets index that text in `chars()`; for
/// EG2's tokenizer it is the input byte-for-byte. Requires `overlap < budget`.
fn chunk_spans(pieces: &[Vec<u8>], budget: usize, overlap: usize) -> Vec<Span> {
    let bytes: Vec<u8> = pieces.concat();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut ends = Vec::with_capacity(pieces.len() + 1);
    ends.push(0usize);
    for p in pieces {
        ends.push(ends.last().unwrap() + p.len());
    }
    let n = pieces.len();
    let mut spans = Vec::new();
    let mut start = 0;
    loop {
        let end = (start + budget).min(n);
        let b0 = floor_boundary(&text, ends[start]);
        let b1 = ceil_boundary(&text, ends[end]);
        spans.push(Span {
            text: text[b0..b1].to_string(),
            char_start: text[..b0].chars().count(),
            char_end: text[..b1].chars().count(),
        });
        if end == n {
            break;
        }
        start = end - overlap;
    }
    spans
}

fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Bounded least-recently-used map. Eviction scans for the oldest tick, which
/// at this size costs less than the embedding it saves.
#[derive(Default)]
struct PromptCache {
    map: HashMap<(&'static str, String), (Vec<f32>, u64)>,
    tick: u64,
}

impl PromptCache {
    fn get(&mut self, prompt: &str) -> Option<Vec<f32>> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.map.get_mut(&(EG2_MODEL, prompt.to_string()))?;
        entry.1 = tick;
        Some(entry.0.clone())
    }

    fn put(&mut self, prompt: String, vector: Vec<f32>) {
        if self.map.len() >= PROMPT_CACHE_CAP
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        self.tick += 1;
        self.map.insert((EG2_MODEL, prompt), (vector, self.tick));
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, http::StatusCode, routing::post};
    use serde_json::Value;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// A llama-server stand-in: one vector per input whose first component is
    /// the input's index + `tag`, and the `X-Worker-Gpu` it was sent recorded.
    /// `/tokenize` counts whitespace-separated words, one token each, pieces
    /// included.
    type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

    async fn fake_llama(tag: f32, seen: Seen) -> String {
        let app = Router::new()
            .route(
                "/v1/embeddings",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        let gpu = headers
                            .get("x-worker-gpu")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        seen.lock().unwrap().push((gpu, body.clone()));
                        let n = body["input"].as_array().map(Vec::len).unwrap_or(0);
                        let data: Vec<Value> = (0..n)
                            .map(|i| json!({"index": i, "embedding": [tag + i as f32, 1.0]}))
                            .collect();
                        Json(json!({"object": "list", "data": data, "model": "eg2.gguf"}))
                    }
                }),
            )
            .route(
                "/tokenize",
                post(|Json(body): Json<Value>| async move {
                    let content = body["content"].as_str().unwrap_or("").to_string();
                    let words: Vec<String> =
                        content.split_inclusive(' ').map(str::to_string).collect();
                    let specials = if body["add_special"].as_bool().unwrap_or(false) {
                        2
                    } else {
                        0
                    };
                    if body["with_pieces"].as_bool().unwrap_or(false) {
                        let tokens: Vec<Value> = words
                            .iter()
                            .enumerate()
                            .map(|(i, w)| json!({"id": i, "piece": w}))
                            .collect();
                        Json(json!({ "tokens": tokens }))
                    } else {
                        Json(json!({ "tokens": vec![0; words.len() + specials] }))
                    }
                }),
            );
        serve(app).await
    }

    async fn down() -> String {
        serve(Router::new().route(
            "/v1/embeddings",
            post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "unloaded") }),
        ))
        .await
    }

    fn gate_with_vision_card() -> GpuLock {
        let g = GpuLock::new();
        g.set_cards(vec![kob_gpu_gate::Card {
            uuid: "GPU-3060".into(),
            total_mb: 12000,
            fast: false,
            enabled: true,
        }]);
        g.set_engine_config(
            kob_gpu_gate::ServiceKind::Vision,
            kob_gpu_gate::EngineDesc {
                peak_mb: 1300,
                home_gpu: "GPU-3060".into(),
                candidate_gpus: vec!["GPU-3060".into()],
                evictable_by: vec![],
                relocatable: false,
                rules_json: String::new(),
            },
        );
        g
    }

    #[tokio::test]
    async fn text_goes_to_the_text_instance_ungated() {
        let vision_seen = Arc::new(Mutex::new(Vec::new()));
        let text_seen = Arc::new(Mutex::new(Vec::new()));
        let client = VisionClient::new(
            Some(fake_llama(100.0, vision_seen.clone()).await),
            Some(fake_llama(0.0, text_seen.clone()).await),
            1024,
            gate_with_vision_card(),
        );
        let req = EmbeddingsRequest {
            input: vec![EmbeddingInput::Text("a".into())],
            model: None,
        };
        let (resp, served) = client.embeddings(&req).await.unwrap();
        assert!(served.guard.is_none(), "text must not hold the GPU");
        assert_eq!(resp.model.as_deref(), Some(EG2_MODEL));
        assert_eq!(text_seen.lock().unwrap().len(), 1);
        assert!(vision_seen.lock().unwrap().is_empty());
    }

    /// Disabled in the gate (or unloaded for a heavy job): text must not wake
    /// `embed-text` behind the gate's back.
    #[tokio::test]
    async fn a_disabled_text_instance_is_not_woken() {
        let vision_seen = Arc::new(Mutex::new(Vec::new()));
        let text_seen = Arc::new(Mutex::new(Vec::new()));
        let gate = gate_with_vision_card();
        gate.set_service_enabled(ServiceKind::TextEmbed, false)
            .await;
        let client = VisionClient::new(
            Some(fake_llama(100.0, vision_seen.clone()).await),
            Some(fake_llama(0.0, text_seen.clone()).await),
            1024,
            gate,
        );
        let (_, served) = client.embed_strings(&["q".into()]).await.unwrap();
        assert!(served.guard.is_some());
        assert!(text_seen.lock().unwrap().is_empty());
        assert_eq!(vision_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn images_go_to_vision_with_the_gate_card_in_the_header() {
        let vision_seen = Arc::new(Mutex::new(Vec::new()));
        let text_seen = Arc::new(Mutex::new(Vec::new()));
        let client = VisionClient::new(
            Some(fake_llama(100.0, vision_seen.clone()).await),
            Some(fake_llama(0.0, text_seen.clone()).await),
            1024,
            gate_with_vision_card(),
        );
        let images = [ImagePart {
            filename: "frame.jpg".into(),
            mime: None,
            bytes: Bytes::from_static(b"\xff\xd8"),
        }];
        let (_guard, vectors, _) = client.embed_images(&images).await.unwrap();
        assert_eq!(vectors, vec![vec![100.0, 1.0]]);
        let seen = vision_seen.lock().unwrap();
        assert_eq!(seen[0].0.as_deref(), Some("GPU-3060"));
        assert_eq!(
            seen[0].1["input"][0]["content"][0]["image_url"]["url"],
            "data:image/jpeg;base64,/9g="
        );
        assert!(text_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn text_falls_back_to_vision_when_the_text_instance_is_down() {
        let vision_seen = Arc::new(Mutex::new(Vec::new()));
        let client = VisionClient::new(
            Some(fake_llama(100.0, vision_seen.clone()).await),
            Some(down().await),
            1024,
            gate_with_vision_card(),
        );
        let (vectors, served) = client.embed_strings(&["q".into()]).await.unwrap();
        assert_eq!(vectors, vec![vec![100.0, 1.0]]);
        assert!(served.guard.is_some(), "the fallback is gated");
        assert_eq!(
            vision_seen.lock().unwrap()[0].0.as_deref(),
            Some("GPU-3060")
        );

        // Unreachable (nothing listening) falls back the same way.
        let client = VisionClient::new(
            Some(fake_llama(100.0, Arc::default()).await),
            Some("http://127.0.0.1:9".into()),
            1024,
            gate_with_vision_card(),
        );
        assert!(client.embed_strings(&["q".into()]).await.is_ok());
    }

    #[tokio::test]
    async fn task_prefixes_are_applied_and_over_length_is_413_shaped() {
        let text_seen = Arc::new(Mutex::new(Vec::new()));
        let client = VisionClient::new(
            None,
            Some(fake_llama(0.0, text_seen.clone()).await),
            8,
            GpuLock::new(),
        );
        let out = client
            .embed_text_inputs(&["heist".into()], EmbedTask::Query, &[], None)
            .await
            .unwrap();
        assert_eq!(out.embeddings.len(), 1);
        assert_eq!(
            text_seen.lock().unwrap()[0].1["input"][0],
            "task: search result | query: heist"
        );

        // "title: none | text: " is 4 words, + 6 words + 2 specials = 12 > 8.
        let long = "one two three four five six".to_string();
        let err = client
            .embed_text_inputs(&["ok".into(), long], EmbedTask::Document, &[], None)
            .await
            .err()
            .unwrap();
        assert!(
            matches!(
                err,
                Error::TooLong {
                    input_index: Some(1),
                    tokens: 12,
                    max_tokens: 8
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn chunking_splits_on_tokens_carries_the_title_and_reports_offsets() {
        let text_seen = Arc::new(Mutex::new(Vec::new()));
        // Limit 10: prefix "title: T | text: " = 4 words + 2 specials = 6, minus
        // the 2-token seam leaves a 2-token text budget per chunk.
        let client = VisionClient::new(
            None,
            Some(fake_llama(0.0, text_seen.clone()).await),
            10,
            GpuLock::new(),
        );
        let text = "aa bb cc dd ee".to_string();
        let out = client
            .embed_text_inputs(
                std::slice::from_ref(&text),
                EmbedTask::Document,
                &[Some("T".into())],
                Some(&ChunkOptions {
                    max_tokens: None,
                    overlap_tokens: 1,
                }),
            )
            .await
            .unwrap();
        let chunks = out.chunks.unwrap();
        let spans: Vec<String> = chunks
            .iter()
            .map(|c| {
                text.chars()
                    .skip(c.char_start)
                    .take(c.char_end - c.char_start)
                    .collect()
            })
            .collect();
        assert_eq!(spans, ["aa bb ", "bb cc ", "cc dd ", "dd ee"]);
        assert!(chunks.iter().all(|c| c.input_index == 0));
        assert_eq!(
            chunks.iter().map(|c| c.chunk_index).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(out.embeddings[0], chunks[0].embedding);
        let sent = text_seen.lock().unwrap()[0].1["input"].clone();
        assert_eq!(sent[1], "title: T | text: bb cc ");

        // An overlap that eats the whole budget is refused, not looped on.
        let err = client
            .embed_text_inputs(
                std::slice::from_ref(&text),
                EmbedTask::Document,
                &[Some("T".into())],
                Some(&ChunkOptions {
                    max_tokens: None,
                    overlap_tokens: 2,
                }),
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(err, Error::BadRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn prompt_vectors_are_cached() {
        let text_seen = Arc::new(Mutex::new(Vec::new()));
        let client = VisionClient::new(
            None,
            Some(fake_llama(0.0, text_seen.clone()).await),
            1024,
            GpuLock::new(),
        );
        let a = client
            .prompt_vectors(&["credits".into(), "scene".into()])
            .await
            .unwrap();
        let b = client
            .prompt_vectors(&["scene".into(), "credits".into(), "logo".into()])
            .await
            .unwrap();
        assert_eq!(b[0], a[1]);
        assert_eq!(b[1], a[0]);
        let seen = text_seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].1["input"], json!(["logo"]));
    }

    #[test]
    fn cosine_matrix_refuses_a_stale_width() {
        let prompts = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let m = cosine_matrix(&[vec![1.0, 1.0]], &prompts).unwrap();
        assert!((m[0][0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!(matches!(
            cosine_matrix(&[vec![0.0; 1152]], &prompts),
            Err(Error::BadRequest(_))
        ));
    }

    #[test]
    fn reads_llama_servers_too_large_refusal() {
        assert_eq!(
            too_large_tokens(
                r#"{"error":{"code":500,"message":"input (1310 tokens) is too large to process. increase the physical batch size (current batch size: 1024)"}}"#
            ),
            Some(1310)
        );
        assert_eq!(too_large_tokens("model not loaded"), None);
    }

    #[test]
    fn chunk_spans_index_chars_not_bytes() {
        let pieces: Vec<Vec<u8>> = ["é", "té ", "ok"]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let spans = chunk_spans(&pieces, 2, 0);
        assert_eq!(
            spans,
            vec![
                Span {
                    text: "été ".into(),
                    char_start: 0,
                    char_end: 4
                },
                Span {
                    text: "ok".into(),
                    char_start: 4,
                    char_end: 6
                },
            ]
        );
    }
}
