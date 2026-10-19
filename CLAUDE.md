# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust service that posts a Discord message when a positive expected value
(+EV) moneyline price appears at one of the operator's books. **EV is computed
by the Owls Insight API, not here.** This project filters and delivers; it does
not price.

## Project status

**Phase 1 (poller-only) is complete, merged to `main`, and deployed on
Railway.** Phase 2 — webhook receiver, HMAC verification, `/healthz` — is
scoped in the design's §10 but not built.

| Document | What it holds |
|---|---|
| [design](docs/superpowers/specs/2026-10-16-discord-ev-bot-design.md) | Intent, goals, anti-goals, every API constraint with its source, and §11's live market measurements |
| [plan](docs/superpowers/plans/2026-10-16-phase1-poller.md) | The phase 1 task breakdown |
| [follow-ups](docs/superpowers/specs/2026-10-17-phase1-followups.md) | 8 deferred minors from the final review |
| [deploy runbook](docs/deploy-railway.md) | Railway setup, env vars, what behaves differently there |

Read the design before proposing changes. It records *why* for decisions that
look arbitrary, several of which are counterintuitive.

## Commands

```sh
cargo test                       # whole suite (156 tests)
cargo test --test dedupe_test    # one suite
cargo clippy --all-targets
cargo fmt

# run locally (needs .env; EVBOT_CONFIG overrides the config path)
set -a; . ./.env; set +a
cargo run

# keep a Mac awake while it runs — lid MUST stay open, caffeinate
# cannot override lid-close sleep
caffeinate -dis cargo run

# embed format previews, to the OPS channel only
cargo run --example preview_alert
```

**Verify container changes locally before deploying.** This is the same
artifact Railway runs:

```sh
docker build -t evbot .
set -a; . ./.env; set +a
docker run --rm -e OWLS_API_KEY -e DISCORD_TOKEN \
  -e DISCORD_ALERT_CHANNEL_ID -e DISCORD_OPS_CHANNEL_ID evbot
```

A deploy config that was never executed locally crash-looped in production on a
path that only existed at build time. One `docker run` would have caught it.

Secrets live in `.env` (gitignored). `.env.example` lists what is required.

## Architecture

Three cooperating loops in [src/runtime.rs](src/runtime.rs), driven by
[src/main.rs](src/main.rs):

```
gating (hourly) ──> active sport set
                         │
poll loop (60s) ─────────┴─> client ─> filter ─> suppressor ─> queue
                                                                 │
publisher ───────────────────────────────────────────────────────┘
                         │
heartbeat (30m) ─────────┴─> ops channel
```

| Module | Responsibility |
|---|---|
| [config.rs](src/config.rs) | Operator criteria; TOML + `EVBOT_` env overrides, validated with bounds |
| [owls/models.rs](src/owls/models.rs) | API response types; **casing varies per struct** |
| [owls/client.rs](src/owls/client.rs) | Auth, quota header capture, error taxonomy |
| [owls/quota.rs](src/owls/quota.rs) | Predictive burn-rate governor (`next_interval`) |
| [gating.rs](src/gating.rs) | Which sports are worth polling (3 calls) |
| [filter.rs](src/filter.rs) | Threshold, allowlist, trade bar, consensus |
| [dedupe.rs](src/dedupe.rs) | **Anti-spam core.** Mirrors the server's rule |
| [poller.rs](src/poller.rs) | Per-cycle orchestration, cold-start seeding, `best_ev` |
| [discord/publisher.rs](src/discord/publisher.rs) | Embeds, newest-wins queue, 429 handling |
| [obs.rs](src/obs.rs) | Ops heartbeat and quota burn warnings |

---

## Invariants — these look arbitrary and are not

Each of these was a real bug or a near miss. Changing one without reading its
reasoning will reintroduce the failure.

**1. `numReplicas = 1`, forever.** Suppression state is in memory per process.
A second replica is a second bot with an empty suppressor: it republishes
everything the first already sent *and* burns a second copy of the monthly
quota. Horizontal scaling is always wrong here. Enforced in `railway.toml`.

**2. Dedupe ageing must stay sport-scoped.** `mark_absent_in_sport` takes a
sport for a reason. An unscoped version ages every other sport's keys once per
sport polled, so at **three or more active sports** every key is evicted inside
a single cycle and republishes. This shipped, passed 128 tests, and collapsed
suppression entirely — because no test used more than two sports. Two is exactly
the boundary that hides it.

**3. The `+1 point` rise check needs its epsilon margin.** `4.1 + 1.0` is not
exactly `5.1` in f64. A bare `>=` on the raw sum silently swallows the alert the
operator most wants.

**4. Cold-start seeding is deliberate.** A restart records the live board and
publishes nothing until the next genuine crossing. Without it every restart
dumps the whole board into the alert channel. The cost — a quiet window after
each deploy — is accepted, not a bug.

**5. Filter order is immovable-before-tunable.** The trade bar is checked
*before* the operator's threshold because the order decides which reason gets
*reported*. Telling someone "your 2.5% threshold blocked this" when the server's
unchangeable 4% exchange bar also blocks it sends them to tune a knob that
cannot help.

**6. Never reintroduce a timer-based silence warning.** It can only say "either
nothing cleared the threshold or the poller has stopped" — the two states that
need telling apart. The heartbeat replaced it: absence of a heartbeat is the
alarm, and its content explains the quiet.

**7. `best_ev` must carry its outcome.** Reporting "best price seen: +4.46%"
beside "1 alert sent" invites the assumption they are the same price. They
frequently are not. `BestEvOutcome` records Published / Seeded / Suppressed /
Rejected-with-reason.

**8. Count deliveries, not queue pushes.** `outcome.published` counts pushes
onto the publish queue. The heartbeat must report what Discord actually
accepted, or the bot claims to have told you something it has not.

**9. Fixtures are captured from the live API, never hand-written.** That is how
the per-struct casing inconsistency, the non-integer `bookPrice`, and the
nullable `canonicalEventId` were all caught. Re-capture rather than invent.

---

## Owls Insight API — load-bearing facts

Base URL `https://api.owlsinsight.com`. Plan: **MVP + WebSocket add-on**.

- **Auth is `Authorization: Bearer <key>`.** Explicitly *not* `x-api-key`.
- **There is no Rust SDK.** The OpenAPI 3.1 spec at
  `https://owlsinsight.com/openapi.json` (291 schemas) is the type source of truth.
- **EV is moneyline-only and pregame-only.** No spreads, totals, props, in-play.
  A product limit with no workaround.
- **EV is not available over the WebSocket.** All 38 WebSocket events carry raw
  or consensus odds. EV comes only from REST `/ev` or the `ev.found` webhook.
  The WebSocket add-on cannot deliver the core signal.
- **EV > 25% is dropped server-side as a data error.** Treat a huge number as a bug.
- **Server-side dedup is a feature.** `ev.found` fires at most once per 15 min
  per event/venue/side. Client filters may only ever *narrow*.

### Silence is the normal state

Measured live on an NFL Sunday (design §11): **all ten sports returned zero
opportunities at `min_ev=0`** — not one positive-EV price anywhere, on the
busiest day of the US betting week. Hand-computed on Cardinals @ Rams, the best
of twelve books was +2.91%; on Chargers @ Chiefs it was −1.37%.

Why: the fair is Pinnacle de-vigged **minus a full percentage point**, so a book
must beat the line by more than a point to show *any* positive EV. Expect alerts
measured in per-day, not per-hour.

### Effective floor = max(operator threshold, server trade bar)

| Venue type | Server bar | Notes |
|---|---|---|
| Sportsbook | 2% | operator's threshold usually binds |
| Exchange / prediction market | **4%** | plus a bid, ≤4¢ spread, ≥$50 at ask |

The exchange requirements are **not expressible in config**. Lowering
`min_ev_pct` does nothing for Novig, Kalshi, or Polymarket.

### Book keys

In scope: `betmgm` `fanduel` `draftkings` `bet365` `novig` `kalshi` `polymarket`.

- `polymarket` is **refused by the webhook allowlist** → REST-only, needs
  `venues=true`. This is why the hybrid design is required.
- **Fanatics is out of scope and cannot be added.** The Fanatics *Sportsbook*
  has no public feed; Fanatics *Markets* is a Nadex prediction market covering
  `boxing, golf` only. Do not re-litigate.

### Sports in scope (10)

`nfl nba mlb nhl ncaaf ncaab wnba soccer tennis mma`. The 4 esports keys are
deliberately excluded.

### Quota arithmetic and gating

MVP: **300,000 req/month**, 400/min, 15 concurrent. All ten sports at 60s is
**144% of quota**. Gating fixes it — poll only sports with an unstarted game in
the lookahead window.

**`/api/v1/schedule` does not cover all ten sports.** Verified against the live
API: `?sport=ncaab` and `?sport=mma` both return **HTTP 400 Unknown sport**.
Structural, not seasonal. So gating is three calls:

| Call | Covers |
|---|---|
| `GET /api/v1/schedule?days=2` | nfl, ncaaf, mlb, nba, wnba, nhl, tennis, soccer |
| `GET /api/v1/ncaab/events` | ncaab |
| `GET /api/v1/mma/events` | mma |

`mma` is not a rename of `ufc`: the captured fixture holds 104 MMA events across
11 promotions, only 27 of them UFC.

Use **`days=2`** — `days=1` covers only the current calendar day in `tz`
(default `America/New_York`), so an evening refresh misses tomorrow.

**REST `/ev` has no dedup of its own.** It is a full board snapshot returning
the same opportunity every poll. See invariants 2–4.

**`venues=true` is warm-on-demand.** Kalshi/Polymarket quotes appear ~15s after
you start asking and stay only "while you keep asking". Going quiet can switch
them *off*, so venue-carrying sports have an interval ceiling
(`VENUE_MAX_INTERVAL`) and degrade last.

Polling is **sequential, one request in flight**. Past 15 concurrent the API
returns an immediate 429 rather than queueing.

---

## Deployment

Railway, built from the [Dockerfile](Dockerfile). See
[docs/deploy-railway.md](docs/deploy-railway.md).

Two runtime-image requirements that are easy to lose:

- **`ca-certificates` must be installed.** rustls verifies TLS against the
  system root store and `debian-slim` ships without it; every HTTPS call fails
  with a certificate error that reads like a network problem.
- **`config/` must be copied beside the binary**, because `Config::load`
  resolves `config/default.toml` relative to the working directory.

On Railway specifically: every deploy is a cold start (invariant 4), config
hot-reload is inert because the filesystem is ephemeral — tune with `EVBOT_*`
environment variables instead — and there is no healthcheck because phase 1
binds no port.

## Phase 2 notes (not yet built)

### Webhook signature verification

Getting this wrong fails silently and accepts forged alerts.

1. Read the **raw body bytes before any JSON parsing.** The HMAC covers the raw
   body; re-serializing breaks verification.
2. HMAC-SHA256 over `t` + `"."` + raw body, keyed with the **full secret
   including the `whsec_` prefix**, as UTF-8.
3. **Constant-time compare** (`subtle`), never `==`.
4. Reject `t` more than 5 minutes from local clock.
5. During rotation the header carries **two** `v1` values — accept either.
6. Dedupe on event `id`, identical across retries.
7. **Return 2xx before posting to Discord.** Budget is 10s; a redirect counts as
   a failure. Hand off to a worker.

5 consecutive failures → backoff; 50 failures plus 72h without success →
`auto_disabled`, recoverable only by `PATCH {"enabled": true}`.

### Webhook URL requirement

HTTPS, default port, **public hostname**. IPs and private hosts are refused at
registration *and on every delivery*. Cannot be developed against localhost.

The webhook payload is **thinner** than REST: it omits `booksInConsensus`,
`kellyFraction`, `edgePp`, `roi`, `fairUnadjusted`, `tradeable`/`reason`.
Recover them by joining the poller's board on `canonicalEventId`.

## Working agreements

- **Git**: the operator asked Claude to handle git operations for now and will
  take them back when they say so. They are learning git and GitHub, so explain
  what a command does rather than running it silently.
- Treat `meta.freshness.stale`, `board_age_ms`, and `quote_age_ms` as
  first-class. The API says when its own data is stale; surface it.
- Webhooks are **beta** ("shape is stable, signals still evolving"). Parse
  defensively — an unknown field is not an error. `Kind` already uses a lenient
  deserializer for this reason; `Side` deliberately does not.
- Secrets come from the environment, never a config file. Operator criteria
  belong in config, never a compiled constant.

## Reference

- Docs https://owlsinsight.com/docs · `llms.txt` https://owlsinsight.com/llms.txt
- OpenAPI https://owlsinsight.com/openapi.json · Status https://owlsinsight.com/status
- MCP server `https://api.owlsinsight.com/mcp` (Streamable HTTP, Bearer auth) —
  attach it to query live odds while developing.
