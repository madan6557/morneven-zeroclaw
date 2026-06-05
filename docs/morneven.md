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

## Bot Manager Parity Target

The first compatibility target is current Bot Manager parity:

- multi active personality runtime
- per-personality provider assignment
- per-personality Telegram channel config
- Telegram topic lock and primary topic routing
- runtime start, stop, restart, status, and logs
- provider usage export for local analytics
- restart restore after Railway restarts

