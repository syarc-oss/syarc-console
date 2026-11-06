//! Pecuniary app: financial suite with checkbook registers, income/liability
//! ledger books with cash-flow analytics, precious-metals portfolios with
//! live pricing, and stock portfolios with live quotes. Ported from the
//! Python (fastmain) implementation; amounts are stored as plain REAL
//! columns instead of the original pgcrypto-encrypted values.

pub mod checkbook;
pub mod ledger;
pub mod pms;
pub mod stocks;

use std::collections::HashMap;

use axum::extract::State;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

pub struct PecuniaryPlugin;

impl PecuniaryPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("pecuniary", "Pecuniary", "/pecuniary")
            .icon("bi-cash-stack");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for PecuniaryPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/pecuniary", get(dashboard))
            .route("/pecuniary/", get(dashboard))
            .merge(checkbook::router())
            .merge(ledger::router())
            .merge(pms::router())
            .merge(stocks::router())
            .with_state(state)
    }
}

// ------------------------------------------------------------- shared utils

pub(crate) fn page(state: &SharedState, template: &str, ctx: Value) -> Html<String> {
    Html(state.templates.render(template, ctx))
}

pub(crate) fn err_json(status: axum::http::StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"detail": msg}))).into_response()
}

pub(crate) fn form_map(raw: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub(crate) fn fv(form: &HashMap<String, String>, key: &str) -> Option<String> {
    form.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub(crate) fn fnum(form: &HashMap<String, String>, key: &str) -> Option<f64> {
    fv(form, key).and_then(|v| {
        v.replace(['$', ','], "").parse::<f64>().ok()
    })
}

pub(crate) fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[allow(dead_code)]
pub(crate) fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[allow(dead_code)]
pub(crate) async fn all_users(db: &syarc_plugin_api::Db, exclude: i64) -> Vec<Value> {
    sqlx::query("SELECT id, username, email FROM users WHERE is_active = 1 AND id != ? ORDER BY username AND username NOT LIKE 'mesh-%'")
        .bind(exclude)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64,_>("id"),
                "username": r.try_get::<String,_>("username").unwrap_or_default(),
                "email": r.try_get::<String,_>("email").unwrap_or_default(),
            })
        })
        .collect()
}

/// Usernames from the MAIN db — the encrypted pecuniary DB has no users
/// table, so queries there must never JOIN users.
pub(crate) async fn usernames(
    state: &syarc_plugin_api::SharedState,
    ids: &[i64],
) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(50) {
        let mut q = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT id, username FROM users WHERE id IN (",
        );
        for (i, id) in chunk.iter().enumerate() {
            if i > 0 {
                q.push(",");
            }
            q.push_bind(id);
        }
        q.push(")");
        if let Ok(rows) = q.build().fetch_all(&state.db).await {
            for r in rows {
                out.insert(
                    r.get::<i64, _>("id"),
                    r.try_get::<String, _>("username").unwrap_or_default(),
                );
            }
        }
    }
    out
}

/// Shared-with lists for owner views: [{share:{id}, shared_user:{...}}].
/// Share rows live in the encrypted pecuniary DB, usernames in the main DB.
pub(crate) async fn shared_users(
    state: &syarc_plugin_api::SharedState,
    table: &str,
    fk_col: &str,
    resource_id: i64,
) -> Vec<Value> {
    let ids: Vec<(i64, i64)> = sqlx::query_as(&format!(
        "SELECT id AS share_id, shared_with_user_id FROM {table} WHERE {fk_col} = ?"
    ))
    .bind(resource_id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for (share_id, uid) in ids {
        if let Ok(Some(r)) = sqlx::query("SELECT id, username, email FROM users WHERE id = ?")
            .bind(uid)
            .fetch_optional(&state.db)
            .await
        {
            out.push(json!({
                "share": {"id": share_id},
                "shared_user": {
                    "id": r.get::<i64,_>("id"),
                    "username": r.try_get::<String,_>("username").unwrap_or_default(),
                    "email": r.try_get::<String,_>("email").unwrap_or_default(),
                }
            }));
        }
    }
    out
}

/// Owner-only user picker: active users excluding self and already-shared.
pub(crate) async fn share_candidates(
    state: &syarc_plugin_api::SharedState,
    table: &str,
    fk_col: &str,
    resource_id: i64,
    owner_id: i64,
) -> Vec<Value> {
    let shared: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT shared_with_user_id FROM {table} WHERE {fk_col} = ?"
    ))
    .bind(resource_id)
    .fetch_all(&state.pecuniary)
    .await
    .unwrap_or_default();
    let mut q = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, username, email FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%' AND id != ",
    );
    q.push_bind(owner_id);
    if !shared.is_empty() {
        q.push(" AND id NOT IN (");
        let mut sep = q.separated(", ");
        for uid in &shared {
            sep.push_bind(uid);
        }
        q.push(")");
    }
    q.push(" ORDER BY username");
    let rows = q
        .build_query_as::<(i64, String, String)>()
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    rows.iter()
        .map(|(id, username, email)| {
            json!({
                "id": id,
                "username": username,
                "email": email,
            })
        })
        .collect()
}

pub(crate) fn parse_ymd(s: &str) -> Option<chrono::NaiveDate> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(y, m, d)
}

// Yahoo Finance quote (same source as the Python implementation).
pub(crate) async fn yahoo_price(client: &reqwest::Client, symbol: &str) -> Option<f64> {
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{symbol}?range=1d&interval=1d"
    );
    let resp = client.get(&url).send().await.ok()?;
    let body: Value = resp.json().await.ok()?;
    body.get("chart")?
        .get("result")?
        .get(0)?
        .get("meta")?
        .get("regularMarketPrice")?
        .as_f64()
}

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .build()
        .unwrap_or_default()
}

// ---------------------------------------------------------------- dashboard

async fn dashboard(user: RequireUser, State(state): State<SharedState>) -> Response {
    let _ = user;
    page(
        &state,
        "pecuniary/dashboard.html",
        json!({
            "allowed_sub_apps": ["checkbook", "pms", "stocks", "ledger"],
        }),
    )
    .into_response()
}

// Keep Redirect import used even if submodules change; harmless.
#[allow(dead_code)]
fn _unused(_r: Redirect) {}
