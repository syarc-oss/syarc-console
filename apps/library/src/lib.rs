//! Library app: music, audiobooks, ebooks, movies, and photos served from a
//! shared media library tree. Ported from the Python (fastmain) implementation.
//! Media roots and static (art/poster/cover) directories are configurable via
//! LIBRARY_* environment variables.

pub mod audiobook;
pub mod core;
pub mod ebook;
pub mod movie;
pub mod music;
pub mod photo;
pub mod scanner;

use std::collections::HashMap;
use std::path::PathBuf;

use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

pub struct LibraryPlugin;

impl LibraryPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("library", "Library", "/library")
            .icon("bi-collection");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for LibraryPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/library", get(core::landing))
            .route("/library/", get(core::landing))
            .route("/library/browse", get(core::browse))
            .merge(music::router())
            .merge(photo::router())
            .merge(audiobook::router())
            .merge(movie::router())
            .merge(ebook::router())
            .with_state(state)
    }

    /// One-time (cheap, idempotent) migration: move legacy centrally stored
    /// art into the library tree next to the media files, so the central
    /// library_static store can be removed. Rows already pointing at
    /// sibling art are skipped.
    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        use sqlx::Row;
        let mut moved = 0usize;

        // Music albums: <album dir>/cover.jpg
        let rows: Vec<(i64, String, Option<String>)> = sqlx::query(
            "SELECT id, directory_path, image_filename FROM music_albums",
        )
        .fetch_all(db)
        .await?
        .iter()
        .map(|r| {
            (
                r.get::<i64, _>("id"),
                r.try_get::<String, _>("directory_path").unwrap_or_default(),
                r.try_get::<Option<String>, _>("image_filename").ok().flatten(),
            )
        })
        .collect();
        for (id, dir, img) in rows {
            if img.as_deref() == Some(music::ART_SIBLING) {
                continue;
            }
            let cover = std::path::Path::new(&dir).join("cover.jpg");
            if !cover.is_file() {
                let legacy = img.as_deref().map(|f| static_dir("music").join("art").join(f));
                match legacy.as_ref().filter(|l| l.is_file()) {
                    Some(l) if std::fs::copy(l, &cover).is_ok() => {}
                    _ => continue,
                }
            }
            let _ = sqlx::query("UPDATE music_albums SET image_filename = ? WHERE id = ?")
                .bind(music::ART_SIBLING)
                .bind(id)
                .execute(db)
                .await;
            moved += 1;
        }

        // Audiobooks: <book dir>/cover.jpg
        let rows: Vec<(i64, String, Option<String>)> = sqlx::query(
            "SELECT id, directory_path, image_filename FROM audiobook_books",
        )
        .fetch_all(db)
        .await?
        .iter()
        .map(|r| {
            (
                r.get::<i64, _>("id"),
                r.try_get::<String, _>("directory_path").unwrap_or_default(),
                r.try_get::<Option<String>, _>("image_filename").ok().flatten(),
            )
        })
        .collect();
        for (id, dir, img) in rows {
            if img.as_deref() == Some(audiobook::ART_SIBLING) {
                continue;
            }
            let cover = std::path::Path::new(&dir).join("cover.jpg");
            if !cover.is_file() {
                let art = static_dir("audiobook").join("art");
                let legacy = img
                    .as_deref()
                    .map(|f| art.join(f))
                    .or_else(|| {
                        // Pre-refactor extraction could produce .png too.
                        let j = art.join(format!("book_{id}.jpg"));
                        if j.is_file() { Some(j) } else { Some(art.join(format!("book_{id}.png"))) }
                    });
                match legacy.as_ref().filter(|l| l.is_file()) {
                    Some(l) if std::fs::copy(l, &cover).is_ok() => {}
                    _ => continue,
                }
            }
            let _ = sqlx::query("UPDATE audiobook_books SET image_filename = ? WHERE id = ?")
                .bind(audiobook::ART_SIBLING)
                .bind(id)
                .execute(db)
                .await;
            moved += 1;
        }

        // Movies: sidecar <stem>.jpg next to the video
        let rows: Vec<(String, String, Option<String>)> = sqlx::query(
            "SELECT id, file_path, poster_filename FROM movies",
        )
        .fetch_all(db)
        .await?
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("id"),
                r.try_get::<String, _>("file_path").unwrap_or_default(),
                r.try_get::<Option<String>, _>("poster_filename").ok().flatten(),
            )
        })
        .collect();
        for (id, fp, poster) in rows {
            if poster.as_deref() == Some(scanner::POSTER_SIBLING) {
                continue;
            }
            let Some(resolved) = scanner::resolve_video_file(&fp) else { continue };
            let sidecar = std::path::PathBuf::from(scanner::sidecar_poster_path(&resolved));
            if !sidecar.is_file() {
                let legacy = poster.as_deref().map(|p| scanner::movie_posters_dir().join(p));
                match legacy.as_ref().filter(|l| l.is_file()) {
                    Some(l) if std::fs::copy(l, &sidecar).is_ok() => {}
                    _ => continue,
                }
            }
            let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
                .bind(scanner::POSTER_SIBLING)
                .bind(&id)
                .execute(db)
                .await;
            moved += 1;
        }

        // Ebooks: <book>.cover.jpg next to the book file
        let rows: Vec<(i64, String, Option<String>)> = sqlx::query(
            "SELECT id, file_path, cover_filename FROM ebook_books",
        )
        .fetch_all(db)
        .await?
        .iter()
        .map(|r| {
            (
                r.get::<i64, _>("id"),
                r.try_get::<String, _>("file_path").unwrap_or_default(),
                r.try_get::<Option<String>, _>("cover_filename").ok().flatten(),
            )
        })
        .collect();
        for (id, fp, cover) in rows {
            if cover.as_deref() == Some(ebook::COVER_SIBLING) {
                continue;
            }
            let sibling = ebook::sibling_cover_path(&fp);
            if !sibling.is_file() {
                let legacy = cover.as_deref().map(|c| ebook::covers_dir().join(c));
                match legacy.as_ref().filter(|l| l.is_file()) {
                    Some(l) if std::fs::copy(l, &sibling).is_ok() => {}
                    _ => continue,
                }
            }
            let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                .bind(ebook::COVER_SIBLING)
                .bind(id)
                .execute(db)
                .await;
            moved += 1;
        }

        if moved > 0 {
            println!("[library] migrated {moved} art files into the library tree");
        }
        Ok(())
    }
}

// ------------------------------------------------------------- shared utils

pub(crate) fn page(state: &SharedState, template: &str, ctx: Value) -> Response {
    Html(state.templates.render(template, ctx)).into_response()
}

pub(crate) fn err_json(status: axum::http::StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"detail": msg}))).into_response()
}

pub(crate) fn form_map(raw: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub(crate) fn fv(form: &HashMap<String, String>, key: &str) -> Option<String> {
    form.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub(crate) fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(crate) fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn is_super(u: &syarc_plugin_api::User) -> bool {
    u.role == "admin"
}

pub(crate) fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

pub(crate) fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

pub(crate) fn root(kind: &str) -> String {
    match kind {
        "music" => env_or("LIBRARY_MUSIC_ROOT", "/usr/local/library/music"),
        "audiobook" => env_or("LIBRARY_AUDIOBOOK_ROOT", "/usr/local/library/audiobooks"),
        "ebook" => env_or("LIBRARY_EBOOK_ROOT", "/usr/local/library/ebooks"),
        "movie" => env_or("LIBRARY_MOVIES_ROOT", "/usr/local/library/movies"),
        "photo" => env_or("LIBRARY_PHOTOS_ROOT", "/usr/local/library/photos"),
        _ => String::new(),
    }
}

/// Writable dir for album art / posters / covers / thumbnails.
pub(crate) fn static_dir(kind: &str) -> PathBuf {
    let base = env_or(
        "LIBRARY_STATIC_ROOT",
        "library_static",
    );
    PathBuf::from(base).join(kind)
}

pub(crate) fn human_size(bytes: i64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1024.0 && i < units.len() - 1 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, units[0])
    } else {
        format!("{:.1} {}", size, units[i])
    }
}

/// All active users except `exclude`, for share pickers.
pub(crate) async fn all_users(db: &syarc_plugin_api::Db, exclude: i64) -> Vec<Value> {
    sqlx::query("SELECT id, username, email FROM users WHERE is_active = 1 AND id != ? ORDER BY username AND username NOT LIKE 'mesh-%'")
        .bind(exclude)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "username": r.try_get::<String,_>("username").unwrap_or_default(),
                "email": r.try_get::<String,_>("email").unwrap_or_default(),
            })
        })
        .collect()
}

/// Shared-with lists for owner views: [{share:{id}, shared_user:{...}}]
pub(crate) async fn shared_users(
    db: &syarc_plugin_api::Db,
    table: &str,
    fk_col: &str,
    resource_id: &str,
) -> Vec<Value> {
    let sql = format!(
        "SELECT sh.id AS share_id, u.id, u.username, u.email FROM {table} sh \
         JOIN users u ON u.id = sh.shared_with_user_id WHERE sh.{fk_col} = ?"
    );
    sqlx::query(&sql)
        .bind(resource_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "share": {"id": r.get::<i64,_>("share_id")},
                "shared_user": {
                    "id": r.get::<i64,_>("id"),
                    "username": r.try_get::<String,_>("username").unwrap_or_default(),
                    "email": r.try_get::<String,_>("email").unwrap_or_default(),
                },
            })
        })
        .collect()
}

/// Serve a file with HTTP Range support (audio/video streaming).
pub(crate) async fn serve_file(path: &str, range_header: Option<&str>) -> Response {
    use tokio::io::{AsyncSeekExt, SeekFrom};

    let Ok(meta) = tokio::fs::metadata(path).await else {
        return err_json(axum::http::StatusCode::NOT_FOUND, "file not found");
    };
    let len = meta.len();
    let ctype = content_type(path);

    let (start, end) = match range_header.and_then(parse_range) {
        Some((s, e)) => (s, e.min(len.saturating_sub(1))),
        None => (0u64, len.saturating_sub(1)),
    };
    if start > end || start >= len {
        return (
            axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
            [("content-range", format!("bytes */{len}"))],
            "range not satisfiable",
        )
            .into_response();
    }

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return err_json(axum::http::StatusCode::NOT_FOUND, "file not found"),
    };
    use tokio::io::AsyncReadExt as _;
    if file.seek(SeekFrom::Start(start)).await.is_err() {
        return err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "seek failed");
    }
    let count = end - start + 1;
    let stream = tokio_util::io::ReaderStream::with_capacity(
        file.take(count),
        1024 * 256,
    );
    let body = axum::body::Body::from_stream(stream);

    let mut resp = Response::new(body);
    let headers = resp.headers_mut();
    headers.insert("content-type", ctype.parse().unwrap());
    headers.insert("accept-ranges", "bytes".parse().unwrap());
    headers.insert(
        "content-range",
        format!("bytes {start}-{end}/{len}").parse().unwrap(),
    );
    headers.insert("content-length", (end - start + 1).to_string().parse().unwrap());
    if range_header.is_some() {
        *resp.status_mut() = axum::http::StatusCode::PARTIAL_CONTENT;
    }
    resp
}

fn parse_range(h: &str) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let start: u64 = s.parse().ok()?;
    let end: u64 = if e.is_empty() {
        u64::MAX
    } else {
        e.parse().ok()?
    };
    Some((start, end.max(start)))
}

fn content_type(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "mp4" | "m4b" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub(crate) async fn scan_music(
    db: &syarc_plugin_api::Db,
    root: Option<String>,
    force: bool,
) {
    scanner::scan_music(db, root, force).await;
}

pub(crate) fn redirect(to: &str) -> Response {
    Redirect::to(to).into_response()
}
