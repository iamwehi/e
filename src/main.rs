//! `e`: interactive prompt and headless mode.

use e::agent::{RunEnd, Session};
use e::config::Config;
use e::exec::{self, Cancel};
use e::term::{self, Ui};
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const USAGE: &str = "usage: e [--model ID] [--verbose] [-p TASK [--json]]

  -p TASK      run one task headless and print the final message
  --json       with -p: stream transcript events to stdout as JSON lines
  --model ID   model to use (default from config)
  -v, --verbose  show reasoning and full tool output

Interactive commands: /model <id>, /new, /skills, /<skill> [task]. A TASK
may also start with /<skill>. Ctrl-X Ctrl-E (editor_key in the config) opens
the prompt in $VISUAL or $EDITOR. Ctrl-C cancels a run, Ctrl-D exits.
Provider: Venice by default (key in VENICE_API_KEY); any OpenAI-compatible
API via config. Config: $E_CONFIG or ~/.config/e/config.toml.";

#[derive(Default)]
struct Args {
    task: Option<String>,
    json: bool,
    verbose: bool,
    model: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-p" | "--print" => a.task = Some(it.next().ok_or("-p needs a task")?),
            "--json" => a.json = true,
            "-v" | "--verbose" => a.verbose = true,
            "--model" | "-m" => a.model = Some(it.next().ok_or("--model needs an id")?),
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if a.json && a.task.is_none() {
        return Err("--json needs -p".into());
    }
    Ok(a)
}

/// Built-in commands; they take precedence over skills of the same name.
const COMMANDS: [&str; 3] = ["model", "new", "skills"];
const HELP: &str = "commands: /model <id>, /new, /skills, /<skill> [task]";

/// Tab completion of `/command` and `/skill` names.
struct Commands(Vec<String>);

impl Commands {
    fn of(s: &Session) -> Self {
        let mut names: Vec<String> = COMMANDS.iter().map(|c| c.to_string()).collect();
        names.extend(s.skills.iter().map(|k| k.name.clone()).filter(|n| !COMMANDS.contains(&n.as_str())));
        Commands(names)
    }
}

impl Completer for Commands {
    type Candidate = String;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        match line[..pos].strip_prefix('/') {
            Some(word) if !word.contains(char::is_whitespace) => {
                Ok((1, self.0.iter().filter(|n| n.starts_with(word)).cloned().collect()))
            }
            _ => Ok((pos, Vec::new())),
        }
    }
}
impl Hinter for Commands {
    type Hint = String;
}
impl Highlighter for Commands {}
impl Validator for Commands {}
impl rustyline::Helper for Commands {}

/// `/name task` as the message that starts skill `name`: `None` if no skill has that name
/// (so `/src/Vault.sol is in scope` stays an ordinary task).
fn skill_task(s: &mut Session, line: &str) -> Option<Result<String, String>> {
    let cmd = line.strip_prefix('/')?;
    let (name, task) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
    s.skill_message(name, task)
}

/// The run Ctrl-C should cancel, if one is active.
static CURRENT: Mutex<Option<Cancel>> = Mutex::new(None);

fn set_current(c: Option<Cancel>) {
    if let Ok(mut cur) = CURRENT.lock() {
        *cur = c;
    }
}

fn exit_now(code: i32) -> ! {
    exec::kill_all_groups();
    term::restore_terminal();
    std::process::exit(code);
}

fn spawn_signal_handler() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt())?;
    let mut trm = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = int.recv() => {
                    let cur = CURRENT.lock().ok().and_then(|c| c.clone());
                    match cur {
                        // A second Ctrl-C while the first is still being handled forces exit.
                        Some(c) if c.is_cancelled() => exit_now(130),
                        Some(c) => c.cancel(),
                        None => {}
                    }
                }
                _ = trm.recv() => exit_now(143),
                _ = hup.recv() => exit_now(129),
            }
        }
    });
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("e: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(if e.is_empty() { 0 } else { 2 });
        }
    };
    term::save_terminal();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        exec::kill_all_groups();
        term::restore_terminal();
        default_hook(info);
    }));
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("e: {e}");
            return ExitCode::from(1);
        }
    };
    let code = rt.block_on(async_main(args));
    exec::kill_all_groups();
    term::restore_terminal();
    ExitCode::from(code)
}

async fn async_main(args: Args) -> u8 {
    if let Err(e) = spawn_signal_handler() {
        eprintln!("e: cannot install signal handlers: {e}");
        return 1;
    }
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("e: config: {e}");
            return 1;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("e: {e}");
            return 1;
        }
    };
    let model = args.model.clone().unwrap_or_else(|| cfg.model.clone());
    let session = match Session::new(cfg, model, cwd, args.json) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("e: {e}");
            return 1;
        }
    };
    match args.task {
        Some(task) => headless(session, &task, args.json, args.verbose).await,
        None => interactive(session, args.verbose).await,
    }
}

async fn headless(mut s: Session, task: &str, json: bool, verbose: bool) -> u8 {
    let mut ui = Ui::new(true, verbose);
    ui.quiet = json;
    for w in &s.warnings {
        if !json {
            ui.notice(&format!("warning: {w}"));
        }
    }
    let expanded = match skill_task(&mut s, task) {
        None => None,
        Some(Ok(msg)) => Some(msg),
        Some(Err(e)) => {
            eprintln!("e: skill: {}", term::sanitize(&e));
            return 1;
        }
    };
    let task = expanded.as_deref().unwrap_or(task);
    let cancel = Cancel::default();
    set_current(Some(cancel.clone()));
    let end = s.run(task, &cancel, &mut |ev| ui.event(ev)).await;
    set_current(None);
    match &end {
        RunEnd::Done(text) if !json => println!("{}", term::sanitize(text).trim_end()),
        RunEnd::Done(_) => {}
        RunEnd::Interrupted => eprintln!("e: interrupted"),
        RunEnd::Budget(m) | RunEnd::Error(m) => eprintln!("e: {}", term::sanitize(m)),
    }
    end.exit_code()
}

async fn interactive(mut s: Session, verbose: bool) -> u8 {
    let mut ui = Ui::new(false, verbose);
    // The line editor writes escape codes; use plain line reading unless both ends are a terminal.
    let mut editor = match term::isatty(0) && term::isatty(1) {
        false => None,
        true => match rustyline::Editor::<Commands, rustyline::history::DefaultHistory>::new() {
            Ok(mut e) => {
                e.set_helper(Some(Commands::of(&s)));
                Some(e)
            }
            Err(e) => {
                eprintln!("e: line editor: {e}");
                return 1;
            }
        },
    };
    // Set by editor_key: the line was accepted only to be opened in $EDITOR.
    let want_editor = match (editor.as_mut(), e::editor::parse_keys(&s.cfg.editor_key)) {
        (Some(ed), Ok(keys)) => e::editor::bind(ed, keys),
        _ => Arc::new(AtomicBool::new(false)),
    };
    // Text to put back at the prompt, from the editor.
    let mut draft: Option<String> = None;
    let greet = |ui: &mut Ui, s: &Session| {
        ui.notice(&format!("e · {} · {} · log {}", s.model, s.cwd.display(), s.transcript.dir.display()));
        for w in &s.warnings {
            ui.notice(&format!("warning: {w}"));
        }
        for k in s.skills.iter().filter(|k| COMMANDS.contains(&k.name.as_str())) {
            ui.notice(&format!(
                "warning: skill \"{}\" can't be started with /{0}: that is a command",
                k.name
            ));
        }
    };
    greet(&mut ui, &s);
    loop {
        let prompt = format!("{} {} > ", s.model, s.meter.display());
        let mut ed = editor.take();
        let initial = draft.take().unwrap_or_default();
        let Ok((ed, line)) = tokio::task::spawn_blocking(move || {
            let r = match ed.as_mut() {
                Some(e) => e.readline_with_initial(&prompt, (&initial, "")),
                None => plain_readline(&prompt),
            };
            (ed, r)
        })
        .await
        else {
            return 1;
        };
        editor = ed;
        if want_editor.swap(false, Ordering::SeqCst)
            && let Ok(text) = &line
        {
            let text = text.clone();
            draft = match tokio::task::spawn_blocking(move || {
                (e::editor::edit(&e::editor::editor_command(), &text), text)
            })
            .await
            {
                Ok((Ok(edited), _)) => Some(edited),
                Ok((Err(err), text)) => {
                    ui.notice(&format!("editor: {err}; prompt unchanged"));
                    Some(text)
                }
                Err(_) => return 1,
            };
            continue;
        }
        let line = match line {
            Ok(l) => l.trim().to_string(),
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return 0,
            Err(e) => {
                eprintln!("e: {e}");
                return 1;
            }
        };
        if let (Some(e), false) = (editor.as_mut(), line.is_empty()) {
            let _ = e.add_history_entry(line.as_str());
        }
        if line.is_empty() {
            continue;
        }
        let task = match line.strip_prefix('/') {
            None => line,
            Some(cmd) => {
                let (name, arg) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
                let task = match (name, arg.trim()) {
                    ("model", "") => {
                        ui.notice(&format!("model: {}", s.model));
                        None
                    }
                    ("model", id) if !id.contains(char::is_whitespace) => {
                        s.model = id.to_string();
                        if !s.cfg.prices.contains_key(id) {
                            ui.notice(&format!("no prices for {id}: showing token counts"));
                        }
                        None
                    }
                    ("new", "") => {
                        let (cfg, model, cwd) = (s.cfg.clone(), s.model.clone(), s.cwd.clone());
                        match Session::new(cfg, model, cwd, false) {
                            Ok(n) => {
                                s = n;
                                if let Some(e) = editor.as_mut() {
                                    e.set_helper(Some(Commands::of(&s)));
                                }
                                greet(&mut ui, &s);
                            }
                            Err(e) => ui.notice(&format!("error: {e}")),
                        }
                        None
                    }
                    ("skills", "") => {
                        list_skills(&mut ui, &s);
                        None
                    }
                    _ if COMMANDS.contains(&name) => {
                        ui.notice(HELP);
                        None
                    }
                    _ => match skill_task(&mut s, &line) {
                        Some(Ok(msg)) => {
                            ui.notice(&format!("skill: {name}"));
                            Some(msg)
                        }
                        Some(Err(e)) => {
                            ui.notice(&format!("error: {e}"));
                            None
                        }
                        None => {
                            ui.notice(HELP);
                            None
                        }
                    },
                };
                match task {
                    Some(task) => task,
                    None => continue,
                }
            }
        };
        let cancel = Cancel::default();
        set_current(Some(cancel.clone()));
        let end = s.run(&task, &cancel, &mut |ev| ui.event(ev)).await;
        set_current(None);
        match end {
            RunEnd::Done(_) => {}
            RunEnd::Interrupted => ui.notice("interrupted"),
            RunEnd::Budget(m) | RunEnd::Error(m) => ui.notice(&format!("error: {m}")),
        }
    }
}

fn list_skills(ui: &mut Ui, s: &Session) {
    if s.skills.is_empty() {
        ui.notice("no skills found");
    }
    for k in &s.skills {
        let mut desc = k.description.split_whitespace().collect::<Vec<_>>().join(" ");
        if desc.chars().count() > 100 {
            desc = desc.chars().take(99).chain(['…']).collect();
        }
        let only = if k.user_only { " (only with /name)" } else { "" };
        ui.notice(&format!("/{}{only}: {desc}", k.name));
    }
}

fn plain_readline(prompt: &str) -> Result<String, ReadlineError> {
    use std::io::{BufRead, Write};
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line)? {
        0 => Err(ReadlineError::Eof),
        _ => Ok(line),
    }
}
