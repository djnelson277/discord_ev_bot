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
poll_interval_secs = 5
gating_refresh_secs = 60
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

#[tokio::test]
async fn a_failed_gating_pass_retries_soon_not_in_an_hour() {
    // REGRESSION (review finding #5). On a gating failure the next attempt was
    // pushed out by gating_refresh_secs (3600 default) while ctx.active kept
    // its previous value -- which at boot is EMPTY. So one 429/5xx/transport
    // blip on the first schedule call left the bot polling nothing for an hour,
    // and because silence_warning returns None whenever active is empty,
    // nothing reported it. With a bad API key it was permanent.
    let owls = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schedule"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&owls)
        .await;
    for s in ["ncaab", "mma"] {
        Mock::given(path(format!("/api/v1/{s}/events")))
            .respond_with(ResponseTemplate::new(500))
            .mount(&owls)
            .await;
    }

    let discord = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"1"}"#))
        .mount(&discord)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(&dir, &owls.uri(), &discord.uri(), 3.5);

    let token = CancellationToken::new();
    let handle = tokio::spawn(run(cfg_path, secrets(), token.clone()));
    // Long enough to pass the first 5s retry, short enough to stay a unit test.
    tokio::time::sleep(Duration::from_millis(7_500)).await;
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;

    let attempts = owls
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v1/schedule")
        .count();
    assert!(
        attempts >= 2,
        "a failed gating pass must retry on a short backoff, saw {attempts} attempt(s)"
    );
}

#[tokio::test]
async fn posts_a_heartbeat_promptly_after_startup() {
    // The first heartbeat used to be scheduled a full interval out, so a fresh
    // deploy produced 30 minutes of silence -- exactly when the operator most
    // needs confirmation, and indistinguishable from the bot having died, since
    // absence of a heartbeat IS the alarm.
    let owls = owls_mock().await;
    let discord = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v10/channels/2/messages")) // the OPS channel
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"1"}"#))
        .mount(&discord)
        .await;

    let dir = tempfile::tempdir().unwrap();
    // heartbeat_secs is 1800 in the written config; a prompt first beat must
    // not wait for it.
    let cfg_path = write_config(&dir, &owls.uri(), &discord.uri(), 3.5);

    let token = CancellationToken::new();
    let handle = tokio::spawn(run(cfg_path, secrets(), token.clone()));
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;

    let posts = discord
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().contains("/channels/2/messages"))
        .count();
    assert!(
        posts >= 1,
        "expected a startup heartbeat within seconds, saw {posts}"
    );
}

fn write_quiet_config(
    dir: &TempDir,
    owls: &str,
    discord: &str,
    start: &str,
    end: &str,
) -> std::path::PathBuf {
    let p = dir.path().join("quiet.toml");
    std::fs::write(
        &p,
        format!(
            r#"
owls_base_url = "{owls}"
discord_base_url = "{discord}"
min_ev_pct = 2.5
poll_interval_secs = 5
gating_refresh_secs = 60
heartbeat_secs = 1800
lookahead_hours = 48
timezone = "America/Denver"
quiet_hours_start = "{start}"
quiet_hours_end = "{end}"
sports = ["nba"]
books = ["fanduel"]
"#
        ),
    )
    .unwrap();
    p
}

#[tokio::test]
async fn makes_no_api_calls_during_quiet_hours() {
    // Suppressing only the alerts would still spend the whole night's quota.
    // A 7-hour window is ~29% of the day's requests, which matters when the
    // month is pacing near the cap.
    let owls = owls_mock().await;
    let discord = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"1"}"#))
        .mount(&discord)
        .await;

    let dir = tempfile::tempdir().unwrap();
    // A window covering the whole day, so the test is independent of when it runs.
    let cfg_path = write_quiet_config(&dir, &owls.uri(), &discord.uri(), "00:00", "23:59");

    let token = CancellationToken::new();
    let handle = tokio::spawn(run(cfg_path, secrets(), token.clone()));
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;

    let owls_calls = owls.received_requests().await.unwrap().len();
    assert_eq!(
        owls_calls, 0,
        "quiet hours must spend no quota at all, saw {owls_calls} request(s)"
    );
}

#[tokio::test]
async fn still_heartbeats_during_quiet_hours() {
    // Absence of a heartbeat is the liveness alarm, so it must not go silent
    // just because the bot is resting -- that would be indistinguishable from
    // a crash every single night.
    let owls = owls_mock().await;
    let discord = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v10/channels/2/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"1"}"#))
        .mount(&discord)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_quiet_config(&dir, &owls.uri(), &discord.uri(), "00:00", "23:59");

    let token = CancellationToken::new();
    let handle = tokio::spawn(run(cfg_path, secrets(), token.clone()));
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;

    let beats = discord
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().contains("/channels/2/"))
        .count();
    assert!(beats >= 1, "expected a quiet-hours heartbeat, saw {beats}");
}

#[tokio::test]
async fn polls_normally_outside_quiet_hours() {
    let owls = owls_mock().await;
    let discord = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"1"}"#))
        .mount(&discord)
        .await;

    let dir = tempfile::tempdir().unwrap();
    // A one-minute window that almost certainly is not now.
    let cfg_path = write_quiet_config(&dir, &owls.uri(), &discord.uri(), "03:00", "03:01");

    let token = CancellationToken::new();
    let handle = tokio::spawn(run(cfg_path, secrets(), token.clone()));
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;

    assert!(
        !owls.received_requests().await.unwrap().is_empty(),
        "outside the window the bot must poll as usual"
    );
}
