# antigravity-status

A fast, configurable Rust status line renderer for Google Antigravity CLI (`agy`).

## Features

- **Agent Mode (`mode`)**: Active `cycle_mode` (`Mode: auto`, `Mode: plan`, `Mode: accept-edits`, etc.), placed first in the list by default.
- **Context Usage Gauge (`context_gauge`)**: Progress bar + percentage from `context_window.used_percentage` and `context_window_size`.
- **Session Input Tokens (`session_input_tokens`)**: Cumulative uncached session input tokens (aggregated from `transcript_path`, with deduplicated `current_usage` / `context_window.total_input_tokens` fallback).
- **Session Cache Read Tokens (`session_cache_read_tokens`)**: Session-level cache read tokens (aggregated from `transcript_path` when available, with deduplicated `current_usage` tracking fallback).
- **Session Cache Write Tokens (`session_cache_write_tokens`)**: Session-level cache creation tokens (deduplicated per-session from `current_usage.cache_creation_input_tokens`).
- **Session Output Tokens (`session_output_tokens`)**: Cumulative output tokens (aggregated from `transcript_path` and `context_window.total_output_tokens`).
- **Session Estimated TPS (`session_estimated_tps`)**: Approximate session-wide output throughput (`~XX.X t/s`), accumulated locally from `agent_state` (`"working"`) durations and output token increments (hidden until a turn completes).
- **Session Estimated Cost (`session_cost`)**: Session spend in USD (`cost.total_usd`, optional `subagent_usd`, with configurable local per-1M token pricing fallback when `cost` is omitted).
- **Model & Effort (`model_and_effort`)**: Active model name with inline reasoning effort (e.g., `Gemini 4 Hydrogen (High)`).
- **5h & Weekly Quota Gauges (`quota_5h_gauge`, `quota_weekly_gauge`)**: Remaining quota gauges automatically switched between `gemini-*` and `3p-*` buckets based on the active model.
- **SQLite History Persistence (`[storage]`)**: Records cross-session token deltas (`session`, `usage_event`) and quota window observations (`quota_window`) into `~/.local/share/antigravity-status/history.db` (WAL mode) for long-term quota equivalence analysis.
- **Right Alignment & `config.toml` Toggles**: Right-aligned by default using `terminal_width` from `agy`; enable/disable any widget individually, customize labels, order, multi-line layout, formatting, and color thresholds with hot reload on every status refresh.

## Installation

> Requires Rust `1.85+` (Edition 2024).

### Quick Install (Copy & Paste)

```bash
cargo install --git https://github.com/united-magic-association/antigravity-status.git && \
mkdir -p ~/.config/antigravity-status && \
curl -fsSL https://raw.githubusercontent.com/united-magic-association/antigravity-status/master/config.example.toml -o ~/.config/antigravity-status/config.toml
```

Then enable it inside `agy` TUI:

```text
/statusline ~/.cargo/bin/antigravity-status
```

### Build from Local Clone

```bash
git clone https://github.com/united-magic-association/antigravity-status.git
cd antigravity-status
cargo install --path .
mkdir -p ~/.config/antigravity-status
cp config.example.toml ~/.config/antigravity-status/config.toml
```

Or register directly in `~/.gemini/antigravity-cli/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.cargo/bin/antigravity-status",
    "enabled": true,
    "padding": 0,
    "stack_with_default": false
  }
}
```

## Configuration

Config lookup order:
1. `--config <path>` CLI flag
2. `AGY_STATUS_CONFIG` environment variable
3. `<cwd>/.antigravity-status.toml` (where `cwd` comes from the `agy` payload or process directory)
4. `~/.config/antigravity-status/config.toml`

Print the default template anytime with:

```bash
antigravity-status --print-default-config > ~/.config/antigravity-status/config.toml
```
