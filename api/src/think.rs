use serde_json::Value;

/// Strip `<think>...</think>` reasoning blocks from LLM output.
///
/// Some models emit a chain-of-thought block wrapped in `<think>…</think>` (or
/// occasionally a bare trailing `</think>` with the prefix missing) before the
/// real answer. We drop everything up to and including the closing tag and
/// return what follows.
pub fn strip_think_tags(content: &str) -> &str {
    if let Some(pos) = content.rfind("</think>") {
        content[pos + "</think>".len()..].trim()
    } else {
        content.trim()
    }
}

/// Strip a leading ```json (or bare ```) markdown code fence and trailing ```
/// from LLM output. Many models wrap structured output in fences even when
/// asked not to; downstream JSON parsers choke on the backticks.
pub fn strip_code_fence(content: &str) -> &str {
    let trimmed = content.trim();
    let stripped_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_start_matches('\n')
        .trim_start_matches('\r');
    stripped_open
        .strip_suffix("```")
        .unwrap_or(stripped_open)
        .trim()
}

/// Parse `s` as JSON, falling back to a stutter-recovery pass on failure.
///
/// Two glitches, both observed from llama-server and both whole-response modes
/// the model falls into rather than one-off typos:
///
/// 1. `{{ … }}` where strict JSON expects single braces.
/// 2. `,""key":` — the quote before every key after the first is doubled. It is
///    deterministic per input at low temperature (`Tuesday` reproduces it on
///    every sample), so leaving it unrepaired is a permanent 502 for whichever
///    prompts happen to trigger it, not an occasional one.
///
/// The risk is corrupting legitimate content: `"{{name}}"` in a templated
/// string, or an empty array element in `["a",""]`. The first is accepted for
/// the same reason as ever — the structured endpoints emit plain data objects —
/// and the second is why the key repair requires a word character after the
/// doubled quote. Either way the strict parse is always tried first, so clean
/// output never reaches the recovery path, and a repair that still fails
/// reports the original error.
pub fn parse_json_lenient(s: &str) -> Result<Value, serde_json::Error> {
    match serde_json::from_str::<Value>(s) {
        Ok(v) => Ok(v),
        Err(orig) => {
            let collapsed = undouble_key_quotes(&s.replace("{{", "{").replace("}}", "}"));
            if collapsed == s {
                return Err(orig);
            }
            // If the recovery pass also fails, return the *original* error so
            // callers see the real shape problem rather than a misleading one
            // about the rewritten string.
            serde_json::from_str(&collapsed).map_err(|_| orig)
        }
    }
}

/// `,""key"` → `,"key"`. Only where a word character follows, which is what
/// distinguishes a stuttered key from the empty string in `["a",""]`.
fn undouble_key_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(",\"\"") {
        let tail = &rest[at + 3..];
        let stuttered = tail
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        out.push_str(&rest[..if stuttered { at + 2 } else { at + 3 }]);
        rest = &rest[at + 3..];
    }
    out.push_str(rest);
    out
}

/// Find the last balanced `{...}` substring. Used to recover JSON when the
/// model dumped everything into reasoning_content and left content empty
/// (happens with thinking models that hit a stop condition mid-trace).
pub fn last_balanced_json_block(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut last: Option<&str> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let mut depth: i32 = 0;
        let mut end: Option<usize> = None;
        for (j, &byte) in bytes.iter().enumerate().skip(i) {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(j);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(j) => {
                let slice = &text[i..=j];
                // Use lenient parse so doubled-brace mangling inside the
                // reasoning trace doesn't drop an otherwise-valid block.
                if parse_json_lenient(slice).is_ok() {
                    last = Some(slice);
                }
                i = j + 1;
            }
            None => break,
        }
    }
    last
}

/// Best-effort JSON extraction from chat-completion content (with an optional
/// reasoning_content fallback). Tries in order:
///
///   1. Strict parse on cleaned content (`</think>` stripped + leading/
///      trailing ```json fence stripped). Handles the well-formed case.
///   2. Last balanced `{...}` from the raw content. Catches the case where
///      the model wrote prose with an embedded code fence partway through
///      — e.g. "Looking at the candidates...\n\n```json\n{...}\n```" — which
///      `strip_code_fence` misses because it only handles leading fences.
///   3. Last balanced `{...}` from reasoning_content. Catches the case where
///      the model emitted its answer inside the think block (content empty).
///
/// Returns `None` only when none of the three strategies finds parseable JSON.
/// This is the load-bearing parser for review/filter/validate/rank — without
/// it, structured-output paths silently return empty accept lists when the
/// model leaks prose between `</think>` and the JSON.
pub fn extract_json_payload(content: &str, reasoning: Option<&str>) -> Option<Value> {
    // 1. strict on cleaned content
    let cleaned = strip_code_fence(strip_think_tags(content));
    if !cleaned.is_empty()
        && let Ok(v) = parse_json_lenient(cleaned)
    {
        return Some(v);
    }
    // 2. embedded JSON in content body
    if let Some(blk) = last_balanced_json_block(content)
        && let Ok(v) = parse_json_lenient(blk)
    {
        return Some(v);
    }
    // 3. reasoning_content fallback
    if let Some(r) = reasoning
        && let Some(blk) = last_balanced_json_block(r)
        && let Ok(v) = parse_json_lenient(blk)
    {
        return Some(v);
    }
    None
}

/// Extract `choices[0].message.reasoning_content` from a non-streaming chat
/// completion body. llama-server populates this when its `--reasoning-format`
/// parser splits the model's `<think>…</think>` trace from the final answer.
/// (That flag — which we do not pass — governs where the trace LANDS; it is not
/// the mythical `--reasoning off` that would disable thinking. Thinking is
/// turned off per request via `chat_template_kwargs.enable_thinking`.) Returns
/// `None` when the field is absent or empty.
///
/// Safe to call after [`strip_think_in_response`]: that function reads the
/// field to recover JSON when content is empty but never removes it.
pub fn extract_reasoning_content(body: &Value) -> Option<String> {
    let s = body
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("reasoning_content")?
        .as_str()?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Walk a parsed chat-completion response body and clean every
/// `choices[].message.content` string in place. Three operations stack:
///
/// 1. Strip `<think>...</think>` blocks (legacy: some models emit thinking in
///    `content` even when llama-server's `--reasoning-format` parser is active).
/// 2. Strip ```json…``` markdown code fences.
/// 3. If `content` ends up empty (model emitted only a thinking trace and
///    llama-server moved it to `reasoning_content`), recover the last balanced
///    JSON object from `reasoning_content` and promote it to `content`.
///
/// Step 3 specifically fixes the koblem failure where thinking-trained models
/// (Qwen3.5-9B-Claude-Opus-Distill, etc.) emit their final JSON answer inside
/// a `<think>` block, llama-server splits it into `reasoning_content`, leaving
/// `content` empty and downstream JSON parsers failing with EOF.
pub fn strip_think_in_response(body: &mut Value) {
    let Some(choices) = body.get_mut("choices").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for choice in choices {
        let Some(message) = choice.get_mut("message").and_then(|m| m.as_object_mut()) else {
            continue;
        };

        // Step 1 + 2: clean content if present.
        let cleaned = message
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| strip_code_fence(strip_think_tags(s)).to_string())
            .unwrap_or_default();

        // Step 3: if cleaned content is empty, try to recover JSON from
        // reasoning_content.
        let final_content = if cleaned.is_empty() {
            message
                .get("reasoning_content")
                .and_then(|r| r.as_str())
                .and_then(last_balanced_json_block)
                .map(str::to_string)
                .unwrap_or(cleaned)
        } else {
            cleaned
        };

        message.insert("content".to_string(), Value::String(final_content));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_full_think_block() {
        let input = "<think>reasoning...</think>The answer is 42.";
        assert_eq!(strip_think_tags(input), "The answer is 42.");
    }

    #[test]
    fn strips_bare_closing_tag() {
        let input = "reasoning leaked out</think>\n{\"ok\":true}";
        assert_eq!(strip_think_tags(input), "{\"ok\":true}");
    }

    #[test]
    fn no_think_block() {
        let input = "Just a plain answer.";
        assert_eq!(strip_think_tags(input), "Just a plain answer.");
    }

    #[test]
    fn trims_whitespace() {
        let input = "<think>thinking</think>\n\n  The answer.  ";
        assert_eq!(strip_think_tags(input), "The answer.");
    }

    #[test]
    fn empty_after_think() {
        let input = "<think>only thinking</think>";
        assert_eq!(strip_think_tags(input), "");
    }

    #[test]
    fn uses_last_closing_tag_when_multiple() {
        let input = "<think>a</think>middle</think>final";
        assert_eq!(strip_think_tags(input), "final");
    }

    // --- strip_code_fence ---

    #[test]
    fn strips_json_code_fence() {
        let input = "```json\n{\"accept\":[2]}\n```";
        assert_eq!(strip_code_fence(input), "{\"accept\":[2]}");
    }

    #[test]
    fn strips_bare_code_fence() {
        let input = "```\n{\"accept\":[]}\n```";
        assert_eq!(strip_code_fence(input), "{\"accept\":[]}");
    }

    #[test]
    fn strips_uppercase_json_fence() {
        let input = "```JSON\n{\"a\":1}\n```";
        assert_eq!(strip_code_fence(input), "{\"a\":1}");
    }

    #[test]
    fn no_fence_passthrough() {
        let input = "{\"accept\":[]}";
        assert_eq!(strip_code_fence(input), "{\"accept\":[]}");
    }

    #[test]
    fn unterminated_open_fence_still_strips_open() {
        // model emitted ```json but ran out of tokens before closing
        let input = "```json\n{\"accept\":[2]}";
        assert_eq!(strip_code_fence(input), "{\"accept\":[2]}");
    }

    // --- parse_json_lenient ---

    #[test]
    fn lenient_parses_clean_json() {
        let v = parse_json_lenient(r#"{"accept":[1,2]}"#).unwrap();
        assert_eq!(v["accept"][1], 2);
    }

    #[test]
    fn lenient_recovers_doubled_outer_braces() {
        // Reported llama-server failure mode: outer braces emitted twice.
        let v = parse_json_lenient(r#"{{"accept":[1,2]}}"#).unwrap();
        assert_eq!(v["accept"][0], 1);
    }

    #[test]
    fn lenient_recovers_all_braces_doubled() {
        let v = parse_json_lenient(r#"{{"x":{{"y":1}}}}"#).unwrap();
        assert_eq!(v["x"]["y"], 1);
    }

    #[test]
    fn lenient_recovers_mixed_doubled_outer_single_inner() {
        // Outer doubled, inner already correct. Replace `{{`→`{` and `}}`→`}`
        // collapses the extra trailing `}}}` down to `}}` which closes
        // properly.
        let v = parse_json_lenient(r#"{{"x":{"y":1}}}"#).unwrap();
        assert_eq!(v["x"]["y"], 1);
    }

    #[test]
    fn lenient_returns_original_error_when_no_doubled_braces() {
        let err = parse_json_lenient("not json at all").unwrap_err();
        // Original error message should mention the actual parse failure,
        // not whatever the rewritten variant would have surfaced.
        assert!(err.to_string().to_lowercase().contains("expected"));
    }

    #[test]
    fn lenient_returns_original_error_when_recovery_also_fails() {
        // Both `{{` and unmatched garbage — recovery shouldn't accidentally
        // succeed.
        let err = parse_json_lenient(r#"{{ "x": (broken) }}"#).unwrap_err();
        let _ = err;
    }

    /// The stutter is whole-response: once the model starts doubling it doubles the quote before
    /// every key, and before array elements too.
    #[test]
    fn lenient_recovers_stuttered_key_quotes() {
        let v = parse_json_lenient(
            r#"{"name":"Tuesday",""subject":"a calendar",""locomotion":["walk",""fly"]}"#,
        )
        .unwrap();
        assert_eq!(v["subject"], "a calendar");
        assert_eq!(v["locomotion"][1], "fly");
    }

    /// The repair must not eat a legitimately empty array element. A valid document never reaches
    /// the recovery path at all, so this is really about the one that arrives already broken.
    #[test]
    fn lenient_leaves_empty_strings_alone() {
        let v = parse_json_lenient(r#"{"a":["x",""],"b":""}"#).unwrap();
        assert_eq!(v["a"][1], "");
        assert_eq!(
            undouble_key_quotes(r#"{{"a":["x",""]}}"#),
            r#"{{"a":["x",""]}}"#
        );
    }

    // --- last_balanced_json_block ---

    #[test]
    fn finds_single_json_block() {
        let input = "thinking blah blah... final answer: {\"accept\":[2]} done";
        assert_eq!(last_balanced_json_block(input), Some("{\"accept\":[2]}"));
    }

    #[test]
    fn returns_last_when_multiple() {
        let input = "first try: {\"accept\":[1]}, then revised to {\"accept\":[2,3]}";
        assert_eq!(last_balanced_json_block(input), Some("{\"accept\":[2,3]}"));
    }

    #[test]
    fn handles_nested_braces() {
        let input = "answer: {\"notes\":\"ok\",\"accept\":[{\"idx\":1,\"reason\":\"r22\"}]}";
        assert_eq!(
            last_balanced_json_block(input),
            Some("{\"notes\":\"ok\",\"accept\":[{\"idx\":1,\"reason\":\"r22\"}]}")
        );
    }

    #[test]
    fn skips_invalid_json_blocks() {
        // first {...} is malformed JavaScript-ish, second is valid JSON
        let input = "draft: {accept: [1]} final: {\"accept\":[2]}";
        assert_eq!(last_balanced_json_block(input), Some("{\"accept\":[2]}"));
    }

    #[test]
    fn returns_none_when_no_json() {
        let input = "Just prose, no braces here.";
        assert_eq!(last_balanced_json_block(input), None);
    }

    #[test]
    fn returns_none_when_unbalanced() {
        let input = "started thinking { but never closed";
        assert_eq!(last_balanced_json_block(input), None);
    }

    #[test]
    fn last_balanced_block_accepts_doubled_braces() {
        // llama-server doubled-brace failure inside a thinking trace —
        // the brace counter still matches `{{...}}` and the lenient
        // parser validates it (collapsing the duplicates internally).
        let input = r#"thinking blah... final: {{"accept":[2]}}"#;
        assert_eq!(last_balanced_json_block(input), Some(r#"{{"accept":[2]}}"#));
    }

    // --- strip_think_in_response (integration) ---

    fn build_response(content: &str, reasoning: Option<&str>) -> Value {
        let mut msg = serde_json::json!({"role": "assistant", "content": content});
        if let Some(r) = reasoning {
            msg["reasoning_content"] = Value::String(r.to_string());
        }
        serde_json::json!({"choices": [{"message": msg}]})
    }

    fn extract_content(body: &Value) -> String {
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn integration_recovers_json_from_reasoning_when_content_empty() {
        // The Daredevil case: thinking-trained model emits its JSON answer
        // inside the <think> block, llama-server moves it to reasoning_content,
        // content is empty. Without recovery, koblem sees empty and parse-fails.
        let mut body = build_response(
            "",
            Some(
                "Let me think about this. Candidate 2 is 1080p H.264 — accepts. Final answer: {\"notes\":\"only candidate 2 matches\",\"accept\":[2]}",
            ),
        );
        strip_think_in_response(&mut body);
        assert_eq!(
            extract_content(&body),
            "{\"notes\":\"only candidate 2 matches\",\"accept\":[2]}"
        );
    }

    #[test]
    fn integration_keeps_valid_content_over_reasoning() {
        // Normal case: model emitted JSON in content, reasoning has thinking.
        // We must not overwrite valid content with reasoning's JSON.
        let mut body = build_response(
            "{\"accept\":[2]}",
            Some("...thinking... final: {\"accept\":[7]}"),
        );
        strip_think_in_response(&mut body);
        assert_eq!(extract_content(&body), "{\"accept\":[2]}");
    }

    #[test]
    fn integration_strips_json_fence_in_content() {
        let mut body = build_response("```json\n{\"accept\":[]}\n```", None);
        strip_think_in_response(&mut body);
        assert_eq!(extract_content(&body), "{\"accept\":[]}");
    }

    #[test]
    fn integration_strips_think_then_fence() {
        let mut body = build_response(
            "<think>analyzing</think>\n```json\n{\"accept\":[1,2]}\n```",
            None,
        );
        strip_think_in_response(&mut body);
        assert_eq!(extract_content(&body), "{\"accept\":[1,2]}");
    }

    #[test]
    fn integration_no_recovery_when_reasoning_has_no_json() {
        let mut body = build_response("", Some("rambling text with no braces at all"));
        strip_think_in_response(&mut body);
        assert_eq!(extract_content(&body), "");
    }

    #[test]
    fn integration_handles_missing_reasoning_field() {
        // Some llama-server versions don't populate reasoning_content at all.
        let mut body = build_response("", None);
        strip_think_in_response(&mut body);
        assert_eq!(extract_content(&body), "");
    }

    // --- extract_json_payload ---

    #[test]
    fn extract_json_strict_path() {
        // Plain JSON, no fences, no prose — should parse via strategy 1.
        let v = extract_json_payload(r#"{"accept":[1,2]}"#, None).unwrap();
        assert_eq!(v["accept"][1], 2);
    }

    #[test]
    fn extract_json_strips_leading_fence() {
        let v = extract_json_payload("```json\n{\"accept\":[3]}\n```", None).unwrap();
        assert_eq!(v["accept"][0], 3);
    }

    #[test]
    fn extract_json_handles_prose_with_embedded_fence() {
        // The actual leak case: model wrote prose preamble, then JSON in a
        // code fence partway through. strip_code_fence misses this; strategy
        // 2 (last_balanced_json_block) catches it.
        let content = "Looking at the candidates provided, I need to check each one.\n\
                       \n\
                       **Analyzing Candidate 1:** Wrong show.\n\
                       \n\
                       **FINAL PICK LIST:**\n\
                       \n\
                       ```json\n\
                       {\"notes\":\"No matching candidates found\",\"accept\":[]}\n\
                       ```";
        let v = extract_json_payload(content, None).unwrap();
        assert_eq!(v["notes"], "No matching candidates found");
        assert!(v["accept"].as_array().unwrap().is_empty());
    }

    #[test]
    fn extract_json_handles_prose_with_unfenced_json() {
        // Even without fences — prose then a JSON object inline.
        let content = "After analysis: {\"accept\":[5,12,23]} — these are the keepers.";
        let v = extract_json_payload(content, None).unwrap();
        assert_eq!(v["accept"][2], 23);
    }

    #[test]
    fn extract_json_falls_back_to_reasoning_when_content_empty() {
        // Existing strip_think_in_response Step-3 case — keep working through
        // the new helper too.
        let v = extract_json_payload("", Some(r#"Let me think... final: {"accept":[7]}"#)).unwrap();
        assert_eq!(v["accept"][0], 7);
    }

    #[test]
    fn extract_json_prefers_content_over_reasoning() {
        // If both have JSON, content wins — reasoning may have an earlier
        // draft that the model superseded in the final answer.
        let v =
            extract_json_payload(r#"{"accept":[2]}"#, Some(r#"draft: {"accept":[1]}"#)).unwrap();
        assert_eq!(v["accept"][0], 2);
    }

    #[test]
    fn extract_json_picks_last_balanced_block_in_prose() {
        // Model wrote two JSON candidates, second is the real answer.
        let content = "First draft: {\"accept\":[1]}, revised: {\"accept\":[2,3]}";
        let v = extract_json_payload(content, None).unwrap();
        assert_eq!(v["accept"][1], 3);
    }

    #[test]
    fn extract_json_returns_none_on_pure_prose() {
        let v = extract_json_payload("Just prose with no JSON at all.", None);
        assert!(v.is_none());
    }

    #[test]
    fn extract_json_returns_none_on_malformed_only() {
        // Brace counter finds {...} but it's not valid JSON.
        let v = extract_json_payload("garbage: {accept: [unquoted]}", None);
        assert!(v.is_none());
    }

    #[test]
    fn extract_json_handles_doubled_braces_via_lenient() {
        // llama-server's doubled-brace sampling glitch should still recover.
        let v = extract_json_payload(r#"{{"accept":[42]}}"#, None).unwrap();
        assert_eq!(v["accept"][0], 42);
    }

    #[test]
    fn extract_json_recovers_from_prose_when_strict_fails() {
        // Strict parse on the cleaned content would fail (it's prose), but
        // strategy 2 finds the embedded JSON.
        let content = "I think the answer is: {\"valid\":true,\"reason\":\"matches\"}. Done.";
        let v = extract_json_payload(content, None).unwrap();
        assert_eq!(v["valid"], true);
    }
}
