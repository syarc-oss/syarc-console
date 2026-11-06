//! Eggs app: coops with hens and egg production records.

use axum::body::Body;
use axum::extract::{Path, State};
use crate::AgriForm;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{
    add_share, err400, err403, err404, form_map, fv, new_uuid, now_str, ok_json, page, parse_json_body,
    redirect, revoke_share, shareable_users, shared_users, user_has_access, user_json, valid_date,
};

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/eggs", get(root))
        .route("/eggs/", get(root))
        .route("/eggs/dashboard", get(dashboard_redirect))
        .route("/eggs/coop/new", get(new_coop_page).post(create_coop))
        .route("/eggs/coop/{coop_id}/edit", get(edit_coop_page).post(update_coop))
        .route("/eggs/coop/{coop_id}/delete", post(delete_coop))
        .route("/eggs/coop/{coop_id}/share", post(share_coop))
        .route("/eggs/coop/{coop_id}/revoke/{share_id}", post(revoke_coop))
        .route("/eggs/coops/{coop_id}", get(view_coop))
        .route("/eggs/coops/{coop_id}/hens", get(hens_page))
        .route("/eggs/coops/{coop_id}/entry", get(entry_page))
        .route("/eggs/coops/{coop_id}/reports", get(reports_page))
        .route("/eggs/coops/{coop_id}/reports/details", post(report_details))
        .route("/eggs/coops/{coop_id}/reports/export", post(report_export))
        .route("/eggs/coops/{coop_id}/reports/summary", post(report_summary))
        .route("/eggs/coops/{coop_id}/reports/producers", post(report_producers))
        .route("/eggs/coops/{coop_id}/hens/list", get(api_list_hens))
        .route("/eggs/coops/{coop_id}/hens/create", post(create_hen))
        .route("/eggs/coops/{coop_id}/hens/{hen_id}", put(update_hen).delete(delete_hen))
        .route("/eggs/coops/{coop_id}/hens/{hen_id}/toggle", axum::routing::patch(toggle_hen))
        .route("/eggs/coops/{coop_id}/production/list", get(production_list))
        .route("/eggs/coops/{coop_id}/production/hens", get(production_hens))
        .route("/eggs/coops/{coop_id}/production/add", post(production_add))
        .route("/eggs/coops/{coop_id}/production/{entry_id}", delete(production_delete))
        .route("/eggs/coops/{coop_id}/production/stats", get(production_stats))
}

fn coop_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "user_id": r.get::<i64,_>("user_id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "is_active": r.get::<i64,_>("is_active") == 1,
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
        "updated_at": r.try_get::<String,_>("updated_at").unwrap_or_default(),
    })
}

async fn get_coop(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM coops WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| coop_row(&r))
}

async fn coop_counts(db: &syarc_plugin_api::Db, coop_id: &str) -> (i64, i64) {
    let hens: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM hens WHERE coop_id = ?",
    )
    .bind(coop_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let eggs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM egg_production ep JOIN hens h ON ep.hen_id = h.id WHERE h.coop_id = ?",
    )
    .bind(coop_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    (hens, eggs)
}

async fn root(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let mut coops: Vec<Value> = Vec::new();

    let own = sqlx::query("SELECT * FROM coops WHERE user_id = ? ORDER BY name")
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    for r in &own {
        let id = r.get::<String, _>("id");
        let (hen_count, egg_count) = coop_counts(db, &id).await;
        coops.push(json!({
            "coop": coop_row(r), "is_owner": true,
            "hen_count": hen_count, "egg_count": egg_count, "shared_at": null,
        }));
    }

    let shared = sqlx::query(
        "SELECT o.*, u.username AS owner_username, sh.created_at AS shared_at \
         FROM coop_shares sh JOIN coops o ON sh.coop_id = o.id JOIN users u ON u.id = o.user_id \
         WHERE sh.shared_with_user_id = ? ORDER BY o.name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &shared {
        let id = r.get::<String, _>("id");
        let (hen_count, egg_count) = coop_counts(db, &id).await;
        coops.push(json!({
            "coop": coop_row(r), "is_owner": false,
            "hen_count": hen_count, "egg_count": egg_count,
            "shared_at": r.try_get::<String,_>("shared_at").unwrap_or_default(),
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
        }));
    }

    page(
        &state,
        "eggs/coop_select.html",
        json!({"user": user_json(&user), "coops": coops}),
    )
}

async fn dashboard_redirect() -> Response {
    redirect("/eggs/")
}

async fn new_coop_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    page(
        &state,
        "eggs/coop_form.html",
        json!({"user": user_json(&user), "coop": null, "mode": "new"}),
    )
}

async fn create_coop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    let Some(name) = fv(&form, "name") else {
        return redirect("/eggs/coop/new");
    };
    let description = fv(&form, "description");
    let id = new_uuid();
    let now = now_str();
    let ok = sqlx::query(
        "INSERT INTO coops (id, user_id, name, description, is_active, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(&name)
    .bind(&description)
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return redirect("/eggs/coop/new");
    }
    let _ = sqlx::query("UPDATE hens SET coop_id = ? WHERE coop_id IS NULL")
        .bind(&id)
        .execute(db)
        .await;
    redirect("/eggs/")
}

async fn edit_coop_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    let coop = sqlx::query("SELECT * FROM coops WHERE id = ? AND user_id = ?")
        .bind(&coop_id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| coop_row(&r));
    match coop {
        None => redirect("/eggs/"),
        Some(coop) => page(
            &state,
            "eggs/coop_form.html",
            json!({"user": user_json(&user), "coop": coop, "mode": "edit"}),
        ),
    }
}

async fn update_coop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("/eggs/coop/{coop_id}/edit"));
    };
    let description = fv(&form, "description");
    let _ = sqlx::query(
        "UPDATE coops SET name = ?, description = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(&name)
    .bind(&description)
    .bind(now_str())
    .bind(&coop_id)
    .bind(user.id)
    .execute(db)
    .await;
    redirect("/eggs/")
}

async fn delete_coop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    let owned: Option<String> = sqlx::query_scalar("SELECT id FROM coops WHERE id = ? AND user_id = ?")
        .bind(&coop_id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owned.is_some() {
        // Deleting the coop cascades to shares; orphan hens/production.
        let _ = sqlx::query("DELETE FROM egg_production WHERE hen_id IN (SELECT id FROM hens WHERE coop_id = ?)")
            .bind(&coop_id)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM hens WHERE coop_id = ?").bind(&coop_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM coop_shares WHERE coop_id = ?").bind(&coop_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM coops WHERE id = ?").bind(&coop_id).execute(db).await;
    }
    redirect("/eggs/")
}

async fn share_coop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM coops WHERE id = ?")
        .bind(&coop_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/eggs/");
    }
    if let Some(target) = fv(&form, "share_with_user_id") {
        let _ = add_share(db, "coop_shares", "coop_id", &coop_id, user.id, &target).await;
    }
    redirect(&format!("/eggs/coops/{coop_id}"))
}

async fn revoke_coop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((coop_id, share_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM coops WHERE id = ?")
        .bind(&coop_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/eggs/");
    }
    revoke_share(db, "coop_shares", "coop_id", &coop_id, &share_id).await;
    redirect(&format!("/eggs/coops/{coop_id}"))
}

async fn view_coop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(coop) = get_coop(db, &coop_id).await else {
        return err404("Coop not found");
    };
    let is_owner = coop["user_id"].as_i64() == Some(user.id);
    if !is_owner && !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let (shared, all_users) = if is_owner {
        (
            shared_users(db, "coop_shares", "coop_id", &coop_id).await,
            shareable_users(db, "coop_shares", "coop_id", &coop_id, user.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    page(
        &state,
        "eggs/eggs_dashboard.html",
        json!({
            "user": user_json(&user), "coop": coop, "is_owner": is_owner,
            "shared_users": shared, "all_users": all_users,
        }),
    )
}

async fn hens_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let coop = get_coop(db, &coop_id).await;
    page(
        &state,
        "eggs/hens.html",
        json!({
            "user": user_json(&user), "coop_id": coop_id,
            "coop_name": coop.map(|c| c["name"].clone()).unwrap_or(json!("Coop")),
        }),
    )
}

async fn entry_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let active_hens = sqlx::query(
        "SELECT id, name FROM hens WHERE is_active = 1 AND coop_id = ? ORDER BY name",
    )
    .bind(&coop_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| json!({"id": r.get::<String,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()}))
    .collect::<Vec<_>>();
    let coop = get_coop(db, &coop_id).await;
    page(
        &state,
        "eggs/entry.html",
        json!({
            "user": user_json(&user), "active_hens": active_hens, "coop_id": coop_id,
            "coop_name": coop.map(|c| c["name"].clone()).unwrap_or(json!("Coop")),
        }),
    )
}

async fn reports_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let hens = sqlx::query(
        "SELECT id, name FROM hens WHERE is_active = 1 AND coop_id = ? ORDER BY name",
    )
    .bind(&coop_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| json!({"id": r.get::<String,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()}))
    .collect::<Vec<_>>();
    let coop = get_coop(db, &coop_id).await;
    page(
        &state,
        "eggs/reports.html",
        json!({
            "user": user_json(&user), "hens": hens, "coop_id": coop_id,
            "coop_name": coop.map(|c| c["name"].clone()).unwrap_or(json!("Coop")),
        }),
    )
}

// --- Report API ---

fn report_filters(body: &Value) -> Option<(String, String, Option<String>)> {
    let start = body.get("start_date")?.as_str()?.to_string();
    let end = body.get("end_date")?.as_str()?.to_string();
    let hen = body.get("hen_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
    Some((start, end, hen))
}

async fn report_rows(
    db: &syarc_plugin_api::Db,
    coop_id: &str,
    start: &str,
    end: &str,
    hen: &Option<String>,
) -> Vec<(String, String, i64, String)> {
    let mut sql = String::from(
        "SELECT ep.collection_date, h.name AS hen_name, COALESCE(ep.quantity,1) AS qty, ep.notes FROM egg_production ep \
         JOIN hens h ON ep.hen_id = h.id \
         WHERE ep.collection_date >= ? AND ep.collection_date <= ? AND h.coop_id = ?",
    );
    if hen.is_some() {
        sql.push_str(" AND ep.hen_id = ?");
    }
    sql.push_str(" ORDER BY ep.collection_date DESC");
    let mut q = sqlx::query(&sql).bind(start).bind(end).bind(coop_id);
    if let Some(h) = hen {
        q = q.bind(h);
    }
    q.fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            (
                r.try_get::<String, _>("collection_date").unwrap_or_default(),
                r.try_get::<String, _>("hen_name").unwrap_or_default(),
                r.try_get::<i64, _>("qty").unwrap_or(1),
                r.try_get::<Option<String>, _>("notes").ok().flatten().unwrap_or_default(),
            )
        })
        .collect()
}

async fn report_details(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, hen)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let rows = report_rows(db, &coop_id, &start, &end, &hen).await;
    ok_json(Value::Array(
        rows.into_iter()
            .map(|(d, name, qty, notes)| {
                json!({"collection_date": d, "hen_name": name, "quantity": qty, "notes": notes})
            })
            .collect(),
    ))
}

async fn report_export(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, hen)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let rows = report_rows(db, &coop_id, &start, &end, &hen).await;
    let mut csv = String::from("Date,Hen,Quantity,Notes\n");
    for (d, name, qty, notes) in rows {
        csv.push_str(&format!("{d},{name},{qty},{notes}\n"));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv")
        .header(
            header::CONTENT_DISPOSITION,
            r#"attachment; filename="egg_production.csv""#,
        )
        .body(Body::from(csv))
        .unwrap()
}

async fn report_summary(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, hen)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let mut sql = String::from(
        "SELECT SUM(COALESCE(ep.quantity,1)), COUNT(DISTINCT ep.collection_date) FROM egg_production ep \
         JOIN hens h ON ep.hen_id = h.id \
         WHERE ep.collection_date >= ? AND ep.collection_date <= ? AND h.coop_id = ?",
    );
    if hen.is_some() {
        sql.push_str(" AND ep.hen_id = ?");
    }
    let total_eggs: i64 = sqlx::query(&sql)
        .bind(&start)
        .bind(&end)
        .bind(&coop_id)
        .bind(hen.as_deref().unwrap_or(""))
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64, _>(0))
        .unwrap_or(0);
    let total_hens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hens WHERE is_active = 1 AND coop_id = ?")
        .bind(&coop_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let span_days: i64 = sqlx::query_scalar(
        "SELECT CAST(julianday(?) - julianday(?) AS INTEGER) + 1",
    )
    .bind(&end)
    .bind(&start)
    .fetch_one(db)
    .await
    .unwrap_or(0)
    .max(0);
    let average = if span_days > 0 { total_eggs as f64 / span_days as f64 } else { 0.0 };
    ok_json(json!({
        "total_eggs": total_eggs, "total_days": span_days,
        "average_per_day": (average * 100.0).round() / 100.0, "total_hens": total_hens,
    }))
}

async fn report_producers(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, hen)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let mut sql = String::from(
        "SELECT h.id, h.name, h.is_active, SUM(COALESCE(ep.quantity,1)) AS total_eggs, \
                COUNT(DISTINCT ep.collection_date) AS days_active \
         FROM hens h JOIN egg_production ep ON ep.hen_id = h.id \
         WHERE ep.collection_date >= ? AND ep.collection_date <= ? AND h.coop_id = ?",
    );
    if hen.is_some() {
        sql.push_str(" AND ep.hen_id = ?");
    }
    sql.push_str(" GROUP BY h.id, h.name, h.is_active ORDER BY total_eggs DESC");
    let mut q = sqlx::query(&sql).bind(&start).bind(&end).bind(&coop_id);
    if let Some(h) = &hen {
        q = q.bind(h);
    }
    let rows = q.fetch_all(db).await.unwrap_or_default();
    let producers: Vec<Value> = rows
        .iter()
        .map(|r| {
            let is_active = r.get::<i64, _>("is_active") == 1;
            let total_eggs = r.get::<i64, _>("total_eggs");
            json!({
                "hen_id": r.get::<String,_>("id"),
                "hen_name": r.try_get::<String,_>("name").unwrap_or_default(),
                "is_active": is_active,
                "total_eggs": total_eggs,
                "days_active": r.get::<i64,_>("days_active"),
                "is_production_ready": is_active && total_eggs > 0,
            })
        })
        .collect();
    let total = producers.len();
    ok_json(json!({
        "producers": producers, "total_producers": total,
        "period_start": start, "period_end": end,
    }))
}

// --- Hen API ---

async fn api_list_hens(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let hens = sqlx::query("SELECT * FROM hens WHERE coop_id = ? ORDER BY name")
        .bind(&coop_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(hen_row)
        .collect::<Vec<_>>();
    ok_json(Value::Array(hens))
}

fn hen_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "breed": r.try_get::<Option<String>,_>("breed").ok().flatten(),
        "acquisition_date": r.try_get::<Option<String>,_>("acquisition_date").ok().flatten(),
        "is_active": r.get::<i64,_>("is_active") == 1,
        "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
    })
}

async fn create_hen(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let Some(name) = fv(&form, "name") else {
        return err400("Name is required");
    };
    let dup: Option<String> =
        sqlx::query_scalar("SELECT id FROM hens WHERE name = ? AND coop_id = ?")
            .bind(&name)
            .bind(&coop_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if dup.is_some() {
        return err400("A hen with this name already exists in this coop");
    }
    let breed = fv(&form, "breed");
    let acquisition_date = fv(&form, "acquisition_date").and_then(|d| valid_date(&d));
    let notes = fv(&form, "notes");
    let id = new_uuid();
    let ok = sqlx::query(
        "INSERT INTO hens (id, name, breed, acquisition_date, is_active, notes, coop_id, created_at, created_by) \
         VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&breed)
    .bind(&acquisition_date)
    .bind(&notes)
    .bind(&coop_id)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return err400("Could not create hen");
    }
    ok_json(json!({"id": id, "message": "Created successfully"}))
}

async fn update_hen(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((coop_id, hen_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM hens WHERE id = ? AND coop_id = ?")
        .bind(&hen_id)
        .bind(&coop_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if existing.is_none() {
        return err404("Hen not found");
    }
    if let Some(breed) = fv(&form, "breed") {
        let _ = sqlx::query("UPDATE hens SET breed = ? WHERE id = ?").bind(breed).bind(&hen_id).execute(db).await;
    }
    if form.contains_key("acquisition_date") {
        let d = fv(&form, "acquisition_date").and_then(|d| valid_date(&d));
        let _ = sqlx::query("UPDATE hens SET acquisition_date = ? WHERE id = ?").bind(d).bind(&hen_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let notes = fv(&form, "notes");
        let _ = sqlx::query("UPDATE hens SET notes = ? WHERE id = ?").bind(notes).bind(&hen_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn toggle_hen(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((coop_id, hen_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("UPDATE hens SET is_active = 1 - is_active WHERE id = ? AND coop_id = ?")
        .bind(&hen_id)
        .bind(&coop_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Status toggled"})),
        _ => err404("Hen not found"),
    }
}

async fn delete_hen(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((coop_id, hen_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let _ = sqlx::query("DELETE FROM egg_production WHERE hen_id = ?").bind(&hen_id).execute(db).await;
    let res = sqlx::query("DELETE FROM hens WHERE id = ? AND coop_id = ?")
        .bind(&hen_id)
        .bind(&coop_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Hen not found"),
    }
}

// --- Production API ---

async fn production_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let rows = sqlx::query(
        "SELECT ep.id, ep.collection_date, ep.notes, ep.quantity, h.name AS hen_name, h.id AS hen_id \
         FROM egg_production ep JOIN hens h ON ep.hen_id = h.id WHERE h.coop_id = ? \
         ORDER BY ep.collection_date DESC, ep.created_at DESC LIMIT 10",
    )
    .bind(&coop_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let entries: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String,_>("id"),
                "collection_date": r.try_get::<String,_>("collection_date").unwrap_or_default(),
                "hen_name": r.try_get::<String,_>("hen_name").unwrap_or_default(),
                "hen_id": r.get::<String,_>("hen_id"),
                "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
                "quantity": r.try_get::<i64,_>("quantity").unwrap_or(1),
            })
        })
        .collect();
    ok_json(Value::Array(entries))
}

async fn production_hens(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let hens: Vec<Value> = sqlx::query("SELECT id, name FROM hens WHERE is_active = 1 AND coop_id = ? ORDER BY name")
        .bind(&coop_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| json!({"id": r.get::<String,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()}))
        .collect();
    ok_json(Value::Array(hens))
}

async fn production_add(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let (Some(hen_id), Some(date)) = (fv(&form, "hen_id"), fv(&form, "collection_date")) else {
        return err400("Missing required fields");
    };
    let Some(date) = valid_date(&date) else {
        return err400("Invalid date");
    };
    let hen: Option<String> = sqlx::query_scalar("SELECT id FROM hens WHERE id = ? AND coop_id = ?")
        .bind(&hen_id)
        .bind(&coop_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if hen.is_none() {
        return err404("Hen not found in this coop");
    }
    let notes = fv(&form, "notes");
    let quantity: i64 = fv(&form, "quantity")
        .and_then(|q| q.trim().parse().ok())
        .filter(|q| (1..=999).contains(q))
        .unwrap_or(1);
    let ok = sqlx::query(
        "INSERT INTO egg_production (id, hen_id, collection_date, notes, quantity, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_uuid())
    .bind(&hen_id)
    .bind(&date)
    .bind(&notes)
    .bind(quantity)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if ok {
        ok_json(json!({"message": "Entry added successfully"}))
    } else {
        err400("Could not add entry")
    }
}

async fn production_delete(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((coop_id, entry_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM egg_production WHERE id = ?")
        .bind(&entry_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Entry deleted successfully"})),
        _ => err404("Entry not found"),
    }
}

async fn production_stats(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(coop_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "coops", "coop_shares", "coop_id", &coop_id, user.id).await {
        return err403("Access denied");
    }
    let total_eggs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM egg_production ep JOIN hens h ON ep.hen_id = h.id WHERE h.coop_id = ?",
    )
    .bind(&coop_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let total_hens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hens WHERE is_active = 1 AND coop_id = ?")
        .bind(&coop_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    ok_json(json!({"total_eggs": total_eggs, "total_hens": total_hens}))
}
