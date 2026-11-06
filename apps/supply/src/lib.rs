//! Supply app: multi-store inventory with nested categories, items with
//! barcodes/quantities/minimums, usage tracking, shortage reports, and a
//! barcode scanner workflow with local/online barcode lookup. Ported from
//! the Python (fastmain) implementation.

mod barcode;

use std::collections::HashMap;

use axum::extract::{Path, Query, RawForm, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

fn parse_ymd(s: &str) -> Option<chrono::NaiveDate> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(y, m, d)
}

pub struct SupplyPlugin;

impl SupplyPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("supply", "Supply", "/supply").icon("bi-box-seam");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for SupplyPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, _state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/supply", get(landing).post(landing))
            .route("/supply/", get(landing))
            .route("/supply/store/new", get(store_new).post(store_create))
            .route("/supply/store/{store_id}/edit", get(store_edit).post(store_update))
            .route("/supply/store/{store_id}/delete", post(store_delete))
            .route("/supply/store/{store_id}/share", post(store_share))
            .route("/supply/store/{store_id}/revoke/{share_id}", post(store_share_revoke))
            .route("/supply/stores/{store_id}", get(store_view))
            .route("/supply/search", get(search_page))
            .route("/supply/barcode-search", get(barcode_search_page))
            .route("/supply/categories/{category_id}/shortage", get(category_shortage))
            .route("/supply/categories/{category_id}", get(category_view))
            .route("/supply/categories/{category_id}/edit", get(category_edit).post(category_update))
            .route("/supply/category/new", get(category_new))
            .route("/supply/categories/new", post(category_create))
            .route("/supply/categories/{category_id}/delete", get(category_delete_page).post(category_delete_confirm))
            .route("/supply/categories/{category_id}/item-detail", get(item_detail))
            .route("/supply/categories/{category_id}/tracked", get(tracked_items))
            .route("/supply/items/new", get(item_new).post(item_create))
            .route("/supply/items/{item_id}/edit", get(item_edit).post(item_update))
            .route("/supply/items/{item_id}/move", get(item_move).post(item_move_post))
            .route("/supply/items/{item_id}/delete", post(item_delete))
            .route("/supply/items/{item_id}/track", get(item_track).post(item_track_post))
            .route("/supply/items/{item_id}/track/{track_id}/finish", post(track_finish))
            .route("/supply/items/{item_id}/track/{track_id}/delete", post(track_delete))
            .route("/supply/scanner", get(scanner_page))
            .route("/supply/print-form", get(print_form))
            .route("/supply/api/items/{item_id}/adjust", post(api_adjust))
            .route("/supply/api/items/{item_id}/lookup-barcode", post(api_lookup_item_barcode))
            .route("/supply/api/items/{item_id}/transfer", post(api_transfer))
            .route("/supply/api/barcode/lookup", post(api_barcode_lookup))
            .route("/supply/api/scanner/batch", post(api_scanner_batch))
            .route("/supply/api/scanner/batch-all", post(api_scanner_batch_all))
            .route("/supply/api/scanner/batch_upload", post(api_scanner_batch_upload))
            .route("/supply/api/scanner/locations", get(api_scanner_locations))
            .route("/supply/api/scanner/pending", get(api_scanner_pending))
            .route("/supply/api/scanner/pending/clear", post(api_scanner_pending_clear))
            .route("/supply/api/scanner/queue/add", post(api_scanner_queue_add))
            .route("/supply/api/scanner/queue/remove", post(api_scanner_queue_remove))
            .route("/supply/api/barcode/db-status", get(api_barcode_db_status))
            .route("/supply/api/barcode/reset-stats", post(api_barcode_reset_stats))
    }
}

// ------------------------------------------------------------------ helpers

fn page(state: &SharedState, template: &str, ctx: Value) -> Html<String> {
    Html(state.templates.render(template, ctx))
}

fn err_json(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"detail": msg}))).into_response()
}

fn form_map(raw: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn fv(form: &HashMap<String, String>, key: &str) -> Option<String> {
    form.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn fint(form: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    fv(form, key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[allow(dead_code)]
fn opt_str(v: Option<&str>) -> Value {
    match v {
        Some(s) if !s.is_empty() => json!(s),
        _ => Value::Null,
    }
}

async fn has_store_access(db: &sqlx::SqlitePool, store_id: i64, user_id: i64) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM supply_stores WHERE id = ? AND user_id = ? \
         UNION ALL SELECT 1 FROM supply_store_shares WHERE store_id = ? AND shared_with_user_id = ?",
    )
    .bind(store_id)
    .bind(user_id)
    .bind(store_id)
    .bind(user_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some()
}

fn store_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "name": r.try_get::<String, _>("name").unwrap_or_default(),
        "description": r.try_get::<Option<String>, _>("description").ok().flatten(),
    })
}

fn category_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "name": r.try_get::<String, _>("name").unwrap_or_default(),
        "description": r.try_get::<Option<String>, _>("description").ok().flatten(),
        "store_id": r.try_get::<Option<i64>, _>("store_id").ok().flatten(),
        "parent_id": r.try_get::<Option<i64>, _>("parent_id").ok().flatten(),
    })
}

fn item_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "key": r.try_get::<String, _>("key").unwrap_or_default(),
        "description": r.try_get::<Option<String>, _>("description").ok().flatten(),
        "barcode": r.try_get::<Option<String>, _>("barcode").ok().flatten(),
        "quantity": r.get::<i64, _>("quantity"),
        "minimum": r.try_get::<Option<i64>, _>("minimum").ok().flatten(),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "notes": r.try_get::<Option<String>, _>("notes").ok().flatten(),
        "store_id": r.try_get::<Option<i64>, _>("store_id").ok().flatten(),
        "category_id": r.get::<i64, _>("category_id"),
        "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
        "updated_at": r.try_get::<String, _>("updated_at").unwrap_or_default(),
    })
}

async fn load_category(db: &sqlx::SqlitePool, id: i64) -> Option<Value> {
    sqlx::query("SELECT * FROM supply_categories WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()?
        .map(|r| category_json(&r))
}

async fn load_item(db: &sqlx::SqlitePool, id: i64) -> Option<Value> {
    sqlx::query("SELECT * FROM supply_items WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()?
        .map(|r| item_json(&r))
}

/// All descendant category ids (including the given id).
async fn descendant_ids(db: &sqlx::SqlitePool, id: i64) -> Vec<i64> {
    let mut out = vec![id];
    let mut queue = vec![id];
    while let Some(cid) = queue.pop() {
        let kids: Vec<i64> = sqlx::query_scalar("SELECT id FROM supply_categories WHERE parent_id = ?")
            .bind(cid)
            .fetch_all(db)
            .await
            .unwrap_or_default();
        for k in kids {
            queue.push(k);
            out.push(k);
        }
    }
    out
}

async fn ancestor_chain(db: &sqlx::SqlitePool, mut parent_id: Option<i64>) -> Vec<Value> {
    let mut chain = Vec::new();
    while let Some(pid) = parent_id {
        let Some(cat) = load_category(db, pid).await else { break };
        parent_id = cat["parent_id"].as_i64();
        chain.push(cat);
    }
    chain.reverse();
    chain
}

async fn category_store_name(db: &sqlx::SqlitePool, store_id: Option<i64>) -> Value {
    match store_id {
        Some(sid) => sqlx::query("SELECT * FROM supply_stores WHERE id = ?")
            .bind(sid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .map(|r| json!(r.try_get::<String, _>("name").unwrap_or_default()))
            .unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// Guard: 403 unless the category is accessible (or category None -> 404).
#[allow(clippy::result_large_err)]
async fn check_category_access(db: &sqlx::SqlitePool, category: &Value, user_id: i64) -> Result<(), Response> {
    if let Some(sid) = category["store_id"].as_i64() {
        if !has_store_access(db, sid, user_id).await {
            return Err(err_json(StatusCode::FORBIDDEN, "Access denied"));
        }
    }
    Ok(())
}

fn qs_i64(params: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    params.get(key).and_then(|v| v.parse().ok()).unwrap_or(default).max(1)
}

// ------------------------------------------------------------------ store selection + CRUD

async fn landing(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let own = sqlx::query("SELECT * FROM supply_stores WHERE user_id = ? AND is_active = 1 ORDER BY name")
        .bind(user.0.id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let shared = sqlx::query(
        "SELECT s.*, sh.created_at AS shared_at, u.username AS owner_username \
         FROM supply_store_shares sh \
         JOIN supply_stores s ON s.id = sh.store_id \
         JOIN users u ON u.id = s.user_id \
         WHERE sh.shared_with_user_id = ? ORDER BY s.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut stores: Vec<Value> = Vec::new();
    for r in &own {
        let sid = r.get::<i64, _>("id");
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM supply_items WHERE store_id = ?",
        )
        .bind(sid)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        stores.push(json!({
            "store": store_json(r), "is_owner": true,
            "item_count": count, "shared_at": Value::Null,
        }));
    }
    for r in &shared {
        let sid = r.get::<i64, _>("id");
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supply_items WHERE store_id = ?")
            .bind(sid)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);
        stores.push(json!({
            "store": store_json(r), "is_owner": false,
            "item_count": count,
            "shared_at": r.try_get::<String, _>("shared_at").unwrap_or_default(),
            "owner_username": r.try_get::<String, _>("owner_username").unwrap_or_default(),
        }));
    }
    page(&state, "supply_store_select.html", json!({"stores": stores}))
}

async fn store_new(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let _ = user;
    page(&state, "supply_store_form.html", json!({"store": null, "mode": "new"}))
}

async fn store_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Redirect {
    let form = form_map(&raw);
    if fv(&form, "name").is_none() {
        return Redirect::to("/supply/store/new");
    }
    let _ = sqlx::query("INSERT INTO supply_stores (user_id, name, description) VALUES (?, ?, ?)")
        .bind(user.0.id)
        .bind(fv(&form, "name").unwrap_or_default())
        .bind(fv(&form, "description"))
        .execute(&state.db)
        .await;
    Redirect::to("/supply/")
}

async fn store_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(store_id): Path<i64>,
) -> Response {
    let store = sqlx::query("SELECT * FROM supply_stores WHERE id = ? AND user_id = ?")
        .bind(store_id)
        .bind(user.0.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(store) = store else {
        return Redirect::to("/supply/").into_response();
    };
    let shared_users = sqlx::query(
        "SELECT sh.id AS share_id, u.id AS user_id, u.username, u.email \
         FROM supply_store_shares sh JOIN users u ON u.id = sh.shared_with_user_id \
         WHERE sh.store_id = ?",
    )
    .bind(store_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        json!({
            "id": r.get::<i64, _>("share_id"),
            "shared_user": {
                "id": r.get::<i64, _>("user_id"),
                "username": r.try_get::<String, _>("username").unwrap_or_default(),
                "email": r.try_get::<String, _>("email").unwrap_or_default(),
            },
        })
    })
    .collect::<Vec<_>>();
    let shared_ids: Vec<i64> = shared_users
        .iter()
        .map(|s| s["shared_user"]["id"].as_i64().unwrap_or(0))
        .collect();
    let all_users = sqlx::query(
        "SELECT id, username, email FROM users WHERE is_active = 1 AND id != ? ORDER BY username AND username NOT LIKE 'mesh-%'",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter(|r| {
        let uid = r.get::<i64, _>("id");
        let listed = shared_ids.contains(&uid);
        if listed {
            // emulate NOT IN: user already shared
            return false;
        }
        true
    })
    .map(|r| {
        json!({
            "id": r.get::<i64, _>("id"),
            "username": r.try_get::<String, _>("username").unwrap_or_default(),
            "email": r.try_get::<String, _>("email").unwrap_or_default(),
        })
    })
    .collect::<Vec<_>>();
    let _ = shared_ids;
    page(
        &state,
        "supply_store_form.html",
        json!({
            "store": store_json(&store), "mode": "edit",
            "shared_users": shared_users, "all_users": all_users,
        }),
    )
    .into_response()
}

async fn store_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(store_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    if fv(&form, "name").is_none() {
        return Redirect::to(&format!("/supply/store/{store_id}/edit")).into_response();
    }
    let _ = sqlx::query(
        "UPDATE supply_stores SET name = ?, description = ?, updated_at = datetime('now') \
         WHERE id = ? AND user_id = ?",
    )
    .bind(fv(&form, "name").unwrap_or_default())
    .bind(fv(&form, "description"))
    .bind(store_id)
    .bind(user.0.id)
    .execute(&state.db)
    .await;
    Redirect::to(&format!("/supply/stores/{store_id}")).into_response()
}

async fn store_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(store_id): Path<i64>,
) -> Redirect {
    let _ = sqlx::query("DELETE FROM supply_stores WHERE id = ? AND user_id = ?")
        .bind(store_id)
        .bind(user.0.id)
        .execute(&state.db)
        .await;
    Redirect::to("/supply/")
}

async fn store_share(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(store_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM supply_stores WHERE id = ?")
        .bind(store_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.0.id) {
        return Redirect::to("/supply/").into_response();
    }
    let form = form_map(&raw);
    let target: Option<i64> = fv(&form, "share_with_user_id").and_then(|v| v.parse().ok());
    let Some(target) = target else {
        return Redirect::to(&format!("/supply/stores/{store_id}")).into_response();
    };
    if target == user.0.id {
        return Redirect::to(&format!("/supply/stores/{store_id}")).into_response();
    }
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO supply_store_shares (store_id, shared_with_user_id) VALUES (?, ?)",
    )
    .bind(store_id)
    .bind(target)
    .execute(&state.db)
    .await;
    Redirect::to(&format!("/supply/store/{store_id}/edit")).into_response()
}

async fn store_share_revoke(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((store_id, share_id)): Path<(i64, i64)>,
) -> Response {
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM supply_stores WHERE id = ?")
        .bind(store_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.0.id) {
        return Redirect::to("/supply/").into_response();
    }
    let _ = sqlx::query("DELETE FROM supply_store_shares WHERE id = ? AND store_id = ?")
        .bind(share_id)
        .bind(store_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/supply/store/{store_id}/edit")).into_response()
}

async fn store_view(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(store_id): Path<i64>,
) -> Response {
    let store = sqlx::query("SELECT * FROM supply_stores WHERE id = ?")
        .bind(store_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(store) = store else {
        return err_json(StatusCode::NOT_FOUND, "Store not found").into_response();
    };
    let is_owner = store.get::<i64, _>("user_id") == user.0.id;
    if !is_owner && !has_store_access(&state.db, store_id, user.0.id).await {
        return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let categories = sqlx::query(
        "SELECT * FROM supply_categories WHERE store_id = ? AND parent_id IS NULL ORDER BY name",
    )
    .bind(store_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let mut cats = Vec::new();
    for r in &categories {
        let cid = r.get::<i64, _>("id");
        let descendants = descendant_ids(&state.db, cid).await;
        let placeholders = descendants.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut q = format!(
            "SELECT COUNT(*) FROM supply_items WHERE category_id IN ({placeholders})"
        );
        let _ = &mut q;
        let count: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM supply_items WHERE category_id IN (SELECT id FROM supply_categories WHERE id = ? OR parent_id = ?)",
        )
        .bind(cid)
        .bind(cid)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        let _ = descendants;
        let mut c = category_json(r);
        c["item_count"] = json!(count);
        cats.push(c);
    }
    page(
        &state,
        "supply_landing.html",
        json!({
            "store": store_json(&store), "is_owner": is_owner,
            "categories": cats,
        }),
    )
    .into_response()
}

// ------------------------------------------------------------------ search

#[allow(dead_code)]
fn like_pattern(word: &str) -> Vec<String> {
    vec![format!("%{word}%")]
}

async fn search_page(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = params.get("q").cloned().unwrap_or_default().trim().to_string();
    let store_id: Option<i64> = params.get("store_id").and_then(|v| v.parse().ok());
    let page_num = qs_i64(&params, "page", 1);
    let per_page = params.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);

    let mut items: Vec<Value> = Vec::new();
    let mut total = 0i64;
    if !query.is_empty() {
        let words: Vec<String> = query.split_whitespace().map(|w| w.to_string()).collect();
        let mut cond = String::new();
        let mut binds: Vec<String> = Vec::new();
        for w in &words {
            for col in ["key", "description", "barcode", "notes", "location"] {
                if !cond.is_empty() {
                    cond.push_str(" OR ");
                }
                cond.push_str(&format!("i.{col} LIKE ? COLLATE NOCASE"));
                binds.push(format!("%{w}%"));
            }
        }
        let accessible: Vec<i64> = accessible_store_ids(&state.db, user.0.id).await;
        let in_stores = if accessible.is_empty() {
            " AND 1=0".to_string()
        } else {
            format!(
                " AND c.store_id IN ({})",
                accessible.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            )
        };
        let store_filter = if let Some(sid) = store_id {
            if !has_store_access(&state.db, sid, user.0.id).await {
                return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
            }
            " AND c.store_id = ?".to_string()
        } else {
            in_stores
        };
        let base = format!(
            "FROM supply_items i JOIN supply_categories c ON c.id = i.category_id \
             LEFT JOIN supply_stores s ON s.id = c.store_id WHERE ({cond}){store_filter}"
        );
        let count_sql = format!("SELECT COUNT(*) {base}");
        let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
        let list_sql = format!(
            "SELECT i.*, c.name AS category_name, c.store_id AS cat_store_id, s.name AS store_name {base} \
             ORDER BY i.key LIMIT ? OFFSET ?"
        );
        let mut list_q = sqlx::query(&list_sql);
        for b in &binds {
            count_q = count_q.bind(b);
            list_q = list_q.bind(b);
        }
        if let Some(sid) = store_id {
            count_q = count_q.bind(sid);
            list_q = list_q.bind(sid);
        } else {
            for sid in &accessible {
                count_q = count_q.bind(sid);
                list_q = list_q.bind(sid);
            }
        }
        total = count_q.fetch_one(&state.db).await.unwrap_or(0);
        let list_q = list_q.bind(per_page).bind((page_num - 1) * per_page);
        let rows = list_q.fetch_all(&state.db).await.unwrap_or_default();
        for r in &rows {
            items.push(json!({
                "item": item_json(r),
                "category": {"id": r.get::<i64, _>("category_id"), "name": r.try_get::<String, _>("category_name").unwrap_or_default()},
                "store": {
                    "id": r.try_get::<Option<i64>, _>("cat_store_id").ok().flatten(),
                    "name": r.try_get::<Option<String>, _>("store_name").ok().flatten(),
                },
            }));
        }
    }
    let store = match store_id {
        Some(sid) => sqlx::query("SELECT * FROM supply_stores WHERE id = ?")
            .bind(sid)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .map(|r| store_json(&r))
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    page(
        &state,
        "supply_search.html",
        json!({
            "query": query, "items": items,
            "store_id": store_id, "store": store,
            "page": page_num, "per_page": per_page, "total": total,
            "total_pages": if total > 0 { (total + per_page - 1) / per_page } else { 0 },
            "current_path": current_path("/supply/search", &params),
        }),
    )
    .into_response()
}

fn current_path(base: &str, params: &HashMap<String, String>) -> String {
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect();
    if qs.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", qs.join("&"))
    }
}

fn urlencode(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn accessible_store_ids(db: &sqlx::SqlitePool, user_id: i64) -> Vec<i64> {
    let mut ids = sqlx::query_scalar::<_, i64>("SELECT id FROM supply_stores WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    ids.extend(
        sqlx::query_scalar::<_, i64>("SELECT store_id FROM supply_store_shares WHERE shared_with_user_id = ?")
            .bind(user_id)
            .fetch_all(db)
            .await
            .unwrap_or_default(),
    );
    ids
}

async fn barcode_search_page(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let barcode = params.get("barcode").cloned().unwrap_or_default().trim().to_string();
    let store_id: Option<i64> = params.get("store_id").and_then(|v| v.parse().ok());
    let page_num = qs_i64(&params, "page", 1);
    let per_page = params.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);
    let mut items: Vec<Value> = Vec::new();
    let searched = !barcode.is_empty();
    if searched {
        let accessible = accessible_store_ids(&state.db, user.0.id).await;
        let store_filter = if let Some(sid) = store_id {
            if !has_store_access(&state.db, sid, user.0.id).await {
                return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
            }
            " AND c.store_id = ?".to_string()
        } else if accessible.is_empty() {
            " AND 1=0".to_string()
        } else {
            format!(
                " AND c.store_id IN ({})",
                accessible.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            )
        };
        let sql = format!(
            "SELECT i.*, c.name AS category_name, s.id AS st_id, s.name AS store_name \
             FROM supply_items i \
             JOIN supply_categories c ON c.id = i.category_id \
             JOIN supply_stores s ON s.id = c.store_id \
             WHERE i.barcode = ?{store_filter} ORDER BY s.name, i.key, i.location"
        );
        let mut q = sqlx::query(&sql).bind(&barcode);
        if let Some(sid) = store_id {
            q = q.bind(sid);
        } else {
            for sid in &accessible {
                q = q.bind(sid);
            }
        }
        for r in q.fetch_all(&state.db).await.unwrap_or_default() {
            items.push(json!({
                "item": item_json(&r),
                "category": {"id": r.get::<i64, _>("category_id"), "name": r.try_get::<String, _>("category_name").unwrap_or_default()},
                "store": {"id": r.get::<i64, _>("st_id"), "name": r.try_get::<String, _>("store_name").unwrap_or_default()},
            }));
        }
    }
    let total = items.len() as i64;
    let offset = ((page_num - 1) * per_page) as usize;
    let items: Vec<Value> = items.into_iter().skip(offset).take(per_page as usize).collect();
    let store = match store_id {
        Some(sid) => sqlx::query("SELECT * FROM supply_stores WHERE id = ?")
            .bind(sid)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .map(|r| store_json(&r))
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    page(
        &state,
        "supply_barcode_search.html",
        json!({
            "barcode": barcode, "items": items, "searched": searched,
            "store_id": store_id, "store": store,
            "page": page_num, "per_page": per_page, "total": total,
            "total_pages": if total > 0 { (total + per_page - 1) / per_page } else { 0 },
            "current_path": current_path("/supply/barcode-search", &params),
        }),
    )
    .into_response()
}

// ------------------------------------------------------------------ categories

async fn category_shortage(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
) -> Response {
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let subs = descendant_ids(&state.db, category_id).await;
    let placeholders = subs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT * FROM supply_items \
         WHERE category_id IN ({placeholders}) AND minimum IS NOT NULL AND minimum > 0 \
         AND quantity < minimum ORDER BY (minimum - quantity) DESC"
    );
    let mut q = sqlx::query(&sql);
    for s in &subs {
        q = q.bind(s);
    }
    let mut shortage_items = Vec::new();
    let mut total_shortage = 0i64;
    for r in q.fetch_all(&state.db).await.unwrap_or_default() {
        let item = item_json(&r);
        let short = item["minimum"].as_i64().unwrap_or(0) - item["quantity"].as_i64().unwrap_or(0);
        total_shortage += short;
        shortage_items.push(json!({"item": item, "shortage": short}));
    }
    let store_name = category_store_name(&state.db, category["store_id"].as_i64()).await;
    page(
        &state,
        "supply_shortage.html",
        json!({
            "category": category, "store": {"id": category["store_id"], "name": store_name},
            "shortage_items": shortage_items, "total_shortage": total_shortage,
        }),
    )
    .into_response()
}

async fn category_view(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let per_page = params
        .get("per_page")
        .and_then(|v| v.parse::<i64>().ok())
        .map(|v| v.clamp(1, 200))
        .unwrap_or(25);
    let page_num = qs_i64(&params, "page", 1);
    let search = params.get("search").cloned().unwrap_or_default().trim().to_string();

    let children = sqlx::query("SELECT * FROM supply_categories WHERE parent_id = ? ORDER BY name")
        .bind(category_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| category_json(&r))
        .collect::<Vec<_>>();

    let descendants = descendant_ids(&state.db, category_id).await;
    let placeholders = descendants.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut cond = String::new();
    let mut binds: Vec<String> = Vec::new();
    if !search.is_empty() {
        let pat = format!("%{search}%");
        for col in ["key", "description", "barcode", "location", "notes"] {
            if !cond.is_empty() {
                cond.push_str(" OR ");
            }
            cond.push_str(&format!("{col} LIKE ? COLLATE NOCASE"));
            binds.push(pat.clone());
        }
        cond = format!(" AND ({cond})");
    }
    let count_sql = format!(
        "SELECT COUNT(*) FROM supply_items WHERE category_id IN ({placeholders}){cond}"
    );
    let list_sql = format!(
        "SELECT * FROM supply_items WHERE category_id IN ({placeholders}){cond} ORDER BY key LIMIT ? OFFSET ?"
    );
    let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
    let mut list_q = sqlx::query(&list_sql);
    for d in &descendants {
        count_q = count_q.bind(d);
        list_q = list_q.bind(d);
    }
    for b in &binds {
        count_q = count_q.bind(b);
        list_q = list_q.bind(b);
    }
    let total = count_q.fetch_one(&state.db).await.unwrap_or(0);
    let rows = list_q
        .bind(per_page)
        .bind((page_num - 1) * per_page)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let items: Vec<Value> = rows.iter().map(item_json).collect();

    let chain = ancestor_chain(&state.db, category["parent_id"].as_i64()).await;

    let cat_names_sql = format!(
        "SELECT id, name FROM supply_categories WHERE id IN ({placeholders})"
    );
    let mut cat_names_q = sqlx::query(&cat_names_sql);
    for d in &descendants {
        cat_names_q = cat_names_q.bind(d);
    }
    let mut category_names = json!({});
    for r in cat_names_q.fetch_all(&state.db).await.unwrap_or_default() {
        category_names[r.get::<i64, _>("id").to_string()] =
            json!(r.try_get::<String, _>("name").unwrap_or_default());
    }

    let key_counts_sql = format!(
        "SELECT key, COUNT(*) AS n FROM supply_items WHERE category_id IN ({placeholders}){cond} GROUP BY key"
    );
    let mut key_counts_q = sqlx::query(&key_counts_sql);
    for d in &descendants {
        key_counts_q = key_counts_q.bind(d);
    }
    for b in &binds {
        key_counts_q = key_counts_q.bind(b);
    }
    let mut key_counts = json!({});
    for r in key_counts_q.fetch_all(&state.db).await.unwrap_or_default() {
        key_counts[r.try_get::<String, _>("key").unwrap_or_default()] =
            json!(r.get::<i64, _>("n"));
    }

    let store_name = category_store_name(&state.db, category["store_id"].as_i64()).await;
    page(
        &state,
        "supply_category.html",
        json!({
            "category": category, "items": items, "children": children,
            "store_id": category["store_id"], "store_name": store_name,
            "ancestor_chain": chain, "category_names": category_names,
            "key_counts": key_counts,
            "page": page_num, "per_page": per_page, "total": total,
            "search": search,
            "current_path": current_path(&format!("/supply/categories/{category_id}"), &params),
            "total_pages": std::cmp::max(1, (total + per_page - 1) / per_page),
        }),
    )
    .into_response()
}

async fn category_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
) -> Response {
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let all = all_categories_except(&state.db, category_id, category["store_id"].as_i64()).await;
    let chain = ancestor_chain(&state.db, category["parent_id"].as_i64()).await;
    page(
        &state,
        "supply_category_form.html",
        json!({
            "category": category, "all_categories": all, "mode": "edit",
            "store_id": category["store_id"], "ancestor_chain": chain,
        }),
    )
    .into_response()
}

async fn all_categories_except(db: &sqlx::SqlitePool, exclude: i64, store_id: Option<i64>) -> Vec<Value> {
    let rows = match store_id {
        Some(sid) => {
            sqlx::query("SELECT * FROM supply_categories WHERE id != ? AND store_id = ? ORDER BY name")
                .bind(exclude)
                .bind(sid)
                .fetch_all(db)
                .await
        }
        None => {
            sqlx::query("SELECT * FROM supply_categories WHERE id != ? ORDER BY name")
                .bind(exclude)
                .fetch_all(db)
                .await
        }
    };
    rows.unwrap_or_default().iter().map(category_json).collect()
}

async fn category_new(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let store_id: Option<i64> = params.get("store_id").and_then(|v| v.parse().ok());
    let parent_id: Option<i64> = params.get("parent_id").and_then(|v| v.parse().ok());
    if let Some(sid) = store_id {
        if !has_store_access(&state.db, sid, user.0.id).await {
            return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
        }
    }
    let all = match store_id {
        Some(sid) => sqlx::query("SELECT * FROM supply_categories WHERE store_id = ? ORDER BY name")
            .bind(sid)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default(),
        None => sqlx::query("SELECT * FROM supply_categories ORDER BY name")
            .fetch_all(&state.db)
            .await
            .unwrap_or_default(),
    };
    let chain = ancestor_chain(&state.db, parent_id).await;
    page(
        &state,
        "supply_category_form.html",
        json!({
            "category": null,
            "all_categories": all.iter().map(category_json).collect::<Vec<_>>(),
            "mode": "new", "parent_id": parent_id, "store_id": store_id,
            "ancestor_chain": chain,
        }),
    )
    .into_response()
}

async fn category_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return Redirect::to("/supply/category/new").into_response();
    };
    let store_id: Option<i64> = fv(&form, "store_id").and_then(|v| v.parse().ok());
    if let Some(sid) = store_id {
        if !has_store_access(&state.db, sid, user.0.id).await {
            return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
        }
    }
    let parent_id: Option<i64> = fv(&form, "parent_id").and_then(|v| v.parse().ok());
    let _ = sqlx::query(
        "INSERT INTO supply_categories (name, description, store_id, parent_id) VALUES (?, ?, ?, ?)",
    )
    .bind(&name)
    .bind(fv(&form, "description"))
    .bind(store_id)
    .bind(parent_id)
    .execute(&state.db)
    .await;
    if let Some(pid) = parent_id {
        return Redirect::to(&format!("/supply/categories/{pid}")).into_response();
    }
    if let Some(sid) = store_id {
        return Redirect::to(&format!("/supply/stores/{sid}")).into_response();
    }
    Redirect::to("/supply/").into_response()
}

async fn category_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return Redirect::to(&format!("/supply/categories/{category_id}/edit")).into_response();
    };
    let new_parent: Option<i64> = fv(&form, "parent_id").and_then(|v| v.parse().ok());
    if new_parent == Some(category_id) {
        return Redirect::to(&format!("/supply/categories/{category_id}/edit")).into_response();
    }
    let _ = sqlx::query(
        "UPDATE supply_categories SET name = ?, description = ?, parent_id = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(&name)
    .bind(fv(&form, "description"))
    .bind(new_parent)
    .bind(category_id)
    .execute(&state.db)
    .await;
    if let Some(pid) = new_parent {
        return Redirect::to(&format!("/supply/categories/{pid}")).into_response();
    }
    if let Some(sid) = category["store_id"].as_i64() {
        return Redirect::to(&format!("/supply/stores/{sid}")).into_response();
    }
    Redirect::to("/supply/").into_response()
}

async fn category_delete_page(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
) -> Response {
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let item_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supply_items WHERE category_id = ?")
        .bind(category_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let child_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supply_categories WHERE parent_id = ?")
        .bind(category_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    if item_count == 0 && child_count == 0 {
        let _ = sqlx::query("DELETE FROM supply_categories WHERE id = ?")
            .bind(category_id)
            .execute(&state.db)
            .await;
        if let Some(pid) = category["parent_id"].as_i64() {
            return Redirect::to(&format!("/supply/categories/{pid}")).into_response();
        }
        if let Some(sid) = category["store_id"].as_i64() {
            return Redirect::to(&format!("/supply/stores/{sid}")).into_response();
        }
        return Redirect::to("/supply/").into_response();
    }
    let others = all_categories_except(&state.db, category_id, category["store_id"].as_i64()).await;
    page(
        &state,
        "supply_category_delete.html",
        json!({
            "category": category, "item_count": item_count, "child_count": child_count,
            "other_categories": others, "store_id": category["store_id"],
        }),
    )
    .into_response()
}

async fn category_delete_confirm(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let form = form_map(&raw);
    let Some(move_to) = fv(&form, "move_to_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to(&format!("/supply/categories/{category_id}/delete")).into_response();
    };
    if move_to == category_id {
        return Redirect::to(&format!("/supply/categories/{category_id}/delete")).into_response();
    }
    let _ = sqlx::query("UPDATE supply_items SET category_id = ? WHERE category_id = ?")
        .bind(move_to)
        .bind(category_id)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("UPDATE supply_categories SET parent_id = ? WHERE parent_id = ?")
        .bind(move_to)
        .bind(category_id)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("DELETE FROM supply_categories WHERE id = ?")
        .bind(category_id)
        .execute(&state.db)
        .await;
    if let Some(pid) = category["parent_id"].as_i64() {
        return Redirect::to(&format!("/supply/categories/{pid}")).into_response();
    }
    if let Some(sid) = category["store_id"].as_i64() {
        return Redirect::to(&format!("/supply/stores/{sid}")).into_response();
    }
    Redirect::to("/supply/").into_response()
}

// ------------------------------------------------------------------ item detail + forms

async fn item_detail(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let key = params.get("key").cloned().unwrap_or_default();
    let descendants = descendant_ids(&state.db, category_id).await;
    let placeholders = descendants.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT * FROM supply_items WHERE category_id IN ({placeholders}) AND key = ? \
         ORDER BY location, created_at"
    );
    let mut q = sqlx::query(&sql);
    for d in &descendants {
        q = q.bind(d);
    }
    let rows = q.bind(&key).fetch_all(&state.db).await.unwrap_or_default();
    let items: Vec<Value> = rows.iter().map(item_json).collect();
    let description = items
        .iter()
        .find_map(|i| i["description"].as_str().map(|s| s.to_string()));
    let total_qty: i64 = items.iter().map(|i| i["quantity"].as_i64().unwrap_or(0)).sum();
    let chain = ancestor_chain(&state.db, category["parent_id"].as_i64()).await;
    let store_name = category_store_name(&state.db, category["store_id"].as_i64()).await;
    let next_url = safe_next(params.get("next")).unwrap_or_else(|| {
        let base = format!("/supply/categories/{category_id}");
        let keep: HashMap<String, String> = [
            ("page".to_string(), params.get("page").cloned().unwrap_or_default()),
            ("per_page".to_string(), params.get("per_page").cloned().unwrap_or_default()),
            ("search".to_string(), params.get("search").cloned().unwrap_or_default()),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect();
        current_path(&base, &keep)
    });
    page(
        &state,
        "supply_item_detail.html",
        json!({
            "category": category, "item_key": key, "items": items,
            "description": description, "total_qty": total_qty,
            "store_id": category["store_id"], "store_name": store_name,
            "ancestor_chain": chain,
            "next_url": next_url,
        }),
    )
    .into_response()
}

async fn item_new(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let category_id: Option<i64> = params.get("category_id").and_then(|v| v.parse().ok());
    let store_id: Option<i64> = params.get("store_id").and_then(|v| v.parse().ok());
    if let Some(sid) = store_id {
        if !has_store_access(&state.db, sid, user.0.id).await {
            return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
        }
    }
    let cats = store_categories(&state.db, store_id).await;
    page(
        &state,
        "supply_item_form.html",
        json!({
            "item": null, "categories": cats, "selected_category_id": category_id,
            "mode": "new", "store_id": store_id,
        }),
    )
    .into_response()
}

async fn store_categories(db: &sqlx::SqlitePool, store_id: Option<i64>) -> Vec<Value> {
    let rows = match store_id {
        Some(sid) => sqlx::query("SELECT * FROM supply_categories WHERE store_id = ? ORDER BY name")
            .bind(sid)
            .fetch_all(db)
            .await
            .unwrap_or_default(),
        None => sqlx::query("SELECT * FROM supply_categories ORDER BY name")
            .fetch_all(db)
            .await
            .unwrap_or_default(),
    };
    rows.iter().map(category_json).collect()
}

fn safe_next(target: Option<&String>) -> Option<String> {
    let t = target?;
    if t.starts_with('/') && !t.starts_with("//") {
        Some(t.clone())
    } else {
        None
    }
}

async fn item_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let category = load_category(&state.db, item["category_id"].as_i64().unwrap_or(0)).await;
    let mut store_id = None;
    if let Some(ref cat) = category {
        if let Some(sid) = cat["store_id"].as_i64() {
            if !has_store_access(&state.db, sid, user.0.id).await {
                return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
            }
            store_id = Some(sid);
        }
    }
    let cats = store_categories(&state.db, store_id).await;
    page(
        &state,
        "supply_item_form.html",
        json!({
            "item": item, "categories": cats, "selected_category_id": Value::Null,
            "mode": "edit", "store_id": store_id,
            "next_url": safe_next(params.get("next")),
        }),
    )
    .into_response()
}

async fn item_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(key) = fv(&form, "key") else {
        return Redirect::to("/supply/items/new").into_response();
    };
    let Some(cat_id) = fv(&form, "category_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/supply/items/new").into_response();
    };
    let Some(category) = load_category(&state.db, cat_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &category, user.0.id).await {
        return r;
    }
    let _ = sqlx::query(
        "INSERT INTO supply_items (key, description, barcode, quantity, minimum, location, notes, category_id, store_id, created_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key)
    .bind(fv(&form, "description"))
    .bind(fv(&form, "barcode"))
    .bind(fint(&form, "quantity", 0))
    .bind(match fint(&form, "minimum", 0) { 0 => None, m => Some(m) })
    .bind(fv(&form, "location"))
    .bind(fv(&form, "notes"))
    .bind(cat_id)
    .bind(category["store_id"].as_i64())
    .bind(user.0.id)
    .execute(&state.db)
    .await;
    Redirect::to(&format!("/supply/categories/{cat_id}")).into_response()
}

async fn item_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let form = form_map(&raw);
    let Some(key) = fv(&form, "key") else {
        return Redirect::to(&format!("/supply/items/{item_id}/edit")).into_response();
    };
    let Some(cat_id) = fv(&form, "category_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to(&format!("/supply/items/{item_id}/edit")).into_response();
    };
    let new_category = load_category(&state.db, cat_id).await;
    if let Some(ref cat) = new_category {
        if let Err(r) = check_category_access(&state.db, cat, user.0.id).await {
            return r;
        }
    }
    let new_store = new_category
        .as_ref()
        .and_then(|c| c["store_id"].as_i64())
        .or(item["store_id"].as_i64());
    let _ = sqlx::query(
        "UPDATE supply_items SET key = ?, description = ?, barcode = ?, quantity = ?, minimum = ?, \
         location = ?, notes = ?, category_id = ?, store_id = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(&key)
    .bind(fv(&form, "description"))
    .bind(fv(&form, "barcode"))
    .bind(fint(&form, "quantity", 0))
    .bind(match fint(&form, "minimum", 0) { 0 => None, m => Some(m) })
    .bind(fv(&form, "location"))
    .bind(fv(&form, "notes"))
    .bind(cat_id)
    .bind(new_store)
    .bind(item_id)
    .execute(&state.db)
    .await;
    let next = safe_next(form.get("next"));
    Redirect::to(&next.unwrap_or_else(|| format!("/supply/categories/{cat_id}"))).into_response()
}

async fn item_move(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
) -> Response {
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let category = load_category(&state.db, item["category_id"].as_i64().unwrap_or(0)).await;
    let mut store_id = None;
    if let Some(ref cat) = category {
        if let Some(sid) = cat["store_id"].as_i64() {
            if !has_store_access(&state.db, sid, user.0.id).await {
                return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
            }
            store_id = Some(sid);
        }
    }
    let cats = all_categories_except(&state.db, item["category_id"].as_i64().unwrap_or(0), store_id).await;
    page(
        &state,
        "supply_item_move.html",
        json!({"item": item, "categories": cats, "store_id": store_id}),
    )
    .into_response()
}

async fn item_move_post(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let _ = user;
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let form = form_map(&raw);
    let Some(target) = fv(&form, "target_category_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to(&format!("/supply/items/{item_id}/move")).into_response();
    };
    if Some(target) == item["category_id"].as_i64() {
        return Redirect::to(&format!("/supply/items/{item_id}/move")).into_response();
    }
    let quantity = fv(&form, "quantity")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(item["quantity"].as_i64().unwrap_or(0));
    if quantity <= 0 || quantity > item["quantity"].as_i64().unwrap_or(0) {
        return Redirect::to(&format!("/supply/items/{item_id}/move")).into_response();
    }
    let target_cat = load_category(&state.db, target).await;
    if quantity >= item["quantity"].as_i64().unwrap_or(0) {
        let _ = sqlx::query(
            "UPDATE supply_items SET category_id = ?, store_id = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(target)
        .bind(target_cat.as_ref().and_then(|c| c["store_id"].as_i64()))
        .bind(item_id)
        .execute(&state.db)
        .await;
    } else {
        let _ = sqlx::query(
            "UPDATE supply_items SET quantity = quantity - ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(quantity)
        .bind(item_id)
        .execute(&state.db)
        .await;
        let _ = sqlx::query(
            "INSERT INTO supply_items (key, description, barcode, quantity, location, notes, category_id, store_id, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(item["key"].as_str())
        .bind(item["description"].as_str())
        .bind(item["barcode"].as_str())
        .bind(quantity)
        .bind(item["location"].as_str())
        .bind(item["notes"].as_str())
        .bind(target)
        .bind(target_cat.as_ref().and_then(|c| c["store_id"].as_i64()).or(item["store_id"].as_i64()))
        .bind(item["created_by"].as_i64().unwrap_or(user.0.id))
        .execute(&state.db)
        .await;
    }
    Redirect::to(&format!("/supply/categories/{target}")).into_response()
}

async fn item_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    let _ = user;
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let _ = sqlx::query("DELETE FROM supply_items WHERE id = ?")
        .bind(item_id)
        .execute(&state.db)
        .await;
    // If the delete came from a search/list view, return to it so the
    // refreshed results are shown instead of the item's category page.
    let fallback = format!("/supply/categories/{}", item["category_id"].as_i64().unwrap_or(0));
    let form_next = fv(&form_map(&body), "next").map(|v| v.to_string());
    let next = safe_next(params.get("next"))
        .or_else(|| safe_next(form_next.as_ref()))
        .unwrap_or(fallback);
    Redirect::to(&next).into_response()
}

// ------------------------------------------------------------------ usage tracking

async fn item_track(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let _ = user;
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let page_num = qs_i64(&params, "page", 1);
    let per_page = params.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);
    let total = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supply_item_tracks WHERE item_id = ?")
        .bind(item_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let rows = sqlx::query(
        "SELECT * FROM supply_item_tracks WHERE item_id = ? ORDER BY open_date DESC LIMIT ? OFFSET ?",
    )
    .bind(item_id)
    .bind(per_page)
    .bind((page_num - 1) * per_page)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let tracks: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "open_date": r.try_get::<String, _>("open_date").unwrap_or_default(),
                "finished_date": r.try_get::<Option<String>, _>("finished_date").ok().flatten(),
                "notes": r.try_get::<Option<String>, _>("notes").ok().flatten(),
            })
        })
        .collect();
    let mut avg_days = Value::Null;
    let finished: Vec<&Value> = tracks.iter().filter(|t| !t["finished_date"].is_null()).collect();
    if finished.len() >= 2 {
        let sum: i64 = finished
            .iter()
            .filter_map(|t| {
                let o = chrono::NaiveDate::parse_from_str(t["open_date"].as_str()?, "%Y-%m-%d").ok()?;
                let f = parse_ymd(t["finished_date"].as_str()?)?;
                Some((f - o).num_days())
            })
            .sum();
        avg_days = json!((sum as f64 / finished.len() as f64 * 10.0).round() / 10.0);
    }
    let category = load_category(&state.db, item["category_id"].as_i64().unwrap_or(0)).await;
    let store_id = category.as_ref().and_then(|c| c["store_id"].as_i64());
    page(
        &state,
        "supply_item_track.html",
        json!({
            "item": item, "tracks": tracks, "store_id": store_id,
            "today": chrono::Local::now().format("%Y-%m-%d").to_string(),
            "avg_days": avg_days,
            "page": page_num, "per_page": per_page, "total": total,
            "total_pages": (total + per_page - 1) / per_page,
        }),
    )
    .into_response()
}

async fn item_track_post(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let _ = user;
    let Some(_) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let form = form_map(&raw);
    let Some(open_date) = fv(&form, "open_date") else {
        return Redirect::to(&format!("/supply/items/{item_id}/track")).into_response();
    };
    let _ = sqlx::query(
        "INSERT INTO supply_item_tracks (item_id, open_date, finished_date, notes) VALUES (?, ?, ?, ?)",
    )
    .bind(item_id)
    .bind(&open_date)
    .bind(fv(&form, "finished_date"))
    .bind(fv(&form, "notes"))
    .execute(&state.db)
    .await;
    Redirect::to(&format!("/supply/items/{item_id}/track")).into_response()
}

async fn track_finish(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((item_id, track_id)): Path<(i64, i64)>,
    RawForm(raw): RawForm,
) -> Response {
    let _ = user;
    let form = form_map(&raw);
    let finished = fv(&form, "finished_date")
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let _ = sqlx::query(
        "UPDATE supply_item_tracks SET finished_date = ? WHERE id = ? AND item_id = ?",
    )
    .bind(&finished)
    .bind(track_id)
    .bind(item_id)
    .execute(&state.db)
    .await;
    Redirect::to(&format!("/supply/items/{item_id}/track")).into_response()
}

async fn track_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path((item_id, track_id)): Path<(i64, i64)>,
) -> Response {
    let _ = user;
    let _ = sqlx::query("DELETE FROM supply_item_tracks WHERE id = ? AND item_id = ?")
        .bind(track_id)
        .bind(item_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/supply/items/{item_id}/track")).into_response()
}

async fn tracked_items(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(category_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let _ = user;
    let Some(category) = load_category(&state.db, category_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    let page_num = qs_i64(&params, "page", 1);
    let per_page = params.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);
    let today = chrono::Local::now().date_naive();
    let total = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supply_items WHERE category_id = ?")
        .bind(category_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let items = sqlx::query(
        "SELECT * FROM supply_items WHERE category_id = ? ORDER BY key LIMIT ? OFFSET ?",
    )
    .bind(category_id)
    .bind(per_page)
    .bind((page_num - 1) * per_page)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let mut tracked = Vec::new();
    for r in &items {
        let item_id = r.get::<i64, _>("id");
        let trows = sqlx::query(
            "SELECT * FROM supply_item_tracks WHERE item_id = ? ORDER BY open_date DESC",
        )
        .bind(item_id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        if trows.is_empty() {
            continue;
        }
        let tracks: Vec<Value> = trows
            .iter()
            .map(|t| {
                json!({
                    "id": t.get::<i64, _>("id"),
                    "open_date": t.try_get::<String, _>("open_date").unwrap_or_default(),
                    "finished_date": t.try_get::<Option<String>, _>("finished_date").ok().flatten(),
                    "notes": t.try_get::<Option<String>, _>("notes").ok().flatten(),
                })
            })
            .collect();
        let finished: Vec<&Value> = tracks.iter().filter(|t| !t["finished_date"].is_null()).collect();
        let active: Vec<&Value> = tracks.iter().filter(|t| t["finished_date"].is_null()).collect();
        let mut avg_days = Value::Null;
        let mut avg_f: Option<f64> = None;
        if finished.len() >= 2 {
            let sum: i64 = finished
                .iter()
                .filter_map(|t| {
                    let o = parse_ymd(t["open_date"].as_str()?)?;
                    let f = parse_ymd(t["finished_date"].as_str()?)?;
                    Some((f - o).num_days())
                })
                .sum();
            avg_f = Some(sum as f64 / finished.len() as f64);
            avg_days = json!(avg_f.map(|v| v.round() as i64));
        }
        let mut projected = Value::Null;
        if let (Some(avg), Some(latest)) = (avg_f, active.first()) {
            if let Some(open) = latest["open_date"].as_str().and_then(parse_ymd) {
                projected = json!((avg - (today - open).num_days() as f64).round() as i64);
            }
        }
        tracked.push(json!({
            "item": item_json(r), "tracks": tracks,
            "avg_days": avg_days, "projected_remaining": projected,
        }));
    }
    page(
        &state,
        "supply_tracked_items.html",
        json!({
            "category": category, "store_id": category["store_id"],
            "tracked_items": tracked, "today": today.format("%Y-%m-%d").to_string(),
            "page": page_num, "per_page": per_page, "total": total,
            "total_pages": (total + per_page - 1) / per_page,
        }),
    )
    .into_response()
}

// ------------------------------------------------------------------ JSON APIs

async fn api_adjust(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    body: String,
) -> Response {
    let _ = user;
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let adjustment = v.get("adjustment").and_then(|a| a.as_i64()).unwrap_or(0);
    let _ = sqlx::query(
        "UPDATE supply_items SET quantity = MAX(0, quantity + ?), updated_at = datetime('now') WHERE id = ?",
    )
    .bind(adjustment)
    .bind(item_id)
    .execute(&state.db)
    .await;
    axum::Json(json!({
        "id": item_id,
        "key": item["key"],
        "quantity": (item["quantity"].as_i64().unwrap_or(0) + adjustment).max(0),
        "adjustment": adjustment,
    }))
    .into_response()
}

async fn api_lookup_item_barcode(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
) -> Response {
    let _ = user;
    let Some(item) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let barcode = item["barcode"].as_str().map(|s| s.to_string());
    let Some(barcode) = barcode else {
        return axum::Json(json!({
            "id": item_id, "barcode": Value::Null,
            "description": item["description"], "updated": false,
        }))
        .into_response();
    };
    let mut looked_up = barcode::lookup_local(&barcode).await;
    if looked_up.is_none() {
        looked_up = barcode::lookup_online(&barcode).await;
    }
    let mut updated = false;
    let mut key = item["key"].clone();
    let mut description = item["description"].clone();
    if let Some(desc) = &looked_up {
        if Some(desc.as_str()) != item["description"].as_str() {
            description = json!(desc);
            let k = item["key"].as_str().unwrap_or("");
            if k.is_empty() || k == barcode {
                key = json!(desc.chars().take(100).collect::<String>());
            }
            let _ = sqlx::query(
                "UPDATE supply_items SET description = ?, key = ?, updated_at = datetime('now') WHERE id = ?",
            )
            .bind(desc)
            .bind(key.as_str())
            .bind(item_id)
            .execute(&state.db)
            .await;
            updated = true;
        }
    }
    axum::Json(json!({
        "id": item_id, "barcode": barcode,
        "description": looked_up.or(description.as_str().map(|s| s.to_string())),
        "key": key, "updated": updated,
    }))
    .into_response()
}

async fn api_transfer(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(item_id): Path<i64>,
    body: String,
) -> Response {
    let Some(source) = load_item(&state.db, item_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Item not found").into_response();
    };
    let category = load_category(&state.db, source["category_id"].as_i64().unwrap_or(0)).await;
    if let Some(ref cat) = category {
        if let Some(sid) = cat["store_id"].as_i64() {
            if !has_store_access(&state.db, sid, user.0.id).await {
                return err_json(StatusCode::FORBIDDEN, "Access denied").into_response();
            }
        }
    }
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let quantity = v.get("quantity").and_then(|q| q.as_i64()).unwrap_or(0);
    let target_location = v
        .get("target_location")
        .and_then(|l| l.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if quantity <= 0 {
        return err_json(StatusCode::BAD_REQUEST, "Transfer quantity must be positive").into_response();
    }
    if quantity > source["quantity"].as_i64().unwrap_or(0) {
        return err_json(StatusCode::BAD_REQUEST, "Not enough quantity to transfer").into_response();
    }
    let src_loc = source["location"].as_str().unwrap_or("").to_lowercase();
    if target_location.as_deref().unwrap_or("").to_lowercase() == src_loc {
        return err_json(StatusCode::BAD_REQUEST, "Source and target location are the same").into_response();
    }
    let target_loc_norm = target_location.as_deref().unwrap_or("").to_lowercase();
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM supply_items WHERE category_id = ? AND key = ? AND LOWER(COALESCE(location,'')) = ? LIMIT 1",
    )
    .bind(source["category_id"].as_i64().unwrap_or(0))
    .bind(source["key"].as_str())
    .bind(&target_loc_norm)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let mut target_id = 0i64;
    let target_quantity;
    if let Some(tid) = existing {
        let _ = sqlx::query("UPDATE supply_items SET quantity = quantity + ?, updated_at = datetime('now') WHERE id = ?")
            .bind(quantity)
            .bind(tid)
            .execute(&state.db)
            .await;
        target_id = tid;
        target_quantity = sqlx::query_scalar::<_, i64>("SELECT quantity FROM supply_items WHERE id = ?")
            .bind(tid)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);
    } else {
        if let Ok(res) = sqlx::query(
            "INSERT INTO supply_items (key, description, barcode, quantity, location, notes, category_id, store_id, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(source["key"].as_str())
        .bind(source["description"].as_str())
        .bind(source["barcode"].as_str())
        .bind(quantity)
        .bind(&target_location)
        .bind(source["notes"].as_str())
        .bind(source["category_id"].as_i64().unwrap_or(0))
        .bind(source["store_id"].as_i64())
        .bind(user.0.id)
        .execute(&state.db)
        .await
        {
            target_id = res.last_insert_rowid();
        }
        target_quantity = quantity;
    }
    let new_src_qty = source["quantity"].as_i64().unwrap_or(0) - quantity;
    let source_deleted = new_src_qty == 0;
    if source_deleted {
        let _ = sqlx::query("DELETE FROM supply_items WHERE id = ?")
            .bind(item_id)
            .execute(&state.db)
            .await;
    } else {
        let _ = sqlx::query("UPDATE supply_items SET quantity = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(new_src_qty)
            .bind(item_id)
            .execute(&state.db)
            .await;
    }
    axum::Json(json!({
        "source_id": item_id, "source_quantity": new_src_qty,
        "source_deleted": source_deleted,
        "target_id": target_id, "target_quantity": target_quantity,
        "target_location": target_location, "transferred": quantity,
    }))
    .into_response()
}

async fn api_barcode_lookup(
    user: RequireUser,
    State(state): State<SharedState>,
    body: String,
) -> Response {
    let _ = user;
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let barcode = v.get("barcode").and_then(|b| b.as_str()).unwrap_or("").trim().to_string();
    if barcode.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Barcode is required").into_response();
    }
    let mut description = barcode::lookup_local(&barcode).await;
    if description.is_none() {
        description = barcode::lookup_online(&barcode).await;
    }
    if description.is_none() {
        description = sqlx::query_scalar::<_, String>(
            "SELECT description FROM supply_items WHERE barcode = ? AND description IS NOT NULL LIMIT 1",
        )
        .bind(&barcode)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    }
    barcode::record_lookup(description.is_some());
    axum::Json(json!({"barcode": barcode, "description": description})).into_response()
}

/// Find the inventory row for a barcode in a store/location, merging duplicates.
async fn find_location_item(
    db: &sqlx::SqlitePool,
    barcode: &str,
    store_id: Option<i64>,
    location: &str,
) -> Option<i64> {
    let norm = location.trim().to_lowercase();
    let loc_filter = if norm.is_empty() {
        "(i.location IS NULL OR TRIM(COALESCE(i.location,'')) = '')".to_string()
    } else {
        "LOWER(TRIM(i.location)) = ?".to_string()
    };
    let sql = format!(
        "SELECT i.id, i.quantity FROM supply_items i \
         JOIN supply_categories c ON c.id = i.category_id \
         WHERE i.barcode = ? AND c.store_id IS ? AND {loc_filter} ORDER BY i.created_at"
    );
    let mut q = sqlx::query(&sql).bind(barcode).bind(store_id);
    if !norm.is_empty() {
        q = q.bind(&norm);
    }
    let rows = q.fetch_all(db).await.ok()?;
    let mut ids: Vec<i64> = rows.iter().map(|r| r.get::<i64, _>("id")).collect();
    if ids.is_empty() {
        return None;
    }
    let primary = ids[0];
    let mut total: i64 = rows.iter().map(|r| r.get::<i64, _>("quantity")).sum();
    let dups: Vec<i64> = ids.split_off(1);
    if !dups.is_empty() {
        for d in &dups {
            let _ = sqlx::query("DELETE FROM supply_items WHERE id = ?").bind(d).execute(db).await;
        }
        let _ = sqlx::query("UPDATE supply_items SET quantity = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(total)
            .bind(primary)
            .execute(db)
            .await;
    } else {
        total = rows[0].get::<i64, _>("quantity");
    }
    let _ = total;
    Some(primary)
}

async fn api_scanner_batch(
    user: RequireUser,
    State(state): State<SharedState>,
    body: String,
) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let barcode = v.get("barcode").and_then(|b| b.as_str()).unwrap_or("").trim().to_string();
    let quantity = v.get("quantity").and_then(|q| q.as_i64()).unwrap_or(1);
    let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("").trim().to_string();
    let description = v.get("description").and_then(|d| d.as_str()).unwrap_or("").trim().to_string();
    let location = v.get("location").and_then(|l| l.as_str()).unwrap_or("").trim().to_string();
    if barcode.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Barcode is required").into_response();
    }
    if location.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Location is required").into_response();
    }
    let Some(cat_id) = v.get("category_id").and_then(|c| c.as_i64()) else {
        return err_json(StatusCode::BAD_REQUEST, "Category is required").into_response();
    };
    let Some(cat) = load_category(&state.db, cat_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &cat, user.0.id).await {
        return r;
    }
    let store_id = cat["store_id"].as_i64();
    let existing = find_location_item(&state.db, &barcode, store_id, &location).await;
    let (action, id) = if let Some(id) = existing {
        let _ = sqlx::query(
            "UPDATE supply_items SET quantity = quantity + ?, location = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(quantity)
        .bind(if location.is_empty() { None } else { Some(&location) })
        .bind(id)
        .execute(&state.db)
        .await;
        ("updated", id)
    } else {
        let looked_up = barcode::lookup_online(&barcode).await;
        let final_desc = if !description.is_empty() {
            Some(description.clone())
        } else {
            looked_up
        };
        let final_key = if !key.is_empty() {
            key
        } else {
            match &final_desc {
                Some(d) => d.chars().take(100).collect::<String>(),
                None => barcode.clone(),
            }
        };
        let res = sqlx::query(
            "INSERT INTO supply_items (key, description, barcode, quantity, location, category_id, store_id, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&final_key)
        .bind(&final_desc)
        .bind(&barcode)
        .bind(quantity)
        .bind(if location.is_empty() { None } else { Some(&location) })
        .bind(cat_id)
        .bind(store_id)
        .bind(user.0.id)
        .execute(&state.db)
        .await;
        let id = res.map(|r| r.last_insert_rowid()).unwrap_or(0);
        ("created", id)
    };
    let item = load_item(&state.db, id).await.unwrap_or(json!({}));
    // Delete only the ONE queue row this review entry corresponds to —
    // the same product may legitimately have more scans still queued.
    let _ = sqlx::query(
        "DELETE FROM supply_pending_barcodes WHERE id = \
         (SELECT id FROM supply_pending_barcodes WHERE uploaded_by = ? AND barcode = ? ORDER BY id DESC LIMIT 1)",
    )
    .bind(user.0.id)
    .bind(&barcode)
    .execute(&state.db)
    .await;
    axum::Json(json!({
        "action": action, "id": id,
        "key": item["key"], "description": item["description"],
        "quantity": item["quantity"],
    }))
    .into_response()
}

async fn api_scanner_batch_all(
    user: RequireUser,
    State(state): State<SharedState>,
    body: String,
) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let location = v.get("location").and_then(|l| l.as_str()).unwrap_or("").trim().to_string();
    if location.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Location is required").into_response();
    }
    let Some(cat_id) = v.get("category_id").and_then(|c| c.as_i64()) else {
        return err_json(StatusCode::BAD_REQUEST, "Category is required").into_response();
    };
    let Some(cat) = load_category(&state.db, cat_id).await else {
        return err_json(StatusCode::NOT_FOUND, "Category not found").into_response();
    };
    if let Err(r) = check_category_access(&state.db, &cat, user.0.id).await {
        return r;
    }
    // The user's pending scan queue IS the work list. Each row is deleted as
    // soon as it is processed, so a lost response or retry can never add the
    // same barcode twice — a retry simply continues with what is left.
    let pending = sqlx::query_as::<_, (i64, String, Option<String>, i64)>(
        "SELECT id, barcode, description, quantity FROM supply_pending_barcodes WHERE uploaded_by = ? ORDER BY id",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    if pending.is_empty() {
        // Already processed (e.g. retry after a lost response): report
        // success with no results so the client treats it as done.
        return axum::Json(json!({"results": []})).into_response();
    }
    let store_id = cat["store_id"].as_i64();
    let mut results = Vec::new();
    for (row_id, barcode, description, row_qty) in &pending {
        if barcode.trim().is_empty() {
            let _ = sqlx::query("DELETE FROM supply_pending_barcodes WHERE id = ?")
                .bind(row_id)
                .execute(&state.db)
                .await;
            continue;
        }
        let quantity = if *row_qty > 0 { *row_qty } else { 1 };
        let key = description
            .as_deref()
            .map(|d| d.trim())
            .filter(|d| !d.is_empty())
            .map(|d| d.chars().take(100).collect::<String>())
            .unwrap_or_else(|| barcode.clone());
        let existing = find_location_item(&state.db, barcode, store_id, &location).await;
        if let Some(id) = existing {
            let _ = sqlx::query(
                "UPDATE supply_items SET quantity = quantity + ?, location = ?, updated_at = datetime('now') WHERE id = ?",
            )
            .bind(quantity)
            .bind(if location.is_empty() { None } else { Some(&location) })
            .bind(id)
            .execute(&state.db)
            .await;
            let it = load_item(&state.db, id).await.unwrap_or(json!({}));
            results.push(json!({
                "barcode": barcode, "action": "updated",
                "key": it["key"], "description": it["description"], "quantity": it["quantity"],
            }));
        } else {
            let final_desc = description.as_deref().map(str::trim).filter(|d| !d.is_empty());
            let _ = sqlx::query(
                "INSERT INTO supply_items (key, description, barcode, quantity, location, category_id, store_id, created_by) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&key)
            .bind(final_desc)
            .bind(barcode)
            .bind(quantity)
            .bind(if location.is_empty() { None } else { Some(&location) })
            .bind(cat_id)
            .bind(store_id)
            .bind(user.0.id)
            .execute(&state.db)
            .await;
            results.push(json!({
                "barcode": barcode, "action": "created",
                "key": key, "description": final_desc, "quantity": quantity,
            }));
        }
        let _ = sqlx::query("DELETE FROM supply_pending_barcodes WHERE id = ?")
            .bind(row_id)
            .execute(&state.db)
            .await;
    }
    axum::Json(json!({"results": results})).into_response()
}

async fn api_scanner_batch_upload(
    user: RequireUser,
    State(state): State<SharedState>,
    body: String,
) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let barcodes = v.get("barcodes").and_then(|b| b.as_array()).cloned().unwrap_or_default();
    if barcodes.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "No barcodes provided").into_response();
    }
    let mut results = Vec::new();
    for b in &barcodes {
        let bc = b.as_str().unwrap_or("").trim().to_string();
        if bc.is_empty() {
            continue;
        }
        let description = barcode::lookup_online(&bc).await;
        results.push(json!({"barcode": bc, "description": description}));
        let _ = sqlx::query(
            "INSERT INTO supply_pending_barcodes (barcode, uploaded_by) VALUES (?, ?)",
        )
        .bind(&bc)
        .bind(user.0.id)
        .execute(&state.db)
        .await;
    }
    axum::Json(json!({"barcodes": results})).into_response()
}

async fn api_scanner_locations(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let accessible = accessible_store_ids(&state.db, user.0.id).await;
    if accessible.is_empty() {
        return axum::Json(json!({"locations": []})).into_response();
    }
    let barcode = params.get("barcode").cloned().unwrap_or_default().trim().to_string();
    let in_stores = accessible.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT DISTINCT i.location FROM supply_items i \
         JOIN supply_categories c ON c.id = i.category_id \
         WHERE c.store_id IN ({in_stores}) AND i.location IS NOT NULL AND i.location != '' {} \
         ORDER BY i.location",
        if barcode.is_empty() { String::new() } else { "AND i.barcode = ?".to_string() }
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for sid in &accessible {
        q = q.bind(sid);
    }
    if !barcode.is_empty() {
        q = q.bind(&barcode);
    }
    let locations = q.fetch_all(&state.db).await.unwrap_or_default();
    axum::Json(json!({"locations": locations})).into_response()
}

async fn api_scanner_pending(
    user: RequireUser,
    State(state): State<SharedState>,
) -> Response {
    let rows = sqlx::query_as::<_, (String, Option<String>, i64)>(
        "SELECT barcode, description, quantity FROM supply_pending_barcodes WHERE uploaded_by = ? ORDER BY created_at, id",
    )
    .bind(user.0.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let barcodes: Vec<Value> = rows
        .into_iter()
        .map(|(barcode, description, quantity)| {
            json!({"barcode": barcode, "description": description, "quantity": if quantity > 0 { quantity } else { 1 }})
        })
        .collect();
    axum::Json(json!({"barcodes": barcodes, "count": barcodes.len()})).into_response()
}

async fn api_scanner_pending_clear(
    user: RequireUser,
    State(state): State<SharedState>,
) -> Response {
    let _ = sqlx::query("DELETE FROM supply_pending_barcodes WHERE uploaded_by = ?")
        .bind(user.0.id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"cleared": true})).into_response()
}

/// Queue scanned barcodes into the caller's server-side scan queue. Each
/// scan is its own row (duplicate products are intentional — they
/// accumulate quantity when committed); items flagged update_only just
/// learn a description on the rows already queued for that barcode.
async fn api_scanner_queue_add(
    user: RequireUser,
    State(state): State<SharedState>,
    body: String,
) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let mut items: Vec<Value> = v
        .get("barcodes")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        if let Some(bc) = v.get("barcode").and_then(|b| b.as_str()) {
            items.push(json!({"barcode": bc, "description": v.get("description").and_then(|d| d.as_str()), "quantity": v.get("quantity").and_then(|q| q.as_i64())}));
        }
    }
    let mut saved = 0;
    for item in &items {
        let Some(bc) = item.get("barcode").and_then(|b| b.as_str()).map(str::trim).filter(|b| !b.is_empty()) else {
            continue;
        };
        let desc = item.get("description").and_then(|d| d.as_str()).filter(|d| !d.trim().is_empty());
        let qty = item.get("quantity").and_then(|q| q.as_i64()).filter(|q| *q > 0).unwrap_or(1);
        // update_only rows just learn a description (e.g. online lookup
        // resolved after the scan was queued); plain rows are new scans —
        // duplicates are intentional, each adds quantity when committed.
        let ok = if item.get("update_only").and_then(|u| u.as_bool()).unwrap_or(false) {
            sqlx::query(
                "UPDATE supply_pending_barcodes SET description = ? WHERE uploaded_by = ? AND barcode = ?",
            )
            .bind(desc)
            .bind(user.0.id)
            .bind(bc)
            .execute(&state.db)
            .await
            .is_ok()
        } else {
            sqlx::query(
                "INSERT INTO supply_pending_barcodes (barcode, uploaded_by, description, quantity) VALUES (?, ?, ?, ?)",
            )
            .bind(bc)
            .bind(user.0.id)
            .bind(desc)
            .bind(qty)
            .execute(&state.db)
            .await
            .is_ok()
        };
        if ok {
            saved += 1;
        }
    }
    axum::Json(json!({"saved": saved})).into_response()
}

/// Remove a single barcode from the caller's scan queue (user removed or
/// skipped it in the review phase).
async fn api_scanner_queue_remove(
    user: RequireUser,
    State(state): State<SharedState>,
    body: String,
) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let barcode = v.get("barcode").and_then(|b| b.as_str()).unwrap_or("").trim().to_string();
    if barcode.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Barcode is required").into_response();
    }
    // Each scan is its own row; removing one scan entry deletes a single
    // row (the most recent for that barcode).
    let _ = sqlx::query(
        "DELETE FROM supply_pending_barcodes WHERE id = \
         (SELECT id FROM supply_pending_barcodes WHERE uploaded_by = ? AND barcode = ? ORDER BY id DESC LIMIT 1)",
    )
        .bind(user.0.id)
        .bind(&barcode)
        .execute(&state.db)
        .await;
    axum::Json(json!({"removed": true})).into_response()
}

async fn api_barcode_db_status(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = (user, &state);
    axum::Json(barcode::db_status().await).into_response()
}

async fn api_barcode_reset_stats(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = (user, &state);
    barcode::reset_stats();
    axum::Json(json!({"reset": true})).into_response()
}

// ------------------------------------------------------------------ scanner + print pages

async fn scanner_page(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut store_id: Option<i64> = params.get("store_id").and_then(|v| v.parse().ok());
    let category_id: Option<i64> = params.get("category_id").and_then(|v| v.parse().ok());
    let mut category = None;
    if let Some(cid) = category_id {
        category = load_category(&state.db, cid).await;
        if store_id.is_none() {
            store_id = category.as_ref().and_then(|c| c["store_id"].as_i64());
        }
    }
    let mut store = Value::Null;
    if let Some(sid) = store_id {
        let row = sqlx::query("SELECT * FROM supply_stores WHERE id = ?")
            .bind(sid)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
        match row {
            Some(r) if has_store_access(&state.db, sid, user.0.id).await => {
                store = store_json(&r);
            }
            _ => return err_json(StatusCode::FORBIDDEN, "Access denied").into_response(),
        }
    }
    let cats = store_categories(&state.db, store_id).await;
    page(
        &state,
        "supply_scanner.html",
        json!({
            "categories": cats, "store_id": store_id, "store": store,
            "category": category, "selected_category_id": category_id,
        }),
    )
    .into_response()
}

async fn print_form(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Html<String> {
    let _ = user;
    let rows = params.get("rows").and_then(|v| v.parse::<i64>().ok()).unwrap_or(30);
    let row_count = rows.clamp(10, 60);
    page(&state, "supply_print_form.html", json!({"row_count": row_count}))
}
