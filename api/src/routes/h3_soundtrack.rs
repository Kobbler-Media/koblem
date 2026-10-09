//! Laying one song across a whole MiniMax-H3 job — the music-video path.
//!
//! ## Where this sits: it RESOLVES, it does not inject
//!
//! 🔴 The soundtrack is an ordinary audio reference that exists from the moment a song is attached,
//! authored into the document like any other, carrying [`H3_SOUNDTRACK_SENTINEL`] where an asset
//! hash would go. This pass runs before `stage_refs` and replaces that sentinel with a real
//! `asset:<hash>` holding THIS generation's span of the song. Nothing else changes.
//!
//! An earlier draft injected the reference here instead, and it could not work: `<Audio N>` ordinals
//! come from the reference list, so a reference that first appears at render time is one the prompt
//! — written long before — can never address. An unaddressed audio reference conditions far more
//! weakly than a declared one, and the failure is invisible: the render succeeds, the log says the
//! file attached, and the performers simply are not singing.
//!
//! Authoring it up front also means every audio rule already in the codebase applies unchanged:
//! ordinal allocation, `audio_count <= visual_count`, the `fully_copy` retention line, and — the
//! one that matters most for authoring — **taking a shot out of the song is ordinary reference
//! scoping.** There is no skip list, because the Director already has that control.
//!
//! ## The two halves, and why both are needed
//!
//! 1. **Conditioning.** Each generation gets its own slice as `<Audio N>` with the `fully_copy`
//!    retention marker, so performers sing the song and motion lands on the beat.
//! 2. **Delivery.** The original file is remuxed over what is delivered — see [`mux_soundtrack`].
//!
//! 🔴 Delivery means EVERY artifact, not just the finished cut, and that distinction was a real bug
//! rather than a hypothetical one. koblem delivers three things: the per-generation partials (which
//! the Director files as takes and `rebuild_cut` concatenates), the whole-job container of a full
//! run (its stitched cut), and a retake's one-generation container. Muxing only the last two left
//! every take and every rebuilt cut carrying H3's reconstruction — and because a retake harvests
//! only its partial, "render this shot" delivered the reconstruction every single time. Each
//! artifact is muxed at its own [`H3AudioSlice::delivered_at_seconds`].
//!
//! 🔴 `fully_copy` **does not copy**. It is a marker in the prompt's `retention_analysis` declaring
//! a relationship; the audio still comes out of the joint denoise, so H3 returns a RECONSTRUCTION
//! of the song. Delivering that would be delivering a lossy cover version. LTX has always worked
//! the same way and for the same reason (`ltx_video.rs`: drive with the upload, then remux, because
//! the model's audio is conditioning only).
//!
//! ## The clock
//!
//! `koblem_client::types::h3_plan_soundtrack` owns the arithmetic and its doc comment owns the
//! reasoning. The one thing worth repeating here: an anchored seam drops a frame at delivery, the
//! dropped frame is a DUPLICATE of the previous generation's last frame, and so the song moment
//! that would have played over it plays over its surviving twin instead. The overlay is therefore
//! continuous across the cut, and each generation's share of it starts at that generation's
//! `delivered_at_seconds` — which is seam-trim aware, and which the engine's partials already agree
//! with, because they arrive trimmed too (a 345-frame generation lands as 14.334 s, not 14.375 s).

use bytes::Bytes;
use serde_json::{Value, json};
use std::path::Path;
use tracing::warn;

use crate::director_assets;

use koblem_client::types::{
    H3_SOUNDTRACK_DEFAULT_DUCK_DB, H3_SOUNDTRACK_SENTINEL, H3AudioSlice, H3Soundtrack,
    H3SoundtrackMode, H3SoundtrackPlan, h3_frames_from_seconds, h3_plan_soundtrack,
};

/// What the pre-pass produced.
pub struct PreparedSoundtrack {
    /// The slice plan, for the job stamp and the drift guard.
    pub plan: H3SoundtrackPlan,
    /// Human-facing notes: generations rendering on their own bed, a song that outruns the cut.
    pub warnings: Vec<String>,
    /// Head frames the slice offsets ASSUMED the seams would drop — one `H3_TRIM_JOIN` per anchored
    /// generation. Checked against the shim's `result.trimmed_head_total`, which is the same
    /// quantity measured on the file that actually shipped. See [`delivery_notes`].
    pub expected_trim_frames: u32,
}

/// Does this generation carry an unresolved soundtrack reference?
///
/// The whole opt-out mechanism: a generation the author scoped the soundtrack away from simply has
/// no such reference, and gets no slice.
fn wants_a_slice(generation: &Value) -> bool {
    generation
        .get("references")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .any(|r| r.get("audio").and_then(Value::as_str) == Some(H3_SOUNDTRACK_SENTINEL))
        })
        .unwrap_or(false)
}

/// Read a generation list's `(frames, is_anchored_continuation, wants_a_slice)` — the planner's input.
///
/// `frames` is taken from the document when present and derived from the shot lengths otherwise,
/// exactly as the render will: the slice has to match what RENDERS, and a generation whose length
/// is implied rather than stated renders at the snap of its authored total.
/// `pub(crate)` so `ltx_director`'s tests can plan against the REAL consumer of `frames` and
/// `continuation` rather than a copy of this logic — a copy is what let the emitter and the planner
/// disagree in the first place.
pub(crate) fn generation_shape(generations: &[Value]) -> Vec<(u32, bool, bool)> {
    generations
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let frames = g
                .get("frames")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or_else(|| {
                    let seconds = g
                        .get("shots")
                        .and_then(Value::as_array)
                        .map(|shots| {
                            shots
                                .iter()
                                .filter_map(|s| s.get("seconds").and_then(Value::as_f64))
                                .sum::<f64>()
                        })
                        .unwrap_or(0.0);
                    h3_frames_from_seconds(seconds)
                });
            // 🔴 An ANCHORED continuation, not merely "not the first". An unanchored seam
            // reproduces nothing, so there is no duplicate frame for delivery to trim and the song
            // must not reach back over one. Mirrors `engine.py::_trim_for`.
            //
            // The default matters: the shim's `H3Continuation.anchor` defaults to true, so a
            // continuation present but silent about `anchor` IS anchored.
            let anchored = g
                .get("continuation")
                .and_then(Value::as_object)
                .map(|c| {
                    c.get("anchor").and_then(Value::as_bool).unwrap_or(true)
                        && c.get("mode").and_then(Value::as_str) != Some("none")
                })
                .unwrap_or(false);
            let _ = i;
            (frames, anchored, wants_a_slice(g))
        })
        .collect()
}

/// Resolve every [`H3_SOUNDTRACK_SENTINEL`] in `raw` to this generation's span of the song.
///
/// Each slice is written into the director asset store and the sentinel replaced with its
/// `asset:<hash>`, so the ordinary `stage_refs` walk that runs next turns it into a multipart part
/// with no knowledge that a soundtrack was involved.
///
/// Returns `Ok(None)` when the project has no soundtrack.
pub async fn prepare(
    soundtrack: Option<&H3Soundtrack>,
    song: Option<&Bytes>,
    raw: &mut Value,
    assets_dir: &Path,
    work_dir: &Path,
) -> Result<Option<PreparedSoundtrack>, String> {
    let Some(_soundtrack) = soundtrack else {
        return Ok(None);
    };
    let Some(song) = song else {
        return Err("soundtrack.audio did not resolve to any bytes".to_string());
    };

    // 🔴 A SCRATCH directory, removed at the end — not the asset store. These are ffmpeg's inputs
    // and outputs, not assets: the source copy is unhashed and the cut slices are already written
    // into the store under their own hashes below. Writing them beside real assets would leave
    // unaddressable junk in a content-addressed directory, growing by one song per render.
    let scratch = work_dir.join(format!(
        "h3-soundtrack-{}",
        director_assets::hash_bytes(song)
    ));
    tokio::fs::create_dir_all(&scratch)
        .await
        .map_err(|e| format!("creating the soundtrack scratch directory: {e}"))?;
    let result = prepare_in(song, raw, assets_dir, &scratch).await;
    // Best-effort: a scratch directory left behind is untidy, not incorrect, and must never turn a
    // successful slice into a failed render.
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    result
}

/// The body of [`prepare`], with a scratch directory guaranteed to exist and to be cleaned up.
async fn prepare_in(
    song: &Bytes,
    raw: &mut Value,
    assets_dir: &Path,
    work_dir: &Path,
) -> Result<Option<PreparedSoundtrack>, String> {
    let song_path = work_dir.join("soundtrack-source");
    tokio::fs::write(&song_path, song.as_ref())
        .await
        .map_err(|e| format!("staging the soundtrack: {e}"))?;
    let song_seconds = probe_duration(&song_path).await?;

    let generations = raw
        .get("generations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let plan = h3_plan_soundtrack(&generation_shape(&generations), song_seconds);

    let mut warnings = Vec::new();

    for slice in &plan.slices {
        if let Some(reason) = &slice.skipped {
            // 🔴 Only worth saying when the generation ASKED for a slice and could not have one. A
            // generation the author scoped the song away from is doing exactly what they told it to,
            // and warning about it every render would train them to ignore the list.
            if generations
                .get(slice.generation)
                .map(wants_a_slice)
                .unwrap_or(false)
            {
                warnings.push(format!(
                    "generation {}: no soundtrack conditioning — {reason}. It still plays under \
                     the muxed song; what it loses is performers singing to it.",
                    slice.generation + 1
                ));
            }
            continue;
        }
        let out = work_dir.join(format!("soundtrack-slice-{}.wav", slice.generation));
        cut_slice(&song_path, &out, slice).await?;
        let bytes = tokio::fs::read(&out)
            .await
            .map_err(|e| format!("reading soundtrack slice {}: {e}", slice.generation + 1))?;

        // Content-addressed like every other reference asset. Two generations that legitimately
        // want the same span therefore share one blob, which is correct: the bytes are identical and
        // `stage_refs` already dedupes by hash on the way to the wire.
        let hash = director_assets::hash_bytes(&bytes);
        director_assets::write_bytes(assets_dir, &hash, &bytes)
            .await
            .map_err(|e| {
                format!(
                    "storing the soundtrack slice for generation {}: {e}",
                    slice.generation + 1
                )
            })?;
        let marker = director_assets::make_ref(&hash);

        if let Some(refs) = raw
            .get_mut("generations")
            .and_then(Value::as_array_mut)
            .and_then(|g| g.get_mut(slice.generation))
            .and_then(|g| g.get_mut("references"))
            .and_then(Value::as_array_mut)
        {
            for r in refs.iter_mut() {
                if r.get("audio").and_then(Value::as_str) == Some(H3_SOUNDTRACK_SENTINEL) {
                    r["audio"] = json!(marker);
                }
            }
        }
    }

    if plan.song_tail_seconds > 0.5 {
        warnings.push(format!(
            "the song runs {:.2}s past the end of the timeline ({:.2}s delivered) — that tail is \
             never reached. Add shots, or trim the track.",
            plan.song_tail_seconds, plan.delivered_seconds
        ));
    }

    // What the SLICE OFFSETS assumed the seams would remove. Computed from the same
    // `generation_shape` the plan was built from, so the two can never disagree about which
    // generations are anchored — and the shim reports the same quantity as
    // `result.trimmed_head_total`, which makes the comparison exact rather than inferential.
    let expected_trim_frames = generation_shape(&generations)
        .iter()
        .filter(|(_, anchored, _)| *anchored)
        .count() as u32
        * koblem_client::types::H3_TRIM_JOIN;

    Ok(Some(PreparedSoundtrack {
        plan,
        warnings,
        expected_trim_frames,
    }))
}

/// What a rendered container actually contains — enough to tell a good take from a hollow one, and
/// enough to say WHICH STAGE hollowed it out.
///
/// 🔴 Written after a take shipped 344 frames of pure zeros. `final_seg_2_v3.mp4` (2026-08-10, job
/// `156446c9`) had a perfectly valid mp4 index — 344 packets with plausible, varied sizes — and a
/// video payload that was entirely `00` bytes. The decoder said `Invalid NAL unit size (0 > 329292)`
/// once per frame, every error value exactly the packet size minus its 4-byte length prefix. The
/// AAC track was real, which is why it played with sound and a black picture. koblem harvested it,
/// marked it `selected`, and shipped it; nothing anywhere checked that the bytes it had just
/// written could be decoded.
///
/// The job-level cut was clean, and the shim builds that from the same per-generation artifact
/// (`engine.py::joinable_media`), so the zeros were introduced on koblem's side of the wire — but
/// the artifacts were gone by the time anyone looked. Hence [`nonzero_ratio`](Self::nonzero_ratio)
/// and the hash: probe the SAME file before and after the mux and the pair says which stage did it.
#[derive(Debug, Clone)]
pub struct MediaProbe {
    pub bytes: u64,
    /// First 16 hex chars of the SHA-256 — enough to compare two stages, short enough for a log.
    pub digest: String,
    /// Fraction of sampled bytes that are non-zero.
    ///
    /// ⚠️ A THRESHOLD here would be wrong, which is why this is reported and never asserted on: a
    /// legitimately dark shot still carries entropy, and the hollow file was ~1.4% non-zero purely
    /// because its audio track was intact. Decodability is the verdict; this is the evidence.
    pub nonzero_ratio: f64,
    pub frames_decoded: u32,
    /// Empty when the head decoded cleanly.
    pub decode_error: String,
}

impl MediaProbe {
    /// Why this file must not ship, or `None` when it is fine.
    pub fn rejection(&self) -> Option<String> {
        if self.bytes == 0 {
            return Some("the file is empty".to_string());
        }
        if !self.decode_error.is_empty() {
            return Some(format!(
                "its video stream does not decode ({}); non-zero bytes {:.2}%, sha256 {}",
                self.decode_error,
                self.nonzero_ratio * 100.0,
                self.digest
            ));
        }
        if self.frames_decoded == 0 {
            return Some(format!(
                "no video frames could be decoded from {} bytes; non-zero bytes {:.2}%, sha256 {}",
                self.bytes,
                self.nonzero_ratio * 100.0,
                self.digest
            ));
        }
        None
    }
}

/// How many frames of the head to decode. One is enough to catch a hollow file — every frame of the
/// 2026-08-10 take failed — and a dozen is still well under a second of work on a 1152x640 clip,
/// which buys the case where only the head survived.
const PROBE_FRAMES: u32 = 12;

/// Read a container and report what is actually in it. See [`MediaProbe`].
///
/// Never returns `Err` for a file that merely FAILED to decode — that is a finding, not an error,
/// and it belongs in the report where the caller can log it. `Err` is reserved for not being able
/// to run the probe at all.
pub async fn probe_media(path: &Path) -> Result<MediaProbe, String> {
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| format!("reading {} to probe it: {e}", path.display()))?;
    let bytes = data.len() as u64;

    // Sampled rather than swept: a 40 MB take is read once here already, and the ratio only has to
    // separate "has content" from "is a hole". Windows are spread across the whole file so a header
    // and a trailing moov cannot make an empty middle look populated.
    const WINDOWS: usize = 64;
    const WINDOW: usize = 4096;
    let mut sampled = 0usize;
    let mut nonzero = 0usize;
    for i in 0..WINDOWS {
        let start = data.len().saturating_sub(WINDOW) * i / WINDOWS.max(1);
        let end = (start + WINDOW).min(data.len());
        if start >= end {
            break;
        }
        sampled += end - start;
        nonzero += data[start..end].iter().filter(|b| **b != 0).count();
    }
    let nonzero_ratio = if sampled == 0 {
        0.0
    } else {
        nonzero as f64 / sampled as f64
    };
    let digest = director_assets::hash_bytes(&data)
        .chars()
        .take(16)
        .collect::<String>();
    drop(data);

    // `-f null` decodes and discards. `-v error` means anything on stderr IS a decode failure, which
    // is what makes this a verdict rather than a heuristic.
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v"])
        .arg(PROBE_FRAMES.to_string())
        .args(["-f", "null", "-"])
        .output()
        .await
        .map_err(|e| format!("running ffmpeg to probe {}: {e}", path.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    // One line is all a log needs; the rest is the same message once per frame.
    // 🔴 Prefer a line from the VIDEO decoder. ffmpeg opens and decodes the audio stream while
    // probing even though only `0:v:0` is mapped (and `-an` does not stop it), so the first line is
    // often the audio's — a real refusal once reported itself as
    // *"its video stream does not decode ([aac @ …] channel element 0.0 duplicate)"*, which reads
    // like the guard rejected a take over its soundtrack. It had not: the same file also said
    // `Invalid NAL unit size (0 > 78140)`. The verdict was right and only the evidence was
    // mis-attributed, which is its own kind of wrong when the message is what the author acts on.
    let audio_line = |l: &&str| l.trim_start().starts_with("[aac");
    let decode_error = if out.status.success() && stderr.trim().is_empty() {
        String::new()
    } else {
        let mut lines = stderr.lines().filter(|l| !l.trim().is_empty()).peekable();
        let picked = stderr
            .lines()
            .find(|l| !l.trim().is_empty() && !audio_line(l))
            .or_else(|| lines.next())
            .unwrap_or("ffmpeg exited non-zero with no message");
        picked.trim().to_string()
    };
    let frames_decoded = if decode_error.is_empty() {
        PROBE_FRAMES
    } else {
        0
    };

    Ok(MediaProbe {
        bytes,
        digest,
        nonzero_ratio,
        frames_decoded,
        decode_error,
    })
}

/// `ffprobe` a file's duration in seconds.
async fn probe_duration(path: &Path) -> Result<f64, String> {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .await
        .map_err(|e| format!("ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffprobe could not read the soundtrack: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| "ffprobe returned no duration for the soundtrack".to_string())
}

/// Cut one slice to a mono-safe 16-bit PCM wav.
///
/// Re-encoded rather than stream-copied, and that is not laziness: `-c copy` on a compressed format
/// can only cut on a container packet boundary, so the slice would silently start up to a frame
/// early or late — and the whole point of the reach-back arithmetic is that the offset is exact.
/// PCM also sidesteps encoder priming, which is what would otherwise shift each slice by ~32 ms.
///
/// 🔴 `-ss` BEFORE `-i` is an input seek and would snap to a keyframe. It is placed after, as an
/// output seek, which decodes to the exact sample.
async fn cut_slice(song: &Path, out: &Path, slice: &H3AudioSlice) -> Result<(), String> {
    // 48 kHz stereo PCM: what the conditioner resamples from anyway, and lossless so the model
    // hears the track rather than an artefact of our own transcode.
    cut_span(
        song,
        out,
        slice.song_offset_seconds,
        slice.slice_seconds,
        48_000,
        2,
    )
    .await
    .map_err(|e| {
        format!(
            "{e} (soundtrack slice for generation {}, {:.3}s + {:.3}s)",
            slice.generation + 1,
            slice.song_offset_seconds,
            slice.slice_seconds
        )
    })
}

/// Cut `[offset, offset+seconds)` out of an audio file to PCM wav at a chosen rate/channel count.
///
/// The one place the seek discipline lives, extracted so a second consumer cannot get it subtly
/// different. Both callers depend on it:
///
/// 🔴 **Re-encoded, never `-c copy`.** On a compressed source `-c copy` can only cut on a container
/// packet boundary, so the span would silently start up to a frame early or late — and the whole
/// point of the reach-back arithmetic is that the offset is exact. PCM also sidesteps encoder
/// priming, which is what would otherwise shift each slice by ~32 ms.
///
/// 🔴 **`-ss` goes AFTER `-i`.** Before it, ffmpeg does an input seek and snaps to a keyframe.
/// Placed here it is an output seek, which decodes to the exact sample.
pub async fn cut_span(
    song: &Path,
    out: &Path,
    offset_seconds: f64,
    seconds: f64,
    sample_rate: u32,
    channels: u32,
) -> Result<(), String> {
    let status = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error"])
        .arg("-i")
        .arg(song)
        .args(["-ss", &format!("{:.6}", offset_seconds.max(0.0))])
        .args(["-t", &format!("{:.6}", seconds.max(0.0))])
        .args(["-ar", &sample_rate.to_string()])
        .args(["-ac", &channels.to_string()])
        .args(["-c:a", "pcm_s16le"])
        .arg(out)
        .status()
        .await
        .map_err(|e| format!("ffmpeg cutting an audio span: {e}"))?;
    if !status.success() {
        return Err("ffmpeg failed cutting an audio span".to_string());
    }
    Ok(())
}

/// Sample rate and channel count for a span cut FOR A LISTENER rather than for the renderer.
///
/// 16 kHz mono, and both halves are deliberate. Every speech/audio encoder in this class front-ends
/// on a 16 kHz log-mel filterbank — the deployed mmproj declares `clip.audio.num_mel_bins = 128` —
/// so anything above 16 kHz is resampled away before the model sees it, and a stereo pair is
/// downmixed. Sending 48 kHz stereo would be a ~6x larger base64 payload over the HTTP hop to
/// llama-server for bytes that are discarded on arrival.
///
/// ⚠️ NOT what the renderer gets. [`cut_slice`] stays 48 kHz stereo: that span is conditioning, and
/// the engine's own resampler is the one that should decide what to throw away.
pub const LISTENING_SPAN_RATE: u32 = 16_000;
pub const LISTENING_SPAN_CHANNELS: u32 = 1;

/// Lay the original song over a delivered artifact, starting `song_offset_seconds` into the song.
///
/// Returns the path actually delivered: the muxed file, or `clip` unchanged when the mode is
/// [`H3SoundtrackMode::Keep`] or the mux fails.
///
/// # The offset is what makes this reusable per GENERATION
///
/// 🔴 This used to overlay from t=0 unconditionally, which is right for one artifact only — the
/// whole-job container of a full run. Every OTHER thing koblem delivers starts somewhere else in
/// the song: a per-generation partial (which is what the Director files as a take, and what
/// `rebuild_cut` concatenates) begins at that generation's `delivered_at_seconds`, and a retake's
/// container holds one generation that is usually not the first. Muxing those from t=0 would lay
/// shot 1's music over shot 2. Passing the offset is what lets one function serve all three.
///
/// The caller takes the offset from [`H3AudioSlice::delivered_at_seconds`], which is already
/// seam-trim aware — and the engine's partials are trimmed the same way, so the two clocks agree
/// without further arithmetic.
///
/// 🔴 An `atrim` on decoded samples, NOT `-ss` before `-i`. Input seek on a compressed song snaps
/// to a packet boundary, which is the exact error `cut_slice` re-encodes to avoid on the
/// conditioning side; doing it loosely here would slide the delivered music against the picture by
/// up to a packet per generation. Skipped entirely at offset 0 so the full-run path ffmpeg
/// invocation is unchanged.
///
/// 🔴 A mux failure is NOT a job failure. The render is the expensive part and it succeeded; a
/// deliverable carrying the model's own audio is worse than intended but still a deliverable, and
/// failing the job here would throw away minutes of GPU time over an ffmpeg invocation. It is
/// logged loudly and reported in the job warnings instead.
pub async fn mux_soundtrack(
    clip: &Path,
    song: &Path,
    soundtrack: &H3Soundtrack,
    song_offset_seconds: f64,
    out: &Path,
) -> Result<bool, String> {
    if soundtrack.mode == H3SoundtrackMode::Keep {
        return Ok(false);
    }
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(["-y", "-loglevel", "error"]).arg("-i").arg(clip);
    cmd.arg("-i").arg(song);

    // Half a millisecond: below that the seek is a no-op the filter graph should not pay for, and
    // a negative offset is a caller bug that must not become `atrim=start=-1`.
    let offset = song_offset_seconds.max(0.0);
    let trimmed = offset > 0.0005;
    // The label the song arrives on, once any trim has been applied.
    let song_in = if trimmed { "[song]" } else { "[1:a]" };
    let trim_stage = if trimmed {
        // `asetpts` rebases the trimmed span to t=0, without which amix/-shortest would see a
        // stream that only starts producing samples `offset` seconds in.
        format!("[1:a]atrim=start={offset:.6},asetpts=PTS-STARTPTS[song];")
    } else {
        String::new()
    };

    match soundtrack.mode {
        H3SoundtrackMode::Replace => {
            // Video from the render, audio from the song, nothing of the model's own audio kept.
            //
            // 🔴 `apad` BEFORE `-shortest`, and it is load-bearing in the direction nobody tests.
            // `-shortest` ends the output at the shortest stream, so a song that runs out mid-clip
            // truncated the PICTURE: measured 2026-08-10, a 14.334 s generation muxed against a
            // song with 4.2 s left came out 4.209 s long — ten seconds of render destroyed, and
            // since the partial IS the harvested take, destroyed permanently. Padding the song with
            // silence makes its stream effectively endless, so `-shortest` can only ever end on the
            // video, which is the behaviour the flag was there for (a song LONGER than the cut must
            // not extend it with a frozen frame; that tail is warned about at plan time).
            //
            // A generation past the end of the song is not an error — the planner already declines
            // to CONDITION it (`the song ends N s into this generation`) and says so. It still plays
            // in the cut, now under silence instead of under a truncation.
            let filter = if trimmed {
                format!("[1:a]atrim=start={offset:.6},asetpts=PTS-STARTPTS,apad[a]")
            } else {
                "[1:a]apad[a]".to_string()
            };
            cmd.args(["-filter_complex", &filter]);
            cmd.args(["-map", "0:v:0", "-map", "[a]"]);
            cmd.args(["-c:v", "copy", "-c:a", "aac", "-b:a", "320k", "-shortest"]);
        }
        H3SoundtrackMode::Mix => {
            let duck = soundtrack
                .mix_duck_db
                .unwrap_or(H3_SOUNDTRACK_DEFAULT_DUCK_DB)
                .abs();
            // The model's own track attenuated under the song, then summed. `dropout_transition=0`
            // stops amix from ramping the gain back up when one input ends.
            let filter = format!(
                "{trim_stage}[0:a]volume={}dB[bed];[bed]{song_in}amix=inputs=2:duration=first:dropout_transition=0[a]",
                -duck
            );
            cmd.args(["-filter_complex", &filter]);
            cmd.args([
                "-map", "0:v:0", "-map", "[a]", "-c:v", "copy", "-c:a", "aac", "-b:a", "320k",
            ]);
        }
        H3SoundtrackMode::Keep => unreachable!("returned above"),
    }
    cmd.arg(out);

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("ffmpeg muxing the soundtrack: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg refused the soundtrack mux ({}): {}",
            soundtrack.mode.as_str(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(true)
}

/// Everything wrong with what the seams did to this cut, in the order it is worth hearing.
///
/// 🔴 **A duration is a symptom; the shim publishes the cause.** This used to be [`drift_note`]
/// alone: measure the delivered clip, compare it against the plan, and blame `H3_TRIM_JOIN` for any
/// gap. That reasoning has exactly one failure mode and it fired on 2026-08-11 — the shim's join
/// trim was CRASHING (an ffmpeg muxer error, every seam, every job), so the cut came back two
/// frames long and the banner told the author that a constant which was perfectly correct had
/// drifted. They are indistinguishable by duration and trivially distinguishable by asking: the
/// completion body carries `trim_failed` per generation and `trimmed_head_total` for the file.
///
/// So the order here is not stylistic. A failed trim EXPLAINS the length, and reporting the
/// length as well would be two notes about one fault pointing at two different culprits.
///
/// 1. **The engine failed to trim.** Its problem, named as its problem, with the seam indices.
/// 2. **The engine trimmed a different amount than the offsets assumed.** This is the real
///    `H3_TRIM_JOIN` drift, now measured in the units the constant is written in.
/// 3. **Neither, and the duration still disagrees.** The old backstop, kept for a shim too old to
///    publish the accounting — and for whatever neither of us has thought of.
// 🔴 UNCALLED SINCE THE H3 CUTOVER, AND DELIBERATELY KEPT. The seam guard needs the completion
// body's `H3TrimReport`, which now only exists inside `queue::adapters::h3`; the payload already
// carries `sliced_for_seconds` / `expected_trim_frames` so the three lines that consume them are all
// that is missing. Deleting it would delete the only check that catches koblem's mirrored
// `H3_TRIM_JOIN` drifting out of step with the shim's real `trim_join` — a drift where both the
// render and the mux succeed and the song merely slides a frame per seam.
#[allow(dead_code)]
pub fn delivery_notes(
    expected_trim_frames: u32,
    trim: &crate::minimax_h3::H3TrimReport,
    sliced_for_seconds: f64,
    delivered_seconds: f64,
) -> Vec<String> {
    if trim.any_trim_failed() {
        let seams = trim
            .trim_failed_generations
            .iter()
            .map(|g| (g + 1).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return vec![format!(
            "the engine could not trim the join at generation {seams}, so the delivered cut keeps \
             a duplicate frame at each of those seams — the picture stutters there and the muxed \
             song runs late by one frame per failed seam from that point on. This is an engine-side \
             failure, not a bad slice: the soundtrack plan was correct. Those generations also have \
             no take, because a file that cannot be trimmed is refused rather than served \
             untrimmed. Re-render."
        )];
    }
    // `frame_count_untrimmed` is 0 only when the shim published no accounting at all, which is the
    // one case where a zero `trimmed_head_total` is not evidence of anything.
    if trim.frame_count_untrimmed > 0 && trim.trimmed_head_total != i64::from(expected_trim_frames)
    {
        return vec![format!(
            "the engine removed {} head frame(s) across the seams but the soundtrack offsets were \
             computed for {expected_trim_frames} — koblem's H3_TRIM_JOIN ({}) no longer matches \
             the shim's `trim_join`, so the muxed song drifts against the picture along the cut. \
             Re-slice before delivering.",
            trim.trimmed_head_total,
            koblem_client::types::H3_TRIM_JOIN,
        )];
    }
    drift_note(sliced_for_seconds, delivered_seconds)
        .into_iter()
        .collect()
}

/// Compare the finished cut's real duration against the length the slices were cut for.
///
/// The BACKSTOP under [`delivery_notes`], not the first line of defence — it cannot tell a trim
/// rule that moved from a trim that crashed, and saying so is that function's job. Kept because it
/// needs nothing from the shim but the file, so it still speaks when the accounting fields do not.
#[allow(dead_code)]
pub fn drift_note(sliced_for_seconds: f64, delivered_seconds: f64) -> Option<String> {
    let fps = koblem_client::types::H3_FPS as f64;
    let delta = delivered_seconds - sliced_for_seconds;
    // Half a frame: below that the difference is container rounding, not a trim rule that moved.
    if delta.abs() < 0.5 / fps {
        return None;
    }
    let frames = delta * fps;
    // 🔴 States the OBSERVATION and stops. The previous wording named `H3_TRIM_JOIN` as the cause,
    // which it cannot know — this check reaches it only when the shim's own trim accounting agreed
    // with the plan or was absent, so by construction the one thing it can rule out is the cause it
    // used to name. A guard that guesses a culprit sends people to the wrong file.
    Some(format!(
        "the finished cut is {delivered_seconds:.3}s but the soundtrack was sliced for \
         {sliced_for_seconds:.3}s ({frames:+.1} frames), so the muxed song drifts against the \
         picture along the cut. The engine's seam accounting did not explain the difference — \
         check the delivered clip before shipping it."
    ))
}

/// The finished clip's real duration, seconds. Feeds [`drift_note`].
///
/// 🔴 The VIDEO stream's duration, not the container's, and the difference is the whole reason this
/// function exists instead of a direct `probe_duration` call. `delivered_seconds` is a count of
/// video frames over 24 fps; the container reports `max(video, audio)`, and the AAC track is longer
/// than the picture because the encoder pads its final partial frame. Measured on a clean 43 s cut:
/// container 43.0727 s, audio 43.0500 s, **video 43.0417 s — exactly what the plan said**.
///
/// So comparing the plan against the container was a guaranteed false positive of ~31 ms, which is
/// more than [`drift_note`]'s half-frame tolerance, and the guard fired on EVERY H3 render. It was
/// telling the truth about the number it was given and the number was the wrong one.
///
/// ⚠️ That mattered more than a noisy log: these warnings now surface in the Director as a banner,
/// so a permanent false positive would have trained the author to dismiss the one thing that tells
/// them a render did not deliver.
#[allow(dead_code)]
pub async fn clip_seconds(clip: &Path) -> Result<f64, String> {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(clip)
        .output()
        .await
        .map_err(|e| format!("ffprobe: {e}"))?;
    if out.status.success()
        && let Ok(secs) = String::from_utf8_lossy(&out.stdout).trim().parse::<f64>()
        && secs > 0.0
    {
        return Ok(secs);
    }
    // A container that does not carry a per-stream duration (or a clip with no video track at all)
    // falls back rather than failing: a coarser number still catches a trim rule that MOVED, which
    // is what the guard is for. It is only the sub-frame case it cannot speak to.
    probe_duration(clip).await
}

/// The suffix a generation's own generated audio is kept under, beside the delivered clip.
pub const MODEL_AUDIO_EXT: &str = "model.m4a";

/// Keep H3's OWN audio for a generation before the song is muxed over it.
///
/// 🔴 The audio track ALONE, not a second copy of the clip, and that is not a compromise —
/// [`mux_soundtrack`] stream-copies the video (`-c:v copy`), so the pre-mux and post-mux pictures
/// are byte-identical. The only thing the mux destroys is the audio, so the audio is the only thing
/// worth keeping; a full second container would duplicate ~40 MB per generation per take to
/// preserve ~600 KB of difference. The pre-mux clip is reconstructible losslessly at any time:
///
/// ```text
/// ffmpeg -i <take>.mp4 -i <take>.model.m4a -map 0:v:0 -map 1:a:0 -c copy premux.mp4
/// ```
///
/// Why keep it at all: in `replace` mode the delivered file carries the SONG, so there is otherwise
/// no way to hear what the model actually generated — and "did it respond to the audio it was
/// given" is the first question asked whenever a performance does not sync. Answering it used to
/// mean flipping the project to `keep` and re-rendering, which is ~20 minutes of GPU to recover
/// something we had already made and thrown away.
///
/// `-c:a copy`: a remux, no re-encode, so it is near-instant and lossless.
pub async fn extract_model_audio(clip: &Path, dest: &Path) -> Result<(), String> {
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(clip)
        .args(["-vn", "-c:a", "copy"])
        .arg(dest)
        .output()
        .await
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// Log and collect a mux failure without failing the job. See [`mux_soundtrack`].
pub fn note_mux_failure(job: &str, err: &str, warnings: &mut Vec<String>) {
    warn!(job = %job, error = %err, "h3 soundtrack mux failed; delivering the model's own audio");
    warnings.push(format!(
        "soundtrack mux failed, so this clip carries the model's own generated audio rather than \
         the supplied song: {err}"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verdict, with no ffmpeg in the loop — nothing else in this crate shells out during
    /// tests, and the decision logic is the part that must not drift.
    fn probe(decode_error: &str, frames: u32, nonzero: f64, bytes: u64) -> MediaProbe {
        MediaProbe {
            bytes,
            digest: "0123456789abcdef".to_string(),
            nonzero_ratio: nonzero,
            frames_decoded: frames,
            decode_error: decode_error.to_string(),
        }
    }

    #[test]
    fn a_take_that_decodes_is_accepted() {
        assert!(
            probe("", PROBE_FRAMES, 0.97, 36_057_823)
                .rejection()
                .is_none()
        );
    }

    /// 🔴 The 2026-08-10 failure, reproduced from the real artifact's own numbers:
    /// `final_seg_2_v3.mp4`, 40,506,708 bytes, **1.57% non-zero** under this exact sampling — the
    /// intact AAC track and nothing else — with a valid mp4 index over a zeroed video payload. The
    /// decoder raised this once per frame, the value each time being the packet size minus its
    /// 4-byte length prefix.
    ///
    /// The real file was run through the shipped probe before this test was written: it REFUSES
    /// that one and ACCEPTS all three healthy siblings from the same job (99.56-99.59% non-zero).
    #[test]
    fn a_hollow_take_is_refused_and_says_why() {
        let p = probe("Invalid NAL unit size (0 > 329292).", 0, 0.0157, 40_506_708);
        let reason = p.rejection().expect("must be refused");
        assert!(reason.contains("does not decode"), "{reason}");
        // The evidence travels with the verdict — without it the next occurrence is as
        // unattributable as the first one was.
        assert!(reason.contains("Invalid NAL unit size"), "{reason}");
        assert!(reason.contains("1.57%"), "{reason}");
        assert!(reason.contains("0123456789abcdef"), "{reason}");
    }

    #[test]
    fn an_empty_file_is_refused_before_anything_else_is_read_into_it() {
        assert_eq!(
            probe("", 0, 0.0, 0).rejection().as_deref(),
            Some("the file is empty")
        );
    }

    /// ⚠️ Darkness is NOT the test. A legitimately dark shot decodes, and refusing it would throw
    /// away good renders — the hollow take was caught by decodability, and the ratio is only ever
    /// evidence in the message.
    #[test]
    fn a_dark_but_decodable_take_is_kept() {
        assert!(
            probe("", PROBE_FRAMES, 0.004, 12_000_000)
                .rejection()
                .is_none()
        );
    }

    /// 🔴 A real refusal reported itself as *"its video stream does not decode ([aac @ …] channel
    /// element 0.0 duplicate)"* — ffmpeg decodes the audio stream while probing even with only
    /// `0:v:0` mapped, so the first stderr line was the audio's. The verdict was correct (the same
    /// file also raised `Invalid NAL unit size`); only the evidence was mis-attributed, and the
    /// evidence is what the author acts on.
    #[test]
    fn the_reported_error_is_the_video_one_not_the_audio_one() {
        // Both hollow-file occurrences put the aac line FIRST.
        let p = probe("Invalid NAL unit size (0 > 78140).", 0, 0.0467, 40_209_788);
        let reason = p.rejection().expect("must be refused");
        assert!(reason.contains("Invalid NAL unit size"), "{reason}");
        assert!(!reason.contains("aac"), "{reason}");
    }

    /// 🔴 The drift guard's tolerance, against the numbers that actually occur. `clip_seconds`
    /// measures the VIDEO stream because the container is `max(video, audio)` and AAC pads its last
    /// frame: on a clean 43 s cut that padding put the container 31 ms past the plan — over the
    /// half-frame tolerance — so the guard fired on every single render while the picture was
    /// perfectly in step.
    #[test]
    fn container_padding_is_not_drift() {
        let planned = 43.041_666;
        // What the container reports, and what the guard used to be handed.
        assert!(drift_note(planned, 43.072_673).is_some());
        // What the video stream reports, which is the quantity `planned` describes.
        assert!(drift_note(planned, 43.041_667).is_none());
        // A trim rule that genuinely moved is still caught: one frame per seam over two seams.
        assert!(drift_note(planned, planned + 2.0 / 24.0).is_some());
    }

    fn report(
        trimmed_head_total: i64,
        trim_failed_generations: Vec<u32>,
    ) -> crate::minimax_h3::H3TrimReport {
        crate::minimax_h3::H3TrimReport {
            trimmed_head_total,
            // Non-zero, so the accounting counts as PRESENT. See `delivery_notes`.
            frame_count_untrimmed: 1035,
            trim_failed_generations,
            engine_warnings: Vec::new(),
        }
    }

    /// 🔴 THE 2026-08-11 MISDIAGNOSIS, as a test. A muxer bug had disabled the shim's join trim
    /// entirely, so a 3-generation cut came back 2 frames long — and the guard, which had only a
    /// duration to reason from, told the author that `H3_TRIM_JOIN` had drifted. It had not: the
    /// constant was right, the trim was dead, and the message sent them to the wrong file while the
    /// real evidence (`trim_failed`, published per generation) went unread.
    ///
    /// The two situations produce the SAME duration. Only the shim's own account separates them.
    #[test]
    fn a_failed_trim_is_not_reported_as_a_drifted_constant() {
        let planned = 43.041_666;
        let delivered = planned + 2.0 / 24.0;

        // Trims 1 and 2 crashed: nothing was removed, and the shim says which seams.
        let notes = delivery_notes(2, &report(0, vec![1, 2]), planned, delivered);
        assert_eq!(notes.len(), 1, "one fault, one note: {notes:?}");
        assert!(notes[0].contains("could not trim the join"), "{}", notes[0]);
        assert!(
            notes[0].contains("generation 2, 3"),
            "1-based seams: {}",
            notes[0]
        );
        assert!(
            !notes[0].contains("H3_TRIM_JOIN"),
            "the constant is not the culprit and must not be named: {}",
            notes[0]
        );

        // Same duration, same plan — but the shim trimmed cleanly and simply trimmed a different
        // amount. NOW it is the constant, and the note says so in frames rather than seconds.
        let notes = delivery_notes(2, &report(0, vec![]), planned, delivered);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("H3_TRIM_JOIN"), "{}", notes[0]);
    }

    /// The clean case has to stay silent, or the banner trains the author to dismiss it — which is
    /// exactly what the container-padding false positive above was doing.
    #[test]
    fn a_cut_whose_seams_agree_with_the_plan_says_nothing() {
        let planned = 43.041_666;
        assert!(delivery_notes(2, &report(2, vec![]), planned, 43.041_667).is_empty());
    }

    /// A shim too old to publish the accounting must not read as "it trimmed nothing" — that would
    /// be a permanent false alarm on every render, the failure mode this guard has already had once.
    #[test]
    fn absent_accounting_falls_back_to_the_duration_check() {
        let planned = 43.041_666;
        let silent = crate::minimax_h3::H3TrimReport::default();
        assert!(delivery_notes(2, &silent, planned, 43.041_667).is_empty());
        // and the backstop still catches a real disagreement
        let notes = delivery_notes(2, &silent, planned, planned + 2.0 / 24.0);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(!notes[0].contains("H3_TRIM_JOIN"), "{}", notes[0]);
    }

    /// ffmpeg that ran, said nothing, and produced no frames is still a refusal: silence plus zero
    /// output is not evidence of a good file.
    #[test]
    fn no_frames_without_an_error_message_is_still_a_refusal() {
        let reason = probe("", 0, 0.5, 1_000)
            .rejection()
            .expect("must be refused");
        assert!(reason.contains("no video frames"), "{reason}");
    }
}
