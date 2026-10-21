use std::collections::BTreeSet;
use std::time::Duration;

use discord_ev_bot::obs::{Heartbeat, heartbeat_message, quota_warning};
use discord_ev_bot::owls::quota::{MONTHLY_QUOTA_MVP, Pacing, QuotaSnapshot};
use discord_ev_bot::poller::{BestEv, BestEvOutcome};

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
    let msg = heartbeat_message(Heartbeat {
        active: &["nfl".to_string(), "nba".to_string()].into_iter().collect(),
        best: Some(BestEv {
            ev_pct: 2.91,
            book: "draftkings".into(),
            sport: "nfl".into(),
            matchup: "Arizona Cardinals @ Los Angeles Rams".into(),
            outcome: BestEvOutcome::Rejected(
                "EV 2.91% does not exceed the configured 3.50% threshold".into(),
            ),
        }),
        threshold_pct: 3.5,
        queued: 0,
        delivered: 0,
        quota: &QuotaSnapshot {
            remaining_month: Some(287_000),
            ..healthy()
        },
        quiet_until: None,
    });
    assert!(msg.contains("2.91"), "best EV missing: {msg}");
    assert!(msg.contains("3.5"), "threshold missing: {msg}");
    assert!(msg.contains("draftkings"), "book missing: {msg}");
    assert!(msg.contains("Rams"), "matchup missing: {msg}");
}

#[test]
fn heartbeat_says_so_when_no_price_was_positive_at_all() {
    // Today's actual state across all ten sports: not one positive-EV price.
    let msg = heartbeat_message(Heartbeat {
        active: &["nfl".to_string()].into_iter().collect(),
        best: None,
        threshold_pct: 3.5,
        queued: 0,
        delivered: 0,
        quota: &healthy(),
        quiet_until: None,
    });
    assert!(
        msg.to_lowercase().contains("no positive"),
        "should distinguish 'nothing positive' from 'nothing cleared': {msg}"
    );
}

#[test]
fn heartbeat_reports_delivered_alerts() {
    let msg = heartbeat_message(Heartbeat {
        active: &["nfl".to_string()].into_iter().collect(),
        best: None,
        threshold_pct: 3.5,
        queued: 4,
        delivered: 4,
        quota: &healthy(),
        quiet_until: None,
    });
    assert!(msg.contains('4'), "alert count missing: {msg}");
    assert!(
        !msg.to_lowercase().contains("warning"),
        "no shortfall, so no warning: {msg}"
    );
}

#[test]
fn heartbeat_mentions_an_idle_bot_rather_than_pretending_to_watch() {
    // No active sports is a legitimate quiet night, and the heartbeat should say
    // that plainly instead of implying it is scanning.
    let msg = heartbeat_message(Heartbeat {
        active: &BTreeSet::new(),
        best: None,
        threshold_pct: 3.5,
        queued: 0,
        delivered: 0,
        quota: &healthy(),
        quiet_until: None,
    });
    assert!(
        msg.to_lowercase().contains("no sport"),
        "should state there is nothing in season: {msg}"
    );
}

#[test]
fn heartbeat_says_a_near_miss_was_blocked_by_the_server_not_your_setting() {
    // The operator read "best price seen: +4.46%" beside "1 alert sent" and
    // reasonably assumed they were the same price. They were not. The outcome
    // has to travel with the number, and it has to name the REAL blocker: for
    // an exchange price under 4% the operator's threshold is irrelevant, so
    // blaming it would send them to tune a knob that cannot help.
    let msg = heartbeat_message(Heartbeat {
        active: &["tennis".to_string()].into_iter().collect(),
        best: Some(BestEv {
            ev_pct: 0.93,
            book: "novig".into(),
            sport: "tennis".into(),
            matchup: "Marino @ Compagnucci".into(),
            outcome: BestEvOutcome::Rejected("EV under 4%".into()),
        }),
        threshold_pct: 2.5,
        queued: 0,
        delivered: 0,
        quota: &healthy(),
        quiet_until: None,
    });
    assert!(msg.contains("0.93"), "{msg}");
    assert!(msg.contains("4%"), "must name the real blocker: {msg}");
}

#[test]
fn heartbeat_distinguishes_a_seeded_price_from_a_blocked_one() {
    // Seeding is by design, not a rejection. Conflating them would make a
    // normal restart look like a misconfiguration.
    let msg = heartbeat_message(Heartbeat {
        active: &["tennis".to_string()].into_iter().collect(),
        best: Some(BestEv {
            ev_pct: 4.46,
            book: "draftkings".into(),
            sport: "tennis".into(),
            matchup: "Marino @ Compagnucci".into(),
            outcome: BestEvOutcome::Seeded,
        }),
        threshold_pct: 2.5,
        queued: 0,
        delivered: 0,
        quota: &healthy(),
        quiet_until: None,
    });
    let low = msg.to_lowercase();
    assert!(
        low.contains("first poll") || low.contains("seeded") || low.contains("silently"),
        "must explain seeding rather than imply a rejection: {msg}"
    );
}

#[test]
fn heartbeat_confirms_when_the_best_price_was_the_one_published() {
    let msg = heartbeat_message(Heartbeat {
        active: &["soccer".to_string()].into_iter().collect(),
        best: Some(BestEv {
            ev_pct: 3.0,
            book: "fanduel".into(),
            sport: "soccer".into(),
            matchup: "Lille @ Monaco".into(),
            outcome: BestEvOutcome::Published,
        }),
        threshold_pct: 2.5,
        queued: 1,
        delivered: 1,
        quota: &healthy(),
        quiet_until: None,
    });
    let low = msg.to_lowercase();
    assert!(
        low.contains("alerted") || low.contains("published"),
        "{msg}"
    );
}

#[test]
fn heartbeat_flags_queued_alerts_that_never_reached_discord() {
    // `published` counts pushes onto the queue, not confirmed deliveries. If
    // Discord rejects them the old wording still claimed "1 alert sent", which
    // is the bot lying about having told you something.
    let msg = heartbeat_message(Heartbeat {
        active: &["soccer".to_string()].into_iter().collect(),
        best: None,
        threshold_pct: 2.5,
        queued: 3,
        delivered: 1,
        quota: &healthy(),
        quiet_until: None,
    });
    let low = msg.to_lowercase();
    assert!(
        low.contains("fail") || low.contains("not deliver") || low.contains("undelivered"),
        "a delivery shortfall must be reported: {msg}"
    );
}

#[test]
fn heartbeat_says_it_is_resting_not_that_nothing_is_in_season() {
    // During quiet hours the active set is empty because polling is paused,
    // not because there are no games. Saying "no sports in season" would be
    // false and would hide the fact that the operator configured this.
    let msg = heartbeat_message(Heartbeat {
        active: &BTreeSet::new(),
        best: None,
        threshold_pct: 2.5,
        queued: 0,
        delivered: 0,
        quota: &healthy(),
        quiet_until: Some("09:30 America/Denver".into()),
    });
    let low = msg.to_lowercase();
    assert!(low.contains("quiet hours"), "{msg}");
    assert!(low.contains("09:30"), "must say when it wakes: {msg}");
    assert!(
        !low.contains("no sports in season"),
        "must not claim an empty slate: {msg}"
    );
}

#[test]
fn heartbeat_outside_quiet_hours_is_unchanged() {
    let msg = heartbeat_message(Heartbeat {
        active: &["nba".to_string()].into_iter().collect(),
        best: None,
        threshold_pct: 2.5,
        queued: 0,
        delivered: 0,
        quota: &healthy(),
        quiet_until: None,
    });
    assert!(!msg.to_lowercase().contains("quiet hours"), "{msg}");
    assert!(msg.contains("nba"), "{msg}");
}
