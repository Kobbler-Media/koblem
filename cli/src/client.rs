use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::multipart;
use serde::Serialize;

const UPLOAD_CHUNK: usize = 256 * 1024;

/// The queue knobs, sent as `X-Queue-*` on every request.
///
/// 🔴 Headers rather than body fields because koblem's render routes strict-parse
/// their bodies and reject unknown keys — and half of them take multipart. On a
/// GET these are simply ignored, which is why they can live beside the token
/// instead of being threaded through nineteen subcommands.
#[derive(Debug, Clone, Default)]
pub struct QueueFlags {
    /// `queue` | `now`. 🔴 The CLI authenticates with an API key, whose server-side
    /// default is `now` — the opposite of what a CLI is for. Without an explicit
    /// mode, "queue five renders overnight" cannot be expressed at all.
    pub mode: Option<String>,
    pub band: Option<i16>,
    pub gpu: Option<String>,
    pub force: bool,
    pub after: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    queue: QueueFlags,
    /// Retry on `503` (GPU busy with a higher-priority job), honoring Retry-After.
    retry_busy: bool,
    /// Upper bound on total time spent waiting out 503s.
    max_wait: Duration,
}

pub type Query<'a> = [(&'a str, String)];

/// Outcome of a single HTTP attempt: a usable response, or a 503 with the
/// upstream's suggested Retry-After (seconds), if any.
enum Attempt {
    Ok(reqwest::Response),
    Busy(Option<u64>),
}

impl Client {
    pub fn new(
        base: String,
        token: Option<String>,
        queue: QueueFlags,
        retry_busy: bool,
        max_wait: Duration,
    ) -> Result<Self> {
        // No request timeout: heavy renders (video, music) run for minutes.
        let http = reqwest::Client::builder()
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
            queue,
            retry_busy,
            max_wait,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn queue_flags(&self) -> &QueueFlags {
        &self.queue
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        };
        if let Some(mode) = &self.queue.mode {
            rb = rb.header("x-queue-mode", mode);
        }
        if let Some(band) = self.queue.band {
            rb = rb.header("x-queue-band", band.to_string());
        }
        if let Some(gpu) = &self.queue.gpu {
            rb = rb.header("x-queue-gpu", gpu);
        }
        if self.queue.force {
            rb = rb.header("x-queue-force", "1");
        }
        if let Some(after) = &self.queue.after {
            rb = rb.header("x-queue-after", after);
        }
        rb
    }

    pub async fn get(&self, path: &str, query: &Query<'_>) -> Result<reqwest::Response> {
        let rb = self.http.get(self.url(path)).query(query);
        self.send(rb).await
    }

    /// GET continuing from byte `from` (0 = whole resource). The caller must
    /// handle both a `206` and a `200` — a range is a request, not a guarantee —
    /// and a `416`, which says the local partial is longer than the resource.
    pub async fn get_range(
        &self,
        path: &str,
        query: &Query<'_>,
        from: u64,
    ) -> Result<reqwest::Response> {
        let mut rb = self.http.get(self.url(path)).query(query);
        if from > 0 {
            rb = rb.header(reqwest::header::RANGE, format!("bytes={from}-"));
        }
        self.send(rb).await
    }

    pub async fn post_json<T: Serialize>(
        &self,
        path: &str,
        query: &Query<'_>,
        body: &T,
    ) -> Result<reqwest::Response> {
        let rb = self.http.post(self.url(path)).query(query).json(body);
        self.send(rb).await
    }

    /// POST an enqueue under an idempotency key.
    ///
    /// 🔴 Not minted globally in [`Client::auth`]. A key is only protection if it is the SAME key
    /// on the retry, and every `kob` render is its own process — a per-invocation UUID would
    /// dedupe nothing a re-run of the loop cares about, while a payload-derived one applied to all
    /// nineteen subcommands would silently collapse a deliberate second take of the same prompt.
    /// Only `queue add` holds a whole payload it can key off, so only `queue add` mints one.
    pub async fn post_json_idempotent<T: Serialize>(
        &self,
        path: &str,
        query: &Query<'_>,
        body: &T,
        key: &str,
    ) -> Result<reqwest::Response> {
        let rb = self
            .http
            .post(self.url(path))
            .query(query)
            .header("idempotency-key", key)
            .json(body);
        self.send(rb).await
    }

    pub async fn patch_json<T: Serialize>(
        &self,
        path: &str,
        query: &Query<'_>,
        body: &T,
    ) -> Result<reqwest::Response> {
        let rb = self.http.patch(self.url(path)).query(query).json(body);
        self.send(rb).await
    }

    pub async fn post_raw(
        &self,
        path: &str,
        query: &Query<'_>,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<reqwest::Response> {
        let rb = self
            .http
            .post(self.url(path))
            .query(query)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(bytes);
        self.send(rb).await
    }

    pub async fn post_multipart(
        &self,
        path: &str,
        query: &Query<'_>,
        form: multipart::Form,
    ) -> Result<reqwest::Response> {
        let rb = self.http.post(self.url(path)).query(query).multipart(form);
        self.send(rb).await
    }

    pub async fn delete(&self, path: &str, query: &Query<'_>) -> Result<reqwest::Response> {
        let rb = self.http.delete(self.url(path)).query(query);
        self.send(rb).await
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let start = Instant::now();
        loop {
            // Streaming multipart bodies aren't cloneable, so those get one shot.
            let Some(builder) = rb.try_clone() else {
                return match self.attempt(rb).await? {
                    Attempt::Ok(r) => Ok(r),
                    Attempt::Busy(_) => bail!(
                        "503 — GPU busy with a higher-priority job, and this upload can't be \
                         auto-retried. Retry shortly."
                    ),
                };
            };
            match self.attempt(builder).await? {
                Attempt::Ok(r) => return Ok(r),
                Attempt::Busy(retry_after) => {
                    let elapsed = start.elapsed();
                    if !self.retry_busy || elapsed >= self.max_wait {
                        bail!(
                            "503 — GPU busy with a higher-priority job. Retry later, or raise \
                             --max-wait."
                        );
                    }
                    let secs = retry_after.unwrap_or(5).clamp(1, 60);
                    let wait = Duration::from_secs(secs).min(self.max_wait - elapsed);
                    eprintln!("GPU busy (503); retrying in {}s…", wait.as_secs().max(1));
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn attempt(&self, rb: reqwest::RequestBuilder) -> Result<Attempt> {
        let resp = self
            .auth(rb)
            .send()
            .await
            .with_context(|| format!("request to {} failed (is koblem running?)", self.base))?;
        let status = resp.status();
        // 416 is only reachable from `get_range`, and there it is an answer
        // ("your partial is stale"), not a failure — hand it back intact.
        if status.is_success() || status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(Attempt::Ok(resp));
        }
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            return Ok(Attempt::Busy(retry_after));
        }
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!(
                "401 Unauthorized — mint a key with `kob keys create <name>` (using an \
                 existing credential, or KOBLEM_API_KEY on the server), or from koblem's \
                 Settings → API Keys. Then `kob config set --token <key>`, or set KOBLEM_TOKEN."
            );
        }
        bail!("koblem returned {status}: {}", body.trim());
    }
}

/// Build a multipart file part from a path, guessing the MIME from the extension and using the
/// file's basename as the part filename.
///
/// STREAMED, not read into memory. It used to be
/// `tokio::fs::read` into `Part::bytes`, which held the whole file — and reqwest then buffered the
/// multipart body on top of that, so uploading a 589 MB clip cost well over a gigabyte of RSS in a
/// CLI whose downloads already stream and resume. The file is opened here and read in chunks as
/// the socket drains it.
///
/// `Content-Length` comes from the file's metadata so the server still sees a sized body rather
/// than a chunked one — a body-limit rejection stays immediate instead of arriving 500 MB in. A
/// file that changes size mid-upload therefore fails the request, which is the honest outcome.
pub async fn file_part(path: &Path) -> Result<multipart::Part> {
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let len = file
        .metadata()
        .await
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let mime = crate::output::mime_for_path(path);
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(
        file,
        UPLOAD_CHUNK,
    ));
    multipart::Part::stream_with_length(body, len)
        .file_name(filename)
        .mime_str(&mime)
        .context("invalid mime")
}
