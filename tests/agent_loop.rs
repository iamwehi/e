mod common;

use common::*;
use e::agent::{RunEnd, Session, history_is_valid};
use e::exec::Cancel;
use e::provider::Message;
use serde_json::{Value, json};
use std::time::Duration;

fn session(url: &str, tag: &str) -> Session {
    Session::new(config(url), e::config::Config::default().model, tempdir(tag), false).unwrap()
}

/// Checks a recorded request body against the API's rules.
fn check_request(body: &Value) {
    let msgs: Vec<Message> = serde_json::from_value(body["messages"].clone()).unwrap();
    assert!(matches!(msgs[0], Message::System { .. }), "system prompt first");
    history_is_valid(&msgs[1..]).unwrap();
    for m in &msgs {
        if let Message::Assistant { reasoning_content, .. } = m {
            assert!(reasoning_content.is_some(), "reasoning_content must be passed back");
        }
    }
    assert_eq!(body["tools"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn loop_runs_tools_and_ends_on_a_reply_without_tool_calls() {
    let m = mock(vec![
        tool_reply(
            &[
                ("c1", "bash", json!({"command": "echo hello"})),
                ("c2", "write", json!({"path": "a/b.txt", "content": "x"})),
            ],
            usage(1000, 50),
        ),
        text_reply("all done", usage(1200, 20)),
    ])
    .await;
    let mut s = session(&m.url, "loop");
    let mut texts = String::new();
    let end = s
        .run("audit", &Cancel::default(), &mut |ev| {
            if let e::agent::Event::Text(t) = ev {
                texts.push_str(t)
            }
        })
        .await;
    assert_eq!(end, RunEnd::Done("all done".into()));
    assert_eq!(texts, "all done");
    let reqs = m.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2, "exactly one request per turn, none after the final reply");
    for r in &reqs {
        check_request(r);
    }
    // The system prompt is identical across requests (prefix caching).
    assert_eq!(reqs[0]["messages"][0], reqs[1]["messages"][0]);
    // Venice: our system prompt only, and a stable cache routing key.
    for r in &reqs {
        assert_eq!(r["model"], "deepseek-v4-1-flash");
        assert_eq!(r["venice_parameters"]["include_venice_system_prompt"], false);
        assert_eq!(r["stream_options"]["include_usage"], true);
        assert!(r["prompt_cache_key"].as_str().unwrap().starts_with("e-"));
    }
    assert_eq!(reqs[0]["prompt_cache_key"], reqs[1]["prompt_cache_key"]);
    assert!(s.meter.cost > 0.0, "default model is priced");
    let msgs = reqs[1]["messages"].as_array().unwrap();
    assert_eq!(msgs[2]["reasoning_content"], "plan");
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "c1");
    assert!(msgs[3]["content"].as_str().unwrap().contains("hello"));
    assert!(msgs[3]["content"].as_str().unwrap().contains("[exit code 0"));
    assert_eq!(msgs[4]["tool_call_id"], "c2");
    assert_eq!(std::fs::read_to_string(s.cwd.join("a/b.txt")).unwrap(), "x");
    assert_eq!(s.context_tokens, 1220);
    // Transcript records every event.
    let log = std::fs::read_to_string(s.transcript.dir.join("transcript.jsonl")).unwrap();
    for kind in ["session", "user", "request", "response", "tool_call", "tool_result", "run_end"] {
        assert!(log.contains(&format!("\"type\":\"{kind}\"")), "missing {kind}");
    }
}

#[tokio::test]
async fn config_can_point_at_deepseek_instead() {
    let m = mock(vec![text_reply("ok", usage(10, 1))]).await;
    let cfg: e::config::Config = serde_json::from_value(json!({
        "base_url": m.url, "api_key_env": "E_TEST_UNSET_KEY", "api_key": "k",
        "extra_body": {}, "prompt_cache_key": false, "model": "deepseek-flash"
    }))
    .unwrap();
    let mut s = Session::new(cfg.clone(), cfg.model.clone(), tempdir("deepseek"), false).unwrap();
    assert_eq!(s.run("x", &Cancel::default(), &mut |_| {}).await, RunEnd::Done("ok".into()));
    let req = m.requests.lock().unwrap()[0].clone();
    assert_eq!(req["model"], "deepseek-flash");
    assert!(req.get("venice_parameters").is_none() && req.get("prompt_cache_key").is_none());

    let missing = e::config::Config { api_key: None, ..cfg };
    let err = Session::new(missing, "m".into(), tempdir("nokey"), false).err().unwrap();
    assert!(err.contains("E_TEST_UNSET_KEY is not set"), "{err}");
}

#[tokio::test]
async fn unknown_tool_and_bad_arguments_become_error_results() {
    let m = mock(vec![
        tool_reply(&[("c1", "nope", json!({})), ("c2", "read", json!({"wrong": 1}))], usage(10, 5)),
        text_reply("ok", usage(20, 5)),
    ])
    .await;
    let mut s = session(&m.url, "badtool");
    let end = s.run("x", &Cancel::default(), &mut |_| {}).await;
    assert_eq!(end, RunEnd::Done("ok".into()));
    history_is_valid(&s.history).unwrap();
    let Message::Tool { content, .. } = &s.history[2] else { panic!() };
    assert!(content.contains("unknown tool"));
    let Message::Tool { content, .. } = &s.history[3] else { panic!() };
    assert!(content.contains("invalid arguments for read"));
}

#[tokio::test]
async fn interrupt_during_a_tool_keeps_history_valid() {
    let m = mock(vec![
        tool_reply(
            &[
                ("c1", "bash", json!({"command": "sleep 30"})),
                ("c2", "bash", json!({"command": "echo never"})),
            ],
            usage(100, 10),
        ),
        text_reply("resumed", usage(200, 10)),
    ])
    .await;
    let mut s = session(&m.url, "int-tool");
    let cancel = Cancel::default();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        c2.cancel();
    });
    let t = std::time::Instant::now();
    let end = s.run("go", &cancel, &mut |_| {}).await;
    assert_eq!(end, RunEnd::Interrupted);
    assert!(t.elapsed() < Duration::from_secs(10), "sleep was killed");
    history_is_valid(&s.history).unwrap();
    let Message::Tool { content, .. } = &s.history[2] else { panic!() };
    assert!(content.contains("interrupted by the user"), "{content}");
    let Message::Tool { content, .. } = &s.history[3] else { panic!() };
    assert!(content.contains("Not executed"), "{content}");

    // The next run sends a history the API accepts.
    let end = s.run("continue", &Cancel::default(), &mut |_| {}).await;
    assert_eq!(end, RunEnd::Done("resumed".into()));
    check_request(&m.requests.lock().unwrap()[1]);
}

#[tokio::test]
async fn interrupt_during_streaming_drops_partial_tool_calls() {
    let mut events =
        vec![json!({"choices": [{"index": 0, "delta": {"content": "Looking at "}, "finish_reason": null}]})];
    events.extend(tool_events(&[("c1", "bash", json!({"command": "ls"}))]));
    let m = mock(vec![Resp::Stall(events), text_reply("fine", usage(10, 1))]).await;
    let mut s = session(&m.url, "int-stream");
    let cancel = Cancel::default();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c2.cancel();
    });
    let end = s.run("go", &cancel, &mut |_| {}).await;
    assert_eq!(end, RunEnd::Interrupted);
    history_is_valid(&s.history).unwrap();
    let Message::Assistant { content, tool_calls, .. } = &s.history[1] else { panic!() };
    assert!(content.starts_with("Looking at "));
    assert!(tool_calls.is_empty());
    assert_eq!(s.run("again", &Cancel::default(), &mut |_| {}).await, RunEnd::Done("fine".into()));
    check_request(&m.requests.lock().unwrap()[1]);
}

#[tokio::test]
async fn budget_warns_at_85_percent_and_ends_at_100() {
    let m = mock(vec![
        tool_reply(&[("c1", "bash", json!({"command": "true"}))], usage(860, 0)),
        tool_reply(&[("c2", "bash", json!({"command": "true"}))], usage(900, 10)),
        tool_reply(&[("c3", "bash", json!({"command": "touch ran"}))], usage(1000, 0)),
    ])
    .await;
    let mut cfg = config(&m.url);
    cfg.context_budget = 1000;
    let mut s = Session::new(cfg, "deepseek-v4-1-flash".into(), tempdir("budget"), false).unwrap();
    let end = s.run("go", &Cancel::default(), &mut |_| {}).await;
    assert!(matches!(end, RunEnd::Budget(_)), "{end:?}");
    assert_eq!(end.exit_code(), 2);
    history_is_valid(&s.history).unwrap();
    let warnings = s
        .history
        .iter()
        .filter(|m| matches!(m, Message::User { content } if content.contains("NOTES.md")))
        .count();
    assert_eq!(warnings, 1, "the wrap-up message is injected once");
    assert!(!s.cwd.join("ran").exists(), "no tool runs once the budget is spent");
    let Some(Message::Tool { content, .. }) = s.history.last() else { panic!() };
    assert!(content.contains("budget"));
}

#[tokio::test]
async fn transient_errors_are_retried_and_fatal_ones_end_the_run() {
    let m = mock(vec![Resp::Status(503, "busy".into()), text_reply("ok", usage(1, 1))]).await;
    let mut s = session(&m.url, "retry");
    let mut notices = 0;
    let end = s
        .run("x", &Cancel::default(), &mut |ev| {
            if let e::agent::Event::Notice(_) = ev {
                notices += 1
            }
        })
        .await;
    assert_eq!(end, RunEnd::Done("ok".into()));
    assert_eq!(notices, 1);

    let m = mock(vec![Resp::Status(400, "{\"error\":\"bad\"}".into())]).await;
    let mut s = session(&m.url, "fatal");
    let end = s.run("x", &Cancel::default(), &mut |_| {}).await;
    assert!(matches!(&end, RunEnd::Error(e) if e.contains("400")), "{end:?}");
    assert_eq!(m.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn truncated_reply_does_not_execute_tool_calls() {
    let mut ev = tool_events(&[("c1", "bash", json!({"command": "touch ran"}))]);
    ev.push(
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "length"}], "usage": usage(10, 10)}),
    );
    let m = mock(vec![Resp::Sse(ev), text_reply("ok", usage(1, 1))]).await;
    let mut s = session(&m.url, "length");
    assert_eq!(s.run("x", &Cancel::default(), &mut |_| {}).await, RunEnd::Done("ok".into()));
    assert!(!s.cwd.join("ran").exists());
    history_is_valid(&s.history).unwrap();
}
