//! Append-only JSONL transcript under `.e/runs/<timestamp>/`, and UTC time helpers.

use serde_json::{Map, Value};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Transcript {
    file: Option<File>,
    /// Also print every event to stdout (`--json`).
    echo: bool,
    pub dir: PathBuf,
}

impl Transcript {
    pub fn create(cwd: &Path, echo: bool) -> std::io::Result<Self> {
        let t = civil(now_unix());
        let stamp =
            format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", t.year, t.month, t.day, t.hour, t.minute, t.second);
        let base = cwd.join(".e").join("runs");
        let mut dir = base.join(&stamp);
        let mut n = 1;
        while dir.exists() {
            dir = base.join(format!("{stamp}-{n}"));
            n += 1;
        }
        std::fs::create_dir_all(&dir)?;
        let file = File::options().create(true).append(true).open(dir.join("transcript.jsonl"))?;
        Ok(Transcript { file: Some(file), echo, dir })
    }

    /// Append one event. `fields` must be a JSON object. Write errors are
    /// reported once and then ignored: losing the log must not break a run.
    pub fn log(&mut self, kind: &str, fields: Value) {
        let mut obj = Map::new();
        obj.insert("ts".into(), Value::from(now_ms()));
        obj.insert("type".into(), Value::from(kind));
        if let Value::Object(m) = fields {
            obj.extend(m);
        }
        let line = Value::Object(obj).to_string();
        if let Some(f) = &mut self.file
            && let Err(e) = writeln!(f, "{line}")
        {
            eprintln!("e: transcript write failed, logging disabled: {e}");
            self.file = None;
        }
        if self.echo {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday.
    pub weekday: u32,
}

/// UTC calendar time from Unix seconds (Howard Hinnant's days-to-civil).
pub fn civil(unix: u64) -> Civil {
    let days = (unix / 86_400) as i64;
    let secs = (unix % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    Civil {
        year: yoe + era * 400 + i64::from(month <= 2),
        month,
        day,
        hour: secs / 3600,
        minute: secs / 60 % 60,
        second: secs % 60,
        weekday: ((days + 4).rem_euclid(7)) as u32,
    }
}

pub fn today() -> String {
    let t = civil(now_unix());
    format!("{:04}-{:02}-{:02}", t.year, t.month, t.day)
}
