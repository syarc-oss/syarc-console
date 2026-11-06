//! Chat Link app: links console users to their Bear Den bridge internet
//! identities (XMPP room nick for now) with a challenge-code verification
//! round trip. The bridge stays the policy enforcement point; this app is
//! the identity provider and management UI.
//!
//! Config (env): BEARDEN_BRIDGE_URL (default http://127.0.0.1:8095),
//! BEARDEN_BRIDGE_TOKEN (shared with the bridge's BEARDEN_ADMIN_TOKEN).

use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;

use syarc_plugin_api::{RequireAdmin, RequireUser, SharedState};

pub struct MeshlinkPlugin;

impl MeshlinkPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("meshlink", "Chat Link", "/meshlink")
            .icon("bi-broadcast");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for MeshlinkPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/meshlink", get(page))
            .route("/meshlink/", get(page))
            .route("/meshlink/link", post(link))
            .route("/meshlink/provision", post(provision))
            .route("/meshlink/verify", post(verify))
            .route("/meshlink/unlink", post(unlink))
            .route("/meshlink/admin/state", post(admin_state))
            .route("/meshlink/admin/tunnel_flush", post(tunnel_flush))
            .route("/meshlink/admin/tunnel_config", post(tunnel_config))
            .with_state(state)
    }

    /// Seed the bridge settings rows (admin-editable under Admin -> Settings)
    /// so they are discoverable; existing values are never overwritten.
    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        for (k, v) in [
            ("bearden_bridge_url", "http://127.0.0.1:8095"),
            ("bearden_bridge_token", ""),
        ] {
            sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, ?)")
                .bind(k)
                .bind(v)
                .execute(db)
                .await?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------- bridge client

/// Bridge admin API location/auth: settings table first, env fallback.
/// Keys: bearden_bridge_url, bearden_bridge_token.
async fn bridge_conf(db: &sqlx::SqlitePool) -> (String, String) {
    async fn get(k: &str, db: &sqlx::SqlitePool) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
            .bind(k)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
    }
    let url = get("bearden_bridge_url", db)
        .await
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::var("BEARDEN_BRIDGE_URL").unwrap_or_else(|_| "http://127.0.0.1:8095".into()));
    let token = get("bearden_bridge_token", db)
        .await
        .unwrap_or_else(|| std::env::var("BEARDEN_BRIDGE_TOKEN").unwrap_or_default());
    (url, token)
}

fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap())
}

/// POST JSON to the bridge admin API; Ok(payload) or Err(message).
async fn bridge_post(
    db: &sqlx::SqlitePool,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (url, token) = bridge_conf(db).await;
    let resp = http()
        .post(format!("{url}{path}"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("bridge unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("bridge rejected the token (check BEARDEN_BRIDGE_TOKEN)".into());
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("bad reply: {e}"))?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v)
    } else {
        Err(v["error"].as_str().unwrap_or("bridge refused").to_string())
    }
}

async fn bridge_get(db: &sqlx::SqlitePool, path: &str) -> Result<serde_json::Value, String> {
    let (url, token) = bridge_conf(db).await;
    let resp = http()
        .get(format!("{url}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("bridge unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("bridge rejected the token (check BEARDEN_BRIDGE_TOKEN)".into());
    }
    resp.json().await.map_err(|e| format!("bad reply: {e}"))
}

// -------------------------------------------------------------------- pages

async fn page(
    axum::extract::State(state): axum::extract::State<SharedState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    RequireUser(user): RequireUser,
) -> Response {
    let links = sqlx::query(
        "SELECT backend, backend_uid, state, linked_at, verified_at \
         FROM meshlink_links WHERE user_id = ? ORDER BY id",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let links: Vec<_> = links
        .iter()
        .map(|r| {
            use sqlx::Row;
            json!({
                "backend": r.get::<String, _>("backend"),
                "uid": r.get::<String, _>("backend_uid"),
                "state": r.get::<String, _>("state"),
                "linked_at": r.get::<String, _>("linked_at"),
                "verified_at": r.try_get::<Option<String>, _>("verified_at").ok().flatten(),
            })
        })
        .collect();

    // Admin extras: live view of the bridge registry + recent events.
    let (admin_users, admin_events, bridge_error, tunnel, tunnel_cfg) = if user.role == "admin" {
        match bridge_get(&state.db, "/users").await {
            Ok(users) => {
                let events = bridge_get(&state.db, "/events?limit=20").await.ok();
                let tunnel = bridge_get(&state.db, "/tunnel/status").await.ok();
                let tunnel_cfg = bridge_get(&state.db, "/tunnel/config").await.ok();
                let users: Vec<_> = users
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|u| {
                                let key = u["key"].as_str().unwrap_or("");
                                let (backend, uid) = key.split_once(':').unwrap_or(("", ""));
                                json!({
                                    "backend": backend,
                                    "uid": uid,
                                    "name": u["name"],
                                    "state": u["state"],
                                    "console_uid": u["console_uid"],
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (Some(users), events, None, tunnel, tunnel_cfg)
            }
            Err(e) => (None, None, Some(e), None, None),
        }
    } else {
        (None, None, None, None, None)
    };

    Html(state.templates.render(
        "meshlink.html",
        json!({
            "username": user.username,
            "request_msg": q.get("msg"),
            "links": links,
            "admin_users": admin_users,
            "admin_events": admin_events,
            "bridge_error": bridge_error,
            "tunnel": tunnel,
            "tunnel_cfg": tunnel_cfg.and_then(|v| v.get("config").cloned()).filter(|c| !c.is_null()).map(|c| json!({
                "enabled": !c.is_null(),
                "peer": c["peer"].clone(),
                "token": c["token"].clone(),
                "tag": c["tag"].clone(),
                "peer_tag": c["peer_tag"].clone(),
                "flush_interval_secs": c["flush_interval_secs"].clone(),
                "mode": c["mode"].clone(),
            })),
        }),
    ))
    .into_response()
}

// ------------------------------------------------------------------- forms

/// Backends the console will forward to the bridge.
const BACKENDS: [&str; 2] = ["xmpp", "matrix"];

fn valid_uid(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

#[derive(Deserialize)]
struct LinkForm {
    backend: String,
    nick: String,
}

async fn link(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireUser(user): RequireUser,
    Form(form): Form<LinkForm>,
) -> Response {
    let backend = form.backend;
    if !BACKENDS.contains(&backend.as_str()) {
        return redirect_msg("/meshlink", "Unknown backend.");
    }
    let nick = form.nick.trim().to_string();
    if !valid_uid(&nick) {
        return redirect_msg("/meshlink", "Name must be letters/digits/-/_ only.");
    }
    if let Err(e) = bridge_post(
        &state.db,
        &format!("/users/{backend}/{nick}/link"),
        &json!({"console_uid": user.id.to_string(), "display_name": user.username}),
    )
    .await
    {
        return redirect_msg("/meshlink", &format!("Bridge: {e}"));
    }
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let _ = sqlx::query(
        "INSERT INTO meshlink_links (user_id, backend, backend_uid, display_name, state, linked_at) \
         VALUES (?, ?, ?, ?, 'pending', ?) \
         ON CONFLICT(user_id, backend, backend_uid) DO UPDATE SET state = 'pending', linked_at = excluded.linked_at",
    )
    .bind(user.id)
    .bind(&backend)
    .bind(&nick)
    .bind(&user.username)
    .bind(&now)
    .execute(&state.db)
    .await;

    let where_next = match backend.as_str() {
        "matrix" => format!(
            "Code sent: @bearden will DM @{nick}:matrix.example.net — check Element for the code."
        ),
        _ => format!(
            "Code sent: join mesh@conference.server1.example.net as '{nick}' and check your private messages."
        ),
    };
    redirect_msg("/meshlink", &where_next)
}

#[derive(Deserialize)]
struct ProvisionForm {
    xmpp_user: String,
    password: String,
}

/// Create an XMPP account on the local Prosody (via the privileged helper)
/// and mark its room nick approved on the bridge immediately: the account
/// was created by us, so no challenge round trip is needed.
async fn provision(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireUser(user): RequireUser,
    Form(form): Form<ProvisionForm>,
) -> Response {
    let name = form.xmpp_user.trim().to_lowercase();
    let password = form.password;
    let valid = name.len() >= 3
        && name.len() <= 20
        && name.chars().next().map(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'));
    if !valid {
        return redirect_msg(
            "/meshlink",
            "Chat username must be 3-20 chars: lowercase letters, digits, . - _ (letter/digit first).",
        );
    }
    if password.len() < 8 {
        return redirect_msg("/meshlink", "Chat password must be at least 8 characters.");
    }

    let out = tokio::process::Command::new("sudo")
        .arg("-n")
        .arg("/usr/local/bin/prosody-create-account.sh")
        .arg(&name)
        .arg(&password)
        .output()
        .await;
    let ok = matches!(&out, Ok(o) if o.status.success());
    if !ok {
        let detail = out
            .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
            .unwrap_or_else(|e| e.to_string());
        let hint = match detail.as_str() {
            "EXISTS" => "that chat username is taken".to_string(),
            "BADUSERNAME" | "BADPASSWORD" => format!("invalid input ({detail})"),
            e if e.contains("a password is required")
                || e.contains("command not found")
                || e.contains("NOTFOUND") => {
                "server-side setup missing: install tools/prosody-create-account.sh and its sudoers rule".to_string()
            }
            "" => "prosodyctl failed".to_string(),
            e => e.to_string(),
        };
        return redirect_msg("/meshlink", &format!("Account not created: {hint}."));
    }

    // Account exists and is ours: vouch on the bridge, record locally.
    if let Err(e) = bridge_post(
        &state.db,
        &format!("/users/xmpp/{name}/vouch"),
        &json!({"console_uid": user.id.to_string(), "display_name": user.username}),
    )
    .await
    {
        return redirect_msg(
            "/meshlink",
            &format!("Account created as {name}@server1.example.net, but bridge vouch failed: {e}"),
        );
    }
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let _ = sqlx::query(
        "INSERT INTO meshlink_links (user_id, backend, backend_uid, display_name, state, linked_at, verified_at)          VALUES (?, 'xmpp', ?, ?, 'verified', ?, ?)          ON CONFLICT(user_id, backend, backend_uid) DO UPDATE SET state = 'verified', verified_at = excluded.verified_at",
    )
    .bind(user.id)
    .bind(&name)
    .bind(&user.username)
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await;

    redirect_msg(
        "/meshlink",
        &format!("Chat account {name}@server1.example.net created and approved. Join mesh@conference.server1.example.net with nick '{name}'."),
    )
}

#[derive(Deserialize)]
struct VerifyForm {
    backend: String,
    nick: String,
    code: String,
}

async fn verify(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireUser(user): RequireUser,
    Form(form): Form<VerifyForm>,
) -> Response {
    let backend = form.backend;
    if !BACKENDS.contains(&backend.as_str()) {
        return redirect_msg("/meshlink", "Unknown backend.");
    }
    let nick = form.nick.trim().to_string();
    let code = form.code.trim().to_string();
    // Ownership check happens bridge-side; only rows belonging to this
    // console user are flipped locally.
    let owns: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM meshlink_links WHERE user_id = ? AND backend = ? AND backend_uid = ?",
    )
    .bind(user.id)
    .bind(&backend)
    .bind(&nick)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if owns.is_none() {
        return redirect_msg("/meshlink", "That nick is not linked to your account.");
    }
    match bridge_post(&state.db, &format!("/users/{backend}/{nick}/verify"), &json!({"code": code})).await {
        Ok(_) => {
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let _ = sqlx::query(
                "UPDATE meshlink_links SET state = 'verified', verified_at = ? WHERE user_id = ? AND backend_uid = ?",
            )
            .bind(&now)
            .bind(user.id)
            .bind(&nick)
            .execute(&state.db)
            .await;
            redirect_msg("/meshlink", "Verified! Your messages now reach the mesh.")
        }
        Err(e) => redirect_msg("/meshlink", &format!("Verify failed: {e}")),
    }
}

#[derive(Deserialize)]
struct UnlinkForm {
    backend: String,
    nick: String,
}

async fn unlink(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireUser(user): RequireUser,
    Form(form): Form<UnlinkForm>,
) -> Response {
    let _ = sqlx::query(
        "DELETE FROM meshlink_links WHERE user_id = ? AND backend = ? AND backend_uid = ?",
    )
    .bind(user.id)
    .bind(&form.backend)
    .bind(form.nick.trim())
    .execute(&state.db)
    .await;
    Redirect::to("/meshlink").into_response()
}

#[derive(Deserialize)]
struct AdminStateForm {
    backend: String,
    uid: String,
    action: String, // approve | ban | deny
}

async fn admin_state(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireAdmin(_admin): RequireAdmin,
    Form(form): Form<AdminStateForm>,
) -> Response {
    let path = format!("/users/{}/{}/{}", form.backend, form.uid, form.action);
    match bridge_post(&state.db, &path, &json!({})).await {
        Ok(_) => {
            if form.action == "deny" {
                let _ = sqlx::query(
                    "DELETE FROM meshlink_links WHERE backend = ? AND backend_uid = ?",
                )
                .bind(&form.backend)
                .bind(form.uid.trim())
                .execute(&state.db)
                .await;
            }
            redirect_msg("/meshlink", &format!("{} {}d.", form.uid, form.action))
        }
        Err(e) => redirect_msg("/meshlink", &format!("Bridge: {e}")),
    }
}

/// Force a tunnel digest flush to the peer Bear Den.
async fn tunnel_flush(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireAdmin(_admin): RequireAdmin,
) -> Response {
    match bridge_post(&state.db, "/tunnel/flush", &json!({})).await {
        Ok(v) => {
            let sent = v.get("sent").and_then(|s| s.as_u64()).unwrap_or(0);
            redirect_msg("/meshlink", &format!("Tunnel flushed: {sent} messages sent."))
        }
        Err(e) => redirect_msg("/meshlink", &format!("Tunnel flush failed: {e}")),
    }
}

/// Update the bridge's tunnel configuration (peer, token, tags, interval,
/// mode) or disable it. Empty optional fields keep their current values.
async fn tunnel_config(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireAdmin(_admin): RequireAdmin,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Response {
    let enabled = form.get("enabled").map(|v| v != "0" && v != "false").unwrap_or(false);
    if !enabled {
        return match bridge_post(&state.db, "/tunnel/config", &json!({"enabled": false})).await {
            Ok(_) => redirect_msg("/meshlink", "Tunnel disabled."),
            Err(e) => redirect_msg("/meshlink", &format!("Tunnel: {e}")),
        };
    }
    let mut body = json!({"enabled": true});
    for k in ["peer", "token", "tag", "peer_tag", "mode"] {
        if let Some(v) = form.get(k) {
            if !v.trim().is_empty() || k == "peer" || k == "token" {
                body[k] = json!(v.trim());
            }
        }
    }
    if let Some(v) = form.get("flush_interval_secs") {
        if let Ok(secs) = v.trim().parse::<u64>() {
            body["flush_interval_secs"] = json!(secs);
        }
    }
    match bridge_post(&state.db, "/tunnel/config", &body).await {
        Ok(_) => redirect_msg("/meshlink", "Tunnel configuration saved."),
        Err(e) => redirect_msg("/meshlink", &format!("Tunnel config failed: {e}")),
    }
}

fn redirect_msg(to: &str, msg: &str) -> Response {
    let sep = if to.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{to}{sep}msg={}", urlencode(msg))).into_response()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
