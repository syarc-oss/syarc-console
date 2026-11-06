//! Group conferencing: rooms are CallRecords with call_type='group'.

use axum::body::Bytes;
use axum::extract::{Path, RawForm, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{
    err400, err404, err409, fetch_call, form_map, fv, hub, new_uuid, now_str, ok_json, page,
    seconds_from_now, user_in_active_call,
};

pub const ROOM_INVITE_TTL_SECONDS: i64 = 120;

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/conferencing/api/create-room", post(create_room))
        .route("/conferencing/api/room/{room_id}/join", post(join_room))
        .route("/conferencing/room/{room_id}", get(room_page))
        .route("/conferencing/api/room/{room_id}/leave", post(leave_room))
        .route(
            "/conferencing/api/room/{room_id}/leave-permanently",
            post(leave_room_permanently),
        )
        .route(
            "/conferencing/api/room/{room_id}/eligible-invitees",
            get(eligible_invitees),
        )
        .route("/conferencing/api/room/{room_id}/invite", post(invite_to_room))
        .route(
            "/conferencing/api/room/{room_id}/pending-invites",
            get(list_pending_invites),
        )
        .route(
            "/conferencing/api/room/{room_id}/cancel-invite/{invitation_id}",
            post(cancel_room_invite),
        )
        .route(
            "/conferencing/api/room/{room_id}/accept-invite/{invitation_id}",
            post(accept_room_invite),
        )
        .route(
            "/conferencing/api/room/{room_id}/decline-invite/{invitation_id}",
            post(decline_room_invite),
        )
}

async fn create_room(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let mut media_type = fv(&form, "media_type").unwrap_or_else(|| "audio".into());
    if media_type != "audio" && media_type != "video" {
        media_type = "audio".into();
    }

    if let Ok(Some(_existing)) = user_in_active_call(&state.db, user.id).await {
        return err409("You are already in a call or room on another device.");
    }

    let room_id = new_uuid();
    let now = now_str();
    let res = sqlx::query(
        "INSERT INTO conferencing_calls (id, caller_id, media_type, status, call_type, started_at) \
         VALUES (?, ?, ?, 'active', 'group', ?)",
    )
    .bind(&room_id)
    .bind(user.id)
    .bind(&media_type)
    .bind(&now)
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let _ = sqlx::query(
        "INSERT INTO conferencing_call_participants (call_id, user_id, joined_at) VALUES (?, ?, ?)",
    )
    .bind(&room_id)
    .bind(user.id)
    .bind(&now)
    .execute(&state.db)
    .await;

    ok_json(json!({
        "room_id": room_id,
        "url": format!("/conferencing/room/{room_id}"),
        "media_type": media_type,
    }))
}

async fn join_room(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(room_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(room) = (match fetch_call(db, &room_id).await {
        Ok(r) => r,
        Err(e) => return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }) else {
        return err404("Room not found");
    };
    if room.call_type != "group" {
        return err404("Room not found");
    }
    if room.status == "ended" {
        return crate::err_json(axum::http::StatusCode::GONE, "Room has ended");
    }
    if let Ok(Some(existing)) = user_in_active_call(db, user.id).await {
        if existing.id != room_id {
            return err409("You are already in a call or room on another device.");
        }
    }

    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM conferencing_call_participants WHERE call_id = ? AND user_id = ?",
    )
    .bind(&room_id)
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    match existing {
        None => {
            let _ = sqlx::query(
                "INSERT INTO conferencing_call_participants (call_id, user_id, joined_at) \
                 VALUES (?, ?, ?)",
            )
            .bind(&room_id)
            .bind(user.id)
            .bind(now_str())
            .execute(db)
            .await;
        }
        Some(_) => {
            let _ = sqlx::query(
                "UPDATE conferencing_call_participants SET left_at = NULL \
                 WHERE call_id = ? AND user_id = ?",
            )
            .bind(&room_id)
            .bind(user.id)
            .execute(db)
            .await;
        }
    }
    broadcast_roster(db, &room_id).await;
    ok_json(json!({"joined": true, "room_id": room_id}))
}

async fn room_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(room_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(room) = (match fetch_call(db, &room_id).await {
        Ok(r) => r,
        Err(e) => return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }) else {
        return err404("Room not found");
    };
    if room.call_type != "group" {
        return err404("Room not found");
    }
    if room.status == "ended" {
        return crate::err_json(axum::http::StatusCode::GONE, "Room has ended");
    }

    let parts = sqlx::query(
        "SELECT p.user_id, u.username FROM conferencing_call_participants p \
         JOIN users u ON u.id = p.user_id \
         WHERE p.call_id = ? AND p.left_at IS NULL ORDER BY p.joined_at",
    )
    .bind(&room_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let participants: Vec<Value> = parts
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("user_id").to_string(),
                "username": r.get::<String, _>("username"),
                "is_self": r.get::<i64, _>("user_id") == user.id,
            })
        })
        .collect();

    page(
        &state,
        "conferencing_room.html",
        json!({
            "username": user.username,
            "user": {"id": user.id, "username": user.username},
            "room": {"id": room.id, "caller_id": room.caller_id,
                     "caller": {"username": room.caller_username}},
            "participants": participants,
            "media_type": room.media_type,
            "ice_config": json!({"iceServers": []}),
        }),
    )
}

async fn leave_room(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(room_id): Path<String>,
) -> Response {
    let db = &state.db;
    let now = now_str();
    let _ = sqlx::query(
        "UPDATE conferencing_call_participants SET left_at = ? \
         WHERE call_id = ? AND user_id = ? AND left_at IS NULL",
    )
    .bind(&now)
    .bind(&room_id)
    .bind(user.id)
    .execute(db)
    .await;
    let _ = sqlx::query(
        "UPDATE conferencing_invitations SET status = 'left' \
         WHERE call_id = ? AND invitee_id = ? AND status = 'accepted'",
    )
    .bind(&room_id)
    .bind(user.id)
    .execute(db)
    .await;

    let room = fetch_call(db, &room_id).await.ok().flatten();
    
    if let Some(room) = &room {
        if room.caller_id == user.id {
            // Creator left — end the room for everyone.
            let _ = crate::end_call(db, &room_id, user.id).await;
            broadcast_roster(db, &room_id).await;
            return ok_json(json!({"left": true, "room_ended": true}));
        }
    }

    // If no participants remain, end the room.
    let remaining = sqlx::query_scalar::<_, i64>(
        "SELECT user_id FROM conferencing_call_participants \
         WHERE call_id = ? AND left_at IS NULL LIMIT 1",
    )
    .bind(&room_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if remaining.is_none() {
        let _ = sqlx::query(
            "UPDATE conferencing_calls SET status = 'ended', ended_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(&room_id)
        .execute(db)
        .await;
    }
    broadcast_roster(db, &room_id).await;
    ok_json(json!({"left": true}))
}

async fn leave_room_permanently(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(room_id): Path<String>,
) -> Response {
    let db = &state.db;
    let _ = sqlx::query(
        "DELETE FROM conferencing_call_participants WHERE call_id = ? AND user_id = ?",
    )
    .bind(&room_id)
    .bind(user.id)
    .execute(db)
    .await;
    let _ = sqlx::query(
        "DELETE FROM conferencing_invitations WHERE call_id = ? AND invitee_id = ?",
    )
    .bind(&room_id)
    .bind(user.id)
    .execute(db)
    .await;

    let remaining = sqlx::query_scalar::<_, i64>(
        "SELECT user_id FROM conferencing_call_participants \
         WHERE call_id = ? AND left_at IS NULL LIMIT 1",
    )
    .bind(&room_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if remaining.is_none() {
        let _ = sqlx::query(
            "UPDATE conferencing_calls SET status = 'ended', ended_at = ? WHERE id = ?",
        )
        .bind(now_str())
        .bind(&room_id)
        .execute(db)
        .await;
    }
    broadcast_roster(db, &room_id).await;
    ok_json(json!({"left": true, "permanent": true}))
}

async fn eligible_invitees(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(room_id): Path<String>,
) -> Response {
    let db = &state.db;
    let in_room: Vec<i64> = sqlx::query_as::<_, (i64,)>(
        "SELECT user_id FROM conferencing_call_participants \
         WHERE call_id = ? AND left_at IS NULL",
    )
    .bind(&room_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id,)| id)
    .collect();

    let invites = sqlx::query_as::<_, (i64, String)>(
        "SELECT invitee_id, status FROM conferencing_invitations WHERE call_id = ?",
    )
    .bind(&room_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let users = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, username FROM users WHERE is_active = 1 AND id != ? AND username NOT LIKE 'mesh-%' ORDER BY username",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let eligible: Vec<Value> = users
        .iter()
        .map(|(id, username)| {
            json!({
                "id": id.to_string(),
                "username": username,
                "in_room": in_room.contains(id),
                "invite_status": invites.iter().find(|(iid, _)| iid == id).map(|(_, s)| s.clone()),
            })
        })
        .collect();
    ok_json(json!({"users": eligible}))
}

async fn invite_to_room(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(room_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err400("invalid JSON body"),
    };
    let Some(recipients) = parsed.get("recipient_ids").and_then(|v| v.as_array()) else {
        return err400("recipient_ids required");
    };
    if recipients.is_empty() {
        return err400("recipient_ids required");
    }

    let Some(room) = (match fetch_call(db, &room_id).await {
        Ok(r) => r,
        Err(e) => return crate::err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }) else {
        return err404("Room not found");
    };
    if room.call_type != "group" {
        return err404("Room not found");
    }
    if room.status == "ended" {
        return crate::err_json(axum::http::StatusCode::GONE, "Room has ended");
    }

    let expires = seconds_from_now(ROOM_INVITE_TTL_SECONDS);
    let mut invited: Vec<(String, i64)> = Vec::new();
    for rid in recipients {
        let Some(rid) = rid.as_i64() else { continue };
        let existing = sqlx::query_as::<_, (String, String)>(
            "SELECT id, status FROM conferencing_invitations WHERE call_id = ? AND invitee_id = ?",
        )
        .bind(&room_id)
        .bind(rid)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        match existing {
            Some((_inv_id, status)) if status == "pending" || status == "rejected" => continue,
            Some((inv_id, _)) => {
                let _ = sqlx::query(
                    "UPDATE conferencing_invitations \
                     SET status = 'pending', responded_at = NULL, expires_at = ? WHERE id = ?",
                )
                .bind(&expires)
                .bind(&inv_id)
                .execute(db)
                .await;
                invited.push((inv_id, rid));
            }
            None => {
                let inv_id = new_uuid();
                let res = sqlx::query(
                    "INSERT INTO conferencing_invitations \
                     (id, call_id, invitee_id, status, created_at, expires_at) \
                     VALUES (?, ?, ?, 'pending', ?, ?)",
                )
                .bind(&inv_id)
                .bind(&room_id)
                .bind(rid)
                .bind(now_str())
                .bind(&expires)
                .execute(db)
                .await;
                if res.is_ok() {
                    invited.push((inv_id, rid));
                }
            }
        }
    }

    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM conferencing_call_participants \
         WHERE call_id = ? AND left_at IS NULL",
    )
    .bind(&room_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    for (invitation_id, rid) in invited.iter() {
        hub::send_to_user(
            *rid,
            &json!({
                "type": "room_invite",
                "room_id": room_id,
                "invitation_id": invitation_id,
                "inviter_username": user.username,
                "media_type": room.media_type,
                "participant_count": count,
            }),
        );
    }
    ok_json(json!({"invited": invited.len()}))
}

async fn list_pending_invites(
    State(state): State<SharedState>,
    RequireUser(_): RequireUser,
    Path(room_id): Path<String>,
) -> Response {
    let rows = sqlx::query(
        "SELECT i.id, i.invitee_id, u.username, i.created_at \
         FROM conferencing_invitations i JOIN users u ON u.id = i.invitee_id \
         WHERE i.call_id = ? AND i.status = 'pending' ORDER BY i.created_at DESC",
    )
    .bind(&room_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let invites: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "invitation_id": r.get::<String, _>("id"),
                "invitee_id": r.get::<i64, _>("invitee_id").to_string(),
                "username": r.get::<String, _>("username"),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect();
    ok_json(json!({"invites": invites}))
}

async fn cancel_room_invite(
    State(state): State<SharedState>,
    RequireUser(_): RequireUser,
    Path((room_id, invitation_id)): Path<(String, String)>,
) -> Response {
    let res = sqlx::query(
        "UPDATE conferencing_invitations SET status = 'cancelled', responded_at = ? \
         WHERE id = ? AND call_id = ? AND status = 'pending'",
    )
    .bind(now_str())
    .bind(&invitation_id)
    .bind(&room_id)
    .execute(&state.db)
    .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"cancelled": true})),
        _ => err404("Pending invitation not found"),
    }
}

async fn accept_room_invite(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((room_id, invitation_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let inv = sqlx::query_scalar::<_, String>(
        "SELECT status FROM conferencing_invitations WHERE id = ? AND invitee_id = ?",
    )
    .bind(&invitation_id)
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let Some(status) = inv else {
        return err404("Invitation not found");
    };
    if status != "pending" {
        return err409(&format!("Invitation already {status}"));
    }
    let _ = sqlx::query(
        "UPDATE conferencing_invitations SET status = 'accepted', responded_at = ? WHERE id = ?",
    )
    .bind(now_str())
    .bind(&invitation_id)
    .execute(db)
    .await;

    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM conferencing_call_participants WHERE call_id = ? AND user_id = ?",
    )
    .bind(&room_id)
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    match existing {
        None => {
            let _ = sqlx::query(
                "INSERT INTO conferencing_call_participants (call_id, user_id, joined_at) \
                 VALUES (?, ?, ?)",
            )
            .bind(&room_id)
            .bind(user.id)
            .bind(now_str())
            .execute(db)
            .await;
        }
        Some(_) => {
            let _ = sqlx::query(
                "UPDATE conferencing_call_participants SET left_at = NULL \
                 WHERE call_id = ? AND user_id = ?",
            )
            .bind(&room_id)
            .bind(user.id)
            .execute(db)
            .await;
        }
    }
    broadcast_roster(db, &room_id).await;
    ok_json(json!({"accepted": true, "room_id": room_id}))
}

async fn decline_room_invite(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((room_id, invitation_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let res = sqlx::query(
        "UPDATE conferencing_invitations SET status = 'rejected', responded_at = ? \
         WHERE id = ? AND invitee_id = ?",
    )
    .bind(now_str())
    .bind(&invitation_id)
    .bind(user.id)
    .execute(db)
    .await;
    if matches!(res, Ok(r) if r.rows_affected() == 0) {
        return err404("Invitation not found");
    }
    if let Ok(Some(room)) = fetch_call(db, &room_id).await {
        hub::send_to_user(
            room.caller_id,
            &json!({
                "type": "invite_declined",
                "room_id": room_id,
                "invitee_id": user.id.to_string(),
                "invitee_username": user.username,
            }),
        );
    }
    ok_json(json!({"declined": true}))
}

/// Send the current participant roster to every active participant.
pub(crate) async fn broadcast_roster(db: &syarc_plugin_api::Db, room_id: &str) {
    let rows = sqlx::query(
        "SELECT p.user_id, u.username FROM conferencing_call_participants p \
         JOIN users u ON u.id = p.user_id \
         WHERE p.call_id = ? AND p.left_at IS NULL ORDER BY p.joined_at",
    )
    .bind(room_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let roster: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("user_id").to_string(),
                "username": r.get::<String, _>("username"),
            })
        })
        .collect();
    let message = json!({"type": "roster", "room_id": room_id, "participants": roster});
    if let Some(entries) = message["participants"].as_array() {
        for entry in entries {
            if let Some(id) = entry["id"].as_str().and_then(|s| s.parse::<i64>().ok()) {
                hub::send_to_user(id, &message);
            }
        }
    }
}
