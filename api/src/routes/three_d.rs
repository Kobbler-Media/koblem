//! Game-asset image → rig route. The uploaded image is optionally cut out with
//! RMBG-2.0, then sent to SPAR3D + RigAnything. Koblem persists the resulting
//! self-contained skinned GLB in the normal generation gallery.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use koblem_client::types::{ImageToRigProps, ImageToRigRequest, ImageToRigResponse};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::routes::depth;
use crate::routes::tier2_helpers::{
    OutputBytes, extract_caller, generation_file_url_by_kind, generations_root, persist_generation,
};
use crate::{AppState, matting::RemoveOptions};

const BODY_LIMIT: usize = 64 * 1024 * 1024;

/// The gallery bucket these generations are stored under. Named so the file
/// route can tell a game asset from every other kind of generation when it
/// decides whether the doorstop is enough to read one.
pub(crate) const SERVICE: &str = "three_d";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/3d/generate", post(generate))
        .route("/api/v1/3d/props", get(props))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
}

/// Same credentials as depth, parts and `/summon`: a real koblem credential OR
/// the paperworld doorstop. This route is the fourth leg of that game's summon
/// — it is reached with the same already-matted sprite, from the same server,
/// in the same render — so requiring a provisioned `Authorization: Bearer` here
/// and nowhere else on that path only ever meant a 401 partway through a summon.
async fn auth_gate(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    depth::auth_gate(state, headers).await
}

async fn props(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    let healthy = state
        .spar3d
        .health()
        .await
        .map(|h| h.status == "ok")
        .unwrap_or(false);
    Json(ImageToRigProps {
        configured: state.spar3d.is_configured(),
        healthy,
        supports_project_front: true,
        supports_two_sided_mirror: true,
        source_texture_resolutions: vec![512, 1024],
    })
    .into_response()
}

async fn generate(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    if let Some(resp) = auth_gate(&state, &headers).await {
        return resp;
    }
    if generations_root(&state, SERVICE).await.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "gallery storage is not configured — set `generations_dir` in Settings",
        );
    }
    let mp = match Multipart::from_request(request, &state).await {
        Ok(mp) => mp,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
    };
    let (req, filename, image) = match parse_multipart(mp).await {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, &msg),
    };
    let original_image = image.clone();
    if let Some(res) = req.source_texture_resolution
        && !matches!(res, 512 | 1024)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "source_texture_resolution must be 512 or 1024",
        );
    }
    if let Some(parts) = &req.semantic_parts
        && (parts.is_empty()
            || parts.iter().any(|part| {
                part.name.trim().is_empty()
                    || part.motion.trim().is_empty()
                    || !(1..=8).contains(&part.count)
            }))
    {
        return err(
            StatusCode::BAD_REQUEST,
            "semantic_parts requires non-empty name/motion and count 1 through 8",
        );
    }
    let (input, matting_elapsed_seconds) = if req.remove_background {
        if !state.matting.is_configured() {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "background removal is enabled but RMBG-2.0 is not configured",
            );
        }
        // Raw PNG, not `png_base64` (§5a-a). Both ends of this hop are ours, and an
        // image-to-rig call with cutout enabled used to encode and decode the same picture
        // twice — once here and once on the way to SPAR3D.
        match state
            .matting
            .remove_single_raw(&filename, image, &RemoveOptions::default())
            .await
        {
            Ok(cutout) if cutout.png.is_empty() => {
                return err(StatusCode::BAD_GATEWAY, "RMBG-2.0 returned no cutout");
            }
            Ok(cutout) => (cutout.png, cutout.elapsed_seconds),
            Err(e) => {
                return err(
                    StatusCode::BAD_GATEWAY,
                    &format!("background removal failed: {e}"),
                );
            }
        }
    } else {
        (image, None)
    };
    let processed_image = input.clone();
    let texture_mode = req.texture_mode;
    match state.spar3d.image_to_rig(input, &req).await {
        Ok((glb, lods, worker_wall_seconds, metadata)) => {
            let lod_count = lods.len();
            let meta = json!({
                "seed": req.seed, "texture_mode": texture_mode,
                "source_texture_resolution": req.source_texture_resolution,
                "rig": req.rig,
                "remove_background": req.remove_background,
                "generate_lods": req.generate_lods,
                "semantic_parts": req.semantic_parts,
                "matting_elapsed_seconds": matting_elapsed_seconds,
                "worker_wall_seconds": worker_wall_seconds,
                "worker_metadata": metadata.clone(),
            });
            // §5a-b: the bundle is NOT persisted. It used to be zipped here and stored beside the
            // very files it contains, doubling the disk cost of every generation for an archive
            // that is a pure function of them. `GET /api/v1/generations/{id}/bundle` rebuilds it.
            let mut files = vec![
                OutputBytes {
                    ext: "glb".into(),
                    mime: "model/gltf-binary".into(),
                    kind: "model".into(),
                    bytes: glb,
                },
                OutputBytes {
                    ext: "png".into(),
                    mime: "image/png".into(),
                    kind: "source".into(),
                    bytes: original_image,
                },
                OutputBytes {
                    ext: "png".into(),
                    mime: "image/png".into(),
                    kind: "processed_source".into(),
                    bytes: processed_image,
                },
            ];
            for (level, lod) in lods.into_iter().enumerate() {
                files.push(OutputBytes {
                    ext: "glb".into(),
                    mime: "model/gltf-binary".into(),
                    kind: format!("model_lod{}", level + 1),
                    bytes: lod,
                });
            }
            let Some(generation_id) =
                persist_generation(&state, SERVICE, "image_to_rig", meta, &files).await
            else {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "GLB generated but saving to gallery failed",
                );
            };
            // §9.2: what this generation cost on the wire, on the same telemetry table every
            // other GPU route writes to. This route had NO log row at all, so an image-to-rig was
            // invisible to the GPU-log UI and to any before/after measurement of the transport.
            log_rig(
                &state,
                extract_caller(&headers),
                &req,
                worker_wall_seconds,
                lod_count,
                200,
                json!({
                    "generation_id": &generation_id,
                    "matting_elapsed_seconds": matting_elapsed_seconds,
                    "wire": state.spar3d.wire().as_meta(),
                }),
            );
            // §5a-c: addressed by what each output IS. The old `0` / `3 + i` / `files.len()`
            // arithmetic only held while this function assembled `files` in one exact order.
            Json(ImageToRigResponse {
                url: generation_file_url_by_kind(&generation_id, "model"),
                bundle_url: bundle_url(&generation_id),
                lod_urls: (1..=lod_count)
                    .map(|level| {
                        generation_file_url_by_kind(&generation_id, &format!("model_lod{level}"))
                    })
                    .collect(),
                generation_id,
                texture_mode,
                source_texture_resolution: req.source_texture_resolution,
                generate_lods: req.generate_lods,
                matting_elapsed_seconds,
                worker_wall_seconds,
                metadata,
            })
            .into_response()
        }
        Err(e) => err(
            StatusCode::BAD_GATEWAY,
            &format!("image-to-rig failed: {e}"),
        ),
    }
}

/// Where the on-demand bundle lives. Not a `file/{index}` URL: there is no stored output for it.
pub(crate) fn bundle_url(generation_id: &str) -> String {
    format!("/api/v1/generations/{generation_id}/bundle")
}

/// The bundle manifest, from the request meta the row already stores plus the hashes of the two
/// source images.
///
/// A pure function of persisted state, which is precisely what lets the zip stop being stored:
/// anything in here that could not be recomputed would have to live on disk forever.
fn manifest(
    meta: &serde_json::Value,
    original_sha: &str,
    processed_sha: &str,
    lod_files: &[String],
) -> serde_json::Value {
    let remove_background = meta["remove_background"].as_bool().unwrap_or(false);
    let semantic_parts = meta.get("semantic_parts").cloned().unwrap_or(json!(null));
    json!({
        "format": "paperworld-game-asset-v1",
        "model": "model.glb",
        "source_original": "source-original",
        "source_processed": if remove_background { "source-cutout.png" } else { "source-processed" },
        "source_original_sha256": original_sha,
        "source_processed_sha256": processed_sha,
        "texture_mode": meta.get("texture_mode").cloned().unwrap_or(json!(null)),
        "source_texture_resolution": meta.get("source_texture_resolution").cloned().unwrap_or(json!(null)),
        "rig": meta["rig"].as_bool().unwrap_or(false),
        "remove_background": remove_background,
        "semantic_parts": semantic_parts,
        "lod": {
            "included": !lod_files.is_empty(),
            "files": lod_files,
            "selection": "load a whole plain GLB by application distance; do not expect MSFT_lod or compression extensions",
        },
        "animation": if !semantic_parts.is_null() { "SAM3 matched confident named bones and emitted procedural motion recipes; when two swing parts map, the GLB also carries the loopable PaperworldWalk candidate clip." } else { "RigAnything skeleton included; animations are not authored by this pipeline." },
    })
}

fn readme(has_semantic_parts: bool) -> &'static str {
    if has_semantic_parts {
        "# Image-to-rig asset bundle\n\n`model.glb` is a self-contained textured, skinned GLB. SAM3 matched the supplied character-part declarations against visible skin weights and renamed only confident joints, taking each label from the noun you declared: a part declared once becomes `Head`, one declared twice becomes `<Part>.L` and `<Part>.R`. The suffix is assigned from the joint's position across the model, so it is consistent with the geometry and, for a front-facing character, with the source image; it is still not asserted anatomy. Each entry in the GLB's `asset.extras.paperworld_asset.rig.semantic_mappings` also carries the far end of its bone chain as `tip`, which is where to attach something a character holds. `motion_recipes` beside it gives one procedural hint per mapped bone, each with a measured unit `axis` — rotate about it for a swing, translate along it for a bob — expressed in the frame named by `space`. When two swing parts mapped, the GLB also contains a small looping `PaperworldWalk` candidate clip driving those same bones about those same axes; it is intentionally not a foot-plant or universal-gait claim. When present, `model.lod1.glb` and `model.lod2.glb` are independently loadable, skinned plain-glTF reductions for application-selected distance LOD, and carry the clip too. `asset-manifest.json` records the declarations and resulting mapping.\n"
    } else {
        "# Image-to-rig asset bundle\n\n`model.glb` is a self-contained textured, skinned GLB. When present, `model.lod1.glb` and `model.lod2.glb` are independently loadable, skinned plain-glTF reductions for application-selected distance LOD; do not rely on a GLB extension to select them. The files deliberately contain no MeshOpt, Draco, or mesh-quantization extensions for Bevy 0.19 portability. The RigAnything skeleton uses the generic `Bone_<joint_index>` naming contract and has no authored walk/fly/swim clips; attach your runtime's generic animation or retargeting system. `source-original` is the submitted image; `source-cutout.png` is the actual RMBG-2.0 input passed to SPAR3D when background removal was enabled. `asset-manifest.json` records names, hashes and processing choices.\n"
    }
}

/// Rebuild an image-to-rig generation's asset bundle from its persisted outputs (§5a-b).
///
/// Reads the files back off disk rather than keeping a copy: on a GLB and two PNGs that is a few
/// hundred milliseconds against permanently halving the store. Deflate on already-compressed
/// meshes and textures earns very little, so almost none of that is the compressor.
pub(crate) async fn bundle_for_generation(
    root: &std::path::Path,
    row: &koblem_client::types::GenerationRow,
) -> Result<Vec<u8>, String> {
    let read = |kind: &str| -> Option<std::path::PathBuf> {
        let output = row.outputs.iter().find(|o| o.kind == kind)?;
        let name = &output.filename;
        // Bare filename only — the same rule the file route enforces.
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return None;
        }
        Some(root.join(name))
    };
    let load = |kind: &'static str| async move {
        match read(kind) {
            Some(path) => tokio::fs::read(&path)
                .await
                .map(Bytes::from)
                .map_err(|e| format!("bundle: reading `{kind}` failed: {e}")),
            None => Err(format!("bundle: generation has no `{kind}` output")),
        }
    };
    let glb = load("model").await?;
    let original = load("source").await?;
    let processed = load("processed_source").await?;
    let mut lods: Vec<Bytes> = Vec::new();
    for level in 1..=2 {
        let kind = format!("model_lod{level}");
        if let Some(path) = read(&kind) {
            lods.push(
                tokio::fs::read(&path)
                    .await
                    .map(Bytes::from)
                    .map_err(|e| format!("bundle: reading `{kind}` failed: {e}"))?,
            );
        }
    }
    let lod_files: Vec<String> = (1..=lods.len())
        .map(|level| format!("model.lod{level}.glb"))
        .collect();
    let meta = &row.request_meta;
    let manifest = manifest(meta, &sha256(&original), &sha256(&processed), &lod_files);
    let has_parts = !meta
        .get("semantic_parts")
        .cloned()
        .unwrap_or(json!(null))
        .is_null();
    let remove_background = meta["remove_background"].as_bool().unwrap_or(false);
    build_bundle(
        &glb,
        &lods,
        &original,
        &processed,
        remove_background,
        &manifest,
        readme(has_parts),
    )
}

fn build_bundle(
    glb: &Bytes,
    lods: &[Bytes],
    original: &Bytes,
    processed: &Bytes,
    has_cutout: bool,
    manifest: &serde_json::Value,
    readme: &str,
) -> Result<Vec<u8>, String> {
    use std::io::Write as _;
    let mut out = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut out);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let manifest_json = serde_json::to_string_pretty(manifest).map_err(|e| e.to_string())?;
    let processed_name = if has_cutout {
        "source-cutout.png"
    } else {
        "source-processed"
    };
    zip.start_file("model.glb", opts)
        .map_err(|e| e.to_string())?;
    zip.write_all(glb).map_err(|e| e.to_string())?;
    for (level, lod) in lods.iter().enumerate() {
        let name = format!("model.lod{}.glb", level + 1);
        zip.start_file(name, opts).map_err(|e| e.to_string())?;
        zip.write_all(lod).map_err(|e| e.to_string())?;
    }
    for (name, bytes) in [
        ("source-original", original.as_ref()),
        (processed_name, processed.as_ref()),
        ("asset-manifest.json", manifest_json.as_bytes()),
        ("README.md", readme.as_bytes()),
    ] {
        zip.start_file(name, opts).map_err(|e| e.to_string())?;
        zip.write_all(bytes).map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

fn sha256(bytes: &Bytes) -> String {
    crate::media_sign::hex(&Sha256::digest(bytes))
}

async fn parse_multipart(mut mp: Multipart) -> Result<(ImageToRigRequest, String, Bytes), String> {
    let mut req = None;
    let mut image = None;
    while let Some(field) = mp.next_field().await.map_err(|e| e.to_string())? {
        match field.name() {
            Some("request") => {
                req = Some(
                    serde_json::from_slice::<ImageToRigRequest>(
                        &field.bytes().await.map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| format!("invalid request: {e}"))?,
                )
            }
            Some("image") => {
                let filename = field.file_name().unwrap_or("input.png").to_string();
                image = Some((filename, field.bytes().await.map_err(|e| e.to_string())?));
            }
            _ => {}
        }
    }
    let (filename, bytes) = image.ok_or_else(|| "missing `image` part".to_string())?;
    if bytes.is_empty() {
        return Err("image is empty".into());
    }
    Ok((req.unwrap_or_default(), filename, bytes))
}

/// Telemetry row for one image-to-rig, mirroring `log_krea2` / `log_flux`.
fn log_rig(
    state: &AppState,
    caller: Option<String>,
    req: &ImageToRigRequest,
    worker_wall_seconds: Option<f64>,
    lod_count: usize,
    status: i32,
    response_meta: serde_json::Value,
) {
    use crate::repositories::gpu_logs::CreateLog;
    use crate::routes::tier2_helpers::log_async;
    log_async(
        state.db.clone(),
        CreateLog {
            service: SERVICE.to_string(),
            endpoint: "image_to_rig".to_string(),
            request_meta: json!({
                "texture_mode": req.texture_mode,
                "source_texture_resolution": req.source_texture_resolution,
                "rig": req.rig,
                "remove_background": req.remove_background,
                "generate_lods": req.generate_lods,
                "lods": lod_count,
            }),
            response_meta,
            sample_id: None,
            // The GPU wait is inside the client's `image_to_rig` (it acquires, then submits), so
            // there is no separate gate figure to report here — unlike the routes that hold the
            // guard themselves. Reporting 0 rather than inventing one.
            gate_wait_ms: 0,
            duration_ms: worker_wall_seconds
                .map(|s| (s * 1000.0) as i64)
                .unwrap_or(0),
            status,
            caller,
        },
    );
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::{GenerationOutput, GenerationRow};

    /// A scratch directory that removes itself. No `tempfile` dependency in this crate, and the
    /// asset-store tests next door do the same thing by hand.
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("koblem-3d-bundle-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn row(
        dir: &std::path::Path,
        meta: serde_json::Value,
        kinds: &[(&str, &str)],
    ) -> GenerationRow {
        let outputs = kinds
            .iter()
            .map(|(kind, filename)| GenerationOutput {
                filename: (*filename).to_string(),
                mime: "application/octet-stream".into(),
                size_bytes: std::fs::metadata(dir.join(filename))
                    .map(|m| m.len() as i64)
                    .unwrap_or(0),
                kind: (*kind).to_string(),
            })
            .collect();
        GenerationRow {
            id: "gen-1".into(),
            service: SERVICE.into(),
            endpoint: "image_to_rig".into(),
            request_meta: meta,
            outputs,
            title: None,
            created_at: "2026-08-07T00:00:00Z".into(),
        }
    }

    /// The zip stopped being stored, so the ONLY thing standing between §5a-b and a silently
    /// different download is that the rebuild is byte-for-byte what the old inline build produced.
    /// Both halves run here against the same inputs, including the manifest and README that are
    /// now recomputed from the row's `request_meta` rather than read back off disk.
    #[tokio::test]
    async fn the_rebuilt_bundle_is_what_the_stored_one_was() {
        let dir = TempDir::new();
        let glb = Bytes::from_static(b"GLB\0 model bytes");
        let lod1 = Bytes::from_static(b"GLB\0 lod one");
        let lod2 = Bytes::from_static(b"GLB\0 lod two");
        let original = Bytes::from_static(b"\x89PNG original");
        let processed = Bytes::from_static(b"\x89PNG cutout");
        for (name, bytes) in [
            ("g-0.glb", &glb),
            ("g-1.png", &original),
            ("g-2.png", &processed),
            ("g-3.glb", &lod1),
            ("g-4.glb", &lod2),
        ] {
            std::fs::write(dir.path.join(name), bytes).expect("write");
        }
        let meta = json!({
            "seed": 7, "texture_mode": "project_front", "source_texture_resolution": 1024,
            "rig": true, "remove_background": true, "generate_lods": true,
            "semantic_parts": [{"name": "leg", "motion": "swing", "count": 2}],
        });

        // What the route used to store, built inline from the bytes it had in hand.
        let lod_files = vec!["model.lod1.glb".to_string(), "model.lod2.glb".to_string()];
        let expected = build_bundle(
            &glb,
            &[lod1, lod2],
            &original,
            &processed,
            true,
            &manifest(&meta, &sha256(&original), &sha256(&processed), &lod_files),
            readme(true),
        )
        .expect("inline build");

        let got = bundle_for_generation(
            &dir.path,
            &row(
                &dir.path,
                meta,
                &[
                    ("model", "g-0.glb"),
                    ("source", "g-1.png"),
                    ("processed_source", "g-2.png"),
                    ("model_lod1", "g-3.glb"),
                    ("model_lod2", "g-4.glb"),
                ],
            ),
        )
        .await
        .expect("rebuild");
        assert_eq!(got, expected);
    }

    /// LODs are optional, and the manifest says so in two places (`lod.included`, `lod.files`).
    /// A rebuild that assumed two LODs would either fail or lie about a generation that has none.
    #[tokio::test]
    async fn a_generation_without_lods_rebuilds_and_says_so() {
        let dir = TempDir::new();
        for name in ["g-0.glb", "g-1.png", "g-2.png"] {
            std::fs::write(dir.path.join(name), b"bytes").expect("write");
        }
        let meta = json!({
            "texture_mode": "generated", "rig": false,
            "remove_background": false, "generate_lods": false, "semantic_parts": null,
        });
        let zip = bundle_for_generation(
            &dir.path,
            &row(
                &dir.path,
                meta.clone(),
                &[
                    ("model", "g-0.glb"),
                    ("source", "g-1.png"),
                    ("processed_source", "g-2.png"),
                ],
            ),
        )
        .await
        .expect("rebuild");

        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("zip parses");
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        assert!(names.contains(&"model.glb".to_string()), "{names:?}");
        // No cutout was made, so the processed source keeps its neutral name.
        assert!(names.contains(&"source-processed".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("lod")), "{names:?}");

        use std::io::Read as _;
        let mut manifest_json = String::new();
        archive
            .by_name("asset-manifest.json")
            .expect("manifest present")
            .read_to_string(&mut manifest_json)
            .expect("manifest reads");
        let parsed: serde_json::Value = serde_json::from_str(&manifest_json).expect("json");
        assert_eq!(parsed["lod"]["included"], json!(false));
        assert_eq!(parsed["lod"]["files"], json!([]));
        assert_eq!(parsed["source_processed"], json!("source-processed"));
    }

    /// A row missing the model — a half-rolled-back persist, or a hand-edited row — must fail
    /// naming the kind, not hand back a zip with a hole in it.
    #[tokio::test]
    async fn a_bundle_over_a_missing_output_fails_naming_the_kind() {
        let dir = TempDir::new();
        std::fs::write(dir.path.join("g-1.png"), b"bytes").expect("write");
        let err = bundle_for_generation(
            &dir.path,
            &row(&dir.path, json!({}), &[("source", "g-1.png")]),
        )
        .await
        .expect_err("no model output");
        assert!(err.contains("model"), "{err}");
    }

    /// §5a-c in one assertion: the URLs a client is handed must not encode a position in the
    /// `files` vector, because inserting an output would then re-point every URL after it.
    #[test]
    fn the_response_urls_name_the_thing_not_its_index() {
        assert_eq!(
            generation_file_url_by_kind("gen-1", "model"),
            "/api/v1/generations/gen-1/file/by-kind/model"
        );
        assert_eq!(bundle_url("gen-1"), "/api/v1/generations/gen-1/bundle");
    }
}
