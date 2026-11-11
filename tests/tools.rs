mod common;

use common::*;
use e::config::{Config, Meter, Usage};
use e::exec::{self, Cancel};
use e::tools::{self, Ctx};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn edit_requires_exactly_one_match() {
    let d = tempdir("edit");
    std::fs::write(d.join("f.sol"), "a = 1;\nb = 1;\nc = 2;\n").unwrap();
    let e = tools::edit(&d, "f.sol", "= 1;", "= 9;").unwrap_err();
    assert!(e.contains("matches 2 times"), "{e}");
    let e = tools::edit(&d, "f.sol", "= 3;", "x").unwrap_err();
    assert!(e.contains("0 matches"), "{e}");
    // Overlapping occurrences count as ambiguous.
    std::fs::write(d.join("g"), "aaa").unwrap();
    assert!(tools::edit(&d, "g", "aa", "b").unwrap_err().contains("2 times"));
    assert!(tools::edit(&d, "f.sol", "", "x").is_err());
    let ok = tools::edit(&d, "f.sol", "c = 2;", "c = 3;").unwrap();
    assert!(ok.contains("line 3"), "{ok}");
    assert_eq!(std::fs::read_to_string(d.join("f.sol")).unwrap(), "a = 1;\nb = 1;\nc = 3;\n");
}

#[test]
fn write_creates_parent_dirs_and_overwrites() {
    let d = tempdir("write");
    tools::write(&d, "x/y/z.t", "one").unwrap();
    tools::write(&d, "x/y/z.t", "two").unwrap();
    assert_eq!(std::fs::read_to_string(d.join("x/y/z.t")).unwrap(), "two");
}

#[test]
fn read_numbers_lines_and_caps_output() {
    let d = tempdir("read");
    let text: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.join("big"), text).unwrap();
    let out = tools::read(&d, "big", None, None).unwrap();
    assert!(out.starts_with("     1\tline 1\n"));
    assert_eq!(out.lines().filter(|l| l.contains("\tline ")).count(), 2000);
    assert!(out.contains("[showing lines 1-2000 of 5000. Use offset=2001 to continue.]"), "{out}");
    let out = tools::read(&d, "big", Some(4990), Some(5)).unwrap();
    assert!(out.contains("  4994\tline 4994") && !out.contains("line 4995\n"));
    assert!(out.contains("Use offset=4995"));
    assert!(tools::read(&d, "big", Some(6000), None).unwrap_err().contains("beyond"));

    // 50 KB cap with long lines.
    let wide: String = (0..200).map(|_| format!("{}\n", "x".repeat(1000))).collect();
    std::fs::write(d.join("wide"), wide).unwrap();
    let out = tools::read(&d, "wide", None, None).unwrap();
    assert!(out.len() <= 50 * 1024 + 200);
    assert!(out.contains("of 200. Use offset="));

    // A single line over the cap.
    std::fs::write(d.join("one"), "y".repeat(80_000)).unwrap();
    assert!(tools::read(&d, "one", None, None).unwrap().contains("longer than 50 KB"));
    std::fs::write(d.join("bin"), [0u8, 1, 2]).unwrap();
    assert!(tools::read(&d, "bin", None, None).unwrap_err().contains("binary"));
}

#[tokio::test]
async fn bash_output_is_truncated_to_the_tail_and_saved_in_full() {
    let d = tempdir("trunc");
    let log = d.join("out.log");
    let x = exec::run("seq 1 100000; echo LAST >&2", &d, Duration::from_secs(30), &log, &Cancel::default())
        .await
        .unwrap();
    assert_eq!(x.exit_code, Some(0));
    assert!(x.truncated());
    assert!(x.tail.len() as u64 <= exec::TAIL_BYTES);
    assert!(x.tail.ends_with("100000\nLAST\n"), "stderr is merged after stdout");
    assert!(x.tail.starts_with(|c: char| c.is_ascii_digit()), "tail starts on a line boundary");
    assert_eq!(std::fs::metadata(&log).unwrap().len(), x.total_bytes);
    let text = x.model_text();
    assert!(text.contains("output truncated") && text.contains(log.to_str().unwrap()));

    let x = exec::run(
        "printf 'a\\x1b[31mred\\x1b]0;title\\x07\\r\\n'; exit 3",
        &d,
        Duration::from_secs(5),
        &log,
        &Cancel::default(),
    )
    .await
    .unwrap();
    assert_eq!(x.exit_code, Some(3));
    assert_eq!(x.tail, "ared\n");
    assert!(!x.truncated() && !x.model_text().contains("truncated"));
}

fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Zombies are dead; they only wait for a parent to reap them.
        Ok(stat) => !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
        Err(_) => false,
    }
}

async fn wait_pids(file: &Path) -> Vec<i32> {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(file)
            && s.lines().count() >= 2
        {
            return s.lines().map(|l| l.trim().parse().unwrap()).collect();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("pids not written");
}

#[tokio::test]
async fn timeout_kills_the_whole_process_tree() {
    let d = tempdir("timeout");
    let pids = d.join("pids");
    // A background grandchild and a nested shell, both ignoring SIGTERM.
    let cmd = format!(
        "trap '' TERM; sleep 300 & echo $! >> {p}; bash -c 'trap \"\" TERM; echo $$ >> {p}; sleep 300'",
        p = pids.display()
    );
    let t = Instant::now();
    let x =
        exec::run(&cmd, &d, Duration::from_millis(800), &d.join("log"), &Cancel::default()).await.unwrap();
    assert!(x.timed_out && x.exit_code.is_none());
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(x.model_text().contains("timed out"));
    let pids = wait_pids(&pids).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for pid in pids {
        assert!(!alive(pid), "pid {pid} survived the timeout");
    }
}

#[tokio::test]
async fn background_processes_do_not_outlive_the_command() {
    let d = tempdir("bg");
    let pids = d.join("pids");
    let cmd =
        format!("sleep 300 & echo $! >> {p}; sleep 300 & echo $! >> {p}; echo started", p = pids.display());
    let x = exec::run(&cmd, &d, Duration::from_secs(10), &d.join("log"), &Cancel::default()).await.unwrap();
    assert_eq!(x.exit_code, Some(0));
    tokio::time::sleep(Duration::from_millis(200)).await;
    for pid in wait_pids(&pids).await {
        assert!(!alive(pid), "pid {pid} survived");
    }
}

#[tokio::test]
async fn cancel_kills_a_running_command() {
    let d = tempdir("cancel");
    let c = Cancel::default();
    let c2 = c.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let x = exec::run("sleep 300", &d, Duration::from_secs(60), &d.join("log"), &c).await.unwrap();
    assert!(x.interrupted && t.elapsed() < Duration::from_secs(5));
}

async fn finding(d: &Path, args: Value) -> (tools::Output, Value) {
    let cfg = Config::default();
    let cancel = Cancel::default();
    let run_dir = d.join(".e/runs/test");
    let ctx = Ctx { cwd: d, run_dir: &run_dir, cfg: &cfg, cancel: &cancel, seq: 1 };
    let out = tools::dispatch("finding", &args.to_string(), &ctx).await;
    let id = out.summary.split(' ').next().unwrap().to_string();
    let rec = std::fs::read_to_string(d.join(format!(".e/findings/{id}.json")))
        .map(|s| serde_json::from_str(&s).unwrap())
        .unwrap_or(Value::Null);
    (out, rec)
}

fn finding_args() -> Value {
    json!({"title": "t", "severity": "high", "class": "reentrancy", "file": "src/V.sol",
           "function": "withdraw", "lines": "10-20", "description": "d", "attack_path": "a", "impact": "i"})
}

#[tokio::test]
async fn finding_status_comes_from_the_poc_exit_code() {
    let d = tempdir("finding");
    let mut a = finding_args();
    a["poc_cmd"] = json!("echo drained; exit 0");
    let (out, rec) = finding(&d, a.clone()).await;
    assert_eq!(out.summary, "F-001 proven");
    assert_eq!(rec["status"], "proven");
    assert_eq!(rec["poc"]["exit_code"], 0);
    assert!(rec["poc"]["output_tail"].as_str().unwrap().contains("drained"));
    assert!(out.content.contains("drained"));

    // The model cannot set the status.
    a["poc_cmd"] = json!("exit 1");
    a["status"] = json!("proven");
    let (out, rec) = finding(&d, a.clone()).await;
    assert_eq!(out.summary, "F-002 suspected");
    assert_eq!(rec["status"], "suspected");

    // No PoC: suspected. Same id overwrites.
    let mut b = finding_args();
    b["id"] = json!("F-001");
    b["title"] = json!("replaced");
    let (out, rec) = finding(&d, b).await;
    assert_eq!(out.summary, "F-001 suspected");
    assert_eq!(rec["title"], "replaced");
    assert!(rec["poc"].is_null());
    assert_eq!(std::fs::read_dir(d.join(".e/findings")).unwrap().count(), 2);

    let mut bad = finding_args();
    bad["id"] = json!("../escape");
    let (out, _) = finding(&d, bad).await;
    assert!(out.is_error);
    let mut bad = finding_args();
    bad["severity"] = json!("catastrophic");
    assert!(finding(&d, bad).await.0.is_error);
}

fn venice_usage(prompt: u64, cached: u64, completion: u64) -> Usage {
    serde_json::from_value(json!({"prompt_tokens": prompt, "completion_tokens": completion,
        "prompt_tokens_details": {"cached_tokens": cached}}))
    .unwrap()
}

#[test]
fn cost_uses_cache_hit_miss_and_output_prices() {
    let cfg = Config::default();
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    // Venice: 1M cached, 2M uncached, 0.5M output; prices have no peak hours.
    let u = venice_usage(3_000_000, 1_000_000, 500_000);
    assert_eq!(u.hit_miss(), (1_000_000, 2_000_000));
    assert!(close(cfg.cost("deepseek-v4-1-flash", &u, 0).unwrap(), 0.0075 + 0.6 + 0.6));
    assert!(close(cfg.cost("deepseek-v4-pro", &u, 0).unwrap(), 0.33 + 3.3 + 1.6505));
    assert_eq!(cfg.cost("other-model", &u, 0), None);
    // No cache details at all: every prompt token is a miss.
    let plain: Usage =
        serde_json::from_value(json!({"prompt_tokens": 1000, "completion_tokens": 10})).unwrap();
    assert_eq!(plain.hit_miss(), (0, 1000));

    // DeepSeek's own API: hit/miss fields and peak/off-peak prices by config.
    let ds: Config = serde_json::from_value(json!({
        "peak_hours_utc": [[1, 4], [6, 10]],
        "prices": {"deepseek-flash": {"hit": 0.006, "miss": 0.3, "output": 1.2,
                   "off_peak": {"hit": 0.003, "miss": 0.15, "output": 0.6}}}
    }))
    .unwrap();
    let u: Usage = serde_json::from_value(json!({"prompt_tokens": 3_000_000, "completion_tokens": 500_000,
        "prompt_cache_hit_tokens": 1_000_000, "prompt_cache_miss_tokens": 2_000_000,
        "prompt_tokens_details": {"cached_tokens": 1_000_000}}))
    .unwrap();
    // Wednesday 2026-11-11 08:00 UTC is peak; 12:00 is off-peak; Saturday is off-peak.
    let peak = 1_794_355_200 + 8 * 3600;
    let off = 1_794_355_200 + 12 * 3600;
    let saturday = 1_794_355_200 + 3 * 86_400 + 8 * 3600;
    assert!(close(ds.cost("deepseek-flash", &u, peak).unwrap(), 0.006 + 0.6 + 0.6));
    assert!(close(ds.cost("deepseek-flash", &u, off).unwrap(), 0.003 + 0.3 + 0.3));
    assert!(close(ds.cost("deepseek-flash", &u, saturday).unwrap(), 0.003 + 0.3 + 0.3));

    let mut m = Meter::default();
    m.add(&u, ds.cost("deepseek-flash", &u, peak));
    assert_eq!(m.display(), "$1.2060");
    let small = Usage { prompt_tokens: 1000, completion_tokens: 234, ..Usage::default() };
    m.add(&small, None);
    assert_eq!(m.display(), "$1.2060+1.2k tok");
    let mut m = Meter::default();
    m.add(&small, None);
    assert_eq!(m.display(), "1.2k tok", "no invented cost without prices");
}

#[test]
fn sanitizer_strips_escapes_and_controls() {
    use e::term::{Sanitizer, sanitize};
    assert_eq!(sanitize("a\x1b[2J\x1b[1;1Hb\x1b]0;pwned\x07c\x1bPq#0\x1b\\d"), "abcd");
    assert_eq!(sanitize("x\ry\x08z\u{9b}31m\u{202e}w\u{7f}\n\t"), "xyzw\n\t");
    // A sequence split across chunks.
    let mut s = Sanitizer::default();
    let out = s.push("hi \x1b[3") + &s.push("1mred\x1b") + &s.push("[0m!");
    assert_eq!(out, "hi red!");
    // An unterminated OSC does not swallow everything after it.
    let long = format!("\x1b]{}", "a".repeat(5000)) + "visible";
    assert!(sanitize(&long).ends_with("visible"));
}

#[test]
fn skills_frontmatter_and_prompt() {
    let d = tempdir("skills");
    let sk = d.join(".e/skills/reentrancy");
    std::fs::create_dir_all(&sk).unwrap();
    std::fs::write(
        sk.join("SKILL.md"),
        "---\nname: reentrancy\ndescription: \"Checks for reentrancy\"\n---\nbody",
    )
    .unwrap();
    std::fs::create_dir_all(d.join(".e/skills/broken")).unwrap();
    std::fs::write(d.join(".e/skills/broken/SKILL.md"), "no frontmatter").unwrap();
    let (p, warnings) = e::prompt::build(&d, "2026-11-11");
    assert!(p.contains(&format!("reentrancy: Checks for reentrancy ({})", sk.join("SKILL.md").display())));
    assert!(p.contains("Date: 2026-11-11") && p.contains(&format!("Working directory: {}", d.display())));
    assert!(warnings.iter().any(|w| w.contains("AUDIT.md")));
    assert!(warnings.iter().any(|w| w.contains("broken")));
    std::fs::write(d.join(".e/AUDIT.md"), "Scope: src/\n").unwrap();
    let (p, warnings) = e::prompt::build(&d, "2026-11-11");
    assert!(p.ends_with("<audit_brief>\nScope: src/\n</audit_brief>"));
    assert!(!warnings.iter().any(|w| w.contains("AUDIT.md")));
}
