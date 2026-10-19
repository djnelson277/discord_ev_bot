# Deploying to Railway

Phase 1 is a **worker**, not a web service. It makes only outbound calls and
listens on no port, so it needs no public domain and no healthcheck. Phase 2's
webhook receiver is what will need those.

---

## Before you start

The repo must be on GitHub, on the branch Railway will watch.

```sh
git push
```

Decide which branch Railway deploys. `main` is the better long-term answer —
Railway redeploys on every push to the watched branch, so pointing it at a
feature branch means every work-in-progress commit goes to production.

---

## 1. Create the service

1. Sign in at [railway.app](https://railway.app) and connect your GitHub account.
2. **New Project → Deploy from GitHub repo →** `djnelson277/discord_ev_bot`.
3. Pick the branch (see above).

Railway reads [`railway.toml`](../railway.toml), which points it at the
[`Dockerfile`](../Dockerfile). The Rust version, build steps and start command
all live there, so you should not need to configure any of it in the dashboard.

The first build takes several minutes — it compiles ~370 crates from scratch.
Dependencies are built in their own layer, so later builds that only touch
`src/` reuse the cache and finish in seconds.

**Verify changes locally before deploying.** The container is the same artifact
Railway runs:

```sh
docker build -t evbot .
set -a; . ./.env; set +a
docker run --rm \
  -e OWLS_API_KEY -e DISCORD_TOKEN \
  -e DISCORD_ALERT_CHANNEL_ID -e DISCORD_OPS_CHANNEL_ID \
  evbot
```

An earlier Nixpacks attempt crash-looped in production on a path that only
existed at build time. One `docker run` would have caught it.

## 2. Set the four variables

**Variables** tab → add each. These are secrets; they are deliberately not in
the repo.

| Variable | Where it comes from |
|---|---|
| `OWLS_API_KEY` | https://owlsinsight.com/dashboard/keys |
| `DISCORD_TOKEN` | Discord Developer Portal → your app → Bot |
| `DISCORD_ALERT_CHANNEL_ID` | right-click `#ev-feed` → Copy Channel ID |
| `DISCORD_OPS_CHANNEL_ID` | right-click `#health-ops` → Copy Channel ID |

Copy them from your local `.env`. The service **fails fast at startup** naming
any variable that is missing or malformed, so a typo shows up immediately in the
deploy logs rather than as mysterious silence.

## 3. Confirm it is alive

Watch the **Deploy Logs**. Expect JSON lines, in this order:

```
starting                 sports=[...] books=[...] min_ev_pct=2.5
gating refreshed         active={...}
seeded board silently    sport=... (one per active sport)
poll complete            polled=N published=0 ...
```

`published=0` on the first cycle is **correct** — see cold start below.

Then confirm the heartbeat arrives in `#health-ops`. That is the real proof:
it means the Owls API, the Discord token and the channel permissions all work
from Railway, not just from your Mac.

---

## Things that behave differently on Railway

### Cold start on every deploy

Railway redeploys on every push to the watched branch, and each restart
re-seeds: the bot records the current board **silently** and publishes nothing
until the next genuine crossing. That is the design working as intended, not a
fault — it exists so a restart never dumps the whole live board into the alert
channel.

The consequence to plan around: **a burst of pushes means a string of silent
windows.** If you are mid-slate and want alerts flowing, avoid redeploying.

### Config hot-reload does nothing here

[`config/default.toml`](../config/default.toml) ships inside the image and the
filesystem is ephemeral, so there is no file to edit at runtime and the watcher
has nothing to observe. On Railway, tune criteria with **environment
variables** instead — they override the file:

| Variable | Overrides |
|---|---|
| `EVBOT_MIN_EV_PCT` | the EV threshold |
| `EVBOT_POLL_INTERVAL_SECS` | poll cadence (floor: 5) |
| `EVBOT_HEARTBEAT_SECS` | heartbeat cadence (floor: 60) |
| `EVBOT_LOOKAHEAD_HOURS` | gating window |

Changing a Railway variable triggers a redeploy, which means a cold start. So
retuning a threshold costs you one silent window. Fine occasionally; not
something to fiddle with during a slate.

### Never raise the replica count

`numReplicas = 1`, enforced in `railway.toml`. Suppression state is in memory
per process, so a second replica is a second bot with an empty suppressor: it
would republish everything the first had already sent **and** burn a second copy
of the monthly quota. Horizontal scaling is always wrong for this service.

### No healthcheck, and that is expected

Railway may note the absence of one. Phase 1 binds no port, so there is nothing
to check. Absence of a heartbeat in `#health-ops` is the liveness signal.
Phase 2 adds `GET /healthz` along with the webhook receiver.

---

## Cost

Hobby plan is **$5/month including $5 of usage credit**. Measured locally, the
service peaks at **11.2 MB RAM** and near-zero CPU, so real consumption sits far
under that credit — expect the $5 floor rather than $5 plus usage.

For context, that is roughly 3% of the Owls Insight MVP + WebSocket
subscription this bot depends on.

---

## Rolling back

Railway keeps previous deploys. **Deployments** tab → pick an earlier one →
**Redeploy**. Faster than reverting a commit, and worth knowing before you need
it rather than during.
