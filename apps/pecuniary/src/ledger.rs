//! Ledger: income/liability books with recurrence, reconciliation,
//! sharing, and monthly cash-flow analytics.

use std::collections::HashMap;

use axum::extract::{Path, Query, RawForm, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{err_json, form_map, fv, fnum, page, shared_users, share_candidates, usernames};

pub const INCOME_TYPES: [&str; 10] = [
    "payroll", "social_security", "interest", "rmd_ira", "dividend",
    "pension", "annuity", "rental", "freelance", "other_income",
];
pub const LIABILITY_TYPES: [&str; 17] = [
    "food", "mortgage", "loan", "income_tax", "property_tax", "utilities",
    "insurance", "registration", "medical", "auto", "education",
    "entertainment", "charity", "home_maintenance", "subscription",
    "other_liability", "rent",
];
pub const RECURRENCE_PERIODS: [&str; 7] = [
    "one_time", "weekly", "biweekly", "monthly", "quarterly", "semiannual", "annual",
];

const MONTH_NAMES: [&str; 13] = [
    "", "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/pecuniary/ledger/select", get(select_book))
        .route("/pecuniary/ledger/book/new", get(book_new).post(book_create))
        .route("/pecuniary/ledger/book/{book_id}/edit", get(book_edit).post(book_update))
        .route("/pecuniary/ledger/book/{book_id}/delete", post(book_delete))
        .route("/pecuniary/ledger/", get(ledger_dashboard))
        .route("/pecuniary/ledger", get(ledger_dashboard))
        .route("/pecuniary/ledger/entry/new", get(entry_new).post(entry_create))
        .route("/pecuniary/ledger/entry/{entry_id}/edit", get(entry_edit).post(entry_update))
        .route("/pecuniary/ledger/entry/{entry_id}/delete", post(entry_delete))
        .route("/pecuniary/ledger/entry/{entry_id}/reconcile", post(entry_reconcile))
        .route("/pecuniary/ledger/analytics/cashflow", get(cashflow))
        .route("/pecuniary/ledger/analytics/cashflow/data", get(cashflow_data))
        .route("/pecuniary/ledger/share", post(share))
        .route("/pecuniary/ledger/revoke/{share_id}", post(revoke))
}

// ------------------------------------------------------------------ helpers

fn book_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "description": r.try_get::<String,_>("description").ok(),
        "is_active": r.get::<i64,_>("is_active") != 0,
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
    })
}

pub(crate) async fn load_book(db: &syarc_plugin_api::Db, id: i64) -> Option<(Value, i64)> {
    let row = sqlx::query("SELECT * FROM ledger_books WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM ledger_books WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .ok()?;
    Some((book_json(&row), owner))
}

pub(crate) async fn has_book_access(db: &syarc_plugin_api::Db, id: i64, user_id: i64) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM ledger_books WHERE id = ? AND user_id = ? \
         UNION ALL SELECT 1 FROM ledger_shares WHERE book_id = ? AND shared_with_user_id = ?",
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

fn entry_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "book_id": r.get::<i64,_>("book_id"),
        "category": r.try_get::<String,_>("category").unwrap_or_default(),
        "entry_type": r.try_get::<String,_>("entry_type").unwrap_or_default(),
        "description": r.try_get::<String,_>("description").unwrap_or_default(),
        "amount": r.get::<f64,_>("amount"),
        "entry_date": r.try_get::<String,_>("entry_date").unwrap_or_default(),
        "recurrence": r.try_get::<String,_>("recurrence").unwrap_or_default(),
        "source": r.try_get::<String,_>("source").ok(),
        "reference_number": r.try_get::<String,_>("reference_number").ok(),
        "notes": r.try_get::<String,_>("notes").ok(),
        "is_reconciled": r.get::<i64,_>("is_reconciled") != 0,
    })
}

// ------------------------------------------------------------- select / CRUD

async fn select_book(user: RequireUser, State(state): State<SharedState>) -> Response {
    let own = sqlx::query(
        "SELECT b.*, (SELECT COUNT(*) FROM ledger_entries e WHERE e.book_id = b.id) AS entry_count \
         FROM ledger_books b WHERE b.user_id = ? AND b.is_active = 1 ORDER BY b.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let shared = sqlx::query(
        "SELECT b.*, sh.created_at AS shared_at, \
         (SELECT COUNT(*) FROM ledger_entries e WHERE e.book_id = b.id) AS entry_count \
         FROM ledger_shares sh \
         JOIN ledger_books b ON b.id = sh.book_id \
         WHERE sh.shared_with_user_id = ? AND b.is_active = 1 ORDER BY b.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let owner_ids: Vec<i64> = shared.iter().map(|r| r.get::<i64, _>("user_id")).collect();
    let names = usernames(&state, &owner_ids).await;

    let mut books: Vec<Value> = own
        .iter()
        .map(|r| {
            json!({
                "book": book_json(r), "is_owner": true,
                "entry_count": r.get::<i64,_>("entry_count"), "shared_at": Value::Null,
            })
        })
        .collect();
    books.extend(shared.iter().map(|r| {
        json!({
            "book": book_json(r), "is_owner": false,
            "entry_count": r.get::<i64,_>("entry_count"),
            "shared_at": r.try_get::<String,_>("shared_at").unwrap_or_default(),
            "owner_username": names.get(&r.get::<i64,_>("user_id")).cloned().unwrap_or_default(),
        })
    }));

    page(&state, "ledger/ledger_select.html", json!({"books": books})).into_response()
}

async fn book_new(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    page(&state, "ledger/book_form.html", json!({"is_edit": false})).into_response()
}

async fn book_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let _ = sqlx::query("INSERT INTO ledger_books (user_id, name, description) VALUES (?, ?, ?)")
        .bind(user.0.id)
        .bind(&name)
        .bind(fv(&form, "description"))
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/ledger/select").into_response()
}

async fn book_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(book_id): Path<i64>,
) -> Response {
    let book = sqlx::query("SELECT * FROM ledger_books WHERE id = ? AND user_id = ?")
        .bind(book_id)
        .bind(user.0.id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten()
        .map(|r| book_json(&r));
    let Some(book) = book else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    page(&state, "ledger/book_form.html", json!({"book": book, "is_edit": true})).into_response()
}

async fn book_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(book_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let _ = sqlx::query("UPDATE ledger_books SET name = ?, description = ? WHERE id = ? AND user_id = ?")
        .bind(&name)
        .bind(fv(&form, "description"))
        .bind(book_id)
        .bind(user.0.id)
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/ledger/select").into_response()
}

async fn book_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(book_id): Path<i64>,
) -> Response {
    let _ = sqlx::query("DELETE FROM ledger_books WHERE id = ? AND user_id = ?")
        .bind(book_id)
        .bind(user.0.id)
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/ledger/select").into_response()
}

// -------------------------------------------------------------- ledger view

async fn ledger_dashboard(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(book_id) = params.get("book_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let Some((book, owner_id)) = load_book(&state.pecuniary, book_id).await else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let is_owner = owner_id == user.0.id;
    if !is_owner && !has_book_access(&state.pecuniary, book_id, user.0.id).await {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    }

    let page_num: i64 = params.get("page").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let per_page: i64 = params.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE book_id = ?")
        .bind(book_id)
        .fetch_one(&state.pecuniary)
        .await
        .unwrap_or(0);

    let rows = sqlx::query(
        "SELECT * FROM ledger_entries WHERE book_id = ? ORDER BY entry_date DESC, id DESC LIMIT ? OFFSET ?",
    )
    .bind(book_id)
    .bind(per_page)
    .bind((page_num - 1) * per_page)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();

    let entries: Vec<Value> = rows.iter().map(entry_json).collect();
    let total_income: f64 = entries
        .iter()
        .filter(|e| e["category"] == "income")
        .map(|e| e["amount"].as_f64().unwrap_or(0.0))
        .sum();
    let total_liability: f64 = entries
        .iter()
        .filter(|e| e["category"] != "income")
        .map(|e| e["amount"].as_f64().unwrap_or(0.0))
        .sum();

    let (shared, candidates) = if is_owner {
        (
            shared_users(&state, "ledger_shares", "book_id", book_id).await,
            share_candidates(&state, "ledger_shares", "book_id", book_id, user.0.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };

    page(
        &state,
        "ledger/ledger.html",
        json!({
            "book": book, "book_id": book_id, "is_owner": is_owner,
            "entries": entries,
            "total_income": total_income, "total_liability": total_liability,
            "net_cash_flow": total_income - total_liability,
            "income_types": INCOME_TYPES,
            "liability_types": LIABILITY_TYPES,
            "recurrence_periods": RECURRENCE_PERIODS,
            "shared_users": shared, "all_users": candidates,
            "page": page_num, "per_page": per_page, "total": total,
            "total_pages": if total > 0 { (total + per_page - 1) / per_page } else { 1 },
        }),
    )
    .into_response()
}

// ------------------------------------------------------------------ entries

async fn entry_new(
    _user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(book_id) = params.get("book_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let category = if params.get("category").map(|c| c == "income").unwrap_or(true) {
        "income"
    } else {
        "liability"
    };
    let entry_types: Vec<&str> = if category == "income" { INCOME_TYPES.to_vec() } else { LIABILITY_TYPES.to_vec() };
    let book_name = load_book(&state.pecuniary, book_id)
        .await
        .map(|(b, _)| b["name"].clone())
        .unwrap_or_else(|| json!("Ledger"));
    page(
        &state,
        "ledger/entry_form.html",
        json!({
            "book_id": book_id, "category": category, "entry_types": entry_types,
            "recurrence_periods": RECURRENCE_PERIODS,
            "is_edit": false, "today": crate::today(), "book_name": book_name,
        }),
    )
    .into_response()
}

async fn entry_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(book_id) = fv(&form, "book_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let back = format!("/pecuniary/ledger/?book_id={book_id}");
    if !has_book_access(&state.pecuniary, book_id, user.0.id).await {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    }
    let (Some(category), Some(entry_type), Some(amount), Some(entry_date)) = (
        fv(&form, "category"),
        fv(&form, "entry_type"),
        fnum(&form, "amount"),
        fv(&form, "entry_date"),
    ) else {
        return Redirect::to(&back).into_response();
    };
    let recurrence = fv(&form, "recurrence").unwrap_or_else(|| "one_time".into());
    let _ = sqlx::query(
        "INSERT INTO ledger_entries (book_id, user_id, category, entry_type, description, amount, entry_date, recurrence, source, reference_number, notes) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(book_id)
    .bind(user.0.id)
    .bind(&category)
    .bind(&entry_type)
    .bind(fv(&form, "description").unwrap_or_default())
    .bind(amount)
    .bind(&entry_date)
    .bind(&recurrence)
    .bind(fv(&form, "source"))
    .bind(fv(&form, "reference_number"))
    .bind(fv(&form, "notes"))
    .execute(&state.pecuniary)
    .await;
    Redirect::to(&back).into_response()
}

async fn entry_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(entry_id): Path<i64>,
) -> Response {
    let row = sqlx::query("SELECT * FROM ledger_entries WHERE id = ? AND user_id = ?")
        .bind(entry_id)
        .bind(user.0.id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let entry = entry_json(&row);
    let book_id = entry["book_id"].as_i64().unwrap_or(0);
    let is_income = entry["category"] == "income";
    let book_name = load_book(&state.pecuniary, book_id)
        .await
        .map(|(b, _)| b["name"].clone())
        .unwrap_or_else(|| json!("Ledger"));
    page(
        &state,
        "ledger/entry_form.html",
        json!({
            "book_id": book_id, "category": entry["category"],
            "entry_types": if is_income { INCOME_TYPES.to_vec() } else { LIABILITY_TYPES.to_vec() },
            "recurrence_periods": RECURRENCE_PERIODS,
            "entry": entry, "is_edit": true, "book_name": book_name,
        }),
    )
    .into_response()
}

async fn entry_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(entry_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let row = sqlx::query("SELECT book_id FROM ledger_entries WHERE id = ? AND user_id = ?")
        .bind(entry_id)
        .bind(user.0.id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    let Some((Some(book_id),)) = row.map(|r| (r.try_get::<i64, _>("book_id").ok(),)) else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let (Some(entry_type), Some(amount), Some(entry_date)) = (
        fv(&form, "entry_type"),
        fnum(&form, "amount"),
        fv(&form, "entry_date"),
    ) else {
        return Redirect::to(&format!("/pecuniary/ledger/?book_id={book_id}")).into_response();
    };
    let recurrence = fv(&form, "recurrence").unwrap_or_else(|| "one_time".into());
    let _ = sqlx::query(
        "UPDATE ledger_entries SET entry_type = ?, description = ?, amount = ?, entry_date = ?, recurrence = ?, source = ?, reference_number = ?, notes = ?, updated_at = datetime('now') \
         WHERE id = ? AND user_id = ?",
    )
    .bind(&entry_type)
    .bind(fv(&form, "description").unwrap_or_default())
    .bind(amount)
    .bind(&entry_date)
    .bind(&recurrence)
    .bind(fv(&form, "source"))
    .bind(fv(&form, "reference_number"))
    .bind(fv(&form, "notes"))
    .bind(entry_id)
    .bind(user.0.id)
    .execute(&state.pecuniary)
    .await;
    Redirect::to(&format!("/pecuniary/ledger/?book_id={book_id}")).into_response()
}

async fn entry_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(entry_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let book_id: Option<i64> =
        sqlx::query_scalar("SELECT book_id FROM ledger_entries WHERE id = ? AND user_id = ?")
            .bind(entry_id)
            .bind(user.0.id)
            .fetch_optional(&state.pecuniary)
            .await
            .ok()
            .flatten();
    if let Some(bid) = book_id {
        let _ = sqlx::query("DELETE FROM ledger_entries WHERE id = ?")
            .bind(entry_id)
            .execute(&state.pecuniary)
            .await;
        return Redirect::to(&format!("/pecuniary/ledger/?book_id={bid}")).into_response();
    }
    if let Some(bid) = params.get("book_id") {
        return Redirect::to(&format!("/pecuniary/ledger/?book_id={bid}")).into_response();
    }
    Redirect::to("/pecuniary/ledger/select").into_response()
}

async fn entry_reconcile(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(entry_id): Path<i64>,
) -> Response {
    let book_id: Option<i64> =
        sqlx::query_scalar("SELECT book_id FROM ledger_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_optional(&state.pecuniary)
            .await
            .ok()
            .flatten();
    if let Some(bid) = book_id {
        if has_book_access(&state.pecuniary, bid, user.0.id).await {
            let _ = sqlx::query("UPDATE ledger_entries SET is_reconciled = 1 - is_reconciled WHERE id = ?")
                .bind(entry_id)
                .execute(&state.pecuniary)
                .await;
            return Redirect::to(&format!("/pecuniary/ledger/?book_id={bid}")).into_response();
        }
    }
    Redirect::to("/pecuniary/ledger/select").into_response()
}

// ---------------------------------------------------------------- analytics

async fn cashflow(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(book_id) = params.get("book_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    if !has_book_access(&state.pecuniary, book_id, user.0.id).await {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    }
    let target_year: i64 = params
        .get("year")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| chrono::Local::now().format("%Y").to_string().parse().unwrap_or(2026));

    let rows = sqlx::query(
        "SELECT * FROM ledger_entries WHERE book_id = ? AND substr(entry_date, 1, 4) = ? ORDER BY entry_date ASC",
    )
    .bind(book_id)
    .bind(target_year.to_string())
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();

    let mut monthly: Vec<(f64, f64)> = vec![(0.0, 0.0); 13];
    let mut income_by_type: Vec<std::collections::HashMap<String, f64>> =
        vec![std::collections::HashMap::new(); 13];
    let mut liability_by_type: Vec<std::collections::HashMap<String, f64>> =
        vec![std::collections::HashMap::new(); 13];

    for r in &rows {
        let entry = entry_json(r);
        let month = entry["entry_date"]
            .as_str()
            .and_then(|d| d.split('-').nth(1))
            .and_then(|m| m.parse::<usize>().ok())
            .unwrap_or(0);
        if !(1..=12).contains(&month) {
            continue;
        }
        let amt = entry["amount"].as_f64().unwrap_or(0.0);
        let etype = entry["entry_type"].as_str().unwrap_or("").to_string();
        if entry["category"] == "income" {
            monthly[month].0 += amt;
            *income_by_type[month].entry(etype).or_insert(0.0) += amt;
        } else {
            monthly[month].1 += amt;
            *liability_by_type[month].entry(etype).or_insert(0.0) += amt;
        }
    }

    let monthly_data: Value = {
        let mut m = serde_json::Map::new();
        for i in 1..=12 {
            m.insert(
                i.to_string(),
                json!({
                    "month": i,
                    "income": round2(monthly[i].0),
                    "liability": round2(monthly[i].1),
                    "net": round2(monthly[i].0 - monthly[i].1),
                    "income_by_type": income_by_type[i],
                    "liability_by_type": liability_by_type[i],
                }),
            );
        }
        Value::Object(m)
    };

    let total_income: f64 = monthly[1..].iter().map(|m| m.0).sum();
    let total_liability: f64 = monthly[1..].iter().map(|m| m.1).sum();

    let mut income_type_totals: std::collections::HashMap<String, f64> =
        std::collections::HashMap::new();
    let mut liability_type_totals: std::collections::HashMap<String, f64> =
        std::collections::HashMap::new();
    for i in 1..=12 {
        for (t, v) in &income_by_type[i] {
            *income_type_totals.entry(t.clone()).or_insert(0.0) += v;
        }
        for (t, v) in &liability_by_type[i] {
            *liability_type_totals.entry(t.clone()).or_insert(0.0) += v;
        }
    }

    let mut available_years: Vec<i64> =
        sqlx::query_scalar::<_, String>("SELECT DISTINCT substr(entry_date, 1, 4) FROM ledger_entries WHERE book_id = ? ORDER BY 1")
            .bind(book_id)
            .fetch_all(&state.pecuniary)
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|y| y.parse().ok())
            .collect();
    if !available_years.contains(&target_year) {
        available_years.push(target_year);
        available_years.sort();
    }

    let chart_data = json!({
        "labels": MONTH_NAMES[1..],
        "income": (1..=13).map(|i| if i <= 12 { round2(monthly[i].0) } else { 0.0 }).collect::<Vec<_>>(),
        "liability": (1..=13).map(|i| if i <= 12 { round2(monthly[i].1) } else { 0.0 }).collect::<Vec<_>>(),
        "net": (1..=13).map(|i| if i <= 12 { round2(monthly[i].0 - monthly[i].1) } else { 0.0 }).collect::<Vec<_>>(),
    });

    let book_name = load_book(&state.pecuniary, book_id)
        .await
        .map(|(b, _)| b["name"].clone())
        .unwrap_or_else(|| json!("Ledger"));

    page(
        &state,
        "ledger/cashflow.html",
        json!({
            "book_id": book_id, "year": target_year,
            "monthly_data": monthly_data,
            "chart_data": chart_data,
            "total_income": round2(total_income),
            "total_liability": round2(total_liability),
            "net_cash_flow": round2(total_income - total_liability),
            "income_type_totals": income_type_totals,
            "liability_type_totals": liability_type_totals,
            "available_years": available_years,
            "month_names": MONTH_NAMES,
            "book_name": book_name,
        }),
    )
    .into_response()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

async fn cashflow_data(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(book_id) = params.get("book_id").and_then(|v| v.parse::<i64>().ok()) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "book_id required");
    };
    if !has_book_access(&state.pecuniary, book_id, user.0.id).await {
        return err_json(axum::http::StatusCode::FORBIDDEN, "Access denied");
    }

    let (start_date, end_date) = (
        params.get("start_date").cloned(),
        params.get("end_date").cloned(),
    );
    let range = start_date.clone().zip(end_date.clone());
    let mut sql = "SELECT * FROM ledger_entries WHERE book_id = ?".to_string();
    if range.is_some() {
        sql.push_str(" AND entry_date >= ? AND entry_date <= ?");
    } else if let Some(y) = params.get("year") {
        sql.push_str(&format!(" AND substr(entry_date, 1, 4) = '{y}'"));
    }
    sql.push_str(" ORDER BY entry_date ASC");

    let mut q = sqlx::query(&sql).bind(book_id);
    if let Some((sd, ed)) = range {
        q = q.bind(sd).bind(ed);
    }
    let rows = q.fetch_all(&state.pecuniary).await.unwrap_or_default();

    let mut monthly: Vec<(f64, f64)> = vec![(0.0, 0.0); 13];
    let mut category_totals: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut entries_out: Vec<Value> = Vec::new();

    for r in &rows {
        let e = entry_json(r);
        let month = e["entry_date"]
            .as_str()
            .and_then(|d| d.split('-').nth(1))
            .and_then(|m| m.parse::<usize>().ok())
            .unwrap_or(0);
        let amt = e["amount"].as_f64().unwrap_or(0.0);
        let is_income = e["category"] == "income";
        if (1..=12).contains(&month) {
            if is_income {
                monthly[month].0 += amt;
            } else {
                monthly[month].1 += amt;
            }
        }
        let key = e["entry_type"].as_str().unwrap_or("").to_string();
        let cat = category_totals.entry(key).or_insert_with(|| json!({"total": 0.0, "category": e["category"].clone()}));
        if let Some(t) = cat.get_mut("total") {
            *t = json!(t.as_f64().unwrap_or(0.0) + amt);
        }
        entries_out.push(json!({
            "date": e["entry_date"], "category": e["category"], "type": e["entry_type"],
            "amount": amt, "description": e["description"],
        }));
    }

    let mut monthly_out = serde_json::Map::new();
    for (i, m) in monthly.iter().enumerate().take(13).skip(1) {
        monthly_out.insert(i.to_string(), json!({"income": m.0, "liability": m.1}));
    }

    axum::Json(json!({
        "entries": entries_out,
        "monthly": Value::Object(monthly_out),
        "category_totals": Value::Object(category_totals),
    }))
    .into_response()
}

// ------------------------------------------------------------------ sharing

async fn share(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let (Some(share_with), Some(book_id)) = (
        fv(&form, "share_with_user_id").and_then(|v| v.parse::<i64>().ok()),
        fv(&form, "book_id").and_then(|v| v.parse::<i64>().ok()),
    ) else {
        return Redirect::to("/pecuniary/ledger/select").into_response();
    };
    let back = format!("/pecuniary/ledger/?book_id={book_id}");
    if share_with == user.0.id {
        return Redirect::to(&back).into_response();
    }
    let owner = load_book(&state.pecuniary, book_id).await.map(|(_, o)| o);
    if owner != Some(user.0.id) {
        return Redirect::to(&back).into_response();
    }
    let _ = sqlx::query("INSERT OR IGNORE INTO ledger_shares (book_id, shared_with_user_id) VALUES (?, ?)")
        .bind(book_id)
        .bind(share_with)
        .execute(&state.pecuniary)
        .await;
    Redirect::to(&back).into_response()
}

async fn revoke(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(share_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let book_id: Option<i64> = sqlx::query_scalar("SELECT book_id FROM ledger_shares WHERE id = ?")
        .bind(share_id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    let mut redirect_bid = params.get("book_id").and_then(|v| v.parse::<i64>().ok());
    if let Some(bid) = book_id {
        if load_book(&state.pecuniary, bid).await.map(|(_, o)| o) == Some(user.0.id) {
            let _ = sqlx::query("DELETE FROM ledger_shares WHERE id = ?")
                .bind(share_id)
                .execute(&state.pecuniary)
                .await;
        }
        redirect_bid = redirect_bid.or(Some(bid));
    }
    if let Some(bid) = redirect_bid {
        return Redirect::to(&format!("/pecuniary/ledger/?book_id={bid}")).into_response();
    }
    Redirect::to("/pecuniary/ledger/select").into_response()
}
