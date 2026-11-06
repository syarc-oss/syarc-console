//! Stock tracker: portfolios of BUY/SELL/DIVIDEND/FEE transactions with
//! live Yahoo quotes, cost basis, and gain/loss reporting.

use std::collections::HashMap;

use axum::extract::{Path, Query, RawForm, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{form_map, fv, http_client, page, shared_users, share_candidates, usernames, yahoo_price};

pub const TX_TYPES: [&str; 4] = ["BUY", "SELL", "DIVIDEND", "FEE"];

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/pecuniary/stocks/select", get(select_portfolio))
        .route("/pecuniary/stocks/portfolio/new", get(portfolio_new).post(portfolio_create))
        .route("/pecuniary/stocks/portfolio/{portfolio_id}/delete", post(portfolio_delete))
        .route("/pecuniary/stocks/share", post(share))
        .route("/pecuniary/stocks/revoke/{share_id}", post(revoke))
        .route("/pecuniary/stocks/", get(dashboard))
        .route("/pecuniary/stocks", get(dashboard))
        .route("/pecuniary/stocks/add", get(add_form).post(add_submit))
        .route("/pecuniary/stocks/detail/{symbol}", get(detail))
        .route("/pecuniary/stocks/delete/{trans_id}", post(delete_tx))
        .route("/pecuniary/stocks/update/{trans_id}", get(update_form).post(update_submit))
        .route("/pecuniary/stocks/price/{symbol}", post(set_price))
}

// ------------------------------------------------------------------ helpers

fn portfolio_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64,_>("id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "broker": r.try_get::<String,_>("broker").unwrap_or_default(),
        "is_active": r.get::<i64,_>("is_active") != 0,
        "created_at": r.try_get::<String,_>("created_at").unwrap_or_default(),
    })
}

async fn load_portfolio(db: &syarc_plugin_api::Db, id: i64) -> Option<(Value, i64)> {
    let row = sqlx::query("SELECT * FROM stock_portfolios WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()?;
    let owner = sqlx::query_scalar::<_, i64>("SELECT user_id FROM stock_portfolios WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await
        .ok()?;
    Some((portfolio_json(&row), owner))
}

async fn has_access(db: &syarc_plugin_api::Db, id: i64, user_id: i64) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM stock_portfolios WHERE id = ? AND user_id = ? \
         UNION ALL SELECT 1 FROM stock_shares WHERE portfolio_id = ? AND shared_with_user_id = ?",
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

// ------------------------------------------------------------- select / CRUD

async fn select_portfolio(user: RequireUser, State(state): State<SharedState>) -> Response {
    let own = sqlx::query(
        "SELECT p.*, (SELECT COUNT(*) FROM stock_transactions t WHERE t.portfolio_id = p.id) AS entry_count \
         FROM stock_portfolios p WHERE p.user_id = ? AND p.is_active = 1 ORDER BY p.name",
    )
    .bind(user.0.id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let shared = sqlx::query(
        "SELECT p.*, sh.created_at AS shared_at, \
         (SELECT COUNT(*) FROM stock_transactions t WHERE t.portfolio_id = p.id) AS entry_count \
         FROM stock_shares sh \
         JOIN stock_portfolios p ON p.id = sh.portfolio_id \
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

    page(&state, "stocks/stocks_select.html", json!({"portfolios": portfolios})).into_response()
}

async fn portfolio_new(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    page(&state, "stocks/portfolio_form.html", json!({})).into_response()
}

async fn portfolio_create(
    user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(name) = fv(&form, "name") else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    let broker = fv(&form, "broker").unwrap_or_else(|| "Default".into());
    let _ = sqlx::query("INSERT INTO stock_portfolios (user_id, name, broker) VALUES (?, ?, ?)")
        .bind(user.0.id)
        .bind(&name)
        .bind(&broker)
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/stocks/select").into_response()
}

async fn portfolio_delete(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(portfolio_id): Path<i64>,
) -> Response {
    let _ = sqlx::query("DELETE FROM stock_portfolios WHERE id = ? AND user_id = ?")
        .bind(portfolio_id)
        .bind(user.0.id)
        .execute(&state.pecuniary)
        .await;
    Redirect::to("/pecuniary/stocks/select").into_response()
}

// ---------------------------------------------------------------- dashboard

async fn dashboard(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    let Some((portfolio, owner_id)) = load_portfolio(&state.pecuniary, portfolio_id).await else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    }
    let is_owner = owner_id == user.0.id;

    let rows = sqlx::query(
        "SELECT * FROM stock_transactions WHERE portfolio_id = ? ORDER BY symbol",
    )
    .bind(portfolio_id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();

    let mut symbols: Vec<String> = Vec::new();
    let mut shares_by: HashMap<String, f64> = HashMap::new();
    let mut cost_by: HashMap<String, f64> = HashMap::new();
    let mut brokers_by: HashMap<String, Vec<String>> = HashMap::new();

    for r in &rows {
        let sym = r.try_get::<String,_>("symbol").unwrap_or_default().to_uppercase();
        let tx_type = r.try_get::<String,_>("type").unwrap_or_default();
        let shares: f64 = r.get("shares");
        let total: f64 = r.get("total_value");
        if !symbols.contains(&sym) {
            symbols.push(sym.clone());
            shares_by.insert(sym.clone(), 0.0);
            cost_by.insert(sym.clone(), 0.0);
            brokers_by.insert(sym.clone(), Vec::new());
        }
        let broker = r.try_get::<String,_>("broker").unwrap_or_default();
        let brokers = brokers_by.get_mut(&sym).unwrap();
        if !brokers.contains(&broker) {
            brokers.push(broker);
        }
        let cur_shares = *shares_by.get(&sym).unwrap();
        let cur_cost = *cost_by.get(&sym).unwrap();
        match tx_type.as_str() {
            "BUY" => {
                shares_by.insert(sym.clone(), cur_shares + shares);
                cost_by.insert(sym.clone(), cur_cost + total);
            }
            "SELL" => {
                let avg_cost = if cur_shares > 0.0 { cur_cost / cur_shares } else { 0.0 };
                shares_by.insert(sym.clone(), cur_shares - shares);
                cost_by.insert(sym.clone(), cur_cost - avg_cost * shares);
            }
            "FEE" => {
                cost_by.insert(sym.clone(), cur_cost - total);
            }
            "DIVIDEND"
                if shares > 0.0 => {
                    shares_by.insert(sym.clone(), cur_shares + shares);
                    cost_by.insert(sym.clone(), cur_cost + total);
                }
            _ => {}
        }
    }

    let manual_rows = sqlx::query("SELECT symbol, price FROM stock_manual_prices")
        .fetch_all(&state.pecuniary)
        .await
        .unwrap_or_default();
    let manual: std::collections::HashMap<String, f64> = manual_rows
        .iter()
        .map(|r| (r.get::<String, _>("symbol"), r.get::<f64, _>("price")))
        .collect();

    let client = http_client();
    let mut portfolio_data: Vec<Value> = Vec::new();
    let (mut total_value, mut total_cost) = (0.0f64, 0.0f64);
    for sym in symbols {
        let sh = *shares_by.get(&sym).unwrap();
        let cb = *cost_by.get(&sym).unwrap();
        if sh <= 0.0 {
            continue;
        }
        let quoted = yahoo_price(&client, &sym).await;
        let (price, is_manual) = match quoted {
            Some(p) if p > 0.0 => (p, false),
            _ => (*manual.get(&sym).unwrap_or(&0.0), true),
        };
        let market_value = sh * price;
        let gl = market_value - cb;
        let gl_pct = if cb > 0.0 { gl / cb * 100.0 } else { 0.0 };
        total_value += market_value;
        total_cost += cb;
        portfolio_data.push(json!({
            "symbol": sym, "shares": sh, "cost_basis": cb,
            "market_value": market_value, "current_price": price,
            "manual_price": manual.get(&sym).copied().unwrap_or(0.0),
            "is_manual": is_manual,
            "gain_loss": gl, "gain_loss_pct": gl_pct,
            "brokers": brokers_by.get(&sym).unwrap(),
        }));
    }

    let (shared, candidates) = if is_owner {
        (
            shared_users(&state, "stock_shares", "portfolio_id", portfolio_id).await,
            share_candidates(&state, "stock_shares", "portfolio_id", portfolio_id, user.0.id).await,
        )
    } else {
        (Vec::new(), Vec::new())
    };

    page(
        &state,
        "stocks/stocks.html",
        json!({
            "portfolio": portfolio, "portfolio_id": portfolio_id, "is_owner": is_owner,
            "portfolio_data": portfolio_data,
            "total_value": total_value, "total_cost": total_cost,
            "total_gl": total_value - total_cost,
            "shared_users": shared, "all_users": candidates,
        }),
    )
    .into_response()
}

// ------------------------------------------------------------- transactions

async fn add_form(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let _ = user;
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    let next = params.get("next").cloned().unwrap_or_else(|| "/pecuniary/stocks".into());
    page(
        &state,
        "stocks/add.html",
        json!({"types": TX_TYPES, "next": next, "portfolio_id": portfolio_id}),
    )
    .into_response()
}

async fn add_submit(
    user: RequireUser,
    State(state): State<SharedState>,
    headers: axum::http::HeaderMap,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let next = fv(&form, "next").unwrap_or_else(|| "/pecuniary/stocks".into());
    let Some(portfolio_id) = fv(&form, "portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    }
    let parse_dec = |k: &str| fv(&form, k).map(|v| v.replace(['$', ','], "").parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);
    let (dec_shares, mut dec_price, mut dec_total) =
        (parse_dec("shares"), parse_dec("price_per_share"), parse_dec("total_value"));
    if dec_total == 0.0 && dec_shares > 0.0 && dec_price > 0.0 {
        dec_total = dec_shares * dec_price;
    }
    if dec_total > 0.0 && dec_price == 0.0 && dec_shares > 0.0 {
        dec_price = dec_total / dec_shares;
    }
    let (Some(symbol), Some(broker), Some(transaction_date), Some(tx_type)) = (
        fv(&form, "symbol"),
        fv(&form, "broker"),
        fv(&form, "transaction_date"),
        fv(&form, "type"),
    ) else {
        return Redirect::to(&format!("/pecuniary/stocks/add?portfolio_id={portfolio_id}&next={next}")).into_response();
    };

    let res = sqlx::query(
        "INSERT INTO stock_transactions (user_id, portfolio_id, symbol, broker, transaction_date, type, shares, price_per_share, total_value) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.0.id)
    .bind(portfolio_id)
    .bind(symbol.to_uppercase())
    .bind(&broker)
    .bind(&transaction_date)
    .bind(tx_type.to_uppercase())
    .bind(dec_shares)
    .bind(dec_price)
    .bind(dec_total)
    .execute(&state.pecuniary)
    .await;

    let is_ajax = headers
        .get("accept")
        .and_then(|a| a.to_str().ok())
        .map(|a| a.contains("application/json"))
        .unwrap_or(false)
        || headers.get("x-requested-with").and_then(|x| x.to_str().ok()) == Some("XMLHttpRequest");
    if is_ajax {
        if let Ok(res) = res {
            return axum::Json(json!({"success": true, "id": res.last_insert_rowid()})).into_response();
        }
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"success": false}))).into_response();
    }
    Redirect::to(&format!("/pecuniary/stocks/?portfolio_id={portfolio_id}")).into_response()
}

async fn detail(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(symbol): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(portfolio_id) = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    }
    let symbol = symbol.to_uppercase();
    let page_num: i64 = params.get("page").and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let per_page: i64 = params.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM stock_transactions WHERE portfolio_id = ? AND symbol = ?",
    )
    .bind(portfolio_id)
    .bind(&symbol)
    .fetch_one(&state.pecuniary)
    .await
    .unwrap_or(0);

    let rows = sqlx::query(
        "SELECT * FROM stock_transactions WHERE portfolio_id = ? AND symbol = ? \
         ORDER BY transaction_date DESC LIMIT ? OFFSET ?",
    )
    .bind(portfolio_id)
    .bind(&symbol)
    .bind(per_page)
    .bind((page_num - 1) * per_page)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();

    let transactions: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "transaction_date": r.try_get::<String,_>("transaction_date").unwrap_or_default(),
                "broker": r.try_get::<String,_>("broker").unwrap_or_default(),
                "type": r.try_get::<String,_>("type").unwrap_or_default(),
                "shares": r.get::<f64,_>("shares"),
                "price_per_share": r.get::<f64,_>("price_per_share"),
                "total_value": r.get::<f64,_>("total_value"),
                "symbol": r.try_get::<String,_>("symbol").unwrap_or_default(),
            })
        })
        .collect();

    let quoted = yahoo_price(&http_client(), &symbol).await;
    let manual: Option<f64> = sqlx::query_scalar("SELECT price FROM stock_manual_prices WHERE symbol = ?")
        .bind(&symbol)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    let (current_price, is_manual) = match quoted {
        Some(p) if p > 0.0 => (p, false),
        _ => (manual.unwrap_or(0.0), true),
    };

    page(
        &state,
        "stocks/stock_detail.html",
        json!({
            "symbol": symbol, "transactions": transactions, "portfolio_id": portfolio_id,
            "current_price": current_price, "manual_price": manual.unwrap_or(0.0),
            "is_manual": is_manual,
            "page": page_num, "per_page": per_page, "total": total,
            "total_pages": if total > 0 { (total + per_page - 1) / per_page } else { 1 },
        }),
    )
    .into_response()
}

async fn delete_tx(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(trans_id): Path<i64>,
) -> Response {
    let row = sqlx::query("SELECT symbol, portfolio_id FROM stock_transactions WHERE id = ? AND user_id = ?")
        .bind(trans_id)
        .bind(user.0.id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    if let Some(row) = row {
        let symbol = row.try_get::<String,_>("symbol").unwrap_or_default();
        let pid = row.try_get::<i64,_>("portfolio_id").ok();
        let _ = sqlx::query("DELETE FROM stock_transactions WHERE id = ?")
            .bind(trans_id)
            .execute(&state.pecuniary)
            .await;
        if let Some(pid) = pid {
            return Redirect::to(&format!("/pecuniary/stocks/detail/{symbol}?portfolio_id={pid}")).into_response();
        }
    }
    Redirect::to("/pecuniary/stocks/select").into_response()
}

async fn update_form(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(trans_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let next = params.get("next").cloned().unwrap_or_else(|| "/pecuniary/stocks".into());
    let row = sqlx::query("SELECT * FROM stock_transactions WHERE id = ? AND user_id = ?")
        .bind(trans_id)
        .bind(user.0.id)
        .fetch_optional(&state.pecuniary)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    let transaction = json!({
        "id": row.get::<i64,_>("id"),
        "symbol": row.try_get::<String,_>("symbol").unwrap_or_default(),
        "broker": row.try_get::<String,_>("broker").unwrap_or_default(),
        "transaction_date": row.try_get::<String,_>("transaction_date").unwrap_or_default(),
        "type": row.try_get::<String,_>("type").unwrap_or_default(),
        "shares": row.get::<f64,_>("shares"),
        "price_per_share": row.get::<f64,_>("price_per_share"),
        "total_value": row.get::<f64,_>("total_value"),
    });
    page(
        &state,
        "stocks/edit.html",
        json!({
            "transaction": transaction, "next": next, "types": TX_TYPES,
            "portfolio_id": row.try_get::<i64,_>("portfolio_id").ok(),
        }),
    )
    .into_response()
}

async fn update_submit(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(trans_id): Path<i64>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let next = fv(&form, "next").unwrap_or_else(|| "/pecuniary/stocks".into());
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM stock_transactions WHERE id = ? AND user_id = ?",
    )
    .bind(trans_id)
    .bind(user.0.id)
    .fetch_optional(&state.pecuniary)
    .await
    .ok()
    .flatten()
    .is_some();
    if !exists {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    }
    let parse_dec = |k: &str| fv(&form, k).map(|v| v.replace(['$', ','], "").parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);
    let (dec_shares, mut dec_price, mut dec_total) =
        (parse_dec("shares"), parse_dec("price_per_share"), parse_dec("total_value"));
    if dec_total == 0.0 && dec_shares > 0.0 && dec_price > 0.0 {
        dec_total = dec_shares * dec_price;
    }
    if dec_total > 0.0 && dec_price == 0.0 && dec_shares > 0.0 {
        dec_price = dec_total / dec_shares;
    }
    let (Some(symbol), Some(broker), Some(transaction_date), Some(tx_type)) = (
        fv(&form, "symbol"),
        fv(&form, "broker"),
        fv(&form, "transaction_date"),
        fv(&form, "type"),
    ) else {
        return Redirect::to(&format!("/pecuniary/stocks/update/{trans_id}?next={next}")).into_response();
    };
    // Accept a few date formats like the original.
    let parsed_date = crate::parse_ymd(&transaction_date)
        .or_else(|| chrono::NaiveDate::parse_from_str(&transaction_date, "%m/%d/%Y").ok())
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or(transaction_date.clone());

    let _ = sqlx::query(
        "UPDATE stock_transactions SET symbol = ?, broker = ?, transaction_date = ?, type = ?, shares = ?, price_per_share = ?, total_value = ? \
         WHERE id = ? AND user_id = ?",
    )
    .bind(symbol.to_uppercase())
    .bind(&broker)
    .bind(&parsed_date)
    .bind(tx_type.to_uppercase())
    .bind(dec_shares)
    .bind(dec_price)
    .bind(dec_total)
    .bind(trans_id)
    .bind(user.0.id)
    .execute(&state.pecuniary)
    .await;
    Redirect::to(&next).into_response()
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
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    let back = format!("/pecuniary/stocks/?portfolio_id={portfolio_id}");
    if share_with == user.0.id {
        return Redirect::to(&back).into_response();
    }
    if load_portfolio(&state.pecuniary, portfolio_id).await.map(|(_, o)| o) != Some(user.0.id) {
        return Redirect::to(&back).into_response();
    }
    let _ = sqlx::query("INSERT OR IGNORE INTO stock_shares (portfolio_id, shared_with_user_id) VALUES (?, ?)")
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
        sqlx::query_scalar("SELECT portfolio_id FROM stock_shares WHERE id = ?")
            .bind(share_id)
            .fetch_optional(&state.pecuniary)
            .await
            .ok()
            .flatten();
    let mut redirect_pid = params.get("portfolio_id").and_then(|v| v.parse::<i64>().ok());
    if let Some(pid) = portfolio_id {
        if load_portfolio(&state.pecuniary, pid).await.map(|(_, o)| o) == Some(user.0.id) {
            let _ = sqlx::query("DELETE FROM stock_shares WHERE id = ?")
                .bind(share_id)
                .execute(&state.pecuniary)
                .await;
        }
        redirect_pid = redirect_pid.or(Some(pid));
    }
    if let Some(pid) = redirect_pid {
        return Redirect::to(&format!("/pecuniary/stocks/?portfolio_id={pid}")).into_response();
    }
    Redirect::to("/pecuniary/stocks/select").into_response()
}

// ------------------------------------------------------------ manual price

async fn set_price(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(symbol): Path<String>,
    RawForm(raw): RawForm,
) -> Response {
    let form = form_map(&raw);
    let Some(portfolio_id) = fv(&form, "portfolio_id").and_then(|v| v.parse::<i64>().ok()) else {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    };
    if !has_access(&state.pecuniary, portfolio_id, user.0.id).await {
        return Redirect::to("/pecuniary/stocks/select").into_response();
    }
    let symbol = fv(&form, "symbol").unwrap_or(symbol).to_uppercase();
    let price = fv(&form, "price")
        .or_else(|| fv(&form, "price_per_share"))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    if price > 0.0 {
        let _ = sqlx::query(
            "INSERT INTO stock_manual_prices (symbol, price, updated_at) VALUES (?, ?, datetime('now')) \
             ON CONFLICT(symbol) DO UPDATE SET price = excluded.price, updated_at = datetime('now')",
        )
        .bind(&symbol)
        .bind(price)
        .execute(&state.pecuniary)
        .await;
    } else {
        let _ = sqlx::query("DELETE FROM stock_manual_prices WHERE symbol = ?")
            .bind(&symbol)
            .execute(&state.pecuniary)
            .await;
    }
    Redirect::to(&format!("/pecuniary/stocks/?portfolio_id={portfolio_id}")).into_response()
}
