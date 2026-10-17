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
use std::time::{Duration, Instant};

use crate::owls::quota::{MONTHLY_QUOTA_MVP, QuotaSnapshot};

const DAYS_IN_MONTH: f64 = 30.0;

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

/// Warns when the month's budget is being spent faster than it can last.
///
/// Returns `None` when spending is on pace, when the plan has no monthly cap,
/// and when the budget is unknown — before the first response there are no
/// headers, and "I don't know yet" is not a fault to report.
///
/// `warn_at` is the fraction of the expected remaining budget below which to
/// warn, e.g. `0.8`.
pub fn quota_warning(q: &QuotaSnapshot, day_of_month: u32, warn_at: f64) -> Option<String> {
    if q.unlimited_month {
        return None;
    }
    let remaining = q.remaining_month?;

    if remaining == 0 {
        return Some(
            "Owls Insight monthly quota is exhausted — polling will 429 until the billing \
             date resets it."
                .to_string(),
        );
    }

    let elapsed = (f64::from(day_of_month) / DAYS_IN_MONTH).clamp(0.0, 1.0);
    let expected = MONTHLY_QUOTA_MVP as f64 * (1.0 - elapsed);
    if expected <= 0.0 {
        return None;
    }

    let ratio = remaining as f64 / expected;
    if ratio >= warn_at {
        return None;
    }

    let spent_pct = 100.0 * (1.0 - remaining as f64 / MONTHLY_QUOTA_MVP as f64);
    Some(format!(
        "Quota burn ahead of pace: {remaining} requests left on day {day_of_month} \
         ({spent_pct:.0}% spent, {:.0}% of the expected remainder). The poller is \
         lengthening its interval to compensate.",
        ratio * 100.0
    ))
}

/// Warns when no alert has been published for too long **while games are live**.
///
/// `active` is the gating pass's active-sport set. When it is empty the silence
/// is expected, not a fault: the poller deliberately spends zero requests on a
/// sport with no upcoming games, so quiet nights are normal and a bare timer
/// would cry wolf nightly. An ops channel that cries wolf is one nobody reads
/// when it matters.
///
/// `last_alert` of `None` means nothing has been published since boot, in which
/// case silence is measured from `started_at`. That distinction is not academic:
/// treating `None` as "maximally quiet" made the bot post a false "the poller
/// has stopped" alarm on every single startup, which is precisely the
/// cry-wolf failure this function exists to avoid. Found by running it.
pub fn silence_warning(
    last_alert: Option<Instant>,
    started_at: Instant,
    active: &BTreeSet<String>,
    threshold: Duration,
    now: Instant,
) -> Option<String> {
    if active.is_empty() {
        return None;
    }

    // With nothing published yet, the clock runs from process start -- a bot up
    // for five seconds has not been silent for an hour.
    let since = last_alert.unwrap_or(started_at);
    let quiet_for = now.saturating_duration_since(since);

    if quiet_for < threshold {
        return None;
    }

    let sports: Vec<&str> = active.iter().map(String::as_str).collect();
    Some(format!(
        "No alert published in {}s while {} sport(s) have live pre-game slates ({}). \
         Either nothing is clearing the threshold, or the poller has stopped.",
        quiet_for.as_secs(),
        active.len(),
        sports.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_active_set_silences_the_silence_warning() {
        let now = Instant::now();
        assert!(
            silence_warning(
                None,
                now - Duration::from_secs(99_999),
                &BTreeSet::new(),
                Duration::from_secs(0),
                now
            )
            .is_none(),
            "no slate means no fault, even with a zero threshold"
        );
    }
}
