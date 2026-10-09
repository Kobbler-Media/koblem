//! The ONE place the tool-call round trip's message shape is decided.
//!
//! Nothing outside this file may assume how a tool result reaches the model:
//! the loop, the registry and the store all deal in [`ToolCall`] /
//! [`ToolResult`] and hand them here. Changing the wire shape is therefore
//! changing [`append_tool_exchange`] and nothing else.
//!
//! The shape is OpenAI native — an assistant turn carrying `tool_calls`, then
//! one `role: "tool"` message per result. Gemma 4's GGUF-baked template renders
//! those into real `<|tool_response|>` special tokens, llama.cpp parses the
//! calls natively, and parallel + multi-hop turns round-trip. Two constraints
//! come from that template and are enforced here rather than at the call sites:
//!
//! * a tool message's `content` MUST be a string — llama.cpp 400s on an object;
//! * `arguments` are sent as an OBJECT when they parse, because the template
//!   renders an object to exact Gemma DSL and brace-strips a string.
//!
//! Folding a result into a user turn instead is NOT an equivalent fallback: the
//! same template emits a dangling `<|tool_response|>` when an assistant
//! `tool_calls` turn is followed by a non-tool message, so the fold corrupts the
//! very context it would be rescuing.

use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw `arguments` string exactly as the model emitted it.
    pub arguments: String,
}

impl ToolCall {
    /// The model's arguments, or an empty object when it emitted something
    /// unparseable (which the dispatcher turns into an error observation
    /// rather than a panic).
    pub fn args(&self) -> Value {
        serde_json::from_str(&self.arguments).unwrap_or_else(|_| json!({}))
    }

    /// What goes back on the wire. An object when it parses — the template
    /// renders that to exact Gemma DSL; a string is brace-stripped instead.
    fn wire_arguments(&self) -> Value {
        serde_json::from_str::<Value>(&self.arguments)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::String(self.arguments.clone()))
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    /// Compact JSON summary. NEVER media bytes — the observation is a summary,
    /// the media reaches the user through the step's output URLs.
    pub observation: Value,
}

/// Extract the assistant's tool calls from a completion response.
pub fn tool_calls_from(response: &Value) -> Vec<ToolCall> {
    response
        .pointer("/choices/0/message/tool_calls")
        .and_then(|v| v.as_array())
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| {
                    let f = c.get("function")?;
                    Some(ToolCall {
                        id: c
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: f.get("name")?.as_str()?.to_string(),
                        arguments: match f.get("arguments") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => "{}".to_string(),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn content_from(response: &Value) -> String {
    response
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Append one completed tool exchange to the running message list. The single
/// point where the tool-result wire shape is decided.
pub fn append_tool_exchange(messages: &mut Vec<Value>, calls: &[ToolCall], results: &[ToolResult]) {
    messages.push(json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": calls
            .iter()
            .map(|c| json!({
                "id": c.id,
                "type": "function",
                "function": {"name": c.name, "arguments": c.wire_arguments()},
            }))
            .collect::<Vec<_>>(),
    }));
    for r in results {
        messages.push(json!({
            "role": "tool",
            "tool_call_id": r.call_id,
            // A string, not the object: llama.cpp answers an object with 400
            // "Expected 'content' to be a string or an array".
            "content": compact(&r.observation),
        }));
    }
}

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "make_image".into(),
            arguments: r#"{"engine":"krea2","prompt":"a dog on a log"}"#.into(),
        }
    }

    fn result() -> ToolResult {
        ToolResult {
            call_id: "c1".into(),
            observation: json!({"ok": true, "engine": "krea2", "outputs": 1}),
        }
    }

    #[test]
    fn tool_calls_are_read_off_a_llama_cpp_response() {
        let resp = json!({"choices":[{"finish_reason":"tool_calls","message":{
            "role":"assistant","content":"",
            "tool_calls":[{"id":"abc","type":"function","function":{
                "name":"make_image","arguments":"{\"engine\":\"krea2\"}"}}]}}]});
        let calls = tool_calls_from(&resp);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "make_image");
        assert_eq!(calls[0].id, "abc");
        assert_eq!(calls[0].args()["engine"], "krea2");
    }

    #[test]
    fn arguments_given_as_an_object_are_tolerated() {
        let resp = json!({"choices":[{"message":{"tool_calls":[{"id":"a","function":{
            "name":"make_image","arguments":{"engine":"flux2"}}}]}}]});
        assert_eq!(tool_calls_from(&resp)[0].args()["engine"], "flux2");
    }

    #[test]
    fn unparseable_arguments_degrade_to_an_empty_object() {
        let c = ToolCall {
            id: "x".into(),
            name: "make_image".into(),
            arguments: "{not json".into(),
        };
        assert_eq!(c.args(), json!({}));
    }

    #[test]
    fn a_plain_reply_yields_no_tool_calls() {
        let resp = json!({"choices":[{"finish_reason":"stop","message":{
            "role":"assistant","content":"  flux2 or krea2?  "}}]});
        assert!(tool_calls_from(&resp).is_empty());
        assert_eq!(content_from(&resp), "flux2 or krea2?");
    }

    #[test]
    fn tool_role_fold_emits_the_native_openai_shape() {
        let mut msgs = Vec::new();
        append_tool_exchange(&mut msgs, &[call()], &[result()]);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], Value::Null);
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "c1");
        assert_eq!(msgs[0]["tool_calls"][0]["type"], "function");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "make_image");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "c1");
    }

    #[test]
    fn tool_message_content_is_a_string() {
        // llama.cpp 400s on an object: "Expected 'content' to be a string or an
        // array". This is the constraint, asserted where it is enforced.
        let mut msgs = Vec::new();
        append_tool_exchange(&mut msgs, &[call()], &[result()]);
        let content = msgs[1]["content"]
            .as_str()
            .expect("content must be a string");
        assert_eq!(
            serde_json::from_str::<Value>(content).unwrap(),
            result().observation
        );
    }

    #[test]
    fn arguments_go_back_as_an_object_when_they_parse() {
        // The template renders an object to exact Gemma DSL; a string is
        // brace-stripped instead.
        let mut msgs = Vec::new();
        append_tool_exchange(&mut msgs, &[call()], &[result()]);
        let args = &msgs[0]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_object(), "expected an object, got {args}");
        assert_eq!(args["engine"], "krea2");
    }

    #[test]
    fn unparseable_arguments_go_back_verbatim_as_a_string() {
        let c = ToolCall {
            id: "c1".into(),
            name: "make_image".into(),
            arguments: "{not json".into(),
        };
        let mut msgs = Vec::new();
        append_tool_exchange(&mut msgs, &[c], &[result()]);
        assert_eq!(
            msgs[0]["tool_calls"][0]["function"]["arguments"],
            "{not json"
        );
    }

    #[test]
    fn parallel_calls_get_one_tool_message_each() {
        let calls = vec![
            call(),
            ToolCall {
                id: "c2".into(),
                name: "make_music".into(),
                arguments: r#"{"engine":"sa3","style":"ambient"}"#.into(),
            },
        ];
        let results = vec![
            result(),
            ToolResult {
                call_id: "c2".into(),
                observation: json!({"ok": false, "error": "sa3 unavailable"}),
            },
        ];
        let mut msgs = Vec::new();
        append_tool_exchange(&mut msgs, &calls, &results);
        assert_eq!(msgs.len(), 3, "one assistant turn + one result per call");
        assert_eq!(msgs[0]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["tool_call_id"], "c1");
        assert_eq!(msgs[2]["tool_call_id"], "c2");
        assert!(
            msgs[2]["content"]
                .as_str()
                .unwrap()
                .contains("sa3 unavailable")
        );
    }

    #[test]
    fn an_observation_never_carries_base64() {
        // The dispatcher's contract, asserted here because this is where the
        // observation becomes prompt tokens.
        let mut msgs = Vec::new();
        append_tool_exchange(&mut msgs, &[call()], &[result()]);
        let payload = msgs[1]["content"].as_str().unwrap();
        assert!(
            payload.len() < 200,
            "observation must stay compact: {payload}"
        );
        assert!(!payload.contains("base64"));
    }
}
