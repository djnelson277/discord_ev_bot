use discord_ev_bot::config::Config;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Environment variables are process-global while `cargo test` runs tests on
/// parallel threads, so any test that loads config — not just the ones that set
/// a variable — must serialize against the others. Without this, a test that
/// sets `EVBOT_MIN_EV_PCT` makes an unrelated test read the overridden value.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Ignores poisoning: a panic in one test should fail that test, not cascade
/// into every later one.
fn guard() -> std::sync::MutexGuard<'static, ()> {
    env_lock().lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn the_shipped_config_loads_and_validates() {
    // Deliberately asserts NOTHING about the operator's tuned values. An
    // earlier version pinned min_ev_pct == 3.5 against this live file, so the
    // moment the operator changed their own threshold the suite went red --
    // which teaches people to ignore test failures. What matters here is that
    // the file parses and passes validation; the values are the operator's.
    let _g = guard();
    let cfg = Config::load(Path::new("config/default.toml")).expect("shipped config must load");

    assert!(!cfg.sports.is_empty(), "sports must be configured");
    assert!(!cfg.books.is_empty(), "books must be configured");
    assert!(
        cfg.min_ev_pct >= 0.0 && cfg.min_ev_pct <= 25.0,
        "threshold {} is outside the range the server can serve",
        cfg.min_ev_pct
    );
    assert!(cfg.poll_interval_secs >= 5);
    assert!(!cfg.owls_base_url.is_empty());
}

#[test]
fn parses_exact_values_from_a_fixed_document() {
    // The parser's own contract, pinned against an inline document so it is
    // independent of whatever the operator has configured today.
    let _g = guard();
    let cfg = Config::from_toml_str(
        r#"
        min_ev_pct = 3.5
        poll_interval_secs = 60
        gating_refresh_secs = 3600
        heartbeat_secs = 1800
        lookahead_hours = 48
        sports = ["nfl","nba","mlb","nhl","ncaaf","ncaab","wnba","soccer","tennis","mma"]
        books = ["betmgm","fanduel","draftkings","bet365","novig","kalshi","polymarket"]
        "#,
    )
    .unwrap();
    assert_eq!(cfg.min_ev_pct, 3.5);
    assert_eq!(cfg.poll_interval_secs, 60);
    assert_eq!(cfg.gating_refresh_secs, 3600);
    assert_eq!(cfg.heartbeat_secs, 1800);
    assert_eq!(cfg.sports.len(), 10);
    assert!(cfg.books.contains(&"polymarket".to_string()));
}

#[test]
fn env_overrides_file() {
    let _g = guard();
    // SAFETY: the guard serializes every env-touching test in this binary.
    unsafe { std::env::set_var("EVBOT_MIN_EV_PCT", "5.0") };
    let cfg = Config::load(Path::new("config/default.toml"));
    unsafe { std::env::remove_var("EVBOT_MIN_EV_PCT") };
    assert_eq!(cfg.unwrap().min_ev_pct, 5.0);
}

#[test]
fn missing_config_file_is_an_error() {
    let _g = guard();
    let err = Config::load(Path::new("config/does-not-exist.toml")).unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "expected a not-found error, got: {err}"
    );
}

#[test]
fn rejects_unknown_sport() {
    let _g = guard();
    let err = Config::from_toml_str("sports = [\"cricket\"]\n").unwrap_err();
    assert!(
        err.to_string().contains("cricket"),
        "error should name the offending sport, got: {err}"
    );
}

#[test]
fn rejects_unknown_book() {
    let _g = guard();
    let err = Config::from_toml_str("books = [\"betfair\"]\n").unwrap_err();
    assert!(
        err.to_string().contains("betfair"),
        "error should name the offending book, got: {err}"
    );
}

#[test]
fn accepts_absent_consensus_minimum() {
    let _g = guard();
    // default.toml leaves it commented out; absence must not be an error
    let cfg = Config::load(Path::new("config/default.toml")).unwrap();
    assert_eq!(cfg.min_books_in_consensus, None);
}

#[test]
fn rejects_a_zero_poll_interval() {
    // REGRESSION (review finding #4). poll_interval_secs = 0 passed validation
    // and made the main loop a tight request loop: 270 EV requests in 500ms
    // against a mock, i.e. instant per-minute rate limiting and the whole
    // monthly quota inside a day. The hot-reload path accepted it live too, so
    // a mid-slate typo could take the account out.
    let _g = guard();
    let err = Config::from_toml_str("poll_interval_secs = 0\n").unwrap_err();
    assert!(err.to_string().contains("poll_interval_secs"), "got: {err}");
}

#[test]
fn rejects_a_poll_interval_below_the_floor() {
    let _g = guard();
    assert!(Config::from_toml_str("poll_interval_secs = 1\n").is_err());
    assert!(Config::from_toml_str("poll_interval_secs = 5\n").is_ok());
}

#[test]
fn rejects_an_absurd_gating_refresh() {
    let _g = guard();
    assert!(Config::from_toml_str("gating_refresh_secs = 0\n").is_err());
}

#[test]
fn rejects_an_empty_sports_or_books_list() {
    // Both are #[serde(default)], so a mistyped key name yields an empty list:
    // a bot that polls nothing, or rejects everything, silently. An empty
    // active set also mutes the silence alarm, so nothing would report it.
    let _g = guard();
    assert!(Config::from_toml_str("sports = []\n").is_err());
    assert!(Config::from_toml_str("books = []\n").is_err());
}

#[test]
fn rejects_a_threshold_above_the_server_ceiling() {
    // The API drops anything over 25% as a data error, so a threshold above it
    // can never match and would silence the bot completely.
    let _g = guard();
    assert!(Config::from_toml_str("min_ev_pct = 30.0\n").is_err());
    assert!(Config::from_toml_str("min_ev_pct = -1.0\n").is_err());
}

#[test]
fn rejects_an_out_of_range_lookahead() {
    let _g = guard();
    assert!(Config::from_toml_str("lookahead_hours = 0\n").is_err());
    assert!(Config::from_toml_str("lookahead_hours = 10000\n").is_err());
}
