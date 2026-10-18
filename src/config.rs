//! Configuration: the meter list (JSON) and runtime settings (environment variables).
//!
//! Adding or removing meters, or switching notification provider, only means editing
//! `config/meters.json` / `DESCO_METERS_JSON` or the environment. No code changes.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::domain::{AccountNo, Backend, Bdt, DomainError, Identifier, MeterNo};

pub const DEFAULT_METERS_PATH: &str = "config/meters.json";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read meter config {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("meter config is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("meter '{unit}': {source}")]
    Meter { unit: String, source: DomainError },
    #[error("meter '{0}' needs an account_no, a meter_no or an identifier")]
    NoIdentifier(String),
    #[error("duplicate unit_name '{0}'")]
    DuplicateUnit(String),
    #[error("no enabled meters in the configuration")]
    NoMeters,
    #[error("invalid value for {key}: '{value}'")]
    BadSetting { key: String, value: String },
}

/// One entry of `meters.json` exactly as written by the user.
#[derive(Debug, Deserialize)]
struct RawMeter {
    unit_name: String,
    /// Optional short label used in narrow WhatsApp tables (max 9 chars shown).
    short_name: Option<String>,
    account_no: Option<String>,
    meter_no: Option<String>,
    /// Either kind of number in one field. 12 digits is treated as a meter number.
    identifier: Option<String>,
    threshold_bdt: Option<f64>,
    notify_phone: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    backend_hint: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A validated meter entry.
#[derive(Debug, Clone)]
pub struct Meter {
    pub unit_name: String,
    pub short_name: Option<String>,
    pub account: Option<AccountNo>,
    pub meter: Option<MeterNo>,
    pub threshold: Bdt,
    pub notify_phone: Option<String>,
    pub enabled: bool,
    pub backend_hint: Option<Backend>,
}

impl Meter {
    fn from_raw(raw: RawMeter, default_threshold: Bdt) -> Result<Self, ConfigError> {
        let unit = raw.unit_name.trim().to_string();
        let wrap = |source| ConfigError::Meter {
            unit: unit.clone(),
            source,
        };

        let mut account = raw
            .account_no
            .as_deref()
            .map(AccountNo::parse)
            .transpose()
            .map_err(wrap)?;
        let mut meter = raw
            .meter_no
            .as_deref()
            .map(MeterNo::parse)
            .transpose()
            .map_err(wrap)?;
        if let Some(id) = raw.identifier.as_deref() {
            match Identifier::parse(id).map_err(wrap)? {
                Identifier::Account(a) => account = account.or(Some(a)),
                Identifier::Meter(m) => meter = meter.or(Some(m)),
            }
        }
        if account.is_none() && meter.is_none() {
            return Err(ConfigError::NoIdentifier(unit));
        }

        let backend_hint = raw
            .backend_hint
            .as_deref()
            .map(Backend::parse)
            .transpose()
            .map_err(wrap)?;

        Ok(Self {
            unit_name: unit.clone(),
            short_name: raw.short_name.filter(|s| !s.trim().is_empty()),
            account,
            meter,
            threshold: Bdt(raw.threshold_bdt.unwrap_or(default_threshold.0)),
            notify_phone: raw.notify_phone.filter(|p| !p.trim().is_empty()),
            enabled: raw.enabled,
            backend_hint,
        })
    }

    /// The identifier used for API calls. An account number works on every endpoint,
    /// so it wins when both are known.
    #[must_use]
    pub fn identifier(&self) -> Option<Identifier> {
        self.account
            .clone()
            .map(Identifier::Account)
            .or_else(|| self.meter.clone().map(Identifier::Meter))
    }

    /// Label for narrow tables.
    #[must_use]
    pub fn label(&self) -> &str {
        self.short_name.as_deref().unwrap_or(&self.unit_name)
    }
}

/// Parses the meter list. Entries with `enabled: false` stay in the list.
pub fn parse_meters(json: &str, default_threshold: Bdt) -> Result<Vec<Meter>, ConfigError> {
    let raw: Vec<RawMeter> = serde_json::from_str(json)?;
    let mut seen = BTreeSet::new();
    let mut meters = Vec::with_capacity(raw.len());
    for entry in raw {
        let meter = Meter::from_raw(entry, default_threshold)?;
        if !seen.insert(meter.unit_name.to_lowercase()) {
            return Err(ConfigError::DuplicateUnit(meter.unit_name));
        }
        meters.push(meter);
    }
    if !meters.iter().any(|m| m.enabled) {
        return Err(ConfigError::NoMeters);
    }
    Ok(meters)
}

/// Loads meters from `DESCO_METERS_JSON` if set, else from `path`
/// (default `config/meters.json`).
pub fn load_meters(
    env: &Env,
    path: Option<&Path>,
    default_threshold: Bdt,
) -> Result<Vec<Meter>, ConfigError> {
    if let Some(json) = env
        .get("DESCO_METERS_JSON")
        .filter(|j| !j.trim().is_empty())
    {
        return parse_meters(json, default_threshold);
    }
    let path = path.unwrap_or_else(|| Path::new(DEFAULT_METERS_PATH));
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    parse_meters(&text, default_threshold)
}

/// Environment variables, optionally topped up from a `.env` file. Real environment
/// variables always win over the file.
#[derive(Debug, Clone, Default)]
pub struct Env {
    vars: HashMap<String, String>,
}

impl Env {
    /// Reads the process environment and an optional `.env` file in the working directory.
    #[must_use]
    pub fn load() -> Self {
        let mut vars = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(".env") {
            vars.extend(parse_dotenv(&text));
        }
        vars.extend(std::env::vars());
        Self { vars }
    }

    #[must_use]
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
    }

    #[must_use]
    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    pub fn parse<T: FromStr>(&self, key: &str, default: T) -> Result<T, ConfigError> {
        match self.get(key) {
            None => Ok(default),
            Some(value) => value.trim().parse().map_err(|_| ConfigError::BadSetting {
                key: key.to_string(),
                value: value.to_string(),
            }),
        }
    }

    #[must_use]
    pub fn flag(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    }
}

fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            out.push((key.trim().to_string(), value.to_string()));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    WhatsAppMeta,
    WhatsAppWebhook,
    Telegram,
    Console,
}

impl Provider {
    fn parse(raw: &str) -> Result<Self, ConfigError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "whatsapp_meta" => Ok(Self::WhatsAppMeta),
            "whatsapp_webhook" => Ok(Self::WhatsAppWebhook),
            "telegram" => Ok(Self::Telegram),
            "console" => Ok(Self::Console),
            other => Err(ConfigError::BadSetting {
                key: "NOTIFY_PROVIDER".to_string(),
                value: other.to_string(),
            }),
        }
    }
}

/// Runtime tunables, all from environment variables with safe defaults.
#[derive(Debug, Clone)]
pub struct Settings {
    pub default_threshold: Bdt,
    /// Extra drop (BDT) since the last alert that triggers a follow-up alert.
    pub followup_drop: f64,
    /// Polite pause between meters during scheduled checks.
    pub rate_limit: Duration,
    /// Pause between meters for interactive (WhatsApp) queries.
    pub on_demand_delay: Duration,
    pub retries: u32,
    pub request_timeout: Duration,
    pub base_url: String,
    pub insecure_tls: bool,
    pub provider: Provider,
    pub notify_tenants: bool,
    pub state_dir: PathBuf,
    pub port: u16,
    pub allowed_senders: Vec<String>,
    pub balance_cache_ttl: Duration,
    pub info_cache_ttl: Duration,
    pub check_interval: Duration,
}

impl Settings {
    pub fn from_env(env: &Env) -> Result<Self, ConfigError> {
        let secs = |key: &str, default: u64| -> Result<Duration, ConfigError> {
            Ok(Duration::from_secs(env.parse(key, default)?))
        };
        let millis = |key: &str, default: u64| -> Result<Duration, ConfigError> {
            Ok(Duration::from_millis(env.parse(key, default)?))
        };
        let allowed_senders = env
            .get("ALLOWED_SENDERS")
            .map(|list| {
                list.split(',')
                    .map(digits_only)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            default_threshold: Bdt(env.parse("DEFAULT_THRESHOLD_BDT", 150.0)?),
            followup_drop: env.parse("FOLLOWUP_DROP_BDT", 25.0)?,
            rate_limit: millis("RATE_LIMIT_MS", 1000)?,
            on_demand_delay: millis("ON_DEMAND_DELAY_MS", 200)?,
            retries: env.parse("MAX_RETRIES", 2)?,
            request_timeout: secs("REQUEST_TIMEOUT_SECS", 20)?,
            base_url: env
                .get_or("DESCO_BASE_URL", "https://prepaid.desco.org.bd/api")
                .trim_end_matches('/')
                .to_string(),
            insecure_tls: env.flag("DESCO_INSECURE_TLS"),
            provider: Provider::parse(env.get_or("NOTIFY_PROVIDER", "console"))?,
            notify_tenants: env.flag("NOTIFY_TENANTS"),
            state_dir: PathBuf::from(env.get_or("STATE_DIR", "state")),
            port: env.parse("PORT", 8080)?,
            allowed_senders,
            balance_cache_ttl: secs("BALANCE_CACHE_SECS", 300)?,
            info_cache_ttl: secs("INFO_CACHE_SECS", 86_400)?,
            check_interval: secs("CHECK_INTERVAL_HOURS", 6)? * 3600,
        })
    }

    /// `true` when `sender` (any phone/chat-id format) may talk to the bot.
    /// With no `ALLOWED_SENDERS` configured everybody is allowed, but replies hide personal data.
    #[must_use]
    pub fn sender_allowed(&self, sender: &str) -> bool {
        if self.allowed_senders.is_empty() {
            return true;
        }
        let digits = digits_only(sender);
        self.allowed_senders.contains(&digits)
    }

    /// Customer name and phone are only shown to people on the allow-list.
    #[must_use]
    pub fn pii_allowed(&self) -> bool {
        !self.allowed_senders.is_empty()
    }
}

#[must_use]
pub fn digits_only(s: &str) -> String {
    // Chat ids look like "8801700000000@c.us"; keep only what precedes '@'.
    let head = s.split('@').next().unwrap_or(s);
    head.chars().filter(char::is_ascii_digit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
        {"unit_name": "Apt 1A", "account_no": "98765421", "meter_no": "667788990011", "enabled": true},
        {"unit_name": "Lift", "identifier": "DESCO-98765432", "threshold_bdt": 500.0, "notify_phone": null},
        {"unit_name": "Off", "identifier": "667788990011", "enabled": false, "backend_hint": "tkdes"}
    ]"#;

    #[test]
    fn parses_meters_with_defaults() {
        let meters = parse_meters(SAMPLE, Bdt(150.0)).expect("sample parses");
        assert_eq!(meters.len(), 3);
        assert!((meters[0].threshold.0 - 150.0).abs() < f64::EPSILON);
        assert!((meters[1].threshold.0 - 500.0).abs() < f64::EPSILON);
        assert_eq!(
            meters[1].account.as_ref().map(AccountNo::as_str),
            Some("98765432")
        );
        assert!(!meters[2].enabled);
        assert_eq!(meters[2].backend_hint, Some(Backend::Tkdes));
    }

    #[test]
    fn rejects_bad_entries() {
        let no_id = r#"[{"unit_name": "X"}]"#;
        assert!(matches!(
            parse_meters(no_id, Bdt(150.0)),
            Err(ConfigError::NoIdentifier(_))
        ));
        let dup = r#"[{"unit_name": "A", "identifier": "98765421"},{"unit_name": "a", "identifier": "98765422"}]"#;
        assert!(matches!(
            parse_meters(dup, Bdt(150.0)),
            Err(ConfigError::DuplicateUnit(_))
        ));
        let bad = r#"[{"unit_name": "A", "account_no": "12"}]"#;
        assert!(matches!(
            parse_meters(bad, Bdt(150.0)),
            Err(ConfigError::Meter { .. })
        ));
    }

    #[test]
    fn settings_defaults_and_allow_list() {
        let env = Env::from_pairs([("ALLOWED_SENDERS", "+880 1700-000000, 8801800000000")]);
        let settings = Settings::from_env(&env).expect("defaults parse");
        assert_eq!(settings.retries, 2);
        assert!(settings.sender_allowed("8801700000000@c.us"));
        assert!(!settings.sender_allowed("8801999999999"));
        assert!(settings.pii_allowed());
    }

    #[test]
    fn dotenv_lines_parse() {
        let pairs = parse_dotenv("# c\nA=1\nexport B=\"two\"\n\nC = 'x y'\n");
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two".to_string()),
                ("C".to_string(), "x y".to_string())
            ]
        );
    }
}
