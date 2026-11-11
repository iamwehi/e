//! Pure helpers and tool edge cases not covered by tools.rs.

mod common;

use common::*;
use e::agent::history_is_valid;
use e::config::{Config, Meter, Usage, fmt_tokens};
use e::exec::{self, Cancel};
use e::provider::{FunctionCall, Message, ToolCall};
use e::tools::{self, Ctx};
use e::transcript::{Transcript, civil};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

#[test]
fn civil_converts_unix_time_to_utc_calendar_dates() {
    let ymd = |t: u64| {
        let c = civil(t);
        (c.year, c.month, c.day, c.hour, c.minute, c.second, c.weekday)
    };
    assert_eq!(ymd(0), (1970, 1, 1, 0, 0, 0, 4), "a Thursday");
    assert_eq!(ymd(951_782_400), (2000, 2, 29, 0, 0, 0, 2), "leap day of a 400-year leap year");
    assert_eq!(ymd(1_709_251_199), (2024, 2, 29, 23, 59, 59, 4));
    assert_eq!(ymd(1_709_251_200), (2024, 3, 1, 0, 0, 0, 5));
    assert_eq!(ymd(1_794_355_200), (2026, 11, 11, 0, 0, 0, 3));
    assert_eq!(ymd(4_102_444_799), (2099, 12, 31, 23, 59, 59, 4));
    // Every day for ~30 years: consecutive, weekdays cycle, months valid.
    let (mut prev, mut wd) = (civil(0), 4);
    for day in 1..11_000u64 {
        let c = civil(day * 86_400 + 43_200);
        wd = (wd + 1) % 7;
        assert_eq!(c.weekday, wd);
        assert_eq!(c.hour, 12);
        let next_day = c.day == prev.day + 1 && c.month == prev.month;
        let next_month =
            c.day == 1 && (c.month == prev.month + 1 || (c.month == 1 && c.year == prev.year + 1));
        assert!(next_day || next_month, "day {day}: {}-{}-{}", c.year, c.month, c.day);
        prev = c;
    }
}

#[test]
fn peak_hours_are_half_open_and_weekdays_only() {
    let cfg = Config::from_toml("peak_hours_utc = [[1, 4], [22, 24]]").unwrap();
    let wed = 1_794_355_200; // 2026-11-11, a Wednesday, 00:00 UTC
    let at = |day: u64, hour: u64| wed + day * 86_400 + hour * 3600;
    assert!(!cfg.is_peak(at(0, 0)));
    assert!(cfg.is_peak(at(0, 1)));
    assert!(cfg.is_peak(at(0, 3) + 3599));
    assert!(!cfg.is_peak(at(0, 4)), "end hour is excluded");
    assert!(cfg.is_peak(at(0, 23)));
    assert!(cfg.is_peak(at(2, 2)), "Friday");
    assert!(!cfg.is_peak(at(3, 2)), "Saturday");
    assert!(!cfg.is_peak(at(4, 2)), "Sunday");
    assert!(cfg.is_peak(at(5, 2)), "Monday");
    assert!(!Config::default().is_peak(at(0, 2)), "no peak hours by default");
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: FunctionCall { name: "bash".into(), arguments: "{}".into() },
    }
}

fn assistant(ids: &[&str]) -> Message {
    Message::Assistant {
        content: String::new(),
        reasoning_content: Some("r".into()),
        tool_calls: ids.iter().map(|i| call(i)).collect(),
    }
}

fn result(id: &str) -> Message {
    Message::Tool { tool_call_id: id.into(), content: "ok".into() }
}

fn user() -> Message {
    Message::User { content: "u".into() }
}

#[test]
fn history_validation_catches_every_misordering() {
    let ok = [user(), assistant(&["a", "b"]), result("a"), result("b"), assistant(&[]), user()];
    history_is_valid(&ok).unwrap();
    history_is_valid(&[]).unwrap();

    let bad: [(&[Message], &str); 5] = [
        (&[user(), result("a")], "unexpected tool result a"),
        (&[assistant(&["a", "b"]), result("b"), result("a")], "unexpected tool result b"),
        (&[assistant(&["a", "b"]), result("a"), user()], "unanswered tool calls [\"b\"]"),
        (&[assistant(&["a"]), assistant(&[])], "message 1: unanswered"),
        (&[assistant(&["a"])], "unanswered tool calls at end"),
    ];
    for (h, want) in bad {
        let e = history_is_valid(h).unwrap_err();
        assert!(e.contains(want), "{e}");
    }
    // A duplicate result after the call was answered.
    assert!(history_is_valid(&[assistant(&["a"]), result("a"), result("a")]).is_err());
}

#[test]
fn messages_serialize_in_the_api_shape() {
    let v = serde_json::to_value(assistant(&["a"])).unwrap();
    assert_eq!(
        v,
        json!({"role": "assistant", "content": "", "reasoning_content": "r", "tool_calls": [
            {"id": "a", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]})
    );
    let plain = Message::Assistant { content: "hi".into(), reasoning_content: None, tool_calls: vec![] };
    assert_eq!(serde_json::to_value(&plain).unwrap(), json!({"role": "assistant", "content": "hi"}));
    assert_eq!(
        serde_json::to_value(result("a")).unwrap(),
        json!({"role": "tool", "tool_call_id": "a", "content": "ok"})
    );
    let back: Message = serde_json::from_value(v).unwrap();
    assert_eq!(back, assistant(&["a"]));
}

#[test]
fn tool_arguments_are_summarized_on_one_short_line() {
    use e::term::summarize_args;
    assert_eq!(
        summarize_args("bash", r#"{"command": "forge test\n  -vvv\t--match x"}"#),
        "forge test -vvv --match x"
    );
    assert_eq!(summarize_args("read", r#"{"path": "src/A.sol", "offset": 40}"#), "src/A.sol:40");
    assert_eq!(summarize_args("read", r#"{"path": "src/A.sol"}"#), "src/A.sol");
    assert_eq!(summarize_args("edit", r#"{"path": "a", "old": "x", "new": "y"}"#), "a");
    assert_eq!(summarize_args("write", r#"{"path": "b", "content": "..."}"#), "b");
    assert_eq!(summarize_args("finding", r#"{"severity": "high", "title": "Drain"}"#), "[high] Drain");
    assert_eq!(summarize_args("mystery", r#"{"a": 1}"#), r#"{"a": 1}"#);
    assert_eq!(summarize_args("bash", "not json"), "");
    assert_eq!(summarize_args("bash", r#"{"command": "echo \u001b]0;title\u0007hi"}"#), "echo hi");
    let long = summarize_args("bash", &json!({"command": "é".repeat(150)}).to_string());
    assert_eq!(long, format!("{}…", "é".repeat(100)));
    let exact = summarize_args("bash", &json!({"command": "x".repeat(100)}).to_string());
    assert_eq!(exact, "x".repeat(100));
}

#[test]
fn prompt_without_skills_or_brief() {
    let d = tempdir("bare-prompt");
    let (p, warnings) = e::prompt::build(&d, "2026-01-02", &[]);
    assert!(!p.contains("available_skills"), "no skills, no catalog");
    assert!(p.ends_with("<audit_brief>\n(no audit brief provided)\n</audit_brief>"));
    assert!(p.contains("Date: 2026-01-02"));
    assert!(!p.contains("{cwd}") && !p.contains("{date}") && !p.contains("{contents of"));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    // The brief is trimmed and the prompt is deterministic.
    std::fs::create_dir_all(d.join(".e")).unwrap();
    std::fs::write(d.join(".e/AUDIT.md"), "\n\n  Scope: all  \n\n").unwrap();
    let (a, _) = e::prompt::build(&d, "2026-01-02", &[]);
    let (b, _) = e::prompt::build(&d, "2026-01-02", &[]);
    assert_eq!(a, b);
    assert!(a.ends_with("<audit_brief>\nScope: all\n</audit_brief>"));
}

#[test]
fn token_counts_and_meter_display() {
    assert_eq!(fmt_tokens(0), "0 tok");
    assert_eq!(fmt_tokens(999), "999 tok");
    assert_eq!(fmt_tokens(1_000), "1.0k tok");
    assert_eq!(fmt_tokens(12_345), "12.3k tok");
    assert_eq!(fmt_tokens(1_000_000), "1.00M tok");
    assert_eq!(fmt_tokens(2_345_678), "2.35M tok");
    assert_eq!(Meter::default().display(), "$0.0000");
    let mut m = Meter::default();
    m.add(&Usage { prompt_tokens: 10, completion_tokens: 0, ..Usage::default() }, Some(0.5));
    m.add(&Usage { prompt_tokens: 10, completion_tokens: 0, ..Usage::default() }, Some(0.25));
    assert_eq!(m.display(), "$0.7500");
    assert_eq!(m.unpriced_tokens, 0, "priced tokens are not counted twice");
}

#[test]
fn usage_split_never_exceeds_the_prompt() {
    let u = |v: Value| serde_json::from_value::<Usage>(v).unwrap();
    // Cached tokens larger than the prompt (seen from proxies): clamped.
    let x = u(json!({"prompt_tokens": 100, "prompt_tokens_details": {"cached_tokens": 150}}));
    assert_eq!(x.split(), (100, 0, 0));
    let x = u(json!({"prompt_tokens": 100, "prompt_tokens_details":
        {"cached_tokens": 70, "cache_creation_input_tokens": 50}}));
    assert_eq!(x.split(), (70, 30, 0));
    // DeepSeek hit tokens alone still count as hits.
    let x = u(json!({"prompt_tokens": 100, "prompt_cache_hit_tokens": 30}));
    assert_eq!(x.split(), (30, 0, 70));
    // `cached_tokens: null` and missing fields.
    let x = u(json!({"prompt_tokens_details": {"cached_tokens": null}}));
    assert_eq!((x.split(), x.total()), ((0, 0, 0), 0));
    // Unknown usage fields are ignored.
    let x =
        u(json!({"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6, "something_new": {"a": 1}}));
    assert_eq!(x.total(), 6);
}

#[test]
fn transcripts_get_their_own_directory_and_typed_lines() {
    let d = tempdir("transcript");
    let mut a = Transcript::create(&d, false).unwrap();
    let b = Transcript::create(&d, false).unwrap();
    assert_ne!(a.dir, b.dir, "two sessions in the same second do not share a log");
    assert_eq!(a.dir.parent().unwrap(), d.join(".e/runs"));
    let name = a.dir.file_name().unwrap().to_str().unwrap().to_string();
    assert!(name.len() == 16 && name.ends_with('Z') && name.as_bytes()[8] == b'T', "{name}");
    a.log("one", json!({"k": "v", "n": 1}));
    a.log("two", json!("not an object"));
    let text = std::fs::read_to_string(a.dir.join("transcript.jsonl")).unwrap();
    let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        (lines[0]["type"].as_str(), lines[0]["k"].as_str(), lines[0]["n"].as_u64()),
        (Some("one"), Some("v"), Some(1))
    );
    assert!(lines[0]["ts"].as_u64().unwrap() > 1_700_000_000_000, "milliseconds");
    assert_eq!(lines[1].as_object().unwrap().len(), 2, "non-object fields are dropped");
}

#[test]
fn tool_schemas_are_well_formed() {
    let s = tools::schemas();
    let tools = s.as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["read", "bash", "edit", "write", "finding"]);
    for t in tools {
        assert_eq!(t["type"], "function");
        let params = &t["function"]["parameters"];
        assert_eq!(params["type"], "object");
        assert!(!t["function"]["description"].as_str().unwrap().is_empty());
        for r in params["required"].as_array().unwrap() {
            assert!(params["properties"].get(r.as_str().unwrap()).is_some(), "{r} is not a property");
        }
    }
    assert_eq!(
        tools[4]["function"]["parameters"]["properties"]["severity"]["enum"].as_array().unwrap().len(),
        5
    );
}

#[test]
fn read_edge_cases() {
    let d = tempdir("read-edge");
    std::fs::write(d.join("empty"), "").unwrap();
    assert_eq!(tools::read(&d, "empty", None, None).unwrap(), "(empty is empty)");
    assert!(tools::read(&d, ".", None, None).unwrap_err().contains("is a directory"));
    assert!(tools::read(&d, "missing", None, None).unwrap_err().starts_with("cannot read missing"));
    std::fs::write(d.join("crlf"), "a\r\nb\r\nc").unwrap();
    assert_eq!(tools::read(&d, "crlf", None, None).unwrap(), "     1\ta\n     2\tb\n     3\tc\n");
    // Reaching the end with a limit adds no continuation note.
    assert_eq!(tools::read(&d, "crlf", Some(2), Some(5)).unwrap(), "     2\tb\n     3\tc\n");
    // offset 0 and limit 0 are treated as 1.
    assert_eq!(
        tools::read(&d, "crlf", Some(0), Some(0)).unwrap(),
        "     1\ta\n\n[showing lines 1-1 of 3. Use offset=2 to continue.]"
    );
    // Absolute paths ignore the working directory.
    let abs = d.join("crlf");
    assert!(tools::read(Path::new("/"), abs.to_str().unwrap(), None, None).unwrap().contains("\ta"));
    // Invalid UTF-8 is shown lossily, not refused.
    std::fs::write(d.join("latin1"), b"caf\xe9\n").unwrap();
    assert!(tools::read(&d, "latin1", None, None).unwrap().contains("caf\u{fffd}"));
    // A long line of multibyte characters is cut on a char boundary.
    std::fs::write(d.join("wide"), "€".repeat(30_000)).unwrap();
    let out = tools::read(&d, "wide", None, None).unwrap();
    assert!(out.contains("longer than 50 KB") && out.len() <= tools::MAX_BYTES + 300);
    // A NUL past the first 8 KB is not treated as binary.
    let mut late = "x\n".repeat(5000).into_bytes();
    late.push(0);
    std::fs::write(d.join("late-nul"), late).unwrap();
    assert!(tools::read(&d, "late-nul", None, Some(1)).is_ok());
}

#[test]
fn edit_edge_cases() {
    let d = tempdir("edit-edge");
    assert!(tools::edit(&d, "nope", "a", "b").unwrap_err().starts_with("cannot read nope"));
    std::fs::write(d.join("u"), "ééé\n").unwrap();
    assert!(tools::edit(&d, "u", "éé", "x").unwrap_err().contains("2 times"), "overlaps on char boundaries");
    std::fs::write(d.join("f"), "one\ntwo\nthree\n").unwrap();
    assert_eq!(tools::edit(&d, "f", "two", "two two").unwrap(), "Edited f: replaced 1 match at line 2.");
    assert_eq!(std::fs::read_to_string(d.join("f")).unwrap(), "one\ntwo two\nthree\n");
    tools::edit(&d, "f", "one\n", "").unwrap();
    assert_eq!(std::fs::read_to_string(d.join("f")).unwrap(), "two two\nthree\n");
}

#[test]
fn write_reports_size() {
    let d = tempdir("write-edge");
    assert_eq!(tools::write(&d, "a", "x\ny\n").unwrap(), "Wrote a: 4 bytes, 2 lines.");
    assert_eq!(tools::write(&d, "b", "").unwrap(), "Wrote b: 0 bytes, 0 lines.");
    std::fs::write(d.join("file"), "").unwrap();
    assert!(tools::write(&d, "file/child", "x").unwrap_err().contains("cannot create"));
}

async fn dispatch_with(d: &Path, cfg: &Config, name: &str, args: &str) -> tools::Output {
    let cancel = Cancel::default();
    let run_dir = d.join(".e/runs/test");
    let ctx = Ctx { cwd: d, run_dir: &run_dir, cfg, cancel: &cancel, seq: 7 };
    tools::dispatch(name, args, &ctx).await
}

async fn dispatch(d: &Path, name: &str, args: &str) -> tools::Output {
    dispatch_with(d, &Config::default(), name, args).await
}

#[tokio::test]
async fn dispatch_validates_arguments() {
    let d = tempdir("dispatch");
    let out = dispatch(&d, "read", "{\"path\": ").await;
    assert!(out.is_error && out.content.starts_with("arguments are not valid JSON"));
    let out = dispatch(&d, "bash", "").await;
    assert!(out.is_error && out.content.contains("missing field `command`"), "{}", out.content);
    let out = dispatch(&d, "nope", "").await;
    assert_eq!((out.is_error, out.content.as_str()), (true, "unknown tool: nope"));
    assert_eq!(out.summary, "error: unknown tool: nope");
    for t in ["0", "-1", "1e20"] {
        let out = dispatch(&d, "bash", &format!("{{\"command\": \"true\", \"timeout_s\": {t}}}")).await;
        assert!(out.is_error && out.content.contains("timeout_s must be a positive number"), "{t}");
    }
    // Large but representable: effectively no timeout, and no overflow when added to now.
    let out = dispatch(&d, "bash", r#"{"command": "true", "timeout_s": 1e15}"#).await;
    assert!(!out.is_error, "{}", out.content);
    let out = dispatch(&d, "write", r#"{"path": "w", "content": "hi"}"#).await;
    assert_eq!((out.is_error, out.summary.as_str()), (false, "ok"));
    let out = dispatch(&d, "read", r#"{"path": "w", "offset": 1, "limit": 1}"#).await;
    assert_eq!(out.content, "     1\thi\n");
    let out = dispatch(&d, "edit", r#"{"path": "w", "old": "zz", "new": "y"}"#).await;
    assert!(out.is_error && out.summary.starts_with("error: old text not found"));
}

#[tokio::test]
async fn bash_tool_reports_failure_and_logs_by_sequence() {
    let d = tempdir("bash-tool");
    let out = dispatch(&d, "bash", r#"{"command": "echo out; echo err >&2; exit 3"}"#).await;
    assert!(out.is_error);
    assert!(out.summary.starts_with("exit 3 · "), "{}", out.summary);
    assert!(out.content.starts_with("out\nerr\n[exit code 3, "), "{}", out.content);
    let log = d.join(".e/runs/test/bash/0007.log");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "out\nerr\n");
    // The configured default timeout applies when the model gives none.
    let cfg = Config { bash_timeout_s: 1, ..Config::default() };
    let out = dispatch_with(&d, &cfg, "bash", r#"{"command": "sleep 30"}"#).await;
    assert!(out.is_error && out.summary == "timeout after 1s", "{}", out.summary);
    // A fractional per-call timeout overrides it.
    let out = dispatch_with(&d, &cfg, "bash", r#"{"command": "sleep 30", "timeout_s": 0.2}"#).await;
    assert_eq!(out.summary, "timeout after 0.2s");
}

#[tokio::test]
async fn exec_runs_in_the_working_directory_with_no_stdin() {
    let d = tempdir("exec");
    let log = d.join("log");
    let x = exec::run("pwd; cat; echo after-cat", &d, Duration::from_secs(10), &log, &Cancel::default())
        .await
        .unwrap();
    assert_eq!(x.exit_code, Some(0));
    let canon = std::fs::canonicalize(&d).unwrap();
    assert_eq!(x.tail, format!("{}\nafter-cat\n", canon.display()), "cat sees EOF at once");
    // Killed by a signal: 128 + signal number, like a shell.
    let x = exec::run("kill -9 $$", &d, Duration::from_secs(10), &log, &Cancel::default()).await.unwrap();
    assert_eq!(x.exit_code, Some(137));
    assert!(!x.success());
    assert_eq!(x.status_line().split(" · ").next(), Some("exit 137"));
    // No output at all.
    let x = exec::run("true", &d, Duration::from_secs(10), &log, &Cancel::default()).await.unwrap();
    assert!(x.tail.is_empty() && x.model_text().starts_with("[exit code 0, "));
    // Invalid UTF-8 in output does not fail the tool.
    let x = exec::run("printf 'a\\xffb\\n'", &d, Duration::from_secs(10), &log, &Cancel::default())
        .await
        .unwrap();
    assert_eq!(x.tail, "a\u{fffd}b\n");
    // A missing working directory is an error, not a panic.
    assert!(
        exec::run("true", &d.join("gone"), Duration::from_secs(1), &log, &Cancel::default()).await.is_err()
    );
}

#[tokio::test]
async fn interrupted_exec_says_so() {
    let d = tempdir("exec-int");
    let c = Cancel::default();
    c.cancel();
    let x = exec::run("echo hi; sleep 30", &d, Duration::from_secs(60), &d.join("log"), &c).await.unwrap();
    assert!(x.interrupted && x.exit_code.is_none() && !x.success());
    assert!(x.status_line().starts_with("interrupted · "));
    assert!(x.model_text().contains("[interrupted by the user after "));
}

async fn finding(d: &Path, cfg: &Config, args: Value) -> (tools::Output, Value) {
    let out = dispatch_with(d, cfg, "finding", &args.to_string()).await;
    let id = out.summary.split(' ').next().unwrap().to_string();
    let rec = std::fs::read_to_string(d.join(format!(".e/findings/{id}.json")))
        .map(|s| serde_json::from_str(&s).unwrap())
        .unwrap_or(Value::Null);
    (out, rec)
}

fn finding_args() -> Value {
    json!({"title": "t", "severity": "High", "class": "c", "file": "f", "function": "g",
           "lines": 42, "description": "d", "attack_path": "a", "impact": "i", "fix": "add a guard"})
}

#[tokio::test]
async fn finding_edge_cases() {
    let d = tempdir("finding-edge");
    let cfg = Config::default();
    let (out, rec) = finding(&d, &cfg, finding_args()).await;
    assert_eq!(out.summary, "F-001 suspected");
    assert_eq!((rec["severity"].as_str(), rec["lines"].as_u64()), (Some("high"), Some(42)));
    assert_eq!(rec["fix"], "add a guard");
    assert!(out.content.contains("No poc_cmd given"));
    assert_eq!(rec["run_dir"], json!(d.join(".e/runs/test")));
    // No temporary files are left behind.
    let names: Vec<String> = std::fs::read_dir(d.join(".e/findings"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["F-001.json"]);

    // Whitespace around an id is trimmed; generated ids skip taken ones.
    let mut a = finding_args();
    a["id"] = json!("  F-002 ");
    assert_eq!(finding(&d, &cfg, a).await.0.summary, "F-002 suspected");
    assert_eq!(finding(&d, &cfg, finding_args()).await.0.summary, "F-003 suspected");
    let mut a = finding_args();
    a["id"] = json!("   ");
    assert_eq!(finding(&d, &cfg, a).await.0.summary, "F-004 suspected", "a blank id is generated");

    // A blank poc_cmd is no PoC.
    let mut a = finding_args();
    a["poc_cmd"] = json!("  ");
    let (out, rec) = finding(&d, &cfg, a).await;
    assert!(out.content.contains("No poc_cmd given") && rec["poc_cmd"].is_null());

    // A PoC that times out does not prove anything.
    let cfg = Config { poc_timeout_s: 1, ..Config::default() };
    let mut a = finding_args();
    a["poc_cmd"] = json!("sleep 30; exit 0");
    let (out, rec) = finding(&d, &cfg, a).await;
    assert!(out.summary.ends_with("suspected"));
    assert_eq!(rec["poc"]["timed_out"], true);
    assert!(rec["poc"]["exit_code"].is_null());
    let log = rec["poc"]["log"].as_str().unwrap();
    assert!(log.ends_with(&format!("poc/{}-0007.log", rec["id"].as_str().unwrap())), "{log}");

    for bad in [".hidden", "a/b", "a b", &"x".repeat(65)] {
        let mut a = finding_args();
        a["id"] = json!(bad);
        let out = dispatch(&d, "finding", &a.to_string()).await;
        assert!(out.is_error && out.content.starts_with("invalid id"), "{bad}");
    }
    let mut a = finding_args();
    a["id"] = json!("x".repeat(64));
    assert!(!dispatch(&d, "finding", &a.to_string()).await.is_error, "64 chars is allowed");
    let mut a = finding_args();
    a.as_object_mut().unwrap().remove("impact");
    let out = dispatch(&d, "finding", &a.to_string()).await;
    assert!(out.is_error && out.content.contains("missing field `impact`"));
}
