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

/// Book keys that can appear as an EV opportunity — a fact about the API, not
/// a preference.
///
/// This deliberately does NOT encode which books the operator uses; that is
/// `Config::books`, which belongs in config so adding a newly-opened account is
/// a one-line change rather than a code edit and redeploy. This list exists
/// only to catch typos like `betfair` or `caesers`.
///
/// Sampled from the live consensus `/odds` board across six sports, plus the
/// two prediction markets that require `venues=true`. Books outside it are v2
/// Source API only: raw per-book feeds with no EV computed, so configuring one
/// could never produce an alert.
pub const EV_CAPABLE_BOOKS: [&str; 18] = [
    // consensus board
    "1xbet",
    "betmgm",
    "betr",
    "bovada",
    "caesars",
    "circa",
    "draftkings",
    "fanduel",
    "hardrock",
    "novig",
    "south_point",
    "stations",
    "westgate",
    "wynn",
    // accepted by the ev.found webhook allowlist, though absent from the
    // consensus board sample — may not be reachable via the poller
    "bet365",
    // prediction markets, require venues=true
    "kalshi",
    "polymarket",
    // the fair-value anchor; rejected with its own message below
    "pinnacle",
];

/// Pinnacle is the fair every other price is scored against, so it can never be
/// offered as value against itself. Configuring it is always a mistake, and
/// "unknown book" would be a misleading way to say so.
pub const FAIR_ANCHOR_BOOK: &str = "pinnacle";

/// Floor on the poll interval. A zero or near-zero value turns the main loop
/// into a tight request loop: measured at ~32,000 requests/minute against a
/// mock, which against the live API is instant rate limiting and the whole
/// monthly quota inside a day.
pub const MIN_POLL_INTERVAL_SECS: u64 = 5;

/// Floor on the gating refresh. Gating costs three requests; refreshing it
/// every second would spend more than the polling it exists to economise.
pub const MIN_GATING_REFRESH_SECS: u64 = 60;

/// Floor on the heartbeat interval. More often than this is ops-channel noise,
/// which is the thing that makes an ops channel unread.
pub const MIN_HEARTBEAT_SECS: u64 = 60;

/// The server drops any EV above this as a data error, so a threshold above it
/// can never match and would silence the bot entirely.
pub const MAX_SENSIBLE_MIN_EV_PCT: f64 = 25.0;

/// Ten days. Beyond this the schedule endpoint's own build window runs out.
pub const MAX_LOOKAHEAD_HOURS: i64 = 240;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),
    #[error("could not read config: {0}")]
    Read(#[from] figment::Error),
    #[error("unknown sport {sport:?} — the EV endpoint serves only: {}", EV_SPORTS.join(", "))]
    UnknownSport { sport: String },
    #[error("unknown book {book:?} — configurable books are: {}", EV_CAPABLE_BOOKS.join(", "))]
    UnknownBook { book: String },
    #[error(
        "{book:?} is the fair-value anchor every other price is scored against, so it can \
         never itself be an opportunity — remove it from `books`"
    )]
    FairAnchorBook { book: String },
    #[error("{field} must be at least {min}, got {got}")]
    TooSmall {
        field: &'static str,
        min: String,
        got: String,
    },
    #[error("{field} must be at most {max}, got {got}")]
    TooLarge {
        field: &'static str,
        max: String,
        got: String,
    },
    #[error("{field} must not be empty — an empty list silently disables the bot")]
    Empty { field: &'static str },
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_base_url")]
    pub owls_base_url: String,

    /// Overridable so tests can point at a mock server. Production never
    /// changes it.
    #[serde(default = "default_discord_base_url")]
    pub discord_base_url: String,

    /// Alert only on EV strictly greater than this, in percent.
    #[serde(default = "default_min_ev_pct")]
    pub min_ev_pct: f64,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(default = "default_gating_refresh")]
    pub gating_refresh_secs: u64,

    /// How often to post a heartbeat to the ops channel.
    ///
    /// The heartbeat exists because silence is this bot's normal state: a live
    /// NFL Sunday check found the best price across twelve books at +2.91%
    /// against a 3.5% threshold. Absence of a heartbeat is the alarm, so this
    /// should stay short enough that you notice it stopping.
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,

    #[serde(default = "default_lookahead_hours")]
    pub lookahead_hours: i64,

    #[serde(default = "default_sports")]
    pub sports: Vec<String>,

    #[serde(default = "default_books")]
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
fn default_discord_base_url() -> String {
    "https://discord.com".to_string()
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
fn default_heartbeat_secs() -> u64 {
    1800
}
fn default_sports() -> Vec<String> {
    [
        "nfl", "nba", "mlb", "nhl", "ncaaf", "ncaab", "wnba", "soccer", "tennis", "mma",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn default_books() -> Vec<String> {
    // Only used when a partial TOML omits `books`; the shipped config is
    // authoritative for what the operator actually holds.
    [
        "betmgm",
        "fanduel",
        "draftkings",
        "caesars",
        "circa",
        "novig",
        "kalshi",
        "polymarket",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
        // Bounds first: a zero interval is the one misconfiguration that can
        // burn the whole monthly quota before anyone notices, and the
        // hot-reload path runs this same validation, so a mid-slate typo is
        // rejected rather than applied.
        if self.poll_interval_secs < MIN_POLL_INTERVAL_SECS {
            return Err(ConfigError::TooSmall {
                field: "poll_interval_secs",
                min: MIN_POLL_INTERVAL_SECS.to_string(),
                got: self.poll_interval_secs.to_string(),
            });
        }
        if self.gating_refresh_secs < MIN_GATING_REFRESH_SECS {
            return Err(ConfigError::TooSmall {
                field: "gating_refresh_secs",
                min: MIN_GATING_REFRESH_SECS.to_string(),
                got: self.gating_refresh_secs.to_string(),
            });
        }
        if self.heartbeat_secs < MIN_HEARTBEAT_SECS {
            return Err(ConfigError::TooSmall {
                field: "heartbeat_secs",
                min: MIN_HEARTBEAT_SECS.to_string(),
                got: self.heartbeat_secs.to_string(),
            });
        }
        if self.lookahead_hours < 1 {
            return Err(ConfigError::TooSmall {
                field: "lookahead_hours",
                min: "1".into(),
                got: self.lookahead_hours.to_string(),
            });
        }
        if self.lookahead_hours > MAX_LOOKAHEAD_HOURS {
            return Err(ConfigError::TooLarge {
                field: "lookahead_hours",
                max: MAX_LOOKAHEAD_HOURS.to_string(),
                got: self.lookahead_hours.to_string(),
            });
        }
        if !self.min_ev_pct.is_finite() || self.min_ev_pct < 0.0 {
            return Err(ConfigError::TooSmall {
                field: "min_ev_pct",
                min: "0".into(),
                got: self.min_ev_pct.to_string(),
            });
        }
        if self.min_ev_pct > MAX_SENSIBLE_MIN_EV_PCT {
            return Err(ConfigError::TooLarge {
                field: "min_ev_pct",
                max: MAX_SENSIBLE_MIN_EV_PCT.to_string(),
                got: self.min_ev_pct.to_string(),
            });
        }
        // An empty list is not a permissive default, it is a silent off switch:
        // no sports means nothing is polled, no books means everything is
        // rejected, and an empty active set also mutes the silence alarm.
        if self.sports.is_empty() {
            return Err(ConfigError::Empty { field: "sports" });
        }
        if self.books.is_empty() {
            return Err(ConfigError::Empty { field: "books" });
        }

        if let Some(sport) = self
            .sports
            .iter()
            .find(|s| !EV_SPORTS.contains(&s.as_str()))
        {
            return Err(ConfigError::UnknownSport {
                sport: sport.clone(),
            });
        }
        if let Some(book) = self.books.iter().find(|b| b.as_str() == FAIR_ANCHOR_BOOK) {
            return Err(ConfigError::FairAnchorBook { book: book.clone() });
        }
        if let Some(book) = self
            .books
            .iter()
            .find(|b| !EV_CAPABLE_BOOKS.contains(&b.as_str()))
        {
            return Err(ConfigError::UnknownBook { book: book.clone() });
        }
        Ok(())
    }
}
