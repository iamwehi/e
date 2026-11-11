//! Writing the prompt in `$VISUAL`/`$EDITOR`, opened from the line editor by
//! a configurable key (`editor_key`, Ctrl-X Ctrl-E by default as in bash).

use rustyline::{
    Cmd, ConditionalEventHandler, Event, EventContext, KeyCode, KeyEvent, Modifiers, RepeatCount,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Parses a key sequence such as `"ctrl-x ctrl-e"`, `"ctrl-g"`, `"alt-e"` or
/// `"f2"`. Empty: no key. A sequence of more than one key must start with
/// `ctrl-x`, the only prefix the line editor reads further keys after.
pub fn parse_keys(spec: &str) -> Result<Vec<KeyEvent>, String> {
    let keys = spec.split_whitespace().map(parse_key).collect::<Result<Vec<_>, _>>()?;
    if keys.len() > 1 && keys[0] != KeyEvent::ctrl('X') {
        return Err(format!("{spec:?}: a sequence of keys must start with ctrl-x"));
    }
    if let [k] = keys[..]
        && (k == KeyEvent::ctrl('C') || k == KeyEvent::ctrl('D'))
    {
        return Err(format!("{spec:?}: ctrl-c and ctrl-d are reserved"));
    }
    Ok(keys)
}

fn parse_key(tok: &str) -> Result<KeyEvent, String> {
    let bad = || format!("bad key {tok:?}: use e.g. ctrl-g, alt-e, f2 or ctrl-x ctrl-e");
    let lower = tok.to_ascii_lowercase();
    let (mut rest, mut mods) = (lower.as_str(), Modifiers::NONE);
    loop {
        if let Some(r) = rest.strip_prefix("ctrl-") {
            (rest, mods) = (r, mods | Modifiers::CTRL);
        } else if let Some(r) = rest.strip_prefix("alt-") {
            (rest, mods) = (r, mods | Modifiers::ALT);
        } else {
            break;
        }
    }
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_graphic() && !mods.is_empty() => {
            Ok(KeyEvent::normalize(KeyEvent(KeyCode::Char(c), mods)))
        }
        _ => match rest.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            Some(n @ 1..=12) => Ok(KeyEvent(KeyCode::F(n), mods)),
            _ => Err(bad()),
        },
    }
}

/// Binds `keys` to "accept the line and ask for the editor". The returned
/// flag is set when the key was pressed; the caller swaps it back to false.
pub fn bind<H: rustyline::Helper, I: rustyline::history::History>(
    ed: &mut rustyline::Editor<H, I>,
    keys: Vec<KeyEvent>,
) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if !keys.is_empty() {
        let handler = Box::new(OpenEditor(flag.clone()));
        ed.bind_sequence(Event::KeySeq(keys), rustyline::EventHandler::Conditional(handler));
    }
    flag
}

struct OpenEditor(Arc<AtomicBool>);

impl ConditionalEventHandler for OpenEditor {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, _: &EventContext) -> Option<Cmd> {
        self.0.store(true, Ordering::SeqCst);
        Some(Cmd::AcceptLine)
    }
}

/// `$VISUAL`, else `$EDITOR`, else `vi`.
pub fn editor_command() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".into())
}

/// Opens `text` in the editor command `cmd` (see [`editor_command`]) and
/// returns what was saved, trailing whitespace removed. `cmd` goes through
/// `sh` like git's, so `EDITOR="code --wait"` works. A non-zero exit (vim's
/// `:cq`) is an error.
pub fn edit(cmd: &str, text: &str) -> Result<String, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let path = std::env::temp_dir().join(format!("e-prompt-{}-{}.md", std::process::id(), nanos.as_nanos()));
    // create_new: never follow a link someone planted in a shared temp dir.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let wrote = f.write_all(text.as_bytes()).and_then(|_| f.flush());
    drop(f);
    let result = wrote.map_err(|e| format!("{}: {e}", path.display())).and_then(|_| {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{cmd} \"$1\""))
            .arg("e")
            .arg(&path)
            .status()
            .map_err(|e| format!("{cmd}: {e}"))?;
        if !status.success() {
            return Err(format!("{cmd} exited with {status}"));
        }
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
    });
    let _ = std::fs::remove_file(&path);
    Ok(result?.trim_end().to_string())
}
