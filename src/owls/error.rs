//! Error taxonomy for Owls Insight calls.
//!
//! The variants exist so callers can react differently: a `RateLimited` means
//! wait exactly as told, a `Forbidden` means the plan cannot do this and
//! retrying will never help, and a `Decode` means the API changed shape under
//! us and a human should look.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum OwlsError {
    /// 429. Covers the per-minute limit, the monthly quota, the 15-concurrent
    /// cap and the history in-flight cap. `retry_after` is from the header, or
    /// a safe default when absent.
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },

    /// 401. The key is missing or wrong; retrying is pointless.
    #[error("missing or invalid API key")]
    Unauthorized,

    /// 403. The plan lacks the feature. Also pointless to retry.
    #[error("plan lacks this feature (current {current_tier:?}, requires {required_tier:?})")]
    Forbidden {
        required_tier: Option<String>,
        current_tier: Option<String>,
        upgrade_url: Option<String>,
    },

    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The response was not the shape we expect. Worth surfacing loudly rather
    /// than skipping: it usually means the API changed.
    #[error("could not decode response: {0}")]
    Decode(String),

    #[error("unexpected HTTP status {0}")]
    Status(u16),

    #[error("invalid base url: {0}")]
    InvalidBaseUrl(String),
}

impl OwlsError {
    /// Whether waiting and trying the same request again could plausibly work.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport(_) => true,
            Self::Status(code) => *code >= 500,
            Self::Unauthorized
            | Self::Forbidden { .. }
            | Self::Decode(_)
            | Self::InvalidBaseUrl(_) => false,
        }
    }
}
