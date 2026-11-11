//! Agent Skills support: <https://agentskills.io/specification>.

mod common;

use common::tempdir;
use e::skills::{Skill, catalog, discover, parse};
use std::path::{Path, PathBuf};

fn skill(root: &Path, dir: &str, text: &str) -> PathBuf {
    let d = root.join(dir);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("SKILL.md"), text).unwrap();
    d.join("SKILL.md")
}

fn problems(text: &str, dir: &str) -> Vec<String> {
    parse(text, dir).unwrap().problems
}

#[test]
fn spec_examples_parse_cleanly() {
    let p = parse(
        "---\nname: skill-name\ndescription: A description of what this skill does.\n---\n",
        "skill-name",
    )
    .unwrap();
    assert_eq!(
        (p.name.as_str(), p.description.as_str()),
        ("skill-name", "A description of what this skill does.")
    );
    assert!(p.problems.is_empty());
    let full = "---\nname: pdf-processing\ndescription: Extract PDF text, fill forms, merge files. Use when handling PDFs.\n\
                license: Apache-2.0\ncompatibility: Requires Python 3.14+ and uv\nmetadata:\n  author: example-org\n  \
                version: \"1.0\"\nallowed-tools: Bash(git:*) Bash(jq:*) Read\n---\n# PDF\n";
    let p = parse(full, "pdf-processing").unwrap();
    assert_eq!(p.description, "Extract PDF text, fill forms, merge files. Use when handling PDFs.");
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    // Fields from other clients are not an error.
    assert!(
        problems("---\nname: a\ndescription: d\nuser-invocable: false\nargument-hint: x\n---\n", "a")
            .is_empty()
    );
    assert!(problems("---\nname: a\ndescription: d\ndisable-model-invocation: false\n---\n", "a").is_empty());
}

#[test]
fn yaml_forms_other_clients_write() {
    // Block scalars, CRLF line endings, a BOM, comments.
    let p = parse(
        "\u{feff}---\r\nname: a # comment\r\ndescription: >-\r\n  Line one\r\n  line two.\r\n---\r\nbody",
        "a",
    )
    .unwrap();
    assert_eq!(p.description, "Line one line two.");
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    let p = parse("---\nname: a\ndescription: |\n  First.\n  Second.\n---\n", "a").unwrap();
    assert_eq!(p.description, "First.\nSecond.");
    // An unquoted colon is invalid YAML; it loads, with a warning.
    let p = parse("---\nname: a\ndescription: Use this skill when: the user asks about PDFs\n---\n", "a")
        .unwrap();
    assert_eq!(p.description, "Use this skill when: the user asks about PDFs");
    assert!(p.problems.iter().any(|w| w.contains("invalid YAML")));
    let p = parse("---\nname: a\ndescription: it's for: Rust's borrowck\n---\n", "a").unwrap();
    assert_eq!(p.description, "it's for: Rust's borrowck");
    // Wrapped values fold into one line, whichever line has the colon.
    for (text, want) in [
        (
            "---\nname: a\ndescription: Use when: the user asks about PDFs,\n  or forms.\nlicense: MIT\n---\n",
            "Use when: the user asks about PDFs, or forms.",
        ),
        (
            "---\nname: a\ndescription: Use for PDFs,\n\tor when: forms.\r\nlicense: MIT\n---\n",
            "Use for PDFs, or when: forms.",
        ),
    ] {
        let p = parse(text, "a").unwrap();
        assert_eq!(p.description, want);
        assert_eq!(p.problems.len(), 1, "{:?}", p.problems);
    }
    // Valid lines, comments included, survive the fallback unchanged; nested values aren't touched.
    let p = parse(
        "---\nname: pdf # renamed from: pdf-tools\ndescription: Use when: PDFs # note: x\nmetadata:\n  version: \"1\"\n---\n",
        "pdf",
    )
    .unwrap();
    assert_eq!((p.name.as_str(), p.description.as_str()), ("pdf", "Use when: PDFs"));
    assert_eq!(p.problems.len(), 1, "{:?}", p.problems);
    // A tab after the colon is valid YAML 1.2 that yaml-rust2 rejects.
    let p = parse("---\nname:\tpdf\ndescription:\tPDFs\n---\n", "pdf").unwrap();
    assert_eq!((p.name.as_str(), p.description.as_str()), ("pdf", "PDFs"));
}

#[test]
fn unusable_skills_are_errors() {
    for (text, why) in [
        ("no frontmatter", "frontmatter"),
        ("---\nname: a\ndescription: d\n", "frontmatter"),
        ("---\nname: a\n---\n", "description"),
        ("---\nname: a\ndescription: \"  \"\n---\n", "description"),
        ("---\nname: a\ndescription: [x, y]\n---\n", "must be a string"),
        ("---\nname: a\ndescription:\n  Use when: PDFs\n---\n", "must be a string"),
        ("---\nname: a\ndescription: d\ndisable-model-invocation: true\n---\n", "disable-model-invocation"),
        (
            "---\nname: a\ndescription: d\ndisable-model-invocation: \"True\"\n---\n",
            "disable-model-invocation",
        ),
        ("---\n- a\n- b\n---\n", "mapping"),
        ("---\n---\n", "mapping"),
        ("---\nname: a\ndescription: [unclosed\n---\n", "invalid YAML"),
        ("---\nname: a\nname: b\ndescription: d\n---\n", "invalid YAML"),
    ] {
        let err = parse(text, "a").unwrap_err();
        assert!(err.contains(why), "{text:?}: {err}");
    }
}

#[test]
fn yaml_aliases_are_refused() {
    let mut bomb =
        String::from("---\nname: a\ndescription: d\nx0: &a0 [lol, lol, lol, lol, lol, lol, lol, lol, lol]\n");
    for i in 1..30 {
        let refs = vec![format!("*a{}", i - 1); 9].join(", ");
        bomb.push_str(&format!("x{i}: &a{i} [{refs}]\n"));
    }
    bomb.push_str("---\n");
    assert!(parse(&bomb, "a").unwrap_err().contains("aliases"));
}

#[test]
fn deep_nesting_is_refused_without_overflowing_the_stack() {
    let deep = format!("---\nname: a\ndescription: d\nx:\n{}x\n---\n", "- ".repeat(100_000));
    assert!(parse(&deep, "a").unwrap_err().contains("nested"));
    let flow = format!("---\nname: a\ndescription: d\nx: {}{}\n---\n", "[".repeat(200), "]".repeat(200));
    assert!(parse(&flow, "a").is_err());
    // Two levels, as metadata uses, are fine.
    assert!(
        parse("---\nname: a\ndescription: d\nmetadata:\n  k: v\n---\n", "a").unwrap().problems.is_empty()
    );
}

#[test]
fn spec_violations_warn_but_load() {
    let cases = [
        ("PDF-Processing", "lowercase"),
        ("-pdf", "hyphen"),
        ("pdf-", "hyphen"),
        ("pdf--processing", "hyphen"),
        ("pdf_processing", "lowercase"),
    ];
    for (name, why) in cases {
        let p = problems(&format!("---\nname: {name}\ndescription: d\n---\n"), name);
        assert!(p.iter().any(|w| w.contains(why)), "{name}: {p:?}");
    }
    let long = "a".repeat(65);
    assert!(
        problems(&format!("---\nname: {long}\ndescription: d\n---\n"), &long)
            .iter()
            .any(|w| w.contains("64"))
    );
    assert!(problems("---\nname: a\ndescription: d\n---\n", "b").iter().any(|w| w.contains("directory")));
    let desc = "d".repeat(1025);
    assert!(
        problems(&format!("---\nname: a\ndescription: {desc}\n---\n"), "a")
            .iter()
            .any(|w| w.contains("1024"))
    );
    let compat = "c".repeat(501);
    assert!(
        problems(&format!("---\nname: a\ndescription: d\ncompatibility: {compat}\n---\n"), "a")
            .iter()
            .any(|w| w.contains("compatibility"))
    );
    assert!(
        problems("---\nname: a\ndescription: d\nmetadata:\n  version: 1.0\n---\n", "a")
            .iter()
            .any(|w| w.contains("metadata"))
    );
    assert!(
        problems("---\nname: a\ndescription: d\nlicense: [MIT]\n---\n", "a")
            .iter()
            .any(|w| w.contains("license"))
    );
    // A name YAML doesn't read as a string falls back to the directory, which keeps the spelling.
    let p = parse("---\nname: 007\ndescription: d\n---\n", "007").unwrap();
    assert_eq!(p.name, "007");
    assert!(p.problems.iter().any(|w| w.contains("quoted string")), "{:?}", p.problems);
    // A missing name falls back to the directory name.
    let p = parse("---\ndescription: d\n---\n", "from-dir").unwrap();
    assert_eq!(p.name, "from-dir");
    assert!(p.problems.iter().any(|w| w.contains("no name")));
    // Unicode lowercase letters are allowed.
    assert!(problems("---\nname: résumé\ndescription: d\n---\n", "résumé").is_empty());
}

#[test]
fn discovery_finds_skill_dirs_only_and_project_wins() {
    let d = tempdir("skill-roots");
    let (project, user) = (d.join("project"), d.join("user"));
    let mine = skill(&project, "fuzzing", "---\nname: fuzzing\ndescription: project fuzzing\n---\n");
    std::fs::create_dir_all(mine.parent().unwrap().join("scripts")).unwrap();
    std::fs::write(mine.parent().unwrap().join("scripts/run.sh"), "echo hi").unwrap();
    skill(&user, "fuzzing", "---\nname: fuzzing\ndescription: user fuzzing\n---\n");
    skill(&user, "slither", "---\nname: slither\ndescription: run slither\n---\n");
    skill(&user, ".hidden", "---\nname: hidden\ndescription: d\n---\n");
    skill(&user, "no-desc", "---\nname: no-desc\n---\n");
    std::fs::create_dir_all(user.join("empty/references")).unwrap();
    std::fs::write(user.join("README.md"), "not a skill").unwrap();
    std::fs::write(user.join("empty/references/x.md"), "x").unwrap();

    let mut warnings = Vec::new();
    let skills = discover(&[project.clone(), d.join("missing"), user.clone()], &mut warnings);
    let names: Vec<(&str, &str)> = skills.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect();
    assert_eq!(names, [("fuzzing", "project fuzzing"), ("slither", "run slither")]);
    assert_eq!(skills[0].location, mine);
    assert!(skills.iter().all(|s| s.location.is_absolute()));
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings.iter().any(|w| w.contains("already defined") && w.contains(&mine.display().to_string()))
    );
    assert!(warnings.iter().any(|w| w.contains("no-desc") && w.contains("description")));

    // A root reachable twice through a symlink is read once, without duplicate warnings, and a
    // shadowed skill doesn't report its spec problems.
    let link = d.join("link");
    std::os::unix::fs::symlink(&user, &link).unwrap();
    skill(&project, "bad", "---\nname: bad\ndescription: project\n---\n");
    skill(&user, "bad", "---\nname: bad\ndescription: user\nlicense: [x]\n---\n");
    let mut warnings = Vec::new();
    let skills = discover(&[project, user, link], &mut warnings);
    assert_eq!(skills.len(), 3);
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(!warnings.iter().any(|w| w.contains("license")));
}

#[test]
fn catalog_lists_locations_and_escapes() {
    assert_eq!(catalog(&[]), "");
    let s = Skill {
        name: "a".into(),
        description: "Find <bugs> &\n  misuse".into(),
        location: PathBuf::from("/x/a/SKILL.md"),
    };
    let c = catalog(&[s]);
    assert!(c.contains("relative to its directory"), "{c}");
    assert!(c.ends_with(
        "<available_skills>\n<skill>\n<name>a</name>\n<description>Find &lt;bugs&gt; &amp; misuse</description>\n\
         <location>/x/a/SKILL.md</location>\n</skill>\n</available_skills>"
    ));
}

#[test]
fn skill_text_cannot_fill_prompt_placeholders() {
    let d = tempdir("skill-placeholder");
    let root = d.join(".e/skills");
    skill(&root, "a", "---\nname: a\ndescription: \"{contents of .e/AUDIT.md} {date} {cwd}\"\n---\n");
    std::fs::write(d.join(".e/AUDIT.md"), "SECRET-BRIEF {skills}").unwrap();
    let (p, _) = e::prompt::build_with(&d, "2026-11-11", &[root]);
    assert!(p.contains("<description>{contents of .e/AUDIT.md} {date} {cwd}</description>"));
    assert_eq!(p.matches("SECRET-BRIEF").count(), 1);
    assert!(p.ends_with("SECRET-BRIEF {skills}\n</audit_brief>"));
}
