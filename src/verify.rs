//! Second opinions on recorded findings.
//!
//! `e verify` re-runs each finding's PoC on a clean copy of the repository
//! (tracked files at HEAD, only the PoC files the finding kept added back)
//! and sets the status from that run. `e triage` starts a fresh session per
//! finding whose only job is to refute it, and records its verdict.

use crate::agent::{Event, Mode, RunEnd, Session};
use crate::config::{Config, Meter};
use crate::exec::{self, Cancel};
use crate::provider::Message;
use crate::report;
use crate::tools::{self, VerdictArgs};
use crate::transcript::now_unix;
use crate::tree;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

fn findings_dir(cwd: &Path) -> std::path::PathBuf {
    cwd.join(".e").join("findings")
}

/// The findings named by `ids` (all of them if empty), as raw records.
fn select(cwd: &Path, ids: &[String], warn: &mut dyn FnMut(&str)) -> Result<Vec<(String, Value)>, String> {
    let mut warnings = Vec::new();
    let all = report::load(&findings_dir(cwd), &mut warnings);
    for w in &warnings {
        warn(&format!("warning: {w}"));
    }
    if ids.is_empty() {
        return Ok(all.into_iter().map(|f| (f.id, f.record)).collect());
    }
    ids.iter()
        .map(|id| {
            all.iter()
                .find(|f| &f.id == id)
                .map(|f| (f.id.clone(), f.record.clone()))
                .ok_or_else(|| format!("no finding {id} in .e/findings"))
        })
        .collect()
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Re-run PoCs. `say` gets one line per finding. Returns whether every PoC
/// that ran passed.
pub async fn verify(
    cfg: &Config,
    cwd: &Path,
    ids: &[String],
    cancel: &Cancel,
    say: &mut dyn FnMut(&str),
) -> Result<bool, String> {
    let scrub = cfg.scrubbed_env();
    let selected = select(cwd, ids, say)?;
    if selected.is_empty() {
        say("no findings to verify");
        return Ok(true);
    }
    let git = tree::toplevel(cwd, &scrub).is_some();
    if !git {
        say("warning: not a git repository: PoCs run in place, not on a clean copy");
    }
    let base = crate::transcript::unique_dir(&cwd.join(".e").join("verify"))
        .map_err(|e| format!("cannot create .e/verify: {e}"))?;
    let abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut all_ok = true;
    for (id, mut rec) in selected {
        if cancel.is_cancelled() {
            break;
        }
        let cmd = text(&rec["poc_cmd"]);
        if cmd.trim().is_empty() {
            say(&format!("{id}: no poc_cmd, skipped"));
            continue;
        }
        let before = text(&rec["status"]);
        let copy = base.join(format!("{id}.tree"));
        let mut notes = Vec::new();
        let workdir = if git {
            match tree::clean_copy(cwd, &copy, &scrub) {
                Ok(w) => w,
                Err(e) => {
                    all_ok = false;
                    say(&format!("{id}: cannot make a clean copy: {e}; status unchanged"));
                    let _ = std::fs::remove_dir_all(&copy);
                    continue;
                }
            }
        } else {
            cwd.to_path_buf()
        };
        let snap = findings_dir(cwd).join(format!("{id}.poc"));
        if git && snap.is_dir() {
            if let Err(e) = tree::restore(&snap, &workdir) {
                notes.push(format!("restoring the PoC files failed: {e}"));
            }
        } else if git {
            notes.push("no PoC files were kept with this finding".to_string());
        }
        // A PoC that names the working directory by absolute path would run against the
        // original tree; point it at the copy instead.
        let mut run_cmd = cmd.clone();
        if git {
            // One pass: the copy lives under the working directory, so its path contains it.
            let named = [abs.display().to_string(), cwd.display().to_string()];
            if let Some(dir) = named.iter().find(|d| run_cmd.contains(d.as_str())) {
                run_cmd = run_cmd.replace(dir.as_str(), &workdir.display().to_string());
                notes.push("poc_cmd named the working directory; it ran with the copy's path".into());
            }
        }
        let log = base.join(format!("{id}.log"));
        let timeout = Duration::from_secs(cfg.poc_timeout_s);
        let x = exec::run(&run_cmd, &workdir, timeout, &log, &scrub, cancel).await;
        if git {
            let _ = std::fs::remove_dir_all(&copy);
        }
        let x = match x {
            Ok(x) => x,
            Err(e) => {
                all_ok = false;
                say(&format!("{id}: cannot run poc_cmd: {e}; status unchanged"));
                continue;
            }
        };
        if x.interrupted {
            say(&format!("{id}: interrupted; status unchanged"));
            break;
        }
        let ok = x.success();
        all_ok &= ok;
        let after = if ok { "proven" } else { "suspected" };
        rec["status"] = json!(after);
        rec["verification"] = json!({
            "at": now_unix(),
            "clean_copy": git,
            "exit_code": x.exit_code,
            "timed_out": x.timed_out,
            "duration_ms": x.duration.as_millis() as u64,
            "output_tail": x.tail,
            "log": x.log_path,
            "notes": notes,
        });
        tools::save_finding(&findings_dir(cwd), &id, &rec)?;
        let mut line = format!("{id}: {before} -> {after} ({})", x.status_line());
        for n in &notes {
            line += &format!("; {n}");
        }
        say(&line);
    }
    Ok(all_ok)
}

/// The user message that starts a triage session for one finding.
pub fn triage_task(id: &str, rec: &Value, cwd: &Path) -> String {
    let mut shown = serde_json::Map::new();
    for k in [
        "title",
        "severity",
        "class",
        "file",
        "function",
        "lines",
        "description",
        "attack_path",
        "impact",
        "poc_cmd",
        "fix",
        "status",
    ] {
        if !rec[k].is_null() {
            shown.insert(k.into(), rec[k].clone());
        }
    }
    if rec["poc"].is_object() {
        shown.insert(
            "poc_result".into(),
            json!({"exit_code": rec["poc"]["exit_code"], "timed_out": rec["poc"]["timed_out"],
                   "output_tail": rec["poc"]["output_tail"]}),
        );
    }
    let body = serde_json::to_string_pretty(&Value::Object(shown)).unwrap_or_default();
    let mut msg = format!(
        "Review finding {id}. This is its record, as the auditor wrote it:\n\n<finding>\n{body}\n</finding>\n"
    );
    let files: Vec<String> = rec["poc_files"]
        .as_array()
        .map(|v| v.iter().filter_map(|f| f["path"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    let snap = findings_dir(cwd).join(format!("{id}.poc"));
    if !files.is_empty() && snap.is_dir() {
        msg += &format!(
            "\nThe files the auditor added for the PoC are copied in {}, at the same relative paths: {}.\n",
            snap.display(),
            files.join(", ")
        );
    }
    msg += "\nTry to refute it, then call verdict.";
    msg
}

/// The last valid verdict call in a session's history.
pub fn last_verdict(history: &[Message]) -> Option<VerdictArgs> {
    history
        .iter()
        .rev()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flat_map(|calls| calls.iter().rev())
        .filter(|c| c.function.name == "verdict")
        .find_map(|c| {
            let a: VerdictArgs = serde_json::from_str(&c.function.arguments).ok()?;
            a.check().ok().map(|v| VerdictArgs { verdict: v, ..a })
        })
}

/// Review findings one by one, each in a fresh session; `meter` carries the
/// cost across them (and in from earlier work) for `max_cost_usd`. Returns
/// the exit code: 0, 1 if a review failed, 3 at the cost limit, 130 if
/// interrupted.
#[allow(clippy::too_many_arguments)]
pub async fn triage(
    cfg: &Config,
    model: &str,
    cwd: &Path,
    ids: &[String],
    meter: &mut Meter,
    cancel: &Cancel,
    say: &mut dyn FnMut(&str),
    ev: &mut dyn FnMut(Event),
) -> Result<u8, String> {
    let selected = select(cwd, ids, say)?;
    if selected.is_empty() {
        say("no findings to triage");
        return Ok(0);
    }
    let mut code = 0;
    for (id, rec) in selected {
        if cancel.is_cancelled() {
            return Ok(130);
        }
        let mut s =
            Session::with_mode(cfg.clone(), model.to_string(), cwd.to_path_buf(), false, Mode::Triage)?;
        s.meter = *meter;
        say(&format!("{id}: reviewing (log {})", s.transcript.dir.display()));
        let end = s.run(&triage_task(&id, &rec, cwd), cancel, ev).await;
        *meter = s.meter;
        let verdict = last_verdict(&s.history);
        // Re-read: the record may have changed while the review ran.
        let path = findings_dir(cwd).join(format!("{id}.json"));
        let mut fresh: Value =
            std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(rec);
        let end_status = match &end {
            RunEnd::Done(_) => "done",
            RunEnd::Interrupted => "interrupted",
            RunEnd::Budget(_) => "budget_exhausted",
            RunEnd::CostLimit(_) => "cost_limit",
            RunEnd::Error(_) => "error",
        };
        match &verdict {
            Some(v) => {
                fresh["triage"] = json!({
                    "verdict": v.verdict,
                    "reasoning": v.reasoning,
                    "severity": v.severity.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()),
                    "model": model,
                    "at": now_unix(),
                    "run_dir": s.transcript.dir,
                });
                tools::save_finding(&findings_dir(cwd), &id, &fresh)?;
                let first = v.reasoning.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                say(&format!("{id}: {} — {}", v.verdict, crate::term::sanitize(first)));
            }
            None => say(&format!("{id}: no verdict ({end_status}); record unchanged")),
        }
        match end {
            RunEnd::Interrupted => return Ok(130),
            RunEnd::CostLimit(m) => {
                say(&m);
                return Ok(3);
            }
            RunEnd::Error(m) => {
                say(&format!("{id}: error: {}", crate::term::sanitize(&m)));
                code = 1;
            }
            RunEnd::Budget(_) | RunEnd::Done(_) => {}
        }
    }
    Ok(code)
}
