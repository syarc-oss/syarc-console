//! Remote management of MeshCore repeaters over the air (meshcli-style).
//!
//! A "Remote management" button on a contact's page opens an admin console
//! for that node: login with the node's admin password, then read live
//! status, run CLI commands, and change settings — same flow as fastmain's
//! remote repeater console / the MeshCore phone app's remote admin.

use axum::extract::{Path, Query, RawForm, State};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::meshcore::form_value;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/platforms/meshcore/contacts/{node_id}/admin", get(admin_entry))
        .route("/platforms/meshcore/contacts/{node_id}/admin/", get(admin_entry))
        .route("/platforms/meshcore/contacts/{node_id}/admin/login", post(admin_login))
        .route("/platforms/meshcore/contacts/{node_id}/admin/logout", post(admin_logout))
        .route("/platforms/meshcore/contacts/{node_id}/admin/status", get(admin_status))
        .route("/platforms/meshcore/contacts/{node_id}/admin/console", get(admin_console))
        .route("/platforms/meshcore/contacts/{node_id}/admin/cmd", post(admin_cmd))
        .route("/platforms/meshcore/contacts/{node_id}/admin/settings", get(admin_settings))
        .route("/platforms/meshcore/contacts/{node_id}/admin/settings/apply", post(admin_settings_apply))
}

#[derive(serde::Deserialize)]
struct AdminFlags {
    err: Option<String>,
    cmd: Option<String>,
    out: Option<String>,
    saved: Option<String>,
}

fn page(state: &SharedState, template: &str, ctx: serde_json::Value) -> axum::response::Response {
    Html(state.templates.render(template, ctx)).into_response()
}

async fn load_contact(
    state: &SharedState,
    node_id: &str,
) -> Option<serde_json::Value> {
    let row = sqlx::query(
        "SELECT node_id, name, node_type, admin_password, admin_mode FROM mesh_directory WHERE node_id = ?",
    )
    .bind(node_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    Some(json!({
        "node_id": row.try_get::<String, _>("node_id").unwrap_or_default(),
        "name": row.try_get::<String, _>("name").unwrap_or_default(),
        "node_type": row.try_get::<i64, _>("node_type").unwrap_or(1),
        "admin_password": row.try_get::<String, _>("admin_password").unwrap_or_default(),
        "admin_mode": row.try_get::<String, _>("admin_mode").unwrap_or_default(),
    }))
}

/// Radio access for remote admin: the local radio when this console owns
/// one, otherwise the mesh broker (which relays to the radio-owning host).
enum AdminLink {
    Radio(&'static std::sync::Arc<crate::radio::RadioClient>),
    Broker(std::sync::Arc<crate::broker::BrokerClient>),
}

async fn admin_link() -> Result<AdminLink, String> {
    if let Some(r) = crate::radio::radio() {
        if r.connected() {
            return Ok(AdminLink::Radio(r));
        }
        return Err(format!("MeshCore radio not connected ({})", r.detail()));
    }
    if let Some(b) = crate::broker::broker() {
        if b.connected() {
            return Ok(AdminLink::Broker(b.clone()));
        }
        return Err("no MeshCore radio on this console and mesh broker not connected".into());
    }
    Err("no MeshCore radio is configured on this console".into())
}

/// Log in to a room server over the air with the room (guest) or admin
/// password. Returns the node's permission byte (0=guest/read-only,
/// 2=read-write, 3=admin); posting requires bit 1 set. `name` is used to
/// push the contact into the radio's store if it isn't known there yet
/// (e.g. a re-flashed node with a new public key).
pub async fn room_server_login(node_id: &str, password: &str, name: &str) -> Result<u8, String> {
    let link = admin_link().await?;
    match &link {
        AdminLink::Radio(r) => match r.remote_login(node_id, password).await {
            Ok(perms) => Ok(perms),
            Err(e) if e.contains("device error") => {
                // contact not in the radio's store: add it and retry
                r.add_contact(node_id, name, 3).await?;
                r.remote_login(node_id, password).await
            }
            Err(e) => Err(e),
        },
        AdminLink::Broker(b) => b
            .admin_rpc("login", node_id, password, name)
            .await
            .and_then(|v| {
                v.as_u64().map(|u| u as u8).ok_or_else(|| "bad login reply".to_string())
            }),
    }
}

impl AdminLink {
    async fn login(&self, node: &str, password: &str) -> Result<u8, String> {
        match self {
            Self::Radio(r) => r.remote_login(node, password).await,
            Self::Broker(b) => b
                .admin_rpc("login", node, password, "")
                .await
                .and_then(|v| {
                    v.as_u64().map(|u| u as u8).ok_or_else(|| "bad login reply".to_string())
                }),
        }
    }

    async fn status(&self, node: &str) -> Result<serde_json::Value, String> {
        match self {
            Self::Radio(r) => r.remote_status(node).await,
            Self::Broker(b) => b.admin_rpc("status", node, "", "").await,
        }
    }

    async fn access_list(&self, node: &str) -> Result<serde_json::Value, String> {
        match self {
            Self::Radio(r) => r.remote_access_list(node).await,
            Self::Broker(b) => b.admin_rpc("acl", node, "", "").await,
        }
    }

    async fn cmd(&self, node: &str, cmd: &str) -> Result<String, String> {
        match self {
            Self::Radio(r) => r.remote_cmd(node, cmd).await,
            Self::Broker(b) => b
                .admin_rpc("cmd", node, "", cmd)
                .await
                .and_then(|v| {
                    v.as_str().map(|s| s.to_string()).ok_or_else(|| "bad reply".to_string())
                }),
        }
    }
}

/// Log in with the stored password (if any) before a privileged operation.
/// Returns Ok only if the node accepted us as admin.
async fn ensure_login(node: &serde_json::Value) -> Result<(), String> {
    let link = admin_link().await?;
    let pw = node.get("admin_password").and_then(|p| p.as_str()).unwrap_or("");
    if pw.is_empty() {
        return Err("not logged in".into());
    }
    let perms = link
        .login(node.get("node_id").and_then(|v| v.as_str()).unwrap_or(""), pw)
        .await?;
    if perms & 1 == 0 {
        return Err("node granted guest permissions (password may be the guest password)".into());
    }
    Ok(())
}

fn err_page(state: &SharedState, node_id: &str, error: &str) -> axum::response::Response {
    page(
        state,
        "meshcore_admin_error.html",
        json!({ "node_id": node_id, "error": error }),
    )
}

// ------------------------------------------------------------------ login

async fn admin_entry(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    Query(q): Query<AdminFlags>,
) -> axum::response::Response {
    let Some(node) = load_contact(&state, &node_id).await else {
        return Html("<h1>404 Not Found</h1>".to_string()).into_response();
    };
    if node.get("admin_password").and_then(|p| p.as_str()).unwrap_or("").is_empty() {
        return page(
            &state,
            "meshcore_admin_login.html",
            json!({
                "node": node,
                "err": q.err.unwrap_or_default(),
                "radio_connected": crate::meshcore::transport_connected(),
            }),
        );
    }
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin/console")).into_response()
}

async fn admin_login(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    RawForm(raw): RawForm,
) -> axum::response::Response {
    let Some(_node) = load_contact(&state, &node_id).await else {
        return Html("<h1>404 Not Found</h1>".to_string()).into_response();
    };
    let body = std::str::from_utf8(&raw).unwrap_or("");
    let password = form_value(body, "password");
    if password.is_empty() {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin?err=empty")).into_response();
    }
    let link = match admin_link().await {
        Ok(r) => r,
        Err(e) => return err_page(&state, &node_id, &e),
    };
    let perms = match link.login(&node_id, &password).await {
        Ok(p) => p,
        Err(e) => {
            let code = if e.contains("unreachable") { "unreachable" } else { "login" };
            return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin?err={code}")).into_response();
        }
    };
    if perms & 1 == 0 {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin?err=guest")).into_response();
    }
    let _ = sqlx::query("UPDATE mesh_directory SET admin_password = ?, admin_mode = 'admin' WHERE node_id = ?")
        .bind(&password)
        .bind(&node_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin/console")).into_response()
}

async fn admin_logout(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
) -> axum::response::Response {
    let _ = sqlx::query("UPDATE mesh_directory SET admin_password = '', admin_mode = 'guest' WHERE node_id = ?")
        .bind(&node_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin")).into_response()
}

// ------------------------------------------------------------------ status

async fn admin_status(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
) -> axum::response::Response {
    let Some(node) = load_contact(&state, &node_id).await else {
        return Html("<h1>404 Not Found</h1>".to_string()).into_response();
    };
    let link = match admin_link().await {
        Ok(r) => r,
        Err(e) => return err_page(&state, &node_id, &e),
    };
    if let Err(e) = ensure_login(&node).await {
        return err_page(&state, &node_id, &format!("login: {e}"));
    }
    let status = match link.status(&node_id).await {
        Ok(v) => v,
        Err(e) => return err_page(&state, &node_id, &format!("status: {e}")),
    };
    // Clock and firmware come from the CLI (not in the binary frame).
    let clock = link.cmd(&node_id, "clock").await.ok();
    let ver = link.cmd(&node_id, "ver").await.ok();
    page(
        &state,
        "meshcore_admin_status.html",
        json!({ "node": node, "status": status, "clock": clock, "ver": ver }),
    )
}

// ------------------------------------------------------------------ console

async fn admin_console(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    Query(q): Query<AdminFlags>,
) -> axum::response::Response {
    let Some(node) = load_contact(&state, &node_id).await else {
        return Html("<h1>404 Not Found</h1>".to_string()).into_response();
    };
    if node.get("admin_password").and_then(|p| p.as_str()).unwrap_or("").is_empty() {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin")).into_response();
    }
    page(
        &state,
        "meshcore_admin_console.html",
        json!({
            "node": node,
            "cmd": q.cmd.unwrap_or_default(),
            "out": q.out.unwrap_or_default(),
            "error": q.err.unwrap_or_default(),
        }),
    )
}

async fn admin_cmd(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    RawForm(raw): RawForm,
) -> axum::response::Response {
    let Some(node) = load_contact(&state, &node_id).await else {
        return Html("<h1>404 Not Found</h1>".to_string()).into_response();
    };
    let link = match admin_link().await {
        Ok(r) => r,
        Err(e) => return err_page(&state, &node_id, &e),
    };
    let body = std::str::from_utf8(&raw).unwrap_or("");
    let cmd = form_value(body, "cmd").trim().replace('\n', " ");
    if cmd.is_empty() {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin/console")).into_response();
    }
    if let Err(e) = ensure_login(&node).await {
        return err_page(&state, &node_id, &format!("login: {e}"));
    }
    // `get acl` is serial-only in the firmware CLI; the official app uses a
    // binary request instead, so intercept it here and use that path.
    if cmd == "get acl" {
        let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if let Ok(rows) = sqlx::query("SELECT node_id, name FROM mesh_directory")
            .fetch_all(&state.db)
            .await
        {
            for r in rows {
                if let (Ok(id), Ok(name)) =
                    (r.try_get::<String, _>("node_id"), r.try_get::<String, _>("name"))
                {
                    if id.len() >= 12 && !name.is_empty() {
                        names.insert(id[..12].to_lowercase(), name);
                    }
                }
            }
        }
        let (out, err) = match link.access_list(&node_id).await {
            Ok(v) => {
                let mut lines = String::from("access control list (admins):\n");
                if let Some(arr) = v.as_array() {
                    for e in arr {
                        let prefix = e.get("pubkey_prefix").and_then(|p| p.as_str()).unwrap_or("?").to_lowercase();
                        let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("");
                        let name = if name.is_empty() {
                            names.get(&prefix).map(|s| s.as_str()).unwrap_or("")
                        } else {
                            name
                        };
                        lines.push_str(&format!(
                            "  {:<14} {:<20} perms {} ({})\n",
                            prefix,
                            name,
                            e.get("perms").and_then(|p| p.as_i64()).unwrap_or(0),
                            e.get("role").and_then(|r| r.as_str()).unwrap_or("?"),
                        ));
                    }
                }
                (lines, String::new())
            }
            Err(e) => (String::new(), e),
        };
        let enc = |s: &str| urlencode(s);
        return Redirect::to(&format!(
            "/platforms/meshcore/contacts/{node_id}/admin/console?cmd={}&out={}&err={}",
            enc(&cmd),
            enc(&out),
            enc(&err)
        ))
        .into_response();
    }
    let (mut out, err) = match link.cmd(&node_id, &cmd).await {
        Ok(text) => (text.trim().to_string(), String::new()),
        Err(e) => (String::new(), e),
    };
    // The repeater's `neighbors` reply is "<8-hex pubkey prefix>:<secs since
    // heard>:<snr>" per line. Prefix each line with the contact name when the
    // prefix matches a known node, so humans don't have to match raw keys.
    if cmd.split_whitespace().next() == Some("neighbors") {
        out = annotate_neighbors(&state.db, &out).await;
    }
    let enc = |s: &str| urlencode(s);
    Redirect::to(&format!(
        "/platforms/meshcore/contacts/{node_id}/admin/console?cmd={}&out={}&err={}",
        enc(&cmd),
        enc(&out),
        enc(&err)
    ))
    .into_response()
}

/// Rewrite `neighbors` output lines, prefixing each with the contact's name
/// when the 8-hex key prefix matches a known mesh_directory node.
async fn annotate_neighbors(db: &sqlx::SqlitePool, out: &str) -> String {
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Ok(rows) = sqlx::query("SELECT node_id, name FROM mesh_directory")
        .fetch_all(db)
        .await
    {
        for r in rows {
            let id = r.try_get::<String, _>("node_id").unwrap_or_default();
            let name = r.try_get::<String, _>("name").unwrap_or_default();
            if id.len() >= 8 && !name.is_empty() {
                names.insert(id[..8].to_lowercase(), name);
            }
        }
    }
    out.lines()
        .map(|line| {
            let Some((key, rest)) = line.split_once(':') else {
                return line.to_string();
            };
            match names.get(&key.to_lowercase()) {
                Some(name) => format!("{name}  {key}:{rest}"),
                None => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b' ' => {
                if b == b' ' {
                    out.push('+');
                } else {
                    out.push(b as char);
                }
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ------------------------------------------------------------------ settings

async fn admin_settings(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    Query(q): Query<AdminFlags>,
) -> axum::response::Response {
    let Some(node) = load_contact(&state, &node_id).await else {
        return Html("<h1>404 Not Found</h1>".to_string()).into_response();
    };
    if node.get("admin_password").and_then(|p| p.as_str()).unwrap_or("").is_empty() {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}/admin")).into_response();
    }
    page(
        &state,
        "meshcore_admin_settings.html",
        json!({ "node": node, "saved": q.saved.unwrap_or_default(), "error": q.err.unwrap_or_default() }),
    )
}

async fn admin_settings_apply(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    RawForm(raw): RawForm,
) -> axum::response::Response {
    let Some(node) = load_contact(&state, &node_id).await else {
        return Html("<h1>404 Not Found</h1>".to_string()).into_response();
    };
    let link = match admin_link().await {
        Ok(r) => r,
        Err(e) => return err_page(&state, &node_id, &e),
    };
    if let Err(e) = ensure_login(&node).await {
        return err_page(&state, &node_id, &format!("login: {e}"));
    }
    let body = std::str::from_utf8(&raw).unwrap_or("");
    let val = |k: &str| form_value(body, k).trim().to_string();
    let mut results: Vec<(&str, String)> = Vec::new();
    let mut cmds: Vec<(&str, String)> = Vec::new();
    let name = val("name");
    if !name.is_empty() {
        cmds.push(("Name", format!("set name {name}")));
    }
    let freq = val("freq");
    let bw = val("bw");
    let sf = val("sf");
    let cr = val("cr");
    if !freq.is_empty() {
        if bw.is_empty() || sf.is_empty() || cr.is_empty() {
            results.push(("Radio", "set radio needs freq, bandwidth, SF and CR".into()));
        } else {
            cmds.push(("Radio", format!("set radio {freq} {bw} {sf} {cr}")));
        }
    }
    let tx = val("tx");
    if !tx.is_empty() {
        cmds.push(("TX power", format!("set tx {tx}")));
    }
    let owner = val("owner_info");
    if !owner.is_empty() {
        cmds.push(("Owner info", format!("set owner.info {owner}")));
    }
    let advert = val("advert_interval");
    if !advert.is_empty() {
        cmds.push(("Advert interval", format!("set advert.interval {advert}")));
    }
    let fadverts = val("flood_advert_interval");
    if !fadverts.is_empty() {
        cmds.push(("Flood advert interval", format!("set flood.advert.interval {fadverts}")));
    }
    let lat = val("lat");
    let lon = val("lon");
    // Manual location: firmware CLI is `set lat <f>` / `set lon <f>`
    // (`gps setloc` only copies a live GPS fix, and nodes without a GPS
    // have no fix to copy).
    if !lat.is_empty() {
        cmds.push(("Latitude", format!("set lat {lat}")));
    }
    if !lon.is_empty() {
        cmds.push(("Longitude", format!("set lon {lon}")));
    }
    match val("gps") {
        s if s == "on" => cmds.push(("GPS", "gps on".into())),
        s if s == "off" => cmds.push(("GPS", "gps off".into())),
        _ => {}
    }
    let gps_advert = val("gps_advert");
    if !gps_advert.is_empty() {
        cmds.push(("GPS advert", format!("gps advert {gps_advert}")));
    }
    if val("sync_clock") == "1" {
        // `clock sync` sets the target's clock from the SENDING node's RTC,
        // which fails with "clock cannot go backwards" if the radio is slow.
        // Sync clock: handled specially in the send loop below.
        cmds.push(("Sync clock", "\u{0}time".to_string()));
    }
    let admin_pw = val("admin_password");
    if !admin_pw.is_empty() {
        cmds.push(("Admin password", format!("password {admin_pw}")));
    }
    let guest_pw = val("guest_password");
    if !guest_pw.is_empty() {
        cmds.push(("Guest password", format!("set guest.password {guest_pw}")));
    }
    let region = val("region");
    if !region.is_empty() {
        cmds.push(("Region", format!("region {region}")));
    }
    let neighbor = val("neighbor_remove");
    if !neighbor.is_empty() {
        cmds.push(("Remove neighbor", format!("neighbor.remove {neighbor}")));
    }

    for (i, (label, cmd)) in cmds.iter().enumerate() {
        // Space out commands: back-to-back CLI frames collide on air and
        // the node is busy writing prefs after each `set`.
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        // Sync clock: substitute a fresh epoch right before each send (RF
        // takes 2-15s and retries add more). Our room-server firmware accepts
        // backward sets for the explicit `time` command.
        let send_cmd;
        let cmd = if cmd == "\u{0}time" {
            send_cmd = format!("time {}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
            &send_cmd
        } else {
            cmd
        };
        let mut outcome = match link.cmd(&node_id, cmd).await {
            Ok(text) => text.trim().to_string(),
            Err(e) => e,
        };
        if outcome.starts_with("no reply") {
            // One retry after the air settles.
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            outcome = match link.cmd(&node_id, cmd).await {
                Ok(text) => text.trim().to_string(),
                Err(e) => e,
            };
        }
        results.push((label, outcome));
    }
    // Keep the stored password in sync if it was rotated.
    if !admin_pw.is_empty() {
        let _ = sqlx::query("UPDATE mesh_directory SET admin_password = ? WHERE node_id = ?")
            .bind(&admin_pw)
            .bind(&node_id)
            .execute(&state.db)
            .await;
    }
    let saved = results
        .iter()
        .map(|(l, r)| format!("{l}: {r}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let enc = urlencode(&saved);
    Redirect::to(&format!(
        "/platforms/meshcore/contacts/{node_id}/admin/settings?saved={enc}"
    ))
    .into_response()
}
