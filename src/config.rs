use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub format: FormatConfig,
    pub storage: StorageConfig,
    pub telemetry: TelemetryConfig,
    pub modules: ModulesConfig,
    pub pricing: PricingConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Alignment {
    Left,
    #[default]
    Right,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub separator: String,
    pub align: Alignment,
    pub right_margin: usize,
    pub order: Vec<String>,
    pub lines: Vec<Vec<String>>,
    pub hide_zero_cache_write: bool,
    pub hide_missing_cost: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            separator: " │ ".to_string(),
            align: Alignment::Right,
            right_margin: 0,
            order: vec![
                "mode".to_string(),
                "context_gauge".to_string(),
                "session_input_tokens".to_string(),
                "session_cache_read_tokens".to_string(),
                "session_cache_write_tokens".to_string(),
                "session_output_tokens".to_string(),
                "session_estimated_tps".to_string(),
                "session_cost".to_string(),
                "model_and_effort".to_string(),
                "quota_5h_gauge".to_string(),
                "quota_weekly_gauge".to_string(),
            ],
            lines: Vec::new(),
            hide_zero_cache_write: false,
            hide_missing_cost: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FormatConfig {
    pub compact_tokens: bool,
    pub cost_precision: usize,
    pub color: bool,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            compact_tokens: true,
            cost_precision: 4,
            color: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub db_path: Option<String>,
}

impl StorageConfig {
    /// Resolves the SQLite database path, anchoring relative `db_path` values to `base_dir` when provided.
    pub fn resolved_db_path_with_base(&self, base_dir: Option<&Path>) -> Option<PathBuf> {
        if let Some(raw) = self.db_path.as_deref().filter(|s| !s.is_empty()) {
            let expanded = expand_tilde(raw);
            if expanded.is_relative()
                && let Some(base) = base_dir.filter(|p| !p.as_os_str().is_empty())
            {
                return Some(base.join(expanded));
            }
            return Some(expanded);
        }
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
            && !xdg.is_empty()
        {
            return Some(
                PathBuf::from(xdg)
                    .join("antigravity-status")
                    .join("default.db"),
            );
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("antigravity-status")
                    .join("default.db"),
            );
        }
        None
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Strict opt-in toggle. Nothing is ever uploaded unless explicitly set to `true`.
    pub enabled: bool,
    /// Worker base URL or full `/api/telemetry/ingest` URL.
    pub endpoint: String,
    /// When `enabled = true`, trigger throttled background uploads from statusline ticks.
    pub auto_upload: bool,
    /// Minimum cooldown seconds before retrying or re-checking background uploads (default: 300).
    pub upload_interval_secs: u64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            auto_upload: true,
            upload_interval_secs: 300,
        }
    }
}

impl TelemetryConfig {
    /// Resolves the normalized `/api/telemetry/ingest` endpoint URL when configured.
    ///
    /// Requires `https://`, or `http://` restricted strictly to loopback hosts
    /// (`localhost`, `127.0.0.1`, `[::1]`), with no leading `-`, userinfo, or whitespace.
    pub fn resolved_ingest_url(&self) -> Option<String> {
        let (_, trimmed) = validate_endpoint_url(&self.endpoint)?;
        if trimmed.ends_with("/api/telemetry/ingest") {
            Some(trimmed.to_string())
        } else {
            Some(format!("{trimmed}/api/telemetry/ingest"))
        }
    }
}

/// Validates a telemetry endpoint URL and returns the allowed curl `--proto` specifier and trimmed URL.
pub fn validate_endpoint_url(raw: &str) -> Option<(&'static str, &str)> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let (is_http, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        (false, rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        (true, rest)
    } else {
        return None;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port_valid) = split_authority_host_port(authority)?;
    if !port_valid || (is_http && !is_loopback_host(host)) {
        return None;
    }
    Some((if is_http { "=http" } else { "=https" }, trimmed))
}

fn split_authority_host_port(authority: &str) -> Option<(&str, bool)> {
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(after_open) = authority.strip_prefix('[') {
        let close_idx = after_open.find(']')?;
        let host = &after_open[..close_idx];
        if host.is_empty() {
            return None;
        }
        let remainder = &after_open[close_idx + 1..];
        let port_valid = if remainder.is_empty() {
            true
        } else if let Some(port) = remainder.strip_prefix(':') {
            !port.is_empty() && port.parse::<u16>().is_ok()
        } else {
            false
        };
        return Some((&authority[..=close_idx + 1], port_valid));
    }
    if let Some((host, port)) = authority.split_once(':') {
        if host.is_empty() {
            return None;
        }
        let port_valid = !port.is_empty() && port.parse::<u16>().is_ok();
        Some((host, port_valid))
    } else {
        Some((authority, true))
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]"
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModulesConfig {
    pub context_gauge: ContextGaugeConfig,
    #[serde(deserialize_with = "de_token_in")]
    pub session_input_tokens: TokenModuleConfig,
    #[serde(deserialize_with = "de_token_cache_r")]
    pub session_cache_read_tokens: TokenModuleConfig,
    #[serde(deserialize_with = "de_token_cache_w")]
    pub session_cache_write_tokens: TokenModuleConfig,
    #[serde(deserialize_with = "de_token_out")]
    pub session_output_tokens: TokenModuleConfig,
    pub session_estimated_tps: TpsModuleConfig,
    pub session_cost: CostModuleConfig,
    pub model_and_effort: ModelAndEffortConfig,
    pub mode: ModeModuleConfig,
    #[serde(deserialize_with = "de_quota_5h")]
    pub quota_5h_gauge: QuotaGaugeConfig,
    #[serde(deserialize_with = "de_quota_weekly")]
    pub quota_weekly_gauge: QuotaGaugeConfig,
}

impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            context_gauge: ContextGaugeConfig::default(),
            session_input_tokens: TokenModuleConfig::with_label("In"),
            session_cache_read_tokens: TokenModuleConfig::with_label("CacheR"),
            session_cache_write_tokens: TokenModuleConfig::with_label("CacheW"),
            session_output_tokens: TokenModuleConfig::with_label("Out"),
            session_estimated_tps: TpsModuleConfig::default(),
            session_cost: CostModuleConfig::default(),
            model_and_effort: ModelAndEffortConfig::default(),
            mode: ModeModuleConfig::default(),
            quota_5h_gauge: QuotaGaugeConfig::with_label("5h"),
            quota_weekly_gauge: QuotaGaugeConfig::with_label("Wk"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ContextGaugeConfig {
    pub enabled: bool,
    pub label: String,
    pub width: usize,
    pub warn_percent: f64,
    pub critical_percent: f64,
    pub filled_char: String,
    pub empty_char: String,
}

impl Default for ContextGaugeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "Ctx".to_string(),
            width: 10,
            warn_percent: 70.0,
            critical_percent: 90.0,
            filled_char: "█".to_string(),
            empty_char: "░".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TokenModuleConfig {
    pub enabled: bool,
    pub label: String,
}

impl Default for TokenModuleConfig {
    fn default() -> Self {
        Self::with_label("")
    }
}

impl TokenModuleConfig {
    fn with_label(label: &str) -> Self {
        Self {
            enabled: true,
            label: label.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaGaugeConfig {
    pub enabled: bool,
    pub label: String,
    pub width: usize,
    pub decimals: usize,
    /// Remaining percentage at or below which the gauge turns yellow.
    pub warn_percent: f64,
    /// Remaining percentage at or below which the gauge turns red.
    pub critical_percent: f64,
    pub filled_char: String,
    pub empty_char: String,
    pub hide_missing: bool,
}

impl Default for QuotaGaugeConfig {
    fn default() -> Self {
        Self::with_label("")
    }
}

impl QuotaGaugeConfig {
    fn with_label(label: &str) -> Self {
        Self {
            enabled: true,
            label: label.to_string(),
            width: 6,
            decimals: 1,
            warn_percent: 30.0,
            critical_percent: 10.0,
            filled_char: "█".to_string(),
            empty_char: "░".to_string(),
            hide_missing: true,
        }
    }
}

macro_rules! de_labeled {
    ($fn_name:ident, $ty:ty, $label:expr) => {
        fn $fn_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<$ty, D::Error> {
            #[derive(Deserialize)]
            struct Patch {
                label: Option<String>,
                #[serde(flatten)]
                rest: $ty,
            }
            let p = Patch::deserialize(d)?;
            let mut out = p.rest;
            out.label = p.label.unwrap_or_else(|| $label.to_string());
            Ok(out)
        }
    };
}

de_labeled!(de_token_in, TokenModuleConfig, "In");
de_labeled!(de_token_cache_r, TokenModuleConfig, "CacheR");
de_labeled!(de_token_cache_w, TokenModuleConfig, "CacheW");
de_labeled!(de_token_out, TokenModuleConfig, "Out");
de_labeled!(de_quota_5h, QuotaGaugeConfig, "5h");
de_labeled!(de_quota_weekly, QuotaGaugeConfig, "Wk");

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TpsModuleConfig {
    pub enabled: bool,
    pub label: String,
    pub prefix: String,
    pub unit: String,
    pub decimals: usize,
    pub hide_missing: bool,
}

impl Default for TpsModuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "TPS".to_string(),
            prefix: "~".to_string(),
            unit: "t/s".to_string(),
            decimals: 1,
            hide_missing: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CostModuleConfig {
    pub enabled: bool,
    pub label: String,
    pub show_subagent: bool,
    /// Estimate USD cost locally from token counts when `agy` omits `cost` (e.g. subscription plans).
    pub estimate_when_missing: bool,
    pub estimated_prefix: String,
}

impl Default for CostModuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "Cost".to_string(),
            show_subagent: false,
            estimate_when_missing: true,
            estimated_prefix: "~".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelAndEffortConfig {
    pub enabled: bool,
    pub label: String,
    /// Show effort inline as `Model (High)` when `effort` is reported.
    pub include_effort: bool,
}

impl Default for ModelAndEffortConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            label: String::new(),
            include_effort: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModeModuleConfig {
    pub enabled: bool,
    pub label: String,
}

impl Default for ModeModuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "Mode:".to_string(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct PricingConfig {
    pub default: TokenRates,
    pub models: BTreeMap<String, TokenRates>,
}

impl PricingConfig {
    /// Finds the best matching per-1M token rate table for the active model.
    pub fn rates_for_model(&self, model_name: Option<&str>) -> &TokenRates {
        if let Some(name) = model_name {
            let lower = name.to_ascii_lowercase();
            // Prefer the longest matching pattern for deterministic specificity.
            let best = self
                .models
                .iter()
                .filter(|(pattern, _)| lower.contains(&pattern.to_ascii_lowercase()))
                .max_by_key(|(pattern, _)| pattern.len());
            if let Some((_, rates)) = best {
                return rates;
            }
        }
        &self.default
    }
}

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct TokenRates {
    pub input_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
    pub output_per_million: f64,
}

impl Default for TokenRates {
    fn default() -> Self {
        Self {
            input_per_million: 2.00,
            cache_read_per_million: 0.20,
            cache_write_per_million: 2.50,
            output_per_million: 12.00,
        }
    }
}

impl TokenRates {
    /// Computes USD cost from token counts using these per-1M token rates.
    pub fn cost_usd(
        &self,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) -> f64 {
        const PER_M: f64 = 1_000_000.0;
        (input_tokens as f64 * self.input_per_million
            + cache_read_tokens as f64 * self.cache_read_per_million
            + cache_write_tokens as f64 * self.cache_write_per_million
            + output_tokens as f64 * self.output_per_million)
            / PER_M
    }
}

/// Resolves and parses `config.toml`, returning both the parsed [`Config`] and the active file path (if any).
/// Loads configuration, returning an explicit error if `--config` cannot be read or parsed.
///
/// Project-local `.antigravity-status.toml` files are intentionally stripped of `[telemetry]`
/// settings so untrusted repositories cannot override telemetry opt-in or endpoints.
pub fn try_load_config_with_path(
    explicit_path: Option<&Path>,
    payload_cwd: Option<&str>,
) -> Result<(Config, Option<PathBuf>), String> {
    if let Some(path) = explicit_path {
        let cfg = try_parse_config_file(path)?;
        return Ok((cfg, Some(path.to_path_buf())));
    }

    if let Ok(env_path) = std::env::var("AGY_STATUS_CONFIG")
        && !env_path.is_empty()
    {
        let path = PathBuf::from(env_path);
        if path.is_file() {
            return Ok((parse_config_file(&path).unwrap_or_default(), Some(path)));
        }
    }

    let project_dir = payload_cwd
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    if let Some(dir) = project_dir {
        let local_cfg = dir.join(".antigravity-status.toml");
        if local_cfg.is_file() {
            let mut cfg = parse_config_file(&local_cfg).unwrap_or_default();
            cfg.telemetry = TelemetryConfig::default();
            return Ok((cfg, Some(local_cfg)));
        }
    }

    if let Some(global_cfg) = default_global_config_path()
        && global_cfg.is_file()
    {
        return Ok((
            parse_config_file(&global_cfg).unwrap_or_default(),
            Some(global_cfg),
        ));
    }

    Ok((Config::default(), None))
}

pub fn default_global_config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(
            PathBuf::from(xdg)
                .join("antigravity-status")
                .join("config.toml"),
        );
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("antigravity-status")
                .join("config.toml"),
        );
    }
    None
}

pub fn parse_config_str(raw: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(raw)
}

fn try_parse_config_file(path: &Path) -> Result<Config, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    parse_config_str(&raw).map_err(|err| format!("invalid TOML in {}: {err}", path.display()))
}

fn parse_config_file(path: &Path) -> Option<Config> {
    match try_parse_config_file(path) {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            eprintln!("antigravity-status: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_cleanly() {
        let raw = include_str!("../config.example.toml");
        let cfg = parse_config_str(raw).expect("config.example.toml should parse");
        assert_eq!(cfg.storage.db_path, None);
        // Legacy `storage.enabled` keys in older user configs are ignored rather than failing.
        let legacy = parse_config_str("[storage]\nenabled = false\n")
            .expect("legacy storage.enabled key should be ignored cleanly");
        assert_eq!(legacy.storage.db_path, None);
        assert!(!cfg.telemetry.enabled);
        assert!(cfg.telemetry.auto_upload);
        assert_eq!(cfg.telemetry.upload_interval_secs, 300);
        assert!(cfg.modules.context_gauge.enabled);
        assert!(cfg.modules.session_estimated_tps.enabled);
        assert!(cfg.modules.session_cost.enabled);
        assert!(cfg.modules.session_cost.estimate_when_missing);
        assert!(cfg.modules.model_and_effort.enabled);
        assert!(cfg.modules.model_and_effort.include_effort);
        assert!(cfg.modules.mode.enabled);
        assert!(cfg.modules.quota_5h_gauge.enabled);
        assert!(cfg.modules.quota_weekly_gauge.enabled);
        assert_eq!(cfg.modules.mode.label, "Mode:");
        assert_eq!(cfg.general.align, Alignment::Right);
        assert_eq!(cfg.general.order.first().map(String::as_str), Some("mode"));
        assert_eq!(cfg.general.order.len(), 11);
    }

    #[test]
    fn partial_config_toggles_one_module_and_preserves_defaults() {
        let raw = r#"
[modules.session_cache_write_tokens]
enabled = false

[modules.session_cost]
enabled = false

[modules.model_and_effort]
include_effort = false

[modules.quota_weekly_gauge]
enabled = false
width = 8
"#;
        let cfg = parse_config_str(raw).expect("partial TOML should parse");
        assert!(cfg.modules.context_gauge.enabled);
        assert!(cfg.modules.session_input_tokens.enabled);
        assert!(cfg.modules.session_cache_read_tokens.enabled);
        assert!(!cfg.modules.session_cache_write_tokens.enabled);
        assert_eq!(cfg.modules.session_cache_write_tokens.label, "CacheW");
        assert!(cfg.modules.session_output_tokens.enabled);
        assert!(cfg.modules.session_estimated_tps.enabled);
        assert_eq!(cfg.modules.session_estimated_tps.label, "TPS");
        assert!(!cfg.modules.session_cost.enabled);
        assert!(cfg.modules.model_and_effort.enabled);
        assert!(!cfg.modules.model_and_effort.include_effort);
        assert!(cfg.modules.mode.enabled);
        assert!(cfg.modules.quota_5h_gauge.enabled);
        assert!(!cfg.modules.quota_weekly_gauge.enabled);
        assert_eq!(cfg.modules.quota_weekly_gauge.width, 8);
        assert_eq!(cfg.modules.quota_weekly_gauge.label, "Wk");
        assert_eq!(cfg.modules.session_input_tokens.label, "In");
    }

    #[test]
    fn pricing_matches_longest_model_substring() {
        let raw = r#"
[pricing.default]
input_per_million = 1.0

[pricing.models."gemini 4"]
input_per_million = 2.5

[pricing.models."gemini 4 hydrogen"]
input_per_million = 3.5
"#;
        let cfg = parse_config_str(raw).unwrap();
        assert_eq!(
            cfg.pricing
                .rates_for_model(Some("Gemini 4 Hydrogen (High)"))
                .input_per_million,
            3.5
        );
        assert_eq!(
            cfg.pricing
                .rates_for_model(Some("Unknown Model"))
                .input_per_million,
            1.0
        );
    }

    #[test]
    fn resolved_ingest_url_enforces_https_and_strict_loopback_http() {
        let valid_https = TelemetryConfig {
            endpoint: "https://metrics.example.com/".to_string(),
            ..Default::default()
        };
        assert_eq!(
            valid_https.resolved_ingest_url().as_deref(),
            Some("https://metrics.example.com/api/telemetry/ingest")
        );

        let valid_local = TelemetryConfig {
            endpoint: "http://127.0.0.1:8787/api/telemetry/ingest".to_string(),
            ..Default::default()
        };
        assert_eq!(
            valid_local.resolved_ingest_url().as_deref(),
            Some("http://127.0.0.1:8787/api/telemetry/ingest")
        );

        for invalid in [
            "http://example.com",
            "http://localhost.evil.com:8787",
            "http://127.0.0.2:8787",
            "https://user@example.com",
            "https:///missing-host",
            "-flag-injection",
            "https://bad host.example",
            "ftp://example.com",
        ] {
            let cfg = TelemetryConfig {
                endpoint: invalid.to_string(),
                ..Default::default()
            };
            assert_eq!(
                cfg.resolved_ingest_url(),
                None,
                "expected {invalid} to be rejected"
            );
        }
    }

    #[test]
    fn try_load_config_errors_on_missing_explicit_file_and_strips_local_telemetry() {
        let uniq = format!(
            "agy-cfg-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(uniq);
        std::fs::create_dir_all(&root).unwrap();

        let missing = root.join("does-not-exist.toml");
        assert!(try_load_config_with_path(Some(&missing), None).is_err());

        let local_cfg = root.join(".antigravity-status.toml");
        std::fs::write(
            &local_cfg,
            r#"
[telemetry]
enabled = true
endpoint = "https://attacker.invalid"
"#,
        )
        .unwrap();
        let (loaded, path) = try_load_config_with_path(None, root.to_str()).unwrap();
        assert_eq!(path.as_deref(), Some(local_cfg.as_path()));
        assert!(!loaded.telemetry.enabled);
        assert!(loaded.telemetry.endpoint.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }
}
