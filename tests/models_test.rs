use discord_ev_bot::owls::models::{
    EvMeta, EvResponse, EventsResponse, Kind, Opportunity, ScheduleResponse, Side,
};

#[test]
fn parses_populated_soccer_fixture() {
    let parsed: EvResponse =
        serde_json::from_str(include_str!("fixtures/ev_soccer_populated.json")).unwrap();
    assert!(parsed.success);
    assert_eq!(parsed.data.len(), 1);

    let event = &parsed.data[0];
    assert_eq!(event.sport, "soccer");
    assert_eq!(event.home_team, "Wuhan Three Towns");
    assert_eq!(event.away_team, "Shanghai Shenhua");
    assert_eq!(
        event.canonical_event_id.as_deref(),
        Some("soccer:Shanghai Shenhua@Wuhan Three Towns-20261018")
    );

    let opp = &event.opportunities[0];
    assert_eq!(opp.book, "caesars");
    assert_eq!(opp.side, Side::Home);
    assert_eq!(opp.book_price, 180.0);
    assert_eq!(opp.ev_pct, 1.15);
    assert_eq!(opp.books_in_consensus, Some(1.0));
    assert_eq!(opp.kind, Some(Kind::Book));
    assert_eq!(opp.tradeable, Some(false));
    assert_eq!(opp.reason.as_deref(), Some("EV under 2%"));
    assert_eq!(opp.quote_age_ms, Some(71347.0));
}

#[test]
fn explicit_json_nulls_become_none() {
    // route, size and link are all null in the captured response
    let parsed: EvResponse =
        serde_json::from_str(include_str!("fixtures/ev_soccer_populated.json")).unwrap();
    let opp = &parsed.data[0].opportunities[0];
    assert_eq!(opp.route, None);
    assert_eq!(opp.size, None);
    assert_eq!(opp.link, None);
}

#[test]
fn parses_empty_but_fresh_board() {
    // ev_nba was captured with no slate: data is empty yet the board is FRESH.
    // "nothing qualifies right now" must not be confused with "no data at all".
    let parsed: EvResponse = serde_json::from_str(include_str!("fixtures/ev_nba.json")).unwrap();
    assert!(parsed.success);
    assert!(parsed.data.is_empty());
    assert!(!parsed.meta.freshness.stale);
    assert!(!parsed.meta.is_empty_sentinel());
}

#[test]
fn detects_empty_board_sentinel() {
    // Review Focus #1: with no data at all the API sends the Unix epoch and
    // stale:true. Treating that epoch as a live timestamp would report the
    // board as ~56 years old rather than absent.
    let raw = r#"{"sport":"nba","market":"h2h","odds_format":"american",
                  "min_ev":3,"max_ev":25,"pregame_only":true,
                  "timestamp":"1970-01-01T00:00:00.000Z",
                  "freshness":{"ageSeconds":null,"stale":true}}"#;
    let meta: EvMeta = serde_json::from_str(raw).unwrap();
    assert!(meta.is_empty_sentinel());
}

#[test]
fn meta_exposes_server_ev_ceiling_and_scope() {
    let parsed: EvResponse = serde_json::from_str(include_str!("fixtures/ev_nba.json")).unwrap();
    assert_eq!(parsed.meta.max_ev, Some(25.0));
    assert_eq!(parsed.meta.pregame_only, Some(true));
    assert_eq!(parsed.meta.market.as_deref(), Some("h2h"));
}

#[test]
fn opportunity_tolerates_absent_optional_fields() {
    // Review Focus #2: nothing is in the schema's `required` list, so a lean
    // object must still parse rather than dropping every alert.
    let raw = r#"{"book":"fanduel","side":"away","team":"Lakers",
                  "bookPrice":-110,"evPct":4.2,"fairProbability":0.55}"#;
    let opp: Opportunity = serde_json::from_str(raw).unwrap();
    assert_eq!(opp.side, Side::Away);
    assert_eq!(opp.tradeable, None);
    assert_eq!(opp.kelly_fraction, None);
    assert_eq!(opp.books_in_consensus, None);
    assert_eq!(opp.kind, None);
}

#[test]
fn book_price_accepts_fractional_odds() {
    // bookPrice is declared number/double; some books carry a half point
    let raw = r#"{"book":"novig","side":"home","team":"X",
                  "bookPrice":117.5,"evPct":5.0,"fairProbability":0.5}"#;
    let opp: Opportunity = serde_json::from_str(raw).unwrap();
    assert_eq!(opp.book_price, 117.5);
}

#[test]
fn side_and_kind_reject_unknown_variants() {
    let bad_side = r#"{"book":"x","side":"sideways","team":"T",
                       "bookPrice":100,"evPct":1.0,"fairProbability":0.5}"#;
    assert!(serde_json::from_str::<Opportunity>(bad_side).is_err());
}

#[test]
fn dedupe_key_prefers_canonical_id() {
    let parsed: EvResponse =
        serde_json::from_str(include_str!("fixtures/ev_tennis_populated.json")).unwrap();
    let event = &parsed.data[0];
    // eventId here is an opaque book id ("1638461209"); the canonical id is the
    // stable sport:away@home-date key and must win.
    assert_eq!(
        event.dedupe_key(),
        "tennis:Marvin Moeller@Gianluca Cadenasso-20261016"
    );
}

#[test]
fn dedupe_key_falls_back_when_canonical_id_is_null() {
    let raw = r#"{"sport":"nba","homeTeam":"H","awayTeam":"A",
                  "commenceTime":"2026-10-16T23:00:00.000Z",
                  "eventId":"opaque-123","canonicalEventId":null,
                  "opportunities":[]}"#;
    let event: discord_ev_bot::owls::models::EvEvent = serde_json::from_str(raw).unwrap();
    assert_eq!(event.dedupe_key(), "opaque-123");
}

#[test]
fn parses_schedule_fixture_with_mixed_statuses() {
    let s: ScheduleResponse = serde_json::from_str(include_str!("fixtures/schedule.json")).unwrap();
    assert!(s.success);
    assert_eq!(s.days, Some(2));
    assert_eq!(s.tz.as_deref(), Some("America/New_York"));
    assert_eq!(s.games.len(), 320);
    // the captured day carries scheduled, in_progress and final games
    assert!(s.games.iter().any(|g| g.status == "scheduled"));
    assert!(s.games.iter().any(|g| g.status == "in_progress"));
    assert!(s.games.iter().any(|g| g.status == "final"));
}

#[test]
fn schedule_sports_exclude_ncaab() {
    // the reason Task 4 needs an /events fallback
    let s: ScheduleResponse = serde_json::from_str(include_str!("fixtures/schedule.json")).unwrap();
    let sports = s.sports.unwrap_or_default();
    assert!(!sports.contains(&"ncaab".to_string()));
    assert!(sports.contains(&"ufc".to_string()));
}

#[test]
fn parses_empty_events_fixture() {
    // ncaab out of season at capture time: empty data, snake_case meta
    let e: EventsResponse =
        serde_json::from_str(include_str!("fixtures/events_ncaab.json")).unwrap();
    assert!(e.success);
    assert!(e.data.is_empty());
    assert_eq!(e.meta.event_count, Some(0));
}

#[test]
fn parses_populated_events_fixture() {
    let e: EventsResponse = serde_json::from_str(include_str!("fixtures/events_mma.json")).unwrap();
    assert!(e.success);
    assert!(!e.data.is_empty());
    let first = &e.data[0];
    assert!(!first.commence_time.to_string().is_empty());
}

#[test]
fn an_unknown_kind_does_not_destroy_the_whole_board() {
    // REGRESSION (review finding #8). Kind was a closed enum, so one
    // unrecognised value was a hard deserialization error for the ENTIRE
    // EvResponse: if Owls adds a third venue type, every poll of every affected
    // sport returns a decode error and the bot goes blind indefinitely while
    // reporting only a warn line. ScheduleGame.status is already a String for
    // exactly this reason, and an unknown kind falls back to the STRICTER 4%
    // bar, so tolerance cannot loosen anything.
    let raw = r#"{"success":true,"data":[{
        "sport":"nba","homeTeam":"H","awayTeam":"A",
        "commenceTime":"2026-10-17T23:00:00.000Z",
        "canonicalEventId":"nba:A@H-20261017",
        "opportunities":[
          {"book":"fanduel","side":"home","team":"H","bookPrice":-110,
           "evPct":5.0,"fairProbability":0.55,"kind":"book"},
          {"book":"novig","side":"away","team":"A","bookPrice":120,
           "evPct":6.0,"fairProbability":0.45,"kind":"exchange"}
        ]}],
        "meta":{"sport":"nba","timestamp":"2026-10-17T18:00:00.000Z",
                "freshness":{"ageSeconds":2,"stale":false}}}"#;

    let parsed: EvResponse = serde_json::from_str(raw).expect("board must still parse");
    let opps = &parsed.data[0].opportunities;
    assert_eq!(opps.len(), 2, "both opportunities must survive");
    assert_eq!(opps[0].kind, Some(Kind::Book));
    assert_eq!(
        opps[1].kind, None,
        "an unknown kind becomes None, which takes the stricter trade bar"
    );
}

#[test]
fn an_unknown_side_still_fails_loudly() {
    // Sides are genuinely closed for a moneyline market, and a wrong side would
    // name the wrong team on an alert -- worse than no alert.
    let raw = r#"{"book":"x","side":"sideways","team":"T",
                  "bookPrice":100,"evPct":1.0,"fairProbability":0.5}"#;
    assert!(serde_json::from_str::<Opportunity>(raw).is_err());
}
