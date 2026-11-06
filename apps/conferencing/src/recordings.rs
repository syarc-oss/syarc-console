//! Recording upload, playback, and management endpoints.
//! Files live under the recording root / <username> /. Access is gated to the
//! recording user or the voicemail recipient.

use std::path::PathBuf;

use axum::extract::{Multipart, Path as AxPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{err400, err403, err404, new_uuid, ok_json};

pub const DEFAULT_RECORDING_ROOT: &str = "/usr/local/recordings/conferencing";

pub fn recording_root() -> PathBuf {
    std::env::var("CONFERENCING_RECORDING_ROOT")
        .unwrap_or_else(|_| DEFAULT_RECORDING_ROOT.into())
        .into()
}

fn user_dir(username: &str) -> PathBuf {
    let safe: String = username
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let dir = recording_root().join(if safe.is_empty() { "anon".into() } else { safe });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/conferencing/api/recordings/upload", post(upload_recording))
        .route("/conferencing/api/recordings", get(list_recordings))
        .route(
            "/conferencing/api/recordings/{recording_id}/file",
            get(serve_recording_file),
        )
        .route(
            "/conferencing/api/recordings/{recording_id}/listened",
            post(mark_listened),
        )
        .route(
            "/conferencing/api/recordings/{recording_id}",
            axum::routing::delete(delete_recording),
        )
}

struct RecordingRow {
    id: String,
    call_id: Option<String>,
    recorded_by_user_id: i64,
    recipient_user_id: Option<i64>,
    kind: String,
    media_type: String,
    file_path: Option<String>,
    duration_seconds: Option<f64>,
    created_at: String,
    listened_at: Option<String>,
}

fn recording_row(r: &sqlx::sqlite::SqliteRow) -> RecordingRow {
    RecordingRow {
        id: r.get("id"),
        call_id: r.try_get::<Option<String>, _>("call_id").ok().flatten(),
        recorded_by_user_id: r.get("recorded_by_user_id"),
        recipient_user_id: r.try_get::<Option<i64>, _>("recipient_user_id").ok().flatten(),
        kind: r.get("kind"),
        media_type: r.get("media_type"),
        file_path: r.try_get::<Option<String>, _>("file_path").ok().flatten(),
        duration_seconds: r.try_get::<Option<f64>, _>("duration_seconds").ok().flatten(),
        created_at: r.get("created_at"),
        listened_at: r.try_get::<Option<String>, _>("listened_at").ok().flatten(),
    }
}

#[allow(clippy::result_large_err)]
fn check_access(rec: &RecordingRow, user_id: i64) -> Result<(), Response> {
    if rec.recorded_by_user_id == user_id {
        return Ok(());
    }
    if rec.recipient_user_id == Some(user_id) {
        return Ok(());
    }
    Err(err403("Not authorized to access this recording"))
}

async fn upload_recording(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    mut multipart: Multipart,
) -> Response {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut call_id: Option<String> = None;
    let mut recipient_id: Option<i64> = None;
    let mut kind = "call".to_string();
    let mut media_type = "audio".to_string();
    let mut duration_seconds: Option<f64> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                match field.bytes().await {
                    Ok(b) => file_bytes = Some(b.to_vec()),
                    Err(_) => file_bytes = Some(Vec::new()),
                }
            }
            "call_id" => call_id = field.text().await.ok().filter(|t| !t.is_empty()),
            "recipient_id" => {
                recipient_id = field
                    .text()
                    .await
                    .ok()
                    .and_then(|t| t.trim().parse::<i64>().ok())
            }
            "kind" => {
                if let Ok(t) = field.text().await {
                    kind = t;
                }
            }
            "media_type" => {
                if let Ok(t) = field.text().await {
                    media_type = t;
                }
            }
            "duration_seconds" => {
                duration_seconds = field
                    .text()
                    .await
                    .ok()
                    .and_then(|t| t.trim().parse::<f64>().ok())
            }
            _ => {
                let _ = field.text().await;
            }
        }
    }

    if kind != "voicemail" && kind != "call" {
        return err400("kind must be voicemail or call");
    }
    if media_type != "audio" && media_type != "video" {
        return err400("media_type must be audio or video");
    }
    if kind == "voicemail" && recipient_id.is_none() {
        return err400("recipient_id required for voicemail");
    }
    let Some(bytes) = file_bytes.filter(|b| !b.is_empty()) else {
        return err400("Empty upload");
    };

    let filename = format!(
        "{}_{}{}.webm",
        uuid::Uuid::new_v4().simple(),
        kind,
        media_type.chars().next().unwrap_or('a')
    );
    let target = user_dir(&user.username).join(&filename);
    if let Err(e) = std::fs::write(&target, &bytes) {
        return crate::err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }

    let id = new_uuid();
    let res = sqlx::query(
        "INSERT INTO conferencing_recordings \
         (id, call_id, recorded_by_user_id, recipient_user_id, kind, media_type, \
          file_path, duration_seconds, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&call_id)
    .bind(user.id)
    .bind(recipient_id)
    .bind(&kind)
    .bind(&media_type)
    .bind(target.to_string_lossy().to_string())
    .bind(duration_seconds)
    .bind(crate::now_str())
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        let _ = std::fs::remove_file(&target);
        return crate::err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }

    ok_json(json!({
        "id": id,
        "kind": kind,
        "media_type": media_type,
        "duration_seconds": duration_seconds,
    }))
}

fn serialize(rec: &RecordingRow, users: &std::collections::HashMap<i64, String>, user_id: i64) -> Value {
    json!({
        "id": rec.id,
        "call_id": rec.call_id,
        "kind": rec.kind,
        "media_type": rec.media_type,
        "duration_seconds": rec.duration_seconds,
        "created_at": rec.created_at,
        "listened_at": rec.listened_at,
        "recorded_by": users.get(&rec.recorded_by_user_id),
        "recipient": rec.recipient_user_id.and_then(|id| users.get(&id)),
        "direction": if rec.recorded_by_user_id == user_id { "outgoing" } else { "incoming" },
    })
}

async fn list_recordings(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let rows = sqlx::query(
        "SELECT * FROM conferencing_recordings \
         WHERE recorded_by_user_id = ? OR recipient_user_id = ? \
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(user.id)
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let mut users = std::collections::HashMap::new();
    for r in &rows {
        for uid in [r.get::<i64, _>("recorded_by_user_id")]
            .into_iter()
            .chain(
                r.try_get::<Option<i64>, _>("recipient_user_id")
                    .ok()
                    .flatten(),
            )
        {
            if let std::collections::hash_map::Entry::Vacant(e) = users.entry(uid) {
                if let Ok(Some(name)) = sqlx::query_scalar::<_, String>(
                    "SELECT username FROM users WHERE id = ?",
                )
                .bind(uid)
                .fetch_optional(&state.db)
                .await
                {
                    e.insert(name);
                }
            }
        }
    }
    let recordings: Vec<Value> = rows
        .iter()
        .map(|r| serialize(&recording_row(r), &users, user.id))
        .collect();
    ok_json(json!({"recordings": recordings}))
}

async fn load_recording(
    state: &SharedState,
    recording_id: &str,
) -> Option<RecordingRow> {
    sqlx::query("SELECT * FROM conferencing_recordings WHERE id = ?")
        .bind(recording_id)
        .fetch_optional(&state.db)
        .await
        .ok()?
        .map(|r| recording_row(&r))
}

async fn serve_recording_file(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AxPath(recording_id): AxPath<String>,
) -> Response {
    let Some(rec) = load_recording(&state, &recording_id).await else {
        return err404("Recording not found");
    };
    if let Err(e) = check_access(&rec, user.id) {
        return e;
    }
    let Some(path) = rec.file_path.as_deref() else {
        return crate::err_json(StatusCode::GONE, "Recording file missing on disk");
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let mime = if rec.media_type == "video" {
                "video/webm"
            } else {
                "audio/webm"
            };
            (
                [
                    (header::CONTENT_TYPE, mime.to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("inline; filename=\"{}_{}.webm\"", rec.kind, rec.id),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => crate::err_json(StatusCode::GONE, "Recording file missing on disk"),
    }
}

async fn mark_listened(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AxPath(recording_id): AxPath<String>,
) -> Response {
    let Some(rec) = load_recording(&state, &recording_id).await else {
        return err404("Recording not found");
    };
    if let Err(e) = check_access(&rec, user.id) {
        return e;
    }
    if rec.listened_at.is_none() {
        let _ = sqlx::query(
            "UPDATE conferencing_recordings SET listened_at = ? WHERE id = ?",
        )
        .bind(crate::now_str())
        .bind(&recording_id)
        .execute(&state.db)
        .await;
    }
    ok_json(json!({"listened": true}))
}

async fn delete_recording(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AxPath(recording_id): AxPath<String>,
) -> Response {
    let Some(rec) = load_recording(&state, &recording_id).await else {
        return err404("Recording not found");
    };
    if let Err(e) = check_access(&rec, user.id) {
        return e;
    }
    let _ = sqlx::query("DELETE FROM conferencing_recordings WHERE id = ?")
        .bind(&recording_id)
        .execute(&state.db)
        .await;
    if let Some(path) = rec.file_path.as_deref() {
        let _ = std::fs::remove_file(path);
    }
    ok_json(json!({"deleted": true}))
}
