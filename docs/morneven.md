# Morneven ZeroClaw Integration

This repository is a Morneven integration fork of ZeroClaw. It is not
affiliated with, endorsed by, or maintained by ZeroClaw Labs.

## Branch Policy

- `development` is the active Morneven integration branch.
- `upstream` should point to `https://github.com/zeroclaw-labs/zeroclaw.git`.
- `origin` should point to `https://github.com/madan6557/morneven-zeroclaw.git`.
- Do not rewrite `main` unless Morneven explicitly asks for that.

## Railway Target

The Morneven runtime target is Railway using private networking. Bot Manager can
continue using its existing Nanobot-named environment variables while they point
to this service:

```text
NANOBOT_INTERNAL_BASE_URL=http://<railway-private-domain>:<port>
NANOBOT_MORNEVEN_RELOAD_TOKEN=<shared secret>
```

Railway private networking must use `http://<private-domain>:<port>`, not
`https://`.

Current production volume mount path:

```text
/zeroclaw-data/data
```

The runtime root for Morneven identities is:

```text
/zeroclaw-data/data/morneven
```

The Docker image sets these defaults:

```text
ZEROCLAW_DATA_DIR=/zeroclaw-data/data
MORNEVEN_ZEROCLAW_ROOT=/zeroclaw-data/data/morneven
```

For one-time Nanobot workspace inheritance, Bot Manager can also point to the
old Nanobot service while the primary URL points to ZeroClaw:

```text
NANOBOT_LEGACY_INTERNAL_BASE_URL=http://<old-nanobot-private-domain>:8080
NANOBOT_LEGACY_MORNEVEN_RELOAD_TOKEN=<optional old shared secret>
```

If the old Nanobot volume is copied into the ZeroClaw service instead, set one
of these on ZeroClaw:

```text
MORNEVEN_NANOBOT_LEGACY_ROOT=/data/.nanobot
NANOBOT_LEGACY_ROOT=/data/.nanobot
```

The importer reads `runtimes/<slug>-<identity8>/workspace`, maps Nanobot root
files to ZeroClaw canonical names, archives old `sessions/` under
`legacy/nanobot/sessions/`, and lets explicit Bot Manager files override legacy
content.

Legacy data under `legacy/nanobot/**` is migration input only. Morneven backup
and extraction jobs must not package it again as active ZeroClaw workspace data.
Likewise, generated artifacts under `backups/**` and `bot-manager/backups/**`
must be excluded from new backups to avoid recursive storage growth.

## Compatibility Contract

The fork exposes the same protected Morneven API shape that Bot Manager already
uses:

- `GET /api/morneven/status`
- `POST /api/morneven/reload`
- `GET /api/morneven/config-secrets`
- `GET /api/morneven/workspace/changes`
- `GET /api/morneven/telegram/topics`
- `GET /api/morneven/provider-usage`
- `POST /api/morneven/gateway/start`
- `POST /api/morneven/gateway/stop`
- `POST /api/morneven/gateway/restart`
- `POST /api/morneven/runtimes/:identity_id/gateway/:action`

Every endpoint requires `x-morneven-reload-token` to match
`MORNEVEN_RELOAD_TOKEN` or `NANOBOT_MORNEVEN_RELOAD_TOKEN`.

`POST /api/morneven/reload` pulls the Bot Manager runtime bundle from
`MORNEVEN_BACKEND_INTERNAL_URL`, `MORNEVEN_BACKEND_PUBLIC_URL`, or the existing
Nanobot backend URL variables. The sync request uses
`MORNEVEN_BOT_MANAGER_SYNC_TOKEN` or `BOT_MANAGER_SYNC_TOKEN`.

## Bot Manager Parity Target

The first compatibility target is current Bot Manager parity:

- multi active personality runtime
- per-personality provider assignment
- per-personality Telegram channel config
- Telegram topic lock and primary topic routing
- runtime start, stop, restart, status, and logs
- provider usage export for local analytics
- restart restore after Railway restarts

## Runtime Behavior

Bot Manager identities are materialized as separate ZeroClaw runtime directories
under `MORNEVEN_ZEROCLAW_ROOT` or `ZEROCLAW_MORNEVEN_ROOT`. If neither is set,
the fork uses `~/.zeroclaw/morneven`.

On the current Railway image, this resolves to `/zeroclaw-data/data/morneven`.
Do not use `/data` as the active mount path for the current deployment.

The parent gateway persists desired runtime state. On Railway restart it restores
any runtime that was marked running. Child runtimes are started with
`MORNEVEN_CHILD_RUNTIME=1`, so they do not recursively restore or spawn other
runtimes.

## Telegram Topic Lock

The Telegram channel reads `telegram-topics.json` from each runtime directory.
It records observed groups and topics, drops inbound messages from locked
topics, blocks outbound messages to forbidden explicit topics, and redirects
main-topic outbound system messages to the configured primary topic when one is
available.

## Provider Usage Export

`GET /api/morneven/provider-usage` normalizes ZeroClaw cost records from
`data/state/costs.jsonl` into the Bot Manager usage event shape, with fallbacks
for older runtime usage files. It includes provider, model, runtime identity,
prompt tokens, completion tokens, cached tokens, total tokens, request count,
timestamp, and cost when ZeroClaw has a non-zero cost record. If cost is zero
because provider pricing is unavailable, Bot Manager can still estimate usage
cost from the token fields.
