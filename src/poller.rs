//! The poll loop: fetch each active sport's EV board, filter, suppress, queue.
//!
//! Sports are polled **sequentially — one request in flight at a time.** This is
//! a correctness requirement, not a style preference: the API refuses requests
//! past 15 concurrent with an immediate 429 rather than queueing them, so a
//! `join_all` over ten sports risks self-inflicted rate limiting for no latency
//! gain at a 60-second cadence.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::config::Config;
use crate::dedupe::{AlertKey, Decision, Suppressor};
use crate::discord::Alert;
use crate::discord::publisher::AlertQueue;
use crate::filter::{Verdict, is_enriched, is_publishable};
use crate::owls::client::OwlsClient;
use crate::owls::error::OwlsError;
use crate::owls::models::Kind;

/// Cap on how long a single 429 may stall the pass. The API's own waits are
/// short (2s for the concurrency and history caps); a longer one means quota
/// trouble, which the governor handles on the next cycle rather than by
/// blocking here.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(10);

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

/// The strongest EV seen in a cycle, whether or not it qualified.
///
/// Recorded so the bot can explain its own silence. A live check on an NFL
/// Sunday found the best price across twelve books at +2.91% against a 3.5%
/// threshold: correct to stay quiet, but indistinguishable from a broken bot
/// unless the near-miss is reported.
#[derive(Debug, Clone, PartialEq)]
pub struct BestEv {
    pub ev_pct: f64,
    pub book: String,
    pub sport: String,
    pub matchup: String,
    /// What actually happened to this price.
    ///
    /// Travels with the number because a heartbeat reading "best price seen:
    /// +4.46%" beside "1 alert sent" invites the operator to assume they are
    /// the same price. They frequently are not, and the difference is the only
    /// thing that tells them whether tuning their threshold would have helped.
    pub outcome: BestEvOutcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BestEvOutcome {
    /// Passed every filter and reached the publish queue.
    Published,
    /// Recorded silently because this was the sport's first usable board.
    /// Not a rejection — alerts begin from the next crossing.
    Seeded,
    /// Cleared the filters but was already alerted recently.
    Suppressed,
    /// Failed a filter. The string is the specific reason, which may be the
    /// operator's threshold, the book allowlist, or the server's trade bar —
    /// three very different things to learn.
    Rejected(String),
}

#[derive(Debug, Default, Clone, PartialEq)]
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
    /// Sports skipped this cycle because the API asked us to back off.
    pub rate_limited: usize,
    /// Strongest EV observed anywhere this cycle, including prices that were
    /// filtered out. `None` means not one positive-EV price existed.
    pub best_ev: Option<BestEv>,
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

    // Drop suppression state and seeding for sports that have left the gating
    // set. Scoped ageing means their keys would otherwise never be aged again
    // (a slow leak across a season), and clearing `seeded` makes a returning
    // sport re-seed silently rather than republish its whole board.
    ctx.suppressor.retain_sports(&ctx.active);
    ctx.seeded.retain(|s| ctx.active.contains(s));
    // Re-armed keys are retained rather than deleted so the re-alert floor can
    // be enforced, so something must bound the map.
    ctx.suppressor.evict_expired(now_mono);

    let sports: Vec<String> = ctx.active.iter().cloned().collect();

    for sport in sports {
        let board = match ctx.client.get_ev(&sport, server_min_ev).await {
            Ok(b) => b,
            Err(err) => {
                out.errors += 1;
                // Honour Retry-After rather than immediately hammering the same
                // account with the next sport's request. A 429 from the burst
                // limiter stays tripped if we keep pushing; a 429 from quota
                // exhaustion means every further request is spent for nothing.
                if let OwlsError::RateLimited { retry_after } = &err {
                    let wait = (*retry_after).min(MAX_RATE_LIMIT_WAIT);
                    warn!(
                        sport,
                        wait_secs = wait.as_secs_f64(),
                        "rate limited; waiting as instructed before the next sport"
                    );
                    out.rate_limited += 1;
                    tokio::time::sleep(wait).await;
                } else {
                    warn!(sport, error = %err, "EV poll failed; continuing with other sports");
                }
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

                // Whether this is the cycle's strongest price is decided
                // BEFORE filtering, because a near-miss is the most
                // informative thing the bot can report when it is quiet. The
                // outcome is stamped on at the end of the iteration, once it is
                // actually known -- reporting the number without the outcome
                // invites the operator to assume the best price was the alert.
                let is_best = out.best_ev.as_ref().is_none_or(|b| opp.ev_pct > b.ev_pct);

                let outcome = match is_publishable(opp, cfg) {
                    Verdict::Reject(reason) => {
                        out.rejected += 1;
                        debug!(sport, book = %opp.book, %reason, "filtered out");
                        BestEvOutcome::Rejected(reason.to_string())
                    }
                    Verdict::Publish => {
                        let key = AlertKey {
                            sport: sport.clone(),
                            event_id: event.dedupe_key(),
                            book: opp.book.clone(),
                            side: opp.side,
                        };
                        seen_this_poll.insert(key.clone());

                        if seeding {
                            // Cold start: record without publishing. Otherwise
                            // every restart dumps the live board into the
                            // channel.
                            ctx.suppressor.seed(&key, opp.ev_pct, now_mono);
                            BestEvOutcome::Seeded
                        } else {
                            match ctx.suppressor.observe(&key, opp.ev_pct, now_mono) {
                                Decision::Publish => {
                                    out.published += 1;
                                    ctx.queue.push(Alert {
                                        event: event.clone(),
                                        opportunity: opp.clone(),
                                        enriched: is_enriched(opp),
                                    });
                                    BestEvOutcome::Published
                                }
                                Decision::Suppress => {
                                    out.suppressed += 1;
                                    BestEvOutcome::Suppressed
                                }
                            }
                        }
                    }
                };

                if is_best {
                    out.best_ev = Some(BestEv {
                        ev_pct: opp.ev_pct,
                        book: opp.book.clone(),
                        sport: sport.clone(),
                        matchup: format!("{} @ {}", event.away_team, event.home_team),
                        outcome,
                    });
                }
            }
        }

        // Age only THIS sport's keys. An unscoped call would age every other
        // sport's keys once per sport polled, evicting them inside a single
        // cycle at three or more active sports and collapsing suppression.
        ctx.suppressor.mark_absent_in_sport(&sport, &seen_this_poll);

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
