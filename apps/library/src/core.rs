//! Library landing page and directory browser.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;

use crate::{err_json, is_super, normalize, page, root, RequireUser, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new().route("/library/browse", get(browse))
}

pub(crate) async fn landing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let db = &state.db;
    let uid = user.id;
    let is_superuser = is_super(&user);

    let music_count: i64 = if is_superuser {
        sqlx::query(
            "SELECT COUNT(*) FROM music_albums WHERE media_type = 'music'",
        )
        .fetch_one(db)
        .await
        .unwrap()
        .get(0)
    } else {
        sqlx::query(
            "SELECT COUNT(*) FROM music_albums WHERE media_type = 'music' AND (\
                (is_public = 1 AND is_banned = 0) OR user_id = ?2 OR \
                id IN (SELECT album_id FROM music_album_shares WHERE shared_with_user_id = ?2))",
        )
        .bind(uid)
        .fetch_one(db)
        .await
        .unwrap()
        .get(0)
    };

    let audiobook_count: i64 = sqlx::query("SELECT COUNT(*) FROM audiobook_books")
        .fetch_one(db)
        .await
        .unwrap()
        .get(0);

    let book_count: i64 = if is_superuser {
        sqlx::query("SELECT COUNT(*) FROM ebook_books")
            .fetch_one(db)
            .await
            .unwrap()
            .get(0)
    } else {
        sqlx::query(
            "SELECT COUNT(*) FROM ebook_books WHERE (is_public = 1 AND is_banned = 0) OR \
             topic_id IN (SELECT id FROM ebook_topics WHERE user_id = ?2) OR \
             topic_id IN (SELECT topic_id FROM ebook_topic_shares WHERE shared_with_user_id = ?2)",
        )
        .bind(uid)
        .fetch_one(db)
        .await
        .unwrap()
        .get(0)
    };

    let movie_count: i64 = if is_superuser {
        sqlx::query("SELECT COUNT(*) FROM movies")
            .fetch_one(db)
            .await
            .unwrap()
            .get(0)
    } else {
        sqlx::query(
            "SELECT COUNT(*) FROM movies WHERE (is_public = 1 AND is_banned = 0) OR \
             category_id IN (SELECT id FROM movie_category WHERE user_id = ?2) OR \
             category_id IN (SELECT category_id FROM movie_category_shares WHERE shared_with_user_id = ?2)",
        )
        .bind(uid)
        .fetch_one(db)
        .await
        .unwrap()
        .get(0)
    };

    let photo_count: i64 = if is_superuser {
        sqlx::query("SELECT COUNT(*) FROM photo_albums")
            .fetch_one(db)
            .await
            .unwrap()
            .get(0)
    } else {
        sqlx::query(
            "SELECT COUNT(*) FROM photo_albums WHERE is_public = 1 OR user_id = ?2 OR \
             id IN (SELECT album_id FROM photo_album_shares WHERE shared_with_user_id = ?2)",
        )
        .bind(uid)
        .fetch_one(db)
        .await
        .unwrap()
        .get(0)
    };

    let ctx = json!({
        "user": crate::core::user_json(&user),
        "allowed_sub_apps": ["ebook", "music", "audiobook", "movie", "photo"],
        "music_count": music_count,
        "music_full_access": is_superuser,
        "audiobook_count": audiobook_count,
        "book_count": book_count,
        "ebook_full_access": is_superuser,
        "movie_count": movie_count,
        "movie_full_access": is_superuser,
        "photo_count": photo_count,
        "photo_full_access": is_superuser,
    });
    page(&state, "library/index.html", ctx)
}

pub(crate) fn user_json(user: &syarc_plugin_api::User) -> Value {
    json!({
        "id": user.id,
        "username": user.username.clone(),
        "is_superuser": is_super(user),
    })
}

#[allow(dead_code)]
pub(crate) fn _unused(s: &str) -> String {
    normalize(s)
}

pub(crate) async fn browse(
    State(_state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let path = q.get("path").map(|s| s.as_str()).unwrap_or("");
    let root_kind = q.get("root").map(|s| s.as_str()).unwrap_or("");

    let resolved = if !root_kind.is_empty() {
        let base = match root_kind {
            "music" | "audiobook" | "ebook" | "movie" | "photo" => root(root_kind),
            _ => return err_json(axum::http::StatusCode::BAD_REQUEST, "Unknown root"),
        };
        let joined = if path.is_empty() {
            base.clone()
        } else if path.starts_with(&base) {
            // The browser widget passes back the absolute entry paths we
            // returned earlier; accept them as-is instead of re-joining.
            path.to_string()
        } else {
            base.clone() + "/" + path.trim_start_matches('/')
        };
        let norm = normalize_path(&joined);
        if !norm.starts_with(&base) {
            return err_json(axum::http::StatusCode::FORBIDDEN, "Path outside allowed root");
        }
        norm
    } else if !path.is_empty() {
        normalize_path(path)
    } else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Must specify root or path");
    };

    let Ok(rd) = std::fs::read_dir(&resolved) else {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Directory not found");
    };

    let mut entries: Vec<(String, String, bool)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let full = e.path().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((name.to_lowercase(), full, is_dir));
    }
    entries.sort();
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(lc, full, is_dir)| {
            let name = full.rsplit('/').next().unwrap_or(&lc).to_string();
            json!({"name": name, "path": full, "is_dir": is_dir})
        })
        .collect();

    axum::Json(json!({"path": resolved, "entries": entries})).into_response()
}

fn normalize_path(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            _ => out.push(part),
        }
    }
    // Preserve the leading slash for absolute paths: callers compare the
    // result against absolute root prefixes ("path outside allowed root"),
    // which a bare join("/") would silently fail.
    let joined = out.join("/");
    if p.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}
