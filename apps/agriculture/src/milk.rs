//! Milk app: animal groups with production and medical records.

use axum::body::Body;
use axum::extract::{Path, State};
use crate::AgriForm;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{
    add_share, days_before_today, err400, err403, err404, form_map, fv, new_uuid, now_str, ok_json,
    page, parse_json_body, redirect, revoke_share, shareable_users, shared_users, user_has_access,
    user_json, valid_date,
};

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/milk", get(root))
        .route("/milk/", get(root))
        .route("/milk/group/new", get(new_group_page).post(create_group))
        .route("/milk/group/{group_id}/edit", get(edit_group_page).post(update_group))
        .route("/milk/group/{group_id}/delete", post(delete_group))
        .route("/milk/groups/{group_id}/share", post(share_group))
        .route("/milk/groups/{group_id}/revoke/{share_id}", post(revoke_group))
        .route("/milk/groups/{group_id}", get(view_group))
        .route("/milk/groups/{group_id}/members", get(members_page))
        .route("/milk/groups/{group_id}/reports", get(reports_page))
        .route("/milk/groups/{group_id}/production", get(production_page))
        .route("/milk/groups/{group_id}/animals/{animal_id}/medical", get(medical_page))
        .route("/milk/groups/{group_id}/animals/list", get(api_list_animals))
        .route("/milk/groups/{group_id}/animals/create", post(api_create_animal))
        .route("/milk/groups/{group_id}/animals/{animal_id}", put(api_update_animal).delete(api_delete_animal))
        .route("/milk/groups/{group_id}/animals/{animal_id}/toggle", axum::routing::patch(api_toggle_producing))
        .route("/milk/groups/{group_id}/animals/{animal_id}/medical/add", post(api_add_medical))
        .route("/milk/groups/{group_id}/animals/{animal_id}/medical/{record_id}", put(api_update_medical).delete(api_delete_medical))
        .route("/milk/groups/{group_id}/production/add", post(api_add_production))
        .route("/milk/groups/{group_id}/production/{record_id}", put(api_update_production).delete(api_delete_production))
        .route("/milk/groups/{group_id}/reports/summary", post(report_summary))
        .route("/milk/groups/{group_id}/reports/details", post(report_details))
        .route("/milk/groups/{group_id}/reports/export", post(report_export))
        .route("/milk/groups/{group_id}/reports/producers", post(report_producers))
        .route("/milk/groups/{group_id}/stats", get(api_group_stats))
}

fn group_row(r: &sqlx::sqlite::SqliteRow) -> Value {
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

async fn get_group(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM milk_groups WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| group_row(&r))
}

fn animal_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "animal_type": r.try_get::<String,_>("animal_type").unwrap_or_default(),
        "breed": r.try_get::<Option<String>,_>("breed").ok().flatten(),
        "acquisition_date": r.try_get::<Option<String>,_>("acquisition_date").ok().flatten(),
        "is_producing": r.get::<i64,_>("is_producing") == 1,
        "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
    })
}

async fn has_access(db: &syarc_plugin_api::Db, group_id: &str, user_id: i64) -> bool {
    user_has_access(db, "milk_groups", "milk_group_shares", "group_id", group_id, user_id).await
}

async fn root(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let mut groups: Vec<Value> = Vec::new();

    let own = sqlx::query("SELECT * FROM milk_groups WHERE user_id = ? ORDER BY name")
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    for r in &own {
        let id = r.get::<String, _>("id");
        let (animal_count, producing_count) = animal_counts(db, &id).await;
        groups.push(json!({
            "group": group_row(r), "is_owner": true,
            "animal_count": animal_count, "producing_count": producing_count, "shared_at": null,
        }));
    }

    let shared = sqlx::query(
        "SELECT g.*, u.username AS owner_username, sh.created_at AS shared_at \
         FROM milk_group_shares sh JOIN milk_groups g ON sh.group_id = g.id \
         JOIN users u ON u.id = g.user_id WHERE sh.shared_with_user_id = ? ORDER BY g.name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &shared {
        let id = r.get::<String, _>("id");
        let (animal_count, producing_count) = animal_counts(db, &id).await;
        groups.push(json!({
            "group": group_row(r), "is_owner": false,
            "animal_count": animal_count, "producing_count": producing_count,
            "shared_at": r.try_get::<String,_>("shared_at").unwrap_or_default(),
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
        }));
    }

    page(
        &state,
        "milk/milk_select.html",
        json!({"user": user_json(&user), "groups": groups}),
    )
}

async fn animal_counts(db: &syarc_plugin_api::Db, group_id: &str) -> (i64, i64) {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM milk_animals WHERE group_id = ?")
        .bind(group_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let producing: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM milk_animals WHERE group_id = ? AND is_producing = 1")
            .bind(group_id)
            .fetch_one(db)
            .await
            .unwrap_or(0);
    (total, producing)
}

async fn new_group_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    page(
        &state,
        "milk/milk_group_form.html",
        json!({"user": user_json(&user), "group": null, "mode": "new"}),
    )
}

async fn create_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect("/milk/group/new");
    };
    let description = fv(&form, "description");
    let now = now_str();
    let _ = sqlx::query(
        "INSERT INTO milk_groups (id, user_id, name, description, is_active, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(new_uuid())
    .bind(user.id)
    .bind(&name)
    .bind(&description)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await;
    redirect("/milk/")
}

async fn edit_group_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let group = sqlx::query("SELECT * FROM milk_groups WHERE id = ? AND user_id = ?")
        .bind(&group_id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|r| group_row(&r));
    match group {
        None => redirect("/milk/"),
        Some(group) => page(
            &state,
            "milk/milk_group_form.html",
            json!({"user": user_json(&user), "group": group, "mode": "edit"}),
        ),
    }
}

async fn update_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("/milk/group/{group_id}/edit"));
    };
    let description = fv(&form, "description");
    let _ = sqlx::query(
        "UPDATE milk_groups SET name = ?, description = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(&name)
    .bind(&description)
    .bind(now_str())
    .bind(&group_id)
    .bind(user.id)
    .execute(&state.db)
    .await;
    redirect("/milk/")
}

async fn delete_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    let owned: Option<String> =
        sqlx::query_scalar("SELECT id FROM milk_groups WHERE id = ? AND user_id = ?")
            .bind(&group_id)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if owned.is_some() {
        let _ = sqlx::query("DELETE FROM milk_production WHERE animal_id IN (SELECT id FROM milk_animals WHERE group_id = ?)").bind(&group_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM milk_medical_records WHERE animal_id IN (SELECT id FROM milk_animals WHERE group_id = ?)").bind(&group_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM milk_animals WHERE group_id = ?").bind(&group_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM milk_group_shares WHERE group_id = ?").bind(&group_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM milk_groups WHERE id = ?").bind(&group_id).execute(db).await;
    }
    redirect("/milk/")
}

async fn share_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM milk_groups WHERE id = ?")
        .bind(&group_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/milk/");
    }
    if let Some(target) = fv(&form, "share_with_user_id") {
        let _ = add_share(db, "milk_group_shares", "group_id", &group_id, user.id, &target).await;
    }
    redirect(&format!("/milk/groups/{group_id}"))
}

async fn revoke_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, share_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM milk_groups WHERE id = ?")
        .bind(&group_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/milk/");
    }
    revoke_share(db, "milk_group_shares", "group_id", &group_id, &share_id).await;
    redirect(&format!("/milk/groups/{group_id}"))
}

async fn view_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(group) = get_group(db, &group_id).await else {
        return err404("Group not found");
    };
    let is_owner = group["user_id"].as_i64() == Some(user.id);
    if !is_owner && !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let (shared, all_users) = if is_owner {
        (
            shared_users(db, "milk_group_shares", "group_id", &group_id).await,
            shareable_users(db, "milk_group_shares", "group_id", &group_id, user.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    page(
        &state,
        "milk/milk_dashboard.html",
        json!({
            "user": user_json(&user), "group": group, "is_owner": is_owner,
            "shared_users": shared, "all_users": all_users,
        }),
    )
}

async fn members_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let group = get_group(db, &group_id).await;
    page(
        &state,
        "milk/milk_members.html",
        json!({
            "user": user_json(&user), "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
        }),
    )
}

async fn reports_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let animals: Vec<Value> = sqlx::query("SELECT * FROM milk_animals WHERE group_id = ? ORDER BY name")
        .bind(&group_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(animal_row)
        .collect();
    let group = get_group(db, &group_id).await;
    page(
        &state,
        "milk/milk_reports.html",
        json!({
            "user": user_json(&user), "animals": animals, "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
        }),
    )
}

async fn production_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let group = get_group(db, &group_id).await;
    let records: Vec<Value> = sqlx::query(
        "SELECT mp.id, mp.collection_date, mp.amount, mp.unit, mp.notes, \
                a.name AS animal_name, a.id AS animal_id, u.username AS recorded_by \
         FROM milk_production mp JOIN milk_animals a ON mp.animal_id = a.id \
         JOIN users u ON u.id = mp.recorded_by \
         WHERE a.group_id = ? ORDER BY mp.collection_date DESC LIMIT 50",
    )
    .bind(&group_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<String,_>("id"),
            "collection_date": r.try_get::<String,_>("collection_date").unwrap_or_default(),
            "animal_name": r.try_get::<String,_>("animal_name").unwrap_or_default(),
            "animal_id": r.get::<String,_>("animal_id"),
            "amount": r.try_get::<f64,_>("amount").unwrap_or(0.0),
            "unit": r.try_get::<String,_>("unit").unwrap_or_default(),
            "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
            "recorded_by": r.try_get::<String,_>("recorded_by").unwrap_or_default(),
        })
    })
    .collect();
    let producing: Vec<Value> = sqlx::query(
        "SELECT id, name FROM milk_animals WHERE is_producing = 1 AND group_id = ? ORDER BY name",
    )
    .bind(&group_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| json!({"id": r.get::<String,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()}))
    .collect();
    page(
        &state,
        "milk/milk_production.html",
        json!({
            "user": user_json(&user), "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
            "records": records, "producing_animals": producing,
        }),
    )
}

async fn medical_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let group = get_group(db, &group_id).await;
    let animal = sqlx::query("SELECT * FROM milk_animals WHERE id = ? AND group_id = ?")
        .bind(&animal_id)
        .bind(&group_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| animal_row(&r));
    let Some(animal) = animal else {
        return err404("Animal not found");
    };
    let records: Vec<Value> = sqlx::query(
        "SELECT m.id, m.record_date, m.description, m.cost, m.notes, u.username AS recorded_by \
         FROM milk_medical_records m JOIN users u ON u.id = m.recorded_by \
         WHERE m.animal_id = ? ORDER BY m.record_date DESC",
    )
    .bind(&animal_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<String,_>("id"),
            "record_date": r.try_get::<String,_>("record_date").unwrap_or_default(),
            "description": r.try_get::<String,_>("description").unwrap_or_default(),
            "cost": r.try_get::<Option<f64>,_>("cost").ok().flatten(),
            "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
            "recorded_by": r.try_get::<String,_>("recorded_by").unwrap_or_default(),
        })
    })
    .collect();
    page(
        &state,
        "milk/milk_medical.html",
        json!({
            "user": user_json(&user), "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
            "animal": animal, "medical_records": records,
        }),
    )
}

// --- Animal API ---

async fn api_list_animals(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let animals: Vec<Value> = sqlx::query("SELECT * FROM milk_animals WHERE group_id = ? ORDER BY name")
        .bind(&group_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(animal_row)
        .collect();
    ok_json(Value::Array(animals))
}

async fn api_create_animal(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let Some(name) = fv(&form, "name") else {
        return err400("Name is required");
    };
    let animal_type = fv(&form, "animal_type").unwrap_or_else(|| "other".into());
    let breed = fv(&form, "breed");
    let acquisition_date = fv(&form, "acquisition_date").and_then(|d| valid_date(&d));
    let is_producing = if form.contains_key("is_producing") { form["is_producing"] == *"on" } else { false };
    let notes = fv(&form, "notes");
    let id = new_uuid();
    let ok = sqlx::query(
        "INSERT INTO milk_animals (id, name, animal_type, breed, acquisition_date, is_producing, notes, group_id, created_at, created_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&animal_type)
    .bind(&breed)
    .bind(&acquisition_date)
    .bind(if is_producing { 1 } else { 0 })
    .bind(&notes)
    .bind(&group_id)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return err400("Could not create animal");
    }
    ok_json(json!({"id": id, "message": "Created successfully"}))
}

async fn api_update_animal(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM milk_animals WHERE id = ? AND group_id = ?")
            .bind(&animal_id)
            .bind(&group_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if existing.is_none() {
        return err404("Animal not found");
    }
    if let Some(v) = fv(&form, "name") {
        let _ = sqlx::query("UPDATE milk_animals SET name = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    if let Some(v) = fv(&form, "animal_type") {
        let _ = sqlx::query("UPDATE milk_animals SET animal_type = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    if form.contains_key("breed") {
        let v = fv(&form, "breed");
        let _ = sqlx::query("UPDATE milk_animals SET breed = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    if form.contains_key("acquisition_date") {
        let d = fv(&form, "acquisition_date").and_then(|d| valid_date(&d));
        let _ = sqlx::query("UPDATE milk_animals SET acquisition_date = ? WHERE id = ?").bind(d).bind(&animal_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let v = fv(&form, "notes");
        let _ = sqlx::query("UPDATE milk_animals SET notes = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn api_toggle_producing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let current: Option<i64> =
        sqlx::query_scalar("SELECT is_producing FROM milk_animals WHERE id = ? AND group_id = ?")
            .bind(&animal_id)
            .bind(&group_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    let Some(current) = current else {
        return err404("Animal not found");
    };
    let _ = sqlx::query("UPDATE milk_animals SET is_producing = ? WHERE id = ?")
        .bind(1 - current)
        .bind(&animal_id)
        .execute(db)
        .await;
    ok_json(json!({"message": "Status toggled", "is_producing": current == 0}))
}

async fn api_delete_animal(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM milk_animals WHERE id = ? AND group_id = ?")
            .bind(&animal_id)
            .bind(&group_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if existing.is_none() {
        return err404("Animal not found");
    }
    let _ = sqlx::query("DELETE FROM milk_production WHERE animal_id = ?").bind(&animal_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM milk_medical_records WHERE animal_id = ?").bind(&animal_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM milk_animals WHERE id = ?").bind(&animal_id).execute(db).await;
    ok_json(json!({"message": "Deleted successfully"}))
}

// --- Production API ---

async fn api_add_production(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let (Some(animal_id), Some(date_str), Some(amount_str)) = (
        fv(&form, "animal_id"),
        fv(&form, "collection_date"),
        fv(&form, "amount"),
    ) else {
        return err400("Animal, date, and amount are required");
    };
    let Some(date) = valid_date(&date_str) else {
        return err400("Invalid date");
    };
    let Ok(amount) = amount_str.parse::<f64>() else {
        return err400("Invalid amount");
    };
    let animal: Option<String> =
        sqlx::query_scalar("SELECT id FROM milk_animals WHERE id = ? AND group_id = ?")
            .bind(&animal_id)
            .bind(&group_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if animal.is_none() {
        return err404("Animal not found in this group");
    }
    let unit = fv(&form, "unit").unwrap_or_else(|| "gallons".into());
    let notes = fv(&form, "notes");
    let id = new_uuid();
    let ok = sqlx::query(
        "INSERT INTO milk_production (id, animal_id, collection_date, amount, unit, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&animal_id)
    .bind(&date)
    .bind(amount)
    .bind(&unit)
    .bind(&notes)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return err400("Could not record production");
    }
    ok_json(json!({"id": id, "message": "Production recorded"}))
}

async fn api_update_production(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, record_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM milk_production WHERE id = ?")
        .bind(&record_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if existing.is_none() {
        return err404("Record not found");
    }
    if let Some(d) = fv(&form, "collection_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE milk_production SET collection_date = ? WHERE id = ?").bind(d).bind(&record_id).execute(db).await;
    }
    if let Some(a) = fv(&form, "amount").and_then(|a| a.parse::<f64>().ok()) {
        let _ = sqlx::query("UPDATE milk_production SET amount = ? WHERE id = ?").bind(a).bind(&record_id).execute(db).await;
    }
    if let Some(u) = fv(&form, "unit") {
        let _ = sqlx::query("UPDATE milk_production SET unit = ? WHERE id = ?").bind(u).bind(&record_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let n = fv(&form, "notes");
        let _ = sqlx::query("UPDATE milk_production SET notes = ? WHERE id = ?").bind(n).bind(&record_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn api_delete_production(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, record_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM milk_production WHERE id = ?")
        .bind(&record_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Record not found"),
    }
}

// --- Medical API ---

async fn api_add_medical(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let (Some(date_str), Some(desc)) = (fv(&form, "record_date"), fv(&form, "description")) else {
        return err400("Date and description are required");
    };
    let Some(date) = valid_date(&date_str) else {
        return err400("Invalid date");
    };
    let cost = fv(&form, "cost").and_then(|c| c.parse::<f64>().ok());
    let notes = fv(&form, "notes");
    let id = new_uuid();
    let ok = sqlx::query(
        "INSERT INTO milk_medical_records (id, animal_id, record_date, description, cost, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&animal_id)
    .bind(&date)
    .bind(&desc)
    .bind(cost)
    .bind(&notes)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return err400("Could not add medical record");
    }
    ok_json(json!({"id": id, "message": "Medical record added"}))
}

async fn api_update_medical(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id, record_id)): Path<(String, String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM milk_medical_records WHERE id = ? AND animal_id = ?",
    )
    .bind(&record_id)
    .bind(&animal_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if existing.is_none() {
        return err404("Record not found");
    }
    if let Some(d) = fv(&form, "record_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE milk_medical_records SET record_date = ? WHERE id = ?").bind(d).bind(&record_id).execute(db).await;
    }
    if let Some(v) = fv(&form, "description") {
        let _ = sqlx::query("UPDATE milk_medical_records SET description = ? WHERE id = ?").bind(v).bind(&record_id).execute(db).await;
    }
    if let Some(c) = fv(&form, "cost").and_then(|c| c.parse::<f64>().ok()) {
        let _ = sqlx::query("UPDATE milk_medical_records SET cost = ? WHERE id = ?").bind(c).bind(&record_id).execute(db).await;
    } else if form.contains_key("cost") {
        let _ = sqlx::query("UPDATE milk_medical_records SET cost = NULL WHERE id = ?").bind(&record_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let n = fv(&form, "notes");
        let _ = sqlx::query("UPDATE milk_medical_records SET notes = ? WHERE id = ?").bind(n).bind(&record_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn api_delete_medical(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id, record_id)): Path<(String, String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM milk_medical_records WHERE id = ? AND animal_id = ?")
        .bind(&record_id)
        .bind(&animal_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Record not found"),
    }
}

// --- Reports ---

fn report_filters(body: &Value) -> Option<(String, String, Option<String>)> {
    let start = body.get("start_date")?.as_str()?.to_string();
    let end = body.get("end_date")?.as_str()?.to_string();
    let animal = body.get("animal_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
    Some((start, end, animal))
}

async fn report_rows(
    db: &syarc_plugin_api::Db,
    group_id: &str,
    start: &str,
    end: &str,
    animal: &Option<String>,
) -> Vec<(String, String, f64, String, String)> {
    let mut sql = String::from(
        "SELECT mp.collection_date, a.name AS animal_name, mp.amount, mp.unit, mp.notes \
         FROM milk_production mp JOIN milk_animals a ON mp.animal_id = a.id \
         WHERE mp.collection_date >= ? AND mp.collection_date <= ? AND a.group_id = ?",
    );
    if animal.is_some() {
        sql.push_str(" AND mp.animal_id = ?");
    }
    sql.push_str(" ORDER BY mp.collection_date DESC");
    let mut q = sqlx::query(&sql).bind(start).bind(end).bind(group_id);
    if let Some(a) = animal {
        q = q.bind(a);
    }
    q.fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            (
                r.try_get::<String, _>("collection_date").unwrap_or_default(),
                r.try_get::<String, _>("animal_name").unwrap_or_default(),
                r.try_get::<f64, _>("amount").unwrap_or(0.0),
                r.try_get::<String, _>("unit").unwrap_or_default(),
                r.try_get::<Option<String>, _>("notes").ok().flatten().unwrap_or_default(),
            )
        })
        .collect()
}

async fn report_summary(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, _)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let sql = "SELECT COALESCE(SUM(mp.amount), 0), COUNT(DISTINCT mp.collection_date) \
               FROM milk_production mp JOIN milk_animals a ON mp.animal_id = a.id \
               WHERE mp.collection_date >= ? AND mp.collection_date <= ? AND a.group_id = ?";
    let (total_milk, total_days): (f64, i64) = sqlx::query(sql)
        .bind(&start)
        .bind(&end)
        .bind(&group_id)
        .fetch_one(db)
        .await
        .map(|r| (r.get::<f64, _>(0), r.get::<i64, _>(1)))
        .unwrap_or((0.0, 0));
    let total_animals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM milk_animals WHERE group_id = ? AND is_producing = 1",
    )
    .bind(&group_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let average = if total_days > 0 { total_milk / total_days as f64 } else { 0.0 };
    ok_json(json!({
        "total_milk": (total_milk * 100.0).round() / 100.0,
        "total_days": total_days,
        "average_per_day": (average * 100.0).round() / 100.0,
        "total_animals": total_animals,
    }))
}

async fn report_details(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, animal)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let rows = report_rows(db, &group_id, &start, &end, &animal).await;
    ok_json(Value::Array(
        rows.into_iter()
            .map(|(d, name, amount, unit, notes)| {
                json!({"collection_date": d, "animal_name": name, "amount": amount, "unit": unit, "notes": notes})
            })
            .collect(),
    ))
}

async fn report_export(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, animal)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let rows = report_rows(db, &group_id, &start, &end, &animal).await;
    let mut csv = String::from("Date,Animal,Amount,Unit,Notes\n");
    for (d, name, amount, unit, notes) in rows {
        csv.push_str(&format!("{d},{name},{amount},{unit},{notes}\n"));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv")
        .header(
            header::CONTENT_DISPOSITION,
            r#"attachment; filename="milk_production.csv""#,
        )
        .body(Body::from(csv))
        .unwrap()
}

async fn report_producers(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let Some((start, end, animal)) = report_filters(&parse_json_body(&body)) else {
        return err400("start_date and end_date required");
    };
    let mut sql = String::from(
        "SELECT a.id, a.name, a.is_producing, SUM(mp.amount) AS total_milk, \
                COUNT(DISTINCT mp.collection_date) AS days_active \
         FROM milk_animals a JOIN milk_production mp ON mp.animal_id = a.id \
         WHERE mp.collection_date >= ? AND mp.collection_date <= ? AND a.group_id = ?",
    );
    if animal.is_some() {
        sql.push_str(" AND mp.animal_id = ?");
    }
    sql.push_str(" GROUP BY a.id, a.name, a.is_producing ORDER BY total_milk DESC");
    let mut q = sqlx::query(&sql).bind(&start).bind(&end).bind(&group_id);
    if let Some(a) = &animal {
        q = q.bind(a);
    }
    let producers: Vec<Value> = q
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let total: f64 = r.try_get::<f64, _>("total_milk").unwrap_or(0.0);
            json!({
                "animal_id": r.get::<String,_>("id"),
                "animal_name": r.try_get::<String,_>("name").unwrap_or_default(),
                "is_producing": r.get::<i64,_>("is_producing") == 1,
                "total_milk": (total * 100.0).round() / 100.0,
                "days_active": r.get::<i64,_>("days_active"),
            })
        })
        .collect();
    let total = producers.len();
    ok_json(json!({
        "producers": producers, "total_producers": total,
        "period_start": start, "period_end": end,
    }))
}

// --- Stats ---

async fn api_group_stats(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let total_animals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM milk_animals WHERE group_id = ?")
        .bind(&group_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let producing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM milk_animals WHERE group_id = ? AND is_producing = 1",
    )
    .bind(&group_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let today = crate::today_str();
    let today_total: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(mp.amount), 0) FROM milk_production mp \
         JOIN milk_animals a ON mp.animal_id = a.id \
         WHERE a.group_id = ? AND mp.collection_date = ?",
    )
    .bind(&group_id)
    .bind(&today)
    .fetch_one(db)
    .await
    .unwrap_or(0.0);
    let week_total: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(mp.amount), 0) FROM milk_production mp \
         JOIN milk_animals a ON mp.animal_id = a.id \
         WHERE a.group_id = ? AND mp.collection_date >= ?",
    )
    .bind(&group_id)
    .bind(days_before_today(7))
    .fetch_one(db)
    .await
    .unwrap_or(0.0);
    ok_json(json!({
        "total_animals": total_animals, "producing": producing,
        "today_total": (today_total * 100.0).round() / 100.0,
        "week_total": (week_total * 100.0).round() / 100.0,
    }))
}
