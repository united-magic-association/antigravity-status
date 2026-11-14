use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::config::TokenRates;

pub const STATS_SCHEMA_VERSION: u32 = 1;
pub const TPS_HISTOGRAM_BUCKETS: usize = 14;
pub const TPS_HISTOGRAM_EDGES: [f64; TPS_HISTOGRAM_BUCKETS - 1] = [
    15.0, 30.0, 45.0, 60.0, 80.0, 100.0, 130.0, 160.0, 220.0, 320.0, 500.0, 800.0, 1300.0,
];
pub const QUOTA_COST_HISTOGRAM_BUCKETS: usize = 14;
pub const QUOTA_COST_HISTOGRAM_EDGES: [f64; QUOTA_COST_HISTOGRAM_BUCKETS - 1] = [
    0.005, 0.01, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0,
];
pub const QUOTA_COST_HISTOGRAM_MAX: f64 = 64.0;
pub const REPORT_PERCENTILES: [(f64, &str); 3] = [(0.50, "p50"), (0.90, "p90"), (0.95, "p95")];
pub const MIN_TPS_SAMPLE_OUTPUT_TOKENS: u64 = 100;
pub const MIN_TPS_SAMPLE_ELAPSED_MS: u64 = 500;
pub const MAX_VALID_TPS: f64 = 2000.0;
pub const MIXED_ATTRIBUTION_KEY: &str = "__mixed__";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatsReport {
    pub schema_version: u32,
    pub scope: String,
    pub window: StatsWindow,
    pub contributors: u64,
    pub usage_by_plan_model: Vec<UsageByPlanModel>,
    pub tps_by_model_effort: Vec<TpsByModelEffort>,
    pub quota_by_plan_bucket_model: Vec<QuotaByPlanBucketModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatsWindow {
    pub start: String,
    pub end: String,
    pub duration_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageByPlanModel {
    pub plan_tier: String,
    pub model: String,
    pub effort: String,
    pub events: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reference_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TpsByModelEffort {
    pub model: String,
    pub effort: String,
    pub sample_count: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub histogram: [u64; TPS_HISTOGRAM_BUCKETS],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaByPlanBucketModel {
    pub plan_tier: String,
    pub bucket: String,
    pub attributed_model: String,
    pub attributed_effort: String,
    pub slices: u64,
    pub delta_quota_fraction: f64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reference_cost_usd: f64,
    #[serde(default)]
    pub cost_per_pct_histogram: [u64; QUOTA_COST_HISTOGRAM_BUCKETS],
}

impl StatsReport {
    /// Creates an empty local `StatsReport` for the given time window.
    pub fn empty_local(start: String, end: String, duration_secs: u64) -> Self {
        Self {
            schema_version: STATS_SCHEMA_VERSION,
            scope: "local".to_string(),
            window: StatsWindow {
                start,
                end,
                duration_secs,
            },
            contributors: 1,
            usage_by_plan_model: Vec::new(),
            tps_by_model_effort: Vec::new(),
            quota_by_plan_bucket_model: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.usage_by_plan_model.is_empty()
            && self.tps_by_model_effort.is_empty()
            && self.quota_by_plan_bucket_model.is_empty()
    }
}

fn bucket_index_from_edges(value: f64, edges: &[f64]) -> usize {
    edges
        .iter()
        .position(|&edge| value < edge)
        .unwrap_or(edges.len())
}

fn estimate_histogram_percentile(
    histogram: &[u64],
    edges: &[f64],
    max_bound: f64,
    q: f64,
) -> Option<f64> {
    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return None;
    }
    let target = (total as f64) * q.clamp(0.0, 1.0);

    let mut cumulative = 0.0;
    for (idx, &count) in histogram.iter().enumerate() {
        let lo = if idx == 0 { 0.0 } else { edges[idx - 1] };
        let hi = edges.get(idx).copied().unwrap_or(max_bound);
        let count_f = count as f64;
        if count > 0 && cumulative + count_f >= target {
            let ratio = ((target - cumulative) / count_f).clamp(0.0, 1.0);
            return Some(lo + ratio * (hi - lo));
        }
        cumulative += count_f;
    }
    Some(max_bound)
}

/// Validates a completed turn TPS sample and returns its histogram bucket index.
pub fn tps_bucket_index(output_tokens: u64, elapsed_ms: u64) -> Option<usize> {
    if output_tokens < MIN_TPS_SAMPLE_OUTPUT_TOKENS || elapsed_ms < MIN_TPS_SAMPLE_ELAPSED_MS {
        return None;
    }
    let tps = (output_tokens as f64) / (elapsed_ms as f64 / 1000.0);
    if !tps.is_finite() || tps <= 0.0 || tps > MAX_VALID_TPS {
        return None;
    }
    Some(bucket_index_from_edges(tps, &TPS_HISTOGRAM_EDGES))
}

/// Estimates a quantile `q` in `[0.0, 1.0]` from the fixed TPS histogram.
pub fn histogram_percentile(histogram: &[u64; TPS_HISTOGRAM_BUCKETS], q: f64) -> Option<f64> {
    estimate_histogram_percentile(histogram, &TPS_HISTOGRAM_EDGES, MAX_VALID_TPS, q)
}

/// Maps a single quota attribution slice (`delta_quota_fraction`, `reference_cost_usd`)
/// into a `$/1% Quota` histogram bucket index (`0..9`).
pub fn quota_cost_bucket_index(
    delta_quota_fraction: f64,
    reference_cost_usd: f64,
) -> Option<usize> {
    if !delta_quota_fraction.is_finite()
        || delta_quota_fraction <= 0.0
        || !reference_cost_usd.is_finite()
        || reference_cost_usd <= 0.0
    {
        return None;
    }
    let cost_per_pct = reference_cost_usd / (delta_quota_fraction * 100.0);
    if !cost_per_pct.is_finite() || cost_per_pct <= 0.0 {
        return None;
    }
    Some(bucket_index_from_edges(
        cost_per_pct,
        &QUOTA_COST_HISTOGRAM_EDGES,
    ))
}

/// Estimates a quantile `q` in `[0.0, 1.0]` from the `$/1% Quota` histogram.
pub fn quota_cost_histogram_percentile(
    histogram: &[u64; QUOTA_COST_HISTOGRAM_BUCKETS],
    q: f64,
) -> Option<f64> {
    estimate_histogram_percentile(
        histogram,
        &QUOTA_COST_HISTOGRAM_EDGES,
        QUOTA_COST_HISTOGRAM_MAX,
        q,
    )
}

/// Deterministic public reference pricing (USD per 1M tokens) used so community
/// quota-to-cost metrics remain comparable regardless of user-local `config.toml` overrides.
pub fn reference_rates_for_model(model: &str) -> TokenRates {
    let lower = model.to_ascii_lowercase();
    if lower.contains("haiku") {
        return TokenRates {
            input_per_million: 1.00,
            cache_read_per_million: 0.10,
            cache_write_per_million: 1.25,
            output_per_million: 5.00,
        };
    }
    if lower.contains("sonnet") {
        return TokenRates {
            input_per_million: 3.00,
            cache_read_per_million: 0.30,
            cache_write_per_million: 3.75,
            output_per_million: 15.00,
        };
    }
    if lower.contains("opus") {
        return TokenRates {
            input_per_million: 5.00,
            cache_read_per_million: 0.50,
            cache_write_per_million: 6.25,
            output_per_million: 25.00,
        };
    }
    if lower.contains("flash") || lower.contains("helium") || lower.contains("lite") {
        return TokenRates {
            input_per_million: 0.30,
            cache_read_per_million: 0.03,
            cache_write_per_million: 0.375,
            output_per_million: 2.50,
        };
    }
    if lower.contains("gpt") {
        return TokenRates {
            input_per_million: 1.75,
            cache_read_per_million: 0.175,
            cache_write_per_million: 1.75,
            output_per_million: 14.00,
        };
    }
    TokenRates::default()
}

pub fn reference_cost_usd(
    model: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
) -> f64 {
    reference_rates_for_model(model).cost_usd(
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        output_tokens,
    )
}

/// Parses human-friendly duration strings such as `30m`, `1h`, `24h`, `7d`, `30d`, or `all`.
pub fn parse_duration_spec(spec: &str) -> Result<Option<u64>, String> {
    let trimmed = spec.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Err("duration cannot be empty".to_string());
    }
    if trimmed == "all" {
        return Ok(None);
    }
    let split_idx = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("duration '{spec}' requires a unit (s, m, h, d, w) or 'all'"))?;
    let (num_str, unit) = trimmed.split_at(split_idx);
    let value: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration number in '{spec}'"))?;
    if value == 0 {
        return Err("duration must be greater than zero".to_string());
    }
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => {
            return Err(format!(
                "unsupported duration unit '{unit}' in '{spec}' (use s, m, h, d, w, or all)"
            ));
        }
    };
    value
        .checked_mul(mult)
        .map(Some)
        .ok_or_else(|| format!("duration '{spec}' overflows"))
}

/// Formats a `StatsReport` into aligned console tables.
pub fn format_stats_console(report: &StatsReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "antigravity-status stats ({}, contributors: {})",
        report.scope, report.contributors
    );
    let _ = writeln!(
        out,
        "Window: {} -> {} ({})",
        report.window.start,
        report.window.end,
        format_duration_human(report.window.duration_secs)
    );

    if report.is_empty() {
        let _ = writeln!(out, "\nNo recorded usage events in this window.");
        return out;
    }

    if !report.usage_by_plan_model.is_empty() {
        let _ = writeln!(out, "\nUsage & Cache by Plan / Model / Effort:");
        let _ = writeln!(
            out,
            "  {:<18} {:<26} {:<8} {:>6} {:>9} {:>9} {:>9} {:>7} {:>10}",
            "Plan", "Model", "Effort", "Events", "Input", "CacheR", "Output", "Hit%", "RefCost"
        );
        for row in &report.usage_by_plan_model {
            let denom = row.input_tokens.saturating_add(row.cache_read_tokens);
            let hit_pct = if denom > 0 {
                (row.cache_read_tokens as f64) * 100.0 / (denom as f64)
            } else {
                0.0
            };
            let _ = writeln!(
                out,
                "  {:<18} {:<26} {:<8} {:>6} {:>9} {:>9} {:>9} {:>6.1}% ${:>9.4}",
                truncate_label(&row.plan_tier, 18),
                truncate_label(&row.model, 26),
                truncate_label(&row.effort, 8),
                row.events,
                compact_count(row.input_tokens),
                compact_count(row.cache_read_tokens),
                compact_count(row.output_tokens),
                hit_pct,
                row.reference_cost_usd
            );
        }
    }

    if !report.tps_by_model_effort.is_empty() {
        let _ = writeln!(out, "\nEstimated Output TPS by Model / Effort:");
        let pct_headers: String = REPORT_PERCENTILES
            .iter()
            .map(|(_, label)| format!(" {:>9}", format!("Est {label}")))
            .collect();
        let _ = writeln!(
            out,
            "  {:<28} {:<8} {:>7} {:>9} {:>11}{}",
            "Model", "Effort", "Samples", "OutTok", "Weighted", pct_headers
        );
        for row in &report.tps_by_model_effort {
            let weighted = if row.elapsed_ms > 0 {
                (row.output_tokens as f64) / (row.elapsed_ms as f64 / 1000.0)
            } else {
                0.0
            };
            let pct_cols: String = REPORT_PERCENTILES
                .iter()
                .map(|(q, _)| {
                    let s = histogram_percentile(&row.histogram, *q)
                        .map_or_else(|| "--".to_string(), |v| format!("{v:.1} t/s"));
                    format!(" {s:>9}")
                })
                .collect();
            let _ = writeln!(
                out,
                "  {:<28} {:<8} {:>7} {:>9} {:>7.1} t/s{}",
                truncate_label(&row.model, 28),
                truncate_label(&row.effort, 8),
                row.sample_count,
                compact_count(row.output_tokens),
                weighted,
                pct_cols
            );
        }
    }

    if !report.quota_by_plan_bucket_model.is_empty() {
        let primary_q = REPORT_PERCENTILES[0].0;
        let mut baseline_cost_per_pct: BTreeMap<(String, String), f64> = BTreeMap::new();
        for row in &report.quota_by_plan_bucket_model {
            if row.attributed_model == MIXED_ATTRIBUTION_KEY || row.delta_quota_fraction <= 0.0 {
                continue;
            }
            let mean_cost = row.reference_cost_usd / (row.delta_quota_fraction * 100.0);
            let representative_cost =
                quota_cost_histogram_percentile(&row.cost_per_pct_histogram, primary_q)
                    .unwrap_or(mean_cost);
            if representative_cost > 0.0 {
                let key = (row.plan_tier.clone(), row.bucket.clone());
                baseline_cost_per_pct
                    .entry(key)
                    .and_modify(|cur| *cur = cur.min(representative_cost))
                    .or_insert(representative_cost);
            }
        }

        let _ = writeln!(out, "\nQuota Cost Equivalence & Relative Model Weight:");
        let pct_headers: String = REPORT_PERCENTILES
            .iter()
            .map(|(_, label)| format!(" {:>9}", format!("$/1% {label}")))
            .collect();
        let _ = writeln!(
            out,
            "  {:<18} {:<14} {:<24} {:<8} {:>7}{} {:>9} {:>7}",
            "Plan",
            "Bucket",
            "Attributed Model",
            "Effort",
            "Drop%",
            pct_headers,
            "$/1%Mean",
            "RelWt"
        );
        for row in &report.quota_by_plan_bucket_model {
            let pct_drop = row.delta_quota_fraction * 100.0;
            let cost_per_pct = if pct_drop > 0.0 {
                row.reference_cost_usd / pct_drop
            } else {
                0.0
            };
            let primary_cost =
                quota_cost_histogram_percentile(&row.cost_per_pct_histogram, primary_q);
            let rep_cost = primary_cost.unwrap_or(cost_per_pct);
            let rel = if row.attributed_model != MIXED_ATTRIBUTION_KEY && rep_cost > 0.0 {
                baseline_cost_per_pct
                    .get(&(row.plan_tier.clone(), row.bucket.clone()))
                    .map(|base| format!("{:.2}x", base / rep_cost))
                    .unwrap_or_else(|| "--".to_string())
            } else {
                "--".to_string()
            };
            let pct_cols: String = REPORT_PERCENTILES
                .iter()
                .map(|(q, _)| {
                    let s = quota_cost_histogram_percentile(&row.cost_per_pct_histogram, *q)
                        .map_or_else(|| "   --    ".to_string(), |v| format!("${v:>8.4}"));
                    format!(" {s:>9}")
                })
                .collect();
            let _ = writeln!(
                out,
                "  {:<18} {:<14} {:<24} {:<8} {:>6.2}%{} ${:>8.4} {:>7}",
                truncate_label(&row.plan_tier, 18),
                truncate_label(&row.bucket, 14),
                truncate_label(&row.attributed_model, 24),
                truncate_label(&row.attributed_effort, 8),
                pct_drop,
                pct_cols,
                cost_per_pct,
                rel
            );
        }
    }

    out
}

fn format_duration_human(secs: u64) -> String {
    if secs == 0 {
        return "all history".to_string();
    }
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn truncate_label(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 1 {
        return text.chars().take(max_chars).collect();
    }
    let prefix: String = text.chars().take(max_chars - 1).collect();
    format!("{prefix}~")
}

fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", (n as f64) / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", (n as f64) / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_all() {
        assert_eq!(parse_duration_spec("30m").unwrap(), Some(1_800));
        assert_eq!(parse_duration_spec("1h").unwrap(), Some(3_600));
        assert_eq!(parse_duration_spec("24h").unwrap(), Some(86_400));
        assert_eq!(parse_duration_spec("7d").unwrap(), Some(604_800));
        assert_eq!(parse_duration_spec("all").unwrap(), None);
        assert!(parse_duration_spec("0h").is_err());
        assert!(parse_duration_spec("10y").is_err());
    }

    #[test]
    fn filters_short_or_unreasonable_tps_samples() {
        assert_eq!(tps_bucket_index(99, 2_000), None);
        assert_eq!(tps_bucket_index(200, 499), None);
        assert_eq!(tps_bucket_index(5_000, 500), None); // 10,000 t/s > MAX_VALID_TPS
        assert_eq!(tps_bucket_index(2_001, 1_000), None); // 2001 t/s > MAX_VALID_TPS
        assert_eq!(tps_bucket_index(100, 2_000), Some(3)); // 50 t/s in [45, 60)
        assert_eq!(tps_bucket_index(400, 2_000), Some(8)); // 200 t/s in [160, 220)
        assert_eq!(tps_bucket_index(1_000, 1_000), Some(12)); // 1000 t/s in [800, 1300)
        assert_eq!(tps_bucket_index(2_000, 1_000), Some(13)); // 2000 t/s in [1300, 2000]
    }

    #[test]
    fn high_tps_histogram_percentiles_cover_full_valid_range() {
        let mut hist = [0_u64; TPS_HISTOGRAM_BUCKETS];
        hist[12] = 1; // 1000 t/s sample falls in [800, 1300)
        assert!((histogram_percentile(&hist, 0.50).unwrap() - 1050.0).abs() < 1e-9);
        assert!((histogram_percentile(&hist, 0.90).unwrap() - 1250.0).abs() < 1e-9);
        assert!((histogram_percentile(&hist, 0.95).unwrap() - 1275.0).abs() < 1e-9);

        let mut top_hist = [0_u64; TPS_HISTOGRAM_BUCKETS];
        top_hist[TPS_HISTOGRAM_BUCKETS - 1] = 1; // [1300, 2000]
        assert!((histogram_percentile(&top_hist, 0.50).unwrap() - 1650.0).abs() < 1e-9);
        assert!((histogram_percentile(&top_hist, 1.0).unwrap() - MAX_VALID_TPS).abs() < 1e-9);
    }

    #[test]
    fn quota_cost_histogram_top_bucket_matches_web_bounds() {
        let mut top_only = [0_u64; QUOTA_COST_HISTOGRAM_BUCKETS];
        top_only[QUOTA_COST_HISTOGRAM_BUCKETS - 1] = 10;
        let p50 = quota_cost_histogram_percentile(&top_only, 0.5).unwrap();
        let p90 = quota_cost_histogram_percentile(&top_only, 0.9).unwrap();
        let p95 = quota_cost_histogram_percentile(&top_only, 0.95).unwrap();
        let p100 = quota_cost_histogram_percentile(&top_only, 1.0).unwrap();
        assert!((p50 - 48.0).abs() < 1e-9);
        assert!((p90 - 60.8).abs() < 1e-9);
        assert!((p95 - 62.4).abs() < 1e-9);
        assert!((p100 - 64.0).abs() < 1e-9);

        // Sub-cent quota slices land in distinct low-cost buckets rather than collapsing together.
        assert_eq!(quota_cost_bucket_index(0.05, 0.0126), Some(0)); // $0.0025/1% -> [0, 0.005)
        assert_eq!(quota_cost_bucket_index(0.02, 0.0126), Some(1)); // $0.0063/1% -> [0.005, 0.01)
        assert_eq!(quota_cost_bucket_index(0.01, 0.0124), Some(2)); // $0.0124/1% -> [0.01, 0.02)
    }
}
