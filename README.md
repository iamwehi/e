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

`$E_CONFIG`, else `~/.config/e/config.json`. Every key is optional; a key
you set replaces the default (so `prices` replaces the whole table). The
defaults:

```json
{
  "base_url": "https://api.venice.ai/api/v1",
  "model": "deepseek-v4-1-flash",
  "api_key_env": "VENICE_API_KEY",
  "extra_body": {"venice_parameters": {"include_venice_system_prompt": false}},
  "prompt_cache_key": true,
  "prompt_cache_retention": "extended",
  "prompt_cache_retention_models": ["openai-gpt-5", "openai-gpt-6"],
  "context_budget": 300000,
  "bash_timeout_s": 120,
  "poc_timeout_s": 600,
  "max_tokens": null,
  "reasoning_effort": null,
  "peak_hours_utc": [],
  "prices": {
    "deepseek-v4-1-flash": {"hit": 0.0075, "miss": 0.3, "output": 1.2}
  }
}
```

The built-in price table also covers Venice's other DeepSeek V4 models
(`deepseek-v4-1-flash*`, `deepseek-v4-flash*`, `deepseek-v4-pro*` and their
`e2ee-` variants). Prices are USD per 1M tokens: `hit` for prompt tokens read
from the cache, `miss` for the rest, `output` for completion tokens (reasoning
included), and an optional `write` for tokens written to the cache by models
that charge a premium for it (otherwise they cost `miss`). Extended-context
price tiers are not modelled. A model without prices shows token counts
instead of a cost; the table is keyed by model id only, so set `prices`
whenever you change `base_url`. `extra_body` is merged into every
request; the default stops Venice from adding its own system prompt.
`prompt_cache_key` sends a per-session key so requests reach a warm cache.

Prompt caching is the provider's: `e` only keeps every request a stable
prefix of the next. Caches expire after a few idle minutes (about 5 for
DeepSeek models on Venice), so a long build or PoC run can cost one uncached
turn. `prompt_cache_retention` is sent only for models whose id starts with
one of `prompt_cache_retention_models`: Venice honours `"extended"` (24 hours)
only on the models it serves from OpenAI and ignores it elsewhere, so it has
no effect on the default model. Set `prompt_cache_retention` to `null` to
never send it.

To use DeepSeek's own API instead:

```json
{
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-flash",
  "api_key_env": "DEEPSEEK_API_KEY",
  "extra_body": {},
  "prompt_cache_key": false,
  "prompt_cache_retention": null,
  "peak_hours_utc": [[1, 4], [6, 10]],
  "prices": {
    "deepseek-flash": {"hit": 0.006, "miss": 0.3, "output": 1.2,
                       "off_peak": {"hit": 0.003, "miss": 0.15, "output": 0.6}},
    "deepseek-v4-flash": {"hit": 0.006, "miss": 0.3, "output": 1.2,
                          "off_peak": {"hit": 0.003, "miss": 0.15, "output": 0.6}},
    "deepseek-v4-pro": {"hit": 0.044, "miss": 1.32, "output": 3.96,
                        "off_peak": {"hit": 0.022, "miss": 0.66, "output": 1.98}}
  }
}
```

Peak hours are Monday to Friday, UTC; `off_peak` rates apply outside them.

## Checks

```bash
cargo test
```

```bash
cargo build --release && python3 scripts/pty_check.py
```
