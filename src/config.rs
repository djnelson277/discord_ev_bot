//! Operator criteria, loaded from a TOML file with `EVBOT_`-prefixed env overrides.
//!
//! Every value an operator tunes lives here rather than in a compiled constant,
//! so the running service can pick up changes without a redeploy.

use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::Deserialize;
use std::path::Path;

/// The 14 sports the Owls Insight EV endpoint serves. A sport outside this set
/// cannot produce EV data, so accepting one would burn quota on guaranteed-empty
/// responses.
pub const EV_SPORTS: [&str; 14] = [
    "cs2", "dota2", "lol", "mlb", "mma", "nba", "ncaab", "ncaaf", "nfl", "nhl", "soccer", "tennis",
    "valorant", "wnba",
];

/// The books this operator holds funded accounts at. Alerting on anything else
/// is noise that trains people to ignore the channel.
pub const ALLOWED_BOOKS: [&str; 7] = [
    "betmgm",
    "fanduel",
    "draftkings",
    "bet365",
    "novig",
    "kalshi",
    "polymarket",
];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),
    #[error("could not read config: {0}")]
    Read(#[from] figment::Error),
    #[error("unknown sport {sport:?} — the EV endpoint serves only: {}", EV_SPORTS.join(", "))]
    UnknownSport { sport: String },
    #[error("unknown book {book:?} — configured books must be among: {}", ALLOWED_BOOKS.join(", "))]
    UnknownBook { book: String },
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_base_url")]
    pub owls_base_url: String,

    /// Alert only on EV strictly greater than this, in percent.
    #[serde(default = "default_min_ev_pct")]
    pub min_ev_pct: f64,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(default = "default_gating_refresh")]
    pub gating_refresh_secs: u64,

    #[serde(default = "default_lookahead_hours")]
    pub lookahead_hours: i64,

    #[serde(default)]
    pub sports: Vec<String>,

    #[serde(default)]
    pub books: Vec<String>,

    /// When set, reject opportunities whose consensus count is known and lower.
    /// An absent count still publishes — failing open and marked beats
    /// suppressing invisibly.
    #[serde(default)]
    pub min_books_in_consensus: Option<u32>,
}

fn default_base_url() -> String {
    "https://api.owlsinsight.com".to_string()
}
fn default_min_ev_pct() -> f64 {
    3.5
}
fn default_poll_interval() -> u64 {
    60
}
fn default_gating_refresh() -> u64 {
    3600
}
fn default_lookahead_hours() -> i64 {
    48
}

impl Config {
    /// Load from `path`, then apply `EVBOT_`-prefixed environment overrides.
    ///
    /// A missing file is an error rather than a silent fall-through to defaults:
    /// starting with compiled-in criteria when the operator expected their own
    /// is the kind of failure that looks like working software.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Err(ConfigError::NotFound(path.display().to_string()));
        }
        Self::from_figment(Figment::from(Toml::file(path)))
    }

    /// Parse config from a TOML string. Fields absent from the string take
    /// their defaults, so callers may supply only the keys under test.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        Self::from_figment(Figment::from(Toml::string(s)))
    }

    fn from_figment(f: Figment) -> Result<Self, ConfigError> {
        let cfg: Config = f.merge(Env::prefixed("EVBOT_")).extract()?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(sport) = self
            .sports
            .iter()
            .find(|s| !EV_SPORTS.contains(&s.as_str()))
        {
            return Err(ConfigError::UnknownSport {
                sport: sport.clone(),
            });
        }
        if let Some(book) = self
            .books
            .iter()
            .find(|b| !ALLOWED_BOOKS.contains(&b.as_str()))
        {
            return Err(ConfigError::UnknownBook { book: book.clone() });
        }
        Ok(())
    }
}
