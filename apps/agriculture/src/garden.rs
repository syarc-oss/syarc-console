//! Garden app: gardens with crop entries, per-crop and per-garden (plot)
//! maintenance. Mirrors the orchard structure.

use axum::extract::{Path, State};
use crate::AgriForm;
use axum::response::Response;
use axum::routing::{get, put};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{
    add_share, err400, err403, err404, form_map, fv, new_uuid, now_str, ok_json, page, redirect,
    revoke_share, shareable_users, shared_users, user_has_access, user_json, valid_date,
};

pub const CROP_ACTIVITY_TYPES: [&str; 11] = [
    "watering",
    "weeding",
    "fertilizing",
    "soil_testing",
    "pest_management",
    "pruning",
    "mulching",
    "thinning",
    "spraying",
    "harvesting",
    "other",
];

pub const PLOT_ACTIVITY_TYPES: [&str; 13] = [
    "tilling",
    "watering",
    "weeding",
    "fertilizing",
    "soil_testing",
    "mulching",
    "composting",
    "pest_management",
    "spraying",
    "cover_crop",
    "irrigation",
    "bed_preparation",
    "other",
];

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/agriculture/agronomy/garden", get(root).post(create_garden))
        .route("/agriculture/agronomy/garden/", get(root))
        .route("/agriculture/agronomy/garden/new", get(new_garden_page).post(create_garden))
        .route("/agriculture/agronomy/garden/{garden_id}/edit", get(edit_garden_page).post(update_garden))
        .route("/agriculture/agronomy/garden/{garden_id}/delete", axum::routing::post(delete_garden))
        .route("/agriculture/agronomy/garden/{garden_id}/share", axum::routing::post(share_garden))
        .route("/agriculture/agronomy/garden/{garden_id}/revoke/{share_id}", axum::routing::post(revoke_garden))
        .route("/agriculture/agronomy/garden/{garden_id}", get(view_garden))
        .route("/agriculture/agronomy/garden/{garden_id}/maintenance", get(plot_maintenance_page))
        .route("/agriculture/agronomy/garden/{garden_id}/maintenance/add", axum::routing::post(add_plot_maintenance))
        .route("/agriculture/agronomy/garden/{garden_id}/maintenance/{record_id}", put(update_plot_maintenance).delete(delete_plot_maintenance))
        .route("/agriculture/agronomy/garden/{garden_id}/crop/{crop_id}", get(crop_detail))
        .route("/agriculture/agronomy/garden/{garden_id}/crops/create", axum::routing::post(create_crop))
        .route("/agriculture/agronomy/garden/{garden_id}/crops/{crop_id}", put(update_crop).delete(delete_crop))
        .route("/agriculture/agronomy/garden/{garden_id}/crops/{crop_id}/maintenance/add", axum::routing::post(add_crop_maintenance))
        .route("/agriculture/agronomy/garden/{garden_id}/crops/{crop_id}/maintenance/{record_id}", put(update_crop_maintenance).delete(delete_crop_maintenance))
}

const BASE: &str = "/agriculture/agronomy/garden";

fn garden_row(r: &sqlx::sqlite::SqliteRow) -> Value {
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

fn crop_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "garden_id": r.get::<String,_>("garden_id"),
        "plot_section": r.try_get::<Option<String>,_>("plot_section").ok().flatten(),
        "crop": r.try_get::<String,_>("crop").unwrap_or_default(),
        "variety": r.try_get::<Option<String>,_>("variety").ok().flatten(),
        "planting_date": r.try_get::<Option<String>,_>("planting_date").ok().flatten(),
        "planted_by": r.try_get::<Option<String>,_>("planted_by").ok().flatten(),
        "days_to_maturity": r.try_get::<Option<i64>,_>("days_to_maturity").ok().flatten(),
        "first_harvest_date": r.try_get::<Option<String>,_>("first_harvest_date").ok().flatten(),
        "last_harvest_date": r.try_get::<Option<String>,_>("last_harvest_date").ok().flatten(),
        "harvested_by": r.try_get::<Option<String>,_>("harvested_by").ok().flatten(),
        "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
        "is_active": r.get::<i64,_>("is_active") == 1,
    })
}

fn maint_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "activity_type": r.try_get::<String,_>("activity_type").unwrap_or_default(),
        "activity_date": r.try_get::<String,_>("activity_date").unwrap_or_default(),
        "person": r.try_get::<Option<String>,_>("person").ok().flatten(),
        "time_spent": r.try_get::<Option<String>,_>("time_spent").ok().flatten(),
        "materials": r.try_get::<Option<String>,_>("materials").ok().flatten(),
        "cost": r.try_get::<Option<f64>,_>("cost").ok().flatten(),
        "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
        "recorded_by": r.try_get::<String,_>("username").unwrap_or_default(),
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
    })
}

async fn access(db: &syarc_plugin_api::Db, garden_id: &str, user: &syarc_plugin_api::User) -> bool {
    user_has_access(db, "gardens", "garden_shares", "garden_id", garden_id, user.id).await
}

async fn get_garden(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM gardens WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| garden_row(&r))
}

async fn root(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let mut gardens: Vec<Value> = Vec::new();

    let own = sqlx::query("SELECT * FROM gardens WHERE user_id = ? ORDER BY name")
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    for r in &own {
        let id = r.get::<String, _>("id");
        let crops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM garden_crops WHERE garden_id = ?")
            .bind(&id)
            .fetch_one(db)
            .await
            .unwrap_or(0);
        gardens.push(json!({"garden": garden_row(r), "is_owner": true, "crop_count": crops, "owner_username": null}));
    }

    let shared = sqlx::query(
        "SELECT g.*, u.username AS owner_username, sh.created_at AS share_created FROM garden_shares sh \
         JOIN gardens g ON sh.garden_id = g.id JOIN users u ON u.id = g.user_id \
         WHERE sh.shared_with_user_id = ? ORDER BY g.name",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &shared {
        let id = r.get::<String, _>("id");
        let crops: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM garden_crops WHERE garden_id = ?")
            .bind(&id)
            .fetch_one(db)
            .await
            .unwrap_or(0);
        gardens.push(json!({
            "garden": garden_row(r), "is_owner": false, "crop_count": crops,
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
        }));
    }

    page(
        &state,
        "garden/garden_select.html",
        json!({"user": user_json(&user), "gardens": gardens}),
    )
}

async fn new_garden_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    page(
        &state,
        "garden/garden_form.html",
        json!({"user": user_json(&user), "garden": null, "mode": "new"}),
    )
}

async fn create_garden(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("{BASE}/new"));
    };
    let id = new_uuid();
    let now = now_str();
    let _ = sqlx::query(
        "INSERT INTO gardens (id, user_id, name, description, location, is_active, created_at, updated_at) \
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
    redirect(&format!("{BASE}/"))
}

async fn edit_garden_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
) -> Response {
    let garden = sqlx::query("SELECT * FROM gardens WHERE id = ? AND user_id = ?")
        .bind(&garden_id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    match garden {
        Some(r) => page(
            &state,
            "garden/garden_form.html",
            json!({"user": user_json(&user), "garden": garden_row(&r), "mode": "edit"}),
        ),
        None => redirect(&format!("{BASE}/")),
    }
}

async fn update_garden(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM gardens WHERE id = ? AND user_id = ?")
        .bind(&garden_id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return redirect(&format!("{BASE}/"));
    }
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("{BASE}/{garden_id}/edit"));
    };
    let _ = sqlx::query(
        "UPDATE gardens SET name = ?, description = ?, location = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(fv(&form, "description"))
    .bind(fv(&form, "location"))
    .bind(now_str())
    .bind(&garden_id)
    .execute(&state.db)
    .await;
    redirect(&format!("{BASE}/"))
}

async fn delete_garden(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM gardens WHERE id = ? AND user_id = ?")
        .bind(&garden_id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_some() {
        let _ = sqlx::query("DELETE FROM garden_crop_maintenance WHERE crop_id IN (SELECT id FROM garden_crops WHERE garden_id = ?)").bind(&garden_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM garden_crops WHERE garden_id = ?").bind(&garden_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM garden_maintenance WHERE garden_id = ?").bind(&garden_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM garden_shares WHERE garden_id = ?").bind(&garden_id).execute(db).await;
        let _ = sqlx::query("DELETE FROM gardens WHERE id = ?").bind(&garden_id).execute(db).await;
    }
    redirect(&format!("{BASE}/"))
}

async fn share_garden(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM gardens WHERE id = ?")
        .bind(&garden_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect(&format!("{BASE}/"));
    }
    let form = form_map(&raw);
    let Some(target) = fv(&form, "share_with_user_id") else {
        return redirect(&format!("{BASE}/{garden_id}"));
    };
    let _ = add_share(db, "garden_shares", "garden_id", &garden_id, user.id, &target).await;
    redirect(&format!("{BASE}/{garden_id}"))
}

async fn revoke_garden(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, share_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM gardens WHERE id = ?")
        .bind(&garden_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect(&format!("{BASE}/"));
    }
    revoke_share(db, "garden_shares", "garden_id", &garden_id, &share_id).await;
    redirect(&format!("{BASE}/{garden_id}"))
}

async fn view_garden(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(garden) = get_garden(db, &garden_id).await else {
        return err404("Garden not found");
    };
    let is_owner = garden["user_id"].as_i64() == Some(user.id);
    if !is_owner && !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let crops = sqlx::query(
        "SELECT * FROM garden_crops WHERE garden_id = ? ORDER BY plot_section, crop",
    )
    .bind(&garden_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let crops: Vec<Value> = crops.iter().map(crop_row).collect();

    let maint = sqlx::query(
        "SELECT gm.*, u.username FROM garden_maintenance gm \
         JOIN users u ON u.id = gm.recorded_by WHERE gm.garden_id = ? \
         ORDER BY gm.activity_date DESC",
    )
    .bind(&garden_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let plot_maintenance: Vec<Value> = maint.iter().map(maint_json).collect();

    let (shared, all_users) = if is_owner {
        (
            shared_users(db, "garden_shares", "garden_id", &garden_id).await,
            shareable_users(db, "garden_shares", "garden_id", &garden_id, user.id).await,
        )
    } else {
        (vec![], vec![])
    };
    page(
        &state,
        "garden/garden_dashboard.html",
        json!({
            "user": user_json(&user), "garden": garden, "crops": crops,
            "plot_maintenance": plot_maintenance,
            "plot_activity_types": PLOT_ACTIVITY_TYPES,
            "crop_activity_types": CROP_ACTIVITY_TYPES,
            "is_owner": is_owner, "shared_users": shared, "all_users": all_users,
        }),
    )
}

async fn plot_maintenance_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let garden = get_garden(db, &garden_id).await;
    let maint = sqlx::query(
        "SELECT gm.*, u.username FROM garden_maintenance gm \
         JOIN users u ON u.id = gm.recorded_by WHERE gm.garden_id = ? \
         ORDER BY gm.activity_date DESC",
    )
    .bind(&garden_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    page(
        &state,
        "garden/plot_maintenance.html",
        json!({
            "user": user_json(&user),
            "garden": garden,
            "garden_id": garden_id,
            "garden_name": garden.as_ref().map(|g| g["name"].clone()).unwrap_or(json!("Garden")),
            "plot_maintenance": maint.iter().map(maint_json).collect::<Vec<_>>(),
            "plot_activity_types": PLOT_ACTIVITY_TYPES,
            "is_owner": garden.as_ref().map(|g| g["user_id"].as_i64() == Some(user.id)).unwrap_or(false),
        }),
    )
}

async fn crop_detail(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, crop_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let garden = get_garden(db, &garden_id).await;
    let crop = sqlx::query("SELECT * FROM garden_crops WHERE id = ? AND garden_id = ?")
        .bind(&crop_id)
        .bind(&garden_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(crop_r) = crop else {
        return err404("Crop not found");
    };
    let maint = sqlx::query(
        "SELECT cm.*, u.username FROM garden_crop_maintenance cm \
         JOIN users u ON u.id = cm.recorded_by WHERE cm.crop_id = ? \
         ORDER BY cm.activity_date DESC",
    )
    .bind(&crop_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    page(
        &state,
        "garden/crop_detail.html",
        json!({
            "user": user_json(&user),
            "garden": garden,
            "garden_id": garden_id,
            "garden_name": garden.as_ref().map(|g| g["name"].clone()).unwrap_or(json!("Garden")),
            "crop": crop_row(&crop_r),
            "maintenance_records": maint.iter().map(maint_json).collect::<Vec<_>>(),
            "activity_types": CROP_ACTIVITY_TYPES,
            "is_owner": garden.as_ref().map(|g| g["user_id"].as_i64() == Some(user.id)).unwrap_or(false),
        }),
    )
}

async fn create_crop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let form = form_map(&raw);
    let Some(crop) = fv(&form, "crop") else {
        return err400("Crop name is required");
    };
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO garden_crops (id, garden_id, plot_section, crop, variety, planting_date, planted_by, days_to_maturity, first_harvest_date, last_harvest_date, harvested_by, notes, is_active, created_at, created_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(&garden_id)
    .bind(fv(&form, "plot_section"))
    .bind(&crop)
    .bind(fv(&form, "variety"))
    .bind(fv(&form, "planting_date").and_then(|d| valid_date(&d)))
    .bind(fv(&form, "planted_by"))
    .bind(fv(&form, "days_to_maturity").and_then(|v| v.parse::<i64>().ok()))
    .bind(fv(&form, "first_harvest_date").and_then(|d| valid_date(&d)))
    .bind(fv(&form, "last_harvest_date").and_then(|d| valid_date(&d)))
    .bind(fv(&form, "harvested_by"))
    .bind(fv(&form, "notes"))
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await;
    ok_json(json!({"id": id, "message": "Crop added"}))
}

async fn update_crop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, crop_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM garden_crops WHERE id = ? AND garden_id = ?")
        .bind(&crop_id)
        .bind(&garden_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Crop not found");
    }
    let form = form_map(&raw);
    for (field, col) in [
        ("plot_section", "plot_section"),
        ("variety", "variety"),
        ("planted_by", "planted_by"),
        ("harvested_by", "harvested_by"),
        ("notes", "notes"),
    ] {
        if let Some(v) = fv(&form, field) {
            let _ = sqlx::query(&format!("UPDATE garden_crops SET {col} = ? WHERE id = ?"))
                .bind(&v)
                .bind(&crop_id)
                .execute(db)
                .await;
        }
    }
    if let Some(v) = fv(&form, "crop") {
        let _ = sqlx::query("UPDATE garden_crops SET crop = ? WHERE id = ?").bind(&v).bind(&crop_id).execute(db).await;
    }
    if form.contains_key("days_to_maturity") {
        let v = fv(&form, "days_to_maturity").and_then(|x| x.parse::<i64>().ok());
        let _ = sqlx::query("UPDATE garden_crops SET days_to_maturity = ? WHERE id = ?").bind(v).bind(&crop_id).execute(db).await;
    }
    for field in ["planting_date", "first_harvest_date", "last_harvest_date"] {
        if let Some(d) = form.get(field) {
            let d = d.trim();
            let v = if d.is_empty() { None } else { valid_date(d) };
            let _ = sqlx::query(&format!("UPDATE garden_crops SET {field} = ? WHERE id = ?"))
                .bind(&v)
                .bind(&crop_id)
                .execute(db)
                .await;
        }
    }
    ok_json(json!({"message": "Updated"}))
}

async fn delete_crop(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, crop_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let _ = sqlx::query("DELETE FROM garden_crop_maintenance WHERE crop_id = ?").bind(&crop_id).execute(db).await;
    let res = sqlx::query("DELETE FROM garden_crops WHERE id = ? AND garden_id = ?")
        .bind(&crop_id)
        .bind(&garden_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted"})),
        _ => err404("Crop not found"),
    }
}

async fn add_crop_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, crop_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM garden_crops WHERE id = ? AND garden_id = ?")
        .bind(&crop_id)
        .bind(&garden_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Crop not found in this garden");
    }
    let form = form_map(&raw);
    let Some(date) = fv(&form, "activity_date").and_then(|d| valid_date(&d)) else {
        return err400("Activity date is required");
    };
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO garden_crop_maintenance (id, crop_id, activity_type, activity_date, person, time_spent, materials, cost, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&crop_id)
    .bind(fv(&form, "activity_type").unwrap_or_else(|| "other".into()))
    .bind(&date)
    .bind(fv(&form, "person"))
    .bind(fv(&form, "time_spent"))
    .bind(fv(&form, "materials"))
    .bind(fv(&form, "cost").and_then(|c| c.parse::<f64>().ok()))
    .bind(fv(&form, "notes"))
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await;
    ok_json(json!({"id": id, "message": "Maintenance record added"}))
}

async fn update_crop_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, crop_id, record_id)): Path<(String, String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM garden_crop_maintenance WHERE id = ? AND crop_id = ?")
        .bind(&record_id)
        .bind(&crop_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Record not found");
    }
    let form = form_map(&raw);
    if let Some(v) = fv(&form, "activity_type") {
        let _ = sqlx::query("UPDATE garden_crop_maintenance SET activity_type = ? WHERE id = ?").bind(&v).bind(&record_id).execute(db).await;
    }
    if let Some(d) = fv(&form, "activity_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE garden_crop_maintenance SET activity_date = ? WHERE id = ?").bind(&d).bind(&record_id).execute(db).await;
    }
    let _ = sqlx::query(
        "UPDATE garden_crop_maintenance SET person = ?, time_spent = ?, materials = ?, cost = ?, notes = ? WHERE id = ?",
    )
    .bind(fv(&form, "person"))
    .bind(fv(&form, "time_spent"))
    .bind(fv(&form, "materials"))
    .bind(fv(&form, "cost").and_then(|c| c.parse::<f64>().ok()))
    .bind(fv(&form, "notes"))
    .bind(&record_id)
    .execute(db)
    .await;
    ok_json(json!({"message": "Updated"}))
}

async fn delete_crop_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, crop_id, record_id)): Path<(String, String, String)>,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM garden_crop_maintenance WHERE id = ? AND crop_id = ?")
        .bind(&record_id)
        .bind(&crop_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted"})),
        _ => err404("Record not found"),
    }
}

async fn add_plot_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(garden_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let form = form_map(&raw);
    let Some(date) = fv(&form, "activity_date").and_then(|d| valid_date(&d)) else {
        return err400("Activity date is required");
    };
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO garden_maintenance (id, garden_id, activity_type, activity_date, person, time_spent, materials, cost, notes, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&garden_id)
    .bind(fv(&form, "activity_type").unwrap_or_else(|| "other".into()))
    .bind(&date)
    .bind(fv(&form, "person"))
    .bind(fv(&form, "time_spent"))
    .bind(fv(&form, "materials"))
    .bind(fv(&form, "cost").and_then(|c| c.parse::<f64>().ok()))
    .bind(fv(&form, "notes"))
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await;
    ok_json(json!({"id": id, "message": "Maintenance record added"}))
}

async fn update_plot_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, record_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM garden_maintenance WHERE id = ? AND garden_id = ?")
        .bind(&record_id)
        .bind(&garden_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Record not found");
    }
    let form = form_map(&raw);
    if let Some(v) = fv(&form, "activity_type") {
        let _ = sqlx::query("UPDATE garden_maintenance SET activity_type = ? WHERE id = ?").bind(&v).bind(&record_id).execute(db).await;
    }
    if let Some(d) = fv(&form, "activity_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE garden_maintenance SET activity_date = ? WHERE id = ?").bind(&d).bind(&record_id).execute(db).await;
    }
    let _ = sqlx::query(
        "UPDATE garden_maintenance SET person = ?, time_spent = ?, materials = ?, cost = ?, notes = ? WHERE id = ?",
    )
    .bind(fv(&form, "person"))
    .bind(fv(&form, "time_spent"))
    .bind(fv(&form, "materials"))
    .bind(fv(&form, "cost").and_then(|c| c.parse::<f64>().ok()))
    .bind(fv(&form, "notes"))
    .bind(&record_id)
    .execute(db)
    .await;
    ok_json(json!({"message": "Updated"}))
}

async fn delete_plot_maintenance(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((garden_id, record_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !access(db, &garden_id, &user).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM garden_maintenance WHERE id = ? AND garden_id = ?")
        .bind(&record_id)
        .bind(&garden_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted"})),
        _ => err404("Record not found"),
    }
}
