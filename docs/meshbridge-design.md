# Bear Den Bridge — Design Draft

Status: draft for discussion. Nothing here is implemented yet.

## Goal

A standalone daemon on Wizard that bridges the Bear Den MeshCore **room
server** (Heltec V4, serial-attached, `/dev/bearden`) to internet messaging.
Anyone on the mesh can join the room natively; anyone on the internet can
join via any enabled backend protocol. No per-user mapping into
syarc-console — the daemon is the hub, the room is the room.

## Survey: what already exists (meshcore.ninja, Oct 2026)

Closest existing work, and what we'd take from each:

| Project | What it does | Takeaway for us |
|---|---|---|
| MeshCore Discord Bridge | bidirectional MeshCore↔Discord, multi-channel routing, chunking, flood protection | Proves the pattern; borrow its flood protection + chunking ideas. Discord-only, not self-hosted federation. |
| meshcore-pi | full Python MeshCore stack on Linux + SPI SX1262; can act as companion / room server / repeater | **Key alternative**: a *software* room server with our own keys — per-internet-user identities without custom firmware. Evaluate first. |
| openHop Core | Python protocol reimplementation, SPI/KISS modems | Same idea as meshcore-pi; fallback option. |
| Spectra | Rust bridge MeshCore/Meshtastic ↔ Veilid DHT | Shows a Rust bridge daemon shape; Veilid is one possible P2P backend. |
| mesh-client | desktop client for Meshtastic, MeshCore, and Reticulum/LXMF | Reticulum/LXMF is a serious P2P backend candidate. |
| MeshCore MQTT Gateway / meshcoretomqtt | LoRa↔MQTT plumbing | MQTT is a fine internal telemetry bus, not a user-facing chat backend. |
| MeshCore Email Gateway | SMTP/IMAP ↔ mesh DMs | Confirms gateway pattern; email = async, LoRa-friendly. |
| MCSim / meshcore_sim | simulators incl. room servers | Use for bridge testing without burning airtime. |

No existing project bridges a MeshCore *room server* to an open federated
chat protocol. We'd be first, but the pieces all exist.

## Topology

```
 mesh phones/repeaters ──LoRa──>  Bear Den (room server, /dev/bearden)
                                       │ serial
                                       ▼
                              bearden-bridge daemon (Wizard)
                                       │ policy / queue / identity
        ┌──────────────┬───────────────┼────────────────┐
        ▼              ▼               ▼                ▼
   XMPP MUC        Matrix room      Nostr        (Discord / email /
   (Prosody)       (conduwuit)      relays        MQTT / webhook …)
```

Multiple backends run concurrently: mesh→internet fans out to all enabled
backends; each backend is an independent egress source to the mesh.

## Major decision: where the room server lives

**Option A — Bear Den firmware is the room server (current).**
Serial CLI only (`advert`, `password`, ACL via `setperm`). Two gaps:
1. **No observe path**: room traffic only reaches serial via `log start`
   (human-formatted text).
2. **No inject path**: no serial command makes the room emit a message.
Both need firmware work: a small machine-readable protocol next to the
CLI (JSON lines or `0x3c`-style framing) with events
(`msg`, `join`, `left`) and commands (`send`, `grant`, `kick`, `list`).
User has stated firmware work on a serial board is acceptable.

**Option B — software room server via meshcore-pi + SPI SX1262 hat.**
The daemon owns the room keys in software: it *is* the hub, so observe +
inject are free, and it can mint per-internet-user secondary keys so each
internet user appears as a real room member (native identity, DMs work).
Cost: a new hardware hat (or repurposing a board), Python stack, and
ownership of protocol-level correctness.

**Recommendation: design the daemon against an adapter interface so A and
B are interchangeable; start with A (hardware already works, firmware diff
is small and reviewable), keep B as the upgrade path to per-user keys.**

## Daemon architecture

```
bearden-bridge/
  radio/      RoomAdapter trait (serial impl for Bear Den firmware)
  policy/     approvals, quotas, airtime governor, clipping, dedupe
  ident/      internet-user registry, key/alias mapping (SQLite)
  backends/   Backend trait + impls: xmpp, matrix, nostr, discord, …
  ctl/        admin API (HTTP) + minimal status page
  main.rs     wiring, supervision
```

### RoomAdapter (mesh side)

```rust
enum RoomEvent {
    Message { sender_key: String, sender_name: String, text: String, ts: u64 },
    ClientJoined { key: String, name: String },
    ClientLeft  { key: String },
    Status { clients: Vec<ClientInfo>, airtime_pct: f32 },
}
enum RoomCommand {
    Send { text: String, from: Option<UserId> },   // from=None: bridge identity
    Grant { name: String }, Kick { key: String },
    SetPassword(String), List,
}
```

Phase 1 semantics for `Send`: the room server transmits attributed as
`name: text` using a single bridge identity (same convention the console
uses today). Phase 2 (firmware or Option B): per-user secondary keys.

### Backend (internet side)

```rust
trait Backend {
    fn name(&self) -> &str;
    async fn run(&self, ctx: BackendCtx) -> Result<(), Error>;
}
// BackendCtx provides: inbound (mesh→internet) event stream,
// outbound submit channel (internet→mesh, already policy-filtered),
// identity lookup, and admin commands.
```

Backends are independent tasks; one failing or being rate-limited never
blocks the others. Adding a backend = one new module + config stanza.

### Policy engine (runs before anything touches RF)

- **Approve-before-transmit**: internet users must be approved by an
  admin before their first RF message (registry state: pending/approved/
  banned). Per-backend opt-in to auto-approve.
- **Quotas**: per-user messages/hour and bytes/hour; global room cap.
- **Airtime governor**: outbound queue with priorities
  (bridge control > replies/DMs > general chat); drops/defers lowest
  priority when the queue exceeds a delay budget (e.g. 60s).
- **Clipping**: hard cap at MeshCore packet length (~200 chars), no
  attachments/markdown passthrough — plain text only.
- **Dedupe/loop guard**: content hash + origin tag; anything the bridge
  itself emitted is never re-injected (prevents mesh→backend→mesh loops
  when multiple backends or other bridges are in the room).

### Identity registry (SQLite)

`users(id, backend, backend_uid, display_name, state, first_seen,
approved_by)`, later `mesh_secondary_key`. Shared across backends so one
human can be approved once.

### Control plane

Small HTTP API + status page: queue depth, airtime, per-backend health,
approve/ban users, kill switch (pauses all RF egress). Can later be
surfaced inside syarc-console as a plugin calling this API.

## Backend shortlist (priority order)

1. **XMPP MUC** (Prosody on Wizard) — lightest, proven bridging target,
   room model matches, server-side moderation.
2. **Matrix** (conduwuit + appservice or mautrix-style bridge) — best
   clients/federation, heavier.
3. **Nostr** — trivially light relay (can run on Wizard), pubkey identity
   dovetails with future per-user mesh keys.
4. Stretch: Reticulum/LXMF (true P2P, see mesh-client), Discord (pattern
   already proven upstream), email (async, LoRa-friendly).

## Build order

1. **Firmware**: add the event/inject serial protocol to Bear Den
   (alongside the existing CLI, don't break it). Test with `screen` +
   MCSim-less live room (phones already joined).
2. **Daemon skeleton**: RoomAdapter(serial) + policy + one backend
   (XMPP), SQLite registry, admin API with kill switch.
3. **Soak test**: run with real mesh traffic; tune governor.
4. Additional backends (Matrix, Nostr) behind the same trait.
5. Option B evaluation: meshcore-pi software room server for per-user
   identities; swap RoomAdapter, nothing else changes.

## Open questions

- Does the stock firmware's `log` output carry enough structure to parse
  sender identity per message, or does the firmware patch need to emit
  structured events from the start? (Check `log` output format first —
  it may unblock Phase 1 without any firmware change on the observe side.)
- Max concurrent room clients on the Heltec (MAX_CLIENTS) vs expected
  internet-user count; eviction policy for idle internet users.
- Whether sage should eventually move into the daemon (it is currently a
  console feature keyed to channels).
