//! A scripted mock of DeepSeek's streaming endpoint on 127.0.0.1. Each
//! connection gets the next scripted response; request bodies are recorded.
#![allow(dead_code)]

use e::config::Config;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub enum Resp {
    /// SSE data payloads, then `[DONE]`.
    Sse(Vec<Value>),
    /// SSE data payloads, then the connection stalls.
    Stall(Vec<Value>),
    Status(u16, String),
}

pub struct Mock {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

pub async fn mock(script: Vec<Resp>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reqs = requests.clone();
    tokio::spawn(async move {
        let mut script = script.into_iter();
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let body = read_request(&mut sock).await;
            reqs.lock().unwrap().push(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null));
            let resp = script.next().unwrap_or(Resp::Status(500, "script exhausted".into()));
            tokio::spawn(async move {
                match resp {
                    Resp::Status(code, text) => {
                        let msg = format!(
                            "HTTP/1.1 {code} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                            text.len()
                        );
                        let _ = sock.write_all(msg.as_bytes()).await;
                    }
                    Resp::Sse(events) => {
                        write_sse(&mut sock, &events).await;
                        let _ = sock.write_all(b"data: [DONE]\n\n").await;
                    }
                    Resp::Stall(events) => {
                        write_sse(&mut sock, &events).await;
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }
                }
            });
        }
    });
    Mock { url, requests }
}

async fn write_sse(sock: &mut tokio::net::TcpStream, events: &[Value]) {
    let head =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n: keep-alive\n\n";
    let _ = sock.write_all(head.as_bytes()).await;
    for e in events {
        let _ = sock.write_all(format!("data: {e}\n\n").as_bytes()).await;
        let _ = sock.flush().await;
    }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return Vec::new();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            return buf[end + 4..].to_vec();
        }
    }
}

pub fn usage(prompt: u64, completion: u64) -> Value {
    json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion,
           "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": prompt,
           "prompt_tokens_details": {"cached_tokens": 0}})
}

/// A text reply split into chunks, with reasoning first.
pub fn text_reply(text: &str, u: Value) -> Resp {
    Resp::Sse(vec![
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "thinking "}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"reasoning_content": "done"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"content": &text[..text.len() / 2]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"content": &text[text.len() / 2..]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "stop"}], "usage": u}),
    ])
}

/// Tool calls `(id, name, args)`, arguments streamed in two pieces.
pub fn tool_events(calls: &[(&str, &str, Value)]) -> Vec<Value> {
    let mut ev = vec![
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "plan"}, "finish_reason": null}]}),
    ];
    for (i, (id, name, args)) in calls.iter().enumerate() {
        let a = args.to_string();
        let (x, y) = a.split_at(a.len() / 2);
        ev.push(json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": i, "id": id, "type": "function", "function": {"name": name, "arguments": x}}]}, "finish_reason": null}]}));
        ev.push(json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": i, "function": {"arguments": y}}]}, "finish_reason": null}]}));
    }
    ev
}

pub fn tool_reply(calls: &[(&str, &str, Value)], u: Value) -> Resp {
    let mut ev = tool_events(calls);
    ev.push(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": u}));
    Resp::Sse(ev)
}

pub fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "e-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn config(url: &str) -> Config {
    Config { base_url: url.into(), api_key: Some("test-key".into()), ..Config::default() }
}
