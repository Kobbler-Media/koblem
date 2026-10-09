//! The API contract the web client is built against. Route-local by design —
//! the shared `koblem-client` crate has no agent command and the browser talks
//! TypeScript, so nothing needs these in Rust outside koblem.

use serde::{Deserialize, Serialize};

/// One attachment as STORED: a content-addressed ref, never bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredAttachment {
    pub kind: String,
    /// `asset:<sha256>` into the director asset store.
    pub asset_ref: String,
    pub mime: String,
}

/// One attachment as SERVED — `url` is directly usable as an element `src`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Attachment {
    pub kind: String,
    pub url: String,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepOutput {
    /// `image` | `video` | `audio` | `text`
    pub kind: String,
    /// Stored WITHOUT the `token` query param; the read path appends the
    /// caller's own token so the URL works from an `<img>`/`<video>` src.
    pub url: String,
    pub generation_id: Option<String>,
    pub mime: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed,
}

impl StepState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub index: i32,
    pub tool: String,
    pub args_summary: String,
    /// `pending` | `running` | `done` | `failed`
    pub state: String,
    pub detail: String,
    pub outputs: Vec<StepOutput>,
}

impl Step {
    pub fn new(index: i32, tool: &str, args_summary: String) -> Self {
        Self {
            index,
            tool: tool.to_string(),
            args_summary,
            state: StepState::Running.as_str().to_string(),
            detail: String::new(),
            outputs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Running,
    Complete,
    Error,
    Cancelled,
}

impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: String,
    pub seq: i32,
    pub role: String,
    pub content: String,
    pub attachments: Vec<Attachment>,
    pub steps: Vec<Step>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

/// `POST /api/v1/agent/sessions/{id}/messages` body.
#[derive(Debug, Clone, Deserialize)]
pub struct SendMessage {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<InboundAttachment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundAttachment {
    /// `image` | `audio`
    pub kind: String,
    pub data_url: String,
    #[serde(default)]
    pub mime: String,
}

/// Append the caller's own bearer token to a stored, token-less media path so
/// the browser can use it as an element `src` (which cannot set headers).
pub fn with_token(url: &str, token: &str) -> String {
    if token.is_empty() {
        return url.to_string();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!(
        "{url}{sep}token={}",
        percent_encoding::utf8_percent_encode(token, percent_encoding::NON_ALPHANUMERIC)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_appended_with_the_right_separator() {
        assert_eq!(with_token("/a/b", "tok"), "/a/b?token=tok");
        assert_eq!(with_token("/a/b?x=1", "tok"), "/a/b?x=1&token=tok");
    }

    #[test]
    fn token_is_percent_encoded() {
        assert_eq!(with_token("/a", "a+b/c="), "/a?token=a%2Bb%2Fc%3D");
    }

    #[test]
    fn a_blank_token_leaves_the_url_alone() {
        assert_eq!(with_token("/a/b", ""), "/a/b");
    }
}
