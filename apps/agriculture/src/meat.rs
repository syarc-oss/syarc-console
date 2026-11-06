//! Meat app: animal groups with quantity, status, medical/process/eval records.

use axum::extract::{Path, State};
use crate::AgriForm;
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{
    add_share, err400, err403, err404, form_map, fv, new_uuid, now_str, ok_json, page, redirect,
    revoke_share, shareable_users, shared_users, user_has_access, user_json, valid_date,
};

pub const DISPOSITIONS: [&str; 4] = ["frozen", "canned", "dried", "fresh"];

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/meat", get(root))
        .route("/meat/", get(root))
        .route("/meat/group/new", get(new_group_page).post(create_group))
        .route("/meat/group/{group_id}/edit", get(edit_group_page).post(update_group))
        .route("/meat/group/{group_id}/delete", post(delete_group))
        .route("/meat/groups/{group_id}/share", post(share_group))
        .route("/meat/groups/{group_id}/revoke/{share_id}", post(revoke_group))
        .route("/meat/groups/{group_id}", get(view_group))
        .route("/meat/groups/{group_id}/members", get(members_page))
        .route("/meat/groups/{group_id}/animals/{animal_id}/medical", get(medical_page))
        .route("/meat/groups/{group_id}/animals/{animal_id}/process", get(process_page))
        .route("/meat/groups/{group_id}/animals/{animal_id}/eval", get(eval_page))
        .route("/meat/groups/{group_id}/animals/list", get(api_list_animals))
        .route("/meat/groups/{group_id}/animals/create", post(api_create_animal))
        .route("/meat/groups/{group_id}/animals/{animal_id}", put(api_update_animal).delete(api_delete_animal))
        .route("/meat/groups/{group_id}/animals/{animal_id}/split-breeder", post(api_split_breeder))
        .route("/meat/groups/{group_id}/animals/{animal_id}/medical/add", post(api_add_medical))
        .route("/meat/groups/{group_id}/animals/{animal_id}/medical/{record_id}", delete(api_delete_medical))
        .route("/meat/groups/{group_id}/animals/{animal_id}/process/add", post(api_add_process))
        .route("/meat/groups/{group_id}/animals/{animal_id}/process/{record_id}", delete(api_delete_process))
        .route("/meat/groups/{group_id}/animals/{animal_id}/eval/add", post(api_add_eval))
        .route("/meat/groups/{group_id}/animals/{animal_id}/eval/{record_id}", put(api_update_eval).delete(api_delete_eval))
        .route("/meat/groups/{group_id}/stats", get(api_group_stats))
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
    sqlx::query("SELECT * FROM meat_groups WHERE id = ?")
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
        "quantity": r.get::<i64,_>("quantity"),
        "acquisition_date": r.try_get::<Option<String>,_>("acquisition_date").ok().flatten(),
        "status": r.try_get::<String,_>("status").unwrap_or_default(),
        "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
    })
}

async fn get_animal(db: &syarc_plugin_api::Db, group_id: &str, animal_id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM meat_animals WHERE id = ? AND group_id = ?")
        .bind(animal_id)
        .bind(group_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| animal_row(&r))
}

async fn has_access(db: &syarc_plugin_api::Db, group_id: &str, user_id: i64) -> bool {
    user_has_access(db, "meat_groups", "meat_group_shares", "group_id", group_id, user_id).await
}

async fn root(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let mut groups: Vec<Value> = Vec::new();

    let own = sqlx::query("SELECT * FROM meat_groups WHERE user_id = ? ORDER BY name")
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    for r in &own {
        let id = r.get::<String, _>("id");
        let (animal_count, active_count) = animal_counts(db, &id).await;
        groups.push(json!({
            "group": group_row(r), "is_owner": true,
            "animal_count": animal_count, "active_count": active_count, "shared_at": null,
        }));
    }

    let shared = sqlx::query(
        "SELECT g.*, u.username AS owner_username, sh.created_at AS shared_at \
         FROM meat_group_shares sh JOIN meat_groups g ON sh.group_id = g.id \
         JOIN users u ON u.id = g.user_id WHERE sh.shared_with_user_id = ? ORDER BY g.name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &shared {
        let id = r.get::<String, _>("id");
        let (animal_count, active_count) = animal_counts(db, &id).await;
        groups.push(json!({
            "group": group_row(r), "is_owner": false,
            "animal_count": animal_count, "active_count": active_count,
            "shared_at": r.try_get::<String,_>("shared_at").unwrap_or_default(),
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
        }));
    }

    page(
        &state,
        "meat/meat_select.html",
        json!({"user": user_json(&user), "groups": groups}),
    )
}

async fn animal_counts(db: &syarc_plugin_api::Db, group_id: &str) -> (i64, i64) {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meat_animals WHERE group_id = ?")
        .bind(group_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meat_animals WHERE group_id = ? AND status IN ('growing', 'breeder')",
    )
    .bind(group_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    (total, active)
}

async fn new_group_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    page(
        &state,
        "meat/meat_group_form.html",
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
        return redirect("/meat/group/new");
    };
    let description = fv(&form, "description");
    let now = now_str();
    let _ = sqlx::query(
        "INSERT INTO meat_groups (id, user_id, name, description, is_active, created_at, updated_at) \
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
    redirect("/meat/")
}

async fn edit_group_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let group = sqlx::query("SELECT * FROM meat_groups WHERE id = ? AND user_id = ?")
        .bind(&group_id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|r| group_row(&r));
    match group {
        None => redirect("/meat/"),
        Some(group) => page(
            &state,
            "meat/meat_group_form.html",
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
        return redirect(&format!("/meat/group/{group_id}/edit"));
    };
    let description = fv(&form, "description");
    let _ = sqlx::query(
        "UPDATE meat_groups SET name = ?, description = ?, updated_at = ? WHERE id = ? AND user_id = ?",
    )
    .bind(&name)
    .bind(&description)
    .bind(now_str())
    .bind(&group_id)
    .bind(user.id)
    .execute(&state.db)
    .await;
    redirect("/meat/")
}

async fn delete_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
) -> Response {
    let db = &state.db;
    let owned: Option<String> =
        sqlx::query_scalar("SELECT id FROM meat_groups WHERE id = ? AND user_id = ?")
            .bind(&group_id)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if owned.is_some() {
        let animals = sqlx::query("SELECT id FROM meat_animals WHERE group_id = ?")
            .bind(&group_id)
            .fetch_all(db)
            .await
            .unwrap_or_default();
        for r in &animals {
            let id = r.get::<String, _>("id");
            let _ = sqlx::query("DELETE FROM meat_medical_records WHERE animal_id = ?").bind(&id).execute(db).await;
            let _ = sqlx::query("DELETE FROM meat_process_records WHERE animal_id = ?").bind(&id).execute(db).await;
            let _ = sqlx::query("DELETE FROM meat_eval_records WHERE animal_id = ?").bind(&id).execute(db).await;
        }
        let _ = sqlx::query("DELETE FROM meat_animals WHERE group_id = ?").bind(&group_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM meat_group_shares WHERE group_id = ?").bind(&group_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM meat_groups WHERE id = ?").bind(&group_id).execute(db).await;
    }
    redirect("/meat/")
}

async fn share_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(group_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM meat_groups WHERE id = ?")
        .bind(&group_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/meat/");
    }
    if let Some(target) = fv(&form, "share_with_user_id") {
        let _ = add_share(db, "meat_group_shares", "group_id", &group_id, user.id, &target).await;
    }
    redirect(&format!("/meat/groups/{group_id}"))
}

async fn revoke_group(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, share_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM meat_groups WHERE id = ?")
        .bind(&group_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/meat/");
    }
    revoke_share(db, "meat_group_shares", "group_id", &group_id, &share_id).await;
    redirect(&format!("/meat/groups/{group_id}"))
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
            shared_users(db, "meat_group_shares", "group_id", &group_id).await,
            shareable_users(db, "meat_group_shares", "group_id", &group_id, user.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    page(
        &state,
        "meat/meat_dashboard.html",
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
        "meat/meat_members.html",
        json!({
            "user": user_json(&user), "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
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
    let Some(animal) = get_animal(db, &group_id, &animal_id).await else {
        return err404("Animal not found");
    };
    let records: Vec<Value> = sqlx::query(
        "SELECT m.id, m.record_date, m.description, m.cost, m.notes, u.username AS recorded_by \
         FROM meat_medical_records m JOIN users u ON u.id = m.recorded_by \
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
        "meat/meat_medical.html",
        json!({
            "user": user_json(&user), "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
            "animal": animal, "medical_records": records,
        }),
    )
}

async fn process_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let group = get_group(db, &group_id).await;
    let Some(animal) = get_animal(db, &group_id, &animal_id).await else {
        return err404("Animal not found");
    };
    let records: Vec<Value> = sqlx::query(
        "SELECT p.id, p.process_date, p.number_processed, p.lbs, p.disposition, p.storage_location, p.notes, \
                u.username AS recorded_by \
         FROM meat_process_records p JOIN users u ON u.id = p.recorded_by \
         WHERE p.animal_id = ? ORDER BY p.process_date DESC",
    )
    .bind(&animal_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<String,_>("id"),
            "process_date": r.try_get::<String,_>("process_date").unwrap_or_default(),
            "number_processed": r.get::<i64,_>("number_processed"),
            "lbs": r.try_get::<Option<f64>,_>("lbs").ok().flatten(),
            "disposition": r.try_get::<String,_>("disposition").unwrap_or_default(),
            "storage_location": r.try_get::<Option<String>,_>("storage_location").ok().flatten(),
            "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
            "recorded_by": r.try_get::<String,_>("recorded_by").unwrap_or_default(),
        })
    })
    .collect();
    page(
        &state,
        "meat/meat_process.html",
        json!({
            "user": user_json(&user), "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
            "animal": animal, "process_records": records,
            "dispositions": DISPOSITIONS,
        }),
    )
}

async fn eval_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let group = get_group(db, &group_id).await;
    let Some(animal) = get_animal(db, &group_id, &animal_id).await else {
        return err404("Animal not found");
    };
    let acquisition = animal["acquisition_date"].as_str().map(String::from);
    let records: Vec<Value> = sqlx::query(
        "SELECT e.id, e.eval_date, e.weight, e.bcs, e.notes, u.username AS recorded_by \
         FROM meat_eval_records e JOIN users u ON u.id = e.recorded_by \
         WHERE e.animal_id = ? ORDER BY e.eval_date DESC",
    )
    .bind(&animal_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        let eval_date = r.try_get::<String, _>("eval_date").unwrap_or_default();
        let days_since = acquisition.as_ref().and_then(|acq| {
            chrono::NaiveDate::parse_from_str(&eval_date, "%Y-%m-%d").ok().zip(
                chrono::NaiveDate::parse_from_str(acq, "%Y-%m-%d").ok(),
            )
        }).map(|(e, a)| (e - a).num_days());
        json!({
            "id": r.get::<String,_>("id"),
            "eval_date": eval_date,
            "days_since": days_since,
            "weight": r.try_get::<Option<f64>,_>("weight").ok().flatten(),
            "bcs": r.get::<i64,_>("bcs"),
            "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
            "recorded_by": r.try_get::<String,_>("recorded_by").unwrap_or_default(),
        })
    })
    .collect();
    page(
        &state,
        "meat/meat_eval.html",
        json!({
            "user": user_json(&user), "group_id": group_id,
            "group_name": group.map(|g| g["name"].clone()).unwrap_or(json!("Group")),
            "animal": animal, "eval_records": records,
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
    let animals: Vec<Value> = sqlx::query("SELECT * FROM meat_animals WHERE group_id = ? ORDER BY name")
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
    let quantity: i64 = fv(&form, "quantity").and_then(|q| q.parse().ok()).unwrap_or(1);
    let acquisition_date = fv(&form, "acquisition_date").and_then(|d| valid_date(&d));
    let notes = fv(&form, "notes");
    let id = new_uuid();
    let ok = sqlx::query(
        "INSERT INTO meat_animals (id, name, animal_type, breed, quantity, acquisition_date, status, notes, group_id, created_at, created_by) \
         VALUES (?, ?, ?, ?, ?, ?, 'growing', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&animal_type)
    .bind(&breed)
    .bind(quantity)
    .bind(&acquisition_date)
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
    if get_animal(db, &group_id, &animal_id).await.is_none() {
        return err404("Animal not found");
    }
    if let Some(v) = fv(&form, "name") {
        let _ = sqlx::query("UPDATE meat_animals SET name = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    if let Some(v) = fv(&form, "animal_type") {
        let _ = sqlx::query("UPDATE meat_animals SET animal_type = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    if form.contains_key("breed") {
        let v = fv(&form, "breed");
        let _ = sqlx::query("UPDATE meat_animals SET breed = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    if let Some(q) = fv(&form, "quantity").and_then(|q| q.parse::<i64>().ok()) {
        let _ = sqlx::query("UPDATE meat_animals SET quantity = ? WHERE id = ?").bind(q).bind(&animal_id).execute(db).await;
    }
    if form.contains_key("acquisition_date") {
        let d = fv(&form, "acquisition_date").and_then(|d| valid_date(&d));
        let _ = sqlx::query("UPDATE meat_animals SET acquisition_date = ? WHERE id = ?").bind(d).bind(&animal_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let v = fv(&form, "notes");
        let _ = sqlx::query("UPDATE meat_animals SET notes = ? WHERE id = ?").bind(v).bind(&animal_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn api_split_breeder(
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
    let Some(animal) = get_animal(db, &group_id, &animal_id).await else {
        return err404("Animal not found");
    };
    if animal["status"] == "processed" {
        return err400("Cannot split a processed animal");
    }
    let quantity = animal["quantity"].as_i64().unwrap_or(0);
    if quantity < 2 {
        return err400("Need at least 2 to split");
    }
    let num_breeders: i64 = fv(&form, "num_breeders").and_then(|n| n.parse().ok()).unwrap_or(1);
    if num_breeders < 1 || num_breeders >= quantity {
        return err400(&format!("Must split between 1 and {}", quantity - 1));
    }
    let _ = sqlx::query("UPDATE meat_animals SET quantity = ? WHERE id = ?")
        .bind(quantity - num_breeders)
        .bind(&animal_id)
        .execute(db)
        .await;
    let ok = sqlx::query(
        "INSERT INTO meat_animals (id, name, animal_type, breed, quantity, acquisition_date, status, notes, group_id, created_at, created_by) \
         VALUES (?, ?, ?, ?, ?, ?, 'breeder', ?, ?, ?, ?)",
    )
    .bind(new_uuid())
    .bind(animal["name"].as_str().unwrap_or_default())
    .bind(animal["animal_type"].as_str().unwrap_or_default())
    .bind(opt_str(&animal["breed"]))
    .bind(num_breeders)
    .bind(opt_str(&animal["acquisition_date"]))
    .bind(opt_str(&animal["notes"]))
    .bind(&group_id)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return err400("Could not split animal");
    }
    ok_json(json!({"message": "Split to breeder"}))
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
    if get_animal(db, &group_id, &animal_id).await.is_none() {
        return err404("Animal not found");
    }
    let _ = sqlx::query("DELETE FROM meat_medical_records WHERE animal_id = ?").bind(&animal_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM meat_process_records WHERE animal_id = ?").bind(&animal_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM meat_eval_records WHERE animal_id = ?").bind(&animal_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM meat_animals WHERE id = ?").bind(&animal_id).execute(db).await;
    ok_json(json!({"message": "Deleted successfully"}))
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
        "INSERT INTO meat_medical_records (id, animal_id, record_date, description, cost, notes, created_at, recorded_by) \
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

async fn api_delete_medical(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id, record_id)): Path<(String, String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM meat_medical_records WHERE id = ? AND animal_id = ?")
        .bind(&record_id)
        .bind(&animal_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Record not found"),
    }
}

// --- Process API ---

async fn api_add_process(
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
    let (Some(date_str), Some(num_str), Some(disposition)) = (
        fv(&form, "process_date"),
        fv(&form, "number_processed"),
        fv(&form, "disposition"),
    ) else {
        return err400("Date, number, and disposition are required");
    };
    let Some(date) = valid_date(&date_str) else {
        return err400("Invalid date");
    };
    let Ok(num_processed) = num_str.parse::<i64>() else {
        return err400("Invalid number");
    };
    let Some(animal) = get_animal(db, &group_id, &animal_id).await else {
        return err404("Animal not found");
    };
    if animal["status"] == "processed" {
        return err400("This animal has already been processed");
    }
    let quantity = animal["quantity"].as_i64().unwrap_or(0);
    if num_processed > quantity {
        return err400(&format!("Cannot process more than {quantity} animals"));
    }
    let lbs = fv(&form, "lbs").and_then(|l| l.parse::<f64>().ok());
    let storage_location = fv(&form, "storage_location");
    let notes = fv(&form, "notes");
    let id = new_uuid();
    let inserted = sqlx::query(
        "INSERT INTO meat_process_records (id, animal_id, process_date, number_processed, lbs, disposition, storage_location, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&animal_id)
    .bind(&date)
    .bind(num_processed)
    .bind(lbs)
    .bind(&disposition)
    .bind(&storage_location)
    .bind(&notes)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !inserted {
        return err400("Could not add process record");
    }
    if num_processed == quantity {
        let _ = sqlx::query("UPDATE meat_animals SET status = 'processed' WHERE id = ?")
            .bind(&animal_id)
            .execute(db)
            .await;
    } else {
        let _ = sqlx::query("UPDATE meat_animals SET quantity = ?, status = 'processed' WHERE id = ?")
            .bind(num_processed)
            .bind(&animal_id)
            .execute(db)
            .await;
        let _ = sqlx::query(
            "INSERT INTO meat_animals (id, name, animal_type, breed, quantity, acquisition_date, status, notes, group_id, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, 'growing', ?, ?, ?, ?)",
        )
        .bind(new_uuid())
        .bind(animal["name"].as_str().unwrap_or_default())
        .bind(animal["animal_type"].as_str().unwrap_or_default())
        .bind(opt_str(&animal["breed"]))
        .bind(quantity - num_processed)
        .bind(opt_str(&animal["acquisition_date"]))
        .bind(opt_str(&animal["notes"]))
        .bind(&group_id)
        .bind(now_str())
        .bind(user.id)
        .execute(db)
        .await;
    }
    ok_json(json!({"id": id, "message": "Process record added"}))
}

async fn api_delete_process(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id, record_id)): Path<(String, String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM meat_animals WHERE id = ?")
        .bind(&animal_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if status.as_deref() == Some("processed") {
        return err400("Cannot delete process records for a processed animal");
    }
    let res = sqlx::query("DELETE FROM meat_process_records WHERE id = ? AND animal_id = ?")
        .bind(&record_id)
        .bind(&animal_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Record not found"),
    }
}

// --- Eval API ---

async fn api_add_eval(
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
    let (Some(date_str), Some(bcs_str)) = (fv(&form, "eval_date"), fv(&form, "bcs")) else {
        return err400("Date and BCS are required");
    };
    let Some(date) = valid_date(&date_str) else {
        return err400("Invalid date");
    };
    let Ok(bcs) = bcs_str.parse::<i64>() else {
        return err400("Invalid BCS");
    };
    let weight = fv(&form, "weight").and_then(|w| w.parse::<f64>().ok());
    let notes = fv(&form, "notes");
    let id = new_uuid();
    let ok = sqlx::query(
        "INSERT INTO meat_eval_records (id, animal_id, eval_date, weight, bcs, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&animal_id)
    .bind(&date)
    .bind(weight)
    .bind(bcs)
    .bind(&notes)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return err400("Could not add eval record");
    }
    ok_json(json!({"id": id, "message": "Eval record added"}))
}

async fn api_update_eval(
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
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM meat_eval_records WHERE id = ? AND animal_id = ?")
            .bind(&record_id)
            .bind(&animal_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if existing.is_none() {
        return err404("Record not found");
    }
    if let Some(d) = fv(&form, "eval_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE meat_eval_records SET eval_date = ? WHERE id = ?").bind(d).bind(&record_id).execute(db).await;
    }
    if let Some(w) = fv(&form, "weight").and_then(|w| w.parse::<f64>().ok()) {
        let _ = sqlx::query("UPDATE meat_eval_records SET weight = ? WHERE id = ?").bind(w).bind(&record_id).execute(db).await;
    } else if form.contains_key("weight") {
        let _ = sqlx::query("UPDATE meat_eval_records SET weight = NULL WHERE id = ?").bind(&record_id).execute(db).await;
    }
    if let Some(b) = fv(&form, "bcs").and_then(|b| b.parse::<i64>().ok()) {
        let _ = sqlx::query("UPDATE meat_eval_records SET bcs = ? WHERE id = ?").bind(b).bind(&record_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let n = fv(&form, "notes");
        let _ = sqlx::query("UPDATE meat_eval_records SET notes = ? WHERE id = ?").bind(n).bind(&record_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn api_delete_eval(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((group_id, animal_id, record_id)): Path<(String, String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &group_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM meat_eval_records WHERE id = ? AND animal_id = ?")
        .bind(&record_id)
        .bind(&animal_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Record not found"),
    }
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
    let total_animals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM meat_animals WHERE group_id = ?")
        .bind(&group_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let growing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meat_animals WHERE group_id = ? AND status = 'growing'",
    )
    .bind(&group_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let breeders: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meat_animals WHERE group_id = ? AND status = 'breeder'",
    )
    .bind(&group_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let processed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meat_animals WHERE group_id = ? AND status = 'processed'",
    )
    .bind(&group_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let total_lbs: Option<f64> = sqlx::query_scalar(
        "SELECT SUM(p.lbs) FROM meat_process_records p JOIN meat_animals a ON p.animal_id = a.id WHERE a.group_id = ?",
    )
    .bind(&group_id)
    .fetch_one(db)
    .await
    .ok()
    .flatten();
    ok_json(json!({
        "total_animals": total_animals, "growing": growing, "breeders": breeders,
        "processed": processed,
        "total_lbs": total_lbs.map(|l| (l * 10.0).round() / 10.0).unwrap_or(0.0),
    }))
}
