//! The five tools: read, bash, edit, write, finding.

use crate::config::Config;
use crate::exec::{self, Cancel};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
const SEVERITIES: [&str; 5] = ["critical", "high", "medium", "low", "info"];

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type": "function", "function": {
        "name": name,
        "description": description,
        "parameters": {"type": "object", "properties": properties, "required": required},
    }})
}

pub fn schemas() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    json!([
        tool(
            "read",
            "Read a text file. Lines are prefixed with their line numbers. Output stops at 2000 lines or 50 KB; the result then says which offset to continue from.",
            json!({
                "path": s("File path, absolute or relative to the working directory"),
                "offset": {"type": "integer", "description": "First line to read, 1-based"},
                "limit": {"type": "integer", "description": "Maximum number of lines to read"},
            }),
            &["path"]
        ),
        tool(
            "bash",
            "Run a command with `bash -c` in the working directory. stdin is /dev/null; stdout and stderr are merged. Returns the exit code, the duration and the last 30 KB of output (the full output is saved to a file when longer). The command and everything it starts are killed on timeout and when the command exits.",
            json!({
                "command": s("The command to run"),
                "timeout_s": {"type": "number", "description": "Timeout in seconds (default 120)"},
            }),
            &["command"]
        ),
        tool(
            "edit",
            "Replace one exact string in a file. `old` must occur exactly once in the file, matching whitespace and newlines; otherwise nothing changes and the error gives the match count.",
            json!({"path": s("File path"), "old": s("Exact text to replace"), "new": s("Replacement text")}),
            &["path", "old", "new"]
        ),
        tool(
            "write",
            "Create or overwrite a file with the given content. Parent directories are created.",
            json!({"path": s("File path"), "content": s("Full file content")}),
            &["path", "content"]
        ),
        tool(
            "finding",
            "Record a vulnerability in .e/findings/<id>.json. If poc_cmd is given, the harness runs it in the working directory and sets status `proven` if it exits 0, `suspected` otherwise; without poc_cmd the status is `suspected`. Reusing an id overwrites that finding.",
            json!({
                "id": s("Optional stable id, letters, digits, '-', '_' or '.'; generated if omitted"),
                "title": s("One-line title"),
                "severity": {"type": "string", "enum": SEVERITIES},
                "class": s("Bug class, e.g. reentrancy, access-control, rounding"),
                "file": s("File containing the bug"),
                "function": s("Function containing the bug"),
                "lines": s("Line range, e.g. 120-145"),
                "description": s("What is wrong, with reference to the code"),
                "attack_path": s("Step-by-step: who calls what, with which inputs"),
                "impact": s("What the attacker gains or breaks"),
                "poc_cmd": s("Command that runs the proof of concept; must exit 0 only if the attack succeeds"),
                "fix": s("Suggested fix"),
            }),
            &[
                "title",
                "severity",
                "class",
                "file",
                "function",
                "lines",
                "description",
                "attack_path",
                "impact"
            ]
        ),
    ])
}

pub struct Ctx<'a> {
    pub cwd: &'a Path,
    pub run_dir: &'a Path,
    pub cfg: &'a Config,
    pub cancel: &'a Cancel,
    /// Sequence number of this call in the run, used for log file names.
    pub seq: u32,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub content: String,
    pub is_error: bool,
    /// Short status for the terminal line.
    pub summary: String,
}

impl Output {
    pub fn ok(content: String, summary: &str) -> Self {
        Output { content, is_error: false, summary: summary.into() }
    }

    pub fn err(msg: String) -> Self {
        let summary = format!("error: {}", msg.lines().next().unwrap_or(""));
        Output { content: msg, is_error: true, summary }
    }
}

fn parse<T: DeserializeOwned>(name: &str, v: Value) -> Result<T, Output> {
    serde_json::from_value(v).map_err(|e| Output::err(format!("invalid arguments for {name}: {e}")))
}

fn sync(r: Result<String, String>) -> Output {
    match r {
        Ok(s) => Output::ok(s, "ok"),
        Err(e) => Output::err(e),
    }
}

pub async fn dispatch(name: &str, args: &str, ctx: &Ctx<'_>) -> Output {
    let args = if args.trim().is_empty() { "{}" } else { args };
    let v: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return Output::err(format!("arguments are not valid JSON: {e}")),
    };
    let run = async {
        Ok::<Output, Output>(match name {
            "read" => {
                let a: ReadArgs = parse(name, v)?;
                sync(read(ctx.cwd, &a.path, a.offset, a.limit))
            }
            "bash" => bash(ctx, parse(name, v)?).await,
            "edit" => {
                let a: EditArgs = parse(name, v)?;
                sync(edit(ctx.cwd, &a.path, &a.old, &a.new))
            }
            "write" => {
                let a: WriteArgs = parse(name, v)?;
                sync(write(ctx.cwd, &a.path, &a.content))
            }
            "finding" => finding(ctx, parse(name, v)?).await,
            _ => Output::err(format!("unknown tool: {name}")),
        })
    };
    run.await.unwrap_or_else(|e| e)
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<u64>,
    limit: Option<u64>,
}

pub fn read(cwd: &Path, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<String, String> {
    let full = resolve(cwd, path);
    let e = |e: std::io::Error| format!("cannot read {path}: {e}");
    if full.is_dir() {
        return Err(format!("{path} is a directory; use bash (ls) to list it"));
    }
    let mut r = BufReader::new(std::fs::File::open(&full).map_err(e)?);
    let head = r.fill_buf().map_err(e)?;
    if head[..head.len().min(8192)].contains(&0) {
        return Err(format!("{path} looks binary; use bash (xxd, strings) to inspect it"));
    }
    let start = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(u64::MAX).max(1);
    let (mut out, mut line, mut n, mut last) = (String::new(), Vec::new(), 0u64, 0u64);
    let mut shown = 0u64;
    let mut too_long = None;
    let mut full_stop = false;
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line).map_err(e)? == 0 {
            break;
        }
        n += 1;
        if n < start || full_stop || shown >= limit {
            continue;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let entry = format!("{n:>6}\t{}\n", String::from_utf8_lossy(&line));
        if shown as usize >= MAX_LINES || out.len() + entry.len() > MAX_BYTES {
            if shown == 0 {
                too_long = Some(entry);
            }
            full_stop = true;
            continue;
        }
        out.push_str(&entry);
        shown += 1;
        last = n;
    }
    if n == 0 {
        return Ok(format!("({path} is empty)"));
    }
    if start > n {
        return Err(format!("offset {start} is beyond the end of {path} ({n} lines)"));
    }
    if let Some(entry) = too_long {
        let mut cut = MAX_BYTES;
        while !entry.is_char_boundary(cut) {
            cut -= 1;
        }
        return Ok(format!(
            "{}\n[line {start} is longer than 50 KB; showing its first 50 KB. Use bash (e.g. sed -n '{start}p' {path} | cut -c 50000-) for the rest.]",
            &entry[..cut]
        ));
    }
    if last < n {
        out += &format!("\n[showing lines {start}-{last} of {n}. Use offset={} to continue.]", last + 1);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    timeout_s: Option<f64>,
}

async fn bash(ctx: &Ctx<'_>, a: BashArgs) -> Output {
    let secs = a.timeout_s.unwrap_or(ctx.cfg.bash_timeout_s as f64);
    if !secs.is_finite() || secs <= 0.0 {
        return Output::err(format!("timeout_s must be a positive number, got {secs}"));
    }
    let log = ctx.run_dir.join("bash").join(format!("{:04}.log", ctx.seq));
    match exec::run(&a.command, ctx.cwd, Duration::from_secs_f64(secs), &log, ctx.cancel).await {
        Ok(x) => Output { content: x.model_text(), is_error: !x.success(), summary: x.status_line() },
        Err(e) => Output::err(format!("failed to run command: {e}")),
    }
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
}

/// Occurrences of `needle`, overlapping ones included.
fn count_occurrences(hay: &str, needle: &str) -> usize {
    let (mut count, mut i) = (0, 0);
    while let Some(p) = hay[i..].find(needle) {
        count += 1;
        i += p + hay[i + p..].chars().next().map_or(1, char::len_utf8);
    }
    count
}

pub fn edit(cwd: &Path, path: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() {
        return Err("old must not be empty".into());
    }
    let full = resolve(cwd, path);
    let text = std::fs::read_to_string(&full).map_err(|e| format!("cannot read {path}: {e}"))?;
    match count_occurrences(&text, old) {
        1 => {}
        0 => {
            return Err(format!(
                "old text not found in {path} (0 matches). It must match exactly, including whitespace and newlines."
            ));
        }
        n => {
            return Err(format!(
                "old text matches {n} times in {path}; it must match exactly once. Include more surrounding text."
            ));
        }
    }
    let pos = text.find(old).unwrap_or(0);
    let line = text[..pos].matches('\n').count() + 1;
    let updated = format!("{}{new}{}", &text[..pos], &text[pos + old.len()..]);
    std::fs::write(&full, updated).map_err(|e| format!("cannot write {path}: {e}"))?;
    Ok(format!("Edited {path}: replaced 1 match at line {line}."))
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub fn write(cwd: &Path, path: &str, content: &str) -> Result<String, String> {
    let full = resolve(cwd, path);
    if let Some(dir) = full.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&full, content).map_err(|e| format!("cannot write {path}: {e}"))?;
    Ok(format!("Wrote {path}: {} bytes, {} lines.", content.len(), content.lines().count()))
}

/// Arguments of the finding tool. Unknown fields (such as `status`) are ignored:
/// the model never sets the status.
#[derive(Deserialize)]
pub struct FindingArgs {
    pub id: Option<String>,
    pub title: String,
    pub severity: String,
    pub class: String,
    pub file: String,
    pub function: String,
    pub lines: Value,
    pub description: String,
    pub attack_path: String,
    pub impact: String,
    pub poc_cmd: Option<String>,
    pub fix: Option<String>,
}

fn valid_id(id: &str) -> bool {
    !id.starts_with('.')
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

async fn finding(ctx: &Ctx<'_>, a: FindingArgs) -> Output {
    let severity = a.severity.to_lowercase();
    if !SEVERITIES.contains(&severity.as_str()) {
        return Output::err(format!("severity must be one of {SEVERITIES:?}, got {:?}", a.severity));
    }
    let dir = ctx.cwd.join(".e").join("findings");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Output::err(format!("cannot create {}: {e}", dir.display()));
    }
    let id = match a.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) if valid_id(id) => id.to_string(),
        Some(id) => return Output::err(format!("invalid id {id:?}: use letters, digits, '-', '_' or '.'")),
        None => (1..)
            .map(|n| format!("F-{n:03}"))
            .find(|id| !dir.join(format!("{id}.json")).exists())
            .unwrap_or_default(),
    };
    let poc_cmd = a.poc_cmd.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let mut note = String::new();
    let poc = match poc_cmd {
        None => {
            note = "No poc_cmd given, so the status is suspected.".into();
            None
        }
        Some(cmd) => {
            let log = ctx.run_dir.join("poc").join(format!("{id}-{:04}.log", ctx.seq));
            let timeout = Duration::from_secs(ctx.cfg.poc_timeout_s);
            match exec::run(cmd, ctx.cwd, timeout, &log, ctx.cancel).await {
                Ok(x) => Some(x),
                Err(e) => {
                    note = format!("Failed to run poc_cmd: {e}");
                    None
                }
            }
        }
    };
    let status = if poc.as_ref().is_some_and(|x| x.success()) { "proven" } else { "suspected" };
    let record = json!({
        "id": id,
        "title": a.title,
        "severity": severity,
        "class": a.class,
        "file": a.file,
        "function": a.function,
        "lines": a.lines,
        "description": a.description,
        "attack_path": a.attack_path,
        "impact": a.impact,
        "poc_cmd": poc_cmd,
        "fix": a.fix,
        "status": status,
        "poc": poc.as_ref().map(|x| json!({
            "exit_code": x.exit_code,
            "timed_out": x.timed_out,
            "interrupted": x.interrupted,
            "duration_ms": x.duration.as_millis() as u64,
            "output_tail": x.tail,
            "log": x.log_path,
        })),
        "updated_at": crate::transcript::now_unix(),
        "run_dir": ctx.run_dir,
    });
    let path = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!(".{id}.json.tmp"));
    let text = serde_json::to_string_pretty(&record).unwrap_or_default();
    if let Err(e) = std::fs::write(&tmp, text + "\n").and_then(|_| std::fs::rename(&tmp, &path)) {
        return Output::err(format!("cannot write {}: {e}", path.display()));
    }
    let mut content = format!("Recorded finding {id} in .e/findings/{id}.json. Status: {status}.");
    match &poc {
        Some(x) => content += &format!("\nPoC output:\n{}", x.model_text()),
        None => content += &format!("\n{note}"),
    }
    Output::ok(content, &format!("{id} {status}"))
}
