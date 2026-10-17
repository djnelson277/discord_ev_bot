//! Decides whether one opportunity is worth alerting on.
//!
//! Client-side filters may only ever *narrow* what the server offers. The
//! server already applies its own dedup and trade bar; this adds the operator's
//! stricter preferences on top. Nothing here may loosen a server rule.

use crate::config::Config;
use crate::owls::models::{Kind, Opportunity};

/// Above this the API itself treats a price as a data error and drops it
/// (`meta.max_ev` is 25). If one still arrives, the board is wrong.
const IMPLAUSIBLE_EV_PCT: f64 = 25.0;

/// Trade bar for a sportsbook price.
const BOOK_TRADE_BAR_PCT: f64 = 2.0;

/// Trade bar for an exchange or prediction-market price. Higher because the
/// server additionally requires a bid, a spread of 4 cents or less, and at
/// least $50 at the ask.
const VENUE_TRADE_BAR_PCT: f64 = 4.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Publish,
    Reject(RejectReason),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RejectReason {
    #[error("EV {ev:.2}% is above the {ceiling:.0}% plausibility ceiling — suspect data")]
    ImplausibleEv { ev: f64, ceiling: f64 },

    #[error("book {book:?} is not in the configured allowlist")]
    BookNotAllowed { book: String },

    #[error("EV {ev:.2}% does not exceed the configured {threshold:.2}% threshold")]
    BelowThreshold { ev: f64, threshold: f64 },

    #[error("not tradeable: {detail}")]
    NotTradeable { detail: String },

    #[error("only {actual} independent source(s) behind the fair, {required} required")]
    BelowConsensusMinimum { actual: f64, required: u32 },
}

/// The EV a price must reach to be considered tradeable when the server did not
/// say. An unknown `kind` takes the stricter bar: over-filtering costs a missed
/// alert, under-filtering costs a bad one.
pub fn effective_trade_bar(kind: Option<Kind>) -> f64 {
    match kind {
        Some(Kind::Book) => BOOK_TRADE_BAR_PCT,
        Some(Kind::Venue) | None => VENUE_TRADE_BAR_PCT,
    }
}

/// Whether this opportunity should reach Discord.
///
/// Checks run most-specific-first so the reported reason is the most
/// informative one: a 40% EV price on a disallowed book is reported as suspect
/// data, not as a book problem.
pub fn is_publishable(opp: &Opportunity, cfg: &Config) -> Verdict {
    // 1. Implausible data outranks everything. A price this good is a bug, and
    //    acting on it is the most expensive mistake available here.
    if opp.ev_pct > IMPLAUSIBLE_EV_PCT {
        return Verdict::Reject(RejectReason::ImplausibleEv {
            ev: opp.ev_pct,
            ceiling: IMPLAUSIBLE_EV_PCT,
        });
    }

    // 2. A book the operator has no account at is pure noise.
    if !cfg.books.iter().any(|b| b == &opp.book) {
        return Verdict::Reject(RejectReason::BookNotAllowed {
            book: opp.book.clone(),
        });
    }

    // 3. The operator's threshold, strictly greater than.
    if opp.ev_pct <= cfg.min_ev_pct {
        return Verdict::Reject(RejectReason::BelowThreshold {
            ev: opp.ev_pct,
            threshold: cfg.min_ev_pct,
        });
    }

    // 4. The trade bar. Prefer the server's own verdict; derive it only when
    //    the field is absent, since the server also weighs liquidity we cannot
    //    see (bid present, spread, size at the ask).
    match opp.tradeable {
        Some(true) => {}
        Some(false) => {
            return Verdict::Reject(RejectReason::NotTradeable {
                detail: opp
                    .reason
                    .clone()
                    .unwrap_or_else(|| "server marked it not tradeable".to_string()),
            });
        }
        None => {
            let bar = effective_trade_bar(opp.kind);
            if opp.ev_pct < bar {
                return Verdict::Reject(RejectReason::NotTradeable {
                    detail: format!("EV {:.2}% is under the {bar:.0}% bar", opp.ev_pct),
                });
            }
        }
    }

    // 5. Consensus minimum, if configured.
    //
    //    Fails OPEN when the count is unknown. A filter that silently
    //    suppresses alerts because a field was missing is worse than one that
    //    lets a marked alert through: the operator can judge a marked alert,
    //    but cannot judge an alert they never saw.
    if let (Some(required), Some(actual)) = (cfg.min_books_in_consensus, opp.books_in_consensus) {
        if actual < f64::from(required) {
            return Verdict::Reject(RejectReason::BelowConsensusMinimum { actual, required });
        }
    }

    Verdict::Publish
}

/// Whether the enriched fields an alert wants are actually present, so the
/// publisher can mark an alert that is missing them rather than hiding it.
pub fn is_enriched(opp: &Opportunity) -> bool {
    opp.books_in_consensus.is_some() && opp.kelly_fraction.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kind_takes_the_stricter_bar() {
        assert_eq!(effective_trade_bar(None), VENUE_TRADE_BAR_PCT);
        assert!(VENUE_TRADE_BAR_PCT > BOOK_TRADE_BAR_PCT);
    }
}
