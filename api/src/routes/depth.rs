//! Depth-estimation routes. Auth-gated `/depth`, ungated `/health`. Thin proxy
//! onto `vision-server`'s `/depth` (Depth-Anything V2 by default, DA3 via
//! `?model=depth-anything-3`) via `crate::depth::DepthClient`.
//!
//! Same request shape as routes/matting.rs: EITHER a single raw image body
//! (Content-Type image/*) OR a multipart body with one-or-more repeated `images`
//! file parts (batch); the tuning knobs ride as query params.
//!
//! Each input yields a 16-bit greyscale depth PNG, plus a confidence PNG when the
//! model has a confidence head — DA3 does, V2 does not. By default they are
//! written to the gallery IN THAT ORDER per result, so the file indexes are
//! consecutive rather than a fixed stride: two maps per result under DA3, one
//! under V2. Gallery storage is a hard precondition and a request that cannot be
//! filed is a 503.
//!
//! A missing confidence map is an ABSENT field, never a filled-in constant. V2
//! has none, and DA3's is a per-image rank rather than a calibrated probability,
//! so a consumer that needs a blend weight should derive one from its own alpha
//! distance field rather than read a default as certainty.
//!
//! `?inline=true` is the opt-out, for callers that want the maps rather than a
//! place to find them: the PNGs come back as base64 and NOTHING is stored — no
//! files, no generations row, so no request_meta and no `depth_ranges` either.
//! Opt-in because it hands a batch of 16-bit PNGs back over JSON, which is the
//! wrong default for the gallery-backed UI and for matting's own rule.
//!
//! The per-result min/max ride in the response in BOTH shapes, and the stored
//! shape additionally records them in the generation's request_meta: the PNGs are
//! min/max normalised, so a gallery row without the bounds is a picture of a
//! depth map rather than a depth map.
//!
//! Auth accepts EITHER a real koblem credential (`require_auth`: env key, web
//! session or a named API key as `Authorization: Bearer`) OR the paperworld
//! doorstop that `/oneshot` and `/character_sheet` take. Depth is the third leg
//! of that game's summon and provisioning it a real credential to reach one
//! GPU-burning route would be the only thing standing between it and the same
//! `x-api-key` it already sends.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::auth::require_auth;
use crate::depth::{DepthModel, DepthOptions, DepthResponse};
use crate::routes::oneshot::key_ok;
use crate::routes::tier2_helpers::{
    OutputBytes, collect_multipart_images, generation_file_url, generations_root,
    image_filename_for, meta_from_request, persist_generation,
};

/// 64 MB body cap — covers a handful of large source images in one batch.
const DEPTH_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/depth", post(depth))
        .route("/api/v1/depth/health", get(health))
        .layer(DefaultBodyLimit::max(DEPTH_BODY_LIMIT))
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct DepthQuery {
    backend: Option<String>,
    model: Option<DepthModel>,
    /// depth-anything-v2 only — floor on the SHORTEST side. See `DepthOptions`:
    /// the two families measure inference resolution on different axes, so there
    /// is no shared `process_res` to get wrong.
    process_res_short: Option<u32>,
    /// depth-anything-3 only — bound on the LONGEST side.
    process_res_long: Option<u32>,
    /// Skipped on the way out: `Serialize` exists here only to seed request_meta,
    /// which is built on the stored path, where this is false by definition.
    #[serde(default, skip_serializing)]
    inline: bool,
}

impl From<DepthQuery> for DepthOptions {
    fn from(q: DepthQuery) -> Self {
        DepthOptions {
            backend: q.backend,
            model: q.model,
            process_res_short: q.process_res_short,
            process_res_long: q.process_res_long,
        }
    }
}

/// Where one result's maps actually are — always a depth map, and a confidence
/// map only when the model produced one. Untagged: the stored and inline shapes
/// share no field name, so a caller branches on which it got.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum DepthMaps {
    Stored {
        depth_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence_url: Option<String>,
    },
    Inline {
        depth_png_base64: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence_png_base64: Option<String>,
    },
}

/// One estimated result. The bounds denormalise the 16-bit pixels back to raw
/// units via `min + (px / 65535) * (max - min)`, so they travel in both shapes;
/// the confidence bounds are absent exactly when the confidence map is.
#[derive(Debug, Serialize)]
pub struct DepthResultOut {
    pub width: u32,
    pub height: u32,
    #[serde(flatten)]
    pub maps: DepthMaps,
    pub depth_min: f64,
    pub depth_max: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence_max: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct DepthRouteResponse {
    pub count: u32,
    pub results: Vec<DepthResultOut>,
    /// Which family answered, and whether larger is nearer (`"disparity"`, V2)
    /// or farther (`"distance"`, DA3). Reading depth without the polarity turns
    /// every sprite inside out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth_polarity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_seconds: Option<f64>,
    /// Absent for an inline request — nothing was filed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
}

/// `POST /api/v1/depth` — single raw image or multipart batch.
async fn depth(
    State(state): State<AppState>,
    Query(query): Query<DepthQuery>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if !state.depth.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "depth service not configured (set DEPTH_SERVICE_URL)",
        );
    }
    let inline = query.inline;
    if !inline && generations_root(&state, "depth").await.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "gallery storage is not configured — set `generations_dir` in Settings; \
             depth estimation serves its maps from the gallery unless you ask for \
             `?inline=true`",
        );
    }

    let restore_meta = meta_from_request(&query, &[("had_image_inputs", json!(true))]);
    let opts: DepthOptions = query.into();

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let images: Vec<(String, Bytes)> = if content_type.starts_with("multipart/") {
        let mp = match Multipart::from_request(request, &state).await {
            Ok(mp) => mp,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        };
        match collect_multipart_images(mp).await {
            Ok(imgs) if !imgs.is_empty() => imgs,
            Ok(_) => return err(StatusCode::BAD_REQUEST, "no image parts in multipart body"),
            Err(e) => return err(StatusCode::BAD_REQUEST, &e),
        }
    } else {
        let bytes = match to_bytes(request.into_body(), DEPTH_BODY_LIMIT).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("reading body: {e}")),
        };
        if bytes.is_empty() {
            return err(StatusCode::BAD_REQUEST, "empty image body");
        }
        vec![(image_filename_for(&content_type), bytes)]
    };

    let r = match state.depth.depth_batch(&images, &opts).await {
        Ok(r) => r,
        Err(crate::depth::Error::NotConfigured) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "depth service not configured",
            );
        }
        Err(crate::depth::Error::Disabled) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "depth service is administratively disabled",
            );
        }
        Err(crate::depth::Error::Upstream(status, body)) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return err(code, &format!("vision-server: {body}"));
        }
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("vision-server: {e}")),
    };

    if r.results.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "vision-server returned no maps");
    }

    let generation_id = if inline {
        None
    } else {
        // Two files per result, in order — a single undecodable entry fails the
        // request rather than silently shifting every url after it onto the
        // wrong map.
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut files: Vec<OutputBytes> = Vec::with_capacity(r.results.len() * 2);
        for x in &r.results {
            for png in [Some(&x.depth_png_base64), x.confidence_png_base64.as_ref()]
                .into_iter()
                .flatten()
            {
                match b64.decode(png) {
                    Ok(bytes) => files.push(OutputBytes {
                        ext: "png".to_string(),
                        mime: "image/png".to_string(),
                        kind: "image".to_string(),
                        bytes: Bytes::from(bytes),
                    }),
                    Err(e) => {
                        return err(
                            StatusCode::BAD_GATEWAY,
                            &format!("vision-server returned an undecodable map: {e}"),
                        );
                    }
                }
            }
        }

        let mut meta = restore_meta;
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("depth_ranges".to_string(), json!(depth_ranges(&r)));
        }

        let Some(id) = persist_generation(&state, "depth", "depth", meta, &files).await else {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "depth estimated but saving to the gallery failed — check the configured \
                 directory is writable",
            );
        };
        Some(id)
    };

    Json(build_response(r, generation_id)).into_response()
}

/// The bounds, alongside the pixel dimensions they belong to, for the gallery
/// row — a stored PNG is meaningless without them.
fn depth_ranges(r: &DepthResponse) -> Vec<serde_json::Value> {
    r.results
        .iter()
        .map(|x| {
            let mut row = json!({
                "width": x.width,
                "height": x.height,
                "depth_min": x.depth_min,
                "depth_max": x.depth_max,
            });
            if let (Some(obj), Some(lo), Some(hi)) =
                (row.as_object_mut(), x.confidence_min, x.confidence_max)
            {
                obj.insert("confidence_min".to_string(), json!(lo));
                obj.insert("confidence_max".to_string(), json!(hi));
            }
            row
        })
        .collect()
}

/// One upstream result carrying its own pixels. The single place the bounds are
/// copied out, so the stored shape below and `/summon`'s inline `depth` stage
/// cannot drift apart on which of them travel.
pub(crate) fn inline_out(x: crate::depth::DepthResult) -> DepthResultOut {
    DepthResultOut {
        width: x.width,
        height: x.height,
        maps: DepthMaps::Inline {
            depth_png_base64: x.depth_png_base64,
            confidence_png_base64: x.confidence_png_base64,
        },
        depth_min: x.depth_min,
        depth_max: x.depth_max,
        confidence_min: x.confidence_min,
        confidence_max: x.confidence_max,
    }
}

/// The file index each result's maps landed on. Mirrors the push order above —
/// depth, then confidence only if there was one — so a V2 batch (one file per
/// result) does not shift every url after the first onto the wrong map.
fn stored_maps(at: &mut usize, has_confidence: bool, id: &str) -> DepthMaps {
    let depth_url = generation_file_url(id, *at);
    *at += 1;
    let confidence_url = has_confidence.then(|| {
        let url = generation_file_url(id, *at);
        *at += 1;
        url
    });
    DepthMaps::Stored {
        depth_url,
        confidence_url,
    }
}

/// `Some(id)` = the maps were filed and are addressed by url; `None` = nothing
/// was stored, so they come back inline.
fn build_response(r: DepthResponse, generation_id: Option<String>) -> DepthRouteResponse {
    let mut at = 0usize;
    let results = r
        .results
        .into_iter()
        .map(|x| {
            let has_confidence = x.confidence_png_base64.is_some();
            let mut out = inline_out(x);
            if let Some(id) = generation_id.as_deref() {
                out.maps = stored_maps(&mut at, has_confidence, id);
            }
            out
        })
        .collect();
    DepthRouteResponse {
        count: r.count,
        results,
        model: r.model,
        depth_polarity: r.depth_polarity,
        backend: r.backend,
        elapsed_seconds: r.elapsed_seconds,
        generation_id,
    }
}

/// One image in, one inline map pair out, nothing filed — `/summon`'s `depth`
/// stage, which wants the maps for a sprite it is streaming rather than a
/// gallery row to go and read.
pub(crate) async fn depth_inline_one(
    state: &AppState,
    filename: &str,
    png: Bytes,
) -> Result<DepthResultOut, String> {
    if !state.depth.is_configured() {
        return Err("depth service not configured (set DEPTH_SERVICE_URL)".to_string());
    }
    let images = [(filename.to_string(), png)];
    let r = state
        .depth
        .depth_batch(&images, &DepthOptions::default())
        .await
        .map_err(|e| format!("vision-server: {e}"))?;
    r.results
        .into_iter()
        .next()
        .map(inline_out)
        .ok_or_else(|| "vision-server returned no maps".to_string())
}

/// `GET /api/v1/depth/health` — ungated liveness probe (UI bootstrap).
async fn health(State(state): State<AppState>) -> Response {
    if !state.depth.is_configured() {
        return Json(json!({
            "healthy": false,
            "worker_alive": false,
            "configured": false,
        }))
        .into_response();
    }
    match state.depth.health().await {
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

/// The cheap half of the gate. A hit skips the session and api_keys lookups
/// entirely rather than falling through to them: the doorstop is a shared secret
/// in its own header and grants nothing anywhere else, so there is nothing for a
/// second opinion to add.
fn doorstop_ok(headers: &HeaderMap) -> bool {
    key_ok(headers)
}

/// Shared with `/summon`, which runs a depth pass of its own: a caller that can
/// reach the streaming summon is by construction a caller that can reach this
/// route, so the two take the same credentials.
pub(crate) async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if doorstop_ok(headers)
        || require_auth(&state.db, state.api_key.as_deref(), headers)
            .await
            .is_ok()
    {
        return None;
    }
    Some(err(StatusCode::UNAUTHORIZED, "unauthorized"))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::DepthResult;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    /// The doorstop is an ALTERNATIVE, not a replacement: it only ever
    /// short-circuits when `ONESHOT_API_KEY` is configured — unset in this test
    /// binary, same as in CI — so every request here, doorstop header included,
    /// has to fall through to `require_auth`.
    #[test]
    fn with_no_doorstop_configured_everything_falls_through_to_require_auth() {
        for bad in [
            &[][..],
            &[("x-api-key", "paperworld-shoddy-key")],
            &[("x-api-key", "hunter2")],
            &[("authorization", "Bearer a-real-koblem-key")],
            &[
                ("x-api-key", ""),
                ("authorization", "Bearer a-real-koblem-key"),
            ],
        ] {
            assert!(
                !doorstop_ok(&headers(bad)),
                "{bad:?} must fall through to require_auth"
            );
        }
    }

    fn response(count: u32) -> DepthResponse {
        with_confidence(count, true)
    }

    fn with_confidence(count: u32, confident: bool) -> DepthResponse {
        DepthResponse {
            count,
            results: (0..count)
                .map(|i| DepthResult {
                    width: 100 + i,
                    height: 200 + i,
                    depth_png_base64: format!("depth{i}"),
                    depth_min: 0.25,
                    depth_max: 9.5,
                    confidence_png_base64: confident.then(|| format!("conf{i}")),
                    confidence_min: confident.then_some(0.1),
                    confidence_max: confident.then_some(0.9),
                })
                .collect(),
            model: Some(
                match confident {
                    true => "depth-anything-3",
                    false => "depth-anything-v2",
                }
                .to_string(),
            ),
            depth_polarity: Some(
                match confident {
                    true => "distance",
                    false => "disparity",
                }
                .to_string(),
            ),
            backend: Some("gpu".to_string()),
            elapsed_seconds: Some(1.5),
        }
    }

    fn json_of(r: DepthResponse, generation_id: Option<&str>) -> serde_json::Value {
        serde_json::to_value(build_response(r, generation_id.map(str::to_string))).unwrap()
    }

    /// The default, and what the gallery UI reads.
    #[test]
    fn a_stored_batch_answers_with_urls_pairwise() {
        let v = json_of(response(2), Some("gen-1"));
        assert_eq!(v["generation_id"], "gen-1");
        assert_eq!(v["count"], 2);
        assert_eq!(
            v["results"][0]["depth_url"],
            "/api/v1/generations/gen-1/file/0"
        );
        assert_eq!(
            v["results"][0]["confidence_url"],
            "/api/v1/generations/gen-1/file/1"
        );
        assert_eq!(
            v["results"][1]["depth_url"],
            "/api/v1/generations/gen-1/file/2"
        );
        assert_eq!(
            v["results"][1]["confidence_url"],
            "/api/v1/generations/gen-1/file/3"
        );
        for k in ["depth_png_base64", "confidence_png_base64"] {
            assert!(v["results"][0].get(k).is_none(), "stored shape leaked {k}");
        }
    }

    /// V2 files ONE map per result, so the urls are consecutive rather than
    /// every-other. A fixed stride here would hand result 1 result 2's depth.
    #[test]
    fn a_stored_batch_without_confidence_packs_the_urls_consecutively() {
        let v = json_of(with_confidence(3, false), Some("gen-1"));
        assert_eq!(v["model"], "depth-anything-v2");
        assert_eq!(v["depth_polarity"], "disparity");
        for (i, expected) in ["0", "1", "2"].iter().enumerate() {
            assert_eq!(
                v["results"][i]["depth_url"],
                format!("/api/v1/generations/gen-1/file/{expected}")
            );
            assert!(
                v["results"][i].get("confidence_url").is_none(),
                "a model with no confidence head must not claim a url for one"
            );
        }
    }

    /// The absence has to survive the whole way out, in both shapes — a zero or
    /// a one would read as a model that was uniformly sure.
    #[test]
    fn a_missing_confidence_map_is_absent_and_never_defaulted() {
        for id in [Some("gen-1"), None] {
            let v = json_of(with_confidence(1, false), id);
            let r = &v["results"][0];
            for k in [
                "confidence_url",
                "confidence_png_base64",
                "confidence_min",
                "confidence_max",
            ] {
                assert!(r.get(k).is_none(), "{id:?} leaked {k}: {r}");
            }
            assert_eq!(r["depth_min"], 0.25, "{id:?}");
        }
        let ranges = depth_ranges(&with_confidence(1, false));
        assert!(ranges[0].get("confidence_min").is_none(), "{ranges:?}");
    }

    /// The opt-in: the pixels themselves, and no evidence they ever existed.
    #[test]
    fn an_inline_batch_answers_with_the_pngs_and_no_generation() {
        let v = json_of(response(2), None);
        assert!(v.get("generation_id").is_none());
        assert_eq!(v["results"][0]["depth_png_base64"], "depth0");
        assert_eq!(v["results"][0]["confidence_png_base64"], "conf0");
        assert_eq!(v["results"][1]["depth_png_base64"], "depth1");
        assert_eq!(v["results"][1]["confidence_png_base64"], "conf1");
        for k in ["depth_url", "confidence_url"] {
            assert!(v["results"][0].get(k).is_none(), "inline shape leaked {k}");
        }
    }

    /// The PNGs are min/max normalised, so a shape that drops the bounds is a
    /// picture of a depth map rather than a depth map.
    #[test]
    fn both_shapes_carry_the_dimensions_and_the_bounds() {
        for id in [Some("gen-1"), None] {
            let v = json_of(response(1), id);
            let r = &v["results"][0];
            assert_eq!(r["width"], 100, "{id:?}");
            assert_eq!(r["height"], 200, "{id:?}");
            assert_eq!(r["depth_min"], 0.25, "{id:?}");
            assert_eq!(r["depth_max"], 9.5, "{id:?}");
            assert_eq!(r["confidence_min"], 0.1, "{id:?}");
            assert_eq!(r["confidence_max"], 0.9, "{id:?}");
            assert_eq!(v["depth_polarity"], "distance", "{id:?}");
        }
    }

    /// Opt-in: `inline` is only on when it is asked for, and the flag never
    /// reaches the gallery row it is the reason not to write.
    #[test]
    fn inline_is_off_unless_asked_for_and_stays_out_of_request_meta() {
        let parse = |q: &str| {
            let uri: axum::http::Uri = format!("/api/v1/depth?{q}").parse().unwrap();
            Query::<DepthQuery>::try_from_uri(&uri).unwrap().0.inline
        };
        assert!(parse("inline=true"));
        assert!(!parse("inline=false"));
        assert!(!parse("process_res_short=518"));
        assert!(!parse(""));

        let meta = meta_from_request(
            &DepthQuery {
                backend: Some("gpu".to_string()),
                model: Some(DepthModel::V3),
                process_res_short: None,
                process_res_long: Some(504),
                inline: true,
            },
            &[("had_image_inputs", json!(true))],
        );
        assert_eq!(meta["process_res_long"], 504);
        assert_eq!(meta["model"], "depth-anything-3");
        assert!(meta.get("inline").is_none(), "meta: {meta}");
    }

    /// The two families measure inference resolution on different axes, so the
    /// query string names the axis and a stale `process_res` binds to nothing
    /// here rather than silently meaning whichever one the model happens to use.
    #[test]
    fn the_resolution_axes_are_named_and_forwarded_separately() {
        let parse = |q: &str| {
            let uri: axum::http::Uri = format!("/api/v1/depth?{q}").parse().unwrap();
            let q = Query::<DepthQuery>::try_from_uri(&uri).unwrap().0;
            DepthOptions::from(q)
        };
        let v2 = parse("model=depth-anything-v2&process_res_short=518");
        assert_eq!(v2.model, Some(DepthModel::V2));
        assert_eq!(v2.process_res_short, Some(518));
        assert_eq!(v2.process_res_long, None);

        let v3 = parse("model=depth-anything-3&process_res_long=504");
        assert_eq!(v3.model, Some(DepthModel::V3));
        assert_eq!(v3.process_res_long, Some(504));
        assert_eq!(v3.process_res_short, None);

        let stale = parse("process_res=504");
        assert_eq!(stale.model, None, "no model means the server default, V2");
        assert_eq!(stale.process_res_short, None);
        assert_eq!(stale.process_res_long, None);
    }

    /// The stored path has to put the bounds somewhere the row can be read back
    /// from; the inline path has no row to put them in.
    #[test]
    fn stored_ranges_pair_the_bounds_with_their_dimensions() {
        let ranges = depth_ranges(&response(2));
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[1]["width"], 101);
        assert_eq!(ranges[1]["height"], 201);
        assert_eq!(ranges[1]["depth_max"], 9.5);
        assert_eq!(ranges[1]["confidence_min"], 0.1);
    }
}
