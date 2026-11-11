//! Configuration, the price table and cost arithmetic.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct Rate {
    pub hit: f64,
    pub miss: f64,
    pub output: f64,
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
    pub base_url: String,
    pub model: String,
    /// Fallback when DEEPSEEK_API_KEY is unset.
    pub api_key: Option<String>,
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
        // DeepSeek prices as published on 2026-11-11 (api-docs.deepseek.com/quick_start/pricing).
        let flash = Prices {
            peak: Rate { hit: 0.006, miss: 0.3, output: 1.2 },
            off_peak: Some(Rate { hit: 0.003, miss: 0.15, output: 0.6 }),
        };
        let pro = Prices {
            peak: Rate { hit: 0.044, miss: 1.32, output: 3.96 },
            off_peak: Some(Rate { hit: 0.022, miss: 0.66, output: 1.98 }),
        };
        let mut prices = BTreeMap::new();
        prices.insert("deepseek-flash".into(), flash.clone());
        // Legacy name, served and billed as deepseek-flash.
        prices.insert("deepseek-v4-flash".into(), flash);
        prices.insert("deepseek-v4-pro".into(), pro);
        Config {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            api_key: None,
            context_budget: 300_000,
            bash_timeout_s: 120,
            poc_timeout_s: 600,
            max_tokens: None,
            reasoning_effort: None,
            peak_hours_utc: vec![[1, 4], [6, 10]],
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
        match std::env::var("DEEPSEEK_API_KEY") {
            Ok(k) if !k.trim().is_empty() => Some(k.trim().to_string()),
            _ => self.api_key.clone().filter(|k| !k.is_empty()),
        }
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
        let (hit, miss) = if u.prompt_cache_hit_tokens + u.prompt_cache_miss_tokens == 0 {
            (0, u.prompt_tokens)
        } else {
            (u.prompt_cache_hit_tokens, u.prompt_cache_miss_tokens)
        };
        let micro =
            hit as f64 * rate.hit + miss as f64 * rate.miss + u.completion_tokens as f64 * rate.output;
        Some(micro / 1_000_000.0)
    }
}

/// The `usage` object of a DeepSeek response.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u64,
}

impl Usage {
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
