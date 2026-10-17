use chrono::{DateTime, Utc};
use discord_ev_bot::config::Config;
use discord_ev_bot::gating::{EVENTS_ONLY_SPORTS, GLOBAL_SCHEDULE_SPORTS, active_sports};
use discord_ev_bot::owls::client::OwlsClient;
use std::collections::BTreeSet;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fixed clock so the window arithmetic is deterministic. The captured schedule
/// fixture spans 2026-10-16T04:00Z to 2026-10-18T03:10Z.
fn now() -> DateTime<Utc> {
    "2026-10-16T00:00:00Z".parse().unwrap()
}

fn cfg_with(base: &str, lookahead_hours: i64) -> Config {
    let mut c = Config::from_toml_str(&format!(
        r#"
        owls_base_url = "{base}"
        lookahead_hours = {lookahead_hours}
        sports = ["nfl","nba","mlb","nhl","ncaaf","ncaab","wnba","soccer","tennis","mma"]
        books = ["betmgm","fanduel","draftkings","bet365","novig","kalshi","polymarket"]
        "#
    ))
    .unwrap();
    c.owls_base_url = base.to_string();
    c
}

/// Mounts all three gating endpoints with the captured fixtures.
///
/// Deliberately sets no call-count expectations: how many calls get made
/// depends on the config under test, and
/// `makes_exactly_three_calls_for_the_full_config` asserts that separately.
async fn server_with_fixtures() -> MockServer {
    let server = MockServer::start().await;
    mount_fixtures(&server, None).await;
    server
}

async fn mount_fixtures(server: &MockServer, expect: Option<u64>) {
    let apply = |m: Mock| match expect {
        Some(n) => m.expect(n),
        None => m,
    };
    apply(
        Mock::given(method("GET"))
            .and(path("/api/v1/schedule"))
            .and(query_param("days", "2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(include_str!("fixtures/schedule.json")),
            ),
    )
    .mount(server)
    .await;
    apply(
        Mock::given(method("GET"))
            .and(path("/api/v1/ncaab/events"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("fixtures/events_ncaab.json")),
            ),
    )
    .mount(server)
    .await;
    apply(
        Mock::given(method("GET"))
            .and(path("/api/v1/mma/events"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("fixtures/events_mma.json")),
            ),
    )
    .mount(server)
    .await;
}

#[tokio::test]
async fn makes_exactly_three_calls_for_the_full_config() {
    // One /schedule covering eight sports, plus one /events each for the two it
    // refuses. ~3 calls/hour is negligible against a 300k monthly budget.
    let server = MockServer::start().await;
    mount_fixtures(&server, Some(1)).await;
    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    active_sports(&client, &cfg_with(&server.uri(), 48), now())
        .await
        .unwrap();
    // MockServer verifies the expectations when it drops.
}

#[test]
fn global_schedule_excludes_ncaab_and_mma() {
    // Proven against the live API: /api/v1/schedule?sport=ncaab and ?sport=mma
    // both return HTTP 400 "Unknown sport". This is structural, not seasonal --
    // it will still 400 in February with a full NCAAB slate.
    assert!(!GLOBAL_SCHEDULE_SPORTS.contains(&"ncaab"));
    assert!(!GLOBAL_SCHEDULE_SPORTS.contains(&"mma"));
    assert!(EVENTS_ONLY_SPORTS.contains(&"ncaab"));
    assert!(EVENTS_ONLY_SPORTS.contains(&"mma"));
}

#[tokio::test]
async fn builds_the_active_set_from_all_three_calls() {
    let server = server_with_fixtures().await;
    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    let cfg = cfg_with(&server.uri(), 48);

    let active = active_sports(&client, &cfg, now()).await.unwrap();

    let expected: BTreeSet<String> = [
        "mlb", "nba", "ncaaf", "nhl", "soccer", "tennis", "wnba", "mma",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(active, expected);
}

#[tokio::test]
async fn mma_comes_from_events_not_the_schedule() {
    // The captured mma events fixture holds 104 events across 11 promotions, of
    // which only 27 are UFC. Gating via /schedule?sport=ufc would miss 77 of
    // them silently, so mma MUST resolve through /events.
    let server = server_with_fixtures().await;
    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    let active = active_sports(&client, &cfg_with(&server.uri(), 48), now())
        .await
        .unwrap();
    assert!(active.contains("mma"));
}

#[tokio::test]
async fn out_of_season_sport_is_inactive() {
    // ncaab's events response is empty at capture time (season starts November)
    let server = server_with_fixtures().await;
    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    let active = active_sports(&client, &cfg_with(&server.uri(), 48), now())
        .await
        .unwrap();
    assert!(!active.contains("ncaab"));
}

#[tokio::test]
async fn sport_with_no_games_in_window_is_inactive() {
    // No NFL game falls in this window (captured on a Friday; NFL plays
    // Sun/Mon/Thu), so polling it would spend quota for a guaranteed empty board
    let server = server_with_fixtures().await;
    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    let active = active_sports(&client, &cfg_with(&server.uri(), 48), now())
        .await
        .unwrap();
    assert!(!active.contains("nfl"));
}

#[tokio::test]
async fn a_narrow_lookahead_shrinks_the_active_set() {
    let server = server_with_fixtures().await;
    let client = OwlsClient::new(&server.uri(), "k").unwrap();

    let wide = active_sports(&client, &cfg_with(&server.uri(), 48), now())
        .await
        .unwrap();

    let server2 = server_with_fixtures().await;
    let client2 = OwlsClient::new(&server2.uri(), "k").unwrap();
    let narrow = active_sports(&client2, &cfg_with(&server2.uri(), 3), now())
        .await
        .unwrap();

    assert!(
        narrow.len() < wide.len(),
        "narrow={narrow:?} should be smaller than wide={wide:?}"
    );
    assert!(narrow.is_subset(&wide));
}

#[tokio::test]
async fn config_narrows_but_never_widens() {
    // The schedule fixture carries esports (cs2, dota2, lol, valorant) and ufc.
    // None are in the operator's list, so none may reach the active set.
    let server = server_with_fixtures().await;
    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    let mut cfg = cfg_with(&server.uri(), 48);
    cfg.sports = vec!["nba".to_string()];

    let active = active_sports(&client, &cfg, now()).await.unwrap();

    assert_eq!(active.len(), 1);
    assert!(active.contains("nba"));
    for leaked in ["cs2", "dota2", "lol", "valorant", "ufc", "soccer"] {
        assert!(!active.contains(leaked), "{leaked} leaked past the config");
    }

    // And it must not have spent requests on sports the operator excluded.
    let events_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/events"))
        .count();
    assert_eq!(
        events_calls, 0,
        "unconfigured sports must not cost gating requests"
    );
}

#[tokio::test]
async fn started_games_do_not_activate_a_sport() {
    // EV is pregame-only, so an in-progress or final game is worth zero requests
    let server = MockServer::start().await;
    let body = r#"{"success":true,"days":2,"games":[
        {"sport":"nba","startTime":"2026-10-16T10:00:00Z","status":"in_progress"},
        {"sport":"nhl","startTime":"2026-10-16T11:00:00Z","status":"final"}
    ]}"#;
    Mock::given(path("/api/v1/schedule"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    for s in EVENTS_ONLY_SPORTS {
        Mock::given(path(format!("/api/v1/{s}/events")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"success":true,"data":[],"meta":{"event_count":0}}"#),
            )
            .mount(&server)
            .await;
    }

    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    let active = active_sports(&client, &cfg_with(&server.uri(), 48), now())
        .await
        .unwrap();
    assert!(active.is_empty(), "got {active:?}");
}

#[tokio::test]
async fn an_events_failure_does_not_lose_the_schedule_sports() {
    // One endpoint erroring must not blank the whole active set -- that would
    // silently stop the poller for every sport.
    let server = MockServer::start().await;
    Mock::given(path("/api/v1/schedule"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/schedule.json")),
        )
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/ncaab/events"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(path("/api/v1/mma/events"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = OwlsClient::new(&server.uri(), "k").unwrap();
    let active = active_sports(&client, &cfg_with(&server.uri(), 48), now())
        .await
        .unwrap();

    assert!(active.contains("nba"), "schedule sports must survive");
    assert!(!active.contains("mma"), "failed events call yields no mma");
}
