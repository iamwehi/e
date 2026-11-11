//! End-to-end tests: the real `e` binary against the mock API, in a scratch
//! project, with its config, home and skills directories isolated.

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

struct Env {
    /// The working directory `e` runs in.
    proj: PathBuf,
    /// `$XDG_CONFIG_HOME`.
    xdg: PathBuf,
    home: PathBuf,
    cfg: PathBuf,
}

/// A scratch project and a config file pointing at `url`.
fn project(tag: &str, url: &str, extra_cfg: &str) -> Env {
    let root = tempdir(tag);
    let e = Env {
        proj: root.join("proj"),
        xdg: root.join("xdg"),
        home: root.join("home"),
        cfg: root.join("cfg.toml"),
    };
    for d in [&e.proj, &e.xdg, &e.home] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(&e.cfg, format!("base_url = \"{url}\"\n{extra_cfg}")).unwrap();
    e
}

fn e_cmd(env: &Env, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_e"));
    c.args(args)
        .current_dir(&env.proj)
        .env("E_CONFIG", &env.cfg)
        .env("VENICE_API_KEY", "test-key")
        .env("XDG_CONFIG_HOME", &env.xdg)
        .env("HOME", &env.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    c
}

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run(mut c: Command) -> Out {
    let o = tokio::time::timeout(Duration::from_secs(60), c.output()).await.expect("e did not exit").unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8(o.stdout).unwrap(),
        stderr: String::from_utf8(o.stderr).unwrap(),
    }
}

/// The single run directory under `.e/runs`.
fn run_dirs(proj: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(proj.join(".e/runs"))
        .map(|r| r.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn transcript(run_dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(run_dir.join("transcript.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn help_and_bad_arguments() {
    let env = project("args", "http://127.0.0.1:1", "");
    let o = run(e_cmd(&env, &["--help"])).await;
    assert_eq!(o.code, Some(0));
    assert!(o.stderr.starts_with("usage: e ") && o.stdout.is_empty(), "{}", o.stderr);
    assert_eq!(run(e_cmd(&env, &["-h"])).await.code, Some(0));
    for (args, msg) in [
        (&["--bogus"][..], "e: unknown argument: --bogus"),
        (&["--json"], "e: --json needs -p"),
        (&["-p"], "e: -p needs a task"),
        (&["-p", "x", "--model"], "e: --model needs an id"),
        (&["stray"], "e: unknown argument: stray"),
    ] {
        let o = run(e_cmd(&env, args)).await;
        assert_eq!(o.code, Some(2), "{args:?}");
        assert!(o.stderr.starts_with(&format!("{msg}\nusage: e ")), "{args:?}: {}", o.stderr);
    }
    assert!(!env.proj.join(".e").exists(), "bad arguments create no run directory");
}

#[tokio::test]
async fn headless_audit_proves_a_finding_end_to_end() {
    let m = mock(vec![
        tool_reply(&[("c1", "read", json!({"path": "src/Vault.sol"}))], usage(1000, 50)),
        tool_reply(
            &[(
                "c2",
                "write",
                json!({"path": "test/poc.sh", "content": "grep -q 'no reentrancy guard' src/Vault.sol\n"}),
            )],
            usage(1200, 50),
        ),
        tool_reply(&[("c3", "bash", json!({"command": "bash test/poc.sh && echo POC-OK"}))], usage(1300, 50)),
        tool_reply(
            &[(
                "c4",
                "finding",
                json!({"title": "Reentrancy in withdraw", "severity": "critical", "class": "reentrancy",
                       "file": "src/Vault.sol", "function": "withdraw", "lines": "2-3",
                       "description": "d", "attack_path": "a", "impact": "drain", "poc_cmd": "bash test/poc.sh"}),
            )],
            usage(1400, 50),
        ),
        // Escape sequences in the final message must not reach the terminal.
        text_reply("1 proven \u{1b}[31mfinding\u{1b}]0;x\u{7}.\n\n", usage(1500, 50)),
    ])
    .await;
    let env = project("audit", &m.url, "");
    std::fs::create_dir_all(env.proj.join("src")).unwrap();
    std::fs::write(env.proj.join("src/Vault.sol"), "contract Vault {\n  // no reentrancy guard\n}\n")
        .unwrap();
    let skills = env.proj.join(".e/skills/reentrancy");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(skills.join("SKILL.md"), "---\nname: reentrancy\ndescription: CEI checklist\n---\n")
        .unwrap();
    let global = env.xdg.join("e/skills/foundry");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(global.join("SKILL.md"), "---\nname: foundry\ndescription: forge recipes\n---\n").unwrap();
    std::fs::write(env.proj.join(".e/AUDIT.md"), "Scope: src/Vault.sol\nOwner is trusted.\n").unwrap();

    let o = run(e_cmd(&env, &["-p", "Audit the vault"])).await;
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert_eq!(o.stdout, "1 proven finding.\n", "only the sanitized final message on stdout");
    assert!(!o.stderr.contains("warning"), "AUDIT.md exists: {}", o.stderr);
    assert!(!format!("{}{}", o.stdout, o.stderr).contains('\u{1b}'), "no escape codes when piped");
    assert!(o.stderr.contains("bash bash test/poc.sh && echo POC-OK -> exit 0"), "{}", o.stderr);
    assert!(o.stderr.contains("finding [critical] Reentrancy in withdraw -> F-001 proven"), "{}", o.stderr);

    let reqs = m.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 5);
    let system = reqs[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains(&format!("Working directory: {}", env.proj.display())));
    assert!(system.contains("<audit_brief>\nScope: src/Vault.sol\nOwner is trusted.\n</audit_brief>"));
    for (name, desc, dir) in [("foundry", "forge recipes", &global), ("reentrancy", "CEI checklist", &skills)]
    {
        let entry = format!(
            "<skill>\n<name>{name}</name>\n<description>{desc}</description>\n<location>{}</location>\n</skill>",
            dir.join("SKILL.md").display()
        );
        assert!(system.contains(&entry), "missing {entry} in {system}");
    }
    assert_eq!(reqs[0]["messages"][1], json!({"role": "user", "content": "Audit the vault"}));
    let read_result = reqs[1]["messages"][3]["content"].as_str().unwrap();
    assert_eq!(read_result, "     1\tcontract Vault {\n     2\t  // no reentrancy guard\n     3\t}\n");
    assert!(reqs[3]["messages"].as_array().unwrap()[7]["content"].as_str().unwrap().starts_with("POC-OK\n"));

    let rec: Value =
        serde_json::from_str(&std::fs::read_to_string(env.proj.join(".e/findings/F-001.json")).unwrap())
            .unwrap();
    assert_eq!(rec["status"], "proven");
    assert_eq!(rec["poc"]["exit_code"], 0);
    assert_eq!(std::fs::read_to_string(env.proj.join("test/poc.sh")).unwrap().lines().count(), 1);

    let dirs = run_dirs(&env.proj);
    assert_eq!(dirs.len(), 1);
    assert_eq!(std::fs::read_to_string(dirs[0].join("bash/0003.log")).unwrap(), "POC-OK\n");
    assert!(dirs[0].join("poc/F-001-0004.log").exists());
    let t = transcript(&dirs[0]);
    assert_eq!(t[0]["type"], "session");
    assert_eq!(t[0]["model"], "deepseek-v4-1-flash");
    assert_eq!(t[0]["warnings"], json!([]));
    let last = t.last().unwrap();
    assert_eq!((last["type"].as_str(), last["end"]["status"].as_str()), (Some("run_end"), Some("done")));
    assert_eq!(last["context_tokens"], 1550);
    let tools: Vec<&str> =
        t.iter().filter(|e| e["type"] == "tool_result").map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(tools, ["read", "write", "bash", "finding"]);
}

#[tokio::test]
async fn json_mode_prints_the_transcript_and_nothing_else() {
    let m = mock(vec![
        tool_reply(&[("c1", "bash", json!({"command": "echo hi"}))], usage(10, 5)),
        text_reply("done", usage(20, 5)),
    ])
    .await;
    let env = project("json", &m.url, "");
    let o = run(e_cmd(&env, &["-p", "go", "--json", "--verbose"])).await;
    assert_eq!(o.code, Some(0));
    assert_eq!(o.stderr, "", "no warnings, progress or reasoning in --json mode");
    let events: Vec<Value> = o.stdout.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "session",
            "user",
            "request",
            "response",
            "tool_call",
            "tool_result",
            "request",
            "response",
            "run_end"
        ]
    );
    assert!(events[0]["warnings"][0].as_str().unwrap().contains("AUDIT.md"), "warnings are in the event");
    assert_eq!(events[8]["end"], json!({"status": "done", "message": "done"}));
    let dir = &run_dirs(&env.proj)[0];
    assert_eq!(
        std::fs::read_to_string(dir.join("transcript.jsonl")).unwrap(),
        o.stdout,
        "same lines as the log"
    );
}

#[tokio::test]
async fn verbose_headless_shows_reasoning_and_tool_output_on_stderr() {
    let m = mock(vec![
        tool_reply(&[("c1", "bash", json!({"command": "echo tool-output"}))], usage(10, 5)),
        text_reply("final", usage(20, 5)),
    ])
    .await;
    let env = project("verbose", &m.url, "");
    let o = run(e_cmd(&env, &["-p", "go", "-v"])).await;
    assert_eq!(o.code, Some(0));
    assert_eq!(o.stdout, "final\n");
    for want in ["[warning: no .e/AUDIT.md", "plan", "thinking done", "tool-output\n[exit code 0", "final"] {
        assert!(o.stderr.contains(want), "missing {want:?} in {}", o.stderr);
    }
    // Without --verbose: neither reasoning nor tool output.
    let m = mock(vec![
        tool_reply(&[("c1", "bash", json!({"command": "echo tool-output"}))], usage(10, 5)),
        text_reply("final", usage(20, 5)),
    ])
    .await;
    let env = project("quiet", &m.url, "");
    let o = run(e_cmd(&env, &["-p", "go"])).await;
    assert!(!o.stderr.contains("thinking") && !o.stderr.contains("tool-output\n"), "{}", o.stderr);
    // With no skills anywhere, the prompt has no skills section.
    let system = m.requests.lock().unwrap()[0]["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(!system.contains("available_skills"), "{system}");
}

#[tokio::test]
async fn model_flag_overrides_the_config() {
    let m = mock(vec![text_reply("a", usage(1, 1)), text_reply("b", usage(1, 1))]).await;
    let env = project("model", &m.url, "model = \"from-config\"\n");
    assert_eq!(run(e_cmd(&env, &["--model", "openai-gpt-55", "-p", "x"])).await.code, Some(0));
    assert_eq!(run(e_cmd(&env, &["-p", "x"])).await.code, Some(0));
    let reqs = m.requests.lock().unwrap().clone();
    assert_eq!(reqs[0]["model"], "openai-gpt-55");
    assert_eq!(reqs[0]["prompt_cache_retention"], "extended");
    assert_eq!(reqs[1]["model"], "from-config");
    assert!(reqs[1].get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn api_errors_exit_1_and_budget_exhaustion_exits_2() {
    let m = mock(vec![Resp::Status(400, "{\"error\": \"bad \\u001b[2Jrequest\"}".into())]).await;
    let env = project("err", &m.url, "");
    let o = run(e_cmd(&env, &["-p", "x"])).await;
    assert_eq!(o.code, Some(1));
    assert!(o.stdout.is_empty());
    assert!(
        o.stderr.contains("e: HTTP 400 Bad Request: {\"error\": \"bad \\u001b[2Jrequest\"}"),
        "{}",
        o.stderr
    );
    let t = transcript(&run_dirs(&env.proj)[0]);
    assert_eq!(t.last().unwrap()["end"]["status"], "error");

    let m = mock(vec![tool_reply(&[("c1", "bash", json!({"command": "touch ran"}))], usage(900, 200))]).await;
    let env = project("budget", &m.url, "context_budget = 1000\n");
    let o = run(e_cmd(&env, &["-p", "x"])).await;
    assert_eq!(o.code, Some(2));
    assert!(o.stderr.contains("e: context budget exhausted: 1100 of 1000 tokens"), "{}", o.stderr);
    assert!(!env.proj.join("ran").exists());
}

#[tokio::test]
async fn api_key_comes_from_the_environment_then_the_config() {
    let m =
        mock(vec![text_reply("a", usage(1, 1)), text_reply("b", usage(1, 1)), text_reply("c", usage(1, 1))])
            .await;
    let env = project("key", &m.url, "api_key = \"from-config\"\n");
    assert_eq!(run(e_cmd(&env, &["-p", "x"])).await.code, Some(0));
    let mut c = e_cmd(&env, &["-p", "x"]);
    c.env_remove("VENICE_API_KEY");
    assert_eq!(run(c).await.code, Some(0));
    let mut c = e_cmd(&env, &["-p", "x"]);
    c.env("VENICE_API_KEY", "  padded \n");
    assert_eq!(run(c).await.code, Some(0));
    let heads = m.heads.lock().unwrap().clone();
    assert!(heads[0].contains("authorization: bearer test-key"));
    assert!(heads[1].contains("authorization: bearer from-config"));
    assert!(heads[2].contains("authorization: bearer padded\r\n"), "{}", heads[2]);

    // No key anywhere: a clear error before any request or run directory.
    let env = project("nokey", &m.url, "api_key_env = \"E_TEST_KEY\"\n");
    let mut c = e_cmd(&env, &["-p", "x"]);
    c.env("E_TEST_KEY", "   ");
    let o = run(c).await;
    assert_eq!(o.code, Some(1));
    assert_eq!(o.stderr, "e: E_TEST_KEY is not set\n");
    assert_eq!(m.requests.lock().unwrap().len(), 3);
    assert!(!env.proj.join(".e").exists());
}

#[tokio::test]
async fn config_file_is_found_and_validated() {
    let m = mock(vec![text_reply("xdg", usage(1, 1)), text_reply("home", usage(1, 1))]).await;
    let env = project("cfgfind", "http://127.0.0.1:1", "");
    // $E_CONFIG empty: $XDG_CONFIG_HOME/e/config.toml.
    std::fs::create_dir_all(env.xdg.join("e")).unwrap();
    std::fs::write(env.xdg.join("e/config.toml"), format!("base_url = \"{}\"\nmodel = \"xdg\"\n", m.url))
        .unwrap();
    let mut c = e_cmd(&env, &["-p", "x"]);
    c.env("E_CONFIG", "");
    assert_eq!(run(c).await.stdout, "xdg\n");
    // No $XDG_CONFIG_HOME: ~/.config/e/config.toml.
    std::fs::create_dir_all(env.home.join(".config/e")).unwrap();
    std::fs::write(
        env.home.join(".config/e/config.toml"),
        format!("base_url = \"{}\"\nmodel = \"home\"\n", m.url),
    )
    .unwrap();
    let mut c = e_cmd(&env, &["-p", "x"]);
    c.env_remove("E_CONFIG").env_remove("XDG_CONFIG_HOME");
    assert_eq!(run(c).await.stdout, "home\n");
    let reqs = m.requests.lock().unwrap().clone();
    assert_eq!((reqs[0]["model"].as_str(), reqs[1]["model"].as_str()), (Some("xdg"), Some("home")));

    // An old JSON config without a TOML one is an error, not silently ignored.
    std::fs::remove_file(env.xdg.join("e/config.toml")).unwrap();
    std::fs::write(env.xdg.join("e/config.json"), "{}").unwrap();
    let mut c = e_cmd(&env, &["-p", "x"]);
    c.env_remove("E_CONFIG");
    let o = run(c).await;
    assert_eq!(o.code, Some(1));
    assert!(o.stderr.contains("config.json is no longer read: rewrite it as"), "{}", o.stderr);

    // Typos and bad values name the file and the key.
    std::fs::write(&env.cfg, "modle = \"x\"\n").unwrap();
    let o = run(e_cmd(&env, &["-p", "x"])).await;
    assert_eq!(o.code, Some(1));
    assert!(o.stderr.starts_with(&format!("e: config: {}: ", env.cfg.display())), "{}", o.stderr);
    assert!(o.stderr.contains("modle"), "{}", o.stderr);
    std::fs::write(&env.cfg, "bash_timeout_s = -5\n").unwrap();
    assert_eq!(run(e_cmd(&env, &["-p", "x"])).await.code, Some(1));
    // A config path that is a directory.
    let mut c = e_cmd(&env, &["-p", "x"]);
    c.env("E_CONFIG", &env.home);
    let o = run(c).await;
    assert_eq!(o.code, Some(1));
    assert!(o.stderr.starts_with("e: config: "), "{}", o.stderr);
    assert_eq!(run_dirs(&env.proj).len(), 2, "only the two good runs left a log");
}

fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
        Err(_) => false,
    }
}

async fn wait_for_pid(file: &Path) -> i32 {
    for _ in 0..500 {
        if let Ok(s) = std::fs::read_to_string(file)
            && let Ok(pid) = s.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{} was not written", file.display());
}

/// Starts `e -p` on a long-running command, sends `sig` once the command
/// runs, and returns the output and the command's pid.
async fn signal_during_a_command(tag: &str, sig: i32) -> (Out, i32, PathBuf) {
    let m = mock(vec![
        tool_reply(&[("c1", "bash", json!({"command": "echo $$ > sleeper.pid; sleep 60"}))], usage(10, 5)),
        text_reply("never", usage(10, 5)),
    ])
    .await;
    let env = project(tag, &m.url, "");
    let child = e_cmd(&env, &["-p", "go"]).spawn().unwrap();
    let sleeper = wait_for_pid(&env.proj.join("sleeper.pid")).await;
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(child.id().unwrap() as i32, sig);
    }
    let o = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("e did not exit on the signal")
        .unwrap();
    let out = Out {
        code: o.status.code(),
        stdout: String::from_utf8(o.stdout).unwrap(),
        stderr: String::from_utf8(o.stderr).unwrap(),
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    (out, sleeper, env.proj)
}

#[tokio::test]
async fn sigint_cancels_the_run_and_kills_the_command() {
    let (o, sleeper, proj) = signal_during_a_command("sigint", libc::SIGINT).await;
    assert_eq!(o.code, Some(130));
    assert!(o.stderr.ends_with("e: interrupted\n"), "{}", o.stderr);
    assert!(o.stdout.is_empty());
    assert!(!alive(sleeper), "the command's process group is killed");
    let t = transcript(&run_dirs(&proj)[0]);
    let result = t.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert!(result["content"].as_str().unwrap().contains("interrupted by the user"));
    assert_eq!(t.last().unwrap()["end"]["status"], "interrupted");
}

#[tokio::test]
async fn sigterm_and_sighup_exit_at_once_and_kill_the_command() {
    for (sig, code) in [(libc::SIGTERM, 143), (libc::SIGHUP, 129)] {
        let (o, sleeper, _) = signal_during_a_command(&format!("sig{sig}"), sig).await;
        assert_eq!(o.code, Some(code), "signal {sig}: {}", o.stderr);
        assert!(!alive(sleeper), "signal {sig}: the command's process group is killed");
    }
}

/// Runs the interactive prompt with `input` on a pipe (plain line reading).
async fn interactive(env: &Env, input: &str) -> Out {
    let mut c = e_cmd(env, &[]);
    c.stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    drop(stdin);
    let o = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await.unwrap().unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8(o.stdout).unwrap(),
        stderr: String::from_utf8(o.stderr).unwrap(),
    }
}

#[tokio::test]
async fn interactive_commands_over_a_pipe() {
    let m = mock(vec![
        text_reply("reply one", usage(100, 10)),
        text_reply("reply two", usage(200, 10)),
        text_reply("reply three", usage(50, 10)),
        text_reply("hunted", usage(60, 10)),
        Resp::Status(400, "rejected".into()),
        text_reply("still here", usage(50, 10)),
    ])
    .await;
    let env = project("interactive", &m.url, "");
    let hunt = env.proj.join(".e/skills/hunt");
    std::fs::create_dir_all(&hunt).unwrap();
    std::fs::write(
        hunt.join("SKILL.md"),
        "---\nname: hunt\ndescription: hunt bugs\n---\nLook for reentrancy.\n",
    )
    .unwrap();
    let input = "first\n/model other-model\n/model\n  second  \n\n/new\nthird\n/skills\n/hunt in Vault.sol\n/bogus\n/model a b\nfourth\nfifth\n";
    let o = interactive(&env, input).await;
    assert_eq!(o.code, Some(0), "EOF exits 0: {}", o.stderr);
    assert!(!o.stdout.contains('\u{1b}'), "no colour on a pipe");
    let out = &o.stdout;
    let greeting = format!("[e · deepseek-v4-1-flash · {} · log ", env.proj.display());
    assert!(out.starts_with(&greeting), "{out}");
    assert!(out.contains("[warning: no .e/AUDIT.md"));
    for want in [
        "deepseek-v4-1-flash $0.0000 > reply one\n",
        "[no prices for other-model: showing token counts]\n",
        "[model: other-model]\n",
        "reply two\n",
        "[e · other-model · ",
        "reply three\n",
        "[/hunt: hunt bugs]\n",
        "[skill: hunt]\n",
        "hunted\n",
        "[commands: /model <id>, /new, /skills, /<skill> [task]]\n",
        "[error: HTTP 400 Bad Request: rejected]\n",
        "still here\n",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    // The prompt shows the model and the running cost; unpriced tokens are counted.
    assert!(out.contains("other-model $0.0000+210 tok > "), "{out}");
    assert_eq!(out.matches("[commands: ").count(), 2, "/bogus and /model with two arguments");
    assert_eq!(out.matches("[e · ").count(), 2, "greeted again after /new");

    let reqs = m.requests.lock().unwrap().clone();
    let summary: Vec<(String, usize, String)> = reqs
        .iter()
        .map(|r| {
            let msgs = r["messages"].as_array().unwrap();
            let last = msgs.last().unwrap()["content"].as_str().unwrap().to_string();
            (r["model"].as_str().unwrap().to_string(), msgs.len(), last)
        })
        .collect();
    let s = |m: &str, n, c: &str| (m.to_string(), n, c.to_string());
    assert_eq!(
        summary,
        [
            s("deepseek-v4-1-flash", 2, "first"),
            s("other-model", 4, "second"),
            s("other-model", 2, "third"),
            s("other-model", 4, &summary[3].2),
            s("other-model", 6, "fourth"),
            // The failed run left its user message; the API accepts consecutive user messages.
            s("other-model", 7, "fifth"),
        ]
    );
    // /hunt sends the skill's instructions with the task after its name.
    let skill_msg = &summary[3].2;
    assert!(
        skill_msg.starts_with("<skill_content name=\"hunt\">\nLook for reentrancy.\n")
            && skill_msg.ends_with("</skill_content>\n\nin Vault.sol"),
        "{skill_msg}"
    );
    // /new starts a new transcript and a new cache key.
    assert_eq!(run_dirs(&env.proj).len(), 2);
    assert_eq!(reqs[0]["prompt_cache_key"], reqs[1]["prompt_cache_key"]);
    assert_ne!(reqs[1]["prompt_cache_key"], reqs[2]["prompt_cache_key"]);
}

#[tokio::test]
async fn interactive_eof_at_once_exits_cleanly() {
    let env = project("eof", "http://127.0.0.1:1", "");
    let o = interactive(&env, "").await;
    assert_eq!(o.code, Some(0));
    assert!(o.stdout.ends_with("deepseek-v4-1-flash $0.0000 > "), "{}", o.stdout);
    assert_eq!(run_dirs(&env.proj).len(), 1, "the session log exists even with no runs");
}
