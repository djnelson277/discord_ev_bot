use std::collections::BTreeSet;
use std::time::Duration;

use discord_ev_bot::obs::{heartbeat_message, quota_warning};
use discord_ev_bot::owls::quota::{MONTHLY_QUOTA_MVP, Pacing, QuotaSnapshot};
use discord_ev_bot::poller::BestEv;

fn healthy() -> QuotaSnapshot {
    QuotaSnapshot {
        remaining_minute: Some(399),
        remaining_month: Some(MONTHLY_QUOTA_MVP),
        unlimited_month: false,
        reset_minute_unix_ms: None,
    }
}

fn pacing(active_sports: usize, day_of_month: u32) -> Pacing {
    Pacing {
        base: Duration::from_secs(60),
        day_of_month,
        active_sports,
        has_venue_sports: false,
    }
}

// ------------------------------------------------------------ quota warnings

#[test]
fn warns_when_burn_rate_outpaces_the_month() {
    // Day 10 of 30 with 90% already spent: this month ends early.
    let q = QuotaSnapshot {
        remaining_month: Some(30_000),
        ..healthy()
    };
    let w = quota_warning(&q, pacing(10, 10), 0.8).expect("should warn");
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
    assert!(quota_warning(&q, pacing(5, 10), 0.8).is_none());
}

#[test]
fn unlimited_plans_never_warn_about_quota() {
    let q = QuotaSnapshot {
        remaining_month: None,
        unlimited_month: true,
        ..healthy()
    };
    assert!(quota_warning(&q, pacing(10, 28), 0.8).is_none());
}

#[test]
fn unknown_budget_does_not_warn() {
    // No headers yet is not a problem to report.
    assert!(quota_warning(&QuotaSnapshot::default(), pacing(10, 15), 0.8).is_none());
}

#[test]
fn exhausted_budget_warns_regardless_of_day() {
    let q = QuotaSnapshot {
        remaining_month: Some(0),
        ..healthy()
    };
    assert!(quota_warning(&q, pacing(10, 2), 0.8).is_some());
}

// ------------------------------------------------------------- heartbeat

#[test]
fn heartbeat_names_the_near_miss_and_the_threshold() {
    // The whole point: make silence self-explaining. "Best EV was 2.91% and
    // your bar is 3.5%" tells the operator the bot is alive AND why it is quiet.
    let msg = heartbeat_message(
        &["nfl".to_string(), "nba".to_string()].into_iter().collect(),
        Some(BestEv {
            ev_pct: 2.91,
            book: "draftkings".into(),
            sport: "nfl".into(),
            matchup: "Arizona Cardinals @ Los Angeles Rams".into(),
        }),
        3.5,
        0,
        &QuotaSnapshot {
            remaining_month: Some(287_000),
            ..healthy()
        },
    );
    assert!(msg.contains("2.91"), "best EV missing: {msg}");
    assert!(msg.contains("3.5"), "threshold missing: {msg}");
    assert!(msg.contains("draftkings"), "book missing: {msg}");
    assert!(msg.contains("Rams"), "matchup missing: {msg}");
}

#[test]
fn heartbeat_says_so_when_no_price_was_positive_at_all() {
    // Today's actual state across all ten sports: not one positive-EV price.
    let msg = heartbeat_message(
        &["nfl".to_string()].into_iter().collect(),
        None,
        3.5,
        0,
        &healthy(),
    );
    assert!(
        msg.to_lowercase().contains("no positive"),
        "should distinguish 'nothing positive' from 'nothing cleared': {msg}"
    );
}

#[test]
fn heartbeat_reports_alerts_when_there_were_some() {
    let msg = heartbeat_message(
        &["nfl".to_string()].into_iter().collect(),
        None,
        3.5,
        4,
        &healthy(),
    );
    assert!(msg.contains('4'), "alert count missing: {msg}");
}

#[test]
fn heartbeat_mentions_an_idle_bot_rather_than_pretending_to_watch() {
    // No active sports is a legitimate quiet night, and the heartbeat should say
    // that plainly instead of implying it is scanning.
    let msg = heartbeat_message(&BTreeSet::new(), None, 3.5, 0, &healthy());
    assert!(
        msg.to_lowercase().contains("no sport"),
        "should state there is nothing in season: {msg}"
    );
}
