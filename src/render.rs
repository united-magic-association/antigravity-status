use crate::config::{Alignment, Config, DefaultLabel, QuotaGaugeConfig};
use crate::payload::{QuotaWindowKind, StatusPayload};
use crate::tracker::SessionMetrics;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";

/// Renders the configured status line(s) for the current session metrics and metadata.
pub fn render_status(
    metrics: &SessionMetrics,
    payload: &StatusPayload,
    config: &Config,
    terminal_width: Option<usize>,
) -> String {
    let line_specs: Vec<&[String]> = if config.general.lines.is_empty() {
        vec![config.general.order.as_slice()]
    } else {
        config.general.lines.iter().map(Vec::as_slice).collect()
    };

    let sep = paint(&config.general.separator, DIM, config.format.color);
    let mut rendered_lines = Vec::new();

    for spec in line_specs {
        let line = render_segment_list(spec, metrics, payload, config, &sep);
        if !line.is_empty() {
            rendered_lines.push(align_line(
                &line,
                config.general.align,
                config.general.right_margin,
                terminal_width,
            ));
        }
    }

    rendered_lines.join("\n")
}

fn render_segment_list(
    ids: &[String],
    metrics: &SessionMetrics,
    payload: &StatusPayload,
    config: &Config,
    sep: &str,
) -> String {
    let mut parts = Vec::new();
    for id in ids {
        if let Some(segment) = render_module(id.trim(), metrics, payload, config) {
            parts.push(segment);
        }
    }
    parts.join(sep)
}

fn align_line(
    line: &str,
    align: Alignment,
    right_margin: usize,
    terminal_width: Option<usize>,
) -> String {
    match (align, terminal_width) {
        (Alignment::Right, Some(width)) if width > right_margin => {
            let target = width - right_margin;
            let vis = visible_width(line);
            if vis < target {
                format!("{}{line}", " ".repeat(target - vis))
            } else {
                line.to_string()
            }
        }
        _ => line.to_string(),
    }
}

fn render_module(
    id: &str,
    metrics: &SessionMetrics,
    payload: &StatusPayload,
    config: &Config,
) -> Option<String> {
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
                    let prefix = if metrics.cost_is_local_estimate {
                        cfg.estimated_prefix.as_str()
                    } else {
                        ""
                    };
                    let mut val = format!("{prefix}${total:.prec$}");
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
        "model_and_effort" => {
            let cfg = &config.modules.model_and_effort;
            if !cfg.enabled {
                return None;
            }
            let raw_name = payload
                .model
                .as_ref()
                .and_then(|m| {
                    m.display_name
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .or(m.id.as_deref().filter(|s| !s.is_empty()))
                })?;

            let effort = payload
                .model
                .as_ref()
                .and_then(|m| m.effort.as_deref())
                .filter(|s| !s.is_empty());

            let display = match effort {
                Some(eff) if cfg.include_effort => ensure_effort_suffix(raw_name, eff),
                Some(eff) => strip_trailing_effort(raw_name, eff).to_string(),
                None => raw_name.to_string(),
            };

            Some(format_kv(&cfg.label, &display, BLUE, color))
        }
        "mode" => {
            let cfg = &config.modules.mode;
            if !cfg.enabled {
                return None;
            }
            let mode = payload
                .cycle_mode
                .as_deref()
                .filter(|s| !s.is_empty())?;
            let tone = match mode.to_ascii_lowercase().as_str() {
                "auto" => GREEN,
                "plan" => YELLOW,
                "accept-edits" => CYAN,
                _ => DIM,
            };
            Some(format_kv(&cfg.label, mode, tone, color))
        }
        "quota_5h_gauge" => {
            let cfg = &config.modules.quota_5h_gauge;
            if !cfg.enabled {
                return None;
            }
            let frac = payload
                .active_quota_bucket(QuotaWindowKind::FiveHour)
                .and_then(|b| b.remaining_fraction);
            render_quota_gauge(frac, cfg, color)
        }
        "quota_weekly_gauge" => {
            let cfg = &config.modules.quota_weekly_gauge;
            if !cfg.enabled {
                return None;
            }
            let frac = payload
                .active_quota_bucket(QuotaWindowKind::Weekly)
                .and_then(|b| b.remaining_fraction);
            render_quota_gauge(frac, cfg, color)
        }
        _ => None,
    }
}

fn render_quota_gauge<L: DefaultLabel>(
    remaining_fraction: Option<f64>,
    cfg: &QuotaGaugeConfig<L>,
    color: bool,
) -> Option<String> {
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

    let (bar, pct_text, tone) = match remaining_fraction {
        Some(frac) => {
            let pct = (frac * 100.0).clamp(0.0, 100.0);
            let filled = ((pct / 100.0) * width as f64).round() as usize;
            let filled = filled.min(width);
            let empty = width - filled;
            let bar = format!(
                "{}{}",
                filled_char.repeat(filled),
                empty_char.repeat(empty)
            );
            let tone = if pct <= cfg.critical_percent {
                RED
            } else if pct <= cfg.warn_percent {
                YELLOW
            } else {
                GREEN
            };
            let prec = cfg.decimals;
            (bar, format!("{pct:.prec$}%"), tone)
        }
        None if cfg.hide_missing => return None,
        None => (empty_char.repeat(width), "--%".to_string(), DIM),
    };

    let prefix = if cfg.label.is_empty() {
        String::new()
    } else {
        format!("{} ", paint(&cfg.label, DIM, color))
    };

    Some(format!(
        "{prefix}[{}] {}",
        paint(&bar, tone, color),
        paint(&pct_text, tone, color)
    ))
}

fn strip_trailing_effort<'a>(name: &'a str, effort: &str) -> &'a str {
    let trimmed = name.trim_end();
    if let Some(without_paren) = trimmed.strip_suffix(')')
        && let Some(open_idx) = without_paren.rfind('(')
    {
        let inner = without_paren[open_idx + 1..].trim();
        if inner.eq_ignore_ascii_case(effort.trim()) {
            return without_paren[..open_idx].trim_end();
        }
    }
    name
}

fn ensure_effort_suffix(name: &str, effort: &str) -> String {
    let trimmed = name.trim_end();
    if let Some(without_paren) = trimmed.strip_suffix(')')
        && let Some(open_idx) = without_paren.rfind('(')
    {
        let inner = without_paren[open_idx + 1..].trim();
        if inner.eq_ignore_ascii_case(effort.trim()) {
            return trimmed.to_string();
        }
    }
    format!("{trimmed} ({})", title_case_effort(effort.trim()))
}

fn title_case_effort(effort: &str) -> String {
    if effort.eq_ignore_ascii_case("xhigh") {
        return "XHigh".to_string();
    }
    let mut chars = effort.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str().to_lowercase()),
        None => String::new(),
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

/// Computes terminal display width of a string while ignoring ANSI CSI escape sequences.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        width += char_display_width(ch);
    }
    width
}

fn char_display_width(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    let cp = ch as u32;
    let wide = matches!(
        cp,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3040..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD
    );
    if wide { 2 } else { 1 }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config_str;

    fn sample_metrics() -> SessionMetrics {
        SessionMetrics {
            used_percentage: Some(42.3),
            context_window_size: 1_048_576,
            input_tokens: 88_244,
            cache_read_tokens: 20_857,
            cache_write_tokens: 4_096,
            output_tokens: 61_074,
            cost_usd: Some(0.1428),
            subagent_cost_usd: Some(0.021),
            cost_is_local_estimate: false,
        }
    }

    fn sample_payload() -> StatusPayload {
        serde_json::from_str(
            r#"{
              "cycle_mode": "auto",
              "terminal_width": 170,
              "model": {
                "id": "Gemini 4 Hydrogen (High)",
                "display_name": "Gemini 4 Hydrogen (High)",
                "effort": "high"
              },
              "quota": {
                "gemini-5h": { "remaining_fraction": 0.933 },
                "gemini-weekly": { "remaining_fraction": 0.922 },
                "3p-5h": { "remaining_fraction": 1.0 },
                "3p-weekly": { "remaining_fraction": 0.539 }
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn renders_gemini_quota_gauges_to_the_right_of_model_and_effort() {
        let mut cfg = Config::default();
        cfg.format.color = false;
        let out = render_status(&sample_metrics(), &sample_payload(), &cfg, Some(170));
        let expected_content = "Mode: auto │ Ctx [████░░░░░░] 42.3% │ In 88.2k │ CacheR 20.9k │ CacheW 4.1k │ Out 61.1k │ Cost $0.1428 │ Gemini 4 Hydrogen (High) │ 5h [██████] 93.3% │ Wk [██████] 92.2%";
        assert!(out.starts_with(" "));
        assert!(out.ends_with(expected_content));
        assert_eq!(visible_width(&out), 170);
    }

    #[test]
    fn switches_to_3p_quota_buckets_for_non_gemini_model() {
        let mut cfg = Config::default();
        cfg.format.color = false;
        cfg.general.align = Alignment::Left;
        cfg.general.order = vec![
            "model_and_effort".to_string(),
            "quota_5h_gauge".to_string(),
            "quota_weekly_gauge".to_string(),
        ];
        let mut payload = sample_payload();
        payload.model.as_mut().unwrap().id = Some("Claude Sonnet 4.6".to_string());
        payload.model.as_mut().unwrap().display_name = Some("Claude Sonnet 4.6".to_string());

        let out = render_status(&sample_metrics(), &payload, &cfg, None);
        assert_eq!(
            out,
            "Claude Sonnet 4.6 (High) │ 5h [██████] 100.0% │ Wk [███░░░] 53.9%"
        );
    }

    #[test]
    fn appends_effort_when_model_name_lacks_suffix() {
        let mut cfg = Config::default();
        cfg.format.color = false;
        cfg.general.align = Alignment::Left;
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "cycle_mode": "plan",
              "model": {
                "id": "custom-model",
                "display_name": "Custom Model",
                "effort": "xhigh"
              }
            }"#,
        )
        .unwrap();
        let out = render_status(&sample_metrics(), &payload, &cfg, None);
        assert!(out.starts_with("Mode: plan │ "));
        assert!(out.ends_with("Custom Model (XHigh)"));
    }

    #[test]
    fn visible_width_ignores_ansi_escapes() {
        let mut cfg = Config::default();
        cfg.format.color = true;
        let out = render_status(&sample_metrics(), &sample_payload(), &cfg, Some(180));
        assert_eq!(visible_width(&out), 180);
    }

    #[test]
    fn hides_disabled_modules_and_supports_multiline_left_aligned() {
        let raw = r#"
[general]
align = "left"
lines = [
  ["mode", "context_gauge", "session_cost"],
  ["session_input_tokens", "session_cache_read_tokens", "session_cache_write_tokens", "session_output_tokens"],
]
hide_zero_cache_write = true

[format]
color = false
compact_tokens = false

[modules.session_cache_read_tokens]
enabled = false
"#;
        let cfg = parse_config_str(raw).unwrap();
        let mut metrics = sample_metrics();
        metrics.cache_write_tokens = 0;

        let out = render_status(&metrics, &sample_payload(), &cfg, Some(120));
        assert_eq!(
            out,
            "Mode: auto │ Ctx [████░░░░░░] 42.3% │ Cost $0.1428\nIn 88,244 │ Out 61,074"
        );
    }

    #[test]
    fn hides_model_effort_when_include_effort_is_false() {
        let mut cfg = Config::default();
        cfg.format.color = false;
        cfg.general.align = Alignment::Left;
        cfg.modules.model_and_effort.include_effort = false;
        let out = render_status(&sample_metrics(), &sample_payload(), &cfg, None);
        assert!(out.contains("│ Gemini 4 Hydrogen │"));
    }

    #[test]
    fn handles_missing_initial_metrics_gracefully() {
        let mut cfg = Config::default();
        cfg.format.color = false;
        cfg.general.align = Alignment::Left;
        cfg.general.hide_missing_cost = true;

        let out = render_status(
            &SessionMetrics::default(),
            &StatusPayload::default(),
            &cfg,
            None,
        );
        assert_eq!(
            out,
            "Ctx [░░░░░░░░░░] --% │ In 0 │ CacheR 0 │ CacheW 0 │ Out 0"
        );
    }
}
