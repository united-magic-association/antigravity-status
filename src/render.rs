use crate::config::Config;
use crate::tracker::SessionMetrics;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";

/// Renders the configured status line(s) for the current session metrics.
pub fn render_status(metrics: &SessionMetrics, config: &Config) -> String {
    let line_specs: Vec<&[String]> = if config.general.lines.is_empty() {
        vec![config.general.order.as_slice()]
    } else {
        config.general.lines.iter().map(Vec::as_slice).collect()
    };

    let sep = paint(&config.general.separator, DIM, config.format.color);
    let mut rendered_lines = Vec::new();

    for spec in line_specs {
        let mut parts = Vec::new();
        for id in spec {
            if let Some(segment) = render_module(id.trim(), metrics, config) {
                parts.push(segment);
            }
        }
        if !parts.is_empty() {
            rendered_lines.push(parts.join(&sep));
        }
    }

    rendered_lines.join("\n")
}

fn render_module(id: &str, metrics: &SessionMetrics, config: &Config) -> Option<String> {
    let color = config.format.color;
    match id {
        "context_gauge" => {
            let cfg = &config.modules.context_gauge;
            if !cfg.enabled {
                return None;
            }
            Some(render_context_gauge(metrics, cfg, color))
        }
        "session_input_tokens" => {
            let cfg = &config.modules.session_input_tokens;
            if !cfg.enabled {
                return None;
            }
            let val = format_tokens(metrics.input_tokens, config.format.compact_tokens);
            Some(format_kv(&cfg.label, &val, CYAN, color))
        }
        "session_cache_read_tokens" => {
            let cfg = &config.modules.session_cache_read_tokens;
            if !cfg.enabled {
                return None;
            }
            let val = format_tokens(metrics.cache_read_tokens, config.format.compact_tokens);
            Some(format_kv(&cfg.label, &val, GREEN, color))
        }
        "session_cache_write_tokens" => {
            let cfg = &config.modules.session_cache_write_tokens;
            if !cfg.enabled || (config.general.hide_zero_cache_write && metrics.cache_write_tokens == 0) {
                return None;
            }
            let val = format_tokens(metrics.cache_write_tokens, config.format.compact_tokens);
            Some(format_kv(&cfg.label, &val, YELLOW, color))
        }
        "session_output_tokens" => {
            let cfg = &config.modules.session_output_tokens;
            if !cfg.enabled {
                return None;
            }
            let val = format_tokens(metrics.output_tokens, config.format.compact_tokens);
            Some(format_kv(&cfg.label, &val, CYAN, color))
        }
        "session_cost" => {
            let cfg = &config.modules.session_cost;
            if !cfg.enabled {
                return None;
            }
            match metrics.cost_usd {
                Some(total) => {
                    let prec = config.format.cost_precision;
                    let mut val = format!("${total:.prec$}");
                    if cfg.show_subagent
                        && let Some(sub) = metrics.subagent_cost_usd
                        && sub > 0.0
                    {
                        val.push_str(&format!(" (sub ${sub:.prec$})"));
                    }
                    Some(format_kv(&cfg.label, &val, MAGENTA, color))
                }
                None if config.general.hide_missing_cost => None,
                None => Some(format_kv(&cfg.label, "N/A", DIM, color)),
            }
        }
        _ => None,
    }
}

fn render_context_gauge(
    metrics: &SessionMetrics,
    cfg: &crate::config::ContextGaugeConfig,
    color: bool,
) -> String {
    let width = cfg.width.max(1);
    let filled_char = if cfg.filled_char.is_empty() {
        "█"
    } else {
        &cfg.filled_char
    };
    let empty_char = if cfg.empty_char.is_empty() {
        "░"
    } else {
        &cfg.empty_char
    };

    let (bar, pct_text, tone) = match metrics.used_percentage {
        Some(pct) => {
            let clamped = pct.clamp(0.0, 100.0);
            let filled = ((clamped / 100.0) * width as f64).round() as usize;
            let filled = filled.min(width);
            let empty = width - filled;
            let bar = format!(
                "{}{}",
                filled_char.repeat(filled),
                empty_char.repeat(empty)
            );
            let tone = if clamped >= cfg.critical_percent {
                RED
            } else if clamped >= cfg.warn_percent {
                YELLOW
            } else {
                GREEN
            };
            (bar, format!("{clamped:.1}%"), tone)
        }
        None => (empty_char.repeat(width), "--%".to_string(), DIM),
    };

    let prefix = if cfg.label.is_empty() {
        String::new()
    } else {
        format!("{} ", paint(&cfg.label, DIM, color))
    };

    format!("{prefix}[{}] {}", paint(&bar, tone, color), paint(&pct_text, tone, color))
}

fn format_kv(label: &str, value: &str, value_color: &str, color: bool) -> String {
    let styled_val = paint(value, value_color, color);
    if label.is_empty() {
        styled_val
    } else {
        format!("{} {}", paint(label, DIM, color), styled_val)
    }
}

fn paint(text: &str, ansi: &str, enabled: bool) -> String {
    if enabled {
        format!("{ansi}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Formats token counts either concisely (`14.2k`, `1.0M`) or with thousands separators.
pub fn format_tokens(n: u64, compact: bool) -> String {
    if !compact {
        return format_with_commas(n);
    }

    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_with_commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}
