# syarc-console

Rust rewrite of the fastmain control console: authentication, sessions,
authorization, and the app-plugin host. Apps live as workspace crates under
`apps/` and are linked in statically via the compile-time registry.

## Layout

- `crates/plugin-api` — the app SDK: `AppPlugin` trait, `AppManifest`,
  shared `AppState` (db + templates), `RequireUser`/`RequireAdmin` extractors,
  argon2 password helpers.
- `crates/core` — the host binary: axum server, SQLite migrations, session
  layer, login/register/dashboard, app seeding + reconciliation, and
  `registry::all_apps()` (the compile-time plugin list).
- `apps/admin` — Administration app (users, categories, settings, system).
- `apps/platforms` — Messaging + Mesh Bridge + Sage bot (port of
  fastmain's `platforms` plugin; see below).

## Platforms porting status

Ported from `fastmain/app/apps/platforms`:

- **Schema** — conversations/participants/messages/read-statuses,
  mesh_nodes, mesh_bridge_mappings, mesh_ingested_frames,
  mesh_user_links, mesh_directory, mesh_traffic (SQLite, INTEGER user
  ids instead of Postgres UUIDs).
- **Messaging web layer** — inbox, compose (auto-bridges mesh
  recipients), conversation view, reply, delete message/thread,
  messages-since API, unread-count API, SSE live stream (single
  process-wide broadcast hub replaces the Python per-worker queues +
  DB safety poll).
- **Broker client** — WebSocket connect/hello/resume/welcome,
  subscribe sync, register_names, frame ingest with idempotent
  `mesh_ingested_frames` claim + `messages.mesh_frame_id` dedup,
  To:/sender-prefix routing, room vs DM mappings, sole-DM fallback,
  catch-all room for unmapped DMs, auto-provisioned mesh users,
  directory mirror, DM/chan sends with acks + REST fallback,
  push-on-post + watermark catch-up, traffic log, stale-channel-replay
  guard (ULID age).
- **Sage** — single-question queue worker with per-sender cooldown,
  OpenAI-compatible chat with tool calls (NWS weather/alerts,
  gazetteer place facts, distance), LoRa truncation (configurable via
  `sage_max_chars`, default 120 — MeshCore rejects DMs over 160 bytes
  and name/prefix overhead eats into that), tools-off degradation,
  reply via channel send.
- **Direct MeshCore radio** (`apps/platforms/src/radio.rs`) — a native
  implementation of the MeshCore companion-radio protocol (the same
  wire protocol the phone apps and the Python `meshcore` library
  speak): TCP framing `0x3c/0x3e + u16le len`, APP_START handshake,
  clock sync, channel-name discovery, contact-directory sync, pushed +
  polled (SYNC_NEXT_MESSAGE) inbound DMs/channel traffic, and outbound
  DM/channel sends. Enabled with `mesh_radio_enabled=1` +
  `mesh_radio_host=host:4000` (or `MESH_RADIO_HOST`); a device path
  instead of a host (e.g. `mesh_radio_host=/dev/ttyACM0`) attaches
  directly over USB serial — no TCP bridge needed. It replaces the
  Python broker entirely — the console participates in the mesh
  directly. Inbound traffic feeds the same ingest pipeline as the
  broker client. `test/fake_radio.py` is a fake companion radio for
  end-to-end testing.
- **Broker server** (`apps/platforms/src/broker_server.rs`) — when this
  console owns the radio, it can share the link with other consoles:
  `mesh_broker_serve_enabled=1` exposes the Python-broker-compatible WS
  endpoint at `/mesh/ws` (auth via `mesh_broker_token`). Other consoles
  (e.g. a development instance) simply set
  `mesh_broker_url=http://<radio-owner>:8090/mesh` — no other changes.
  Channel traffic fans out to every subscribed console; DMs go only to
  consoles subscribed to that node (source tracking); outbound dm/chan
  frames are forwarded to the radio with acks; the contact directory is
  pushed to all clients. `PORT=<n>` selects the listen port (default
  8090).
- **MeshCore app** (`apps/platforms/src/meshcore.rs`) — a phone-app-like
  view at `/platforms/meshcore/`: hub (radio status + links) with
  **Contacts** (mesh directory, capped at the 350 most-recently-synced
  entries), **Channels** (channels you follow, with follow/unfollow;
  channel 0 "Public" is always followed), the Cascadia mesh map
  (<https://cascadiamesh.org/map/>), and per-channel traffic views with a
  send box. Channel sends go through whichever transport is active
  (direct radio or WS broker) and are logged in `mesh_traffic`. Followed
  channels live in the `mesh_followed_channels` table.
- **Gazetteer** — all five JSON datasets compiled in.

Not yet ported (stubs/omissions): `Link:` remote-access commands,
cross-instance `name@instance` client pseudo-nodes (Phase 3),
Meshtastic node mirroring (`mt_nodes`), router admin poll/clock-sync,
JSONL files-mode storage, sage deterministic fast-path + weather
pre-cache, forums. Router nesting: plugins register full paths and are
`merge`d (not `nest`ed) so trailing-slash URLs match exactly.

## Run

    cd /usr/local/dev_apps/syarc-console
    DATABASE_URL="sqlite:///usr/local/dev_apps/syarc-console/syarc-console.db?mode=rwc" \
        cargo run

Listens on http://0.0.0.0:8090. The first registered user becomes admin.

## Adding an app

1. `cargo new --lib apps/<name>` and add it to workspace members.
2. Implement `AppPlugin` (manifest + router; see `apps/admin`).
3. Add one line to `crates/core/src/registry.rs::all_apps()`.
4. Create `apps/<name>/src/templates/*.html` (auto-loaded, extends "base.html").

Only the changed app crate recompiles plus a quick final link.

## Notes

- Sessions currently use an in-memory store (`MemoryStore`); swap for
  `tower-sessions-sqlx-store` before running multiple processes.
- Templates are loaded from source dirs at startup (paths relative to the
  workspace root — run from the repo root).

## The syarc ecosystem

syarc-console is one component of a small set of self-hostable applications
designed to let an individual or a community operate independently and
securely on their own hardware and networks:

- **[bearden-bridge](https://github.com/syarc-oss/bearden-bridge)** — bridges
  a MeshCore LoRa room-server radio to Matrix/XMPP chat, relays
  store-and-forward tunnels between distant meshes, and carries Open TAK
  voice/photo media over a second LoRa profile.
- **syarc-console** (this project) — the web console: accounts, app
  permissions, messaging, library/media, calendar/tasks, pecuniary
  (encrypted financial ledgers), surveillance, knowledge base, and mesh
  platform tooling.

Each app is standalone and speaks open protocols (Matrix, XMPP, HTTP), so
deployments can be decentralized: run one console, or one per household,
federated over chat and radio with no cloud dependency. Sensitive data
(e.g. financial ledgers via SQLCipher) is encrypted at rest, and the
console supports LiteFS-based primary/replica failover.

## Acknowledgements

Significant portions of this codebase were developed with the Z.AI GLM
large language model acting as an autonomous coding agent (Crush CLI),
under human direction and review.
