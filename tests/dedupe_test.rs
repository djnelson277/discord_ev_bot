use discord_ev_bot::dedupe::{AlertKey, Decision, Suppressor};
use discord_ev_bot::owls::models::Side;
use std::collections::HashSet;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(7200); // 2 hours
const RISE_PP: f64 = 1.0;

fn sup() -> Suppressor {
    Suppressor::new(WINDOW, RISE_PP)
}

fn t0() -> Instant {
    Instant::now()
}

fn key() -> AlertKey {
    AlertKey {
        sport: "nba".into(),
        event_id: "nba:Lakers@Celtics-20261017".into(),
        book: "fanduel".into(),
        side: Side::Home,
    }
}

fn other_key() -> AlertKey {
    AlertKey {
        sport: "nba".into(),
        event_id: "nba:Lakers@Celtics-20261017".into(),
        book: "draftkings".into(), // same game, different book
        side: Side::Home,
    }
}

#[test]
fn first_observation_publishes() {
    let mut s = sup();
    assert_eq!(s.observe(&key(), 4.0, t0()), Decision::Publish);
}

#[test]
fn repeat_within_the_window_is_suppressed() {
    // REST /ev has no dedup of its own: it returns the same qualifying
    // opportunity on EVERY poll for as long as the price lives. At 60s that is
    // 60 identical posts an hour without this.
    let mut s = sup();
    let start = t0();
    assert_eq!(s.observe(&key(), 4.0, start), Decision::Publish);
    for minute in 1..=10 {
        assert_eq!(
            s.observe(&key(), 4.0, start + Duration::from_secs(60 * minute)),
            Decision::Suppress,
            "minute {minute} should still be suppressed"
        );
    }
}

#[test]
fn a_small_rise_does_not_republish() {
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    assert_eq!(
        s.observe(&key(), 4.9, start + Duration::from_secs(60)),
        Decision::Suppress
    );
}

#[test]
fn a_full_point_rise_republishes() {
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    assert_eq!(
        s.observe(&key(), 5.0, start + Duration::from_secs(60)),
        Decision::Publish
    );
}

#[test]
fn an_exact_one_point_rise_survives_float_representation() {
    // Review Focus #4: 4.1 + 1.0 != 5.1 exactly in f64. A bare `>=` on the raw
    // sum silently swallows the alert the operator most wants.
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.1, start);
    assert_eq!(
        s.observe(&key(), 5.1, start + Duration::from_secs(60)),
        Decision::Publish,
        "4.1 -> 5.1 is a full point and must republish"
    );

    // And a few more values where the arithmetic is not exact
    for (first, second) in [(0.1, 1.1), (2.3, 3.3), (7.7, 8.7)] {
        let mut s = sup();
        let start = t0();
        s.observe(&key(), first, start);
        assert_eq!(
            s.observe(&key(), second, start + Duration::from_secs(60)),
            Decision::Publish,
            "{first} -> {second} is a full point"
        );
    }
}

#[test]
fn a_rise_just_under_a_point_stays_suppressed() {
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    assert_eq!(
        s.observe(&key(), 4.99, start + Duration::from_secs(60)),
        Decision::Suppress
    );
}

#[test]
fn republishing_resets_the_window_and_the_baseline() {
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start); // publish, baseline 4.0
    s.observe(&key(), 5.0, start + Duration::from_secs(60)); // publish, baseline 5.0
    // 5.5 is +1.5 over the ORIGINAL baseline but only +0.5 over the current one
    assert_eq!(
        s.observe(&key(), 5.5, start + Duration::from_secs(120)),
        Decision::Suppress
    );
}

#[test]
fn window_expiry_republishes_the_same_price() {
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    assert_eq!(
        s.observe(&key(), 4.0, start + WINDOW + Duration::from_secs(1)),
        Decision::Publish
    );
}

#[test]
fn rearms_after_two_consecutive_absences() {
    // A price that went away and came back is a genuinely new crossing -- but
    // only once the 15-minute floor has passed. This test previously used 300s
    // and encoded the bug that let a flickering price alert every few minutes.
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    s.mark_absent_in_sport("nba", &HashSet::new());
    s.mark_absent_in_sport("nba", &HashSet::new());
    assert_eq!(
        s.observe(&key(), 4.0, start + Duration::from_secs(300)),
        Decision::Suppress,
        "five minutes is inside the floor"
    );
    assert_eq!(
        s.observe(&key(), 4.0, start + Duration::from_secs(16 * 60)),
        Decision::Publish
    );
}

#[test]
fn a_single_absence_does_not_rearm() {
    // One missing poll is noise -- a flickering price must not re-alert.
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    s.mark_absent_in_sport("nba", &HashSet::new());
    assert_eq!(
        s.observe(&key(), 4.0, start + Duration::from_secs(120)),
        Decision::Suppress
    );
}

#[test]
fn being_seen_resets_the_absence_counter() {
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);

    let mut seen = HashSet::new();
    seen.insert(key());

    s.mark_absent_in_sport("nba", &HashSet::new()); // absent once
    s.mark_absent_in_sport("nba", &seen); // seen again -> counter resets
    s.mark_absent_in_sport("nba", &HashSet::new()); // absent once more, not twice in a row
    assert_eq!(
        s.observe(&key(), 4.0, start + Duration::from_secs(300)),
        Decision::Suppress
    );
}

#[test]
fn seeded_keys_publish_nothing_but_still_honour_a_rise() {
    // Cold start: record the live board silently so a restart does not dump
    // every qualifying price across ten sports into the channel at once.
    let mut s = sup();
    let start = t0();
    s.seed(&key(), 4.0, start);
    assert_eq!(
        s.observe(&key(), 4.0, start + Duration::from_secs(60)),
        Decision::Suppress
    );
    assert_eq!(
        s.observe(&key(), 5.0, start + Duration::from_secs(120)),
        Decision::Publish,
        "a real move after seeding must still alert"
    );
}

#[test]
fn keys_are_independent_across_books() {
    // The same game at two books is two opportunities, not one.
    let mut s = sup();
    let start = t0();
    assert_eq!(s.observe(&key(), 4.0, start), Decision::Publish);
    assert_eq!(s.observe(&other_key(), 4.0, start), Decision::Publish);
}

#[test]
fn keys_are_independent_across_sides() {
    let mut s = sup();
    let start = t0();
    let mut away = key();
    away.side = Side::Away;
    assert_eq!(s.observe(&key(), 4.0, start), Decision::Publish);
    assert_eq!(s.observe(&away, 4.0, start), Decision::Publish);
}

#[test]
fn tracked_key_count_is_observable() {
    // The poller logs this; an unbounded map would be a leak.
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    s.observe(&other_key(), 4.0, start);
    assert_eq!(s.tracked(), 2);

    // Absence now RE-ARMS rather than deleting. Retaining the entry is the
    // point: last_published_at is what enforces the 15-minute floor, and
    // deleting it was how a flickering price bypassed the floor entirely.
    s.mark_absent_in_sport("nba", &HashSet::new());
    s.mark_absent_in_sport("nba", &HashSet::new());
    assert_eq!(s.tracked(), 2, "re-armed keys are retained, not dropped");

    // evict_expired is what bounds the map instead.
    s.evict_expired(start + WINDOW + Duration::from_secs(60));
    assert_eq!(s.tracked(), 0);
}

#[test]
fn mark_absent_must_not_age_keys_belonging_to_other_sports() {
    // REGRESSION (review finding #1). poll_once calls mark_absent once per
    // sport, passing only that sport's observed keys. Because the key map is
    // global, every OTHER sport's keys get aged once per sport polled -- so at
    // three or more active sports every key reaches ABSENCES_TO_REARM within a
    // single cycle and is evicted before its own next observation. An evicted
    // key is unknown at observe() and publishes, collapsing suppression
    // entirely at the default 10-sport config.
    let mut s = sup();
    let start = t0();

    let a = AlertKey {
        sport: "sportA".into(),
        event_id: "sportA:g".into(),
        book: "fanduel".into(),
        side: Side::Home,
    };
    let b = AlertKey {
        sport: "sportB".into(),
        event_id: "sportB:g".into(),
        book: "fanduel".into(),
        side: Side::Home,
    };
    let c = AlertKey {
        sport: "sportC".into(),
        event_id: "sportC:g".into(),
        book: "fanduel".into(),
        side: Side::Home,
    };

    // Cycle 1: seed all three, as the first poll of each sport would.
    for k in [&a, &b, &c] {
        s.seed(k, 5.0, start);
    }
    assert_eq!(s.tracked(), 3);

    // Cycle 2: three sports, each observed then followed by its own
    // mark_absent -- exactly what poll_once does today.
    let t = start + Duration::from_secs(60);
    let mut published = 0;
    for k in [&a, &b, &c] {
        if s.observe(k, 5.0, t) == Decision::Publish {
            published += 1;
        }
        s.mark_absent_in_sport(&k.sport, &HashSet::from([k.clone()]));
    }

    assert_eq!(
        published, 0,
        "an unchanged price must stay suppressed; aging is leaking across sports"
    );
    assert_eq!(
        s.tracked(),
        3,
        "no key may be evicted while its own sport was observed this cycle"
    );
}

#[test]
fn rearming_does_not_bypass_the_minimum_realert_interval() {
    // REGRESSION. Observed in production: the same DraftKings price alerted at
    // 03:16, 03:38 and 03:53 with an identical 9.26% EV, and another at 06:02,
    // 06:11 and 06:30 at 2.77%.
    //
    // Cause: mark_absent_in_sport DELETED the key after two absences, so the
    // next observation saw an unknown key and published with no time check at
    // all. A price that flickers out of the board for two polls -- routine for
    // an obscure tennis market -- could therefore re-alert every couple of
    // minutes, bypassing the 2-hour window entirely.
    //
    // The server's own rule has a floor this implementation was missing:
    // "at most once per 15 minutes for one event, venue and side".
    let mut s = sup();
    let start = t0();

    assert_eq!(s.observe(&key(), 9.26, start), Decision::Publish);

    // price drops off the board for two polls, then returns unchanged
    s.mark_absent_in_sport("nba", &HashSet::new());
    s.mark_absent_in_sport("nba", &HashSet::new());

    assert_eq!(
        s.observe(&key(), 9.26, start + Duration::from_secs(120)),
        Decision::Suppress,
        "a flickering price must not re-alert two minutes later"
    );
    assert_eq!(
        s.observe(&key(), 9.26, start + Duration::from_secs(14 * 60)),
        Decision::Suppress,
        "still inside the 15-minute floor"
    );
}

#[test]
fn a_rearmed_price_may_alert_again_once_the_floor_has_passed() {
    // The re-arm is still meaningful: a price that genuinely went away and came
    // back is a new crossing. It just cannot happen every two minutes.
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 9.26, start);
    s.mark_absent_in_sport("nba", &HashSet::new());
    s.mark_absent_in_sport("nba", &HashSet::new());

    assert_eq!(
        s.observe(&key(), 9.26, start + Duration::from_secs(16 * 60)),
        Decision::Publish,
        "past the floor, a returning price is a genuine new crossing"
    );
}

#[test]
fn a_material_rise_still_beats_the_floor() {
    // A price improving by a full point is the one thing worth interrupting
    // for, exactly as the server treats it.
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    assert_eq!(
        s.observe(&key(), 5.5, start + Duration::from_secs(60)),
        Decision::Publish
    );
}

#[test]
fn absent_keys_are_still_eventually_evicted() {
    // Retaining state instead of deleting it must not turn the map into a leak.
    let mut s = sup();
    let start = t0();
    s.observe(&key(), 4.0, start);
    assert_eq!(s.tracked(), 1);

    // absent, and long past the suppression window
    for _ in 0..4 {
        s.mark_absent_in_sport("nba", &HashSet::new());
    }
    s.evict_expired(start + WINDOW + Duration::from_secs(60));
    assert_eq!(s.tracked(), 0, "stale absent keys must not accumulate");
}
