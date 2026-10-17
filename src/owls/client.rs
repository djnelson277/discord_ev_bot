//! HTTP client for the Owls Insight REST API.
//!
//! One `reqwest::Client` is built per `OwlsClient` and reused for every request.
//! That matters more than usual here: a fresh client per call leaks connection
//! pools, and the API refuses requests past 15 concurrent with an immediate 429
//! rather than queueing them.

use std::sync::Mutex;
use std::time::Duration;

use reqwest::StatusCode;

use super::error::OwlsError;
use super::models::{EvResponse, EventsResponse, ScheduleResponse};
use super::quota::QuotaSnapshot;

/// Used when a 429 arrives without a `Retry-After` header. The docs specify 2
/// seconds for the concurrency and history caps, and anything above zero is
/// enough to stop a busy loop.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(2);

pub struct OwlsClient {
    http: reqwest::Client,
    base: String,
    api_key: String,
    quota: Mutex<QuotaSnapshot>,
}

impl OwlsClient {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, OwlsError> {
        if base_url.is_empty() {
            return Err(OwlsError::InvalidBaseUrl("empty".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("discord_ev_bot/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            quota: Mutex::new(QuotaSnapshot::default()),
        })
    }

    /// The most recent quota reading. Updated on every response, including
    /// error responses — a 429 still tells us where we stand.
    pub fn quota(&self) -> QuotaSnapshot {
        self.quota.lock().expect("quota mutex poisoned").clone()
    }

    /// Positive-EV opportunities for one sport.
    ///
    /// `venues=true` is always sent: without it Kalshi and Polymarket are
    /// absent, and Polymarket is the one configured book that webhooks cannot
    /// deliver, so the poller is its only source.
    pub async fn get_ev(&self, sport: &str, min_ev: f64) -> Result<EvResponse, OwlsError> {
        let url = format!("{}/api/v1/{sport}/ev", self.base);
        self.get_json(
            &url,
            &[("venues", "true".into()), ("min_ev", fmt_num(min_ev))],
        )
        .await
    }

    /// Every game across the covered sports for a calendar day.
    ///
    /// Pass `days = 2`: `days=1` returns only the current day in the response's
    /// timezone, so a late-evening refresh would see none of tomorrow's slate.
    /// Does not cover `ncaab`, and carries `mma` only as `ufc` — use
    /// [`Self::get_events`] for those.
    pub async fn get_schedule(&self, days: u8) -> Result<ScheduleResponse, OwlsError> {
        let url = format!("{}/api/v1/schedule", self.base);
        self.get_json(&url, &[("days", days.to_string())]).await
    }

    /// Upcoming events for one sport, without prices. The gating fallback for
    /// the sports `/schedule` does not serve.
    pub async fn get_events(&self, sport: &str) -> Result<EventsResponse, OwlsError> {
        let url = format!("{}/api/v1/{sport}/events", self.base);
        self.get_json(&url, &[]).await
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<T, OwlsError> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.api_key)
            .query(query)
            .send()
            .await?;

        let status = resp.status();
        let headers = resp.headers().clone();

        // Record quota before any early return: an error response still carries
        // the headers, and a 429 is exactly when we most want to know.
        if let Ok(mut slot) = self.quota.lock() {
            *slot = QuotaSnapshot::from_headers(&headers);
        }

        // 403 is handled before the generic mapping because its useful detail
        // (which tier the call needs) is in the body, not the headers.
        if status == StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            return Err(parse_forbidden(&body));
        }

        if let Some(err) = classify(status, &headers) {
            return Err(err);
        }

        let body = resp.text().await?;
        serde_json::from_str(&body).map_err(|e| {
            // Keep a bounded excerpt: enough to see what arrived, not enough to
            // dump a 250KB schedule response into the logs.
            let excerpt: String = body.chars().take(240).collect();
            OwlsError::Decode(format!("{e} (body began: {excerpt})"))
        })
    }
}

/// Maps a non-success status onto the error taxonomy. Returns `None` for 2xx.
fn classify(status: StatusCode, headers: &reqwest::header::HeaderMap) -> Option<OwlsError> {
    if status.is_success() {
        return None;
    }
    match status {
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_RETRY_AFTER);
            Some(OwlsError::RateLimited { retry_after })
        }
        StatusCode::UNAUTHORIZED => Some(OwlsError::Unauthorized),
        // FORBIDDEN is handled in get_json, where the body is available.
        other => Some(OwlsError::Status(other.as_u16())),
    }
}

/// Extracts the tier detail a 403 carries. A body that is missing or not JSON
/// still yields a `Forbidden` — the status alone is the authoritative signal,
/// and the fields only improve the message.
fn parse_forbidden(body: &str) -> OwlsError {
    let v: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let field =
        |key: &str| -> Option<String> { v.as_ref()?.get(key)?.as_str().map(str::to_string) };
    OwlsError::Forbidden {
        required_tier: field("requiredTier"),
        current_tier: field("currentTier"),
        upgrade_url: field("upgradeUrl"),
    }
}

/// Formats a float for a query string without a trailing `.0`.
///
/// `min_ev` is derived from the operator's threshold via `floor()`, so 3.5
/// becomes 3.0 — which must be sent as `min_ev=3`.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_num;

    #[test]
    fn whole_floats_lose_the_decimal() {
        assert_eq!(fmt_num(3.0), "3");
        assert_eq!(fmt_num(3.5_f64.floor()), "3");
        assert_eq!(fmt_num(0.0), "0");
    }

    #[test]
    fn fractional_floats_are_preserved() {
        assert_eq!(fmt_num(2.5), "2.5");
    }
}
