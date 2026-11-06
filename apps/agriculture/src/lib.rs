//! Agriculture app suite: agronomy (orchard, garden), husbandry (eggs, milk,
//! meat), and foraging. Ported from the Python (fastmain) implementation.
//! Entity ids keep their UUID strings; user ids are console INTEGERs.

pub mod eggs;
pub mod foraging;
pub mod garden;
pub mod hub;
pub mod meat;
pub mod milk;
pub mod orchard;

use std::collections::HashMap;

use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Router;
use serde_json::{json, Value};

use syarc_plugin_api::SharedState;

/// Form body that accepts both urlencoded and multipart/form-data requests
/// (pages submit via fetch with FormData, which is multipart). Yields the
/// urlencoded byte string expected by `form_map`.
pub(crate) struct AgriForm(pub Vec<u8>);

impl<S: Send + Sync> axum::extract::FromRequest<S> for AgriForm {
    type Rejection = Response;

    async fn from_request(
        req: axum::http::Request<axum::body::Body>,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let is_multipart = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("multipart/form-data"));
        if !is_multipart {
            let axum::extract::RawForm(raw) =
                axum::extract::RawForm::from_request(req, state)
                    .await
                    .map_err(|e| e.into_response())?;
            return Ok(AgriForm(raw.to_vec()));
        }
        let mut multipart =
            axum::extract::Multipart::from_request(req, state)
                .await
                .map_err(|e| e.into_response())?;
        let mut pairs: Vec<(String, String)> = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| e.into_response())?
        {
            if let Some(name) = field.name().map(|n| n.to_string()) {
                let value = field.text().await.unwrap_or_default();
                pairs.push((name, value));
            }
        }
        let mut ser = form_urlencoded::Serializer::new(String::new());
        for (name, value) in &pairs {
            ser.append_pair(name, value);
        }
        Ok(AgriForm(ser.finish().into_bytes()))
    }
}


pub struct AgriculturePlugin;
pub struct AgronomyPlugin;
pub struct HusbandryPlugin;
pub struct OrchardPlugin;
pub struct GardenPlugin;
pub struct EggsPlugin;
pub struct MilkPlugin;
pub struct MeatPlugin;
pub struct ForagingPlugin;

impl AgriculturePlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("agriculture", "Agriculture", "/agriculture")
            .icon("bi-flower1");
}
impl AgronomyPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("agronomy", "Agronomy", "/agriculture/agronomy")
            .icon("bi-flower1")
            .hidden();
}
impl HusbandryPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("husbandry", "Husbandry", "/agriculture/husbandry")
            .icon("bi-heart")
            .hidden();
}
impl OrchardPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("orchard", "Orchard Management", "/orchard")
            .icon("bi-tree")
            .hidden();
}
impl GardenPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("garden", "Gardens", "/agriculture/agronomy/garden")
            .icon("bi-flower2")
            .hidden();
}
impl EggsPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("eggs", "Egg Management", "/eggs")
            .icon("bi-egg-fried")
            .hidden();
}
impl MilkPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("milk", "Milk Production", "/milk")
            .icon("bi-cup-straw")
            .hidden();
}
impl MeatPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("meat", "Meat Production", "/meat")
            .icon("bi-egg-fried")
            .hidden();
}
impl ForagingPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("foraging", "Foraging", "/agriculture/foraging")
            .icon("bi-basket3")
            .hidden();
}

macro_rules! impl_plugin {
    ($t:ty, $routes:expr) => {
        #[async_trait::async_trait]
        impl syarc_plugin_api::AppPlugin for $t {
            fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
                &Self::MANIFEST
            }
            fn router(&self, state: SharedState) -> Router<SharedState> {
                #[allow(unused_imports)]
                use axum::extract::State as _State;
                let _ = &state;
                $routes.with_state(state)
            }
        }
    };
}

impl_plugin!(AgriculturePlugin, hub::agriculture_router());
impl_plugin!(AgronomyPlugin, hub::agronomy_router());
impl_plugin!(HusbandryPlugin, hub::husbandry_router());
impl_plugin!(OrchardPlugin, orchard::router());
impl_plugin!(GardenPlugin, garden::router());
impl_plugin!(EggsPlugin, eggs::router());
impl_plugin!(MilkPlugin, milk::router());
impl_plugin!(MeatPlugin, meat::router());
impl_plugin!(ForagingPlugin, foraging::router());

// ------------------------------------------------------------- shared utils

pub(crate) fn page(state: &SharedState, template: &str, ctx: Value) -> Response {
    Html(state.templates.render(template, ctx)).into_response()
}

pub(crate) fn user_json(u: &syarc_plugin_api::User) -> Value {
    json!({"id": u.id, "username": u.username, "is_admin": u.role == "admin"})
}

pub(crate) fn redirect(to: &str) -> Response {
    Redirect::to(to).into_response()
}

pub(crate) fn err403(msg: &str) -> Response {
    err_json(axum::http::StatusCode::FORBIDDEN, msg)
}

pub(crate) fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}

pub(crate) fn err400(msg: &str) -> Response {
    err_json(axum::http::StatusCode::BAD_REQUEST, msg)
}

pub(crate) fn err_json(status: axum::http::StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"detail": msg}))).into_response()
}

pub(crate) fn ok_json(v: Value) -> Response {
    axum::Json(v).into_response()
}

pub(crate) fn form_map(raw: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub(crate) fn fv(form: &HashMap<String, String>, key: &str) -> Option<String> {
    form.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub(crate) fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(crate) fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub(crate) fn days_before_today(days: i64) -> String {
    (chrono::Local::now() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

/// Valid YYYY-MM-DD or None.
pub(crate) fn valid_date(s: &str) -> Option<String> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .map(|_| s.to_string())
}

/// Owner-or-share access check, mirroring the Python `user_has_*_access`.
pub(crate) async fn user_has_access(
    db: &syarc_plugin_api::Db,
    owner_table: &str,
    share_table: &str,
    fk_col: &str,
    resource_id: &str,
    user_id: i64,
) -> bool {
    let owner: Option<i64> =
        sqlx::query_scalar(&format!("SELECT user_id FROM {owner_table} WHERE id = ?"))
            .bind(resource_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    match owner {
        Some(uid) if uid == user_id => true,
        Some(_) => {
            sqlx::query_scalar::<_, String>(&format!(
                "SELECT id FROM {share_table} WHERE {fk_col} = ? AND shared_with_user_id = ?"
            ))
            .bind(resource_id)
            .bind(user_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .is_some()
        }
        None => false,
    }
}

/// Users the resource is shared with: [{share:{id}, shared_user:{id,username,email}}]
pub(crate) async fn shared_users(
    db: &syarc_plugin_api::Db,
    share_table: &str,
    fk_col: &str,
    resource_id: &str,
) -> Vec<Value> {
    use sqlx::Row;
    let sql = format!(
        "SELECT sh.id AS share_id, u.id, u.username, u.email FROM {share_table} sh \
         JOIN users u ON u.id = sh.shared_with_user_id WHERE sh.{fk_col} = ?"
    );
    sqlx::query(&sql)
        .bind(resource_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!([
                {"id": r.get::<String,_>("share_id")},
                {
                    "id": r.get::<i64,_>("id"),
                    "username": r.try_get::<String,_>("username").unwrap_or_default(),
                    "email": r.try_get::<String,_>("email").unwrap_or_default(),
                },
            ])
        })
        .collect()
}

/// All active users except the owner and already-shared ones, for share pickers.
pub(crate) async fn shareable_users(
    db: &syarc_plugin_api::Db,
    share_table: &str,
    fk_col: &str,
    resource_id: &str,
    owner_id: i64,
) -> Vec<Value> {
    use sqlx::Row;
    let sql = format!(
        "SELECT id, username, email FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%' AND id != ? AND id NOT IN \
         (SELECT shared_with_user_id FROM {share_table} WHERE {fk_col} = ?) ORDER BY username"
    );
    sqlx::query(&sql)
        .bind(owner_id)
        .bind(resource_id)
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

/// Add a share row; returns Ok(()) or an error message.
/// Mirrors the Python share handler checks (owner-only, no self/dup).
pub(crate) async fn add_share(
    db: &syarc_plugin_api::Db,
    share_table: &str,
    fk_col: &str,
    resource_id: &str,
    owner_id: i64,
    shared_with: &str,
) -> Result<(), String> {
    let Ok(target) = shared_with.parse::<i64>() else {
        return Err("Invalid user ID.".into());
    };
    if target == owner_id {
        return Err("Cannot share with yourself.".into());
    }
    let exists: Option<String> = sqlx::query_scalar(&format!(
        "SELECT id FROM {share_table} WHERE {fk_col} = ? AND shared_with_user_id = ?"
    ))
    .bind(resource_id)
    .bind(target)
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?;
    if exists.is_some() {
        return Err("Already shared with this user.".into());
    }
    sqlx::query(&format!(
        "INSERT INTO {share_table} (id, {fk_col}, shared_with_user_id, created_at) VALUES (?, ?, ?, ?)"
    ))
    .bind(new_uuid())
    .bind(resource_id)
    .bind(target)
    .bind(now_str())
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) async fn revoke_share(
    db: &syarc_plugin_api::Db,
    share_table: &str,
    fk_col: &str,
    resource_id: &str,
    share_id: &str,
) {
    let _ = sqlx::query(&format!(
        "DELETE FROM {share_table} WHERE id = ? AND {fk_col} = ?"
    ))
    .bind(share_id)
    .bind(resource_id)
    .execute(db)
    .await;
}

/// Parse a JSON request body (report endpoints).
pub(crate) fn parse_json_body(raw: &[u8]) -> Value {
    serde_json::from_slice(raw).unwrap_or(Value::Null)
}
