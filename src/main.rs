//! Entry point.
//!
//! Reads secrets from the environment, initialises structured logging, and runs
//! until SIGTERM or SIGINT. Config lives in a file so the operator can retune
//! thresholds without a redeploy; secrets never do.

use std::path::PathBuf;

use discord_ev_bot::obs::init_tracing;
use discord_ev_bot::runtime::{Secrets, run};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    init_tracing();

    let secrets = match Secrets::from_env() {
        Ok(s) => s,
        Err(err) => {
            error!(error = %err, "startup failed");
            return std::process::ExitCode::FAILURE;
        }
    };

    let config_path = std::env::var("EVBOT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config/default.toml"));

    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone());

    match run(config_path, secrets, shutdown).await {
        Ok(()) => {
            info!("stopped cleanly");
            std::process::ExitCode::SUCCESS
        }
        Err(err) => {
            error!(error = %err, "stopped with an error");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Cancels on SIGTERM or SIGINT.
///
/// SIGTERM matters for deployment: container platforms send it before stopping
/// a task, and a process that ignores it gets killed mid-publish on every
/// deploy instead of draining its queue.
fn spawn_signal_handler(token: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(err) => {
                    error!(error = %err, "could not install SIGTERM handler");
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => info!("SIGTERM received"),
                _ = tokio::signal::ctrl_c() => info!("SIGINT received"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            info!("interrupt received");
        }
        token.cancel();
    });
}
