//! Unknown-field rejection for the render-submit bodies.
//!
//! # Why
//!
//! `kob video generate --engine wan` posted an LTX-shaped body to `/api/v1/wan-vace/generate`.
//! The route deserialises [`WanVaceGenerateRequest`], serde drops what it does not recognise, and
//! the request 202'd: the opening frame, the reference image and every sampling knob were
//! discarded, and a t2v clip came back from an i2v ask — discovered only after a full render. The
//! CLI is fixed; any other client can still do it.
//!
//! So the render submits deserialise through here instead: a body carrying a field the endpoint's
//! type does not have is a 400 in milliseconds, and the message NAMES the offending fields plus
//! the accepted field each most likely meant.
//!
//! # Why not `#[serde(deny_unknown_fields)]`
//!
//! `LtxVideoGenerateRequest` is also the shape of the `request` document a saved Director timeline
//! carries, and `routes/ltx_director.rs` re-parses that STORED document on every run and retake. A
//! `deny_unknown_fields` on the type would reject documents saved by an older build — a worse bug
//! than the one being fixed. The strictness therefore lives at the HTTP boundary only: the
//! internal re-parse keeps plain `serde_json::from_value` and is unaffected.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;

/// The render-submit parse: unknown-field rejection, and every `asset:<hash>` becomes a
/// `part:<name>` marker and its bytes come back alongside, to be attached to the engine request as
/// binary parts (§3).
///
/// The typed request's media fields therefore hold ~20-byte markers rather than megabytes of
/// base64, all the way from here to the multipart form — no artifact is ever a JSON string on this
/// path, and one asset used across ten shots is one upload.
///
/// A ref the store lost still fails here, naming the hashes ([`missing_refs_message`]) — the
/// property §3 says must be preserved exactly, because the engine's answer to an unusable image is
/// a body-less 400 that names nothing.
pub async fn stage_asset_json_strict<T: DeserializeOwned>(
    dir: &Path,
    raw: &[u8],
    endpoint: &str,
) -> Result<(T, crate::director_assets::StagedParts), String> {
    let mut value: Value = serde_json::from_slice(raw).map_err(|e| format!("bad json: {e}"))?;
    let staged = crate::director_assets::stage_refs(dir, &mut value, "a_")
        .await
        .map_err(|missing| crate::director_assets::missing_refs_message(&missing))?;
    Ok((from_value_strict(value, endpoint)?, staged))
}

/// Deserialize `value` as `T`, failing when it carries any field `T` does not have — at any depth,
/// so a foreign per-segment key is caught as well as a foreign top-level one.
pub fn from_value_strict<T: DeserializeOwned>(value: Value, endpoint: &str) -> Result<T, String> {
    let mut unknown: Vec<String> = Vec::new();
    let parsed: T = serde_ignored::deserialize(value, |path| unknown.push(path.to_string()))
        .map_err(|e| format!("bad request: {e}"))?;
    if unknown.is_empty() {
        return Ok(parsed);
    }
    Err(unknown_fields_message(
        endpoint,
        &unknown,
        accepted_fields::<T>(),
    ))
}

fn unknown_fields_message(endpoint: &str, unknown: &[String], accepted: &[&str]) -> String {
    let shown: Vec<String> = unknown
        .iter()
        .take(10)
        .map(|f| match closest_accepted(f, accepted) {
            Some(hint) => format!("unknown field `{f}`, expected `{hint}`"),
            None => format!("unknown field `{f}`"),
        })
        .collect();
    let more = unknown.len().saturating_sub(shown.len());
    format!(
        "{endpoint}: {}{} — this body was not built for this endpoint; the unmatched fields would \
         have been dropped and the render would not be the one you asked for",
        shown.join("; "),
        if more > 0 {
            format!("; +{more} more")
        } else {
            String::new()
        },
    )
}

/// The accepted field `unknown` most plausibly meant, or `None` when nothing is close enough.
/// Nested paths (`segments.0.image`) get no suggestion — `accepted` is the top level.
fn closest_accepted<'a>(unknown: &str, accepted: &[&'a str]) -> Option<&'a str> {
    if unknown.contains('.') {
        return None;
    }
    let budget = (unknown.len() / 2).max(2);
    accepted
        .iter()
        .map(|f| (edit_distance(unknown, f), *f))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, f)| (*d, f.len()))
        .map(|(_, f)| f)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row = prev.clone();
    for (i, ca) in a.chars().enumerate() {
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            row[j + 1] = sub.min(prev[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

/// The field names `T`'s derived `Deserialize` declares, taken from the derive itself so the list
/// can never drift from the type. A derived struct impl opens with `deserialize_struct(name,
/// FIELDS, visitor)`, so a deserializer that does nothing but fail out of that call hands back
/// `FIELDS`; anything that is not a plain struct yields an empty list and simply gets no
/// "expected" hint.
fn accepted_fields<T: DeserializeOwned>() -> &'static [&'static str] {
    match T::deserialize(FieldProbe) {
        Err(ProbeStop(fields)) => fields.unwrap_or(&[]),
        Ok(_) => &[],
    }
}

struct FieldProbe;

#[derive(Debug)]
struct ProbeStop(Option<&'static [&'static str]>);

impl std::fmt::Display for ProbeStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("field probe")
    }
}

impl std::error::Error for ProbeStop {}

impl serde::de::Error for ProbeStop {
    fn custom<M: std::fmt::Display>(_: M) -> Self {
        ProbeStop(None)
    }
}

impl<'de> serde::Deserializer<'de> for FieldProbe {
    type Error = ProbeStop;

    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, ProbeStop> {
        Err(ProbeStop(Some(fields)))
    }

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        _visitor: V,
    ) -> Result<V::Value, ProbeStop> {
        Err(ProbeStop(None))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

/// Bodies for the per-route rejection tests, built from the real request types so a rename cannot
/// leave the tests asserting against a shape no client sends.
#[cfg(test)]
pub(crate) mod test_bodies {
    use koblem_client::types::{
        LtxImageSource, LtxSegment, LtxVideoGenerateRequest, WanImageSource, WanSegment,
        WanVaceGenerateRequest, WanVaceMode,
    };

    /// An i2v LTX submit: the shape `kob video generate` sent to `wan-vace /generate`.
    pub(crate) fn ltx() -> Vec<u8> {
        serde_json::to_vec(&LtxVideoGenerateRequest {
            segments: vec![LtxSegment {
                prompt: "a woman turns to camera".to_string(),
                seconds: Some(4.0),
                ..Default::default()
            }],
            image_source: LtxImageSource::Upload,
            steps: Some(8),
            ..Default::default()
        })
        .expect("serialize")
    }

    /// The same render asked for in wan's own vocabulary.
    pub(crate) fn wan() -> Vec<u8> {
        serde_json::to_vec(&WanVaceGenerateRequest {
            mode: WanVaceMode::I2v,
            segments: vec![WanSegment {
                prompt: "a woman turns to camera".to_string(),
                seconds: Some(4.0),
            }],
            init_source: WanImageSource::Upload,
            frames: Some(65),
            ..Default::default()
        })
        .expect("serialize")
    }

    /// No `asset:<hash>` ref appears in either body, so no store is read.
    pub(crate) fn no_assets_dir() -> &'static std::path::Path {
        std::path::Path::new("/nonexistent-koblem-asset-store")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::{LtxVideoGenerateRequest, WanVaceGenerateRequest};
    use serde_json::json;

    #[test]
    fn accepted_fields_come_from_the_derive() {
        let wan = accepted_fields::<WanVaceGenerateRequest>();
        assert!(wan.contains(&"init_source"), "{wan:?}");
        assert!(!wan.contains(&"image_source"), "{wan:?}");
        assert!(accepted_fields::<LtxVideoGenerateRequest>().contains(&"image_source"));
    }

    /// The whole point of the rejection: the message must say which key was wrong AND what the
    /// endpoint wanted instead, or a client author still has to diff two schemas by hand.
    #[test]
    fn the_message_names_the_offending_field_and_its_intended_twin() {
        let msg = from_value_strict::<WanVaceGenerateRequest>(
            json!({"segments": [{"prompt": "a"}], "image_source": {"kind": "upload"}}),
            "wan-vace /generate",
        )
        .expect_err("an LTX image slot is not a wan field");
        assert!(msg.contains("wan-vace /generate"), "{msg}");
        assert!(msg.contains("unknown field `image_source`"), "{msg}");
        assert!(msg.contains("expected `init_source`"), "{msg}");
    }

    #[test]
    fn a_foreign_field_is_caught_inside_a_segment_too() {
        let msg = from_value_strict::<WanVaceGenerateRequest>(
            json!({"segments": [{"prompt": "a", "keyframes": []}]}),
            "wan-vace /generate",
        )
        .expect_err("LTX per-shot keys are not wan segment fields");
        assert!(msg.contains("segments.0.keyframes"), "{msg}");
    }

    #[test]
    fn a_field_with_no_plausible_twin_is_named_without_a_guess() {
        let msg = from_value_strict::<WanVaceGenerateRequest>(
            json!({"segments": [{"prompt": "a"}], "hires_chain": []}),
            "wan-vace /generate",
        )
        .expect_err("hires_chain is LTX-only");
        assert!(msg.contains("unknown field `hires_chain`"), "{msg}");
        assert!(!msg.contains("expected"), "{msg}");
    }
}
