//! Git checks for PoCs: which tracked files differ from HEAD, which files are
//! new, a copy of the new files kept with a finding, and a clean copy of the
//! repository for `e verify`.
//!
//! The repository is untrusted, and so is its `.git/config`: every git call
//! turns off fsmonitor and hooks (both run arbitrary commands) and drops the
//! scrubbed variables from git's environment.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A file larger than this is not copied into a PoC snapshot.
pub const SNAPSHOT_FILE_MAX: u64 = 1024 * 1024;
/// Total size and count of a PoC snapshot.
pub const SNAPSHOT_TOTAL_MAX: u64 = 8 * 1024 * 1024;
pub const SNAPSHOT_FILES_MAX: usize = 500;

pub fn git(cwd: &Path, scrub: &[&str], args: &[&str]) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.fsmonitor=", "-c", "core.hooksPath=/dev/null", "-c", "core.quotePath=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    for name in scrub {
        cmd.env_remove(name);
    }
    let out = cmd.output().map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {}: {}", args.join(" "), err.trim()));
    }
    Ok(out.stdout)
}

/// The repository root containing `cwd`, or `None` outside a work tree.
pub fn toplevel(cwd: &Path, scrub: &[&str]) -> Option<PathBuf> {
    let out = git(cwd, scrub, &["rev-parse", "--show-toplevel"]).ok()?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

fn paths(out: &[u8]) -> Vec<String> {
    out.split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        // The harness's own files are not part of the code under audit.
        .filter(|p| p != ".e" && !p.starts_with(".e/"))
        .collect()
}

/// The work tree relative to `cwd`, limited to `cwd`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// Tracked files that differ from HEAD (staged or not, deleted included).
    pub modified: Vec<String>,
    /// Files git doesn't track and doesn't ignore.
    pub untracked: Vec<String>,
}

/// `Ok(None)` when `cwd` is not in a git work tree.
pub fn state(cwd: &Path, scrub: &[&str]) -> Result<Option<State>, String> {
    if toplevel(cwd, scrub).is_none() {
        return Ok(None);
    }
    let modified = git(cwd, scrub, &["diff", "HEAD", "--name-only", "--relative", "--no-renames", "-z"])?;
    let untracked = git(cwd, scrub, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(Some(State { modified: paths(&modified), untracked: paths(&untracked) }))
}

/// The entries of `modified` that `allow` doesn't cover. An `allow` entry
/// ending in `/` covers a directory; any other entry covers that path and,
/// if it is a directory, everything under it.
pub fn disallowed(modified: &[String], allow: &[String]) -> Vec<String> {
    let covered = |p: &str| {
        allow.iter().any(|a| {
            let a = a.trim_start_matches("./");
            match a.strip_suffix('/') {
                Some(dir) => p.starts_with(&format!("{dir}/")),
                None => p == a || p.starts_with(&format!("{a}/")),
            }
        })
    };
    modified.iter().filter(|p| !covered(p)).cloned().collect()
}

/// Copy `files` (relative to `cwd`) into `dest`, replacing what was there.
/// Returns the copied files and the skipped ones, with reasons. Symbolic
/// links and special files are skipped: only regular files are copied.
pub fn snapshot(cwd: &Path, files: &[String], dest: &Path) -> Result<(Vec<Value>, Vec<Value>), String> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| format!("cannot remove {}: {e}", dest.display()))?;
    }
    let (mut copied, mut skipped, mut total) = (Vec::new(), Vec::new(), 0u64);
    for rel in files {
        let skip = |reason: &str| json!({"path": rel, "reason": reason});
        // git lists paths inside the tree; refuse anything that could leave it anyway.
        if Path::new(rel).is_absolute() || rel.split('/').any(|c| c == "..") {
            skipped.push(skip("path leaves the working directory"));
            continue;
        }
        let src = cwd.join(rel);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(e) => {
                skipped.push(skip(&format!("cannot stat: {e}")));
                continue;
            }
        };
        if !meta.is_file() {
            skipped.push(skip("not a regular file"));
            continue;
        }
        if meta.len() > SNAPSHOT_FILE_MAX {
            skipped.push(skip("larger than 1 MB"));
            continue;
        }
        if copied.len() >= SNAPSHOT_FILES_MAX || total + meta.len() > SNAPSHOT_TOTAL_MAX {
            skipped.push(skip("snapshot limit reached (500 files, 8 MB)"));
            continue;
        }
        let to = dest.join(rel);
        let r = to.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::copy(&src, &to));
        match r {
            Ok(n) => {
                total += n;
                copied.push(json!({"path": rel, "bytes": n}));
            }
            Err(e) => skipped.push(skip(&format!("cannot copy: {e}"))),
        }
    }
    Ok((copied, skipped))
}

/// Copy every regular file under `src` into `dest`, keeping relative paths.
pub fn restore(src: &Path, dest: &Path) -> std::io::Result<usize> {
    let mut n = 0;
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() {
                let rel = path.strip_prefix(src).unwrap_or(&path);
                let to = dest.join(rel);
                if let Some(p) = to.parent() {
                    std::fs::create_dir_all(p)?;
                }
                // A file of the clean tree at that path is replaced, not written through.
                if std::fs::symlink_metadata(&to).is_ok() {
                    std::fs::remove_file(&to)?;
                }
                std::fs::copy(&path, &to)?;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// A copy of the repository that contains `cwd`, with tracked files reset to
/// HEAD and untracked files removed (ignored files, such as installed
/// dependencies and build caches, are kept). `cwd`'s `.e/` is left out.
/// Returns the directory in the copy that corresponds to `cwd`.
pub fn clean_copy(cwd: &Path, dest: &Path, scrub: &[&str]) -> Result<PathBuf, String> {
    let top = toplevel(cwd, scrub).ok_or("not a git repository")?;
    let cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
    let top = top.canonicalize().map_err(|e| e.to_string())?;
    if !top.join(".git").is_dir() {
        // A worktree or submodule: .git is a file pointing at a shared git
        // directory, and resetting the copy would change the original's index.
        return Err(format!("{}/.git is not a directory (a worktree or submodule)", top.display()));
    }
    let rel = cwd.strip_prefix(&top).map_err(|_| "the working directory is outside the repository")?;
    std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let exclude = Path::new(".").join(rel).join(".e");
    let mut tar = Command::new("bash");
    tar.arg("-c")
        .arg(r#"set -o pipefail; tar -C "$1" --exclude="$3" -cf - . | tar -C "$2" -xf -"#)
        .arg("bash")
        .arg(&top)
        .arg(dest)
        .arg(&exclude)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in scrub {
        tar.env_remove(name);
    }
    let out = tar.output().map_err(|e| format!("cannot run tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "copying the repository failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    git(dest, scrub, &["reset", "-q", "--hard", "HEAD"])?;
    git(dest, scrub, &["clean", "-fdq"])?;
    // Best effort: submodules (such as Foundry's lib/) back to their recorded state.
    let _ = git(
        dest,
        scrub,
        &["submodule", "foreach", "--quiet", "--recursive", "git reset -q --hard && git clean -fdq"],
    );
    Ok(dest.join(rel))
}
