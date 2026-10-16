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
fn loads_defaults_from_toml() {
    let _g = guard();
    let cfg = Config::load(Path::new("config/default.toml")).unwrap();
    assert_eq!(cfg.min_ev_pct, 3.5);
    assert_eq!(cfg.poll_interval_secs, 60);
    assert_eq!(cfg.gating_refresh_secs, 3600);
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
