use discord_ev_bot::config::Config;
use discord_ev_bot::filter::{RejectReason, Verdict, effective_trade_bar, is_publishable};
use discord_ev_bot::owls::models::{EvResponse, Kind, Opportunity};

fn cfg() -> Config {
    Config::from_toml_str(
        r#"
        min_ev_pct = 3.5
        books = ["betmgm","fanduel","draftkings","bet365","novig","kalshi","polymarket"]
        "#,
    )
    .unwrap()
}

/// Built from JSON so production code carries no test-only constructor.
fn opp(json: &str) -> Opportunity {
    serde_json::from_str(json).unwrap()
}

fn book_opp(book: &str, ev: f64) -> Opportunity {
    opp(&format!(
        r#"{{"book":"{book}","side":"home","team":"T","bookPrice":150,
             "evPct":{ev},"fairProbability":0.45,"kind":"book","tradeable":true}}"#
    ))
}

#[test]
fn threshold_is_strictly_greater_than() {
    // The operator asked for "over 3.5%", so exactly 3.5 does not qualify.
    assert!(matches!(
        is_publishable(&book_opp("fanduel", 3.5), &cfg()),
        Verdict::Reject(RejectReason::BelowThreshold { .. })
    ));
    assert!(matches!(
        is_publishable(&book_opp("fanduel", 3.51), &cfg()),
        Verdict::Publish
    ));
}

#[test]
fn rejects_book_outside_the_operators_allowlist() {
    // caesars is a real book the API serves, but not one of the operator's 7.
    // An alert for a book you cannot bet trains people to ignore the channel.
    assert!(matches!(
        is_publishable(&book_opp("caesars", 9.0), &cfg()),
        Verdict::Reject(RejectReason::BookNotAllowed { .. })
    ));
}

#[test]
fn the_real_captured_opportunity_is_rejected_by_book_first() {
    // The live soccer board returned caesars at 1.15% EV, tradeable:false.
    // It fails three ways; the allowlist check is the most specific and must win.
    let parsed: EvResponse =
        serde_json::from_str(include_str!("fixtures/ev_soccer_populated.json")).unwrap();
    let real = &parsed.data[0].opportunities[0];
    assert!(matches!(
        is_publishable(real, &cfg()),
        Verdict::Reject(RejectReason::BookNotAllowed { .. })
    ));
}

#[test]
fn honours_an_explicit_tradeable_false() {
    let mut o = book_opp("fanduel", 5.0);
    o.tradeable = Some(false);
    assert!(matches!(
        is_publishable(&o, &cfg()),
        Verdict::Reject(RejectReason::NotTradeable { .. })
    ));
}

#[test]
fn trade_bar_differs_by_venue_type() {
    assert_eq!(effective_trade_bar(Some(Kind::Book)), 2.0);
    assert_eq!(effective_trade_bar(Some(Kind::Venue)), 4.0);
    // An unknown kind takes the stricter bar rather than the looser one.
    assert_eq!(effective_trade_bar(None), 4.0);
}

#[test]
fn absent_tradeable_falls_back_to_the_trade_bar() {
    // Review Focus #2: tradeable is optional in the schema. Treating absence as
    // "not tradeable" would silently drop every alert; deriving it keeps the
    // server's own rule.
    let mut venue = opp(
        r#"{"book":"kalshi","side":"home","team":"T","bookPrice":150,
             "evPct":3.8,"fairProbability":0.45,"kind":"venue"}"#,
    );
    venue.tradeable = None;
    // 3.8 clears the operator's 3.5 but not the 4% exchange bar
    assert!(matches!(
        is_publishable(&venue, &cfg()),
        Verdict::Reject(RejectReason::NotTradeable { .. })
    ));

    let mut book = book_opp("fanduel", 3.8);
    book.tradeable = None;
    // same EV, but a sportsbook's bar is 2%
    assert!(matches!(is_publishable(&book, &cfg()), Verdict::Publish));
}

#[test]
fn exchanges_effectively_floor_at_four_percent() {
    // Consequence of the trade bar the operator should expect: Novig, Kalshi and
    // Polymarket cannot alert at 3.5 even though that is the configured setting.
    for venue_book in ["novig", "kalshi", "polymarket"] {
        let o = opp(&format!(
            r#"{{"book":"{venue_book}","side":"home","team":"T","bookPrice":150,
                 "evPct":3.9,"fairProbability":0.45,"kind":"venue"}}"#
        ));
        assert!(
            matches!(is_publishable(&o, &cfg()), Verdict::Reject(_)),
            "{venue_book} at 3.9% should not clear the 4% exchange bar"
        );
    }
}

#[test]
fn rejects_ev_above_the_server_ceiling() {
    // The API drops anything over 25% as a data error. If one reaches us the
    // board is lying, and staking on it would be the worst possible outcome.
    assert!(matches!(
        is_publishable(&book_opp("fanduel", 30.0), &cfg()),
        Verdict::Reject(RejectReason::ImplausibleEv { .. })
    ));
}

#[test]
fn implausible_ev_outranks_every_other_reason() {
    // A 40% EV price on a disallowed book is first and foremost suspect data.
    assert!(matches!(
        is_publishable(&book_opp("caesars", 40.0), &cfg()),
        Verdict::Reject(RejectReason::ImplausibleEv { .. })
    ));
}

#[test]
fn consensus_minimum_rejects_a_known_lower_count() {
    let mut c = cfg();
    c.min_books_in_consensus = Some(2);
    let mut o = book_opp("fanduel", 6.0);
    o.books_in_consensus = Some(1.0); // Pinnacle alone priced the fair
    assert!(matches!(
        is_publishable(&o, &c),
        Verdict::Reject(RejectReason::BelowConsensusMinimum { .. })
    ));
}

#[test]
fn consensus_minimum_fails_open_when_the_count_is_unknown() {
    // Fail open and mark, never suppress invisibly: an operator filter that
    // silently eats alerts is worse than one that lets a marked alert through.
    let mut c = cfg();
    c.min_books_in_consensus = Some(2);
    let mut o = book_opp("fanduel", 6.0);
    o.books_in_consensus = None;
    assert!(matches!(is_publishable(&o, &c), Verdict::Publish));
}

#[test]
fn no_consensus_minimum_configured_accepts_single_source() {
    let mut o = book_opp("fanduel", 6.0);
    o.books_in_consensus = Some(1.0);
    assert!(matches!(is_publishable(&o, &cfg()), Verdict::Publish));
}

#[test]
fn threshold_is_configurable_upward() {
    let mut c = cfg();
    c.min_ev_pct = 8.0;
    assert!(matches!(
        is_publishable(&book_opp("fanduel", 6.0), &c),
        Verdict::Reject(RejectReason::BelowThreshold { .. })
    ));
    assert!(matches!(
        is_publishable(&book_opp("fanduel", 8.5), &c),
        Verdict::Publish
    ));
}

#[test]
fn reject_reasons_are_human_readable() {
    // These strings reach the ops channel; a bare enum name is not an explanation.
    let v = is_publishable(&book_opp("caesars", 9.0), &cfg());
    let Verdict::Reject(reason) = v else {
        panic!("expected a rejection")
    };
    let text = reason.to_string();
    assert!(
        text.contains("caesars") || text.contains("book"),
        "got: {text}"
    );
}
