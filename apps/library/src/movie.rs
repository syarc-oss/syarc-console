//! Movie sub-app: categories (optionally nested), movies, DVD conversion,
//! posters via ffmpeg, sharing and public links. Ported from fastmain.

use axum::extract::{Multipart, Path, Query, RawForm, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    err_json, form_map, fv, is_super, new_uuid, now_str, page, redirect, root, serve_file,
    shared_users, RequireUser, SharedState,
};
use crate::scanner::{
    determine_movie_file_type, determine_movie_title, dir_size, extract_first_frame,
    extract_year_from_name, is_video_ext, movie_posters_dir, scan_movies,
};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/library/movie", get(index))
        .route("/library/movie/", get(index))
        .route("/library/movie/browse/users", get(browse_users))
        .route("/library/movie/browse/users/{user_id}", get(browse_user_categories))
        .route("/library/movie/browse/categories", get(browse_categories))
        .route("/library/movie/browse/movies", get(browse_movies))
        .route("/library/movie/search", get(search))
        .route("/library/movie/category/new", get(new_category_page).post(create_category))
        .route("/library/movie/category/{category_id}", get(view_category))
        .route("/library/movie/category/{category_id}/movie-ids", get(category_movie_ids))
        .route("/library/movie/category/{category_id}/edit", get(edit_category_page).post(update_category))
        .route("/library/movie/category/{category_id}/toggle-visibility", post(toggle_category_visibility))
        .route("/library/movie/category/{category_id}/delete", post(delete_category))
        .route("/library/movie/category/{category_id}/share", post(share_category))
        .route("/library/movie/category/{category_id}/revoke/{share_id}", post(revoke_share))
        .route("/library/movie/category/{category_id}/import", get(import_page))
        .route("/library/movie/category/{category_id}/import-path", post(import_from_path))
        .route("/library/movie/category/{category_id}/rescan", post(rescan_category))
        .route("/library/movie/category/{category_id}/rescan-full", post(rescan_category_full))
        .route("/library/movie/movie/{movie_id}", get(view_movie))
        .route("/library/movie/stream/{movie_id}", get(stream_movie))
        .route("/library/movie/player/{movie_id}", get(movie_player))
        .route("/library/movie/movie/{movie_id}/edit", get(edit_movie_form))
        .route("/library/movie/movie/edit", post(update_movie))
        .route("/library/movie/movie/{movie_id}/move", post(move_movie))
        .route("/library/movie/movie/{movie_id}/delete", post(delete_movie))
        .route("/library/movie/movie/{movie_id}/generate-poster", post(generate_movie_poster))
        .route("/library/movie/movie/{movie_id}/convert-dvd", post(convert_dvd))
        .route("/library/movie/generate-posters", post(generate_missing_posters))
        .route("/library/movie/movie/{movie_id}/make-public", post(make_movie_public))
        .route("/library/movie/movie/{movie_id}/remove-public", post(remove_movie_public))
        .route("/library/movie/add", post(add_movies_quick))
        .route("/library/movie/scan", post(trigger_scan))
        .route("/library/movie/scan/my-library", post(scan_my_library))
        .route("/library/movie/scan/full-rescan", post(full_rescan))
        .route("/library/movie/admin", get(admin_page))
        .route("/library/movie/admin/movie/{movie_id}/ban", post(ban_movie))
        .route("/library/movie/admin/movie/{movie_id}/unban", post(unban_movie))
        .route("/library/movie/admin/movie/{movie_id}/remove-public", post(admin_remove_public_movie))
        .route("/library/movie/admin/category/{category_id}/remove-public", post(admin_remove_public_category))
        .route("/library/movie/poster/{movie_id}", get(movie_poster))
        .route("/library/movie/movie_static/posters/{file}", get(static_poster))
}

fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}
fn err403(msg: &str) -> Response {
    err_json(axum::http::StatusCode::FORBIDDEN, msg)
}
fn err400(msg: &str) -> Response {
    err_json(axum::http::StatusCode::BAD_REQUEST, msg)
}

fn public_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(root("movie")).join("public")
}

fn category_dir_for(username: &str, name: &str) -> String {
    std::path::Path::new(&root("movie"))
        .join(username)
        .join(name)
        .to_string_lossy()
        .into_owned()
}

// ------------------------------------------------------------------ rows

fn category_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "user_id": r.get::<i64,_>("user_id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "normalized_name": r.try_get::<String,_>("normalized_name").unwrap_or_default(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "directory_path": r.try_get::<String,_>("directory_path").unwrap_or_default(),
        "movie_count": r.get::<i64,_>("movie_count"),
        "parent_id": r.try_get::<Option<String>,_>("parent_id").ok().flatten(),
        "created_at": r.try_get::<Option<String>,_>("created_at").ok().flatten(),
    })
}

fn movie_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    let size = r.try_get::<Option<i64>,_>("file_size").ok().flatten();
    json!({
        "id": r.get::<String,_>("id"),
        "category_id": r.get::<String,_>("category_id"),
        "title": r.try_get::<String,_>("title").unwrap_or_default(),
        "normalized_title": r.try_get::<String,_>("normalized_title").unwrap_or_default(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "year": r.try_get::<Option<i64>,_>("year").ok().flatten(),
        "file_type": r.try_get::<String,_>("file_type").unwrap_or_default(),
        "file_path": r.try_get::<String,_>("file_path").unwrap_or_default(),
        "file_size": size,
        "size_human": size.map(crate::human_size).unwrap_or_default(),
        "poster_filename": r.try_get::<Option<String>,_>("poster_filename").ok().flatten(),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "is_banned": r.get::<i64,_>("is_banned") == 1,
        "created_at": r.try_get::<Option<String>,_>("created_at").ok().flatten(),
    })
}

async fn get_category(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query(
        "SELECT c.*, u.username AS owner_username FROM movie_category c \
         JOIN users u ON u.id = c.user_id WHERE c.id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| {
        let mut c = category_row(&r);
        c["owner"] =
            json!({"username": r.try_get::<String, _>("owner_username").unwrap_or_default()});
        c
    })
}

async fn get_movie(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM movies WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| movie_row(&r))
}

async fn user_has_category_access(db: &syarc_plugin_api::Db, category_id: &str, user_id: i64) -> bool {
    let Ok(Some(r)) = sqlx::query("SELECT user_id, is_public FROM movie_category WHERE id = ?")
        .bind(category_id)
        .fetch_optional(db)
        .await
    else {
        return false;
    };
    if r.get::<i64,_>("user_id") == user_id || r.get::<i64,_>("is_public") == 1 {
        return true;
    }
    sqlx::query("SELECT 1 FROM movie_category_shares WHERE category_id = ? AND shared_with_user_id = ?")
        .bind(category_id)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn user_is_category_owner(db: &syarc_plugin_api::Db, category_id: &str, user_id: i64) -> bool {
    sqlx::query("SELECT 1 FROM movie_category WHERE id = ? AND user_id = ?")
        .bind(category_id)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn make_category_public(db: &syarc_plugin_api::Db, category_id: &str) {
    let _ = sqlx::query("UPDATE movie_category SET is_public = 1, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(category_id)
        .execute(db)
        .await;
    if let Some(r) = sqlx::query(
        "SELECT u.username, c.name FROM movie_category c JOIN users u ON u.id = c.user_id WHERE c.id = ?",
    )
    .bind(category_id)
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
    let _ = sqlx::query("UPDATE movies SET is_public = 1 WHERE category_id = ? AND is_banned = 0")
        .bind(category_id)
        .execute(db)
        .await;
}

async fn make_category_private(db: &syarc_plugin_api::Db, category_id: &str) {
    let _ = sqlx::query("UPDATE movie_category SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(category_id)
        .execute(db)
        .await;
    if let Some(r) = sqlx::query(
        "SELECT u.username, c.name FROM movie_category c JOIN users u ON u.id = c.user_id WHERE c.id = ?",
    )
    .bind(category_id)
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
    let _ = sqlx::query("UPDATE movies SET is_public = 0 WHERE category_id = ?")
        .bind(category_id)
        .execute(db)
        .await;
}

async fn refresh_movie_count(db: &syarc_plugin_api::Db, category_id: &str) -> i64 {
    let count = sqlx::query("SELECT COUNT(*) AS c FROM movies WHERE category_id = ?")
        .bind(category_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    let _ = sqlx::query("UPDATE movie_category SET movie_count = ? WHERE id = ?")
        .bind(count)
        .bind(category_id)
        .execute(db)
        .await;
    count
}

fn copy_path_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_path_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

#[derive(Deserialize)]
struct Q {
    page: Option<i64>,
    per_page: Option<i64>,
    #[serde(default)]
    letter: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    cat_page: Option<i64>,
    #[serde(default)]
    cat_per_page: Option<i64>,
    #[serde(default)]
    cat_letter: Option<String>,
    #[serde(default)]
    skip_seconds: Option<i64>,
}

fn one_letter(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .filter(|l| l.chars().count() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|l| l.to_uppercase())
}

fn q_page(q: &Q) -> (i64, i64) {
    (q.page.unwrap_or(1).max(1), q.per_page.unwrap_or(24).clamp(12, 120))
}

// ----------------------------------------------------------------- pages

async fn index(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let write_access = is_super(&user);

    let own_categories: Vec<Value> = if write_access {
        sqlx::query("SELECT * FROM movie_category WHERE user_id = ? ORDER BY name")
            .bind(user.id)
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .iter()
            .map(category_row)
            .collect()
    } else {
        vec![]
    };

    let shared_categories: Vec<Value> = if write_access {
        sqlx::query(
            "SELECT c.*, u.username AS owner_username FROM movie_category_shares sh \
             JOIN movie_category c ON c.id = sh.category_id \
             JOIN users u ON u.id = c.user_id \
             WHERE sh.shared_with_user_id = ? ORDER BY c.name",
        )
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let mut c = category_row(r);
            c["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
            c
        })
        .collect()
    } else {
        vec![]
    };

    let total_movies = sqlx::query(
        "SELECT COUNT(*) AS c FROM movies m JOIN movie_category c ON m.category_id = c.id WHERE c.user_id = ?",
    )
    .bind(user.id)
    .fetch_one(db)
    .await
    .map(|r| r.get::<i64,_>("c"))
    .unwrap_or(0);

    let public_count = sqlx::query("SELECT COUNT(*) AS c FROM movie_category WHERE is_public = 1")
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);

    let public_movies_count = sqlx::query(
        "SELECT COUNT(*) AS c FROM movies m JOIN movie_category c ON m.category_id = c.id \
         WHERE c.is_public = 1 AND m.is_public = 1 AND m.is_banned = 0",
    )
    .fetch_one(db)
    .await
    .map(|r| r.get::<i64,_>("c"))
    .unwrap_or(0);

    page(
        &state,
        "movie/index.html",
        json!({
            "user": crate::core::user_json(&user),
            "own_categories": own_categories,
            "shared_categories": shared_categories,
            "total_movies": total_movies,
            "public_category_count": public_count,
            "public_movies_count": public_movies_count,
            "full_access": true,
            "write_access": write_access,
        }),
    )
}

async fn browse_users(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let rows = sqlx::query(
        "SELECT c.user_id, u.username, COUNT(c.id) AS category_count \
         FROM movie_category c JOIN users u ON u.id = c.user_id \
         GROUP BY c.user_id, u.username ORDER BY u.username",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let users: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "user_id": r.get::<i64,_>("user_id"),
                "username": r.try_get::<String,_>("username").unwrap_or_default(),
                "category_count": r.get::<i64,_>("category_count"),
            })
        })
        .collect();
    page(
        &state,
        "movie/browse_users.html",
        json!({
            "user": crate::core::user_json(&user),
            "users_with_categories": users,
        }),
    )
}

async fn browse_user_categories(
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

    let sql = if user_id == user.id {
        "SELECT * FROM movie_category WHERE user_id = ? ORDER BY name".to_string()
    } else {
        "SELECT * FROM movie_category WHERE user_id = ? AND (is_public = 1 OR id IN \
         (SELECT category_id FROM movie_category_shares WHERE shared_with_user_id = ?)) ORDER BY name"
            .to_string()
    };
    let q = sqlx::query(&sql).bind(user_id);
    let q = if user_id == user.id {
        q
    } else {
        q.bind(user.id)
    };
    let categories: Vec<Value> = q
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(category_row)
        .collect();

    page(
        &state,
        "movie/browse_user_categories.html",
        json!({
            "user": crate::core::user_json(&user),
            "target_user": {"id": user_id, "username": target_username},
            "categories": categories,
        }),
    )
}

async fn browse_categories(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page) = q_page(&q);

    let rows = sqlx::query(
        "SELECT c.*, u.username AS owner_username FROM movie_category c \
         JOIN users u ON u.id = c.user_id \
         WHERE c.user_id = ?1 OR c.is_public = 1 OR c.id IN \
         (SELECT category_id FROM movie_category_shares WHERE shared_with_user_id = ?1) \
         ORDER BY c.name LIMIT ?2 OFFSET ?3",
    )
    .bind(user.id)
    .bind(per_page)
    .bind((page_no - 1) * per_page)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let total = sqlx::query(
        "SELECT COUNT(*) AS c FROM movie_category c \
         WHERE c.user_id = ?1 OR c.is_public = 1 OR c.id IN \
         (SELECT category_id FROM movie_category_shares WHERE shared_with_user_id = ?1)",
    )
    .bind(user.id)
    .fetch_one(db)
    .await
    .map(|r| r.get::<i64,_>("c"))
    .unwrap_or(0);

    let categories: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut c = category_row(r);
            c["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
            c
        })
        .collect();

    page(
        &state,
        "movie/browse_categories.html",
        json!({
            "user": crate::core::user_json(&user),
            "categories": categories,
            "current_page": page_no,
            "total_pages": (total + per_page - 1) / per_page,
            "total": total,
            "per_page": per_page,
        }),
    )
}

async fn browse_movies(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page) = q_page(&q);
    let letter = one_letter(&q.letter);

    let (vis, like) = if is_super(&user) {
        (String::new(), String::new())
    } else {
        (
            " AND (c.user_id = ?1 OR c.id IN (SELECT category_id FROM movie_category_shares WHERE shared_with_user_id = ?1) OR (m.is_public = 1 AND m.is_banned = 0))".to_string(),
            String::new(),
        )
    };
    let like = match &letter {
        Some(l) => format!(" AND m.title LIKE '{l}%'"),
        None => like,
    };

    let total: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) AS c FROM movies m JOIN movie_category c ON m.category_id = c.id WHERE 1=1 {vis}{like}"
    ))
    .bind(user.id)
    .fetch_one(db)
    .await
    .map(|r| r.get::<i64,_>("c"))
    .unwrap_or(0);

    let rows = sqlx::query(&format!(
        "SELECT m.*, c.name AS category_name, u.username AS owner_username \
         FROM movies m JOIN movie_category c ON m.category_id = c.id \
         JOIN users u ON u.id = c.user_id WHERE 1=1 {vis}{like} \
         ORDER BY m.title LIMIT ?2 OFFSET ?3"
    ))
    .bind(user.id)
    .bind(per_page)
    .bind((page_no - 1) * per_page)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let movies: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "movie": movie_row(r),
                "category_name": r.try_get::<String,_>("category_name").unwrap_or_default(),
                "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
            })
        })
        .collect();

    page(
        &state,
        "movie/browse_movies.html",
        json!({
            "user": crate::core::user_json(&user),
            "movies": movies,
            "current_page": page_no,
            "total_pages": (total + per_page - 1) / per_page,
            "total": total,
            "per_page": per_page,
            "current_letter": letter,
        }),
    )
}

async fn search(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let query = q.q.as_deref().unwrap_or("").trim().to_string();
    let mut results: Vec<Value> = Vec::new();

    if !query.is_empty() {
        let query_lower = query.to_lowercase();
        let pat = format!("%{}%", query.replace('%', ""));
        let pat_lower = format!("%{}%", query_lower.replace('%', ""));

        let cats = sqlx::query(
            "SELECT c.*, u.username AS owner_username FROM movie_category c \
             JOIN users u ON u.id = c.user_id \
             WHERE (c.user_id = ?1 OR c.is_public = 1 OR c.id IN \
               (SELECT category_id FROM movie_category_shares WHERE shared_with_user_id = ?1)) \
             AND (c.normalized_name LIKE ?2 OR c.description LIKE ?3 OR u.username LIKE ?3) \
             ORDER BY c.name",
        )
        .bind(user.id)
        .bind(&pat_lower)
        .bind(&pat)
        .fetch_all(db)
        .await
        .unwrap_or_default();
        for r in cats {
            let mut c = category_row(&r);
            c["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
            results.push(json!({"type": "category", "category": c, "owner_username": c["owner_username"].clone()}));
        }

        let movies = sqlx::query(
            "SELECT m.*, c.name AS category_name, u.username AS owner_username, c.user_id AS cat_user_id, c.id AS cat_id, c.normalized_name AS cat_norm, c.directory_path AS cat_dir, c.is_public AS cat_public, c.movie_count AS cat_count, c.parent_id AS cat_parent, c.description AS cat_desc, c.created_at AS cat_created \
             FROM movies m JOIN movie_category c ON m.category_id = c.id \
             JOIN users u ON u.id = c.user_id \
             WHERE (c.user_id = ?1 OR c.is_public = 1 OR c.id IN \
               (SELECT category_id FROM movie_category_shares WHERE shared_with_user_id = ?1)) \
             AND (m.normalized_title LIKE ?2 OR m.description LIKE ?3) \
             ORDER BY m.created_at DESC",
        )
        .bind(user.id)
        .bind(&pat_lower)
        .bind(&pat)
        .fetch_all(db)
        .await
        .unwrap_or_default();
        for r in movies {
            let cat = json!({
                "id": r.get::<String,_>("cat_id"),
                "user_id": r.get::<i64,_>("cat_user_id"),
                "name": r.try_get::<String,_>("category_name").unwrap_or_default(),
                "normalized_name": r.try_get::<String,_>("cat_norm").unwrap_or_default(),
                "description": r.try_get::<Option<String>,_>("cat_desc").ok().flatten(),
                "is_public": r.get::<i64,_>("cat_public") == 1,
                "directory_path": r.try_get::<String,_>("cat_dir").unwrap_or_default(),
                "movie_count": r.get::<i64,_>("cat_count"),
                "parent_id": r.try_get::<Option<String>,_>("cat_parent").ok().flatten(),
                "created_at": r.try_get::<Option<String>,_>("cat_created").ok().flatten(),
            });
            results.push(json!({
                "type": "movie",
                "movie": movie_row(&r),
                "category": cat,
                "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
            }));
        }
    }

    page(
        &state,
        "movie/search.html",
        json!({
            "user": crate::core::user_json(&user),
            "query": query,
            "results": results,
            "page": q.page.unwrap_or(1),
            "per_page": q.per_page.unwrap_or(24),
            "total": results.len(),
        }),
    )
}

// --------------------------------------------------------- category CRUD

async fn new_category_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let own: Vec<Value> = sqlx::query(
        "SELECT * FROM movie_category WHERE user_id = ? AND parent_id IS NULL ORDER BY name",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .iter()
    .map(category_row)
    .collect();
    page(
        &state,
        "movie/category_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "category": Value::Null,
            "mode": "new",
            "own_categories": own,
        }),
    )
}

async fn create_category(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect("/library/movie/category/new");
    };
    let norm = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");

    let exists = sqlx::query(
        "SELECT 1 FROM movie_category WHERE user_id = ? AND normalized_name = ?",
    )
    .bind(user.id)
    .bind(&norm)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some();
    if exists {
        return redirect("/library/movie/category/new");
    }

    let parent_id = fv(&form, "parent_id");
    let category_dir = if let Some(pid) = &parent_id {
        match sqlx::query("SELECT directory_path FROM movie_category WHERE id = ? AND user_id = ?")
            .bind(pid)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
        {
            Some(r) => {
                let p = r.try_get::<String,_>("directory_path").unwrap_or_default();
                std::path::Path::new(&p).join(&name).to_string_lossy().into_owned()
            }
            None => return redirect("/library/movie/category/new"),
        }
    } else {
        category_dir_for(&user.username, &name)
    };
    let _ = std::fs::create_dir_all(&category_dir);

    let id = new_uuid();
    let is_public = form.get("is_public").map(|v| v == "on").unwrap_or(false);
    let _ = sqlx::query(
        "INSERT INTO movie_category (id, user_id, name, normalized_name, description, is_public, directory_path, parent_id, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(&name)
    .bind(&norm)
    .bind(fv(&form, "description"))
    .bind(is_public as i64)
    .bind(&category_dir)
    .bind(&parent_id)
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;

    if is_public {
        make_category_public(db, &id).await;
    }
    redirect(&format!("/library/movie/category/{id}"))
}

async fn view_category(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    let is_owner = category["user_id"].as_i64() == Some(user.id) && is_super(&user);
    if !is_owner
        && !user_has_category_access(db, &category_id, user.id).await
        && category["is_public"] != json!(true)
    {
        return err403("Access denied");
    }

    let (page_no, per_page) = q_page(&q);
    let letter = one_letter(&q.letter);
    let filter = if !is_owner {
        " AND m.is_public = 1 AND m.is_banned = 0"
    } else {
        ""
    };
    let like = letter
        .as_ref()
        .map(|l| format!(" AND m.title LIKE '{l}%'"))
        .unwrap_or_default();

    let total: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) AS c FROM movies m WHERE m.category_id = ?{filter}{like}"
    ))
    .bind(&category_id)
    .fetch_one(db)
    .await
    .map(|r| r.get::<i64,_>("c"))
    .unwrap_or(0);

    let movies: Vec<Value> = sqlx::query(&format!(
        "SELECT m.* FROM movies m WHERE m.category_id = ?{filter}{like} \
         ORDER BY m.title LIMIT ? OFFSET ?"
    ))
    .bind(&category_id)
    .bind(per_page)
    .bind((page_no - 1) * per_page)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(movie_row)
    .collect();

    let shared = if is_owner {
        shared_users(db, "movie_category_shares", "category_id", &category_id).await
    } else {
        vec![]
    };

    let subcategories: Vec<Value> = sqlx::query(
        "SELECT * FROM movie_category WHERE parent_id = ? ORDER BY name",
    )
    .bind(&category_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(category_row)
    .collect();

    let mut subcat_movie_counts = serde_json::Map::new();
    for sc in &subcategories {
        let cid = sc["id"].as_str().unwrap_or("").to_string();
        let c = sqlx::query("SELECT COUNT(*) AS c FROM movies WHERE category_id = ?")
            .bind(&cid)
            .fetch_one(db)
            .await
            .map(|r| r.get::<i64,_>("c"))
            .unwrap_or(0);
        subcat_movie_counts.insert(cid, json!(c));
    }

    let parent_name = if let Some(pid) = category["parent_id"].as_str() {
        sqlx::query("SELECT name FROM movie_category WHERE id = ?")
            .bind(pid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String,_>("name").ok())
    } else {
        None
    };

    page(
        &state,
        "movie/category.html",
        json!({
            "user": crate::core::user_json(&user),
            "category": category,
            "movies": movies,
            "is_owner": is_owner,
            "shared_users": shared,
            "subcategories": subcategories,
            "subcat_movie_counts": Value::Object(subcat_movie_counts),
            "parent_name": parent_name,
            "current_page": page_no,
            "total_pages": (total + per_page - 1) / per_page,
            "total": total,
            "per_page": per_page,
            "current_letter": letter,
            "full_access": true,
        }),
    )
}

async fn category_movie_ids(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_category_access(db, &category_id, user.id).await {
        return err403("Access denied");
    }
    let ids: Vec<String> = sqlx::query("SELECT id FROM movies WHERE category_id = ? ORDER BY title")
        .bind(&category_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("id"))
        .collect();
    axum::Json(json!({"movie_ids": ids})).into_response()
}

async fn edit_category_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err404("Category not found");
    }
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    let shared = shared_users(db, "movie_category_shares", "category_id", &category_id).await;
    let shared_ids: Vec<i64> = shared
        .iter()
        .filter_map(|s| s["shared_user"]["id"].as_i64())
        .collect();
    let users = crate::all_users(db, user.id)
        .await
        .into_iter()
        .filter(|u| !shared_ids.contains(&u["id"].as_i64().unwrap_or(0)))
        .collect::<Vec<_>>();
    page(
        &state,
        "movie/category_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "category": category,
            "mode": "edit",
            "shared_users": shared,
            "all_users": users,
        }),
    )
}

async fn update_category(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
    RawForm(raw): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return redirect("/library/movie/");
    }
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("/library/movie/category/{category_id}/edit"));
    };
    let norm = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let was_public = sqlx::query("SELECT is_public FROM movie_category WHERE id = ?")
        .bind(&category_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("is_public") == 1)
        .unwrap_or(false);
    let is_public = form.get("is_public").map(|v| v == "on").unwrap_or(false);

    let _ = sqlx::query(
        "UPDATE movie_category SET name = ?, normalized_name = ?, description = ?, is_public = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(&norm)
    .bind(fv(&form, "description"))
    .bind(is_public as i64)
    .bind(now_str())
    .bind(&category_id)
    .execute(db)
    .await;

    if is_public && !was_public {
        make_category_public(db, &category_id).await;
    } else if !is_public && was_public {
        make_category_private(db, &category_id).await;
    }
    redirect(&format!("/library/movie/category/{category_id}"))
}

async fn toggle_category_visibility(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err404("Category not found");
    }
    let is_public = sqlx::query("SELECT is_public FROM movie_category WHERE id = ?")
        .bind(&category_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("is_public") == 1)
        .unwrap_or(false);
    if is_public {
        make_category_private(db, &category_id).await;
    } else {
        make_category_public(db, &category_id).await;
    }
    axum::Json(json!({
        "message": if is_public { "Category is now private." } else { "Category is now public." },
        "is_public": !is_public,
    }))
    .into_response()
}

async fn delete_category(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(category) = get_category(db, &category_id).await else {
        return redirect("/library/movie/");
    };
    if category["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/movie/");
    }
    let count = sqlx::query("SELECT COUNT(*) AS c FROM movies WHERE category_id = ?")
        .bind(&category_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    if count > 0 {
        return redirect(&format!("/library/movie/category/{category_id}"));
    }
    if category["is_public"] == json!(true) {
        make_category_private(db, &category_id).await;
    }
    let dir = category["directory_path"].as_str().unwrap_or("").to_string();
    let _ = sqlx::query("DELETE FROM movie_category WHERE id = ?")
        .bind(&category_id)
        .execute(db)
        .await;
    if !dir.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    redirect("/library/movie/")
}

async fn share_category(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
    RawForm(raw): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return redirect("/library/movie/");
    }
    let form = form_map(&raw);
    let Some(share_with) = fv(&form, "share_with_user_id") else {
        return redirect(&format!("/library/movie/category/{category_id}"));
    };
    let Ok(target) = share_with.parse::<i64>() else {
        return redirect(&format!("/library/movie/category/{category_id}"));
    };
    if target == user.id {
        return redirect(&format!("/library/movie/category/{category_id}"));
    }
    let exists = sqlx::query(
        "SELECT 1 FROM movie_category_shares WHERE category_id = ? AND shared_with_user_id = ?",
    )
    .bind(&category_id)
    .bind(target)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some();
    if !exists {
        let _ = sqlx::query(
            "INSERT INTO movie_category_shares (category_id, shared_with_user_id, created_at) VALUES (?, ?, ?)",
        )
        .bind(&category_id)
        .bind(target)
        .bind(now_str())
        .execute(db)
        .await;
    }
    redirect(&format!("/library/movie/category/{category_id}/edit"))
}

async fn revoke_share(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((category_id, share_id)): Path<(String, i64)>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return redirect("/library/movie/");
    }
    let _ = sqlx::query("DELETE FROM movie_category_shares WHERE id = ? AND category_id = ?")
        .bind(share_id)
        .bind(&category_id)
        .execute(db)
        .await;
    redirect(&format!("/library/movie/category/{category_id}/edit"))
}

// ----------------------------------------------------------------- import

async fn import_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err404("Category not found");
    }
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    page(
        &state,
        "movie/import.html",
        json!({
            "user": crate::core::user_json(&user),
            "category": category,
        }),
    )
}

/// Collect video items (files, or directories that hold videos) from a path.
fn collect_import_items(import_path: &str) -> Vec<String> {
    let p = std::path::Path::new(import_path);
    let mut items = Vec::new();
    if p.is_file() {
        if is_video_ext(&p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()) {
            items.push(import_path.to_string());
        }
        return items;
    }
    if !p.is_dir() {
        return items;
    }
    let mut names: Vec<String> = std::fs::read_dir(p)
        .map(|rd| {
            rd.filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for entry_name in names {
        if entry_name.starts_with("._") {
            continue;
        }
        let entry_path = p.join(&entry_name);
        if (entry_path.is_file() && is_video_ext(&entry_name))
            || (entry_path.is_dir()
                && (entry_path.join("VIDEO_TS").is_dir()
                    || std::fs::read_dir(&entry_path)
                        .map(|rd| {
                            rd.flatten().any(|e| is_video_ext(&e.file_name().to_string_lossy()))
                        })
                        .unwrap_or(false)))
        {
            items.push(entry_path.to_string_lossy().into_owned());
        }
    }
    items
}

async fn import_from_path(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
    RawForm(raw): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err404("Category not found");
    }
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    let form = form_map(&raw);
    let Some(import_path) = fv(&form, "import_path") else {
        return redirect(&format!("/library/movie/category/{category_id}/import"));
    };
    if !std::path::Path::new(&import_path).exists() {
        return redirect(&format!("/library/movie/category/{category_id}/import"));
    }
    let make_public = form.get("make_public").map(|v| v == "on").unwrap_or(false);

    let category_dir = category["directory_path"].as_str().unwrap_or("").to_string();
    let norm_import = std::fs::canonicalize(&import_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| import_path.clone());
    let norm_cat = std::fs::canonicalize(&category_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| category_dir.clone());
    let in_place = norm_import == norm_cat || norm_import.starts_with(&format!("{norm_cat}/"));

    let known: Vec<String> = sqlx::query("SELECT file_path FROM movies")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.try_get::<String,_>("file_path").ok())
        .collect();

    let mut imported = 0i64;

    for src_path in collect_import_items(&norm_import) {
        let dest_path = if in_place {
            src_path.clone()
        } else {
            let dest_name = std::path::Path::new(&src_path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut dest = std::path::Path::new(&category_dir).join(&dest_name);
            if dest.exists() {
                let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
                if std::path::Path::new(&src_path).is_dir() {
                    dest = std::path::Path::new(&category_dir).join(format!("{suffix}_{dest_name}"));
                } else {
                    let stem = std::path::Path::new(&dest_name)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let ext = std::path::Path::new(&dest_name)
                        .extension()
                        .map(|s| format!(".{}", s.to_string_lossy()))
                        .unwrap_or_default();
                    dest = std::path::Path::new(&category_dir).join(format!("{stem}_{suffix}{ext}"));
                }
            }
            let dest_s = dest.to_string_lossy().into_owned();
            if std::path::Path::new(&src_path).is_dir() {
                let _ = copy_path_recursive(std::path::Path::new(&src_path), &dest);
            } else {
                let _ = std::fs::copy(&src_path, &dest);
            }
            dest_s
        };

        if known.iter().any(|k| k == &dest_path) {
            continue;
        }

        let entry_name = std::path::Path::new(&dest_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = determine_movie_title(&entry_name);
        let year = extract_year_from_name(&entry_name);
        let is_public = make_public || category["is_public"] == json!(true);
        let id = new_uuid();
        let _ = sqlx::query(
            "INSERT INTO movies (id, category_id, title, normalized_title, file_type, file_path, file_size, year, is_public, is_banned, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&id)
        .bind(&category_id)
        .bind(&title)
        .bind(title.to_lowercase().trim())
        .bind(determine_movie_file_type(&dest_path))
        .bind(&dest_path)
        .bind(dir_size(&dest_path))
        .bind(year)
        .bind(is_public as i64)
        .bind(now_str())
        .bind(now_str())
        .execute(db)
        .await;
        imported += 1;
    }

    if imported > 0 {
        refresh_movie_count(db, &category_id).await;
    }
    redirect(&format!("/library/movie/category/{category_id}"))
}

async fn rescan_category(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err404("Category not found");
    }
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    let category_dir = category["directory_path"].as_str().unwrap_or("").to_string();
    if !std::path::Path::new(&category_dir).is_dir() {
        return axum::Json(json!({"message": "Category directory does not exist on disk."}))
            .into_response();
    }

    let existing: Vec<(String, String, Option<String>)> = sqlx::query(
        "SELECT id, file_path, poster_filename FROM movies WHERE category_id = ?",
    )
    .bind(&category_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        (
            r.get::<String,_>("id"),
            r.try_get::<String,_>("file_path").unwrap_or_default(),
            r.try_get::<Option<String>,_>("poster_filename").ok().flatten(),
        )
    })
    .collect();

    let disk_items = collect_import_items(&category_dir);

    let mut added = 0i64;
    let mut poster_count = 0i64;
    let mut new_ids: Vec<String> = Vec::new();
    for fpath in &disk_items {
        if existing.iter().any(|(_, fp, _)| fp == fpath) {
            continue;
        }
        let entry_name = std::path::Path::new(fpath)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = determine_movie_title(&entry_name);
        let year = extract_year_from_name(&entry_name);
        let id = new_uuid();
        let _ = sqlx::query(
            "INSERT INTO movies (id, category_id, title, normalized_title, file_type, file_path, file_size, year, is_public, is_banned, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&id)
        .bind(&category_id)
        .bind(&title)
        .bind(title.to_lowercase().trim())
        .bind(determine_movie_file_type(fpath))
        .bind(fpath)
        .bind(dir_size(fpath))
        .bind(year)
        .bind((category["is_public"] == json!(true)) as i64)
        .bind(now_str())
        .bind(now_str())
        .execute(db)
        .await;
        added += 1;
        new_ids.push(id);
    }

    for id in &new_ids {
        if let Some(m) = get_movie(db, id).await {
            let fp = m["file_path"].as_str().unwrap_or("").to_string();
            if let Some(poster) = extract_first_frame(&fp, id, None).await {
                let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
                    .bind(poster)
                    .bind(id)
                    .execute(db)
                    .await;
                poster_count += 1;
            }
        }
    }

    let mut removed = 0i64;
    for (id, fp, poster) in &existing {
        if !std::path::Path::new(fp).exists() {
            if let Some(p) = poster {
                let pp = movie_posters_dir().join(p);
                let _ = std::fs::remove_file(pp);
            }
            let _ = sqlx::query("DELETE FROM movies WHERE id = ?").bind(id).execute(db).await;
            removed += 1;
        }
    }

    let mut regenerated = 0i64;
    let missing: Vec<(String, String)> = sqlx::query(
        "SELECT id, file_path FROM movies WHERE category_id = ? AND poster_filename IS NULL",
    )
    .bind(&category_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<String,_>("id"), r.try_get::<String,_>("file_path").unwrap_or_default()))
    .collect();
    for (id, fp) in missing {
        if let Some(poster) = extract_first_frame(&fp, &id, None).await {
            let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
                .bind(poster)
                .bind(&id)
                .execute(db)
                .await;
            regenerated += 1;
        }
    }

    let count = refresh_movie_count(db, &category_id).await;

    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!("Added {added} new"));
    }
    if poster_count > 0 {
        parts.push(format!("generated {poster_count} posters"));
    }
    if regenerated > 0 {
        parts.push(format!("generated {regenerated} missing posters"));
    }
    if removed > 0 {
        parts.push(format!("removed {removed} missing"));
    }
    if parts.is_empty() {
        return axum::Json(json!({"message": "Category is already in sync. No changes needed."}))
            .into_response();
    }
    axum::Json(json!({
        "message": format!("Rescan complete: {}. Category now has {count} movies.", parts.join(", ")),
    }))
    .into_response()
}

async fn rescan_category_full(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err404("Category not found");
    }
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    let category_dir = category["directory_path"].as_str().unwrap_or("").to_string();
    if !std::path::Path::new(&category_dir).is_dir() {
        return redirect(&format!("/library/movie/category/{category_id}/edit"));
    }

    let posters: Vec<(String, Option<String>)> = sqlx::query(
        "SELECT id, poster_filename FROM movies WHERE category_id = ?",
    )
    .bind(&category_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<String,_>("id"), r.try_get::<Option<String>,_>("poster_filename").ok().flatten()))
    .collect();
    for (_, poster) in &posters {
        if let Some(p) = poster.as_deref().filter(|p| *p != crate::scanner::POSTER_SIBLING) {
            let _ = std::fs::remove_file(movie_posters_dir().join(p));
        }
    }
    let deleted = posters.len() as i64;
    let _ = sqlx::query("DELETE FROM movies WHERE category_id = ?")
        .bind(&category_id)
        .execute(db)
        .await;

    let disk_items = collect_import_items(&category_dir);
    let mut poster_count = 0i64;
    let mut new_count = 0i64;
    for fpath in &disk_items {
        let entry_name = std::path::Path::new(fpath)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = determine_movie_title(&entry_name);
        let year = extract_year_from_name(&entry_name);
        let id = new_uuid();
        let _ = sqlx::query(
            "INSERT INTO movies (id, category_id, title, normalized_title, file_type, file_path, file_size, year, is_public, is_banned, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&id)
        .bind(&category_id)
        .bind(&title)
        .bind(title.to_lowercase().trim())
        .bind(determine_movie_file_type(fpath))
        .bind(fpath)
        .bind(dir_size(fpath))
        .bind(year)
        .bind((category["is_public"] == json!(true)) as i64)
        .bind(now_str())
        .bind(now_str())
        .execute(db)
        .await;
        new_count += 1;
        if let Some(poster) = extract_first_frame(fpath, &id, None).await {
            let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
                .bind(poster)
                .bind(&id)
                .execute(db)
                .await;
            poster_count += 1;
        }
    }

    refresh_movie_count(db, &category_id).await;
    redirect(&format!(
        "/library/movie/category/{category_id}?msg=Full+rescan+complete:+deleted+{deleted},+re-imported+{new_count}+movies,+generated+{poster_count}+posters."
    ))
}

// -------------------------------------------------------- view / stream

async fn view_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let category_id = movie["category_id"].as_str().unwrap_or("").to_string();
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    if !user_has_category_access(db, &category_id, user.id).await
        && movie["is_public"] != json!(true)
        && category["is_public"] != json!(true)
    {
        return err403("Access denied");
    }
    let is_owner = category["user_id"].as_i64() == Some(user.id) && is_super(&user);
    let own_categories: Vec<Value> = if is_owner {
        sqlx::query("SELECT * FROM movie_category WHERE user_id = ? ORDER BY name")
            .bind(user.id)
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .iter()
            .map(category_row)
            .collect()
    } else {
        vec![]
    };
    page(
        &state,
        "movie/movie.html",
        json!({
            "user": crate::core::user_json(&user),
            "movie": movie,
            "category": category,
            "is_owner": is_owner,
            "full_access": true,
            "own_categories": own_categories,
            "cat_page": q.cat_page.unwrap_or(1),
            "cat_per_page": q.cat_per_page.unwrap_or(24),
            "cat_letter": q.cat_letter.clone().unwrap_or_default(),
        }),
    )
}

async fn stream_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let db = &state.db;
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let category_id = movie["category_id"].as_str().unwrap_or("").to_string();
    if !user_has_category_access(db, &category_id, user.id).await
        && movie["is_public"] != json!(true)
    {
        let cat_public = get_category(db, &category_id)
            .await
            .map(|c| c["is_public"] == json!(true))
            .unwrap_or(false);
        if !cat_public {
            return err403("Access denied");
        }
    }

    let mut file_path = movie["file_path"].as_str().unwrap_or("").to_string();
    if std::path::Path::new(&file_path).is_dir() {
        if let Some(resolved) = crate::scanner::resolve_video_file(&file_path) {
            file_path = resolved;
        }
    }
    if !std::path::Path::new(&file_path).is_file() {
        return err404("Movie file not found on disk");
    }
    serve_file(&file_path, headers.get("range").and_then(|v| v.to_str().ok())).await
}

async fn movie_player(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(mut movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    if let Some(cid) = movie["category_id"].as_str() {
        if let Some(cat) = get_category(db, cid).await {
            movie["category"] = cat;
        }
    }
    page(
        &state,
        "movie/player.html",
        json!({
            "user": crate::core::user_json(&user),
            "movie": movie,
        }),
    )
}

// ------------------------------------------------------------ movie CRUD

async fn edit_movie_form(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
    Query(q): Query<Q>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let category_id = movie["category_id"].as_str().unwrap_or("").to_string();
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err403("Only the category owner can edit movies");
    }
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    let own_categories: Vec<Value> = sqlx::query(
        "SELECT * FROM movie_category WHERE user_id = ? ORDER BY name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(category_row)
    .collect();
    page(
        &state,
        "movie/edit_movie.html",
        json!({
            "user": crate::core::user_json(&user),
            "movie": movie,
            "category": category,
            "own_categories": own_categories,
            "is_owner": true,
            "full_access": true,
            "cat_page": q.cat_page.unwrap_or(1),
            "cat_per_page": q.cat_per_page.unwrap_or(24),
            "cat_letter": q.cat_letter.clone().unwrap_or_default(),
        }),
    )
}

async fn update_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    mut multipart: Multipart,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;

    let mut movie_id: Option<String> = None;
    let mut title: Option<String> = None;
    let mut year: Option<String> = None;
    let mut description: Option<String> = None;
    let mut category_id: Option<String> = None;
    let mut poster_data: Option<String> = None;
    let mut poster_file: Option<(String, Vec<u8>)> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "poster" => {
                let filename = field.file_name().unwrap_or("").to_string();
                if !filename.is_empty() {
                    if let Ok(bytes) = field.bytes().await {
                        poster_file = Some((filename, bytes.to_vec()));
                    }
                }
            }
            _ => {
                if let Ok(text) = field.text().await {
                    match name.as_str() {
                        "movie_id" => movie_id = Some(text),
                        "title" => title = Some(text),
                        "year" => year = Some(text),
                        "description" => description = Some(text),
                        "category_id" => category_id = Some(text),
                        "poster_data" => poster_data = Some(text),
                        _ => {}
                    }
                }
            }
        }
    }

    let Some(movie_id) = movie_id.filter(|s| !s.trim().is_empty()) else {
        return err400("Missing required fields");
    };
    let Some(new_title) = title.filter(|s| !s.trim().is_empty()) else {
        return err400("Missing required fields");
    };
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let old_cat = movie["category_id"].as_str().unwrap_or("").to_string();
    if !user_is_category_owner(db, &old_cat, user.id).await {
        return err403("Only the category owner can edit movies");
    }

    let new_year = year.as_deref().and_then(|y| y.trim().parse::<i64>().ok());
    let _ = sqlx::query(
        "UPDATE movies SET title = ?, normalized_title = ?, description = ?, year = ?, updated_at = ? WHERE id = ?",
    )
    .bind(new_title.trim())
    .bind(new_title.trim().to_lowercase())
    .bind(description.as_deref().map(str::trim).filter(|d| !d.is_empty()))
    .bind(new_year)
    .bind(now_str())
    .bind(&movie_id)
    .execute(db)
    .await;

    if let Some(new_cat) = category_id.filter(|s| !s.trim().is_empty()) {
        if user_is_category_owner(db, &new_cat, user.id).await {
            let _ = sqlx::query("UPDATE movies SET category_id = ? WHERE id = ?")
                .bind(&new_cat)
                .bind(&movie_id)
                .execute(db)
                .await;
        }
    }

    let poster_dir = movie_posters_dir();
    let mut new_poster: Option<String> = None;
    let mut poster_bytes: Option<Vec<u8>> = None;

    if let Some(data) = poster_data.filter(|d| d.starts_with("data:image")) {
        let Some((header, b64)) = data.split_once(',') else {
            return err400("Invalid image format from clipboard");
        };
        let ext = header.split('/').nth(1).unwrap_or("").split(';').next().unwrap_or("");
        if !matches!(ext, "jpeg" | "png" | "gif" | "webp") {
            return err400("Invalid image format from clipboard");
        }
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
            poster_bytes = Some(bytes);
        }
    } else if let Some((filename, bytes)) = poster_file {
        let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
        if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp") {
            return err400("Invalid image format");
        }
        poster_bytes = Some(bytes);
    }

    if let Some(bytes) = poster_bytes {
        // Poster lives next to the video file; the central posters dir is
        // only a fallback when the library tree is unwritable.
        if let Some(fp) = movie["file_path"].as_str() {
            if let Some(resolved) = crate::scanner::resolve_video_file(fp) {
                let sidecar = crate::scanner::sidecar_poster_path(&resolved);
                if std::fs::write(&sidecar, &bytes).is_ok() {
                    new_poster = Some(crate::scanner::POSTER_SIBLING.to_string());
                }
            }
        }
        if new_poster.is_none() {
            let name = format!("{}.jpg", uuid::Uuid::new_v4().simple());
            if std::fs::write(poster_dir.join(&name), &bytes).is_ok() {
                new_poster = Some(name);
            }
        }
        if let Some(val) = &new_poster {
            if let Some(old) = movie["poster_filename"].as_str() {
                if old != crate::scanner::POSTER_SIBLING && old != val {
                    let _ = std::fs::remove_file(poster_dir.join(old));
                }
            }
            let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
                .bind(val)
                .bind(&movie_id)
                .execute(db)
                .await;
        }
    }

    axum::Json(json!({
        "message": "Movie metadata updated successfully",
        "redirect": format!("/library/movie/movie/{movie_id}"),
    }))
    .into_response()
}

async fn move_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
    RawForm(raw): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&raw);
    let Some(target) = fv(&form, "target_category_id") else {
        return err400("Target category is required");
    };
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let old_cat = movie["category_id"].as_str().unwrap_or("").to_string();
    if !user_is_category_owner(db, &old_cat, user.id).await {
        return err403("Only the category owner can move movies");
    }
    let Some(target_category) = get_category(db, &target).await else {
        return err404("Target category not found or not owned by you");
    };
    if target_category["user_id"].as_i64() != Some(user.id) {
        return err404("Target category not found or not owned by you");
    }
    if target == old_cat {
        return err400("Movie is already in this category");
    }

    let old_path = movie["file_path"].as_str().unwrap_or("").to_string();
    let filename = std::path::Path::new(&old_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target_dir = target_category["directory_path"].as_str().unwrap_or("").to_string();
    let mut dest = std::path::Path::new(&target_dir).join(&filename);
    if dest.exists() {
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
        if std::path::Path::new(&old_path).is_dir() {
            dest = std::path::Path::new(&target_dir).join(format!("{suffix}_{filename}"));
        } else {
            let stem = std::path::Path::new(&filename)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = std::path::Path::new(&filename)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            dest = std::path::Path::new(&target_dir).join(format!("{stem}_{suffix}{ext}"));
        }
    }
    let dest_s = dest.to_string_lossy().into_owned();
    if std::path::Path::new(&old_path).is_dir() {
        if copy_path_recursive(std::path::Path::new(&old_path), &dest).is_ok() {
            let _ = std::fs::remove_dir_all(&old_path);
        }
    } else if std::fs::rename(&old_path, &dest).is_err()
        && std::fs::copy(&old_path, &dest).is_ok() {
            let _ = std::fs::remove_file(&old_path);
        }

    let movie_public = movie["is_public"] == json!(true);
    let target_public = target_category["is_public"] == json!(true);
    let new_public = if target_public { 1 } else { 0 };
    let _ = movie_public; // visibility follows the target category
    let _ = sqlx::query("UPDATE movies SET category_id = ?, file_path = ?, is_public = ?, updated_at = ? WHERE id = ?")
        .bind(&target)
        .bind(&dest_s)
        .bind(new_public)
        .bind(now_str())
        .bind(&movie_id)
        .execute(db)
        .await;

    refresh_movie_count(db, &old_cat).await;
    refresh_movie_count(db, &target).await;

    let tname = target_category["name"].as_str().unwrap_or("").to_string();
    axum::Json(json!({
        "message": format!("Moved '{}' to '{}'", movie["title"].as_str().unwrap_or(""), tname),
        "redirect": format!("/library/movie/movie/{movie_id}"),
    }))
    .into_response()
}

async fn delete_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
    RawForm(raw): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("Access denied");
    }
    let db = &state.db;
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let form = form_map(&raw);
    let delete_from_disk = form
        .get("delete_from_disk")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let category_id = movie["category_id"].as_str().unwrap_or("").to_string();
    let title = movie["title"].as_str().unwrap_or("").to_string();
    let file_path = movie["file_path"].as_str().unwrap_or("").to_string();
    let poster = movie["poster_filename"].as_str().map(|s| s.to_string());

    let _ = sqlx::query("DELETE FROM movies WHERE id = ?")
        .bind(&movie_id)
        .execute(db)
        .await;
    refresh_movie_count(db, &category_id).await;

    if let Some(p) = poster.as_deref().filter(|p| *p != crate::scanner::POSTER_SIBLING) {
        let _ = std::fs::remove_file(movie_posters_dir().join(p));
    }
    // The sidecar next to the video goes too (when not deleting the whole
    // directory from disk, which removes it anyway).
    if !delete_from_disk {
        if let Some(resolved) = crate::scanner::resolve_video_file(&file_path) {
            let _ = std::fs::remove_file(crate::scanner::sidecar_poster_path(&resolved));
        }
    }

    let mut disk_msg = String::new();
    if delete_from_disk && !file_path.is_empty() {
        let p = std::path::Path::new(&file_path);
        if p.exists() {
            if p.is_dir() {
                if std::fs::remove_dir_all(p).is_ok() {
                    disk_msg = " and files removed from disk".into();
                }
            } else if std::fs::remove_file(p).is_ok() {
                disk_msg = " and files removed from disk".into();
            }
        }
    }

    axum::Json(json!({"message": format!("Movie '{title}' deleted from database{disk_msg}")}))
        .into_response()
}

async fn generate_movie_poster(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
    Query(q): Query<Q>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let file_path = movie["file_path"].as_str().unwrap_or("").to_string();
    let skip = q.skip_seconds.unwrap_or(3).clamp(1, 300) as u32;
    let Some(poster) = extract_first_frame(&file_path, &movie_id, Some(skip)).await else {
        return err400("Could not extract frame from video. Check server logs for details.");
    };
    if let Some(old) = movie["poster_filename"].as_str() {
        if old != poster && old != crate::scanner::POSTER_SIBLING {
            let _ = std::fs::remove_file(movie_posters_dir().join(old));
        }
    }
    let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
        .bind(&poster)
        .bind(&movie_id)
        .execute(db)
        .await;
    axum::Json(json!({
        "message": format!("Poster generated for '{}'", movie["title"].as_str().unwrap_or("")),
        "poster": poster,
    }))
    .into_response()
}

fn ffmpeg_convert_dvd(input: String, output: String) -> bool {
    std::process::Command::new("ffmpeg")
        .args([
            "-y", "-nostdin", "-i", &input,
            "-c:v", "libx264", "-preset", "medium", "-crf", "23",
            "-c:a", "aac", "-b:a", "128k",
            "-movflags", "+faststart",
            &output,
        ])
        .output()
        .map(|o| {
            if !o.status.success() {
                println!("[library/movie] DVD conversion failed: {}", String::from_utf8_lossy(&o.stderr));
                false
            } else {
                true
            }
        })
        .unwrap_or(false)
        && std::path::Path::new(&output)
            .metadata()
            .map(|m| m.len() > 1000)
            .unwrap_or(false)
}

async fn convert_dvd(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let owned = sqlx::query(
        "SELECT m.* FROM movies m JOIN movie_category c ON m.category_id = c.id WHERE m.id = ? AND c.user_id = ?",
    )
    .bind(&movie_id)
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let Some(row) = owned else {
        return err404("Movie not found");
    };
    let movie = movie_row(&row);
    if movie["file_type"].as_str() != Some("dvd") {
        return err400("Movie is not a DVD");
    }
    let category_id = movie["category_id"].as_str().unwrap_or("").to_string();
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };

    let title = movie["title"].as_str().unwrap_or("").to_string();
    let safe_title: String = title
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || "-.()".contains(*c))
        .collect::<String>()
        .trim()
        .to_string();
    let category_dir = category["directory_path"].as_str().unwrap_or("").to_string();
    let mut output_filename = format!("{safe_title}.mp4");
    let mut output_path = std::path::Path::new(&category_dir).join(&output_filename);
    if output_path.exists() {
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
        output_filename = format!("{safe_title}_{suffix}.mp4");
        output_path = std::path::Path::new(&category_dir).join(&output_filename);
    }
    let output_s = output_path.to_string_lossy().into_owned();
    let file_path = movie["file_path"].as_str().unwrap_or("").to_string();
    let input_clone = file_path.clone();
    let output_clone = output_s.clone();
    let success = tokio::task::spawn_blocking(move || {
        ffmpeg_convert_dvd(input_clone, output_clone)
    })
    .await
    .ok()
    .unwrap_or(false);

    if !success {
        let _ = std::fs::remove_file(&output_s);
        return err_json(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "DVD conversion failed. Check server logs.");
    }

    let new_id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO movies (id, category_id, title, normalized_title, file_type, file_path, file_size, year, is_public, is_banned, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'mp4', ?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(&new_id)
    .bind(&category_id)
    .bind(&title)
    .bind(movie["normalized_title"].as_str().unwrap_or(""))
    .bind(&output_s)
    .bind(dir_size(&output_s))
    .bind(movie["year"].as_i64())
    .bind((movie["is_public"] == json!(true)) as i64)
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;
    refresh_movie_count(db, &category_id).await;

    axum::Json(json!({
        "success": true,
        "message": format!("Converted to MP4. New movie created at {output_s}"),
        "new_movie_id": new_id,
    }))
    .into_response()
}

async fn generate_missing_posters(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let rows: Vec<(String, String)> = sqlx::query(
        "SELECT m.id, m.file_path FROM movies m \
         JOIN movie_category c ON m.category_id = c.id \
         WHERE m.poster_filename IS NULL AND c.user_id = ?",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<String,_>("id"), r.try_get::<String,_>("file_path").unwrap_or_default()))
    .collect();

    let total = rows.len();
    let mut count = 0i64;
    for (id, fp) in rows {
        if let Some(poster) = extract_first_frame(&fp, &id, None).await {
            let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
                .bind(poster)
                .bind(&id)
                .execute(db)
                .await;
            count += 1;
        }
    }
    axum::Json(json!({"message": format!("Generated {count} posters from {total} movies without posters")}))
        .into_response()
}

// ------------------------------------------------------- public / private

async fn make_movie_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let category_id = movie["category_id"].as_str().unwrap_or("").to_string();
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err403("Only the category owner can do this");
    }
    if movie["is_banned"] == json!(true) {
        return err400("This movie has been banned from public view");
    }
    let _ = sqlx::query("UPDATE movies SET is_public = 1, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&movie_id)
        .execute(db)
        .await;
    let cat_public = get_category(db, &category_id)
        .await
        .map(|c| c["is_public"] == json!(true))
        .unwrap_or(false);
    if !cat_public {
        make_category_public(db, &category_id).await;
    }
    redirect(&format!("/library/movie/movie/{movie_id}"))
}

async fn remove_movie_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(movie) = get_movie(db, &movie_id).await else {
        return err404("Movie not found");
    };
    let category_id = movie["category_id"].as_str().unwrap_or("").to_string();
    if !user_is_category_owner(db, &category_id, user.id).await {
        return err403("Only the category owner can do this");
    }
    let _ = sqlx::query("UPDATE movies SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&movie_id)
        .execute(db)
        .await;
    redirect(&format!("/library/movie/movie/{movie_id}"))
}

async fn add_movies_quick(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&raw);
    let Some(file_path) = fv(&form, "file_path") else {
        return err400("File path and category are required.");
    };
    let Some(category_id) = fv(&form, "category_id") else {
        return err400("File path and category are required.");
    };
    if !std::path::Path::new(&file_path).exists() {
        return err400(&format!("Path not found: {file_path}"));
    }
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found.");
    };
    if category["user_id"].as_i64() != Some(user.id) {
        return err404("Category not found.");
    }

    let category_dir = category["directory_path"].as_str().unwrap_or("").to_string();
    let norm_import = std::fs::canonicalize(&file_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file_path.clone());
    let norm_cat = std::fs::canonicalize(&category_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| category_dir.clone());
    let in_place = norm_import == norm_cat || norm_import.starts_with(&format!("{norm_cat}/"));

    let known: Vec<String> = sqlx::query("SELECT file_path FROM movies")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.try_get::<String,_>("file_path").ok())
        .collect();

    let mut imported = 0i64;
    let mut skipped = 0i64;

    for src_path in collect_import_items(&norm_import) {
        let dest_path = if in_place {
            src_path.clone()
        } else {
            let dest_name = std::path::Path::new(&src_path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut dest = std::path::Path::new(&category_dir).join(&dest_name);
            if dest.exists() {
                let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
                if std::path::Path::new(&src_path).is_dir() {
                    dest = std::path::Path::new(&category_dir).join(format!("{suffix}_{dest_name}"));
                } else {
                    let stem = std::path::Path::new(&dest_name)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let ext = std::path::Path::new(&dest_name)
                        .extension()
                        .map(|s| format!(".{}", s.to_string_lossy()))
                        .unwrap_or_default();
                    dest = std::path::Path::new(&category_dir).join(format!("{stem}_{suffix}{ext}"));
                }
            }
            let dest_s = dest.to_string_lossy().into_owned();
            if std::path::Path::new(&src_path).is_dir() {
                let _ = copy_path_recursive(std::path::Path::new(&src_path), &dest);
            } else {
                let _ = std::fs::copy(&src_path, &dest);
            }
            dest_s
        };

        if known.iter().any(|k| k == &dest_path) {
            skipped += 1;
            continue;
        }

        let entry_name = std::path::Path::new(&dest_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = determine_movie_title(&entry_name);
        let year = extract_year_from_name(&entry_name);
        let id = new_uuid();
        let _ = sqlx::query(
            "INSERT INTO movies (id, category_id, title, normalized_title, file_type, file_path, file_size, year, is_public, is_banned, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&id)
        .bind(&category_id)
        .bind(&title)
        .bind(title.to_lowercase().trim())
        .bind(determine_movie_file_type(&dest_path))
        .bind(&dest_path)
        .bind(dir_size(&dest_path))
        .bind(year)
        .bind((category["is_public"] == json!(true)) as i64)
        .bind(now_str())
        .bind(now_str())
        .execute(db)
        .await;
        imported += 1;
    }

    if imported > 0 {
        refresh_movie_count(db, &category_id).await;
    }

    let mut msg = format!("Imported {imported} movie(s).");
    if skipped > 0 {
        msg.push_str(&format!(" Skipped {skipped} already in database."));
    }
    axum::Json(json!({"success": true, "message": msg})).into_response()
}

// ------------------------------------------------------------------ scan

async fn trigger_scan(State(state): State<SharedState>, RequireUser(_user): RequireUser,) -> Response {
    let db = state.db.clone();
    tokio::spawn(async move {
        scan_movies(&db, None, None, false).await;
    });
    axum::Json(json!({"message": "Movie scan started."})).into_response()
}

async fn scan_my_library(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let db = state.db.clone();
    let username = user.username.clone();
    tokio::spawn(async move {
        scan_movies(&db, None, Some(username), false).await;
    });
    axum::Json(json!({"message": "Scanning your movie library..."})).into_response()
}

async fn full_rescan(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = state.db.clone();
    let username = user.username.clone();
    let _ = sqlx::query(
        "DELETE FROM movies WHERE category_id IN (SELECT id FROM movie_category WHERE user_id = ?)",
    )
    .bind(user.id)
    .execute(&db)
    .await;
    let _ = sqlx::query(
        "DELETE FROM movie_category_shares WHERE category_id IN (SELECT id FROM movie_category WHERE user_id = ?)",
    )
    .bind(user.id)
    .execute(&db)
    .await;
    let _ = sqlx::query("DELETE FROM movie_category WHERE user_id = ?")
        .bind(user.id)
        .execute(&db)
        .await;

    tokio::spawn(async move {
        scan_movies(&db, None, Some(username), false).await;
    });
    axum::Json(json!({"message": "Deleted all movie data. Rescanning from disk..."})).into_response()
}

// ----------------------------------------------------------------- admin

async fn admin_page(State(state): State<SharedState>, RequireUser(user): RequireUser,) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;

    let map_rows = |rows: Vec<sqlx::sqlite::SqliteRow>| -> Vec<Value> {
        rows.iter()
            .map(|r| {
                json!({
                    "category": category_row(r),
                    "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
                })
            })
            .collect()
    };

    let public_categories = sqlx::query(
        "SELECT c.*, u.username AS owner_username FROM movie_category c \
         JOIN users u ON u.id = c.user_id WHERE c.is_public = 1 ORDER BY c.name",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let public_categories = map_rows(public_categories);

    let all_categories = sqlx::query(
        "SELECT c.*, u.username AS owner_username FROM movie_category c \
         JOIN users u ON u.id = c.user_id ORDER BY c.name",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let all_categories = map_rows(all_categories);

    let banned_rows = sqlx::query(
        "SELECT m.*, c.name AS category_name, c.user_id AS cat_user_id, c.normalized_name AS cat_norm, c.description AS cat_desc, c.is_public AS cat_public, c.directory_path AS cat_dir, c.movie_count AS cat_count, c.parent_id AS cat_parent, c.created_at AS cat_created, u.username AS owner_username \
         FROM movies m JOIN movie_category c ON m.category_id = c.id \
         JOIN users u ON u.id = c.user_id WHERE m.is_banned = 1 ORDER BY m.created_at DESC",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let banned_movies: Vec<Value> = banned_rows
        .iter()
        .map(|r| {
            json!({
                "movie": movie_row(r),
                "category": json!({
                    "id": r.get::<String,_>("category_id"),
                    "user_id": r.get::<i64,_>("cat_user_id"),
                    "name": r.try_get::<String,_>("category_name").unwrap_or_default(),
                    "normalized_name": r.try_get::<String,_>("cat_norm").unwrap_or_default(),
                    "description": r.try_get::<Option<String>,_>("cat_desc").ok().flatten(),
                    "is_public": r.get::<i64,_>("cat_public") == 1,
                    "directory_path": r.try_get::<String,_>("cat_dir").unwrap_or_default(),
                    "movie_count": r.get::<i64,_>("cat_count"),
                    "parent_id": r.try_get::<Option<String>,_>("cat_parent").ok().flatten(),
                    "created_at": r.try_get::<Option<String>,_>("cat_created").ok().flatten(),
                }),
                "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
            })
        })
        .collect();

    page(
        &state,
        "movie/admin.html",
        json!({
            "user": crate::core::user_json(&user),
            "public_categories": public_categories,
            "banned_movies": banned_movies,
            "all_categories": all_categories,
        }),
    )
}

async fn ban_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE movies SET is_banned = 1, is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&movie_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Movie banned and removed from public view."})).into_response()
}

async fn unban_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE movies SET is_banned = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&movie_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Movie unbanned."})).into_response()
}

async fn admin_remove_public_movie(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE movies SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&movie_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Movie removed from public view."})).into_response()
}

async fn admin_remove_public_category(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(category_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(category) = get_category(db, &category_id).await else {
        return err404("Category not found");
    };
    make_category_private(db, &category_id).await;
    let name = category["name"].as_str().unwrap_or("").to_string();
    axum::Json(json!({"message": format!("Category '{name}' removed from public.")})).into_response()
}

// ------------------------------------------------------------ static art

async fn static_poster(Path(file): Path<String>) -> Response {
    if file.contains("..") || file.contains('/') {
        return err404("not found");
    }
    let path = movie_posters_dir().join(&file);
    if !path.is_file() {
        return err404("not found");
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ctype = match file.rsplit('.').next().unwrap_or("") {
                "png" => "image/png",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "image/jpeg",
            };
            ([(axum::http::header::CONTENT_TYPE, ctype)], bytes).into_response()
        }
        Err(_) => err404("not found"),
    }
}

/// Serve a movie's poster: the sidecar jpg next to the video file first,
/// then the legacy central posters dir.
async fn movie_poster(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(movie_id): Path<String>,
) -> Response {
    let row = sqlx::query("SELECT file_path, poster_filename FROM movies WHERE id = ?")
        .bind(&movie_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else { return err404("not found") };
    let fp: String = row.try_get::<String,_>("file_path").unwrap_or_default();
    if let Some(resolved) = crate::scanner::resolve_video_file(&fp) {
        let sidecar = crate::scanner::sidecar_poster_path(&resolved);
        if std::path::Path::new(&sidecar).is_file() {
            return serve_file(&sidecar, None).await;
        }
    }
    if let Some(p) = row
        .try_get::<Option<String>,_>("poster_filename")
        .ok()
        .flatten()
        .filter(|p| p != crate::scanner::POSTER_SIBLING && !p.is_empty())
    {
        let path = movie_posters_dir().join(p);
        if path.is_file() {
            return serve_file(&path.to_string_lossy(), None).await;
        }
    }
    err404("not found")
}
