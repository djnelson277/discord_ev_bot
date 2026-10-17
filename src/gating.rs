//! Decides which sports are worth polling right now.
//!
//! The EV board is pregame-only, so a sport with no upcoming game returns a
//! guaranteed-empty response. Polling all ten sports unconditionally at 60s is
//! 432,000 requests/month against a 300,000 cap; gating on the slate brings
//! that to roughly 218,000 while allowing a *faster* cadence on the sports that
//! actually have games.
//!
//! ## Why this needs three calls instead of one
//!
//! `GET /api/v1/schedule` covers many sports in one request, but **not all of
//! the operator's**. Verified against the live API:
//!
//! ```text
//! GET /api/v1/schedule?sport=ncaab -> 400 Unknown sport
//! GET /api/v1/schedule?sport=mma   -> 400 Unknown sport
//! valid: nfl, ncaaf, mlb, nba, wnba, nhl, tennis, soccer, ufc,
//!        lol, cs2, valorant, dota2
//! ```
//!
//! That is structural, not seasonal — it will still 400 in February with a full
//! college basketball slate. `GET /api/v1/{sport}/events` covers both, so those
//! two sports gate through it.
//!
//! `mma` is not merely a renaming of `ufc`. The captured events fixture holds
//! 104 MMA events across 11 promotions (One Friday Fights, Cage Warriors, PFL,
//! ONE, Oktagon, BKFC, A1 Combat…) of which only 27 are UFC. Gating `mma`
//! through `?sport=ufc` would leave the poller blind to 77 of 104 events while
//! appearing to work.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use tracing::warn;

use crate::config::Config;
use crate::owls::client::OwlsClient;
use crate::owls::error::OwlsError;

/// Operator sports that `GET /api/v1/schedule` serves, so one call covers them all.
pub const GLOBAL_SCHEDULE_SPORTS: [&str; 8] = [
    "nfl", "ncaaf", "mlb", "nba", "wnba", "nhl", "tennis", "soccer",
];

/// Operator sports `/schedule` refuses with a 400. These gate through
/// `GET /api/v1/{sport}/events` instead, one call each.
pub const EVENTS_ONLY_SPORTS: [&str; 2] = ["ncaab", "mma"];

/// `days` for the schedule call.
///
/// Two, not one: `days=1` returns only the current calendar day in the
/// response's timezone (default `America/New_York`), so a late-evening refresh
/// would see nothing of tomorrow's slate and stop polling before the night's
/// games.
pub const SCHEDULE_DAYS: u8 = 2;

/// Sports with a game that has not started, inside the configured lookahead.
///
/// Always intersected with `cfg.sports`, so configuration narrows the set and
/// can never widen it: the schedule response carries esports and `ufc` that the
/// operator never asked for, and none may reach the poller.
///
/// A failure on one of the `/events` calls is logged and skipped rather than
/// propagated. Returning an error would blank the entire active set and stop
/// polling every sport, which is a far worse outcome than losing one sport's
/// gating for an hour.
pub async fn active_sports(
    client: &OwlsClient,
    cfg: &Config,
    now: DateTime<Utc>,
) -> Result<BTreeSet<String>, OwlsError> {
    let window_end = now + Duration::hours(cfg.lookahead_hours);
    let wanted: BTreeSet<&str> = cfg.sports.iter().map(String::as_str).collect();
    let mut active = BTreeSet::new();

    // One call for the eight sports /schedule covers.
    let schedule = client.get_schedule(SCHEDULE_DAYS).await?;
    for game in &schedule.games {
        if !wanted.contains(game.sport.as_str()) {
            continue;
        }
        if game.is_pregame() && in_window(game.start_time, now, window_end) {
            active.insert(game.sport.clone());
        }
    }

    // One call each for the two it refuses.
    for sport in EVENTS_ONLY_SPORTS {
        if !wanted.contains(sport) {
            continue;
        }
        match client.get_events(sport).await {
            Ok(events) => {
                let has_upcoming = events
                    .data
                    .iter()
                    .any(|e| e.is_pregame() && in_window(e.commence_time, now, window_end));
                if has_upcoming {
                    active.insert(sport.to_string());
                }
            }
            Err(err) => warn!(
                sport,
                error = %err,
                "events gating call failed; treating sport as inactive this cycle"
            ),
        }
    }

    Ok(active)
}

fn in_window(t: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    t >= start && t <= end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_sports_are_fully_covered_by_the_two_lists() {
        // Every sport the operator configured must be gateable by some route,
        // or it would be polled blindly or never polled at all.
        let operator = [
            "nfl", "nba", "mlb", "nhl", "ncaaf", "ncaab", "wnba", "soccer", "tennis", "mma",
        ];
        for s in operator {
            assert!(
                GLOBAL_SCHEDULE_SPORTS.contains(&s) || EVENTS_ONLY_SPORTS.contains(&s),
                "{s} has no gating route"
            );
        }
    }

    #[test]
    fn the_two_lists_do_not_overlap() {
        for s in EVENTS_ONLY_SPORTS {
            assert!(!GLOBAL_SCHEDULE_SPORTS.contains(&s));
        }
    }

    #[test]
    fn window_is_inclusive_at_both_ends() {
        let a: DateTime<Utc> = "2026-10-16T00:00:00Z".parse().unwrap();
        let b: DateTime<Utc> = "2026-10-18T00:00:00Z".parse().unwrap();
        assert!(in_window(a, a, b));
        assert!(in_window(b, a, b));
        assert!(!in_window(a - Duration::seconds(1), a, b));
        assert!(!in_window(b + Duration::seconds(1), a, b));
    }
}
