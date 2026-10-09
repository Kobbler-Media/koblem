//! Bidirectional translation of MeiliSearch tool parameters.
//!
//! MeiliSearch defines `_meiliSearchInIndex` with a `filter` string param that
//! requires MeiliSearch filter syntax (`is_monitored = true`). Our local LLM
//! (Qwen 3.5 VL 9B) can't construct valid filter syntax reliably. Instead, we
//! replace `filter` with semantic boolean/enum params the model handles
//! perfectly, then translate back to filter syntax in the response.

use serde_json::{Map, Value, json};

const MEILI_TOOL_NAME: &str = "_meiliSearchInIndex";

/// Rewrite tool definitions in a chat completion request body.
///
/// For `_meiliSearchInIndex`: replaces the `filter` string param with semantic
/// params (`monitored_only`, `in_library_only`, `watched`) and cleans up
/// bloated MeiliSearch descriptions.
pub fn rewrite_request_tools(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(|v| v.as_array_mut()) else {
        return;
    };

    for tool in tools {
        let Some(func) = tool.get_mut("function") else {
            continue;
        };
        let is_meili = func
            .get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n == MEILI_TOOL_NAME);
        if !is_meili {
            continue;
        }

        // Clean up bloated description
        if let Some(desc) = func.get("description").and_then(|d| d.as_str()) {
            let cleaned = clean_tool_description(desc);
            func.as_object_mut()
                .unwrap()
                .insert("description".into(), Value::String(cleaned));
        }

        // Rewrite parameters
        let Some(params) = func.get_mut("parameters") else {
            continue;
        };

        // Remove filter from required array (before borrowing properties)
        if let Some(required) = params.get_mut("required").and_then(|r| r.as_array_mut()) {
            required.retain(|v| v.as_str() != Some("filter"));
        }

        let Some(props) = params.get_mut("properties").and_then(|p| p.as_object_mut()) else {
            continue;
        };

        // Remove the filter param — model can't construct valid syntax
        props.remove("filter");

        // Add semantic params
        props.insert(
            "monitored_only".into(),
            json!({
                "type": "boolean",
                "description": "Only return monitored/tracked items"
            }),
        );
        props.insert(
            "in_library_only".into(),
            json!({
                "type": "boolean",
                "description": "Only return items that are in the user's library"
            }),
        );
        props.insert(
            "watched".into(),
            json!({
                "type": "string",
                "enum": ["any", "watched", "unwatched"],
                "description": "Filter by watch status"
            }),
        );
    }
}

/// Translate semantic tool call arguments back to MeiliSearch filter syntax.
///
/// Returns the rewritten arguments JSON string. Non-MeiliSearch tool calls
/// are returned unchanged.
pub fn rewrite_tool_call_args(name: &str, arguments: &str) -> String {
    if name != MEILI_TOOL_NAME {
        return arguments.to_string();
    }

    let Ok(mut args) = serde_json::from_str::<Map<String, Value>>(arguments) else {
        return arguments.to_string();
    };

    let mut filters = Vec::new();

    if let Some(Value::Bool(true)) = args.remove("monitored_only") {
        filters.push("is_monitored = true".to_string());
    }
    if let Some(Value::Bool(true)) = args.remove("in_library_only") {
        filters.push("is_in_library = true".to_string());
    }
    match args.remove("watched").and_then(|v| match v {
        Value::String(s) => Some(s),
        _ => None,
    }) {
        Some(ref s) if s == "watched" => filters.push("is_watched = true".to_string()),
        Some(ref s) if s == "unwatched" => filters.push("is_watched = false".to_string()),
        _ => {} // "any" or absent = no filter
    }

    let filter_str = filters.join(" AND ");
    args.insert("filter".into(), Value::String(filter_str));

    serde_json::to_string(&args).unwrap_or_else(|_| arguments.to_string())
}

/// Rewrite tool calls in a non-streaming response body.
pub fn rewrite_response_tool_calls(body: &mut Value) {
    let Some(choices) = body.get_mut("choices").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for choice in choices {
        rewrite_tool_calls_in_message(choice.get_mut("message"));
    }
}

fn rewrite_tool_calls_in_message(msg: Option<&mut Value>) {
    let Some(msg) = msg else { return };
    let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for tc in tool_calls {
        let Some(func) = tc.get_mut("function") else {
            continue;
        };
        let name = func
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default();
        if name != MEILI_TOOL_NAME {
            continue;
        }
        if let Some(args_str) = func.get("arguments").and_then(|a| a.as_str()) {
            let rewritten = rewrite_tool_call_args(name, args_str);
            func.as_object_mut()
                .unwrap()
                .insert("arguments".into(), Value::String(rewritten));
        }
    }
}

/// Extract concise index descriptions from MeiliSearch's bloated tool description.
fn clean_tool_description(desc: &str) -> String {
    let mut index_descriptions = Vec::new();
    for line in desc.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("- ")
            && trimmed.contains(':')
            && let Some((_, after)) = trimmed.split_once(':')
            && !after.trim().is_empty()
        {
            index_descriptions.push(trimmed.to_string());
        }
    }
    if index_descriptions.is_empty() {
        return desc.chars().take(200).collect();
    }
    format!(
        "Search the user's media library. Available indexes:\n{}",
        index_descriptions.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meili_tool() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "_meiliSearchInIndex",
                "description": "Search across indexes.\n- tv_shows: TV shows in the library\n- movies: Movies in the library\n- books: Books collection",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "index_uid": {"type": "string", "enum": ["tv_shows", "movies", "books"]},
                        "q": {"type": "string", "description": "Search query"},
                        "filter": {
                            "type": "string",
                            "description": "## Facet distributions of the tv_shows index\nis_monitored: true, false.\nis_in_library: true, false."
                        }
                    },
                    "required": ["index_uid", "q", "filter"]
                }
            }
        })
    }

    #[test]
    fn request_rewrite_replaces_filter_with_semantic_params() {
        let mut body = json!({
            "messages": [{"role": "user", "content": "What sci-fi shows am I monitoring?"}],
            "tools": [sample_meili_tool()]
        });

        rewrite_request_tools(&mut body);

        let props = &body["tools"][0]["function"]["parameters"]["properties"];
        assert!(props.get("filter").is_none(), "filter should be removed");
        assert!(props.get("monitored_only").is_some());
        assert!(props.get("in_library_only").is_some());
        assert!(props.get("watched").is_some());
        assert_eq!(
            props["watched"]["enum"],
            json!(["any", "watched", "unwatched"])
        );

        // filter removed from required
        let required = body["tools"][0]["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        assert!(!required.iter().any(|v| v.as_str() == Some("filter")));
    }

    #[test]
    fn request_rewrite_cleans_description() {
        let mut body = json!({
            "tools": [sample_meili_tool()]
        });
        rewrite_request_tools(&mut body);

        let desc = body["tools"][0]["function"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.starts_with("Search the user's media library"));
        assert!(desc.contains("tv_shows"));
    }

    #[test]
    fn request_rewrite_ignores_non_meili_tools() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "other_tool",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "filter": {"type": "string"}
                        }
                    }
                }
            }]
        });

        let original = body.clone();
        rewrite_request_tools(&mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn response_rewrite_monitored_only() {
        let args = r#"{"index_uid":"tv_shows","q":"sci-fi","monitored_only":true}"#;
        let result = rewrite_tool_call_args(MEILI_TOOL_NAME, args);
        let parsed: Map<String, Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["filter"], "is_monitored = true");
        assert!(parsed.get("monitored_only").is_none());
        assert_eq!(parsed["index_uid"], "tv_shows");
        assert_eq!(parsed["q"], "sci-fi");
    }

    #[test]
    fn response_rewrite_multiple_filters() {
        let args =
            r#"{"index_uid":"tv_shows","q":"all","monitored_only":true,"watched":"unwatched"}"#;
        let result = rewrite_tool_call_args(MEILI_TOOL_NAME, args);
        let parsed: Map<String, Value> = serde_json::from_str(&result).unwrap();

        let filter = parsed["filter"].as_str().unwrap();
        assert!(filter.contains("is_monitored = true"));
        assert!(filter.contains("is_watched = false"));
        assert!(filter.contains(" AND "));
    }

    #[test]
    fn response_rewrite_in_library() {
        let args = r#"{"index_uid":"books","q":"Sanderson","in_library_only":true}"#;
        let result = rewrite_tool_call_args(MEILI_TOOL_NAME, args);
        let parsed: Map<String, Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["filter"], "is_in_library = true");
    }

    #[test]
    fn response_rewrite_watched() {
        let args = r#"{"index_uid":"movies","q":"all","watched":"watched"}"#;
        let result = rewrite_tool_call_args(MEILI_TOOL_NAME, args);
        let parsed: Map<String, Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["filter"], "is_watched = true");
    }

    #[test]
    fn response_rewrite_no_semantic_params_yields_empty_filter() {
        let args = r#"{"index_uid":"tv_shows","q":"sci-fi"}"#;
        let result = rewrite_tool_call_args(MEILI_TOOL_NAME, args);
        let parsed: Map<String, Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["filter"], "");
    }

    #[test]
    fn response_rewrite_non_meili_unchanged() {
        let args = r#"{"q":"test"}"#;
        let result = rewrite_tool_call_args("other_tool", args);
        assert_eq!(result, args);
    }

    #[test]
    fn rewrite_response_body_tool_calls() {
        let mut body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_0",
                        "type": "function",
                        "function": {
                            "name": "_meiliSearchInIndex",
                            "arguments": "{\"index_uid\":\"tv_shows\",\"q\":\"sci-fi\",\"monitored_only\":true}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });

        rewrite_response_tool_calls(&mut body);

        let args_str = body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let args: Map<String, Value> = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["filter"], "is_monitored = true");
        assert!(args.get("monitored_only").is_none());
    }
}
