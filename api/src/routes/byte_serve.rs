//! Shared range-aware byte serving for on-disk / in-memory media (audio, video,
//! images, transcripts). Honours a single HTTP `Range` header so browsers can
//! seek/scrub `<audio>`/`<video>`; falls back to a full 200 with
//! `Accept-Ranges: bytes` when no range is requested.
//!
//! Two entry points, same wire contract:
//! * [`serve_bytes`] for content already resident in RAM (job store results,
//!   agent outputs) — slices the buffer.
//! * [`serve_file`] for content on disk — seeks and streams, so a 1 MB range of
//!   a 53 MB clip reads 1 MB, and a full read never materialises the file.

use std::path::Path;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Chunk size the file reader hands to the body. Big enough that a 50 MB clip is
/// ~800 reads rather than ~6000, small enough to stay off the large-alloc path.
const STREAM_CHUNK: usize = 64 * 1024;

/// Media that MAY be rewritten in place (a legacy `{uuid}-{i}` gallery name, an avatar portrait):
/// a cache may hold it but must revalidate every time.
const CACHE_CONTROL: &str = "private, max-age=0, must-revalidate";

/// Media whose NAME IS ITS HASH cannot change without changing its URL, so it is cacheable
/// forever (§6.2). Same value the director asset store has served since it was written — this is
/// the whole reason to content-address the gallery, and it only becomes true once the name is
/// derived from the bytes.
pub const CACHE_CONTROL_IMMUTABLE: &str = "private, max-age=31536000, immutable";

/// Serve `bytes` with `content_type`, honouring a single-range `Range` header.
pub fn serve_bytes(headers: &HeaderMap, bytes: Bytes, content_type: &str) -> Response {
    let total = bytes.len() as u64;

    let Some(raw) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, total)
            .body(Body::from(bytes))
            .unwrap();
    };

    match parse_range(raw, total) {
        Some((start, end)) => {
            // `end` is inclusive; Bytes::slice end is exclusive.
            let slice = bytes.slice(start as usize..(end as usize + 1));
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .header(header::CONTENT_LENGTH, slice.len())
                .body(Body::from(slice))
                .unwrap()
        }
        None => unsatisfiable(total),
    }
}

/// Serve the file at `path`, streaming from disk instead of buffering it.
///
/// Same status codes, `Content-Range` and `Accept-Ranges` as [`serve_bytes`],
/// plus a `(size, mtime)` `ETag` so a re-read costs a 304. `Err` is the open /
/// stat failure, left to the caller so each route keeps its own 404 wording.
pub async fn serve_file(
    headers: &HeaderMap,
    path: &Path,
    content_type: &str,
) -> std::io::Result<Response> {
    serve_file_cached(headers, path, content_type, CACHE_CONTROL).await
}

/// [`serve_file`] with the caching policy chosen by the caller — which is the only thing that
/// differs between a rewritable path and a content-addressed one.
pub async fn serve_file_cached(
    headers: &HeaderMap,
    path: &Path,
    content_type: &str,
    cache_control: &str,
) -> std::io::Result<Response> {
    let mut file = tokio::fs::File::open(path).await?;
    // Stat the OPEN handle, not the path: the validator then describes exactly
    // the bytes this response streams, even if the file is replaced mid-serve.
    let meta = file.metadata().await?;
    let total = meta.len();
    let tag = etag(&meta);

    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|sent| etag_matches(sent, &tag))
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &tag)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::ACCEPT_RANGES, "bytes")
            .body(Body::empty())
            .unwrap());
    }

    let range = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(raw) => match parse_range(raw, total) {
            Some(r) => Some(r),
            None => return Ok(unsatisfiable(total)),
        },
        None => None,
    };

    let (status, start, len) = match range {
        Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        None => (StatusCode::OK, 0, total),
    };
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let stream = ReaderStream::with_capacity(file.take(len), STREAM_CHUNK);

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len)
        .header(header::ETAG, &tag)
        .header(header::CACHE_CONTROL, cache_control);
    if let Some((start, end)) = range {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    Ok(builder.body(Body::from_stream(stream)).unwrap())
}

fn unsatisfiable(total: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .body(Body::empty())
        .unwrap()
}

/// `(size, mtime)` validator. Not the file name: a rebuilt cut is rewritten in
/// place, so a name-derived tag would pin stale content forever.
pub fn etag(meta: &std::fs::Metadata) -> String {
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{stamp:x}\"", meta.len())
}

/// `If-None-Match` matching, per RFC 9110: `*`, or any member of the comma list,
/// weak comparison (a `W/` prefix on either side is not a difference for media).
pub fn etag_matches(sent: &str, tag: &str) -> bool {
    let strip = |v: &str| v.trim().trim_start_matches("W/").to_string();
    let tag = strip(tag);
    sent.trim() == "*" || sent.split(',').any(|candidate| strip(candidate) == tag)
}

/// Parse a single byte range against `total`. Returns an inclusive
/// `(start, end)` clamped to the content, or None if absent/malformed/
/// unsatisfiable. Multi-range requests fall back to the first range.
fn parse_range(spec: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = spec.trim().strip_prefix("bytes=")?;
    // Only the first range of a comma list is served.
    let first = spec.split(',').next()?.trim();
    let (start_s, end_s) = first.split_once('-')?;
    let (start_s, end_s) = (start_s.trim(), end_s.trim());

    let last = total - 1;
    let (start, end) = if start_s.is_empty() {
        // suffix range: bytes=-N → last N bytes
        let n: u64 = end_s.parse().ok()?;
        if n == 0 {
            return None;
        }
        (total.saturating_sub(n), last)
    } else {
        let start: u64 = start_s.parse().ok()?;
        let end = if end_s.is_empty() {
            last
        } else {
            end_s.parse::<u64>().ok()?.min(last)
        };
        (start, end)
    };

    if start > end || start > last {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn full_when_no_range() {
        assert_eq!(parse_range("", 100), None); // no bytes= prefix
    }

    #[test]
    fn explicit_range() {
        assert_eq!(parse_range("bytes=0-499", 1000), Some((0, 499)));
        assert_eq!(parse_range("bytes=500-999", 1000), Some((500, 999)));
    }

    #[test]
    fn open_ended_range() {
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
    }

    #[test]
    fn suffix_range() {
        assert_eq!(parse_range("bytes=-200", 1000), Some((800, 999)));
        // suffix larger than content clamps to whole file
        assert_eq!(parse_range("bytes=-5000", 1000), Some((0, 999)));
    }

    #[test]
    fn end_clamped_to_content() {
        assert_eq!(parse_range("bytes=900-100000", 1000), Some((900, 999)));
    }

    #[test]
    fn first_of_multi_range() {
        assert_eq!(parse_range("bytes=0-99,200-299", 1000), Some((0, 99)));
    }

    #[test]
    fn unsatisfiable_ranges() {
        assert_eq!(parse_range("bytes=2000-3000", 1000), None); // start past end
        assert_eq!(parse_range("bytes=abc-def", 1000), None);
        assert_eq!(parse_range("bytes=-0", 1000), None);
        assert_eq!(parse_range("bytes=0-99", 0), None); // empty content
    }

    #[test]
    fn a_client_revalidates_on_the_etag_it_was_served() {
        let tag = "\"1f4-abc\"";
        assert!(etag_matches(tag, tag));
        assert!(etag_matches("*", tag));
        assert!(etag_matches("W/\"1f4-abc\"", tag));
        assert!(etag_matches("\"other\", \"1f4-abc\"", tag));
        assert!(!etag_matches("\"1f4-abd\"", tag));
        assert!(!etag_matches("", tag));
    }

    // ── streaming path ────────────────────────────────────────────────────
    //
    // The wire contract is the point: `serve_file` must be swappable for
    // `serve_bytes` at any disk-backed call site without a client noticing.

    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(bytes: &[u8]) -> Self {
            let path =
                std::env::temp_dir().join(format!("koblem-byte-serve-{}", uuid::Uuid::now_v7()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn req(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), v.parse().unwrap());
        }
        h
    }

    async fn body_of(resp: Response) -> Vec<u8> {
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn head(resp: &Response, name: header::HeaderName) -> String {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn streamed_full_read_matches_the_buffered_response() {
        let content: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let f = TempFile::new(&content);
        let streamed = serve_file(&req(&[]), &f.0, "video/webm").await.unwrap();
        let buffered = serve_bytes(&req(&[]), Bytes::from(content.clone()), "video/webm");

        assert_eq!(streamed.status(), buffered.status());
        assert_eq!(streamed.status(), StatusCode::OK);
        assert_eq!(head(&streamed, header::ACCEPT_RANGES), "bytes");
        assert_eq!(head(&streamed, header::CONTENT_TYPE), "video/webm");
        assert_eq!(
            head(&streamed, header::CONTENT_LENGTH),
            head(&buffered, header::CONTENT_LENGTH)
        );
        assert!(!head(&streamed, header::ETAG).is_empty());
        assert_eq!(body_of(streamed).await, content);
    }

    #[tokio::test]
    async fn streamed_range_matches_the_buffered_response() {
        let content: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let f = TempFile::new(&content);
        let h = req(&[(header::RANGE, "bytes=1000-1999")]);
        let streamed = serve_file(&h, &f.0, "video/webm").await.unwrap();
        let buffered = serve_bytes(&h, Bytes::from(content.clone()), "video/webm");

        assert_eq!(streamed.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(streamed.status(), buffered.status());
        assert_eq!(
            head(&streamed, header::CONTENT_RANGE),
            "bytes 1000-1999/4096"
        );
        assert_eq!(
            head(&streamed, header::CONTENT_RANGE),
            head(&buffered, header::CONTENT_RANGE)
        );
        assert_eq!(head(&streamed, header::CONTENT_LENGTH), "1000");
        assert_eq!(body_of(streamed).await, content[1000..2000]);
    }

    /// `Range: bytes=N-` is how a resumed download continues a partial file.
    #[tokio::test]
    async fn streamed_open_ended_range_serves_the_tail() {
        let content: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let f = TempFile::new(&content);
        let resp = serve_file(&req(&[(header::RANGE, "bytes=4000-")]), &f.0, "video/webm")
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(head(&resp, header::CONTENT_RANGE), "bytes 4000-4095/4096");
        assert_eq!(head(&resp, header::CONTENT_LENGTH), "96");
        assert_eq!(body_of(resp).await, content[4000..]);
    }

    #[tokio::test]
    async fn streamed_unsatisfiable_range_is_416() {
        let f = TempFile::new(&[0u8; 100]);
        let resp = serve_file(&req(&[(header::RANGE, "bytes=500-600")]), &f.0, "image/png")
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(head(&resp, header::CONTENT_RANGE), "bytes */100");
    }

    /// The validator must describe the bytes actually streamed, and a re-read
    /// carrying it back must cost a 304 with no body.
    #[tokio::test]
    async fn a_reread_with_the_served_etag_is_304() {
        let f = TempFile::new(b"the quick brown fox");
        let first = serve_file(&req(&[]), &f.0, "text/plain").await.unwrap();
        let tag = head(&first, header::ETAG);
        assert!(!tag.is_empty());

        let second = serve_file(&req(&[(header::IF_NONE_MATCH, &tag)]), &f.0, "text/plain")
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(head(&second, header::ETAG), tag);
        assert!(body_of(second).await.is_empty());
    }

    /// A rewrite at the same path must invalidate: the tag carries the size.
    #[tokio::test]
    async fn a_rewritten_file_gets_a_new_etag() {
        let f = TempFile::new(b"before");
        let tag = head(
            &serve_file(&req(&[]), &f.0, "text/plain").await.unwrap(),
            header::ETAG,
        );
        std::fs::write(&f.0, b"after the rewrite").unwrap();
        let resp = serve_file(&req(&[(header::IF_NONE_MATCH, &tag)]), &f.0, "text/plain")
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_ne!(head(&resp, header::ETAG), tag);
        assert_eq!(body_of(resp).await, b"after the rewrite");
    }

    #[tokio::test]
    async fn a_missing_file_is_the_callers_error_to_word() {
        let missing = std::env::temp_dir().join("koblem-byte-serve-does-not-exist");
        assert!(serve_file(&req(&[]), &missing, "video/webm").await.is_err());
    }
}
