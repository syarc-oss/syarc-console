//! Surveillance app: zones of cameras, live streams proxied through
//! go2rtc, ffmpeg segment recording, Reolink AI motion/alarm polling,
//! and camera webhook triggers. Ported from the Python (fastmain)
//! implementation.

mod motion;
mod recordings;

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Path, Query, RawForm, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

pub struct SurveillancePlugin;

impl SurveillancePlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("surveillance", "Surveillance", "/surveillance")
            .icon("bi-camera-video");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for SurveillancePlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, _state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/surveillance", get(landing))
            .route("/surveillance/", get(landing))
            .route("/surveillance/zone/new", get(zone_new).post(zone_create))
            .route("/surveillance/zone/{zone_id}", get(zone_view))
            .route("/surveillance/zone/{zone_id}/edit", get(zone_edit).post(zone_update))
            .route("/surveillance/zone/{zone_id}/delete", post(zone_delete))
            .route("/surveillance/zone/{zone_id}/share", post(zone_share))
            .route("/surveillance/zone/{zone_id}/share/{share_id}/delete", post(share_delete))
            .route("/surveillance/zone/{zone_id}/camera/new", get(camera_new).post(camera_create))
            .route("/surveillance/zone/{zone_id}/camera/{camera_id}/view", get(camera_view))
            .route("/surveillance/zone/{zone_id}/camera/{camera_id}/edit", get(camera_edit).post(camera_update))
            .route("/surveillance/zone/{zone_id}/camera/{camera_id}/delete", post(camera_delete))
            // go2rtc proxies
            .route("/surveillance/api/go2rtc/stream", get(go2rtc_stream))
            .route("/surveillance/api/go2rtc/webrtc", post(go2rtc_webrtc))
            // alarm events
            .route("/surveillance/api/zone/{zone_id}/alarm-events", get(alarm_events))
            .route("/surveillance/api/zone/{zone_id}/alarm-events/delete", post(alarm_events_delete))
            // recordings
            .route("/surveillance/api/camera/{id}/recordings", get(recordings_list))
            .route("/surveillance/api/camera/{id}/recordings/{filename}", get(recording_serve).delete(recording_delete).post(recording_delete))
            .route("/surveillance/api/camera/{id}/recordings/{filename}/save", post(recording_save))
            .route("/surveillance/api/camera/{id}/recordings/{filename}/clip", post(recording_clip))
            .route("/surveillance/api/camera/{id}/snapshot", post(snapshot_save))
            .route("/surveillance/api/camera/{id}/saved", get(saved_list))
            .route("/surveillance/api/camera/{id}/saved/delete", post(saved_delete))
            // ignore windows
            .route("/surveillance/api/camera/{id}/ignore-windows", get(ignore_windows_list).post(ignore_window_create))
            .route("/surveillance/api/camera/{id}/ignore-windows/{window_id}/delete", post(ignore_window_delete))
    }

    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        recordings::start_manager(db.clone()).await;
        motion::start_services(db.clone()).await;
        Ok(())
    }
}

// ------------------------------------------------------------------ helpers

fn page(state: &SharedState, template: &str, ctx: serde_json::Value) -> Html<String> {
    Html(state.templates.render(template, ctx))
}

fn form_map(raw: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn fv(form: &HashMap<String, String>, key: &str) -> Option<String> {
    form.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn fbool(form: &HashMap<String, String>, key: &str) -> bool {
    form.contains_key(key)
}

fn fint(form: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    fv(form, key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub async fn setting(db: &sqlx::SqlitePool, key: &str, default: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| default.to_string())
}

pub async fn go2rtc_api(db: &sqlx::SqlitePool) -> String {
    setting(db, "go2rtc_url", "http://127.0.0.1:1984").await
}

pub async fn go2rtc_is_running(db: &sqlx::SqlitePool) -> bool {
    reqwest::get(format!("{}/api/streams", go2rtc_api(db).await))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

pub fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Full RTSP URL with credentials embedded (raw, like the recorder uses).
pub fn build_rtsp_url(camera: &serde_json::Value) -> String {
    let url = camera["stream_url"].as_str().unwrap_or("");
    let user = camera["username"].as_str().unwrap_or("");
    let pass = camera["password"].as_str().unwrap_or("");
    if !user.is_empty() && url.starts_with("rtsp://") {
        let rest = &url["rtsp://".len()..];
        return format!("rtsp://{user}:{pass}@{rest}");
    }
    url.to_string()
}

fn go2rtc_src(camera: &serde_json::Value) -> String {
    let full = build_rtsp_url(camera);
    let enc = percent_encoding::utf8_percent_encode(&full, percent_encoding::NON_ALPHANUMERIC);
    enc.to_string()
}

/// The camera's own web interface: https://<stream host>.
fn camera_web_url(camera: &serde_json::Value) -> Option<String> {
    let url = camera["stream_url"].as_str()?;
    let after_scheme = url
        .strip_prefix("rtsp://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    // skip credentials (user:pass@) if present, then take up to :port or /path
    let host_part = after_scheme.rsplit_once('@').map_or(after_scheme, |(_, h)| h);
    let host = host_part.split([':', '/']).next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(format!("https://{host}"))
}

async fn load_camera(db: &sqlx::SqlitePool, id: i64) -> Option<serde_json::Value> {
    sqlx::query("SELECT * FROM surveillance_cameras WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()?
        .map(|r| camera_json(&r))
}

pub fn camera_row_json(r: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    camera_json(r)
}

pub fn camera_json(r: &sqlx::sqlite::SqliteRow) -> serde_json::Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "user_id": r.get::<i64, _>("user_id"),
        "name": r.try_get::<String, _>("name").unwrap_or_default(),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "stream_url": r.try_get::<String, _>("stream_url").unwrap_or_default(),
        "stream_type": r.try_get::<String, _>("stream_type").unwrap_or_else(|_| "mjpeg".into()),
        "snapshot_url": r.try_get::<Option<String>, _>("snapshot_url").ok().flatten(),
        "username": r.try_get::<Option<String>, _>("username").ok().flatten(),
        "password": r.try_get::<Option<String>, _>("password").ok().flatten(),
        "is_enabled": r.get::<i64, _>("is_enabled") != 0,
        "recording_enabled": r.get::<i64, _>("recording_enabled") != 0,
        "recording_retention_days": r.get::<i64, _>("recording_retention_days"),
        "recording_mode": r.try_get::<String, _>("recording_mode").unwrap_or_else(|_| "continuous".into()),
        "motion_timeout": r.get::<i64, _>("motion_timeout"),
        "rotation": r.get::<i64, _>("rotation"),
        "alarm_enabled": r.get::<i64, _>("alarm_enabled") != 0,
        "alarm_types": r.try_get::<String, _>("alarm_types").unwrap_or_else(|_| "people".into()),
        "alarm_start": r.try_get::<String, _>("alarm_start").unwrap_or_else(|_| "22:00".into()),
        "alarm_end": r.try_get::<String, _>("alarm_end").unwrap_or_else(|_| "06:00".into()),
        "trigger_camera_ids": r.try_get::<Option<String>, _>("trigger_camera_ids").ok().flatten(),
        "trigger_delay": r.get::<i64, _>("trigger_delay"),
    })
}

fn zone_json(r: sqlx::sqlite::SqliteRow) -> serde_json::Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "user_id": r.get::<i64, _>("user_id"),
        "name": r.try_get::<String, _>("name").unwrap_or_default(),
        "description": r.try_get::<Option<String>, _>("description").ok().flatten(),
        "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
    })
}

/// (zone, is_owner, can_edit) if the user may see the zone.
async fn zone_access(
    db: &sqlx::SqlitePool,
    zone_id: i64,
    user_id: i64,
) -> Option<(serde_json::Value, bool, bool)> {
    let row = sqlx::query(
        "SELECT z.*, \
                (SELECT COUNT(*) FROM surveillance_zone_shares s \
                  WHERE s.zone_id = z.id AND s.shared_with_user_id = ? AND s.can_edit = 1) AS shared_edit \
         FROM surveillance_zones z WHERE z.id = ?",
    )
    .bind(user_id)
    .bind(zone_id)
    .fetch_optional(db)
    .await
    .ok()??;
    let zone = zone_json(row);
    let is_owner = zone["user_id"].as_i64() == Some(user_id);
    let shared_edit = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM surveillance_zone_shares \
         WHERE zone_id = ? AND shared_with_user_id = ? AND can_edit = 1",
    )
    .bind(zone_id)
    .bind(user_id)
    .fetch_one(db)
    .await
    .unwrap_or(0) > 0;
    if !is_owner && !shared_edit {
        // viewer share?
        let viewer = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM surveillance_zone_shares WHERE zone_id = ? AND shared_with_user_id = ?",
        )
        .bind(zone_id)
        .bind(user_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
        if viewer == 0 {
            return None;
        }
        return Some((zone, false, false));
    }
    Some((zone, is_owner, is_owner || shared_edit))
}

async fn zone_cameras(db: &sqlx::SqlitePool, zone_id: i64) -> Vec<serde_json::Value> {
    sqlx::query(
        "SELECT c.* FROM surveillance_cameras c \
         JOIN surveillance_zone_cameras zc ON zc.camera_id = c.id \
         WHERE zc.zone_id = ? ORDER BY zc.display_order, c.name",
    )
    .bind(zone_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| camera_json(&r))
    .collect()
}

fn err_json(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"detail": msg}))).into_response()
}

// ------------------------------------------------------------------ pages

async fn landing(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let owned = sqlx::query("SELECT * FROM surveillance_zones WHERE user_id = ? ORDER BY name")
        .bind(user.0.id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let shared = sqlx::query(
        "SELECT z.* FROM surveillance_zones z \
         JOIN surveillance_zone_shares s ON s.zone_id = z.id \
         WHERE s.shared_with_user_id = ? ORDER BY z.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut items: Vec<serde_json::Value> = Vec::new();
    for (rows, is_owner) in [(owned, true), (shared, false)] {
        for r in rows {
            let zone = zone_json(r);
            let count = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM surveillance_zone_cameras WHERE zone_id = ?",
            )
            .bind(zone["id"].as_i64().unwrap_or(0))
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);
            items.push(json!({"zone": zone, "is_owner": is_owner, "camera_count": count}));
        }
    }
    page(&state, "surveillance_landing.html", json!({"zone_data": items}))
}

async fn zone_new(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let _ = user;
    page(&state, "surveillance_zone_form.html", json!({"zone": null, "is_edit": false}))
}

async fn zone_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Redirect {
    let form = form_map(&raw);
    let name = fv(&form, "name").unwrap_or_default();
    if !name.is_empty() {
        let _ = sqlx::query(
            "INSERT INTO surveillance_zones (user_id, name, description) VALUES (?, ?, ?)",
        )
        .bind(user.0.id)
        .bind(&name)
        .bind(fv(&form, "description"))
        .execute(&state.db)
        .await;
    }
    Redirect::to("/surveillance/")
}

async fn zone_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
) -> Response {
    match zone_access(&state.db, zone_id, user.0.id).await {
        Some((zone, _, true)) => {
            page(&state, "surveillance_zone_form.html", json!({"zone": zone, "is_edit": true})).into_response()
        }
        _ => err_json(StatusCode::NOT_FOUND, "Zone not found").into_response(),
    }
}

async fn zone_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    if zone_access(&state.db, zone_id, user.0.id).await.map(|(_, _, e)| e) != Some(true) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let form = form_map(&raw);
    let name = fv(&form, "name").unwrap_or_default();
    if !name.is_empty() {
        let _ = sqlx::query(
            "UPDATE surveillance_zones SET name = ?, description = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(&name)
        .bind(fv(&form, "description"))
        .bind(zone_id)
        .execute(&state.db)
        .await;
    }
    Redirect::to(&format!("/surveillance/zone/{zone_id}")).into_response()
}

async fn zone_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
) -> Response {
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM surveillance_zones WHERE id = ?")
        .bind(zone_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    // Detach cameras that only belong to this zone, then drop zone+links+shares.
    let linked: Vec<i64> = sqlx::query_scalar(
        "SELECT camera_id FROM surveillance_zone_cameras WHERE zone_id = ?",
    )
    .bind(zone_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let _ = sqlx::query("DELETE FROM surveillance_zone_cameras WHERE zone_id = ?")
        .bind(zone_id)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("DELETE FROM surveillance_zone_shares WHERE zone_id = ?")
        .bind(zone_id)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("DELETE FROM surveillance_zones WHERE id = ?")
        .bind(zone_id)
        .execute(&state.db)
        .await;
    for cam_id in linked {
        let others = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM surveillance_zone_cameras WHERE camera_id = ?",
        )
        .bind(cam_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        if others == 0 {
            let _ = sqlx::query("DELETE FROM surveillance_cameras WHERE id = ?")
                .bind(cam_id)
                .execute(&state.db)
                .await;
        }
    }
    recordings::refresh(&state.db).await;
    motion::refresh(&state.db).await;
    Redirect::to("/surveillance/").into_response()
}

async fn zone_share(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Redirect {
    let form = form_map(&raw);
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM surveillance_zones WHERE id = ?")
        .bind(zone_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if owner == Some(user.0.id) {
        if let Some(username) = fv(&form, "username") {
            let target: Option<i64> =
                sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
                    .bind(&username)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten();
            if let Some(uid) = target {
                if uid != user.0.id {
                    let _ = sqlx::query(
                        "INSERT INTO surveillance_zone_shares (zone_id, shared_with_user_id, can_edit) \
                         VALUES (?, ?, ?) ON CONFLICT(zone_id, shared_with_user_id) \
                         DO UPDATE SET can_edit = excluded.can_edit",
                    )
                    .bind(zone_id)
                    .bind(uid)
                    .bind(fbool(&form, "can_edit") as i64)
                    .execute(&state.db)
                    .await;
                }
            }
        }
    }
    Redirect::to(&format!("/surveillance/zone/{zone_id}"))
}

async fn share_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((zone_id, share_id)): Path<(i64, i64)>,
) -> Redirect {
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM surveillance_zones WHERE id = ?")
        .bind(zone_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if owner == Some(user.0.id) {
        let _ = sqlx::query("DELETE FROM surveillance_zone_shares WHERE id = ? AND zone_id = ?")
            .bind(share_id)
            .bind(zone_id)
            .execute(&state.db)
            .await;
    }
    Redirect::to(&format!("/surveillance/zone/{zone_id}"))
}

async fn zone_view(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
) -> Response {
    let Some((zone, is_owner, can_edit)) = zone_access(&state.db, zone_id, user.0.id).await else {
        return err_json(StatusCode::NOT_FOUND, "Zone not found").into_response();
    };
    let cameras = zone_cameras(&state.db, zone_id).await;

    // Stream URL maps keyed by camera id (like the Python templates expect).
    let mut go2rtc_urls = json!({});
    let mut go2rtc_webrtc_urls = json!({});
    let mut camera_web_urls = json!({});
    let mut stream_types = json!({});
    for cam in &cameras {
        let id = cam["id"].as_i64().unwrap_or(0);
        let st = cam["stream_type"].as_str().unwrap_or("mjpeg");
        stream_types[&id.to_string()] = json!(st);
        if st == "rtsp" || st == "hls" {
            let src = go2rtc_src(cam);
            go2rtc_urls[&id.to_string()] =
                json!(format!("/surveillance/api/go2rtc/stream?src={src}"));
            go2rtc_webrtc_urls[&id.to_string()] =
                json!(format!("/surveillance/api/go2rtc/webrtc?src={src}"));
        }
        if let Some(web) = camera_web_url(cam) {
            camera_web_urls[&id.to_string()] = json!(web);
        }
    }

    let shares = sqlx::query(
        "SELECT s.id, s.can_edit, u.username FROM surveillance_zone_shares s \
         JOIN users u ON u.id = s.shared_with_user_id WHERE s.zone_id = ?",
    )
    .bind(zone_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        json!({
            "id": r.get::<i64, _>("id"),
            "can_edit": r.get::<i64, _>("can_edit") != 0,
            "username": r.try_get::<String, _>("username").unwrap_or_default(),
        })
    })
    .collect::<Vec<_>>();

    let go2rtc_running = reqwest::get(format!("{}/api/streams", go2rtc_api(&state.db).await))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    page(
        &state,
        "surveillance_zone.html",
        json!({
            "zone": zone,
            "is_owner": is_owner,
            "can_edit": can_edit,
            "cameras": cameras,
            "go2rtc_urls": go2rtc_urls,
            "go2rtc_webrtc_urls": go2rtc_webrtc_urls,
            "camera_web_urls": camera_web_urls,
            "stream_types": stream_types,
            "shares": shares,
            "go2rtc_running": go2rtc_running,
        }),
    )
    .into_response()
}

// ------------------------------------------------------------------ camera pages

async fn camera_new(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
) -> Response {
    if zone_access(&state.db, zone_id, user.0.id).await.map(|(_, _, e)| e) != Some(true) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let others = sqlx::query(
        "SELECT DISTINCT c.id, c.name FROM surveillance_cameras c \
         JOIN surveillance_zone_cameras zc ON zc.camera_id = c.id \
         JOIN surveillance_zones z ON z.id = zc.zone_id WHERE z.user_id = ?",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| json!({"id": r.get::<i64, _>("id"), "name": r.try_get::<String, _>("name").unwrap_or_default()}))
    .collect::<Vec<_>>();
    let go2rtc_running = go2rtc_is_running(&state.db).await;
    page(
        &state,
        "surveillance_camera_form.html",
        json!({"zone": {"id": zone_id}, "camera": null, "other_cameras": others, "is_edit": false, "go2rtc_running": go2rtc_running}),
    )
    .into_response()
}

fn apply_camera_fields(
    sql_set: &mut Vec<String>,
    binds: &mut Vec<String>,
    form: &HashMap<String, String>,
    is_edit: bool,
) -> (String, String) {
    // Returns (name, stream_url) required values.
    let name = fv(form, "name").unwrap_or_default();
    let stream_url = fv(form, "stream_url").unwrap_or_default();
    let sval = |k: &str, d: &str| fv(form, k).unwrap_or_else(|| d.to_string());
    sql_set.push("location = ?".into()); binds.push(fv(form, "location").unwrap_or_default());
    sql_set.push("stream_type = ?".into()); binds.push(sval("stream_type", "mjpeg"));
    if !is_edit {
        sql_set.push("is_enabled = ?".into()); binds.push("1".into());
    }
    sql_set.push("recording_enabled = ?".into()); binds.push((fbool(form, "recording_enabled") as i64).to_string());
    sql_set.push("alarm_enabled = ?".into()); binds.push((fbool(form, "alarm_enabled") as i64).to_string());
    sql_set.push("recording_retention_days = ?".into()); binds.push(fint(form, "recording_retention_days", 30).to_string());
    sql_set.push("recording_mode = ?".into()); binds.push(sval("recording_mode", "continuous"));
    sql_set.push("motion_timeout = ?".into()); binds.push(fint(form, "motion_timeout", 30).to_string());
    sql_set.push("rotation = ?".into()); binds.push(fint(form, "rotation", 0).to_string());
    sql_set.push("alarm_types = ?".into()); binds.push(sval("alarm_types", "people"));
    sql_set.push("alarm_start = ?".into()); binds.push(sval("alarm_start", "22:00"));
    sql_set.push("alarm_end = ?".into()); binds.push(sval("alarm_end", "06:00"));
    sql_set.push("trigger_camera_ids = ?".into()); binds.push(fv(form, "trigger_camera_ids").unwrap_or_default());
    sql_set.push("trigger_delay = ?".into()); binds.push(fint(form, "trigger_delay", 0).to_string());
    sql_set.push("updated_at = datetime('now')".into());
    (name, stream_url)
}

async fn camera_create(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    if zone_access(&state.db, zone_id, user.0.id).await.map(|(_, _, e)| e) != Some(true) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let form = form_map(&raw);
    let (name, stream_url) = apply_camera_fields(&mut vec![], &mut vec![], &form, false);
    if name.is_empty() || stream_url.is_empty() {
        return Redirect::to(&format!("/surveillance/zone/{zone_id}/camera/new")).into_response();
    }
    let insert = sqlx::query(
        "INSERT INTO surveillance_cameras \
         (user_id, name, stream_url, username, password, location, stream_type, \
          recording_enabled, alarm_enabled, recording_retention_days, recording_mode, \
          motion_timeout, rotation, alarm_types, alarm_start, alarm_end, \
          trigger_camera_ids, trigger_delay) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.0.id)
    .bind(&name)
    .bind(&stream_url)
    .bind(fv(&form, "username"))
    .bind(fv(&form, "password"))
    .bind(fv(&form, "location"))
    .bind(fv(&form, "stream_type").unwrap_or_else(|| "mjpeg".into()))
    .bind(fbool(&form, "recording_enabled") as i64)
    .bind(fbool(&form, "alarm_enabled") as i64)
    .bind(fint(&form, "recording_retention_days", 30))
    .bind(fv(&form, "recording_mode").unwrap_or_else(|| "continuous".into()))
    .bind(fint(&form, "motion_timeout", 30))
    .bind(fint(&form, "rotation", 0))
    .bind(fv(&form, "alarm_types").unwrap_or_else(|| "people".into()))
    .bind(fv(&form, "alarm_start").unwrap_or_else(|| "22:00".into()))
    .bind(fv(&form, "alarm_end").unwrap_or_else(|| "06:00".into()))
    .bind(fv(&form, "trigger_camera_ids"))
    .bind(fint(&form, "trigger_delay", 0))
    .execute(&state.db)
    .await;
    // last_insert_rowid() must come from the INSERT result: the connection
    // pool can serve `SELECT last_insert_rowid()` from a different
    // connection, which links the camera to the zone with a wrong id.
    if let Ok(res) = insert {
        let row = res.last_insert_rowid();
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO surveillance_zone_cameras (zone_id, camera_id, display_order) \n             VALUES (?, ?, (SELECT COALESCE(MAX(display_order) + 1, 0) \n                            FROM surveillance_zone_cameras WHERE zone_id = ?))",
        )
        .bind(zone_id)
        .bind(row)
        .execute(&state.db)
        .await;
    }
    recordings::refresh(&state.db).await;
    motion::refresh(&state.db).await;
    Redirect::to(&format!("/surveillance/zone/{zone_id}")).into_response()
}

async fn camera_view(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((zone_id, camera_id)): Path<(i64, i64)>,
) -> Response {
    if zone_access(&state.db, zone_id, user.0.id).await.is_none() {
        return err_json(StatusCode::NOT_FOUND, "Zone not found").into_response();
    }
    let Some(camera) = load_camera(&state.db, camera_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    let st = camera["stream_type"].as_str().unwrap_or("mjpeg");
    let (g, w) = if st == "rtsp" || st == "hls" {
        let src = go2rtc_src(&camera);
        (
            format!("/surveillance/api/go2rtc/stream?src={src}"),
            format!("/surveillance/api/go2rtc/webrtc?src={src}"),
        )
    } else {
        (String::new(), String::new())
    };
    let go2rtc_running = reqwest::get(format!("{}/api/streams", go2rtc_api(&state.db).await))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    let siblings: Vec<serde_json::Value> = zone_cameras(&state.db, zone_id)
        .await
        .into_iter()
        .map(|c| json!({"id": c["id"], "name": c["name"]}))
        .collect();
    let camera_web = camera_web_url(&camera);
    let can_edit = zone_access(&state.db, zone_id, user.0.id)
        .await
        .map(|(_, _, e)| e)
        .unwrap_or(false);
    page(
        &state,
        "surveillance_camera_detail.html",
        json!({
            "zone": {"id": zone_id},
            "camera": camera,
            "go2rtc_url": g,
            "go2rtc_webrtc_url": w,
            "camera_web_url": camera_web,
            "go2rtc_running": go2rtc_running,
            "siblings": siblings,
            "can_edit": can_edit,
        }),
    )
    .into_response()
}

async fn camera_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((zone_id, camera_id)): Path<(i64, i64)>,
) -> Response {
    let Some(camera) = load_camera(&state.db, camera_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let go2rtc_running = go2rtc_is_running(&state.db).await;
    page(
        &state,
        "surveillance_camera_form.html",
        json!({"zone": {"id": zone_id}, "camera": camera, "other_cameras": [], "is_edit": true, "go2rtc_running": go2rtc_running}),
    )
    .into_response()
}

async fn camera_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((zone_id, camera_id)): Path<(i64, i64)>,
    RawForm(raw): RawForm,
) -> Response {
    let Some(camera) = load_camera(&state.db, camera_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let form = form_map(&raw);
    let name = fv(&form, "name").unwrap_or_default();
    if name.is_empty() {
        return Redirect::to(&format!("/surveillance/zone/{zone_id}/camera/{camera_id}/edit")).into_response();
    }
    // Credentials only overwritten when provided.
    let username = fv(&form, "username").or_else(|| {
        camera["username"].as_str().map(|s| s.to_string())
    });
    let password = fv(&form, "password").or_else(|| {
        camera["password"].as_str().map(|s| s.to_string())
    });
    let _ = sqlx::query(
        "UPDATE surveillance_cameras SET \
           name = ?, stream_url = ?, username = ?, password = ?, location = ?, \
           stream_type = ?, is_enabled = ?, recording_enabled = ?, alarm_enabled = ?, \
           recording_retention_days = ?, recording_mode = ?, motion_timeout = ?, \
           rotation = ?, alarm_types = ?, alarm_start = ?, alarm_end = ?, \
           trigger_camera_ids = ?, trigger_delay = ?, updated_at = datetime('now') \
         WHERE id = ?",
    )
    .bind(&name)
    .bind(fv(&form, "stream_url").unwrap_or_else(|| camera["stream_url"].as_str().unwrap_or("").to_string()))
    .bind(&username)
    .bind(&password)
    .bind(fv(&form, "location"))
    .bind(fv(&form, "stream_type").unwrap_or_else(|| "mjpeg".into()))
    .bind(fbool(&form, "is_enabled") as i64)
    .bind(fbool(&form, "recording_enabled") as i64)
    .bind(fbool(&form, "alarm_enabled") as i64)
    .bind(fint(&form, "recording_retention_days", 30))
    .bind(fv(&form, "recording_mode").unwrap_or_else(|| "continuous".into()))
    .bind(fint(&form, "motion_timeout", 30))
    .bind(fint(&form, "rotation", 0))
    .bind(fv(&form, "alarm_types").unwrap_or_else(|| "people".into()))
    .bind(fv(&form, "alarm_start").unwrap_or_else(|| "22:00".into()))
    .bind(fv(&form, "alarm_end").unwrap_or_else(|| "06:00".into()))
    .bind(fv(&form, "trigger_camera_ids"))
    .bind(fint(&form, "trigger_delay", 0))
    .bind(camera_id)
    .execute(&state.db)
    .await;
    recordings::refresh(&state.db).await;
    motion::refresh(&state.db).await;
    Redirect::to(&format!("/surveillance/zone/{zone_id}/camera/{camera_id}/view")).into_response()
}

async fn camera_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((zone_id, camera_id)): Path<(i64, i64)>,
) -> Response {
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM surveillance_cameras WHERE id = ?")
        .bind(camera_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let _ = sqlx::query("DELETE FROM surveillance_zone_cameras WHERE zone_id = ? AND camera_id = ?")
        .bind(zone_id)
        .bind(camera_id)
        .execute(&state.db)
        .await;
    let others = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM surveillance_zone_cameras WHERE camera_id = ?",
    )
    .bind(camera_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);
    if others == 0 {
        let _ = sqlx::query("DELETE FROM surveillance_cameras WHERE id = ?")
            .bind(camera_id)
            .execute(&state.db)
            .await;
    }
    recordings::refresh(&state.db).await;
    motion::refresh(&state.db).await;
    Redirect::to(&format!("/surveillance/zone/{zone_id}")).into_response()
}

// ------------------------------------------------------------------ go2rtc proxies

async fn go2rtc_stream(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let _ = user;
    let Some(src) = params.get("src") else {
        return err_json(StatusCode::BAD_REQUEST, "missing src").into_response();
    };
    let url = format!("{}/api/stream.mp4?src={src}", go2rtc_api(&state.db).await);
    let client = reqwest::Client::new();
    let resp = match client.get(&url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return err_json(StatusCode::BAD_GATEWAY, "go2rtc stream unavailable").into_response(),
    };
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("video/mp4")
        .to_string();
    let body = Body::from_stream(resp.bytes_stream());
    match axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
    {
        Ok(r) => r,
        Err(_) => err_json(StatusCode::BAD_GATEWAY, "stream build failed").into_response(),
    }
}

async fn go2rtc_webrtc(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let _ = user;
    let Some(src) = params.get("src") else {
        return err_json(StatusCode::BAD_REQUEST, "missing src").into_response();
    };
    let url = format!("{}/api/webrtc?src={src}", go2rtc_api(&state.db).await);
    let client = reqwest::Client::new();
    match client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/sdp")
        .body(body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            let sdp = r.bytes().await.unwrap_or_default();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/sdp")],
                sdp,
            )
                .into_response()
        }
        _ => err_json(StatusCode::BAD_GATEWAY, "go2rtc WebRTC negotiation failed").into_response(),
    }
}

// ------------------------------------------------------------------ alarm events

async fn alarm_events(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if zone_access(&state.db, zone_id, user.0.id).await.is_none() {
        return err_json(StatusCode::NOT_FOUND, "Zone not found").into_response();
    }
    let hours: i64 = params.get("hours").and_then(|h| h.parse().ok()).unwrap_or(24);
    let rows = sqlx::query(
        "SELECT e.id, e.camera_name, e.trigger_type, e.created_at \
         FROM surveillance_alarm_events e \
         JOIN surveillance_zone_cameras zc ON zc.camera_id = e.camera_id \
         WHERE zc.zone_id = ? \
           AND e.created_at >= datetime('now', ?) \
         ORDER BY e.created_at DESC LIMIT 200",
    )
    .bind(zone_id)
    .bind(format!("-{hours} hours"))
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let events: Vec<_> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "camera_name": r.try_get::<String, _>("camera_name").unwrap_or_default(),
                "trigger_type": r.try_get::<String, _>("trigger_type").unwrap_or_default(),
                "timestamp": r.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect();
    axum::Json(json!({"events": events})).into_response()
}

async fn alarm_events_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(zone_id): Path<i64>,
    body: String,
) -> Response {
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM surveillance_zones WHERE id = ?")
        .bind(zone_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let mut deleted = 0;
    if let Some(ids) = v.get("ids").and_then(|i| i.as_array()) {
        for id in ids {
            if let Some(id) = id.as_i64() {
                let r = sqlx::query(
                    "DELETE FROM surveillance_alarm_events WHERE id = ? AND camera_id IN \
                     (SELECT camera_id FROM surveillance_zone_cameras WHERE zone_id = ?)",
                )
                .bind(id)
                .bind(zone_id)
                .execute(&state.db)
                .await;
                if let Ok(r) = r {
                    deleted += r.rows_affected();
                }
            }
        }
    }
    axum::Json(json!({"deleted": deleted})).into_response()
}

// ------------------------------------------------------------------ recordings API

pub async fn recording_dirs_for(db: &sqlx::SqlitePool, camera: &serde_json::Value) -> (std::path::PathBuf, std::path::PathBuf) {
    let username: String = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
        .bind(camera["user_id"].as_i64().unwrap_or(0))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "user".into());
    let root = recordings::recording_root(db).await;
    let cam = slugify(camera["name"].as_str().unwrap_or("camera"));
    let p = std::path::Path::new(&root).join(username).join(cam);
    (p.clone(), p.join("saved"))
}

fn list_mp4s(dir: &std::path::Path, recursive: bool) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if recursive {
                out.extend(list_mp4s(&p, recursive));
            }
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) == Some("mp4") {
            let meta = e.metadata().ok();
            out.push(json!({
                "name": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                "modified": meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0).map(|t| t.to_rfc3339())),
            }));
        }
    }
    out.sort_by(|a, b| b["name"].as_str().cmp(&a["name"].as_str()));
    out
}

async fn recordings_list(user: RequireUser, State(state): State<SharedState>, Path(id): Path<i64>) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let (dir, _saved) = recording_dirs_for(&state.db, &camera).await;
    let recordings = list_mp4s(&dir, false);
    let segments: Vec<serde_json::Value> = recordings
        .iter()
        .map(|r| {
            let name = r["name"].as_str().unwrap_or_default();
            json!({
                "path": name,
                "filename": name,
                "size": r["size"].as_i64().unwrap_or(0),
                "modified": r["modified"].clone(),
            })
        })
        .collect();
    axum::Json(json!({
        "recordings": recordings,
        "segments": segments,
        "root": dir.to_string_lossy(),
        "camera_name": camera["name"].as_str().unwrap_or_default(),
        "recording": camera["recording_enabled"].as_bool().unwrap_or(false),
    }))
    .into_response()
}

async fn recording_serve(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((id, filename)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let filename = recordings::sanitize_filename(&filename);
    let (dir, saved) = recording_dirs_for(&state.db, &camera).await;
    // Look in recordings dir, saved dir, and saved/snapshots.
    let candidates = [
        dir.join(&filename),
        saved.join(&filename),
        saved.join("snapshots").join(&filename),
    ];
    let path = candidates.into_iter().find(|p| p.is_file());
    let Some(path) = path else {
        return err_json(StatusCode::NOT_FOUND, "file not found").into_response();
    };
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let ctype = if filename.ends_with(".jpg") || filename.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "video/mp4"
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split('-').next())
        .and_then(|s| s.parse::<u64>().ok());
    match range {
        Some(start) if start < len => {
            let end = len - 1;
            let mut f = std::fs::File::open(&path).unwrap();
            use std::io::Seek;
            let _ = f.seek(std::io::SeekFrom::Start(start));
            let take = (end - start + 1) as usize;
            let mut buf = vec![0u8; take];
            let n = std::io::Read::read(&mut f, &mut buf).unwrap_or(0);
            buf.truncate(n);
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, ctype),
                    (header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}").as_str()),
                    (header::ACCEPT_RANGES, "bytes"),
                ],
                buf,
            )
                .into_response()
        }
        _ => {
            let data = std::fs::read(&path).unwrap_or_default();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ctype),
                    (header::ACCEPT_RANGES, "bytes"),
                ],
                data,
            )
                .into_response()
        }
    }
}

async fn recording_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((id, filename)): Path<(i64, String)>,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let filename = recordings::sanitize_filename(&filename);
    let (dir, saved) = recording_dirs_for(&state.db, &camera).await;
    for p in [dir.join(&filename), saved.join(&filename), saved.join("snapshots").join(&filename)] {
        let _ = std::fs::remove_file(p);
    }
    axum::Json(json!({"ok": true})).into_response()
}

async fn recording_save(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((id, filename)): Path<(i64, String)>,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let filename = recordings::sanitize_filename(&filename);
    let (dir, saved) = recording_dirs_for(&state.db, &camera).await;
    let _ = std::fs::create_dir_all(&saved);
    let _ = std::fs::rename(dir.join(&filename), saved.join(&filename));
    axum::Json(json!({"ok": true})).into_response()
}

async fn recording_clip(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((id, filename)): Path<(i64, String)>,
    body: String,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let filename = recordings::sanitize_filename(&filename);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let start = v.get("start").and_then(|s| s.as_f64()).unwrap_or(0.0);
    let end = v.get("end").and_then(|e| e.as_f64());
    let source = v.get("source").and_then(|s| s.as_str()).unwrap_or("recording");
    let (dir, saved) = recording_dirs_for(&state.db, &camera).await;
    let src_dir = if source == "saved" { saved.clone() } else { dir.clone() };
    let src = src_dir.join(&filename);
    if !src.is_file() {
        return err_json(StatusCode::NOT_FOUND, "source recording not found").into_response();
    }
    let mut name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("clip")
        .to_string();
    name = recordings::sanitize_filename(&name);
    if !name.ends_with(".mp4") {
        name.push_str(".mp4");
    }
    let _ = std::fs::create_dir_all(&saved);
    let dest = saved.join(&name);
    match recordings::ffmpeg_clip(&src, &dest, start, end).await {
        Ok(()) => axum::Json(json!({"ok": true, "name": name})).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e).into_response(),
    }
}

async fn snapshot_save(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    body: String,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let data_url = v.get("data").and_then(|d| d.as_str()).unwrap_or("");
    let Some(b64) = data_url.split(",").nth(1) else {
        return err_json(StatusCode::BAD_REQUEST, "missing data").into_response();
    };
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(_) => return err_json(StatusCode::BAD_REQUEST, "bad base64").into_response(),
    };
    let name = chrono::Local::now().format("snapshot_%Y%m%d_%H%M%S.jpg").to_string();
    let (_, saved) = recording_dirs_for(&state.db, &camera).await;
    let dir = saved.join("snapshots");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(&name);
    if std::fs::write(&path, bytes).is_ok() {
        axum::Json(json!({"ok": true, "name": name})).into_response()
    } else {
        err_json(StatusCode::INTERNAL_SERVER_ERROR, "write failed").into_response()
    }
}

async fn saved_list(user: RequireUser, State(state): State<SharedState>, Path(id): Path<i64>) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let (_, saved) = recording_dirs_for(&state.db, &camera).await;
    let videos = list_mp4s(&saved, false);
    let snapshots: Vec<_> = list_jpgs(&saved.join("snapshots"));
    axum::Json(json!({"videos": videos, "snapshots": snapshots})).into_response()
}

fn list_jpgs(dir: &std::path::Path) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jpg") {
            out.push(json!({
                "name": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "size": e.metadata().map(|m| m.len()).unwrap_or(0),
            }));
        }
    }
    out.sort_by(|a, b| b["name"].as_str().cmp(&a["name"].as_str()));
    out
}

async fn saved_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    body: String,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("videos");
    let name = recordings::sanitize_filename(v.get("name").and_then(|n| n.as_str()).unwrap_or(""));
    let (_, saved) = recording_dirs_for(&state.db, &camera).await;
    let p = if kind == "snapshots" {
        saved.join("snapshots").join(name)
    } else {
        saved.join(name)
    };
    let _ = std::fs::remove_file(p);
    axum::Json(json!({"ok": true})).into_response()
}

// ------------------------------------------------------------------ ignore windows

async fn ignore_windows_list(user: RequireUser, State(state): State<SharedState>, Path(id): Path<i64>) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let rows = sqlx::query(
        "SELECT * FROM surveillance_ignore_windows WHERE camera_id = ? ORDER BY start_time",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let windows: Vec<_> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "name": r.try_get::<Option<String>, _>("name").ok().flatten(),
                "start_time": r.try_get::<String, _>("start_time").unwrap_or_default(),
                "end_time": r.try_get::<String, _>("end_time").unwrap_or_default(),
                "days_of_week": r.try_get::<String, _>("days_of_week").unwrap_or_default(),
                "is_enabled": r.get::<i64, _>("is_enabled") != 0,
            })
        })
        .collect();
    axum::Json(json!({"windows": windows})).into_response()
}

async fn ignore_window_create(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    body: String,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let g = |k: &str, d: &str| v.get(k).and_then(|s| s.as_str()).unwrap_or(d).to_string();
    let _ = sqlx::query(
        "INSERT INTO surveillance_ignore_windows (camera_id, name, start_time, end_time, days_of_week) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(v.get("name").and_then(|n| n.as_str()))
    .bind(g("start_time", "00:00"))
    .bind(g("end_time", "00:00"))
    .bind(g("days_of_week", "0,1,2,3,4,5,6"))
    .execute(&state.db)
    .await;
    axum::Json(json!({"ok": true})).into_response()
}

async fn ignore_window_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((id, window_id)): Path<(i64, i64)>,
) -> Response {
    let Some(camera) = load_camera(&state.db, id).await else {
        return err_json(StatusCode::NOT_FOUND, "Camera not found").into_response();
    };
    if camera["user_id"].as_i64() != Some(user.0.id) {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let _ = sqlx::query("DELETE FROM surveillance_ignore_windows WHERE id = ? AND camera_id = ?")
        .bind(window_id)
        .bind(id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"ok": true})).into_response()
}
