//! HTTP routes and WS call-control handlers for 1:1 calls.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState, User};

use crate::{
    accept_call, call_ctx, call_json, call_row, end_call, err400, err403, err404, err409,
    fetch_call, format_duration, hub, new_uuid, now_str, ok_json, page, redirect, reject_call,
    seconds_from_now, user_in_active_call, CALL_SQL,
};

pub const CALL_INVITATION_TTL_SECONDS: i64 = 30;

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/conferencing", get(dashboard))
        .route("/conferencing/", get(dashboard))
        .route("/conferencing/history", get(history_page))
        .route("/conferencing/api/online", get(api_online))
        .route("/conferencing/api/history", get(api_history))
        .route("/conferencing/api/dismiss-missed", post(dismiss_missed))
        .route("/conferencing/api/ice-config", get(ice_config))
        .route("/conferencing/api/my-active-call", get(my_active_call))
        .route("/conferencing/api/end-call/{call_id}", post(end_call_endpoint))
        .route("/conferencing/api/calls/{call_id}/dismiss", post(dismiss_call))
        .route("/conferencing/api/calls/{call_id}/accept", post(accept_call_endpoint))
        .route("/conferencing/api/call-status/{call_id}", get(call_status))
        .route("/conferencing/api/calls/initiate", post(initiate_call))
        .route("/conferencing/call/{call_id}", get(call_page))
}

// ------------------------------------------------------------------- pages

async fn dashboard(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;

    let history_sql = format!(
        "{CALL_SQL} \
         LEFT JOIN conferencing_call_participants p ON p.call_id = c.id \
         LEFT JOIN conferencing_invitations i ON i.call_id = c.id \
         LEFT JOIN conferencing_call_dismissals d ON d.call_id = c.id AND d.user_id = ? \
         WHERE (c.caller_id = ? OR p.user_id = ? OR i.invitee_id = ?) AND d.id IS NULL \
         ORDER BY c.started_at DESC LIMIT 25"
    );
    let history = match sqlx::query(&history_sql)
        .bind(user.id)
        .bind(user.id)
        .bind(user.id)
        .bind(user.id)
        .fetch_all(db)
        .await
    {
        Ok(rows) => rows,
        Err(e) => return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let history_view: Vec<Value> = history
        .iter()
        .map(|r| {
            let c = call_row(r);
            json!({"call": call_ctx(&c, user.id)})
        })
        .collect();

    let missed = sqlx::query(
        "SELECT i.id AS invitation_id, i.created_at, c.id AS call_id, c.media_type, \
                u.username AS caller_username \
         FROM conferencing_invitations i \
         JOIN conferencing_calls c ON c.id = i.call_id \
         JOIN users u ON u.id = c.caller_id \
         WHERE i.invitee_id = ? AND i.status = 'missed' AND i.acknowledged_at IS NULL \
         ORDER BY i.created_at DESC",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let missed_view: Vec<Value> = missed
        .iter()
        .map(|r| {
            json!({
                "invitation_id": r.get::<String, _>("invitation_id"),
                "call_id": r.get::<String, _>("call_id"),
                "caller_username": r.get::<String, _>("caller_username"),
                "media_type": r.get::<String, _>("media_type"),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect();

    let voicemails = sqlx::query(
        "SELECT r.*, u.username AS from_username FROM conferencing_recordings r \
         LEFT JOIN users u ON u.id = r.recorded_by_user_id \
         WHERE r.recipient_user_id = ? AND r.kind = 'voicemail' \
         ORDER BY r.created_at DESC LIMIT 20",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let voicemail_view: Vec<Value> = voicemails
        .iter()
        .map(|r| {
            let listened = r.try_get::<Option<String>, _>("listened_at").ok().flatten().is_some();
            let dur = r.try_get::<Option<f64>, _>("duration_seconds").ok().flatten();
            json!({
                "id": r.get::<String, _>("id"),
                "from_username": r.try_get::<Option<String>, _>("from_username").ok().flatten()
                    .unwrap_or_else(|| "Unknown".into()),
                "media_type": r.get::<String, _>("media_type"),
                "duration_label": format_duration(dur),
                "created_at": r.get::<String, _>("created_at"),
                "listened": listened,
            })
        })
        .collect();
    let unlistened_voicemail_count = voicemail_view
        .iter()
        .filter(|v| !v["listened"].as_bool().unwrap_or(false))
        .count();

    let my_recordings = sqlx::query(
        "SELECT * FROM conferencing_recordings \
         WHERE recorded_by_user_id = ? AND kind = 'call' \
         ORDER BY created_at DESC LIMIT 10",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let my_recordings_view: Vec<Value> = my_recordings
        .iter()
        .map(|r| {
            let dur = r.try_get::<Option<f64>, _>("duration_seconds").ok().flatten();
            json!({
                "id": r.get::<String, _>("id"),
                "media_type": r.get::<String, _>("media_type"),
                "duration_label": format_duration(dur),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect();

    let contacts = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, username FROM users WHERE is_active = 1 AND id != ? AND username NOT LIKE 'mesh-%' ORDER BY username",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let contact_status: Vec<Value> = contacts
        .iter()
        .map(|(id, username)| {
            json!({
                "user": {"id": id, "username": username},
                "online": hub::is_online(*id),
            })
        })
        .collect();

    page(
        &state,
        "conferencing_dashboard.html",
        json!({
            "username": user.username,
            "history_view": history_view,
            "missed_count": missed_view.len(),
            "missed_view": missed_view,
            "voicemail_view": voicemail_view,
            "unlistened_voicemail_count": unlistened_voicemail_count,
            "my_recordings": my_recordings_view,
            "contact_status": contact_status,
        }),
    )
}

async fn history_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let sql = format!(
        "{CALL_SQL} \
         LEFT JOIN conferencing_call_participants p ON p.call_id = c.id \
         LEFT JOIN conferencing_invitations i ON i.call_id = c.id \
         WHERE c.caller_id = ? OR p.user_id = ? OR i.invitee_id = ? \
         ORDER BY c.started_at DESC LIMIT 200"
    );
    let rows = sqlx::query(&sql)
        .bind(user.id)
        .bind(user.id)
        .bind(user.id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let history_view: Vec<Value> = rows
        .iter()
        .map(|r| json!({"call": call_ctx(&call_row(r), user.id)}))
        .collect();
    page(
        &state,
        "conferencing_history.html",
        json!({"username": user.username, "history_view": history_view}),
    )
}

async fn call_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(call_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(call) = (match fetch_call(db, &call_id).await {
        Ok(c) => c,
        Err(e) => return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }) else {
        return err404("Call not found");
    };

    let is_caller = call.caller_id == user.id;
    if !is_caller {
        let participant = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM conferencing_call_participants WHERE call_id = ? AND user_id = ?",
        )
        .bind(&call.id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        if participant.is_none() {
            let invitation = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM conferencing_invitations \
                 WHERE call_id = ? AND invitee_id = ? AND status IN ('pending','accepted')",
            )
            .bind(&call.id)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
            if invitation.is_none() {
                return err403("Not a participant in this call");
            }
        }
    }

    if call.status == "ended" || call.status == "rejected" {
        return redirect("/conferencing/");
    }

    // Resolve the peer (the other user in a 1:1 call).
    let peer: Option<(i64, String)> = if is_caller {
        let peer_participant = sqlx::query_as::<_, (i64,)>(
            "SELECT user_id FROM conferencing_call_participants \
             WHERE call_id = ? AND user_id != ? ORDER BY joined_at DESC LIMIT 1",
        )
        .bind(&call.id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        if let Some((pid,)) = peer_participant {
            sqlx::query_as::<_, (i64, String)>("SELECT id, username FROM users WHERE id = ?")
                .bind(pid)
                .fetch_optional(db)
                .await
                .ok()
                .flatten()
        } else {
            sqlx::query_as::<_, (i64, String)>(
                "SELECT u.id, u.username FROM conferencing_invitations i \
                 JOIN users u ON u.id = i.invitee_id \
                 WHERE i.call_id = ? ORDER BY i.created_at DESC LIMIT 1",
            )
            .bind(&call.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
        }
    } else {
        sqlx::query_as::<_, (i64, String)>("SELECT id, username FROM users WHERE id = ?")
            .bind(call.caller_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
    };

    page(
        &state,
        "conferencing_call.html",
        json!({
            "username": user.username,
            "call": {"id": call.id},
            "peer": peer.map(|(id, username)| json!({"id": id, "username": username})),
            "is_caller": is_caller,
            "media_type": call.media_type,
            "ice_config": json!({"iceServers": []}),
            "call_status": call.status,
        }),
    )
}

// ---------------------------------------------------------------- json apis

async fn api_online(RequireUser(_): RequireUser) -> Response {
    ok_json(json!({"users": hub::online_users()}))
}

async fn api_history(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let sql = format!(
        "{CALL_SQL} \
         LEFT JOIN conferencing_call_participants p ON p.call_id = c.id \
         LEFT JOIN conferencing_invitations i ON i.call_id = c.id \
         WHERE c.caller_id = ? OR p.user_id = ? OR i.invitee_id = ? \
         ORDER BY c.started_at DESC LIMIT 50"
    );
    let rows = sqlx::query(&sql)
        .bind(user.id)
        .bind(user.id)
        .bind(user.id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let calls: Vec<Value> = rows.iter().map(|r| call_json(&call_row(r), user.id)).collect();
    ok_json(json!({"calls": calls}))
}

async fn dismiss_missed(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let _ = sqlx::query(
        "UPDATE conferencing_invitations SET acknowledged_at = ? \
         WHERE invitee_id = ? AND status = 'missed' AND acknowledged_at IS NULL",
    )
    .bind(now_str())
    .bind(user.id)
    .execute(&state.db)
    .await;
    ok_json(json!({"dismissed": true}))
}

async fn ice_config(RequireUser(_): RequireUser) -> Response {
    ok_json(json!({"iceServers": []}))
}

async fn my_active_call(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let db = &state.db;

    // 1) Calls this user placed.
    let sql = format!(
        "{CALL_SQL} WHERE c.caller_id = ? AND c.status IN ('active','ringing') \
         ORDER BY c.started_at DESC LIMIT 1"
    );
    let mut call = sqlx::query(&sql)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| call_row(&r));

    if call.is_none() {
        // 2) Calls joined as participant.
        if let Some((call_id,)) = sqlx::query_as::<_, (String,)>(
            "SELECT call_id FROM conferencing_call_participants \
             WHERE user_id = ? AND left_at IS NULL ORDER BY joined_at DESC LIMIT 1",
        )
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        {
            if let Ok(Some(c)) = fetch_call(db, &call_id).await {
                if c.status == "active" {
                    call = Some(c);
                }
            }
        }
    }

    if let Some(c) = call {
        return ok_json(json!({"call": {
            "id": c.id,
            "status": c.status,
            "media_type": c.media_type,
            "is_caller": c.caller_id == user.id,
            "call_type": c.call_type,
        }}));
    }

    // 2b) Active group rooms the user left (offer rejoin to non-creators).
    let left = sqlx::query_as::<_, (String,)>(
        "SELECT call_id FROM conferencing_call_participants \
         WHERE user_id = ? AND left_at IS NOT NULL ORDER BY left_at DESC LIMIT 5",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for (call_id,) in left {
        if let Ok(Some(room)) = fetch_call(db, &call_id).await {
            if room.status == "active" && room.call_type == "group" && room.caller_id != user.id {
                return ok_json(json!({"call": {
                    "id": room.id,
                    "status": "rejoin_room",
                    "media_type": room.media_type,
                    "is_caller": false,
                    "call_type": "group",
                }}));
            }
        }
    }

    // 3) Pending invitations.
    if let Some(inv) = sqlx::query(
        "SELECT i.id AS invitation_id, i.status AS inv_status, \
                c.id, c.caller_id, c.media_type, c.status, c.call_type, c.started_at, c.ended_at, \
                u.username AS caller_username \
         FROM conferencing_invitations i \
         JOIN conferencing_calls c ON c.id = i.call_id \
         JOIN users u ON u.id = c.caller_id \
         WHERE i.invitee_id = ? AND i.status IN ('pending','missed','accepted','rejected') \
         ORDER BY i.created_at DESC LIMIT 1",
    )
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    {
        let c = call_row(&inv);
        let should_show = if c.call_type == "direct" {
            c.status == "ringing"
        } else {
            c.status == "active"
        };
        let inv_status = inv
            .try_get::<String, _>("inv_status")
            .unwrap_or_default();
        if should_show && inv_status == "pending" {
            return ok_json(json!({"call": {
                "id": c.id,
                "invitation_id": inv.get::<String, _>("invitation_id"),
                "status": if c.call_type == "group" { "incoming_room" } else { "incoming" },
                "media_type": c.media_type,
                "is_caller": false,
                "caller_id": c.caller_id.to_string(),
                "caller_username": c.caller_username,
            }}));
        }
    }

    ok_json(json!({"call": null}))
}

async fn end_call_endpoint(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(call_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(call) = (match fetch_call(db, &call_id).await {
        Ok(c) => c,
        Err(e) => return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }) else {
        return err404("Call not found");
    };
    if call.caller_id != user.id {
        let participant = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM conferencing_call_participants WHERE call_id = ? AND user_id = ?",
        )
        .bind(&call_id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        if participant.is_none() {
            return err403("Not a participant");
        }
    }
    if let Err(e) = end_call(db, &call_id, user.id).await {
        return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    ok_json(json!({"ended": true}))
}

async fn dismiss_call(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(call_id): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM conferencing_calls WHERE id = ?")
        .bind(&call_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Call not found");
    }
    let _ = sqlx::query(
        "INSERT INTO conferencing_call_dismissals (call_id, user_id, dismissed_at) \
         SELECT ?, ?, ? WHERE NOT EXISTS (\
             SELECT 1 FROM conferencing_call_dismissals WHERE call_id = ? AND user_id = ?)",
    )
    .bind(&call_id)
    .bind(user.id)
    .bind(now_str())
    .bind(&call_id)
    .bind(user.id)
    .execute(db)
    .await;
    ok_json(json!({"dismissed": true}))
}

async fn accept_call_endpoint(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(call_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(call) = (match fetch_call(db, &call_id).await {
        Ok(c) => c,
        Err(e) => return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }) else {
        return err404("Call not found");
    };
    if call.status != "ringing" && call.status != "active" {
        return err409(&format!("Call is {}", call.status));
    }
    if let Ok(Some(existing)) = user_in_active_call(db, user.id).await {
        if existing.id != call_id {
            return err409("You are already in a call on another device. End that call first.");
        }
    }
    match accept_call(db, &call, &user).await {
        Ok(Ok(())) => ok_json(json!({"accepted": true, "call_id": call_id})),
        Ok(Err(msg)) => {
            let status = if msg.starts_with("No invitation") { 404 } else { 409 };
            crate::err_json(
                axum::http::StatusCode::from_u16(status).unwrap(),
                &msg,
            )
        }
        Err(e) => crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn call_status(
    State(state): State<SharedState>,
    RequireUser(_): RequireUser,
    Path(call_id): Path<String>,
) -> Response {
    let status: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT status FROM conferencing_calls WHERE id = ?",
    )
    .bind(&call_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    ok_json(json!({"status": status.unwrap_or_else(|| "not_found".into())}))
}

async fn initiate_call(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    body: Bytes,
) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err400("invalid JSON body"),
    };
    // Single-session guard: don't start a new call if already in one.
    if let Ok(Some(_existing)) = user_in_active_call(&state.db, user.id).await {
        return err409("You are already in a call on another device. End that call first.");
    }
    match ws_initiate(&state, &user, &parsed).await {
        Ok(Some(call_id)) => ok_json(json!({
            "call_id": call_id,
            "url": format!("/conferencing/call/{call_id}"),
        })),
        _ => err400("to is required"),
    }
}

// ----------------------------------------------- WS call-control handlers

/// Create a ringing direct call + invitation, ring the invitee, and echo
/// call_created back to the caller. Returns the call id.
pub(crate) async fn ws_initiate(
    state: &SharedState,
    user: &User,
    msg: &Value,
) -> Result<Option<String>, sqlx::Error> {
    let Some(invitee_id) = msg.get("to").and_then(|v| v.as_i64()) else {
        return Ok(None);
    };
    let media_type = msg
        .get("media_type")
        .and_then(|v| v.as_str())
        .unwrap_or("audio")
        .to_string();

    let db = &state.db;

    let call_id = new_uuid();
    let invitation_id = new_uuid();
    let now = now_str();
    let expires = seconds_from_now(CALL_INVITATION_TTL_SECONDS);

    sqlx::query(
        "INSERT INTO conferencing_calls (id, caller_id, media_type, status, call_type, started_at) \
         VALUES (?, ?, ?, 'ringing', 'direct', ?)",
    )
    .bind(&call_id)
    .bind(user.id)
    .bind(&media_type)
    .bind(&now)
    .execute(db)
    .await?;
    sqlx::query(
        "INSERT INTO conferencing_call_participants (call_id, user_id, joined_at) VALUES (?, ?, ?)",
    )
    .bind(&call_id)
    .bind(user.id)
    .bind(&now)
    .execute(db)
    .await?;
    sqlx::query(
        "INSERT INTO conferencing_invitations (id, call_id, invitee_id, status, created_at, expires_at) \
         VALUES (?, ?, ?, 'pending', ?, ?)",
    )
    .bind(&invitation_id)
    .bind(&call_id)
    .bind(invitee_id)
    .bind(&now)
    .bind(&expires)
    .execute(db)
    .await?;

    let invitee_username: Option<String> =
        sqlx::query_scalar::<_, String>("SELECT username FROM users WHERE id = ?")
            .bind(invitee_id)
            .fetch_optional(db)
            .await?;

    hub::send_to_user(
        invitee_id,
        &json!({
            "type": "incoming_call",
            "call_id": call_id,
            "invitation_id": invitation_id,
            "caller_id": user.id.to_string(),
            "caller_username": user.username,
            "media_type": media_type,
        }),
    );
    hub::send_to_user(
        user.id,
        &json!({
            "type": "call_created",
            "call_id": call_id,
            "media_type": media_type,
            "to_username": invitee_username.unwrap_or_default(),
        }),
    );
    Ok(Some(call_id))
}

pub(crate) async fn ws_call_response(
    state: &SharedState,
    user: &User,
    msg: &Value,
) -> Result<(), sqlx::Error> {
    let Some(call_id) = msg.get("call_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let Some(call) = fetch_call(&state.db, call_id).await? else {
        return Ok(());
    };
    match msg.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "call_accept" => {
            let _ = accept_call(&state.db, &call, user).await;
        }
        "call_reject" => {
            let _ = reject_call(&state.db, &call, user).await;
        }
        "call_end" => {
            // Only an active participant (or the caller) may end the call.
            let participant = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM conferencing_call_participants \
                 WHERE call_id = ? AND user_id = ? AND left_at IS NULL",
            )
            .bind(call_id)
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?;
            if participant.is_none() && call.caller_id != user.id {
                return Ok(());
            }
            end_call(&state.db, call_id, user.id).await?;
        }
        _ => {}
    }
    Ok(())
}
