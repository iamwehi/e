//! `e report`: loading, ordering, duplicate detection and Markdown output.

mod common;

use common::tempdir;
use e::report::{self, Finding};
use serde_json::{Value, json};
use std::path::Path;

fn write(dir: &Path, id: &str, v: Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
}

fn rec(id: &str, severity: &str, status: &str, file: &str, class: &str, lines: Value) -> Value {
    json!({"id": id, "title": format!("title {id}"), "severity": severity, "status": status,
           "class": class, "file": file, "function": "f", "lines": lines, "description": "d",
           "attack_path": "a", "impact": "i", "poc_cmd": null, "poc": null})
}

fn ids(f: &[Finding]) -> Vec<&str> {
    f.iter().map(|f| f.id.as_str()).collect()
}

#[test]
fn findings_sort_proven_first_then_by_severity_and_bad_files_warn() {
    let d = tempdir("report-load").join("findings");
    let mut w = Vec::new();
    assert!(report::load(&d, &mut w).is_empty() && w.is_empty(), "a missing directory is no findings");
    write(&d, "A", rec("A", "low", "proven", "x", "c", json!("1")));
    write(&d, "B", rec("B", "critical", "suspected", "x", "c", json!("1")));
    write(&d, "C", rec("C", "high", "proven", "x", "c", json!("1")));
    write(&d, "D", rec("D", "CRITICAL", "suspected", "x", "c", json!("1")));
    std::fs::write(d.join("bad.json"), "{nope").unwrap();
    std::fs::write(d.join(".E.json.tmp"), "{}").unwrap();
    std::fs::write(d.join("notes.txt"), "x").unwrap();
    let f = report::load(&d, &mut w);
    assert_eq!(ids(&f), ["C", "A", "B", "D"]);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].starts_with("skipped bad.json: "), "{w:?}");
}

#[test]
fn line_ranges_and_duplicates() {
    assert_eq!(report::line_range("120-145"), Some((120, 145)));
    assert_eq!(report::line_range("L145–L120"), Some((120, 145)));
    assert_eq!(report::line_range("42"), Some((42, 42)));
    assert_eq!(report::line_range("1, 7, 9"), Some((1, 9)));
    assert_eq!(report::line_range("n/a"), None);

    let d = tempdir("report-dups").join("findings");
    write(&d, "A", rec("A", "high", "proven", "src/V.sol", "reentrancy", json!("120-145")));
    write(&d, "B", rec("B", "high", "proven", "./src/V.sol", "Reentrancy", json!(130)));
    write(&d, "C", rec("C", "high", "proven", "src/V.sol", "reentrancy", json!("146-150")));
    write(&d, "D", rec("D", "high", "proven", "src/V.sol", "rounding", json!("120-145")));
    write(&d, "E", rec("E", "high", "proven", "src/W.sol", "reentrancy", json!("120-145")));
    write(&d, "F", rec("F", "high", "proven", "src/V.sol", "reentrancy", json!("?")));
    let f = report::load(&d, &mut Vec::new());
    let get = |id: &str| f.iter().find(|x| x.id == id).unwrap();
    assert!(report::likely_duplicates(get("A"), get("B")), "same file and class, overlapping lines");
    assert!(!report::likely_duplicates(get("A"), get("C")), "adjacent, not overlapping");
    assert!(!report::likely_duplicates(get("A"), get("D")), "other class");
    assert!(!report::likely_duplicates(get("A"), get("E")), "other file");
    assert!(report::likely_duplicates(get("A"), get("F")), "lines unparsable: same function decides");
}

#[test]
fn markdown_is_sanitized_and_escaped() {
    let d = tempdir("report-md").join("findings");
    assert_eq!(report::render(&[]), "# Findings\n\nNo findings recorded.\n");
    let mut a = rec("F-001", "critical", "proven", "src/V.sol", "reentrancy", json!("2-3"));
    a["title"] = json!("Drain | via \u{1b}[31mreentry\u{1b}]0;pwn\u{7}");
    a["poc_cmd"] = json!("forge test --mt `x`");
    let tail: String = (1..=60).map(|i| format!("line {i}\n")).collect::<String>() + "```\nend\n";
    a["poc"] = json!({"exit_code": 0, "timed_out": false, "interrupted": false, "duration_ms": 1500,
                      "output_tail": tail, "log": "/r/poc/F-001-0004.log"});
    a["fix"] = json!("Update state first.");
    write(&d, "F-001", a);
    let mut b = rec("F-002", "low", "suspected", "src/V.sol", "reentrancy", json!(3));
    b["poc_cmd"] = json!("bash poc.sh");
    b["poc"] = json!({"exit_code": null, "timed_out": true, "interrupted": false, "duration_ms": 600000,
                      "output_tail": "", "log": "/r/poc/F-002.log"});
    write(&d, "F-002", b);
    let md = report::render(&report::load(&d, &mut Vec::new()));
    assert!(!md.contains('\u{1b}') && !md.contains('\u{7}'), "{md}");
    assert!(md.contains("2 findings: 1 proven, 1 suspected. By severity: 1 critical, 1 low."), "{md}");
    assert!(
        md.contains("| [F-001](#finding-f-001) | critical | proven | - | Drain \\| via reentry | `src/V.sol`, `f`, lines 2-3 |"),
        "{md}"
    );
    assert!(md.contains("## F-001: Drain | via reentry\n"), "{md}");
    assert!(md.contains("**Possible duplicate of:** [F-002](#finding-f-002)"), "{md}");
    assert!(md.contains("Command: `` forge test --mt `x` ``\n"), "{md}");
    assert!(md.contains("Result: exit code 0 after 1.5s (full log: `/r/poc/F-001-0004.log`)"), "{md}");
    assert!(md.contains("Last 40 lines of output:\n\n````\nline 23\n"), "{md}");
    assert!(md.contains("line 60\n```\nend\n````\n"), "a longer fence wraps backticks in the output");
    assert!(!md.contains("line 22\n"), "{md}");
    assert!(md.contains("### Suggested fix\n\nUpdate state first.\n"), "{md}");
    assert!(md.contains("Result: timed out after 600.0s"), "{md}");
    assert!(md.find("## F-001").unwrap() < md.find("## F-002").unwrap());
}
