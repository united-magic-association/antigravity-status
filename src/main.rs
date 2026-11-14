mod config;
mod db;
mod payload;
mod render;
mod stats;
mod telemetry;
mod tracker;

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

use payload::StatusPayload;

const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.example.toml");

enum Command {
    Render(RenderArgs),
    Stats(StatsArgs),
    Telemetry(TelemetryArgs),
}

struct RenderArgs {
    config_path: Option<PathBuf>,
    dump_path: Option<PathBuf>,
    print_default_config: bool,
    show_help: bool,
    show_version: bool,
}

struct StatsArgs {
    config_path: Option<PathBuf>,
    duration: String,
    json: bool,
    show_help: bool,
}

enum TelemetryAction {
    Status,
    Preview,
    Upload,
}

struct TelemetryArgs {
    action: TelemetryAction,
    config_path: Option<PathBuf>,
    db_path: Option<PathBuf>,
    dry_run: bool,
    quiet: bool,
    show_help: bool,
}

fn parse_args() -> Result<Command, String> {
    let mut raw_args: Vec<String> = std::env::args().skip(1).collect();
    match raw_args.first().map(String::as_str) {
        Some("stats") => {
            raw_args.remove(0);
            parse_stats_args(&raw_args).map(Command::Stats)
        }
        Some("telemetry") => {
            raw_args.remove(0);
            parse_telemetry_args(&raw_args).map(Command::Telemetry)
        }
        _ => parse_render_args(&raw_args).map(Command::Render),
    }
}

fn parse_render_args(raw_args: &[String]) -> Result<RenderArgs, String> {
    let mut args = RenderArgs {
        config_path: None,
        dump_path: None,
        print_default_config: false,
        show_help: false,
        show_version: false,
    };

    let mut iter = raw_args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let val = iter
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "--config requires a non-empty file path".to_string())?;
                args.config_path = Some(PathBuf::from(val));
            }
            "--dump" => {
                let val = iter
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "--dump requires a non-empty file path".to_string())?;
                args.dump_path = Some(PathBuf::from(val));
            }
            "--print-default-config" => args.print_default_config = true,
            "--help" | "-h" => args.show_help = true,
            "--version" | "-V" => args.show_version = true,
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(args)
}

fn parse_stats_args(raw_args: &[String]) -> Result<StatsArgs, String> {
    let mut args = StatsArgs {
        config_path: None,
        duration: "24h".to_string(),
        json: false,
        show_help: false,
    };

    let mut iter = raw_args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let val = iter
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "--config requires a non-empty file path".to_string())?;
                args.config_path = Some(PathBuf::from(val));
            }
            "--duration" | "-d" => {
                let val = iter
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        "--duration requires a value (e.g. 1h, 24h, 7d, all)".to_string()
                    })?;
                args.duration = val.clone();
            }
            "--json" => args.json = true,
            "--help" | "-h" => args.show_help = true,
            other => return Err(format!("unknown stats argument: {other}")),
        }
    }

    Ok(args)
}

fn parse_telemetry_args(raw_args: &[String]) -> Result<TelemetryArgs, String> {
    if raw_args.is_empty() {
        return Ok(TelemetryArgs {
            action: TelemetryAction::Status,
            config_path: None,
            db_path: None,
            dry_run: false,
            quiet: false,
            show_help: true,
        });
    }

    let mut action: Option<TelemetryAction> = None;
    let mut config_path = None;
    let mut db_path = None;
    let mut dry_run = false;
    let mut quiet = false;
    let mut show_help = false;

    let mut iter = raw_args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let val = iter
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "--config requires a non-empty file path".to_string())?;
                config_path = Some(PathBuf::from(val));
            }
            "--db" => {
                let val = iter
                    .next()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "--db requires a non-empty file path".to_string())?;
                db_path = Some(PathBuf::from(val));
            }
            "--dry-run" => dry_run = true,
            "--quiet" => quiet = true,
            "--help" | "-h" => show_help = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown telemetry argument: {other}"));
            }
            pos => {
                let parsed = match pos {
                    "status" => TelemetryAction::Status,
                    "preview" => TelemetryAction::Preview,
                    "upload" => TelemetryAction::Upload,
                    other => {
                        return Err(format!(
                            "unknown telemetry subcommand '{other}' (expected: status, preview, upload)"
                        ));
                    }
                };
                if action.is_some() {
                    return Err(format!("unexpected extra telemetry argument: {pos}"));
                }
                action = Some(parsed);
            }
        }
    }

    if show_help {
        return Ok(TelemetryArgs {
            action: action.unwrap_or(TelemetryAction::Status),
            config_path,
            db_path,
            dry_run,
            quiet,
            show_help: true,
        });
    }

    let action = action.ok_or_else(|| {
        "missing telemetry subcommand (expected: status, preview, upload)".to_string()
    })?;

    Ok(TelemetryArgs {
        action,
        config_path,
        db_path,
        dry_run,
        quiet,
        show_help: false,
    })
}

fn print_help() {
    println!(
        "\
antigravity-status {}
Configurable status line renderer for Google Antigravity CLI (agy).

USAGE:
    antigravity-status [OPTIONS]
    antigravity-status stats [OPTIONS]
    antigravity-status telemetry <status|preview|upload> [OPTIONS]

SUBCOMMANDS:
    stats                       Summarize recorded local usage, TPS, and quota equivalence
    telemetry                   Inspect or upload opt-in anonymous hourly aggregates

OPTIONS:
    -c, --config <PATH>         Path to config.toml
        --dump <PATH>           Append raw stdin JSON payload to PATH for inspection
        --print-default-config  Print default config.toml template and exit
    -h, --help                  Print help information
    -V, --version               Print version information",
        env!("CARGO_PKG_VERSION")
    );
}

fn print_stats_help() {
    println!(
        "\
antigravity-status stats
Summarize recorded local usage, TPS, and quota equivalence by duration.

USAGE:
    antigravity-status stats [OPTIONS]

OPTIONS:
    -d, --duration <DUR>        Time window: 30m, 1h, 6h, 24h, 7d, 30d, or all (default: 24h)
        --json                  Output StatsReport v1 JSON instead of console tables
    -c, --config <PATH>         Path to config.toml
    -h, --help                  Print help information"
    );
}

fn print_telemetry_help() {
    println!(
        "\
antigravity-status telemetry
Inspect or upload opt-in anonymous hourly usage aggregates via curl.

USAGE:
    antigravity-status telemetry <SUBCOMMAND> [OPTIONS]

SUBCOMMANDS:
    status                      Show opt-in config state and pending closed hours
    preview                     Print exact sanitized JSON payloads pending upload
    upload                      Upload closed hourly slices to Worker /api/telemetry/ingest

OPTIONS:
        --dry-run               With `upload`, print payloads instead of sending HTTP POST
        --quiet                 Suppress stdout progress messages
        --db <PATH>             Override SQLite database path
    -c, --config <PATH>         Path to config.toml
    -h, --help                  Print help information"
    );
}

fn main() {
    let cmd = match parse_args() {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("antigravity-status: {err}");
            process::exit(2);
        }
    };

    match cmd {
        Command::Render(args) => run_render(args),
        Command::Stats(args) => run_stats(args),
        Command::Telemetry(args) => run_telemetry(args),
    }
}

fn run_stats(args: StatsArgs) {
    if args.show_help {
        print_stats_help();
        return;
    }

    let duration_secs = match stats::parse_duration_spec(&args.duration) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("antigravity-status: {err}");
            process::exit(2);
        }
    };

    let (cfg, resolved_config_path) =
        match config::try_load_config_with_path(args.config_path.as_deref(), None) {
            Ok(loaded) => loaded,
            Err(err) => {
                eprintln!("antigravity-status: {err}");
                process::exit(1);
            }
        };
    let base_dir = resolved_config_path.as_deref().and_then(|p| p.parent());
    let Some(db_path) = cfg.storage.resolved_db_path_with_base(base_dir) else {
        eprintln!("antigravity-status: unable to resolve SQLite database path");
        process::exit(1);
    };

    let end_unix = db::current_unix_secs();
    let start_unix = duration_secs.map(|d| end_unix.saturating_sub(d));
    let report = match db::query_stats_report(&db_path, start_unix, end_unix) {
        Ok(r) => r,
        Err(err) => {
            eprintln!(
                "antigravity-status: failed to query SQLite stats at {}: {err}",
                db_path.display()
            );
            process::exit(1);
        }
    };

    if args.json {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(err) => {
                eprintln!("antigravity-status: failed to serialize stats JSON: {err}");
                process::exit(1);
            }
        }
    } else {
        print!("{}", stats::format_stats_console(&report));
    }
}

fn run_telemetry(args: TelemetryArgs) {
    if args.show_help {
        print_telemetry_help();
        return;
    }

    let (cfg, resolved_config_path) =
        match config::try_load_config_with_path(args.config_path.as_deref(), None) {
            Ok(loaded) => loaded,
            Err(err) => {
                eprintln!("antigravity-status: {err}");
                process::exit(1);
            }
        };
    let base_dir = resolved_config_path.as_deref().and_then(|p| p.parent());
    let Some(db_path) = args
        .db_path
        .or_else(|| cfg.storage.resolved_db_path_with_base(base_dir))
    else {
        eprintln!("antigravity-status: unable to resolve SQLite database path");
        process::exit(1);
    };

    let res = match args.action {
        TelemetryAction::Status => telemetry::print_status(&cfg, &db_path),
        TelemetryAction::Preview => telemetry::print_preview(&db_path),
        TelemetryAction::Upload => {
            telemetry::upload_pending(&cfg, &db_path, args.dry_run, args.quiet)
        }
    };

    if let Err(err) = res {
        if !args.quiet {
            eprintln!("antigravity-status: {err}");
        }
        process::exit(1);
    }
}

fn run_render(args: RenderArgs) {
    if args.show_help {
        print_help();
        return;
    }

    if args.show_version {
        println!("antigravity-status {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.print_default_config {
        print!("{DEFAULT_CONFIG_TEMPLATE}");
        return;
    }

    let mut raw_stdin = String::new();
    if let Err(err) = io::stdin().read_to_string(&mut raw_stdin) {
        eprintln!("antigravity-status: failed to read stdin: {err}");
    }

    if let Some(dump_path) = args.dump_path.as_ref()
        && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(dump_path)
    {
        let _ = writeln!(file, "{}", raw_stdin.trim());
    }

    let payload: StatusPayload = if raw_stdin.trim().is_empty() {
        StatusPayload::default()
    } else {
        serde_json::from_str(&raw_stdin).unwrap_or_else(|err| {
            eprintln!("antigravity-status: failed to parse stdin JSON: {err}");
            StatusPayload::default()
        })
    };

    let (cfg, resolved_config_path) = match config::try_load_config_with_path(
        args.config_path.as_deref(),
        payload.cwd.as_deref(),
    ) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("antigravity-status: {err}");
            process::exit(1);
        }
    };
    let fallback_rates = if cfg.modules.session_cost.estimate_when_missing {
        Some(cfg.pricing.rates_for_model(payload.effective_model_name()))
    } else {
        None
    };
    let base_dir = resolved_config_path
        .as_deref()
        .and_then(|p| p.parent())
        .or_else(|| payload.cwd.as_deref().map(Path::new));
    let metrics = if let Some(db_path) = cfg.storage.resolved_db_path_with_base(base_dir) {
        let metrics = match db::resolve_and_record(&db_path, &payload, fallback_rates) {
            Ok(m) => m,
            Err(err) => {
                eprintln!(
                    "antigravity-status: failed to record SQLite snapshot at {}: {err}",
                    db_path.display()
                );
                tracker::resolve_metrics_stateless(&payload, fallback_rates)
            }
        };
        telemetry::maybe_spawn_background_upload(&db_path, &cfg, resolved_config_path.as_deref());
        metrics
    } else {
        tracker::resolve_metrics_stateless(&payload, fallback_rates)
    };
    let terminal_width = payload
        .terminal_width
        .filter(|&w| w > 0)
        .map(usize::from)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&w| w > 0)
        });
    let output = render::render_status(&metrics, &payload, &cfg, terminal_width);

    if !output.is_empty() {
        println!("{output}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_args_accept_options_before_or_after_subcommand() {
        let parsed = parse_telemetry_args(&[
            "--config".to_string(),
            "/tmp/cfg.toml".to_string(),
            "upload".to_string(),
            "--dry-run".to_string(),
            "--db".to_string(),
            "/tmp/data.db".to_string(),
        ])
        .expect("should parse flags before and after subcommand");
        assert!(matches!(parsed.action, TelemetryAction::Upload));
        assert_eq!(parsed.config_path, Some(PathBuf::from("/tmp/cfg.toml")));
        assert_eq!(parsed.db_path, Some(PathBuf::from("/tmp/data.db")));
        assert!(parsed.dry_run);
    }

    #[test]
    fn cli_parsers_reject_empty_path_arguments() {
        assert!(
            parse_telemetry_args(&["status".to_string(), "--db".to_string(), "".to_string()])
                .is_err()
        );
        assert!(
            parse_telemetry_args(&["status".to_string(), "-c".to_string(), "  ".to_string()])
                .is_err()
        );
        assert!(parse_stats_args(&["--config".to_string(), "".to_string()]).is_err());
        assert!(parse_render_args(&["--dump".to_string(), "".to_string()]).is_err());
    }
}
