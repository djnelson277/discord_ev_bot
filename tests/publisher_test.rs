use discord_ev_bot::discord::Alert;
use discord_ev_bot::discord::publisher::{AlertQueue, DiscordPublisher, build_embed};
use discord_ev_bot::owls::models::{EvEvent, EvResponse, Opportunity};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn real_event_and_opp() -> (EvEvent, Opportunity) {
    let parsed: EvResponse =
        serde_json::from_str(include_str!("fixtures/ev_soccer_populated.json")).unwrap();
    let ev = parsed.data[0].clone();
    let opp = ev.opportunities[0].clone();
    (ev, opp)
}

fn embed_json(ev: &EvEvent, opp: &Opportunity, enriched: bool) -> String {
    serde_json::to_string(&build_embed(ev, opp, enriched)).unwrap()
}

#[test]
fn embed_carries_the_fields_a_decision_needs() {
    let (ev, opp) = real_event_and_opp();
    let json = embed_json(&ev, &opp, true);
    for needed in [
        "Wuhan Three Towns", // home team
        "Shanghai Shenhua",  // away team
        "caesars",           // the book
        "1.15",              // EV percent
    ] {
        assert!(json.contains(needed), "embed missing {needed}: {json}");
    }
}

#[test]
fn american_odds_keep_their_sign() {
    let (ev, mut opp) = real_event_and_opp();
    opp.book_price = 180.0;
    assert!(embed_json(&ev, &opp, true).contains("+180"));

    opp.book_price = -110.0;
    assert!(embed_json(&ev, &opp, true).contains("-110"));
}

#[test]
fn half_point_odds_are_not_truncated() {
    let (ev, mut opp) = real_event_and_opp();
    opp.book_price = 117.5;
    assert!(embed_json(&ev, &opp, true).contains("+117.5"));
}

#[test]
fn single_source_fair_is_flagged() {
    // booksInConsensus == 1 means Pinnacle alone priced the fair. Staking full
    // Kelly on a single-source number is exactly what this warning prevents.
    let (ev, mut opp) = real_event_and_opp();
    opp.books_in_consensus = Some(1.0);
    let json = embed_json(&ev, &opp, true);
    assert!(
        json.to_lowercase().contains("pinnacle only"),
        "expected a single-source warning: {json}"
    );
}

#[test]
fn multi_source_fair_is_not_flagged() {
    let (ev, mut opp) = real_event_and_opp();
    opp.books_in_consensus = Some(4.0);
    let json = embed_json(&ev, &opp, true).to_lowercase();
    assert!(!json.contains("pinnacle only"));
}

#[test]
fn unenriched_alerts_are_marked_not_hidden() {
    // A cold cache must still publish. Holding the alert would make a silent
    // bot right after deploy, which is the anti-goal.
    let (ev, opp) = real_event_and_opp();
    let json = embed_json(&ev, &opp, false).to_lowercase();
    assert!(json.contains("unenriched"), "got: {json}");
}

#[test]
fn staleness_is_visible_on_the_face_of_the_embed() {
    let (ev, mut opp) = real_event_and_opp();
    opp.quote_age_ms = Some(71347.0);
    let json = embed_json(&ev, &opp, true);
    // 71347ms -> 71s; the operator must be able to see the price is a minute old
    assert!(json.contains("71"), "quote age not surfaced: {json}");
}

#[test]
fn link_becomes_a_url_when_present() {
    let (ev, mut opp) = real_event_and_opp();
    opp.link = Some("https://example.test/bet/123".into());
    assert!(embed_json(&ev, &opp, true).contains("https://example.test/bet/123"));
}

#[test]
fn absent_link_does_not_panic_or_emit_null_url() {
    let (ev, mut opp) = real_event_and_opp();
    opp.link = None;
    let json = embed_json(&ev, &opp, true);
    assert!(
        !json.contains("\"url\":null"),
        "a null url is invalid to Discord: {json}"
    );
}

#[test]
fn kelly_is_shown_when_known_and_omitted_when_not() {
    let (ev, mut opp) = real_event_and_opp();
    opp.kelly_fraction = Some(0.0064);
    assert!(embed_json(&ev, &opp, true).to_lowercase().contains("kelly"));

    opp.kelly_fraction = None;
    let json = embed_json(&ev, &opp, false);
    assert!(!json.contains("\"value\":null"), "got: {json}");
}

// ----------------------------------------------------------------- the queue

fn alert_with_ev(ev_pct: f64) -> Alert {
    let (event, mut opportunity) = real_event_and_opp();
    opportunity.ev_pct = ev_pct;
    Alert {
        event,
        opportunity,
        enriched: true,
    }
}

#[test]
fn queue_holds_alerts_in_order() {
    let q = AlertQueue::new(4);
    q.push(alert_with_ev(1.0));
    q.push(alert_with_ev(2.0));
    assert_eq!(q.len(), 2);
    assert_eq!(q.pop().unwrap().opportunity.ev_pct, 1.0);
    assert_eq!(q.pop().unwrap().opportunity.ev_pct, 2.0);
    assert!(q.pop().is_none());
}

#[test]
fn full_queue_evicts_the_oldest() {
    // A stale +EV price is worthless, so newest wins. Dropping the newest would
    // mean a backlog of dead prices crowds out the live one.
    let q = AlertQueue::new(2);
    q.push(alert_with_ev(1.0));
    q.push(alert_with_ev(2.0));
    let evicted = q.push(alert_with_ev(3.0));

    assert_eq!(evicted, 1, "one alert should have been dropped");
    assert_eq!(q.len(), 2);
    assert_eq!(q.pop().unwrap().opportunity.ev_pct, 2.0, "1.0 was evicted");
    assert_eq!(q.pop().unwrap().opportunity.ev_pct, 3.0);
}

#[test]
fn queue_reports_total_dropped() {
    let q = AlertQueue::new(1);
    q.push(alert_with_ev(1.0));
    q.push(alert_with_ev(2.0));
    q.push(alert_with_ev(3.0));
    assert_eq!(
        q.dropped(),
        2,
        "drops must be countable for the ops channel"
    );
}

// ------------------------------------------------------------- the publisher

#[tokio::test]
async fn posts_to_the_channel_with_bot_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v10/channels/999/messages"))
        .and(header("authorization", "Bot tok"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"1"}"#))
        .expect(1)
        .mount(&server)
        .await;

    let (ev, opp) = real_event_and_opp();
    let p = DiscordPublisher::new(&server.uri(), "tok", 999).unwrap();
    p.post_embed(&build_embed(&ev, &opp, true)).await.unwrap();
}

#[tokio::test]
async fn honours_discord_rate_limit_retry_after() {
    // Discord answers 429 with {"retry_after": <seconds>}. Ignoring it and
    // retrying hot gets the bot temporarily banned from the route.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string(r#"{"retry_after":0.05,"global":false}"#),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"1"}"#))
        .mount(&server)
        .await;

    let (ev, opp) = real_event_and_opp();
    let p = DiscordPublisher::new(&server.uri(), "tok", 999).unwrap();
    // must succeed by waiting and retrying rather than erroring out
    p.post_embed(&build_embed(&ev, &opp, true)).await.unwrap();
}

#[tokio::test]
async fn a_permanent_failure_errors_rather_than_looping_forever() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let (ev, opp) = real_event_and_opp();
    let p = DiscordPublisher::new(&server.uri(), "tok", 999).unwrap();
    assert!(p.post_embed(&build_embed(&ev, &opp, true)).await.is_err());
}
