//! Audiobook sub-app: topics (shared directories), books, chapters,
//! player, playlists, admin. Ported from fastmain's audiobook router.

use axum::extract::{Path, Query, RawForm, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::path::Path as FsPath;

use crate::{
    err_json, form_map, fv, is_super, music, new_uuid, now_str, page, redirect, root, serve_file,
    shared_users, static_dir, RequireUser, SharedState,
};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/library/audiobook", get(index))
        .route("/library/audiobook/", get(index))
        .route("/library/audiobook/scan", get(trigger_scan))
        .route("/library/audiobook/clean", get(trigger_clean))
        .route("/library/audiobook/scan/force", get(force_scan))
        .route("/library/audiobook/browse/users", get(browse_users))
        .route("/library/audiobook/browse/users/{user_id}", get(browse_user_topics))
        .route("/library/audiobook/browse/topics", get(browse_topics))
        .route("/library/audiobook/browse/authors", get(browse_authors))
        .route("/library/audiobook/browse/albums", get(browse_albums))
        .route("/library/audiobook/browse/songs", get(browse_songs))
        .route("/library/audiobook/topic/new", get(new_topic_page).post(create_topic))
        .route("/library/audiobook/topic/{topic_id}", get(view_topic))
        .route("/library/audiobook/topic/{topic_id}/edit", get(edit_topic_page).post(update_topic))
        .route("/library/audiobook/topic/{topic_id}/toggle-visibility", post(toggle_topic_visibility))
        .route("/library/audiobook/topic/{topic_id}/delete", post(delete_topic))
        .route("/library/audiobook/topic/{topic_id}/share", post(share_topic))
        .route("/library/audiobook/topic/{topic_id}/revoke/{share_id}", post(revoke_share))
        .route("/library/audiobook/topic/{topic_id}/import", get(import_page))
        .route("/library/audiobook/topic/{topic_id}/import-path", post(import_from_path))
        .route("/library/audiobook/album/{album_id}", get(view_album))
        .route("/library/audiobook/player", get(player))
        .route("/library/audiobook/stream/{track_id}", get(stream_track))
        .route("/library/audiobook/search", get(search))
        .route("/library/audiobook/book/{book_id}/edit", get(edit_book_form))
        .route("/library/audiobook/book/{book_id}/move-topic", post(move_book_topic))
        .route("/library/audiobook/book/edit", post(update_book))
        .route("/library/audiobook/chapters/edit", post(update_chapters))
        .route("/library/audiobook/author/{author_id}", get(view_author))
        .route("/library/audiobook/playlists", get(list_playlists))
        .route("/library/audiobook/api/playlists", get(api_list_playlists))
        .route("/library/audiobook/playlists/import", get(import_playlist_page))
        .route("/library/audiobook/playlists/import/process", post(process_playlist_import))
        .route("/library/audiobook/playlists/{playlist_name}", get(view_playlist_details))
        .route("/library/audiobook/playlists/create", post(create_playlist))
        .route("/library/audiobook/playlists/{playlist_name}/add", post(add_to_playlist))
        .route("/library/audiobook/playlists/{playlist_name}/delete", post(delete_playlist))
        .route("/library/audiobook/playlists/{playlist_name}/item/{item_id}/delete", post(delete_playlist_item))
        .route("/library/audiobook/playlists/{playlist_name}/random/add", post(random_add_books))
        .route("/library/audiobook/playlists/{playlist_name}/analyze", get(analyze_playlist))
        .route("/library/audiobook/playlists/{playlist_name}/export", get(export_playlist))
        .route("/library/audiobook/add", post(add_books_manual))
        .route("/library/audiobook/admin", get(admin_page))
        .route("/library/audiobook/admin/topic/{topic_id}/toggle-visibility", post(admin_toggle_topic))
        .route("/library/audiobook/book/{book_id}/toggle-public", post(toggle_book_public))
        .route("/library/audiobook/book/{book_id}/ban", post(ban_book))
        .route("/library/audiobook/book/{book_id}/unban", post(unban_book))
        .route("/library/audiobook/book/{book_id}/art", get(book_art))
        .route("/library/audiobook_static/art/{file}", get(static_art))
}

fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}
fn err400(msg: &str) -> Response {
    err_json(axum::http::StatusCode::BAD_REQUEST, msg)
}
fn err403(msg: &str) -> Response {
    err_json(axum::http::StatusCode::FORBIDDEN, msg)
}

fn art_dir() -> std::path::PathBuf {
    static_dir("audiobook").join("art")
}

/// Visibility WHERE fragment for audiobook_books (aliased b) binding user id at ?1.
fn visibility(user: &syarc_plugin_api::User) -> String {
    if is_super(user) {
        String::new()
    } else {
        " AND (b.topic_id IN (SELECT id FROM audiobook_topics WHERE user_id = ?1) \
          OR b.topic_id IN (SELECT topic_id FROM audiobook_topic_shares WHERE shared_with_user_id = ?1) \
          OR (b.is_public = 1 AND b.is_banned = 0))"
            .to_string()
    }
}

async fn user_has_topic_access(db: &syarc_plugin_api::Db, topic_id: &str, user_id: i64) -> bool {
    let Ok(Some(r)) = sqlx::query("SELECT user_id, is_public FROM audiobook_topics WHERE id = ?")
        .bind(topic_id)
        .fetch_optional(db)
        .await
    else {
        return false;
    };
    if r.get::<i64,_>("user_id") == user_id || r.get::<i64,_>("is_public") == 1 {
        return true;
    }
    sqlx::query("SELECT 1 FROM audiobook_topic_shares WHERE topic_id = ? AND shared_with_user_id = ?")
        .bind(topic_id)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn get_topic(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query(
        "SELECT t.*, u.username AS owner_username FROM audiobook_topics t \
         JOIN users u ON u.id = t.user_id WHERE t.id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| {
        let mut t = topic_row(&r);
        t["owner"] =
            json!({"username": r.try_get::<String, _>("owner_username").unwrap_or_default()});
        t
    })
}

fn topic_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "user_id": r.get::<i64,_>("user_id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "normalized_name": r.try_get::<String,_>("normalized_name").unwrap_or_default(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "directory_path": r.try_get::<String,_>("directory_path").unwrap_or_default(),
        "book_count": r.get::<i64,_>("book_count"),
        "parent_id": r.try_get::<Option<String>,_>("parent_id").ok().flatten(),
        "created_at": r.try_get::<Option<String>,_>("created_at").ok().flatten(),
    })
}

fn book_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "title": r.try_get::<String,_>("title").unwrap_or_default(),
        "normalized_title": r.try_get::<String,_>("normalized_title").unwrap_or_default(),
        "author_id": r.get::<i64,_>("author_id"),
        "image_filename": r.try_get::<Option<String>,_>("image_filename").ok().flatten(),
        "updated_at": r.try_get::<Option<String>,_>("updated_at").ok().flatten(),
        "topic_id": r.try_get::<Option<String>,_>("topic_id").ok().flatten(),
        "user_id": r.try_get::<Option<i64>,_>("user_id").ok().flatten(),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "is_banned": r.get::<i64,_>("is_banned") == 1,
        "directory_path": r.try_get::<Option<String>,_>("directory_path").ok().flatten(),
    })
}

fn chapter_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "title": r.try_get::<String,_>("title").unwrap_or_default(),
        "file_path": r.try_get::<String,_>("file_path").unwrap_or_default(),
        "track_number": r.try_get::<Option<String>,_>("track_number").ok().flatten(),
        "size": r.try_get::<Option<f64>,_>("size").ok().flatten(),
        "book_id": r.get::<i64,_>("book_id"),
    })
}

fn author_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "normalized_name": r.try_get::<String,_>("normalized_name").unwrap_or_default(),
    })
}

async fn author_name(db: &syarc_plugin_api::Db, id: i64) -> String {
    sqlx::query("SELECT name FROM audiobook_authors WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("name").ok())
        .unwrap_or_else(|| "Unknown".into())
}

async fn get_book(db: &syarc_plugin_api::Db, id: i64) -> Option<Value> {
    sqlx::query("SELECT * FROM audiobook_books WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| book_row(&r))
}

async fn make_topic_public(
    db: &syarc_plugin_api::Db,
    topic_id: &str,
    owner_username: &str,
    topic_name: &str,
    _directory_path: &str,
) {
    let _ = sqlx::query("UPDATE audiobook_topics SET is_public = 1, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(topic_id)
        .execute(db)
        .await;
    let link = path_or_public().join(format!("{owner_username}_{topic_name}"));
    if link.is_symlink() {
        let _ = std::fs::remove_file(&link);
    }
    let _ = sqlx::query(
        "UPDATE audiobook_books SET is_public = 1 WHERE topic_id = ? AND is_banned = 0",
    )
    .bind(topic_id)
    .execute(db)
    .await;
}

fn path_or_public() -> std::path::PathBuf {
    std::path::PathBuf::from(root("audiobook")).join("public")
}

async fn make_topic_private(db: &syarc_plugin_api::Db, topic_id: &str) {
    let _ = sqlx::query("UPDATE audiobook_topics SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(topic_id)
        .execute(db)
        .await;
    if let Some(r) = sqlx::query(
        "SELECT u.username, t.name FROM audiobook_topics t JOIN users u ON u.id = t.user_id WHERE t.id = ?",
    )
    .bind(topic_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    {
        let link = path_or_public().join(format!(
            "{}_{}",
            r.try_get::<String,_>("username").unwrap_or_default(),
            r.try_get::<String,_>("name").unwrap_or_default()
        ));
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
    let _ = sqlx::query("UPDATE audiobook_books SET is_public = 0 WHERE topic_id = ?")
        .bind(topic_id)
        .execute(db)
        .await;
}

async fn refresh_topic_count(db: &syarc_plugin_api::Db, topic_id: &str) -> i64 {
    let count = sqlx::query("SELECT COUNT(*) AS c FROM audiobook_books WHERE topic_id = ?")
        .bind(topic_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    let _ = sqlx::query("UPDATE audiobook_topics SET book_count = ? WHERE id = ?")
        .bind(count)
        .bind(topic_id)
        .execute(db)
        .await;
    count
}

async fn playlists_dropdown(db: &syarc_plugin_api::Db, user_id: i64) -> Vec<Value> {
    sqlx::query(
        "SELECT name, MAX(created_at) AS latest FROM audiobook_playlists WHERE user_id = ? GROUP BY name ORDER BY latest DESC",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "name": r.try_get::<String,_>("name").unwrap_or_default(),
            "created_at": r.try_get::<String,_>("latest").unwrap_or_default(),
        })
    })
    .collect()
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
    author_id: Option<i64>,
    #[serde(default)]
    from_letter: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    shuffle: Option<String>,
}

fn one_letter(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .filter(|l| l.chars().count() == 1 && l.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|l| l.to_uppercase())
}

// ------------------------------------------------------------------- pages

async fn index(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let write_access = is_super(&user);

    let own_topics: Vec<Value> = if write_access {
        sqlx::query(
            "SELECT * FROM audiobook_topics WHERE user_id = ? AND parent_id IS NULL ORDER BY name",
        )
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(topic_row)
        .collect()
    } else {
        vec![]
    };

    let shared_topics: Vec<Value> = if write_access {
        sqlx::query(
            "SELECT t.*, u.username AS owner_username FROM audiobook_topic_shares sh \
             JOIN audiobook_topics t ON t.id = sh.topic_id \
             JOIN users u ON u.id = t.user_id \
             WHERE sh.shared_with_user_id = ? ORDER BY t.name",
        )
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let mut t = topic_row(r);
            t["owner_username"] = json!(r.try_get::<String,_>("owner_username").unwrap_or_default());
            t
        })
        .collect()
    } else {
        vec![]
    };

    let count = |sql: String, binds: Vec<i64>| async move {
        let mut q = sqlx::query(&sql);
        for b in binds {
            q = q.bind(b);
        }
        q.fetch_one(db).await.map(|r| r.get::<i64,_>("c")).unwrap_or(0)
    };

    let total_books = if write_access {
        count(
            "SELECT COUNT(*) AS c FROM audiobook_books b JOIN audiobook_topics t ON b.topic_id = t.id WHERE t.user_id = ?".into(),
            vec![user.id],
        )
        .await
    } else {
        0
    };
    let public_topic_count = count("SELECT COUNT(*) AS c FROM audiobook_topics WHERE is_public = 1".into(), vec![]).await;
    let public_books_count = count(
        "SELECT COUNT(*) AS c FROM audiobook_books b JOIN audiobook_topics t ON b.topic_id = t.id \
         WHERE t.is_public = 1 AND b.is_public = 1 AND b.is_banned = 0".into(),
        vec![],
    )
    .await;
    let legacy_book_count = count("SELECT COUNT(*) AS c FROM audiobook_books WHERE topic_id IS NULL".into(), vec![]).await;
    let total_authors = count("SELECT COUNT(*) AS c FROM audiobook_authors".into(), vec![]).await;
    let all_books_count = count("SELECT COUNT(*) AS c FROM audiobook_books".into(), vec![]).await;

    let vis = visibility(&user);
    let sql = format!(
        "SELECT b.*, a.name AS author_name FROM audiobook_books b \
         JOIN audiobook_authors a ON a.id = b.author_id WHERE 1=1 {vis} \
         ORDER BY b.id DESC LIMIT 12"
    );
    let recent_books: Vec<Value> = sqlx::query(&sql)
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let mut b = book_row(r);
            b["author"] = json!({"id": r.get::<i64,_>("author_id"), "name": r.try_get::<String,_>("author_name").unwrap_or_default()});
            b
        })
        .collect();

    page(
        &state,
        "audiobook/index.html",
        json!({
            "user": crate::core::user_json(&user),
            "own_topics": own_topics,
            "shared_topics": shared_topics,
            "total_books": total_books,
            "public_topic_count": public_topic_count,
            "public_books_count": public_books_count,
            "legacy_book_count": legacy_book_count,
            "total_authors": total_authors,
            "all_books_count": all_books_count,
            "recent_books": recent_books,
            "full_access": write_access,
            "write_access": write_access,
        }),
    )
}

async fn browse_users(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let sql = if is_super(&user) {
        "SELECT t.user_id, u.username, COUNT(t.id) AS topic_count FROM audiobook_topics t \
         JOIN users u ON u.id = t.user_id GROUP BY t.user_id, u.username ORDER BY u.username"
    } else {
        "SELECT t.user_id, u.username, COUNT(t.id) AS topic_count FROM audiobook_topics t \
         JOIN users u ON u.id = t.user_id WHERE t.is_public = 1 \
         GROUP BY t.user_id, u.username ORDER BY u.username"
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
                "topic_count": r.get::<i64,_>("topic_count"),
            })
        })
        .collect();
    page(
        &state,
        "audiobook/browse_users.html",
        json!({
            "user": crate::core::user_json(&user),
            "users_with_topics": users,
        }),
    )
}

async fn browse_user_topics(
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
    let rows = if user_id == user.id && is_super(&user) {
        sqlx::query(
            "SELECT * FROM audiobook_topics WHERE user_id = ? AND parent_id IS NULL ORDER BY name",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
    } else {
        sqlx::query(
            "SELECT * FROM audiobook_topics WHERE user_id = ? AND parent_id IS NULL AND ( \
              is_public = 1 OR id IN (SELECT topic_id FROM audiobook_topic_shares WHERE shared_with_user_id = ?) \
             ) ORDER BY name",
        )
        .bind(user_id)
        .bind(user.id)
        .fetch_all(db)
        .await
    }
    .unwrap_or_default();
    let topics: Vec<Value> = rows.iter().map(topic_row).collect();

    let mut counts = serde_json::Map::new();
    if let Ok(crows) = sqlx::query(
        "SELECT topic_id, COUNT(*) AS c FROM audiobook_books WHERE topic_id IN \
         (SELECT id FROM audiobook_topics WHERE user_id = ? AND parent_id IS NULL) GROUP BY topic_id",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
    {
        for r in crows {
            counts.insert(r.get::<String,_>("topic_id"), json!(r.get::<i64,_>("c")));
        }
    }

    page(
        &state,
        "audiobook/browse_user_topics.html",
        json!({
            "user": crate::core::user_json(&user),
            "target_user": {"id": user_id, "username": target.try_get::<String,_>("username").unwrap_or_default()},
            "topics": topics,
            "topic_book_counts": Value::Object(counts),
        }),
    )
}

async fn browse_topics(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let page_no = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(24).clamp(12, 120);

    let ids: Vec<String> = sqlx::query(
        "SELECT id FROM audiobook_topics WHERE user_id = ? \
         UNION SELECT topic_id FROM audiobook_topic_shares WHERE shared_with_user_id = ? \
         UNION SELECT id FROM audiobook_topics WHERE is_public = 1",
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

    let mut topics: Vec<Value> = Vec::new();
    if !ids.is_empty() {
        let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT t.*, u.username AS owner_username FROM audiobook_topics t \
             JOIN users u ON u.id = t.user_id WHERE t.id IN ({ph}) AND t.parent_id IS NULL \
             ORDER BY t.name LIMIT ? OFFSET ?"
        );
        let mut query = sqlx::query(&sql);
        for id in &ids {
            query = query.bind(id);
        }
        if let Ok(rows) = query.bind(per_page).bind(offset).fetch_all(db).await {
            let page_ids: Vec<String> = rows.iter().map(|r| r.get::<String,_>("id")).collect();
            let mut book_counts = serde_json::Map::new();
            let mut sub_counts = serde_json::Map::new();
            if !page_ids.is_empty() {
                let ph2 = page_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let bsql = format!(
                    "SELECT topic_id, COUNT(*) AS c FROM audiobook_books WHERE topic_id IN ({ph2}) GROUP BY topic_id"
                );
                let mut bq = sqlx::query(&bsql);
                for id in &page_ids {
                    bq = bq.bind(id);
                }
                if let Ok(brows) = bq.fetch_all(db).await {
                    for r in brows {
                        book_counts.insert(r.get::<String,_>("topic_id"), json!(r.get::<i64,_>("c")));
                    }
                }
                let ssql = format!(
                    "SELECT parent_id, COUNT(*) AS c FROM audiobook_topics WHERE parent_id IN ({ph2}) GROUP BY parent_id"
                );
                let mut sq = sqlx::query(&ssql);
                for id in &page_ids {
                    sq = sq.bind(id);
                }
                if let Ok(srows) = sq.fetch_all(db).await {
                    for r in srows {
                        sub_counts.insert(r.get::<String,_>("parent_id"), json!(r.get::<i64,_>("c")));
                    }
                }
            }
            topics = rows
                .iter()
                .map(|r| {
                    let id = r.get::<String,_>("id");
                    json!({
                        "topic": topic_row(r),
                        "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
                        "visible_book_count": book_counts.get(&id).cloned().unwrap_or(json!(0)),
                        "subtopic_count": sub_counts.get(&id).cloned().unwrap_or(json!(0)),
                    })
                })
                .collect();
        }
    }

    page(
        &state,
        "audiobook/browse_topics.html",
        json!({
            "user": crate::core::user_json(&user),
            "topics": topics,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
            "full_access": is_super(&user),
        }),
    )
}

// ------------------------------------------------------------- topic CRUD

async fn new_topic_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let own: Vec<Value> = sqlx::query(
        "SELECT * FROM audiobook_topics WHERE user_id = ? ORDER BY name",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .iter()
    .map(topic_row)
    .collect();
    page(
        &state,
        "audiobook/topic_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "topic": Value::Null,
            "mode": "new",
            "own_topics": own,
        }),
    )
}

async fn create_topic(
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
        return redirect("/library/audiobook/topic/new");
    };
    let norm = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");

    let dup = sqlx::query(
        "SELECT 1 FROM audiobook_topics WHERE user_id = ? AND normalized_name = ?",
    )
    .bind(user.id)
    .bind(&norm)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some();
    if dup {
        return redirect("/library/audiobook/topic/new");
    }

    let mut parent_id: Option<String> = None;
    let topic_dir: String;
    if let Some(pid) = fv(&form, "parent_id") {
        match sqlx::query("SELECT directory_path FROM audiobook_topics WHERE id = ? AND user_id = ?")
            .bind(&pid)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
        {
            Some(r) => {
                topic_dir = format!("{}/{}", r.try_get::<String,_>("directory_path").unwrap_or_default(), name);
                parent_id = Some(pid);
            }
            None => return redirect("/library/audiobook/topic/new"),
        }
    } else {
        topic_dir = format!("{}/{}/{}", root("audiobook"), user.username, name);
    }
    let _ = std::fs::create_dir_all(&topic_dir);

    let id = new_uuid();
    let is_public = fv(&form, "is_public").as_deref() == Some("on");
    let _ = sqlx::query(
        "INSERT INTO audiobook_topics (id, user_id, name, normalized_name, description, is_public, \
         directory_path, book_count, parent_id, created_at, updated_at) \
         VALUES (?,?,?,?,?,?,?,0,?,?,?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(&name)
    .bind(&norm)
    .bind(fv(&form, "description"))
    .bind(is_public as i64)
    .bind(&topic_dir)
    .bind(&parent_id)
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;

    if is_public {
        make_topic_public(db, &id, &user.username, &name, &topic_dir).await;
    }
    redirect(&format!("/library/audiobook/topic/{id}"))
}

async fn view_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    let full_access = is_super(&user);
    let is_owner = topic["user_id"].as_i64() == Some(user.id) && full_access;
    if !is_owner && !user_has_topic_access(db, &topic_id, user.id).await && topic["is_public"] != json!(true) {
        return err403("Access denied");
    }

    let page_no = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(24).clamp(12, 120);
    let letter = one_letter(&q.letter);

    let mut cond = String::new();
    if !is_owner {
        if topic["is_public"] == json!(true) {
            cond = " AND b.is_banned = 0".into();
        } else {
            cond = " AND b.is_public = 1 AND b.is_banned = 0".into();
        }
    }
    if letter.is_some() {
        cond.push_str(" AND b.title LIKE ?");
    }

    let sqlc = format!("SELECT COUNT(*) AS c FROM audiobook_books b WHERE b.topic_id = ? {cond}");
    let mut cq = sqlx::query(&sqlc).bind(&topic_id);
    if letter.is_some() {
        cq = cq.bind(format!("{}%", letter.clone().unwrap()));
    }
    let total = cq.fetch_one(db).await.map(|r| r.get::<i64,_>("c")).unwrap_or(0);
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_no - 1) * per_page;

    let sqlb = format!(
        "SELECT b.*, a.name AS author_name FROM audiobook_books b \
         JOIN audiobook_authors a ON a.id = b.author_id \
         WHERE b.topic_id = ? {cond} ORDER BY b.title LIMIT ? OFFSET ?"
    );
    let mut bq = sqlx::query(&sqlb).bind(&topic_id);
    if let Some(ref l) = letter {
        bq = bq.bind(format!("{l}%"));
    }
    let books: Vec<Value> = bq
        .bind(per_page)
        .bind(offset)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let mut b = book_row(r);
            b["author"] = json!({"id": r.get::<i64,_>("author_id"), "name": r.try_get::<String,_>("author_name").unwrap_or_default()});
            b
        })
        .collect();

    let subtopics: Vec<Value> = sqlx::query(
        "SELECT * FROM audiobook_topics WHERE parent_id = ? ORDER BY name",
    )
    .bind(&topic_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(topic_row)
    .collect();

    let mut sub_counts = serde_json::Map::new();
    if let Ok(rows) = sqlx::query(
        "SELECT topic_id, COUNT(*) AS c FROM audiobook_books WHERE topic_id IN \
         (SELECT id FROM audiobook_topics WHERE parent_id = ?) GROUP BY topic_id",
    )
    .bind(&topic_id)
    .fetch_all(db)
    .await
    {
        for r in rows {
            sub_counts.insert(r.get::<String,_>("topic_id"), json!(r.get::<i64,_>("c")));
        }
    }

    let shared = if is_owner {
        shared_users(db, "audiobook_topic_shares", "topic_id", &topic_id).await
    } else {
        vec![]
    };

    let parent_name = match topic["parent_id"].as_str() {
        Some(pid) => sqlx::query("SELECT name FROM audiobook_topics WHERE id = ?")
            .bind(pid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String,_>("name").ok()),
        None => None,
    };

    page(
        &state,
        "audiobook/topic.html",
        json!({
            "user": crate::core::user_json(&user),
            "topic": topic,
            "books": books,
            "subtopics": subtopics,
            "subtopic_book_counts": Value::Object(sub_counts),
            "is_owner": is_owner,
            "shared_users": shared,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
            "current_letter": letter,
            "full_access": full_access,
            "parent_name": parent_name,
        }),
    )
}

async fn edit_topic_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !is_super(&user) {
        return err403("write access required");
    }
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    if topic["user_id"].as_i64() != Some(user.id) {
        return err403("Access denied");
    }
    let shared = shared_users(db, "audiobook_topic_shares", "topic_id", &topic_id).await;
    let shared_ids: Vec<i64> = shared
        .iter()
        .filter_map(|s| s["shared_user"]["id"].as_i64())
        .collect();
    let all_users = crate::all_users(db, user.id)
        .await
        .into_iter()
        .filter(|u| !shared_ids.contains(&u["id"].as_i64().unwrap_or(0)))
        .collect::<Vec<_>>();
    let own: Vec<Value> = sqlx::query(
        "SELECT * FROM audiobook_topics WHERE user_id = ? AND id != ? ORDER BY name",
    )
    .bind(user.id)
    .bind(&topic_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(topic_row)
    .collect();

    page(
        &state,
        "audiobook/topic_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "topic": topic,
            "mode": "edit",
            "shared_users": shared,
            "all_users": all_users,
            "own_topics": own,
        }),
    )
}

async fn update_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return redirect("/library/audiobook/");
    };
    if topic["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/audiobook/");
    }
    let form = form_map(&form);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("/library/audiobook/topic/{topic_id}/edit"));
    };
    let norm = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let was_public = topic["is_public"] == json!(true);
    let is_public = fv(&form, "is_public").as_deref() == Some("on");

    let _ = sqlx::query(
        "UPDATE audiobook_topics SET name = ?, normalized_name = ?, description = ?, is_public = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(&norm)
    .bind(fv(&form, "description"))
    .bind(is_public as i64)
    .bind(now_str())
    .bind(&topic_id)
    .execute(db)
    .await;

    if is_public && !was_public {
        let dir = topic["directory_path"].as_str().unwrap_or_default().to_string();
        make_topic_public(db, &topic_id, &user.username, &name, &dir).await;
    } else if !is_public && was_public {
        make_topic_private(db, &topic_id).await;
    }
    redirect(&format!("/library/audiobook/topic/{topic_id}"))
}

async fn toggle_topic_visibility(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    if topic["is_public"] == json!(true) {
        make_topic_private(db, &topic_id).await;
        axum::Json(json!({"message": "Topic is now private.", "is_public": false})).into_response()
    } else {
        let dir = topic["directory_path"].as_str().unwrap_or_default().to_string();
        let name = topic["name"].as_str().unwrap_or_default().to_string();
        make_topic_public(db, &topic_id, &user.username, &name, &dir).await;
        axum::Json(json!({"message": "Topic is now public.", "is_public": true})).into_response()
    }
}

async fn delete_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return redirect("/library/audiobook/");
    };
    if topic["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/audiobook/");
    }
    let books = sqlx::query("SELECT COUNT(*) AS c FROM audiobook_books WHERE topic_id = ?")
        .bind(&topic_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    if books > 0 {
        return redirect(&format!("/library/audiobook/topic/{topic_id}"));
    }
    let children = sqlx::query("SELECT COUNT(*) AS c FROM audiobook_topics WHERE parent_id = ?")
        .bind(&topic_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    if children > 0 {
        return redirect(&format!("/library/audiobook/topic/{topic_id}"));
    }
    if topic["is_public"] == json!(true) {
        make_topic_private(db, &topic_id).await;
    }
    let dir = topic["directory_path"].as_str().unwrap_or_default().to_string();
    let _ = sqlx::query("DELETE FROM audiobook_topic_shares WHERE topic_id = ?")
        .bind(&topic_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM audiobook_topics WHERE id = ?")
        .bind(&topic_id)
        .execute(db)
        .await;
    if FsPath::new(&dir).is_dir() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    redirect("/library/audiobook/")
}

async fn share_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return redirect("/library/audiobook/");
    };
    if topic["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/audiobook/");
    }
    let form = form_map(&form);
    let Some(share_with) = fv(&form, "share_with_user_id") else {
        return redirect(&format!("/library/audiobook/topic/{topic_id}"));
    };
    let Ok(target_id) = share_with.parse::<i64>() else {
        return redirect(&format!("/library/audiobook/topic/{topic_id}"));
    };
    if target_id == user.id {
        return redirect(&format!("/library/audiobook/topic/{topic_id}"));
    }
    let exists = sqlx::query(
        "SELECT 1 FROM audiobook_topic_shares WHERE topic_id = ? AND shared_with_user_id = ?",
    )
    .bind(&topic_id)
    .bind(target_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some();
    if exists {
        return redirect(&format!("/library/audiobook/topic/{topic_id}"));
    }
    let _ = sqlx::query(
        "INSERT INTO audiobook_topic_shares (topic_id, shared_with_user_id, created_at) VALUES (?,?,?)",
    )
    .bind(&topic_id)
    .bind(target_id)
    .bind(now_str())
    .execute(db)
    .await;
    redirect(&format!("/library/audiobook/topic/{topic_id}/edit"))
}

async fn revoke_share(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((topic_id, share_id)): Path<(String, i64)>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return redirect("/library/audiobook/");
    };
    if topic["user_id"].as_i64() != Some(user.id) {
        return redirect("/library/audiobook/");
    }
    let _ = sqlx::query("DELETE FROM audiobook_topic_shares WHERE id = ? AND topic_id = ?")
        .bind(share_id)
        .bind(&topic_id)
        .execute(db)
        .await;
    redirect(&format!("/library/audiobook/topic/{topic_id}/edit"))
}

async fn import_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let Some(topic) = get_topic(&state.db, &topic_id).await else {
        return err404("Topic not found");
    };
    page(
        &state,
        "audiobook/import.html",
        json!({"user": crate::core::user_json(&user), "topic": topic}),
    )
}

fn is_audio_ext(ext: &str) -> bool {
    matches!(ext, "mp3" | "m4b" | "m4a" | "flac" | "ogg" | "opus")
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

async fn get_or_create_author(db: &syarc_plugin_api::Db, name: &str) -> i64 {
    let name = if name.trim().is_empty() { "Unknown Author" } else { name.trim() };
    let norm = name.to_lowercase();
    if let Some(r) = sqlx::query("SELECT id FROM audiobook_authors WHERE normalized_name = ?")
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    {
        return r.get(0);
    }
    sqlx::query("INSERT INTO audiobook_authors (name, normalized_name) VALUES (?,?)")
        .bind(name)
        .bind(&norm)
        .execute(db)
        .await
        .map(|r| r.last_insert_rowid())
        .unwrap_or(0)
}

fn extract_cover_art(audio_path: &str, book_id: i64, directory_path: &str) -> Option<String> {
    let (data, _is_png) = music::embedded_art(audio_path)?;
    write_book_art(directory_path, book_id, &data)
}

/// Sentinel stored in audiobook_books.image_filename when the art is
/// cover.jpg inside the book's own directory in the library tree.
pub(crate) const ART_SIBLING: &str = "sibling";

/// Write book art as `<directory_path>/cover.jpg` (next to the audio
/// files); falls back to the legacy central art dir when the directory is
/// missing or unwritable. Returns the image_filename value to store.
pub(crate) fn write_book_art(directory_path: &str, book_id: i64, data: &[u8]) -> Option<String> {
    if !directory_path.is_empty() {
        let p = std::path::Path::new(directory_path).join("cover.jpg");
        if std::fs::write(&p, data).is_ok() {
            return Some(ART_SIBLING.to_string());
        }
    }
    let dir = art_dir();
    let _ = std::fs::create_dir_all(&dir);
    let filename = format!("book_{book_id}.jpg");
    std::fs::write(dir.join(&filename), data).ok()?;
    Some(filename)
}

async fn import_from_path(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("write access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    let form = form_map(&form);
    let Some(import_path) = fv(&form, "import_path") else {
        return redirect(&format!("/library/audiobook/topic/{topic_id}/import"));
    };
    let make_public = form.get("make_public").map(|v| v == "on").unwrap_or(false);
    let base = FsPath::new(&import_path);
    if !base.exists() {
        return redirect(&format!("/library/audiobook/topic/{topic_id}/import"));
    }

    let topic_dir = topic["directory_path"].as_str().unwrap_or_default().to_string();
    let topic_is_public = topic["is_public"] == json!(true);
    let is_public = make_public || topic_is_public;
    let is_inside = import_path == topic_dir || import_path.starts_with(&format!("{topic_dir}/"));

    let known: Vec<String> = if is_inside {
        sqlx::query(
            "SELECT c.file_path FROM audiobook_chapters c JOIN audiobook_books b ON b.id = c.book_id WHERE b.topic_id = ?",
        )
        .bind(&topic_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("file_path"))
        .collect()
    } else {
        vec![]
    };

    // Collect audio files: (path, author_dir_name)
    let mut collected: Vec<(String, String)> = Vec::new();
    let walk = |start: &FsPath| -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![start.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let fname = e.file_name().to_string_lossy().to_string();
                if !is_audio_ext(&ext_of(&fname)) {
                    continue;
                }
                let full = p.to_string_lossy().to_string();
                let author_dir = p
                    .parent()
                    .and_then(|par| par.file_name())
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unknown Author".into());
                out.push((full, author_dir));
            }
        }
        out
    };
    if base.is_file() {
        if is_audio_ext(&ext_of(&import_path)) {
            let author_dir = base
                .parent()
                .and_then(|par| par.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Unknown Author".into());
            collected.push((import_path.clone(), author_dir));
        }
    } else {
        collected = walk(base);
    }
    collected.sort();

    let mut imported = 0i64;
    for (full_path, author_dir_name) in collected {
        if is_inside && known.contains(&full_path) {
            continue;
        }
        let dest = if is_inside {
            full_path.clone()
        } else {
            let dest_name = full_path.rsplit('/').next().unwrap_or("book").to_string();
            let mut cand = format!("{topic_dir}/{dest_name}");
            if FsPath::new(&cand).exists() {
                let (stem_base, stem_ext) = dest_name.rsplit_once('.').map(|(s, e)| (s.to_string(), e.to_string())).unwrap();
                cand = format!("{topic_dir}/{}_{}.{stem_ext}", stem_base, &new_uuid().replace('-', "")[..8]);
            }
            if std::fs::copy(&full_path, &cand).is_err() {
                continue;
            }
            cand
        };

        let ext = ext_of(&dest);
        let meta = music::read_audio_meta(&dest, false);
        let (book_title, author_name) = if ext == "m4b" || ext == "m4a" {
            (
                if meta.title.is_empty() || meta.title == full_path.rsplit('/').next().unwrap_or("") {
                    dest.rsplit_once('.').map(|(s, _)| s.rsplit('/').next().unwrap_or("").to_string()).unwrap_or_default()
                } else {
                    meta.title.clone()
                },
                meta.artist.unwrap_or(author_dir_name),
            )
        } else {
            (author_dir_name.clone(), "Unknown Author".to_string())
        };

        let author_id = get_or_create_author(db, &author_name).await;
        let title = if book_title.is_empty() { "Unknown Title".into() } else { book_title };
        let res = sqlx::query(
            "INSERT INTO audiobook_books (title, normalized_title, author_id, topic_id, user_id, is_public, is_banned, directory_path, updated_at) \
             VALUES (?,?,?,?,?,?,0,?,?)",
        )
        .bind(&title)
        .bind(title.to_lowercase().trim())
        .bind(author_id)
        .bind(&topic_id)
        .bind(user.id)
        .bind(is_public as i64)
        .bind(dest.rsplit_once('/').map(|(d, _)| d.to_string()))
        .bind(now_str())
        .execute(db)
        .await;
        let Ok(res) = res else { continue };
        let book_id = res.last_insert_rowid();

        let _ = sqlx::query(
            "INSERT INTO audiobook_chapters (title, file_path, track_number, size, book_id) VALUES (?,?,?,?,?)",
        )
        .bind(&meta.title)
        .bind(&dest)
        .bind(meta.track_num.as_deref().or(Some("1")))
        .bind(meta.size_mb)
        .bind(book_id)
        .execute(db)
        .await;

        let book_dir = std::path::Path::new(&dest)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(art) = extract_cover_art(&dest, book_id, &book_dir) {
            let _ = sqlx::query("UPDATE audiobook_books SET image_filename = ? WHERE id = ?")
                .bind(&art)
                .bind(book_id)
                .execute(db)
                .await;
        }
        imported += 1;
    }

    if imported > 0 {
        refresh_topic_count(db, &topic_id).await;
        if topic_is_public {
            let name = topic["name"].as_str().unwrap_or_default().to_string();
            make_topic_public(db, &topic_id, &user.username, &name, &topic_dir).await;
        }
    }
    redirect(&format!("/library/audiobook/topic/{topic_id}"))
}

// ------------------------------------------------------------- book detail

async fn view_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<i64>,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let Some(album) = get_book(db, album_id).await else {
        return err404("Album not found");
    };
    if !is_super(&user) {
        let mut has_access = false;
        if let Some(tid) = album["topic_id"].as_str() {
            has_access = user_has_topic_access(db, tid, user.id).await;
        }
        if !has_access && !(album["is_public"] == json!(true) && album["is_banned"] != json!(true)) {
            return err403("Access denied");
        }
    }

    let author = sqlx::query("SELECT * FROM audiobook_authors WHERE id = ?")
        .bind(album["author_id"].as_i64().unwrap_or(0))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| author_row(&r))
        .unwrap_or(json!({"id": 0, "name": "Unknown"}));

    let tracks: Vec<Value> = sqlx::query(
        "SELECT * FROM audiobook_chapters WHERE book_id = ? ORDER BY track_number, title",
    )
    .bind(album_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(chapter_row)
    .collect();

    let playlists = playlists_dropdown(db, user.id).await;
    let topic = match album["topic_id"].as_str() {
        Some(tid) => get_topic(db, tid).await,
        None => None,
    };

    page(
        &state,
        "audiobook/audiobook.html",
        json!({
            "user": crate::core::user_json(&user),
            "album": album,
            "author": author,
            "tracks": tracks,
            "playlists": playlists,
            "from_letter": q.from_letter,
            "source": q.source,
            "topic": topic,
            "write_access": is_super(&user),
        }),
    )
}

async fn player(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let source = q.source.unwrap_or_else(|| "all".into());
    let shuffle = q.shuffle.unwrap_or_else(|| "true".into());
    let vis = visibility(&user);

    let mut tracks: Vec<Value> = Vec::new();
    let source_name: String;

    let track_select = |cond: &str| {
        format!(
            "SELECT c.id, c.title, c.track_number, b.id AS book_id, b.title AS book_title, \
             b.image_filename, a.id AS author_id, a.name AS author_name \
             FROM audiobook_chapters c \
             JOIN audiobook_books b ON b.id = c.book_id \
             JOIN audiobook_authors a ON a.id = b.author_id WHERE {cond}"
        )
    };
    let to_track = |r: &sqlx::sqlite::SqliteRow| {
        json!({
            "id": r.get::<i64,_>("id"),
            "title": r.try_get::<String,_>("title").unwrap_or_default(),
            "trackNumber": r.try_get::<Option<String>,_>("track_number").ok().flatten(),
            "albumId": r.get::<i64,_>("book_id"),
            "albumTitle": r.try_get::<String,_>("book_title").unwrap_or_default(),
            "albumImage": r.try_get::<Option<String>,_>("image_filename").ok().flatten(),
            "artistId": r.get::<i64,_>("author_id"),
            "artistName": r.try_get::<String,_>("author_name").unwrap_or_default(),
        })
    };

    if let Some(album_id) = source.strip_prefix("album:") {
        let Ok(id) = album_id.parse::<i64>() else {
            return err404("No tracks found for this source");
        };
        let Some(album) = get_book(db, id).await else {
            return err403("Access denied");
        };
        source_name = format!(
            "{} - {}",
            album["title"].as_str().unwrap_or(""),
            author_name(db, album["author_id"].as_i64().unwrap_or(0)).await
        );
        let sql = track_select(&format!("1=1 {vis} AND c.book_id = ? ORDER BY c.track_number"));
        let mut tq = sqlx::query(&sql);
        if !vis.is_empty() {
            tq = tq.bind(user.id);
        }
        tracks = tq
            .bind(id)
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .iter()
            .map(to_track)
            .collect();
    } else if let Some(pname) = source.strip_prefix("playlist:") {
        source_name = format!("Playlist: {pname}");
        let items: Vec<(String, i64)> = sqlx::query(
            "SELECT item_type, item_id FROM audiobook_playlist_items \
             WHERE playlist_name = ? AND user_id = ? ORDER BY id ASC",
        )
        .bind(pname)
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| (r.get::<String,_>("item_type"), r.get::<i64,_>("item_id")))
        .collect();

        for (item_type, item_id) in items {
            let cond = if item_type == "album" {
                "c.book_id = ?"
            } else {
                "c.id = ?"
            };
            let sql = track_select(&format!("1=1 {vis} AND {cond} ORDER BY c.track_number"));
            let mut tq = sqlx::query(&sql);
            if !vis.is_empty() {
                tq = tq.bind(user.id);
            }
            if let Ok(rows) = tq
                .bind(item_id)
                .fetch_all(db)
                .await
            {
                for r in rows {
                    tracks.push(to_track(&r));
                }
            }
        }
    } else if let Some(song_id) = source.strip_prefix("song:") {
        let Ok(id) = song_id.parse::<i64>() else {
            return err404("No tracks found for this source");
        };
        source_name = "Now Playing".into();
        let sql = track_select("c.id = ?");
        if let Ok(rows) = sqlx::query(&sql).bind(id).fetch_all(db).await {
            for r in rows {
                tracks.push(to_track(&r));
            }
        }
    } else {
        source_name = "Random Shuffle".into();
        let sql = track_select(&format!("1=1 {vis} ORDER BY RANDOM() LIMIT 1000"));
        tracks = sqlx::query(&sql)
            .bind(user.id)
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .iter()
            .map(to_track)
            .collect();
    }

    if tracks.is_empty() {
        return err404("No tracks found for this source");
    }

    page(
        &state,
        "audiobook/player.html",
        json!({
            "tracks": tracks,
            "source": source,
            "source_name": source_name,
            "shuffle": shuffle != "false",
        }),
    )
}

async fn stream_track(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(track_id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let db = &state.db;
    let Some(track) = sqlx::query("SELECT file_path, book_id FROM audiobook_chapters WHERE id = ?")
        .bind(track_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    else {
        return err404(&format!("Track {track_id} not found"));
    };
    let file_path = track.get::<String,_>("file_path");
    if !is_super(&user) {
        let Some(book) = get_book(db, track.get::<i64,_>("book_id")).await else {
            return err403("Access denied");
        };
        let mut has_access = false;
        if let Some(tid) = book["topic_id"].as_str() {
            has_access = user_has_topic_access(db, tid, user.id).await;
        }
        if !has_access && !(book["is_public"] == json!(true) && book["is_banned"] != json!(true)) {
            return err403("Access denied");
        }
    }
    if !FsPath::new(&file_path).exists() {
        return err404("Audio file missing from disk.");
    }
    serve_file(&file_path, headers.get("range").and_then(|v| v.to_str().ok())).await
}

async fn search(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let Some(query) = fv(&std::collections::HashMap::from([(
        "q".to_string(),
        q.q.clone().unwrap_or_default(),
    )]), "q") else {
        return axum::Json(json!({"authors": [], "albums": [], "tracks": []})).into_response();
    };
    let vis = visibility(&user);
    let pat = format!("%{query}%");

    let authors: Vec<Value> = sqlx::query(
        "SELECT * FROM audiobook_authors WHERE normalized_name LIKE ?",
    )
    .bind(format!("%{}%", query.to_lowercase()))
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(author_row)
    .collect();
    let author_ids: Vec<i64> = authors.iter().filter_map(|a| a["id"].as_i64()).collect();

    let mut albums: Vec<Value> = Vec::new();
    {
        let sql = format!(
            "SELECT b.*, a.name AS author_name FROM audiobook_books b \
             JOIN audiobook_authors a ON a.id = b.author_id WHERE 1=1 {vis} AND b.title LIKE ?"
        );
        let mut sq = sqlx::query(&sql);
        if !vis.is_empty() {
            sq = sq.bind(user.id);
        }
        if let Ok(rows) = sq.bind(&pat).fetch_all(db).await {
            for r in rows {
                let mut b = book_row(&r);
                b["author"] = json!({"id": r.get::<i64,_>("author_id"), "name": r.try_get::<String,_>("author_name").unwrap_or_default()});
                albums.push(b);
            }
        }
        if !author_ids.is_empty() {
            let ph = author_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT b.*, a.name AS author_name FROM audiobook_books b \
                 JOIN audiobook_authors a ON a.id = b.author_id WHERE 1=1 {vis} AND b.author_id IN ({ph})"
            );
            let mut aq = sqlx::query(&sql);
            if !vis.is_empty() {
                aq = aq.bind(user.id);
            }
            for id in &author_ids {
                aq = aq.bind(id);
            }
            if let Ok(rows) = aq.fetch_all(db).await {
                let existing: Vec<i64> = albums.iter().filter_map(|b| b["id"].as_i64()).collect();
                for r in rows {
                    let id = r.get::<i64,_>("id");
                    if existing.contains(&id) {
                        continue;
                    }
                    let mut b = book_row(&r);
                    b["author"] = json!({"id": r.get::<i64,_>("author_id"), "name": r.try_get::<String,_>("author_name").unwrap_or_default()});
                    albums.push(b);
                }
            }
        }
    }

    let mut track_data: Vec<Value> = Vec::new();
    if let Ok(rows) = sqlx::query(
        "SELECT c.id, c.title, b.id AS book_id, b.title AS book_title, b.author_id, \
         b.topic_id, b.is_public, b.is_banned \
         FROM audiobook_chapters c JOIN audiobook_books b ON b.id = c.book_id \
         WHERE c.title LIKE ?",
    )
    .bind(&pat)
    .fetch_all(db)
    .await
    {
        for r in rows {
            let is_public = r.get::<i64,_>("is_public") == 1;
            let banned = r.get::<i64,_>("is_banned") == 1;
            let mut visible = is_super(&user);
            if !visible {
                if let Some(tid) = r.try_get::<Option<String>,_>("topic_id").ok().flatten() {
                    visible = user_has_topic_access(db, &tid, user.id).await;
                }
            }
            if !visible && is_public && !banned {
                visible = true;
            }
            if !visible {
                continue;
            }
            track_data.push(json!({
                "id": r.get::<i64,_>("id"),
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "album_id": r.get::<i64,_>("book_id"),
                "album_title": r.try_get::<String,_>("book_title").unwrap_or_default(),
                "author_name": author_name(db, r.get::<i64,_>("author_id")).await,
            }));
        }
    }

    axum::Json(json!({
        "authors": authors,
        "albums": albums,
        "tracks": track_data,
    }))
    .into_response()
}

// ------------------------------------------------------------ edit metadata

async fn edit_book_form(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
    Query(q): Query<Q>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(album) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    let author = sqlx::query("SELECT * FROM audiobook_authors WHERE id = ?")
        .bind(album["author_id"].as_i64().unwrap_or(0))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| author_row(&r))
        .unwrap_or(json!({"id": 0, "name": "Unknown"}));
    let tracks: Vec<Value> = sqlx::query(
        "SELECT * FROM audiobook_chapters WHERE book_id = ? ORDER BY track_number, title",
    )
    .bind(book_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(chapter_row)
    .collect();

    let topics: Vec<Value> = sqlx::query(
        "SELECT t.id, t.name, t.directory_path, u.username AS owner FROM audiobook_topics t \
         LEFT JOIN users u ON u.id = t.user_id ORDER BY u.username, t.name",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<String,_>("id"),
            "name": r.get::<String,_>("name"),
            "directory_path": r.try_get::<String,_>("directory_path").unwrap_or_default(),
            "owner": r.try_get::<String,_>("owner").unwrap_or_default(),
        })
    })
    .collect();

    page(
        &state,
        "audiobook/edit_book.html",
        json!({
            "user": crate::core::user_json(&user),
            "album": album,
            "author": author,
            "tracks": tracks,
            "topics": topics,
            "from_letter": q.from_letter,
        }),
    )
}

async fn update_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(book_id) = fv(&form, "book_id").and_then(|s| s.parse::<i64>().ok()) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Missing required fields");
    };
    let Some(new_title) = fv(&form, "title") else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Missing required fields");
    };
    let Some(album) = get_book(db, book_id).await else {
        return err404("Book not found");
    };

    let _ = sqlx::query("UPDATE audiobook_books SET title = ?, normalized_title = ?, updated_at = ? WHERE id = ?")
        .bind(&new_title)
        .bind(new_title.to_lowercase().trim())
        .bind(now_str())
        .bind(book_id)
        .execute(db)
        .await;

    if let Some(new_author) = fv(&form, "author") {
        let new_id = get_or_create_author(db, &new_author).await;
        let old_id = album["author_id"].as_i64().unwrap_or(0);
        if old_id != new_id {
            let _ = sqlx::query("UPDATE audiobook_books SET author_id = ? WHERE id = ?")
                .bind(new_id)
                .bind(book_id)
                .execute(db)
                .await;
            let remaining = sqlx::query("SELECT COUNT(*) AS c FROM audiobook_books WHERE author_id = ?")
                .bind(old_id)
                .fetch_one(db)
                .await
                .map(|r| r.get::<i64,_>("c"))
                .unwrap_or(0);
            if remaining == 0 {
                let _ = sqlx::query("DELETE FROM audiobook_authors WHERE id = ?")
                    .bind(old_id)
                    .execute(db)
                    .await;
            }
        }
    }

    if let Some(cover_data) = form.get("cover_data").filter(|s| s.starts_with("data:image")) {
        let (header, b64) = cover_data.split_once(',').unwrap_or(("", ""));
        let ext0 = header.split('/').nth(1).unwrap_or("").split(';').next().unwrap_or("");
        if !matches!(ext0, "jpeg" | "png" | "gif" | "webp") {
            return err_json(axum::http::StatusCode::BAD_REQUEST, "Invalid image format from clipboard");
        }
        let directory: String = sqlx::query_scalar(
            "SELECT directory_path FROM audiobook_books WHERE id = ?",
        )
        .bind(book_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        if let Ok(data) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
            if let Some(val) = write_book_art(&directory, book_id, &data) {
                if let Some(old) = album["image_filename"].as_str() {
                    if old != ART_SIBLING && old != val {
                        let _ = std::fs::remove_file(art_dir().join(old));
                    }
                }
                let _ = sqlx::query("UPDATE audiobook_books SET image_filename = ? WHERE id = ?")
                    .bind(&val)
                    .bind(book_id)
                    .execute(db)
                    .await;
            }
        }
    }

    axum::Json(json!({"message": "Book metadata updated successfully"})).into_response()
}

async fn move_book_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(target_topic_id) = fv(&form, "topic_id").filter(|s| !s.is_empty()) else {
        return err400("Select a target topic");
    };
    let Some(book) = sqlx::query(
        "SELECT b.directory_path, b.topic_id, a.name AS author_name FROM audiobook_books b \
         LEFT JOIN audiobook_authors a ON a.id = b.author_id WHERE b.id = ?",
    )
    .bind(book_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    else {
        return err404("Book not found");
    };
    let src_dir: String = book.try_get("directory_path").unwrap_or_default();
    let old_topic: Option<String> = book.try_get("topic_id").ok().flatten();
    let author_name: String = book.try_get("author_name").unwrap_or_else(|_| "Unknown Author".into());
    if old_topic.as_deref() == Some(target_topic_id.as_str()) {
        return err400("Book is already in that topic");
    }
    let Some(target) = sqlx::query("SELECT id, directory_path, user_id FROM audiobook_topics WHERE id = ?")
        .bind(&target_topic_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    else {
        return err404("Target topic not found");
    };
    let target_dir: String = target.try_get("directory_path").unwrap_or_default();
    let target_uid: i64 = target.try_get("user_id").unwrap_or(user.id);
    if target_dir.is_empty() || !std::path::Path::new(&target_dir).is_dir() {
        return err400("Target topic directory does not exist on disk");
    }
    if src_dir.is_empty() || !std::path::Path::new(&src_dir).is_dir() {
        return err_json(axum::http::StatusCode::CONFLICT, "Book directory not found on disk; cannot move files");
    }

    // Destination follows the topic/Author/Book layout convention.
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == ' ' || c == '.' || c == '&' || c == '\'' || c == '-' { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let author_dir = if safe(&author_name).is_empty() { "Unknown Author".to_string() } else { safe(&author_name) };
    let book_name = src_dir.trim_end_matches('/').rsplit('/').next().unwrap_or("book").to_string();
    let dest_dir = format!("{target_dir}/{author_dir}/{book_name}");
    if std::path::Path::new(&dest_dir).exists() {
        return err400("A directory with this book name already exists in the target topic");
    }
    if let Some(parent) = std::path::Path::new(&dest_dir).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::rename(&src_dir, &dest_dir) {
        return err_json(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Move failed: {e}"),
        );
    }

    let _ = sqlx::query(
        "UPDATE audiobook_books SET topic_id = ?, user_id = ?, directory_path = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&target_topic_id)
    .bind(target_uid)
    .bind(&dest_dir)
    .bind(now_str())
    .bind(book_id)
    .execute(db)
    .await;
    if let Ok(rows) = sqlx::query("SELECT id, file_path FROM audiobook_chapters WHERE book_id = ?")
        .bind(book_id)
        .fetch_all(db)
        .await
    {
        for r in rows {
            let cid: i64 = r.get("id");
            let fp: String = r.get("file_path");
            if let Some(rest) = fp.strip_prefix(&src_dir) {
                let _ = sqlx::query("UPDATE audiobook_chapters SET file_path = ? WHERE id = ?")
                    .bind(format!("{dest_dir}{rest}"))
                    .bind(cid)
                    .execute(db)
                    .await;
            }
        }
    }
    if let Some(old) = &old_topic {
        refresh_topic_count(db, old).await;
    }
    refresh_topic_count(db, &target_topic_id).await;

    axum::Json(json!({"message": "Book moved", "topic_id": target_topic_id, "directory": dest_dir})).into_response()
}

async fn update_chapters(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let form = form_map(&form);
    let mut ids: Vec<i64> = Vec::new();
    for i in 0..10000 {
        if let Some(v) = form.get(&format!("chapter_id_{i}")) {
            if let Ok(id) = v.parse::<i64>() {
                ids.push(id);
            }
        }
        if form.len() <= i && i > ids.len() {
            break;
        }
    }
    // Also support raw key iteration order (chapter_id_* keys)
    let mut ordered: Vec<(i64, Option<String>)> = Vec::new();
    for k in form.keys() {
        if let Some(idstr) = k.strip_prefix("chapter_id_") {
            if let Ok(id) = idstr.parse::<i64>() {
                let title = form.get(&format!("chapter_{id}")).cloned();
                ordered.push((id, title));
            }
        }
    }
    let _ = ids;
    for (idx, (cid, title)) in ordered.iter().enumerate() {
        if let Some(t) = title {
            let _ = sqlx::query(
                "UPDATE audiobook_chapters SET title = ?, track_number = ? WHERE id = ?",
            )
            .bind(t.trim())
            .bind((idx + 1).to_string())
            .bind(cid)
            .execute(&state.db)
            .await;
        }
    }
    axum::Json(json!({"message": "Chapter titles updated successfully"})).into_response()
}

// ----------------------------------------------------------------- browse

async fn browse_authors(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let page_no = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(50).clamp(50, 200);
    let letter = one_letter(&q.letter);
    let vis = visibility(&user);

    let mut cond = String::new();
    if letter.is_some() {
        cond.push_str(" AND a.name LIKE ?1");
    }
    let sqlc = format!("SELECT COUNT(*) AS c FROM audiobook_authors a WHERE 1=1 {cond}");
    let mut cq = sqlx::query(&sqlc);
    if letter.is_some() {
        cq = cq.bind(format!("{}%", letter.clone().unwrap()));
    }
    let total = cq.fetch_one(db).await.map(|r| r.get::<i64,_>("c")).unwrap_or(0);
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_no - 1) * per_page;

    let sql = format!(
        "SELECT a.* FROM audiobook_authors a WHERE 1=1 {cond} ORDER BY a.name LIMIT ? OFFSET ?"
    );
    let mut aq = sqlx::query(&sql);
    if letter.is_some() {
        aq = aq.bind(format!("{}%", letter.clone().unwrap()));
    }
    let authors = aq.bind(per_page).bind(offset).fetch_all(db).await.unwrap_or_default();

    let mut counts = serde_json::Map::new();
    let super_user = is_super(&user);
    for r in &authors {
        let id = r.get::<i64,_>("id");
        let sqlb = if super_user {
            "SELECT COUNT(*) AS c FROM audiobook_books b WHERE b.author_id = ?".to_string()
        } else {
            format!(
                "SELECT COUNT(*) AS c FROM audiobook_books b WHERE 1=1 {vis} AND b.author_id = ?1"
            )
        };
        let mut cq = sqlx::query(&sqlb);
        if !super_user {
            cq = cq.bind(user.id);
        }
        let c = cq
            .bind(id)
            .fetch_one(db)
            .await
            .map(|r| r.get::<i64,_>("c"))
            .unwrap_or(0);
        counts.insert(id.to_string(), json!(c));
    }
    let authors: Vec<Value> = authors
        .iter()
        .map(author_row)
        .zip(counts.values())
        .map(|(mut a, c)| {
            a["book_count"] = c.clone();
            a
        })
        .collect();

    page(
        &state,
        "audiobook/browse_authors.html",
        json!({
            "user": crate::core::user_json(&user),
            "authors": authors,
            "author_album_counts": Value::Object(counts),
            "current_letter": letter,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
        }),
    )
}

async fn browse_albums(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let page_no = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(24).clamp(24, 120);
    let letter = one_letter(&q.letter);
    let _vis = &visibility(&user);

    // The visibility fragment uses a plain ? for the user id; bind order is
    // letter (if present), user id (admins have no vis fragment), then
    // LIMIT/OFFSET.
    let vis = if is_super(&user) { String::new() } else {
        " AND (b.topic_id IN (SELECT id FROM audiobook_topics WHERE user_id = ?) \
          OR b.topic_id IN (SELECT topic_id FROM audiobook_topic_shares WHERE shared_with_user_id = ?) \
          OR (b.is_public = 1 AND b.is_banned = 0))".to_string()
    };
    let user_bind = vis.contains('?');

    let mut cond = String::new();
    if letter.is_some() {
        cond.push_str(" AND b.title LIKE ?");
    }
    if let Some(aid) = q.author_id {
        cond.push_str(&format!(" AND b.author_id = {aid}"));
    }

    let sqlc = format!("SELECT COUNT(*) AS c FROM audiobook_books b WHERE 1=1 {cond} {vis}");
    let mut cq = sqlx::query(&sqlc);
    if let Some(l) = &letter {
        cq = cq.bind(format!("{l}%"));
    }
    if user_bind {
        cq = cq.bind(user.id).bind(user.id);
    }
    let total = cq.fetch_one(db).await.map(|r| r.get::<i64,_>("c")).unwrap_or(0);
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_no - 1) * per_page;

    let sql = format!(
        "SELECT b.*, a.name AS author_name FROM audiobook_books b \
         JOIN audiobook_authors a ON a.id = b.author_id WHERE 1=1 {cond} {vis} \
         ORDER BY b.title LIMIT ? OFFSET ?"
    );
    let mut aq = sqlx::query(&sql);
    if let Some(l) = &letter {
        aq = aq.bind(format!("{l}%"));
    }
    if user_bind {
        aq = aq.bind(user.id).bind(user.id);
    }
    let albums: Vec<Value> = aq
        .bind(per_page)
        .bind(offset)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let mut b = book_row(r);
            b["author"] = json!({"id": r.get::<i64,_>("author_id"), "name": r.try_get::<String,_>("author_name").unwrap_or_default()});
            b
        })
        .collect();

    let selected_author = match q.author_id {
        Some(aid) => sqlx::query("SELECT * FROM audiobook_authors WHERE id = ?")
            .bind(aid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .map(|r| author_row(&r)),
        None => None,
    };

    page(
        &state,
        "audiobook/browse_albums.html",
        json!({
            "user": crate::core::user_json(&user),
            "albums": albums,
            "playlists": playlists_dropdown(db, user.id).await,
            "current_letter": letter,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "selected_author": selected_author,
            "author_id": q.author_id,
            "per_page": per_page,
        }),
    )
}

async fn view_author(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(author_id): Path<i64>,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let Some(author) = sqlx::query("SELECT * FROM audiobook_authors WHERE id = ?")
        .bind(author_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| author_row(&r))
    else {
        return err404("Artist not found");
    };
    let vis = visibility(&user);
    let page_no = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(24).clamp(24, 120);

    // The visibility fragment binds the user id at ?1, but only for
    // non-super users; number the author filter explicitly and bind
    // conditionally so LIMIT/OFFSET slots never shift.
    let bind_vis = !vis.is_empty();
    let author_cond = if bind_vis {
        "b.author_id = ?2".to_string()
    } else {
        "b.author_id = ?".to_string()
    };

    let count_sql = format!(
        "SELECT COUNT(*) AS c FROM audiobook_books b WHERE {author_cond} {vis}"
    );
    let mut cq = sqlx::query(&count_sql);
    if bind_vis {
        cq = cq.bind(user.id);
    }
    let total = cq
        .bind(author_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64,_>("c"))
        .unwrap_or(0);
    let use_pagination = total > per_page;

    let mut sql = format!(
        "SELECT b.*, a.name AS author_name FROM audiobook_books b \
         JOIN audiobook_authors a ON a.id = b.author_id WHERE {author_cond} {vis} ORDER BY b.title"
    );
    if use_pagination {
        sql.push_str(" LIMIT ? OFFSET ?");
    }
    let list_sql = sql.clone();
    let mut aq = sqlx::query(&list_sql);
    if bind_vis {
        aq = aq.bind(user.id);
    }
    aq = aq.bind(author_id);
    if use_pagination {
        aq = aq.bind(per_page).bind((page_no - 1) * per_page);
    }
    let albums: Vec<Value> = aq
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let mut b = book_row(r);
            b["author"] = json!({"id": r.get::<i64,_>("author_id"), "name": r.try_get::<String,_>("author_name").unwrap_or_default()});
            b
        })
        .collect();

    let mut ctx = json!({
        "user": crate::core::user_json(&user),
        "author": author,
        "albums": albums,
        "playlists": playlists_dropdown(db, user.id).await,
        "total": total,
        "use_pagination": use_pagination,
        "from_letter": q.from_letter,
    });
    if use_pagination {
        ctx["current_page"] = json!(page_no);
        ctx["total_pages"] = json!((total + per_page - 1) / per_page);
        ctx["per_page"] = json!(per_page);
    }
    page(&state, "audiobook/author.html", ctx)
}

async fn browse_songs(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let page_no = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(50).clamp(50, 200);
    let letter = one_letter(&q.letter);
    let _vis = &visibility(&user);

    let vis = if is_super(&user) { String::new() } else {
        " AND (b.topic_id IN (SELECT id FROM audiobook_topics WHERE user_id = ?) \
          OR b.topic_id IN (SELECT topic_id FROM audiobook_topic_shares WHERE shared_with_user_id = ?) \
          OR (b.is_public = 1 AND b.is_banned = 0))".to_string()
    };
    let user_bind = vis.contains('?');

    let mut cond = String::new();
    if letter.is_some() {
        cond.push_str(" AND c.title LIKE ?");
    }
    let sqlc = format!(
        "SELECT COUNT(*) AS c FROM audiobook_chapters c JOIN audiobook_books b ON b.id = c.book_id WHERE 1=1 {cond} {vis}"
    );
    let mut cq = sqlx::query(&sqlc);
    if let Some(l) = &letter {
        cq = cq.bind(format!("{l}%"));
    }
    if user_bind {
        cq = cq.bind(user.id).bind(user.id);
    }
    let total = cq.fetch_one(db).await.map(|r| r.get::<i64,_>("c")).unwrap_or(0);
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_no - 1) * per_page;

    let sql = format!(
        "SELECT c.id, c.title, c.track_number, b.id AS book_id, b.title AS book_title, \
         b.image_filename, a.id AS author_id, a.name AS author_name \
         FROM audiobook_chapters c \
         JOIN audiobook_books b ON b.id = c.book_id \
         JOIN audiobook_authors a ON a.id = b.author_id WHERE 1=1 {cond} {vis} \
         ORDER BY c.title LIMIT ? OFFSET ?"
    );
    let mut sq = sqlx::query(&sql);
    if let Some(l) = &letter {
        sq = sq.bind(format!("{l}%"));
    }
    if user_bind {
        sq = sq.bind(user.id).bind(user.id);
    }
    let mut track_num = offset;
    let tracks: Vec<Value> = sq
        .bind(per_page)
        .bind(offset)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            track_num += 1;
            json!({
                "id": r.get::<i64,_>("id"),
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "trackNumber": r.try_get::<Option<String>,_>("track_number").ok().flatten()
                    .unwrap_or_else(|| track_num.to_string()),
                "albumId": r.get::<i64,_>("book_id"),
                "albumTitle": r.try_get::<String,_>("book_title").unwrap_or_default(),
                "albumImage": r.try_get::<Option<String>,_>("image_filename").ok().flatten(),
                "artistId": r.get::<i64,_>("author_id"),
                "artistName": r.try_get::<String,_>("author_name").unwrap_or_default(),
            })
        })
        .collect();

    page(
        &state,
        "audiobook/browse_songs.html",
        json!({
            "user": crate::core::user_json(&user),
            "tracks": tracks,
            "playlists": playlists_dropdown(db, user.id).await,
            "current_letter": letter,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
        }),
    )
}

// -------------------------------------------------------------- playlists

async fn list_playlists(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let playlists = playlists_dropdown(&state.db, user.id).await;
    page(
        &state,
        "audiobook/playlists.html",
        json!({"playlists": playlists}),
    )
}

async fn api_list_playlists(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    axum::Json(playlists_dropdown(&state.db, user.id).await).into_response()
}

async fn import_playlist_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let playlists = playlists_dropdown(&state.db, user.id).await;
    page(
        &state,
        "audiobook/import_playlist.html",
        json!({"playlists": playlists}),
    )
}

/// Parse one import line: "Author - Book" (whole book) or
/// "Author - Book - Chapter" (single chapter).
async fn ab_import_find_book(
    db: &syarc_plugin_api::Db,
    author_name: &str,
    book_title: &str,
) -> Option<i64> {
    let author = sqlx::query("SELECT id FROM audiobook_authors WHERE name = ? COLLATE NOCASE")
        .bind(author_name)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;
    let author_id: i64 = author.get("id");
    let like = format!("%{}%", book_title);
    let row = sqlx::query(
        "SELECT id FROM audiobook_books WHERE author_id = ? AND (title LIKE ? COLLATE NOCASE OR normalized_title LIKE ? COLLATE NOCASE)",
    )
    .bind(author_id)
    .bind(&like)
    .bind(&like)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()?;
    Some(row.get("id"))
}

async fn process_playlist_import(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let mut playlist_name = fv(&form, "playlist_name").unwrap_or_default();
    let create_new = form.get("create_new").map(|v| v == "true").unwrap_or(false);
    let new_playlist_name = fv(&form, "new_playlist_name").unwrap_or_default();
    let import_text = form.get("import_text").cloned().unwrap_or_default();

    if create_new {
        if new_playlist_name.is_empty() {
            return axum::Json(json!({"success": false, "message": "New playlist name is required"})).into_response();
        }
        let exists = sqlx::query("SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?")
            .bind(&new_playlist_name)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
        if exists.is_some() {
            return axum::Json(json!({"success": false, "message": format!("Playlist '{new_playlist_name}' already exists")})).into_response();
        }
        playlist_name = new_playlist_name;
        let _ = sqlx::query("INSERT INTO audiobook_playlists (name, user_id, created_at) VALUES (?,?,?)")
            .bind(&playlist_name)
            .bind(user.id)
            .bind(now_str())
            .execute(db)
            .await;
    } else {
        if playlist_name.is_empty() {
            return axum::Json(json!({"success": false, "message": "Please select an existing playlist or create a new one"})).into_response();
        }
        let exists = sqlx::query("SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?")
            .bind(&playlist_name)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
        if exists.is_none() {
            return axum::Json(json!({"success": false, "message": "Playlist not found"})).into_response();
        }
    }

    let norm = crate::music::normalize_unicode;
    let mut lines: Vec<(Option<String>, String, Option<String>)> = Vec::new();
    for line in import_text.lines() {
        let line = norm(line.trim());
        if line.is_empty() {
            continue;
        }
        let sep = [" - "].iter().find(|s| line.contains(*s)).copied();
        if let Some(sep) = sep {
            let parts: Vec<&str> = line.split(sep).map(|p| p.trim()).collect();
            if parts.len() >= 2 {
                lines.push((
                    Some(norm(parts[0])),
                    norm(parts[1]),
                    parts.get(2).map(|p| norm(p)),
                ));
                continue;
            }
        }
        lines.push((None, norm(&line), None));
    }
    if lines.is_empty() {
        return axum::Json(json!({"success": false, "message": "No valid items found in import text"})).into_response();
    }

    let mut found = Vec::new();
    let mut not_found = Vec::new();

    for (author, book, chapter) in &lines {
        let book_id = match author {
            Some(a) => ab_import_find_book(db, a, book).await,
            None => {
                let like = format!("%{}%", book);
                sqlx::query("SELECT id FROM audiobook_books WHERE title LIKE ? COLLATE NOCASE LIMIT 1")
                    .bind(&like)
                    .fetch_optional(db)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.get::<i64, _>("id"))
            }
        };
        let Some(book_id) = book_id else {
            not_found.push(format!(
                "{} - {}{}",
                author.as_deref().unwrap_or("?"),
                book,
                chapter.as_deref().map(|c| format!(" - {c}")).unwrap_or_default()
            ));
            continue;
        };

        if let Some(chapter_title) = chapter {
            let like = format!("%{chapter_title}%");
            let row = sqlx::query(
                "SELECT id FROM audiobook_chapters WHERE book_id = ? AND title LIKE ? COLLATE NOCASE LIMIT 1",
            )
            .bind(book_id)
            .bind(&like)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
            match row {
                Some(r) => {
                    let cid: i64 = r.get("id");
                    let dupe = sqlx::query(
                        "SELECT 1 FROM audiobook_playlist_items WHERE playlist_name = ? AND user_id = ? AND item_type = 'song' AND item_id = ?",
                    )
                    .bind(&playlist_name)
                    .bind(user.id)
                    .bind(cid)
                    .fetch_optional(db)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                    let label = format!("{} - {} - {}", author.as_deref().unwrap_or("?"), book, chapter_title);
                    if !dupe {
                        let _ = sqlx::query(
                            "INSERT INTO audiobook_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)",
                        )
                        .bind(&playlist_name)
                        .bind(user.id)
                        .bind("song")
                        .bind(cid)
                        .execute(db)
                        .await;
                    }
                    found.push(if dupe { format!("{label} (already in playlist)") } else { label });
                }
                None => not_found.push(format!("{} - {} - {}", author.as_deref().unwrap_or("?"), book, chapter_title)),
            }
        } else {
            let dupe = sqlx::query(
                "SELECT 1 FROM audiobook_playlist_items WHERE playlist_name = ? AND user_id = ? AND item_type = 'album' AND item_id = ?",
            )
            .bind(&playlist_name)
            .bind(user.id)
            .bind(book_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .is_some();
            let label = format!("{} - {}", author.as_deref().unwrap_or("?"), book);
            if !dupe {
                let _ = sqlx::query(
                    "INSERT INTO audiobook_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)",
                )
                .bind(&playlist_name)
                .bind(user.id)
                .bind("album")
                .bind(book_id)
                .execute(db)
                .await;
            }
            found.push(if dupe { format!("{label} (already in playlist)") } else { label });
        }
    }

    axum::Json(json!({
        "success": true,
        "message": format!("Import complete: {} added, {} not found", found.len(), not_found.len()),
        "found_count": found.len(),
        "not_found_count": not_found.len(),
        "found": found,
        "not_found": not_found,
        "playlist_name": playlist_name,
    }))
    .into_response()
}

async fn view_playlist_details(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query(
        "SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?",
    )
    .bind(&playlist_name)
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some();
    if !exists {
        return err404("Playlist not found");
    }

    let items_raw: Vec<(i64, String, i64)> = sqlx::query(
        "SELECT id, item_type, item_id FROM audiobook_playlist_items \
         WHERE playlist_name = ? AND user_id = ? ORDER BY id ASC",
    )
    .bind(&playlist_name)
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<i64,_>("id"), r.get::<String,_>("item_type"), r.get::<i64,_>("item_id")))
    .collect();

    let mut items: Vec<Value> = Vec::new();
    for (item_row_id, item_type, item_id) in items_raw {
        if item_type == "album" {
            if let Some(album) = get_book(db, item_id).await {
                items.push(json!({
                    "type": "album", "id": item_row_id, "item_id": item_id,
                    "title": album["title"], "artist": author_name(db, album["author_id"].as_i64().unwrap_or(0)).await,
                    "image": album["image_filename"],
                }));
            }
        } else if item_type == "artist" {
            if let Some(r) = sqlx::query("SELECT name FROM audiobook_authors WHERE id = ?")
                .bind(item_id)
                .fetch_optional(db)
                .await
                .ok()
                .flatten()
            {
                items.push(json!({
                    "type": "artist", "id": item_row_id, "item_id": item_id,
                    "title": r.try_get::<String,_>("name").unwrap_or_default(),
                    "artist": "Artist", "image": Value::Null,
                }));
            }
        } else if let Some(r) = sqlx::query(
            "SELECT c.title, b.id AS book_id, b.title AS book_title, b.image_filename, b.author_id \
             FROM audiobook_chapters c JOIN audiobook_books b ON b.id = c.book_id WHERE c.id = ?",
        )
        .bind(item_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        {
            items.push(json!({
                "type": "song", "id": item_row_id, "item_id": item_id,
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "artist": author_name(db, r.get::<i64,_>("author_id")).await,
                "album": r.try_get::<String,_>("book_title").unwrap_or_default(),
                "image": r.try_get::<Option<String>,_>("image_filename").ok().flatten(),
            }));
        }
    }

    page(
        &state,
        "audiobook/playlist_details.html",
        json!({"playlist_name": playlist_name, "items": items}),
    )
}

async fn create_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    let form = form_map(&form);
    let Some(name) = fv(&form, "name") else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Playlist name is required");
    };
    let book_title = fv(&form, "book_title").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let exists = sqlx::query("SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?")
        .bind(&name)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .is_some();
    if exists {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Playlist with this name already exists");
    }
    let _ = sqlx::query(
        "INSERT INTO audiobook_playlists (name, user_id, created_at) VALUES (?,?,?)",
    )
    .bind(&name)
    .bind(user.id)
    .bind(now_str())
    .execute(&state.db)
    .await;

    if let Some(title) = book_title {
        let like = format!("%{title}%");
        let book = sqlx::query(
            "SELECT id FROM audiobook_books WHERE title LIKE ? COLLATE NOCASE OR normalized_title LIKE ? COLLATE NOCASE LIMIT 1",
        )
        .bind(&like)
        .bind(&like)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<i64, _>("id"));
        match book {
            Some(id) => {
                let _ = sqlx::query(
                    "INSERT INTO audiobook_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)",
                )
                .bind(&name)
                .bind(user.id)
                .bind("album")
                .bind(id)
                .execute(&state.db)
                .await;
            }
            None => {
                let _ = sqlx::query("DELETE FROM audiobook_playlists WHERE name = ? AND user_id = ?")
                    .bind(&name)
                    .bind(user.id)
                    .execute(&state.db)
                    .await;
                return err_json(
                    axum::http::StatusCode::BAD_REQUEST,
                    &format!("No book found matching '{title}'"),
                );
            }
        }
    }
    axum::Json(json!({"success": true, "message": format!("Playlist '{name}' created"), "playlist_name": name}))
        .into_response()
}

async fn random_add_books(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&form);
    let count = fv(&form, "count").and_then(|s| s.parse::<i64>().ok()).unwrap_or(10).clamp(1, 100);
    let exists = sqlx::query("SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Playlist not found");
    }
    let books: Vec<i64> = sqlx::query(
        "SELECT b.id FROM audiobook_books b \
         WHERE b.id NOT IN (SELECT item_id FROM audiobook_playlist_items \
                            WHERE playlist_name = ? AND user_id = ? AND item_type = 'album') \
         ORDER BY RANDOM() LIMIT ?",
    )
    .bind(&playlist_name)
    .bind(user.id)
    .bind(count)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| r.get::<i64, _>("id"))
    .collect();
    for id in &books {
        let _ = sqlx::query(
            "INSERT INTO audiobook_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)",
        )
        .bind(&playlist_name)
        .bind(user.id)
        .bind("album")
        .bind(id)
        .execute(db)
        .await;
    }
    axum::Json(json!({
        "success": true,
        "message": format!("Added {} random book{} to '{playlist_name}'", books.len(), if books.len() == 1 { "" } else { "s" }),
    }))
    .into_response()
}

async fn analyze_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query("SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Playlist not found");
    }
    let items: Vec<(String, i64)> = sqlx::query(
        "SELECT item_type, item_id FROM audiobook_playlist_items WHERE playlist_name = ? AND user_id = ?",
    )
    .bind(&playlist_name)
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<String, _>("item_type"), r.get::<i64, _>("item_id")))
    .collect();
    let mut total_tracks = 0usize;
    let mut total_files = 0usize;
    let mut total_size = 0.0f64;
    for (item_type, item_id) in &items {
        if item_type == "album" {
            if let Ok(row) = sqlx::query(
                "SELECT COUNT(*) AS n, COUNT(DISTINCT file_path) AS f, COALESCE(SUM(size), 0.0) AS s \
                 FROM audiobook_chapters WHERE book_id = ?",
            )
            .bind(item_id)
            .fetch_one(db)
            .await
            {
                total_tracks += row.try_get::<i64, _>("n").unwrap_or(0) as usize;
                total_files += row.try_get::<i64, _>("f").unwrap_or(0) as usize;
                total_size += row.try_get::<f64, _>("s").unwrap_or(0.0);
            }
        } else {
            if let Some(row) = sqlx::query("SELECT size FROM audiobook_chapters WHERE id = ?")
                .bind(item_id)
                .fetch_optional(db)
                .await
                .ok()
                .flatten()
            {
                total_tracks += 1;
                total_files += 1;
                total_size += row.try_get::<f64, _>("size").unwrap_or(0.0);
            }
        }
    }
    axum::Json(json!({
        "success": true,
        "total_tracks": total_tracks,
        "total_files": total_files,
        "total_size_mb": total_size,
    }))
    .into_response()
}

async fn add_to_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    let form = form_map(&form);
    let item_type = form.get("type").cloned().unwrap_or_else(|| "song".into());
    let item_id = fv(&form, "item_id").and_then(|s| s.parse::<i64>().ok());

    let exists = sqlx::query("SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .is_some();
    if !exists {
        return err404("Playlist not found");
    }
    if let Some(id) = item_id {
        let dup = sqlx::query(
            "SELECT 1 FROM audiobook_playlist_items WHERE playlist_name = ? AND user_id = ? AND item_type = ? AND item_id = ?",
        )
        .bind(&playlist_name)
        .bind(user.id)
        .bind(&item_type)
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .is_some();
        if dup {
            return axum::Json(json!({
                "success": true,
                "message": format!("Already in playlist '{playlist_name}'"),
                "playlist_name": playlist_name,
            }))
            .into_response();
        }
        let _ = sqlx::query(
            "INSERT INTO audiobook_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)",
        )
        .bind(&playlist_name)
        .bind(user.id)
        .bind(&item_type)
        .bind(id)
        .execute(&state.db)
        .await;
    }
    axum::Json(json!({
        "success": true,
        "message": format!("Added to playlist '{playlist_name}'"),
        "playlist_name": playlist_name,
    }))
    .into_response()
}

async fn delete_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
) -> Response {
    let _ = sqlx::query("DELETE FROM audiobook_playlist_items WHERE playlist_name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("DELETE FROM audiobook_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .execute(&state.db)
        .await;
    redirect("/library/audiobook/playlists")
}

async fn delete_playlist_item(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((playlist_name, item_id)): Path<(String, i64)>,
) -> Response {
    let _ = sqlx::query(
        "DELETE FROM audiobook_playlist_items WHERE id = ? AND playlist_name = ? AND user_id = ?",
    )
    .bind(item_id)
    .bind(&playlist_name)
    .bind(user.id)
    .execute(&state.db)
    .await;
    redirect(&format!("/library/audiobook/playlists/{playlist_name}"))
}

// ------------------------------------------------------------------ admin

async fn export_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query("SELECT 1 FROM audiobook_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Playlist not found");
    }
    let items: Vec<(String, i64)> = sqlx::query(
        "SELECT item_type, item_id FROM audiobook_playlist_items WHERE playlist_name = ? AND user_id = ? ORDER BY id",
    )
    .bind(&playlist_name)
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<String, _>("item_type"), r.get::<i64, _>("item_id")))
    .collect();

    let mut chapter_ids: Vec<String> = Vec::new();
    for (item_type, item_id) in &items {
        match item_type.as_str() {
            "album" => {
                let ids = sqlx::query("SELECT id FROM audiobook_chapters WHERE book_id = ? ORDER BY track_number, id")
                    .bind(item_id)
                    .fetch_all(db)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(|r| r.get::<i64, _>("id").to_string())
                    .collect::<Vec<_>>();
                chapter_ids.extend(ids);
            }
            "artist" => {
                let ids = sqlx::query(
                    "SELECT c.id FROM audiobook_chapters c JOIN audiobook_books b ON b.id = c.book_id \
                     WHERE b.author_id = ? ORDER BY b.title, c.track_number, c.id",
                )
                .bind(item_id)
                .fetch_all(db)
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| r.get::<i64, _>("id").to_string())
                .collect::<Vec<_>>();
                chapter_ids.extend(ids);
            }
            _ => chapter_ids.push(item_id.to_string()),
        }
    }
    if chapter_ids.is_empty() {
        return err400("No chapters found in playlist");
    }
    let ph = chapter_ids.join(",");
    let rows = sqlx::query(&format!(
        "SELECT a.name AS author, b.title AS book, c.title AS chapter, c.file_path \
         FROM audiobook_chapters c JOIN audiobook_books b ON b.id = c.book_id \
         JOIN audiobook_authors a ON a.id = b.author_id WHERE c.id IN ({ph})"
    ))
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let mut tar: Vec<u8> = Vec::new();
    for r in &rows {
        let author = r.try_get::<String, _>("author").unwrap_or_default();
        let book = r.try_get::<String, _>("book").unwrap_or_default();
        let file_path = r.try_get::<String, _>("file_path").unwrap_or_default();
        let Ok(data) = tokio::fs::read(&file_path).await else {
            continue;
        };
        let base = file_path.rsplit('/').next().unwrap_or("chapter");
        let entry = format!("{playlist_name}/{author}/{book}/{base}");
        crate::music::tar_append(&mut tar, &entry, &data);
    }
    tar.extend(std::iter::repeat_n(0u8, 1024));

    (
        [(axum::http::header::CONTENT_TYPE, "application/x-tar".to_string()),
         (axum::http::header::CONTENT_DISPOSITION,
          format!("attachment; filename=\"{playlist_name}.tar\""))],
        tar,
    )
        .into_response()
}

async fn add_books_manual(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let folder_path = fv(&form, "file_path").unwrap_or_default();
    if folder_path.is_empty() {
        return err400("Folder path is required");
    }
    let root = crate::root("audiobook");
    let folder = folder_path.trim_end_matches('/').to_string();
    if !folder.starts_with(&root) || !std::path::Path::new(&folder).is_dir() {
        return err400(&format!("Directory not found under the audiobook root: {folder}"));
    }
    let db = state.db.clone();
    let before = sqlx::query("SELECT COUNT(*) AS c FROM audiobook_books")
        .fetch_one(&db)
        .await
        .map(|r| r.get::<i64, _>("c"))
        .unwrap_or(0);
    crate::scanner::scan_audiobooks_scope(&db, &folder).await;
    let after = sqlx::query("SELECT COUNT(*) AS c FROM audiobook_books")
        .fetch_one(&db)
        .await
        .map(|r| r.get::<i64, _>("c"))
        .unwrap_or(0);
    let added = (after - before).max(0);
    axum::Json(json!({
        "success": true,
        "message": if added > 0 {
            format!("Added {added} new book(s) from {}", folder.rsplit('/').next().unwrap_or(&folder))
        } else {
            "No new books found (already in the library)".to_string()
        },
    }))
    .into_response()
}

async fn admin_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let all_topics: Vec<Value> = sqlx::query(
        "SELECT t.*, u.username AS owner_username, \
         (SELECT COUNT(*) FROM audiobook_books b WHERE b.topic_id = t.id) AS book_count \
         FROM audiobook_topics t JOIN users u ON u.id = t.user_id ORDER BY t.name",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "topic": topic_row(r),
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
            "book_count": r.get::<i64,_>("book_count"),
        })
    })
    .collect();
    let public_topics: Vec<Value> = all_topics
        .iter()
        .filter(|t| t["topic"]["is_public"] == json!(true))
        .cloned()
        .collect();

    let banned_books: Vec<Value> = sqlx::query(
        "SELECT b.*, t.name AS topic_name, u.username AS owner_username \
         FROM audiobook_books b \
         LEFT JOIN audiobook_topics t ON t.id = b.topic_id \
         LEFT JOIN users u ON u.id = b.user_id \
         WHERE b.is_banned = 1 ORDER BY b.title",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "book": book_row(r),
            "topic": r.try_get::<Option<String>,_>("topic_name").ok().flatten().map(|n| json!({"name": n})),
            "owner_username": r.try_get::<Option<String>,_>("owner_username").ok().flatten(),
        })
    })
    .collect();

    page(
        &state,
        "audiobook/admin.html",
        json!({
            "user": crate::core::user_json(&user),
            "all_topics": all_topics,
            "public_topics": public_topics,
            "banned_books": banned_books,
        }),
    )
}

async fn admin_toggle_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    let owner_username = sqlx::query("SELECT username FROM users WHERE id = ?")
        .bind(topic["user_id"].as_i64().unwrap_or(0))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("username").ok())
        .unwrap_or_else(|| user.username.clone());
    if topic["is_public"] == json!(true) {
        make_topic_private(db, &topic_id).await;
        axum::Json(json!({"message": "Topic is now private.", "is_public": false})).into_response()
    } else {
        let dir = topic["directory_path"].as_str().unwrap_or_default().to_string();
        let name = topic["name"].as_str().unwrap_or_default().to_string();
        make_topic_public(db, &topic_id, &owner_username, &name, &dir).await;
        axum::Json(json!({"message": "Topic is now public.", "is_public": true})).into_response()
    }
}

async fn toggle_book_public(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    let Some(book) = get_book(&state.db, book_id).await else {
        return err404("Book not found");
    };
    let new_val = book["is_public"] != json!(true);
    let _ = sqlx::query("UPDATE audiobook_books SET is_public = ? WHERE id = ?")
        .bind(new_val as i64)
        .bind(book_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({
        "message": format!("Book is now {}.", if new_val {"public"} else {"private"}),
        "is_public": new_val,
    }))
    .into_response()
}

async fn ban_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE audiobook_books SET is_banned = 1, is_public = 0 WHERE id = ?")
        .bind(book_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Book banned from public view."})).into_response()
}

async fn unban_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE audiobook_books SET is_banned = 0 WHERE id = ?")
        .bind(book_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Book unbanned."})).into_response()
}

// ------------------------------------------------------------ scan/static

async fn trigger_scan(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scanner::scan_audiobooks(&db, None, false).await;
    });
    axum::Json(json!({"message": "Audiobook scan started."})).into_response()
}

async fn trigger_clean(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scanner::clean_audiobook_database(&db).await;
    });
    axum::Json(json!({"message": "Audiobook database clean started in background."})).into_response()
}

async fn force_scan(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scanner::scan_audiobooks(&db, None, true).await;
    });
    axum::Json(json!({"message": "Audiobook force scan started - tables truncated, rebuilding."}))
        .into_response()
}

async fn static_art(Path(file): Path<String>) -> Response {
    let path = static_dir("audiobook").join("art").join(file);
    serve_file(&path.to_string_lossy(), None).await
}

/// Serve a book's art: cover.jpg in the book's own directory first, then
/// the legacy central art store for pre-migration images.
async fn book_art(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    let row = sqlx::query(
        "SELECT directory_path, image_filename FROM audiobook_books WHERE id = ?",
    )
    .bind(book_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(row) = row else { return err404("Book not found") };
    let dir: String = row.try_get::<String,_>("directory_path").unwrap_or_default();
    if !dir.is_empty() {
        let p = std::path::Path::new(&dir).join("cover.jpg");
        if p.is_file() {
            return serve_file(&p.to_string_lossy(), None).await;
        }
    }
    if let Some(f) = row
        .try_get::<Option<String>,_>("image_filename")
        .ok()
        .flatten()
        .filter(|f| f != ART_SIBLING && !f.is_empty())
    {
        let p = art_dir().join(f);
        if p.is_file() {
            return serve_file(&p.to_string_lossy(), None).await;
        }
    }
    err404("no art")
}
