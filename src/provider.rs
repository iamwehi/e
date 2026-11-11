//! OpenAI-compatible streaming chat completions: Venice by default, DeepSeek's
//! own API by configuration.
//!
//! Assistant messages keep their `reasoning_content` and it is sent back on
//! every request. DeepSeek answers 400 without it when a request carries
//! `tools` (which ours always do); Venice asks for the assistant message to
//! be sent back unmodified, `reasoning_content` included.

use crate::config::Usage;
use crate::exec::Cancel;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

pub enum Delta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
    Notice(String),
}

/// One assistant reply, complete or partial.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum End {
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<Usage>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<ChunkDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChunkDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

const RETRIES: u32 = 4;

pub struct Client {
    http: reqwest::Client,
    url: String,
    key: String,
}

impl Client {
    pub fn new(base_url: &str, key: String) -> Result<Self, String> {
        let base = base_url.trim_end_matches('/');
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Reasoning can take minutes before the first visible token.
            .read_timeout(Duration::from_secs(600));
        let host = base.split("://").nth(1).unwrap_or("").split(['/', ':']).next().unwrap_or("");
        if host == "127.0.0.1" || host == "localhost" {
            b = b.no_proxy();
        }
        let http = b.build().map_err(|e| format!("http client: {e}"))?;
        Ok(Client { http, url: format!("{base}/chat/completions"), key })
    }

    /// Send one request and stream the reply. Retries 429 and 5xx statuses,
    /// connection failures, and failures inside the stream (Venice reports a
    /// timeout before the first chunk as an in-band error frame) as long as
    /// nothing of the reply has arrived yet. Once it has, never retries.
    pub async fn stream(&self, body: &Value, cancel: &Cancel, on: &mut dyn FnMut(Delta)) -> (Reply, End) {
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let mut attempt = 0;
        loop {
            let err = match self.attempt(&bytes, cancel, on).await {
                Attempt::Finished(reply, end) => return (reply, end),
                Attempt::Retry(msg) => msg,
            };
            if attempt >= RETRIES {
                return (Reply::default(), End::Failed(err));
            }
            let wait = 2u64 << attempt;
            attempt += 1;
            on(Delta::Notice(format!("{err}; retry {attempt}/{RETRIES} in {wait}s")));
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                _ = cancel.cancelled() => return (Reply::default(), End::Cancelled),
            }
        }
    }

    async fn attempt(&self, bytes: &[u8], cancel: &Cancel, on: &mut dyn FnMut(Delta)) -> Attempt {
        let req = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(bytes.to_vec());
        let sent = tokio::select! {
            r = req.send() => r,
            _ = cancel.cancelled() => return Attempt::Finished(Reply::default(), End::Cancelled),
        };
        let mut resp = match sent {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                let status = r.status();
                let text: String = r.text().await.unwrap_or_default().chars().take(2000).collect();
                let msg = format!("HTTP {status}: {}", text.trim());
                return match status.as_u16() {
                    429 | 500 | 502 | 503 | 504 => Attempt::Retry(msg),
                    _ => Attempt::Finished(Reply::default(), End::Failed(msg)),
                };
            }
            Err(e) => return Attempt::Retry(format!("request failed: {e}")),
        };

        let mut reply = Reply::default();
        let mut slots = HashMap::new();
        let mut buf: Vec<u8> = Vec::new();
        let fail = |reply: Reply, msg: String, retryable: bool| {
            if retryable && reply.is_empty() {
                Attempt::Retry(msg)
            } else {
                Attempt::Finished(reply, End::Failed(msg))
            }
        };
        loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c,
                _ = cancel.cancelled() => return Attempt::Finished(reply, End::Cancelled),
            };
            match chunk {
                Ok(Some(bytes)) => buf.extend_from_slice(&bytes),
                Ok(None) => break,
                Err(e) => return fail(reply, format!("stream failed: {e}"), true),
            }
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw);
                let Some(data) = line.trim_end().strip_prefix("data:") else { continue };
                let data = data.trim_start();
                if data == "[DONE]" {
                    return Attempt::Finished(reply, End::Done);
                }
                if let Err((msg, retryable)) = apply(&mut reply, &mut slots, data, on) {
                    return fail(reply, msg, retryable);
                }
            }
        }
        if reply.finish_reason.is_some() {
            Attempt::Finished(reply, End::Done)
        } else {
            fail(reply, "stream ended before the reply finished".into(), true)
        }
    }
}

enum Attempt {
    Finished(Reply, End),
    Retry(String),
}

impl Reply {
    /// Nothing of the reply has arrived (and nothing was shown).
    fn is_empty(&self) -> bool {
        self.content.is_empty() && self.reasoning.is_none() && self.tool_calls.is_empty()
    }
}

/// Apply one stream chunk. `slots` maps a stream `index` to the tool call it
/// currently feeds: Venice may give every call of a turn the same index, so
/// a delta with a new `id` starts a new call even when its index is taken.
/// Errors carry whether a retry could help.
fn apply(
    reply: &mut Reply,
    slots: &mut HashMap<usize, usize>,
    data: &str,
    on: &mut dyn FnMut(Delta),
) -> Result<(), (String, bool)> {
    let chunk: Chunk =
        serde_json::from_str(data).map_err(|e| (format!("bad stream chunk: {e}: {data}"), false))?;
    if let Some(err) = chunk.error {
        let text = err.to_string();
        let permanent = text.contains("invalid_request_error") || text.contains("context_length");
        return Err((format!("API error: {text}"), !permanent));
    }
    if let Some(u) = chunk.usage {
        reply.usage = Some(u);
    }
    for choice in chunk.choices {
        if let Some(d) = choice.delta {
            if let Some(r) = d.reasoning_content {
                on(Delta::Reasoning(&r));
                reply.reasoning.get_or_insert_with(String::new).push_str(&r);
            }
            if let Some(t) = d.content {
                on(Delta::Text(&t));
                reply.content.push_str(&t);
            }
            for tc in d.tool_calls.unwrap_or_default() {
                let id = tc.id.filter(|id| !id.is_empty());
                let slot = slots.get(&tc.index).copied().filter(|&p| {
                    let cur: &str = &reply.tool_calls[p].id;
                    cur.is_empty() || id.as_deref().is_none_or(|id| id == cur)
                });
                let p = slot.unwrap_or_else(|| {
                    reply.tool_calls.push(ToolCall {
                        id: String::new(),
                        kind: "function".into(),
                        function: FunctionCall { name: String::new(), arguments: String::new() },
                    });
                    slots.insert(tc.index, reply.tool_calls.len() - 1);
                    reply.tool_calls.len() - 1
                });
                let call = &mut reply.tool_calls[p];
                if let Some(id) = id {
                    call.id = id;
                }
                if let Some(f) = tc.function {
                    if let Some(n) = f.name.filter(|n| !n.is_empty()) {
                        call.function.name = n;
                    }
                    if let Some(a) = f.arguments {
                        call.function.arguments.push_str(&a);
                    }
                }
            }
        }
        if choice.finish_reason.is_some() {
            reply.finish_reason = choice.finish_reason;
        }
    }
    Ok(())
}
