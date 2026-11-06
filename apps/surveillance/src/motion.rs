use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Datelike, Timelike};
use serde_json::{json, Value};
use sqlx::Row;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::recordings::{camera_output_dir, recording_root, run_segmented_recording};
use crate::{build_rtsp_url, camera_row_json, setting};

// ------------------------------------------------------------------ time helpers

fn now_minutes() -> u32 {
    let now = chrono::Local::now();
    now.hour() * 60 + now.minute()
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let mut it = s.split(':');
    let h: u32 = it.next()?.parse().ok()?;
    let m: u32 = it.next().unwrap_or("0").parse().ok()?;
    Some(h * 60 + m)
}

pub fn is_in_time_window(start: &str, end: &str) -> bool {
    let (Some(start), Some(end)) = (parse_hhmm(start), parse_hhmm(end)) else { return false };
    let now = now_minutes();
    if start <= end {
        start <= now && now < end
    } else {
        now >= start || now < end
    }
}

fn is_in_days(days_of_week: &str) -> bool {
    // 0=Mon..6=Sun (chrono: Mon=0)
    let wd = chrono::Local::now().weekday().num_days_from_monday().to_string();
    days_of_week.split(',').any(|d| d.trim() == wd)
}

// ------------------------------------------------------------------ reolink client

fn https_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client")
}

static TOKENS: std::sync::OnceLock<Mutex<HashMap<String, (String, Instant)>>> =
    std::sync::OnceLock::new();

fn token_cache() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stream_host(camera: &Value) -> Option<String> {
    let url = camera["stream_url"].as_str()?;
    let rest = url.split("://").nth(1)?;
    let hostport = rest.split('/').next()?;
    let host = hostport.rsplit('@').next()?;
    let host = host.split(':').next()?;
    if host.is_empty() { None } else { Some(host.to_string()) }
}

async fn get_token(camera: &Value, host: &str) -> Option<String> {
    {
        let cache = token_cache().lock().await;
        if let Some((tok, at)) = cache.get(host) {
            if at.elapsed().as_secs() < 1800 {
                return Some(tok.clone());
            }
        }
    }
    let user = camera["username"].as_str().unwrap_or("admin");
    let pass = camera["password"].as_str().unwrap_or("");
    let body = json!([{"cmd": "Login", "action": 0,
        "param": {"User": {"userName": user, "password": pass}}}]);
    let url = format!("https://{host}/cgi-bin/api.cgi?cmd=Login");
    let resp = https_client().post(&url).json(&body).send().await.ok()?;
    let data: Value = resp.json().await.ok()?;
    if data.as_array().and_then(|a| a.first()).and_then(|d| d.get("code")) == Some(&json!(0)) {
        let token = data[0]["value"]["Token"]["name"].as_str()?.to_string();
        token_cache().lock().await.insert(host.to_string(), (token.clone(), Instant::now()));
        Some(token)
    } else {
        None
    }
}

/// Returns (ai_value, md_state) or None on failure/auth error.
async fn poll_camera(host: &str, token: &str) -> Option<(Value, i64)> {
    let client = https_client();
    let url = format!("https://{host}/cgi-bin/api.cgi?cmd=GetAiState&token={token}");
    let body = json!([{"cmd": "GetAiState", "action": 0, "param": {"channel": 0}}]);
    let resp = client.post(&url).json(&body).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    let first = data.as_array()?.first()?;
    if first.get("code") != Some(&json!(0)) {
        return None;
    }
    let ai_val = first.get("value").cloned().unwrap_or(json!({}));
    let mut md_state = 0i64;
    let url2 = format!("https://{host}/cgi-bin/api.cgi?cmd=GetMdState&token={token}");
    if let Ok(r2) = client.get(&url2).send().await {
        if r2.status().is_success() {
            if let Ok(d2) = r2.json::<Value>().await {
                md_state = d2[0]["value"]["state"].as_i64().unwrap_or(0);
            }
        }
    }
    Some((ai_val, md_state))
}

fn ai_alarm(ai: &Value, field: &str) -> bool {
    let v = &ai[field];
    v.get("support") == Some(&json!(1)) && v.get("alarm_state") == Some(&json!(1))
}

/// Poll motion state. Returns trigger type label if motion detected.
async fn check_motion(camera: &Value) -> Option<String> {
    let host = stream_host(camera)?;
    let token = get_token(camera, &host).await?;
    let (ai_val, md_state) = match poll_camera(&host, &token).await {
        Some(v) => v,
        None => {
            token_cache().lock().await.remove(&host);
            return None;
        }
    };
    let mut triggers: Vec<&str> = Vec::new();
    for (field, label) in [("people", "person"), ("vehicle", "vehicle"), ("dog_cat", "animal")] {
        if ai_alarm(&ai_val, field) {
            triggers.push(label);
        }
    }
    if !triggers.is_empty() {
        return Some(triggers.join(", "));
    }
    if md_state == 1 {
        return Some("motion".into());
    }
    None
}

/// Check alarm condition for a camera (within alarm window + type match).
async fn check_alarm(camera: &Value) -> Option<String> {
    if !is_in_time_window(
        camera["alarm_start"].as_str().unwrap_or("22:00"),
        camera["alarm_end"].as_str().unwrap_or("06:00"),
    ) {
        return None;
    }
    let alarm_types: Vec<String> = camera["alarm_types"]
        .as_str()
        .unwrap_or("people")
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let host = stream_host(camera)?;
    let token = get_token(camera, &host).await?;
    let (ai_val, md_state) = poll_camera(&host, &token).await?;
    let field_map = [
        ("people", "people"), ("vehicle", "vehicle"), ("dog_cat", "dog_cat"),
        ("animal", "dog_cat"), ("person", "people"), ("car", "vehicle"),
    ];
    let mut triggers: Vec<String> = Vec::new();
    for atype in &alarm_types {
        let field = field_map.iter().find(|(a, _)| a == atype).map(|(_, b)| *b).unwrap_or(atype);
        if ai_alarm(&ai_val, field) {
            triggers.push(atype.clone());
        }
    }
    if !triggers.is_empty() {
        return Some(triggers.join(", "));
    }
    if alarm_types.iter().any(|t| t == "motion") && md_state == 1 {
        return Some("motion".into());
    }
    None
}

async fn in_ignore_window(db: &sqlx::SqlitePool, camera_id: i64) -> bool {
    let rows = sqlx::query(
        "SELECT start_time, end_time, days_of_week FROM surveillance_ignore_windows \
         WHERE camera_id = ? AND is_enabled = 1",
    )
    .bind(camera_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in rows {
        let days: String = r.try_get("days_of_week").unwrap_or_default();
        let start: String = r.try_get("start_time").unwrap_or_default();
        let end: String = r.try_get("end_time").unwrap_or_default();
        if is_in_days(&days) && is_in_time_window(&start, &end) {
            return true;
        }
    }
    false
}

// ------------------------------------------------------------------ recorder registry

struct Recorder {
    trigger: Arc<Notify>,
    cancel: CancellationToken,
}

struct Services {
    recorders: HashMap<i64, Arc<Recorder>>, // active motion/alarm recorders
    pollers: HashMap<i64, CancellationToken>, // active motion pollers
    alarm_active: HashSet<i64>,
    webhook_started: bool,
}

static SERVICES: std::sync::OnceLock<Mutex<Services>> = std::sync::OnceLock::new();

fn services() -> &'static Mutex<Services> {
    SERVICES.get_or_init(|| {
        Mutex::new(Services {
            recorders: HashMap::new(),
            pollers: HashMap::new(),
            alarm_active: HashSet::new(),
            webhook_started: false,
        })
    })
}

pub async fn start_services(db: sqlx::SqlitePool) {
    refresh(&db).await;
    let start_webhook = {
        let mut s = services().lock().await;
        let first = !s.webhook_started;
        s.webhook_started = true;
        first
    };
    tokio::spawn(alarm_loop(db.clone()));
    if start_webhook {
        tokio::spawn(webhook_server(db.clone()));
    }
}

/// Reconcile motion pollers with camera config.
pub async fn refresh(db: &sqlx::SqlitePool) {
    let rows = sqlx::query(
        "SELECT * FROM surveillance_cameras \
         WHERE is_enabled = 1 AND recording_enabled = 1 AND recording_mode = 'motion'",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let wanted: HashSet<i64> = rows.iter().map(|r| r.get::<i64, _>("id")).collect();

    {
        let mut s = services().lock().await;
        let current: Vec<i64> = s.pollers.keys().copied().collect();
        for id in current {
            if !wanted.contains(&id) {
                if let Some(tok) = s.pollers.remove(&id) {
                    tok.cancel();
                }
            }
        }
    }
    for r in rows {
        let cam = camera_row_json(&r);
        let id = cam["id"].as_i64().unwrap_or(0);
        let already = {
            let s = services().lock().await;
            s.pollers.contains_key(&id)
        };
        if id > 0 && !already {
            let tok = CancellationToken::new();
            services().lock().await.pollers.insert(id, tok.clone());
            tokio::spawn(motion_poller(db.clone(), cam, tok));
        }
    }
    let count = services().lock().await.pollers.len();
    eprintln!("[surveillance] motion services: {count} motion camera(s)");
}

async fn motion_poller(db: sqlx::SqlitePool, camera: Value, cancel: CancellationToken) {
    let camera = Arc::new(camera);
    let camera_id = camera["id"].as_i64().unwrap_or(0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        if let Some(trigger_type) = check_motion(&camera).await {
            if !in_ignore_window(&db, camera_id).await {
                trigger_recording(&db, camera_id, &trigger_type).await;
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

/// Trigger (or extend) a motion recording for a camera, creating one if none is active.
pub async fn trigger_recording(db: &sqlx::SqlitePool, camera_id: i64, trigger_type: &str) {
    let existing = {
        let s = services().lock().await;
        s.recorders.get(&camera_id).cloned()
    };
    if let Some(rec) = existing {
        if !rec.cancel.is_cancelled() {
            rec.trigger.notify_one();
            spawn_cascade(db.clone(), camera_id);
            return;
        }
    }
    let row = sqlx::query("SELECT * FROM surveillance_cameras WHERE id = ?")
        .bind(camera_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else { return };
    let cam = camera_row_json(&row);
    if cam["is_enabled"] != json!(true) {
        return;
    }
    let username: String = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
        .bind(cam["user_id"].as_i64().unwrap_or(0))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "recordings".into());
    let root = recording_root(db).await;
    let dir: PathBuf = camera_output_dir(&root, &username, cam["name"].as_str().unwrap_or("camera"));
    let rec = Arc::new(Recorder { trigger: Arc::new(Notify::new()), cancel: CancellationToken::new() });
    services().lock().await.recorders.insert(camera_id, rec.clone());
    eprintln!(
        "[surveillance] {trigger_type} detected on {}, starting recording",
        cam["name"].as_str().unwrap_or("camera")
    );
    let rtsp = build_rtsp_url(&cam);
    let label = cam["name"].as_str().unwrap_or("camera").to_string();
    let timeout = cam["motion_timeout"].as_i64().unwrap_or(30).max(5) as u64;
    {
        let rec2 = rec.clone();
        tokio::spawn(async move {
            run_segmented_recording(rtsp, dir, label, rec2.cancel.clone()).await;
        });
    }
    // Idle watcher: stop the recorder after motion_timeout seconds without triggers.
    {
        let rec2 = rec.clone();
        tokio::spawn(async move {
            loop {
                let fired = tokio::select! {
                    _ = rec2.trigger.notified() => true,
                    _ = tokio::time::sleep(Duration::from_secs(timeout)) => false,
                };
                if !fired {
                    rec2.cancel.cancel();
                    break;
                }
            }
        });
    }
    rec.trigger.notify_one();
    spawn_cascade(db.clone(), camera_id);
}

fn spawn_cascade(db: sqlx::SqlitePool, camera_id: i64) {
    tokio::spawn(async move {
        let row = sqlx::query(
            "SELECT trigger_camera_ids, trigger_delay FROM surveillance_cameras WHERE id = ?",
        )
        .bind(camera_id)
        .fetch_optional(&db)
        .await
        .ok()
        .flatten();
        let Some(row) = row else { return };
        let ids: String = row.try_get("trigger_camera_ids").unwrap_or_default();
        let delay: i64 = row.get("trigger_delay");
        let targets: Vec<i64> = ids.split(',').filter_map(|t| t.trim().parse().ok()).collect();
        if targets.is_empty() {
            return;
        }
        if delay > 0 {
            tokio::time::sleep(Duration::from_secs(delay as u64)).await;
        }
        for tid in targets {
            let ok = sqlx::query_scalar::<_, i64>(
                "SELECT is_enabled FROM surveillance_cameras WHERE id = ?",
            )
            .bind(tid)
            .fetch_optional(&db)
            .await
            .ok()
            .flatten();
            if ok == Some(1) {
                trigger_recording(&db, tid, "cascade").await;
            }
        }
    });
}

// ------------------------------------------------------------------ alarm loop

async fn alarm_loop(db: sqlx::SqlitePool) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        let rows = sqlx::query(
            "SELECT * FROM surveillance_cameras WHERE is_enabled = 1 AND alarm_enabled = 1",
        )
        .fetch_all(&db)
        .await
        .unwrap_or_default();
        for r in rows {
            let cam = camera_row_json(&r);
            let id = cam["id"].as_i64().unwrap_or(0);
            let Some(trigger) = check_alarm(&cam).await else {
                services().lock().await.alarm_active.remove(&id);
                continue;
            };
            let already = services().lock().await.alarm_active.contains(&id);
            if already {
                continue;
            }
            if in_ignore_window(&db, id).await {
                continue;
            }
            services().lock().await.alarm_active.insert(id);
            let _ = sqlx::query(
                "INSERT INTO surveillance_alarm_events (camera_id, camera_name, trigger_type) \
                 VALUES (?, ?, ?)",
            )
            .bind(id)
            .bind(cam["name"].as_str().unwrap_or(""))
            .bind(&trigger)
            .execute(&db)
            .await;
            eprintln!("[surveillance] alarm: {} - {}", cam["name"].as_str().unwrap_or(""), trigger);
            trigger_recording(&db, id, "alarm").await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ------------------------------------------------------------------ webhook server

async fn respond(sock: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.shutdown().await;
}

async fn handle_webhook(sock: &mut tokio::net::TcpStream, db: &sqlx::SqlitePool) -> std::io::Result<()> {
    let mut buf = vec![0u8; 4096];
    let n = match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await {
        Ok(Ok(n)) => n,
        _ => 0,
    };
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    let first = req.split("\r\n").next().unwrap_or("").to_string();
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    if method != "POST" {
        respond(sock, "405 Method Not Allowed", "").await;
        return Ok(());
    }
    let path_no_qs = path.split('?').next().unwrap_or("");
    let segs: Vec<&str> = path_no_qs.trim_matches('/').split('/').collect();
    if segs.len() < 4 || segs[0..3] != ["api", "webhook", "motion"] {
        respond(sock, "404 Not Found", "").await;
        return Ok(());
    }
    let Ok(camera_id): Result<i64, _> = segs[3].parse() else {
        respond(sock, "404 Not Found", "").await;
        return Ok(());
    };
    let secret = path
        .split('?')
        .nth(1)
        .and_then(|qs| {
            qs.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                if k == "secret" { Some(v.to_string()) } else { None }
            })
        })
        .unwrap_or_default();
    let expected = match std::env::var("SURVEILLANCE_WEBHOOK_SECRET") {
        Ok(v) => v,
        Err(_) => setting(db, "surveillance_webhook_secret", "").await,
    };
    if expected.is_empty() || secret != expected {
        respond(sock, "403 Forbidden", "").await;
        return Ok(());
    }
    let row = sqlx::query_scalar::<_, i64>("SELECT is_enabled FROM surveillance_cameras WHERE id = ?")
        .bind(camera_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if row == Some(1) && !in_ignore_window(db, camera_id).await {
        trigger_recording(db, camera_id, "webhook").await;
    }
    respond(sock, "200 OK", "{\"status\":\"ok\"}").await;
    Ok(())
}

async fn webhook_server(db: sqlx::SqlitePool) {
    let port: u16 = match std::env::var("SURVEILLANCE_WEBHOOK_PORT").ok().and_then(|p| p.parse().ok()) {
        Some(p) => p,
        None => setting(&db, "surveillance_webhook_port", "8081").await.parse().unwrap_or(8081),
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[surveillance] webhook server failed to bind {addr}: {e}");
            return;
        }
    };
    eprintln!("[surveillance] webhook server listening on {addr}");
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(a) => a,
            Err(_) => continue,
        };
        let db = db.clone();
        tokio::spawn(async move {
            let _ = handle_webhook(&mut sock, &db).await;
        });
    }
}
