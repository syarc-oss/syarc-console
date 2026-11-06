//! Messaging: inbox, compose, conversations, reply, delete, live SSE.

use std::convert::Infallible;

use axum::extract::{Form, Path, Query, RawForm, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::hub::Hub;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/platforms/messaging", get(inbox))
        .route("/platforms/messaging/", get(inbox))
        .route("/platforms/messaging/compose", get(compose_page).post(send_message))
        .route("/platforms/messaging/groups/new", get(group_new_page).post(group_create))
        .route("/platforms/messaging/groups/{id}/join", post(group_join))
        .route("/platforms/messaging/conversations/{id}", get(view_conversation))
        .route("/platforms/messaging/conversations/{id}/api/messages", get(messages_api))
        .route("/platforms/messaging/conversations/{id}/reply", post(reply))
        .route("/platforms/messaging/conversations/{id}/delete", post(delete_conversation))
        .route("/platforms/messaging/conversations/{id}/messages/{msg_id}/delete", post(delete_message))
        .route("/platforms/messaging/conversations/{id}/events", get(conversation_events))
        .route("/platforms/messaging/api/unread-count", get(unread_count_api))
}

pub(crate) fn new_id() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

async fn is_participant(db: &sqlx::SqlitePool, conv_id: &str, user_id: i64) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM conversation_participants WHERE conversation_id = ? AND user_id = ?",
    )
    .bind(conv_id)
    .bind(user_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some()
}

async fn unread_count(db: &sqlx::SqlitePool, user_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages m \
         JOIN conversation_participants p ON p.conversation_id = m.conversation_id \
         LEFT JOIN message_read_statuses r ON r.message_id = m.id AND r.user_id = ? \
         WHERE p.user_id = ? AND m.sender_id != ? AND r.id IS NULL",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(user_id)
    .fetch_one(db)
    .await
    .unwrap_or(0)
}

async fn inbox(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let uid = user.0.id;
    let rows = sqlx::query(
        "SELECT c.id, c.subject, c.updated_at FROM conversations c \
         JOIN conversation_participants p ON p.conversation_id = c.id \
         WHERE p.user_id = ? AND c.kind = 'dm' ORDER BY c.updated_at DESC",
    )
    .bind(uid)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut conversations: Vec<serde_json::Value> = Vec::new();
    for r in rows {
        let conv_id: String = r.get("id");
        let subject: String = r.get("subject");
        let updated: String = r.get("updated_at");
        let last: Option<(String, String)> = sqlx::query(
            "SELECT m.content, u.username FROM messages m \
             JOIN users u ON u.id = m.sender_id \
             WHERE m.conversation_id = ? ORDER BY m.created_at DESC LIMIT 1",
        )
        .bind(&conv_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None)
        .map(|row| (row.get("content"), row.get("username")));
        let unread: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m \
             LEFT JOIN message_read_statuses r ON r.message_id = m.id AND r.user_id = ? \
             WHERE m.conversation_id = ? AND m.sender_id != ? AND r.id IS NULL",
        )
        .bind(uid)
        .bind(&conv_id)
        .bind(uid)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        let others: Vec<String> = sqlx::query_scalar(
            "SELECT u.username FROM conversation_participants p \
             JOIN users u ON u.id = p.user_id \
             WHERE p.conversation_id = ? AND p.user_id != ?",
        )
        .bind(&conv_id)
        .bind(uid)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        conversations.push(json!({
            "conversation": { "id": conv_id, "subject": subject, "updated_at": updated },
            "last_message": last.map(|(content, username)| json!({"content": content, "sender": {"username": username}})),
            "unread_count": unread,
            "other_participants": others.iter().map(|u| json!({"username": u})).collect::<Vec<_>>(),
        }));
    }
    let total: i64 = conversations.iter().map(|c| c["unread_count"].as_i64().unwrap_or(0)).sum();

    // Groups the user belongs to.
    let my_groups = conversation_summaries(
        &state.db,
        uid,
        "SELECT c.id FROM conversations c \
         JOIN conversation_participants p ON p.conversation_id = c.id \
         WHERE p.user_id = ? AND c.kind IN ('group', 'channel') ORDER BY c.updated_at DESC",
    )
    .await;
    // Public groups the user has not joined yet.
    let public_groups = conversation_summaries(
        &state.db,
        uid,
        "SELECT c.id FROM conversations c \
         WHERE c.kind = 'group' AND c.is_public = 1 AND c.id NOT IN \
         (SELECT conversation_id FROM conversation_participants WHERE user_id = ?) \
         ORDER BY c.updated_at DESC",
    )
    .await;
    // Mesh channels: conversations bridged to a #chan node.
    let mesh_channels = conversation_summaries(
        &state.db,
        uid,
        "SELECT DISTINCT c.id FROM conversations c \
         JOIN mesh_bridge_mappings m ON m.conversation_id = c.id \
         JOIN mesh_nodes n ON n.id = m.mesh_node_id \
         WHERE n.node_id LIKE '#chan%' AND m.enabled = 1 ORDER BY c.updated_at DESC",
    )
    .await;

    Html(state.templates.render(
        "inbox.html",
        json!({
            "user": { "id": uid, "username": user.0.username },
            "conversations": conversations,
            "my_groups": my_groups,
            "public_groups": public_groups,
            "mesh_channels": mesh_channels,
            "total_unread": total,
            "mesh_permitted": user.0.role == "admin",
        }),
    ))
}

/// Lightweight rows for the Groups section of the inbox.
async fn conversation_summaries(db: &sqlx::SqlitePool, uid: i64, sql: &str) -> Vec<serde_json::Value> {
    let ids: Vec<String> = sqlx::query_scalar(sql)
        .bind(uid)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    for cid in ids {
        let conv: Option<(String, i64, String)> = sqlx::query(
            "SELECT subject, is_public, updated_at FROM conversations WHERE id = ?",
        )
        .bind(&cid)
        .fetch_optional(db)
        .await
        .unwrap_or(None)
        .map(|r| (r.get("subject"), r.get("is_public"), r.get("updated_at")));
        let Some((subject, is_public, updated)) = conv else { continue };
        let members: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_participants WHERE conversation_id = ?",
        )
        .bind(&cid)
        .fetch_one(db)
        .await
        .unwrap_or(0);
        let last: Option<(String, String)> = sqlx::query(
            "SELECT m.content, u.username FROM messages m \
             JOIN users u ON u.id = m.sender_id \
             WHERE m.conversation_id = ? ORDER BY m.created_at DESC LIMIT 1",
        )
        .bind(&cid)
        .fetch_optional(db)
        .await
        .unwrap_or(None)
        .map(|row| (row.get("content"), row.get("username")));
        let node_id: Option<String> = sqlx::query_scalar(
            "SELECT n.node_id FROM mesh_bridge_mappings m \
             JOIN mesh_nodes n ON n.id = m.mesh_node_id \
             WHERE m.conversation_id = ? AND n.node_id LIKE '#chan%' LIMIT 1",
        )
        .bind(&cid)
        .fetch_optional(db)
        .await
        .unwrap_or(None);
        let joined: bool = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM conversation_participants WHERE conversation_id = ? AND user_id = ?",
        )
        .bind(&cid)
        .bind(uid)
        .fetch_one(db)
        .await
        .unwrap_or(0)
            > 0;
        out.push(json!({
            "id": cid,
            "subject": subject,
            "is_public": is_public == 1,
            "is_channel": node_id.is_some(),
            "chan_idx": node_id.as_deref().and_then(|n| n.strip_prefix("#chan")).and_then(|n| n.parse::<u8>().ok()),
            "members": members,
            "joined": joined,
            "last_message": last.map(|(content, username)| json!({"content": content, "sender": username})),
            "updated_at": updated,
        }));
    }
    out
}

async fn compose_page(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let users: Vec<serde_json::Value> = sqlx::query(
        "SELECT id, username FROM users WHERE is_active = 1 AND id != ? ORDER BY username",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| json!({ "id": r.get::<i64, _>("id"), "username": r.get::<String, _>("username") }))
    .collect();
    // Mesh companion nodes appear as selectable recipients, labeled with
    // the node's long name instead of the synthetic mesh- user account.
    let mesh_names: serde_json::Map<String, serde_json::Value> = sqlx::query(
        "SELECT bound_user_id, long_name, node_id FROM mesh_nodes ORDER BY long_name",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| {
        let uid: Option<i64> = r.try_get("bound_user_id").ok()?;
        let uid = uid?;
        let long: String = r.get("long_name");
        let node_id: String = r.get("node_id");
        let label = if node_id.starts_with("#chan") { node_id.clone() } else { long };
        if label.is_empty() { None } else { Some((uid.to_string(), json!(label))) }
    })
    .collect();
    Html(state.templates.render(
        "compose.html",
        json!({ "all_users": users, "mesh_names": mesh_names }),
    ))
}

async fn group_new_page(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let users: Vec<serde_json::Value> = sqlx::query(
        "SELECT id, username FROM users WHERE is_active = 1 AND id != ? \
         AND username NOT LIKE 'mesh-%' ORDER BY username",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| json!({ "id": r.get::<i64, _>("id"), "username": r.get::<String, _>("username") }))
    .collect();
    Html(state.templates.render("group_new.html", json!({ "all_users": users })))
}

fn parse_group_form(raw: &[u8]) -> (String, String, Vec<i64>) {
    let mut name = String::new();
    let mut visibility = String::from("private");
    let mut member_ids = Vec::new();
    for (k, v) in form_urlencoded::parse(raw) {
        match k.as_ref() {
            "name" => name = v.into_owned(),
            "visibility" => visibility = v.into_owned(),
            "member_ids" => {
                if let Ok(id) = v.parse::<i64>() {
                    member_ids.push(id);
                }
            }
            _ => {}
        }
    }
    (name, visibility, member_ids)
}

async fn group_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> impl IntoResponse {
    let (name, visibility, member_ids) = parse_group_form(&raw);
    let uid = user.0.id;
    let name = name.trim();
    if name.is_empty() {
        return Redirect::to("/platforms/messaging/groups/new");
    }
    let is_public = if visibility == "public" { 1 } else { 0 };
    let conv_id = new_id();
    let ts = now();
    let mut tx = state.db.begin().await.unwrap();
    let _ = sqlx::query(
        "INSERT INTO conversations (id, subject, created_by, created_at, updated_at, kind, is_public) \
         VALUES (?, ?, ?, ?, ?, 'group', ?)",
    )
    .bind(&conv_id).bind(name).bind(uid).bind(&ts).bind(&ts).bind(is_public)
    .execute(&mut *tx).await;
    let _ = sqlx::query("INSERT INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)")
        .bind(new_id()).bind(&conv_id).bind(uid)
        .execute(&mut *tx).await;
    for mid in &member_ids {
        if *mid != uid {
            let _ = sqlx::query(
                "INSERT OR IGNORE INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)",
            )
            .bind(new_id()).bind(&conv_id).bind(mid)
            .execute(&mut *tx).await;
        }
    }
    let _ = tx.commit().await;
    Redirect::to(&format!("/platforms/messaging/conversations/{conv_id}"))
}

async fn group_join(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let uid = user.0.id;
    let is_public: Option<i64> = sqlx::query_scalar(
        "SELECT is_public FROM conversations WHERE id = ? AND kind = 'group'",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);
    if is_public == Some(1) {
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)",
        )
        .bind(new_id())
        .bind(&id)
        .bind(uid)
        .execute(&state.db)
        .await;
    }
    Redirect::to(&format!("/platforms/messaging/conversations/{id}"))
}

#[derive(Deserialize)]
pub struct ComposeForm {
    subject: String,
    content: String,
    #[serde(rename = "recipient_ids")]
    recipient_ids: Vec<String>,
}

fn parse_compose_form(raw: &[u8]) -> ComposeForm {
    let mut subject = String::new();
    let mut content = String::new();
    let mut recipient_ids = Vec::new();
    for (k, v) in form_urlencoded::parse(raw) {
        match k.as_ref() {
            "subject" => subject = v.into_owned(),
            "content" => content = v.into_owned(),
            "recipient_ids" => recipient_ids.push(v.into_owned()),
            _ => {}
        }
    }
    ComposeForm { subject, content, recipient_ids }
}

async fn send_message(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> impl IntoResponse {
    let form = parse_compose_form(&raw);
    let uid = user.0.id;
    let subject = form.subject.trim();
    let content = form.content.trim();
    if subject.is_empty() || content.is_empty() || form.recipient_ids.is_empty() {
        return Redirect::to("/platforms/messaging/compose");
    }
    let conv_id = new_id();
    let msg_id = new_id();
    let ts = now();
    let mut tx = state.db.begin().await.unwrap();
    let _ = sqlx::query("INSERT INTO conversations (id, subject, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&conv_id).bind(subject).bind(uid).bind(&ts).bind(&ts)
        .execute(&mut *tx).await;
    let _ = sqlx::query("INSERT INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)")
        .bind(new_id()).bind(&conv_id).bind(uid)
        .execute(&mut *tx).await;
    let mut valid_recipients: Vec<i64> = Vec::new();
    for r in &form.recipient_ids {
        if let Ok(rid) = r.parse::<i64>() {
            if rid != uid {
                let _ = sqlx::query("INSERT OR IGNORE INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)")
                    .bind(new_id()).bind(&conv_id).bind(rid)
                    .execute(&mut *tx).await;
                valid_recipients.push(rid);
            }
        }
    }
    let _ = sqlx::query("INSERT INTO messages (id, conversation_id, sender_id, content, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&msg_id).bind(&conv_id).bind(uid).bind(content).bind(&ts)
        .execute(&mut *tx).await;
    let _ = sqlx::query("INSERT INTO message_read_statuses (id, message_id, user_id) VALUES (?, ?, ?)")
        .bind(new_id()).bind(&msg_id).bind(uid)
        .execute(&mut *tx).await;
    let _ = tx.commit().await;

    // Bridge the conversation to any mesh node bound to a recipient.
    if !valid_recipients.is_empty() {
        let placeholders = vec!["?"; valid_recipients.len()].join(",");
        let sql = format!(
            "SELECT id FROM mesh_nodes WHERE bound_user_id IN ({placeholders})"
        );
        let mut nodes = sqlx::query(&sql);
        for rid in &valid_recipients {
            nodes = nodes.bind(rid);
        }
        let nodes = nodes.fetch_all(&state.db).await;
        if let Ok(nodes) = nodes {
            let mut bridged = false;
            for n in nodes {
                let node_pk: String = n.get("id");
                let res = sqlx::query(
                    "INSERT OR IGNORE INTO mesh_bridge_mappings (id, conversation_id, mesh_node_id) VALUES (?, ?, ?)",
                )
                .bind(new_id())
                .bind(&conv_id)
                .bind(&node_pk)
                .execute(&state.db)
                .await;
                if res.map(|r| r.rows_affected() > 0).unwrap_or(false) {
                    bridged = true;
                }
            }
            if bridged {
                if let Some(b) = crate::broker::broker() {
                    let _ = b.sync_subscriptions().await;
                    let b = b.clone();
                    let (c, m) = (conv_id.clone(), msg_id.clone());
                    tokio::spawn(async move { b.push_conversation_message(&c, &m).await });
                }
            }
        }
    }
    Redirect::to(&format!("/platforms/messaging/conversations/{conv_id}"))
}

async fn view_conversation(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let uid = user.0.id;
    let conv = sqlx::query("SELECT id, subject FROM conversations WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);
    let Some(conv) = conv else {
        return Html("<h1>404 Not Found</h1>").into_response();
    };
    if !is_participant(&state.db, &id, uid).await {
        // Public channel conversations (room mapping with no owner) are open:
        // auto-join instead of denying access.
        let public_channel: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM mesh_bridge_mappings \
             WHERE conversation_id = ? AND owner_user_id IS NULL AND enabled = 1",
        )
        .bind(&id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        if public_channel.is_none() {
            return Html("<h1>403 Access denied</h1>").into_response();
        }
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO conversation_participants (id, conversation_id, user_id) \
             VALUES (?, ?, ?)",
        )
        .bind(new_id())
        .bind(&id)
        .bind(uid)
        .execute(&state.db)
        .await;
    }

    // Viewing marks everything as read.
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO message_read_statuses (id, message_id, user_id) \
         SELECT lower(hex(randomblob(16))), m.id, ? FROM messages m WHERE m.conversation_id = ?",
    )
    .bind(uid)
    .bind(&id)
    .execute(&state.db)
    .await;

    let msgs = sqlx::query(
        "SELECT m.id, m.content, m.created_at, u.username, u.id AS sender_id \
         FROM messages m JOIN users u ON u.id = m.sender_id \
         WHERE m.conversation_id = ? ORDER BY m.created_at ASC",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let participants = sqlx::query(
        "SELECT u.id, u.username FROM conversation_participants p \
         JOIN users u ON u.id = p.user_id WHERE p.conversation_id = ?",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let is_mesh: bool = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM mesh_bridge_mappings WHERE conversation_id = ? LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .is_some();

    Html(state.templates.render(
        "conversation.html",
        json!({
            "user": { "id": uid, "username": user.0.username },
            "conversation": { "id": conv.get::<String, _>("id"), "subject": conv.get::<String, _>("subject") },
            "message_rows": msgs.iter().map(|m| json!([
                { "id": m.get::<String, _>("id"), "content": m.get::<String, _>("content"),
                  "created_at": m.get::<String, _>("created_at"), "sender_id": m.get::<i64, _>("sender_id") },
                { "username": m.get::<String, _>("username") }
            ])).collect::<Vec<_>>(),
            "participants": participants.iter().map(|p| json!([null, {
                "id": p.get::<i64, _>("id"), "username": p.get::<String, _>("username") }])).collect::<Vec<_>>(),
            "is_mesh": is_mesh,
            "poll_after": chrono::Utc::now().to_rfc3339(),
        }),
    ))
    .into_response()
}

#[derive(Deserialize)]
struct MessagesApiQuery {
    after: Option<String>,
}

async fn messages_api(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(q): Query<MessagesApiQuery>,
) -> impl IntoResponse {
    let uid = user.0.id;
    if !is_participant(&state.db, &id, uid).await {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let after = q.after.filter(|a| !a.is_empty());
    let rows = match &after {
        Some(a) => sqlx::query(
            "SELECT m.content, m.created_at, u.username FROM messages m \
             JOIN users u ON u.id = m.sender_id \
             WHERE m.conversation_id = ? AND m.created_at > ? ORDER BY m.created_at ASC",
        )
        .bind(&id)
        .bind(a.replace('T', " ").trim_end_matches('Z'))
        .fetch_all(&state.db)
        .await,
        None => sqlx::query(
            "SELECT m.content, m.created_at, u.username FROM messages m \
             JOIN users u ON u.id = m.sender_id \
             WHERE m.conversation_id = ? ORDER BY m.created_at ASC",
        )
        .bind(&id)
        .fetch_all(&state.db)
        .await,
    }
    .unwrap_or_default();

    let _ = sqlx::query(
        "INSERT OR IGNORE INTO message_read_statuses (id, message_id, user_id) \
         SELECT lower(hex(randomblob(16))), m.id, ? FROM messages m WHERE m.conversation_id = ?",
    )
    .bind(uid)
    .bind(&id)
    .execute(&state.db)
    .await;

    let messages: Vec<serde_json::Value> = rows
        .iter()
        .map(|m| {
            json!({
                "sender": m.get::<String, _>("username"),
                "content": m.get::<String, _>("content"),
                "created_at": m.get::<String, _>("created_at"),
            })
        })
        .collect();
    axum::Json(json!({
        "messages": messages,
        "server_time": chrono::Utc::now().to_rfc3339(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ReplyForm {
    content: String,
}

async fn reply(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Form(form): Form<ReplyForm>,
) -> axum::response::Response {
    let uid = user.0.id;
    if !is_participant(&state.db, &id, uid).await {
        return Html("<h1>403 Access denied</h1>").into_response();
    }
    let content = form.content.trim();
    if content.is_empty() {
        return Redirect::to(&format!("/platforms/messaging/conversations/{id}")).into_response();
    }
    let msg_id = new_id();
    let ts = now();
    let _ = sqlx::query(
        "INSERT INTO messages (id, conversation_id, sender_id, content, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&msg_id)
    .bind(&id)
    .bind(uid)
    .bind(content)
    .bind(&ts)
    .execute(&state.db)
    .await;
    let _ = sqlx::query("INSERT INTO message_read_statuses (id, message_id, user_id) VALUES (?, ?, ?)")
        .bind(new_id())
        .bind(&msg_id)
        .bind(uid)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("UPDATE conversations SET updated_at = ? WHERE id = ?")
        .bind(&ts)
        .bind(&id)
        .execute(&state.db)
        .await;

    Hub::global().notify(&id);
    if let Some(b) = crate::broker::broker() {
        let b = b.clone();
        let (c, m) = (id.clone(), msg_id.clone());
        tokio::spawn(async move { b.push_conversation_message(&c, &m).await });
    }
    Redirect::to(&format!("/platforms/messaging/conversations/{id}")).into_response()
}

async fn delete_message(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((id, msg_id)): Path<(String, String)>,
) -> axum::response::Response {
    let uid = user.0.id;
    if !is_participant(&state.db, &id, uid).await {
        return Html("<h1>403 Access denied</h1>").into_response();
    }
    let sender: Option<i64> = sqlx::query_scalar("SELECT sender_id FROM messages WHERE id = ? AND conversation_id = ?")
        .bind(&msg_id)
        .bind(&id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);
    match sender {
        Some(s) if s == uid || user.0.role == "admin" => {
            let _ = sqlx::query("DELETE FROM messages WHERE id = ?").bind(&msg_id).execute(&state.db).await;
        }
        Some(_) => return Html("<h1>403 Not your message</h1>").into_response(),
        None => return Html("<h1>404 Not Found</h1>").into_response(),
    }
    Redirect::to(&format!("/platforms/messaging/conversations/{id}")).into_response()
}

async fn delete_conversation(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let uid = user.0.id;
    if !is_participant(&state.db, &id, uid).await {
        return Html("<h1>403 Access denied</h1>").into_response();
    }
    let _ = sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await;
    if let Some(b) = crate::broker::broker() {
        let _ = b.sync_subscriptions().await;
    }
    Redirect::to("/platforms/messaging/").into_response()
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<String>,
}

async fn conversation_events(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let uid = user.0.id;
    let mut rx = Hub::global().subscribe();
    let mut after: String = q
        .after
        .filter(|a| !a.is_empty())
        .map(|a| a.replace('T', " ").trim_end_matches('Z').to_string())
        .unwrap_or_else(now);
    let db = state.db.clone();
    let conv_id = id.clone();
    let is_part = is_participant(&db, &conv_id, uid).await;

    let stream = async_stream::stream! {
        if !is_part {
            yield Ok(Event::default().event("error").data("access denied"));
            return;
        }
        let mut sent: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let mut progressed = false;
            // DB poll (authoritative; also covers direct writes).
            let rows = sqlx::query(
                "SELECT m.id, m.content, m.created_at, u.username FROM messages m \
                 JOIN users u ON u.id = m.sender_id \
                 WHERE m.conversation_id = ? AND m.created_at > ? ORDER BY m.created_at ASC",
            )
            .bind(&conv_id)
            .bind(&after)
            .fetch_all(&db)
            .await
            .unwrap_or_default();
            for r in rows {
                let mid: String = r.get("id");
                after = r.get("created_at");
                if sent.insert(mid.clone()) {
                    let data = json!({
                        "id": mid,
                        "sender": r.get::<String, _>("username"),
                        "content": r.get::<String, _>("content"),
                        "created_at": r.get::<String, _>("created_at"),
                    });
                    yield Ok(Event::default().id(&mid).event("message").data(data.to_string()));
                    progressed = true;
                }
            }
            if !progressed {
                yield Ok(Event::default().comment("keepalive"));
            }
            // Wait for hub wake-up or timeout.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while let Ok(ev) = rx.recv().await {
                    if ev.conversation_id == conv_id {
                        return;
                    }
                }
            })
            .await;
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn unread_count_api(user: RequireUser, State(state): State<SharedState>) -> impl IntoResponse {
    let count = unread_count(&state.db, user.0.id).await;
    axum::Json(json!({ "unread_count": count }))
}
