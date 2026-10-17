# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

**Phase 1 complete** on branch `feat/phase1-poller` (poller-only; no webhook
receiver). Design and plan:

- [docs/superpowers/specs/2026-10-16-discord-ev-bot-design.md](docs/superpowers/specs/2026-10-16-discord-ev-bot-design.md)
- [docs/superpowers/plans/2026-10-16-phase1-poller.md](docs/superpowers/plans/2026-10-16-phase1-poller.md)

Read the design before proposing changes — it records decisions and the
reasoning behind them, several of which are counterintuitive. Phase 2 (webhook
receiver, HMAC verification, hosting) is scoped in the design's §10.

## Commands

```sh
cargo test                       # whole suite
cargo test --test dedupe_test    # one suite
cargo test -- --nocapture        # with println output
cargo clippy --all-targets
cargo fmt

# run it (needs .env loaded; EVBOT_CONFIG overrides the config path)
set -a; . ./.env; set +a
cargo run

# post embed format previews to the OPS channel (never the alert channel)
cargo run --example preview_alert
```

Secrets come from `.env` (gitignored). `.env.example` lists what is required.

## Architecture

Three cooperating loops in [src/runtime.rs](src/runtime.rs), driven by
[src/main.rs](src/main.rs):

```
gating (hourly) ──> active sport set
                         │
poll loop (60s) ─────────┴─> client ─> filter ─> suppressor ─> queue
                                                                 │
publisher ───────────────────────────────────────────────────────┘
```

| Module | Responsibility |
|---|---|
| [src/config.rs](src/config.rs) | Operator criteria; TOML + `EVBOT_` env overrides, validated |
| [src/owls/models.rs](src/owls/models.rs) | API response types; **casing varies per struct** |
| [src/owls/client.rs](src/owls/client.rs) | Auth, quota header capture, error taxonomy |
| [src/owls/quota.rs](src/owls/quota.rs) | `next_interval` burn-rate governor |
| [src/gating.rs](src/gating.rs) | Which sports are worth polling (3 calls) |
| [src/filter.rs](src/filter.rs) | Threshold, allowlist, trade bar, consensus |
| [src/dedupe.rs](src/dedupe.rs) | **Anti-spam core.** Mirrors the server's rule |
| [src/poller.rs](src/poller.rs) | Per-cycle orchestration, cold-start seeding |
| [src/discord/publisher.rs](src/discord/publisher.rs) | Embeds, newest-wins queue, 429 handling |
| [src/obs.rs](src/obs.rs) | Quota and slate-aware silence warnings |

Three things here are load-bearing and easy to break:

- **`dedupe.rs` mirrors the server's suppression rule** because REST `/ev` has
  none of its own. Its `+1 point` comparison needs the epsilon margin; a bare
  `>=` silently swallows real alerts.
- **Cold-start seeding** means a restart publishes nothing until the next
  genuine crossing. That is deliberate, not a bug.
- **`silence_warning` measures from process start** when nothing has been
  published. Treating `None` as "maximally quiet" made the bot false-alarm on
  every startup — found by running it, not by a test.

Tests use captured live responses in `tests/fixtures/`, not hand-written JSON.
That is how the casing inconsistency, the non-integer `bookPrice`, and the
nullable `canonicalEventId` were all caught. Prefer re-capturing a fixture over
inventing one.

## What this is

A Rust service that posts a Discord message when a positive expected value
(+EV) moneyline price appears at one of the operator's books. EV is computed
**by the Owls Insight API**, not here. This project filters and delivers; it
does not price.

## Toolchain

Rust 1.92.0, Python 3.13.2, Docker 27.4.0, git 2.39.5, gh 2.95.0.
**No Node/npm** — anything Node-based (including n8n) requires Docker.

## Owls Insight API — load-bearing facts

Base URL `https://api.owlsinsight.com`. Plan: **MVP + WebSocket add-on**.

- **Auth is `Authorization: Bearer <key>`.** Explicitly *not* `x-api-key`.
  The WebSocket is different again: the key goes in the Socket.IO **query
  string**, not a header and not the auth object.
- **There is no Rust SDK.** TypeScript, Python, Go, and Kotlin only. The
  OpenAPI 3.1 spec at `https://owlsinsight.com/openapi.json` (291 schemas) is
  the source of truth for types.
- **EV is moneyline-only and pregame-only.** No spreads, totals, props, or
  in-play. This is a product limit with no workaround — do not design around
  the hope that it changes.
- **EV is not available over the WebSocket.** All 38 WebSocket events carry raw
  or consensus odds. EV comes only from REST `/api/v1/{sport}/ev` or the
  `ev.found` webhook. The WebSocket add-on cannot deliver the core signal.
- **Server-side dedup is a feature, not an obstacle.** `ev.found` fires at most
  once per 15 min per event/venue/side, then suppresses for 2 hours unless EV
  rises ≥1 point. Never bypass it. Client-side filters may only ever *narrow*.
- **`ev.found` sends no backfill** — "only the crossings that come after"
  registration. The channel is silent after deploy until a new crossing. The
  REST poller covers the current board.
- **The webhook payload is thinner than the REST response.** It omits
  `booksInConsensus`, `kellyFraction`, `edgePp`, `roi`, `fairUnadjusted`, and
  `tradeable`/`reason`. Recover them by joining the poller's cached board on
  `canonicalEventId`.
- **EV > 25% is dropped server-side as a data error.** Real edges are
  single-digit to low double-digit percent. Treat a huge number as a bug.

### Filter mismatch between the two surfaces

| Filter | REST `/ev` | Webhook `ev_found` |
|---|---|---|
| EV floor | `min_ev` — any number | `min_ev` — **enum: 1, 2, 3, 5, 8** |
| Book | `book` — exactly one | `venues` — a list, 11 allowed keys |
| Exchanges | `venues=true` | `kinds: [book\|venue]` |
| Sports | one per call | `sports` — a list |

The operator's 3.5% threshold is **not expressible server-side**. Register at
`min_ev: 3`, cut at `> 3.5` in the receiver.

### Trade bar sets the real floor

`ev.found` only fires on *tradeable* prices: sportsbooks need ≥2% EV; exchanges
and prediction markets need ≥4% plus a bid, a spread ≤4¢, and ≥$50 at the ask.
So at a 3.5% setting, BetMGM/FanDuel/DraftKings/bet365 alert at 3.5%, while
Novig/Kalshi/Polymarket effectively floor at **4%**.

### Book keys

In scope: `betmgm` `fanduel` `draftkings` `bet365` `novig` `kalshi` `polymarket`.

- `polymarket` is **refused by the webhook allowlist** → REST-only, needs
  `venues=true`. This is why the hybrid design is required rather than optional.
- `kalshi` needs `kinds` to include `venue`.
- **Fanatics is out of scope and cannot be added.** Two products share the
  name: the Fanatics *Sportsbook* has no public feed anywhere in the API, and
  Fanatics *Markets* (`fanaticsmarkets`) is a Nadex-settled prediction market
  covering only `boxing, golf` — neither is an EV sport. Do not re-litigate
  this; no path produces Fanatics EV.

### Sports in scope (10)

`nfl nba mlb nhl ncaaf ncaab wnba soccer tennis mma` — all within `ev.found`
coverage. The 4 esports keys (`cs2 dota2 lol valorant`) are deliberately excluded.

### Quota arithmetic

MVP: **300,000 req/month**, 400/min, 15 concurrent. Naive polling of all 10
sports at 60s is **144% of quota** — it does not fit.

The fix is schedule gating: poll `/ev` only for sports with an unstarted game in
the lookahead window. EV is pregame-only, so a sport with no slate is worth zero
requests. At ~5 active sports and 60s: ~218,000/month (73%).

**But `/api/v1/schedule` does not cover all ten sports.** Its `sport` parameter
accepts only `nfl, ncaaf, mlb, nba, wnba, nhl, tennis, soccer, ufc` —
**`ncaab` is absent** and **`mma` exists only as `ufc`**. `/api/v1/{sport}/schedule`
excludes `mma` too. Use **`GET /api/v1/{sport}/events`**, which covers both
`ncaab` and `mma`. So gating is three calls per refresh, not one:

| Call | Covers |
|---|---|
| `GET /api/v1/schedule?days=2` | nfl, ncaaf, mlb, nba, wnba, nhl, tennis, soccer |
| `GET /api/v1/ncaab/events` | ncaab |
| `GET /api/v1/mma/events` | mma |

Use **`days=2`** — `days=1` returns only the current calendar day in `tz`
(default `America/New_York`), so a late-evening refresh misses tomorrow's slate.

**REST `/ev` has no dedup of its own.** It is a full board snapshot returning the
same qualifying opportunity on every poll. The 15-min/2-hour/+1-point rule is a
property of `ev.found` *firing*, not of the data. The poller must mirror that
rule client-side, share dedupe state with the webhook path, and **seed silently
on cold start** — otherwise every restart dumps the whole live board into the
channel.

**`venues=true` is warm-on-demand.** Kalshi/Polymarket prices appear ~15s after
the first request and stay on "while you keep asking." Going quiet on a sport can
switch those quotes *off*, not merely make them stale — so venue-carrying sports
need a floor on their poll interval and must be last to degrade.

Read `X-RateLimit-Remaining-Month` on every response and degrade the interval as
budget depletes rather than stopping dead at the cap.

### Webhook signature verification

Getting this wrong fails silently and accepts forged alerts.

1. Read the **raw body bytes before any JSON parsing.** The HMAC covers the raw
   body; re-serializing changes the bytes and breaks verification.
2. HMAC-SHA256 over `t` + `"."` + raw body, keyed with the **full secret
   including the `whsec_` prefix**, as UTF-8.
3. **Constant-time compare** (use `subtle`), never `==`.
4. Reject `t` more than 5 minutes from local clock.
5. During secret rotation the header carries **two** `v1` values — accept either.
6. Dedupe on event `id`, identical across retries.
7. **Return 2xx before posting to Discord.** The budget is 10s (3s to connect)
   and a redirect counts as a failure. Hand off to a worker; never post on the
   request path.

Failure consequences: 5 consecutive failures → backoff; 50 failures plus 72h
without success → `auto_disabled`, recoverable only by `PATCH {"enabled": true}`.

### Webhook URL requirement

HTTPS, default port, **public hostname**. IP addresses and private/internal
hosts are refused at registration *and on every delivery*. The webhook path
cannot be developed against localhost — use a tunnel or build the polling path
first.

## Working agreements

- **Git and GitHub are performed manually by the operator**, who is learning
  them. Explain commands and let them run them. Do not run `git commit`,
  `git push`, or create branches/PRs unless explicitly asked.
- Treat `meta.freshness.stale`, `board_age_ms`, and `quote_age_ms` as
  first-class. The API tells us when its own data is stale; surface it rather
  than hiding it.
- Webhooks are documented as **beta** ("shape is stable, signals still
  evolving"). Parse defensively — an unknown field is not an error.
- Secrets (`OWLS_API_KEY`, `OWLS_WEBHOOK_SECRET`, `DISCORD_TOKEN`) come from the
  environment, never the config file. Every operator-facing criterion belongs in
  hot-reloadable config, never a compiled constant.

## Reference

- Docs https://owlsinsight.com/docs · `llms.txt` https://owlsinsight.com/llms.txt
- OpenAPI https://owlsinsight.com/openapi.json · Status https://owlsinsight.com/status
- MCP server `https://api.owlsinsight.com/mcp` (Streamable HTTP, Bearer auth) —
  attach it to query live odds while developing.
