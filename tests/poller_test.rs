use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use discord_ev_bot::config::Config;
use discord_ev_bot::dedupe::Suppressor;
use discord_ev_bot::discord::publisher::AlertQueue;
use discord_ev_bot::owls::client::OwlsClient;
use discord_ev_bot::owls::quota::{
    MONTHLY_QUOTA_MVP, Pacing, QuotaSnapshot, VENUE_MAX_INTERVAL, next_interval,
};
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

fn pacing(active_sports: usize, day_of_month: u32) -> Pacing {
    Pacing {
        base: BASE,
        day_of_month,
        active_sports,
        has_venue_sports: false,
    }
}

#[test]
fn interval_is_unchanged_when_the_budget_covers_the_plan() {
    // 5 sports at 60s is 7,200/day; half a month left with 160k is ~10.6k/day.
    let q = QuotaSnapshot {
        remaining_month: Some(160_000),
        ..healthy()
    };
    assert_eq!(next_interval(&q, pacing(5, 15)), BASE);
}

#[test]
fn interval_lengthens_before_the_budget_runs_out_not_after() {
    // REGRESSION (review finding #3). The old governor was reactive: it only
    // engaged once spending had already fallen behind pace, by which point the
    // factor was ~1.0, and simulation showed 8-10 active sports exhausting the
    // month around day 29. 10 sports at 60s is 14,400/day; 50k over the 15
    // days left is 3,333/day, so it must slow down roughly 4x.
    let q = QuotaSnapshot {
        remaining_month: Some(50_000),
        ..healthy()
    };
    let out = next_interval(&q, pacing(10, 15));
    assert!(out > BASE * 3, "expected real degradation, got {out:?}");
}

#[test]
fn the_last_day_of_the_month_still_degrades() {
    // REGRESSION (review finding #3). On day 30, elapsed_fraction was 1.0 so
    // `expected` was 0.0 and the function returned the UN-degraded base
    // interval regardless of how little budget was left -- the governor
    // switched itself off on exactly the day it was most needed.
    let q = QuotaSnapshot {
        remaining_month: Some(500),
        ..healthy()
    };
    let out = next_interval(&q, pacing(10, 30));
    assert!(out > BASE, "day 30 must still degrade, got {out:?}");
    let out31 = next_interval(&q, pacing(10, 31));
    assert!(out31 > BASE, "day 31 must still degrade, got {out31:?}");
}

#[test]
fn degradation_is_capped_so_the_poller_never_effectively_stops() {
    let q = QuotaSnapshot {
        remaining_month: Some(1),
        ..healthy()
    };
    let out = next_interval(&q, pacing(10, 28));
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
    assert_eq!(next_interval(&q, pacing(10, 28)), BASE);
}

#[test]
fn unknown_budget_does_not_degrade() {
    // Before the first response there are no headers; degrading on no
    // information would slow the bot for nothing.
    assert_eq!(
        next_interval(&QuotaSnapshot::default(), pacing(10, 15)),
        BASE
    );
}

#[test]
fn no_active_sports_means_no_spend_to_pace() {
    let q = QuotaSnapshot {
        remaining_month: Some(10),
        ..healthy()
    };
    assert_eq!(next_interval(&q, pacing(0, 28)), BASE);
}

#[test]
fn venue_sports_cap_the_interval_however_depleted_the_budget() {
    // REGRESSION (review finding #6). venue_sports was written and never read,
    // and next_interval had no floor, so quota pressure stretched every sport
    // to 8x. venues=true quotes are warm-on-demand and go cold when you stop
    // asking, so that silently loses Kalshi and Polymarket -- and Polymarket is
    // the one book the poller is the sole source for.
    let q = QuotaSnapshot {
        remaining_month: Some(1),
        ..healthy()
    };
    let mut p = pacing(10, 28);

    let without = next_interval(&q, p);
    p.has_venue_sports = true;
    let with = next_interval(&q, p);

    assert!(with < without, "venue sports must cap degradation");
    assert!(with <= VENUE_MAX_INTERVAL);
}

#[test]
fn the_venue_cap_never_polls_faster_than_configured() {
    // The cap limits degradation; it must not override a deliberately slow
    // base interval into something faster.
    let q = QuotaSnapshot {
        remaining_month: Some(300_000),
        ..healthy()
    };
    let slow = Duration::from_secs(600);
    let out = next_interval(
        &q,
        Pacing {
            base: slow,
            day_of_month: 1,
            active_sports: 1,
            has_venue_sports: true,
        },
    );
    assert_eq!(out, slow);
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

/// Builds a board for `sport` with one publishable opportunity.
fn board_for(sport: &str, ev_pct: f64) -> String {
    format!(
        r#"{{"success":true,"data":[{{
            "sport":"{sport}","homeTeam":"H","awayTeam":"A",
            "commenceTime":"2026-10-17T23:00:00.000Z",
            "canonicalEventId":"{sport}:A@H-20261017",
            "opportunities":[{{
                "book":"fanduel","side":"home","team":"H","bookPrice":-105,
                "evPct":{ev_pct},"fairProbability":0.52,"kind":"book",
                "tradeable":true,"booksInConsensus":4,"kellyFraction":0.02
            }}]
        }}],"meta":{{
            "sport":"{sport}","timestamp":"2026-10-17T18:00:00.000Z",
            "freshness":{{"ageSeconds":4,"stale":false}}
        }}}}"#
    )
}

#[tokio::test]
async fn suppression_holds_across_many_sports_over_many_cycles() {
    // REGRESSION (review finding #1). With three or more active sports the
    // unscoped per-sport mark_absent aged every OTHER sport's keys once per
    // sport polled, evicting them inside a single cycle so every board
    // republished every 60s -- suppression collapsing completely at the default
    // 10-sport configuration. Two sports is exactly the boundary that hides it,
    // which is why every earlier test missed this.
    let server = MockServer::start().await;
    let sports = ["nba", "nhl", "soccer", "tennis", "mlb"];
    for s in sports {
        mount_ev(&server, s, board_for(s, 6.0)).await;
    }

    let mut c = ctx(&server, &sports);
    let mut conf = cfg(&server.uri());
    conf.sports = sports.iter().map(|s| s.to_string()).collect();
    let t = Instant::now();

    // Cycle 1 seeds every sport silently.
    let first = poll_once(&mut c, &conf, t).await;
    assert_eq!(first.polled, 5);
    assert_eq!(first.published, 0);
    assert_eq!(first.seeded, 5);

    // Every later cycle must suppress all five, forever, while prices are
    // unchanged and inside the window.
    for cycle in 2..=6 {
        let out = poll_once(&mut c, &conf, t + Duration::from_secs(60 * cycle)).await;
        assert_eq!(
            out.published, 0,
            "cycle {cycle} republished {} unchanged opportunities",
            out.published
        );
        assert_eq!(out.suppressed, 5, "cycle {cycle} should suppress all five");
        assert_eq!(c.queue.len(), 0);
    }

    assert_eq!(
        c.suppressor.tracked(),
        5,
        "all five keys must survive; eviction is leaking across sports"
    );
}

#[tokio::test]
async fn a_sport_leaving_the_active_set_drops_its_state_and_reseeds() {
    // Scoped ageing means an inactive sport's keys are never aged, so they must
    // be dropped when it leaves gating -- otherwise the map leaks across a
    // season. And on return the sport must re-seed rather than republish its
    // whole board.
    let server = MockServer::start().await;
    for s in ["nba", "nhl"] {
        mount_ev(&server, s, board_for(s, 6.0)).await;
    }

    let mut c = ctx(&server, &["nba", "nhl"]);
    let mut conf = cfg(&server.uri());
    conf.sports = vec!["nba".into(), "nhl".into()];
    let t = Instant::now();

    poll_once(&mut c, &conf, t).await; // seeds both
    assert_eq!(c.suppressor.tracked(), 2);

    // nhl goes out of season.
    c.active.remove("nhl");
    poll_once(&mut c, &conf, t + Duration::from_secs(60)).await;
    assert_eq!(c.suppressor.tracked(), 1, "nhl's key must be dropped");
    assert!(
        !c.seeded.contains("nhl"),
        "nhl must no longer count as seeded"
    );

    // nhl returns: it re-seeds silently instead of dumping its board.
    c.active.insert("nhl".to_string());
    let out = poll_once(&mut c, &conf, t + Duration::from_secs(120)).await;
    assert_eq!(
        out.published, 0,
        "a returning sport must re-seed, not republish"
    );
    assert_eq!(out.seeded, 1);
}

#[tokio::test]
async fn a_rate_limit_is_waited_out_rather_than_ignored() {
    // Review Focus #3: Retry-After was parsed and then discarded by every
    // caller, so a 429 on sport 1 was followed within milliseconds by requests
    // for every other sport against the same account.
    let server = MockServer::start().await;
    Mock::given(path("/api/v1/nba/ev"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .mount(&server)
        .await;
    mount_ev(&server, "nhl", board_for("nhl", 6.0)).await;

    let mut c = ctx(&server, &["nba", "nhl"]);
    let conf = cfg(&server.uri());

    let started = std::time::Instant::now();
    let out = poll_once(&mut c, &conf, Instant::now()).await;
    let elapsed = started.elapsed();

    assert_eq!(out.rate_limited, 1);
    assert_eq!(out.errors, 1);
    assert_eq!(out.polled, 1, "nhl must still be polled after the wait");
    assert!(
        elapsed >= Duration::from_millis(900),
        "the pass must actually wait, took {elapsed:?}"
    );
}

#[tokio::test]
async fn tracks_the_best_ev_seen_even_when_nothing_qualifies() {
    // Today's live check: the best EV across twelve books on an NFL Sunday was
    // +2.91%, under the operator's 3.5% threshold -- so the bot correctly said
    // nothing. Without recording the near-miss there is no way to tell "I am
    // watching and the market is tight" from "I am broken".
    let server = MockServer::start().await;
    let board = r#"{"success":true,"data":[{
        "sport":"nfl","homeTeam":"Los Angeles Rams","awayTeam":"Arizona Cardinals",
        "commenceTime":"2026-10-18T20:05:00.000Z",
        "canonicalEventId":"nfl:Arizona Cardinals@Los Angeles Rams-20261018",
        "opportunities":[
          {"book":"fanduel","side":"home","team":"Los Angeles Rams","bookPrice":-800,
           "evPct":1.2,"fairProbability":0.85,"kind":"book","tradeable":false},
          {"book":"draftkings","side":"away","team":"Arizona Cardinals","bookPrice":500,
           "evPct":2.91,"fairProbability":0.13,"kind":"book","tradeable":false}
        ]}],
        "meta":{"sport":"nfl","timestamp":"2026-10-18T19:20:00.000Z",
                "freshness":{"ageSeconds":1,"stale":false}}}"#;
    mount_ev(&server, "nfl", board.to_string()).await;

    let mut c = ctx(&server, &["nfl"]);
    let mut conf = cfg(&server.uri());
    conf.sports = vec!["nfl".into()];
    c.seeded.insert("nfl".to_string()); // past the seeding poll

    let out = poll_once(&mut c, &conf, Instant::now()).await;

    assert_eq!(out.published, 0, "nothing clears 3.5%");
    assert_eq!(out.rejected, 2);

    let best = out.best_ev.expect("the near-miss must be recorded");
    assert_eq!(best.ev_pct, 2.91);
    assert_eq!(best.book, "draftkings");
    assert_eq!(best.sport, "nfl");
    assert!(best.matchup.contains("Rams"), "got {}", best.matchup);
}

#[tokio::test]
async fn best_ev_spans_every_sport_in_the_cycle() {
    let server = MockServer::start().await;
    for (sport, ev) in [("nba", 2.0), ("nhl", 3.1), ("soccer", 1.4)] {
        let board = format!(
            r#"{{"success":true,"data":[{{
                "sport":"{sport}","homeTeam":"H","awayTeam":"A",
                "commenceTime":"2026-10-18T23:00:00.000Z",
                "canonicalEventId":"{sport}:A@H-20261018",
                "opportunities":[{{"book":"fanduel","side":"home","team":"H",
                   "bookPrice":-110,"evPct":{ev},"fairProbability":0.5,
                   "kind":"book","tradeable":false}}]}}],
                "meta":{{"sport":"{sport}","timestamp":"2026-10-18T19:20:00.000Z",
                    "freshness":{{"ageSeconds":1,"stale":false}}}}}}"#
        );
        mount_ev(&server, sport, board).await;
    }

    let mut c = ctx(&server, &["nba", "nhl", "soccer"]);
    let mut conf = cfg(&server.uri());
    conf.sports = vec!["nba".into(), "nhl".into(), "soccer".into()];
    for s in ["nba", "nhl", "soccer"] {
        c.seeded.insert(s.to_string());
    }

    let out = poll_once(&mut c, &conf, Instant::now()).await;
    let best = out.best_ev.expect("should record a best");
    assert_eq!(best.ev_pct, 3.1);
    assert_eq!(best.sport, "nhl");
}

#[tokio::test]
async fn an_empty_board_records_no_best_ev() {
    let server = MockServer::start().await;
    mount_ev(&server, "nba", EMPTY_FRESH_BOARD.to_string()).await;
    let mut c = ctx(&server, &["nba"]);
    c.seeded.insert("nba".to_string());
    let out = poll_once(&mut c, &cfg(&server.uri()), Instant::now()).await;
    assert!(out.best_ev.is_none());
}
