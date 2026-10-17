use std::time::Duration;

use discord_ev_bot::config::Config;
use discord_ev_bot::runtime::{Secrets, run, watch_config};
use tempfile::TempDir;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EMPTY_BOARD: &str = r#"{"success":true,"data":[],"meta":{
    "sport":"nba","timestamp":"2026-10-17T18:00:00.000Z",
    "freshness":{"ageSeconds":3,"stale":false}}}"#;

fn write_config(dir: &TempDir, owls: &str, discord: &str, min_ev: f64) -> std::path::PathBuf {
    let p = dir.path().join("config.toml");
    std::fs::write(
        &p,
        format!(
            r#"
owls_base_url = "{owls}"
discord_base_url = "{discord}"
min_ev_pct = {min_ev}
poll_interval_secs = 1
gating_refresh_secs = 1
lookahead_hours = 48
sports = ["nba"]
books = ["fanduel"]
"#
        ),
    )
    .unwrap();
    p
}

fn secrets() -> Secrets {
    Secrets {
        owls_api_key: "test-key".into(),
        discord_token: "test-token".into(),
        alert_channel: 1,
        ops_channel: 2,
    }
}

async fn owls_mock() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schedule"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"success":true,"days":2,"games":[]}"#),
        )
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/nba/ev"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EMPTY_BOARD))
        .mount(&s)
        .await;
    s
}

// --------------------------------------------------------------- shutdown

#[tokio::test]
async fn run_exits_cleanly_when_cancelled() {
    // Cloud platforms send SIGTERM before stopping a container. A run loop that
    // ignores cancellation gets killed mid-publish on every deploy.
    let owls = owls_mock().await;
    let discord = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(&dir, &owls.uri(), &discord.uri(), 3.5);

    let token = CancellationToken::new();
    let handle = tokio::spawn(run(cfg_path, secrets(), token.clone()));

    tokio::time::sleep(Duration::from_millis(300)).await;
    token.cancel();

    let result = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("run must return promptly after cancellation")
        .expect("task must not panic");
    assert!(result.is_ok(), "run returned {result:?}");
}

#[tokio::test]
async fn run_fails_fast_on_an_unreadable_config() {
    // Starting with compiled-in defaults when the operator expected their own
    // criteria is a failure that looks like working software.
    let result = run(
        "/nonexistent/config.toml".into(),
        secrets(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.is_err());
}

// ------------------------------------------------------------- hot reload

#[tokio::test]
async fn a_config_change_is_picked_up_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_config(&dir, "http://unused", "http://unused", 3.5);

    let (tx, mut rx) = watch::channel(Config::load(&p).unwrap());
    let _watcher = watch_config(p.clone(), tx).unwrap();
    assert_eq!(rx.borrow().min_ev_pct, 3.5);

    write_config(&dir, "http://unused", "http://unused", 6.0);

    tokio::time::timeout(Duration::from_secs(10), rx.changed())
        .await
        .expect("a config write must be observed")
        .unwrap();
    assert_eq!(rx.borrow().min_ev_pct, 6.0);
}

#[tokio::test]
async fn an_invalid_reload_keeps_the_previous_config() {
    // A typo in the file must not take the bot down mid-slate, and must not
    // silently reset the operator's criteria to defaults either.
    let dir = tempfile::tempdir().unwrap();
    let p = write_config(&dir, "http://unused", "http://unused", 3.5);

    let (tx, rx) = watch::channel(Config::load(&p).unwrap());
    let _watcher = watch_config(p.clone(), tx).unwrap();

    std::fs::write(&p, "min_ev_pct = \"not a number\"\n").unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        rx.borrow().min_ev_pct,
        3.5,
        "an invalid reload must leave the running config untouched"
    );
}

#[tokio::test]
async fn a_reload_with_an_unknown_sport_is_rejected() {
    // Validation applies on reload, not just at boot.
    let dir = tempfile::tempdir().unwrap();
    let p = write_config(&dir, "http://unused", "http://unused", 3.5);

    let (tx, rx) = watch::channel(Config::load(&p).unwrap());
    let _watcher = watch_config(p.clone(), tx).unwrap();

    std::fs::write(&p, "sports = [\"cricket\"]\nmin_ev_pct = 9.0\n").unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        rx.borrow().min_ev_pct,
        3.5,
        "a config failing validation must not be applied even in part"
    );
}

// --------------------------------------------------------------- secrets

#[test]
fn secrets_name_the_missing_variable() {
    // A startup failure that does not say which variable is missing costs the
    // operator a guessing game.
    let err = Secrets::from_env_with(|_| None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("OWLS_API_KEY"), "got: {msg}");
}

#[test]
fn secrets_reject_a_non_numeric_channel_id() {
    let err = Secrets::from_env_with(|k| match k {
        "OWLS_API_KEY" => Some("a".into()),
        "DISCORD_TOKEN" => Some("b".into()),
        "DISCORD_ALERT_CHANNEL_ID" => Some("not-a-number".into()),
        "DISCORD_OPS_CHANNEL_ID" => Some("2".into()),
        _ => None,
    })
    .unwrap_err();
    assert!(err.to_string().contains("DISCORD_ALERT_CHANNEL_ID"));
}

#[test]
fn secrets_load_when_every_variable_is_present() {
    let s = Secrets::from_env_with(|k| match k {
        "OWLS_API_KEY" => Some("key".into()),
        "DISCORD_TOKEN" => Some("tok".into()),
        "DISCORD_ALERT_CHANNEL_ID" => Some("111".into()),
        "DISCORD_OPS_CHANNEL_ID" => Some("222".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(s.alert_channel, 111);
    assert_eq!(s.ops_channel, 222);
}
