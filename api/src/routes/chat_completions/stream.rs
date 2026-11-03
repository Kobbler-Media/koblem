use std::collections::BTreeMap;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::tool_rewrite;

const DONE: &str = "data: [DONE]\n\n";

#[derive(Default)]
pub(super) struct Summary {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
}

struct HeldCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Search-tool calls are held until their choice finishes because the
/// argument rewrite needs the complete JSON; everything else streams through.
#[derive(Default)]
struct Relay {
    held: BTreeMap<(u64, u64), HeldCall>,
    template: Option<Value>,
    summary: Summary,
}

enum Line {
    Frames(Vec<String>),
    Done,
}

impl Relay {
    fn push_line(&mut self, line: &str) -> Line {
        let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
            return Line::Frames(vec![format!("{line}\n\n")]);
        };
        if payload == "[DONE]" {
            return Line::Done;
        }
        let Ok(mut chunk) = serde_json::from_str::<Value>(payload) else {
            return Line::Frames(vec![format!("{line}\n\n")]);
        };
        self.remember_template(&chunk);
        self.record(&chunk);

        let mut frames = Vec::new();
        let mut held_any = false;
        let mut finished = Vec::new();
        if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
            for choice in choices {
                let ci = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(delta) = choice.get_mut("delta") {
                    held_any |= self.hold_search_calls(ci, delta);
                }
                if choice.get("finish_reason").is_some_and(|f| !f.is_null()) {
                    finished.push(ci);
                }
            }
        }
        for ci in finished {
            frames.extend(self.release(Some(ci)));
        }
        if !(held_any && is_empty_chunk(&chunk)) {
            frames.push(if held_any {
                frame(&chunk)
            } else {
                format!("data: {payload}\n\n")
            });
        }
        Line::Frames(frames)
    }

    fn remember_template(&mut self, chunk: &Value) {
        if self.template.is_none()
            && let Some(obj) = chunk.as_object()
            && obj.contains_key("choices")
        {
            let mut t = obj.clone();
            t.remove("choices");
            t.remove("usage");
            self.template = Some(Value::Object(t));
        }
    }

    fn record(&mut self, chunk: &Value) {
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                self.summary.finish_reason = Some(fr.to_string());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(s) = delta.get("content").and_then(Value::as_str) {
                self.summary.content.push_str(s);
            }
            if let Some(s) = delta.get("reasoning_content").and_then(Value::as_str) {
                self.summary.reasoning.push_str(s);
            }
        }
    }

    fn hold_search_calls(&mut self, ci: u64, delta: &mut Value) -> bool {
        let Some(obj) = delta.as_object_mut() else {
            return false;
        };
        let Some(calls) = obj.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            return false;
        };
        let before = calls.len();
        calls.retain(|tc| {
            let ti = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
            let func = tc.get("function");
            let args = func
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let id = tc.get("id").and_then(Value::as_str).map(String::from);
            if let Some(held) = self.held.get_mut(&(ci, ti)) {
                held.arguments.push_str(args);
                held.id = held.id.take().or(id);
                return false;
            }
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !tool_rewrite::is_rewritten_tool(name) {
                return true;
            }
            self.held.insert(
                (ci, ti),
                HeldCall {
                    id,
                    name: name.to_string(),
                    arguments: args.to_string(),
                },
            );
            false
        });
        let held_any = calls.len() != before;
        if calls.is_empty() {
            obj.remove("tool_calls");
        }
        held_any
    }

    fn release(&mut self, choice: Option<u64>) -> Vec<String> {
        let keys: Vec<(u64, u64)> = self
            .held
            .keys()
            .filter(|(ci, _)| choice.is_none_or(|c| c == *ci))
            .copied()
            .collect();
        let mut by_choice: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
        for key @ (ci, ti) in keys {
            let call = self.held.remove(&key).expect("key just listed");
            by_choice.entry(ci).or_default().push(json!({
                "index": ti,
                "id": call.id.unwrap_or_else(|| format!("call_{ti}")),
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": tool_rewrite::rewrite_tool_call_args(&call.name, &call.arguments),
                },
            }));
        }
        by_choice
            .into_iter()
            .map(|(ci, calls)| {
                let mut chunk = self.template.clone().unwrap_or_else(|| json!({}));
                chunk["choices"] = json!([{
                    "index": ci,
                    "delta": {"tool_calls": calls},
                    "finish_reason": null,
                }]);
                frame(&chunk)
            })
            .collect()
    }
}

fn is_empty_chunk(chunk: &Value) -> bool {
    chunk.get("usage").is_none_or(Value::is_null)
        && chunk
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().all(|c| {
                    c.get("finish_reason").is_none_or(Value::is_null)
                        && c.get("delta")
                            .and_then(Value::as_object)
                            .is_none_or(|d| d.is_empty())
                })
            })
}

fn frame(chunk: &Value) -> String {
    format!("data: {chunk}\n\n")
}

fn error_frame(message: &str) -> String {
    frame(&json!({
        "error": {"message": message, "type": "upstream_error", "code": 502}
    }))
}

/// hyper only flushes buffered frames when the body stream goes pending, so an
/// `Err` polled straight after the error event would abort the connection with
/// that event still unsent.
pub(super) fn flush_before_abort(
    rx: Receiver<Result<String, std::io::Error>>,
) -> impl Stream<Item = Result<String, std::io::Error>> {
    futures::stream::unfold(rx, |mut rx| async move {
        let item = rx.recv().await?;
        if item.is_err() {
            tokio::task::yield_now().await;
        }
        Some((item, rx))
    })
}

/// Relays upstream SSE to `tx`. On upstream failure it sends an OpenAI-style
/// error event and then an `Err`, which aborts the response body so clients
/// that ignore error events still see a broken stream rather than a clean end.
pub(super) async fn relay<S, E>(
    mut upstream: S,
    tx: &Sender<Result<String, std::io::Error>>,
) -> Summary
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut relay = Relay::default();
    let mut buf: Vec<u8> = Vec::new();

    let saw_done = 'read: loop {
        let eof = match upstream.next().await {
            Some(Ok(b)) => {
                buf.extend_from_slice(&b);
                false
            }
            Some(Err(e)) => break 'read Err(format!("upstream stream failed: {e}")),
            None => {
                buf.push(b'\n');
                true
            }
        };
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let frames = match relay.push_line(line) {
                Line::Done => break 'read Ok(true),
                Line::Frames(f) => f,
            };
            for f in frames {
                if tx.send(Ok(f)).await.is_err() {
                    break 'read Err("client disconnected".to_string());
                }
            }
        }
        if eof {
            break 'read Ok(false);
        }
    };

    let outcome = match saw_done {
        Ok(true) => Ok(()),
        Ok(false) if relay.summary.finish_reason.is_some() => Ok(()),
        Ok(false) => Err("upstream stream ended before the completion finished".to_string()),
        Err(e) => Err(e),
    };

    match outcome {
        Ok(()) => {
            for f in relay.release(None) {
                let _ = tx.send(Ok(f)).await;
            }
            let _ = tx.send(Ok(DONE.to_string())).await;
        }
        Err(message) => {
            let _ = tx.send(Ok(error_frame(&message))).await;
            let _ = tx.send(Err(std::io::Error::other(message.clone()))).await;
            relay.summary.error = Some(message);
        }
    }
    relay.summary
}
