# AGENTS.md

Rust workspace porting the Python `fastmain` console: an axum + SQLite app
plugin host. Apps are workspace crates under `apps/`, linked statically via a
compile-time registry in `crates/core/src/registry.rs::all_apps()`.

## Commands

```bash
# Run (from repo root — templates are loaded from source dirs at startup,
# paths are relative to the workspace root)
DATABASE_URL="sqlite://$PWD/syarc-console.db?mode=rwc" cargo run
# Listens on 0.0.0.0:8090 (override with PORT). First registered user becomes admin.

cargo build                  # dev build
cargo build --release        # release build used by deployment/toprod.sh
cargo test                   # no unit tests exist in the tree (as of writing)
deployment/toprod.sh [host]  # build release, deploy via ssh/scp to prox-lxc LXCs, mirror backup
```

## Adding a new app (the core extension pattern)

1. `cargo new --lib apps/<name>`, add to workspace members in root `Cargo.toml`.
2. Implement `AppPlugin` (manifest + router); see `apps/admin` or `apps/library`.
3. Add one line to `crates/core/src/registry.rs::all_apps()`.
4. Create `apps/<name>/src/templates/*.html` (extends `base.html`).
5. **Gotcha**: `crates/plugin-api/src/templates.rs` hardcodes the template
   directory list — you must add `load_dir(&mut env, "apps/<name>/src/templates")`
   there too, or templates silently won't be found.
6. New schema goes in a new `crates/core/migrations/NNNN_<app>.sql` (sequential;
   embedded in the binary via `sqlx::migrate!`, run at startup).

## Architecture

- `crates/plugin-api` — the SDK: `AppPlugin` trait, `AppManifest` (builder-style
  const constructors), `AppState { db, templates }`, `SharedState = Arc<AppState>`,
  `RequireUser`/`RequireAdmin` extractors (redirect to login), argon2 password
  helpers, minijinja `Templates` (with a custom `urlencode` filter).
- `crates/core` — host binary: axum server, migrations, session layer
  (`tower_sessions` `MemoryStore`, 12h inactivity), login/register/dashboard,
  app seeding + reconciliation, compile-time plugin registry.
- `apps/*` — plugins: admin, platforms (messaging/mesh/Sage bot — see README for
  extensive porting notes), surveillance, supply, pecuniary, library.

### Routing gotcha

Plugins return **fully-qualified-path routers** that the host `merge`s (not
`nest`s) in `main.rs`. Register full paths like `/library/browse` inside each
plugin (note trailing-slash variants are registered explicitly, e.g.
`.route("/library", ...)` and `.route("/library/", ...)`). Sub-modules merge
their routers the same way.

### App lifecycle

At startup: migrations run → each plugin's `on_startup(db)` seeds data →
`seed_apps` upserts `apps` rows from manifests (preserving admin-assigned
categories) → `reconcile_apps` deactivates rows for removed plugins.
`on_user_registered` lets plugins grant per-user defaults after registration.

## Data model

SQLite via sqlx (runtime queries with `?` placeholders, `query_as`/`query_scalar`,
not compile-time macros). User ids are `INTEGER` (fastmain used Postgres UUIDs).
Migrations live only in `crates/core/migrations/`. The dev DB
`syarc-console.db` sits in the repo root.

## Porting / data migration from fastmain

`tools/migrate_<app>.py` copy data from the fastmain Postgres DB into SQLite,
remapping user UUIDs to console ids by username. Run with the fastmain venv's
Python (`asyncpg`), **after** the console has applied the corresponding
migration. Example:
`/usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_library.py`

## Testing helpers

`test/` contains Python harnesses, not Rust tests: `fake_radio.py` (fake
MeshCore companion radio for end-to-end radio testing), `mock_llm.py`,
`serial_bridge.py`.

## Deployment

`deployment/LXC.md` documents the prox-lxc1/prox-lxc2 LXC pair (LiteFS SQLite
replication, Consul primary election on server1, keepalived VIP). Release
binaries are cross-built on the dev box (no Rust toolchain on LXCs); migrations
are embedded, but **templates ship in the git tree** and are loaded from disk at
startup — `git archive HEAD` is what gets deployed, so uncommitted template
changes won't reach production via `toprod.sh`.

## Known limitations (from README)

- Sessions use in-memory `MemoryStore` — swap for `tower-sessions-sqlx-store`
  before running multiple processes.
- Templates load from source dirs relative to repo root; always run from the
  workspace root.

## Acknowledgements

Developed with the Z.AI GLM large language model as an autonomous coding
agent (Crush CLI), under human direction and review.
