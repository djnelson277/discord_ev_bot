use discord_ev_bot::owls::client::OwlsClient;
use discord_ev_bot::owls::error::OwlsError;
use std::time::Duration;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EV_BODY: &str = include_str!("fixtures/ev_soccer_populated.json");

#[tokio::test]
async fn sends_bearer_authorization_header() {
    // The API takes `Authorization: Bearer`, explicitly NOT `x-api-key`.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/nba/ev"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EV_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let c = OwlsClient::new(&server.uri(), "test-key").unwrap();
    c.get_ev("nba", 3.0).await.unwrap();
}

#[tokio::test]
async fn always_sends_venues_true() {
    // Without venues=true, Kalshi and Polymarket never appear -- and Polymarket
    // is the one book webhooks cannot serve, so losing it loses the book.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("venues", "true"))
        .and(query_param("min_ev", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EV_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let c = OwlsClient::new(&server.uri(), "k").unwrap();
    c.get_ev("nba", 3.0).await.unwrap();
}

#[tokio::test]
async fn min_ev_is_sent_without_a_trailing_decimal() {
    // 3.5_f64.floor() is 3.0; the query must read min_ev=3, not min_ev=3.0
    let server = MockServer::start().await;
    Mock::given(query_param("min_ev", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EV_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let c = OwlsClient::new(&server.uri(), "k").unwrap();
    c.get_ev("nba", 3.5_f64.floor()).await.unwrap();
}

#[tokio::test]
async fn captures_rate_limit_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-RateLimit-Remaining-Minute", "399")
                .insert_header("X-RateLimit-Remaining-Month", "285000")
                .set_body_string(EV_BODY),
        )
        .mount(&server)
        .await;

    let c = OwlsClient::new(&server.uri(), "k").unwrap();
    c.get_ev("nba", 3.0).await.unwrap();

    let q = c.quota();
    assert_eq!(q.remaining_minute, Some(399));
    assert_eq!(q.remaining_month, Some(285_000));
    assert!(!q.unlimited_month);
}

#[tokio::test]
async fn unlimited_month_header_is_not_a_parse_error() {
    // Hall of Fame sends the literal string "unlimited" where a number goes.
    // Treating that as a parse failure would make the governor think the
    // monthly budget was unknown and degrade for no reason.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-RateLimit-Remaining-Month", "unlimited")
                .set_body_string(EV_BODY),
        )
        .mount(&server)
        .await;

    let c = OwlsClient::new(&server.uri(), "k").unwrap();
    c.get_ev("nba", 3.0).await.unwrap();

    let q = c.quota();
    assert!(q.unlimited_month);
    assert_eq!(q.remaining_month, None);
}

#[tokio::test]
async fn maps_429_to_rate_limited_honouring_retry_after() {
    // Review Focus #3. Requests past the 15-concurrent cap are refused
    // immediately rather than queued, so a hot retry loop makes it worse.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
        .mount(&server)
        .await;

    let err = OwlsClient::new(&server.uri(), "k")
        .unwrap()
        .get_ev("nba", 3.0)
        .await
        .unwrap_err();

    match err {
        OwlsError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Duration::from_secs(2))
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn a_429_without_retry_after_still_backs_off() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let err = OwlsClient::new(&server.uri(), "k")
        .unwrap()
        .get_ev("nba", 3.0)
        .await
        .unwrap_err();

    match err {
        OwlsError::RateLimited { retry_after } => {
            assert!(retry_after >= Duration::from_secs(1), "must not busy-loop")
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_403_to_forbidden_with_required_tier() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            r#"{"error":"tier","requiredTier":"mvp","currentTier":"bench","upgradeUrl":"https://x"}"#,
        ))
        .mount(&server)
        .await;

    let err = OwlsClient::new(&server.uri(), "k")
        .unwrap()
        .get_ev("nba", 3.0)
        .await
        .unwrap_err();

    match err {
        OwlsError::Forbidden { required_tier, .. } => {
            assert_eq!(required_tier.as_deref(), Some("mvp"))
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_401_to_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = OwlsClient::new(&server.uri(), "k")
        .unwrap()
        .get_ev("nba", 3.0)
        .await
        .unwrap_err();

    assert!(matches!(err, OwlsError::Unauthorized));
}

#[tokio::test]
async fn schedule_sends_days_parameter() {
    // days=2, not 1: days=1 covers only the current calendar day in tz, so a
    // late-evening refresh would see nothing of tomorrow's slate.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schedule"))
        .and(query_param("days", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/schedule.json")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let c = OwlsClient::new(&server.uri(), "k").unwrap();
    let s = c.get_schedule(2).await.unwrap();
    assert_eq!(s.games.len(), 320);
}

#[tokio::test]
async fn events_hits_the_per_sport_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/ncaab/events"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/events_ncaab.json")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let c = OwlsClient::new(&server.uri(), "k").unwrap();
    let e = c.get_events("ncaab").await.unwrap();
    assert!(e.data.is_empty());
}

#[tokio::test]
async fn malformed_body_is_a_decode_error_not_a_panic() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
        .mount(&server)
        .await;

    let err = OwlsClient::new(&server.uri(), "k")
        .unwrap()
        .get_ev("nba", 3.0)
        .await
        .unwrap_err();

    assert!(matches!(err, OwlsError::Decode(_)), "got {err:?}");
}
