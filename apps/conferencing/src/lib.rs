//! Conferencing app: WebRTC audio/video calling, group rooms, voicemail.
//! Ported from the Python (fastmain) implementation. The console is a single
//! process, so the Postgres LISTEN/NOTIFY broker was dropped; the in-process
//! connection hub is authoritative. Call/invitation/recording ids keep their
//! UUID strings; user ids are console INTEGERs.

pub mod hub;
pub mod recordings;
pub mod rooms;
pub mod router;
pub mod sweep;
pub mod ws;

use axum::response::{Html, IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

use syarc_plugin_api::SharedState;

pub struct ConferencingPlugin;

impl ConferencingPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("conferencing", "Conferencing", "/conferencing")
            .icon("bi-telephone");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for ConferencingPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {
        let mut r = Router::new()
            .merge(router::router())
            .merge(rooms::router())
            .merge(recordings::router());
        r = r.route(
            "/conferencing/ws",
            axum::routing::get(ws::ws_handler),
        );
        r.with_state(state)
    }

    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        hub::init(db.clone());
        tokio::spawn(sweep::sweep_loop(db.clone()));
        Ok(())
    }
}

// ------------------------------------------------------------- shared utils

pub(crate) fn page(state: &SharedState, template: &str, ctx: Value) -> Response {
    Html(state.templates.render(template, ctx)).into_response()
}

pub(crate) fn redirect(to: &str) -> Response {
    axum::response::Redirect::to(to).into_response()
}

pub(crate) fn err400(msg: &str) -> Response {
    err_json(axum::http::StatusCode::BAD_REQUEST, msg)
}

pub(crate) fn err403(msg: &str) -> Response {
    err_json(axum::http::StatusCode::FORBIDDEN, msg)
}

pub(crate) fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}

pub(crate) fn err409(msg: &str) -> Response {
    err_json(axum::http::StatusCode::CONFLICT, msg)
}

pub(crate) fn err_json(status: axum::http::StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"detail": msg}))).into_response()
}

pub(crate) fn ok_json(v: Value) -> Response {
    axum::Json(v).into_response()
}

pub(crate) fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn form_map(raw: &[u8]) -> std::collections::HashMap<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub(crate) fn fv(form: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
    form.get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub(crate) fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(crate) fn seconds_from_now(secs: i64) -> String {
    (chrono::Local::now() + chrono::Duration::seconds(secs))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

pub(crate) fn parse_dt(s: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()
}

pub(crate) fn format_duration(seconds: Option<f64>) -> String {
    let Some(sec) = seconds else { return String::new() };
    let sec = sec.max(0.0) as i64;
    let (m, s) = (sec / 60, sec % 60);
    let (h, m) = (m / 60, m % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub(crate) fn call_duration_seconds(started: &str, ended: Option<&str>) -> Option<f64> {
    let ended = ended?;
    parse_dt(ended)
            .zip(parse_dt(started))
            .map(|(e, s)| (e - s).num_seconds() as f64)
}

/// Relay a WebRTC signaling message (SDP/ICE/peer_ready) to `to`.
pub(crate) fn relay_webrtc(from: i64, msg: &Value) {
    let Some(to) = msg.get("to").and_then(|v| v.as_i64()) else {
        return;
    };
    let mut envelope = serde_json::Map::new();
    envelope.insert("from".into(), json!(from.to_string()));
    if let Some(t) = msg.get("type") {
        envelope.insert("type".into(), t.clone());
    }
    if let Some(obj) = msg.as_object() {
        for (k, v) in obj {
            if k != "to" && k != "type" {
                envelope.insert(k.clone(), v.clone());
            }
        }
    }
    hub::send_to_user(to, &Value::Object(envelope));
}

// --------------------------------------------------------- shared call logic

/// Row shape for a call joined with its caller's username.
pub(crate) struct CallRow {
    pub id: String,
    pub caller_id: i64,
    pub caller_username: String,
    pub media_type: String,
    pub status: String,
    pub call_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
}

pub(crate) fn call_row(r: &sqlx::sqlite::SqliteRow) -> CallRow {
    use sqlx::Row;
    CallRow {
        id: r.get("id"),
        caller_id: r.get("caller_id"),
        caller_username: r
            .try_get::<Option<String>, _>("caller_username")
            .ok()
            .flatten()
            .unwrap_or_else(|| "Unknown".into()),
        media_type: r.get("media_type"),
        status: r.get("status"),
        call_type: r.get("call_type"),
        started_at: r.get("started_at"),
        ended_at: r
            .try_get::<Option<String>, _>("ended_at")
            .ok()
            .flatten(),
    }
}

pub(crate) const CALL_SQL: &str = "SELECT c.*, u.username AS caller_username \
     FROM conferencing_calls c JOIN users u ON u.id = c.caller_id";

pub(crate) fn call_json(c: &CallRow, user_id: i64) -> Value {
    json!({
        "id": c.id,
        "caller": c.caller_username,
        "caller_id": c.caller_id,
        "direction": if c.caller_id == user_id { "outgoing" } else { "incoming" },
        "media_type": c.media_type,
        "status": c.status,
        "call_type": c.call_type,
        "started_at": c.started_at,
        "ended_at": c.ended_at,
        "duration_seconds": call_duration_seconds(&c.started_at, c.ended_at.as_deref()),
        "duration_label": format_duration(call_duration_seconds(&c.started_at, c.ended_at.as_deref())),
    })
}

pub(crate) fn call_ctx(c: &CallRow, user_id: i64) -> Value {
    let mut v = call_json(c, user_id);
    v["caller"] = json!({"username": c.caller_username, "id": c.caller_id});
    v
}

/// Single-session guard: the active/ringing call this user is in, if any.
pub(crate) async fn user_in_active_call(
    db: &syarc_plugin_api::Db,
    user_id: i64,
) -> sqlx::Result<Option<CallRow>> {
    let call = sqlx::query_as::<_, (String,)>("SELECT id FROM conferencing_calls \
         WHERE caller_id = ? AND status IN ('active','ringing') LIMIT 1")
        .bind(user_id)
        .fetch_optional(db)
        .await?;
    if let Some((id,)) = call {
        return fetch_call(db, &id).await;
    }
    let parts = sqlx::query_as::<_, (String,)>(
        "SELECT call_id FROM conferencing_call_participants \
         WHERE user_id = ? AND left_at IS NULL",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    for (call_id,) in parts {
        if let Some(c) = fetch_call(db, &call_id).await? {
            if c.status == "active" || c.status == "ringing" {
                return Ok(Some(c));
            }
        }
    }
    Ok(None)
}

pub(crate) async fn fetch_call(
    db: &syarc_plugin_api::Db,
    call_id: &str,
) -> sqlx::Result<Option<CallRow>> {
    let sql = format!("{CALL_SQL} WHERE c.id = ?");
    let row = sqlx::query(&sql).bind(call_id).fetch_optional(db).await?;
    Ok(row.map(|r| call_row(&r)))
}

/// Stamp left_at on everyone still in the call, mark it ended, and notify
/// the other participants over the hub. Shared by end-call/call_end/room-end.
pub(crate) async fn end_call(
    db: &syarc_plugin_api::Db,
    call_id: &str,
    by_user: i64,
) -> sqlx::Result<()> {
    let now = now_str();
    sqlx::query("UPDATE conferencing_call_participants SET left_at = ? \
                 WHERE call_id = ? AND left_at IS NULL")
        .bind(&now)
        .bind(call_id)
        .execute(db)
        .await?;
    sqlx::query("UPDATE conferencing_calls SET status = 'ended', ended_at = ? WHERE id = ?")
        .bind(&now)
        .bind(call_id)
        .execute(db)
        .await?;
    let ids = sqlx::query_as::<_, (i64,)>(
        "SELECT user_id FROM conferencing_call_participants WHERE call_id = ?",
    )
    .bind(call_id)
    .fetch_all(db)
    .await?;
    for (uid,) in ids {
        if uid != by_user {
            hub::send_to_user(
                uid,
                &json!({"type": "call_ended", "call_id": call_id, "by": by_user.to_string()}),
            );
        }
    }
    Ok(())
}

/// WS/HTTP accept: mark invitation accepted, add participant, activate call,
/// notify the caller.
pub(crate) async fn accept_call(
    db: &syarc_plugin_api::Db,
    call: &CallRow,
    user: &syarc_plugin_api::User,
) -> sqlx::Result<Result<(), String>> {
    let now = now_str();
    let res = sqlx::query(
        "UPDATE conferencing_invitations SET status = 'accepted', responded_at = ? \
         WHERE call_id = ? AND invitee_id = ? AND status = 'pending'",
    )
    .bind(&now)
    .bind(&call.id)
    .bind(user.id)
    .execute(db)
    .await?;
    if res.rows_affected() == 0 {
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT status FROM conferencing_invitations WHERE call_id = ? AND invitee_id = ?",
        )
        .bind(&call.id)
        .bind(user.id)
        .fetch_optional(db)
        .await?;
        return Ok(Err(match existing {
            None => "No invitation found for this call".into(),
            Some(status) => format!("Invitation already {status}"),
        }));
    }
    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM conferencing_call_participants WHERE call_id = ? AND user_id = ?",
    )
    .bind(&call.id)
    .bind(user.id)
    .fetch_optional(db)
    .await?;
    if existing.is_none() {
        sqlx::query(
            "INSERT INTO conferencing_call_participants (call_id, user_id, joined_at) \
             VALUES (?, ?, ?)",
        )
        .bind(&call.id)
        .bind(user.id)
        .bind(&now)
        .execute(db)
        .await?;
    }
    sqlx::query("UPDATE conferencing_calls SET status = 'active' WHERE id = ?")
        .bind(&call.id)
        .execute(db)
        .await?;
    hub::send_to_user(
        call.caller_id,
        &json!({
            "type": "call_accepted",
            "call_id": call.id,
            "responder_id": user.id.to_string(),
            "responder_username": user.username,
        }),
    );
    Ok(Ok(()))
}

/// Reject (direct call): mark invitation rejected, end the call, notify caller.
pub(crate) async fn reject_call(
    db: &syarc_plugin_api::Db,
    call: &CallRow,
    user: &syarc_plugin_api::User,
) -> sqlx::Result<()> {
    let now = now_str();
    sqlx::query(
        "UPDATE conferencing_invitations SET status = 'rejected', responded_at = ? \
         WHERE call_id = ? AND invitee_id = ?",
    )
    .bind(&now)
    .bind(&call.id)
    .bind(user.id)
    .execute(db)
    .await?;
    sqlx::query("UPDATE conferencing_calls SET status = 'rejected', ended_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&call.id)
        .execute(db)
        .await?;
    hub::send_to_user(
        call.caller_id,
        &json!({
            "type": "call_rejected",
            "call_id": call.id,
            "responder_id": user.id.to_string(),
        }),
    );
    Ok(())
}
