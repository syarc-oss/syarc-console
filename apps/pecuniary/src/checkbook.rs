//! Checkbook: bank registers with encrypted-in-origin payment/deposit
//! amounts (stored plaintext here), running balances, and sharing.

use axum::extract::{Path, Query, RawForm, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{err_json, form_map, fv, fnum, page, shared_users, share_candidates, usernames};

const ACCOUNT_TYPES: [&str; 5] = ["checking", "savings", "money_market", "cd", "other"];

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/pecuniary/checkbook/select", get(select_checkbook))
        .route("/pecuniary/checkbook/register/new", get(register_new).post(register_create))
        .route("/pecuniary/checkbook/register/{register_id}/edit", get(register_edit).post(register_update))
        .route("/pecuniary/checkbook/register/{register_id}/delete", post(register_delete))
        .route("/pecuniary/checkbook/", get(list_checks))
        .route("/pecuniary/checkbook", get(list_checks))
        .route("/pecuniary/checkbook/entry/{entry_id}", get(get_entry))
        .route("/pecuniary/checkbook/add", post(add_entry))
        .route("/pecuniary/checkbook/delete/{entry_id}", post(delete_entry))
        .route("/pecuniary/checkbook/toggle_returned/{entry_id}", post(toggle_returned))
        .route("/pecuniary/checkbook/share", post(share))
        .route("/pecuniary/checkbook/revoke/{share_id}", post(revoke))
}

// ------------------------------------------------------------------ helpers

fn register_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "bank_name": r.try_get::<String,_>("bank_name").unwrap_or_default(),
        "account_number": r.try_get::<String,_>("account_number").ok(),
        "account_type": r.try_get::<String,_>("account_type").unwrap_or_default(),
        "description": r.try_get::<String,_>("description").ok(),
        "is_active": r.get::<i64,_>("is_active") != 0,
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
    })
}

async fn load_register(db: &syarc_plugin_api::Db, id: i64) -> Option<Value> {
    sqlx::query("SELECT * FROM checkbook_registers WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| register_json(&r))
}

async fn has_register_access(db: &syarc_plugin_api::Db, id: i64, user_id: i64) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM checkbook_registers WHERE id = ? AND user_id = ? \
         UNION ALL SELECT 1 FROM checkbook_shares WHERE register_id = ? AND shared_with_user_id = ?",
    )
    .bind(id)
    .bind(user_id)
    .bind(id)
    .bind(user_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some()
}

async fn has_entry_access(db: &syarc_plugin_api::Db, entry_id: i64, user_id: i64) -> Option<i64> {
    let register_id: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT register_id FROM checkbook WHERE id = ?",
    )
    .bind(entry_id)
    .fetch_one(db)
    .await
    .ok()?;
    if let Some(rid) = register_id {
        if has_register_access(db, rid, user_id).await {
            return Some(rid);
        }
    }
    None
}

/// Recompute running balances for every entry in the register.
async fn update_balances(db: &syarc_plugin_api::Db, register_id: i64) {
    let rows = sqlx::query(
        "SELECT id, payment, deposit FROM checkbook WHERE register_id = ? ORDER BY date ASC, id ASC",
    )
    .bind(register_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let mut balance = 0.0f64;
    for r in rows {
        let id: i64 = r.get("id");
        let payment: f64 = r.get("payment");
        let deposit: f64 = r.get("deposit");
        balance += deposit - payment;
        let _ = sqlx::query("UPDATE checkbook SET balance = ? WHERE id = ?")
            .bind((balance * 100.0).round() / 100.0)
            .bind(id)
            .execute(db)
            .await;
    }
}

// ------------------------------------------------------------- select / CRUD

async fn select_checkbook(user: RequireUser, State(state): State<SharedState>) -> Response {
    let own = sqlx::query(
        "SELECT r.*, (SELECT COUNT(*) FROM checkbook c WHERE c.register_id = r.id) AS entry_count \
         FROM checkbook_registers r WHERE r.user_id = ? ORDER BY r.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let shared = sqlx::query(
        "SELECT r.*, sh.created_at AS shared_at, \
         (SELECT COUNT(*) FROM checkbook c WHERE c.register_id = r.id) AS entry_count \
         FROM checkbook_shares sh \
         JOIN checkbook_registers r ON r.id = sh.register_id \
         WHERE sh.shared_with_user_id = ? ORDER BY r.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let owner_ids: Vec<i64> = shared.iter().map(|r| r.get::<i64, _>("user_id")).collect();
    let names = usernames(&state, &owner_ids).await;

    let mut registers: Vec<Value> = own
        .iter()
        .map(|r| {
            json!({
                "register": register_json(r),
                "is_owner": true,
                "entry_count": r.get::<i64,_>("entry_count"),
                "shared_at": Value::Null,
            })
        })
        .collect();
    registers.extend(shared.iter().map(|r| {
        json!({
            "register": register_json(r),
            "is_owner": false,
            "entry_count": r.get::<i64,_>("entry_count"),
            "shared_at": r.try_get::<String,_>("shared_at").unwrap_or_default(),
            "owner_username": names.get(&r.get::<i64,_>("user_id")).cloned().unwrap_or_default(),
        })
    }));

    page(&state, "checkbook/checkbook_select.html", json!({"registers": registers})).into_response()
}

async fn register_new(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    page(
        &state,
        "checkbook/checkbook_register_form.html",
        json!({"account_types": ACCOUNT_TYPES, "is_edit": false}),
    )
    .into_response()
}

async fn register_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let (Some(name), Some(bank_name), Some(account_type)) =
        (fv(&form, "name"), fv(&form, "bank_name"), fv(&form, "account_type"))
    else {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    };
    let _ = sqlx::query(
        "INSERT INTO checkbook_registers (user_id, name, bank_name, account_number, account_type, description) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(user.0.id)
    .bind(&name)
    .bind(&bank_name)
    .bind(fv(&form, "account_number"))
    .bind(&account_type)
    .bind(fv(&form, "description"))
    .execute(&state.pecuniary)
    .await;
    Redirect::to("/pecuniary/checkbook/select").into_response()
}

async fn register_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(register_id): Path<i64>,
) -> Response {
    let register = sqlx::query("SELECT * FROM checkbook_registers WHERE id = ? AND user_id = ?")
        .bind(register_id)
        .bind(user.0.id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten()
        .map(|r| register_json(&r));
    let Some(register) = register else {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    };
    page(
        &state,
        "checkbook/checkbook_register_form.html",
        json!({"register": register, "account_types": ACCOUNT_TYPES, "is_edit": true}),
    )
    .into_response()
}

async fn register_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(register_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let (Some(name), Some(bank_name), Some(account_type)) =
        (fv(&form, "name"), fv(&form, "bank_name"), fv(&form, "account_type"))
    else {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    };
    let res = sqlx::query(
        "UPDATE checkbook_registers SET name = ?, bank_name = ?, account_number = ?, account_type = ?, description = ? \
         WHERE id = ? AND user_id = ?",
    )
    .bind(&name)
    .bind(&bank_name)
    .bind(fv(&form, "account_number"))
    .bind(&account_type)
    .bind(fv(&form, "description"))
    .bind(register_id)
    .bind(user.0.id)
    .execute(&state.pecuniary)
    .await;
    let _ = res;
    Redirect::to("/pecuniary/checkbook/select").into_response()
}

async fn register_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(register_id): Path<i64>,
) -> Response {
    let _ = sqlx::query("DELETE FROM checkbook_registers WHERE id = ? AND user_id = ?")
        .bind(register_id)
        .bind(user.0.id)
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/checkbook/select").into_response()
}

// ------------------------------------------------------------- main register

async fn list_checks(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(register_id) = params.get("register_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    };
    let Some(register) = load_register(&state.pecuniary, register_id).await else {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    };
    let is_owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM checkbook_registers WHERE id = ?")
        .bind(register_id)
        .fetch_one(&state.pecuniary)
        .await
        .ok()
        == Some(user.0.id);
    if !is_owner && !has_register_access(&state.pecuniary, register_id, user.0.id).await {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    }

    let page_num: i64 = params.get("page").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let per_page: i64 = params.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);
    let sort = params.get("sort").cloned().unwrap_or_else(|| "date".into());
    let sort_dir = if params.get("sort_dir").map(|d| d == "asc").unwrap_or(false) { "ASC" } else { "DESC" };

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checkbook WHERE register_id = ?")
        .bind(register_id)
        .fetch_one(&state.pecuniary)
        .await
        .unwrap_or(0);

    let order = match sort.as_str() {
        "type" => format!("type {sort_dir}, date {sort_dir}"),
        "check_number" => format!("check_number {sort_dir}, date {sort_dir}"),
        "payee_description" => format!("payee_description {sort_dir}, date {sort_dir}"),
        _ => format!("date {sort_dir}, id {sort_dir}"),
    };
    let sql = format!(
        "SELECT c.* FROM checkbook c \
         WHERE c.register_id = ? ORDER BY {order} LIMIT ? OFFSET ?"
    );
    let rows = sqlx::query(&sql)
        .bind(register_id)
        .bind(per_page)
        .bind((page_num - 1) * per_page)
        .fetch_all(&state.pecuniary)
        .await
        .unwrap_or_default();
    let creator_ids: Vec<i64> = rows.iter().map(|r| r.get::<i64, _>("user_id")).collect();
    let creator_names = usernames(&state, &creator_ids).await;

    let entries: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "date": r.try_get::<String,_>("date").unwrap_or_default(),
                "type": r.try_get::<String,_>("type").unwrap_or_default(),
                "check_number": r.try_get::<i64,_>("check_number").ok(),
                "payee_description": r.try_get::<String,_>("payee_description").unwrap_or_default(),
                "payment": r.get::<f64,_>("payment"),
                "deposit": r.get::<f64,_>("deposit"),
                "balance": r.get::<f64,_>("balance"),
                "returned": r.get::<i64,_>("returned") != 0,
                "user_id": r.get::<i64,_>("user_id"),
                "creator": creator_names.get(&r.get::<i64,_>("user_id")).cloned().unwrap_or_default(),
            })
        })
        .collect();

    let next_check_number: Value = if is_owner {
        let max_check: Option<i64> =
            sqlx::query_scalar("SELECT MAX(check_number) FROM checkbook WHERE register_id = ?")
                .bind(register_id)
                .fetch_one(&state.pecuniary)
                .await
                .ok()
                .flatten();
        match max_check {
            Some(m) if m > 0 => json!(m + 1),
            _ => json!(1),
        }
    } else {
        Value::Null
    };

    let last_date: String = sqlx::query_scalar::<_, String>(
        "SELECT date FROM checkbook WHERE register_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(register_id)
    .fetch_optional(&state.pecuniary)
    .await
    .ok()
    .flatten()
    .unwrap_or_default();

    let (shared, candidates) = if is_owner {
        (
            shared_users(&state, "checkbook_shares", "register_id", register_id).await,
            share_candidates(&state, "checkbook_shares", "register_id", register_id, user.0.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };

    page(
        &state,
        "checkbook/checkbook.html",
        json!({
            "user": {"id": user.0.id},
            "register": register, "register_id": register_id, "is_owner": is_owner,
            "entries": entries, "next_check_number": next_check_number,
            "last_date": last_date,
            "shared_users": shared, "all_users": candidates,
            "page": page_num, "per_page": per_page, "total": total,
            "total_pages": if total > 0 { (total + per_page - 1) / per_page } else { 1 },
            "sort": sort,
            "sort_dir": if sort_dir == "ASC" { "asc" } else { "desc" },
        }),
    )
    .into_response()
}

// ------------------------------------------------------------------ entries

async fn get_entry(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(entry_id): Path<i64>,
) -> Response {
    let row = sqlx::query("SELECT * FROM checkbook WHERE id = ?")
        .bind(entry_id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Entry not found");
    };
    if has_entry_access(&state.pecuniary, entry_id, user.0.id).await.is_none() {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Entry not found");
    }
    let payment: f64 = row.get("payment");
    let deposit: f64 = row.get("deposit");
    axum::Json(json!({
        "type": row.try_get::<String,_>("type").unwrap_or_default(),
        "date": row.try_get::<String,_>("date").unwrap_or_default(),
        "check_number": row.try_get::<i64,_>("check_number").ok(),
        "amount": if payment != 0.0 { payment } else { deposit },
        "payee": row.try_get::<String,_>("payee_description").unwrap_or_default(),
    }))
    .into_response()
}

async fn add_entry(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(register_id) = fv(&form, "register_id")
        .as_deref()
        .or_else(|| params.get("register_id").map(|s| s.as_str()))
        .and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    };
    if !has_register_access(&state.pecuniary, register_id, user.0.id).await {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    }
    let Some(entry_type) = fv(&form, "entry_type").map(|t| t.to_uppercase()) else {
        return Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response();
    };
    let Some(amount) = fnum(&form, "amount") else {
        return Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response();
    };
    let Some(entry_date) = fv(&form, "entry_date") else {
        return Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response();
    };
    let check_number = fv(&form, "check_number").and_then(|c| c.parse::<i64>().ok());
    let payee = fv(&form, "payee").unwrap_or_default();

    let (payment, deposit) = if matches!(entry_type.as_str(), "DEPOSIT" | "TRANSFER_IN") {
        (0.0, amount)
    } else {
        (amount, 0.0)
    };

    if let Some(entry_id) = fv(&form, "entry_id").and_then(|v| v.parse::<i64>().ok()) {
        if has_entry_access(&state.pecuniary, entry_id, user.0.id).await.is_some() {
            let _ = sqlx::query(
                "UPDATE checkbook SET type = ?, check_number = ?, date = ?, payee_description = ?, payment = ?, deposit = ? WHERE id = ?",
            )
            .bind(&entry_type)
            .bind(check_number)
            .bind(&entry_date)
            .bind(&payee)
            .bind(payment)
            .bind(deposit)
            .bind(entry_id)
            .execute(&state.pecuniary)
            .await;
        }
    } else {
        let owner_id: Option<i64> =
            sqlx::query_scalar("SELECT user_id FROM checkbook_registers WHERE id = ?")
                .bind(register_id)
                .fetch_one(&state.pecuniary)
                .await
                .ok();
        if owner_id != Some(user.0.id) {
            return Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response();
        }
        let _ = sqlx::query(
            "INSERT INTO checkbook (register_id, user_id, type, check_number, date, payee_description, payment, deposit) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(register_id)
        .bind(user.0.id)
        .bind(&entry_type)
        .bind(check_number)
        .bind(&entry_date)
        .bind(&payee)
        .bind(payment)
        .bind(deposit)
        .execute(&state.pecuniary)
        .await;
    }

    update_balances(&state.pecuniary, register_id).await;
    Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response()
}

async fn delete_entry(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(entry_id): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Some(register_id) = has_entry_access(&state.pecuniary, entry_id, user.0.id).await {
        let _ = sqlx::query("DELETE FROM checkbook WHERE id = ?")
            .bind(entry_id)
            .execute(&state.pecuniary)
            .await;
        update_balances(&state.pecuniary, register_id).await;
        return Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response();
    }
    if let Some(rid) = params.get("register_id") {
        return Redirect::to(&format!("/pecuniary/checkbook/?register_id={rid}")).into_response();
    }
    Redirect::to("/pecuniary/checkbook/select").into_response()
}

async fn toggle_returned(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(entry_id): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Some(register_id) = has_entry_access(&state.pecuniary, entry_id, user.0.id).await {
        let _ = sqlx::query("UPDATE checkbook SET returned = 1 - returned WHERE id = ?")
            .bind(entry_id)
            .execute(&state.pecuniary)
            .await;
        return Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response();
    }
    if let Some(rid) = params.get("register_id") {
        return Redirect::to(&format!("/pecuniary/checkbook/?register_id={rid}")).into_response();
    }
    Redirect::to("/pecuniary/checkbook/select").into_response()
}

// ------------------------------------------------------------------ sharing

async fn share(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let (Some(share_with), Some(register_id)) = (
        fv(&form, "share_with_user_id").and_then(|v| v.parse::<i64>().ok()),
        fv(&form, "register_id").and_then(|v| v.parse::<i64>().ok()),
    ) else {
        return Redirect::to("/pecuniary/checkbook/select").into_response();
    };
    let back = format!("/pecuniary/checkbook/?register_id={register_id}");
    if share_with == user.0.id {
        return Redirect::to(&back).into_response();
    }
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM checkbook_registers WHERE id = ?")
        .bind(register_id)
        .fetch_one(&state.pecuniary)
        .await
        .ok();
    if owner != Some(user.0.id) {
        return Redirect::to(&back).into_response();
    }
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO checkbook_shares (register_id, shared_with_user_id) VALUES (?, ?)",
    )
    .bind(register_id)
    .bind(share_with)
    .execute(&state.pecuniary)
    .await;
    Redirect::to(&back).into_response()
}

async fn revoke(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(share_id): Path<i64>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let register_id = params
        .get("register_id")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let _ = sqlx::query(
        "DELETE FROM checkbook_shares WHERE id = ? AND register_id = ? AND register_id IN \
         (SELECT id FROM checkbook_registers WHERE user_id = ?)",
    )
    .bind(share_id)
    .bind(register_id)
    .bind(user.0.id)
    .execute(&state.pecuniary)
    .await;
    Redirect::to(&format!("/pecuniary/checkbook/?register_id={register_id}")).into_response()
}
