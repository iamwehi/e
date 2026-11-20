//! `e report`: the findings under `.e/findings/` as one Markdown document.
//!
//! Findings are written by the model, which reads untrusted code, so every
//! string is sanitized before it reaches the output.

use crate::term::sanitize;
use serde_json::Value;
use std::path::Path;

const SEVERITIES: [&str; 5] = ["critical", "high", "medium", "low", "info"];
/// Lines of PoC output shown per finding; the log has the rest.
const POC_LINES: usize = 40;

#[derive(Debug, Clone)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub status: String,
    pub class: String,
    pub file: String,
    pub function: String,
    pub lines: String,
    pub record: Value,
}

impl Finding {
    fn from_json(v: Value) -> Result<Finding, String> {
        let s = |k: &str| match &v[k] {
            Value::String(s) => sanitize(s).trim().to_string(),
            Value::Null => String::new(),
            other => sanitize(&other.to_string()),
        };
        let id = s("id");
        if id.is_empty() {
            return Err("no id".into());
        }
        Ok(Finding {
            id,
            title: s("title"),
            severity: s("severity").to_lowercase(),
            status: s("status"),
            class: s("class"),
            file: s("file"),
            function: s("function"),
            lines: s("lines"),
            record: v,
        })
    }

    fn field(&self, k: &str) -> String {
        match &self.record[k] {
            Value::String(s) => sanitize(s).trim().to_string(),
            _ => String::new(),
        }
    }

    fn severity_rank(&self) -> usize {
        SEVERITIES.iter().position(|s| *s == self.severity).unwrap_or(SEVERITIES.len())
    }

    fn status_rank(&self) -> usize {
        match self.status.as_str() {
            "proven" => 0,
            "suspected" => 1,
            _ => 2,
        }
    }
}

/// Every finding in `dir`, sorted proven first, then by severity, then by id.
/// Files that can't be read or parsed are skipped with a warning.
pub fn load(dir: &Path, warnings: &mut Vec<String>) -> Vec<Finding> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warnings.push(format!("cannot read {}: {e}", dir.display()));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden files are the finding tool's temporary files.
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
            .and_then(Finding::from_json);
        match parsed {
            Ok(f) => out.push(f),
            Err(e) => warnings.push(format!("skipped {}: {}", sanitize(&name), sanitize(&e))),
        }
    }
    out.sort_by(|a, b| {
        (a.status_rank(), a.severity_rank(), &a.id).cmp(&(b.status_rank(), b.severity_rank(), &b.id))
    });
    out
}

/// First and last number in a line spec such as `120-145`, `L120–L145` or `42`.
pub fn line_range(spec: &str) -> Option<(u64, u64)> {
    let nums: Vec<u64> = spec
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    let (a, b) = (*nums.first()?, *nums.last()?);
    Some((a.min(b), a.max(b)))
}

fn norm_path(p: &str) -> String {
    p.trim().trim_start_matches("./").to_string()
}

/// Two findings likely describe the same bug: same file and bug class, and
/// overlapping lines (or the same function when lines can't be compared).
pub fn likely_duplicates(a: &Finding, b: &Finding) -> bool {
    if a.file.is_empty() || norm_path(&a.file) != norm_path(&b.file) {
        return false;
    }
    if !a.class.eq_ignore_ascii_case(&b.class) {
        return false;
    }
    match (line_range(&a.lines), line_range(&b.lines)) {
        (Some((a0, a1)), Some((b0, b1))) => a0 <= b1 && b0 <= a1,
        _ => !a.function.is_empty() && a.function == b.function,
    }
}

/// Text for one cell of a Markdown table.
fn cell(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace(['\n', '\t'], " ")
}

/// `text` in a code fence longer than any run of backticks inside it.
fn fenced(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{}\n{fence}\n", text.trim_end_matches('\n'))
}

/// Inline code, safe for any content.
fn code(s: &str) -> String {
    let s = s.replace('\n', " ");
    let longest = s.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let ticks = "`".repeat(longest + 1);
    let pad = if s.starts_with('`') || s.ends_with('`') { " " } else { "" };
    format!("{ticks}{pad}{s}{pad}{ticks}")
}

fn location(f: &Finding) -> String {
    let mut parts = Vec::new();
    if !f.file.is_empty() {
        parts.push(code(&f.file));
    }
    if !f.function.is_empty() {
        parts.push(code(&f.function));
    }
    if !f.lines.is_empty() {
        parts.push(format!("lines {}", f.lines));
    }
    parts.join(", ")
}

/// The report as Markdown.
pub fn render(findings: &[Finding]) -> String {
    let mut out = String::from("# Findings\n\n");
    if findings.is_empty() {
        out += "No findings recorded.\n";
        return out;
    }
    let count = |pred: &dyn Fn(&Finding) -> bool| findings.iter().filter(|f| pred(f)).count();
    let proven = count(&|f| f.status == "proven");
    out += &format!(
        "{} finding{}: {proven} proven, {} suspected.",
        findings.len(),
        if findings.len() == 1 { "" } else { "s" },
        findings.len() - proven
    );
    let by_sev: Vec<String> = SEVERITIES
        .iter()
        .filter_map(|s| match count(&|f| f.severity == *s) {
            0 => None,
            n => Some(format!("{n} {s}")),
        })
        .collect();
    if !by_sev.is_empty() {
        out += &format!(" By severity: {}.", by_sev.join(", "));
    }
    out += " Proven means the harness ran the proof of concept and it exited 0.\n\n";

    let dups: Vec<Vec<&str>> = findings
        .iter()
        .map(|f| {
            findings
                .iter()
                .filter(|g| g.id != f.id && likely_duplicates(f, g))
                .map(|g| g.id.as_str())
                .collect()
        })
        .collect();

    out += "| ID | Severity | Status | Title | Location |\n| --- | --- | --- | --- | --- |\n";
    for f in findings {
        out += &format!(
            "| [{id}](#{anchor}) | {} | {} | {} | {} |\n",
            cell(&f.severity),
            cell(&f.status),
            cell(&f.title),
            cell(&location(f)),
            id = cell(&f.id),
            anchor = anchor(&f.id),
        );
    }

    for (f, dup) in findings.iter().zip(&dups) {
        out +=
            &format!("\n<a id=\"{}\"></a>\n\n## {}: {}\n\n", anchor(&f.id), f.id, f.title.replace('\n', " "));
        let mut meta = vec![format!("**Severity:** {}", f.severity), format!("**Status:** {}", f.status)];
        if !f.class.is_empty() {
            meta.push(format!("**Class:** {}", f.class));
        }
        out += &meta.join(" · ");
        out += "\n";
        let loc = location(f);
        if !loc.is_empty() {
            out += &format!("\n**Location:** {loc}\n");
        }
        if !dup.is_empty() {
            let links: Vec<String> = dup.iter().map(|d| format!("[{d}](#{})", anchor(d))).collect();
            out += &format!(
                "\n**Possible duplicate of:** {} (same file and class, overlapping lines)\n",
                links.join(", ")
            );
        }
        for (key, heading) in
            [("description", "Description"), ("attack_path", "Attack path"), ("impact", "Impact")]
        {
            let text = f.field(key);
            if !text.is_empty() {
                out += &format!("\n### {heading}\n\n{text}\n");
            }
        }
        out += "\n### Proof of concept\n\n";
        out += &poc_section(f);
        let fix = f.field("fix");
        if !fix.is_empty() {
            out += &format!("\n### Suggested fix\n\n{fix}\n");
        }
    }
    out
}

fn poc_section(f: &Finding) -> String {
    let cmd = f.field("poc_cmd");
    if cmd.is_empty() {
        return "None given.\n".into();
    }
    let mut s = format!("Command: {}\n", code(&cmd));
    let poc = &f.record["poc"];
    if !poc.is_object() {
        return s + "\nThe harness could not run it.\n";
    }
    let secs = poc["duration_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
    let result = if poc["timed_out"] == true {
        "timed out".to_string()
    } else if poc["interrupted"] == true {
        "interrupted".to_string()
    } else {
        match poc["exit_code"].as_i64() {
            Some(c) => format!("exit code {c}"),
            None => "no exit code".to_string(),
        }
    };
    s += &format!("\nResult: {result} after {secs:.1}s");
    if let Some(log) = poc["log"].as_str() {
        s += &format!(" (full log: {})", code(&sanitize(log)));
    }
    s += "\n";
    let tail = sanitize(poc["output_tail"].as_str().unwrap_or(""));
    let lines: Vec<&str> = tail.lines().collect();
    if !lines.is_empty() {
        let shown = &lines[lines.len().saturating_sub(POC_LINES)..];
        if shown.len() < lines.len() {
            s += &format!("\nLast {} lines of output:\n\n", shown.len());
        } else {
            s += "\nOutput:\n\n";
        }
        s += &fenced(&shown.join("\n"));
    }
    s
}

/// An HTML id for a finding id (letters, digits, '-', '_', '.' are kept).
fn anchor(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c.to_ascii_lowercase() } else { '-' })
        .collect();
    format!("finding-{s}")
}
