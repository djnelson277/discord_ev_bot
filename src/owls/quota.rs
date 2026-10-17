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
