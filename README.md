# e

A minimal security-audit agent harness. A small loop, four general tools
(`read`, `bash`, `edit`, `write`) and one audit tool (`finding`), a short
system prompt, and state in plain files under `.e/`. The default provider is
[Venice](https://venice.ai) with its DeepSeek V4.1 Flash model; any
OpenAI-compatible chat completions API (such as DeepSeek's own) works by
config.

`e` assumes it runs inside an external sandbox: it has no approvals,
permission prompts, command filtering or path restrictions. The sandbox does
not protect the API key, though, since `e` holds it inside: so `e` removes it
(and every variable in `scrub_env`) from the environment of each command the
agent runs, because those commands run the code under audit. An `api_key` in
the config file is still readable by anything that can read the file; keep
the file outside the sandbox or use the environment variable.

## Usage

```bash
export VENICE_API_KEY=...
cd path/to/target && e
```

```bash
e -p "Audit src/Vault.sol"            # headless: prints the final message
e -p "Audit src/Vault.sol" --json     # headless: transcript events as JSON lines
e -p "/reentrancy Audit src/Vault.sol" # start a skill, then the task
e -p "Audit src/" --rounds 5 --max-cost 10  # up to 5 sessions, at most $10
e verify                              # re-run every PoC on a clean copy
e triage --model deepseek-v4-pro      # a skeptical second opinion on each finding
e report > REPORT.md                  # the findings as Markdown
```

`--rounds N` keeps a headless audit going past one context window. When a
session runs out of room (the context budget is exhausted, or the model
stopped after the harness's 85% warning told it to write `.e/NOTES.md`), `e`
starts a new session that reads `NOTES.md` and continues, up to N sessions.
A session that finishes on its own ends the loop. stdout gets the last
session's final message; each session has its own run directory.

`--max-cost USD` (or `max_cost_usd` in the config) caps the spend of one `e`
process, across all its sessions, rounds and triage reviews. At 85% the model
is told to wrap up; once the cost reaches the cap, pending tool calls are not
run and `e` exits 3. It needs prices for the model, since otherwise the cost
is unknown: a model without prices fails with an error when a cap is set.

`e report` reads `.e/findings/` and prints one Markdown document: a summary,
a table, then each finding with its PoC command, result, verification and
triage verdict, and the last 40 lines of its latest PoC output. Proven
findings come first, then by severity. Findings in the same file and bug
class with overlapping lines are marked as possible duplicates of each other.
It needs no config or key and starts no run.

`e verify` and `e triage` are described under [Proofs of concept](#proofs-of-concept).

At the prompt: `/model <id>`, `/new`, `/skills`, `/<skill> [task]` (Tab completes names).
Ctrl-X Ctrl-E opens the prompt in `$VISUAL` (else `$EDITOR`, else `vi`); when
you save and quit, the text comes back to the prompt for Enter to send, and
quitting with an error (vim's `:cq`) leaves the prompt as it was. Ctrl-C
cancels a run, Ctrl-D exits.
`--verbose` shows reasoning and full tool output.

Exit codes: 0 done, 1 error, 2 context budget exhausted, 3 cost limit
reached, 4 (`e verify`) a PoC failed, 130 interrupted, 143 SIGTERM.

## Files (under `.e/` in the working directory)

| Path | Who writes it | What |
| --- | --- | --- |
| `AUDIT.md` | you | Scope, trust assumptions, known issues, build and test commands. Appended to the system prompt. |
| `NOTES.md` | the agent | Memory between runs. The harness never parses it. |
| `skills/<name>/` | you | Project [skills](#skills). |
| `runs/<timestamp>/` | harness | `transcript.jsonl`, plus `bash/` and `poc/` output logs. One per session. |
| `findings/<id>.json` | `finding` tool | Status is `proven` only if `poc_cmd` exited 0 when the harness ran it, with no tracked file changed. `e verify` and `e triage` add to it; `e report` renders them. |
| `findings/<id>.poc/` | `finding` tool | Copies of the new files (untracked, not ignored) present when the PoC ran. |
| `verify/<timestamp>/` | `e verify` | A log per PoC run. The clean copies are deleted after each run. |

## Proofs of concept

A finding's status is set by the harness, never by the model, and only to
`proven` when the harness itself ran `poc_cmd` and it exited 0. Three things
make that status worth more than an exit code:

**The code under audit must be unmodified.** In a git repository, `finding`
lists the tracked files that differ from `HEAD` (staged or not) before and
after the PoC runs. If any differ, the finding is `suspected` however the PoC
exited, and the model is told which files to restore. A PoC that passes
because the model planted the bug proves nothing. PoCs go in new files. If a
PoC must change an existing file, such as a test helper, list it in
`allow_modified` (a path, or a directory ending in `/`). Outside a git
repository nothing can be checked, and the finding says so.

**The PoC is kept.** The new files present when the PoC runs (untracked and
not git-ignored, so not `node_modules` or build output) are copied to
`.e/findings/<id>.poc/`: regular files only, at most 1 MB each and 8 MB or
500 files in all. Anyone can read exactly what was run.

**`e verify [ID...]` re-runs it on a clean copy.** For each finding with a
`poc_cmd` (all, or the IDs given), `e verify` copies the repository with
`tar` (leaving out `.e/`), resets tracked files to `HEAD`, deletes untracked
files but keeps ignored ones (installed dependencies, build caches), resets
submodules, adds back the finding's kept PoC files, and runs `poc_cmd` there.
The status becomes `proven` or `suspected` from that run, and the result is
stored under `verification`. This catches PoCs that depended on leftover
state, on files the finding didn't keep, or on modified code. An absolute
path to the working directory in `poc_cmd` is rewritten to the copy. It needs
no API key. Outside a git repository, or when `.git` is not a directory (a
worktree or submodule, whose shared git directory a reset would change),
PoCs run in place or not at all. Exit code 0 if every PoC passed, 4 if any
failed.

**`e triage [ID...]` asks for a second opinion.** False positives are the
main cost of an LLM audit. `e triage` starts a fresh session per finding with
its own system prompt: a skeptical reviewer who tries to refute the finding,
checks the code path, the PoC and the trust assumptions in `AUDIT.md`, and
ends with the `verdict` tool (`confirmed`, `refuted` or `uncertain`, a
reason, and optionally a different severity). It has `read`, `bash`, `edit`
and `write` but not `finding`, and it sees the finding's record (not the
auditor's transcript) and where its PoC files are. The verdict is stored
under `triage`; the status is left alone. A different, stronger model is a
good choice: `e triage --model deepseek-v4-pro`. `--max-cost` covers all the
reviews together.

The git commands `e` runs disable `core.fsmonitor` and hooks, which a
malicious repository could use to run commands, and get the same scrubbed
environment as the agent's commands.

## Skills

Skills follow the [Agent Skills](https://agentskills.io/specification) format, so skills
written for other agents work unchanged. A skill is a directory with a `SKILL.md` (YAML
frontmatter with `name` and `description`, then instructions) and any other files it needs,
conventionally under `scripts/`, `references/` and `assets/`.

`e` looks in these directories, first match wins on a name clash:

1. `.e/skills/` in the working directory
2. `~/.config/e/skills/` (`$XDG_CONFIG_HOME/e/skills/`)
3. `~/.agents/skills/`

To install a skill, copy or symlink its directory into one of them. The target's own
`.agents/skills/` and `.claude/skills/` are not read: they come with the code under audit,
which is untrusted.

The system prompt lists each skill's name, description and the absolute path of its
`SKILL.md`. The model reads a `SKILL.md` when the skill fits the task, and loads the files it
references on demand, resolving relative paths against the skill's directory. Skills are found
when a session starts (launch or `/new`); `/skills` lists them.

You can also start a skill yourself: `/<name> [task]`, at the prompt or as the start of a `-p`
task. `e` sends the `SKILL.md` instructions (read at that moment, frontmatter removed), the
skill's directory and a list of its files, then your task. Starting the same skill again in a
session refers back to it instead of repeating it. A skill with `disable-model-invocation:
true` is left out of the system prompt, so only you can start it. `/model`, `/new` and
`/skills` take precedence over skills of the same name. In a `-p` task, `/word` that names no
skill is sent as written, so a task can start with a path. Frontmatter that breaks the spec, such as a `name`
that doesn't match its directory, produces a warning. A skill without a description or with
frontmatter that can't be parsed is skipped with a warning.

## Config

`$E_CONFIG`, else `~/.config/e/config.toml` (`$XDG_CONFIG_HOME` is honoured).
Every key is optional; a key you set replaces the default whole (so `prices`
replaces the whole table). Unknown keys are errors. The defaults:

```toml
base_url = "https://api.venice.ai/api/v1"
model = "deepseek-v4-1-flash"
api_key_env = "VENICE_API_KEY"
# api_key = "..."            # fallback when $VENICE_API_KEY is unset
scrub_env = ["VENICE_API_KEY", "DEEPSEEK_API_KEY"]  # removed from commands, as is api_key_env
# max_cost_usd = 10.0        # unset: no spending cap
allow_modified = []          # tracked paths a PoC may change, e.g. ["test/", "foundry.toml"]
prompt_cache_key = true
prompt_cache_retention = "extended"
prompt_cache_retention_models = ["openai-gpt-52", "openai-gpt-54", "openai-gpt-55", "openai-gpt-55-pro"]
context_budget = 300_000
bash_timeout_s = 120
poc_timeout_s = 600
# max_tokens = 32_000        # unset: the provider's default
# reasoning_effort = "high"  # unset: not sent
peak_hours_utc = []
editor_key = "ctrl-x ctrl-e"  # e.g. "ctrl-g", "alt-e", "f2"; "" for none

[extra_body]
venice_parameters = { include_venice_system_prompt = false }

[prices.deepseek-v4-1-flash]
hit = 0.0075
miss = 0.3
output = 1.2
```

The built-in price table also covers Venice's other DeepSeek V4 models
(`deepseek-v4-1-flash*`, `deepseek-v4-flash*`, `deepseek-v4-pro*` and their
`e2ee-` variants). Prices are USD per 1M tokens: `hit` for prompt tokens read
from the cache, `miss` for the rest, `output` for completion tokens (reasoning
included), and an optional `write` for tokens written to the cache by models
that charge a premium for it (otherwise they cost `miss`). Extended-context
price tiers are not modelled. A model without prices shows token counts
instead of a cost; the table is keyed by model id only, so set `prices`
whenever you change `base_url`. Quote model ids that contain a dot:
`[prices."gpt-5.5"]`. A multi-key `editor_key` must start with `ctrl-x`
(the line editor reads further keys only after that prefix). `extra_body` is merged into every request; the default
stops Venice from adding its own system prompt. `prompt_cache_key` sends a
per-session key so requests reach a warm cache.

Prompt caching is the provider's: `e` only keeps every request a stable
prefix of the next. Caches expire after a few idle minutes (about 5 for
DeepSeek models on Venice), so a long build or PoC run can cost one uncached
turn. `prompt_cache_retention` is sent only for the model ids listed in
`prompt_cache_retention_models`. Venice forwards `"extended"` (24 hours) to
OpenAI and other models ignore it; OpenAI honours it only on GPT-5.5, 5.5 Pro,
5.4 and 5.2 among the models Venice serves (GPT-5.6 and later control cache
lifetime differently). It has no effect on the default model. Set
`prompt_cache_retention = ""` to never send it.

To use DeepSeek's own API instead:

```toml
base_url = "https://api.deepseek.com"
model = "deepseek-flash"
api_key_env = "DEEPSEEK_API_KEY"
extra_body = {}
prompt_cache_key = false
prompt_cache_retention = ""
peak_hours_utc = [[1, 4], [6, 10]]

[prices.deepseek-flash]
hit = 0.006
miss = 0.3
output = 1.2
off_peak = { hit = 0.003, miss = 0.15, output = 0.6 }

[prices.deepseek-v4-flash]
hit = 0.006
miss = 0.3
output = 1.2
off_peak = { hit = 0.003, miss = 0.15, output = 0.6 }

[prices.deepseek-v4-pro]
hit = 0.044
miss = 1.32
output = 3.96
off_peak = { hit = 0.022, miss = 0.66, output = 1.98 }
```

Peak hours are Monday to Friday, UTC; `off_peak` rates apply outside them.

## Checks

```bash
cargo test
```

```bash
cargo build --release && python3 scripts/pty_check.py
```

Every test runs against a scripted mock of the API on 127.0.0.1 and needs no
key or network. `tests/cli.rs` runs the `e` binary end to end (headless,
`--json`, the prompt over a pipe, config lookup, exit codes and signals);
`scripts/pty_check.py` covers what needs a real terminal.

CI (`.github/workflows/checks.yml`) runs both, and these, on every pull
request and tag:

```bash
cargo fmt --all --check
```

```bash
cargo clippy --locked --all-targets --all-features -- -D warnings
```

```bash
cargo deny --locked check
```

`deny.toml` holds the licence allow list and denies every RustSec advisory.
