use axum::extract::{Form, Path, State};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use syarc_plugin_api::{
    hash_password, AppManifest, AppPlugin, Db, RequireAdmin, SharedState,
};

pub struct AdminPlugin;

impl AdminPlugin {
    pub const MANIFEST: AppManifest = AppManifest::new("admin", "Administration", "/admin")
        .icon("bi-gear")
        .category("System")
        .hidden();
}

#[async_trait::async_trait]
impl AppPlugin for AdminPlugin {
    fn manifest(&self) -> &'static AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, _state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/admin", get(index))
            .route("/admin/", get(index))
            .route("/admin/users", get(users_list).post(users_create))
            .route("/admin/users/{id}", post(users_update))
            .route("/admin/users/{id}/delete", post(users_delete))
            .route("/admin/users/{id}/permissions", post(user_permissions))
            .route("/admin/categories", get(categories_list).post(categories_create))
            .route("/admin/categories/{id}/delete", post(categories_delete))
            .route("/admin/settings", get(settings_list).post(settings_save))
            .route("/admin/settings/tunnel", post(tunnel_toggle))
            .route("/admin/restart", post(restart_server))
            .route("/admin/system", get(system))
    }
}

fn page(state: &SharedState, template: &str, ctx: serde_json::Value) -> Html<String> {
    Html(state.templates.render(&format!("{template}.html"), ctx))
}

async fn index(user: RequireAdmin, State(state): State<SharedState>) -> Html<String> {
    page(
        &state,
        "admin_index",
        json!({
            "username": user.0.username,
            "version": env!("CARGO_PKG_VERSION"),
        }),
    )
}

#[derive(Deserialize)]
struct UserForm {
    username: String,
    email: String,
    password: String,
    role: String,
    is_active: Option<String>,
}

async fn users_list(user: RequireAdmin, State(state): State<SharedState>) -> Html<String> {
    let rows = sqlx::query(
        "SELECT id, username, email, role, is_active, created_at, last_login_at \
         FROM users ORDER BY id",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let apps: Vec<(String, String)> = sqlx::query_as::<_, (String, String)>(
        "SELECT id, name FROM apps WHERE is_active = 1 ORDER BY name",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let perms: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
        "SELECT user_id, app_id FROM user_app_permissions",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let apps_json: Vec<serde_json::Value> = apps
        .iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();

    let mut users: Vec<serde_json::Value> = Vec::new();
    let mut mesh: Vec<serde_json::Value> = Vec::new();
    for r in rows {
        let username: String = r.get("username");
        let id: i64 = r.get("id");
        let entry = json!({
            "id": id,
            "username": username,
            "email": r.get::<String, _>("email"),
            "role": r.get::<String, _>("role"),
            "is_active": r.get::<i64, _>("is_active") == 1,
            "created_at": r.get::<Option<String>, _>("created_at"),
            "last_login_at": r.get::<Option<String>, _>("last_login_at"),
            "is_admin": r.get::<String, _>("role") == "admin",
            "permissions": perms
                .iter()
                .filter(|(uid, _)| *uid == id)
                .map(|(_, app)| app.clone())
                .collect::<Vec<String>>(),
        });
        if username.starts_with("mesh-") {
            mesh.push(entry);
        } else {
            users.push(entry);
        }
    }
    page(
        &state,
        "admin_users",
        json!({
            "users": users,
            "mesh_users": mesh,
            "apps": apps_json,
            "me_id": user.0.id,
        }),
    )
}

async fn users_create(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Form(form): Form<UserForm>,
) -> impl IntoResponse {
    if form.password.len() < 8 {
        return Redirect::to("/admin/users");
    }
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO users (username, email, password_hash, role) VALUES (?, ?, ?, ?)",
    )
    .bind(&form.username)
    .bind(&form.email)
    .bind(hash_password(&form.password))
    .bind(if form.role == "admin" { "admin" } else { "user" })
    .execute(&state.db)
    .await;
    Redirect::to("/admin/users")
}

async fn users_update(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    Form(form): Form<UserForm>,
) -> impl IntoResponse {
    let active = form.is_active.is_some() as i64;
    sqlx::query("UPDATE users SET username = ?, email = ?, role = ?, is_active = ? WHERE id = ?")
        .bind(&form.username)
        .bind(&form.email)
        .bind(&form.role)
        .bind(active)
        .bind(id)
        .execute(&state.db)
        .await
        .ok();
    if !form.password.is_empty() && form.password.len() >= 8 {
        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(hash_password(&form.password))
            .bind(id)
            .execute(&state.db)
            .await
            .ok();
    }
    Redirect::to("/admin/users")
}

async fn users_delete(
    admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    if id != admin.0.id {
        sqlx::query("UPDATE users SET is_active = 0 WHERE id = ?")
            .bind(id)
            .execute(&state.db)
            .await
            .ok();
    }
    Redirect::to("/admin/users")
}

async fn user_permissions(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
    axum::extract::RawForm(raw): axum::extract::RawForm,
) -> impl IntoResponse {
    let form: std::collections::HashMap<String, String> =
        form_urlencoded::parse(&raw)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    let _ = sqlx::query("DELETE FROM user_app_permissions WHERE user_id = ?")
        .bind(id)
        .execute(&state.db)
        .await;
    for key in form.keys() {
        let Some(app_id) = key.strip_prefix("perm_") else { continue };
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO user_app_permissions (user_id, app_id)              SELECT ?, id FROM apps WHERE id = ?",
        )
        .bind(id)
        .bind(app_id)
        .execute(&state.db)
        .await;
    }
    Redirect::to("/admin/users")
}

#[derive(Deserialize)]
struct CategoryForm {
    name: String,
}

async fn categories_list(_admin: RequireAdmin, State(state): State<SharedState>) -> Html<String> {
    let mut cats: Vec<serde_json::Value> = Vec::new();
    let rows = sqlx::query(
        "SELECT c.id, c.name, c.slug, COUNT(a.id) AS app_count \
         FROM app_categories c LEFT JOIN apps a ON a.category_id = c.id \
         GROUP BY c.id ORDER BY c.name",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    for r in rows {
        cats.push(json!({
            "id": r.get::<i64, _>("id"),
            "name": r.get::<String, _>("name"),
            "slug": r.get::<String, _>("slug"),
            "app_count": r.get::<i64, _>("app_count"),
        }));
    }
    page(&state, "admin_categories", json!({ "categories": cats }))
}

async fn categories_create(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Form(form): Form<CategoryForm>,
) -> impl IntoResponse {
    let slug = slugify(&form.name);
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO app_categories (name, slug) VALUES (?, ?)",
    )
    .bind(&form.name)
    .bind(&slug)
    .execute(&state.db)
    .await;
    Redirect::to("/admin/categories")
}

async fn categories_delete(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    sqlx::query("UPDATE apps SET category_id = NULL WHERE category_id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .ok();
    sqlx::query("DELETE FROM app_categories WHERE id = ?")
        .bind(id)
        .execute(&state.db)
        .await
        .ok();
    Redirect::to("/admin/categories")
}

#[derive(Deserialize)]
struct SettingsForm {
    key: String,
    value: String,
}

/// One-line descriptions for known settings keys, shown on the admin
/// settings page. Unknown keys show an em dash.
fn setting_description(key: &str) -> &'static str {
    match key {
        "site_name" => "Site name shown in the console header and page titles.",
        "bearden_bridge_url" => "Bear Den bridge API base URL (e.g. http://192.168.1.10:8096) for mesh chat, radio control, and media.",
        "bearden_bridge_token" => "Admin bearer token for the Bear Den bridge API.",
        "mesh_instance_name" => "Name this console advertises to the mesh network.",
        "mesh_radio_enabled" => "Enable the local mesh radio transport (read at startup).",
        "mesh_radio_host" => "Serial device or host of the local mesh radio (e.g. /dev/mesh-console; read at startup).",
        "mesh_radio_bluetooth" => "Bluetooth address of the mesh radio, if using BT instead of serial.",
        "mesh_broker_enabled" => "Act as a mesh broker client: forward traffic to/from a remote console (read at startup).",
        "mesh_broker_serve_enabled" => "Serve as mesh broker: accept connections from remote consoles (e.g. prox-lxc1 uses server1 as broker; read at startup).",
        "mesh_broker_url" => "URL of the remote mesh broker to connect to (read at startup).",
        "mesh_broker_token" => "Shared secret for mesh broker client/server auth (read at startup).",
        "mesh_channel_replay_max_secs" => "When joining a mesh channel, replay at most this many seconds of history.",
        "mesh_media_dir" => "Directory where received mesh media clips are stored.",
        "mesh_retention_days" => "Days to keep mesh channel message history (0 = keep forever).",
        "sage_enabled" => "Enable the Sage AI assistant in mesh chat.",
        "sage_base_url" => "Base URL of the Sage AI backend API.",
        "sage_model" => "Model name to request from the Sage AI backend.",
        "sage_channels" => "Mesh channel names Sage listens on (comma-separated).",
        "sage_max_chars" => "Maximum response length Sage will send per message.",
        "sage_cooldown_secs" => "Minimum seconds between Sage responses.",
        "sage_cooldown_exempt" => "Comma-separated sender names exempt from the Sage cooldown (for testing).",
        "sage_workers" => "Number of concurrent Sage request workers.",
        "sage_kiwix_url" => "Kiwix-serve URL for Sage's offline knowledge search (empty disables search_knowledge).",
        "go2rtc_url" => "go2rtc stream proxy base URL for surveillance live view (checked per request; no restart needed).",
        "surveillance_recording_root" => "Root directory for surveillance camera recordings (NFS mount).",
        "surveillance_webhook_port" => "Port the console listens on for camera motion webhooks.",
        "surveillance_webhook_secret" => "Shared secret cameras send with motion webhooks.",
        "recording_enabled" => "Master switch for camera recording (per-camera overrides may also apply).",
        "recording_mode" => "Camera recording mode: continuous or motion.",
        "recording_retention_days" => "Days to keep surveillance recordings before pruning.",
        "wx_latitude" => "Latitude for calendar weather forecasts (admin-settable at /calendar/weather/location).",
        "wx_longitude" => "Longitude for calendar weather forecasts.",
        _ => "",
    }
}

async fn settings_list(_admin: RequireAdmin, State(state): State<SharedState>) -> Html<String> {
    let mut settings: Vec<serde_json::Value> = Vec::new();
    // Seed keys that only take effect via settings but have no row yet, so
    // they show up on the page (settings_list only renders existing rows).
    for key in ["mesh_retention_days"] {
        sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, '0')")
            .bind(key)
            .execute(&state.db)
            .await
            .ok();
    }
    let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    for r in rows {
        let key = r.get::<String, _>("key");
        settings.push(json!({
            "key": key,
            "value": r.get::<String, _>("value"),
            "desc": setting_description(&key),
        }));
    }
    // Bearden tunnel state (bridge GET /tunnel/config) for the toggle card.
    // Cache the full config whenever we see one: the bridge DELETES it on
    // disable, so the cache is what lets Enable restore the peer/token.
    let (tunnel_enabled, tunnel_error) = match bridge_get(&state.db, "/tunnel/config").await {
        Ok(v) => {
            let enabled = v["enabled"].as_bool().unwrap_or(false);
            if let Some(c) = v.get("config").and_then(|c| c.as_object()) {
                let _ = sqlx::query(
                    "INSERT INTO settings (key, value) VALUES ('bearden_tunnel_config', ?) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                )
                .bind(serde_json::to_string(c).unwrap_or_default())
                .execute(&state.db)
                .await;
            }
            (Some(enabled), None)
        }
        Err(e) => (None, Some(e)),
    };
    page(
        &state,
        "admin_settings",
        json!({ "settings": settings, "tunnel_enabled": tunnel_enabled, "tunnel_error": tunnel_error }),
    )
}

/// Enable or disable the local bearden-bridge tunnel. The bridge's
/// POST /tunnel/config REPLACES the whole config, so fetch the current
/// one, flip `enabled`, and send it back in full.
async fn tunnel_toggle(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Redirect {
    let enable = form.get("enabled").map(|v| v == "1" || v == "true").unwrap_or(false);
    let current = bridge_get(&state.db, "/tunnel/config").await.ok();
    // The bridge DELETES the config on disable, so when it comes back null
    // fall back to our cached copy (settings key bearden_tunnel_config).
    let mut body = current
        .as_ref()
        .and_then(|v| v.get("config"))
        .filter(|c| c.is_object())
        .cloned();
    if body.as_ref().map(|b| b.is_object()) != Some(true) {
        body = setting_get(&state.db, "bearden_tunnel_config")
            .await
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|c: &serde_json::Value| c.is_object());
    }
    let mut body = body.unwrap_or_else(|| json!({}));
    body["enabled"] = json!(enable);
    match bridge_post(&state.db, "/tunnel/config", &body).await {
        Ok(_) => Redirect::to("/admin/settings"),
        Err(_) => Redirect::to("/admin/settings"),
    }
}

async fn setting_get(db: &sqlx::SqlitePool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
}

fn bridge_http() -> &'static reqwest::Client {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap()
    })
}

async fn bridge_conf(db: &sqlx::SqlitePool) -> (String, String) {
    let url = setting_get(db, "bearden_bridge_url")
        .await
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            std::env::var("BEARDEN_BRIDGE_URL").unwrap_or_else(|_| "http://127.0.0.1:8095".into())
        });
    let token = setting_get(db, "bearden_bridge_token")
        .await
        .unwrap_or_else(|| std::env::var("BEARDEN_BRIDGE_TOKEN").unwrap_or_default());
    (url, token)
}

async fn bridge_get(db: &sqlx::SqlitePool, path: &str) -> Result<serde_json::Value, String> {
    let (url, token) = bridge_conf(db).await;
    let resp = bridge_http()
        .get(format!("{url}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("bridge unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("bridge rejected the token".into());
    }
    resp.json().await.map_err(|e| format!("bad reply: {e}"))
}

async fn bridge_post(
    db: &sqlx::SqlitePool,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (url, token) = bridge_conf(db).await;
    let resp = bridge_http()
        .post(format!("{url}{path}"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("bridge unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("bridge rejected the token".into());
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("bad reply: {e}"))?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v)
    } else {
        Err(v["error"].as_str().unwrap_or("bridge refused").to_string())
    }
}

/// Restart the console process. Both systemd units (syarc-console on the
/// dev box, litefs on the nodes) use Restart=always, so exiting recycles
/// the server and startup-only settings (e.g. mesh broker) are re-read.
async fn restart_server(_admin: RequireAdmin) -> Redirect {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        eprintln!("restart requested via admin settings page");
        std::process::exit(0);
    });
    Redirect::to("/admin/settings")
}

async fn settings_save(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Form(form): Form<SettingsForm>,
) -> impl IntoResponse {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(&form.key)
    .bind(&form.value)
    .execute(&state.db)
    .await
    .ok();
    Redirect::to("/admin/settings")
}

async fn system(user: RequireAdmin, State(state): State<SharedState>) -> Html<String> {
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let app_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apps WHERE is_active = 1")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    page(
        &state,
        "admin_system",
        json!({
            "username": user.0.username,
            "version": env!("CARGO_PKG_VERSION"),
            "user_count": user_count,
            "app_count": app_count,
            "rust_version": option_env!("CFG_RUST_VERSION").unwrap_or("stable"),
            "cluster": cluster_status().await,
        }),
    )
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()
}

async fn consul_primary_query() -> String {
    let url = format!(
        "http://{}/v1/kv/litefs/primary",
        env_or("CONSUL_HTTP_ADDR", "127.0.0.1:8500")
    );
    let Some(client) = http_client() else { return "unreachable".into() };
    let Ok(resp) = client.get(&url).send().await else {
        return "unreachable".into();
    };
    if !resp.status().is_success() {
        return "no lease".into();
    }
    let Ok(list) = resp.json::<serde_json::Value>().await else {
        return "unparseable".into();
    };
    list.as_array()
        .and_then(|a| a.first())
        .and_then(|kv| kv.get("Value").and_then(|v| v.as_str()))
        .and_then(|b64| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(b64).ok()
        })
        .map(|bytes| {
            let raw = String::from_utf8_lossy(&bytes).trim().to_string();
            serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| {
                    v.get("hostname")
                        .and_then(|h| h.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or(raw)
        })
        .unwrap_or_else(|| "unknown".into())
}

async fn litefs_local_available() -> bool {
    // A node running LiteFS exposes its HTTP API on localhost:20202.
    let Some(client) = http_client() else { return false };
    client
        .get("http://127.0.0.1:20202/db")
        .send()
        .await
        .is_ok()
}

async fn cluster_status() -> serde_json::Value {
    let hostname = whoami::fallible::hostname().unwrap_or_default();

    // LiteFS v0.5's /.primary returns 404; Consul holds the elected primary.
    let consul_primary = consul_primary_query().await;
    let litefs_role = if !litefs_local_available().await {
        "standalone (no LiteFS)".to_string()
    } else {
        let is_primary = !hostname.is_empty() && consul_primary == hostname;
        if is_primary {
            "primary".to_string()
        } else {
            format!("replica (primary: {consul_primary})")
        }
    };

    let library_root = {
        let music = env_or("LIBRARY_MUSIC_ROOT", "/usr/local/library/music");
        std::path::Path::new(&music)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/usr/local/library"))
    };
    let probe = library_root.join(".syarc-rw-probe");
    let library_writable = std::fs::write(&probe, b"x")
        .map(|_| {
            let _ = std::fs::remove_file(&probe);
            true
        })
        .unwrap_or(false);

    let storage_state = std::fs::read_to_string("/var/lib/syarc/etc/storage-state.json")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| json!({"storage_role": "unknown", "host": "-", "changed": "-"}));
    let zsync_last = std::fs::read_to_string("/var/lib/syarc/etc/zsync-last")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "never (no state file)".into());

    json!({
        "hostname": hostname,
        "litefs_role": litefs_role,
        "consul_primary": consul_primary,
        "library_writable": library_writable,
        "storage_state": storage_state,
        "zsync_last": zsync_last,
    })
}

fn slugify(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .to_lowercase();
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() { "uncategorized".into() } else { trimmed }
}

// Keep Db referenced so the plugin-api signature stays stable for future hooks.
#[allow(dead_code)]
fn _db_type_check(_: Db) {}
