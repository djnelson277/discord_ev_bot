//! Alert suppression.
//!
//! ## Why the poller needs its own
//!
//! The 15-minute / 2-hour / +1-point rule the design leans on is a property of
//! the **`ev.found` webhook firing**. `GET /api/v1/{sport}/ev` has no dedup
//! concept at all: it is a full board snapshot that returns the same qualifying
//! opportunity on *every* poll for as long as the price lives. At a 60-second
//! cadence that is sixty identical posts an hour.
//!
//! So this mirrors the server's rule client-side, keyed on
//! `(event, book, side)`. Deliberately the same shape as the server's, so an
//! opportunity behaves identically whether it arrived by poll now or by webhook
//! in phase 2 — the two paths will share this state.
//!
//! ## The cold-start problem
//!
//! On boot the poller sees a board already full of qualifying prices. Publishing
//! them is a burst of every live opportunity across ten sports, on every single
//! restart — the alert-spam anti-goal. [`Suppressor::seed`] records them without
//! publishing, which matches `ev.found` semantics ("only the crossings that come
//! after"). The cost is explicit: a restart during a slate means a quiet channel
//! until the next genuine crossing. A missed alert is recoverable; a channel
//! people mute is not.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::owls::models::Side;

/// Consecutive polls a key must be missing before it re-arms.
///
/// Two, not one: a single missing poll is noise, and a flickering price that
/// re-armed every time it blinked would defeat the whole mechanism.
const ABSENCES_TO_REARM: u32 = 2;

/// Identity of one alertable opportunity.
///
/// `event_id` is `EvEvent::dedupe_key()` — the canonical `sport:away@home-date`
/// key where available, so the same game dedupes across books. `book` covers
/// exchanges and prediction markets too; the API field is `book` for all of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AlertKey {
    pub event_id: String,
    pub book: String,
    pub side: Side,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Publish,
    Suppress,
}

#[derive(Debug, Clone)]
struct State {
    /// EV at the moment we last published this key. The baseline a rise is
    /// measured against, not the highest EV ever seen.
    last_published_ev: f64,
    last_published_at: Instant,
    absent_polls: u32,
}

pub struct Suppressor {
    window: Duration,
    rise_pp: f64,
    seen: HashMap<AlertKey, State>,
}

impl Suppressor {
    /// `window` is how long a published key stays suppressed; `rise_pp` is the
    /// EV increase in percentage points that overrides that suppression.
    pub fn new(window: Duration, rise_pp: f64) -> Self {
        Self {
            window,
            rise_pp,
            seen: HashMap::new(),
        }
    }

    /// How many keys are currently tracked. Exposed so the poller can log it:
    /// a map that only ever grows is a leak, and `mark_absent` is what bounds it.
    pub fn tracked(&self) -> usize {
        self.seen.len()
    }

    /// Record a key without publishing, for cold start.
    ///
    /// Stores the same state a publish would, so a later rise of `rise_pp` still
    /// alerts — seeding suppresses the current board, it does not blind the
    /// suppressor to real movement.
    pub fn seed(&mut self, key: &AlertKey, ev: f64, now: Instant) {
        self.seen.insert(
            key.clone(),
            State {
                last_published_ev: ev,
                last_published_at: now,
                absent_polls: 0,
            },
        );
    }

    /// Decide whether this observation should be published.
    pub fn observe(&mut self, key: &AlertKey, ev: f64, now: Instant) -> Decision {
        let decision = match self.seen.get(key) {
            // Never seen, or re-armed after going away: a genuine new crossing.
            None => Decision::Publish,
            Some(state) => {
                let window_expired = now.duration_since(state.last_published_at) >= self.window;
                if window_expired || self.is_material_rise(state.last_published_ev, ev) {
                    Decision::Publish
                } else {
                    Decision::Suppress
                }
            }
        };

        match decision {
            Decision::Publish => {
                self.seen.insert(
                    key.clone(),
                    State {
                        last_published_ev: ev,
                        last_published_at: now,
                        absent_polls: 0,
                    },
                );
            }
            Decision::Suppress => {
                // Still seen this poll, so reset the absence counter.
                if let Some(state) = self.seen.get_mut(key) {
                    state.absent_polls = 0;
                }
            }
        }

        decision
    }

    /// Whether `ev` is at least `rise_pp` above the published baseline.
    ///
    /// The epsilon margin is required, not defensive dressing: `4.1 + 1.0` is
    /// not exactly `5.1` in f64, so a bare `>=` on the raw sum silently swallows
    /// precisely the alert the operator most wants. Scaled by magnitude so it
    /// holds for large EV values too.
    fn is_material_rise(&self, baseline: f64, ev: f64) -> bool {
        let target = baseline + self.rise_pp;
        let tolerance = f64::EPSILON * 8.0 * target.abs().max(1.0);
        ev >= target - tolerance
    }

    /// Age every tracked key against the set observed this poll, dropping those
    /// absent for [`ABSENCES_TO_REARM`] consecutive polls.
    ///
    /// Dropping is what re-arms a key: a price that went away and came back is a
    /// new crossing and should alert again. It is also what bounds the map.
    pub fn mark_absent(&mut self, seen_this_poll: &std::collections::HashSet<AlertKey>) {
        self.seen.retain(|key, state| {
            if seen_this_poll.contains(key) {
                state.absent_polls = 0;
                true
            } else {
                state.absent_polls += 1;
                state.absent_polls < ABSENCES_TO_REARM
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k() -> AlertKey {
        AlertKey {
            event_id: "e".into(),
            book: "b".into(),
            side: Side::Home,
        }
    }

    #[test]
    fn material_rise_tolerates_float_error() {
        let s = Suppressor::new(Duration::from_secs(1), 1.0);
        // these sums are all inexact in binary floating point
        assert!(s.is_material_rise(4.1, 5.1));
        assert!(s.is_material_rise(0.1, 1.1));
        assert!(s.is_material_rise(2.3, 3.3));
        // and it must not become so loose that a sub-point rise slips through
        assert!(!s.is_material_rise(4.0, 4.99));
    }

    #[test]
    fn seeding_does_not_count_as_publishing() {
        let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
        let now = Instant::now();
        s.seed(&k(), 4.0, now);
        assert_eq!(s.tracked(), 1);
        assert_eq!(s.observe(&k(), 4.0, now), Decision::Suppress);
    }
}
