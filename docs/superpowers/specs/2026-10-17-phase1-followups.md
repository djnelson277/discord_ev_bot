# Phase 1 — deferred follow-ups

**Date:** 2026-10-17
**Source:** final whole-branch review of `feat/phase1-poller`

The review raised 1 Critical and 7 Important findings, all fixed in `1f49b64`
with regression tests. These are the **Minor** ones, deliberately not fixed —
none changes behaviour an operator would notice, so they were left for the
operator to prioritise rather than padding the fix pass.

Recorded here because the execution ledger that held them is gitignored scratch.

---

## Worth doing before the first busy slate

### 1. Quota headers get wiped by a header-less response
`src/owls/quota.rs` · `QuotaSnapshot::from_headers` returns all-`None` when a
response carries no rate-limit headers (a CDN or proxy error page, say), and
`client.rs` overwrites unconditionally. Both the governor and the burn warning
treat `None` as "no information, don't act", so a single bad response silently
disables both until the next good one — and clears `unlimited_month`.

**Fix:** merge rather than replace; only overwrite fields the response carried.

### 2. Shutdown drain is unbounded and the poll pass is not cancellation-aware
`src/runtime.rs` · Cancellation is observed only at the top of the loop and in
the sleep. A cancel arriving inside `poll_once` waits out up to 10 sequential
requests at a 20s timeout (~200s worst case), and `post_embed` can sleep up to
60s × 4 retries per queued alert during the drain. Container platforms SIGKILL
at 10–30s, so the "graceful shutdown" guarantee holds only while the APIs are
healthy. The runtime test passes because its mocks answer instantly.

**Fix:** pass the cancellation token into `poll_once` and check between sports;
bound the drain with a deadline.

### 3. `EvResponse.success` is parsed and never checked
`src/owls/models.rs` · A `200` carrying `success: false` with a well-formed
`meta` would be treated as a live board.

**Fix:** treat `success == false` as unusable, alongside the staleness checks.

---

## Cosmetic or judgement calls

### 4. `board_age_ms` never reaches an operator
Design §6.4 and §6.7 both ask for board age on the embed. `Board::age_ms` is
parsed and unused, and `Alert` carries no board context, so embeds show quote
age only. A spec-vs-plan gap: the plan's Task 7 didn't list it.

### 5. Lowering `min_ev_pct` by hot-reload dumps the whole newly-qualifying board
Nothing re-seeds on a criteria change, so every already-seeded sport publishes
everything that now clears the lower bar, at once. Arguably exactly what the
operator asked for — but worth a deliberate decision given how much care went
into avoiding board dumps everywhere else.

### 6. Constants that arguably belong in config
`SUPPRESSION_WINDOW`, `SUPPRESSION_RISE_PP`, `ALERT_QUEUE_CAPACITY`,
`SILENCE_THRESHOLD`, `QUOTA_WARN_AT` (`src/runtime.rs`) and
`MONTHLY_QUOTA_MVP` (`src/owls/quota.rs`) are compiled in, against the Global
Constraint that operator criteria live in hot-reloadable config. The window and
rise defensibly mirror the server's own rule. The 90-minute silence threshold,
the 0.8 warn fraction and the plan quota are more operator-dependent — and the
quota constant is plan-specific, so it must move if the plan ever changes.

### 7. `result_large_err` on `OwlsError` (clippy, 9 occurrences)
The `Forbidden` variant holds three `Option<String>`, making every
`Result<_, OwlsError>` large. **Fix:** box the variant's payload.

### 8. Two trivial clippy lints
A collapsible `if` and a constant-value assertion in `src/filter.rs`.

---

## Not a follow-up, but the real open risk

**None of this has run against a live board carrying actual opportunities.**
Every live run so far happened outside a pregame window, so every board was
empty — meaning the filter, suppression and publish paths have only ever been
exercised by tests and fixtures, never end to end on real data.

The 5-sport × 6-cycle regression test covers the suppression logic that the
Critical finding broke, but the first genuine slate is the actual proving
ground. Watch the first one with `RUST_LOG=debug` to see rejection reasons.
