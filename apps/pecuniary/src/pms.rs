//! Precious metals: portfolios of bullion transactions with live spot
//! pricing (Yahoo Finance commodities futures) and per-metal summaries.

use std::collections::HashMap;

use axum::extract::{Path, Query, RawForm, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{err_json, form_map, fv, http_client, page, shared_users, share_candidates, usernames, yahoo_price};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/pecuniary/pms/select", get(select_portfolio))
        .route("/pecuniary/pms/portfolio/new", get(portfolio_new).post(portfolio_create))
        .route("/pecuniary/pms/portfolio/{portfolio_id}/edit", get(portfolio_edit).post(portfolio_update))
        .route("/pecuniary/pms/portfolio/{portfolio_id}/delete", post(portfolio_delete))
        .route("/pecuniary/pms/share", post(share))
        .route("/pecuniary/pms/revoke/{share_id}", post(revoke))
        .route("/pecuniary/pms/", get(pms_root))
        .route("/pecuniary/pms", get(pms_root))
        .route("/pecuniary/pms/input", get(input_page))
        .route("/pecuniary/pms/transactions", get(list_transactions).post(create_transaction))
        .route("/pecuniary/pms/transactions/{transaction_id}", put(update_transaction).delete(delete_transaction))
        .route("/pecuniary/pms/transactions/summary", get(summary))
        .route("/pecuniary/pms/transactions/detailed-summary", get(detailed_summary))
        .route("/pecuniary/pms/current-values", get(list_current_values))
        .route("/pecuniary/pms/current-values/{metal}", put(update_current_value))
        .route("/pecuniary/pms/metal-current-values", get(metal_current_values))
        .route("/pecuniary/pms/fetch-prices", post(fetch_prices))
}

// ------------------------------------------------------------------ helpers

fn portfolio_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "description": r.try_get::<String,_>("description").ok(),
        "is_active": r.get::<i64,_>("is_active") != 0,
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
    })
}

async fn load_portfolio(db: &syarc_plugin_api::Db, id: i64) -> Option<(Value, i64)> {
    let row = sqlx::query("SELECT * FROM pms_portfolios WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM pms_portfolios WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .ok()?;
    Some((portfolio_json(&row), owner))
}

async fn has_access(db: &syarc_plugin_api::Db, id: i64, user_id: i64) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM pms_portfolios WHERE id = ? AND user_id = ? \
         UNION ALL SELECT 1 FROM pms_shares WHERE portfolio_id = ? AND shared_with_user_id = ?",
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

fn tx_json(r: &sqlx::sqlite::SqliteRow, current_values: &HashMap<String, f64>) -> Value {
    let metal = r.try_get::<String,_>("metal").unwrap_or_default();
    let ml = metal.to_lowercase();
    let cost: f64 = r.get("cost");
    let ounces: f64 = r.get("ounces");
    let current_value = current_values
        .get(&ml)
        .map(|p| json!(ounces * p))
        .unwrap_or(Value::Null);
    json!({
        "id": r.get::<i64,_>("id"),
        "metal": metal,
        "date_purchased": r.try_get::<String,_>("date_purchased").unwrap_or_default(),
        "date_received": r.try_get::<String,_>("date_received").ok(),
        "vendor": r.try_get::<String,_>("vendor").unwrap_or_default(),
        "units": r.get::<f64,_>("units"),
        "description": r.try_get::<String,_>("description").unwrap_or_default(),
        "mint": r.try_get::<String,_>("mint").unwrap_or_default(),
        "cost": cost,
        "ounces": ounces,
        "cost_per_ounce": if ounces > 0.0 { cost / ounces } else { 0.0 },
        "disposition": r.try_get::<String,_>("disposition").unwrap_or_default(),
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
        "created_by": r.get::<i64,_>("created_by"),
        "current_value": current_value,
    })
}

async fn current_values(db: &syarc_plugin_api::Db) -> HashMap<String, f64> {
    sqlx::query("SELECT metal, price FROM pms_current_values WHERE price IS NOT NULL")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            Some((
                r.try_get::<String,_>("metal").ok()?.to_lowercase(),
                r.get::<f64,_>("price"),
            ))
        })
        .collect()
}

// ------------------------------------------------------------- select / CRUD

async fn select_portfolio(user: RequireUser, State(state): State<SharedState>) -> Response {
    let own = sqlx::query(
        "SELECT p.*, (SELECT COUNT(*) FROM pms_transactions t WHERE t.portfolio_id = p.id) AS entry_count \
         FROM pms_portfolios p WHERE p.user_id = ? AND p.is_active = 1 ORDER BY p.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let shared = sqlx::query(
        "SELECT p.*, sh.created_at AS shared_at, \
         (SELECT COUNT(*) FROM pms_transactions t WHERE t.portfolio_id = p.id) AS entry_count \
         FROM pms_shares sh \
         JOIN pms_portfolios p ON p.id = sh.portfolio_id \
         WHERE sh.shared_with_user_id = ? AND p.is_active = 1 ORDER BY p.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let owner_ids: Vec<i64> = shared.iter().map(|r| r.get::<i64, _>("user_id")).collect();
    let names = usernames(&state, &owner_ids).await;

    let mut portfolios: Vec<Value> = own
        .iter()
        .map(|r| {
            json!({
                "portfolio": portfolio_json(r), "is_owner": true,
                "entry_count": r.get::<i64,_>("entry_count"), "shared_at": Value::Null,
            })
        })
        .collect();
    portfolios.extend(shared.iter().map(|r| {
        json!({
            "portfolio": portfolio_json(r), "is_owner": false,
            "entry_count": r.get::<i64,_>("entry_count"),
            "shared_at": r.try_get::<String,_>("shared_at").unwrap_or_default(),
            "owner_username": names.get(&r.get::<i64,_>("user_id")).cloned().unwrap_or_default(),
        })
    }));

    page(&state, "pms/pms_select.html", json!({"portfolios": portfolios})).into_response()
}

async fn portfolio_new(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    page(&state, "pms/portfolio_form.html", json!({})).into_response()
}

async fn portfolio_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return Redirect::to("/pecuniary/pms/select").into_response();
    };
    let _ = sqlx::query("INSERT INTO pms_portfolios (user_id, name, description) VALUES (?, ?, ?)")
        .bind(user.0.id)
        .bind(&name)
        .bind(fv(&form, "description"))
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/pms/select").into_response()
}

async fn portfolio_edit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(portfolio_id): Path<i64>,
) -> Response {
    let portfolio = sqlx::query("SELECT * FROM pms_portfolios WHERE id = ? AND user_id = ?")
        .bind(portfolio_id)
        .bind(user.0.id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten()
        .map(|r| portfolio_json(&r));
    let Some(portfolio) = portfolio else {
        return Redirect::to("/pecuniary/pms/select").into_response();
    };
    page(&state, "pms/portfolio_form.html", json!({"portfolio": portfolio, "is_edit": true})).into_response()
}

async fn portfolio_update(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(portfolio_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return Redirect::to("/pecuniary/pms/select").into_response();
    };
    let _ = sqlx::query("UPDATE pms_portfolios SET name = ?, description = ? WHERE id = ? AND user_id = ?")
        .bind(&name)
        .bind(fv(&form, "description"))
        .bind(portfolio_id)
        .bind(user.0.id)
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/pms/select").into_response()
}

async fn portfolio_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(portfolio_id): Path<i64>,
) -> Response {
    let _ = sqlx::query("DELETE FROM pms_portfolios WHERE id = ? AND user_id = ?")
        .bind(portfolio_id)
        .bind(user.0.id)
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/pms/select").into_response()
}

async fn pms_root(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/pms/select").into_response();
    };
    let Some((portfolio, owner_id)) = load_portfolio(&state.pecuniary, portfolio_id).await else {
        return Redirect::to("/pecuniary/pms/select").into_response();
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return Redirect::to("/pecuniary/pms/select").into_response();
    }
    let is_owner = owner_id == user.0.id;
    let (shared, candidates) = if is_owner {
        (
            shared_users(&state, "pms_shares", "portfolio_id", portfolio_id).await,
            share_candidates(&state, "pms_shares", "portfolio_id", portfolio_id, user.0.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    page(
        &state,
        "pms/listings.html",
        json!({
            "portfolio": portfolio, "portfolio_id": portfolio_id, "is_owner": is_owner,
            "shared_users": shared, "all_users": candidates,
        }),
    )
    .into_response()
}

async fn input_page(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/pms/select").into_response();
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return Redirect::to("/pecuniary/pms/select").into_response();
    }
    page(&state, "pms/input.html", json!({"portfolio_id": portfolio_id})).into_response()
}

// -------------------------------------------------------------- transactions

async fn list_transactions(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Invalid portfolio_id");
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return err_json(axum::http::StatusCode::FORBIDDEN, "No access to this portfolio");
    }
    let skip: i64 = params.get("skip").and_then(|v| v.parse().ok()).unwrap_or(0).max(0);
    let limit: i64 = params.get("limit").and_then(|v| v.parse().ok()).unwrap_or(200).clamp(1, 500);
    let rows = sqlx::query(
        "SELECT * FROM pms_transactions WHERE portfolio_id = ? ORDER BY date_purchased DESC LIMIT ? OFFSET ?",
    )
    .bind(portfolio_id)
    .bind(limit)
    .bind(skip)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let cv = current_values(&state.pecuniary).await;
    axum::Json(Value::Array(rows.iter().map(|r| tx_json(r, &cv)).collect())).into_response()
}

async fn create_transaction(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
    raw: axum::body::Bytes,
) -> Response {
    // Accept both JSON body and form-encoded body (the original used JSON).
    let body = String::from_utf8_lossy(&raw).to_string();
    let v: Value = if body.trim_start().starts_with('{') {
        serde_json::from_str(&body).unwrap_or(json!({}))
    } else {
        let form = form_map(&raw);
        let mut m = serde_json::Map::new();
        for (k, val) in form {
            m.insert(k, json!(val));
        }
        Value::Object(m)
    };
    let portfolio_id = params
        .get("portfolio_id")
        .and_then(|p| p.parse::<i64>().ok())
        .or_else(|| v.get("portfolio_id").and_then(|p| p.as_i64()));
    let Some(portfolio_id) = portfolio_id else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Invalid portfolio_id");
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return err_json(axum::http::StatusCode::FORBIDDEN, "No access");
    }
    let gs = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());
    let gf = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let res = sqlx::query(
        "INSERT INTO pms_transactions (portfolio_id, metal, date_purchased, date_received, vendor, units, description, mint, cost, ounces, disposition, created_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(portfolio_id)
    .bind(gs("metal").unwrap_or_else(|| "silver".into()))
    .bind(gs("date_purchased").unwrap_or_else(crate::today))
    .bind(gs("date_received"))
    .bind(gs("vendor").unwrap_or_default())
    .bind(gf("units"))
    .bind(gs("description").unwrap_or_default())
    .bind(gs("mint").unwrap_or_default())
    .bind(gf("cost"))
    .bind(gf("ounces"))
    .bind(gs("disposition").unwrap_or_else(|| "pending".into()))
    .bind(user.0.id)
    .execute(&state.pecuniary)
    .await;
    let Ok(res) = res else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Insert failed");
    };
    let id = res.last_insert_rowid();
    let row = sqlx::query("SELECT * FROM pms_transactions WHERE id = ?")
        .bind(id)
        .fetch_one(&state.pecuniary)
        .await
        .unwrap();
    let cv = current_values(&state.pecuniary).await;
    axum::Json(tx_json(&row, &cv)).into_response()
}

async fn update_transaction(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(transaction_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
    raw: axum::body::Bytes,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Invalid portfolio_id");
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return err_json(axum::http::StatusCode::FORBIDDEN, "No access");
    }
    let body = String::from_utf8_lossy(&raw).to_string();
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    if let Some(dr) = v.get("date_received").and_then(|d| d.as_str()) {
        let _ = sqlx::query("UPDATE pms_transactions SET date_received = ? WHERE id = ? AND portfolio_id = ?")
            .bind(dr)
            .bind(transaction_id)
            .bind(portfolio_id)
            .execute(&state.pecuniary)
            .await;
    }
    if let Some(disp) = v.get("disposition").and_then(|d| d.as_str()) {
        let _ = sqlx::query("UPDATE pms_transactions SET disposition = ? WHERE id = ? AND portfolio_id = ?")
            .bind(disp)
            .bind(transaction_id)
            .bind(portfolio_id)
            .execute(&state.pecuniary)
            .await;
    }
    let row = sqlx::query("SELECT * FROM pms_transactions WHERE id = ? AND portfolio_id = ?")
        .bind(transaction_id)
        .bind(portfolio_id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Not found");
    };
    let cv = current_values(&state.pecuniary).await;
    axum::Json(tx_json(&row, &cv)).into_response()
}

async fn delete_transaction(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(transaction_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Invalid portfolio ID");
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return err_json(axum::http::StatusCode::FORBIDDEN, "Access denied");
    }
    let res = sqlx::query("DELETE FROM pms_transactions WHERE id = ? AND portfolio_id = ?")
        .bind(transaction_id)
        .bind(portfolio_id)
        .execute(&state.pecuniary)
        .await;
    if res.map(|r| r.rows_affected()).unwrap_or(0) == 0 {
        return err_json(axum::http::StatusCode::NOT_FOUND, "Transaction not found");
    }
    axum::Json(json!({"message": "Transaction deleted"})).into_response()
}

async fn summary(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Invalid portfolio ID");
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return err_json(axum::http::StatusCode::FORBIDDEN, "Access denied");
    }
    let (total_ounces, total_cost): (f64, f64) = sqlx::query(
        "SELECT COALESCE(SUM(ounces), 0), COALESCE(SUM(cost), 0) FROM pms_transactions WHERE portfolio_id = ?",
    )
    .bind(portfolio_id)
    .fetch_one(&state.pecuniary)
    .await
    .map(|r| (r.get::<f64,_>(0), r.get::<f64,_>(1)))
    .unwrap_or((0.0, 0.0));

    let cv = current_values(&state.pecuniary).await;
    let current_total_value: f64 = sqlx::query(
        "SELECT metal, ounces FROM pms_transactions WHERE portfolio_id = ?",
    )
    .bind(portfolio_id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default()
    .iter()
    .filter_map(|r| {
        let ml = r.try_get::<String,_>("metal").ok()?.to_lowercase();
        let price = *cv.get(&ml)?;
        Some(r.get::<f64,_>("ounces") * price)
    })
    .sum();

    axum::Json(json!({
        "total_ounces": total_ounces,
        "total_cost": total_cost,
        "average_cost_per_ounce": if total_ounces > 0.0 { total_cost / total_ounces } else { 0.0 },
        "current_total_value": current_total_value,
    }))
    .into_response()
}

async fn detailed_summary(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return err_json(axum::http::StatusCode::BAD_REQUEST, "Invalid portfolio ID");
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return err_json(axum::http::StatusCode::FORBIDDEN, "Access denied");
    }
    let rows = sqlx::query(
        "SELECT metal, ounces, cost FROM pms_transactions WHERE portfolio_id = ?",
    )
    .bind(portfolio_id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();

    let mut by_metal: std::collections::HashMap<String, (f64, f64, i64)> =
        std::collections::HashMap::new();
    for r in &rows {
        let ml = r.try_get::<String,_>("metal").unwrap_or_default().to_lowercase();
        let e = by_metal.entry(ml).or_insert((0.0, 0.0, 0));
        e.0 += r.get::<f64,_>("ounces");
        e.1 += r.get::<f64,_>("cost");
        e.2 += 1;
    }

    let cv = current_values(&state.pecuniary).await;
    let mut detailed = Vec::new();
    let (mut grand_oz, mut grand_cost, mut grand_val) = (0.0, 0.0, 0.0);
    for (metal, (oz, cost, count)) in &by_metal {
        let cur_price = cv.get(metal).copied().unwrap_or(0.0);
        let cur_val = oz * cur_price;
        let gl = cur_val - cost;
        let gl_pct = if *cost > 0.0 { gl / cost * 100.0 } else { 0.0 };
        detailed.push(json!({
            "metal": metal, "total_ounces": oz, "total_cost": cost,
            "transaction_count": count,
            "avg_cost_per_ounce": if *oz > 0.0 { cost / oz } else { 0.0 },
            "current_price_per_ounce": cur_price,
            "current_total_value": cur_val,
            "gain_loss": gl, "gain_loss_percent": gl_pct,
        }));
        grand_oz += oz;
        grand_cost += cost;
        grand_val += cur_val;
    }
    detailed.sort_by(|a, b| a["metal"].as_str().cmp(&b["metal"].as_str()));

    axum::Json(json!({
        "by_metal": detailed,
        "grand_totals": {
            "total_ounces": grand_oz, "total_cost": grand_cost,
            "current_total_value": grand_val,
            "total_gain_loss": grand_val - grand_cost,
            "total_gain_loss_percent": if grand_cost > 0.0 { (grand_val - grand_cost) / grand_cost * 100.0 } else { 0.0 },
        }
    }))
    .into_response()
}

// ------------------------------------------------------------ current values

async fn list_current_values(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    let rows = sqlx::query("SELECT * FROM pms_current_values ORDER BY metal")
        .fetch_all(&state.pecuniary)
        .await
        .unwrap_or_default();
    axum::Json(Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "metal": r.try_get::<String,_>("metal").unwrap_or_default(),
                    "price": r.get::<f64,_>("price"),
                    "updated_at": r.try_get::<String,_>("updated_at").unwrap_or_default(),
                })
            })
            .collect(),
    ))
    .into_response()
}

async fn metal_current_values(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    let mut metals = serde_json::Map::new();
    for m in ["gold", "silver", "platinum", "palladium"] {
        metals.insert(
            m.to_string(),
            json!({"price": 0, "formatted": "$0.00"}),
        );
    }
    for r in sqlx::query("SELECT metal, price FROM pms_current_values")
        .fetch_all(&state.pecuniary)
        .await
        .unwrap_or_default()
    {
        let ml = r.try_get::<String,_>("metal").unwrap_or_default().to_lowercase();
        let price = r.get::<f64,_>("price");
        if metals.contains_key(&ml) {
            metals.insert(ml, json!({"price": price, "formatted": format!("${price:.2}")}));
        }
    }
    axum::Json(Value::Object(metals)).into_response()
}

async fn update_current_value(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(metal): Path<String>,
    raw: axum::body::Bytes,
) -> Response {
    let _ = user;
    let ml = metal.to_lowercase();
    let body = String::from_utf8_lossy(&raw).to_string();
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    if let Some(price) = v.get("price").and_then(|p| p.as_f64()) {
        let _ = sqlx::query(
            "INSERT INTO pms_current_values (metal, price, updated_at) VALUES (?, ?, datetime('now')) \
             ON CONFLICT(metal) DO UPDATE SET price = excluded.price, updated_at = datetime('now')",
        )
        .bind(&ml)
        .bind(price)
        .execute(&state.pecuniary)
        .await;
    }
    let row = sqlx::query("SELECT * FROM pms_current_values WHERE metal = ?")
        .bind(&ml)
        .fetch_one(&state.pecuniary)
        .await;
    match row {
        Ok(r) => axum::Json(json!({
            "metal": r.try_get::<String,_>("metal").unwrap_or_default(),
            "price": r.get::<f64,_>("price"),
            "updated_at": r.try_get::<String,_>("updated_at").unwrap_or_default(),
        }))
        .into_response(),
        Err(_) => err_json(axum::http::StatusCode::NOT_FOUND, "Not found"),
    }
}

async fn fetch_prices(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    let client = http_client();
    let tickers = [
        ("gold", "GC=F"),
        ("silver", "SI=F"),
        ("platinum", "PL=F"),
        ("palladium", "PA=F"),
    ];
    let mut results = Vec::new();
    for (metal, sym) in tickers {
        let price = yahoo_price(&client, sym).await;
        results.push((metal.to_string(), price));
    }

    let mut updated = Vec::new();
    for (metal, price) in results {
        if let Some(price) = price {
            let _ = sqlx::query(
                "INSERT INTO pms_current_values (metal, price, updated_at) VALUES (?, ?, datetime('now')) \
                 ON CONFLICT(metal) DO UPDATE SET price = excluded.price, updated_at = datetime('now')",
            )
            .bind(&metal)
            .bind(price)
            .execute(&state.pecuniary)
            .await;
            updated.push(json!({"metal": metal, "price": price}));
        }
    }
    axum::Json(json!({
        "status": "success",
        "message": format!("Updated {} metals.", updated.len()),
        "updated": updated,
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
    let (Some(share_with), Some(portfolio_id)) = (
        fv(&form, "share_with_user_id").and_then(|v| v.parse::<i64>().ok()),
        fv(&form, "portfolio_id").and_then(|v| v.parse::<i64>().ok()),
    ) else {
        return Redirect::to("/pecuniary/pms/select").into_response();
    };
    let back = format!("/pecuniary/pms/?portfolio_id={portfolio_id}");
    if share_with == user.0.id {
        return Redirect::to(&back).into_response();
    }
    if load_portfolio(&state.pecuniary, portfolio_id).await.map(|(_, o)| o) != Some(user.0.id) {
        return Redirect::to(&back).into_response();
    }
    let _ = sqlx::query("INSERT OR IGNORE INTO pms_shares (portfolio_id, shared_with_user_id) VALUES (?, ?)")
        .bind(portfolio_id)
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
    let portfolio_id: Option<i64> =
        sqlx::query_scalar("SELECT portfolio_id FROM pms_shares WHERE id = ?")
            .bind(share_id)
            .fetch_optional(&state.pecuniary)
            .await
            .ok()
            .flatten();
    let mut redirect_pid = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok());
    if let Some(pid) = portfolio_id {
        if load_portfolio(&state.pecuniary, pid).await.map(|(_, o)| o) == Some(user.0.id) {
            let _ = sqlx::query("DELETE FROM pms_shares WHERE id = ?")
                .bind(share_id)
                .execute(&state.pecuniary)
                .await;
        }
        redirect_pid = redirect_pid.or(Some(pid));
    }
    if let Some(pid) = redirect_pid {
        return Redirect::to(&format!("/pecuniary/pms/?portfolio_id={pid}")).into_response();
    }
    Redirect::to("/pecuniary/pms/select").into_response()
}
