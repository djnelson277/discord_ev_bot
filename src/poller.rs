//! The poll loop: fetch each active sport's EV board, filter, suppress, queue.
//!
//! Sports are polled **sequentially — one request in flight at a time.** This is
//! a correctness requirement, not a style preference: the API refuses requests
//! past 15 concurrent with an immediate 429 rather than queueing them, so a
//! `join_all` over ten sports risks self-inflicted rate limiting for no latency
//! gain at a 60-second cadence.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, info, warn};

use crate::config::Config;
use crate::dedupe::{AlertKey, Decision, Suppressor};
use crate::discord::Alert;
use crate::discord::publisher::AlertQueue;
use crate::filter::{Verdict, is_enriched, is_publishable};
use crate::owls::client::OwlsClient;
use crate::owls::models::Kind;

/// Mutable state carried across polls.
///
/// `Config` is deliberately **not** a field here. It is hot-reloaded through a
/// watch channel, so a snapshot captured at boot would freeze the operator's
/// threshold forever and defeat the runtime-changeability requirement. The
/// caller passes the current value into each [`poll_once`].
pub struct PollContext {
    pub client: OwlsClient,
    pub suppressor: Suppressor,
    pub queue: Arc<AlertQueue>,
    /// Sports to poll this cycle, from the gating pass.
    pub active: BTreeSet<String>,
    /// Sports whose board has been recorded once, so publishing may begin.
    pub seeded: HashSet<String>,
    /// Sports that have ever shown an exchange or prediction-market price.
    ///
    /// These get an interval floor: `venues=true` quotes are warm-on-demand and
    /// stay live only "while you keep asking", so going quiet on such a sport
    /// can switch the quotes off rather than merely making them stale.
    pub venue_sports: HashSet<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollOutcome {
    /// Sports whose board was fetched and usable.
    pub polled: usize,
    pub published: usize,
    pub suppressed: usize,
    /// Failed a filter (threshold, allowlist, trade bar, consensus).
    pub rejected: usize,
    /// Sports recorded silently because this was their first usable board.
    pub seeded: usize,
    pub stale_boards: usize,
    pub errors: usize,
}

/// One pass over the active sports.
///
/// Takes only the monotonic clock. Wall-clock time belongs to gating, which
/// compares `commence_time`; the EV board is pregame-only server-side, so the
/// poll loop never needs a calendar. `Instant` is also the right clock for the
/// suppression window, being immune to NTP steps and DST.
///
/// Never returns an error: a single sport's failure is logged and the loop
/// continues. Propagating would let one endpoint's bad minute stop every other
/// sport, which is strictly worse than losing one board for one cycle.
pub async fn poll_once(ctx: &mut PollContext, cfg: &Config, now_mono: Instant) -> PollOutcome {
    let mut out = PollOutcome::default();
    // The server-side floor; the operator's finer threshold is applied locally
    // by `is_publishable`, since the API only accepts whole-number min_ev.
    let server_min_ev = cfg.min_ev_pct.floor();

    let sports: Vec<String> = ctx.active.iter().cloned().collect();

    for sport in sports {
        let board = match ctx.client.get_ev(&sport, server_min_ev).await {
            Ok(b) => b,
            Err(err) => {
                out.errors += 1;
                warn!(sport, error = %err, "EV poll failed; continuing with other sports");
                continue;
            }
        };

        // A stale board must not be acted on, and must not mark the sport
        // seeded: doing so would silently suppress the first real board later.
        if board.meta.is_unusable() {
            out.stale_boards += 1;
            warn!(
                sport,
                stale = board.meta.freshness.stale,
                "board is stale or empty-sentinel; skipping"
            );
            continue;
        }

        out.polled += 1;
        let seeding = !ctx.seeded.contains(&sport);
        let mut seen_this_poll: HashSet<AlertKey> = HashSet::new();

        for event in &board.data {
            for opp in &event.opportunities {
                if matches!(opp.kind, Some(Kind::Venue)) {
                    ctx.venue_sports.insert(sport.clone());
                }

                match is_publishable(opp, cfg) {
                    Verdict::Reject(reason) => {
                        out.rejected += 1;
                        debug!(sport, book = %opp.book, %reason, "filtered out");
                        continue;
                    }
                    Verdict::Publish => {}
                }

                let key = AlertKey {
                    event_id: event.dedupe_key(),
                    book: opp.book.clone(),
                    side: opp.side,
                };
                seen_this_poll.insert(key.clone());

                if seeding {
                    // Cold start: record without publishing. Otherwise every
                    // restart dumps the whole live board into the channel.
                    ctx.suppressor.seed(&key, opp.ev_pct, now_mono);
                    continue;
                }

                match ctx.suppressor.observe(&key, opp.ev_pct, now_mono) {
                    Decision::Publish => {
                        out.published += 1;
                        ctx.queue.push(Alert {
                            event: event.clone(),
                            opportunity: opp.clone(),
                            enriched: is_enriched(opp),
                        });
                    }
                    Decision::Suppress => out.suppressed += 1,
                }
            }
        }

        // Age keys for this sport's observations. Done per sport so one sport's
        // board does not look like an absence for another's keys.
        ctx.suppressor.mark_absent(&seen_this_poll);

        if seeding {
            ctx.seeded.insert(sport.clone());
            out.seeded += 1;
            info!(
                sport,
                tracked = ctx.suppressor.tracked(),
                "seeded board silently; alerts begin from the next crossing"
            );
        }
    }

    out
}
