//! Agent Skills (<https://agentskills.io/specification>): discovery, `SKILL.md`
//! frontmatter, and the catalog the model sees.
//!
//! Only `name` and `description` go into the system prompt. The model activates a skill by
//! reading its `SKILL.md` with the `read` tool and loads bundled files (`scripts/`,
//! `references/`, `assets/`, ...) the same way, resolving relative paths against the skill
//! directory. Validation is lenient, as the client guide recommends: a spec violation that
//! doesn't stop a skill from working is a warning; a skill without a description or with
//! unparseable frontmatter is skipped.

use std::path::{Path, PathBuf};
use yaml_rust2::parser::{MarkedEventReceiver, Parser};
use yaml_rust2::scanner::Marker;
use yaml_rust2::{Event, Yaml, YamlLoader};

/// Larger `SKILL.md` files are skipped. The spec recommends under 500 lines.
const MAX_SKILL_MD: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Absolute path to `SKILL.md`. Its parent is the skill directory.
    pub location: PathBuf,
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
/// the earlier directory name), and the shadowed skill is reported in `warnings`.
pub fn discover(roots: &[PathBuf], warnings: &mut Vec<String>) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
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
            let (name, description) = match parse(&text, &dir_name) {
                Ok(parsed) => {
                    warnings.extend(parsed.problems.iter().map(|p| format!("{shown}: {p}")));
                    (parsed.name, parsed.description)
                }
                Err(e) => {
                    warnings.push(format!("{shown}: skipped: {e}"));
                    continue;
                }
            };
            if let Some(first) = skills.iter().find(|s| s.name == name) {
                warnings.push(format!(
                    "{shown}: skipped: skill \"{name}\" is already defined by {}",
                    first.location.display()
                ));
                continue;
            }
            skills.push(Skill { name, description, location });
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
    pub problems: Vec<String>,
}

/// Parse `SKILL.md` text from the directory `dir_name`. Errors mean the skill is unusable.
pub fn parse(text: &str, dir_name: &str) -> Result<Parsed, String> {
    let yaml = split(text).ok_or("no YAML frontmatter (a leading `---` block)")?;
    let mut problems = Vec::new();
    let map = match load_yaml(yaml) {
        Ok(map) => map,
        // Skills written for other clients often have unquoted colons in values, which
        // their parsers accept. Retry with those values quoted.
        Err(e) => match load_yaml(&quote_colon_values(yaml)) {
            Ok(map) => {
                problems.push(format!("invalid YAML frontmatter ({e}); loaded after quoting values"));
                map
            }
            Err(_) => return Err(format!("invalid YAML frontmatter: {e}")),
        },
    };
    let Yaml::Hash(map) = map else { return Err("frontmatter is not a mapping".into()) };
    let field = |k: &str| map.get(&Yaml::String(k.into()));

    let description = match field("description").and_then(scalar) {
        Some(d) if !d.trim().is_empty() => d.trim().to_string(),
        _ => return Err("frontmatter has no description".into()),
    };
    let name = match field("name").and_then(scalar) {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => {
            problems.push(format!("frontmatter has no name; using the directory name \"{dir_name}\""));
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
    Ok(Parsed { name, description, problems })
}

/// The skills section of the system prompt: how to use skills, then the catalog. Empty when
/// there are no skills, so the model isn't told about a feature it can't use.
pub fn catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Skills are directories of instructions, scripts and
reference files for bug classes and tools. When a skill's
description matches your task, read its SKILL.md (at the
listed location) before you start. Paths in a skill are
relative to its directory, the parent of SKILL.md: read
and bash work in the working directory, so pass absolute
paths.

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

/// The YAML between a first line of `---` and the next `---` line.
fn split(text: &str) -> Option<&str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut end = first.len();
    for line in lines {
        if line.trim_end() == "---" {
            return Some(&text[first.len()..end]);
        }
        end += line.len();
    }
    None
}

/// The YAML document in `src`. Aliases are refused: the loader expands them by copying, so a
/// few nested ones would take exponential memory, and frontmatter has no use for them.
fn load_yaml(src: &str) -> Result<Yaml, String> {
    struct Aliases(bool);
    impl MarkedEventReceiver for Aliases {
        fn on_event(&mut self, ev: Event, _: Marker) {
            self.0 |= matches!(ev, Event::Alias(_));
        }
    }
    let mut aliases = Aliases(false);
    Parser::new_from_str(src).load(&mut aliases, true).map_err(|e| e.to_string())?;
    if aliases.0 {
        return Err("YAML aliases are not supported".into());
    }
    let docs = YamlLoader::load_from_str(src).map_err(|e| e.to_string())?;
    Ok(docs.into_iter().next().unwrap_or(Yaml::Null))
}

/// Single-quote plain top-level values that contain `: `, such as
/// `description: Use when: the user asks`.
fn quote_colon_values(yaml: &str) -> String {
    let mut out = String::with_capacity(yaml.len() + 16);
    for line in yaml.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let eol = &line[content.len()..];
        let fixed = content.split_once(':').and_then(|(key, value)| {
            let plain_key =
                !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            let value = value.strip_prefix([' ', '\t'])?.trim();
            let plain_value =
                !value.is_empty() && !value.starts_with(['"', '\'', '|', '>', '[', '{', '&', '*', '!', '#']);
            (plain_key && plain_value && (value.contains(": ") || value.ends_with(':')))
                .then(|| format!("{key}: '{}'", value.replace('\'', "''")))
        });
        out.push_str(fixed.as_deref().unwrap_or(content));
        out.push_str(eol);
    }
    out
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
