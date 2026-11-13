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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub enabled: bool,
    pub db_path: Option<String>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_path: None,
        }
    }
}

impl StorageConfig {
    /// Resolves the SQLite database path, expanding leading `~/` when configured.
    pub fn resolved_db_path(&self) -> Option<PathBuf> {
        if let Some(raw) = self.db_path.as_deref().filter(|s| !s.is_empty()) {
            return Some(expand_tilde(raw));
        }
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
            && !xdg.is_empty()
        {
            return Some(
                PathBuf::from(xdg)
                    .join("antigravity-status")
                    .join("history.db"),
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
                    .join("history.db"),
            );
        }
        None
    }
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

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ModulesConfig {
    pub context_gauge: ContextGaugeConfig,
    pub session_input_tokens: TokenModuleConfig<InputDefault>,
    pub session_cache_read_tokens: TokenModuleConfig<CacheReadDefault>,
    pub session_cache_write_tokens: TokenModuleConfig<CacheWriteDefault>,
    pub session_output_tokens: TokenModuleConfig<OutputDefault>,
    pub session_cost: CostModuleConfig,
    pub model_and_effort: ModelAndEffortConfig,
    pub mode: ModeModuleConfig,
    pub quota_5h_gauge: QuotaGaugeConfig<Quota5hDefault>,
    pub quota_weekly_gauge: QuotaGaugeConfig<QuotaWeeklyDefault>,
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

pub trait DefaultLabel {
    fn default_label() -> &'static str;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct InputDefault;
impl DefaultLabel for InputDefault {
    fn default_label() -> &'static str {
        "In"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheReadDefault;
impl DefaultLabel for CacheReadDefault {
    fn default_label() -> &'static str {
        "CacheR"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheWriteDefault;
impl DefaultLabel for CacheWriteDefault {
    fn default_label() -> &'static str {
        "CacheW"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OutputDefault;
impl DefaultLabel for OutputDefault {
    fn default_label() -> &'static str {
        "Out"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Quota5hDefault;
impl DefaultLabel for Quota5hDefault {
    fn default_label() -> &'static str {
        "5h"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct QuotaWeeklyDefault;
impl DefaultLabel for QuotaWeeklyDefault {
    fn default_label() -> &'static str {
        "Wk"
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TokenModuleConfig<L: DefaultLabel> {
    pub enabled: bool,
    pub label: String,
    #[serde(skip)]
    _marker: std::marker::PhantomData<L>,
}

impl<L: DefaultLabel> Default for TokenModuleConfig<L> {
    fn default() -> Self {
        Self {
            enabled: true,
            label: L::default_label().to_string(),
            _marker: std::marker::PhantomData,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaGaugeConfig<L: DefaultLabel> {
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
    #[serde(skip)]
    _marker: std::marker::PhantomData<L>,
}

impl<L: DefaultLabel> Default for QuotaGaugeConfig<L> {
    fn default() -> Self {
        Self {
            enabled: true,
            label: L::default_label().to_string(),
            width: 6,
            decimals: 1,
            warn_percent: 30.0,
            critical_percent: 10.0,
            filled_char: "█".to_string(),
            empty_char: "░".to_string(),
            hide_missing: true,
            _marker: std::marker::PhantomData,
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

/// Resolves and parses `config.toml` according to CLI, env, project, and user precedence.
pub fn load_config(explicit_path: Option<&Path>, payload_cwd: Option<&str>) -> Config {
    if let Some(path) = explicit_path {
        return parse_config_file(path).unwrap_or_default();
    }

    if let Ok(env_path) = std::env::var("AGY_STATUS_CONFIG")
        && !env_path.is_empty()
    {
        let path = PathBuf::from(env_path);
        if path.exists() {
            return parse_config_file(&path).unwrap_or_default();
        }
    }

    let project_dir = payload_cwd
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    if let Some(dir) = project_dir {
        let local_cfg = dir.join(".antigravity-status.toml");
        if local_cfg.is_file() {
            return parse_config_file(&local_cfg).unwrap_or_default();
        }
    }

    if let Some(global_cfg) = default_global_config_path()
        && global_cfg.is_file()
    {
        return parse_config_file(&global_cfg).unwrap_or_default();
    }

    Config::default()
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

fn parse_config_file(path: &Path) -> Option<Config> {
    let raw = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!("antigravity-status: failed to read {}: {err}", path.display());
            return None;
        }
    };

    match parse_config_str(&raw) {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            eprintln!("antigravity-status: invalid TOML in {}: {err}", path.display());
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
        assert!(cfg.storage.enabled);
        assert!(cfg.modules.context_gauge.enabled);
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
        assert_eq!(cfg.general.order.len(), 10);
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
"#;
        let cfg = parse_config_str(raw).expect("partial TOML should parse");
        assert!(cfg.modules.context_gauge.enabled);
        assert!(cfg.modules.session_input_tokens.enabled);
        assert!(cfg.modules.session_cache_read_tokens.enabled);
        assert!(!cfg.modules.session_cache_write_tokens.enabled);
        assert!(cfg.modules.session_output_tokens.enabled);
        assert!(!cfg.modules.session_cost.enabled);
        assert!(cfg.modules.model_and_effort.enabled);
        assert!(!cfg.modules.model_and_effort.include_effort);
        assert!(cfg.modules.mode.enabled);
        assert!(cfg.modules.quota_5h_gauge.enabled);
        assert!(!cfg.modules.quota_weekly_gauge.enabled);
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
            cfg.pricing.rates_for_model(Some("Gemini 4 Hydrogen (High)")).input_per_million,
            3.5
        );
        assert_eq!(
            cfg.pricing.rates_for_model(Some("Unknown Model")).input_per_million,
            1.0
        );
    }
}
