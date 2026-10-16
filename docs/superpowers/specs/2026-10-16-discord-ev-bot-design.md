# Discord +EV Alert Bot — Design

**Date:** 2026-10-16
**Status:** Approved for planning
**Author:** David (with Claude)

---

## 1. Intent

A Rust service that posts a message into Discord when a positive expected value
(+EV) moneyline price appears at one of the operator's sportsbooks or exchanges.

The data comes from the **Owls Insight API** (`https://api.owlsinsight.com`),
which computes EV server-side against a Pinnacle de-vigged fair line. This
project does **not** compute EV itself. It is a delivery and filtering layer on
top of someone else's pricing engine.

### Why now

The operator holds an Owls Insight **MVP plan plus the WebSocket add-on**. The
MVP tier is the first tier that unlocks both the EV endpoint and webhooks, so
the capability is already paid for and unused. The manual alternative — watching
a web dashboard — does not scale and misses short-lived edges.

### Why this shape

Three findings from the API reference drove the architecture:

1. **EV is not available over the WebSocket.** All 38 documented WebSocket
   events carry raw or consensus odds. EV is published only via REST
   `GET /api/v1/{sport}/ev` and the `ev.found` webhook. The WebSocket add-on
   the operator pays for cannot deliver the core signal.
2. **The `ev.found` webhook already solves alert deduplication** — at most once
   per 15 minutes per event/venue/side, then suppressed for 2 hours unless EV
   rises by ≥1 point. Reimplementing this is the main failure mode of EV bots
   and it is free here.
3. **`ev.found` refuses some books the operator uses.** Polymarket is not in the
   webhook allowlist, so it must come from REST polling. A hybrid design is
   therefore required, not optional.

---

## 2. Goals

- Post a Discord message when a +EV moneyline price clears the configured
  threshold at a configured book, within seconds of the API publishing it.
- Cover all eight configured book/venue keys, including Polymarket, which
  webhooks cannot deliver.
- Survive dropped webhook deliveries (delivery is at-least-once and unordered).
- Keep every operator-facing criterion changeable at runtime without a redeploy.
- Stay inside the MVP tier's 300,000 requests/month quota with headroom.

## 3. Non-goals

- Computing EV, fair value, or de-vigging. The API does this.
- Placing or staking bets. The bot informs; a human acts.
- Spreads, totals, player props, or live/in-play EV. The API serves **moneyline
  only, pregame only** for EV, and that is a product limit with no workaround.
- A web UI or dashboard. Discord is the interface.
- WebSocket streaming in v1. It carries no EV, so it is deferred to a later
  enrichment phase.
- **Discord slash commands in v1.** The bot is post-only to start. It connects
  with a bot token so commands can be added in v2 without re-architecting, but
  no interaction handling, command registration, or permission scoping is built
  now. Consequence: runtime config changes mean editing the file on the host,
  and an on-demand "what's live right now" lookup is unavailable until v2.
- **Webhook endpoint self-registration.** The operator registers it by hand
  (see §6.6). The bot never creates or deletes endpoints.

## 4. Anti-goals

Things that would make this a failure even if it technically works:

- **Alert spam.** A channel people mute is worthless. Server-side dedup must be
  preserved, never bypassed, and client-side filters must only ever narrow.
- **Alerting on an unusable price.** A book the operator has no funded account
  at, or a price already dead by the time it posts. Staleness must be visible.
- **Silent failure.** A bot that stops receiving webhooks and says nothing is
  worse than no bot, because it is trusted. Liveness must be observable.
- **Hardcoded criteria.** Thresholds, sports, and books compiled into the binary.

---

## 5. Constraints

### From the API

| Constraint | Value | Source |
|---|---|---|
| Auth (REST) | `Authorization: Bearer <key>` — not `x-api-key` | `[authentication]` |
| Auth (WebSocket) | API key in Socket.IO query string | `[websocket]` |
| EV market scope | **Moneyline only, pregame only** | `/ev` notes |
| EV sports (14) | `cs2 dota2 lol mlb mma nba ncaab ncaaf nfl nhl soccer tennis valorant wnba` | `/ev` |
| EV board staleness | No opportunity served if snapshot > 90s old | `/ev` |
| Price freshness | Sportsbook < 3 min; exchange quote < 2 min | `/ev` |
| EV ceiling | > 25% EV dropped server-side as data error | `/ev` |
| Monthly quota | 300,000 req/month (MVP) | `[rate-limits]` |
| Per-minute limit | 400 req/min, 15 concurrent | `[rate-limits]` |
| Quota headers | `X-RateLimit-Remaining-Minute`, `-Month`, `-Reset-Minute`, `-Reset-Month` | `[rate-limits]` |
| Webhook endpoints | 3 max (MVP) | `[webhooks-api]` |
| Webhook event caps | 120/min, 20,000/day (MVP) | `[webhooks-api]` |
| Webhook receiver SLA | Any 2xx within 10s (3s to connect); redirects are failures | `[webhooks-api]` |
| Webhook URL rules | HTTPS, default port, public hostname. IPs and private hosts refused at registration **and every delivery** | `[webhooks-api]` |
| Signature | `Owls-Signature: t=<unix>,v1=<hex>` — HMAC-SHA256 over `t` + `.` + raw body, secret includes the `whsec_` prefix | `[webhooks-api]` |
| Clock skew | Reject `t` more than 5 minutes from local clock | `[webhooks-api]` |
| Auto-disable | 5 consecutive failures → backoff; 50 failures + 72h no success → `auto_disabled` | `[webhooks-api]` |
| No Rust SDK | TypeScript, Python, Go, Kotlin only | `[sdk]` |

### Webhook vs REST filter mismatch

These two surfaces do not accept the same filters, and the difference is
load-bearing:

| Filter | REST `/ev` | Webhook `ev_found` |
|---|---|---|
| EV floor | `min_ev` — any number | `min_ev` — **enum: 1, 2, 3, 5, 8** |
| Book | `book` — exactly one key | `venues` — a list, from 11 allowed keys |
| Exchanges | `venues=true` (adds Kalshi + Polymarket) | `kinds: [book\|venue]` |
| Sports | one per call (path param) | `sports` — a list |

The operator's **3.5%** threshold is not expressible server-side. Register at
`min_ev: 3` and apply the `> 3.5` cut in the receiver.

### Trade bar — effective floors differ by venue type

`ev.found` fires only on prices that are **tradeable**:

- Sportsbooks: EV ≥ 2%
- Exchanges and prediction markets: EV ≥ 4%, plus a bid, a spread ≤ 4¢, and
  ≥ $50 at the ask

So the operator's 3.5% setting yields:

| Venue | Effective floor |
|---|---|
| BetMGM, FanDuel, DraftKings, bet365 | **3.5%** |
| Novig, Kalshi, Polymarket | **4%** (trade bar dominates) |

### Operator configuration (as given)

**Sports (10):** `nfl nba mlb nhl ncaaf ncaab wnba soccer tennis mma`
All ten are within `ev.found` coverage — registration will not 400.

**Books (8 requested → 7 viable):**

| Book | Key | `ev.found` | REST `/ev` |
|---|---|---|---|
| BetMGM | `betmgm` | yes | yes |
| FanDuel | `fanduel` | yes | yes |
| DraftKings | `draftkings` | yes | yes |
| bet365 | `bet365` | yes | yes |
| Novig | `novig` | yes | yes |
| Kalshi | `kalshi` | yes (`kinds` incl. `venue`) | yes, needs `venues=true` |
| Polymarket | `polymarket` | **no — refused** | yes, needs `venues=true` |
| Fanatics | — | **no** | **no — excluded from scope** |

**On Fanatics:** dropped by operator decision after the following was
established. Two distinct products share the name. The **Fanatics Sportsbook**
has no public feed and appears nowhere in the API. **Fanatics Markets**
(`fanaticsmarkets`) is a Nadex-settled prediction market whose coverage is
`boxing, golf` — neither is an EV sport. No path, including a custom engine on
the v2 Source API, produces Fanatics EV.

**EV threshold:** > 3.5%, configurable.

### Environment

| Tool | State |
|---|---|
| Rust | 1.92.0 ✅ |
| Python | 3.13.2 ✅ (no `uv`) |
| Docker | 27.4.0 ✅ |
| git / gh | 2.39.5 / 2.95.0 ✅ |
| Node / npm | ❌ absent |
| n8n | ❌ absent (Node-based; would need Docker) |

The operator is learning git and GitHub during this project and wants to run
those commands manually. Automation should not silently perform git operations.

---

## 6. Architecture

> **Phasing note.** This section describes the complete system. §10 splits it
> into two phases: **phase 1 ships the poller path only** (§6.2–§6.5, §6.7) and
> needs no hosting. The webhook receiver (§6.1), its bootstrap (§6.6), and the
> enrichment join in §6.3 are **phase 2**. The "primary / secondary" labels
> below describe the finished system; in phase 1 the poller is the only path,
> and it carries `booksInConsensus` and `kellyFraction` natively, so no
> enrichment is needed until webhooks exist.

```
                    ┌──────────────────────────────┐
                    │      Owls Insight API        │
                    └──────────────────────────────┘
                       │ ev.found          │ REST /ev
                       │ (push, 6 books)   │ (poll, all books)
                       ▼                   ▼
              ┌─────────────────┐   ┌──────────────────┐
              │ axum receiver   │   │ schedule-gated   │
              │ · verify HMAC   │   │ poller (60s)     │
              │ · reject skew   │   │ · /schedule 1/hr │
              │ · dedupe on id  │   │ · /ev?venues=true│
              │ · 2xx < 10s     │   │ · quota governor │
              └────────┬────────┘   └─────────┬────────┘
                       │                      │
                       └──────────┬───────────┘
                                  ▼
                       ┌──────────────────────┐
                       │  filter + enrich     │
                       │  · > 3.5% cut        │
                       │  · book allowlist    │
                       │  · booksInConsensus  │
                       │  · kellyFraction     │
                       │  · local dedupe      │
                       └──────────┬───────────┘
                                  ▼
                       ┌──────────────────────┐
                       │  Discord publisher   │
                       │  · embed + deep link │
                       │  · rate-limit aware  │
                       └──────────────────────┘
```

### 6.1 Webhook receiver (primary path)

An `axum` HTTP service, publicly reachable over HTTPS on port 443.

Registration body:

```json
{
  "url": "https://<host>/owls/ev",
  "event_types": ["ev.found"],
  "description": "Discord +EV alerts",
  "filters": {
    "sports": ["nfl","nba","mlb","nhl","ncaaf","ncaab","wnba","soccer","tennis","mma"],
    "ev_found": {
      "min_ev": 3,
      "kinds": ["book","venue"],
      "venues": ["betmgm","fanduel","draftkings","bet365","kalshi","novig"]
    }
  }
}
```

Request handling order matters:

1. Read the **raw body bytes** before any JSON parsing. The HMAC is computed
   over the raw body; re-serializing changes the bytes and breaks the signature.
2. Parse `Owls-Signature` (`t=<unix>,v1=<hex>`). During secret rotation the
   header carries **two** `v1` values — accept either.
3. Compute HMAC-SHA256 over `t` + `"."` + raw body, keyed with the full secret
   including the `whsec_` prefix, as UTF-8. Compare in **constant time**.
4. Reject if `t` is more than 5 minutes from local clock.
5. Dedupe on the event `id`, which is identical across retries.
6. **Return 2xx immediately.** Hand off to a worker channel; do not post to
   Discord on the request path. The 10-second budget is not negotiable and
   Discord latency is outside our control.

### 6.2 Schedule-gated poller (secondary path)

Serves three purposes in v1: Polymarket coverage, dropped-delivery safety net,
and the enrichment cache. A fourth — on-demand board queries — arrives with the
v2 command surface and needs no change to the poller.

#### Schedule gating — three calls, not one

`GET /api/v1/schedule` does **not** cover all ten sports. Its `sport` parameter
accepts only `nfl, ncaaf, mlb, nba, wnba, nhl, tennis, soccer, ufc`:

- **`ncaab` is absent entirely** — and it has near-daily slates Nov–Mar, i.e.
  exactly the season this bot will be used.
- **`mma` appears only as `ufc`**, a narrower set: Bellator and PFL are MMA but
  not UFC, while the EV endpoint keys on `mma`.

`GET /api/v1/{sport}/schedule` doesn't rescue this either — it excludes `mma`.
But **`GET /api/v1/{sport}/events` covers both `ncaab` and `mma`**, and returns
`commence_time` and `status`, which is everything gating needs. All paying tiers.

So the hourly refresh is three calls:

| Call | Covers |
|---|---|
| `GET /api/v1/schedule?days=2` | nfl, ncaaf, mlb, nba, wnba, nhl, tennis, soccer |
| `GET /api/v1/ncaab/events` | ncaab |
| `GET /api/v1/mma/events` | mma |

**`days=2`, not `days=1`.** `days=1` returns only the current calendar day in
`tz` (default `America/New_York`), so a late-evening refresh sees nothing of
tomorrow's slate. `meta.status` reports `partial` when the range exceeds
`buildWindow` (3 UTC days back to 9 ahead).

Gating cost: 3 calls/hour ≈ **2,160 req/month**. Negligible.

#### Poll loop

- Build the active-sport set: sports with an unstarted game inside the
  configured lookahead window. EV is pregame-only, so a sport with no upcoming
  slate is worth zero requests.
- Every 60s: `GET /api/v1/{sport}/ev?venues=true&min_ev=3` per active sport.
  One call per sport returns every book, so this simultaneously catches
  Polymarket, refreshes enrichment data, and backfills anything a webhook
  dropped.
- Filter the poller path on `tradeable` for parity with the webhook trade bar.
  REST exposes the flag and a `reason`, so parity costs nothing.

Budget at ~5 active sports: **~218,000 req/month (73% of quota)**, leaving
~82,000 for enrichment, commands, and webhook management.

#### Degradation priority under quota pressure

The governor sheds load least-valuable-first:

1. **Drop the safety-net sweep** for sports whose books are all webhook-covered.
   Dropped deliveries are rare; this sweep is belt-and-braces.
2. **Lengthen the interval** on sports with no venue-eligible prices.
3. **Never drop a sport carrying Kalshi or Polymarket exposure.** Polymarket is
   structurally unavailable via webhook, so polling is its *only* path — losing
   it loses the book entirely.

One caveat on stretching intervals: `venues=true` prices are warm-on-demand —
"they appear within about 15 seconds and stay on while you keep asking." Going
quiet on a sport may switch Kalshi/Polymarket quotes **off**, not merely make
them staler. Venue-carrying sports therefore need a floor on their interval.

A quota governor reads `X-RateLimit-Remaining-Month` on every response and
degrades the interval as the month's budget depletes, rather than stopping dead
at the cap.

### 6.3 Filter and enrich

- Apply the `> 3.5` cut (config).
- Apply the book allowlist (config) — client-side filters may only narrow.
- Join against the cached poller board on `canonicalEventId` to recover
  `booksInConsensus` and `kellyFraction`, which the webhook payload omits.
#### Dedupe — and why the poller needs its own

The 15-min/2-hour/+1-point suppression is a property of **`ev.found` firing**.
REST `/ev` has no dedup concept whatsoever: it is a full board snapshot that
returns the same qualifying opportunity on **every poll** for as long as the
price lives. At 60s intervals, a naive poller would republish the same edge
sixty times an hour. A backstop against our own double-processing is not
sufficient to drive the poll loop.

So the poller **mirrors the server's rule client-side**, keyed on
`(event_id, venue, side)`:

- Publish on first observation.
- Suppress for 2 hours thereafter, unless `evPct` rises ≥1 point above the
  value last published, which re-publishes and resets the window.
- Re-arm once the price has been absent or non-`tradeable` for two consecutive
  polls.

This is deliberately the same shape as the server's rule, so an opportunity
behaves identically whether it arrived by push or by poll. **Webhook-delivered
alerts share this dedupe state**, which also prevents a webhook and a poll from
double-posting the same edge.

#### Cold start: seed silently

On boot the poller sees a board already full of qualifying prices. Publishing
them is a burst of every live opportunity across ten sports straight into the
channel — the alert-spam anti-goal, on every single restart.

So: **on the first poll of a sport after boot, record the board into the dedupe
state and publish nothing.** Alert only on crossings observed afterwards. This
matches `ev.found` semantics ("only the crossings that come after"), making push
and poll consistent.

The cost is explicit: a restart during an active slate means a quiet channel
until the next genuine crossing, and a Polymarket edge live across the restart
is never announced. That is the correct trade — a missed alert is recoverable,
a channel people mute is not. An on-demand board lookup covers "what's live
right now."
- Flag `booksInConsensus == 1` visually — Pinnacle alone priced that fair.

#### When enrichment is unavailable: publish anyway, marked

The cache can be cold after a restart, the sport may be outside the active poll
set, or the board may be stale. In that case:

- **Publish the alert without the enriched fields, visibly marked as
  unenriched.** Never hold or drop it. Holding alerts on a cold cache produces a
  silent bot immediately after deploy — the anti-goal.
- **Never block the publish path on an outbound API call.** The "return 2xx
  immediately" design exists to decouple these; a per-alert enrich call
  reintroduces exactly the latency and quota coupling it avoids.
- `booksInConsensus` is therefore a **display flag by default, not a hard
  filter.** It may be configured as a hard filter, but when configured that way
  and the value is unavailable, the alert publishes marked rather than being
  silently suppressed — an operator filter that fails closed and invisibly is
  worse than one that fails open and says so.

### 6.4 Discord publisher

**Post-only in v1.** Connects with a bot token (`DISCORD_TOKEN`) over the
gateway, but registers no commands and handles no interactions. The token rather
than an incoming-webhook URL is deliberate: it costs nothing now and means v2
commands are an addition rather than a rewrite.

- One embed per opportunity: teams, commence time, book, side, price, EV%,
  fair probability, Kelly fraction, consensus count, quote age, and the `link`
  deep link from the payload.
- A bounded worker queue with backpressure. If Discord rate-limits, drop
  **oldest** alerts first — a stale +EV price is worthless, so newest-wins is
  correct here.
- Staleness must be visible on the face of the embed, from `quote_age_ms` and
  `board_age_ms`.

### 6.5 Configuration

Hot-reloadable file. Every operator criterion lives here; none compiled in:

- sports list, book/venue allowlist, EV threshold (global + per-book overrides)
- poll interval, schedule lookahead window
- `booksInConsensus` minimum
- Discord channel routing (per-sport channels are a plausible later want)

Changes that affect server-side filters are pushed with
`PATCH /api/v1/webhooks/{id}` (filters merge per section; `sports` replaces the
list). Limit: 30 endpoint changes/minute/account.

Secrets — `OWLS_API_KEY`, `OWLS_WEBHOOK_SECRET`, `DISCORD_TOKEN` — come from the
environment, never the config file.

### 6.6 Webhook bootstrap — operator-performed, once

The operator registers the endpoint by hand. The bot **never** creates or
deletes webhook endpoints.

Why: the signing secret is returned exactly once, at `POST /api/v1/webhooks`
(and at `rotate-secret`) — later reads give only `secret_hint`. Self-registration
would therefore need somewhere to persist a secret the design keeps in the
environment. It also carries a sharp failure mode: MVP caps the account at **3
endpoints and disabled ones still count**, so a create-on-boot bug bricks the
account after three restarts.

Bootstrap, run once by the operator:

1. `POST /api/v1/webhooks` with the registration body in §6.1.
2. Save `secret` (`whsec_...`) to `OWLS_WEBHOOK_SECRET` — it is shown only here.
3. Save `id` to `OWLS_WEBHOOK_ID`.
4. `POST /api/v1/webhooks/{id}/test` to verify the receiver and its signature
   check end to end before trusting it.

The bot then reads both from the environment and may only:

- `PATCH /api/v1/webhooks/{id}` to reconcile filters when config changes
- `GET /api/v1/webhooks/{id}` to read health
- `PATCH {"enabled": true}` to recover from `auto_disabled`

A missing or mismatched `OWLS_WEBHOOK_ID` is a **startup failure**, not a
warning. Running without it means the bot cannot reconcile filters or detect
auto-disablement, which is the silent-failure anti-goal.

### 6.7 Observability

Non-negotiable, because a silent bot is an actively harmful bot:

**Routing: a separate ops channel**, configurable, never the alert channel. The
anti-goals require the alert channel stay signal-only, and health noise there
would train the operator to skim exactly the channel that must be read.

- Track time since last webhook delivery. Alert if it exceeds a threshold
  during an active slate. **Silence must be judged against the slate model**,
  not a bare timer: §6.2 establishes that a sport with no upcoming games is
  worth zero requests, so long legitimate silences are normal and a naive timer
  will cry wolf. Only warn when the slate says games are live and pre-start.
- Surface `meta.freshness.stale` and `board_age_ms` — the API tells us when its
  own data is stale.
- Poll `GET /api/v1/webhooks/{id}` for `status`, `consecutive_failures`, and
  `last_success_at`. Detect `auto_disabled` and report it loudly.
- Log remaining monthly quota; warn at a configurable burn rate.

---

## 7. Decisions made, and by whom

| Decision | Who |
|---|---|
| Rust as the implementation language | Operator |
| Sports: the 10 listed, excluding the 4 esports | Operator |
| Books: the 8 listed | Operator |
| EV threshold > 3.5% | Operator |
| Criteria must be runtime-changeable | Operator |
| Drop Fanatics from scope | Operator, after the no-feed finding |
| Git/GitHub performed manually, as a teaching exercise | Operator |
| Post-only in v1; bot token now, slash commands in v2 | Operator |
| Operator registers the webhook endpoint by hand | Operator |
| Run locally on this machine first, cloud-portable later | Operator |
| Hosting is optional, not a prerequisite — open to it if simpler | Operator |
| Phase 1 is poller-only: no receiver, no tunnel, no hosting | Claude |
| Webhooks deferred to phase 2, where hosting is recommended over tunnelling | Claude |
| 12-factor portability rules enforced from day one | Claude |
| Webhooks as the primary signal path | Claude |
| Schedule-gated polling as the secondary path | Claude |
| Defer WebSocket to a later enrichment phase | Claude |
| Drop n8n from the critical path | Claude |
| 60s poll interval, 1h schedule refresh | Claude |
| Return 2xx before posting to Discord | Claude |
| Newest-wins under Discord backpressure | Claude |
| Poller mirrors the server's dedup rule client-side | Claude |
| Cold start seeds the dedupe set silently, publishes nothing | Claude |
| Shared dedupe state across the push and poll paths | Claude |
| Unenriched alerts publish marked, never held | Claude |
| `booksInConsensus` is a display flag by default, not a hard filter | Claude |
| Health alerts go to a separate ops channel | Claude |
| `/events` gating fallback for `ncaab` and `mma` | Claude |
| `days=2` on the schedule refresh | Claude |
| Venue-carrying sports are last to degrade under quota pressure | Claude |
| Poller filters on `tradeable` for webhook parity | Claude |

## 8. Left to the builder

- Crate selection. Expected shape: `tokio`, `axum`, `reqwest`, `serde`,
  `hmac` + `sha2` + `subtle` (constant-time compare), a Discord client
  (`serenity` or `twilight`), and a rate limiter (`governor`).
- Whether to generate types from `https://owlsinsight.com/openapi.json`
  (OpenAPI 3.1, 291 schemas) or hand-write the subset actually used. Generation
  is likely right given the schema count, but the toolchain is the builder's
  call.
- Module boundaries and internal error taxonomy.
- Persistence: whether the dedupe set and enrichment cache need to survive
  restart, or an in-memory structure with a cold-start window is acceptable.
- Hosting and TLS termination. Must satisfy the public-HTTPS-port-443
  requirement, and must be reachable from the public internet.
- Test strategy, including how to exercise signature verification. The API
  provides `POST /api/v1/webhooks/{id}/test`, which sends a real signed
  `webhook.test` event — valuable for an end-to-end check.

## 9. Open risks

1. **Webhooks are beta.** Documented as "shape is stable, signals still
   evolving." Treat payload fields defensively; an unknown field is not an error.
2. **Hosting is a hard prerequisite.** The public-HTTPS requirement means the
   webhook path cannot be developed against localhost. Early work may need the
   polling path first, or a tunnel, to make progress.
3. **Quota is tight if the active-sport set runs large.** Ten simultaneously
   active sports at 60s is 144% of quota. The governor must degrade gracefully,
   and the lookahead window is the main tuning lever. NCAAB season (Nov–Mar)
   overlaps NFL, NBA, NHL and NCAAF, so the winter active set is the stress
   case — and `ncaab` is the one sport that cannot be schedule-gated by the
   cheap global call.
5. **`venues=true` warm-on-demand coupling.** Kalshi and Polymarket prices
   switch on about 15 seconds after the first request and stay on only "while
   you keep asking." Interval changes and sport-dropping therefore have a
   side effect on data *availability*, not just freshness. Needs measurement
   once running; the exact decay behaviour is undocumented.
4. **`ev.found` sends no backfill.** "Only the crossings that come after"
   registration. The channel is silent after deploy until a new crossing; the
   poller covers the current board.

---

## 10. Deployment: two phases

**Operator requirement:** run on this Mac now; movable to cloud later. The
operator is unsure about hosting mechanics and cost, so phase 1 is designed to
need none.

### The asymmetry that makes this easy

Only **one** component needs inbound connectivity: the webhook receiver.
Everything else is outbound-only and works on any laptop behind any NAT:

| Component | Direction | Works locally? |
|---|---|---|
| REST poller → `api.owlsinsight.com` | outbound HTTPS | **yes** |
| Discord publisher → Discord gateway | outbound WSS | **yes** |
| Webhook receiver ← Owls Insight | **inbound HTTPS** | **no** |

Webhook delivery cannot reach localhost: the API refuses IP addresses, private
and reserved addresses, and internal hostnames, at registration *and on every
delivery*. No local configuration changes this.

### Phase 1 — local, no hosting, no tunnel

**Poller-only.** A complete, useful bot that runs with `cargo run` on this Mac
and needs no public address, no tunnel, and no hosting spend.

It is not a degraded version. REST `/ev?venues=true` returns **every** book,
so phase 1 covers all seven viable venues — **one more than webhooks can**,
since `ev.found` refuses Polymarket. The only loss is latency: alerts arrive
within one poll interval (60s) instead of on push.

It is also **materially simpler**, because the enrichment subsystem disappears.
Enrichment existed only to recover `booksInConsensus` and `kellyFraction`, which
the thin webhook payload omits — REST carries both natively. Phase 1 is
therefore:

```
poller → dedupe → Discord publisher
```

No HMAC verification, no raw-body handling, no receiver, no public URL, no
enrichment join, no tunnel.

### Phase 2 — add webhooks, which needs hosting

Adds the `axum` receiver and §6.1's signature verification for push-latency
alerts on the six webhook-eligible books. The poller **remains** — it is still
Polymarket's only path and the dropped-delivery safety net.

This phase requires a public HTTPS hostname. Two ways to get one:

**Host it (recommended).** A small always-on instance provides the hostname
natively and deletes the tunnel as a moving part.

| Option | Approx. cost | Notes |
|---|---|---|
| **Fly.io** | ~$2–4/mo | `shared-cpu-1x`, 256MB. Free `*.fly.dev` HTTPS. Needs a Dockerfile. Fine-grained cost control. |
| **Railway** | ~$5/mo | Deploys from GitHub, auto-TLS, `*.up.railway.app`. Gentlest path. |
| **Render** | ~$7/mo | **Its free tier spins down when idle — fatal for a webhook receiver.** Paid tier only. |
| **Hetzner / DO VPS** | ~$4–6/mo | Self-managed: OS updates, TLS via Caddy. Most learning, most work. |

Verify current pricing before committing; these move. A Rust binary at ~256MB
RAM sits at the bottom of every pricing tier, which is a real cost advantage
over a Python equivalent here.

**Or tunnel from this Mac.** Keeps compute local. Needs a *stable* hostname —
if the URL rotates per restart, each restart needs a
`PATCH /api/v1/webhooks/{id}`, and a forgotten PATCH means silent alert loss
with no error anywhere.

| Option | Stable hostname | Notes |
|---|---|---|
| **Tailscale Funnel** | yes, free | Stable `*.ts.net`, no domain needed. Lowest setup cost. |
| **Cloudflare Tunnel** (named) | yes, free | Needs a domain on Cloudflare. Ephemeral `trycloudflare` URLs rotate — avoid. |
| ngrok free | **no** | Rotates per restart. Static domains are paid. |

Neither is installed; Homebrew is available. Note the tunnel also requires this
Mac to be awake and online whenever a game is pre-start, which hosting does not.

### Portability rules — follow from day one

The cloud move is only cheap if nothing local leaks into the code:

- **Config from file + env override.** No machine-specific absolute paths.
- **Bind address and port from env** (`BIND_ADDR`, `PORT`). Never hardcode.
- **Public URL is config, not derived.** The webhook URL the bot reports and
  reconciles must come from config, because locally it is a tunnel hostname and
  in cloud it is the service's own.
- **Logs to stdout**, structured. No log files — every cloud platform captures
  stdout and nothing captures your local files.
- **Graceful shutdown on SIGTERM.** Cloud platforms send it before stopping a
  container; draining the publisher queue on signal prevents losing queued
  alerts on every deploy.
- **A health endpoint** (`GET /healthz`) returning 200 when the receiver is
  live. Cloud platforms need it for liveness checks; locally it is how you
  confirm the tunnel is wired up.
- **State directory from config**, defaulting to something relative. If
  persistence is added (see below), it must not assume a macOS path.

### Dev loop vs deployment artifact

Run natively with `cargo run` for development — containerizing the inner loop
adds rebuild latency for no benefit while iterating. Add a **multi-stage
Dockerfile** as the deployment artifact. Because the rules above make the binary
environment-agnostic, the Dockerfile is additive and can land late without
reshaping anything.

### Persistence

§8 leaves the mechanism to the builder. The local-first requirement narrows it:
whatever is chosen must move with the service. **SQLite** (single file, no
separate process, no network dependency) is the portable choice if the dedupe
set and enrichment cache need to survive restart. In-memory remains acceptable
for v1 — cold start already seeds silently, so a restart loses suppression state
and re-seeds rather than spamming.

Avoid anything requiring a separate managed service in v1; that converts a
one-machine move into a multi-service migration.

### Eventual cloud target

Worth stating plainly: a small always-on cloud instance (Fly.io, Railway,
Render) provides the public HTTPS hostname the API demands **natively**, which
deletes the tunnel entirely. The tunnel exists only to satisfy the local-first
requirement. When the move happens, the change is: point `PUBLIC_URL` at the new
host, `PATCH` the webhook `url`, done — provided the portability rules above
were followed.

---

## Appendix: reference

- Docs: https://owlsinsight.com/docs · `llms.txt`: https://owlsinsight.com/llms.txt
- OpenAPI 3.1: https://owlsinsight.com/openapi.json (v0.78.0, 60 paths, 291 schemas)
- Keys: https://owlsinsight.com/dashboard/keys · Status: https://owlsinsight.com/status
- MCP server: `https://api.owlsinsight.com/mcp` (Streamable HTTP, Bearer auth)
