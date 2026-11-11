//! The streaming client on its own: framing, tool call assembly, retries.

mod common;

use common::*;
use e::exec::Cancel;
use e::provider::{Client, Delta, End, Reply};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Stream one request; returns the reply, how it ended and the notices shown.
async fn stream(url: &str) -> (Reply, End, Vec<String>) {
    let client = Client::new(url, "sk-test".into()).unwrap();
    let mut notices = Vec::new();
    let (reply, end) = client
        .stream(&json!({"x": 1}), &Cancel::default(), &mut |d| {
            if let Delta::Notice(n) = d {
                notices.push(n)
            }
        })
        .await;
    (reply, end, notices)
}

#[tokio::test]
async fn request_goes_to_chat_completions_with_bearer_auth() {
    let m = mock(vec![text_reply("hi", usage(1, 1))]).await;
    let client = Client::new(&format!("{}/v1/", m.url), "sk-test".into()).unwrap();
    let body = json!({"model": "m", "messages": [], "stream": true});
    let (reply, end) = client.stream(&body, &Cancel::default(), &mut |_| {}).await;
    assert_eq!((reply.content.as_str(), end), ("hi", End::Done));
    let head = m.heads.lock().unwrap()[0].clone();
    assert!(head.starts_with("post /v1/chat/completions http/1.1"), "trailing slash trimmed: {head}");
    assert!(head.contains("authorization: bearer sk-test"), "{head}");
    assert!(head.contains("accept: text/event-stream"), "{head}");
    assert!(head.contains("content-type: application/json"), "{head}");
    assert_eq!(m.requests.lock().unwrap()[0], body, "the body is sent as given");
}

#[tokio::test]
async fn sse_framing_variants_are_parsed() {
    // No space after `data:`, CRLF line ends, comments, `event:` lines and blank lines.
    let raw = [
        ": comment\r\n",
        "event: message\r\n",
        &format!("data:{}\r\n\r\n", delta(json!({"reasoning_content": "r1"}))),
        &format!("data: {}\r\n", delta(json!({"content": "Hel"}))),
        "\r\n",
        &format!("data:   {}\n\n", delta(json!({"content": "lo"}))),
        &format!("data: {}\n\n", finish("stop", usage(10, 2))),
        "data: [DONE]\r\n\r\n",
        "data: this is never parsed\n\n",
    ]
    .concat();
    let m = mock(vec![Resp::Raw(raw)]).await;
    let (reply, end, notices) = stream(&m.url).await;
    assert_eq!(end, End::Done);
    assert!(notices.is_empty());
    assert_eq!(reply.content, "Hello");
    assert_eq!(reply.reasoning.as_deref(), Some("r1"));
    assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    assert_eq!(reply.usage.unwrap().prompt_tokens, 10);
}

#[tokio::test]
async fn deltas_are_reported_in_order() {
    let m = mock(vec![text_reply("abcd", usage(1, 1))]).await;
    let client = Client::new(&m.url, "k".into()).unwrap();
    let mut seen = Vec::new();
    client
        .stream(&json!({}), &Cancel::default(), &mut |d| match d {
            Delta::Text(t) => seen.push(format!("t:{t}")),
            Delta::Reasoning(t) => seen.push(format!("r:{t}")),
            Delta::Notice(n) => seen.push(format!("n:{n}")),
        })
        .await;
    assert_eq!(seen, ["r:thinking ", "r:done", "t:ab", "t:cd", "t:"]);
}

#[tokio::test]
async fn tool_calls_are_assembled_from_interleaved_deltas() {
    let tc = |tc: Value| delta(json!({"tool_calls": [tc]}));
    let ev = vec![
        tc(json!({"index": 0, "id": "a", "type": "function", "function": {"name": "read", "arguments": ""}})),
        tc(
            json!({"index": 1, "id": "b", "type": "function", "function": {"name": "bash", "arguments": "{\"comm"}}),
        ),
        tc(json!({"index": 0, "function": {"name": "", "arguments": "{\"path\":"}})),
        tc(json!({"index": 1, "id": "", "function": {"arguments": "and\": \"ls\"}"}})),
        tc(json!({"index": 0, "function": {"arguments": " \"x\"}"}})),
        // A call whose id never arrives: the agent fills it in.
        tc(json!({"index": 2, "function": {"name": "write", "arguments": "{}"}})),
        finish("tool_calls", usage(5, 5)),
    ];
    let m = mock(vec![Resp::Sse(ev)]).await;
    let (reply, end, _) = stream(&m.url).await;
    assert_eq!(end, End::Done);
    let calls: Vec<(&str, &str, &str)> = reply
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.function.name.as_str(), c.function.arguments.as_str()))
        .collect();
    assert_eq!(
        calls,
        [("a", "read", "{\"path\": \"x\"}"), ("b", "bash", "{\"command\": \"ls\"}"), ("", "write", "{}")]
    );
    assert!(reply.tool_calls.iter().all(|c| c.kind == "function"));
    assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
}

#[tokio::test]
async fn deepseek_usage_fields_are_kept() {
    let u = json!({"prompt_tokens": 100, "completion_tokens": 7,
                   "prompt_cache_hit_tokens": 60, "prompt_cache_miss_tokens": 40});
    let m = mock(vec![Resp::Sse(vec![delta(json!({"content": "x"})), finish("stop", u)])]).await;
    let (reply, _, _) = stream(&m.url).await;
    let u = reply.usage.unwrap();
    assert_eq!(u.split(), (60, 0, 40));
    assert_eq!(u.total(), 107);
}

#[tokio::test]
async fn a_finished_reply_without_done_marker_is_complete() {
    let m = mock(vec![Resp::Cut(vec![delta(json!({"content": "ok"})), finish("stop", usage(1, 1))])]).await;
    let (reply, end, notices) = stream(&m.url).await;
    assert_eq!((reply.content.as_str(), end), ("ok", End::Done));
    assert!(notices.is_empty());
    assert_eq!(m.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_stream_cut_before_any_output_is_retried() {
    let m = mock(vec![Resp::Cut(vec![]), text_reply("second", usage(1, 1))]).await;
    let t = Instant::now();
    let (reply, end, notices) = stream(&m.url).await;
    assert_eq!((reply.content.as_str(), end), ("second", End::Done));
    assert_eq!(notices, ["stream ended before the reply finished; retry 1/4 in 2s"]);
    assert!(t.elapsed() >= Duration::from_secs(2), "backs off before retrying");
    assert_eq!(m.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_stream_cut_after_output_fails_without_retry() {
    let m =
        mock(vec![Resp::Cut(vec![delta(json!({"content": "par"}))]), text_reply("never", usage(1, 1))]).await;
    let (reply, end, notices) = stream(&m.url).await;
    assert_eq!(end, End::Failed("stream ended before the reply finished".into()));
    assert_eq!(reply.content, "par", "the partial reply is returned");
    assert!(notices.is_empty());
    assert_eq!(m.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_malformed_chunk_is_not_retried() {
    let m = mock(vec![Resp::Raw("data: {not json\n\n".into()), text_reply("never", usage(1, 1))]).await;
    let (_, end, _) = stream(&m.url).await;
    assert!(matches!(&end, End::Failed(e) if e.starts_with("bad stream chunk") && e.contains("{not json")));
    assert_eq!(m.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rate_limits_are_retried_and_client_errors_are_not() {
    let m = mock(vec![Resp::Status(429, "slow down".into()), text_reply("ok", usage(1, 1))]).await;
    let (reply, end, notices) = stream(&m.url).await;
    assert_eq!((reply.content.as_str(), end), ("ok", End::Done));
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("429") && notices[0].contains("slow down"), "{notices:?}");

    for code in [400, 401, 403, 404, 422] {
        let m = mock(vec![Resp::Status(code, "  nope \n".into())]).await;
        let (_, end, notices) = stream(&m.url).await;
        let End::Failed(e) = end else { panic!("{code}: {end:?}") };
        assert!(e.starts_with(&format!("HTTP {code}")) && e.ends_with(": nope"), "{e}");
        assert!(notices.is_empty(), "{code} is not retried");
    }
}

#[tokio::test]
async fn long_error_bodies_are_cut_to_2000_chars() {
    let m = mock(vec![Resp::Status(400, "é".repeat(5000))]).await;
    let (_, end, _) = stream(&m.url).await;
    let End::Failed(e) = end else { panic!() };
    assert_eq!(e.chars().filter(|&c| c == 'é').count(), 2000);
}

#[tokio::test]
async fn cancel_during_the_retry_backoff_returns_at_once() {
    // Nothing listens on this port: the connection is refused and retried.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let client = Client::new(&format!("http://127.0.0.1:{port}"), "k".into()).unwrap();
    let cancel = Cancel::default();
    let c = cancel.clone();
    let mut notices = Vec::new();
    let t = Instant::now();
    let (_, end) = client
        .stream(&json!({}), &cancel, &mut |d| {
            if let Delta::Notice(n) = d {
                notices.push(n);
                c.cancel();
            }
        })
        .await;
    assert_eq!(end, End::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(2), "did not wait out the backoff");
    assert_eq!(notices.len(), 1);
    assert!(notices[0].starts_with("request failed"), "{notices:?}");
}

#[tokio::test]
async fn cancel_while_waiting_for_the_first_byte() {
    let m = mock(vec![Resp::Stall(vec![])]).await;
    let client = Client::new(&m.url, "k".into()).unwrap();
    let cancel = Cancel::default();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.cancel();
    });
    let t = Instant::now();
    let (reply, end) = client.stream(&json!({}), &cancel, &mut |_| {}).await;
    assert_eq!(end, End::Cancelled);
    assert!(reply.content.is_empty() && reply.tool_calls.is_empty());
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn permanent_in_band_errors_keep_the_api_message() {
    let err = json!({"error": {"type": "invalid_request_error", "message": "messages[3]: bad role"}});
    let m = mock(vec![Resp::Sse(vec![err])]).await;
    let (_, end, notices) = stream(&m.url).await;
    let End::Failed(e) = end else { panic!() };
    assert!(e.starts_with("API error: ") && e.contains("bad role"), "{e}");
    assert!(notices.is_empty());
}
