//! `e`: interactive prompt and headless mode.

use e::agent::{RunEnd, Session};
use e::config::Config;
use e::exec::{self, Cancel};
use e::term::{self, Ui};
use rustyline::error::ReadlineError;
use std::process::ExitCode;
use std::sync::Mutex;

const USAGE: &str = "usage: e [--model ID] [--verbose] [-p TASK [--json]]

  -p TASK      run one task headless and print the final message
  --json       with -p: stream transcript events to stdout as JSON lines
  --model ID   model to use (default from config)
  -v, --verbose  show reasoning and full tool output

Interactive commands: /model <id>, /new. Ctrl-C cancels a run, Ctrl-D exits.
Config: $E_CONFIG or ~/.config/e/config.json. Key: DEEPSEEK_API_KEY.";

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
        true => match rustyline::DefaultEditor::new() {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("e: line editor: {e}");
                return 1;
            }
        },
    };
    let greet = |ui: &mut Ui, s: &Session| {
        ui.notice(&format!("e · {} · {} · log {}", s.model, s.cwd.display(), s.transcript.dir.display()));
        for w in &s.warnings {
            ui.notice(&format!("warning: {w}"));
        }
    };
    greet(&mut ui, &s);
    loop {
        let prompt = format!("{} {} > ", s.model, s.meter.display());
        let mut ed = editor.take();
        let Ok((ed, line)) = tokio::task::spawn_blocking(move || {
            let r = match ed.as_mut() {
                Some(e) => e.readline(&prompt),
                None => plain_readline(&prompt),
            };
            (ed, r)
        })
        .await
        else {
            return 1;
        };
        editor = ed;
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
        if line.starts_with('/') {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next(), parts.next()) {
                (Some("/model"), Some(id), None) => {
                    s.model = id.to_string();
                    if !s.cfg.prices.contains_key(id) {
                        ui.notice(&format!("no prices for {id}: showing token counts"));
                    }
                }
                (Some("/model"), None, _) => ui.notice(&format!("model: {}", s.model)),
                (Some("/new"), None, _) => {
                    let (cfg, model, cwd) = (s.cfg.clone(), s.model.clone(), s.cwd.clone());
                    match Session::new(cfg, model, cwd, false) {
                        Ok(n) => {
                            s = n;
                            greet(&mut ui, &s);
                        }
                        Err(e) => ui.notice(&format!("error: {e}")),
                    }
                }
                _ => ui.notice("commands: /model <id>, /new"),
            }
            continue;
        }
        let cancel = Cancel::default();
        set_current(Some(cancel.clone()));
        let end = s.run(&line, &cancel, &mut |ev| ui.event(ev)).await;
        set_current(None);
        match end {
            RunEnd::Done(_) => {}
            RunEnd::Interrupted => ui.notice("interrupted"),
            RunEnd::Budget(m) | RunEnd::Error(m) => ui.notice(&format!("error: {m}")),
        }
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
