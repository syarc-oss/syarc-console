//! Orchard app: orchards with trees and per-tree / per-orchard maintenance.

use axum::extract::{Path, State};
use crate::AgriForm;
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{
    add_share, err400, err403, err404, form_map, fv, new_uuid, now_str, ok_json, page, redirect,
    revoke_share, shareable_users, shared_users, user_has_access, user_json, valid_date,
};

pub const TREE_MAINTENANCE_TYPES: [&str; 12] = [
    "fertilizing",
    "pruning",
    "harvesting",
    "pest_management",
    "watering",
    "spraying",
    "mulching",
    "thinning",
    "disease_treatment",
    "soil_amendment",
    "weed_control",
    "other",
];

pub const ORCHARD_MAINTENANCE_TYPES: [&str; 13] = [
    "mowing",
    "fertilizing",
    "pruning",
    "pest_management",
    "spraying",
    "weed_control",
    "irrigation",
    "mulching",
    "soil_amendment",
    "cover_crop",
    "fence_repair",
    "harvesting",
    "other",
];

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/orchard", get(root))
        .route("/orchard/", get(root))
        .route("/orchard/new", get(new_orchard_page).post(create_orchard))
        .route("/orchard/{orchard_id}/edit", get(edit_orchard_page).post(update_orchard))
        .route("/orchard/{orchard_id}/delete", post(delete_orchard))
        .route("/orchard/{orchard_id}/share", post(share_orchard))
        .route("/orchard/{orchard_id}/revoke/{share_id}", post(revoke_orchard))
        .route("/orchard/{orchard_id}", get(view_orchard))
        .route("/orchard/{orchard_id}/maintenance", get(maintenance_page))
        .route("/orchard/{orchard_id}/maintenance/add", post(add_maintenance))
        .route("/orchard/{orchard_id}/maintenance/{record_id}", put(update_maintenance).delete(delete_maintenance))
        .route("/orchard/{orchard_id}/trees", get(trees_page))
        .route("/orchard/{orchard_id}/trees/list", get(api_list_trees))
        .route("/orchard/{orchard_id}/trees/create", post(create_tree))
        .route("/orchard/{orchard_id}/trees/{tree_id}", get(tree_detail).put(update_tree).delete(delete_tree))
        .route("/orchard/{orchard_id}/trees/{tree_id}/toggle", axum::routing::patch(toggle_tree))
        .route("/orchard/{orchard_id}/trees/{tree_id}/maintenance/list", get(list_tree_maintenance))
        .route("/orchard/{orchard_id}/trees/{tree_id}/maintenance/add", post(add_tree_maintenance))
        .route("/orchard/{orchard_id}/trees/{tree_id}/maintenance/{record_id}", put(update_tree_maintenance).delete(delete_tree_maintenance))
        .route("/orchard/{orchard_id}/stats", get(stats))
}

fn orchard_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "user_id": r.get::<i64,_>("user_id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "location": r.try_get::<Option<String>,_>("location").ok().flatten(),
        "is_active": r.get::<i64,_>("is_active") == 1,
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
        "updated_at": r.try_get::<String,_>("updated_at").unwrap_or_default(),
    })
}

async fn get_orchard(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM orchards WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| orchard_row(&r))
}

fn tree_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "tree_type": r.try_get::<String,_>("tree_type").unwrap_or_default(),
        "variety": r.try_get::<String,_>("variety").unwrap_or_default(),
        "planting_date": r.try_get::<Option<String>,_>("planting_date").ok().flatten(),
        "is_active": r.get::<i64,_>("is_active") == 1,
        "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
        "location_notes": r.try_get::<Option<String>,_>("location_notes").ok().flatten(),
        "orchard_id": r.get::<String,_>("orchard_id"),
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
    })
}

fn maint_record_json(r: &sqlx::sqlite::SqliteRow, type_col: &str, date_col: &str) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "maintenance_type": r.try_get::<String,_>(type_col).unwrap_or_default(),
        "maintenance_date": r.try_get::<String,_>(date_col).unwrap_or_default(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "products_used": r.try_get::<Option<String>,_>("products_used").ok().flatten(),
        "cost": r.try_get::<Option<f64>,_>("cost").ok().flatten(),
        "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
        "recorded_by": r.try_get::<String,_>("username").unwrap_or_default(),
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
    })
}

async fn count(db: &syarc_plugin_api::Db, sql: &str, id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap_or(0)
}

async fn root(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let mut orchards: Vec<Value> = Vec::new();

    let own = sqlx::query("SELECT * FROM orchards WHERE user_id = ? ORDER BY name")
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    for r in &own {
        let id = r.get::<String, _>("id");
        let trees = count(db, "SELECT COUNT(*) FROM trees WHERE orchard_id = ?", &id).await;
        let maint = count(
            db,
            "SELECT COUNT(*) FROM maintenance_records mr JOIN trees t ON mr.tree_id = t.id WHERE t.orchard_id = ?",
            &id,
        )
        .await;
        orchards.push(json!({
            "orchard": orchard_row(r), "is_owner": true,
            "tree_count": trees, "maintenance_count": maint, "shared_at": null,
        }));
    }

    let shared = sqlx::query(
        "SELECT o.*, u.username AS owner_username FROM orchard_shares sh \
         JOIN orchards o ON sh.orchard_id = o.id JOIN users u ON u.id = o.user_id \
         WHERE sh.shared_with_user_id = ? ORDER BY o.name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &shared {
        let id = r.get::<String, _>("id");
        let trees = count(db, "SELECT COUNT(*) FROM trees WHERE orchard_id = ?", &id).await;
        let maint = count(
            db,
            "SELECT COUNT(*) FROM maintenance_records mr JOIN trees t ON mr.tree_id = t.id WHERE t.orchard_id = ?",
            &id,
        )
        .await;
        orchards.push(json!({
            "orchard": orchard_row(r), "is_owner": false,
            "tree_count": trees, "maintenance_count": maint,
            "shared_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
        }));
    }

    page(
        &state,
        "orchard/orchard_select.html",
        json!({"user": user_json(&user), "orchards": orchards}),
    )
}

async fn new_orchard_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    page(
        &state,
        "orchard/orchard_form.html",
        json!({"user": user_json(&user), "orchard": null, "mode": "new"}),
    )
}

async fn create_orchard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect("/orchard/new");
    };
    let id = new_uuid();
    let now = now_str();
    let _ = sqlx::query(
        "INSERT INTO orchards (id, user_id, name, description, location, is_active, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(&name)
    .bind(fv(&form, "description"))
    .bind(fv(&form, "location"))
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await;
    redirect("/orchard/")
}

async fn edit_orchard_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
) -> Response {
    let orchard = sqlx::query("SELECT * FROM orchards WHERE id = ? AND user_id = ?")
        .bind(&orchard_id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    match orchard {
        Some(r) => page(
            &state,
            "orchard/orchard_form.html",
            json!({"user": user_json(&user), "orchard": orchard_row(&r), "mode": "edit"}),
        ),
        None => redirect("/orchard/"),
    }
}

async fn update_orchard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let exists = sqlx::query_scalar::<_, String>(
        "SELECT id FROM orchards WHERE id = ? AND user_id = ?",
    )
    .bind(&orchard_id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if exists.is_none() {
        return redirect("/orchard/");
    }
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("/orchard/{orchard_id}/edit"));
    };
    let _ = sqlx::query(
        "UPDATE orchards SET name = ?, description = ?, location = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(fv(&form, "description"))
    .bind(fv(&form, "location"))
    .bind(now_str())
    .bind(&orchard_id)
    .execute(&state.db)
    .await;
    redirect("/orchard/")
}

async fn delete_orchard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM orchards WHERE id = ? AND user_id = ?")
        .bind(&orchard_id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_some() {
        // FKs cascade via explicit deletes (shares, trees, records)
        let _ = sqlx::query("DELETE FROM maintenance_records WHERE tree_id IN (SELECT id FROM trees WHERE orchard_id = ?)").bind(&orchard_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM trees WHERE orchard_id = ?").bind(&orchard_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM orchard_maintenance WHERE orchard_id = ?").bind(&orchard_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM orchard_shares WHERE orchard_id = ?").bind(&orchard_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM orchards WHERE id = ?").bind(&orchard_id).execute(db).await;
    }
    redirect("/orchard/")
}

async fn share_orchard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM orchards WHERE id = ?")
        .bind(&orchard_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/orchard/");
    }
    let form = form_map(&raw);
    let Some(target) = fv(&form, "share_with_user_id") else {
        return redirect(&format!("/orchard/{orchard_id}"));
    };
    if add_share(db, "orchard_shares", "orchard_id", &orchard_id, user.id, &target)
        .await
        .is_err()
    {
        return redirect(&format!("/orchard/{orchard_id}"));
    }
    redirect(&format!("/orchard/{orchard_id}"))
}

async fn revoke_orchard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, share_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM orchards WHERE id = ?")
        .bind(&orchard_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/orchard/");
    }
    revoke_share(db, "orchard_shares", "orchard_id", &orchard_id, &share_id).await;
    redirect(&format!("/orchard/{orchard_id}"))
}

async fn view_orchard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(orchard) = get_orchard(db, &orchard_id).await else {
        return err404("Orchard not found");
    };
    let is_owner = orchard["user_id"].as_i64() == Some(user.id);
    if !is_owner
        && !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id)
            .await
    {
        return err403("Access denied");
    }
    let (shared, all_users) = if is_owner {
        (
            shared_users(db, "orchard_shares", "orchard_id", &orchard_id).await,
            shareable_users(db, "orchard_shares", "orchard_id", &orchard_id, user.id).await,
        )
    } else {
        (vec![], vec![])
    };
    page(
        &state,
        "orchard/orchard_dashboard.html",
        json!({
            "user": user_json(&user), "orchard": orchard, "is_owner": is_owner,
            "shared_users": shared, "all_users": all_users,
        }),
    )
}

async fn maintenance_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(orchard) = get_orchard(db, &orchard_id).await else {
        return err404("Orchard not found");
    };
    let is_owner = orchard["user_id"].as_i64() == Some(user.id);
    if !is_owner
        && !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id)
            .await
    {
        return err403("Access denied");
    }
    let rows = sqlx::query(
        "SELECT om.*, u.username FROM orchard_maintenance om \
         JOIN users u ON u.id = om.recorded_by WHERE om.orchard_id = ? \
         ORDER BY om.maintenance_date DESC",
    )
    .bind(&orchard_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let records: Vec<Value> = rows.iter().map(|r| maint_record_json(r, "maintenance_type", "maintenance_date")).collect();
    page(
        &state,
        "orchard/orchard_maintenance.html",
        json!({
            "user": user_json(&user), "orchard": orchard,
            "maintenance_records": records,
            "maintenance_types": ORCHARD_MAINTENANCE_TYPES,
            "is_owner": is_owner,
        }),
    )
}

async fn trees_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let name = sqlx::query_scalar::<_, String>("SELECT name FROM orchards WHERE id = ?")
        .bind(&orchard_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Orchard".into());
    page(
        &state,
        "orchard/trees.html",
        json!({"user": user_json(&user), "orchard_id": orchard_id, "orchard_name": name}),
    )
}

async fn api_list_trees(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let rows = sqlx::query("SELECT * FROM trees WHERE orchard_id = ? ORDER BY tree_type, variety")
        .bind(&orchard_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let mut list = Vec::new();
    for r in &rows {
        let mut t = tree_row(r);
        let last: Option<String> = sqlx::query_scalar(
            "SELECT maintenance_date FROM maintenance_records WHERE tree_id = ? ORDER BY maintenance_date DESC LIMIT 1",
        )
        .bind(t["id"].as_str().unwrap_or(""))
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        t["last_maintenance"] = json!(last);
        list.push(t);
    }
    ok_json(Value::Array(list))
}

async fn create_tree(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let form = form_map(&raw);
    let (Some(tree_type), Some(variety)) = (fv(&form, "tree_type"), fv(&form, "variety")) else {
        return err400("tree_type and variety are required");
    };
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO trees (id, tree_type, variety, planting_date, is_active, notes, location_notes, orchard_id, created_at, created_by) \
         VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&tree_type)
    .bind(&variety)
    .bind(fv(&form, "planting_date").and_then(|d| valid_date(&d)))
    .bind(fv(&form, "notes"))
    .bind(fv(&form, "location_notes"))
    .bind(&orchard_id)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await;
    ok_json(json!({"id": id, "message": "Created successfully"}))
}

async fn tree_detail(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let orchard = get_orchard(db, &orchard_id).await;
    let tree = sqlx::query("SELECT * FROM trees WHERE id = ? AND orchard_id = ?")
        .bind(&tree_id)
        .bind(&orchard_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(tree_rowv) = tree else {
        return err404("Tree not found");
    };
    let tree = tree_row(&tree_rowv);
    let rows = sqlx::query(
        "SELECT mr.*, u.username FROM maintenance_records mr \
         JOIN users u ON u.id = mr.recorded_by WHERE mr.tree_id = ? \
         ORDER BY mr.maintenance_date DESC",
    )
    .bind(&tree_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let records: Vec<Value> = rows.iter().map(|r| maint_record_json(r, "maintenance_type", "maintenance_date")).collect();
    page(
        &state,
        "orchard/tree_detail.html",
        json!({
            "user": user_json(&user),
            "orchard_id": orchard_id,
            "orchard_name": orchard.as_ref().map(|o| o["name"].clone()).unwrap_or(json!("Orchard")),
            "tree": tree,
            "maintenance_records": records,
            "maintenance_types": TREE_MAINTENANCE_TYPES,
            "is_owner": orchard.map(|o| o["user_id"].as_i64() == Some(user.id)).unwrap_or(false),
        }),
    )
}

async fn update_tree(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM trees WHERE id = ? AND orchard_id = ?")
        .bind(&tree_id)
        .bind(&orchard_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Tree not found");
    }
    let form = form_map(&raw);
    if let Some(v) = fv(&form, "tree_type") {
        let _ = sqlx::query("UPDATE trees SET tree_type = ? WHERE id = ?").bind(&v).bind(&tree_id).execute(db).await;
    }
    if let Some(v) = fv(&form, "variety") {
        let _ = sqlx::query("UPDATE trees SET variety = ? WHERE id = ?").bind(&v).bind(&tree_id).execute(db).await;
    }
    if let Some(d) = form.get("planting_date") {
        let d = d.trim();
        let v = if d.is_empty() { None } else { valid_date(d) };
        let _ = sqlx::query("UPDATE trees SET planting_date = ? WHERE id = ?").bind(&v).bind(&tree_id).execute(db).await;
    }
    if let Some(v) = form.get("notes") {
        let v = v.trim();
        let v = if v.is_empty() { None } else { Some(v.to_string()) };
        let _ = sqlx::query("UPDATE trees SET notes = ? WHERE id = ?").bind(&v).bind(&tree_id).execute(db).await;
    }
    if let Some(v) = form.get("location_notes") {
        let v = v.trim();
        let v = if v.is_empty() { None } else { Some(v.to_string()) };
        let _ = sqlx::query("UPDATE trees SET location_notes = ? WHERE id = ?").bind(&v).bind(&tree_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn toggle_tree(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("UPDATE trees SET is_active = 1 - is_active WHERE id = ? AND orchard_id = ?")
        .bind(&tree_id)
        .bind(&orchard_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Status toggled"})),
        _ => err404("Tree not found"),
    }
}

async fn delete_tree(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let _ = sqlx::query("DELETE FROM maintenance_records WHERE tree_id = ?").bind(&tree_id).execute(db).await;
    let res = sqlx::query("DELETE FROM trees WHERE id = ? AND orchard_id = ?")
        .bind(&tree_id)
        .bind(&orchard_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Tree not found"),
    }
}

async fn list_tree_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let rows = sqlx::query(
        "SELECT mr.*, u.username FROM maintenance_records mr \
         JOIN users u ON u.id = mr.recorded_by WHERE mr.tree_id = ? \
         ORDER BY mr.maintenance_date DESC",
    )
    .bind(&tree_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    ok_json(Value::Array(
        rows.iter().map(|r| maint_record_json(r, "maintenance_type", "maintenance_date")).collect(),
    ))
}

async fn add_tree_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM trees WHERE id = ? AND orchard_id = ?")
        .bind(&tree_id)
        .bind(&orchard_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Tree not found in this orchard");
    }
    let form = form_map(&raw);
    let mtype = fv(&form, "maintenance_type").unwrap_or_else(|| "other".into());
    let date = fv(&form, "maintenance_date")
        .and_then(|d| valid_date(&d))
        .unwrap_or_else(crate::today_str);
    let cost = fv(&form, "cost").and_then(|c| c.parse::<f64>().ok());
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO maintenance_records (id, tree_id, maintenance_type, maintenance_date, description, products_used, cost, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&tree_id)
    .bind(&mtype)
    .bind(&date)
    .bind(fv(&form, "description"))
    .bind(fv(&form, "products_used"))
    .bind(cost)
    .bind(fv(&form, "notes"))
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await;
    ok_json(json!({"id": id, "message": "Maintenance record added successfully"}))
}

async fn update_tree_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id, record_id)): Path<(String, String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let form = form_map(&raw);
    if let Some(v) = fv(&form, "maintenance_type") {
        let _ = sqlx::query("UPDATE maintenance_records SET maintenance_type = ? WHERE id = ? AND tree_id = ?").bind(&v).bind(&record_id).bind(&tree_id).execute(db).await;
    }
    if let Some(d) = fv(&form, "maintenance_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE maintenance_records SET maintenance_date = ? WHERE id = ? AND tree_id = ?").bind(&d).bind(&record_id).bind(&tree_id).execute(db).await;
    }
    if form.contains_key("description") {
        let _ = sqlx::query("UPDATE maintenance_records SET description = ? WHERE id = ? AND tree_id = ?").bind(fv(&form, "description")).bind(&record_id).bind(&tree_id).execute(db).await;
    }
    if form.contains_key("products_used") {
        let _ = sqlx::query("UPDATE maintenance_records SET products_used = ? WHERE id = ? AND tree_id = ?").bind(fv(&form, "products_used")).bind(&record_id).bind(&tree_id).execute(db).await;
    }
    if form.contains_key("cost") {
        let cost = fv(&form, "cost").and_then(|c| c.parse::<f64>().ok());
        let _ = sqlx::query("UPDATE maintenance_records SET cost = ? WHERE id = ? AND tree_id = ?").bind(cost).bind(&record_id).bind(&tree_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let _ = sqlx::query("UPDATE maintenance_records SET notes = ? WHERE id = ? AND tree_id = ?").bind(fv(&form, "notes")).bind(&record_id).bind(&tree_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn delete_tree_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, tree_id, record_id)): Path<(String, String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM maintenance_records WHERE id = ? AND tree_id = ?")
        .bind(&record_id)
        .bind(&tree_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Maintenance record not found"),
    }
}

async fn add_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let form = form_map(&raw);
    let Some(date) = fv(&form, "maintenance_date").and_then(|d| valid_date(&d)) else {
        return err400("Maintenance date is required");
    };
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO orchard_maintenance (id, orchard_id, maintenance_type, maintenance_date, description, products_used, cost, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&orchard_id)
    .bind(fv(&form, "maintenance_type").unwrap_or_else(|| "other".into()))
    .bind(&date)
    .bind(fv(&form, "description"))
    .bind(fv(&form, "products_used"))
    .bind(fv(&form, "cost").and_then(|c| c.parse::<f64>().ok()))
    .bind(fv(&form, "notes"))
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await;
    ok_json(json!({"id": id, "message": "Orchard maintenance recorded"}))
}

async fn update_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, record_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM orchard_maintenance WHERE id = ? AND orchard_id = ?")
        .bind(&record_id)
        .bind(&orchard_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Record not found");
    }
    let form = form_map(&raw);
    if let Some(v) = fv(&form, "maintenance_type") {
        let _ = sqlx::query("UPDATE orchard_maintenance SET maintenance_type = ? WHERE id = ?").bind(&v).bind(&record_id).execute(db).await;
    }
    if let Some(d) = fv(&form, "maintenance_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE orchard_maintenance SET maintenance_date = ? WHERE id = ?").bind(&d).bind(&record_id).execute(db).await;
    }
    let _ = sqlx::query("UPDATE orchard_maintenance SET description = ?, products_used = ?, cost = ?, notes = ? WHERE id = ?")
        .bind(fv(&form, "description"))
        .bind(fv(&form, "products_used"))
        .bind(fv(&form, "cost").and_then(|c| c.parse::<f64>().ok()))
        .bind(fv(&form, "notes"))
        .bind(&record_id)
        .execute(db)
        .await;
    ok_json(json!({"message": "Updated"}))
}

async fn delete_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((orchard_id, record_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM orchard_maintenance WHERE id = ? AND orchard_id = ?")
        .bind(&record_id)
        .bind(&orchard_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted"})),
        _ => err404("Record not found"),
    }
}

async fn stats(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(orchard_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !user_has_access(db, "orchards", "orchard_shares", "orchard_id", &orchard_id, user.id).await {
        return err403("Access denied");
    }
    let total = count(db, "SELECT COUNT(*) FROM trees WHERE orchard_id = ?", &orchard_id).await;
    let active = count(db, "SELECT COUNT(*) FROM trees WHERE orchard_id = ? AND is_active = 1", &orchard_id).await;
    let total_maint = count(
        db,
        "SELECT COUNT(*) FROM maintenance_records mr JOIN trees t ON mr.tree_id = t.id WHERE t.orchard_id = ?",
        &orchard_id,
    )
    .await;
    let rows = sqlx::query(
        "SELECT tree_type, COUNT(*) AS c FROM trees WHERE orchard_id = ? AND is_active = 1 GROUP BY tree_type",
    )
    .bind(&orchard_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let mut types = serde_json::Map::new();
    for r in rows {
        types.insert(
            r.try_get::<String, _>("tree_type").unwrap_or_default(),
            json!(r.get::<i64, _>("c")),
        );
    }
    ok_json(json!({
        "total_trees": total,
        "active_trees": active,
        "total_maintenance": total_maint,
        "tree_types": Value::Object(types),
    }))
}
