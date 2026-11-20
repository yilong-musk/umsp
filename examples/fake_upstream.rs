//! Fake upstream used by the load test (spec 8.4). Not part of the binary.
//!
//! ```text
//! cargo run --release --example fake_upstream -- \
//!     --listen 127.0.0.1:19999 --delay-ms 2 --chunks 12 --chunk-ms 4
//! ```
//!
//! Serves `POST /v1/chat/completions` in both shapes the gateway proxies:
//! a JSON completion (with usage) and an SSE stream ending in `[DONE]`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

#[derive(Debug)]
struct Opts {
    delay_ms: u64,
    chunks: usize,
    chunk_ms: u64,
}

#[tokio::main]
async fn main() {
    let mut opts = Opts {
        delay_ms: 0,
        chunks: 12,
        chunk_ms: 4,
    };
    let mut listen = "127.0.0.1:19999".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().expect("--listen needs a value"),
            "--delay-ms" => {
                opts.delay_ms = args
                    .next()
                    .expect("--delay-ms needs a value")
                    .parse()
                    .unwrap()
            }
            "--chunks" => {
                opts.chunks = args
                    .next()
                    .expect("--chunks needs a value")
                    .parse()
                    .unwrap()
            }
            "--chunk-ms" => {
                opts.chunk_ms = args
                    .next()
                    .expect("--chunk-ms needs a value")
                    .parse()
                    .unwrap()
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    let opts = Arc::new(opts);

    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/chat/completions", post(chat))
        .with_state(opts.clone());
    let listener = tokio::net::TcpListener::bind(&listen).await.unwrap();
    println!(
        "{}",
        json!({"event": "ready", "addr": listener.local_addr().unwrap().to_string(),
               "delay_ms": opts.delay_ms, "chunks": opts.chunks, "chunk_ms": opts.chunk_ms})
    );
    axum::serve(listener, app).await.unwrap();
}

async fn chat(State(opts): State<Arc<Opts>>, Json(body): Json<Value>) -> Response {
    if opts.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(opts.delay_ms)).await;
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if !stream {
        return Json(json!({
            "id": "load-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 20, "total_tokens": 32}
        }))
        .into_response();
    }

    let usage = json!({
        "prompt_tokens": 12,
        "completion_tokens": 20,
        "total_tokens": 32,
        "prompt_tokens_details": {"cached_tokens": 3},
        "completion_tokens_details": {"reasoning_tokens": 5}
    });
    let mut events: Vec<String> = Vec::with_capacity(opts.chunks + 2);
    for index in 0..opts.chunks {
        events.push(format!(
            "data: {}\n\n",
            json!({
                "id": "load-1",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"content": format!("c{index}")}}]
            })
        ));
    }
    events.push(format!(
        "data: {}\n\n",
        json!({"id": "load-1", "choices": [], "usage": usage})
    ));
    events.push("data: [DONE]\n\n".to_string());

    let gap = Duration::from_millis(opts.chunk_ms);
    let body = Body::from_stream(futures_util::stream::unfold(
        (0usize, events, gap),
        |(index, events, gap)| async move {
            if index >= events.len() {
                return None;
            }
            if index > 0 && !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
            let chunk = events[index].clone();
            Some((
                Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)),
                (index + 1, events, gap),
            ))
        },
    ));
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .expect("valid response")
}
