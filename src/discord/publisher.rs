//! Builds Discord embeds and posts them.
//!
//! Implemented directly on `reqwest` rather than a Discord library. Phase 1's
//! entire Discord surface is `POST /channels/{id}/messages`; a full client
//! brings a gateway and websocket stack (measured: +132 crates) for no benefit
//! until v2 adds slash commands. The one thing it would earn its weight for is
//! rate-limit handling, which at this alert volume is explicit `retry_after`
//! handling in a serialized worker.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::Serialize;

use super::Alert;
use crate::owls::models::{EvEvent, Opportunity, Side};

/// Discord rejects embeds over 6000 characters total; we stay far under, but
/// descriptions get truncated defensively rather than risking a 400.
const MAX_FIELD_CHARS: usize = 1024;

/// How many times to obey a 429 before giving up. Discord's waits are short
/// (sub-second typically); an unbounded loop would hide a real problem.
const MAX_RATE_LIMIT_RETRIES: u32 = 3;

const COLOR_GOOD: u32 = 0x2ECC71; // green
const COLOR_CAUTION: u32 = 0xF1C40F; // amber, for single-source or unenriched

// ------------------------------------------------------------- embed payload

#[derive(Debug, Clone, Serialize)]
pub struct Embed {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub color: u32,
    /// Omitted entirely when absent: Discord rejects `"url": null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub fields: Vec<EmbedField>,
    pub footer: EmbedFooter,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbedFooter {
    pub text: String,
}

/// Formats American odds with an explicit sign, preserving a half point.
///
/// `bookPrice` is a JSON number, not an integer — some books price on halves —
/// so truncating to i64 would silently misreport the odds.
fn fmt_american(price: f64) -> String {
    let body = if price.fract() == 0.0 {
        format!("{}", price.abs() as i64)
    } else {
        format!("{}", price.abs())
    };
    if price < 0.0 {
        format!("-{body}")
    } else {
        format!("+{body}")
    }
}

fn side_label(side: Side, opp: &Opportunity, ev: &EvEvent) -> String {
    opp.team.clone().unwrap_or_else(|| match side {
        Side::Home => ev.home_team.clone(),
        Side::Away => ev.away_team.clone(),
        Side::Draw => "Draw".to_string(),
    })
}

fn truncate(mut s: String) -> String {
    if s.chars().count() > MAX_FIELD_CHARS {
        s = s.chars().take(MAX_FIELD_CHARS - 1).collect::<String>() + "…";
    }
    s
}

/// Builds the alert embed.
///
/// `enriched` false means the poller could not supply `booksInConsensus` and
/// `kellyFraction` (cold cache, or the sport is outside the active poll set).
/// The alert is still published and visibly marked — holding it would make a
/// silent bot right after deploy.
pub fn build_embed(ev: &EvEvent, opp: &Opportunity, enriched: bool) -> Embed {
    let pick = side_label(opp.side, opp, ev);
    let single_source = opp.books_in_consensus == Some(1.0);

    let mut fields = vec![
        EmbedField {
            name: "Pick".to_string(),
            value: truncate(format!("{pick} @ {}", fmt_american(opp.book_price))),
            inline: true,
        },
        EmbedField {
            name: "Book".to_string(),
            value: opp.book.clone(),
            inline: true,
        },
        EmbedField {
            name: "EV".to_string(),
            value: format!("{:.2}%", opp.ev_pct),
            inline: true,
        },
        EmbedField {
            name: "Fair probability".to_string(),
            value: format!("{:.1}%", opp.fair_probability * 100.0),
            inline: true,
        },
    ];

    if let Some(kelly) = opp.kelly_fraction {
        fields.push(EmbedField {
            name: "Kelly (full)".to_string(),
            value: format!("{:.2}% of bankroll", kelly * 100.0),
            inline: true,
        });
    }

    if let Some(n) = opp.books_in_consensus {
        fields.push(EmbedField {
            name: "Fair sources".to_string(),
            value: if single_source {
                "1 — Pinnacle only".to_string()
            } else {
                format!("{n:.0}")
            },
            inline: true,
        });
    }

    if let Some(age_ms) = opp.quote_age_ms {
        fields.push(EmbedField {
            name: "Quote age".to_string(),
            value: format!("{:.0}s", age_ms / 1000.0),
            inline: true,
        });
    }

    let mut notes = Vec::new();
    if single_source {
        notes.push("Pinnacle only — no independent agreement on this fair");
    }
    if !enriched {
        notes.push("unenriched — consensus and Kelly unavailable this cycle");
    }

    Embed {
        title: format!("{} @ {}", ev.away_team, ev.home_team),
        description: if notes.is_empty() {
            None
        } else {
            Some(truncate(format!("⚠ {}", notes.join(" · "))))
        },
        color: if single_source || !enriched {
            COLOR_CAUTION
        } else {
            COLOR_GOOD
        },
        url: opp.link.clone(),
        fields,
        footer: EmbedFooter {
            text: format!(
                "{} · starts {}",
                ev.sport,
                ev.commence_time.format("%b %d %H:%M UTC")
            ),
        },
    }
}

// --------------------------------------------------------------------- queue

/// Bounded alert queue that evicts the **oldest** entry when full.
///
/// Newest-wins is the right direction here: a +EV price goes stale in minutes,
/// so a backlog of dead prices crowding out the live one is the failure to
/// avoid. Drops are counted so the ops channel can report them.
pub struct AlertQueue {
    capacity: usize,
    inner: Mutex<VecDeque<Alert>>,
    dropped: AtomicUsize,
}

impl AlertQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(VecDeque::new()),
            dropped: AtomicUsize::new(0),
        }
    }

    /// Pushes an alert, returning how many were evicted to make room.
    pub fn push(&self, alert: Alert) -> usize {
        let mut q = self.inner.lock().expect("alert queue poisoned");
        let mut evicted = 0;
        while q.len() >= self.capacity {
            q.pop_front();
            evicted += 1;
        }
        q.push_back(alert);
        if evicted > 0 {
            self.dropped.fetch_add(evicted, Ordering::Relaxed);
        }
        evicted
    }

    pub fn pop(&self) -> Option<Alert> {
        self.inner.lock().expect("alert queue poisoned").pop_front()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("alert queue poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total alerts dropped to backpressure since start.
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

// ----------------------------------------------------------------- publisher

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("discord rejected the message: HTTP {status} {body}")]
    Rejected { status: u16, body: String },
    #[error("still rate limited after {0} retries")]
    RateLimitExhausted(u32),
}

#[derive(serde::Deserialize)]
struct RateLimitBody {
    retry_after: Option<f64>,
}

pub struct DiscordPublisher {
    http: reqwest::Client,
    base: String,
    token: String,
    channel_id: u64,
}

impl DiscordPublisher {
    pub fn new(base_url: &str, token: &str, channel_id: u64) -> Result<Self, PublishError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent(concat!("discord_ev_bot/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            channel_id,
        })
    }

    /// Posts one embed, obeying `retry_after` on a 429.
    ///
    /// Discord answers a rate limit with `{"retry_after": <seconds>}`. Retrying
    /// hot instead of waiting gets the bot throttled harder on that route.
    pub async fn post_embed(&self, embed: &Embed) -> Result<(), PublishError> {
        let url = format!(
            "{}/api/v10/channels/{}/messages",
            self.base, self.channel_id
        );
        let payload = serde_json::json!({ "embeds": [embed] });

        for _ in 0..=MAX_RATE_LIMIT_RETRIES {
            let resp = self
                .http
                .post(&url)
                .header("Authorization", format!("Bot {}", self.token))
                .json(&payload)
                .send()
                .await?;

            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }

            if status.as_u16() == 429 {
                let body = resp.text().await.unwrap_or_default();
                let wait = serde_json::from_str::<RateLimitBody>(&body)
                    .ok()
                    .and_then(|b| b.retry_after)
                    .unwrap_or(1.0)
                    .clamp(0.0, 60.0);
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }

            let body = resp.text().await.unwrap_or_default();
            return Err(PublishError::Rejected {
                status: status.as_u16(),
                body: body.chars().take(240).collect(),
            });
        }

        Err(PublishError::RateLimitExhausted(MAX_RATE_LIMIT_RETRIES))
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_american;

    #[test]
    fn american_odds_always_carry_a_sign() {
        assert_eq!(fmt_american(180.0), "+180");
        assert_eq!(fmt_american(-110.0), "-110");
        assert_eq!(fmt_american(117.5), "+117.5");
        assert_eq!(fmt_american(-112.5), "-112.5");
    }
}
