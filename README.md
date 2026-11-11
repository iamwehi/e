# e

A minimal security-audit agent harness. A small loop, four general tools
(`read`, `bash`, `edit`, `write`) and one audit tool (`finding`), a short
system prompt, and state in plain files under `.e/`. The default provider is
[Venice](https://venice.ai) with its DeepSeek V4.1 Flash model; any
OpenAI-compatible chat completions API (such as DeepSeek's own) works by
config.

`e` assumes it runs inside an external sandbox: it has no approvals,
permission prompts, command filtering or path restrictions.

## Usage

```bash
export VENICE_API_KEY=...
cd path/to/target && e
```

```bash
e -p "Audit src/Vault.sol"            # headless: prints the final message
e -p "Audit src/Vault.sol" --json     # headless: transcript events as JSON lines
```

At the prompt: `/model <id>`, `/new`. Ctrl-C cancels a run, Ctrl-D exits.
`--verbose` shows reasoning and full tool output.

Exit codes: 0 done, 1 error, 2 context budget exhausted, 130 interrupted,
143 SIGTERM.

## Files (under `.e/` in the working directory)

| Path | Who writes it | What |
| --- | --- | --- |
| `AUDIT.md` | you | Scope, trust assumptions, known issues, build and test commands. Appended to the system prompt. |
| `NOTES.md` | the agent | Memory between runs. The harness never parses it. |
| `skills/<name>/SKILL.md` | you | Frontmatter `name` and `description`. Also read from `~/.config/e/skills`. |
| `runs/<timestamp>/` | harness | `transcript.jsonl`, plus `bash/` and `poc/` output logs. One per session. |
| `findings/<id>.json` | `finding` tool | Status is `proven` only if `poc_cmd` exited 0 when the harness ran it. |

## Config

`$E_CONFIG`, else `~/.config/e/config.toml` (`$XDG_CONFIG_HOME` is honoured).
Every key is optional; a key you set replaces the default whole (so `prices`
replaces the whole table). Unknown keys are errors. The defaults:

```toml
base_url = "https://api.venice.ai/api/v1"
model = "deepseek-v4-1-flash"
api_key_env = "VENICE_API_KEY"
# api_key = "..."            # fallback when $VENICE_API_KEY is unset
prompt_cache_key = true
prompt_cache_retention = "extended"
prompt_cache_retention_models = ["openai-gpt-52", "openai-gpt-54", "openai-gpt-55", "openai-gpt-55-pro"]
context_budget = 300_000
bash_timeout_s = 120
poc_timeout_s = 600
# max_tokens = 32_000        # unset: the provider's default
# reasoning_effort = "high"  # unset: not sent
peak_hours_utc = []

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
`[prices."gpt-5.5"]`. `extra_body` is merged into every request; the default
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
