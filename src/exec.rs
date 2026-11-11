//! Command executor shared by `bash` and `finding`'s PoC runs.
//!
//! Each command runs as `bash -c` in its own session (so its own process
//! group, detached from the terminal), with stdin from /dev/null and stdout
//! and stderr both written straight to a log file. Nothing is buffered in
//! memory; the result carries the last [`TAIL_BYTES`] of the file. When the
//! shell exits, times out or is interrupted, the whole group is killed, so
//! nothing a command starts outlives the tool call.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const TAIL_BYTES: u64 = 30 * 1024;

/// A cancellation flag shared by the loop, the provider and the executor.
#[derive(Clone)]
pub struct Cancel(Arc<watch::Sender<bool>>);

impl Default for Cancel {
    fn default() -> Self {
        Cancel(Arc::new(watch::Sender::new(false)))
    }
}

impl Cancel {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once [`Cancel::cancel`] has been called.
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }
}

static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn kill_group(pgid: i32) {
    // SAFETY: plain syscall; a negative pid addresses the process group.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Kill every live command group. Called on SIGTERM/SIGHUP and at exit.
pub fn kill_all_groups() {
    if let Ok(groups) = GROUPS.lock() {
        for &g in groups.iter() {
            kill_group(g);
        }
    }
}

#[derive(Debug)]
pub struct Exec {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub interrupted: bool,
    pub duration: Duration,
    pub timeout: Duration,
    /// Last [`TAIL_BYTES`] of output, control sequences stripped.
    pub tail: String,
    pub total_bytes: u64,
    pub log_path: PathBuf,
}

impl Exec {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.interrupted
    }

    pub fn truncated(&self) -> bool {
        self.total_bytes > TAIL_BYTES
    }

    /// Short status for the terminal, e.g. `exit 0 · 1.2s`.
    pub fn status_line(&self) -> String {
        let secs = self.duration.as_secs_f64();
        if self.timed_out {
            format!("timeout after {}s", self.timeout.as_secs_f64())
        } else if self.interrupted {
            format!("interrupted · {secs:.1}s")
        } else {
            match self.exit_code {
                Some(c) => format!("exit {c} · {secs:.1}s"),
                None => format!("no exit code · {secs:.1}s"),
            }
        }
    }

    /// The tool result text the model sees.
    pub fn model_text(&self) -> String {
        let mut s = self.tail.clone();
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        let secs = self.duration.as_secs_f64();
        if self.timed_out {
            s += &format!("[timed out after {}s; process group killed]", self.timeout.as_secs_f64());
        } else if self.interrupted {
            s += &format!("[interrupted by the user after {secs:.1}s; process group killed]");
        } else {
            match self.exit_code {
                Some(c) => s += &format!("[exit code {c}, {secs:.1}s]"),
                None => s += &format!("[no exit code, {secs:.1}s]"),
            }
        }
        if self.truncated() {
            s += &format!(
                "\n[output truncated: showing the last {} of {} bytes; full output in {}]",
                TAIL_BYTES,
                self.total_bytes,
                self.log_path.display()
            );
        }
        s
    }
}

pub async fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    log_path: &Path,
    cancel: &Cancel,
) -> std::io::Result<Exec> {
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = File::create(log_path)?;
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let pgid = child.id().map(|p| p as i32);
    if let (Some(g), Ok(mut groups)) = (pgid, GROUPS.lock()) {
        groups.push(g);
    }

    let (mut timed_out, mut interrupted) = (false, false);
    let waited = tokio::select! {
        status = child.wait() => Some(status),
        _ = tokio::time::sleep(timeout) => { timed_out = true; None }
        _ = cancel.cancelled() => { interrupted = true; None }
    };
    if let Some(g) = pgid {
        kill_group(g);
    }
    let status = match waited {
        Some(s) => Some(s?),
        None => child.wait().await.ok(),
    };
    let duration = start.elapsed();
    if let (Some(g), Ok(mut groups)) = (pgid, GROUPS.lock()) {
        groups.retain(|&x| x != g);
    }

    let exit_code = status.and_then(|s| s.code().or_else(|| s.signal().map(|n| 128 + n)));
    let (tail, total_bytes) = read_tail(log_path, TAIL_BYTES)?;
    Ok(Exec {
        exit_code: if timed_out || interrupted { None } else { exit_code },
        timed_out,
        interrupted,
        duration,
        timeout,
        tail,
        total_bytes,
        log_path: log_path.to_path_buf(),
    })
}

/// Last `max` bytes of a file, starting after the first newline when cut.
fn read_tail(path: &Path, max: u64) -> std::io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    if start > 0
        && let Some(i) = buf.iter().position(|&b| b == b'\n')
        && i + 1 < buf.len()
    {
        buf.drain(..=i);
    }
    Ok((crate::term::sanitize(&String::from_utf8_lossy(&buf)), len))
}
