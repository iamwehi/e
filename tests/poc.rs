//! PoC integrity: the git tree check and snapshot in `finding`, `e verify` on a clean copy,
//! `e triage` sessions, and the cost cap.

mod common;

use common::*;
use e::agent::{Mode, RunEnd, Session};
use e::config::{Config, Meter, Prices, Rate};
use e::exec::Cancel;
use e::tools::{self, Ctx};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

fn sh(dir: &Path, cmd: &str) {
    let out = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A git repository with a committed `src/V.sol` that contains `marker`.
fn repo(tag: &str, marker: &str) -> PathBuf {
    let d = tempdir(tag);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/V.sol"), format!("contract V {{ // {marker}\n}}\n")).unwrap();
    std::fs::write(d.join(".gitignore"), "deps/\n").unwrap();
    sh(&d, "git init -q && git add -A && git commit -qm init");
    d
}

async fn finding_with(d: &Path, cfg: &Config, args: Value) -> (tools::Output, Value) {
    let cancel = Cancel::default();
    let run_dir = d.join(".e/runs/test");
    let ctx = Ctx { cwd: d, run_dir: &run_dir, cfg, cancel: &cancel, seq: 1 };
    let out = tools::dispatch("finding", &args.to_string(), &ctx).await;
    assert!(!out.is_error, "{}", out.content);
    let id = out.summary.split(' ').next().unwrap().to_string();
    let rec =
        serde_json::from_str(&std::fs::read_to_string(d.join(format!(".e/findings/{id}.json"))).unwrap())
            .unwrap();
    (out, rec)
}

fn args(id: &str, poc: &str) -> Value {
    json!({"id": id, "title": "t", "severity": "high", "class": "c", "file": "src/V.sol", "function": "f",
           "lines": "1", "description": "d", "attack_path": "a", "impact": "i", "poc_cmd": poc})
}

#[tokio::test]
async fn a_poc_against_modified_tracked_files_is_not_proven() {
    let d = repo("tree", "safe");
    let cfg = Config::default();
    std::fs::create_dir_all(d.join("test")).unwrap();
    std::fs::write(d.join("test/poc.sh"), "grep -q vulnerable src/V.sol\n").unwrap();
    std::fs::create_dir_all(d.join("deps")).unwrap();
    std::fs::write(d.join("deps/big.bin"), "ignored").unwrap();
    // The model "proves" the bug by planting it.
    std::fs::write(d.join("src/V.sol"), "contract V { // vulnerable\n}\n").unwrap();
    let (out, rec) = finding_with(&d, &cfg, args("A", "bash test/poc.sh")).await;
    assert_eq!(out.summary, "A suspected");
    assert_eq!(rec["poc"]["exit_code"], 0, "the PoC itself passed");
    assert_eq!(rec["tree"], json!({"git": true, "modified": ["src/V.sol"], "disallowed": ["src/V.sol"]}));
    assert!(out.content.contains("these tracked files differ from git HEAD: src/V.sol"), "{}", out.content);
    // New files are kept with the finding; ignored and harness files are not.
    assert_eq!(rec["poc_files"], json!([{"path": "test/poc.sh", "bytes": 29}]));
    assert_eq!(
        std::fs::read_to_string(d.join(".e/findings/A.poc/test/poc.sh")).unwrap(),
        "grep -q vulnerable src/V.sol\n"
    );
    assert!(!d.join(".e/findings/A.poc/deps").exists() && !d.join(".e/findings/A.poc/.e").exists());

    // allow_modified covers a path the PoC may change.
    let cfg = Config { allow_modified: vec!["src/".into()], ..Config::default() };
    let (out, rec) = finding_with(&d, &cfg, args("B", "bash test/poc.sh")).await;
    assert_eq!(out.summary, "B proven", "{}", out.content);
    assert_eq!(rec["tree"]["disallowed"], json!([]));

    // Restored: proven.
    sh(&d, "git checkout -q HEAD -- src/V.sol && sed -i s/safe/vulnerable/ test/poc.sh");
    std::fs::write(d.join("test/poc.sh"), "grep -q safe src/V.sol\n").unwrap();
    let (out, rec) = finding_with(&d, &Config::default(), args("C", "bash test/poc.sh")).await;
    assert_eq!(out.summary, "C proven", "{}", out.content);
    assert_eq!(rec["tree"]["modified"], json!([]));

    // A file the PoC run itself changes counts too.
    let (out, _) =
        finding_with(&d, &Config::default(), args("D", "echo x >> src/V.sol && bash test/poc.sh")).await;
    assert_eq!(out.summary, "D suspected");
    sh(&d, "git checkout -q HEAD -- src/V.sol");

    // Re-recording without a PoC drops the old snapshot.
    let mut a = args("A", "");
    a["poc_cmd"] = Value::Null;
    finding_with(&d, &Config::default(), a).await;
    assert!(!d.join(".e/findings/A.poc").exists());
}

#[tokio::test]
async fn outside_git_the_tree_is_not_checked() {
    let d = tempdir("nogit");
    let (out, rec) = finding_with(&d, &Config::default(), args("A", "true")).await;
    assert_eq!(out.summary, "A proven");
    assert_eq!(rec["tree"], json!({"git": false}));
    assert!(out.content.contains("Not a git repository"), "{}", out.content);
    assert_eq!(rec["poc_files"], json!([]));
}

#[test]
fn allow_modified_matching() {
    let m: Vec<String> = ["src/A.sol", "test/T.sol", "foundry.toml", "testdata/x"].map(String::from).to_vec();
    let allow = |a: &[&str]| e::tree::disallowed(&m, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(allow(&[]), m);
    assert_eq!(allow(&["test/", "./foundry.toml"]), ["src/A.sol", "testdata/x"]);
    assert_eq!(
        allow(&["test"]),
        ["src/A.sol", "foundry.toml", "testdata/x"],
        "a name covers a directory, not a prefix"
    );
}

async fn verify(d: &Path, ids: &[&str]) -> (Result<bool, String>, Vec<String>) {
    let mut lines = Vec::new();
    let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    let r = e::verify::verify(&Config::default(), d, &ids, &Cancel::default(), &mut |l| lines.push(l.into()))
        .await;
    (r, lines)
}

fn status(d: &Path, id: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(d.join(format!(".e/findings/{id}.json"))).unwrap()).unwrap()
}

#[tokio::test]
async fn verify_reruns_pocs_on_a_clean_copy() {
    let d = repo("verify", "vulnerable");
    std::fs::create_dir_all(d.join("test")).unwrap();
    std::fs::create_dir_all(d.join("deps")).unwrap();
    std::fs::write(d.join("deps/lib.sh"), "true\n").unwrap();
    std::fs::write(d.join("test/poc.sh"), "bash deps/lib.sh && grep -q vulnerable src/V.sol && touch ran\n")
        .unwrap();
    let abs = d.canonicalize().unwrap();
    let (out, _) = finding_with(&d, &Config::default(), args("A", "bash test/poc.sh")).await;
    assert_eq!(out.summary, "A proven");
    let (out, _) =
        finding_with(&d, &Config::default(), args("ABS", &format!("bash {}/test/poc.sh", abs.display())))
            .await;
    assert_eq!(out.summary, "ABS proven");
    std::fs::remove_file(d.join("ran")).unwrap();

    // Later the working tree changes: the PoC file is gone, the source differs, and a stray
    // file appears. None of that reaches the clean copy.
    std::fs::remove_file(d.join("test/poc.sh")).unwrap();
    std::fs::write(d.join("src/V.sol"), "contract V {}\n").unwrap();
    std::fs::write(d.join("stray.txt"), "x").unwrap();
    let (r, lines) = verify(&d, &[]).await;
    assert_eq!(r, Ok(true), "{lines:?}");
    assert!(lines[0].starts_with("A: proven -> proven (exit 0 · "), "{lines:?}");
    assert!(
        lines[1].starts_with("ABS: proven -> proven") && lines[1].contains("ran with the copy's path"),
        "{lines:?}"
    );
    assert!(!d.join("ran").exists(), "the PoC ran in the copy, not in the original");
    assert_eq!(
        std::fs::read_to_string(d.join("src/V.sol")).unwrap(),
        "contract V {}\n",
        "original untouched"
    );
    let rec = status(&d, "A");
    assert_eq!(rec["verification"]["clean_copy"], true);
    assert_eq!(rec["verification"]["exit_code"], 0);
    let vdir = std::fs::read_dir(d.join(".e/verify")).unwrap().next().unwrap().unwrap().path();
    assert!(vdir.join("A.log").exists() && !vdir.join("A.tree").exists(), "logs kept, copies removed");

    // A finding whose PoC only passed because of a planted change is demoted.
    std::fs::write(d.join("src/V.sol"), "contract V { // planted\n}\n").unwrap();
    std::fs::write(d.join("test/p2.sh"), "grep -q planted src/V.sol\n").unwrap();
    let cfg = Config { allow_modified: vec!["src/".into()], ..Config::default() };
    let (out, _) = finding_with(&d, &cfg, args("P", "bash test/p2.sh")).await;
    assert_eq!(out.summary, "P proven");
    let (r, lines) = verify(&d, &["P"]).await;
    assert_eq!(r, Ok(false));
    assert!(lines[0].starts_with("P: proven -> suspected (exit 1"), "{lines:?}");
    assert_eq!(status(&d, "P")["status"], "suspected");

    let (r, _) = verify(&d, &["NOPE"]).await;
    assert_eq!(r, Err("no finding NOPE in .e/findings".into()));
}

fn priced(url: &str, per_m: f64) -> Config {
    let mut cfg = config(url);
    let rate = Rate { hit: per_m, miss: per_m, output: per_m, write: None };
    cfg.prices.insert("deepseek-v4-1-flash".into(), Prices { peak: rate, off_peak: None });
    cfg
}

#[tokio::test]
async fn the_cost_limit_stops_the_run_and_warns_first() {
    // $1000 per 1M tokens: usage(1000, 50) costs $1.05.
    let m = mock(vec![tool_reply(&[("c1", "bash", json!({"command": "touch ran"}))], usage(1000, 50))]).await;
    let d = tempdir("cost");
    let mut cfg = priced(&m.url, 1000.0);
    cfg.max_cost_usd = Some(1.0);
    let mut s = Session::new(cfg, "deepseek-v4-1-flash".into(), d.clone(), false).unwrap();
    let end = s.run("go", &Cancel::default(), &mut |_| {}).await;
    assert_eq!(end, RunEnd::CostLimit("cost limit reached: $1.0500 of $1.00".into()));
    assert_eq!(end.exit_code(), 3);
    assert!(!d.join("ran").exists(), "tool calls after the limit are not run");
    e::agent::history_is_valid(&s.history).unwrap();
    // The next run in the same session stops before any request.
    let end = s.run("again", &Cancel::default(), &mut |_| {}).await;
    assert!(matches!(end, RunEnd::CostLimit(_)));
    assert_eq!(m.requests.lock().unwrap().len(), 1);

    let m = mock(vec![
        tool_reply(&[("c1", "bash", json!({"command": "true"}))], usage(1000, 50)),
        text_reply("noted", usage(10, 0)),
    ])
    .await;
    let mut cfg = priced(&m.url, 1000.0);
    cfg.max_cost_usd = Some(1.2);
    let mut s = Session::new(cfg, "deepseek-v4-1-flash".into(), tempdir("cost2"), false).unwrap();
    assert_eq!(s.run("go", &Cancel::default(), &mut |_| {}).await, RunEnd::Done("noted".into()));
    assert!(s.budget_warned);
    let reqs = m.requests.lock().unwrap().clone();
    let last =
        reqs[1]["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(
        last.starts_with("[harness] $1.0500 of the $1.20 cost limit spent. Update .e/NOTES.md"),
        "{last}"
    );

    // Without prices the cost is unknown, so a limit can't be kept.
    let m = mock(vec![]).await;
    let mut cfg = config(&m.url);
    cfg.max_cost_usd = Some(1.0);
    let mut s = Session::new(cfg, "no-prices".into(), tempdir("cost3"), false).unwrap();
    let end = s.run("go", &Cancel::default(), &mut |_| {}).await;
    assert!(matches!(&end, RunEnd::Error(m) if m.contains("no-prices has no prices")), "{end:?}");
    assert!(m.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn triage_records_a_verdict_from_a_fresh_reviewer() {
    let d = repo("triage", "x");
    std::fs::create_dir_all(d.join("test")).unwrap();
    std::fs::write(d.join("test/poc.sh"), "true\n").unwrap();
    finding_with(&d, &Config::default(), args("A", "bash test/poc.sh")).await;
    let mut b = args("B", "");
    b["poc_cmd"] = Value::Null;
    finding_with(&d, &Config::default(), b).await;
    let m = mock(vec![
        // The reviewer has no finding tool.
        tool_reply(&[("c1", "finding", json!({"title": "x"}))], usage(10, 5)),
        tool_reply(
            &[(
                "c2",
                "verdict",
                json!({"verdict": "Refuted", "reasoning": "onlyOwner guards it.\nSee line 3.",
                                       "severity": "info"}),
            )],
            usage(20, 5),
        ),
        text_reply("Refuted.", usage(30, 5)),
        // B: no verdict.
        text_reply("I could not decide.", usage(10, 5)),
    ])
    .await;
    let mut lines = Vec::new();
    let mut meter = Meter::default();
    let code = e::verify::triage(
        &config(&m.url),
        "deepseek-v4-1-flash",
        &d,
        &[],
        &mut meter,
        &Cancel::default(),
        &mut |l| lines.push(l.to_string()),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(code, 0);
    assert!(lines[1].starts_with("A: refuted — onlyOwner guards it."), "{lines:?}");
    assert!(lines[3].starts_with("B: no verdict (done)"), "{lines:?}");
    let rec = status(&d, "A");
    assert_eq!(rec["triage"]["verdict"], "refuted");
    assert_eq!(rec["triage"]["severity"], "info");
    assert_eq!(rec["triage"]["reasoning"], "onlyOwner guards it.\nSee line 3.");
    assert_eq!(rec["status"], "proven", "triage does not change the status");
    assert!(status(&d, "B")["triage"].is_null());
    assert!(meter.cost > 0.0, "the cost carries across reviews");

    let reqs = m.requests.lock().unwrap().clone();
    let names: Vec<&str> = reqs[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["read", "bash", "edit", "write", "verdict"]);
    assert!(reqs[0]["messages"][0]["content"].as_str().unwrap().starts_with("You are a skeptical reviewer"));
    let task = reqs[0]["messages"][1]["content"].as_str().unwrap();
    assert!(task.starts_with("Review finding A.") && task.contains("<finding>\n{"), "{task}");
    assert!(
        task.contains("\"poc_cmd\": \"bash test/poc.sh\"")
            && task.contains("A.poc, at the same relative paths: test/poc.sh")
    );
    assert!(!task.contains("run_dir"), "{task}");
    let refused = reqs[1]["messages"][3]["content"].as_str().unwrap();
    assert_eq!(refused, "unknown tool: finding");
    assert!(!d.join(".e/findings/F-001.json").exists());
    let s = Session::with_mode(config(&m.url), "m".into(), d.clone(), false, Mode::Triage).unwrap();
    assert_eq!(s.mode, Mode::Triage);
}
