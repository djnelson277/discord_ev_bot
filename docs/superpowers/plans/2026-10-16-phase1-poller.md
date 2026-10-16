# Phase 1 — Poller-Only +EV Discord Bot: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust service that polls the Owls Insight EV board and posts a Discord message when a +EV moneyline price clears the operator's criteria at one of their books.

**Architecture:** One async binary. An hourly gating pass builds the set of sports with upcoming games; a 60-second poll loop fetches each active sport's EV board, filters opportunities, passes survivors through a suppression state machine that mirrors the server's `ev.found` dedup rule, and hands publishable alerts to a Discord publisher over a bounded channel. No inbound HTTP — nothing in phase 1 needs a public address.

**Tech Stack:** Rust 1.92, tokio, reqwest (rustls), serde, figment (config), chrono + chrono-tz, serenity (Discord), governor (rate limiting), tracing (structured stdout logs), notify (config hot-reload), wiremock (test HTTP).

**Spec:** [docs/superpowers/specs/2026-10-16-discord-ev-bot-design.md](../specs/2026-10-16-discord-ev-bot-design.md)

## Global Constraints

- Rust **1.92.0**; edition 2021.
- Auth header is exactly `Authorization: Bearer <key>` — **not** `x-api-key`.
- Base URL `https://api.owlsinsight.com`, from config, never hardcoded in call sites.
- EV is **moneyline only, pregame only**. Do not model other markets.
- Sports (10): `nfl nba mlb nhl ncaaf ncaab wnba soccer tennis mma`.
- Books (7): `betmgm fanduel draftkings bet365 novig kalshi polymarket`.
- EV threshold: **strictly greater than 3.5%**, configurable.
- Trade bar: sportsbooks **≥2%**, exchanges and prediction markets **≥4%**.
- Quota: **300,000 req/month**, 400/min, 15 concurrent.
- Poll interval **60s**; gating refresh **hourly** with `days=2`.
- EV above **25%** is a server-side data error; treat any that arrives as suspect.
- A board with `meta.freshness.stale == true` yields no opportunities.
- Prices are **American odds** (`meta.odds_format`).
- Secrets (`OWLS_API_KEY`, `DISCORD_TOKEN`, channel ids) come from env only, never config files.
- All operator criteria live in hot-reloadable config; **no compiled-in constants**.
- Logs are structured, to **stdout**. No log files.
- Graceful shutdown on **SIGTERM**, draining the publisher queue.
- No absolute paths; no machine-specific assumptions.

## Review Focus

Input classes the spec implies that no task's happy-path tests would exercise. Each has its test assigned to the task that owns the code.

1. **Empty board sentinel** — with no data, `meta.timestamp` is `1970-01-01T00:00:00.000Z` and `freshness.stale` is true. Parsing must not treat the epoch as a live timestamp or panic. → Task 2.
2. **Optional opportunity fields absent** — `tradeable`, `kellyFraction`, `booksInConsensus`, `link`, `quoteAgeMs` are documented as optional. Absence must not panic or silently drop every alert. → Tasks 2, 5.
3. **429 and concurrency rejection** — requests past the 15-concurrent cap are refused immediately, not queued. `Retry-After` must be honoured rather than retried hot. → Task 3.
4. **Float boundary on the +1 point rule** — `ev >= last_published + 1.0` on `f64` must not misfire on representation error at exactly 1.0. → Task 6.
5. **`venues=true` warm-up** — the first request for a sport returns no Kalshi/Polymarket quotes for ~15s. This must not be cached as "no venue edge" or cause the sport to be dropped from polling. → Task 8.

---

## Prerequisites (operator, before Task 2)

Capture real API responses as test fixtures. Run these yourself — they also teach you the endpoints, and they ground every model in this plan against reality rather than my reading of the docs.

```bash
export OWLS_API_KEY='<your key>'
mkdir -p tests/fixtures

curl -sS -H "Authorization: Bearer $OWLS_API_KEY" \
  "https://api.owlsinsight.com/api/v1/nba/ev?venues=true&min_ev=3" \
  > tests/fixtures/ev_nba.json

curl -sS -H "Authorization: Bearer $OWLS_API_KEY" \
  "https://api.owlsinsight.com/api/v1/schedule?days=2" \
  > tests/fixtures/schedule.json

curl -sS -H "Authorization: Bearer $OWLS_API_KEY" \
  "https://api.owlsinsight.com/api/v1/ncaab/events" \
  > tests/fixtures/events_ncaab.json

curl -sS -H "Authorization: Bearer $OWLS_API_KEY" \
  "https://api.owlsinsight.com/api/v1/mma/events" \
  > tests/fixtures/events_mma.json
```

If `ev_nba.json` comes back with an empty `data` array (no NBA slate, or nothing qualifying), capture a second sport that does have games, and keep the empty one — it is the Review Focus #1 fixture.

**These fixtures contain no secrets and are safe to commit.** If any response unexpectedly echoes a key, redact before committing.

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` | Dependencies, 2021 edition, Rust 1.92 floor |
| `.gitignore` | `/target`, `.env` |
| `.env.example` | Documents required env vars, no values |
| `config/default.toml` | Default operator criteria |
| `src/main.rs` | Wiring, signal handling, task spawn |
| `src/config.rs` | `Config` type, file+env load, hot-reload |
| `src/owls/models.rs` | EV / schedule / events response types |
| `src/owls/client.rs` | `OwlsClient`: auth, requests, rate-limit capture |
| `src/owls/quota.rs` | `QuotaState`, degradation decisions |
| `src/gating.rs` | Active-sport set from schedule + events |
| `src/filter.rs` | Threshold, allowlist, trade-bar filtering |
| `src/dedupe.rs` | Suppression state machine |
| `src/poller.rs` | Poll loop orchestration |
| `src/discord/publisher.rs` | Embed construction, bounded send queue |
| `src/obs.rs` | Ops alerts, slate-aware silence detection |

---

### Task 1: Project scaffold and configuration

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `.env.example`, `config/default.toml`, `src/main.rs`, `src/config.rs`
- Test: `tests/config_test.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Config` with fields `poll_interval_secs: u64`, `gating_refresh_secs: u64`, `min_ev_pct: f64`, `sports: Vec<String>`, `books: Vec<String>`, `min_books_in_consensus: Option<u32>`, `owls_base_url: String`, `lookahead_hours: i64`; and `Config::load(path: &Path) -> Result<Config, ConfigError>` which layers `default.toml` then env vars prefixed `EVBOT_`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn loads_defaults_from_toml() {
    let cfg = Config::load(Path::new("config/default.toml")).unwrap();
    assert_eq!(cfg.min_ev_pct, 3.5);
    assert_eq!(cfg.poll_interval_secs, 60);
    assert_eq!(cfg.gating_refresh_secs, 3600);
    assert_eq!(cfg.sports.len(), 10);
    assert!(cfg.books.contains(&"polymarket".to_string()));
}

#[test]
fn env_overrides_file() {
    std::env::set_var("EVBOT_MIN_EV_PCT", "5.0");
    let cfg = Config::load(Path::new("config/default.toml")).unwrap();
    assert_eq!(cfg.min_ev_pct, 5.0);
    std::env::remove_var("EVBOT_MIN_EV_PCT");
}

#[test]
fn rejects_unknown_sport() {
    // a sport outside the 14 EV-supported keys must fail validation
    let err = Config::from_toml_str("sports = [\"cricket\"]\n").unwrap_err();
    assert!(err.to_string().contains("cricket"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test config_test`
Expected: FAIL — `Config` does not exist.

- [ ] **Step 3: Write `Cargo.toml`, `.gitignore`, `.env.example`, `config/default.toml`**

`.gitignore` must contain `/target` and `.env`.

`.env.example` lists, with no values: `OWLS_API_KEY`, `DISCORD_TOKEN`, `DISCORD_ALERT_CHANNEL_ID`, `DISCORD_OPS_CHANNEL_ID`.

`config/default.toml` carries the Global Constraints values: `min_ev_pct = 3.5`, `poll_interval_secs = 60`, `gating_refresh_secs = 3600`, `lookahead_hours = 48`, `owls_base_url = "https://api.owlsinsight.com"`, the 10 sports, the 7 books.

- [ ] **Step 4: Implement `src/config.rs`**

`Config::load` uses figment layering `Toml::file(path)` then `Env::prefixed("EVBOT_")`. Add `Config::from_toml_str(&str)` for tests. Validation rejects any sport outside the 14 EV-supported keys and any book outside the 7, naming the offender in the error.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test config_test`
Expected: PASS (3 tests)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml .gitignore .env.example config/ src/ tests/
git commit -m "feat: project scaffold and layered configuration"
```

---

### Task 2: Owls API response models

**Files:**
- Create: `src/owls/mod.rs`, `src/owls/models.rs`
- Test: `tests/models_test.rs`

**Interfaces:**
- Consumes: fixtures from Prerequisites.
- Produces: `EvResponse { success: bool, data: Vec<EvEvent>, meta: EvMeta }`; `EvEvent { event_id: String, canonical_event_id: String, sport: String, home_team: String, away_team: String, commence_time: DateTime<Utc>, opportunities: Vec<Opportunity> }`; `Opportunity { book: String, side: Side, team: String, book_price: i32, ev_pct: f64, fair_probability: f64, kelly_fraction: Option<f64>, books_in_consensus: Option<u32>, kind: Option<Kind>, tradeable: Option<bool>, reason: Option<String>, link: Option<String>, quote_age_ms: Option<u64> }`; `EvMeta { timestamp: DateTime<Utc>, freshness: Freshness, max_ev: Option<f64> }`; `Freshness { age_seconds: Option<i64>, stale: bool }`; `enum Side { Home, Away, Draw }`; `enum Kind { Book, Venue }`; `ScheduleResponse`, `EventsResponse` with `commence_time` and `status` per entry. `EvMeta::is_empty_sentinel(&self) -> bool`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn parses_real_ev_fixture() {
    let raw = include_str!("fixtures/ev_nba.json");
    let parsed: EvResponse = serde_json::from_str(raw).unwrap();
    assert!(parsed.success);
}

#[test]
fn opportunity_tolerates_absent_optional_fields() {
    // Review Focus #2: tradeable, kellyFraction, booksInConsensus, link, quoteAgeMs all absent
    let raw = r#"{"book":"fanduel","side":"home","team":"Lakers",
                  "bookPrice":-110,"evPct":4.2,"fairProbability":0.55}"#;
    let opp: Opportunity = serde_json::from_str(raw).unwrap();
    assert_eq!(opp.tradeable, None);
    assert_eq!(opp.kelly_fraction, None);
    assert_eq!(opp.books_in_consensus, None);
}

#[test]
fn detects_empty_board_sentinel() {
    // Review Focus #1: epoch timestamp + stale means "no data", not a live board
    let raw = r#"{"timestamp":"1970-01-01T00:00:00.000Z",
                  "freshness":{"ageSeconds":null,"stale":true}}"#;
    let meta: EvMeta = serde_json::from_str(raw).unwrap();
    assert!(meta.is_empty_sentinel());
}

#[test]
fn live_board_is_not_sentinel() {
    let raw = r#"{"timestamp":"2026-10-16T12:00:00.000Z",
                  "freshness":{"ageSeconds":12,"stale":false}}"#;
    let meta: EvMeta = serde_json::from_str(raw).unwrap();
    assert!(!meta.is_empty_sentinel());
}

#[test]
fn parses_schedule_and_events_fixtures() {
    let s: ScheduleResponse = serde_json::from_str(include_str!("fixtures/schedule.json")).unwrap();
    assert!(s.success);
    let e: EventsResponse = serde_json::from_str(include_str!("fixtures/events_ncaab.json")).unwrap();
    assert!(e.success);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test models_test`
Expected: FAIL — types do not exist.

- [ ] **Step 3: Implement `src/owls/models.rs`**

Use `#[serde(rename_all = "camelCase")]` to match the API's casing. Every field the spec marks optional is `Option<T>`; do not use `#[serde(default)]` to paper over a missing required field. `is_empty_sentinel` returns true when `timestamp` equals the Unix epoch **and** `freshness.stale` is true.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test models_test`
Expected: PASS (5 tests)

- [ ] **Step 5: Commit**

```bash
git add src/owls/ tests/
git commit -m "feat: Owls Insight response models with empty-board detection"
```

---

### Task 3: REST client with auth and rate-limit capture

**Files:**
- Create: `src/owls/client.rs`, `src/owls/quota.rs`, `src/error.rs`
- Test: `tests/client_test.rs`

**Interfaces:**
- Consumes: `Config` (Task 1), models (Task 2).
- Produces: `OwlsClient::new(base_url: &str, api_key: &str) -> OwlsClient`; `async fn get_ev(&self, sport: &str, min_ev: f64) -> Result<EvResponse, OwlsError>`; `async fn get_schedule(&self, days: u8) -> Result<ScheduleResponse, OwlsError>`; `async fn get_events(&self, sport: &str) -> Result<EventsResponse, OwlsError>`; `fn quota(&self) -> QuotaSnapshot`. `enum OwlsError { RateLimited { retry_after: Duration }, Forbidden { required_tier: Option<String> }, Transport(..), Decode(..), Status(u16) }`. `QuotaSnapshot { remaining_minute: Option<u64>, remaining_month: Option<u64>, unlimited_month: bool }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn sends_bearer_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/nba/ev"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            include_str!("fixtures/ev_nba.json")))
        .expect(1)
        .mount(&server).await;
    let c = OwlsClient::new(&server.uri(), "test-key");
    c.get_ev("nba", 3.0).await.unwrap();
}

#[tokio::test]
async fn sends_venues_true_and_min_ev() {
    // venues=true is required or Kalshi and Polymarket never appear
    let server = MockServer::start().await;
    Mock::given(query_param("venues", "true"))
        .and(query_param("min_ev", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            include_str!("fixtures/ev_nba.json")))
        .expect(1)
        .mount(&server).await;
    OwlsClient::new(&server.uri(), "k").get_ev("nba", 3.0).await.unwrap();
}

#[tokio::test]
async fn captures_rate_limit_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200)
            .insert_header("X-RateLimit-Remaining-Minute", "399")
            .insert_header("X-RateLimit-Remaining-Month", "285000")
            .set_body_string(include_str!("fixtures/ev_nba.json")))
        .mount(&server).await;
    let c = OwlsClient::new(&server.uri(), "k");
    c.get_ev("nba", 3.0).await.unwrap();
    let q = c.quota();
    assert_eq!(q.remaining_month, Some(285_000));
    assert_eq!(q.remaining_minute, Some(399));
}

#[tokio::test]
async fn unlimited_month_header_is_not_a_parse_error() {
    // Hall of Fame sends the literal string "unlimited"
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200)
            .insert_header("X-RateLimit-Remaining-Month", "unlimited")
            .set_body_string(include_str!("fixtures/ev_nba.json")))
        .mount(&server).await;
    let c = OwlsClient::new(&server.uri(), "k");
    c.get_ev("nba", 3.0).await.unwrap();
    assert!(c.quota().unlimited_month);
}

#[tokio::test]
async fn maps_429_to_rate_limited_with_retry_after() {
    // Review Focus #3
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
        .mount(&server).await;
    let err = OwlsClient::new(&server.uri(), "k").get_ev("nba", 3.0).await.unwrap_err();
    assert!(matches!(err, OwlsError::RateLimited { retry_after }
                     if retry_after == Duration::from_secs(2)));
}

#[tokio::test]
async fn maps_403_to_forbidden_with_required_tier() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            r#"{"error":"tier","requiredTier":"mvp","currentTier":"bench"}"#))
        .mount(&server).await;
    let err = OwlsClient::new(&server.uri(), "k").get_ev("nba", 3.0).await.unwrap_err();
    assert!(matches!(err, OwlsError::Forbidden { required_tier: Some(t) } if t == "mvp"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test client_test`
Expected: FAIL — `OwlsClient` does not exist.

- [ ] **Step 3: Implement `src/error.rs` and `src/owls/client.rs`**

`reqwest::Client` built once and reused — a fresh client per request leaks connection pools and will trip the 15-concurrent cap. `get_ev` always sends `venues=true`; `min_ev` is serialized without a trailing `.0`. Rate-limit headers are stored behind a `Mutex` or `ArcSwap` and read via `quota()`; a `Remaining-Month` value of `"unlimited"` sets `unlimited_month` rather than failing to parse. A missing `Retry-After` on a 429 defaults to 2 seconds.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test client_test`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
git add src/ tests/
git commit -m "feat: Owls REST client with auth, quota capture, error taxonomy"
```

---

### Task 4: Schedule gating

**Files:**
- Create: `src/gating.rs`
- Test: `tests/gating_test.rs`

**Interfaces:**
- Consumes: `OwlsClient` (Task 3), `Config.lookahead_hours` and `Config.sports` (Task 1).
- Produces: `async fn active_sports(client: &OwlsClient, cfg: &Config, now: DateTime<Utc>) -> Result<BTreeSet<String>, OwlsError>`; `const GLOBAL_SCHEDULE_SPORTS: [&str; 8]`; `const EVENTS_ONLY_SPORTS: [&str; 2]` = `["ncaab", "mma"]`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn global_schedule_sports_exclude_ncaab_and_mma() {
    // /api/v1/schedule accepts only nfl ncaaf mlb nba wnba nhl tennis soccer ufc
    assert!(!GLOBAL_SCHEDULE_SPORTS.contains(&"ncaab"));
    assert!(!GLOBAL_SCHEDULE_SPORTS.contains(&"mma"));
    assert!(EVENTS_ONLY_SPORTS.contains(&"ncaab"));
    assert!(EVENTS_ONLY_SPORTS.contains(&"mma"));
}

#[tokio::test]
async fn includes_sport_with_upcoming_game_in_window() {
    // schedule fixture has an NBA game 3h out; lookahead is 48h
    let set = active_sports(&client, &cfg, now).await.unwrap();
    assert!(set.contains("nba"));
}

#[tokio::test]
async fn excludes_sport_with_no_upcoming_games() {
    // wnba out of season: schedule returns count 0 for it
    let set = active_sports(&client, &cfg, now).await.unwrap();
    assert!(!set.contains("wnba"));
}

#[tokio::test]
async fn excludes_already_started_games() {
    // EV is pregame-only: a game with status in_progress must not activate its sport
    let set = active_sports(&client, &cfg, now).await.unwrap();
    assert!(!set.contains("nhl"));
}

#[tokio::test]
async fn gates_ncaab_via_events_endpoint() {
    let server = /* mock /api/v1/ncaab/events with an upcoming game */;
    let set = active_sports(&client, &cfg, now).await.unwrap();
    assert!(set.contains("ncaab"));
}

#[tokio::test]
async fn makes_exactly_three_gating_requests() {
    // 1 global schedule + 1 ncaab/events + 1 mma/events
    // mounts carry .expect(1); MockServer verifies on drop
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test gating_test`
Expected: FAIL — `active_sports` does not exist.

- [ ] **Step 3: Implement `src/gating.rs`**

Call `get_schedule(2)` once, plus `get_events` for each of `EVENTS_ONLY_SPORTS`. A sport is active when it has at least one entry whose `status` is not started and whose `commence_time` falls between `now` and `now + lookahead_hours`. Intersect the result with `cfg.sports` so config narrows but never widens. Map the schedule's `ufc` entries onto the `mma` key only if `mma` is not already active from its own events call.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test gating_test`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
git add src/gating.rs tests/gating_test.rs
git commit -m "feat: schedule gating with events fallback for ncaab and mma"
```

---

### Task 5: Opportunity filtering

**Files:**
- Create: `src/filter.rs`
- Test: `tests/filter_test.rs`

**Interfaces:**
- Consumes: `Opportunity`, `EvEvent`, `EvMeta` (Task 2), `Config` (Task 1).
- Produces: `fn is_publishable(opp: &Opportunity, cfg: &Config) -> Verdict`; `enum Verdict { Publish, Reject(RejectReason) }`; `enum RejectReason { BelowThreshold, BookNotAllowed, NotTradeable, ImplausibleEv, BelowConsensusMinimum }`; `fn effective_trade_bar(kind: Option<Kind>) -> f64`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn threshold_is_strictly_greater_than() {
    // operator asked for "over 3.5%"
    assert!(matches!(is_publishable(&opp_with_ev(3.5), &cfg), Verdict::Reject(RejectReason::BelowThreshold)));
    assert!(matches!(is_publishable(&opp_with_ev(3.51), &cfg), Verdict::Publish));
}

#[test]
fn rejects_book_outside_allowlist() {
    let opp = opp_with_book("bovada");  // valid key, not one of the operator's 7
    assert!(matches!(is_publishable(&opp, &cfg), Verdict::Reject(RejectReason::BookNotAllowed)));
}

#[test]
fn honours_explicit_tradeable_false() {
    let mut opp = opp_with_ev(5.0);
    opp.tradeable = Some(false);
    assert!(matches!(is_publishable(&opp, &cfg), Verdict::Reject(RejectReason::NotTradeable)));
}

#[test]
fn absent_tradeable_falls_back_to_trade_bar_by_kind() {
    // Review Focus #2: tradeable is optional, so derive it rather than dropping everything
    assert_eq!(effective_trade_bar(Some(Kind::Book)), 2.0);
    assert_eq!(effective_trade_bar(Some(Kind::Venue)), 4.0);

    let mut venue = opp_with_ev(3.8);      // clears 3.5 but not the 4% venue bar
    venue.kind = Some(Kind::Venue);
    venue.tradeable = None;
    assert!(matches!(is_publishable(&venue, &cfg), Verdict::Reject(RejectReason::NotTradeable)));

    let mut book = opp_with_ev(3.8);       // clears both 3.5 and the 2% book bar
    book.kind = Some(Kind::Book);
    book.tradeable = None;
    assert!(matches!(is_publishable(&book, &cfg), Verdict::Publish));
}

#[test]
fn rejects_implausible_ev_above_server_ceiling() {
    // >25% is dropped server-side as a data error; defend anyway
    assert!(matches!(is_publishable(&opp_with_ev(30.0), &cfg),
                     Verdict::Reject(RejectReason::ImplausibleEv)));
}

#[test]
fn consensus_minimum_applies_only_when_value_present() {
    let mut cfg = cfg.clone();
    cfg.min_books_in_consensus = Some(2);

    let mut single = opp_with_ev(5.0);
    single.books_in_consensus = Some(1);
    assert!(matches!(is_publishable(&single, &cfg),
                     Verdict::Reject(RejectReason::BelowConsensusMinimum)));

    let mut unknown = opp_with_ev(5.0);
    unknown.books_in_consensus = None;   // must publish, not silently suppress
    assert!(matches!(is_publishable(&unknown, &cfg), Verdict::Publish));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test filter_test`
Expected: FAIL — `is_publishable` does not exist.

- [ ] **Step 3: Implement `src/filter.rs`**

Order the checks so the most specific reason wins: implausible EV, then book allowlist, then threshold, then trade bar, then consensus minimum. When `tradeable` is `None`, compare `ev_pct` against `effective_trade_bar(opp.kind)`; a `None` kind defaults to the stricter 4% bar. When `books_in_consensus` is `None` and a minimum is configured, **publish** — the spec requires failing open and marking, never suppressing invisibly.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test filter_test`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
git add src/filter.rs tests/filter_test.rs
git commit -m "feat: opportunity filtering with trade-bar fallback"
```

---

### Task 6: Suppression state machine

**Files:**
- Create: `src/dedupe.rs`
- Test: `tests/dedupe_test.rs`

**Interfaces:**
- Consumes: `Opportunity`, `Side` (Task 2).
- Produces: `struct Suppressor`; `Suppressor::new(window: Duration, rise_pp: f64) -> Self`; `fn observe(&mut self, key: &AlertKey, ev: f64, now: Instant) -> Decision`; `fn mark_absent(&mut self, seen: &HashSet<AlertKey>) -> ()`; `fn seed(&mut self, key: &AlertKey, ev: f64, now: Instant)`; `enum Decision { Publish, Suppress }`; `struct AlertKey { event_id: String, book: String, side: Side }` with `AlertKey::from(ev: &EvEvent, opp: &Opportunity) -> AlertKey`.

**Naming note:** the server describes its rule as once per "event, venue and side", but the API response field is `book` on every opportunity including exchanges and prediction markets. This plan uses `book` throughout to match the data model — `AlertKey.book` is `Opportunity.book`, whether that value is `fanduel` or `polymarket`. `event_id` is `EvEvent.canonical_event_id`, which is the id the spec says `ev.found` correlates on, so phase 2's webhook path can share this state unchanged.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn first_observation_publishes() {
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    assert_eq!(s.observe(&key(), 4.0, t0()), Decision::Publish);
}

#[test]
fn repeat_within_window_is_suppressed() {
    // REST /ev returns the same opportunity every poll; without this it posts 60x/hour
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    s.observe(&key(), 4.0, t0());
    assert_eq!(s.observe(&key(), 4.0, t0() + Duration::from_secs(60)), Decision::Suppress);
    assert_eq!(s.observe(&key(), 4.9, t0() + Duration::from_secs(120)), Decision::Suppress);
}

#[test]
fn rise_of_one_point_republishes() {
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    s.observe(&key(), 4.0, t0());
    assert_eq!(s.observe(&key(), 5.0, t0() + Duration::from_secs(60)), Decision::Publish);
}

#[test]
fn exact_one_point_rise_is_not_lost_to_float_error() {
    // Review Focus #4: 4.1 + 1.0 != 5.1 exactly in f64
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    s.observe(&key(), 4.1, t0());
    assert_eq!(s.observe(&key(), 5.1, t0() + Duration::from_secs(60)), Decision::Publish);
}

#[test]
fn window_expiry_republishes() {
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    s.observe(&key(), 4.0, t0());
    assert_eq!(s.observe(&key(), 4.0, t0() + Duration::from_secs(7201)), Decision::Publish);
}

#[test]
fn rearms_after_two_consecutive_absences() {
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    s.observe(&key(), 4.0, t0());
    s.mark_absent(&HashSet::new());
    s.mark_absent(&HashSet::new());
    assert_eq!(s.observe(&key(), 4.0, t0() + Duration::from_secs(300)), Decision::Publish);
}

#[test]
fn single_absence_does_not_rearm() {
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    s.observe(&key(), 4.0, t0());
    s.mark_absent(&HashSet::new());
    assert_eq!(s.observe(&key(), 4.0, t0() + Duration::from_secs(120)), Decision::Suppress);
}

#[test]
fn seeded_key_publishes_nothing_but_still_honours_rise() {
    // cold start: record the live board silently, then alert only on real movement
    let mut s = Suppressor::new(Duration::from_secs(7200), 1.0);
    s.seed(&key(), 4.0, t0());
    assert_eq!(s.observe(&key(), 4.0, t0() + Duration::from_secs(60)), Decision::Suppress);
    assert_eq!(s.observe(&key(), 5.0, t0() + Duration::from_secs(120)), Decision::Publish);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test dedupe_test`
Expected: FAIL — `Suppressor` does not exist.

- [ ] **Step 3: Implement `src/dedupe.rs`**

Per-key state is `{ last_published_ev: f64, last_published_at: Instant, absent_polls: u32 }`. `observe` publishes when the key is unknown, when `now - last_published_at >= window`, or when `ev + f64::EPSILON * 8.0 >= last_published_ev + rise_pp` — the epsilon margin is what Review Focus #4 requires; do not write a bare `>=` on the raw sum. Publishing resets all three fields. `mark_absent` increments `absent_polls` for every key not in `seen` and removes keys reaching 2. `seed` writes the same state as a publish but returns nothing.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test dedupe_test`
Expected: PASS (8 tests)

- [ ] **Step 5: Commit**

```bash
git add src/dedupe.rs tests/dedupe_test.rs
git commit -m "feat: suppression state machine mirroring server dedup rule"
```

---

### Task 7: Discord publisher

**Files:**
- Create: `src/discord/mod.rs`, `src/discord/publisher.rs`
- Test: `tests/publisher_test.rs`

**Interfaces:**
- Consumes: `EvEvent`, `Opportunity` (Task 2).
- Produces: `fn build_embed(ev: &EvEvent, opp: &Opportunity, enriched: bool) -> CreateEmbed`; `struct Publisher`; `Publisher::new(http: Arc<Http>, channel: ChannelId, capacity: usize) -> (Publisher, Sender<Alert>)`; `async fn run(self) -> ()`; `struct Alert { event: EvEvent, opportunity: Opportunity, enriched: bool }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn embed_contains_required_decision_fields() {
    let e = build_embed(&event(), &opp_full(), true);
    let s = format!("{:?}", e);
    for needle in ["Lakers", "fanduel", "4.2", "Kelly", "EV"] {
        assert!(s.contains(needle), "embed missing {needle}");
    }
}

#[test]
fn embed_marks_single_source_fair() {
    // booksInConsensus == 1 means Pinnacle alone priced it
    let mut o = opp_full();
    o.books_in_consensus = Some(1);
    assert!(format!("{:?}", build_embed(&event(), &o, true)).contains("Pinnacle only"));
}

#[test]
fn embed_marks_unenriched_alerts() {
    assert!(format!("{:?}", build_embed(&event(), &opp_full(), false)).contains("unenriched"));
}

#[test]
fn embed_surfaces_staleness() {
    let mut o = opp_full();
    o.quote_age_ms = Some(150_000);
    assert!(format!("{:?}", build_embed(&event(), &o, true)).contains("150"));
}

#[test]
fn embed_omits_link_field_when_absent() {
    let mut o = opp_full();
    o.link = None;
    let _ = build_embed(&event(), &o, true);  // must not panic
}

#[tokio::test]
async fn queue_drops_oldest_when_full() {
    // a stale +EV price is worthless, so newest wins
    let (_pub, tx) = Publisher::new(http_stub(), channel(), 2);
    tx.try_send(alert_with_ev(1.0)).unwrap();
    tx.try_send(alert_with_ev(2.0)).unwrap();
    tx.try_send(alert_with_ev(3.0)).unwrap();   // must succeed, evicting ev=1.0
    assert_eq!(drain(&tx).first().unwrap().opportunity.ev_pct, 2.0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test publisher_test`
Expected: FAIL — `build_embed` does not exist.

- [ ] **Step 3: Implement `src/discord/publisher.rs`**

`build_embed` includes teams, commence time, book, side, American price, EV%, fair probability, Kelly fraction, consensus count, and quote age; `link` becomes a URL field only when present. `enriched == false` adds a visible "unenriched" marker. `books_in_consensus == Some(1)` adds a "Pinnacle only" warning.

The queue is a bounded channel wrapped so a full queue evicts the oldest entry before pushing — `tokio::sync::mpsc` has no built-in eviction, so hold a `VecDeque` behind a `Mutex` with a `Notify`, or use a ring buffer. `run` consumes alerts and sends them, logging send failures without exiting.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test publisher_test`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
git add src/discord/ tests/publisher_test.rs
git commit -m "feat: Discord embed builder and newest-wins publish queue"
```

---

### Task 8: Poll loop and quota governor

**Files:**
- Create: `src/poller.rs`
- Modify: `src/owls/quota.rs`
- Test: `tests/poller_test.rs`, `tests/quota_test.rs`

**Interfaces:**
- Consumes: `OwlsClient` (3), `active_sports` (4), `is_publishable` (5), `Suppressor` (6), `Alert` sender (7).
- Produces: `async fn poll_once(ctx: &mut PollContext, cfg: &Config, now_wall: DateTime<Utc>, now_mono: Instant) -> Result<PollOutcome, OwlsError>`; `struct PollContext { client, suppressor, tx, active: BTreeSet<String>, seeded: HashSet<String>, venue_sports: HashSet<String> }`; `PollOutcome { polled: usize, published: usize, suppressed: usize }`; `fn next_interval(base: Duration, q: &QuotaSnapshot, day_of_month: u32) -> Duration`.

**Pre-flight Ruling 1 — two clocks, deliberately.** `now_wall` is compared
against `commence_time` for gating; `now_mono` is passed to `Suppressor`, which
measures the 2-hour window and must be immune to NTP steps and DST. `Instant`
and `DateTime<Utc>` are not interconvertible, so both are parameters rather than
one being derived from the other. Task 9's `silence_warning` already takes
`Instant` for the same reason.

**Pre-flight Ruling 2 — `cfg` is a parameter, not state.** `Config` is
deliberately *not* a field of `PollContext`. Task 10 hot-reloads config through
a `watch` channel; a snapshot captured in `PollContext` at boot would silently
ignore every later change and freeze the operator's threshold — defeating the
spec's runtime-changeability requirement. The caller reads the current value and
passes it in on each poll.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn interval_unchanged_when_quota_healthy() {
    let q = QuotaSnapshot { remaining_month: Some(250_000), ..healthy() };
    assert_eq!(next_interval(Duration::from_secs(60), &q, 10), Duration::from_secs(60));
}

#[test]
fn interval_lengthens_as_monthly_budget_depletes() {
    let q = QuotaSnapshot { remaining_month: Some(5_000), ..healthy() };
    assert!(next_interval(Duration::from_secs(60), &q, 25) > Duration::from_secs(60));
}

#[test]
fn unlimited_month_never_degrades() {
    let q = QuotaSnapshot { unlimited_month: true, remaining_month: None, ..healthy() };
    assert_eq!(next_interval(Duration::from_secs(60), &q, 28), Duration::from_secs(60));
}

#[tokio::test]
async fn first_poll_of_a_sport_seeds_without_publishing() {
    // cold start must not dump the whole live board into the channel
    let mut ctx = ctx_with_active(&["nba"]);
    let out = poll_once(&mut ctx, now()).await.unwrap();
    assert_eq!(out.published, 0);
    assert!(out.suppressed > 0);
    assert!(ctx.seeded.contains("nba"));
}

#[tokio::test]
async fn second_poll_publishes_a_new_crossing() {
    let mut ctx = ctx_with_active(&["nba"]);
    poll_once(&mut ctx, now()).await.unwrap();              // seeds
    ctx.client = client_returning_higher_ev();              // +2pp on one price
    let out = poll_once(&mut ctx, now() + chrono::Duration::seconds(60)).await.unwrap();
    assert_eq!(out.published, 1);
}

#[tokio::test]
async fn stale_board_yields_no_opportunities() {
    let mut ctx = ctx_with_client(client_returning_stale_board());
    assert_eq!(poll_once(&mut ctx, now()).await.unwrap().published, 0);
}

#[tokio::test]
async fn venue_warmup_does_not_mark_sport_inactive() {
    // Review Focus #5: first venues=true call returns no Kalshi/Polymarket for ~15s
    let mut ctx = ctx_with_active(&["nba"]);
    ctx.client = client_returning_no_venue_quotes();
    poll_once(&mut ctx, now()).await.unwrap();
    assert!(ctx.active.contains("nba"));
    assert!(ctx.venue_sports.contains("nba"));  // stays on the venue-priority list
}

#[tokio::test]
async fn rate_limit_error_does_not_abort_remaining_sports() {
    let mut ctx = ctx_with_active(&["nba", "nhl"]);  // nba 429s, nhl succeeds
    let out = poll_once(&mut ctx, now()).await.unwrap();
    assert_eq!(out.polled, 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test poller_test --test quota_test`
Expected: FAIL — `poll_once` does not exist.

- [ ] **Step 3: Implement `src/owls/quota.rs` governor and `src/poller.rs`**

`next_interval` compares `remaining_month` against the budget that *should* remain given `day_of_month`, and scales the interval up when actual is behind pace; `unlimited_month` short-circuits to `base`. Degradation order is the spec's §6.2: never lengthen past the floor for sports in `venue_sports`.

`poll_once` iterates `active` **sequentially — one request in flight at a time.** This is a correctness requirement, not a style preference: requests beyond the 15-concurrent cap are refused with an immediate 429 rather than queued, so a `join_all` over ten sports risks self-inflicted rate limiting for no latency gain at a 60-second cadence. Call `get_ev(sport, cfg.min_ev_pct.floor())` — `3.5_f64.floor()` is `3.0`, which is the server-side floor, with the strict `> 3.5` cut applied by `is_publishable`. Skip the board entirely when `meta.is_empty_sentinel()` or `meta.freshness.stale`. For each opportunity it runs `is_publishable`, then `observe` or `seed` depending on whether the sport is in `seeded`. It records which sports returned any `Kind::Venue` opportunity into `venue_sports`. A per-sport error is logged and the loop continues — one sport's 429 must not starve the rest.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test poller_test --test quota_test`
Expected: PASS (8 tests)

- [ ] **Step 5: Commit**

```bash
git add src/poller.rs src/owls/quota.rs tests/
git commit -m "feat: poll loop with cold-start seeding and quota governor"
```

---

### Task 9: Observability and ops alerts

**Files:**
- Create: `src/obs.rs`
- Test: `tests/obs_test.rs`

**Interfaces:**
- Consumes: `QuotaSnapshot` (3), active-sport set (4).
- Produces: `fn quota_warning(q: &QuotaSnapshot, day_of_month: u32, warn_at_pct: f64) -> Option<String>`; `fn silence_warning(last_alert: Option<Instant>, active: &BTreeSet<String>, threshold: Duration, now: Instant) -> Option<String>`; `fn init_tracing() -> ()`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn warns_when_burn_rate_outpaces_the_month() {
    // 90% spent by day 10 of 30
    let q = QuotaSnapshot { remaining_month: Some(30_000), ..healthy() };
    assert!(quota_warning(&q, 10, 0.8).is_some());
}

#[test]
fn silent_when_burn_rate_is_on_pace() {
    let q = QuotaSnapshot { remaining_month: Some(200_000), ..healthy() };
    assert!(quota_warning(&q, 10, 0.8).is_none());
}

#[test]
fn no_silence_warning_when_no_sports_are_active() {
    // a quiet Tuesday with no slate is not a fault
    let quiet = BTreeSet::new();
    assert!(silence_warning(None, &quiet, Duration::from_secs(3600), t0()).is_none());
}

#[test]
fn warns_on_silence_only_while_a_slate_is_live() {
    let active: BTreeSet<String> = ["nba"].iter().map(|s| s.to_string()).collect();
    let long_ago = t0() - Duration::from_secs(7200);
    assert!(silence_warning(Some(long_ago), &active, Duration::from_secs(3600), t0()).is_some());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test obs_test`
Expected: FAIL — `quota_warning` does not exist.

- [ ] **Step 3: Implement `src/obs.rs`**

`init_tracing` configures `tracing_subscriber` with a JSON formatter writing to stdout. `quota_warning` compares spend against linear pace for `day_of_month` and returns a message when spend exceeds `warn_at_pct` of pace. `silence_warning` returns `None` whenever `active` is empty — the spec requires silence be judged against the slate, not a bare timer.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test obs_test`
Expected: PASS (4 tests)

- [ ] **Step 5: Commit**

```bash
git add src/obs.rs tests/obs_test.rs
git commit -m "feat: quota and slate-aware silence warnings, structured logging"
```

---

### Task 10: Wiring, shutdown, and config hot-reload

**Files:**
- Modify: `src/main.rs`
- Create: `src/config_watch.rs`
- Test: `tests/shutdown_test.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: a runnable binary. `async fn run(cfg_path: PathBuf, shutdown: CancellationToken) -> Result<()>`; `fn watch_config(path: PathBuf, tx: watch::Sender<Config>) -> Result<RecommendedWatcher>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn run_exits_cleanly_on_cancellation() {
    let token = CancellationToken::new();
    let h = tokio::spawn(run("config/default.toml".into(), token.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(5), h).await.unwrap().unwrap().is_ok());
}

#[tokio::test]
async fn config_change_is_picked_up_without_restart() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("c.toml");
    write_config(&p, 3.5);
    let (tx, mut rx) = watch::channel(Config::load(&p).unwrap());
    let _w = watch_config(p.clone(), tx).unwrap();
    write_config(&p, 6.0);
    tokio::time::timeout(Duration::from_secs(5), rx.changed()).await.unwrap().unwrap();
    assert_eq!(rx.borrow().min_ev_pct, 6.0);
}

#[tokio::test]
async fn invalid_config_on_reload_keeps_the_previous_value() {
    // a typo in the file must not take the bot down mid-slate
    let dir = tempdir().unwrap();
    let p = dir.path().join("c.toml");
    write_config(&p, 3.5);
    let (tx, rx) = watch::channel(Config::load(&p).unwrap());
    let _w = watch_config(p.clone(), tx).unwrap();
    std::fs::write(&p, "min_ev_pct = \"not a number\"").unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(rx.borrow().min_ev_pct, 3.5);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test shutdown_test`
Expected: FAIL — `run` does not exist.

- [ ] **Step 3: Implement `src/config_watch.rs` and `src/main.rs`**

`main` reads secrets from env (failing fast with a clear message naming any missing var), calls `init_tracing`, then `run`. `run` spawns the gating refresh, the poll loop, the publisher, and the config watcher, and selects on `shutdown`. On cancellation it stops accepting new alerts and drains the publisher queue before returning. `watch_config` uses `notify`; a reload that fails validation logs an error and leaves the previous `Config` in place.

Install a SIGTERM and SIGINT handler that cancels the token.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS — all suites.

- [ ] **Step 5: Verify the binary runs against the live API**

Run: `OWLS_API_KEY=... DISCORD_TOKEN=... DISCORD_ALERT_CHANNEL_ID=... cargo run`
Expected: structured JSON logs on stdout showing a gating pass, an active-sport set, and a first poll that seeds without publishing. No Discord message on the first cycle — that is correct behaviour.

- [ ] **Step 6: Commit**

```bash
git add src/ tests/
git commit -m "feat: wire poller, publisher, config watch, graceful shutdown"
```

---

## Out of scope for phase 1

Carried to phase 2 (see spec §10): the `axum` webhook receiver, HMAC-SHA256 signature verification, the webhook bootstrap runbook, the enrichment join, `GET /healthz`, the Dockerfile, and hosting. Phase 1's portability rules are what make those additive rather than a rewrite.
