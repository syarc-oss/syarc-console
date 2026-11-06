use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sqlx::Row;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{build_rtsp_url, setting, slugify};

const SEGMENT_SECONDS: i64 = 300;

pub async fn recording_root(db: &sqlx::SqlitePool) -> String {
    if let Ok(env) = std::env::var("SURVEILLANCE_RECORDING_ROOT") {
        if !env.is_empty() {
            return env;
        }
    }
    setting(
        db,
        "surveillance_recording_root",
        "/usr/local/recordings/surveillance",
    )
    .await
}

pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '/' && *c != '\\' && *c != '\0')
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_string()
}

async fn owner_username(db: &sqlx::SqlitePool, user_id: i64) -> String {
    sqlx::query_scalar::<_, String>("SELECT username FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "recordings".into())
}

pub fn camera_output_dir(root: &str, username: &str, camera_name: &str) -> PathBuf {
    Path::new(root).join(username).join(slugify(camera_name))
}

fn ffmpeg_segment_args(rtsp_url: &str, pattern: &str) -> Vec<String> {
    [
        "-hide_banner", "-loglevel", "error",
        "-rtsp_transport", "tcp",
        "-i", rtsp_url,
        "-c", "copy", "-an",
        "-f", "segment",
        "-segment_time", &SEGMENT_SECONDS.to_string(),
        "-reset_timestamps", "1",
        "-segment_format", "mp4",
        "-segment_format_options", "movflags=+frag_keyframe+empty_moov+default_base_moof",
        "-strftime", "1", pattern,
    ]
    .into_iter()
    .map(|s| s.to_string())
    .collect()
}

/// Run a segmented ffmpeg recording until the token is cancelled.
/// Restarts ffmpeg if it crashes. Used for both continuous and motion modes.
pub async fn run_segmented_recording(
    rtsp_url: String,
    out_dir: PathBuf,
    label: String,
    cancel: CancellationToken,
) {
    let _ = std::fs::create_dir_all(&out_dir);
    let pattern = out_dir.join("%Y%m%d_%H%M%S.mp4");
    let pattern = pattern.to_string_lossy().to_string();
    while !cancel.is_cancelled() {
        let args = ffmpeg_segment_args(&rtsp_url, &pattern);
        let child = tokio::process::Command::new("ffmpeg")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[surveillance] failed to spawn ffmpeg for {label}: {e}");
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(10)) => continue,
                }
            }
        };
        eprintln!("[surveillance] recording started for {label} in {}", out_dir.display());
        tokio::select! {
            status = child.wait() => {
                let code = status.ok().and_then(|s| s.code());
                if cancel.is_cancelled() {
                    eprintln!("[surveillance] recording stopped for {label}");
                    break;
                }
                eprintln!("[surveillance] ffmpeg for {label} exited ({code:?}), restarting in 10s");
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                }
            }
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                eprintln!("[surveillance] recording stopped for {label}");
                break;
            }
        }
    }
}

// ------------------------------------------------------------------ manager

struct Manager {
    recorders: HashMap<i64, CancellationToken>,
    retention_started: bool,
}

static MANAGER: std::sync::OnceLock<Mutex<Manager>> = std::sync::OnceLock::new();

fn manager() -> &'static Mutex<Manager> {
    MANAGER.get_or_init(|| Mutex::new(Manager { recorders: HashMap::new(), retention_started: false }))
}

pub async fn start_manager(db: sqlx::SqlitePool) {
    refresh(&db).await;
    let mut m = manager().lock().await;
    if !m.retention_started {
        m.retention_started = true;
        drop(m);
        tokio::spawn(retention_loop(db));
    }
}

/// Re-read cameras and reconcile the set of continuous recorders.
pub async fn refresh(db: &sqlx::SqlitePool) {
    let rows = sqlx::query(
        "SELECT * FROM surveillance_cameras \
         WHERE is_enabled = 1 AND recording_enabled = 1 AND recording_mode = 'continuous'",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let mut wanted: HashMap<i64, (String, PathBuf, String)> = HashMap::new(); // id -> (rtsp, out_dir, label)
    for r in &rows {
        let cam = crate::camera_row_json(r);
        let id = cam["id"].as_i64().unwrap_or(0);
        let username = owner_username(db, cam["user_id"].as_i64().unwrap_or(0)).await;
        let root = recording_root(db).await;
        let dir = camera_output_dir(&root, &username, cam["name"].as_str().unwrap_or("camera"));
        wanted.insert(
            id,
            (build_rtsp_url(&cam), dir, cam["name"].as_str().unwrap_or("camera").to_string()),
        );
    }
    let mut m = manager().lock().await;
    let ids: Vec<i64> = m.recorders.keys().copied().collect();
    for id in ids {
        if !wanted.contains_key(&id) {
            if let Some(tok) = m.recorders.remove(&id) {
                tok.cancel();
            }
        }
    }
    for (id, (rtsp, dir, label)) in wanted {
        if let std::collections::hash_map::Entry::Vacant(e) = m.recorders.entry(id) {
            let tok = CancellationToken::new();
            e.insert(tok.clone());
            tokio::spawn(run_segmented_recording(rtsp, dir, label, tok));
        }
    }
    eprintln!("[surveillance] recording manager: {} continuous camera(s)", m.recorders.len());
}

async fn retention_loop(db: sqlx::SqlitePool) {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        if let Err(e) = retention_sweep(&db).await {
            eprintln!("[surveillance] retention cleanup failed: {e}");
        }
    }
}

async fn retention_sweep(db: &sqlx::SqlitePool) -> std::io::Result<()> {
    let rows = sqlx::query(
        "SELECT name, user_id, recording_retention_days FROM surveillance_cameras \
         WHERE recording_enabled = 1",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let root = recording_root(db).await;
    for r in rows {
        let name: String = r.try_get("name").unwrap_or_default();
        let user_id: i64 = r.get("user_id");
        let retention: i64 = r.get::<i64, _>("recording_retention_days").max(1);
        let username = owner_username(db, user_id).await;
        let dir = camera_output_dir(&root, &username, &name);
        sweep_dir(&dir, retention);
    }
    Ok(())
}

fn sweep_dir(dir: &Path, retention_days: i64) {
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days as u64 * 86400);
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("mp4") {
            if let Ok(meta) = e.metadata() {
                if meta.modified().map(|m| m < cutoff).unwrap_or(false) {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
}

// ------------------------------------------------------------------ clipping

pub async fn ffmpeg_clip(src: &Path, dest: &Path, start: f64, end: Option<f64>) -> Result<(), String> {
    let src = src.to_string_lossy().to_string();
    let dest = dest.to_string_lossy().to_string();
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-hide_banner").arg("-loglevel").arg("error");
    if start > 0.0 {
        cmd.arg("-ss").arg(format!("{start}"));
    }
    cmd.arg("-i").arg(&src);
    if let Some(end) = end {
        let dur = (end - start).max(0.0);
        cmd.arg("-t").arg(format!("{dur}"));
    }
    cmd.arg("-c").arg("copy").arg("-avoid_negative_ts").arg("make_zero").arg(&dest);
    let out = cmd.output().await.map_err(|e| format!("ffmpeg failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ffmpeg exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}
