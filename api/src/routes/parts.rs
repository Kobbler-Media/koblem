//! Part-segmentation routes. Auth-gated `/parts`, ungated `/health`. Thin proxy
//! onto `vision-server`'s `/parts` (SAM 3 promptable concept segmentation) via
//! `crate::parts::PartsClient`.
//!
//! One image in — a raw body (Content-Type image/*) or a multipart body with an
//! `image`/`file`/`images` part — plus the noun phrases to look for, and one
//! greyscale instance mask out per located part, tagged with the phrase that
//! found it.
//!
//! EVERY phrase goes in ONE request. SAM 3 encodes the image once and then runs a
//! cheap prompt-conditioned decode per phrase, so `prompts` is a list and a
//! caller that loops over it pays the whole encode again each time. The phrases
//! ride as a repeated `prompts=` query param, one comma/newline separated value,
//! or a multipart field — whichever the request already had a place for.
//!
//! Storage mirrors routes/depth.rs: by default the masks are written to the
//! gallery in `parts` order and answered as urls, gallery storage is a hard
//! precondition, and `?inline=true` is the opt-out that hands the PNGs back as
//! base64 and stores nothing.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::parts::{PartMask, PartsOptions, PartsResponse};
use crate::routes::depth::auth_gate;
use crate::routes::tier2_helpers::{
    OutputBytes, generation_file_url, generations_root, image_filename_for, meta_from_request,
    persist_generation,
};

/// 64 MB body cap — one large source image with room to spare.
const PARTS_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/parts", post(parts))
        .route("/api/v1/parts/health", get(health))
        .layer(DefaultBodyLimit::max(PARTS_BODY_LIMIT))
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct PartsQuery {
    backend: Option<String>,
    threshold: Option<f32>,
    nms: Option<f32>,
    /// Per-phrase cap. The character sheet's own count is a far better filter
    /// than a score cut; 0 leaves it uncapped.
    max_instances: Option<u32>,
    /// Skipped on the way out: `Serialize` exists here only to seed request_meta,
    /// which is built on the stored path, where this is false by definition.
    #[serde(default, skip_serializing)]
    inline: bool,
}

impl From<PartsQuery> for PartsOptions {
    fn from(q: PartsQuery) -> Self {
        PartsOptions {
            backend: q.backend,
            threshold: q.threshold,
            nms: q.nms,
            max_instances: q.max_instances,
        }
    }
}

/// Where one located part's mask actually is. Untagged: the stored and inline
/// shapes share no field name, so a caller branches on which it got.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PartMaskRef {
    Stored { mask_url: String },
    Inline { mask_png_base64: String },
}

/// One located instance. `name` is the phrase that found it — `/summon` rewrites
/// it back to the character sheet's own part name before it goes out.
#[derive(Debug, Serialize)]
pub struct PartOut {
    pub name: String,
    pub score: f64,
    #[serde(rename = "box")]
    pub bbox: [f64; 4],
    pub area: u64,
    #[serde(flatten)]
    pub mask: PartMaskRef,
}

#[derive(Debug, Serialize)]
pub struct PartsRouteResponse {
    pub width: u32,
    pub height: u32,
    pub prompts: Vec<String>,
    pub count: u32,
    pub parts: Vec<PartOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encode_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decode_seconds: Vec<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Absent for an inline request — nothing was filed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
}

/// `POST /api/v1/parts` — one image, every phrase.
async fn parts(
    State(state): State<AppState>,
    Query(query): Query<PartsQuery>,
    Query(pairs): Query<Vec<(String, String)>>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.parts.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "parts service not configured (set DEPTH_SERVICE_URL — `/parts` is served by the \
             same vision-server container as `/depth`)",
        );
    }
    let inline = query.inline;
    if !inline && generations_root(&state, "parts").await.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "gallery storage is not configured — set `generations_dir` in Settings; \
             part segmentation serves its masks from the gallery unless you ask for \
             `?inline=true`",
        );
    }

    let mut prompts = prompts_from_query(&pairs);
    let opts: PartsOptions = query.clone().into();

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let image: (String, Bytes) = if content_type.starts_with("multipart/") {
        let mp = match Multipart::from_request(request, &state).await {
            Ok(mp) => mp,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        };
        match collect_multipart(mp).await {
            Ok((Some(img), more)) => {
                prompts.extend(more);
                img
            }
            Ok((None, _)) => {
                return err(StatusCode::BAD_REQUEST, "no image part in multipart body");
            }
            Err(e) => return err(StatusCode::BAD_REQUEST, &e),
        }
    } else {
        let bytes = match to_bytes(request.into_body(), PARTS_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
        };
        if bytes.is_empty() {
            return err(StatusCode::BAD_REQUEST, "empty image body");
        }
        (image_filename_for(&content_type), bytes)
    };

    let prompts = dedupe(prompts);
    if prompts.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "prompts is required — repeat `?prompts=` per phrase, send one comma-separated \
             value, or add a `prompts` multipart field, e.g. `?prompts=arm,leg,head`",
        );
    }

    let restore_meta = meta_from_request(
        &query,
        &[
            ("had_image_inputs", json!(true)),
            ("prompts", json!(prompts)),
        ],
    );

    let r = match state.parts.parts(image, &prompts, &opts).await {
        Ok(r) => r,
        Err(crate::parts::Error::NotConfigured) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "parts service not configured",
            );
        }
        Err(crate::parts::Error::Disabled) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "parts service is administratively disabled",
            );
        }
        Err(crate::parts::Error::Upstream(status, body)) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return err(code, &format!("vision-server: {body}"));
        }
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("vision-server: {e}")),
    };

    let generation_id = if inline || r.parts.is_empty() {
        None
    } else {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut files: Vec<OutputBytes> = Vec::with_capacity(r.parts.len());
        for p in &r.parts {
            match b64.decode(&p.mask_png_base64) {
                Ok(bytes) => files.push(OutputBytes {
                    ext: "png".to_string(),
                    mime: "image/png".to_string(),
                    kind: "image".to_string(),
                    bytes: Bytes::from(bytes),
                }),
                Err(e) => {
                    return err(
                        StatusCode::BAD_GATEWAY,
                        &format!("vision-server returned an undecodable mask: {e}"),
                    );
                }
            }
        }

        let mut meta = restore_meta;
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("parts_located".to_string(), json!(located(&r)));
        }

        let Some(id) = persist_generation(&state, "parts", "parts", meta, &files).await else {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "parts located but saving to the gallery failed — check the configured \
                 directory is writable",
            );
        };
        Some(id)
    };

    Json(build_response(r, generation_id)).into_response()
}

/// Which phrase found which mask, alongside its score and box — a stored mask
/// PNG on its own says nothing about what it is a mask of.
fn located(r: &PartsResponse) -> Vec<serde_json::Value> {
    r.parts
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "score": p.score,
                "box": p.bbox,
                "area": p.area,
            })
        })
        .collect()
}

/// One upstream part carrying its own pixels. The single place the fields are
/// copied out, so the stored shape and `/summon`'s inline `parts` stage cannot
/// drift apart on which of them travel.
pub(crate) fn inline_out(p: PartMask) -> PartOut {
    PartOut {
        name: p.name,
        score: p.score,
        bbox: p.bbox,
        area: p.area,
        mask: PartMaskRef::Inline {
            mask_png_base64: p.mask_png_base64,
        },
    }
}

/// `Some(id)` = the masks were filed and are addressed by url; `None` = nothing
/// was stored, so they come back inline.
fn build_response(r: PartsResponse, generation_id: Option<String>) -> PartsRouteResponse {
    let parts = r
        .parts
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let mut out = inline_out(p);
            if let Some(id) = generation_id.as_deref() {
                out.mask = PartMaskRef::Stored {
                    mask_url: generation_file_url(id, i),
                };
            }
            out
        })
        .collect();
    PartsRouteResponse {
        width: r.width,
        height: r.height,
        prompts: r.prompts,
        count: r.count,
        parts,
        model: r.model,
        encode_seconds: r.encode_seconds,
        decode_seconds: r.decode_seconds,
        elapsed_seconds: r.elapsed_seconds,
        backend: r.backend,
        generation_id,
    }
}

/// The masks AND the frame they index. A consumer registers each mask against
/// its own crop proportionally, so the model's resolution travels with them —
/// it need not be the resolution of the pixels it was handed.
pub(crate) struct InlineParts {
    pub width: u32,
    pub height: u32,
    pub parts: Vec<PartOut>,
}

/// One image, every phrase, nothing filed — `/summon`'s `parts` stage, which
/// wants the masks for a sprite it is streaming rather than a gallery row to go
/// and read.
pub(crate) async fn parts_inline_one(
    state: &AppState,
    filename: &str,
    png: Bytes,
    prompts: &[String],
    max_instances: Option<u32>,
) -> Result<InlineParts, String> {
    if !state.parts.is_configured() {
        return Err("parts service not configured (set DEPTH_SERVICE_URL)".to_string());
    }
    let opts = PartsOptions {
        max_instances,
        ..Default::default()
    };
    let r = state
        .parts
        .parts((filename.to_string(), png), prompts, &opts)
        .await
        .map_err(|e| format!("vision-server: {e}"))?;
    Ok(InlineParts {
        width: r.width,
        height: r.height,
        parts: r.parts.into_iter().map(inline_out).collect(),
    })
}

/// `GET /api/v1/parts/health` — ungated liveness probe (UI bootstrap).
async fn health(State(state): State<AppState>) -> Response {
    if !state.parts.is_configured() {
        return Json(json!({
            "healthy": false,
            "worker_alive": false,
            "configured": false,
        }))
        .into_response();
    }
    match state.parts.health().await {
        Ok(h) => Json(json!({
            "healthy": h.healthy,
            "status": h.status,
            "worker_alive": h.worker_alive,
            "backend": h.backend,
            "in_flight": h.in_flight,
            "configured": true,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "healthy": false,
            "worker_alive": false,
            "configured": true,
        }))
        .into_response(),
    }
}

/// A JSON array, or a comma/newline separated list — the same three spellings
/// `vision-server` itself accepts.
fn split_prompts(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[')
        && let Ok(v) = serde_json::from_str::<Vec<String>>(trimmed)
    {
        return v
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    trimmed
        .split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn prompts_from_query(pairs: &[(String, String)]) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == "prompts")
        .flat_map(|(_, v)| split_prompts(v))
        .collect()
}

fn dedupe(prompts: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(prompts.len());
    for p in prompts {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The image part plus any `prompts` field, in one pass — `collect_multipart_images`
/// drains everything that is not an image, and the phrases are the point here.
async fn collect_multipart(
    mut mp: Multipart,
) -> Result<(Option<(String, Bytes)>, Vec<String>), String> {
    let mut image: Option<(String, Bytes)> = None;
    let mut prompts: Vec<String> = Vec::new();
    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or_default().to_string();
                match name.as_str() {
                    "images" | "image" | "file" => {
                        let fname = field
                            .file_name()
                            .map(str::to_string)
                            .unwrap_or_else(|| "image.png".to_string());
                        let bytes = field
                            .bytes()
                            .await
                            .map_err(|e| format!("reading image part: {e}"))?;
                        if image.is_none() {
                            image = Some((fname, bytes));
                        }
                    }
                    "prompts" => {
                        let text = field
                            .text()
                            .await
                            .map_err(|e| format!("reading prompts part: {e}"))?;
                        prompts.extend(split_prompts(&text));
                    }
                    _ => {
                        let _ = field.bytes().await;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("malformed multipart: {e}")),
        }
    }
    Ok((image, prompts))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(count: usize) -> PartsResponse {
        PartsResponse {
            width: 1024,
            height: 1024,
            prompts: vec!["arm".to_string(), "leg".to_string()],
            count: count as u32,
            parts: (0..count)
                .map(|i| PartMask {
                    name: format!("part{i}"),
                    score: 0.6 + i as f64 / 100.0,
                    bbox: [i as f64, 2.0, 3.0, 4.0],
                    area: 4096 + i as u64,
                    mask_png_base64: format!("mask{i}"),
                })
                .collect(),
            model: Some("sam3".to_string()),
            encode_seconds: Some(4.2),
            decode_seconds: vec![1.01, 0.98],
            elapsed_seconds: Some(6.3),
            backend: Some("vulkan".to_string()),
        }
    }

    fn json_of(r: PartsResponse, generation_id: Option<&str>) -> serde_json::Value {
        serde_json::to_value(build_response(r, generation_id.map(str::to_string))).unwrap()
    }

    /// The default, and what the gallery UI reads: one file per located part, in
    /// `parts` order, so a mask url always belongs to the phrase beside it.
    #[test]
    fn a_stored_run_answers_with_urls_in_parts_order() {
        let v = json_of(response(3), Some("gen-1"));
        assert_eq!(v["generation_id"], "gen-1");
        assert_eq!(v["count"], 3);
        for i in 0..3 {
            assert_eq!(
                v["parts"][i]["mask_url"],
                format!("/api/v1/generations/gen-1/file/{i}")
            );
            assert_eq!(v["parts"][i]["name"], format!("part{i}"));
            assert!(
                v["parts"][i].get("mask_png_base64").is_none(),
                "stored shape leaked the pixels"
            );
        }
    }

    /// The opt-in: the pixels themselves, and no evidence they ever existed.
    #[test]
    fn an_inline_run_answers_with_the_pngs_and_no_generation() {
        let v = json_of(response(2), None);
        assert!(v.get("generation_id").is_none());
        assert_eq!(v["parts"][0]["mask_png_base64"], "mask0");
        assert_eq!(v["parts"][1]["mask_png_base64"], "mask1");
        assert!(v["parts"][0].get("mask_url").is_none());
    }

    /// A mask is only usable against the pixels it indexes, so the dimensions and
    /// the phrase/score/box that found it travel in both shapes.
    #[test]
    fn both_shapes_carry_the_dimensions_the_phrases_and_the_boxes() {
        for id in [Some("gen-1"), None] {
            let v = json_of(response(1), id);
            assert_eq!(v["width"], 1024, "{id:?}");
            assert_eq!(v["height"], 1024, "{id:?}");
            assert_eq!(v["prompts"], json!(["arm", "leg"]), "{id:?}");
            assert_eq!(v["model"], "sam3", "{id:?}");
            assert_eq!(v["encode_seconds"], 4.2, "{id:?}");
            assert_eq!(v["decode_seconds"], json!([1.01, 0.98]), "{id:?}");
            let p = &v["parts"][0];
            assert_eq!(p["score"], 0.6, "{id:?}");
            assert_eq!(p["box"], json!([0.0, 2.0, 3.0, 4.0]), "{id:?}");
            assert_eq!(p["area"], 4096, "{id:?}");
        }
    }

    /// Nothing located is a successful empty answer, not a 502 and not a gallery
    /// row with no files in it.
    #[test]
    fn locating_nothing_is_an_empty_list() {
        let v = json_of(response(0), None);
        assert_eq!(v["count"], 0);
        assert_eq!(v["parts"], json!([]));
    }

    /// Three spellings, one list — and repeated params accumulate rather than
    /// overwrite, because every phrase belongs in the same request.
    #[test]
    fn prompts_are_accepted_in_every_spelling() {
        let cases: &[(&str, &[&str])] = &[
            ("arm,leg,head", &["arm", "leg", "head"]),
            (" arm , leg ,, head ", &["arm", "leg", "head"]),
            ("arm\nleg\nhead", &["arm", "leg", "head"]),
            (r#"["arm","leg","head"]"#, &["arm", "leg", "head"]),
            (r#"[ "front arm" , "leg" ]"#, &["front arm", "leg"]),
            ("", &[]),
            ("  ", &[]),
        ];
        for (raw, expected) in cases {
            assert_eq!(split_prompts(raw), *expected, "raw: {raw:?}");
        }

        let pairs = |q: &str| -> Vec<String> {
            let uri: axum::http::Uri = format!("/api/v1/parts?{q}").parse().unwrap();
            let pairs = Query::<Vec<(String, String)>>::try_from_uri(&uri)
                .unwrap()
                .0;
            prompts_from_query(&pairs)
        };
        assert_eq!(pairs("prompts=arm&prompts=leg"), ["arm", "leg"]);
        assert_eq!(pairs("prompts=arm,leg&backend=gpu"), ["arm", "leg"]);
        assert_eq!(pairs("prompts=front%20arm"), ["front arm"]);
        assert!(pairs("backend=gpu").is_empty());

        assert_eq!(
            dedupe(vec![
                "arm".to_string(),
                "leg".to_string(),
                "arm".to_string()
            ]),
            ["arm", "leg"]
        );
    }

    /// The scalar knobs still parse when the phrases share the query string, and
    /// `inline` is opt-in and never reaches the gallery row it is the reason not
    /// to write.
    #[test]
    fn the_knobs_parse_alongside_repeated_prompts_and_inline_stays_out_of_meta() {
        let parse = |q: &str| {
            let uri: axum::http::Uri = format!("/api/v1/parts?{q}").parse().unwrap();
            Query::<PartsQuery>::try_from_uri(&uri).unwrap().0
        };
        let q = parse("prompts=arm&prompts=leg&backend=gpu&threshold=0.41&nms=0.2&max_instances=2");
        assert_eq!(q.backend.as_deref(), Some("gpu"));
        assert_eq!(q.threshold, Some(0.41));
        assert_eq!(q.nms, Some(0.2));
        assert_eq!(q.max_instances, Some(2));
        assert!(!q.inline);
        assert!(parse("inline=true").inline);

        let opts: PartsOptions = q.into();
        assert_eq!(opts.threshold, Some(0.41));
        assert_eq!(opts.max_instances, Some(2));

        let meta = meta_from_request(
            &PartsQuery {
                backend: Some("gpu".to_string()),
                threshold: Some(0.35),
                nms: None,
                max_instances: Some(2),
                inline: true,
            },
            &[("prompts", json!(["arm", "leg"]))],
        );
        assert_eq!(meta["max_instances"], 2);
        assert_eq!(meta["prompts"], json!(["arm", "leg"]));
        assert!(meta.get("inline").is_none(), "meta: {meta}");
    }

    /// A stored mask PNG says nothing about what it is a mask of, so the row
    /// records which phrase found each one.
    #[test]
    fn the_gallery_row_records_which_phrase_found_each_mask() {
        let l = located(&response(2));
        assert_eq!(l.len(), 2);
        assert_eq!(l[1]["name"], "part1");
        assert_eq!(l[1]["score"], 0.61);
        assert_eq!(l[1]["area"], 4097);
        assert_eq!(l[0]["box"], json!([0.0, 2.0, 3.0, 4.0]));
    }
}
