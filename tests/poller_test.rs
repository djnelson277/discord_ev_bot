use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use discord_ev_bot::config::Config;
use discord_ev_bot::dedupe::Suppressor;
use discord_ev_bot::discord::publisher::AlertQueue;
use discord_ev_bot::owls::client::OwlsClient;
use discord_ev_bot::owls::quota::{MONTHLY_QUOTA_MVP, QuotaSnapshot, next_interval};
use discord_ev_bot::poller::{PollContext, poll_once};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BASE: Duration = Duration::from_secs(60);

fn healthy() -> QuotaSnapshot {
    QuotaSnapshot {
        remaining_minute: Some(399),
        remaining_month: Some(MONTHLY_QUOTA_MVP),
        unlimited_month: false,
        reset_minute_unix_ms: None,
    }
}

// ------------------------------------------------------------ quota governor

#[test]
fn interval_is_unchanged_when_spending_is_on_pace() {
    // Half the month gone, a bit over half the budget left.
    let q = QuotaSnapshot {
        remaining_month: Some(160_000),
        ..healthy()
    };
    assert_eq!(next_interval(BASE, &q, 15), BASE);
}

#[test]
fn interval_lengthens_when_burning_too_fast() {
    // Day 10 of the month with only 5k left: at this rate the month dies early.
    let q = QuotaSnapshot {
        remaining_month: Some(5_000),
        ..healthy()
    };
    assert!(
        next_interval(BASE, &q, 10) > BASE,
        "a depleted budget must slow the poller down"
    );
}

#[test]
fn degradation_is_capped_so_the_poller_never_effectively_stops() {
    let q = QuotaSnapshot {
        remaining_month: Some(1),
        ..healthy()
    };
    let out = next_interval(BASE, &q, 28);
    assert!(out > BASE);
    assert!(
        out <= BASE * 8,
        "degradation must stay bounded, got {out:?}"
    );
}

#[test]
fn unlimited_month_never_degrades() {
    let q = QuotaSnapshot {
        remaining_month: None,
        unlimited_month: true,
        ..healthy()
    };
    assert_eq!(next_interval(BASE, &q, 28), BASE);
}

#[test]
fn unknown_budget_does_not_degrade() {
    // Before the first response we have no headers. Degrading on no information
    // would slow the bot down for nothing.
    let q = QuotaSnapshot::default();
    assert_eq!(next_interval(BASE, &q, 15), BASE);
}

// -------------------------------------------------------------- the poll loop

fn cfg(base: &str) -> Config {
    let mut c = Config::from_toml_str(
        r#"
        min_ev_pct = 3.5
        sports = ["nba","soccer"]
        books = ["fanduel","draftkings","betmgm","bet365","novig","kalshi","polymarket"]
        "#,
    )
    .unwrap();
    c.owls_base_url = base.to_string();
    c
}

/// An EV board carrying one publishable opportunity on a configured book.
fn board_with_ev(ev_pct: f64) -> String {
    format!(
        r#"{{"success":true,"data":[{{
            "sport":"nba","homeTeam":"Celtics","awayTeam":"Lakers",
            "commenceTime":"2026-10-17T23:00:00.000Z",
            "eventId":"opaque-1","canonicalEventId":"nba:Lakers@Celtics-20261017",
            "opportunities":[{{
                "book":"fanduel","side":"home","team":"Celtics","bookPrice":-105,
                "evPct":{ev_pct},"fairProbability":0.52,"kind":"book",
                "tradeable":true,"booksInConsensus":4,"kellyFraction":0.02
            }}]
        }}],"meta":{{
            "sport":"nba","market":"h2h","max_ev":25,"pregame_only":true,
            "timestamp":"2026-10-17T18:00:00.000Z",
            "freshness":{{"ageSeconds":4,"stale":false}}
        }}}}"#
    )
}

const EMPTY_FRESH_BOARD: &str = r#"{"success":true,"data":[],"meta":{
    "sport":"nba","timestamp":"2026-10-17T18:00:00.000Z",
    "freshness":{"ageSeconds":4,"stale":false}}}"#;

const STALE_BOARD: &str = r#"{"success":true,"data":[],"meta":{
    "sport":"nba","timestamp":"1970-01-01T00:00:00.000Z",
    "freshness":{"ageSeconds":null,"stale":true}}}"#;

async fn mount_ev(server: &MockServer, sport: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/{sport}/ev")))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

fn ctx(server: &MockServer, sports: &[&str]) -> PollContext {
    PollContext {
        client: OwlsClient::new(&server.uri(), "k").unwrap(),
        suppressor: Suppressor::new(Duration::from_secs(7200), 1.0),
        queue: Arc::new(AlertQueue::new(64)),
        active: sports
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
        seeded: HashSet::new(),
        venue_sports: HashSet::new(),
    }
}

#[tokio::test]
async fn first_poll_of_a_sport_seeds_without_publishing() {
    // Cold start must not dump the whole live board into the channel.
    let server = MockServer::start().await;
    mount_ev(&server, "nba", board_with_ev(6.0)).await;
    let mut c = ctx(&server, &["nba"]);

    let out = poll_once(&mut c, &cfg(&server.uri()), Instant::now()).await;

    assert_eq!(out.polled, 1);
    assert_eq!(out.published, 0, "nothing may publish on the seeding poll");
    assert_eq!(out.seeded, 1);
    assert!(c.seeded.contains("nba"));
    assert_eq!(c.queue.len(), 0);
}

#[tokio::test]
async fn second_poll_suppresses_an_unchanged_price() {
    let server = MockServer::start().await;
    mount_ev(&server, "nba", board_with_ev(6.0)).await;
    let mut c = ctx(&server, &["nba"]);
    let conf = cfg(&server.uri());
    let t = Instant::now();

    poll_once(&mut c, &conf, t).await;
    let out = poll_once(&mut c, &conf, t + Duration::from_secs(60)).await;

    assert_eq!(out.published, 0);
    assert_eq!(out.suppressed, 1);
    assert_eq!(c.queue.len(), 0);
}

#[tokio::test]
async fn a_material_rise_after_seeding_publishes() {
    let server = MockServer::start().await;
    // seed at 6.0
    Mock::given(path("/api/v1/nba/ev"))
        .respond_with(ResponseTemplate::new(200).set_body_string(board_with_ev(6.0)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // then the price improves by more than a point
    Mock::given(path("/api/v1/nba/ev"))
        .respond_with(ResponseTemplate::new(200).set_body_string(board_with_ev(7.5)))
        .mount(&server)
        .await;

    let mut c = ctx(&server, &["nba"]);
    let conf = cfg(&server.uri());
    let t = Instant::now();

    poll_once(&mut c, &conf, t).await;
    let out = poll_once(&mut c, &conf, t + Duration::from_secs(60)).await;

    assert_eq!(out.published, 1);
    assert_eq!(c.queue.len(), 1);
    let alert = c.queue.pop().unwrap();
    assert_eq!(alert.opportunity.ev_pct, 7.5);
    assert!(alert.enriched, "board carried consensus and kelly");
}

#[tokio::test]
async fn a_stale_board_yields_nothing_and_is_not_seeded() {
    // Acting on a stale board is worse than acting on nothing. It must also not
    // count as seeded, or the real board would be silently suppressed later.
    let server = MockServer::start().await;
    mount_ev(&server, "nba", STALE_BOARD.to_string()).await;
    let mut c = ctx(&server, &["nba"]);

    let out = poll_once(&mut c, &cfg(&server.uri()), Instant::now()).await;

    assert_eq!(out.published, 0);
    assert_eq!(out.stale_boards, 1);
    assert!(
        !c.seeded.contains("nba"),
        "a stale board must not mark the sport seeded"
    );
}

#[tokio::test]
async fn an_empty_but_fresh_board_counts_as_seeded() {
    // Distinct from stale: the feed is healthy, there is simply nothing
    // qualifying. The sport is now seeded, so a later crossing alerts.
    let server = MockServer::start().await;
    mount_ev(&server, "nba", EMPTY_FRESH_BOARD.to_string()).await;
    let mut c = ctx(&server, &["nba"]);

    let out = poll_once(&mut c, &cfg(&server.uri()), Instant::now()).await;

    assert_eq!(out.stale_boards, 0);
    assert!(c.seeded.contains("nba"));
}

#[tokio::test]
async fn one_sports_rate_limit_does_not_starve_the_others() {
    let server = MockServer::start().await;
    Mock::given(path("/api/v1/nba/ev"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .mount(&server)
        .await;
    mount_ev(&server, "soccer", EMPTY_FRESH_BOARD.to_string()).await;

    let mut c = ctx(&server, &["nba", "soccer"]);
    let out = poll_once(&mut c, &cfg(&server.uri()), Instant::now()).await;

    assert_eq!(out.errors, 1);
    assert_eq!(out.polled, 1, "soccer must still have been polled");
    assert!(c.seeded.contains("soccer"));
    assert!(!c.seeded.contains("nba"));
}

#[tokio::test]
async fn venue_warmup_keeps_the_sport_on_the_venue_list() {
    // Review Focus #5: venues=true prices are warm-on-demand -- the first
    // request returns no Kalshi/Polymarket quotes for ~15s. That must not be
    // recorded as "no venue edge here" and cost the sport its interval floor.
    let server = MockServer::start().await;
    let venue_board = r#"{"success":true,"data":[{
        "sport":"nba","homeTeam":"H","awayTeam":"A",
        "commenceTime":"2026-10-17T23:00:00.000Z",
        "canonicalEventId":"nba:A@H-20261017",
        "opportunities":[{"book":"kalshi","side":"home","team":"H","bookPrice":120,
            "evPct":5.5,"fairProbability":0.5,"kind":"venue","tradeable":true}]
    }],"meta":{"sport":"nba","timestamp":"2026-10-17T18:00:00.000Z",
        "freshness":{"ageSeconds":3,"stale":false}}}"#;

    Mock::given(path("/api/v1/nba/ev"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EMPTY_FRESH_BOARD))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/nba/ev"))
        .respond_with(ResponseTemplate::new(200).set_body_string(venue_board))
        .mount(&server)
        .await;

    let mut c = ctx(&server, &["nba"]);
    let conf = cfg(&server.uri());
    let t = Instant::now();

    poll_once(&mut c, &conf, t).await; // warm-up: no venue quotes yet
    poll_once(&mut c, &conf, t + Duration::from_secs(60)).await;

    assert!(
        c.venue_sports.contains("nba"),
        "a sport that has ever shown a venue price keeps its interval floor"
    );
}

#[tokio::test]
async fn opportunities_on_unconfigured_books_are_filtered_out() {
    let server = MockServer::start().await;
    let caesars = r#"{"success":true,"data":[{
        "sport":"nba","homeTeam":"H","awayTeam":"A",
        "commenceTime":"2026-10-17T23:00:00.000Z",
        "canonicalEventId":"nba:A@H-20261017",
        "opportunities":[{"book":"caesars","side":"home","team":"H","bookPrice":150,
            "evPct":9.0,"fairProbability":0.5,"kind":"book","tradeable":true,
            "booksInConsensus":5,"kellyFraction":0.04}]
    }],"meta":{"sport":"nba","timestamp":"2026-10-17T18:00:00.000Z",
        "freshness":{"ageSeconds":3,"stale":false}}}"#;
    mount_ev(&server, "nba", caesars.to_string()).await;

    let mut c = ctx(&server, &["nba"]);
    c.seeded.insert("nba".to_string()); // skip the seeding poll
    let out = poll_once(&mut c, &cfg(&server.uri()), Instant::now()).await;

    assert_eq!(out.published, 0);
    assert_eq!(out.rejected, 1);
    assert_eq!(c.queue.len(), 0);
}

#[tokio::test]
async fn a_price_below_the_threshold_is_rejected_not_suppressed() {
    // Rejected and suppressed are different states: a rejected price never
    // entered the dedupe map, so if it later clears the bar it must still alert.
    let server = MockServer::start().await;
    mount_ev(&server, "nba", board_with_ev(2.0)).await;
    let mut c = ctx(&server, &["nba"]);
    c.seeded.insert("nba".to_string());

    let out = poll_once(&mut c, &cfg(&server.uri()), Instant::now()).await;

    assert_eq!(out.rejected, 1);
    assert_eq!(out.suppressed, 0);
    assert_eq!(c.suppressor.tracked(), 0, "rejects must not be tracked");
}
