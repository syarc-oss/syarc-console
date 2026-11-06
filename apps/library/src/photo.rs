//! Photo sub-app: albums (with nested sub-albums), photo import/upload,
//! thumbnails, sharing, public links, and disk scanning.
//! Ported from fastmain's library photo router.

use axum::extract::{Multipart, Path, Query, RawForm, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::path::{Path as FsPath, PathBuf};

use crate::{
    err_json, form_map, fv, is_super, new_uuid, now_str, page, redirect, root, serve_file,
    shared_users, RequireUser, SharedState,
};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/library/photo", get(index))
        .route("/library/photo/", get(index))
        .route("/library/photo/browse/users", get(browse_users))
        .route("/library/photo/browse/users/{user_id}", get(browse_user_albums))
        .route("/library/photo/browse/albums", get(browse_albums))
        .route("/library/photo/browse/photos", get(browse_photos))
        .route("/library/photo/search", get(search_page))
        .route("/library/photo/album/new", get(new_album_page).post(create_album))
        .route("/library/photo/album/{album_id}", get(view_album))
        .route("/library/photo/album/{album_id}/photo-ids", get(album_photo_ids))
        .route("/library/photo/album/{album_id}/edit", get(edit_album_page).post(update_album))
        .route("/library/photo/album/{album_id}/toggle-visibility", post(toggle_visibility))
        .route("/library/photo/album/{album_id}/clear-db", post(clear_album_db))
        .route("/library/photo/album/{album_id}/delete", post(delete_album))
        .route("/library/photo/album/{album_id}/share", post(share_album))
        .route("/library/photo/album/{album_id}/revoke/{share_id}", post(revoke_share))
        .route("/library/photo/album/{album_id}/upload", get(upload_page).post(upload_photos))
        .route("/library/photo/album/{album_id}/import-path", post(import_from_path))
        .route("/library/photo/album/{album_id}/rescan", post(rescan_album))
        .route("/library/photo/album/{album_id}/regenerate-thumbnails", post(regenerate_thumbnails))
        .route("/library/photo/album/{album_id}/remove-thumbnail-photos", post(remove_thumbnail_photos))
        .route("/library/photo/photo/{photo_id}", get(view_photo))
        .route("/library/photo/photo/{photo_id}/file", get(serve_photo_file))
        .route("/library/photo/photo/{photo_id}/thumbnail", get(serve_photo_thumbnail))
        .route("/library/photo/photo/{photo_id}/edit", post(update_photo))
        .route("/library/photo/photo/{photo_id}/delete", post(delete_photo))
        .route("/library/photo/photo/{photo_id}/move", post(move_photo))
        .route("/library/photo/photo/{photo_id}/remove-public", post(user_remove_photo_public))
        .route("/library/photo/photo/{photo_id}/make-public", post(user_make_photo_public))
        .route("/library/photo/admin", get(admin_page))
        .route("/library/photo/admin/photo/{photo_id}/ban", post(admin_ban_photo))
        .route("/library/photo/admin/photo/{photo_id}/unban", post(admin_unban_photo))
        .route("/library/photo/admin/photo/{photo_id}/remove-public", post(admin_remove_public_photo))
        .route("/library/photo/admin/album/{album_id}/remove-public", post(admin_remove_public_album))
        .route("/library/photo/add", post(add_photos_quick))
        .route("/library/photo/scan/my-library", post(scan_my_photo_library))
        .route("/library/photo/scan/full-rescan", post(full_rescan_photos))
        .route("/library/photo/scan/force", post(trigger_photo_force_scan))
}

const THUMBNAIL_MAX_DIMENSION: u32 = 400;
const THUMBNAIL_MAX_FILESIZE: u64 = 100 * 1024;

fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}
fn err403(msg: &str) -> Response {
    err_json(axum::http::StatusCode::FORBIDDEN, msg)
}

fn allowed_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "heic" | "heif"
    )
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

fn public_dir() -> PathBuf {
    PathBuf::from(root("photo")).join("public")
}

fn safe_dest_path(dest_dir: &str, filename: &str) -> PathBuf {
    let dest = FsPath::new(dest_dir).join(filename);
    if !dest.exists() {
        return dest;
    }
    let (base, ext) = filename
        .rsplit_once('.')
        .map(|(b, e)| (b.to_string(), format!(".{e}")))
        .unwrap_or((filename.to_string(), String::new()));
    let mut n = 1;
    loop {
        let cand = FsPath::new(dest_dir).join(format!("{base}_{n}{ext}"));
        if !cand.exists() {
            return cand;
        }
        n += 1;
    }
}

fn get_image_dimensions(path: &str) -> (Option<i64>, Option<i64>) {
    image::ImageReader::open(path)
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .map(|(w, h)| (Some(w as i64), Some(h as i64)))
        .unwrap_or((None, None))
}

fn is_likely_thumbnail(file_path: &str, file_size: i64, width: Option<i64>, height: Option<i64>) -> bool {
    if file_size > 0 && (file_size as u64) < THUMBNAIL_MAX_FILESIZE {
        return true;
    }
    if let (Some(w), Some(h)) = (width, height) {
        if w <= THUMBNAIL_MAX_DIMENSION as i64 && h <= THUMBNAIL_MAX_DIMENSION as i64 {
            return true;
        }
    }
    let basename = FsPath::new(file_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    ["thumb_", "tn_", "thumb-", "tn-", "preview_", "preview-"]
        .iter()
        .any(|p| basename.starts_with(p))
}

fn generate_thumbnail(file_path: &str, thumb_dir: &str, photo_id: &str) -> Option<String> {
    if matches!(ext_of(file_path).as_str(), "heic" | "heif") {
        return None; // HEIC thumbnails unsupported in this build
    }
    let img = image::open(file_path).ok()?;
    let rgb = img.thumbnail(300, 300).to_rgb8();
    std::fs::create_dir_all(thumb_dir).ok()?;
    let name = format!("thumb_{}.jpg", photo_id.replace('-', ""));
    let out = FsPath::new(thumb_dir).join(&name);
    let f = std::fs::File::create(&out).ok()?;
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(f, 80);
    rgb.write_with_encoder(enc).ok()?;
    Some(name)
}

fn album_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "user_id": r.get::<i64,_>("user_id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "normalized_name": r.try_get::<String,_>("normalized_name").unwrap_or_default(),
        "description": r.try_get::<String,_>("description").ok(),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "directory_path": r.try_get::<String,_>("directory_path").unwrap_or_default(),
        "cover_photo_id": r.try_get::<String,_>("cover_photo_id").ok(),
        "photo_count": r.get::<i64,_>("photo_count"),
        "parent_id": r.try_get::<String,_>("parent_id").ok(),
        "created_at": r.try_get::<String,_>("created_at").ok(),
    })
}

fn photo_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "album_id": r.get::<String,_>("album_id"),
        "filename": r.try_get::<String,_>("filename").unwrap_or_default(),
        "file_path": r.try_get::<String,_>("file_path").unwrap_or_default(),
        "file_size": r.get::<i64,_>("file_size"),
        "file_type": r.try_get::<String,_>("file_type").unwrap_or_default(),
        "width": r.try_get::<i64,_>("width").ok(),
        "height": r.try_get::<i64,_>("height").ok(),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "is_banned": r.get::<i64,_>("is_banned") == 1,
        "caption": r.try_get::<String,_>("caption").ok(),
        "annotation": r.try_get::<String,_>("annotation").ok(),
        "taken_at": r.try_get::<String,_>("taken_at").ok(),
        "thumbnail_filename": r.try_get::<String,_>("thumbnail_filename").ok(),
        "created_at": r.try_get::<String,_>("created_at").ok(),
    })
}

async fn get_album(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM photo_albums WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| album_row(&r))
}

async fn get_photo(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM photos WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| photo_row(&r))
}

async fn user_has_album_access(db: &syarc_plugin_api::Db, album_id: &str, user_id: i64) -> bool {
    let Ok(Some(r)) = sqlx::query(
        "SELECT user_id, is_public FROM photo_albums WHERE id = ?",
    )
    .bind(album_id)
    .fetch_optional(db)
    .await
    else {
        return false;
    };
    if r.get::<i64,_>("user_id") == user_id {
        return true;
    }
    if r.get::<i64,_>("is_public") == 1 {
        return true;
    }
    sqlx::query(
        "SELECT 1 FROM photo_album_shares WHERE album_id = ? AND shared_with_user_id = ?",
    )
    .bind(album_id)
    .bind(user_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some()
}

async fn user_is_album_owner(db: &syarc_plugin_api::Db, album_id: &str, user_id: i64) -> bool {
    sqlx::query("SELECT 1 FROM photo_albums WHERE id = ? AND user_id = ?")
        .bind(album_id)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn make_album_public(db: &syarc_plugin_api::Db, album_id: &str, owner_username: &str, album_name: &str) {
    let _ = sqlx::query("UPDATE photo_albums SET is_public = 1, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(album_id)
        .execute(db)
        .await;
    let link = public_dir().join(format!("{owner_username}_{album_name}"));
    if link.is_symlink() {
        let _ = std::fs::remove_file(&link);
    }
    let _ = sqlx::query(
        "UPDATE photos SET is_public = 1 WHERE album_id = ? AND is_banned = 0",
    )
    .bind(album_id)
    .execute(db)
    .await;
}

async fn make_album_private(db: &syarc_plugin_api::Db, album_id: &str) {
    let _ = sqlx::query("UPDATE photo_albums SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(album_id)
        .execute(db)
        .await;
    if let Some(r) = sqlx::query(
        "SELECT u.username, a.name FROM photo_albums a JOIN users u ON u.id = a.user_id WHERE a.id = ?",
    )
    .bind(album_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    {
        let link = public_dir().join(format!(
            "{}_{}",
            r.try_get::<String,_>("username").unwrap_or_default(),
            r.try_get::<String,_>("name").unwrap_or_default()
        ));
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
    let _ = sqlx::query("UPDATE photos SET is_public = 0 WHERE album_id = ?")
        .bind(album_id)
        .execute(db)
        .await;
}

async fn refresh_album_count(db: &syarc_plugin_api::Db, album_id: &str) -> i64 {
    let count = sqlx::query("SELECT COUNT(*) AS c FROM photos WHERE album_id = ?")
        .bind(album_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    let _ = sqlx::query("UPDATE photo_albums SET photo_count = ? WHERE id = ?")
        .bind(count)
        .bind(album_id)
        .execute(db)
        .await;
    count
}

/// Insert a photo row for a file already on disk.
async fn insert_photo(
    db: &syarc_plugin_api::Db,
    album_id: &str,
    album_is_public: bool,
    filename: &str,
    file_path: &str,
    thumb_dir: &str,
) {
    let photo_id = new_uuid();
    let file_size = std::fs::metadata(file_path).map(|m| m.len() as i64).unwrap_or(0);
    let (width, height) = get_image_dimensions(file_path);
    let thumb = generate_thumbnail(file_path, thumb_dir, &photo_id);
    let _ = sqlx::query(
        "INSERT INTO photos (id, album_id, filename, file_path, file_size, file_type, \
         width, height, is_public, thumbnail_filename, created_at, updated_at) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&photo_id)
    .bind(album_id)
    .bind(filename)
    .bind(file_path)
    .bind(file_size)
    .bind(ext_of(filename))
    .bind(width)
    .bind(height)
    .bind(album_is_public as i64)
    .bind(thumb)
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;
}

// ------------------------------------------------------------------- pages

async fn index(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let own_albums: Vec<Value> = sqlx::query(
        "SELECT * FROM photo_albums WHERE user_id = ? ORDER BY name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(album_row)
    .collect();

    let shared_albums: Vec<Value> = match sqlx::query(
        "SELECT a.*, sh.created_at AS shared_at FROM photo_album_shares sh \
         JOIN photo_albums a ON a.id = sh.album_id \
         WHERE sh.shared_with_user_id = ? ORDER BY a.name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    {
        Ok(rows) => rows
            .iter()
            .map(|r| {
                let mut a = album_row(r);
                a["shared_at"] = json!(r.try_get::<String,_>("shared_at").ok());
                a
            })
            .collect(),
        Err(_) => vec![],
    };

    let public_albums: Vec<Value> = sqlx::query(
        "SELECT * FROM photo_albums WHERE is_public = 1 ORDER BY name",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(album_row)
    .collect();

    let total_photos = sqlx::query(
        "SELECT COUNT(*) AS c FROM photos p JOIN photo_albums a ON p.album_id = a.id WHERE a.user_id = ?",
    )
    .bind(user.id)
    .fetch_one(db)
    .await
    .map(|r| r.get::<i64,_>("c"))
    .unwrap_or(0);

    page(
        &state,
        "photo/index.html",
        json!({
            "user": crate::core::user_json(&user),
            "own_albums": own_albums,
            "shared_albums": shared_albums,
            "public_albums": public_albums,
            "total_photos": total_photos,
            "full_access": is_super(&user),
        }),
    )
}

#[derive(Deserialize)]
struct PageQ {
    page: Option<i64>,
    per_page: Option<i64>,
    #[serde(default)]
    letter: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    parent_id: Option<String>,
}
fn clamp_page(q: &PageQ, default_per: i64, min_per: i64, max_per: i64) -> (i64, i64) {
    let per = q.per_page.unwrap_or(default_per).clamp(min_per, max_per);
    (q.page.unwrap_or(1).max(1), per)
}

async fn browse_users(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let sql = if is_super(&user) {
        "SELECT a.user_id, u.username, COUNT(a.id) AS album_count \
         FROM photo_albums a JOIN users u ON u.id = a.user_id \
         GROUP BY a.user_id, u.username ORDER BY u.username"
    } else {
        "SELECT a.user_id, u.username, COUNT(a.id) AS album_count \
         FROM photo_albums a JOIN users u ON u.id = a.user_id \
         WHERE a.is_public = 1 \
         GROUP BY a.user_id, u.username ORDER BY u.username"
    };
    let users: Vec<Value> = sqlx::query(sql)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "user_id": r.get::<i64,_>("user_id"),
                "username": r.try_get::<String,_>("username").unwrap_or_default(),
                "album_count": r.get::<i64,_>("album_count"),
            })
        })
        .collect();
    page(
        &state,
        "photo/browse_users.html",
        json!({
            "user": crate::core::user_json(&user),
            "users_with_albums": users,
        }),
    )
}

async fn browse_user_albums(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(user_id): Path<i64>,
) -> Response {
    let db = &state.db;
    let Some(target) = sqlx::query("SELECT id, username FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    else {
        return err404("User not found");
    };
    let target_username = target.try_get::<String,_>("username").unwrap_or_default();

    let albums: Vec<Value> = if user_id == user.id || is_super(&user) {
        sqlx::query("SELECT * FROM photo_albums WHERE user_id = ? ORDER BY name")
            .bind(user_id)
            .fetch_all(db)
            .await
    } else {
        sqlx::query(
            "SELECT * FROM photo_albums WHERE user_id = ? AND ( \
              is_public = 1 OR id IN \
              (SELECT album_id FROM photo_album_shares WHERE shared_with_user_id = ?) \
             ) ORDER BY name",
        )
        .bind(user_id)
        .bind(user.id)
        .fetch_all(db)
        .await
    }
    .unwrap_or_default()
    .iter()
    .map(album_row)
    .collect();

    page(
        &state,
        "photo/browse_user_albums.html",
        json!({
            "user": crate::core::user_json(&user),
            "target_user": {"id": user_id, "username": target_username},
            "albums": albums,
        }),
    )
}

async fn browse_albums(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<PageQ>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page) = clamp_page(&q, 24, 12, 120);

    let ids: Vec<String> = sqlx::query(
        "SELECT id FROM photo_albums WHERE user_id = ? \
         UNION SELECT album_id FROM photo_album_shares WHERE shared_with_user_id = ? \
         UNION SELECT id FROM photo_albums WHERE is_public = 1",
    )
    .bind(user.id)
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| r.get::<String,_>("id"))
    .collect();

    let total = ids.len() as i64;
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_no - 1) * per_page;

    let mut albums: Vec<Value> = Vec::new();
    if !ids.is_empty() {
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT a.*, u.username AS owner_username FROM photo_albums a \
             JOIN users u ON u.id = a.user_id WHERE a.id IN ({placeholders}) \
             ORDER BY a.name LIMIT ? OFFSET ?"
        );
        let mut query = sqlx::query(&sql);
        for id in &ids {
            query = query.bind(id);
        }
        query = query.bind(per_page).bind(offset);
        if let Ok(rows) = query.fetch_all(db).await {
            albums = rows
                .iter()
                .map(|r| {
                    let mut a = album_row(r);
                    a["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
                    a
                })
                .collect();
        }
    }

    page(
        &state,
        "photo/browse_albums.html",
        json!({
            "user": crate::core::user_json(&user),
            "albums": albums,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
        }),
    )
}

async fn browse_photos(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<PageQ>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page) = clamp_page(&q, 48, 12, 200);
    let full_access = is_super(&user);

    let mut where_clause = String::new();
    if !full_access {
        where_clause = " AND (p.album_id IN (SELECT id FROM photo_albums WHERE user_id = ?1) \
            OR p.album_id IN (SELECT album_id FROM photo_album_shares WHERE shared_with_user_id = ?1) \
            OR (p.is_public = 1 AND p.is_banned = 0))".to_string();
    }
    let letter = q
        .letter
        .as_deref()
        .filter(|l| l.chars().count() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|l| l.to_uppercase());
    if letter.is_some() {
        where_clause.push_str(" AND p.filename LIKE ?2");
    }

    let sql_count = format!(
        "SELECT COUNT(*) AS c FROM photos p JOIN photo_albums a ON p.album_id = a.id WHERE 1=1 {where_clause}"
    );
    let mut cquery = sqlx::query(&sql_count).bind(user.id);
    if letter.is_some() {
        cquery = cquery.bind(format!("{}%", letter.clone().unwrap()));
    }
    let total = cquery.fetch_one(db).await.map(|r| r.get::<i64,_>("c")).unwrap_or(0);
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_no - 1) * per_page;

    let sql = format!(
        "SELECT p.*, a.name AS album_name, u.username AS owner_username \
         FROM photos p JOIN photo_albums a ON p.album_id = a.id \
         JOIN users u ON u.id = a.user_id WHERE 1=1 {where_clause} \
         ORDER BY p.created_at DESC LIMIT ?3 OFFSET ?4"
    );
    let mut pquery = sqlx::query(&sql).bind(user.id);
    if let Some(ref l) = letter {
        pquery = pquery.bind(format!("{l}%"));
    }
    let rows = pquery
        .bind(per_page)
        .bind(offset)
        .fetch_all(db)
        .await
        .unwrap_or_default();

    let photos: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut p = photo_row(r);
            p["album_name"] = json!(r.try_get::<String,_>("album_name").unwrap_or_default());
            p["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
            p
        })
        .collect();

    page(
        &state,
        "photo/browse_photos.html",
        json!({
            "user": crate::core::user_json(&user),
            "photos": photos,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
            "current_letter": letter,
            "full_access": full_access,
        }),
    )
}

async fn search_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<PageQ>,
) -> Response {
    let db = &state.db;
    let query = q.q.as_deref().unwrap_or("").trim().to_string();
    let mut results: Vec<Value> = Vec::new();

    if !query.is_empty() {
        let ql = query.to_lowercase();
        let pat = format!("%{query}%");

        if let Ok(rows) = sqlx::query(
            "SELECT a.*, u.username AS owner_username FROM photo_albums a \
             JOIN users u ON u.id = a.user_id WHERE a.id IN ( \
               SELECT id FROM photo_albums WHERE user_id = ?1 \
               UNION SELECT album_id FROM photo_album_shares WHERE shared_with_user_id = ?1 \
               UNION SELECT id FROM photo_albums WHERE is_public = 1) \
             AND (a.normalized_name LIKE ?2 OR a.description LIKE ?2 OR u.username LIKE ?2) \
             ORDER BY a.name",
        )
        .bind(user.id)
        .bind(&pat)
        .fetch_all(db)
        .await
        {
            for r in &rows {
                let mut a = album_row(r);
                a["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
                results.push(json!({"type": "album", "album": a, "owner_username": a["owner_username"]}));
            }
        }

        if let Ok(rows) = sqlx::query(
            "SELECT p.*, a.id AS a_id, a.name AS album_name, a.user_id AS a_user_id, \
              a.normalized_name AS a_norm, a.description AS a_desc, a.is_public AS a_pub, \
              a.directory_path AS a_dir, a.cover_photo_id AS a_cover, a.photo_count AS a_count, \
              a.parent_id AS a_parent, a.created_at AS a_created, \
              u.username AS owner_username \
             FROM photos p JOIN photo_albums a ON p.album_id = a.id \
             JOIN users u ON u.id = a.user_id WHERE a.id IN ( \
               SELECT id FROM photo_albums WHERE user_id = ?1 \
               UNION SELECT album_id FROM photo_album_shares WHERE shared_with_user_id = ?1 \
               UNION SELECT id FROM photo_albums WHERE is_public = 1) \
             AND (p.caption LIKE ?2 OR p.filename LIKE ?2) \
             ORDER BY p.created_at DESC",
        )
        .bind(user.id)
        .bind(&pat)
        .fetch_all(db)
        .await
        {
            let _ = ql;
            for r in &rows {
                let album = json!({
                    "id": r.get::<String,_>("a_id"),
                    "user_id": r.get::<i64,_>("a_user_id"),
                    "name": r.try_get::<String,_>("album_name").unwrap_or_default(),
                    "normalized_name": r.try_get::<String,_>("a_norm").unwrap_or_default(),
                    "description": r.try_get::<String,_>("a_desc").ok(),
                    "is_public": r.get::<i64,_>("a_pub") == 1,
                    "directory_path": r.try_get::<String,_>("a_dir").unwrap_or_default(),
                    "cover_photo_id": r.try_get::<String,_>("a_cover").ok(),
                    "photo_count": r.get::<i64,_>("a_count"),
                    "parent_id": r.try_get::<String,_>("a_parent").ok(),
                    "created_at": r.try_get::<String,_>("a_created").ok(),
                });
                results.push(json!({
                    "type": "photo",
                    "photo": photo_row(r),
                    "album": album,
                    "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
                }));
            }
        }
    }

    page(
        &state,
        "photo/search.html",
        json!({
            "user": crate::core::user_json(&user),
            "query": query,
            "results": results,
            "total": results.len(),
        }),
    )
}

// ------------------------------------------------------------- album CRUD

async fn new_album_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<PageQ>,
) -> Response {
    let own_albums: Vec<Value> = sqlx::query(
        "SELECT * FROM photo_albums WHERE user_id = ? AND parent_id IS NULL ORDER BY name",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .iter()
    .map(album_row)
    .collect();

    page(
        &state,
        "photo/album_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "album": Value::Null,
            "mode": "new",
            "own_albums": own_albums,
            "preselected_parent_id": q.parent_id,
        }),
    )
}

async fn create_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(name) = fv(&form, "name") else {
        return redirect("/library/photo/album/new");
    };
    let norm_name = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");

    let dup = sqlx::query(
        "SELECT 1 FROM photo_albums WHERE user_id = ? AND normalized_name = ?",
    )
    .bind(user.id)
    .bind(&norm_name)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some();
    if dup {
        return redirect("/library/photo/album/new");
    }

    let album_dir: String;
    let mut parent_id: Option<String> = None;
    match fv(&form, "parent_id") {
        Some(pid) => match sqlx::query(
            "SELECT directory_path FROM photo_albums WHERE id = ? AND user_id = ?",
        )
        .bind(&pid)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        {
            Some(r) => {
                album_dir = format!(
                    "{}/{}",
                    r.try_get::<String,_>("directory_path").unwrap_or_default(),
                    name
                );
                parent_id = Some(pid);
            }
            None => return redirect("/library/photo/album/new"),
        },
        None => {
            album_dir = format!("{}/{}/{}", root("photo"), user.username, name);
        }
    }
    let _ = std::fs::create_dir_all(&album_dir);

    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO photo_albums (id, user_id, name, normalized_name, description, is_public, \
         directory_path, parent_id, photo_count, created_at, updated_at) \
         VALUES (?,?,?,?,?,?,?,?,0,?,?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(&name)
    .bind(&norm_name)
    .bind(fv(&form, "description"))
    .bind((fv(&form, "is_public").as_deref() == Some("on")) as i64)
    .bind(&album_dir)
    .bind(&parent_id)
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;

    redirect(&format!("/library/photo/album/{id}"))
}

async fn view_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
    Query(q): Query<PageQ>,
) -> Response {
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    let full_access = is_super(&user);
    let is_owner = album["user_id"].as_i64() == Some(user.id) && full_access;
    if !user_has_album_access(db, &album_id, user.id).await && album["is_public"] != json!(true) {
        return err403("Access denied");
    }

    let (page_no, per_page) = clamp_page(&q, 48, 12, 200);
    let total = sqlx::query("SELECT COUNT(*) AS c FROM photos WHERE album_id = ?")
        .bind(&album_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_no - 1) * per_page;
    let photos: Vec<Value> = sqlx::query(
        "SELECT * FROM photos WHERE album_id = ? ORDER BY filename LIMIT ? OFFSET ?",
    )
    .bind(&album_id)
    .bind(per_page)
    .bind(offset)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(photo_row)
    .collect();

    let shared = if is_owner {
        shared_users(db, "photo_album_shares", "album_id", &album_id).await
    } else {
        vec![]
    };

    let sections: Vec<Value> = sqlx::query(
        "SELECT * FROM photo_albums WHERE parent_id = ? ORDER BY name",
    )
    .bind(&album_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(album_row)
    .collect();
    let mut section_photo_counts = serde_json::Map::new();
    if let Ok(rows) = sqlx::query(
        "SELECT album_id, COUNT(*) AS c FROM photos WHERE album_id IN \
         (SELECT id FROM photo_albums WHERE parent_id = ?) GROUP BY album_id",
    )
    .bind(&album_id)
    .fetch_all(db)
    .await
    {
        for r in rows {
            section_photo_counts.insert(
                r.get::<String,_>("album_id"),
                json!(r.get::<i64,_>("c")),
            );
        }
    }

    let parent_name = match album["parent_id"].as_str() {
        Some(pid) => sqlx::query("SELECT name FROM photo_albums WHERE id = ?")
            .bind(pid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String,_>("name").ok()),
        None => None,
    };

    let owner_username = sqlx::query("SELECT username FROM users WHERE id = ?")
        .bind(album["user_id"].as_i64().unwrap_or(0))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("username").ok())
        .unwrap_or_default();
    let mut album_ctx = album.clone();
    album_ctx["owner"] = json!({"username": owner_username});

    page(
        &state,
        "photo/album.html",
        json!({
            "user": crate::core::user_json(&user),
            "album": album_ctx,
            "photos": photos,
            "is_owner": is_owner,
            "shared_users": shared,
            "sections": sections,
            "section_photo_counts": Value::Object(section_photo_counts),
            "parent_name": parent_name,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
            "full_access": full_access,
        }),
    )
}

async fn album_photo_ids(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !user_has_album_access(&state.db, &album_id, user.id).await {
        return err403("Access denied");
    }
    let ids: Vec<String> = sqlx::query(
        "SELECT id FROM photos WHERE album_id = ? ORDER BY filename",
    )
    .bind(&album_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| r.get::<String,_>("id"))
    .collect();
    axum::Json(json!({"photo_ids": ids})).into_response()
}

async fn edit_album_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    if album["user_id"].as_i64() != Some(user.id) || !is_super(&user) {
        return err403("Access denied");
    }
    let shared = shared_users(db, "photo_album_shares", "album_id", &album_id).await;
    let shared_ids: Vec<i64> = shared
        .iter()
        .filter_map(|s| s["shared_user"]["id"].as_i64())
        .collect();
    let all = crate::all_users(db, user.id)
        .await
        .into_iter()
        .filter(|u| !shared_ids.contains(&u["id"].as_i64().unwrap_or(0)))
        .collect::<Vec<_>>();

    page(
        &state,
        "photo/album_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "album": album,
            "mode": "edit",
            "shared_users": shared,
            "all_users": all,
        }),
    )
}

async fn update_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return redirect("/library/photo/");
    };
    if album["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/photo/");
    }
    let form = form_map(&form);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("/library/photo/album/{album_id}/edit"));
    };
    let norm_name = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");

    if let Some(new_dir) = fv(&form, "directory_path") {
        if !FsPath::new(&new_dir).is_dir() {
            return redirect(&format!("/library/photo/album/{album_id}/edit"));
        }
        let _ = sqlx::query("UPDATE photo_albums SET directory_path = ? WHERE id = ?")
            .bind(&new_dir)
            .bind(&album_id)
            .execute(db)
            .await;
    }

    let was_public = album["is_public"] == json!(true);
    let is_public = fv(&form, "is_public").as_deref() == Some("on");
    let _ = sqlx::query(
        "UPDATE photo_albums SET name = ?, normalized_name = ?, description = ?, is_public = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(&norm_name)
    .bind(fv(&form, "description"))
    .bind(is_public as i64)
    .bind(now_str())
    .bind(&album_id)
    .execute(db)
    .await;

    if is_public && !was_public {
        let _dir = album["directory_path"].as_str().unwrap_or_default().to_string();
        make_album_public(db, &album_id, &user.username, &name).await;
    } else if !is_public && was_public {
        make_album_private(db, &album_id).await;
    }

    redirect(&format!("/library/photo/album/{album_id}"))
}

async fn toggle_visibility(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    if album["is_public"] == json!(true) {
        make_album_private(db, &album_id).await;
        axum::Json(json!({"message": "Album is now private.", "is_public": false})).into_response()
    } else {
        let _dir = album["directory_path"].as_str().unwrap_or_default().to_string();
        let name = album["name"].as_str().unwrap_or_default().to_string();
        make_album_public(db, &album_id, &user.username, &name).await;
        axum::Json(json!({"message": "Album is now public.", "is_public": true})).into_response()
    }
}

async fn clear_album_db(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return redirect("/library/photo/");
    };
    if album["is_public"] == json!(true) {
        make_album_private(db, &album_id).await;
    }
    let thumb_dir = format!("{}/.thumbnails", album["directory_path"].as_str().unwrap_or_default());
    if let Ok(rows) = sqlx::query("SELECT thumbnail_filename FROM photos WHERE album_id = ?")
        .bind(&album_id)
        .fetch_all(db)
        .await
    {
        for r in rows {
            if let Some(t) = r.try_get::<Option<String>,_>("thumbnail_filename").ok().flatten() {
                let p = format!("{thumb_dir}/{t}");
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    let _ = sqlx::query("DELETE FROM photos WHERE album_id = ?")
        .bind(&album_id)
        .execute(db)
        .await;
    let _ = sqlx::query("UPDATE photo_albums SET photo_count = 0 WHERE id = ?")
        .bind(&album_id)
        .execute(db)
        .await;
    redirect(&format!("/library/photo/album/{album_id}"))
}

async fn delete_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return redirect("/library/photo/");
    };
    if album["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/photo/");
    }
    if album["is_public"] == json!(true) {
        make_album_private(db, &album_id).await;
    }
    let dir = album["directory_path"].as_str().unwrap_or_default().to_string();
    let _ = sqlx::query("DELETE FROM photo_album_shares WHERE album_id = ?")
        .bind(&album_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM photos WHERE album_id = ?")
        .bind(&album_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM photo_albums WHERE id = ?")
        .bind(&album_id)
        .execute(db)
        .await;
    if FsPath::new(&dir).is_dir() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    redirect("/library/photo/")
}

async fn share_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return redirect("/library/photo/");
    };
    if album["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/photo/");
    }
    let form = form_map(&form);
    let Some(share_with) = fv(&form, "share_with_user_id") else {
        return redirect(&format!("/library/photo/album/{album_id}"));
    };
    let Ok(target_id) = share_with.parse::<i64>() else {
        return redirect(&format!("/library/photo/album/{album_id}"));
    };
    if target_id == user.id {
        return redirect(&format!("/library/photo/album/{album_id}"));
    }
    let exists = sqlx::query(
        "SELECT 1 FROM photo_album_shares WHERE album_id = ? AND shared_with_user_id = ?",
    )
    .bind(&album_id)
    .bind(target_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some();
    if exists {
        return redirect(&format!("/library/photo/album/{album_id}"));
    }
    let _ = sqlx::query(
        "INSERT INTO photo_album_shares (album_id, shared_with_user_id, created_at) VALUES (?,?,?)",
    )
    .bind(&album_id)
    .bind(target_id)
    .bind(now_str())
    .execute(db)
    .await;
    redirect(&format!("/library/photo/album/{album_id}/edit"))
}

async fn revoke_share(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((album_id, share_id)): Path<(String, i64)>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return redirect("/library/photo/");
    };
    if album["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/photo/");
    }
    let _ = sqlx::query("DELETE FROM photo_album_shares WHERE id = ? AND album_id = ?")
        .bind(share_id)
        .bind(&album_id)
        .execute(db)
        .await;
    redirect(&format!("/library/photo/album/{album_id}/edit"))
}

// -------------------------------------------------------- upload / import

async fn upload_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    if album["user_id"].as_i64() != Some(user.id) || !is_super(&user) {
        return err403("Access denied");
    }
    page(
        &state,
        "photo/upload.html",
        json!({
            "user": crate::core::user_json(&user),
            "album": album,
        }),
    )
}

async fn upload_photos(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
    mut multipart: Multipart,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    if album["user_id"].as_i64() != Some(user.id) {
        return err403("Access denied");
    }
    let album_dir = album["directory_path"].as_str().unwrap_or_default().to_string();
    let thumb_dir = format!("{album_dir}/.thumbnails");
    let album_is_public = album["is_public"] == json!(true);

    let mut uploaded = 0i64;
    while let Ok(Some(field)) = multipart.next_field().await {
        let Some(filename) = field.file_name().map(|s| s.to_string()) else {
            continue;
        };
        if !allowed_image_ext(&ext_of(&filename)) {
            continue;
        }
        let Ok(bytes) = field.bytes().await else {
            continue;
        };
        let dest = safe_dest_path(&album_dir, &filename);
        if tokio::fs::write(&dest, &bytes).await.is_err() {
            continue;
        }
        let dest_str = dest.to_string_lossy().to_string();
        insert_photo(db, &album_id, album_is_public, &filename, &dest_str, &thumb_dir).await;
        uploaded += 1;
    }

    let count = refresh_album_count(db, &album_id).await;
    let _ = count;
    if album_is_public && uploaded > 0 {
        make_album_public(db, &album_id, &user.username, album["name"].as_str().unwrap_or("")).await;
    }
    redirect(&format!("/library/photo/album/{album_id}"))
}

async fn import_from_path(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    let form = form_map(&form);
    let Some(import_path) = fv(&form, "import_path") else {
        return redirect(&format!("/library/photo/album/{album_id}/upload"));
    };
    let skip_thumbnails = form.get("skip_thumbnails").map(|v| v == "on").unwrap_or(false);
    let pdir = FsPath::new(&import_path);
    if !pdir.is_dir() {
        return redirect(&format!("/library/photo/album/{album_id}/upload"));
    }

    let album_dir = album["directory_path"].as_str().unwrap_or_default().to_string();
    let thumb_dir = format!("{album_dir}/.thumbnails");
    let album_is_public = album["is_public"] == json!(true);
    let is_inside = import_path == album_dir || import_path.starts_with(&format!("{album_dir}/"));

    let known: Vec<String> = sqlx::query("SELECT file_path FROM photos WHERE album_id = ?")
        .bind(&album_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("file_path"))
        .collect();

    let mut imported = 0i64;
    let mut skipped_thumb = 0i64;
    let mut skipped_existing = 0i64;
    let mut entries: Vec<String> = std::fs::read_dir(pdir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();

    for filename in entries {
        if !allowed_image_ext(&ext_of(&filename)) {
            continue;
        }
        let src = format!("{import_path}/{filename}");
        if src.starts_with(&format!("{album_dir}/.thumbnails")) {
            continue;
        }
        let dest = if is_inside {
            if known.contains(&src) {
                skipped_existing += 1;
                continue;
            }
            src.clone()
        } else {
            let d = safe_dest_path(&album_dir, &filename);
            if std::fs::copy(&src, &d).is_err() {
                continue;
            }
            d.to_string_lossy().to_string()
        };
        if known.contains(&dest) {
            skipped_existing += 1;
            continue;
        }
        let size = std::fs::metadata(&dest).map(|m| m.len() as i64).unwrap_or(0);
        if skip_thumbnails {
            let (w, h) = get_image_dimensions(&dest);
            if is_likely_thumbnail(&dest, size, w, h) {
                skipped_thumb += 1;
                continue;
            }
        }
        insert_photo(db, &album_id, album_is_public, &filename, &dest, &thumb_dir).await;
        imported += 1;
    }

    if imported > 0 {
        refresh_album_count(db, &album_id).await;
        if album_is_public {
            make_album_public(db, &album_id, &user.username, album["name"].as_str().unwrap_or("")).await;
        }
    }

    axum::Json(json!({
        "message": format!(
            "Imported {imported} photos. Skipped {skipped_existing} already in database, {skipped_thumb} likely thumbnails."
        )
    }))
    .into_response()
}

async fn rescan_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    let album_dir = album["directory_path"].as_str().unwrap_or_default().to_string();
    let thumb_dir = format!("{album_dir}/.thumbnails");
    if !FsPath::new(&album_dir).is_dir() {
        return axum::Json(json!({"message": format!("Album directory does not exist on disk: {album_dir}")})).into_response();
    }
    let album_is_public = album["is_public"] == json!(true);

    // existing photos by path
    let existing: Vec<(String, String, Option<String>)> = sqlx::query(
        "SELECT id, file_path, thumbnail_filename FROM photos WHERE album_id = ?",
    )
    .bind(&album_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        (
            r.get::<String,_>("id"),
            r.get::<String,_>("file_path"),
            r.try_get::<Option<String>,_>("thumbnail_filename").ok().flatten(),
        )
    })
    .collect();
    let known_paths: Vec<String> = existing.iter().map(|(_, p, _)| p.clone()).collect();

    let mut disk_files: Vec<String> = std::fs::read_dir(&album_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| e.path().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    disk_files.sort();

    let mut added = 0i64;
    for fpath in &disk_files {
        if known_paths.contains(fpath) {
            continue;
        }
        let filename = fpath.rsplit('/').next().unwrap_or("").to_string();
        if !allowed_image_ext(&ext_of(&filename)) {
            continue;
        }
        insert_photo(db, &album_id, album_is_public, &filename, fpath, &thumb_dir).await;
        added += 1;
    }

    let mut removed = 0i64;
    for (photo_id, fpath, thumb) in &existing {
        if !FsPath::new(fpath).exists() {
            if let Some(t) = thumb {
                let tp = format!("{thumb_dir}/{t}");
                let _ = std::fs::remove_file(&tp);
            }
            let _ = sqlx::query("DELETE FROM photos WHERE id = ?")
                .bind(photo_id)
                .execute(db)
                .await;
            removed += 1;
        }
    }

    let mut regenerated = 0i64;
    if let Ok(rows) = sqlx::query(
        "SELECT id, file_path, thumbnail_filename FROM photos WHERE album_id = ?",
    )
    .bind(&album_id)
    .fetch_all(db)
    .await
    {
        for r in rows {
            let pid = r.get::<String,_>("id");
            let fpath = r.get::<String,_>("file_path");
            let thumb = r.try_get::<Option<String>,_>("thumbnail_filename").ok().flatten();
            let needs = match &thumb {
                Some(t) => !FsPath::new(&format!("{thumb_dir}/{t}")).exists(),
                None => true,
            };
            if needs {
                if let Some(t) = generate_thumbnail(&fpath, &thumb_dir, &pid) {
                    let _ = sqlx::query("UPDATE photos SET thumbnail_filename = ? WHERE id = ?")
                        .bind(&t)
                        .bind(&pid)
                        .execute(db)
                        .await;
                    regenerated += 1;
                }
            }
        }
    }

    let count = refresh_album_count(db, &album_id).await;
    if album_is_public && added > 0 {
        make_album_public(db, &album_id, &user.username, album["name"].as_str().unwrap_or("")).await;
    }

    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!("Added {added} new"));
    }
    if removed > 0 {
        parts.push(format!("removed {removed} missing"));
    }
    if regenerated > 0 {
        parts.push(format!("regenerated {regenerated} thumbnails"));
    }
    if parts.is_empty() {
        return axum::Json(json!({"message": format!(
            "Album is already in sync. {} image files on disk, {} in database. Directory: {}",
            disk_files.len(), existing.len(), album_dir
        )})).into_response();
    }
    axum::Json(json!({"message": format!(
        "Rescan complete: {}. Album now has {count} photos.", parts.join(", ")
    )})).into_response()
}

async fn regenerate_thumbnails(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    let thumb_dir = format!(
        "{}/.thumbnails",
        album["directory_path"].as_str().unwrap_or_default()
    );
    let rows = sqlx::query("SELECT id, file_path FROM photos WHERE album_id = ?")
        .bind(&album_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let total = rows.len();
    let mut generated = 0i64;
    for r in rows {
        let pid = r.get::<String,_>("id");
        let fpath = r.get::<String,_>("file_path");
        if !FsPath::new(&fpath).exists() {
            continue;
        }
        if let Some(t) = generate_thumbnail(&fpath, &thumb_dir, &pid) {
            let _ = sqlx::query("UPDATE photos SET thumbnail_filename = ? WHERE id = ?")
                .bind(&t)
                .bind(&pid)
                .execute(db)
                .await;
            generated += 1;
        }
    }
    axum::Json(json!({"message": format!("Generated {generated} thumbnails out of {total} photos.")})).into_response()
}

async fn remove_thumbnail_photos(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    let album_dir = album["directory_path"].as_str().unwrap_or_default().to_string();
    let thumb_dir = format!("{album_dir}/.thumbnails");
    let rows = sqlx::query(
        "SELECT id, file_path, file_size, width, height, thumbnail_filename FROM photos WHERE album_id = ?",
    )
    .bind(&album_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let mut removed = 0i64;
    for r in rows {
        let pid = r.get::<String,_>("id");
        let fpath = r.get::<String,_>("file_path");
        let size = r.get::<i64,_>("file_size");
        let w = r.try_get::<i64,_>("width").ok();
        let h = r.try_get::<i64,_>("height").ok();
        if is_likely_thumbnail(&fpath, size, w, h) {
            let _ = std::fs::remove_file(&fpath);
            if let Some(t) = r.try_get::<Option<String>,_>("thumbnail_filename").ok().flatten() {
                let _ = std::fs::remove_file(format!("{thumb_dir}/{t}"));
            }
            let _ = sqlx::query("DELETE FROM photos WHERE id = ?")
                .bind(&pid)
                .execute(db)
                .await;
            removed += 1;
        }
    }
    if removed > 0 {
        refresh_album_count(db, &album_id).await;
    }
    axum::Json(json!({"message": format!("Removed {removed} likely thumbnail photos from album.")})).into_response()
}

// ------------------------------------------------------------ photo CRUD

async fn view_photo(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    let album_id = photo["album_id"].as_str().unwrap_or_default().to_string();
    let Some(album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    if !user_has_album_access(db, &album_id, user.id).await
        && photo["is_public"] != json!(true)
        && album["is_public"] != json!(true)
    {
        return err403("Access denied");
    }
    let full_access = is_super(&user);
    let is_owner = album["user_id"].as_i64() == Some(user.id) && full_access;

    let own_albums: Vec<Value> = if is_owner {
        sqlx::query("SELECT * FROM photo_albums WHERE user_id = ? ORDER BY name")
            .bind(user.id)
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .iter()
            .map(album_row)
            .collect()
    } else {
        vec![]
    };

    let owner_username = sqlx::query("SELECT username FROM users WHERE id = ?")
        .bind(album["user_id"].as_i64().unwrap_or(0))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("username").ok())
        .unwrap_or_default();
    let mut album_ctx = album.clone();
    album_ctx["owner"] = json!({"username": owner_username});

    page(
        &state,
        "photo/photo_detail.html",
        json!({
            "user": crate::core::user_json(&user),
            "photo": photo,
            "album": album_ctx,
            "is_owner": is_owner,
            "full_access": full_access,
            "own_albums": own_albums,
        }),
    )
}

async fn serve_photo_file(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    if !user_has_album_access(db, photo["album_id"].as_str().unwrap_or(""), user.id).await {
        return err403("Access denied");
    }
    let path = photo["file_path"].as_str().unwrap_or_default();
    if !FsPath::new(path).exists() {
        return err404("File not found on disk");
    }
    serve_file(path, None).await
}

async fn serve_photo_thumbnail(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    let album_id = photo["album_id"].as_str().unwrap_or("");
    if !user_has_album_access(db, album_id, user.id).await {
        return err403("Access denied");
    }
    let Some(album) = get_album(db, album_id).await else {
        return err404("Album not found");
    };
    let file_path = photo["file_path"].as_str().unwrap_or_default().to_string();
    let thumb_dir = format!("{}/.thumbnails", album["directory_path"].as_str().unwrap_or_default());

    let thumb_name = photo["thumbnail_filename"].as_str().map(|s| s.to_string());
    let Some(name) = thumb_name else {
        return serve_photo_file(State(state), RequireUser(user), Path(photo_id)).await;
    };
    let mut thumb_path = format!("{thumb_dir}/{name}");
    if !FsPath::new(&thumb_path).exists() {
        if let Some(new_name) = generate_thumbnail(&file_path, &thumb_dir, &photo_id) {
            let _ = sqlx::query("UPDATE photos SET thumbnail_filename = ? WHERE id = ?")
                .bind(&new_name)
                .bind(&photo_id)
                .execute(db)
                .await;
            thumb_path = format!("{thumb_dir}/{new_name}");
        } else {
            return serve_photo_file(State(state), RequireUser(user), Path(photo_id)).await;
        }
    }
    serve_file(&thumb_path, None).await
}

async fn update_photo(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    if !user_is_album_owner(db, photo["album_id"].as_str().unwrap_or(""), user.id).await {
        return err403("Only the album owner can edit photos");
    }
    let form = form_map(&form);
    if let Some(caption) = form.get("caption") {
        let v = caption.trim();
        let _ = sqlx::query("UPDATE photos SET caption = ?, updated_at = ? WHERE id = ?")
            .bind(if v.is_empty() { None } else { Some(v) })
            .bind(now_str())
            .bind(&photo_id)
            .execute(db)
            .await;
    }
    if let Some(annotation) = form.get("annotation") {
        let v = annotation.trim();
        let _ = sqlx::query("UPDATE photos SET annotation = ?, updated_at = ? WHERE id = ?")
            .bind(if v.is_empty() { None } else { Some(v) })
            .bind(now_str())
            .bind(&photo_id)
            .execute(db)
            .await;
    }
    axum::Json(json!({"message": "Photo updated"})).into_response()
}

async fn delete_photo(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    let album_id = photo["album_id"].as_str().unwrap_or_default().to_string();
    let is_owner = user_is_album_owner(db, &album_id, user.id).await;
    if !is_owner && !is_super(&user) {
        return err403("Access denied");
    }
    let file_path = photo["file_path"].as_str().unwrap_or_default().to_string();
    let thumb = photo["thumbnail_filename"].as_str().map(|s| s.to_string());

    let _ = sqlx::query("DELETE FROM photos WHERE id = ?")
        .bind(&photo_id)
        .execute(db)
        .await;
    refresh_album_count(db, &album_id).await;

    if FsPath::new(&file_path).exists() {
        let _ = std::fs::remove_file(&file_path);
    }
    if let Some(t) = thumb {
        if let Some(album) = get_album(db, &album_id).await {
            let tp = format!("{}/.thumbnails/{t}", album["directory_path"].as_str().unwrap_or_default());
            let _ = std::fs::remove_file(&tp);
        }
    }
    redirect(&format!("/library/photo/album/{album_id}"))
}

async fn move_photo(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(target_album_id) = fv(&form, "target_album_id") else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Target album is required");
    };
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    let album_id = photo["album_id"].as_str().unwrap_or_default().to_string();
    if !user_is_album_owner(db, &album_id, user.id).await {
        return err403("Only the album owner can move photos");
    }
    let Some(target_album) = sqlx::query(
        "SELECT * FROM photo_albums WHERE id = ? AND user_id = ?",
    )
    .bind(&target_album_id)
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| album_row(&r))
    else {
        return err404("Target album not found or not owned by you");
    };
    if target_album_id == album_id {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Photo is already in this album");
    }

    let Some(old_album) = get_album(db, &album_id).await else {
        return err404("Album not found");
    };
    let filename = photo["filename"].as_str().unwrap_or_default().to_string();
    let old_path = photo["file_path"].as_str().unwrap_or_default().to_string();
    let old_thumb_dir = format!("{}/.thumbnails", old_album["directory_path"].as_str().unwrap_or_default());
    let new_album_dir = target_album["directory_path"].as_str().unwrap_or_default().to_string();
    let new_thumb_dir = format!("{new_album_dir}/.thumbnails");
    let _ = std::fs::create_dir_all(&new_thumb_dir);

    let dest = safe_dest_path(&new_album_dir, &filename);
    let dest_str = dest.to_string_lossy().to_string();
    if FsPath::new(&old_path).exists() {
        let _ = std::fs::rename(&old_path, &dest);
        if !FsPath::new(&dest).exists() {
            let _ = std::fs::copy(&old_path, &dest);
            let _ = std::fs::remove_file(&old_path);
        }
    }
    if let Some(t) = photo["thumbnail_filename"].as_str() {
        let old_t = format!("{old_thumb_dir}/{t}");
        let new_t = format!("{new_thumb_dir}/{t}");
        if FsPath::new(&old_t).exists() {
            let _ = std::fs::rename(&old_t, &new_t);
        }
    }

    let target_is_public = target_album["is_public"] == json!(true);
    let _ = sqlx::query(
        "UPDATE photos SET album_id = ?, file_path = ?, is_public = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&target_album_id)
    .bind(&dest_str)
    .bind(target_is_public as i64)
    .bind(now_str())
    .bind(&photo_id)
    .execute(db)
    .await;

    refresh_album_count(db, &album_id).await;
    refresh_album_count(db, &target_album_id).await;

    axum::Json(json!({
        "message": format!("Moved '{filename}' to '{}'", target_album["name"].as_str().unwrap_or("")),
        "redirect": format!("/library/photo/photo/{photo_id}"),
    }))
    .into_response()
}

async fn user_remove_photo_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    if !user_is_album_owner(db, photo["album_id"].as_str().unwrap_or(""), user.id).await {
        return err403("Only the album owner can do this");
    }
    let _ = sqlx::query("UPDATE photos SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&photo_id)
        .execute(db)
        .await;
    redirect(&format!("/library/photo/photo/{photo_id}"))
}

async fn user_make_photo_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(photo) = get_photo(db, &photo_id).await else {
        return err404("Photo not found");
    };
    if !user_is_album_owner(db, photo["album_id"].as_str().unwrap_or(""), user.id).await {
        return err403("Only the album owner can do this");
    }
    if photo["is_banned"] == json!(true) {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "This photo has been banned from public view");
    }
    let _ = sqlx::query("UPDATE photos SET is_public = 1, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&photo_id)
        .execute(db)
        .await;
    redirect(&format!("/library/photo/photo/{photo_id}"))
}

// ------------------------------------------------------------------ admin

async fn admin_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;

    let with_owner = |sql: &'static str| async move {
        sqlx::query(sql)
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| {
                json!({
                    "album": album_row(r),
                    "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
                })
            })
            .collect::<Vec<Value>>()
    };

    let public_albums = with_owner(
        "SELECT a.*, u.username AS owner_username FROM photo_albums a \
         JOIN users u ON u.id = a.user_id WHERE a.is_public = 1 ORDER BY a.name",
    )
    .await;
    let all_albums = with_owner(
        "SELECT a.*, u.username AS owner_username FROM photo_albums a \
         JOIN users u ON u.id = a.user_id ORDER BY a.name",
    )
    .await;

    let banned: Vec<Value> = sqlx::query(
        "SELECT p.*, a.name AS album_name, u.username AS owner_username \
         FROM photos p JOIN photo_albums a ON p.album_id = a.id \
         JOIN users u ON u.id = a.user_id WHERE p.is_banned = 1 ORDER BY p.created_at DESC",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        let mut p = photo_row(r);
        p["album_name"] = json!(r.try_get::<String,_>("album_name").unwrap_or_default());
        p["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
        json!({
            "photo": p,
            "album": {"id": "", "name": r.try_get::<String,_>("album_name").unwrap_or_default()},
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
        })
    })
    .collect();

    page(
        &state,
        "photo/admin.html",
        json!({
            "user": crate::core::user_json(&user),
            "public_albums": public_albums,
            "banned_photos": banned,
            "all_albums": all_albums,
        }),
    )
}

async fn admin_ban_photo(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE photos SET is_banned = 1, is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&photo_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Photo banned and removed from public view."})).into_response()
}

async fn admin_unban_photo(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE photos SET is_banned = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&photo_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Photo unbanned."})).into_response()
}

async fn admin_remove_public_photo(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(photo_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE photos SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&photo_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Photo removed from public view."})).into_response()
}

async fn admin_remove_public_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let Some(album) = get_album(&state.db, &album_id).await else {
        return err404("Album not found");
    };
    make_album_private(&state.db, &album_id).await;
    axum::Json(json!({"message": format!("Album '{}' removed from public.", album["name"].as_str().unwrap_or(""))})).into_response()
}

// -------------------------------------------------------- quick add / scans

async fn add_photos_quick(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let (Some(file_path), Some(album_id)) = (fv(&form, "file_path"), fv(&form, "album_id")) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "File path and album are required.");
    };
    let pdir = FsPath::new(&file_path);
    if !pdir.exists() {
        return err_json(axum::http::StatusCode::BAD_REQUEST, &format!("Path not found: {file_path}"));
    }
    let Some(album) = sqlx::query("SELECT * FROM photo_albums WHERE id = ? AND user_id = ?")
        .bind(&album_id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| album_row(&r))
        else {
            return err404("Album not found.");
        };
    if !pdir.is_dir() {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Path must be a directory containing images.");
    }

    let album_dir = album["directory_path"].as_str().unwrap_or_default().to_string();
    let thumb_dir = format!("{album_dir}/.thumbnails");
    let album_is_public = album["is_public"] == json!(true);
    let is_inside = file_path == album_dir || file_path.starts_with(&format!("{album_dir}/"));

    let known: Vec<String> = sqlx::query("SELECT file_path FROM photos WHERE album_id = ?")
        .bind(&album_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("file_path"))
        .collect();
    let global: Vec<String> = sqlx::query("SELECT file_path FROM photos")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("file_path"))
        .collect();

    let mut imported = 0i64;
    let mut skipped = 0i64;
    let mut entries: Vec<String> = std::fs::read_dir(pdir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();

    for filename in entries {
        if !allowed_image_ext(&ext_of(&filename)) {
            continue;
        }
        let src = format!("{file_path}/{filename}");
        if src.starts_with(&format!("{album_dir}/.thumbnails")) {
            continue;
        }
        let dest = if is_inside {
            if known.contains(&src) {
                skipped += 1;
                continue;
            }
            src.clone()
        } else {
            let d = safe_dest_path(&album_dir, &filename);
            if std::fs::copy(&src, &d).is_err() {
                continue;
            }
            d.to_string_lossy().to_string()
        };
        if known.contains(&dest) || global.contains(&dest) {
            skipped += 1;
            continue;
        }
        insert_photo(db, &album_id, album_is_public, &filename, &dest, &thumb_dir).await;
        imported += 1;
    }

    if imported > 0 {
        refresh_album_count(db, &album_id).await;
        if album_is_public {
            make_album_public(db, &album_id, &user.username, album["name"].as_str().unwrap_or("")).await;
        }
    }

    axum::Json(json!({
        "success": true,
        "message": format!("Imported {imported} photo(s). Skipped {skipped} already in database."),
    }))
    .into_response()
}

async fn scan_my_photo_library(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let db = state.db.clone();
    let own = sqlx::query("SELECT id FROM photo_albums WHERE user_id = ?")
        .bind(user.id)
        .fetch_all(&db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("id"))
        .collect::<Vec<String>>();

    if own.is_empty() {
        let uid = user.id;
        let username = user.username.clone();
        tokio::spawn(async move {
            scan_user_photos_from_disk(&db, uid, &username).await;
        });
        return axum::Json(json!({"message": "No albums in database. Scanning disk for photos..."})).into_response();
    }
    let n = own.len();
    tokio::spawn(async move {
        for album_id in own {
            rescan_album_task(&db, &album_id).await;
        }
    });
    axum::Json(json!({"message": format!("Scanning {n} album(s)...")})).into_response()
}

async fn full_rescan_photos(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let db = state.db.clone();
    let _ = sqlx::query(
        "DELETE FROM photos WHERE album_id IN (SELECT id FROM photo_albums WHERE user_id = ?)",
    )
    .bind(user.id)
    .execute(&db)
    .await;
    let _ = sqlx::query(
        "DELETE FROM photo_album_shares WHERE album_id IN (SELECT id FROM photo_albums WHERE user_id = ?)",
    )
    .bind(user.id)
    .execute(&db)
    .await;
    let _ = sqlx::query("DELETE FROM photo_albums WHERE user_id = ?")
        .bind(user.id)
        .execute(&db)
        .await;

    let username = user.username.clone();
    tokio::spawn(async move {
        run_photo_force_scan(&db, Some(&username)).await;
    });
    axum::Json(json!({"message": "Deleted all photo data. Rescanning from disk..."})).into_response()
}

async fn trigger_photo_force_scan(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        run_photo_force_scan(&db, None).await;
    });
    axum::Json(json!({"message": "Photo force rescan started. Truncating tables and re-importing from disk."})).into_response()
}

// -------------------------------------------------------- background scans

pub(crate) async fn run_photo_force_scan(db: &syarc_plugin_api::Db, scan_user: Option<&str>) {
    let _ = sqlx::query("DELETE FROM photo_album_shares").execute(db).await;
    let _ = sqlx::query("DELETE FROM photos").execute(db).await;
    let _ = sqlx::query("DELETE FROM photo_albums").execute(db).await;

    let root = root("photo");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut usernames: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != "public" && n != ".thumbnails" && !n.starts_with('.'))
        .collect();
    usernames.sort();

    for username in usernames {
        if let Some(su) = scan_user {
            if username != su {
                continue;
            }
        }
        let Some(db_user) = sqlx::query("SELECT id FROM users WHERE username = ?")
            .bind(&username)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
        else {
            continue;
        };
        let uid = db_user.get::<i64,_>("id");
        scan_user_dir(db, uid, &username, &root, true).await;
    }
}

async fn scan_user_photos_from_disk(
    db: &syarc_plugin_api::Db,
    user_id: i64,
    username: &str,
) {
    let root = root("photo");
    scan_user_dir(db, user_id, username, &root, false).await;
}

async fn scan_user_dir(
    db: &syarc_plugin_api::Db,
    user_id: i64,
    username: &str,
    root: &str,
    force: bool,
) {
    let user_path = format!("{root}/{username}");
    let Ok(entries) = std::fs::read_dir(&user_path) else {
        return;
    };
    let mut albums: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.'))
        .collect();
    albums.sort();

    for album_name in albums {
        let norm = album_name.to_lowercase().trim().to_string();
        if !force {
            let exists = sqlx::query(
                "SELECT 1 FROM photo_albums WHERE user_id = ? AND normalized_name = ?",
            )
            .bind(user_id)
            .bind(&norm)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .is_some();
            if exists {
                continue;
            }
        }
        let album_path = format!("{user_path}/{album_name}");
        let thumb_dir = format!("{album_path}/.thumbnails");
        let album_id = new_uuid();
        let _ = sqlx::query(
            "INSERT INTO photo_albums (id, user_id, name, normalized_name, is_public, \
             directory_path, photo_count, created_at, updated_at) VALUES (?,?,?,?,0,?,0,?,?)",
        )
        .bind(&album_id)
        .bind(user_id)
        .bind(&album_name)
        .bind(&norm)
        .bind(&album_path)
        .bind(now_str())
        .bind(now_str())
        .execute(db)
        .await;

        let mut count = 0i64;
        if let Ok(files) = std::fs::read_dir(&album_path) {
            let mut names: Vec<String> = files
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.starts_with("._") && allowed_image_ext(&ext_of(n)))
                .collect();
            names.sort();
            for filename in names {
                let fpath = format!("{album_path}/{filename}");
                insert_photo(db, &album_id, false, &filename, &fpath, &thumb_dir).await;
                count += 1;
            }
        }
        let _ = sqlx::query("UPDATE photo_albums SET photo_count = ? WHERE id = ?")
            .bind(count)
            .bind(&album_id)
            .execute(db)
            .await;
    }
}

async fn rescan_album_task(db: &syarc_plugin_api::Db, album_id: &str) {
    let Some(album) = get_album(db, album_id).await else {
        return;
    };
    let album_dir = album["directory_path"].as_str().unwrap_or_default().to_string();
    if !FsPath::new(&album_dir).is_dir() {
        return;
    }
    let thumb_dir = format!("{album_dir}/.thumbnails");
    let album_is_public = album["is_public"] == json!(true);

    let known: Vec<String> = sqlx::query("SELECT file_path FROM photos WHERE album_id = ?")
        .bind(album_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("file_path"))
        .collect();

    let Ok(entries) = std::fs::read_dir(&album_dir) else {
        return;
    };
    let mut added = 0i64;
    for e in entries.filter_map(|e| e.ok()) {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        let filename = e.file_name().to_string_lossy().to_string();
        if filename.starts_with("._") || !allowed_image_ext(&ext_of(&filename)) {
            continue;
        }
        let fpath = path.to_string_lossy().to_string();
        if known.contains(&fpath) {
            continue;
        }
        insert_photo(db, album_id, album_is_public, &filename, &fpath, &thumb_dir).await;
        added += 1;
    }
    if added > 0 {
        refresh_album_count(db, album_id).await;
    }
}
