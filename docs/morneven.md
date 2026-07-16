# Morneven ZeroClaw Operations

## Deployment Contract

Morneven uses ZeroClaw as the only active Bot Manager runtime service. The
production volume must be mounted at:

```text
/zeroclaw-data/data
```

The runtime root is:

```text
/zeroclaw-data/data/morneven
```

Required ZeroClaw service variables:

```dotenv
ZEROCLAW_DATA_DIR=/zeroclaw-data/data
MORNEVEN_ZEROCLAW_ROOT=/zeroclaw-data/data/morneven
MORNEVEN_PRODUCTION_HARDENING=true
MORNEVEN_WEB_AUTH_ENABLED=true
MORNEVEN_WEB_SESSION_SECRET=<unique secret, minimum 32 characters>
MORNEVEN_WEB_SESSION_TTL_SECONDS=14400
MORNEVEN_RELOAD_TOKEN=<unique secret, minimum 16 characters>
MORNEVEN_BACKEND_INTERNAL_URL=http://<backend-private-domain>:8080
MORNEVEN_BOT_MANAGER_SYNC_TOKEN=<unique secret, minimum 16 characters>
```

The session secret, reload token, and sync token must be different. Production
startup fails before the gateway binds a port when auth, secrets, backend URL,
or data paths are invalid.

Backend variables that point to this service:

```dotenv
ZEROCLAW_INTERNAL_BASE_URL=http://<zeroclaw-private-domain>:8080
ZEROCLAW_MORNEVEN_RELOAD_TOKEN=<same value as MORNEVEN_RELOAD_TOKEN>
BOT_MANAGER_SYNC_TOKEN=<same value as MORNEVEN_BOT_MANAGER_SYNC_TOKEN>
```

## Persistent Layout

The volume contains current operational data only:

```text
/zeroclaw-data/data/
  morneven/
    gateway-desired-state.json
    morneven.log
    morneven.log.1
    morneven.log.2
    morneven.log.3
    provider-usage.jsonl
    runtime-state.json
    runtimes/
      <personality-slug>-<identity-prefix>/
        .morneven-runtime-manifest.json
        config.json
        config.toml
        gateway.log
        gateway.log.1
        gateway.log.2
        gateway.pid
        telegram-topics.json
        workspace/
        data/
```

Parent logs rotate at 2 MiB with three archives. Personality gateway logs
rotate at 1 MiB with two archives. A sync stops removed personalities before
deleting runtime directories that are no longer present in the backend bundle.

## Runtime Sync

The backend serves the current runtime bundle through the internal Bot Manager
API. ZeroClaw authenticates with `MORNEVEN_BOT_MANAGER_SYNC_TOKEN`, writes the
materialized files, removes files that disappeared from the manifest, and
preserves the requested start or stop state for personalities that still exist.

Runtime actions are available per personality and globally:

- `start`
- `stop`
- `restart`
- bundle sync
- bundle sync followed by restart

Scheduled start and stop definitions live in the backend database. When start
and stop are due at the same instant, stop wins. The global runtime freeze also
lives in the backend and blocks manual or scheduled starts until it is removed.

## Backup And Restore

Backups are created by the backend Data Extraction feature. The full archive
uses `morneven-zeroclaw-backup/v1` and includes:

- a manifest with SHA-256 checksums
- backend datasets
- storage objects
- encrypted Bot Manager identities and credentials
- the current ZeroClaw runtime bundle
- schedule definitions
- runtime control state

An archive never embeds previous backup archives. Restore validates the schema
and every checksum before import. Restored schedules remain disabled and all
restored runtimes remain stopped until an Author reviews and enables them.

The default retention is three backups for seven days. Cleanup begins when
managed backup storage reaches 350 MiB. A new backup is rejected when projected
usage exceeds 450 MiB after cleanup.

## Redeployment Check

Use the following check for every image or configuration change:

1. Record the byte count and file list under `/zeroclaw-data/data`.
2. Redeploy the same revision three times without creating new runtime data.
3. Sync the Bot Manager bundle after each deployment.
4. Confirm that runtime directory count remains equal to personality count.
5. Confirm that log archives remain within the configured rotation limits.
6. Confirm that total storage does not grow from duplicate materialization.
7. Create a full backup and validate its manifest and checksums.
8. Run a restore or materialization dry-run before treating the backup as valid.

Example inspection commands inside a shell-equipped image:

```sh
du -h -d 3 /zeroclaw-data/data
find /zeroclaw-data/data/morneven/runtimes -maxdepth 2 -type f -print
```

The production image is distroless. Use the platform volume browser, a
temporary shell-equipped maintenance image attached to the same volume, or the
backend backup manifest for inspection.

## Security Verification

Before production deployment:

1. Confirm the public dashboard requires Morneven authentication.
2. Confirm direct requests without a valid web session or reload token fail.
3. Confirm the backend is reached through its private URL.
4. Confirm the three production secrets are unique and stored only as platform
   secrets.
5. Confirm the volume is mounted exactly at `/zeroclaw-data/data`.
6. Confirm no executable or archive from user upload storage is mounted into
   the ZeroClaw runtime.
7. Run `cargo fmt --all -- --check`, Clippy, tests, and `cargo deny check`.

## Maintainer Note

The Morneven Railway deployment target is the Linux container built by the
repository `Dockerfile`. Full native Windows compatibility is not part of the
current release scope. Some upstream workspace tests assume Unix tools such as
`sh`, `sleep`, `env`, and POSIX path syntax, so a full test run from Windows can
report platform-only failures even when the Linux deployment path is healthy.

Future maintainers who need native Windows support should add a dedicated
Windows test job, replace Unix-only test fixtures with platform-aware helpers,
and verify shell, skill test, cron, tunnel, attachment path, and subprocess
behavior before declaring Windows supported. Linux CI or a Linux container
remains the release gate for Railway.

## Manual Shutdown

The owner performs the final shutdown manually:

1. Enable global runtime freeze in Bot Manager.
2. Confirm every personality reports `stopped`.
3. Create and verify a final full backup.
4. Disable scheduled backups and runtime schedules if the environment will stay
   offline for an extended period.
5. Stop the ZeroClaw service.
6. Stop the backend and frontend services.
7. Keep the persistent volume and repository available for a future restart.
