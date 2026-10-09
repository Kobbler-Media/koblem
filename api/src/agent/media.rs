//! What `remove_background` and `transcribe` operate on: the most recent image
//! / audio in the conversation, whether the user attached it or a previous tool
//! produced it. Built once per turn from the stored history so the tools need no
//! "which one?" argument the model would have to guess an index for.

use super::wire::{Step, StoredAttachment};

#[derive(Debug, Clone, PartialEq)]
pub struct MediaRef {
    /// Content-addressed hash for a user attachment.
    pub asset_hash: Option<String>,
    /// koblem-relative path for a generated output.
    pub url: Option<String>,
    pub mime: String,
}

#[derive(Debug, Default, Clone)]
pub struct MediaIndex {
    images: Vec<MediaRef>,
    audio: Vec<MediaRef>,
}

impl MediaIndex {
    /// Feed history in chronological order; the last thing pushed wins.
    pub fn push_attachments(&mut self, attachments: &[StoredAttachment]) {
        for a in attachments {
            let Some(hash) = crate::director_assets::ref_hash(&a.asset_ref) else {
                continue;
            };
            let r = MediaRef {
                asset_hash: Some(hash.to_string()),
                url: None,
                mime: a.mime.clone(),
            };
            match a.kind.as_str() {
                "image" => self.images.push(r),
                "audio" => self.audio.push(r),
                _ => {}
            }
        }
    }

    pub fn push_steps(&mut self, steps: &[Step]) {
        for s in steps {
            for o in &s.outputs {
                let r = MediaRef {
                    asset_hash: None,
                    url: Some(o.url.clone()),
                    mime: o.mime.clone(),
                };
                match o.kind.as_str() {
                    "image" => self.images.push(r),
                    "audio" => self.audio.push(r),
                    _ => {}
                }
            }
        }
    }

    pub fn latest_image(&self) -> Option<&MediaRef> {
        self.images.last()
    }

    pub fn latest_audio(&self) -> Option<&MediaRef> {
        self.audio.last()
    }
}

/// Pixel size read straight out of an encoded image's header.
///
/// `edit_image` needs it: `/api/v1/images/generate` defaults to 512×512 and
/// resizes the init image to whatever it is told, so dispatching without the
/// source's own shape squashes every landscape photo into a square. No decoder
/// is linked into this crate and none is wanted for four header reads.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    png(bytes)
        .or_else(|| gif(bytes))
        .or_else(|| webp(bytes))
        .or_else(|| jpeg(bytes))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as u32)
}

fn png(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(b"\x89PNG\r\n\x1a\n") || b.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(b, 16)?, be32(b, 20)?))
}

fn gif(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(b"GIF87a") && !b.starts_with(b"GIF89a") {
        return None;
    }
    Some((le16(b, 6)?, le16(b, 8)?))
}

fn webp(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(b"RIFF") || b.get(8..12)? != b"WEBP" {
        return None;
    }
    match b.get(12..16)? {
        // Extended format states the canvas size directly, as two 24-bit
        // minus-one values.
        b"VP8X" => {
            let w = u32::from_le_bytes([b[24], b[25], b[26], 0]) + 1;
            let h = u32::from_le_bytes([b[27], b[28], b[29], 0]) + 1;
            Some((w, h))
        }
        // Lossy: the VP8 keyframe header, past its 3-byte start code.
        b"VP8 " => {
            if b.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            Some((le16(b, 26)? & 0x3fff, le16(b, 28)? & 0x3fff))
        }
        // Lossless: 14 bits each, packed across bytes 21..25.
        b"VP8L" => {
            let bits = u32::from_le_bytes([b[21], b[22], b[23], *b.get(24)?]);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        _ => None,
    }
}

fn jpeg(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(b"\xff\xd8") {
        return None;
    }
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // SOF0..SOF15 carry the frame size; C4/C8/CC are Huffman/arithmetic
        // tables that share the range.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        let len = u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]) as usize;
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::wire::StepOutput;

    fn attachment(kind: &str, hash: &str) -> StoredAttachment {
        StoredAttachment {
            kind: kind.to_string(),
            asset_ref: format!("asset:{}", hash.repeat(64 / hash.len())),
            mime: "image/png".to_string(),
        }
    }

    #[test]
    fn the_most_recent_media_wins() {
        let mut idx = MediaIndex::default();
        idx.push_attachments(&[attachment("image", "a")]);
        idx.push_steps(&[Step {
            index: 0,
            tool: "make_image".into(),
            args_summary: String::new(),
            state: "done".into(),
            detail: String::new(),
            outputs: vec![StepOutput {
                kind: "image".into(),
                url: "/api/v1/generations/g/file/0".into(),
                generation_id: Some("g".into()),
                mime: "image/png".into(),
            }],
        }]);
        assert_eq!(
            idx.latest_image().unwrap().url.as_deref(),
            Some("/api/v1/generations/g/file/0"),
            "a freshly rendered image must beat an older attachment"
        );
    }

    #[test]
    fn images_and_audio_are_tracked_separately() {
        let mut idx = MediaIndex::default();
        idx.push_attachments(&[attachment("audio", "b"), attachment("image", "c")]);
        assert!(idx.latest_audio().unwrap().asset_hash.is_some());
        assert_ne!(idx.latest_audio(), idx.latest_image());
    }

    #[test]
    fn a_malformed_asset_ref_is_ignored_rather_than_dispatched() {
        let mut idx = MediaIndex::default();
        idx.push_attachments(&[StoredAttachment {
            kind: "image".into(),
            asset_ref: "asset:../../etc/passwd".into(),
            mime: "image/png".into(),
        }]);
        assert!(idx.latest_image().is_none());
    }

    #[test]
    fn text_and_video_outputs_are_not_offered_as_matting_or_stt_sources() {
        let mut idx = MediaIndex::default();
        idx.push_steps(&[Step {
            index: 0,
            tool: "make_video".into(),
            args_summary: String::new(),
            state: "done".into(),
            detail: String::new(),
            outputs: vec![
                StepOutput {
                    kind: "video".into(),
                    url: "/v".into(),
                    generation_id: None,
                    mime: "video/webm".into(),
                },
                StepOutput {
                    kind: "text".into(),
                    url: "/t".into(),
                    generation_id: None,
                    mime: "text/plain".into(),
                },
            ],
        }]);
        assert!(idx.latest_image().is_none());
        assert!(idx.latest_audio().is_none());
    }

    #[test]
    fn an_empty_history_offers_nothing() {
        let idx = MediaIndex::default();
        assert!(idx.latest_image().is_none());
        assert!(idx.latest_audio().is_none());
    }

    #[test]
    fn png_dimensions_come_from_the_ihdr_chunk() {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&1920u32.to_be_bytes());
        b.extend_from_slice(&1080u32.to_be_bytes());
        assert_eq!(dimensions(&b), Some((1920, 1080)));
    }

    #[test]
    fn jpeg_dimensions_are_found_by_walking_to_the_frame_header() {
        // APP0/JFIF ahead of the SOF0, which is the normal camera layout.
        let mut b = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        b.extend_from_slice(b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
        b.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&768u16.to_be_bytes());
        b.extend_from_slice(&1024u16.to_be_bytes());
        b.extend_from_slice(&[0x03, 0x01, 0x22, 0x00]);
        assert_eq!(dimensions(&b), Some((1024, 768)));
    }

    #[test]
    fn gif_and_webp_headers_are_understood_too() {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&640u16.to_le_bytes());
        g.extend_from_slice(&480u16.to_le_bytes());
        g.extend_from_slice(&[0; 8]);
        assert_eq!(dimensions(&g), Some((640, 480)));

        let mut w = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        w.extend_from_slice(&[0; 8]);
        w.extend_from_slice(&[0x7f, 0x03, 0x00]); // 895 + 1 = 896
        w.extend_from_slice(&[0x1f, 0x02, 0x00]); // 543 + 1 = 544
        assert_eq!(dimensions(&w), Some((896, 544)));
    }

    #[test]
    fn a_format_with_no_reader_is_reported_as_unknown_rather_than_guessed() {
        assert_eq!(dimensions(b"not an image at all"), None);
        assert_eq!(dimensions(&[]), None);
        // Truncated PNG: the signature is there but the IHDR body is not.
        assert_eq!(dimensions(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0"), None);
    }
}
