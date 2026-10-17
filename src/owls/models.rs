//! Owls Insight response types, derived from the OpenAPI schema and validated
//! against captured live responses in `tests/fixtures/`.
//!
//! **Casing is not uniform across one response.** The EV `meta` object is
//! snake_case at its top level (`odds_format`, `min_ev`, `pregame_only`) while
//! its nested `freshness` and `board` objects are camelCase (`ageSeconds`,
//! `builtAt`, `venueQuotes`), and `data[].opportunities[]` is camelCase
//! throughout. `/events` is snake_case everywhere. So `rename_all` is applied
//! per struct rather than once globally — a single global rule silently fails
//! on half of these.
//!
//! The schema marks nothing as `required`, so fields that a response could
//! plausibly omit are `Option`. The handful left non-optional (`book`, `side`,
//! `ev_pct`, `book_price`, `fair_probability`) are the ones an opportunity is
//! meaningless without: if one is missing we want a loud parse error, not a
//! silently malformed alert.

use chrono::{DateTime, Utc};
use serde::Deserialize;

// ---------------------------------------------------------------- EV endpoint

#[derive(Debug, Clone, Deserialize)]
pub struct EvResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub data: Vec<EvEvent>,
    pub meta: EvMeta,
}

/// Top level is snake_case; `freshness` and `board` beneath it are not.
#[derive(Debug, Clone, Deserialize)]
pub struct EvMeta {
    #[serde(default)]
    pub sport: Option<String>,
    /// Always `h2h` — the EV board is moneyline only.
    #[serde(default)]
    pub market: Option<String>,
    #[serde(default)]
    pub odds_format: Option<String>,
    #[serde(default)]
    pub min_ev: Option<f64>,
    /// Server-side ceiling above which a price is treated as a data error
    /// (observed: 25).
    #[serde(default)]
    pub max_ev: Option<f64>,
    #[serde(default)]
    pub fair_method: Option<String>,
    /// Always true — the EV board never covers started games.
    #[serde(default)]
    pub pregame_only: Option<bool>,
    #[serde(default)]
    pub events: Option<u64>,
    #[serde(default)]
    pub opportunities: Option<u64>,
    pub timestamp: DateTime<Utc>,
    pub freshness: Freshness,
    #[serde(default)]
    pub board: Option<Board>,
}

impl EvMeta {
    /// True when the response carries no data at all, which the API signals with
    /// the Unix epoch as `timestamp` plus `freshness.stale`.
    ///
    /// Distinct from an empty-but-fresh board: a sport with no qualifying price
    /// right now returns `data: []` with a current timestamp and `stale: false`.
    /// Conflating the two would report a working feed as decades stale.
    pub fn is_empty_sentinel(&self) -> bool {
        self.timestamp.timestamp() == 0 && self.freshness.stale
    }

    /// True when the board is too old to act on, by either signal.
    pub fn is_unusable(&self) -> bool {
        self.is_empty_sentinel()
            || self.freshness.stale
            || self.board.as_ref().is_some_and(|b| b.stale)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Freshness {
    #[serde(default)]
    pub age_seconds: Option<f64>,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    #[serde(default)]
    pub built_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub age_ms: Option<f64>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub venues: bool,
    #[serde(default)]
    pub venue_quotes: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvEvent {
    /// Book-specific and opaque — a UUID on some sports, a bare number on
    /// others. Not stable across books, so not a dedupe key.
    #[serde(default)]
    pub event_id: Option<String>,
    /// The stable `sport:away@home-YYYYMMDD` key. Nullable per schema.
    #[serde(default)]
    pub canonical_event_id: Option<String>,
    pub sport: String,
    pub home_team: String,
    pub away_team: String,
    pub commence_time: DateTime<Utc>,
    #[serde(default)]
    pub market: Option<String>,
    #[serde(default)]
    pub books_in_consensus: Option<f64>,
    #[serde(default)]
    pub fair_probability: Option<SideProbabilities>,
    #[serde(default)]
    pub fair: Option<Fair>,
    #[serde(default)]
    pub opportunities: Vec<Opportunity>,
}

impl EvEvent {
    /// The identity used for alert suppression.
    ///
    /// Prefers `canonical_event_id` so the same game dedupes across books.
    /// Falls back to `event_id` when the canonical id is null: that degrades to
    /// per-book-id suppression, which over-alerts rather than collapsing
    /// distinct games onto one key — the safer direction to fail.
    pub fn dedupe_key(&self) -> String {
        self.canonical_event_id
            .clone()
            .or_else(|| self.event_id.clone())
            .unwrap_or_else(|| {
                format!(
                    "{}:{}@{}-{}",
                    self.sport,
                    self.away_team,
                    self.home_team,
                    self.commence_time.format("%Y%m%d")
                )
            })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SideProbabilities {
    #[serde(default)]
    pub home: Option<f64>,
    #[serde(default)]
    pub away: Option<f64>,
    #[serde(default)]
    pub draw: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fair {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub anchor: Option<String>,
    #[serde(default)]
    pub sources: Option<f64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub home: Option<f64>,
    #[serde(default)]
    pub away: Option<f64>,
    #[serde(default)]
    pub draw: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Home,
    Away,
    Draw,
}

/// Maps an unrecognised `kind` to `None` instead of failing the whole response.
///
/// `Side` is deliberately NOT lenient: sides are genuinely closed for a
/// moneyline market, and an alert naming the wrong team is worse than no alert.
fn lenient_kind<'de, D>(d: D) -> Result<Option<Kind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = Option::deserialize(d)?;
    Ok(raw.and_then(|s| match s.as_str() {
        "book" => Some(Kind::Book),
        "venue" => Some(Kind::Venue),
        _ => None,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A sportsbook. Trade bar is 2% EV.
    Book,
    /// An exchange or prediction market. Trade bar is 4% EV plus liquidity.
    Venue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Opportunity {
    pub book: String,
    pub side: Side,
    /// American odds. A `number` in the schema, not an integer — some books
    /// carry a half point.
    pub book_price: f64,
    pub ev_pct: f64,
    /// The *conservative* fair: the true fair minus 1 percentage point.
    pub fair_probability: f64,

    #[serde(default)]
    pub market: Option<String>,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub book_implied: Option<f64>,
    #[serde(default)]
    pub fair_price: Option<f64>,
    #[serde(default)]
    pub edge_pp: Option<f64>,
    /// Full-Kelly bankroll fraction, computed at `fair_probability`.
    #[serde(default)]
    pub kelly_fraction: Option<f64>,
    /// Independent sources behind the fair this price was scored against.
    /// `1` means Pinnacle alone. A `number` in the schema, so f64.
    #[serde(default)]
    pub books_in_consensus: Option<f64>,
    /// Unknown values deserialize to `None` rather than failing. A closed enum
    /// here made ONE unrecognised value a hard error for the entire board, so a
    /// new venue type on Owls' side would blind the bot indefinitely. `None`
    /// takes the stricter 4% trade bar, so tolerance cannot loosen anything.
    #[serde(default, deserialize_with = "lenient_kind")]
    pub kind: Option<Kind>,
    /// The fair before the 1-point safety margin.
    #[serde(default)]
    pub fair_unadjusted: Option<f64>,
    #[serde(default)]
    pub roi: Option<f64>,
    /// Venue trading fee per contract in dollars; 0 for a sportsbook.
    #[serde(default)]
    pub fee: Option<f64>,
    /// Whether this clears the server's trade bar. When absent, derive it from
    /// `kind` — see `filter::effective_trade_bar`.
    #[serde(default)]
    pub tradeable: Option<bool>,
    /// Why `tradeable` is false, e.g. "EV under 2%".
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub size: Option<f64>,
    /// Deep link to the bet, when the book provides one.
    #[serde(default)]
    pub link: Option<String>,
    /// Age of the price when the response was served.
    #[serde(default)]
    pub quote_age_ms: Option<f64>,
}

// ---------------------------------------------------------- Schedule endpoint

/// `GET /api/v1/schedule` — one call covering many sports for a calendar day.
///
/// Does **not** cover `ncaab`, and carries `mma` only as `ufc`; see
/// `EventsResponse` for the gating fallback those two need.
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub days: Option<u32>,
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub sports: Option<Vec<String>>,
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub games: Vec<ScheduleGame>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleGame {
    #[serde(default)]
    pub id: Option<String>,
    pub sport: String,
    #[serde(default)]
    pub league: Option<String>,
    pub start_time: DateTime<Utc>,
    /// One of `scheduled`, `in_progress`, `delayed`, `final`, `postponed`,
    /// `cancelled`, `time_tbd`, `unconfirmed`. Kept as a string because the
    /// docs state the set can grow and an unknown value must not fail the parse.
    pub status: String,
    #[serde(default)]
    pub status_detail: Option<String>,
    #[serde(default)]
    pub event_id: Option<String>,
}

impl ScheduleGame {
    /// Whether this game has not started, and so could carry pregame EV.
    pub fn is_pregame(&self) -> bool {
        matches!(self.status.as_str(), "scheduled" | "delayed" | "time_tbd")
    }
}

// ------------------------------------------------------------ Events endpoint

/// `GET /api/v1/{sport}/events` — snake_case throughout, unlike the EV board.
/// The gating fallback for `ncaab` and `mma`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventsResponse {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub data: Vec<EventSummary>,
    pub meta: EventsMeta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventsMeta {
    #[serde(default)]
    pub sport: Option<String>,
    #[serde(default)]
    pub sport_key: Option<String>,
    #[serde(default)]
    pub available_leagues: Vec<String>,
    #[serde(default)]
    pub event_count: Option<u64>,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub freshness: Option<Freshness>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventSummary {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub sport_key: Option<String>,
    #[serde(default)]
    pub sport_title: Option<String>,
    pub commence_time: DateTime<Utc>,
    #[serde(default)]
    pub home_team: Option<String>,
    #[serde(default)]
    pub away_team: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub league: Option<String>,
    #[serde(default, rename = "eventId")]
    pub event_id: Option<String>,
}

impl EventSummary {
    /// Whether this event has not started. An absent status is treated as
    /// pregame: the endpoint's purpose is listing upcoming events, and
    /// excluding an unlabelled one would silently drop a sport from gating.
    pub fn is_pregame(&self) -> bool {
        match self.status.as_deref() {
            None => true,
            Some(s) => matches!(s, "scheduled" | "delayed" | "time_tbd"),
        }
    }
}
