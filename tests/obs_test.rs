use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use discord_ev_bot::obs::{quota_warning, silence_warning};
use discord_ev_bot::owls::quota::{MONTHLY_QUOTA_MVP, QuotaSnapshot};

fn healthy() -> QuotaSnapshot {
    QuotaSnapshot {
        remaining_minute: Some(399),
        remaining_month: Some(MONTHLY_QUOTA_MVP),
        unlimited_month: false,
        reset_minute_unix_ms: None,
    }
}

fn active(sports: &[&str]) -> BTreeSet<String> {
    sports.iter().map(|s| s.to_string()).collect()
}

// ------------------------------------------------------------ quota warnings

#[test]
fn warns_when_burn_rate_outpaces_the_month() {
    // Day 10 of 30 with 90% already spent: this month ends early.
    let q = QuotaSnapshot {
        remaining_month: Some(30_000),
        ..healthy()
    };
    let w = quota_warning(&q, 10, 0.8).expect("should warn");
    assert!(
        w.contains("30000") || w.contains("30,000") || w.contains('%'),
        "got: {w}"
    );
}

#[test]
fn silent_when_spending_is_on_pace() {
    let q = QuotaSnapshot {
        remaining_month: Some(200_000),
        ..healthy()
    };
    assert!(quota_warning(&q, 10, 0.8).is_none());
}

#[test]
fn unlimited_plans_never_warn_about_quota() {
    let q = QuotaSnapshot {
        remaining_month: None,
        unlimited_month: true,
        ..healthy()
    };
    assert!(quota_warning(&q, 28, 0.8).is_none());
}

#[test]
fn unknown_budget_does_not_warn() {
    // No headers yet is not a problem to report.
    assert!(quota_warning(&QuotaSnapshot::default(), 15, 0.8).is_none());
}

#[test]
fn exhausted_budget_warns_regardless_of_day() {
    let q = QuotaSnapshot {
        remaining_month: Some(0),
        ..healthy()
    };
    assert!(quota_warning(&q, 2, 0.8).is_some());
}

// ---------------------------------------------------------- silence warnings

#[test]
fn no_silence_warning_when_nothing_is_in_season() {
    // A quiet Tuesday with no slate is not a fault. Judging silence on a bare
    // timer would cry wolf every night, and an ops channel that cries wolf is
    // one nobody reads when it matters.
    let quiet = BTreeSet::new();
    let now = Instant::now();
    assert!(
        silence_warning(
            None,
            now - Duration::from_secs(99_999),
            &quiet,
            Duration::from_secs(3600),
            now
        )
        .is_none()
    );
}

#[test]
fn no_silence_warning_before_the_threshold_elapses() {
    let now = Instant::now();
    let recent = now - Duration::from_secs(60);
    assert!(
        silence_warning(
            Some(recent),
            now - Duration::from_secs(99_999),
            &active(&["nba"]),
            Duration::from_secs(3600),
            now
        )
        .is_none()
    );
}

#[test]
fn warns_on_long_silence_while_a_slate_is_live() {
    let now = Instant::now();
    let long_ago = now - Duration::from_secs(7200);
    let w = silence_warning(
        Some(long_ago),
        now - Duration::from_secs(99_999),
        &active(&["nba", "nhl"]),
        Duration::from_secs(3600),
        now,
    )
    .expect("should warn");
    assert!(w.to_lowercase().contains("alert"), "got: {w}");
}

#[test]
fn does_not_cry_wolf_at_startup() {
    // Found by running the binary: `last_alert: None` at boot was treated as
    // "maximally quiet", so the bot posted a false "the poller has stopped"
    // alarm on EVERY start. Silence must be measured from process start, not
    // from the beginning of time.
    let now = Instant::now();
    let just_started = now - Duration::from_secs(5);
    assert!(
        silence_warning(
            None,
            just_started,
            &active(&["nba", "nhl"]),
            Duration::from_secs(3600),
            now
        )
        .is_none(),
        "a freshly started bot has not been silent for an hour"
    );
}

#[test]
fn warns_when_nothing_has_been_sent_since_a_long_ago_start() {
    // Still reports the genuine case: up for hours, games live, nothing sent.
    let now = Instant::now();
    let started_long_ago = now - Duration::from_secs(7200);
    assert!(
        silence_warning(
            None,
            started_long_ago,
            &active(&["nba"]),
            Duration::from_secs(3600),
            now
        )
        .is_some()
    );
}
