mod config;
mod payload;
mod render;
mod tracker;

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process;

use payload::StatusPayload;

const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.example.toml");

struct CliArgs {
    config_path: Option<PathBuf>,
    dump_path: Option<PathBuf>,
    print_default_config: bool,
    show_help: bool,
    show_version: bool,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = CliArgs {
        config_path: None,
        dump_path: None,
        print_default_config: false,
        show_help: false,
        show_version: false,
    };

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--config requires a file path".to_string())?;
                args.config_path = Some(PathBuf::from(val));
            }
            "--dump" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--dump requires a file path".to_string())?;
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

fn print_help() {
    println!(
        "\
antigravity-status {}
Configurable status line renderer for Google Antigravity CLI (agy).

USAGE:
    antigravity-status [OPTIONS]

OPTIONS:
    -c, --config <PATH>         Path to config.toml
        --dump <PATH>           Append raw stdin JSON payload to PATH for inspection
        --print-default-config  Print default config.toml template and exit
    -h, --help                  Print help information
    -V, --version               Print version information",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args = match parse_args() {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("antigravity-status: {err}");
            process::exit(2);
        }
    };

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

    let cfg = config::load_config(args.config_path.as_deref(), payload.cwd.as_deref());
    let cache_dir = tracker::default_cache_dir();
    let metrics = tracker::resolve_metrics(&payload, cache_dir.as_deref());
    let output = render::render_status(&metrics, &cfg);

    if !output.is_empty() {
        println!("{output}");
    }
}
