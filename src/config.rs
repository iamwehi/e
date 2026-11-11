//! Configuration, the price table and cost arithmetic.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// USD per 1M tokens. `write` is for prompt tokens written to the cache
/// (some Venice models charge a premium); absent, they cost `miss`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct Rate {
    pub hit: f64,
    pub miss: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Prices {
    #[serde(flatten)]
    pub peak: Rate,
    /// Rates outside `peak_hours_utc`. Absent: `peak` applies at all times.
    #[serde(default)]
    pub off_peak: Option<Rate>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Any OpenAI-compatible chat completions API; Venice by default.
    pub base_url: String,
    pub model: String,
    /// Environment variable holding the API key.
    pub api_key_env: String,
    /// Fallback when the `api_key_env` variable is unset.
    pub api_key: Option<String>,
    /// Extra top-level request fields, merged into every request body.
    pub extra_body: Map<String, Value>,
    /// Send a per-session `prompt_cache_key` so requests reach a warm cache.
    pub prompt_cache_key: bool,
    /// `prompt_cache_retention` value, sent only for models whose id starts
    /// with one of `prompt_cache_retention_models`.
    pub prompt_cache_retention: Option<String>,
    pub prompt_cache_retention_models: Vec<String>,
    /// Context tokens per run (prompt + completion of the latest response).
    pub context_budget: u64,
    pub bash_timeout_s: u64,
    pub poc_timeout_s: u64,
    pub max_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    /// Peak windows [start, end) in UTC hours, Monday to Friday.
    pub peak_hours_utc: Vec<[u32; 2]>,
    pub prices: BTreeMap<String, Prices>,
}

impl Default for Config {
    fn default() -> Self {
        // Venice prices from GET /models on 2026-11-11: input (cache miss),
        // cache_input (cache hit) and output, USD per 1M tokens. None of these
        // models has a cache_write price or an extended-context tier.
        let rate =
            |miss, hit, output| Prices { peak: Rate { hit, miss, output, write: None }, off_peak: None };
        let mut prices = BTreeMap::new();
        for (model, p) in [
            ("deepseek-v4-1-flash", rate(0.3, 0.0075, 1.2)),
            ("deepseek-v4-1-flash-1017", rate(0.3, 0.0075, 1.2)),
            ("deepseek-v4-1-flash-uncensored", rate(0.33, 0.00825, 1.32)),
            ("deepseek-v4-1-flash-1017-uncensored", rate(0.33, 0.00825, 1.32)),
            ("deepseek-v4-1-flash-1017-ultrafast", rate(3.0, 0.06, 12.0)),
            ("deepseek-v4-flash", rate(0.138, 0.028, 0.275)),
            ("deepseek-v4-flash-0731", rate(0.13, 0.028, 0.26)),
            ("deepseek-v4-flash-0731-fast", rate(0.35, 0.0875, 0.7)),
            ("e2ee-deepseek-v4-flash", rate(0.182, 0.038, 0.373)),
            ("e2ee-deepseek-v4-1-flash-1017-abliterated", rate(0.375, 0.0075, 1.5)),
            ("deepseek-v4-pro", rate(1.65, 0.33, 3.301)),
            ("deepseek-v4-pro-0813", rate(1.65, 0.165, 4.95)),
        ] {
            prices.insert(model.to_string(), p);
        }
        let mut extra_body = Map::new();
        // Venice otherwise adds its own system prompt next to ours.
        extra_body.insert("venice_parameters".into(), json!({"include_venice_system_prompt": false}));
        Config {
            base_url: "https://api.venice.ai/api/v1".into(),
            model: "deepseek-v4-1-flash".into(),
            api_key_env: "VENICE_API_KEY".into(),
            api_key: None,
            extra_body,
            prompt_cache_key: true,
            // Venice honours "extended" (24 h instead of minutes) only on the
            // models it serves from OpenAI (changelog, 2026). Other models
            // ignore it; gpt-oss runs elsewhere, hence no plain "openai-gpt-".
            prompt_cache_retention: Some("extended".into()),
            prompt_cache_retention_models: vec!["openai-gpt-5".into(), "openai-gpt-6".into()],
            context_budget: 300_000,
            bash_timeout_s: 120,
            poc_timeout_s: 600,
            max_tokens: None,
            reasoning_effort: None,
            peak_hours_utc: Vec::new(),
            prices,
        }
    }
}

/// `$XDG_CONFIG_HOME/e` or `~/.config/e`.
pub fn config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("e"))
}

impl Config {
    /// `$E_CONFIG`, else `config.json` in [`config_dir`].
    pub fn path() -> Option<PathBuf> {
        match std::env::var_os("E_CONFIG") {
            Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
            _ => Some(config_dir()?.join("config.json")),
        }
    }

    /// Defaults, overridden field by field by the config file if it exists.
    pub fn load() -> Result<Config, String> {
        let Some(path) = Self::path() else { return Ok(Config::default()) };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn api_key(&self) -> Option<String> {
        match std::env::var(&self.api_key_env) {
            Ok(k) if !k.trim().is_empty() => Some(k.trim().to_string()),
            _ => self.api_key.clone().filter(|k| !k.is_empty()),
        }
    }

    /// The `prompt_cache_retention` to send for `model`, if any.
    pub fn cache_retention(&self, model: &str) -> Option<&str> {
        let supported = self.prompt_cache_retention_models.iter().any(|p| model.starts_with(p.as_str()));
        self.prompt_cache_retention.as_deref().filter(|_| supported)
    }

    pub fn is_peak(&self, unix: u64) -> bool {
        let t = crate::transcript::civil(unix);
        let weekday = (1..=5).contains(&t.weekday);
        weekday && self.peak_hours_utc.iter().any(|[a, b]| (*a..*b).contains(&t.hour))
    }

    /// Cost of one response in USD, or `None` if the model has no prices.
    pub fn cost(&self, model: &str, u: &Usage, unix: u64) -> Option<f64> {
        let p = self.prices.get(model)?;
        let rate = match p.off_peak {
            Some(off) if !self.is_peak(unix) => off,
            _ => p.peak,
        };
        let (hit, write, miss) = u.split();
        let micro = hit as f64 * rate.hit
            + write as f64 * rate.write.unwrap_or(rate.miss)
            + miss as f64 * rate.miss
            + u.completion_tokens as f64 * rate.output;
        Some(micro / 1_000_000.0)
    }
}

/// The `usage` object of a response. Venice (like OpenAI) reports cache hits
/// in `prompt_tokens_details.cached_tokens`; DeepSeek's own API reports
/// `prompt_cache_hit_tokens` and `prompt_cache_miss_tokens`.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
pub struct PromptDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

impl Usage {
    /// Prompt tokens read from cache, written to cache, and neither.
    pub fn split(&self) -> (u64, u64, u64) {
        if let (Some(hit), Some(miss)) = (self.prompt_cache_hit_tokens, self.prompt_cache_miss_tokens) {
            return (hit, 0, miss);
        }
        let details = self.prompt_tokens_details.unwrap_or_default();
        let hit = details.cached_tokens.or(self.prompt_cache_hit_tokens).unwrap_or(0).min(self.prompt_tokens);
        let write = details.cache_creation_input_tokens.unwrap_or(0).min(self.prompt_tokens - hit);
        (hit, write, self.prompt_tokens - hit - write)
    }

    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// Running session cost. Tokens from unpriced models are counted, not costed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Meter {
    pub cost: f64,
    pub unpriced_tokens: u64,
}

impl Meter {
    pub fn add(&mut self, u: &Usage, cost: Option<f64>) {
        match cost {
            Some(c) => self.cost += c,
            None => self.unpriced_tokens += u.total(),
        }
    }

    pub fn display(&self) -> String {
        let (t, c) = (self.unpriced_tokens, self.cost);
        if t == 0 {
            format!("${c:.4}")
        } else if c == 0.0 {
            fmt_tokens(t)
        } else {
            format!("${c:.4}+{}", fmt_tokens(t))
        }
    }
}

pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => format!("{n} tok"),
        1_000..=999_999 => format!("{:.1}k tok", n as f64 / 1e3),
        _ => format!("{:.2}M tok", n as f64 / 1e6),
    }
}
