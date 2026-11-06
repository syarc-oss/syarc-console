//! Music sub-app: artists/albums/songs browsing, streaming player,
//! playlists, admin metadata/art management, and the directory scanner.

use axum::extract::{Path, Query, RawForm, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;

use crate::{
    err_json, form_map, fv, human_size, is_super, now_str, page, serve_file, static_dir,
    RequireUser, SharedState,
};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/library/music", get(index))
        .route("/library/music/", get(index))
        .route("/library/music/scan", get(trigger_scan))
        .route("/library/music/clean", get(trigger_clean))
        .route("/library/music/scan/force", get(force_scan))
        .route("/library/music/scan/my-library", post(scan_my_library))
        .route("/library/music/browse/artists", get(browse_artists))
        .route("/library/music/browse/albums", get(browse_albums))
        .route("/library/music/browse/songs", get(browse_songs))
        .route("/library/music/author/{author_id}", get(view_artist))
        .route("/library/music/album/{album_id}", get(view_album))
        .route("/library/music/album/{album_id}/toggle-public", post(toggle_album_public))
        .route("/library/music/album/{album_id}/ban", post(ban_album))
        .route("/library/music/album/{album_id}/unban", post(unban_album))
        .route("/library/music/player", get(player))
        .route("/library/music/search", get(search))
        .route("/library/music/stream/{track_id}", get(stream_track))
        .route("/library/music/playlists", get(list_playlists))
        .route("/library/music/api/playlists", get(api_list_playlists))
        .route("/library/music/playlists/import", get(import_playlist_page))
        .route("/library/music/playlists/create", post(create_playlist))
        .route("/library/music/playlists/random/add", post(random_add_to_playlist))
        .route("/library/music/playlists/{playlist_name}", get(view_playlist_details))
        .route("/library/music/playlists/{playlist_name}/add", post(add_to_playlist))
        .route("/library/music/playlists/{playlist_name}/delete", post(delete_playlist))
        .route("/library/music/playlists/{playlist_name}/item/{item_id}/delete", post(delete_playlist_item))
        .route("/library/music/playlists/{playlist_name}/analyze", get(analyze_playlist))
        .route("/library/music/playlists/{playlist_name}/export", get(export_playlist))
        .route("/library/music/playlists/import/process", post(process_playlist_import))
        .route("/library/music/admin", get(music_admin))
        .route("/library/music/admin/album-info", get(get_album_info))
        .route("/library/music/admin/update-metadata", post(update_album_metadata))
        .route("/library/music/admin/update-art", post(update_album_art))
        .route("/library/music/admin/upload-art", post(upload_album_art))
        .route("/library/music/admin/album/delete", post(delete_album))
        .route("/library/music/add", post(add_album_manual))
        .route("/library/music/art/{album_id}", get(album_art))
        .route("/library/music/admin/rebuild-art", post(admin_rebuild_art))
        .route("/library/music/admin/albums/all-public", post(admin_all_albums_public))
        .route("/library/music/admin/albums/all-private", post(admin_all_albums_private))
        .route("/library/music_static/art/{file}", get(static_art))
}

fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}
fn err400(msg: &str) -> Response {
    err_json(axum::http::StatusCode::BAD_REQUEST, msg)
}

async fn album_json(db: &syarc_plugin_api::Db, id: i64) -> Option<Value> {
    sqlx::query("SELECT * FROM music_albums WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "author_id": r.get::<i64,_>("author_id"),
                "media_type": r.try_get::<String,_>("media_type").unwrap_or_default(),
                "image_filename": r.try_get::<String,_>("image_filename").ok(),
                "updated_at": r.try_get::<String,_>("updated_at").ok(),
                "user_id": r.try_get::<i64,_>("user_id").ok(),
                "is_public": r.get::<i64,_>("is_public") == 1,
                "is_banned": r.get::<i64,_>("is_banned") == 1,
                "directory_path": r.try_get::<String,_>("directory_path").ok(),
            })
        })
}

async fn author_name(db: &syarc_plugin_api::Db, id: i64) -> String {
    sqlx::query("SELECT name FROM music_authors WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("name").ok())
        .unwrap_or_else(|| "Unknown".into())
}

async fn playlists_dropdown(db: &syarc_plugin_api::Db, user_id: i64) -> Vec<Value> {
    sqlx::query(
        "SELECT name, created_at FROM music_playlists WHERE user_id = ? \
         GROUP BY name ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "name": r.try_get::<String,_>("name").unwrap_or_default(),
            "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
        })
    })
    .collect()
}

// ------------------------------------------------------------------- pages

async fn index(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let q = |sql: &'static str| sqlx::query(sql).fetch_one(db);
    let public_album_count: i64 = q("SELECT COUNT(*) FROM music_albums WHERE is_public = 1 AND is_banned = 0 AND media_type = 'music'").await.unwrap().get(0);
    let public_track_count: i64 = q("SELECT COUNT(*) FROM music_tracks t JOIN music_albums a ON a.id = t.album_id WHERE a.is_public = 1 AND a.is_banned = 0 AND a.media_type = 'music'").await.unwrap().get(0);
    let artist_count: i64 = q("SELECT COUNT(*) FROM music_authors").await.unwrap().get(0);

    page(
        &state,
        "music/index.html",
        json!({
            "user": {"id": user.id, "username": user.username, "is_superuser": is_super(&user)},
            "full_access": is_super(&user),
            "write_access": is_super(&user),
            "public_album_count": public_album_count,
            "public_track_count": public_track_count,
            "artist_count": artist_count,
        }),
    )
}

type Q = HashMap<String, String>;
fn qi(q: &Q, k: &str, default: i64) -> i64 {
    q.get(k).and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn qs(q: &Q, k: &str) -> String {
    q.get(k).cloned().unwrap_or_default()
}

async fn browse_artists(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let letter = qs(&q, "letter");
    let page_n = qi(&q, "page", 1);
    let per_page = qi(&q, "per_page", 50).clamp(50, 200);

    let like = format!("{}%", letter.to_uppercase());
    let is_letter = letter.chars().count() == 1
        && letter.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false);
    let (where_clause, bind_letter) = if is_letter {
        ("WHERE name LIKE ? COLLATE NOCASE", true)
    } else {
        ("", false)
    };

    let total: i64 = {
        let sql = format!("SELECT COUNT(*) FROM music_authors {where_clause}");
        let mut rq = sqlx::query(&sql);
        if bind_letter {
            rq = rq.bind(&like);
        }
        rq.fetch_one(db).await.unwrap().get(0)
    };
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_n - 1) * per_page;

    let sql = format!(
        "SELECT * FROM music_authors {where_clause} ORDER BY name LIMIT ? OFFSET ?"
    );
    let mut rq = sqlx::query(&sql);
    if bind_letter {
        rq = rq.bind(&like);
    }
    let rows = rq
        .bind(per_page)
        .bind(offset)
        .fetch_all(db)
        .await
        .unwrap_or_default();

    let mut authors = Vec::new();
    let mut counts = serde_json::Map::new();
    for r in &rows {
        let id: i64 = r.get("id");
        let cnt: i64 = sqlx::query(
            "SELECT COUNT(*) FROM music_albums WHERE author_id = ? AND media_type = 'music'",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
        .get(0);
        counts.insert(id.to_string(), json!(cnt));
        authors.push(json!({
            "id": id,
            "name": r.try_get::<String,_>("name").unwrap_or_default(),
            "album_count": cnt,
        }));
    }

    page(
        &state,
        "music/browse_artists.html",
        json!({
            "user": {"username": user.username},
            "authors": authors,
            "author_album_counts": Value::Object(counts),
            "current_letter": letter,
            "current_page": page_n,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
            "write_access": is_super(&user),
        }),
    )
}

async fn browse_albums(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let letter = qs(&q, "letter");
    let author_id = q.get("author_id").and_then(|v| v.parse::<i64>().ok());
    let my_library = q.get("my_library").map(|v| v == "true" || v == "1").unwrap_or(false);
    let page_n = qi(&q, "page", 1);
    let per_page = qi(&q, "per_page", 24).clamp(24, 120);

    let mut wheres: Vec<String> = vec!["a.media_type = 'music'".into()];
    if my_library && is_super(&user) {
        wheres.push(
            "(a.user_id = ?1 OR a.id IN (SELECT album_id FROM music_album_shares WHERE shared_with_user_id = ?1))"
                .into(),
        );
    }
    if letter.chars().count() == 1 && letter.chars().next().unwrap().is_ascii_alphabetic() {
        wheres.push(format!("a.title LIKE '{}%'", letter.to_uppercase()));
    }
    if let Some(aid) = author_id {
        wheres.push(format!("a.author_id = {aid}"));
    }
    let where_clause = wheres.join(" AND ");

    let total: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) FROM music_albums a WHERE {where_clause}"
    ))
    .fetch_one(db)
    .await
    .unwrap()
    .get(0);

    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_n - 1) * per_page;

    // Bind the user id only when the my_library filter contributed its ?1
    // placeholder; binding it unconditionally shifts LIMIT/OFFSET by one.
    let bind_user = my_library && is_super(&user);
    let sql = format!(
        "SELECT a.*, u2.name AS author_name FROM music_albums a \
         JOIN music_authors u2 ON u2.id = a.author_id WHERE {where_clause} \
         ORDER BY a.title LIMIT ? OFFSET ?"
    );
    let mut rq = sqlx::query(&sql);
    if bind_user {
        rq = rq.bind(user.id);
    }
    let rows = rq
        .bind(per_page)
        .bind(offset)
        .fetch_all(db)
        .await
        .unwrap_or_default();

    let albums: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "author_id": r.get::<i64,_>("author_id"),
                "author_name": r.try_get::<String,_>("author_name").unwrap_or_default(),
                "image_filename": r.try_get::<String,_>("image_filename").ok(),
                "is_public": r.get::<i64,_>("is_public") == 1,
            })
        })
        .collect();

    let selected_author = match author_id {
        Some(aid) => json!({"id": aid, "name": author_name(db, aid).await}),
        None => Value::Null,
    };

    page(
        &state,
        "music/browse_albums.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "albums": albums,
            "playlists": playlists_dropdown(db, user.id).await,
            "current_letter": letter,
            "current_page": page_n,
            "total_pages": total_pages,
            "total": total,
            "selected_author": selected_author,
            "author_id": author_id,
            "per_page": per_page,
            "write_access": is_super(&user),
            "my_library": my_library,
        }),
    )
}

async fn view_artist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(author_id): Path<i64>,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let author = sqlx::query("SELECT * FROM music_authors WHERE id = ?")
        .bind(author_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(author) = author else {
        return err404("Artist not found");
    };

    let from_letter = qs(&q, "from_letter");
    let page_n = qi(&q, "page", 1);
    let per_page = qi(&q, "per_page", 24).clamp(24, 120);

    let total: i64 = sqlx::query(
        "SELECT COUNT(*) FROM music_albums WHERE author_id = ? AND media_type = 'music'",
    )
    .bind(author_id)
    .fetch_one(db)
    .await
    .unwrap()
    .get(0);
    let use_pagination = total > per_page;
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_n - 1) * per_page;

    let sql = if use_pagination {
        format!("SELECT * FROM music_albums WHERE author_id = {author_id} AND media_type = 'music' ORDER BY title LIMIT {per_page} OFFSET {offset}")
    } else {
        format!("SELECT * FROM music_albums WHERE author_id = {author_id} AND media_type = 'music' ORDER BY title")
    };
    let rows = sqlx::query(&sql).fetch_all(db).await.unwrap_or_default();
    let albums: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "image_filename": r.try_get::<String,_>("image_filename").ok(),
                "is_public": r.get::<i64,_>("is_public") == 1,
            })
        })
        .collect();

    page(
        &state,
        "music/author.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "author": {"id": author.get::<i64,_>("id"), "name": author.try_get::<String,_>("name").unwrap_or_default()},
            "albums": albums,
            "playlists": playlists_dropdown(db, user.id).await,
            "total": total,
            "current_page": page_n,
            "total_pages": total_pages,
            "use_pagination": use_pagination,
            "per_page": per_page,
            "write_access": is_super(&user),
            "from_letter": from_letter,
        }),
    )
}

async fn view_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<i64>,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let Some(album) = album_json(db, album_id).await else {
        return err404("Album not found");
    };
    let author = json!({
        "id": album["author_id"],
        "name": author_name(db, album["author_id"].as_i64().unwrap()).await,
    });
    let rows = sqlx::query(
        "SELECT * FROM music_tracks WHERE album_id = ? ORDER BY track_number, title",
    )
    .bind(album_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let tracks: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "track_number": r.try_get::<String,_>("track_number").ok(),
                "size": r.try_get::<f64,_>("size").ok(),
                "file_size": r.try_get::<f64,_>("file_size").ok(),
            })
        })
        .collect();

    page(
        &state,
        "music/album.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "album": album,
            "author": author,
            "tracks": tracks,
            "playlists": playlists_dropdown(db, user.id).await,
            "write_access": is_super(&user),
            "from_letter": qs(&q, "from_letter"),
        }),
    )
}

async fn toggle_album_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<i64>,
) -> Response {
    let db = &state.db;
    let owner: Option<i64> = sqlx::query("SELECT user_id FROM music_albums WHERE id = ?")
        .bind(album_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<i64,_>("user_id").ok());
    let Some(owner_id) = owner else {
        return err404("Album not found");
    };
    if owner_id != user.id && !is_super(&user) {
        return err_json(axum::http::StatusCode::FORBIDDEN, "Not your album");
    }
    let new_val: i64 = sqlx::query(
        "UPDATE music_albums SET is_public = 1 - is_public, updated_at = ? WHERE id = ? \
         RETURNING is_public",
    )
    .bind(now_str())
    .bind(album_id)
    .fetch_one(db)
    .await
    .map(|r| r.get(0))
    .unwrap_or(0);
    axum::Json(json!({"success": true, "is_public": new_val == 1})).into_response()
}

async fn admin_set_all_albums_visibility(state: &SharedState, user: &syarc_plugin_api::User, public: bool) -> Response {
    if !is_super(user) {
        return err_json(axum::http::StatusCode::FORBIDDEN, "superuser required");
    }
    let sql = if public {
        "UPDATE music_albums SET is_public = 1, updated_at = ? WHERE is_banned = 0"
    } else {
        "UPDATE music_albums SET is_public = 0, updated_at = ?"
    };
    let _ = sqlx::query(sql).bind(now_str()).execute(&state.db).await;
    axum::response::Redirect::to("/library/music/admin").into_response()
}

async fn admin_all_albums_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    admin_set_all_albums_visibility(&state, &user, true).await
}

async fn admin_all_albums_private(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    admin_set_all_albums_visibility(&state, &user, false).await
}

async fn ban_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err_json(axum::http::StatusCode::FORBIDDEN, "superuser required");
    }
    sqlx::query("UPDATE music_albums SET is_banned = 1, is_public = 0 WHERE id = ?")
        .bind(album_id)
        .execute(&state.db)
        .await
        .ok();
    axum::Json(json!({"message": "Album banned from public view."})).into_response()
}

async fn unban_album(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(album_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err_json(axum::http::StatusCode::FORBIDDEN, "superuser required");
    }
    sqlx::query("UPDATE music_albums SET is_banned = 0 WHERE id = ?")
        .bind(album_id)
        .execute(&state.db)
        .await
        .ok();
    axum::Json(json!({"message": "Album unbanned."})).into_response()
}

// ------------------------------------------------------------------ player

const TRACK_SELECT: &str = "SELECT mt.id, mt.title, mt.track_number, \
     ma.id AS album_id, ma.title AS album_title, ma.image_filename AS album_image, \
     ma.updated_at AS album_updated_at, \
     mauth.id AS artist_id, mauth.name AS artist_name \
     FROM music_tracks mt \
     JOIN music_albums ma ON mt.album_id = ma.id \
     JOIN music_authors mauth ON ma.author_id = mauth.id";

fn track_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "title": r.try_get::<String,_>("title").unwrap_or_default(),
        "trackNumber": r.try_get::<String,_>("track_number").unwrap_or_else(|_| "0".into()),
        "albumId": r.get::<i64,_>("album_id"),
        "albumTitle": r.try_get::<String,_>("album_title").unwrap_or_default(),
        "albumImage": r.try_get::<String,_>("album_image").ok(),
        "albumUpdatedAt": r.try_get::<String,_>("album_updated_at").ok(),
        "artistId": r.get::<i64,_>("artist_id"),
        "artistName": r.try_get::<String,_>("artist_name").unwrap_or_default(),
    })
}

async fn player(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let source = qs(&q, "source");
    let shuffle = q.get("shuffle").map(|v| v != "false").unwrap_or(true);
    let max_tracks = 1000;
    let tracks: Vec<Value>;
    let mut source_name = String::new();

    if let Some(id) = source.strip_prefix("album:") {
        let album_id: i64 = id.parse().unwrap_or(0);
        if let Some(a) = album_json(db, album_id).await {
            source_name = format!("{} - {}", a["title"], author_name(db, a["author_id"].as_i64().unwrap()).await);
        }
        let rows = sqlx::query(&format!(
            "{TRACK_SELECT} WHERE mt.album_id = ? AND ma.media_type = 'music' ORDER BY mt.track_number"
        ))
        .bind(album_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
        tracks = rows.iter().map(track_row).collect();
    } else if let Some(id) = source.strip_prefix("artist:") {
        let artist_id: i64 = id.parse().unwrap_or(0);
        source_name = format!("{} - All Songs", author_name(db, artist_id).await);
        let rows = sqlx::query(&format!(
            "{TRACK_SELECT} WHERE mauth.id = ? AND ma.media_type = 'music' ORDER BY ma.title, mt.track_number LIMIT {max_tracks}"
        ))
        .bind(artist_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
        tracks = rows.iter().map(track_row).collect();
    } else if let Some(name) = source.strip_prefix("playlist:") {
        source_name = format!("Playlist: {name}");
        let items = sqlx::query(
            "SELECT item_type, item_id FROM music_playlist_items \
             WHERE playlist_name = ? AND user_id = ? ORDER BY id",
        )
        .bind(name)
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
        let mut t = Vec::new();
        for it in &items {
            let ty: String = it.try_get("item_type").unwrap_or_default();
            let iid: i64 = it.get("item_id");
            let rows = match ty.as_str() {
                "album" => sqlx::query(&format!(
                    "{TRACK_SELECT} WHERE mt.album_id = ? AND ma.media_type = 'music' ORDER BY mt.track_number"
                ))
                .bind(iid)
                .fetch_all(db)
                .await,
                "artist" => sqlx::query(&format!(
                    "{TRACK_SELECT} WHERE mauth.id = ? AND ma.media_type = 'music' ORDER BY ma.title, mt.track_number"
                ))
                .bind(iid)
                .fetch_all(db)
                .await,
                _ => sqlx::query(&format!(
                    "{TRACK_SELECT} WHERE mt.id = ? AND ma.media_type = 'music'"
                ))
                .bind(iid)
                .fetch_all(db)
                .await,
            }
            .unwrap_or_default();
            t.extend(rows.iter().map(track_row));
        }
        tracks = t;
    } else if let Some(id) = source.strip_prefix("song:") {
        source_name = "Now Playing".into();
        let song_id: i64 = id.parse().unwrap_or(0);
        let rows = sqlx::query(&format!(
            "{TRACK_SELECT} WHERE mt.id = ? AND ma.media_type = 'music'"
        ))
        .bind(song_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
        tracks = rows.iter().map(track_row).collect();
    } else {
        source_name = "Random Shuffle".into();
        let rows = sqlx::query(&format!(
            "{TRACK_SELECT} WHERE ma.media_type = 'music' ORDER BY RANDOM() LIMIT {max_tracks}"
        ))
        .fetch_all(db)
        .await
        .unwrap_or_default();
        tracks = rows.iter().map(track_row).collect();
    }

    if tracks.is_empty() {
        return err404("No tracks found for this source");
    }

    page(
        &state,
        "music/player.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "tracks": tracks,
            "source": source,
            "source_name": source_name,
            "shuffle": shuffle,
        }),
    )
}

async fn search(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let query = qs(&q, "q");
    if query.is_empty() {
        return axum::Json(json!({"artists": [], "albums": [], "tracks": []})).into_response();
    }
    let like = format!("%{}%", query.to_lowercase());
    let like_title = format!("%{query}%");

    let artists = sqlx::query("SELECT id, name FROM music_authors WHERE normalized_name LIKE ?")
        .bind(&like)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| json!({"id": r.get::<i64,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()}))
        .collect::<Vec<_>>();

    let albums = sqlx::query(
        "SELECT DISTINCT a.id, a.title, a.author_id, a.image_filename, a.updated_at \
         FROM music_albums a WHERE (a.title LIKE ? COLLATE NOCASE \
         OR a.author_id IN (SELECT id FROM music_authors WHERE normalized_name LIKE ?)) \
         AND a.media_type = 'music'",
    )
    .bind(&like_title)
    .bind(&like)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<i64,_>("id"),
            "title": r.try_get::<String,_>("title").unwrap_or_default(),
            "author_id": r.get::<i64,_>("author_id"),
            "image": r.try_get::<String,_>("image_filename").ok(),
            "updated_at": r.try_get::<String,_>("updated_at").ok(),
        })
    })
    .collect::<Vec<_>>();

    let tracks = sqlx::query(
        "SELECT mt.id, mt.title, mt.album_id, ma.title AS album_title, mauth.name AS author_name \
         FROM music_tracks mt \
         JOIN music_albums ma ON ma.id = mt.album_id \
         JOIN music_authors mauth ON mauth.id = ma.author_id \
         WHERE mt.title LIKE ? COLLATE NOCASE AND ma.media_type = 'music'",
    )
    .bind(&like_title)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<i64,_>("id"),
            "title": r.try_get::<String,_>("title").unwrap_or_default(),
            "album_id": r.get::<i64,_>("album_id"),
            "album_title": r.try_get::<String,_>("album_title").unwrap_or_default(),
            "author_name": r.try_get::<String,_>("author_name").unwrap_or_default(),
        })
    })
    .collect::<Vec<_>>();

    axum::Json(json!({"artists": artists, "albums": albums, "tracks": tracks})).into_response()
}

async fn stream_track(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(track_id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let path: Option<String> = sqlx::query("SELECT file_path FROM music_tracks WHERE id = ?")
        .bind(track_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("file_path").ok());
    let Some(path) = path else {
        return err404(&format!("Track {track_id} not found"));
    };
    if !std::path::Path::new(&path).exists() {
        return err404("Audio file missing from disk.");
    }
    serve_file(&path, headers.get("range").and_then(|v| v.to_str().ok())).await
}

async fn browse_songs(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let letter = qs(&q, "letter");
    let page_n = qi(&q, "page", 1);
    let per_page = qi(&q, "per_page", 50).clamp(50, 200);

    let cond = if letter == "0" {
        "AND mt.title GLOB '[^A-Za-z]*'"
    } else if letter.chars().count() == 1 && letter.chars().next().unwrap().is_ascii_alphabetic() {
        // safe: single letter
        &format!("AND mt.title LIKE '{}%' COLLATE NOCASE", letter.to_uppercase())
    } else {
        ""
    };

    let total: i64 = sqlx::query(&format!(
        "SELECT COUNT(*) FROM music_tracks mt JOIN music_albums ma ON ma.id = mt.album_id \
         WHERE ma.media_type = 'music' {cond}"
    ))
    .fetch_one(db)
    .await
    .unwrap()
    .get(0);
    let total_pages = (total + per_page - 1) / per_page;
    let offset = (page_n - 1) * per_page;

    let rows = sqlx::query(&format!(
        "SELECT mt.id, mt.title, mt.track_number, ma.id AS album_id, ma.title AS album_title, \
         ma.image_filename AS album_image, mauth.id AS artist_id, mauth.name AS artist_name \
         FROM music_tracks mt JOIN music_albums ma ON ma.id = mt.album_id \
         JOIN music_authors mauth ON mauth.id = ma.author_id \
         WHERE ma.media_type = 'music' {cond} ORDER BY mt.title LIMIT {per_page} OFFSET {offset}"
    ))
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let mut track_num = offset;
    let tracks: Vec<Value> = rows
        .iter()
        .map(|r| {
            track_num += 1;
            let tn = r
                .try_get::<String,_>("track_number")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| track_num.to_string());
            json!({
                "id": r.get::<i64,_>("id"),
                "title": r.try_get::<String,_>("title").unwrap_or_default(),
                "trackNumber": tn,
                "albumId": r.get::<i64,_>("album_id"),
                "albumTitle": r.try_get::<String,_>("album_title").unwrap_or_default(),
                "albumImage": r.try_get::<String,_>("album_image").ok(),
                "artistId": r.get::<i64,_>("artist_id"),
                "artistName": r.try_get::<String,_>("artist_name").unwrap_or_default(),
            })
        })
        .collect();

    page(
        &state,
        "music/browse_songs.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "tracks": tracks,
            "playlists": playlists_dropdown(db, user.id).await,
            "current_letter": letter,
            "current_page": page_n,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
            "write_access": is_super(&user),
        }),
    )
}

// --------------------------------------------------------------- playlists

async fn list_playlists(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let playlists = playlists_dropdown(&state.db, user.id).await;
    page(
        &state,
        "music/playlists.html",
        json!({"user": {"id": user.id, "username": user.username}, "playlists": playlists}),
    )
}

async fn api_list_playlists(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    axum::Json(playlists_dropdown(&state.db, user.id).await).into_response()
}

async fn import_playlist_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    page(
        &state,
        "music/import_playlist.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "playlists": playlists_dropdown(&state.db, user.id).await,
        }),
    )
}

async fn view_playlist_details(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query(
        "SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?",
    )
    .bind(&playlist_name)
    .bind(user.id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if exists.is_none() {
        return err404("Playlist not found");
    }

    let rows = sqlx::query(
        "SELECT id, item_type, item_id FROM music_playlist_items \
         WHERE playlist_name = ? AND user_id = ? ORDER BY id",
    )
    .bind(&playlist_name)
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let mut items = Vec::new();
    for r in &rows {
        let ty: String = r.try_get("item_type").unwrap_or_default();
        let iid: i64 = r.get("item_id");
        let id: i64 = r.get("id");
        match ty.as_str() {
            "album" => {
                if let Some(a) = album_json(db, iid).await {
                    items.push(json!({
                        "type": "album", "id": id, "item_id": iid,
                        "title": a["title"],
                        "artist": author_name(db, a["author_id"].as_i64().unwrap()).await,
                        "image": a["image_filename"],
                        "art_id": iid,
                    }));
                }
            }
            "artist" => {
                items.push(json!({
                    "type": "artist", "id": id, "item_id": iid,
                    "title": author_name(db, iid).await, "artist": "Artist", "image": null,
                }));
            }
            _ => {
                let row = sqlx::query(
                    "SELECT t.title, a.title AS album_title, a.image_filename, a.id AS album_id \
                     FROM music_tracks t JOIN music_albums a ON a.id = t.album_id WHERE t.id = ?",
                )
                .bind(iid)
                .fetch_optional(db)
                .await
                .ok()
                .flatten();
                if let Some(t) = row {
                    items.push(json!({
                        "type": "song", "id": id, "item_id": iid,
                        "title": t.try_get::<String,_>("title").unwrap_or_default(),
                        "artist": author_name(db, t.get::<i64,_>("album_id")).await,
                        "album": t.try_get::<String,_>("album_title").unwrap_or_default(),
                        "image": t.try_get::<String,_>("image_filename").ok(),
                        "art_id": t.try_get::<i64,_>("album_id").ok(),
                    }));
                }
            }
        }
    }

    page(
        &state,
        "music/playlist_details.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "playlist_name": playlist_name,
            "items": items,
        }),
    )
}

async fn create_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let name = fv(&form, "name").unwrap_or_default();
    let playlist_type = form.get("type").cloned().unwrap_or_else(|| "song".into());
    let item_id: Option<i64> = fv(&form, "item_id").and_then(|v| v.parse().ok());

    if name.is_empty() {
        return err400("Playlist name is required");
    }
    let exists = sqlx::query("SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?")
        .bind(&name)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if exists.is_some() {
        return err400("Playlist with this name already exists");
    }

    sqlx::query("INSERT INTO music_playlists (name, user_id, created_at) VALUES (?,?,?)")
        .bind(&name)
        .bind(user.id)
        .bind(now_str())
        .execute(&state.db)
        .await
        .ok();
    if let Some(iid) = item_id {
        sqlx::query(
            "INSERT INTO music_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)",
        )
        .bind(&name)
        .bind(user.id)
        .bind(&playlist_type)
        .bind(iid)
        .execute(&state.db)
        .await
        .ok();
    }
    axum::Json(json!({
        "success": true,
        "message": format!("Playlist '{name}' created"),
        "playlist_name": name,
    }))
    .into_response()
}

async fn random_add_to_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    use rand::rngs::StdRng;
    use rand::seq::SliceRandom;
    use rand::SeedableRng;
    let db = &state.db;
    let form = form_map(&raw);
    let playlist_name = fv(&form, "playlist_name").unwrap_or_default();
    let add_type = form.get("add_type").cloned().unwrap_or_else(|| "tracks".into());
    let count: i64 = form.get("count").and_then(|v| v.parse().ok()).unwrap_or(10);
    if playlist_name.is_empty() {
        return err400("Playlist name is required");
    }
    if count < 1 {
        return err400("Count must be at least 1");
    }
    let exists = sqlx::query("SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Playlist not found");
    }

    let (all_sql, existing_sql, item_type) = if add_type == "albums" {
        ("SELECT id FROM music_albums WHERE media_type = 'music'",
         "SELECT item_id FROM music_playlist_items WHERE playlist_name = ? AND user_id = ? AND item_type = 'album'",
         "album")
    } else {
        ("SELECT t.id FROM music_tracks t JOIN music_albums a ON a.id = t.album_id WHERE a.media_type = 'music'",
         "SELECT item_id FROM music_playlist_items WHERE playlist_name = ? AND user_id = ? AND item_type = 'song'",
         "song")
    };

    let existing: Vec<i64> = sqlx::query(existing_sql)
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let all: Vec<i64> = sqlx::query(all_sql)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let available: Vec<i64> = all.into_iter().filter(|id| !existing.contains(id)).collect();
    if available.is_empty() {
        return axum::Json(json!({
            "success": false,
            "message": format!("No more {} available to add", if add_type == "albums" {"albums"} else {"tracks"}),
        }))
        .into_response();
    }

    let mut rng = StdRng::from_entropy();
    let mut pool = available.clone();
    pool.shuffle(&mut rng);
    let selected: Vec<i64> = pool.into_iter().take(count as usize).collect();

    let mut added_items = Vec::new();
    for id in &selected {
        sqlx::query("INSERT INTO music_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)")
            .bind(&playlist_name)
            .bind(user.id)
            .bind(item_type)
            .bind(id)
            .execute(db)
            .await
            .ok();
        let label = if add_type == "albums" {
            if let Some(a) = album_json(db, *id).await {
                format!("{} - {}", author_name(db, a["author_id"].as_i64().unwrap()).await, a["title"].as_str().unwrap_or(""))
            } else {
                String::new()
            }
        } else {
            sqlx::query(
                "SELECT t.title, ma.title AS album_title, mauth.name AS artist_name \
                 FROM music_tracks t JOIN music_albums ma ON ma.id = t.album_id \
                 JOIN music_authors mauth ON mauth.id = ma.author_id WHERE t.id = ?",
            )
            .bind(id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .map(|r| format!("{} - {} - {}",
                r.try_get::<String,_>("artist_name").unwrap_or_default(),
                r.try_get::<String,_>("album_title").unwrap_or_default(),
                r.try_get::<String,_>("title").unwrap_or_default()))
            .unwrap_or_default()
        };
        added_items.push(label);
    }

    axum::Json(json!({
        "success": true,
        "message": format!("Added {} {} to '{}'", selected.len(),
            if add_type == "albums" {"albums"} else {"tracks"}, playlist_name),
        "added_count": selected.len(),
        "items": added_items,
    }))
    .into_response()
}

async fn add_to_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
    RawForm(raw): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let item_type = form.get("type").cloned().unwrap_or_else(|| "song".into());
    let item_id: Option<i64> = fv(&form, "item_id").and_then(|v| v.parse().ok());

    let exists = sqlx::query("SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Playlist not found");
    }

    if let Some(iid) = item_id {
        let dupe = sqlx::query(
            "SELECT 1 FROM music_playlist_items WHERE playlist_name = ? AND user_id = ? \
             AND item_type = ? AND item_id = ?",
        )
        .bind(&playlist_name)
        .bind(user.id)
        .bind(&item_type)
        .bind(iid)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        if dupe.is_some() {
            return axum::Json(json!({
                "success": true,
                "message": format!("Already in playlist '{playlist_name}'"),
                "playlist_name": playlist_name,
            }))
            .into_response();
        }
        sqlx::query("INSERT INTO music_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)")
            .bind(&playlist_name)
            .bind(user.id)
            .bind(&item_type)
            .bind(iid)
            .execute(db)
            .await
            .ok();
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
    let db = &state.db;
    sqlx::query("DELETE FROM music_playlist_items WHERE playlist_name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .execute(db)
        .await
        .ok();
    sqlx::query("DELETE FROM music_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .execute(db)
        .await
        .ok();
    crate::redirect("/library/music/playlists")
}

async fn delete_playlist_item(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((playlist_name, item_id)): Path<(String, i64)>,
) -> Response {
    sqlx::query(
        "DELETE FROM music_playlist_items WHERE id = ? AND playlist_name = ? AND user_id = ?",
    )
    .bind(item_id)
    .bind(&playlist_name)
    .bind(user.id)
    .execute(&state.db)
    .await
    .ok();
    crate::redirect(&format!("/library/music/playlists/{playlist_name}"))
}

/// Resolve playlist items to a flat list of track ids.
pub(crate) async fn playlist_track_ids(
    db: &syarc_plugin_api::Db,
    table_items: &str,
    name: &str,
    user_id: i64,
) -> Vec<i64> {
    let rows = sqlx::query(&format!(
        "SELECT item_type, item_id FROM {table_items} WHERE playlist_name = ? AND user_id = ? ORDER BY id"
    ))
    .bind(name)
    .bind(user_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let mut ids = Vec::new();
    for r in rows {
        let ty: String = r.try_get("item_type").unwrap_or_default();
        let iid: i64 = r.get("item_id");
        match ty.as_str() {
            "album" => {
                let ts: Vec<i64> = sqlx::query("SELECT id FROM music_tracks WHERE album_id = ?")
                    .bind(iid)
                    .fetch_all(db)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(|x| x.get(0))
                    .collect();
                ids.extend(ts);
            }
            "artist" => {
                let ts: Vec<i64> = sqlx::query(
                    "SELECT t.id FROM music_tracks t JOIN music_albums a ON a.id = t.album_id \
                     WHERE a.author_id = ? AND a.media_type = 'music'",
                )
                .bind(iid)
                .fetch_all(db)
                .await
                .unwrap_or_default()
                .iter()
                .map(|x| x.get(0))
                .collect();
                ids.extend(ts);
            }
            _ => ids.push(iid),
        }
    }
    ids
}

async fn analyze_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query("SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Playlist not found");
    }
    let track_ids = playlist_track_ids(db, "music_playlist_items", &playlist_name, user.id).await;
    if track_ids.is_empty() {
        return err400("No tracks found in playlist");
    }
    let mut placeholders = String::new();
    for (i, id) in track_ids.iter().enumerate() {
        if i > 0 {
            placeholders.push(',');
        }
        placeholders.push_str(&id.to_string());
    }
    let rows = sqlx::query(&format!(
        "SELECT mauth.name AS author_name, ma.title AS album_title, mt.size \
         FROM music_tracks mt JOIN music_albums ma ON ma.id = mt.album_id \
         JOIN music_authors mauth ON mauth.id = ma.author_id \
         WHERE mt.id IN ({placeholders})"
    ))
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let total_tracks = rows.len();
    let mut total_size_mb = 0.0;
    let mut dirs: Vec<(String, String)> = Vec::new();
    for r in &rows {
        if let Ok(s) = r.try_get::<f64,_>("size") {
            total_size_mb += s;
        }
        let key = (
            r.try_get::<String,_>("author_name").unwrap_or_default(),
            r.try_get::<String,_>("album_title").unwrap_or_default(),
        );
        if !dirs.contains(&key) {
            dirs.push(key);
        }
    }
    let total_files = total_tracks + dirs.len();
    axum::Json(json!({
        "total_tracks": total_tracks,
        "total_files": total_files,
        "total_size_mb": total_size_mb,
        "unique_author_albums": dirs.len(),
    }))
    .into_response()
}

/// Minimal ustar tar writer for playlist export.
pub(crate) fn tar_append(tar: &mut Vec<u8>, path: &str, data: &[u8]) {
    let mut header = [0u8; 512];
    let name = path.as_bytes();
    header[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
    let oct = |buf: &mut [u8], v: u64| {
        let s = format!("{v:011o}");
        let n = s.len().min(buf.len());
        buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    };
    oct(&mut header[100..112], 0o644);
    oct(&mut header[108..116], 0);
    oct(&mut header[116..124], 0);
    oct(&mut header[124..136], data.len() as u64);
    oct(&mut header[136..148], 0);
    header[148..156].copy_from_slice(b"        ");
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\x00");
    let checksum: u32 = header.iter().map(|b| *b as u32).sum();
    oct(&mut header[148..156], checksum as u64);
    header[155] = 0;
    tar.extend_from_slice(&header);
    tar.extend_from_slice(data);
    let pad = (512 - (data.len() % 512)) % 512;
    tar.extend(std::iter::repeat_n(0u8, pad));
}

async fn export_playlist(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(playlist_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query("SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?")
        .bind(&playlist_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Playlist not found");
    }
    let track_ids = playlist_track_ids(db, "music_playlist_items", &playlist_name, user.id).await;
    if track_ids.is_empty() {
        return err400("No tracks found in playlist");
    }
    let mut placeholders = String::new();
    for (i, id) in track_ids.iter().enumerate() {
        if i > 0 {
            placeholders.push(',');
        }
        placeholders.push_str(&id.to_string());
    }
    let rows = sqlx::query(&format!(
        "SELECT mauth.name AS author, ma.title AS album, mt.title AS track, mt.file_path \
         FROM music_tracks mt JOIN music_albums ma ON ma.id = mt.album_id \
         JOIN music_authors mauth ON mauth.id = ma.author_id \
         WHERE mt.id IN ({placeholders})"
    ))
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let mut tar: Vec<u8> = Vec::new();
    for r in &rows {
        let author = r.try_get::<String,_>("author").unwrap_or_default();
        let album = r.try_get::<String,_>("album").unwrap_or_default();
        let file_path = r.try_get::<String,_>("file_path").unwrap_or_default();
        let Ok(data) = tokio::fs::read(&file_path).await else {
            continue;
        };
        let base = file_path.rsplit('/').next().unwrap_or("track");
        let entry = format!("{playlist_name}/{author}/{album}/{base}");
        tar_append(&mut tar, &entry, &data);
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

pub(crate) fn normalize_unicode(s: &str) -> String {
    s.replace(['\u{2018}', '\u{2019}', '\u{201B}', '\u{2032}', '\u{2035}', '\u{00B4}'], "'")
        .replace(['\u{201C}', '\u{201D}', '\u{201F}'], "\"")
        .replace(['\u{2013}', '\u{2014}', '\u{2015}'], "-")
}

async fn find_album_fuzzy(
    db: &syarc_plugin_api::Db,
    author_id: i64,
    title: &str,
) -> Option<i64> {
    let normalized = title.replace(['\u{2013}', '\u{2014}'], "-");
    let like = format!("%{normalized}%");
    let hit = sqlx::query(
        "SELECT id FROM music_albums WHERE title LIKE ? COLLATE NOCASE AND author_id = ? AND media_type = 'music'",
    )
    .bind(&like)
    .bind(author_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if hit.is_some() {
        return hit.map(|r| r.get::<i64,_>("id"));
    }
    let fuzzy: String = normalized
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || "-_&+".contains(*c))
        .collect();
    let like2 = format!("%{fuzzy}%");
    sqlx::query(
        "SELECT id FROM music_albums WHERE title LIKE ? COLLATE NOCASE AND author_id = ? AND media_type = 'music'",
    )
    .bind(&like2)
    .bind(author_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| r.get::<i64,_>("id"))
}

async fn find_author_like(db: &syarc_plugin_api::Db, name: &str) -> Option<i64> {
    let like = format!("%{name}%");
    sqlx::query("SELECT id FROM music_authors WHERE name LIKE ? COLLATE NOCASE")
        .bind(&like)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<i64,_>("id"))
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
        let exists = sqlx::query("SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?")
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
        sqlx::query("INSERT INTO music_playlists (name, user_id, created_at) VALUES (?,?,?)")
            .bind(&playlist_name)
            .bind(user.id)
            .bind(now_str())
            .execute(db)
            .await
            .ok();
    } else {
        if playlist_name.is_empty() {
            return axum::Json(json!({"success": false, "message": "Please select an existing playlist or create a new one"})).into_response();
        }
        let exists = sqlx::query("SELECT 1 FROM music_playlists WHERE name = ? AND user_id = ?")
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

    let mut import_items: Vec<(Option<String>, String, Option<String>)> = Vec::new();
    for line in import_text.lines() {
        let line = normalize_unicode(line.trim());
        if line.is_empty() {
            continue;
        }
        let sep = [" - ", " \u{2013} ", " \u{2014} ", "\u{2013} ", "\u{2014} "]
            .iter()
            .find(|s| line.contains(*s))
            .copied();
        if let Some(sep) = sep {
            let parts: Vec<&str> = line.split(sep).map(|p| p.trim()).collect();
            if parts.len() >= 2 {
                import_items.push((
                    Some(normalize_unicode(parts[0])),
                    normalize_unicode(parts[1]),
                    parts.get(2).map(|p| normalize_unicode(p)),
                ));
                continue;
            }
        }
        import_items.push((None, normalize_unicode(&line), None));
    }
    if import_items.is_empty() {
        return axum::Json(json!({"success": false, "message": "No valid items found in import text"})).into_response();
    }

    let mut found = Vec::new();
    let mut not_found = Vec::new();

    for (author, album, track) in &import_items {
        if let Some(track_title) = track {
            let mut resolved = false;
            if let Some(author_name) = author {
                if let Some(aid) = find_author_like(db, author_name).await {
                    if let Some(album_id) = find_album_fuzzy(db, aid, album).await {
                        let like = format!("%{track_title}%");
                        if let Some(row) = sqlx::query(
                            "SELECT id FROM music_tracks WHERE title LIKE ? COLLATE NOCASE AND album_id = ?",
                        )
                        .bind(&like)
                        .bind(album_id)
                        .fetch_optional(db)
                        .await
                        .ok()
                        .flatten()
                        {
                            let tid: i64 = row.get("id");
                            let dupe = sqlx::query(
                                "SELECT 1 FROM music_playlist_items WHERE playlist_name = ? AND user_id = ? AND item_type = 'song' AND item_id = ?",
                            )
                            .bind(&playlist_name).bind(user.id).bind(tid)
                            .fetch_optional(db).await.ok().flatten();
                            let label = format!("{author_name} - {album} - {track_title}");
                            if dupe.is_none() {
                                sqlx::query("INSERT INTO music_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)")
                                    .bind(&playlist_name).bind(user.id).bind("song").bind(tid)
                                    .execute(db).await.ok();
                                found.push(label);
                            } else {
                                found.push(format!("{label} (already in playlist)"));
                            }
                            resolved = true;
                        }
                    }
                }
            }
            if !resolved {
                not_found.push(format!("{} - {} - {}", author.as_deref().unwrap_or(""), album, track_title));
            }
        } else {
            // album import
            let mut album_found = false;
            let mut album_id = None;
            if let Some(author_name) = author {
                if let Some(aid) = find_author_like(db, author_name).await {
                    album_id = find_album_fuzzy(db, aid, album).await;
                }
            }
            if album_id.is_none() {
                let normalized = album.replace(['\u{2013}', '\u{2014}'], "-");
                let like = format!("%{normalized}%");
                album_id = sqlx::query(
                    "SELECT id FROM music_albums WHERE title LIKE ? COLLATE NOCASE AND media_type = 'music'",
                )
                .bind(&like)
                .fetch_optional(db)
                .await
                .ok()
                .flatten()
                .map(|r| r.get::<i64,_>("id"));
            }
            match album_id {
                Some(aid) => {
                    let dupe = sqlx::query(
                        "SELECT 1 FROM music_playlist_items WHERE playlist_name = ? AND user_id = ? AND item_type = 'album' AND item_id = ?",
                    )
                    .bind(&playlist_name).bind(user.id).bind(aid)
                    .fetch_optional(db).await.ok().flatten();
                    let an = sqlx::query(
                        "SELECT mauth.name AS a FROM music_albums ma JOIN music_authors mauth ON mauth.id = ma.author_id WHERE ma.id = ?",
                    )
                    .bind(aid).fetch_optional(db).await.ok().flatten()
                    .and_then(|r| r.try_get::<String,_>("a").ok())
                    .unwrap_or_else(|| "Unknown".into());
                    let label = format!("{an} - {album}");
                    if dupe.is_none() {
                        sqlx::query("INSERT INTO music_playlist_items (playlist_name, user_id, item_type, item_id) VALUES (?,?,?,?)")
                            .bind(&playlist_name).bind(user.id).bind("album").bind(aid)
                            .execute(db).await.ok();
                        found.push(label);
                    } else {
                        found.push(format!("{label} (already in playlist)"));
                    }
                    album_found = true;
                }
                None => {
                    not_found.push(match author {
                        Some(a) => format!("{a} - {album}"),
                        None => album.clone(),
                    });
                }
            }
            let _ = album_found;
        }
    }

    axum::Json(json!({
        "success": true,
        "message": format!("Import complete to '{playlist_name}'"),
        "playlist_name": playlist_name,
        "found": found,
        "not_found": not_found,
        "found_count": found.len(),
        "not_found_count": not_found.len(),
    }))
    .into_response()
}

// ------------------------------------------------------------------- admin

async fn get_album_info(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let db = &state.db;
    let artist = qs(&q, "artist");
    let album = qs(&q, "album");
    let Some(aid) = find_author_like(db, &artist).await else {
        return axum::Json(json!({"found": false, "error": format!("Artist '{artist}' not found")})).into_response();
    };
    let normalized = album.replace(['\u{2013}', '\u{2014}'], "-");
    let like = format!("%{normalized}%");
    let row = sqlx::query("SELECT id, title, image_filename FROM music_albums WHERE title LIKE ? COLLATE NOCASE AND author_id = ?")
        .bind(&like)
        .bind(aid)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return axum::Json(json!({"found": false, "error": format!("Album '{album}' not found")})).into_response();
    };
    let id: i64 = row.get("id");
    let image: Option<String> = row.try_get("image_filename").ok();
    let art_url = image.as_ref().map(|_| format!("/library/music/art/{id}?t={}", chrono::Local::now().timestamp()));
    axum::Json(json!({
        "found": true,
        "album_id": id,
        "title": row.try_get::<String,_>("title").unwrap_or_default(),
        "art_filename": image,
        "art_url": art_url,
    }))
    .into_response()
}

async fn music_admin(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(q): Query<Q>,
) -> Response {
    let album_id = q.get("album_id").and_then(|v| v.parse::<i64>().ok());
    let mut album = Value::Null;
    let mut author = Value::Null;
    if let Some(aid) = album_id {
        if let Some(a) = album_json(&state.db, aid).await {
            author = json!({"id": a["author_id"], "name": author_name(&state.db, a["author_id"].as_i64().unwrap()).await});
            album = a;
        }
    }
    page(
        &state,
        "music/admin.html",
        json!({
            "user": {"id": user.id, "username": user.username},
            "album": album,
            "author": author,
            "from_letter": qs(&q, "from_letter"),
            "album_id": album_id,
        }),
    )
}

fn norm_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .trim()
        .to_string()
}

async fn update_album_metadata(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let new_artist = fv(&form, "artist").unwrap_or_default();
    let new_title = fv(&form, "album").unwrap_or_default();
    let album_id: Option<i64> = fv(&form, "album_id").and_then(|v| v.parse().ok());

    if new_artist.is_empty() || new_title.is_empty() {
        return err400("Artist name and album title are required.");
    }
    let Some(album_id) = album_id else {
        return err404("Album not found. Access this page from an album's Edit Metadata button.");
    };
    let row = sqlx::query("SELECT author_id, title FROM music_albums WHERE id = ?")
        .bind(album_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return err404("Album not found.");
    };
    let current_author_id: i64 = row.get("author_id");
    let current_title: String = row.try_get("title").unwrap_or_default();
    let current_author_name = author_name(db, current_author_id).await;

    let mut artist_changed = false;
    if norm_name(&new_artist) != norm_name(&current_author_name) {
        let existing = sqlx::query("SELECT id FROM music_authors WHERE normalized_name = ?")
            .bind(norm_name(&new_artist))
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
        if let Some(e) = existing {
            let new_author_id: i64 = e.get("id");
            sqlx::query("UPDATE music_albums SET author_id = ? WHERE id = ?")
                .bind(new_author_id)
                .bind(album_id)
                .execute(db)
                .await
                .ok();
            let remaining: i64 = sqlx::query("SELECT COUNT(*) FROM music_albums WHERE author_id = ?")
                .bind(current_author_id)
                .fetch_one(db)
                .await
                .unwrap()
                .get(0);
            if remaining == 0 {
                sqlx::query("DELETE FROM music_authors WHERE id = ?")
                    .bind(current_author_id)
                    .execute(db)
                    .await
                    .ok();
            }
        } else {
            sqlx::query("UPDATE music_authors SET name = ?, normalized_name = ? WHERE id = ?")
                .bind(&new_artist)
                .bind(norm_name(&new_artist))
                .bind(current_author_id)
                .execute(db)
                .await
                .ok();
        }
        artist_changed = true;
    }
    let title_changed = new_title != current_title;
    if title_changed {
        sqlx::query("UPDATE music_albums SET title = ?, updated_at = ? WHERE id = ?")
            .bind(&new_title)
            .bind(now_str())
            .bind(album_id)
            .execute(db)
            .await
            .ok();
    }
    if !artist_changed && !title_changed {
        return axum::Json(json!({"success": true, "message": "No changes detected."})).into_response();
    }
    let mut parts = Vec::new();
    if title_changed {
        parts.push(format!("Album title updated to \"{new_title}\""));
    }
    if artist_changed {
        parts.push(format!("Artist updated to \"{new_artist}\""));
    }
    axum::Json(json!({
        "success": true,
        "message": format!("{}.", parts.join("; ")),
        "new_title": new_title,
        "new_artist": new_artist,
    }))
    .into_response()
}

async fn update_album_art(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let target_artist = fv(&form, "target_artist").unwrap_or_default();
    let target_album = fv(&form, "target_album").unwrap_or_default();
    let source_artist = fv(&form, "source_artist").unwrap_or_default();
    let source_album = fv(&form, "source_album").unwrap_or_default();
    let source_track = fv(&form, "source_track").unwrap_or_default();

    if [target_artist.is_empty(), target_album.is_empty(), source_artist.is_empty(), source_album.is_empty(), source_track.is_empty()]
        .iter()
        .any(|e| *e)
    {
        return err400("All fields must be provided");
    }

    let Some(t_aid) = find_author_like(db, &target_artist).await else {
        return err404(&format!("Target album not found: Artist '{target_artist}' not found"));
    };
    let Some(target_album_id) = find_album_fuzzy(db, t_aid, &target_album).await else {
        return err404(&format!("Target album not found: Album '{target_album}' not found for artist '{target_artist}'"));
    };

    let Some(s_aid) = find_author_like(db, &source_artist).await else {
        return err404(&format!("Source track not found: Artist '{source_artist}' not found"));
    };
    let Some(source_album_id) = find_album_fuzzy(db, s_aid, &source_album).await else {
        return err404(&format!("Source track not found: Album '{source_album}' not found"));
    };
    let like = format!("%{source_track}%");
    let source_path = sqlx::query("SELECT file_path FROM music_tracks WHERE title LIKE ? COLLATE NOCASE AND album_id = ?")
        .bind(&like)
        .bind(source_album_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("file_path").ok());
    let Some(source_path) = source_path else {
        return err404(&format!("Source track not found: Track '{source_track}' not found in album '{source_album}'"));
    };
    if !std::path::Path::new(&source_path).exists() {
        return err404("Source track file not found on filesystem");
    }

    let Some((data, _is_png)) = embedded_art(&source_path) else {
        return err400("No embedded artwork found in source track");
    };

    let dir: String = sqlx::query("SELECT directory_path FROM music_albums WHERE id = ?")
        .bind(target_album_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("directory_path").ok())
        .unwrap_or_default();
    let art_filename = write_album_art(&dir, target_album_id, &data);
    sqlx::query("UPDATE music_albums SET image_filename = ?, updated_at = ? WHERE id = ?")
        .bind(&art_filename)
        .bind(now_str())
        .bind(target_album_id)
        .execute(db)
        .await
        .ok();
    axum::Json(json!({
        "success": true,
        "message": format!("Successfully extracted and saved artwork for '{target_album}'"),
    }))
    .into_response()
}

async fn upload_album_art(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let artist = fv(&form, "artist").unwrap_or_default();
    let album = fv(&form, "album").unwrap_or_default();
    let artwork_data = fv(&form, "artwork_data");
    let embed = form.get("embed_in_tracks").map(|v| v != "false").unwrap_or(true);

    if artist.is_empty() || album.is_empty() {
        return err400("Artist and album must be provided");
    }
    // Artwork arrives as base64 clipboard paste (form-encoded; file uploads
    // are handled the same way by the template's JS).
    let Some(b64) = artwork_data.filter(|s| s.starts_with("data:image")) else {
        return err400("Please provide an image file or paste from clipboard");
    };
    let payload = b64.split(',').nth(1).unwrap_or("");
    use base64::Engine;
    let Ok(data) = base64::engine::general_purpose::STANDARD.decode(payload) else {
        return err400("Invalid image data");
    };

    let Some(aid) = find_author_like(db, &artist).await else {
        return err404(&format!("Artist '{artist}' not found"));
    };
    let Some(album_id) = find_album_fuzzy(db, aid, &album).await else {
        return err404(&format!("Album '{album}' not found for artist '{artist}'"));
    };

    let dir: String = sqlx::query("SELECT directory_path FROM music_albums WHERE id = ?")
        .bind(album_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("directory_path").ok())
        .unwrap_or_default();
    let art_filename = write_album_art(&dir, album_id, &data);
    sqlx::query("UPDATE music_albums SET image_filename = ?, updated_at = ? WHERE id = ?")
        .bind(&art_filename)
        .bind(now_str())
        .bind(album_id)
        .execute(db)
        .await
        .ok();

    let mut embed_summary = String::new();
    if embed {
        // Tag re-embedding into source files is skipped in the console port:
        // the art file is authoritative for display.
        embed_summary = "\nTag embedding not available in console port.".into();
    }
    axum::Json(json!({
        "success": true,
        "message": format!("Successfully uploaded artwork for '{album}'{embed_summary}"),
    }))
    .into_response()
}

async fn delete_album(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let artist_name_in = fv(&form, "artist").unwrap_or_default();
    let album_name = fv(&form, "album").unwrap_or_default();
    let delete_from_disk = form.get("delete_from_disk").map(|v| v == "true").unwrap_or(false);

    if artist_name_in.is_empty() || album_name.is_empty() {
        return err400("Artist and album are required");
    }
    let Some(aid) = find_author_like(db, &artist_name_in).await else {
        return err404(&format!("Artist '{artist_name_in}' not found"));
    };
    let Some(album_id) = find_album_fuzzy(db, aid, &album_name).await else {
        return err404(&format!("Album '{album_name}' not found for artist '{artist_name_in}'"));
    };

    let title: String = sqlx::query("SELECT title FROM music_albums WHERE id = ?")
        .bind(album_id)
        .fetch_one(db)
        .await
        .map(|r| r.get(0))
        .unwrap_or_default();
    let image_filename: Option<String> =
        sqlx::query("SELECT image_filename FROM music_albums WHERE id = ?")
            .bind(album_id)
            .fetch_one(db)
            .await
            .ok()
            .and_then(|r| r.try_get(0).ok());
    let track_paths: Vec<String> = sqlx::query("SELECT file_path FROM music_tracks WHERE album_id = ?")
        .bind(album_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.try_get(0).ok())
        .collect();

    sqlx::query("DELETE FROM music_playlist_items WHERE item_id = ? AND item_type = 'album'")
        .bind(album_id)
        .execute(db)
        .await
        .ok();
    sqlx::query("DELETE FROM music_tracks WHERE album_id = ?").bind(album_id).execute(db).await.ok();
    sqlx::query("DELETE FROM music_albums WHERE id = ?").bind(album_id).execute(db).await.ok();

    if let Some(img) = &image_filename {
        let _ = std::fs::remove_file(static_dir("music").join("art").join(img));
    }
    let mut disk_msg = String::new();
    if delete_from_disk {
        let mut removed = 0;
        for p in &track_paths {
            if std::fs::remove_file(p).is_ok() {
                removed += 1;
            }
        }
        disk_msg = format!(", {removed} track file(s) removed from disk");
    }
    axum::Json(json!({
        "success": true,
        "message": format!("Album '{title}' deleted from database{disk_msg}"),
    }))
    .into_response()
}

// ---------------------------------------------------------------- manual add

fn find_mp3s(dir: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.path().to_string_lossy().to_string())
        .filter(|p| p.to_lowercase().ends_with(".mp3"))
        .collect();
    files.sort();
    files
}

async fn get_or_create_author(db: &syarc_plugin_api::Db, name: &str) -> i64 {
    let name = if name.trim().is_empty() { "Unknown Artist" } else { name.trim() };
    let norm = name.to_lowercase();
    if let Some(r) = sqlx::query("SELECT id FROM music_authors WHERE normalized_name = ?")
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    {
        return r.get(0);
    }
    sqlx::query("INSERT INTO music_authors (name, normalized_name) VALUES (?,?)")
        .bind(name)
        .bind(&norm)
        .execute(db)
        .await
        .map(|r| r.last_insert_rowid())
        .unwrap_or(0)
}

pub(crate) async fn get_or_create_album(
    db: &syarc_plugin_api::Db,
    author_id: i64,
    folder_name: &str,
    folder_path: &str,
    sample_mp3: &str,
    user_id: Option<i64>,
    is_public: bool,
) -> i64 {
    if let Some(r) = sqlx::query(
        "SELECT id FROM music_albums WHERE author_id = ? AND title = ?",
    )
    .bind(author_id)
    .bind(folder_name)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    {
        let id: i64 = r.get(0);
        if let Some(uid) = user_id {
            sqlx::query("UPDATE music_albums SET user_id = COALESCE(user_id, ?), directory_path = COALESCE(directory_path, ?) WHERE id = ? AND user_id IS NULL")
                .bind(uid)
                .bind(folder_path)
                .bind(id)
                .execute(db)
                .await
                .ok();
        }
        return id;
    }
    let id = sqlx::query(
        "INSERT INTO music_albums (title, author_id, media_type, library_id, user_id, is_public, directory_path, updated_at) \
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(folder_name)
    .bind(author_id)
    .bind("music")
    .bind(Option::<String>::None)
    .bind(user_id)
    .bind(is_public as i64)
    .bind(folder_path)
    .bind(now_str())
    .execute(db)
    .await
    .map(|r| r.last_insert_rowid())
    .unwrap_or(0);
    if album_dir_art(folder_path).is_some() {
        sqlx::query("UPDATE music_albums SET image_filename = ? WHERE id = ?")
            .bind(ART_SIBLING)
            .bind(id)
            .execute(db)
            .await
            .ok();
    } else if let Some((data, _)) = embedded_art(sample_mp3) {
        let val = write_album_art(folder_path, id, &data);
        sqlx::query("UPDATE music_albums SET image_filename = ? WHERE id = ?")
            .bind(val)
            .bind(id)
            .execute(db)
            .await
            .ok();
    }
    id
}

/// Read embedded cover art from an audio file, returning (bytes, is_png).
pub(crate) fn embedded_art(path: &str) -> Option<(Vec<u8>, bool)> {
    use lofty::file::TaggedFileExt;
    use lofty::picture::Picture;
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic: &Picture = tag.pictures().first()?;
    let is_png = pic.mime_type() == Some(&lofty::picture::MimeType::Png);
    Some((pic.data().to_vec(), is_png))
}

/// MP3/flac/m4a tag metadata for the scanner and manual add.
pub(crate) struct AudioMeta {
    pub title: String,
    pub track_num: Option<String>,
    pub size_mb: Option<f64>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub disc: Option<u32>,
}

pub(crate) fn read_audio_meta(path: &str, use_disc_format: bool) -> AudioMeta {
    let fallback = |path: &str| AudioMeta {
        title: path.rsplit('/').next().unwrap_or("track").to_string(),
        track_num: None,
        size_mb: None,
        artist: None,
        album: None,
        disc: None,
    };
    let Ok(tagged) = lofty::read_from_path(path) else {
        return fallback(path);
    };
    use lofty::file::TaggedFileExt;
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return fallback(path);
    };
    use lofty::tag::ItemKey;
    let get = |key: ItemKey| {
        tag.get(&key)
            .and_then(|i| i.value().text())
            .map(|s| s.to_string())
    };

    let digits = |s: &str| -> String { s.chars().filter(|c| c.is_ascii_digit()).collect() };
    let mut track_num = "01".to_string();
    if let Some(trck) = get(ItemKey::TrackNumber) {
        let d = digits(trck.split('/').next().unwrap_or(""));
        if !d.is_empty() {
            track_num = format!("{:02}", d.parse::<u32>().unwrap_or(1));
        }
    }
    let mut disc = 0u32;
    if let Some(tpos) = get(ItemKey::DiscNumber) {
        let d = digits(tpos.split('/').next().unwrap_or(""));
        if !d.is_empty() {
            disc = d.parse().unwrap_or(0);
        }
    }
    let track_display = if use_disc_format && disc > 0 {
        format!("{disc}-{track_num}")
    } else {
        track_num
    };

    let raw_title = get(ItemKey::TrackTitle).unwrap_or_else(|| "Unknown Title".into());
    let title = clean_track_title(&raw_title);
    let size_mb = std::fs::metadata(path)
        .ok()
        .map(|m| ((m.len() as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0);

    AudioMeta {
        title,
        track_num: Some(track_display),
        size_mb,
        artist: get(ItemKey::TrackArtist).or_else(|| get(ItemKey::AlbumArtist)),
        album: get(ItemKey::AlbumTitle),
        disc: Some(disc),
    }
}

pub(crate) fn clean_track_title(title: &str) -> String {
    let patterns = [
        (r"^(\d{4}-\d{2}-\d{2})_(\d{1,2})-(\d{2})\s+(.+)$", 4),
        (r"^(\d{4}-\d{2}-\d{2})_(\d{2})\s+(.+)$", 3),
        (r"^(\d{1,2})-(\d{2})\s+(.+)$", 3),
        (r"^(\d{1,2})-(\d{2})-(.+)$", 3),
        (r"^(\d{1,2})\s+(.+)$", 2),
        (r"^(\d{1,2})\.(.+)$", 2),
        (r"^(\d{1,2})\.\s+(.+)$", 2),
        (r"^(\d{1,2})-(.+)$", 2),
    ];
    for (pat, group) in patterns {
        if let Ok(re) = regex::Regex::new(pat) {
            if let Some(caps) = re.captures(title) {
                if let Some(m) = caps.get(group) {
                    return m.as_str().to_string();
                }
            }
        }
    }
    title.to_string()
}

pub(crate) async fn create_track(
    db: &syarc_plugin_api::Db,
    album_id: i64,
    file_path: &str,
    meta: &AudioMeta,
    user_id: Option<i64>,
    is_public: bool,
) {
    let exists = sqlx::query("SELECT 1 FROM music_tracks WHERE file_path = ?")
        .bind(file_path)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_some() {
        return;
    }
    let _ = sqlx::query(
        "INSERT INTO music_tracks (title, file_path, track_number, size, album_id, user_id, is_public, file_size) \
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(&meta.title)
    .bind(file_path)
    .bind(&meta.track_num)
    .bind(meta.size_mb)
    .bind(album_id)
    .bind(user_id)
    .bind(is_public as i64)
    .bind(meta.size_mb)
    .execute(db)
    .await;
}

async fn add_single_album(
    db: &syarc_plugin_api::Db,
    user_id: i64,
    folder_path: &str,
    mp3s: &[String],
) -> Value {
    let first = read_audio_meta(&mp3s[0], false);
    let author_name_out = first
        .artist
        .clone()
        .unwrap_or_else(|| {
            let parent = folder_path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            parent
                .split_once(" - ")
                .map(|(a, _)| a.trim().to_string())
                .unwrap_or_else(|| "Unknown Artist".into())
        });
    let album_title = first.album.clone().unwrap_or_else(|| {
        let dir_name = folder_path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let lowered = dir_name.to_lowercase().replace(['_', '-'], "");
        if ["cd1", "cd2", "cd 1", "cd 2", "disc1", "disc2", "disc 1", "disc 2"]
            .iter()
            .any(|p| lowered.contains(p))
        {
            let mut parts: Vec<&str> = folder_path.trim_end_matches('/').split('/').collect();
            parts.pop();
            parts.pop().unwrap_or(dir_name).to_string()
        } else {
            dir_name
                .split_once(" - ")
                .map(|(_, b)| b.trim().to_string())
                .unwrap_or_else(|| dir_name.to_string())
        }
    });

    let author_id = get_or_create_author(db, &author_name_out).await;
    let album_id = get_or_create_album(db, author_id, &album_title, folder_path, &mp3s[0], Some(user_id), true).await;
    let mut added = 0;
    for mp3 in mp3s {
        let meta = read_audio_meta(mp3, false);
        create_track(db, album_id, mp3, &meta, Some(user_id), true).await;
        added += 1;
    }
    json!({
        "success": true,
        "message": format!("Album '{album_title}' by '{author_name_out}' added with {added} tracks"),
        "tracks_added": added,
        "album_id": album_id,
    })
}

async fn add_album_manual(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(raw): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let folder_path = fv(&form, "file_path").unwrap_or_default();
    if folder_path.is_empty() {
        return err400("Folder path is required");
    }
    if !std::path::Path::new(&folder_path).is_dir() {
        return err400(&format!("Directory not found: {folder_path}"));
    }

    let mp3s = find_mp3s(&folder_path);
    if !mp3s.is_empty() {
        let r = add_single_album(db, user.id, &folder_path, &mp3s).await;
        return axum::Json(r).into_response();
    }

    let mut album_dirs: Vec<(String, Vec<String>)> = Vec::new();
    let mut subdirs: Vec<String> = std::fs::read_dir(&folder_path)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.path().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    subdirs.sort();
    for subdir in subdirs {
        let mp3s = find_mp3s(&subdir);
        if !mp3s.is_empty() {
            album_dirs.push((subdir, mp3s));
        } else if let Ok(rd) = std::fs::read_dir(&subdir) {
            for sub2 in rd.flatten().filter(|e| e.path().is_dir()) {
                let p = sub2.path().to_string_lossy().to_string();
                let mp3s = find_mp3s(&p);
                if !mp3s.is_empty() {
                    album_dirs.push((p, mp3s));
                }
            }
        }
    }
    if album_dirs.is_empty() {
        return err400("No .mp3 files found in the specified directory or its subdirectories");
    }

    let mut results = Vec::new();
    let mut total_tracks = 0;
    for (dir, mp3s) in &album_dirs {
        let r = add_single_album(db, user.id, dir, mp3s).await;
        if r["success"].as_bool().unwrap_or(false) {
            total_tracks += r["tracks_added"].as_i64().unwrap_or(0);
        }
        results.push(r);
    }
    let added = results.iter().filter(|r| r["success"].as_bool().unwrap_or(false)).count();
    let skipped = results.len() - added;
    let message = format!(
        "Added {added} album(s) with {total_tracks} tracks{}",
        if skipped > 0 { format!(", {skipped} skipped") } else { String::new() }
    );
    axum::Json(json!({
        "success": true,
        "message": message,
        "album_id": results.first().and_then(|r| r["album_id"].as_i64()),
        "details": results,
    }))
    .into_response()
}

// -------------------------------------------------------------------- scan

async fn trigger_scan(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
) -> Response {
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scan_music(&db, None, false).await;
    });
    axum::Json(json!({"message": "Music scan started."})).into_response()
}

async fn trigger_clean(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
) -> Response {
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scanner::clean_music_database(&db).await;
    });
    axum::Json(json!({"message": "Music database clean started in background."})).into_response()
}

async fn force_scan(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
) -> Response {
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scan_music(&db, None, true).await;
    });
    axum::Json(json!({"message": "Music force scan started - tables truncated, rebuilding."})).into_response()
}

async fn scan_my_library(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let user_dir = format!("{}/{}", crate::root("music"), user.username);
    if !std::path::Path::new(&user_dir).is_dir() {
        return axum::Json(json!({"message": format!("Your library directory does not exist yet: {user_dir}")})).into_response();
    }
    let db = state.db.clone();
    let msg = format!("Scanning your library: {user_dir}");
    tokio::spawn(async move {
        crate::scan_music(&db, Some(user_dir), false).await;
    });
    axum::Json(json!({"message": msg})).into_response()
}

async fn static_art(Path(file): Path<String>) -> Response {
    let path = static_dir("music").join("art").join(file);
    serve_file(&path.to_string_lossy(), None).await
}

/// Sentinel stored in music_albums.image_filename when the art is a file in
/// the album's own directory in the library tree.
pub(crate) const ART_SIBLING: &str = "sibling";

/// Find an album art file inside the album's directory (cover.jpg etc.).
pub(crate) fn album_dir_art(directory_path: &str) -> Option<std::path::PathBuf> {
    if directory_path.is_empty() {
        return None;
    }
    for name in [
        "cover.jpg", "cover.jpeg", "cover.png", "cover.webp",
        "folder.jpg", "folder.png", "front.jpg", "art.jpg",
    ] {
        let p = std::path::Path::new(directory_path).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Serve an album's art from its directory, falling back to the legacy
/// static art store for pre-migration images.
async fn album_art(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(album_id): Path<i64>,
) -> Response {
    let db = &state.db;
    let row = sqlx::query("SELECT image_filename, directory_path FROM music_albums WHERE id = ?")
        .bind(album_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Album not found");
    };
    let image: Option<String> = row.try_get("image_filename").ok();
    let dir: String = row.try_get::<String,_>("directory_path").unwrap_or_default();
    if let Some(p) = album_dir_art(&dir) {
        return serve_file(&p.to_string_lossy(), None).await;
    }
    if let Some(f) = image.as_deref().filter(|f| *f != ART_SIBLING && !f.is_empty()) {
        let p = static_dir("music").join("art").join(f);
        if p.is_file() {
            return serve_file(&p.to_string_lossy(), None).await;
        }
    }
    err_json(axum::http::StatusCode::NOT_FOUND, "no art")
}

/// Write album art into the album's directory as cover.jpg. Returns the
/// image_filename value to store ("sibling", or a legacy filename when the
/// album has no directory and we fell back to the static store).
pub(crate) fn write_album_art(directory_path: &str, album_id: i64, data: &[u8]) -> String {
    if !directory_path.is_empty() {
        let p = std::path::Path::new(directory_path).join("cover.jpg");
        if std::fs::write(&p, data).is_ok() {
            return ART_SIBLING.to_string();
        }
    }
    let art_dir = static_dir("music").join("art");
    let _ = std::fs::create_dir_all(&art_dir);
    let name = format!("album_{album_id}.jpg");
    let _ = std::fs::write(art_dir.join(&name), data);
    name
}

/// Admin: (re)generate album art for every album. Art already present as a
/// file in the album directory is registered; albums without one get art
/// extracted from the first track that has embedded artwork, written into
/// the album directory so it is shared and survives rescans.
async fn admin_rebuild_art(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err_json(axum::http::StatusCode::FORBIDDEN, "admin access required");
    }
    let db = &state.db;
    let albums: Vec<(i64, String, Option<String>)> = sqlx::query(
        "SELECT id, directory_path, image_filename FROM music_albums WHERE media_type = 'music'",
    )
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                (
                    r.get::<i64,_>("id"),
                    r.try_get::<String,_>("directory_path").unwrap_or_default(),
                    r.try_get::<Option<String>,_>("image_filename").ok().flatten(),
                )
            })
            .collect()
    })
    .unwrap_or_default();
    let mut registered = 0i64;
    let mut extracted = 0i64;
    let mut kept = 0i64;
    for (id, dir, image) in &albums {
        if album_dir_art(dir).is_some() {
            if image.as_deref() != Some(ART_SIBLING) {
                let _ = sqlx::query("UPDATE music_albums SET image_filename = ? WHERE id = ?")
                    .bind(ART_SIBLING).bind(id).execute(db).await;
            }
            registered += 1;
            continue;
        }
        if image.as_deref() == Some(ART_SIBLING) {
            // Pointed at a sibling that is gone; fall through to extract.
        } else if image.as_deref().map(|f| !f.is_empty()).unwrap_or(false) {
            let legacy = static_dir("music").join("art").join(image.as_deref().unwrap_or(""));
            if legacy.is_file() {
                if let Ok(data) = std::fs::read(&legacy) {
                    let val = write_album_art(dir, *id, &data);
                    let _ = sqlx::query("UPDATE music_albums SET image_filename = ? WHERE id = ?")
                        .bind(val).bind(id).execute(db).await;
                    registered += 1;
                    continue;
                }
            }
        }
        // Extract embedded art from the first track that has one.
        let sample: Option<String> = sqlx::query(
            "SELECT file_path FROM music_tracks WHERE album_id = ? LIMIT 1",
        )
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<String,_>("file_path").ok());
        let mut done = false;
        if let Some(track) = sample {
            if let Some((data, _)) = embedded_art(&track) {
                let val = write_album_art(dir, *id, &data);
                let _ = sqlx::query("UPDATE music_albums SET image_filename = ? WHERE id = ?")
                    .bind(val).bind(id).execute(db).await;
                extracted += 1;
                done = true;
            }
        }
        if !done {
            kept += 1;
        }
    }
    axum::Json(json!({
        "message": format!(
            "Registered {registered} existing cover file(s), extracted {extracted} new cover(s); {kept} album(s) have no artwork available"
        )
    }))
    .into_response()
}

#[allow(dead_code)]
fn _unused(v: f64) -> String {
    human_size(v as i64)
}
