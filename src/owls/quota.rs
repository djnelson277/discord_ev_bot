//! Quota accounting from response headers.
//!
//! Every REST response carries the remaining budget, so the poller never has to
//! guess how much of the month it has spent.

use reqwest::header::HeaderMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaSnapshot {
    pub remaining_minute: Option<u64>,
    pub remaining_month: Option<u64>,
    /// True when the plan has no monthly cap. The header carries the literal
    /// string "unlimited" in that case, which is not a number.
    pub unlimited_month: bool,
    pub reset_minute_unix_ms: Option<u64>,
}

impl QuotaSnapshot {
    pub fn from_headers(h: &HeaderMap) -> Self {
        let num = |name: &str| -> Option<u64> { h.get(name)?.to_str().ok()?.trim().parse().ok() };
        let month_raw = h
            .get("x-ratelimit-remaining-month")
            .and_then(|v| v.to_str().ok())
            .map(str::trim);

        Self {
            remaining_minute: num("x-ratelimit-remaining-minute"),
            remaining_month: month_raw.and_then(|v| v.parse().ok()),
            unlimited_month: month_raw.is_some_and(|v| v.eq_ignore_ascii_case("unlimited")),
            reset_minute_unix_ms: num("x-ratelimit-reset-minute"),
        }
    }
}

/// The MVP tier's monthly request allowance.
///
/// A compile-time constant because the operator is on MVP. A different plan
/// makes the burn warning wrong by a constant factor, so this moves to config
/// if the plan ever changes.
pub const MONTHLY_QUOTA_MVP: u64 = 300_000;

/// Days assumed in a billing cycle for pacing.
///
/// The quota resets on the billing date rather than the 1st, so pacing keyed to
/// the calendar day can be out of phase by up to a few weeks: slightly lax just
/// after a mid-month reset, slightly strict just before one. Acceptable because
/// the governor only has to prevent exhaustion, not optimise spend.
const DAYS_IN_CYCLE: f64 = 30.0;

/// Never slow the poller by more than this factor, however depleted the budget.
/// An unbounded multiplier would silently turn the bot off, which is the
/// silent-failure anti-goal; running slowly and warning is strictly better.
const MAX_DEGRADATION: f64 = 8.0;

/// Seconds in a day, for projecting daily request spend.
const SECS_PER_DAY: f64 = 86_400.0;

/// Hard ceiling on the poll interval while any active sport carries exchange or
/// prediction-market prices.
///
/// `venues=true` quotes are warm-on-demand: they appear ~15s after you start
/// asking and stay live only "while you keep asking". Stretching past this
/// switches them **off** rather than merely making them stale, which silently
/// loses Kalshi and Polymarket — and Polymarket is the one configured book the
/// poller is the sole source for, since webhooks refuse it.
pub const VENUE_MAX_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);

/// Everything the pacing decision needs besides the quota reading.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// The operator's configured interval.
    pub base: std::time::Duration,
    pub day_of_month: u32,
    /// How many sports this cycle will poll. Each one is a request.
    pub active_sports: usize,
    /// Whether any active sport has shown an exchange or prediction-market
    /// price, which imposes [`VENUE_MAX_INTERVAL`].
    pub has_venue_sports: bool,
}

/// The poll interval to use next.
///
/// **Predictive, not reactive.** It projects what the configured interval would
/// spend over the days left in the cycle and slows down if that exceeds the
/// remaining budget. The earlier reactive version only engaged once spending
/// had already fallen behind pace — by which point the correction was ~1.0 —
/// and let 8-10 active sports exhaust the month around day 29.
///
/// Returns `base` unchanged when the plan has no monthly cap, when the budget is
/// unknown (no headers yet, and degrading on no information slows the bot for
/// nothing), and when nothing is active so there is no spend to pace.
pub fn next_interval(q: &QuotaSnapshot, p: Pacing) -> std::time::Duration {
    let mut interval = p.base;

    if !q.unlimited_month && p.active_sports > 0 {
        if let Some(remaining) = q.remaining_month {
            // At least one day, so the final day of the cycle still paces --
            // the old `expected <= 0` guard made the governor switch itself off
            // on exactly the day it was most needed.
            let days_left = (DAYS_IN_CYCLE - f64::from(p.day_of_month)).max(1.0);
            let affordable_per_day = remaining as f64 / days_left;

            let base_secs = p.base.as_secs_f64().max(1.0);
            let needed_per_day = (SECS_PER_DAY / base_secs) * p.active_sports as f64;

            if needed_per_day > affordable_per_day && affordable_per_day > 0.0 {
                let factor = (needed_per_day / affordable_per_day).clamp(1.0, MAX_DEGRADATION);
                interval = p.base.mul_f64(factor);
            } else if affordable_per_day <= 0.0 {
                interval = p.base.mul_f64(MAX_DEGRADATION);
            }
        }
    }

    if p.has_venue_sports {
        // A ceiling on degradation, never a speed-up: max() of the base keeps a
        // deliberately slow configured interval intact.
        interval = interval.min(VENUE_MAX_INTERVAL.max(p.base));
    }

    interval
}

/// Whether the governor is currently slowing the poller down.
pub fn is_degraded(q: &QuotaSnapshot, p: Pacing) -> bool {
    next_interval(q, p) > p.base
}
