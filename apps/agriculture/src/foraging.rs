//! Foraging app: outings with free-text participants and results.

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

pub(crate) fn router() -> Router<SharedState> {
    Router::new()
        .route("/agriculture/foraging", get(root))
        .route("/agriculture/foraging/", get(root))
        .route("/agriculture/foraging/create", post(create_outing))
        .route("/agriculture/foraging/{outing_id}", put(update_outing).delete(delete_outing))
        .route("/agriculture/foraging/{outing_id}/complete", post(complete_outing))
        .route("/agriculture/foraging/{outing_id}/share", post(share_outing))
        .route("/agriculture/foraging/{outing_id}/revoke/{share_id}", post(revoke_outing))
        .route("/agriculture/foraging/{outing_id}/participants", get(participants_page))
        .route("/agriculture/foraging/{outing_id}/participants/add", post(add_participant))
        .route("/agriculture/foraging/{outing_id}/participants/{participant_id}", put(update_participant).delete(delete_participant))
        .route("/agriculture/foraging/{outing_id}/results", get(results_page))
        .route("/agriculture/foraging/{outing_id}/results/add", post(add_result))
        .route("/agriculture/foraging/{outing_id}/results/{result_id}", put(update_result).delete(delete_result))
}

fn outing_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "user_id": r.get::<i64,_>("user_id"),
        "outing_date": r.try_get::<String,_>("outing_date").unwrap_or_default(),
        "outing_type": r.try_get::<String,_>("outing_type").unwrap_or_default(),
        "duration": r.try_get::<Option<String>,_>("duration").ok().flatten(),
        "location": r.try_get::<Option<String>,_>("location").ok().flatten(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "status": r.try_get::<String,_>("status").unwrap_or_default(),
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
        "updated_at": r.try_get::<String,_>("updated_at").unwrap_or_default(),
    })
}

async fn get_outing(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query("SELECT * FROM foraging_outings WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| outing_row(&r))
}

async fn has_access(db: &syarc_plugin_api::Db, outing_id: &str, user_id: i64) -> bool {
    user_has_access(db, "foraging_outings", "foraging_shares", "outing_id", outing_id, user_id).await
}

async fn root(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let mut outings: Vec<Value> = Vec::new();

    let own = sqlx::query("SELECT * FROM foraging_outings WHERE user_id = ? ORDER BY outing_date DESC")
        .bind(user.id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    for r in &own {
        let id = r.get::<String, _>("id");
        let (participant_count, result_count) = outing_counts(db, &id).await;
        outings.push(json!({
            "outing": outing_row(r), "is_owner": true,
            "participant_count": participant_count, "result_count": result_count,
            "shared_at": null,
        }));
    }

    let shared = sqlx::query(
        "SELECT o.*, u.username AS owner_username, sh.created_at AS shared_at \
         FROM foraging_shares sh JOIN foraging_outings o ON sh.outing_id = o.id \
         JOIN users u ON u.id = o.user_id WHERE sh.shared_with_user_id = ? \
         ORDER BY o.outing_date DESC",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &shared {
        let id = r.get::<String, _>("id");
        let (participant_count, result_count) = outing_counts(db, &id).await;
        outings.push(json!({
            "outing": outing_row(r), "is_owner": false,
            "participant_count": participant_count, "result_count": result_count,
            "shared_at": r.try_get::<String,_>("shared_at").unwrap_or_default(),
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
        }));
    }

    page(
        &state,
        "foraging/foraging_select.html",
        json!({"user": user_json(&user), "outings": outings}),
    )
}

async fn outing_counts(db: &syarc_plugin_api::Db, outing_id: &str) -> (i64, i64) {
    let p: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM foraging_participants WHERE outing_id = ?")
        .bind(outing_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let r: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM foraging_results WHERE outing_id = ?")
        .bind(outing_id)
        .fetch_one(db)
        .await
        .unwrap_or(0);
    (p, r)
}

async fn create_outing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let (Some(date_str), Some(outing_type)) = (fv(&form, "outing_date"), fv(&form, "outing_type")) else {
        return redirect("/agriculture/foraging/");
    };
    let Some(outing_date) = valid_date(&date_str) else {
        return redirect("/agriculture/foraging/");
    };
    let duration = fv(&form, "duration");
    let location = fv(&form, "location");
    let description = fv(&form, "description");
    let now = now_str();
    let _ = sqlx::query(
        "INSERT INTO foraging_outings (id, user_id, outing_date, outing_type, duration, location, description, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'planned', ?, ?)",
    )
    .bind(new_uuid())
    .bind(user.id)
    .bind(&outing_date)
    .bind(&outing_type)
    .bind(&duration)
    .bind(&location)
    .bind(&description)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await;
    redirect("/agriculture/foraging/")
}

async fn update_outing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    let owned: Option<String> =
        sqlx::query_scalar("SELECT id FROM foraging_outings WHERE id = ? AND user_id = ?")
            .bind(&outing_id)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if owned.is_none() {
        return err404("Outing not found");
    }
    if let Some(d) = fv(&form, "outing_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE foraging_outings SET outing_date = ? WHERE id = ?").bind(d).bind(&outing_id).execute(db).await;
    }
    if let Some(v) = fv(&form, "outing_type") {
        let _ = sqlx::query("UPDATE foraging_outings SET outing_type = ? WHERE id = ?").bind(v).bind(&outing_id).execute(db).await;
    }
    for col in ["duration", "location", "description"] {
        if form.contains_key(col) {
            let v = fv(&form, col);
            let _ = sqlx::query(&format!("UPDATE foraging_outings SET {col} = ? WHERE id = ?"))
                .bind(v)
                .bind(&outing_id)
                .execute(db)
                .await;
        }
    }
    let _ = sqlx::query("UPDATE foraging_outings SET updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&outing_id)
        .execute(db)
        .await;
    ok_json(json!({"message": "Updated successfully"}))
}

async fn delete_outing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
) -> Response {
    let db = &state.db;
    let owned: Option<String> =
        sqlx::query_scalar("SELECT id FROM foraging_outings WHERE id = ? AND user_id = ?")
            .bind(&outing_id)
            .bind(user.id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if owned.is_none() {
        return err404("Outing not found");
    }
    let _ = sqlx::query("DELETE FROM foraging_participants WHERE outing_id = ?").bind(&outing_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM foraging_results WHERE outing_id = ?").bind(&outing_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM foraging_shares WHERE outing_id = ?").bind(&outing_id).execute(db).await;
    let _ = sqlx::query("DELETE FROM foraging_outings WHERE id = ?").bind(&outing_id).execute(db).await;
    ok_json(json!({"message": "Deleted successfully"}))
}

async fn complete_outing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("UPDATE foraging_outings SET status = 'completed', updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(&outing_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Outing marked as completed"})),
        _ => err404("Outing not found"),
    }
}

async fn share_outing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM foraging_outings WHERE id = ?")
        .bind(&outing_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/agriculture/foraging/");
    }
    if let Some(target) = fv(&form, "share_with_user_id") {
        let _ = add_share(db, "foraging_shares", "outing_id", &outing_id, user.id, &target).await;
    }
    redirect(&format!("/agriculture/foraging/{outing_id}/participants"))
}

async fn revoke_outing(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((outing_id, share_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM foraging_outings WHERE id = ?")
        .bind(&outing_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if owner != Some(user.id) {
        return redirect("/agriculture/foraging/");
    }
    revoke_share(db, "foraging_shares", "outing_id", &outing_id, &share_id).await;
    redirect(&format!("/agriculture/foraging/{outing_id}/participants"))
}

async fn participants_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let Some(outing) = get_outing(db, &outing_id).await else {
        return err404("Outing not found");
    };
    let is_owner = outing["user_id"].as_i64() == Some(user.id);
    let (shared, all_users) = if is_owner {
        (
            shared_users(db, "foraging_shares", "outing_id", &outing_id).await,
            shareable_users(db, "foraging_shares", "outing_id", &outing_id, user.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let participants: Vec<Value> = sqlx::query(
        "SELECT id, name FROM foraging_participants WHERE outing_id = ? ORDER BY name",
    )
    .bind(&outing_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| json!({"id": r.get::<String,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()}))
    .collect();
    page(
        &state,
        "foraging/foraging_participants.html",
        json!({
            "user": user_json(&user), "outing": outing, "outing_id": outing_id,
            "is_owner": is_owner, "participants": participants,
            "shared_users": shared, "all_users": all_users,
        }),
    )
}

async fn results_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let Some(outing) = get_outing(db, &outing_id).await else {
        return err404("Outing not found");
    };
    let results: Vec<Value> = sqlx::query(
        "SELECT fr.id, fr.result_date, fr.item, fr.location, fr.quantity, fr.lbs, fr.notes, fr.participant, \
                u.username AS recorded_by \
         FROM foraging_results fr JOIN users u ON u.id = fr.recorded_by \
         WHERE fr.outing_id = ? ORDER BY fr.result_date DESC",
    )
    .bind(&outing_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<String,_>("id"),
            "result_date": r.try_get::<String,_>("result_date").unwrap_or_default(),
            "item": r.try_get::<String,_>("item").unwrap_or_default(),
            "location": r.try_get::<Option<String>,_>("location").ok().flatten(),
            "quantity": r.try_get::<Option<i64>,_>("quantity").ok().flatten(),
            "lbs": r.try_get::<Option<f64>,_>("lbs").ok().flatten(),
            "notes": r.try_get::<Option<String>,_>("notes").ok().flatten(),
            "recorded_by": r.try_get::<String,_>("recorded_by").unwrap_or_default(),
            "participant": r.try_get::<Option<String>,_>("participant").ok().flatten(),
        })
    })
    .collect();
    let participants: Vec<Value> = sqlx::query(
        "SELECT id, name FROM foraging_participants WHERE outing_id = ? ORDER BY name",
    )
    .bind(&outing_id)
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| {
        json!({"id": r.get::<String,_>("id"), "name": r.get::<String,_>("name")})
    })
    .collect();
    page(
        &state,
        "foraging/foraging_results.html",
        json!({
            "user": user_json(&user), "outing": outing, "outing_id": outing_id,
            "results": results, "participants": participants,
        }),
    )
}

async fn add_participant(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let Some(name) = fv(&form, "name") else {
        return err400("Name is required");
    };
    let id = new_uuid();
    let ok = sqlx::query("INSERT INTO foraging_participants (id, outing_id, name) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(&outing_id)
        .bind(&name)
        .execute(db)
        .await
        .is_ok();
    if !ok {
        return err400("Could not add participant");
    }
    ok_json(json!({"id": id, "message": "Participant added"}))
}

async fn update_participant(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((outing_id, participant_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let Some(name) = fv(&form, "name") else {
        return ok_json(json!({"message": "Updated successfully"}));
    };
    let res = sqlx::query("UPDATE foraging_participants SET name = ? WHERE id = ? AND outing_id = ?")
        .bind(&name)
        .bind(&participant_id)
        .bind(&outing_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Updated successfully"})),
        _ => err404("Participant not found"),
    }
}

async fn delete_participant(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((outing_id, participant_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM foraging_participants WHERE id = ? AND outing_id = ?")
        .bind(&participant_id)
        .bind(&outing_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Participant not found"),
    }
}

async fn add_result(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(outing_id): Path<String>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let (Some(date_str), Some(item)) = (fv(&form, "result_date"), fv(&form, "item")) else {
        return err400("Date and item are required");
    };
    let Some(result_date) = valid_date(&date_str) else {
        return err400("Invalid date");
    };
    let location = fv(&form, "location");
    let quantity = fv(&form, "quantity").and_then(|q| q.parse::<i64>().ok());
    let lbs = fv(&form, "lbs").and_then(|l| l.parse::<f64>().ok());
    let notes = fv(&form, "notes");
    let participant = fv(&form, "participant").filter(|p| !p.is_empty());
    let id = new_uuid();
    let ok = sqlx::query(
        "INSERT INTO foraging_results (id, outing_id, result_date, item, location, quantity, lbs, notes, participant, created_at, recorded_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&outing_id)
    .bind(&result_date)
    .bind(&item)
    .bind(&location)
    .bind(quantity)
    .bind(lbs)
    .bind(&notes)
    .bind(&participant)
    .bind(now_str())
    .bind(user.id)
    .execute(db)
    .await
    .is_ok();
    if !ok {
        return err400("Could not add result");
    }
    ok_json(json!({"id": id, "message": "Result added"}))
}

async fn update_result(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((outing_id, result_id)): Path<(String, String)>,
    AgriForm(raw): AgriForm,
) -> Response {
    let form = form_map(&raw);
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM foraging_results WHERE id = ? AND outing_id = ?")
            .bind(&result_id)
            .bind(&outing_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    if existing.is_none() {
        return err404("Result not found");
    }
    if let Some(d) = fv(&form, "result_date").and_then(|d| valid_date(&d)) {
        let _ = sqlx::query("UPDATE foraging_results SET result_date = ? WHERE id = ?").bind(d).bind(&result_id).execute(db).await;
    }
    if let Some(v) = fv(&form, "item") {
        let _ = sqlx::query("UPDATE foraging_results SET item = ? WHERE id = ?").bind(v).bind(&result_id).execute(db).await;
    }
    if form.contains_key("location") {
        let v = fv(&form, "location");
        let _ = sqlx::query("UPDATE foraging_results SET location = ? WHERE id = ?").bind(v).bind(&result_id).execute(db).await;
    }
    if let Some(q) = fv(&form, "quantity").and_then(|q| q.parse::<i64>().ok()) {
        let _ = sqlx::query("UPDATE foraging_results SET quantity = ? WHERE id = ?").bind(q).bind(&result_id).execute(db).await;
    } else if form.contains_key("quantity") {
        let _ = sqlx::query("UPDATE foraging_results SET quantity = NULL WHERE id = ?").bind(&result_id).execute(db).await;
    }
    if let Some(l) = fv(&form, "lbs").and_then(|l| l.parse::<f64>().ok()) {
        let _ = sqlx::query("UPDATE foraging_results SET lbs = ? WHERE id = ?").bind(l).bind(&result_id).execute(db).await;
    } else if form.contains_key("lbs") {
        let _ = sqlx::query("UPDATE foraging_results SET lbs = NULL WHERE id = ?").bind(&result_id).execute(db).await;
    }
    if form.contains_key("participant") {
        let v = fv(&form, "participant").filter(|p| !p.is_empty());
        let _ = sqlx::query("UPDATE foraging_results SET participant = ? WHERE id = ?").bind(v).bind(&result_id).execute(db).await;
    }
    if form.contains_key("notes") {
        let v = fv(&form, "notes");
        let _ = sqlx::query("UPDATE foraging_results SET notes = ? WHERE id = ?").bind(v).bind(&result_id).execute(db).await;
    }
    ok_json(json!({"message": "Updated successfully"}))
}

async fn delete_result(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((outing_id, result_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    if !has_access(db, &outing_id, user.id).await {
        return err403("Access denied");
    }
    let res = sqlx::query("DELETE FROM foraging_results WHERE id = ? AND outing_id = ?")
        .bind(&result_id)
        .bind(&outing_id)
        .execute(db)
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => ok_json(json!({"message": "Deleted successfully"})),
        _ => err404("Result not found"),
    }
}
