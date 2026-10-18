//! Wiring: secrets, the three loops, config hot-reload, graceful shutdown.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Datelike, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::{Config, ConfigError};
use crate::dedupe::Suppressor;
use crate::discord::publisher::{AlertQueue, DiscordPublisher, build_embed};
use crate::gating::active_sports;
use crate::obs::{heartbeat_message, quota_warning};
use crate::owls::client::OwlsClient;
use crate::owls::quota::{Pacing, next_interval};
use crate::poller::{PollContext, poll_once};

/// How long a published alert key stays suppressed. Mirrors the server's
/// `ev.found` rule so push and poll behave identically.
const SUPPRESSION_WINDOW: Duration = Duration::from_secs(2 * 60 * 60);

/// EV rise, in percentage points, that overrides suppression.
const SUPPRESSION_RISE_PP: f64 = 1.0;

/// Alerts buffered before the oldest are dropped.
const ALERT_QUEUE_CAPACITY: usize = 256;

/// Fraction of the expected remaining monthly budget below which to warn.
const QUOTA_WARN_AT: f64 = 0.8;

/// How often to re-check health. Independent of the poll interval so quota
/// degradation does not also slow down the reporting of that degradation.
const HEALTH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// First retry delay after a failed gating pass, doubling each consecutive
/// failure and capped at the configured refresh interval.
///
/// Without a short retry a single transport blip on the first schedule call
/// left `active` empty for a full hour — and an empty active set also mutes the
/// silence alarm, so the bot sat completely idle and completely unmonitored.
/// Starting short matters most at boot, where there is no previous active set
/// to fall back on and the bot is doing nothing at all until gating succeeds.
/// Doubling keeps a sustained outage from costing three requests every five
/// seconds indefinitely.
const GATING_RETRY_BASE: Duration = Duration::from_secs(5);

/// Consecutive gating failures before telling the operator. One blip is noise;
/// a run of them means the bot is polling nothing.
const GATING_FAILURES_BEFORE_ALARM: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("missing required environment variable {0} (see .env.example)")]
    MissingEnv(&'static str),
    #[error("environment variable {name} must be a number, got {value:?}")]
    NotANumber { name: &'static str, value: String },
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("could not build HTTP client: {0}")]
    Client(String),
    #[error("could not watch the config file: {0}")]
    Watch(String),
}

/// Credentials and channel targets. Never read from the config file: a secret in
/// a file that gets committed is the one mistake with no undo.
#[derive(Debug, Clone)]
pub struct Secrets {
    pub owls_api_key: String,
    pub discord_token: String,
    pub alert_channel: u64,
    pub ops_channel: u64,
}

impl Secrets {
    pub fn from_env() -> Result<Self, StartupError> {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    /// Reads through an injected lookup so the failure paths are testable
    /// without mutating process environment, which is global and would make
    /// parallel tests flaky.
    pub fn from_env_with<F>(get: F) -> Result<Self, StartupError>
    where
        F: Fn(&'static str) -> Option<String>,
    {
        fn require<F>(get: &F, name: &'static str) -> Result<String, StartupError>
        where
            F: Fn(&'static str) -> Option<String>,
        {
            get(name)
                .filter(|v| !v.trim().is_empty())
                .ok_or(StartupError::MissingEnv(name))
        }

        fn require_u64<F>(get: &F, name: &'static str) -> Result<u64, StartupError>
        where
            F: Fn(&'static str) -> Option<String>,
        {
            let raw = require(get, name)?;
            raw.trim()
                .parse()
                .map_err(|_| StartupError::NotANumber { name, value: raw })
        }

        Ok(Self {
            owls_api_key: require(&get, "OWLS_API_KEY")?,
            discord_token: require(&get, "DISCORD_TOKEN")?,
            alert_channel: require_u64(&get, "DISCORD_ALERT_CHANNEL_ID")?,
            ops_channel: require_u64(&get, "DISCORD_OPS_CHANNEL_ID")?,
        })
    }
}

/// Watches the config file, publishing validated reloads onto `tx`.
///
/// A reload that fails to parse or validate is logged and **discarded**: the
/// running config stays in place. Neither crashing nor silently reverting to
/// defaults is acceptable — one takes the bot down mid-slate, the other resets
/// the operator's criteria without telling them.
///
/// The returned watcher must be kept alive; dropping it stops the watching.
pub fn watch_config(
    path: PathBuf,
    tx: watch::Sender<Config>,
) -> Result<RecommendedWatcher, StartupError> {
    let watched = path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(_) => match Config::load(&watched) {
                Ok(cfg) => {
                    if tx.send(cfg).is_err() {
                        // Every receiver is gone, so the service is shutting down.
                    } else {
                        info!("configuration reloaded");
                    }
                }
                Err(err) => warn!(
                    error = %err,
                    "config reload rejected; keeping the previous configuration"
                ),
            },
            Err(err) => warn!(error = %err, "config watch error"),
        }
    })
    .map_err(|e| StartupError::Watch(e.to_string()))?;

    // Watch the parent directory, not the file. Editors routinely replace a file
    // by writing a temp file and renaming it over the original, which destroys
    // the inode a file watch is bound to and makes further edits invisible.
    let dir = path.parent().unwrap_or(Path::new("."));
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(|e| StartupError::Watch(e.to_string()))?;

    Ok(watcher)
}

/// Runs the service until `shutdown` is cancelled.
///
/// Three cooperating loops: gating refresh, the poll loop, and the publisher.
/// On cancellation the poller stops producing and the publisher drains what is
/// already queued before returning, so a deploy does not discard alerts that
/// were a moment from being sent.
pub async fn run(
    config_path: PathBuf,
    secrets: Secrets,
    shutdown: CancellationToken,
) -> Result<(), StartupError> {
    let initial = Config::load(&config_path)?;
    info!(
        sports = ?initial.sports,
        books = ?initial.books,
        min_ev_pct = initial.min_ev_pct,
        "starting"
    );

    let (cfg_tx, cfg_rx) = watch::channel(initial.clone());
    let _watcher = watch_config(config_path, cfg_tx)?;

    let queue = Arc::new(AlertQueue::new(ALERT_QUEUE_CAPACITY));

    let publisher = DiscordPublisher::new(
        &initial.discord_base_url,
        &secrets.discord_token,
        secrets.alert_channel,
    )
    .map_err(|e| StartupError::Client(e.to_string()))?;
    let ops_publisher = DiscordPublisher::new(
        &initial.discord_base_url,
        &secrets.discord_token,
        secrets.ops_channel,
    )
    .map_err(|e| StartupError::Client(e.to_string()))?;

    let mut ctx = PollContext {
        client: OwlsClient::new(&initial.owls_base_url, &secrets.owls_api_key)
            .map_err(|e| StartupError::Client(e.to_string()))?,
        suppressor: Suppressor::new(SUPPRESSION_WINDOW, SUPPRESSION_RISE_PP),
        queue: Arc::clone(&queue),
        active: BTreeSet::new(),
        seeded: HashSet::new(),
        venue_sports: HashSet::new(),
    };

    let publish_queue = Arc::clone(&queue);
    let publish_token = shutdown.clone();
    let publisher_task = tokio::spawn(async move {
        loop {
            match publish_queue.pop() {
                Some(alert) => {
                    let embed = build_embed(&alert.event, &alert.opportunity, alert.enriched);
                    if let Err(err) = publisher.post_embed(&embed).await {
                        // Logged, not retried: by the time a Discord failure
                        // resolves the price is usually gone, and a retry queue
                        // would publish dead alerts.
                        error!(error = %err, "failed to publish alert");
                    }
                }
                None => {
                    if publish_token.is_cancelled() {
                        break; // queue drained and we are shutting down
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        info!("publisher drained");
    });

    let mut gating_due = Instant::now();
    let mut health_due = Instant::now();
    let mut gating_failures: u32 = 0;
    let mut heartbeat_due = Instant::now() + Duration::from_secs(initial.heartbeat_secs);
    let mut alerts_since_heartbeat: usize = 0;
    let mut best_since_heartbeat: Option<crate::poller::BestEv> = None;

    while !shutdown.is_cancelled() {
        let cfg = cfg_rx.borrow().clone();

        if Instant::now() >= gating_due {
            match active_sports(&ctx.client, &cfg, Utc::now()).await {
                Ok(active) => {
                    info!(active = ?active, "gating refreshed");
                    ctx.active = active;
                    gating_failures = 0;
                    gating_due = Instant::now() + Duration::from_secs(cfg.gating_refresh_secs);
                }
                Err(err) => {
                    gating_failures += 1;
                    warn!(
                        error = %err,
                        consecutive = gating_failures,
                        "gating refresh failed; retrying sooner than the full interval"
                    );
                    // Retry soon, not in an hour. Keeping the previous set is
                    // useless at boot, where it is empty -- the bot would poll
                    // nothing, and the silence alarm is muted by an empty
                    // active set, so nothing would report it.
                    let backoff = GATING_RETRY_BASE
                        .saturating_mul(1u32 << gating_failures.saturating_sub(1).min(10))
                        .min(Duration::from_secs(cfg.gating_refresh_secs));
                    gating_due = Instant::now() + backoff;

                    if gating_failures == GATING_FAILURES_BEFORE_ALARM {
                        let msg = format!(
                            "Gating has failed {gating_failures} times in a row ({err}). \
                             Active sports: {}. While this persists the poller has nothing \
                             to poll.",
                            if ctx.active.is_empty() {
                                "none".to_string()
                            } else {
                                ctx.active.iter().cloned().collect::<Vec<_>>().join(", ")
                            }
                        );
                        warn!(warning = %msg, "health");
                        if let Err(e) = ops_publisher.post_embed(&ops_embed(&msg)).await {
                            error!(error = %e, "failed to publish gating alarm");
                        }
                    }
                }
            }
        }

        let outcome = poll_once(&mut ctx, &cfg, Instant::now()).await;
        // Read the outcome, never the queue length. The publisher is a
        // concurrent consumer that pops as soon as anything is queued, and
        // poll_once awaits a network round-trip per sport, so an alert pushed
        // while polling sport 1 of 10 is normally already sent by the time
        // poll_once returns and the length is unchanged.
        alerts_since_heartbeat += outcome.published;
        if let Some(best) = &outcome.best_ev {
            if best_since_heartbeat
                .as_ref()
                .is_none_or(|b| best.ev_pct > b.ev_pct)
            {
                best_since_heartbeat = Some(best.clone());
            }
        }
        info!(
            polled = outcome.polled,
            published = outcome.published,
            suppressed = outcome.suppressed,
            rejected = outcome.rejected,
            stale = outcome.stale_boards,
            errors = outcome.errors,
            "poll complete"
        );

        let quota = ctx.client.quota();
        let day = Utc::now().day();
        let pacing = Pacing {
            base: Duration::from_secs(cfg.poll_interval_secs),
            day_of_month: day,
            active_sports: ctx.active.len(),
            has_venue_sports: ctx.active.iter().any(|s| ctx.venue_sports.contains(s)),
        };
        let interval = next_interval(&quota, pacing);

        // The heartbeat is the liveness signal, and its content explains the
        // quiet. It replaces the old silence warning, which could only say
        // "either nothing cleared the threshold or the poller has stopped" --
        // the two states the operator most needs told apart.
        if Instant::now() >= heartbeat_due {
            let msg = heartbeat_message(
                &ctx.active,
                best_since_heartbeat.take(),
                cfg.min_ev_pct,
                alerts_since_heartbeat,
                &quota,
            );
            info!(heartbeat = %msg, "heartbeat");
            if let Err(err) = ops_publisher.post_embed(&heartbeat_embed(&msg)).await {
                error!(error = %err, "failed to publish heartbeat");
            }
            alerts_since_heartbeat = 0;
            heartbeat_due = Instant::now() + Duration::from_secs(cfg.heartbeat_secs);
        }

        if Instant::now() >= health_due {
            for warning in [quota_warning(&quota, pacing, QUOTA_WARN_AT)]
                .into_iter()
                .flatten()
            {
                warn!(warning = %warning, "health");
                let embed = ops_embed(&warning);
                if let Err(err) = ops_publisher.post_embed(&embed).await {
                    error!(error = %err, "failed to publish health warning");
                }
            }
            health_due = Instant::now() + HEALTH_INTERVAL;
        }

        // Sleep, but wake immediately on shutdown rather than finishing a full
        // interval first.
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
    }

    info!("shutdown requested; draining publisher");
    let _ = publisher_task.await;
    Ok(())
}

/// A calm, visually distinct heartbeat so it never reads as an alarm.
fn heartbeat_embed(message: &str) -> crate::discord::publisher::Embed {
    use crate::discord::publisher::{Embed, EmbedFooter};
    Embed {
        title: "Heartbeat".to_string(),
        description: Some(message.to_string()),
        color: 0x95A5A6, // grey: informational, not a warning
        url: None,
        fields: Vec::new(),
        footer: EmbedFooter {
            text: "discord_ev_bot".to_string(),
        },
    }
}

fn ops_embed(message: &str) -> crate::discord::publisher::Embed {
    use crate::discord::publisher::{Embed, EmbedFooter};
    Embed {
        title: "Health".to_string(),
        description: Some(message.to_string()),
        color: 0xE67E22,
        url: None,
        fields: Vec::new(),
        footer: EmbedFooter {
            text: "discord_ev_bot".to_string(),
        },
    }
}
