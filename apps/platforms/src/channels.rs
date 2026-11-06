//! Channels: a MeshCore-app-style view of public group channels.
//!
//! Each channel (`#chanN`) is backed by a shared conversation with a
//! room mapping (owner NULL) to the channel pseudo-node — the same
//! machinery the bridge uses — so inbound channel traffic lands in the
//! conversation and replies go out over the radio with send_chan.

use axum::extract::Path;
use axum::extract::State;
use axum::response::{Html, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use sqlx::Row;
use sqlx::SqlitePool;

use syarc_plugin_api::{RequireUser, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/platforms/messaging/channels", get(channels_page))
        .route("/platforms/messaging/channels/", get(channels_page))
        .route("/platforms/messaging/channels/{idx}/open", post(open_channel))
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Channel index -> display name, merged from the live radio and any
/// channel pseudo-nodes already known to the bridge.
async fn known_channels(db: &SqlitePool) -> Vec<(u8, String)> {
    let mut out: Vec<(u8, String)> = Vec::new();
    if let Some(r) = crate::radio::radio() {
        if r.connected() {
            out = r.channels();
        }
    }
    if let Ok(rows) = sqlx::query(
        "SELECT node_id, long_name FROM mesh_nodes WHERE node_id LIKE '#chan%'",
    )
    .fetch_all(db)
    .await
    {
        for r in rows {
            let node_id: String = r.try_get::<String, _>("node_id").unwrap_or_default();
            if let Some(n) = node_id.strip_prefix("#chan").and_then(|n| n.parse::<u8>().ok()) {
                let name: String = r.try_get::<String, _>("long_name").unwrap_or_default();
                match out.iter_mut().find(|(i, _)| *i == n) {
                    Some(entry) => {
                        if entry.1.is_empty() && !name.is_empty() {
                            entry.1 = name;
                        }
                    }
                    None => out.push((n, name)),
                }
            }
        }
    }
    out.sort_by_key(|(i, _)| *i);
    out
}

async fn channels_page(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let uid = user.0.id;
    let mut channels: Vec<serde_json::Value> = Vec::new();
    for (idx, name) in known_channels(&state.db).await {
        let node_id = format!("#chan{idx}");
        // Existing room conversation for this channel, if any.
        let conv: Option<String> = sqlx::query_scalar(
            "SELECT m.conversation_id FROM mesh_bridge_mappings m \
             JOIN mesh_nodes n ON n.id = m.mesh_node_id \
             WHERE n.node_id = ? AND m.enabled = 1 AND m.owner_user_id IS NULL \
             ORDER BY m.created_at DESC LIMIT 1",
        )
        .bind(&node_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        let joined = match &conv {
            Some(c) => sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM conversation_participants WHERE conversation_id = ? AND user_id = ?",
            )
            .bind(c)
            .bind(uid)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0)
                > 0,
            None => false,
        };
        let last: Option<String> = sqlx::query_scalar(
            "SELECT created_at FROM mesh_traffic WHERE node_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&node_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        channels.push(json!({
            "idx": idx,
            "node_id": node_id,
            "name": if name.is_empty() { format!("chan{idx}") } else { name },
            "conversation_id": conv,
            "joined": joined,
            "last_activity": last,
        }));
    }
    Html(state.templates.render("channels.html", json!({ "channels": channels })))
}

/// Ensure the channel's shared conversation exists (creating node,
/// conversation, and room mapping as needed), join it, and jump there.
async fn open_channel(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(idx): Path<u8>,
) -> Redirect {
    let uid = user.0.id;
    let node_id = format!("#chan{idx}");
    let name = crate::radio::radio().map(|r| r.channel_name(idx)).unwrap_or_default();
    let name = if name.is_empty() { format!("chan{idx}") } else { name };

    // Channel pseudo-node (also auto-created on first traffic; create it
    // here so the mapping can exist before any traffic arrives).
    let node_pk: Option<String> = sqlx::query_scalar("SELECT id FROM mesh_nodes WHERE node_id = ?")
        .bind(&node_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let node_pk = match node_pk {
        Some(pk) => pk,
        None => {
            let pk = new_id();
            let short: String = name.chars().take(16).collect();
            let long: String = name.chars().take(64).collect();
            let _ = sqlx::query(
                "INSERT INTO mesh_nodes (id, node_id, protocol, short_name, long_name, bound_user_id) \
                 VALUES (?, ?, 'meshcore', ?, ?, 1)",
            )
            .bind(&pk)
            .bind(&node_id)
            .bind(&short)
            .bind(&long)
            .execute(&state.db)
            .await;
            pk
        }
    };

    // Existing room conversation?
    let conv: Option<String> = sqlx::query_scalar(
        "SELECT m.conversation_id FROM mesh_bridge_mappings m \
         WHERE m.mesh_node_id = ? AND m.enabled = 1 AND m.owner_user_id IS NULL \
         ORDER BY m.created_at DESC LIMIT 1",
    )
    .bind(&node_pk)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let conv = match conv {
        Some(c) => c,
        None => {
            let c = new_id();
            let ts = now();
            let _ = sqlx::query(
                "INSERT INTO conversations (id, subject, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&c)
            .bind(format!("#chan{idx} {name}"))
            .bind(uid)
            .bind(&ts)
            .bind(&ts)
            .execute(&state.db)
            .await;
            let _ = sqlx::query(
                "INSERT INTO mesh_bridge_mappings (id, conversation_id, mesh_node_id) VALUES (?, ?, ?)",
            )
            .bind(new_id())
            .bind(&c)
            .bind(&node_pk)
            .execute(&state.db)
            .await;
            if let Some(b) = crate::broker::broker() {
                let _ = b.sync_subscriptions().await;
            }
            c
        }
    };

    // Join if not already a participant.
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)",
    )
    .bind(new_id())
    .bind(&conv)
    .bind(uid)
    .execute(&state.db)
    .await;

    Redirect::to(&format!("/platforms/messaging/conversations/{conv}"))
}
