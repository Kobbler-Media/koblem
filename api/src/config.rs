use std::collections::HashMap;

use kob_gpu_gate::ServiceKind;

/// koblem configuration, loaded from environment variables.
pub struct Config {
    /// URL of the llama-server instance (default: http://localhost:8080)
    pub llama_server_url: String,
    /// URL of a SECOND llama-server instance hosting gemma-3-12b-it (+ the gemma
    /// vision mmproj) used as the "auto-director" planning LLM. Optional — when
    /// unset the auto-director falls back to the main `llama_server_url` (Qwen)
    /// so the `/plan` endpoint still works. Gemma is preferred because it is the
    /// LTX text encoder (speaks the LTX prompt space) AND a VLM (accepts the
    /// reference frames). From `DIRECTOR_LLM_SERVICE_URL`.
    pub director_llm_url: Option<String>,
    /// Beefy mode's target: Qwen3.8-27B across BOTH cards. `None` → the feature
    /// is not deployed here and the routes answer 501. See `beefy.rs`.
    pub beefy_llm_url: Option<String>,
    /// Gate kinds Beefy still admits beside the 27B, from `BEEFY_ALLOW`
    /// (comma-separated wire names, e.g. `stt,vision,tts,text_embed`). Unset →
    /// `text_embed` alone; set empty → the whole-box lock.
    pub beefy_allow: Vec<ServiceKind>,
    /// The 27B's measured footprint per card UUID, from `BEEFY_RESERVE_MB`
    /// (`<uuid>=<MiB>,<uuid>=<MiB>`). 🔴 A card left out is reserved WHOLE, so a
    /// typo'd UUID locks that card rather than opening it. Give the text embedder's
    /// card an entry (measured: ~15,250 on a 16 GiB 5060 Ti after a cold 195k-token
    /// request, leaving room for its 600 MiB line), or Beefy unloads it on entry.
    pub beefy_reserve_mb: HashMap<String, u32>,
    /// Softmax temperature over the label logprobs of `/api/v1/beefy/systemone`'s
    /// emulation, from `SYSTEMONE_TEMPERATURE`. Must match the engine's native
    /// route, or the two paths answer the same request differently.
    pub systemone_temperature: f64,
    /// Address to bind the HTTP server (default: 0.0.0.0:8090)
    pub bind_address: String,
    /// Context size configured on llama-server (informational, for /v1/models)
    pub context_size: u32,
    /// PostgreSQL connection URL
    pub database_url: String,
    /// Initial admin user email (creates user on startup if no users exist)
    pub admin_email: Option<String>,
    /// Initial admin user password
    pub admin_password: Option<String>,
    /// Environment variable API key (like kobload's KOBLOAD_API_KEY)
    pub api_key: Option<String>,
    /// NATS server URL for async review job consumption (optional).
    pub nats_url: Option<String>,
    /// `embed-vision`: the GPU EmbeddingGemma-2 llama-server (images, and text
    /// when `embed-text` is down). DB settings override this.
    pub vision_url: Option<String>,
    /// `embed-text`: the always-on EmbeddingGemma-2 text instance. DB settings
    /// override this.
    pub text_embed_url: Option<String>,
    /// `embed-text`'s `-ub`, from `TEXT_EMBED_MAX_TOKENS`: the longest input
    /// (prefix included) `/v1/text_embeddings` accepts unchunked.
    pub text_embed_max_tokens: usize,
    /// `tts-breeze` service base URL — the only TTS engine. DB settings
    /// override this.
    pub tts_breeze_url: Option<String>,
    /// Parakeet STT service base URL. DB settings override this.
    pub stt_url: Option<String>,
    /// acestep.cpp ace-server (music) base URL. DB settings override this.
    /// Optional — the music tab is disabled when unset.
    pub ace_server_url: Option<String>,
    /// songgen.cpp songgen-server (the heavier LeVo2 music engine) base URL. DB
    /// settings override this. Optional — the songgen music tab is hidden when
    /// unset. Time-shares the GPU with acestep under the same `music` gate.
    pub songgen_server_url: Option<String>,
    /// sa3-server (Stable Audio 3 port — the LIGHT music engine) base URL. DB
    /// settings override this. Optional — the sa3 tab is disabled when unset.
    /// Shares acestep's light `music` gate.
    pub sa3_server_url: Option<String>,
    /// MiniMax Music 3 base URL — the `comfy-api` shim's `music3` plugin (`POST
    /// /music3/v1/generate`, poll `GET /music3/v1/jobs/{id}`). DB settings override this. Empty ⇒
    /// the Music 3 tab reports "not configured".
    ///
    /// 🔴 Defaults to the SAME host as `minimax_h3_url` and `ltx25_url`, and that is not a
    /// copy-paste: `h3`, `ltx25` and `music3` are three plugins mounted on ONE comfy-api process in
    /// front of ONE ComfyUI, told apart by route prefix rather than by port. It is its own variable
    /// so Music 3 can be split onto another host later without touching the other two.
    ///
    /// 🔴 What it does NOT do: any form of audio conditioning. The DAV VAE cannot encode
    /// (ComfyUI's `sd.py` installs a `_no_encode`), so there is no a2a, inpaint, continuation,
    /// clone or reference timbre — every one of them is a 501 naming the reason. This is the only
    /// music engine on the surface with no audio input at all.
    pub music3_url: Option<String>,
    /// The upscaler's base URL — the `comfy-api` shim's `upscale` plugin (`POST
    /// /upscale/v1/generate`, poll `GET /upscale/v1/jobs/{id}`). DB settings override this.
    /// Empty ⇒ the Video → Upscale tab reports "not configured".
    ///
    /// 🔴 THE PREFIX IS ENGINE-NEUTRAL ON PURPOSE. It was `/seedvr2/v1` and the engine behind it
    /// changed within the day — LTX-2.5's own upsampler replaced SeedVR2 on measurements. The
    /// route names what the caller wants, not what happens to be answering.
    ///
    /// 🔴 The FOURTH plugin on the same comfy-api process as `h3`, `ltx25` and `music3` — one
    /// ComfyUI, one single-slot queue, told apart by route prefix rather than by port. Its own
    /// variable for the same reason Music 3 has one: so it can be split onto another host later.
    ///
    /// 🔴 What it does NOT do: anything generative. It is a RESTORER — no prompt, no negative, no
    /// reference. It takes an mp4 and gives a bigger mp4 back; every generative field is a 501
    /// naming the reason. The only LoRA is the detailer, as a boolean.
    ///
    /// ⚠️ It also needs [`Config::upscale_staging_dir`]: the source clip is handed over BY PATH, so
    /// koblem and comfy-api must both mount that directory at the same absolute path.
    pub upscale_url: Option<String>,
    /// The directory koblem writes a source clip into so comfy-api can read it by path.
    ///
    /// 🔴 THE SAME ABSOLUTE PATH IN BOTH CONTAINERS. A source video is routinely gigabytes; the
    /// `part:` route would buffer it in koblem, again in comfy-api's multipart parser and again in
    /// its probe. Handing over a path means the bytes never cross HTTP at all. Defaults to
    /// `/var/lib/koblem/upscale-staging`, which is on the same filesystem as `director-renders` so a take
    /// can be HARD-LINKED in rather than copied.
    pub upscale_staging_dir: String,
    /// Root of the queue's own on-disk working files — per-job segment directories and the clips
    /// engines stream out before they are filed in the gallery (`queue::files`). From
    /// `QUEUE_FILES_DIR`.
    ///
    /// ⚠️ Must be a VOLUME in compose. An upscale resumes from the segments already on disk, so a
    /// root that lives in the container image re-renders finished GPU work after every restart.
    pub queue_files_dir: String,
    /// Where `POST /api/v1/upscale/upload` captures a source clip, from `QUEUE_CAPTURE_DIR`.
    ///
    /// 🔴 Defaults to a subdirectory of [`Config::upscale_staging_dir`] and that is the whole
    /// point: the adapter links the capture into the shared mount at dispatch, and a hard link
    /// across filesystems degrades to copying a multi-gigabyte video. A subdirectory rather than
    /// the staging root itself so the retention sweep can only ever see captures.
    pub queue_capture_dir: String,
    /// flux2.cpp sd-server (FLUX.2-Klein image gen) base URL. DB settings
    /// override this. Optional — the images tab is disabled when unset.
    pub flux2_url: Option<String>,
    /// krea2 sd-server (Krea-2-Turbo text→image) base URL. The same sd.cpp fork
    /// as flux2 running a different checkpoint, offered as a second tab on the
    /// Images page. DB settings override this. Optional — the Krea2 tab reports
    /// "not configured" when unset. TEXT-TO-IMAGE ONLY (no edit model).
    pub krea2_url: Option<String>,
    /// LTX-2.5 base URL — the `comfy-api` shim's `ltx25` plugin, and the only LTX engine. It
    /// speaks the wire the retired C++ `ltx-video` service defined (multipart
    /// `POST /ltx/v1/generate`, poll `GET /sdcpp/v1/jobs/{id}`, `/segments/{n}`, `/media`,
    /// `/cancel`, `GET /sdapi/v1/loras`) on a ComfyUI runtime instead of the sd.cpp fork. DB
    /// settings override this. Empty ⇒ the video tab and every LTX Director project are disabled.
    ///
    /// 🔴 Defaults to the SAME host as `minimax_h3_url`, and that is not a
    /// copy-paste: `h3` and `ltx25` are two plugins mounted on ONE comfy-api
    /// process in front of ONE ComfyUI (see that service's README, "Two plugins,
    /// one ComfyUI"). They are told apart by route prefix, not by port. It is its
    /// own variable so the two can be split later without touching H3.
    ///
    /// 🔴 What it does NOT deliver, and what the 2.3 request document therefore
    /// carries fields for that go nowhere: no v2v/control frames, no Prompt Relay
    /// beats and no DiT variants — and the container is **mp4**, because ComfyUI
    /// v0.32.0's only audio-carrying save node is `SaveVideo`.
    ///
    /// ⚠️ It DOES keep a latent bank now (kobbler `6ca0b6f2`), so `resume_job_id`,
    /// `segments[i].bank_job_id` and `retake_from` are honoured — but only when
    /// `COMFY_API_LTX_BANK_DIR` is set on that container, and they are a 501
    /// naming the variable when it is not. That is a deployment fact rather than
    /// a capability, which is why it is asked live (`LtxVideoProps::bank_enabled`)
    /// instead of being decided here. What it still does not do is re-pin the
    /// shots after a retake — the tail is restored as it was.
    pub ltx25_url: Option<String>,
    /// MiniMax-H3 joint audio+video base URL — the `kobbler-h3-wangp` shim (a WanGP
    /// runtime, NOT the sd.cpp fork every other engine here is). Its own contract:
    /// `/h3/v1/generate` takes a `generations[]` list, and poll / media / per-generation
    /// media / cancel are `/h3/v1/jobs/…`. DB settings override this. Optional — a
    /// Director project can only select the `h3` engine when this is set.
    ///
    /// Live-settable through Settings (`set_base_url` re-registers on the gate), which is
    /// what makes an A/B between two H3 backends a config change rather than a deploy.
    pub minimax_h3_url: Option<String>,
    /// matting-server (vision.cpp BiRefNet / RMBG-2.0 background removal) base
    /// URL. Optional — the background-removal tab is disabled when unset.
    /// Ungated (no GpuLock): the model is tiny and idle-unloads on its own.
    pub matting_url: Option<String>,
    /// SPAR3D + RigAnything image-to-rig supervisor. DB settings override this
    /// optional URL so operators can relocate the isolated 3D worker.
    pub spar3d_url: Option<String>,
    /// **Pixal3D** — the `longcat-rig` `avatar_server` (photo → rigged LOD ladder → prompted
    /// animation). DB settings override this. Empty ⇒ the 3D → Pixal3D tab reports "not
    /// configured".
    ///
    /// 🔴 Its own service and its own budget line, NOT a second `spar3d_url`. The two solve the
    /// same problem, share nothing in implementation, and have very different appetites: spar3d
    /// reserves 2.6 GB, a Pixal3D rig peaks near 8.7 and refuses to start below an 11 GB floor
    /// rather than degrading to fit. It is pinned to the 3060 by UUID in compose.
    ///
    /// ⚠️ What it does NOT do: return in a browser's patience. The first tier is ~450 s. Every
    /// route in `routes::pixal3d` is 202-and-poll for that reason; there is no synchronous arm.
    pub pixal3d_url: Option<String>,
    /// The directory koblem writes a browser-edited rig into so `/v1/animate` can read it.
    ///
    /// 🔴 THE SAME ABSOLUTE PATH IN BOTH CONTAINERS, exactly like [`Config::upscale_staging_dir`].
    /// `/v1/animate` takes a rig as a job id or a path **the engine container can open**, and a GLB
    /// the user repaired in their browser is neither until it is written here.
    ///
    /// ⚠️ Missing mount ⇒ edits are still saved, previewed and downloadable; only *animating* one
    /// is refused, in those words. It is not a precondition for the tab working.
    pub pixal3d_staging_dir: String,
    /// vision-server `/depth` (Depth-Anything V2, or 3 on request) base URL. DB settings override
    /// this. Its OWN variable even though it defaults to the same host as
    /// `matting_url` today, so depth can move to another card/host later without
    /// touching matting. Set it to the empty string to disable the endpoint.
    pub depth_url: Option<String>,
    /// 🔴 **OFF by default: attach this generation's slice of the project song to the H3 prompt
    /// WRITER's user turn**, so the model hears the tone it is writing to instead of only reading
    /// about it. From `KOBLEM_H3_WRITER_SONG_AUDIO` (`1`/`true`/`yes`/`on`).
    ///
    /// An environment switch rather than a request field or a build-time constant, because it can
    /// then be flipped with a compose edit and a restart — no web rebuild, no document migration,
    /// and no per-project state to clean up if it turns out to be a bad idea.
    ///
    /// ⚠️ **The cost is UNMEASURED and that is why it is off.** ~14 s of audio through Gemma 4's
    /// mmproj is some unknown number of context tokens on a server configured for 73728, in a
    /// request that already carries a vendored guide, a reference roster and up to three images. An
    /// attachment that silently overflows the window would degrade every drafted prompt in a way
    /// that looks like the model getting worse, not like a flag being on. Measure it against
    /// `llm_logs.prompt_tokens` — with and without — before considering the default.
    ///
    /// ⚠️ Also requires an audio-capable mmproj on whatever `LLAMA_SERVER_URL` points at. The
    /// deployed Gemma 4 F16 mmproj is (`clip.has_audio_encoder = true`); a text/vision-only one
    /// answers `Failed to load image or audio file` and takes the draft with it.
    pub h3_writer_song_audio: bool,
}

/// Same container as matting today (`vision-server`, which also answers to the
/// `matting` network alias) — but reached by its own name so the two can split.
const DEFAULT_DEPTH_SERVICE_URL: &str = "http://vision-server:8898";

/// Same container as MiniMax-H3 today (`comfy-api`, which mounts the `h3` and
/// `ltx25` plugins on one ComfyUI) — but reached by its own name so the two can
/// split. Defaulted rather than left unset because the deployment that has an
/// LTX-2.5 to talk to is exactly the one already running comfy-api for H3; set
/// `LTX25_SERVICE_URL=` (empty) to take the engine off the menu.
const DEFAULT_LTX25_SERVICE_URL: &str = "http://comfy-api:8190";

/// Same container as LTX-2.5 and MiniMax-H3 today (`comfy-api`, which mounts the `h3`, `ltx25` and
/// `music3` plugins on one ComfyUI) — but reached by its own name so they can split. Defaulted for
/// the same reason as LTX-2.5: the deployment with a Music 3 to talk to is exactly the one already
/// running comfy-api. Set `MUSIC3_SERVICE_URL=` (empty) to take the tab off the menu.
const DEFAULT_MUSIC3_SERVICE_URL: &str = "http://comfy-api:8190";
/// 🔴 Must match the bind mount in BOTH compose files — kobbler's root `docker-compose.yml` mounts
/// it read-write here and `docker/comfyui/compose.yml` mounts the same host directory read-only at
/// the SAME absolute path on the engine side. `core/staging.py` opens the string this service
/// sends, verbatim; a mismatch is a 503 there naming the mount.
const DEFAULT_UPSCALE_STAGING_DIR: &str = "/var/lib/koblem/upscale-staging";

const DEFAULT_QUEUE_FILES_DIR: &str = "/var/lib/koblem/queue-files";

/// The longcat-rig service, which runs by default under compose on loopback :8102 and is reached
/// from koblem by its compose service name. Defaulted rather than left unset for the same reason
/// as LTX-2.5: the deployment that has a rig engine to talk to is the one already running it.
/// Set `PIXAL3D_SERVICE_URL=` (empty) to take the tab off the menu.
const DEFAULT_PIXAL3D_SERVICE_URL: &str = "http://longcat-rig:8102";

/// 🔴 Must match the bind mount in BOTH compose blocks — koblem-api mounts it read-write and
/// longcat-rig mounts the same host directory at the SAME absolute path. The engine opens the
/// string koblem sends, verbatim.
const DEFAULT_PIXAL3D_STAGING_DIR: &str = "/var/lib/koblem/pixal3d-staging";

const DEFAULT_VISION_SERVICE_URL: &str = "http://embed-vision:8080";
const DEFAULT_TEXT_EMBED_SERVICE_URL: &str = "http://embed-text:8080";
/// `embed-text`'s `-ub` beside Beefy on the 5060 Ti: 2048 OOMs on its first full
/// request there, so 1024 is the largest that fits. The count includes the
/// BOS/EOS `/tokenize` reports with `add_special`.
const DEFAULT_TEXT_EMBED_MAX_TOKENS: usize = 1024;

/// The always-on text embedder rides out Beefy unless the operator says
/// otherwise: every local service's search depends on it.
const DEFAULT_BEEFY_ALLOW: &str = "text_embed";

/// `BEEFY_ALLOW`. An unknown name is dropped with a warning — the fail-safe
/// direction, since a kind off the list is simply refused during Beefy.
fn parse_beefy_allow(raw: &str) -> Vec<ServiceKind> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|name| {
            let kind = ServiceKind::parse(name);
            if kind.is_none() {
                tracing::warn!(name, "BEEFY_ALLOW: unknown service kind, ignored");
            }
            kind
        })
        .collect()
}

/// `BEEFY_RESERVE_MB`. A malformed entry is dropped with a warning, which leaves
/// that card reserved whole — again the fail-safe direction.
fn parse_beefy_reserve(raw: &str) -> HashMap<String, u32> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let parsed = entry
                .split_once('=')
                .and_then(|(uuid, mb)| {
                    Some((uuid.trim().to_string(), mb.trim().parse::<u32>().ok()?))
                })
                .filter(|(uuid, _)| !uuid.is_empty());
            if parsed.is_none() {
                tracing::warn!(entry, "BEEFY_RESERVE_MB: expected <uuid>=<MiB>, ignored");
            }
            parsed
        })
        .collect()
}

impl Config {
    pub fn from_env() -> Self {
        let upscale_staging_dir = std::env::var("UPSCALE_STAGING_DIR")
            .unwrap_or_else(|_| DEFAULT_UPSCALE_STAGING_DIR.to_string());
        let queue_capture_dir = std::env::var("QUEUE_CAPTURE_DIR")
            .unwrap_or_else(|_| format!("{upscale_staging_dir}/captures"));
        Self {
            llama_server_url: std::env::var("LLAMA_SERVER_URL")
                .unwrap_or_else(|_| "http://localhost:8080".to_string()),
            beefy_llm_url: std::env::var("BEEFY_LLM_SERVICE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            beefy_allow: parse_beefy_allow(
                &std::env::var("BEEFY_ALLOW").unwrap_or_else(|_| DEFAULT_BEEFY_ALLOW.to_string()),
            ),
            beefy_reserve_mb: parse_beefy_reserve(
                &std::env::var("BEEFY_RESERVE_MB").unwrap_or_default(),
            ),
            systemone_temperature: std::env::var("SYSTEMONE_TEMPERATURE")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|t| t.is_finite() && *t > 0.0)
                .unwrap_or(1.0),
            director_llm_url: std::env::var("DIRECTOR_LLM_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            bind_address: std::env::var("BIND_ADDRESS")
                .unwrap_or_else(|_| "0.0.0.0:8090".to_string()),
            context_size: std::env::var("CONTEXT_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(131_072),
            database_url: std::env::var("DATABASE_URL").expect("DATABASE_URL must be set"),
            admin_email: std::env::var("KOBLEM_ADMIN_EMAIL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            admin_password: secret_env_value(
                std::env::var("KOBLEM_ADMIN_PASSWORD").ok(),
                "KOBLEM_ADMIN_PASSWORD",
            ),
            api_key: secret_env_value(std::env::var("KOBLEM_API_KEY").ok(), "KOBLEM_API_KEY"),
            nats_url: std::env::var("NATS_URL").ok(),
            vision_url: Some(
                std::env::var("VISION_SERVICE_URL")
                    .unwrap_or_else(|_| DEFAULT_VISION_SERVICE_URL.to_string()),
            )
            .filter(|s| !s.trim().is_empty()),
            text_embed_url: Some(
                std::env::var("TEXT_EMBED_SERVICE_URL")
                    .unwrap_or_else(|_| DEFAULT_TEXT_EMBED_SERVICE_URL.to_string()),
            )
            .filter(|s| !s.trim().is_empty()),
            text_embed_max_tokens: std::env::var("TEXT_EMBED_MAX_TOKENS")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_TEXT_EMBED_MAX_TOKENS),
            tts_breeze_url: std::env::var("TTS_BREEZE_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            stt_url: std::env::var("STT_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            ace_server_url: std::env::var("ACE_SERVER_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            songgen_server_url: std::env::var("SONGGEN_SERVER_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            sa3_server_url: std::env::var("SA3_SERVER_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            music3_url: Some(
                std::env::var("MUSIC3_SERVICE_URL")
                    .unwrap_or_else(|_| DEFAULT_MUSIC3_SERVICE_URL.to_string()),
            )
            .filter(|s| !s.trim().is_empty()),
            upscale_url: Some(
                std::env::var("UPSCALE_SERVICE_URL")
                    .unwrap_or_else(|_| DEFAULT_MUSIC3_SERVICE_URL.to_string()),
            )
            .filter(|s| !s.trim().is_empty()),
            upscale_staging_dir,
            queue_files_dir: std::env::var("QUEUE_FILES_DIR")
                .unwrap_or_else(|_| DEFAULT_QUEUE_FILES_DIR.to_string()),
            queue_capture_dir,
            flux2_url: std::env::var("FLUX2_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            krea2_url: std::env::var("KREA2_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            ltx25_url: Some(
                std::env::var("LTX25_SERVICE_URL")
                    .unwrap_or_else(|_| DEFAULT_LTX25_SERVICE_URL.to_string()),
            )
            .filter(|s| !s.trim().is_empty()),
            minimax_h3_url: std::env::var("MINIMAX_H3_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            matting_url: std::env::var("MATTING_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            spar3d_url: std::env::var("SPAR3D_SERVICE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            pixal3d_url: Some(
                std::env::var("PIXAL3D_SERVICE_URL")
                    .unwrap_or_else(|_| DEFAULT_PIXAL3D_SERVICE_URL.to_string()),
            )
            .filter(|s| !s.trim().is_empty()),
            pixal3d_staging_dir: std::env::var("PIXAL3D_STAGING_DIR")
                .unwrap_or_else(|_| DEFAULT_PIXAL3D_STAGING_DIR.to_string()),
            depth_url: Some(
                std::env::var("DEPTH_SERVICE_URL")
                    .unwrap_or_else(|_| DEFAULT_DEPTH_SERVICE_URL.to_string()),
            )
            .filter(|s| !s.trim().is_empty()),
            h3_writer_song_audio: env_flag("KOBLEM_H3_WRITER_SONG_AUDIO"),
        }
    }
}

/// An opt-in boolean environment switch. Absent, empty and anything unrecognised all read as OFF —
/// a flag whose cost is unmeasured must never turn itself on because somebody wrote `KOBLEM_X=maybe`.
fn env_flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// A secret read from the environment: blank filters to `None` like every other
/// optional URL in this file, but the literal placeholder value shipped in
/// `.env.example` (`CHANGEME`, any case) is refused outright rather than
/// silently accepted as a real secret — a copied-but-unedited `.env` must fail
/// loudly at boot, not start up with a secret an attacker can read from this
/// repo's own example file.
fn secret_env_value(raw: Option<String>, var_name: &str) -> Option<String> {
    let v = raw?.trim().to_string();
    if v.is_empty() {
        return None;
    }
    if v.eq_ignore_ascii_case("changeme") {
        panic!(
            "{var_name} is set to the placeholder value \"CHANGEME\" from .env.example — set a \
             real value, or unset it to disable whatever it gates"
        );
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_or_unset_secret_is_none() {
        assert_eq!(secret_env_value(None, "KOBLEM_API_KEY"), None);
        assert_eq!(
            secret_env_value(Some("".to_string()), "KOBLEM_API_KEY"),
            None
        );
        assert_eq!(
            secret_env_value(Some("   ".to_string()), "KOBLEM_API_KEY"),
            None
        );
    }

    #[test]
    fn a_real_secret_passes_through_trimmed() {
        assert_eq!(
            secret_env_value(Some("  s3cr3t  ".to_string()), "KOBLEM_API_KEY"),
            Some("s3cr3t".to_string())
        );
    }

    #[test]
    #[should_panic(expected = "KOBLEM_API_KEY")]
    fn the_changeme_placeholder_is_refused() {
        secret_env_value(Some("CHANGEME".to_string()), "KOBLEM_API_KEY");
    }

    #[test]
    #[should_panic(expected = "KOBLEM_ADMIN_PASSWORD")]
    fn the_changeme_placeholder_is_refused_case_insensitively() {
        secret_env_value(Some("changeme".to_string()), "KOBLEM_ADMIN_PASSWORD");
    }
}
