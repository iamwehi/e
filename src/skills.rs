//! Agent Skills (<https://agentskills.io/specification>): discovery, `SKILL.md`
//! frontmatter, and the catalog the model sees.
//!
//! Only `name` and `description` go into the system prompt. The model activates a skill by
//! reading its `SKILL.md` with the `read` tool and loads bundled files (`scripts/`,
//! `references/`, `assets/`, ...) the same way, resolving relative paths against the skill
//! directory. The user activates one with `/name`, which sends its instructions ([`activate`]). Validation is lenient, as the client guide recommends: a spec violation that
//! doesn't stop a skill from working is a warning; a skill without a description or with
//! unparseable frontmatter is skipped.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use yaml_rust2::parser::Parser;
use yaml_rust2::{Event, Yaml, YamlLoader};

/// Larger `SKILL.md` files are skipped. The spec recommends under 500 lines.
const MAX_SKILL_MD: u64 = 1 << 20;
/// Deeper frontmatter is refused: the YAML loader recurses per level. `metadata` needs two.
const MAX_YAML_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Absolute path to `SKILL.md`. Its parent is the skill directory.
    pub location: PathBuf,
    /// `disable-model-invocation`: left out of the catalog, so only `/name` starts it.
    pub user_only: bool,
}

/// Skill roots, highest precedence first: `<cwd>/.e/skills`, then the user-level
/// `~/.config/e/skills` and `~/.agents/skills`. The target's own `.agents/skills` and
/// `.claude/skills` are deliberately not scanned: they ship with the code under audit, which
/// is untrusted, and would let it inject instructions into the auditor.
pub fn roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".e").join("skills")];
    roots.extend(crate::config::config_dir().map(|d| d.join("skills")));
    roots.extend(crate::config::home_dir().map(|h| h.join(".agents").join("skills")));
    roots
}

/// Every `<root>/<dir>/SKILL.md`, sorted by name. On a name clash the earlier root wins (then
/// the earlier directory name), and the shadowed skill is reported in `warnings`. A file seen
/// before through a symlink is skipped silently.
pub fn discover(roots: &[PathBuf], warnings: &mut Vec<String>) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let path = dir.join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            let location = std::path::absolute(&path).unwrap_or(path);
            if !seen.insert(std::fs::canonicalize(&location).unwrap_or_else(|_| location.clone())) {
                continue;
            }
            let shown = location.display();
            let text = match std::fs::metadata(&location) {
                Ok(m) if m.len() > MAX_SKILL_MD => {
                    warnings.push(format!("{shown}: skipped: larger than {MAX_SKILL_MD} bytes"));
                    continue;
                }
                _ => match std::fs::read_to_string(&location) {
                    Ok(text) => text,
                    Err(e) => {
                        warnings.push(format!("{shown}: skipped: {e}"));
                        continue;
                    }
                },
            };
            let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let parsed = match parse(&text, &dir_name) {
                Ok(parsed) => parsed,
                Err(e) => {
                    warnings.push(format!("{shown}: skipped: {e}"));
                    continue;
                }
            };
            if let Some(first) = skills.iter().find(|s| s.name == parsed.name) {
                warnings.push(format!(
                    "{shown}: skipped: skill \"{}\" is already defined by {}",
                    parsed.name,
                    first.location.display()
                ));
                continue;
            }
            warnings.extend(parsed.problems.iter().map(|p| format!("{shown}: {p}")));
            let Parsed { name, description, user_only, .. } = parsed;
            skills.push(Skill { name, description, location, user_only });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// A parsed `SKILL.md`: the fields `e` uses, and spec violations that don't prevent loading.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub name: String,
    pub description: String,
    pub user_only: bool,
    pub problems: Vec<String>,
}

/// Parse `SKILL.md` text from the directory `dir_name`. Errors mean the skill is unusable.
pub fn parse(text: &str, dir_name: &str) -> Result<Parsed, String> {
    let (yaml, _) = split(text).ok_or("no YAML frontmatter (a leading `---` block)")?;
    let mut problems = Vec::new();
    let map = match load_yaml(yaml) {
        Ok(map) => map,
        // Skills written for other clients often have unquoted colons in values, which
        // their parsers accept. Retry with those values quoted.
        Err(e) => match load_yaml(&quote_plain_values(yaml)) {
            Ok(map) => {
                problems.push(format!("invalid YAML frontmatter ({e}); loaded after quoting values"));
                map
            }
            Err(_) => return Err(format!("invalid YAML frontmatter: {e}")),
        },
    };
    let Yaml::Hash(map) = map else { return Err("frontmatter is not a mapping".into()) };
    let field = |k: &str| map.get(&Yaml::String(k.into()));

    let description = match field("description") {
        None | Some(Yaml::Null) => return Err("frontmatter has no description".into()),
        Some(d) => match scalar(d) {
            Some(d) if !d.trim().is_empty() => d.trim().to_string(),
            Some(_) => return Err("description is empty".into()),
            None => return Err("description must be a string (quote it if it contains \": \")".into()),
        },
    };
    // Skills only the user may start, with `/name`. The client guide says to hide them from
    // the model entirely.
    let user_only = match field("disable-model-invocation") {
        Some(Yaml::Boolean(b)) => *b,
        Some(Yaml::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    };
    let name = match field("name") {
        Some(Yaml::String(n)) if !n.trim().is_empty() => n.trim().to_string(),
        None | Some(Yaml::Null) | Some(Yaml::String(_)) => {
            problems.push(format!("frontmatter has no name; using the directory name \"{dir_name}\""));
            dir_name.to_string()
        }
        // Unquoted `007` reads as the integer 7; the directory keeps the spelling.
        Some(_) => {
            problems.push(format!("name must be a quoted string; using the directory name \"{dir_name}\""));
            dir_name.to_string()
        }
    };

    let len = name.chars().count();
    if len > 64 {
        problems.push(format!("name is {len} characters; the maximum is 64"));
    }
    if !name.chars().all(|c| c == '-' || (c.is_alphanumeric() && !c.is_uppercase())) {
        problems.push(format!("name \"{name}\" may only contain lowercase letters, digits and hyphens"));
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        problems.push(format!(
            "name \"{name}\" must not start or end with a hyphen or contain consecutive hyphens"
        ));
    }
    if name != dir_name {
        problems.push(format!("name \"{name}\" does not match its directory \"{dir_name}\""));
    }
    let len = description.chars().count();
    if len > 1024 {
        problems.push(format!("description is {len} characters; the maximum is 1024"));
    }
    for key in ["license", "compatibility", "allowed-tools"] {
        match field(key) {
            None => {}
            Some(Yaml::String(s)) if key == "compatibility" => {
                let len = s.trim().chars().count();
                if len == 0 || len > 500 {
                    problems.push(format!("compatibility is {len} characters; it must be 1 to 500"));
                }
            }
            Some(Yaml::String(_)) => {}
            Some(_) => problems.push(format!("{key} must be a string")),
        }
    }
    match field("metadata") {
        None => {}
        Some(Yaml::Hash(m)) if m.iter().all(|(k, v)| k.as_str().is_some() && v.as_str().is_some()) => {}
        Some(_) => problems.push("metadata must map string keys to string values".into()),
    }
    Ok(Parsed { name, description, user_only, problems })
}

/// The skills section of the system prompt: how to use skills, then the catalog of skills the
/// model may start. Empty when there are none, so the model isn't told about a feature it
/// can't use.
pub fn catalog(skills: &[Skill]) -> String {
    let skills: Vec<&Skill> = skills.iter().filter(|s| !s.user_only).collect();
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Skills are directories of instructions, scripts and
reference files for bug classes and tools. When a skill's
description matches your task, read its SKILL.md (at the
listed location) before you start. Paths in a skill are
relative to its directory, the parent of SKILL.md, which
some skills call {baseDir}. read and bash work in the
working directory, so pass them absolute paths.

<available_skills>\n",
    );
    for s in skills {
        out.push_str(&format!(
            "<skill>\n<name>{}</name>\n<description>{}</description>\n<location>{}</location>\n</skill>\n",
            xml_escape(&s.name),
            xml_escape(&s.description.split_whitespace().collect::<Vec<_>>().join(" ")),
            xml_escape(&s.location.display().to_string()),
        ));
    }
    out.push_str("</available_skills>");
    out
}

/// At most this many bundled files are listed when a skill is activated.
const MAX_RESOURCES: usize = 50;

/// The message that activates `skill` for the user's `/name`: the `SKILL.md` body, read now
/// so edits since startup count, then the skill directory and its bundled files (listed, not
/// read; the model loads what the instructions call for).
pub fn activate(skill: &Skill) -> Result<String, String> {
    let shown = skill.location.display();
    match std::fs::metadata(&skill.location) {
        Ok(m) if m.len() > MAX_SKILL_MD => return Err(format!("{shown}: larger than {MAX_SKILL_MD} bytes")),
        Ok(_) => {}
        Err(e) => return Err(format!("{shown}: {e}")),
    }
    let text = std::fs::read_to_string(&skill.location).map_err(|e| format!("{shown}: {e}"))?;
    let (_, body) = split(&text).ok_or_else(|| format!("{shown}: no YAML frontmatter"))?;
    let dir = skill.location.parent().unwrap_or(Path::new("/"));
    let mut files = Vec::new();
    let more = list_files(dir, Path::new(""), 0, &mut files);
    let mut out = format!(
        "<skill_content name=\"{}\">\n{}\n\nSkill directory: {}\nRelative paths in this skill are relative to the skill directory.\n",
        xml_escape(&skill.name).replace('"', "&quot;"),
        body.trim(),
        dir.display(),
    );
    if !files.is_empty() {
        out.push_str("\n<skill_resources>\n");
        for f in &files {
            out.push_str(&format!("<file>{}</file>\n", xml_escape(&f.display().to_string())));
        }
        if more {
            out.push_str(&format!("(more files not listed; only the first {MAX_RESOURCES} are)\n"));
        }
        out.push_str("</skill_resources>\n");
    }
    out.push_str("</skill_content>");
    Ok(out)
}

/// Add files under `dir` to `files` as paths relative to the skill directory, sorted, skipping
/// hidden entries and the top-level `SKILL.md`, at most four levels deep. True if the listing
/// hit [`MAX_RESOURCES`].
fn list_files(dir: &Path, rel: &Path, depth: usize, files: &mut Vec<PathBuf>) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut entries: Vec<_> =
        entries.flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.')).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let (path, rel) = (e.path(), rel.join(e.file_name()));
        if path.is_dir() {
            if depth < 3 && list_files(&path, &rel, depth + 1, files) {
                return true;
            }
        } else if rel != Path::new("SKILL.md") {
            if files.len() == MAX_RESOURCES {
                return true;
            }
            files.push(rel);
        }
    }
    false
}

/// The YAML between a first line of `---` and the next `---` line, and the body after it.
fn split(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut end = first.len();
    for line in lines {
        if line.trim_end() == "---" {
            return Some((&text[first.len()..end], &text[end + line.len()..]));
        }
        end += line.len();
    }
    None
}

/// The YAML document in `src`. A first pass over the parser's events, which doesn't recurse,
/// refuses what the recursive loader can't take: nesting deeper than [`MAX_YAML_DEPTH`] (a
/// stack overflow) and aliases (copied on expansion, so a few nested ones take exponential
/// memory). Frontmatter needs neither.
fn load_yaml(src: &str) -> Result<Yaml, String> {
    let mut parser = Parser::new_from_str(src);
    let mut depth = 0usize;
    loop {
        match parser.next_token().map_err(|e| e.to_string())?.0 {
            Event::StreamEnd => break,
            Event::Alias(_) => return Err("YAML aliases are not supported".into()),
            Event::SequenceStart(..) | Event::MappingStart(..) => {
                depth += 1;
                if depth > MAX_YAML_DEPTH {
                    return Err(format!("YAML nested deeper than {MAX_YAML_DEPTH} levels"));
                }
            }
            Event::SequenceEnd | Event::MappingEnd => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let docs = YamlLoader::load_from_str(src).map_err(|e| e.to_string())?;
    Ok(docs.into_iter().next().unwrap_or(Yaml::Null))
}

/// Rewrite top-level plain values that other clients' YAML parsers accept and yaml-rust2
/// doesn't, as single-quoted scalars: ones containing `: ` (`description: Use when: x`),
/// and ones after a tab (`name:\tx`). Indented continuation lines are folded in, as YAML
/// folds a plain scalar, and comments are dropped.
fn quote_plain_values(yaml: &str) -> String {
    let lines: Vec<&str> = yaml.split_inclusive('\n').collect();
    let mut out = String::with_capacity(yaml.len() + 16);
    let mut i = 0;
    while i < lines.len() {
        let mut end = i + 1;
        while end < lines.len() && lines[end].starts_with([' ', '\t']) && !lines[end].trim().is_empty() {
            end += 1;
        }
        match requote(&lines[i..end]) {
            Some(fixed) => {
                out.push_str(&fixed);
                i = end;
            }
            None => {
                out.push_str(lines[i]);
                i += 1;
            }
        }
    }
    out
}

/// A `key: value` line and its continuation lines as `key: 'value'`, if it needs quoting.
fn requote(group: &[&str]) -> Option<String> {
    let (key, value) = group[0].split_once(':')?;
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    let tab = value.starts_with('\t');
    let value = strip_comment(value.strip_prefix([' ', '\t'])?).trim();
    if value.is_empty()
        || value.starts_with(['"', '\'', '|', '>', '[', '{', '&', '*', '!', '#', '%', '@', '`'])
    {
        return None;
    }
    let pieces: Vec<&str> = std::iter::once(value)
        .chain(group[1..].iter().map(|l| strip_comment(l).trim()))
        .filter(|p| !p.is_empty())
        .collect();
    let colon = |p: &&str| p.contains(": ") || p.contains(":\t") || p.ends_with(':');
    if !tab && !pieces.iter().any(colon) {
        return None;
    }
    let last = group[group.len() - 1];
    let eol = &last[last.trim_end_matches(['\n', '\r']).len()..];
    Some(format!("{key}: '{}'{eol}", pieces.join(" ").replace('\'', "''")))
}

/// `s` up to a YAML comment: a `#` after a space or tab.
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    (1..b.len()).find(|&i| b[i] == b'#' && matches!(b[i - 1], b' ' | b'\t')).map_or(s, |i| &s[..i - 1])
}

/// A scalar as text. YAML reads unquoted `1.0` or `true` as non-strings.
fn scalar(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
