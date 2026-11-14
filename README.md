# antigravity-status

A fast, configurable Rust status line renderer for Google Antigravity CLI (`agy`).

## Features

- **Agent Mode (`mode`)**: Active `cycle_mode` (`Mode: auto`, `Mode: plan`, `Mode: accept-edits`, etc.), placed first in the list by default.
- **Context Usage Gauge (`context_gauge`)**: Progress bar + percentage from `context_window.used_percentage` and `context_window_size`.
- **Session Input Tokens (`session_input_tokens`)**: Cumulative uncached session input tokens (aggregated from `transcript_path`, with deduplicated `current_usage` / `context_window.total_input_tokens` fallback).
- **Session Cache Read Tokens (`session_cache_read_tokens`)**: Session-level cache read tokens (aggregated from `transcript_path` when available, with deduplicated `current_usage` tracking fallback).
- **Session Cache Write Tokens (`session_cache_write_tokens`)**: Session-level cache creation tokens (aggregated from `transcript_path` when available, with deduplicated `current_usage` tracking fallback; set to `enabled = false` in [config.example.toml](config.example.toml) because upstream `agy` does not currently populate cache creation fields).
- **Session Output Tokens (`session_output_tokens`)**: Cumulative output tokens (aggregated from `transcript_path` and `context_window.total_output_tokens`).
- **Session Estimated TPS (`session_estimated_tps`)**: Approximate session-wide output throughput (`~XX.X t/s`), accumulated locally from `agent_state` (`"working"`) durations and output token increments (hidden until a turn completes).
- **Session Estimated Cost (`session_cost`)**: Session spend in USD (`cost.total_usd`, optional `subagent_usd`, with configurable local per-1M token pricing fallback when `cost` is omitted).
- **Model & Effort (`model_and_effort`)**: Active model name with inline reasoning effort (e.g., `Gemini 4 Hydrogen (High)`).
- **5h & Weekly Quota Gauges (`quota_5h_gauge`, `quota_weekly_gauge`)**: Remaining quota gauges automatically switched between `gemini-*` and `3p-*` buckets based on the active model.
- **SQLite State & History Persistence (`[storage]`)**: Persists cross-tick session state (`session`, used by TPS and cumulative token metrics), usage deltas (`usage_event`), per-turn throughput (`tps_sample`), quota equivalence (`quota_bucket_cursor`, `quota_attribution`), and telemetry state (`telemetry_state`) into `$XDG_DATA_HOME/antigravity-status/default.db` (falling back to `~/.local/share/antigravity-status/default.db`; WAL mode; configurable via `storage.db_path`).
- **Right Alignment & `config.toml` Toggles**: Right-aligned by default using `terminal_width` from `agy`; enable/disable any widget individually, customize labels, order, multi-line layout, formatting, and color thresholds with hot reload on every status refresh.

## Installation

> Requires Rust `1.88+` (Edition 2024).

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
3. `<cwd>/.antigravity-status.toml` (where `cwd` comes from the `agy` payload or process directory; `[telemetry]` settings are stripped here so repositories cannot enable or redirect uploads)
4. `$XDG_CONFIG_HOME/antigravity-status/config.toml` (falling back to `~/.config/antigravity-status/config.toml`)

Print the default template anytime with:

```bash
antigravity-status --print-default-config > ~/.config/antigravity-status/config.toml
```

## Duration Stats & Opt-In Anonymous Telemetry

Summarize local SQLite history by time window (console tables or `StatsReport` v1 JSON; supported `--duration` values include `30m`, `1h`, `6h`, `24h`, `7d`, `30d`, `all`, or custom `<n><s|m|h|d|w>`, defaulting to `24h`):

```bash
antigravity-status stats --duration 24h
antigravity-status stats --duration 7d --json > my-stats.json
```

Anonymous telemetry is **strictly opt-in** (`enabled = false` by default). Only finalized whole UTC hours are eligible (never the current open hour), payloads contain only aggregated counts and histograms (no session IDs, file paths, prompts, or raw timestamps), and uploads invoke `curl` (`https://` only, with `http://` restricted to loopback hosts).

To participate using the shared community Worker, enable `[telemetry]` in your global `~/.config/antigravity-status/config.toml`:

```toml
[telemetry]
enabled = true
endpoint = "https://antigravity-status-telemetry.umahq.workers.dev"
auto_upload = true
upload_interval_secs = 300
```

Inspect and manage uploads anytime:

```bash
antigravity-status telemetry status
antigravity-status telemetry preview
antigravity-status telemetry upload --dry-run
antigravity-status telemetry upload
```

## Dashboard & Benchmark Pipeline

- `shared/`: Shared telemetry schema constants, TypeScript types, and histogram percentile math reused by `worker/` and `web/`.
- `worker/`: Cloudflare Worker (`antigravity-status-telemetry`) + D1 schema migration, scheduled hourly cron rollup (`10 * * * *`), and read-only `/api/telemetry/stats` export. See [worker/README.md](worker/README.md) for self-hosting steps.
- `web/`: Static Vite + Chart.js dashboard for browsing community rolling JSON snapshots or importing local `antigravity-status stats --json` reports (`bun --cwd web install && bun run --cwd web dev`).
- Companion repository [`united-magic-association/antigravity-status-telemetry-data`](https://github.com/united-magic-association/antigravity-status-telemetry-data): Hourly sync script (`scripts/sync.ts`, `15 * * * *`), public `data/` snapshots (`manifest.json`, `rolling/*.json`, `hourly/*.json`), and GitHub Actions workflow (`sync-and-deploy.yml`) that builds `web/` and deploys to GitHub Pages.
