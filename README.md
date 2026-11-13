# antigravity-status

A fast, configurable Rust status line renderer for Google Antigravity CLI (`agy`).

## Features

- **Context Usage Gauge**: Progress bar + percentage from `context_window.used_percentage` and `context_window_size`.
- **Session Input Tokens**: Cumulative input tokens (`context_window.total_input_tokens`).
- **Session Cache Read Tokens**: Session-level cache read tokens (aggregated from `transcript_path` when available, with deduplicated `current_usage` tracking fallback).
- **Session Cache Write Tokens**: Session-level cache creation tokens (deduplicated per-session from `current_usage.cache_creation_input_tokens`).
- **Session Output Tokens**: Cumulative output tokens (`context_window.total_output_tokens`).
- **Session Estimated Cost**: Session spend in USD (`cost.total_usd`, optional `subagent_usd`).
- **Per-Item `config.toml` Toggles**: Enable/disable any widget individually, customize labels, order, multi-line layout, formatting, and color thresholds with hot reload on every status refresh.

## Quick Start

### 1. Build

```bash
cargo build --release
```

### 2. Configure `config.toml`

```bash
mkdir -p ~/.config/antigravity-status
cp config.example.toml ~/.config/antigravity-status/config.toml
```

Config lookup order:
1. `--config <path>` CLI flag
2. `AGY_STATUS_CONFIG` environment variable
3. `<cwd>/.antigravity-status.toml` (where `cwd` comes from the `agy` payload or process directory)
4. `~/.config/antigravity-status/config.toml`

### 3. Register in Antigravity CLI

Inside `agy` TUI:

```text
/statusline /path/to/antigravity-status/target/release/antigravity-status
```

Or in `~/.gemini/antigravity-cli/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/path/to/antigravity-status/target/release/antigravity-status",
    "enabled": true,
    "padding": 0,
    "stack_with_default": false
  }
}
```
