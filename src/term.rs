//! Terminal output: sanitizing untrusted text, line-oriented printing and
//! restoring the terminal state on every exit path.

use crate::agent::Event;
use std::io::Write;
use std::sync::OnceLock;

#[derive(Clone, Copy, Default, PartialEq)]
enum St {
    #[default]
    Normal,
    Esc,
    Csi,
    /// OSC, DCS, SOS, PM, APC: runs until BEL or ST.
    Str,
    StrEsc,
}

/// Longest escape string we keep swallowing before giving up on it.
const MAX_SEQ: usize = 4096;

/// Strips control characters and escape sequences. Keeps `\n` and `\t`.
/// Stateful so a sequence split across stream chunks is still removed.
#[derive(Default)]
pub struct Sanitizer {
    st: St,
    len: usize,
}

impl Sanitizer {
    pub fn push(&mut self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        for c in input.chars() {
            self.len += 1;
            self.st = match self.st {
                St::Normal => {
                    self.len = 0;
                    match c {
                        '\x1b' => St::Esc,
                        '\u{9b}' => St::Csi,
                        '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => St::Str,
                        '\n' | '\t' => {
                            out.push(c);
                            St::Normal
                        }
                        // Bidi overrides and isolates can make text read differently than it is.
                        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => St::Normal,
                        c if c.is_control() => St::Normal,
                        c => {
                            out.push(c);
                            St::Normal
                        }
                    }
                }
                St::Esc => match c {
                    '[' => St::Csi,
                    ']' | 'P' | 'X' | '^' | '_' => St::Str,
                    '\x20'..='\x2f' => St::Esc,
                    _ => St::Normal,
                },
                St::Csi => match c {
                    '\x40'..='\x7e' => St::Normal,
                    '\x20'..='\x3f' if self.len < MAX_SEQ => St::Csi,
                    _ => St::Normal,
                },
                St::Str => match c {
                    '\x07' | '\u{9c}' => St::Normal,
                    '\x1b' => St::StrEsc,
                    _ if self.len >= MAX_SEQ => St::Normal,
                    _ => St::Str,
                },
                St::StrEsc => St::Normal,
            };
        }
        out
    }
}

pub fn sanitize(s: &str) -> String {
    Sanitizer::default().push(s)
}

pub fn isatty(fd: i32) -> bool {
    // SAFETY: plain syscall on a file descriptor number.
    unsafe { libc::isatty(fd) == 1 }
}

static SAVED: OnceLock<Option<libc::termios>> = OnceLock::new();

/// Remember the terminal state at startup, before anything changes it.
pub fn save_terminal() {
    SAVED.get_or_init(|| {
        if !isatty(0) {
            return None;
        }
        // SAFETY: termios is plain data; tcgetattr fills it in.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            (libc::tcgetattr(0, &mut t) == 0).then_some(t)
        }
    });
}

/// Put the terminal back into the state saved at startup.
pub fn restore_terminal() {
    if let Some(Some(t)) = SAVED.get() {
        // SAFETY: restores a termios struct obtained from tcgetattr.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
    }
}

/// One-line, sanitized, shortened tool arguments for the terminal.
pub fn summarize_args(name: &str, args: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let s = match name {
        "bash" => field("command"),
        "read" => match v.get("offset").and_then(|x| x.as_u64()) {
            Some(o) => format!("{}:{o}", field("path")),
            None => field("path"),
        },
        "edit" | "write" => field("path"),
        "finding" => format!("[{}] {}", field("severity"), field("title")),
        _ => args.to_string(),
    };
    let s = sanitize(&s).split_whitespace().collect::<Vec<_>>().join(" ");
    match s.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

/// Line-oriented printer for loop events. Plain text when not a TTY.
pub struct Ui {
    pub color: bool,
    pub verbose: bool,
    pub quiet: bool,
    stderr: bool,
    text: Sanitizer,
    reasoning: Sanitizer,
    at_line_start: bool,
    in_reasoning: bool,
}

impl Ui {
    pub fn new(stderr: bool, verbose: bool) -> Self {
        Ui {
            color: isatty(if stderr { 2 } else { 1 }),
            verbose,
            quiet: false,
            stderr,
            text: Sanitizer::default(),
            reasoning: Sanitizer::default(),
            at_line_start: true,
            in_reasoning: false,
        }
    }

    fn write(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.at_line_start = s.ends_with('\n');
        let _ = if self.stderr {
            let mut e = std::io::stderr().lock();
            e.write_all(s.as_bytes()).and_then(|_| e.flush())
        } else {
            let mut o = std::io::stdout().lock();
            o.write_all(s.as_bytes()).and_then(|_| o.flush())
        };
    }

    fn style(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    fn newline(&mut self) {
        if !self.at_line_start {
            self.write("\n");
        }
    }

    /// A message from the harness itself (already trusted text).
    pub fn notice(&mut self, msg: &str) {
        self.newline();
        let s = self.style("33", &format!("[{}]", sanitize(msg)));
        self.write(&format!("{s}\n"));
    }

    pub fn event(&mut self, ev: Event) {
        if self.quiet {
            return;
        }
        match ev {
            Event::Text(t) => {
                if self.in_reasoning {
                    self.in_reasoning = false;
                    self.newline();
                }
                let s = self.text.push(t);
                self.write(&s);
            }
            Event::Reasoning(t) if self.verbose => {
                self.in_reasoning = true;
                let s = self.reasoning.push(t);
                let s = self.style("2", &s);
                self.write(&s);
            }
            Event::Reasoning(_) => {}
            Event::TurnEnd => {
                self.text = Sanitizer::default();
                self.reasoning = Sanitizer::default();
                self.in_reasoning = false;
                self.newline();
            }
            Event::ToolStart { name, args, cost } => {
                self.newline();
                let line = format!("{cost} {name} {}", summarize_args(name, args));
                let s = self.style("2", &line);
                self.write(&s);
            }
            Event::ToolEnd { summary, content, is_error } => {
                let s = self.style(if is_error { "31" } else { "2" }, &format!(" -> {}", sanitize(summary)));
                self.write(&format!("{s}\n"));
                if self.verbose && !content.is_empty() {
                    let body = sanitize(content);
                    let s = self.style("2", body.trim_end());
                    self.write(&format!("{s}\n"));
                }
            }
            Event::Notice(n) => self.notice(n),
        }
    }
}
