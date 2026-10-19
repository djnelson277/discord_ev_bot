//! Health reporting.
//!
//! A bot that silently stops is worse than no bot, because it is trusted. These
//! are the two conditions that produce that failure: the monthly budget running
//! out mid-month, and alerts stopping while games are live.
//!
//! Both warnings route to the **ops** channel, never the alert channel. The
//! alert channel must stay signal-only; health noise there would train the
//! operator to skim exactly the channel that has to be read.

use std::collections::BTreeSet;

use crate::owls::quota::{MONTHLY_QUOTA_MVP, Pacing, QuotaSnapshot, is_degraded};
use crate::poller::{BestEv, BestEvOutcome};

/// Initialises structured logging to stdout.
///
/// Stdout, not a file: every cloud platform captures stdout and nothing
/// captures local log files, so writing to one guarantees the logs are lost on
/// the move the design plans for.
pub fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .try_init();
}

/// Warns when the month's budget will not cover the configured plan.
///
/// Tied to the governor's own decision rather than a second, parallel pacing
/// formula: if `next_interval` is slowing the poller down, the operator should
/// know, and the two can never disagree. The earlier version had its own
/// `expected <= 0` guard and so went silent on the last day of the cycle —
/// exactly when it mattered most.
///
/// Returns `None` when the plan has no monthly cap and when the budget is
/// unknown, because "I don't know yet" is not a fault to report.
pub fn quota_warning(q: &QuotaSnapshot, pacing: Pacing, _warn_at: f64) -> Option<String> {
    if q.unlimited_month {
        return None;
    }
    let remaining = q.remaining_month?;

    if remaining == 0 {
        return Some(
            "Owls Insight monthly quota is exhausted — every request will 429 until the \
             billing date resets it. The bot is effectively off."
                .to_string(),
        );
    }

    if !is_degraded(q, pacing) {
        return None;
    }

    let spent_pct = 100.0 * (1.0 - remaining as f64 / MONTHLY_QUOTA_MVP as f64);
    Some(format!(
        "Quota pacing: {remaining} requests left on day {} with {} sport(s) active \
         ({spent_pct:.0}% of the month spent). The poller has lengthened its interval to \
         stay inside the cap.",
        pacing.day_of_month, pacing.active_sports
    ))
}

// `silence_warning` was removed when the heartbeat replaced it. It could only
// ever say "either nothing cleared the threshold or the poller has stopped" --
// precisely the two states an operator needs told apart -- and a live NFL
// Sunday check showed silence is the normal case, so the ambiguous version
// would have fired constantly. The heartbeat answers the question instead:
// absence of it is the alarm, and its content explains the quiet.

/// A periodic "here is what I saw" report for the ops channel.
///
/// This exists because silence is the bot's normal state and was previously
/// ambiguous. A live check during an NFL Sunday slate found the best price
/// across twelve books at +2.91% against a 3.5% threshold, and on another game
/// not one positive-EV price existed at all — both correct reasons to stay
/// quiet, and both indistinguishable from a crash.
///
/// Absence of a heartbeat is the alarm; its content explains the quiet.
///
/// `queued` counts alerts pushed onto the publish queue and `delivered` counts
/// those Discord actually accepted. They are reported separately because an
/// earlier version conflated them and would say "1 alert sent" for a message
/// Discord had rejected — the bot claiming to have told you something it had
/// not.
pub fn heartbeat_message(
    active: &BTreeSet<String>,
    best: Option<BestEv>,
    threshold_pct: f64,
    queued: usize,
    delivered: usize,
    quota: &QuotaSnapshot,
) -> String {
    let mut parts = Vec::new();

    if active.is_empty() {
        parts.push(
            "No sports in season or pre-game right now, so nothing is being polled.".to_string(),
        );
    } else {
        let sports: Vec<&str> = active.iter().map(String::as_str).collect();
        parts.push(format!(
            "Watching {} sport(s): {}.",
            active.len(),
            sports.join(", ")
        ));
    }

    parts.push(match delivered {
        0 if queued == 0 => "No alerts since the last report.".to_string(),
        n => format!("{n} alert(s) delivered since the last report."),
    });

    if queued > delivered {
        parts.push(format!(
            "WARNING: {} alert(s) were queued but never accepted by Discord — check the logs \
             for publish failures.",
            queued - delivered
        ));
    }

    parts.push(match best {
        Some(b) => {
            let verdict = match &b.outcome {
                BestEvOutcome::Published => "alerted \u{2713}".to_string(),
                // Not a rejection. Saying so plainly stops a normal restart
                // looking like a misconfiguration.
                BestEvOutcome::Seeded => format!(
                    "not alerted: this was the first poll of {} since start, so the board was \
                     recorded silently and alerts begin from the next crossing",
                    b.sport
                ),
                BestEvOutcome::Suppressed => {
                    "not alerted: already alerted recently, still inside the suppression window"
                        .to_string()
                }
                // The reason is the server's where one applies, because the
                // operator's threshold is irrelevant to a price the server
                // would refuse anyway.
                BestEvOutcome::Rejected(why) => format!("not alerted: {why}"),
            };
            format!(
                "Best price seen: {:+.2}% EV at {} on {} ({}) — {verdict}. Your threshold is \
                 >{:.2}%.",
                b.ev_pct, b.book, b.matchup, b.sport, threshold_pct
            )
        }
        None if active.is_empty() => String::new(),
        // Distinct from "nothing cleared the bar": not one book beat the
        // conservative fair on any game, which is what an efficient market
        // looks like.
        None => format!(
            "No positive-EV price on any game this period — every book was at or \
             behind the fair. Your threshold is >{threshold_pct:.2}%."
        ),
    });

    if quota.unlimited_month {
        parts.push("Quota: unlimited.".to_string());
    } else if let Some(r) = quota.remaining_month {
        let pct = 100.0 * r as f64 / MONTHLY_QUOTA_MVP as f64;
        parts.push(format!(
            "Quota: {r} requests left ({pct:.0}% of the month)."
        ));
    }

    parts.retain(|p| !p.is_empty());
    parts.join(" ")
}
