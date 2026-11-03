use super::proxy;
use crate::llama::LlamaClient;
use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use futures::StreamExt;
use kob_gpu_gate::GpuLock;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const SEARCH: &str = "_meiliSearchInIndex";
const OTHER: &str = "get_weather";

#[derive(Clone)]
enum Frame {
    Bytes(Vec<u8>),
    Wait,
    Break,
}

fn data(v: Value) -> Frame {
    Frame::Bytes(format!("data: {v}\n\n").into_bytes())
}

fn done() -> Frame {
    Frame::Bytes(b"data: [DONE]\n\n".to_vec())
}

fn chunk(delta: Value, finish: Option<&str>) -> Frame {
    data(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }))
}

fn text(s: &str) -> Frame {
    chunk(json!({"content": s}), None)
}

fn call_start(index: u64, name: &str, args: &str) -> Frame {
    chunk(
        json!({"tool_calls": [{
            "index": index,
            "id": format!("call_{index}"),
            "type": "function",
            "function": {"name": name, "arguments": args},
        }]}),
        None,
    )
}

fn call_args(index: u64, args: &str) -> Frame {
    chunk(
        json!({"tool_calls": [{"index": index, "function": {"arguments": args}}]}),
        None,
    )
}

fn finish(reason: &str) -> Frame {
    chunk(json!({}), Some(reason))
}

fn usage() -> Frame {
    data(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test",
        "choices": [],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
    }))
}

fn search_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": SEARCH,
            "description": "Search.\n- movies: Movies in the library",
            "parameters": {
                "type": "object",
                "properties": {
                    "index_uid": {"type": "string"},
                    "q": {"type": "string"},
                    "filter": {"type": "string"},
                },
                "required": ["index_uid", "q", "filter"],
            },
        },
    })
}

fn other_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": OTHER,
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
        },
    })
}

fn request(stream: bool) -> Value {
    json!({
        "model": "test",
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [search_tool(), other_tool()],
    })
}

enum Upstream {
    Sse(Vec<Frame>),
    Json(Value),
}

struct Harness {
    koblem: String,
    gate: Arc<Notify>,
    upstream_bodies: Arc<Mutex<Vec<Value>>>,
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn sse_body(frames: Vec<Frame>, gate: Arc<Notify>) -> Response {
    let stream = futures::stream::unfold(
        (frames.into_iter(), gate),
        |(mut frames, gate)| async move {
            loop {
                match frames.next()? {
                    Frame::Wait => gate.notified().await,
                    Frame::Bytes(b) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        return Some((Ok(Bytes::from(b)), (frames, gate)));
                    }
                    Frame::Break => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        return Some((
                            Err(std::io::Error::other("engine crashed")),
                            (frames, gate),
                        ));
                    }
                }
            }
        },
    );
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn harness(upstream: Upstream) -> Harness {
    let gate = Arc::new(Notify::new());
    let upstream_bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = Arc::new(upstream);
    let (engine_gate, seen) = (gate.clone(), upstream_bodies.clone());
    let engine = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let (gate, seen, upstream) = (engine_gate.clone(), seen.clone(), upstream.clone());
            async move {
                seen.lock().unwrap().push(body);
                match &*upstream {
                    Upstream::Sse(frames) => sse_body(frames.clone(), gate),
                    Upstream::Json(v) => Json(v.clone()).into_response(),
                }
            }
        }),
    );
    let llama = LlamaClient::new(serve(engine).await, GpuLock::new());
    let db = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .unwrap();
    let koblem = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let (llama, db) = (llama.clone(), db.clone());
            async move { proxy(&llama, &db, &headers, body).await }
        }),
    );
    Harness {
        koblem: serve(koblem).await,
        gate,
        upstream_bodies,
    }
}

#[derive(Debug, Default)]
struct Relayed {
    events: Vec<String>,
    broke: bool,
}

impl Relayed {
    fn json(&self) -> Vec<Value> {
        self.events
            .iter()
            .filter_map(|e| serde_json::from_str(e).ok())
            .collect()
    }

    fn done(&self) -> bool {
        self.events.iter().any(|e| e == "[DONE]")
    }

    fn content(&self) -> String {
        self.json()
            .iter()
            .filter_map(|c| {
                c.pointer("/choices/0/delta/content")?
                    .as_str()
                    .map(String::from)
            })
            .collect()
    }

    fn finish_reasons(&self) -> Vec<String> {
        self.json()
            .iter()
            .filter_map(|c| {
                c.pointer("/choices/0/finish_reason")?
                    .as_str()
                    .map(String::from)
            })
            .collect()
    }

    fn usage(&self) -> Option<Value> {
        self.json()
            .into_iter()
            .find_map(|c| c.get("usage").filter(|u| !u.is_null()).cloned())
    }

    fn errors(&self) -> Vec<Value> {
        self.json()
            .into_iter()
            .filter_map(|c| c.get("error").cloned())
            .collect()
    }

    fn tool_calls(&self) -> Vec<(String, String, Value)> {
        let mut calls: Vec<(u64, String, String, String)> = Vec::new();
        for c in self.json() {
            let Some(tcs) = c
                .pointer("/choices/0/delta/tool_calls")
                .and_then(Value::as_array)
            else {
                continue;
            };
            for tc in tcs {
                let index = tc["index"].as_u64().unwrap();
                let slot = match calls.iter().position(|c| c.0 == index) {
                    Some(i) => i,
                    None => {
                        calls.push((index, String::new(), String::new(), String::new()));
                        calls.len() - 1
                    }
                };
                if let Some(id) = tc["id"].as_str() {
                    calls[slot].1 = id.to_string();
                }
                if let Some(name) = tc.pointer("/function/name").and_then(Value::as_str) {
                    calls[slot].2 = name.to_string();
                }
                if let Some(args) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                    calls[slot].3.push_str(args);
                }
            }
        }
        calls.sort_by_key(|c| c.0);
        calls
            .into_iter()
            .map(|(_, id, name, args)| {
                (
                    id,
                    name,
                    serde_json::from_str(&args).unwrap_or(Value::String(args)),
                )
            })
            .collect()
    }
}

fn split_events(buf: &mut String, out: &mut Vec<String>) {
    while let Some(end) = buf.find("\n\n") {
        let event = buf[..end].to_string();
        buf.drain(..end + 2);
        if let Some(payload) = event.strip_prefix("data:") {
            out.push(payload.trim().to_string());
        }
    }
}

async fn post_stream(h: &Harness, body: Value) -> reqwest::Response {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", h.koblem))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "status {}", resp.status());
    resp
}

async fn read_until<S>(
    stream: &mut S,
    relayed: &mut Relayed,
    buf: &mut String,
    until: impl Fn(&Relayed) -> bool,
) where
    S: futures::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    while !until(relayed) {
        match stream.next().await {
            Some(Ok(b)) => {
                buf.push_str(std::str::from_utf8(&b).expect("relay split a UTF-8 sequence"));
                split_events(buf, &mut relayed.events);
            }
            Some(Err(_)) => {
                relayed.broke = true;
                return;
            }
            None => return,
        }
    }
}

async fn stream_all(h: &Harness, body: Value) -> Relayed {
    let mut stream = post_stream(h, body).await.bytes_stream();
    let mut relayed = Relayed::default();
    let mut buf = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        read_until(&mut stream, &mut relayed, &mut buf, |_| false),
    )
    .await
    .expect("relay hung");
    relayed
}

#[tokio::test]
async fn text_and_usage_after_a_tool_call_are_relayed() {
    for (name, args, expected) in [
        (
            SEARCH,
            r#"{"index_uid":"movies","q":"alien","monitored_only":true}"#,
            json!({"index_uid": "movies", "q": "alien", "filter": "is_monitored = true"}),
        ),
        (OTHER, r#"{"city":"Oslo"}"#, json!({"city": "Oslo"})),
    ] {
        let (head, tail) = args.split_at(args.len() / 2);
        let h = harness(Upstream::Sse(vec![
            text("Let me check. "),
            call_start(0, name, head),
            call_args(0, tail),
            text("One moment."),
            finish("tool_calls"),
            usage(),
            done(),
        ]))
        .await;

        let r = stream_all(&h, request(true)).await;

        assert_eq!(
            r.content(),
            "Let me check. One moment.",
            "{name}: {:?}",
            r.events
        );
        assert_eq!(
            r.tool_calls(),
            vec![("call_0".into(), name.into(), expected)],
            "{name}"
        );
        assert_eq!(r.finish_reasons(), vec!["tool_calls"], "{name}");
        assert_eq!(r.usage().unwrap()["total_tokens"], 18, "{name}");
        assert!(r.done() && !r.broke, "{name}");
    }
}

#[tokio::test]
async fn streamed_request_has_search_tool_definition_rewritten() {
    let h = harness(Upstream::Sse(vec![text("hi"), finish("stop"), done()])).await;
    stream_all(&h, request(true)).await;
    let sent = h.upstream_bodies.lock().unwrap()[0].clone();
    let props = &sent["tools"][0]["function"]["parameters"]["properties"];
    assert!(props.get("filter").is_none());
    assert!(props.get("monitored_only").is_some());
    assert_eq!(sent["tools"][1], other_tool());
}

#[tokio::test]
async fn non_search_tool_calls_stream_as_they_arrive() {
    let h = harness(Upstream::Sse(vec![
        call_start(0, OTHER, r#"{"city":"#),
        call_args(0, r#""Oslo"}"#),
        Frame::Wait,
        call_start(
            1,
            SEARCH,
            r#"{"index_uid":"movies","q":"x","watched":"watched"}"#,
        ),
        finish("tool_calls"),
        done(),
    ]))
    .await;

    let mut stream = post_stream(&h, request(true)).await.bytes_stream();
    let mut r = Relayed::default();
    let mut buf = String::new();
    let other_args_seen = |r: &Relayed| {
        r.json()
            .iter()
            .filter_map(|c| c.pointer("/choices/0/delta/tool_calls/0/function/arguments"))
            .filter_map(Value::as_str)
            .collect::<String>()
            == r#"{"city":"Oslo"}"#
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        read_until(&mut stream, &mut r, &mut buf, other_args_seen),
    )
    .await
    .expect("non-search tool call deltas were held back instead of streamed");
    assert!(r.finish_reasons().is_empty());

    h.gate.notify_one();
    tokio::time::timeout(
        Duration::from_secs(5),
        read_until(&mut stream, &mut r, &mut buf, |_| false),
    )
    .await
    .unwrap();

    assert_eq!(
        r.tool_calls(),
        vec![
            ("call_0".into(), OTHER.into(), json!({"city": "Oslo"})),
            (
                "call_1".into(),
                SEARCH.into(),
                json!({"index_uid": "movies", "q": "x", "filter": "is_watched = true"}),
            ),
        ]
    );
    assert_eq!(r.finish_reasons(), vec!["tool_calls"]);
    assert!(r.done() && !r.broke);
}

#[tokio::test]
async fn upstream_failure_mid_stream_surfaces_an_error() {
    let cases: Vec<(&str, Vec<Frame>)> = vec![
        ("connection drop", vec![text("Hel"), Frame::Break]),
        ("eof before finish", vec![text("Hel")]),
        (
            "drop during search call",
            vec![
                text("Hel"),
                call_start(0, SEARCH, r#"{"index_uid":"#),
                Frame::Break,
            ],
        ),
        (
            "drop during other call",
            vec![
                text("Hel"),
                call_start(0, OTHER, r#"{"city":"#),
                Frame::Break,
            ],
        ),
    ];
    for (case, frames) in cases {
        let h = harness(Upstream::Sse(frames)).await;
        let r = stream_all(&h, request(true)).await;

        assert_eq!(r.content(), "Hel", "{case}");
        assert!(
            !r.done(),
            "{case}: clean [DONE] after a broken upstream: {:?}",
            r.events
        );
        assert!(r.finish_reasons().is_empty(), "{case}: {:?}", r.events);
        let errors = r.errors();
        assert_eq!(errors.len(), 1, "{case}: {:?}", r.events);
        assert!(
            errors[0]["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{case}"
        );
        let last: Value = serde_json::from_str(r.events.last().unwrap()).unwrap();
        assert!(
            last.get("error").is_some(),
            "{case}: error must be the last event"
        );
        assert!(
            r.broke,
            "{case}: body must end abnormally so clients that ignore error events still fail"
        );
        assert!(
            r.tool_calls().iter().all(|(_, name, _)| name != SEARCH),
            "{case}: half-buffered search call flushed"
        );
    }
}

#[tokio::test]
async fn upstream_that_finishes_without_done_still_ends_cleanly() {
    let h = harness(Upstream::Sse(vec![text("Hello"), finish("stop"), usage()])).await;
    let r = stream_all(&h, request(true)).await;
    assert_eq!(r.content(), "Hello");
    assert_eq!(r.finish_reasons(), vec!["stop"]);
    assert!(r.usage().is_some());
    assert!(r.errors().is_empty());
    assert!(r.done() && !r.broke);
}

#[tokio::test]
async fn multibyte_characters_split_across_upstream_reads_survive() {
    let Frame::Bytes(whole) = text("café 🎬") else {
        unreachable!()
    };
    let cut = whole.windows(2).position(|w| w == [0xC3, 0xA9]).unwrap() + 1;
    let h = harness(Upstream::Sse(vec![
        Frame::Bytes(whole[..cut].to_vec()),
        Frame::Bytes(whole[cut..].to_vec()),
        finish("stop"),
        done(),
    ]))
    .await;
    let r = stream_all(&h, request(true)).await;
    assert_eq!(r.content(), "café 🎬");
}

#[tokio::test]
async fn non_streaming_rewrites_only_search_tool_calls() {
    let h = harness(Upstream::Json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Looking.",
                "tool_calls": [
                    {"id": "call_0", "type": "function", "function": {"name": OTHER, "arguments": "{\"city\":\"Oslo\"}"}},
                    {"id": "call_1", "type": "function", "function": {"name": SEARCH, "arguments": "{\"index_uid\":\"books\",\"q\":\"x\",\"in_library_only\":true}"}},
                ],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
    })))
    .await;

    let body: Value = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", h.koblem))
        .json(&request(false))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let calls = body["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls[0]["function"]["arguments"], "{\"city\":\"Oslo\"}");
    let search: Value =
        serde_json::from_str(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        search,
        json!({"index_uid": "books", "q": "x", "filter": "is_in_library = true"})
    );
    assert_eq!(body["choices"][0]["message"]["content"], "Looking.");
    assert_eq!(body["usage"]["total_tokens"], 18);
    let sent = h.upstream_bodies.lock().unwrap()[0].clone();
    assert!(sent["tools"][0]["function"]["parameters"]["properties"]["monitored_only"].is_object());
}
