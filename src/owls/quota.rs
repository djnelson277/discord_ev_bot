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

/// The MVP tier's monthly request allowance. Used as the denominator for burn
/// pacing; a plan change makes this a config value rather than a constant.
pub const MONTHLY_QUOTA_MVP: u64 = 300_000;

/// Days assumed in a month for pacing. The quota resets on the billing date
/// rather than the 1st, so this is an approximation by design — it only needs
/// to be good enough to notice spending twice as fast as sustainable.
const DAYS_IN_MONTH: f64 = 30.0;

/// Never slow the poller by more than this factor, however depleted the budget.
/// An unbounded multiplier would silently turn the bot off, which is the
/// silent-failure anti-goal; running slowly and warning is strictly better.
const MAX_DEGRADATION: u32 = 8;

/// The poll interval to use next, given how much of the month's budget remains.
///
/// Returns `base` unchanged when spending is on pace, when the plan has no
/// monthly cap, or when the budget is simply unknown (before the first response
/// there are no headers, and degrading on no information would slow the bot for
/// nothing).
pub fn next_interval(
    base: std::time::Duration,
    q: &QuotaSnapshot,
    day_of_month: u32,
) -> std::time::Duration {
    if q.unlimited_month {
        return base;
    }
    let Some(remaining) = q.remaining_month else {
        return base;
    };

    let elapsed_fraction = (f64::from(day_of_month) / DAYS_IN_MONTH).clamp(0.0, 1.0);
    let expected = MONTHLY_QUOTA_MVP as f64 * (1.0 - elapsed_fraction);

    // Ahead of pace, or so late in the month that nothing is expected to remain.
    if expected <= 0.0 || remaining as f64 >= expected {
        return base;
    }

    let factor = (expected / remaining.max(1) as f64).clamp(1.0, f64::from(MAX_DEGRADATION));
    base.mul_f64(factor)
}
