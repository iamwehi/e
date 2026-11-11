# e

A minimal security-audit agent harness. A small loop, four general tools
(`read`, `bash`, `edit`, `write`) and one audit tool (`finding`), a short
system prompt, and state in plain files under `.e/`. The model is DeepSeek.

`e` assumes it runs inside an external sandbox: it has no approvals,
permission prompts, command filtering or path restrictions.

## Usage

```bash
export DEEPSEEK_API_KEY=...
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
you set replaces the default (so `prices` replaces the whole table).

```json
{
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-flash",
  "context_budget": 300000,
  "bash_timeout_s": 120,
  "poc_timeout_s": 600,
  "max_tokens": null,
  "reasoning_effort": null,
  "peak_hours_utc": [[1, 4], [6, 10]],
  "prices": {
    "deepseek-flash": {"hit": 0.006, "miss": 0.3, "output": 1.2,
                       "off_peak": {"hit": 0.003, "miss": 0.15, "output": 0.6}}
  }
}
```

Prices are USD per 1M tokens. Peak hours are Monday to Friday, UTC. A model
without prices shows token counts instead of a cost.

## Checks

```bash
cargo test
```

```bash
cargo build --release && python3 scripts/pty_check.py
```
