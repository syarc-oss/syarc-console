//! MeshCore app: a phone-app-like view over the direct radio link.
//!
//! Hub -> Contacts (directory, capped), Channels (followed, add/remove),
//! the cascadiamesh.org map, and per-channel traffic views with a send
//! box — mirroring how the MeshCore companion app presents the mesh.

use axum::extract::{Path, Query, RawForm, State};
use axum::response::{Html, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use sqlx::Row;
use base64::Engine as _;

use syarc_plugin_api::{RequireAdmin, RequireUser, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/platforms/meshcore", get(hub))
        .route("/platforms/meshcore/", get(hub))
        .route("/platforms/meshcore/contacts", get(contacts))
        .route("/platforms/meshcore/contacts/add", post(contact_add))
        .route("/platforms/meshcore/contacts/{node_id}", get(contact_view))
        .route("/platforms/meshcore/contacts/{node_id}/favorite", post(contact_favorite))
        .route("/platforms/meshcore/contacts/{node_id}/pin", post(contact_pin))
        .route("/platforms/meshcore/contacts/{node_id}/traffic", post(contact_traffic))
        .route("/platforms/meshcore/contacts/{node_id}/send", post(contact_send))
        .route("/platforms/meshcore/contacts/{node_id}/roomlogin", post(contact_room_login))
        .route("/platforms/meshcore/channels", get(channels))
        .route("/platforms/meshcore/channels/", get(channels))
        .route("/platforms/meshcore/channels/{idx}", get(channel_view))
        .route("/platforms/meshcore/channels/{idx}/send", post(channel_send))
        .route("/platforms/meshcore/channels/{idx}/msg/{msg_id}/delete", post(channel_msg_delete))
        .route("/platforms/meshcore/channels/{idx}/follow", post(channel_follow))
        .route("/platforms/meshcore/channels/{idx}/unfollow", post(channel_unfollow))
        .route("/platforms/meshcore/channels/add", post(channel_add))
        .route("/platforms/meshcore/settings", get(settings_page))
        .route("/platforms/meshcore/settings/", get(settings_page))
        .route("/platforms/meshcore/settings/save", post(settings_save))
        .route("/platforms/meshcore/settings/advert", post(settings_advert))
        .route("/platforms/meshcore/settings/reboot", post(settings_reboot))
}

fn page(state: &SharedState, template: &str, ctx: serde_json::Value) -> Html<String> {
    Html(state.templates.render(template, ctx))
}

fn radio_channel_name(idx: u8) -> String {
    crate::radio::radio()
        .map(|r| r.channel_name(idx))
        .unwrap_or_else(|| format!("chan{idx}"))
}

pub(crate) fn transport_connected() -> bool {
    if let Some(r) = crate::radio::radio() {
        if r.connected() {
            return true;
        }
    }
    crate::broker::broker().map(|b| b.connected()).unwrap_or(false)
}

// ------------------------------------------------------------------ hub

async fn hub(_user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let contact_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mesh_directory")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let followed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mesh_followed_channels")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let settings = crate::settings::MeshSettings::load(&state.db).await;
    // The direct MeshCore radio is the primary transport; the mesh broker
    // is only a fallback for consoles without an attached radio.
    let (transport, connected, detail) = match crate::radio::radio() {
        Some(r) => ("radio", r.connected(), r.detail()),
        None => (
            "broker",
            crate::broker::broker().map(|b| b.connected()).unwrap_or(false),
            if settings.broker_enabled { "radio disabled; using mesh broker".into() } else { "radio disabled".into() },
        ),
    };
    page(
        &state,
        "meshcore_hub.html",
        json!({
            "radio_connected": connected,
            "transport": transport,
            "transport_detail": detail,
            "contact_count": contact_count,
            "followed_count": followed,
        }),
    )
}

// ------------------------------------------------------------------ contacts

async fn contacts(
    _user: RequireUser,
    State(state): State<SharedState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Html<String> {
    let rows = sqlx::query(
        "SELECT node_id, name, node_type, gps_lat, gps_lon, is_favorite, last_synced_at \
         FROM mesh_directory ORDER BY is_favorite DESC, name COLLATE NOCASE LIMIT 400",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let contacts: Vec<_> = rows
        .into_iter()
        .map(|r| {
            json!({
                "node_id": r.try_get::<String, _>("node_id").unwrap_or_default(),
                "name": r.try_get::<String, _>("name").unwrap_or_default(),
                "node_type": r.try_get::<i64, _>("node_type").unwrap_or(1),
                "lat": r.try_get::<Option<f64>, _>("gps_lat").ok().flatten(),
                "lon": r.try_get::<Option<f64>, _>("gps_lon").ok().flatten(),
                "last_synced": r.try_get::<String, _>("last_synced_at").unwrap_or_default(),
                "is_favorite": r.try_get::<i64, _>("is_favorite").unwrap_or(0) == 1,
            })
        })
        .collect();
    page(
        &state,
        "meshcore_contacts.html",
        json!({ "contacts": contacts, "added": q.get("added"), "add_error": q.get("err") }),
    )
}

/// Manually add a contact by full public key (e.g. a room server whose
/// advert the radio never sees, but whose key you know from the device).
async fn contact_add(
    _user: RequireUser,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Redirect {
    let body = std::str::from_utf8(&raw).unwrap_or("");
    let name = form_value(body, "name").trim().to_string();
    let key = form_value(body, "pubkey")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    let node_type: i64 = form_value(body, "node_type").parse().unwrap_or(3);
    let err = if name.is_empty() || name.len() > 40 {
        Some("name must be 1-40 characters")
    } else if key.len() != 64 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        Some("public key must be the full 64 hex characters (not the 8-char prefix)")
    } else if !(1..=4).contains(&node_type) {
        Some("invalid node type")
    } else {
        None
    };
    if let Some(e) = err {
        return Redirect::to(&format!(
            "/platforms/meshcore/contacts?err={}",
            crate::meshcore::meshcore_urlencode(e)
        ));
    }
    let exists: Option<String> = sqlx::query_scalar("SELECT name FROM mesh_directory WHERE node_id = ?")
        .bind(&key)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    if exists.is_some() {
        return Redirect::to("/platforms/meshcore/contacts?err=already+in+directory");
    }
    let _ = sqlx::query(
        "INSERT INTO mesh_directory (id, node_id, node_type, name, is_favorite, is_manual) VALUES (?, ?, ?, ?, 1, 1)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&key)
    .bind(node_type)
    .bind(&name)
    .execute(&state.db)
    .await;
    // Pin on the radio so the contact survives store churn (the firmware
    // evicts non-favourite contacts when its table is full).
    if let Some(r) = crate::radio::radio() {
        r.pin_contact(&key, &name, node_type as u8).await;
    }
    Redirect::to(&format!(
        "/platforms/meshcore/contacts?added={}",
        crate::meshcore::meshcore_urlencode(&name)
    ))
}

async fn contact_pin(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
) -> Redirect {
    let row = sqlx::query("SELECT name, node_type FROM mesh_directory WHERE node_id = ?")
        .bind(&node_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return Redirect::to("/platforms/meshcore/contacts");
    };
    let name: String = row.try_get("name").unwrap_or_default();
    let node_type: i64 = row.try_get("node_type").unwrap_or(1);
    let Some(r) = crate::radio::radio() else {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?pin=false"));
    };
    let mut ok = r.pin_contact(&node_id, &name, node_type.clamp(0, 255) as u8).await;
    if !ok {
        // The radio's contact table is full. Free slots by dropping the
        // stalest mirrored contacts (they churn back via adverts if still
        // active), enable overwrite-oldest so future churn self-manages,
        // then retry the pin.
        let stale: Vec<String> = sqlx::query_scalar(
            "SELECT node_id FROM mesh_directory \
             WHERE is_manual = 0 AND is_favorite = 0 \
             ORDER BY last_synced_at ASC LIMIT 20",
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        for key in &stale {
            if r.remove_contact(key).await {
                let _ = sqlx::query("DELETE FROM mesh_directory WHERE node_id = ?")
                    .bind(key)
                    .execute(&state.db)
                    .await;
            }
        }
        if let Some((config, max_hops)) = r.get_autoadd_config().await {
            let _ = r.set_autoadd_config(config | 0x01, max_hops).await;
        }
        ok = r.pin_contact(&node_id, &name, node_type.clamp(0, 255) as u8).await;
    }
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?pin={ok}"))
}

#[derive(serde::Deserialize)]
struct ContactFlags {
    sent: Option<String>,
    send_failed: Option<String>,
    pin: Option<String>,
    room_login: Option<String>,
}

async fn contact_view(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    Query(q): Query<ContactFlags>,
) -> Html<String> {
    let c = sqlx::query(
        "SELECT node_id, name, node_type, gps_lat, gps_lon, is_favorite, show_traffic, last_heard, last_synced_at, admin_password \
         FROM mesh_directory WHERE node_id = ?",
    )
    .bind(&node_id)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);
    let Some(c) = c else {
        return Html("<h1>404 Not Found</h1>".to_string());
    };
    let lat: Option<f64> = c.try_get("gps_lat").ok().flatten();
    let lon: Option<f64> = c.try_get("gps_lon").ok().flatten();
    let traffic = sqlx::query(
        "SELECT direction, text, created_at FROM mesh_traffic WHERE node_id = ? \
         ORDER BY created_at DESC, id DESC LIMIT 20",
    )
    .bind(&node_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        json!({
            "direction": r.try_get::<String, _>("direction").unwrap_or_default(),
            "text": r.try_get::<String, _>("text").unwrap_or_default(),
            "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
        })
    })
    .collect::<Vec<_>>();
    page(
        &state,
        "meshcore_contact_detail.html",
        json!({
            "contact": {
                "node_id": c.try_get::<String, _>("node_id").unwrap_or_default(),
                "name": c.try_get::<String, _>("name").unwrap_or_default(),
                "node_type": c.try_get::<i64, _>("node_type").unwrap_or(1),
                "lat": lat,
                "lon": lon,
                "is_favorite": c.try_get::<i64, _>("is_favorite").unwrap_or(0) == 1,
                "show_traffic": c.try_get::<i64, _>("show_traffic").unwrap_or(1) == 1,
                "last_heard": c.try_get::<Option<String>, _>("last_heard").ok().flatten(),
                "last_synced": c.try_get::<String, _>("last_synced_at").unwrap_or_default(),
                "room_password_set": !c.try_get::<String, _>("admin_password").unwrap_or_default().is_empty(),
            },
            "traffic": traffic,
            "radio_connected": transport_connected(),
            "sent": q.sent.is_some(),
            "send_failed": q.send_failed.is_some(),
            "pin_ok": q.pin.as_deref() == Some("true"),
            "pin_failed": q.pin.is_some() && q.pin.as_deref() != Some("true"),
            "room_login_ok": q.room_login.as_deref() == Some("ok"),
            "room_login_guest": q.room_login.as_deref() == Some("guest"),
            "room_login_failed": q.room_login.is_some() && q.room_login.as_deref() != Some("ok") && q.room_login.as_deref() != Some("guest"),
            "map_url": match (lat, lon) {
                (Some(la), Some(lo)) => Some(format!("https://cascadiamesh.org/map/#map={}/{}/12", lo, la)),
                _ => None,
            },
        }),
    )
}

async fn contact_room_login(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    RawForm(raw): RawForm,
) -> Redirect {
    let body = std::str::from_utf8(&raw).unwrap_or("");
    let password = form_value(body, "password");
    if password.is_empty() {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?room_login=empty"));
    }
    let name: String = sqlx::query_scalar("SELECT COALESCE(NULLIF(name,''), substr(node_id,1,12)) FROM mesh_directory WHERE node_id = ?")
        .bind(&node_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or_else(|_| node_id.chars().take(12).collect());
    let perms = match crate::remote_admin::room_server_login(&node_id, &password, &name).await {
        Ok(p) => p,
        Err(e) => {
            return Redirect::to(&format!(
                "/platforms/meshcore/contacts/{node_id}?room_login=failed&err={}",
                crate::meshcore::meshcore_urlencode(&e)
            ));
        }
    };
    if perms & 2 == 0 {
        // Admitted as read-only guest: the node will silently drop posts.
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?room_login=guest"));
    }
    let mode = if perms & 1 != 0 { "admin" } else { "guest" };
    let _ = sqlx::query("UPDATE mesh_directory SET admin_password = ?, admin_mode = ? WHERE node_id = ?")
        .bind(&password)
        .bind(mode)
        .bind(&node_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?room_login=ok"))
}

async fn contact_favorite(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
) -> Redirect {
    let _ = sqlx::query("UPDATE mesh_directory SET is_favorite = 1 - is_favorite WHERE node_id = ?")
        .bind(&node_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}"))
}

async fn contact_traffic(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
) -> Redirect {
    let _ = sqlx::query("UPDATE mesh_directory SET show_traffic = 1 - show_traffic WHERE node_id = ?")
        .bind(&node_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}"))
}

async fn contact_send(
    user: RequireUser,
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
    RawForm(raw): RawForm,
) -> Redirect {
    let body = std::str::from_utf8(&raw).unwrap_or("");
    let text = form_value(body, "text").trim().to_string();
    if text.is_empty() {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?send_failed=1"));
    }
    let row = sqlx::query("SELECT name, node_type, admin_password FROM mesh_directory WHERE node_id = ?")
        .bind(&node_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let (name, node_type, room_pw) = row
        .map(|r| {
            (
                r.try_get::<String, _>("name").unwrap_or_default(),
                r.try_get::<i64, _>("node_type").unwrap_or(1),
                r.try_get::<String, _>("admin_password").unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    // Room servers admit guests read-only and then silently ignore their
    // posts: log in with the stored room/admin password first so the post
    // is accepted.
    if node_type == 3 && !room_pw.is_empty() {
        let _ = crate::remote_admin::room_server_login(&node_id, &room_pw, &name).await;
    }
    let Some(b) = crate::broker::broker() else {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?send_failed=1"));
    };
    if !b.send_dm(&node_id, &text).await {
        return Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?send_failed=1"));
    }
    // Room servers: the DM text becomes a room post, and inbound room
    // traffic is threaded into the shared room conversation — use the same
    // one so the reply appears in the same thread. Other node types get a
    // per-user DM conversation (mirroring inbound mesh DM threading).
    let conv = if node_type == 3 {
        b.open_room_conversation(&node_id, &name).await
    } else {
        b.open_dm_conversation(&node_id, &name, user.0.id).await
    };
    if let Ok(conv_id) = conv {
        let msg_id = crate::messaging::new_id();
        let ts = crate::messaging::now();
        let _ = sqlx::query(
            "INSERT INTO messages (id, conversation_id, sender_id, content, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&msg_id).bind(&conv_id).bind(user.0.id).bind(&text).bind(&ts)
        .execute(&state.db)
        .await;
        let _ = sqlx::query("INSERT INTO message_read_statuses (id, message_id, user_id) VALUES (?, ?, ?)")
            .bind(crate::messaging::new_id()).bind(&msg_id).bind(user.0.id)
            .execute(&state.db)
            .await;
        let _ = sqlx::query("UPDATE conversations SET updated_at = ? WHERE id = ?")
            .bind(&ts).bind(&conv_id)
            .execute(&state.db)
            .await;
        return Redirect::to(&format!("/platforms/messaging/conversations/{conv_id}"));
    }
    Redirect::to(&format!("/platforms/meshcore/contacts/{node_id}?sent=1"))
}

// ------------------------------------------------------------------ channels

async fn channels(
    user: RequireUser,
    State(state): State<SharedState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Html<String> {
    // Channel 0 (Public) is always available: seed it on first visit.
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO mesh_followed_channels (idx, name) VALUES (0, 'Public')",
    )
    .execute(&state.db)
    .await;

    // The attached radio is the source of truth for slot names (slots can
    // be renamed from the phone app). Sync stored names so a renamed slot
    // never shows a stale name.
    if let Some(r) = crate::radio::radio() {
        for (idx, name) in r.channels() {
            if name.is_empty() {
                continue;
            }
            let _ = sqlx::query(
                "INSERT INTO mesh_followed_channels (idx, name) VALUES (?, ?) \
                 ON CONFLICT(idx) DO UPDATE SET name = excluded.name",
            )
            .bind(idx as i64)
            .bind(&name)
            .execute(&state.db)
            .await;
        }
    }

    // Followed channels with last activity.
    let followed = sqlx::query(
        "SELECT f.idx, f.name, \
                (SELECT MAX(created_at) FROM mesh_traffic WHERE node_id = '#chan' || f.idx) AS last_activity \
         FROM mesh_followed_channels f ORDER BY f.idx",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        let idx: i64 = r.try_get("idx").unwrap_or(0);
        let stored: String = r.try_get("name").unwrap_or_default();
        json!({
            "idx": idx,
            "name": if stored.is_empty() { radio_channel_name(idx as u8) } else { stored },
            "last_activity": r.try_get::<Option<String>, _>("last_activity").ok().flatten(),
        })
    })
    .collect::<Vec<_>>();

    // All channels the radio knows about (for adding).
    let mut available: Vec<serde_json::Value> = Vec::new();
    let followed_idx: std::collections::HashSet<i64> = followed
        .iter()
        .filter_map(|f| f.get("idx").and_then(|i| i.as_i64()))
        .collect();
    if let Some(r) = crate::radio::radio() {
        for (idx, name) in r.channels() {
            if !followed_idx.contains(&(idx as i64)) {
                available.push(json!({ "idx": idx, "name": name }));
            }
        }
    }
    if available.is_empty() {
        // Fall back to any channels seen in traffic but not followed.
        let rows = sqlx::query(
            "SELECT DISTINCT node_id FROM mesh_traffic WHERE node_id LIKE '#chan%'",
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        for r in rows {
            let node_id: String = r.try_get("node_id").unwrap_or_default();
            if let Some(n) = node_id
                .strip_prefix("#chan")
                .and_then(|n| n.parse::<i64>().ok())
            {
                if !followed_idx.contains(&n) && !available.iter().any(|a| a["idx"] == json!(n)) {
                    available.push(json!({ "idx": n, "name": radio_channel_name(n as u8) }));
                }
            }
        }
    }

    page(
        &state,
        "meshcore_channels.html",
        json!({
            "followed": followed,
            "available": available,
            "radio_connected": transport_connected(),
            "is_admin": user.0.role == "admin",
            "msg": q.get("msg").cloned().unwrap_or_default(),
            "err": q.get("err").cloned().unwrap_or_default(),
        }),
    )
}

/// Admin: create/update a channel on the attached companion radio via
/// CMD_SET_CHANNEL. Accepts PSK as 32 hex chars or base64 (16 bytes).
/// Follows the new channel automatically.
async fn channel_add(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    RawForm(raw): RawForm,
) -> Redirect {
    let body = std::str::from_utf8(&raw).unwrap_or("");
    let name = form_value(body, "name").trim().to_string();
    let psk = form_value(body, "psk").trim().to_string();
    let idx_raw = form_value(body, "idx").trim().to_string();
    let fail = |e: &str| {
        Redirect::to(&format!(
            "/platforms/meshcore/channels?err={}",
            meshcore_urlencode(e)
        ))
    };
    if name.is_empty() || psk.is_empty() {
        return fail("Name and PSK are required");
    }
    // PSK: 32 hex chars, or base64 decoding to exactly 16 bytes.
    let secret: [u8; 16] = if psk.len() == 32 && psk.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut out = [0u8; 16];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&psk[i * 2..i * 2 + 2], 16).unwrap_or(0);
        }
        out
    } else {
        match base64::engine::general_purpose::STANDARD.decode(&psk) {
            Ok(v) if v.len() == 16 => {
                let mut out = [0u8; 16];
                out.copy_from_slice(&v);
                out
            }
            _ => return fail("PSK must be 32 hex characters or base64 encoding 16 bytes"),
        }
    };
    let Some(r) = crate::radio::radio() else {
        return fail("No radio attached to this console");
    };
    let idx: u8 = if idx_raw.is_empty() {
        // Lowest free slot (skip 0 = Public).
        let used: std::collections::HashSet<u8> = r.channels().into_iter().map(|(i, _)| i).collect();
        match (1..8u8).find(|i| !used.contains(i)) {
            Some(i) => i,
            None => return fail("No free channel slot (0-7 all in use)"),
        }
    } else {
        match idx_raw.parse::<u8>() {
            Ok(i) if i > 0 && i < 8 => i,
            _ => return fail("Slot must be 1-7"),
        }
    };
    match r.set_channel(idx, &name, secret).await {
        Ok(()) => {
            let _ = sqlx::query(
                "INSERT INTO mesh_followed_channels (idx, name) VALUES (?, ?) \
                 ON CONFLICT(idx) DO UPDATE SET name = excluded.name",
            )
            .bind(idx)
            .bind(&name)
            .execute(&state.db)
            .await;
            Redirect::to(&format!(
                "/platforms/meshcore/channels?msg={}",
                meshcore_urlencode(&format!("Channel '{name}' saved to slot {idx} and followed"))
            ))
        }
        Err(e) => fail(&format!("Radio refused: {e}")),
    }
}

async fn channel_follow(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(idx): Path<i64>,
    body: axum::body::Bytes,
) -> Redirect {
    // Optional custom name (radio-less consoles have no name source).
    let b = std::str::from_utf8(&body).unwrap_or("");
    let custom = form_value(b, "name").trim().to_string();
    let name = if custom.is_empty() {
        radio_channel_name(idx.clamp(0, 255) as u8)
    } else {
        custom
    };
    let _ = sqlx::query(
        "INSERT INTO mesh_followed_channels (idx, name) VALUES (?, ?) \
         ON CONFLICT(idx) DO UPDATE SET name = excluded.name",
    )
    .bind(idx)
    .bind(&name)
    .execute(&state.db)
    .await;
    Redirect::to("/platforms/meshcore/channels")
}

async fn channel_unfollow(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(idx): Path<i64>,
) -> Redirect {
    let _ = sqlx::query("DELETE FROM mesh_followed_channels WHERE idx = ?")
        .bind(idx)
        .execute(&state.db)
        .await;
    Redirect::to("/platforms/meshcore/channels")
}

// ------------------------------------------------------------------ channel view

/// Strip up to 3 leading "<name>: " sender labels (added by mesh hops and
/// the den forwarders) so re-deliveries compare equal.
fn strip_sender_prefixes(mut t: &str) -> &str {
    for _ in 0..3 {
        let Some(colon) = t.find(": ") else { break };
        let name = &t[..colon];
        if name.is_empty()
            || name.len() > 24
            || !name.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '_' || c == '-')
        {
            break;
        }
        t = &t[colon + 2..];
    }
    t
}

/// True when two "YYYY-MM-DD HH:MM:SS" stamps are within `secs` of each other.
fn same_window(a: &str, b: &str, secs: i64) -> bool {
    let parse = |s: &str| -> Option<i64> {
        let mut it = s.split_whitespace();
        let d = it.next()?;
        let t = it.next()?;
        let mut dp = d.split('-');
        let mut tp = t.split(':');
        let (y, mo, da) = (dp.next()?.parse::<i64>().ok()?, dp.next()?.parse::<i64>().ok()?, dp.next()?.parse::<i64>().ok()?);
        let (h, mi, s) = (tp.next()?.parse::<i64>().ok()?, tp.next()?.parse::<i64>().ok()?, tp.next()?.parse::<i64>().ok()?);
        // days since epoch (civil algorithm)
        let (y2, m2) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
        let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
        let yoe = y2 - era * 400;
        let doy = (153 * (m2 - 3) + 2) / 5 + da - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Some(era * 146097 + doe - 719468 + h * 3600 + mi * 60 + s)
    };
    match (parse(a), parse(b)) {
        (Some(x), Some(y)) => (x - y).abs() <= secs,
        _ => false,
    }
}

async fn channel_view(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path(idx): Path<i64>,
) -> Html<String> {
    let node_id = format!("#chan{idx}");
    let name = {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT name FROM mesh_followed_channels WHERE idx = ?",
        )
        .bind(idx)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        match stored.filter(|s| !s.is_empty()) {
            Some(n) => n,
            None => radio_channel_name(idx.clamp(0, 255) as u8),
        }
    };
    let entries = sqlx::query(
        "SELECT id, direction, node_id, node_name, text, created_at \
         FROM mesh_traffic WHERE node_id = ? ORDER BY id DESC LIMIT 300",
    )
    .bind(&node_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        json!({
            "media": false,
            "id": r.try_get::<i64, _>("id").unwrap_or(0),
            "direction": r.try_get::<String, _>("direction").unwrap_or_default(),
            "text": r.try_get::<String, _>("text").unwrap_or_default(),
            "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
        })
    })
    .collect::<Vec<_>>();
    // Collapse re-deliveries of the same text: RF floods and the den-to-den
    // forwarders re-send identical texts (each hop prepending "<node>: "),
    // which used to spam the timeline as a bounce. Compare with leading
    // sender prefixes stripped and drop repeats seen within 30 minutes.
    let mut entries: Vec<serde_json::Value> = entries;
    {
        let mut seen: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        entries.retain(|e| {
            let text = e["text"].as_str().unwrap_or("");
            let at = e["created_at"].as_str().unwrap_or("");
            let key = strip_sender_prefixes(text).to_string();
            match seen.get(&key) {
                Some(prev_at) if same_window(prev_at, at, 1800) => false,
                _ => {
                    seen.insert(key, at.to_string());
                    true
                }
            }
        });
    }
    let followed = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM mesh_followed_channels WHERE idx = ?",
    )
    .bind(idx)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0)
        > 0;
    // Voice/photo clips captured on this channel ride the same channel as
    // text but live in mesh_media - merge them into the timeline so each
    // clip is a message line with Play/Download/Delete on it.
    let mut timeline: Vec<serde_json::Value> = sqlx::query(
        "SELECT id, sid, kind, mode, size_bytes, dur, caption, created_at \
         FROM mesh_media WHERE (chan_idx = ? OR chan_idx2 = ?) AND status = 'complete' \
         ORDER BY id DESC LIMIT 50",
    )
    .bind(idx)
    .bind(idx)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        json!({
            "media": true,
            "id": r.try_get::<i64, _>("id").unwrap_or(0),
            "sid": r.try_get::<String, _>("sid").unwrap_or_default(),
            "kind": r.try_get::<String, _>("kind").unwrap_or_default(),
            "mode": r.try_get::<i64, _>("mode").unwrap_or(0),
            "size_bytes": r.try_get::<Option<i64>, _>("size_bytes").ok().flatten(),
            "dur": r.try_get::<i64, _>("dur").unwrap_or(0),
            "caption": r.try_get::<String, _>("caption").unwrap_or_default(),
            "created_at": r.try_get::<String, _>("created_at").unwrap_or_default(),
        })
    })
    .chain(entries)
    .collect();
    timeline.sort_by(|a, b| {
        b.get("created_at").and_then(|v| v.as_str()).unwrap_or("")
            .cmp(a.get("created_at").and_then(|v| v.as_str()).unwrap_or(""))
    });
    timeline.truncate(300);
    page(
        &state,
        "meshcore_channel.html",
        json!({ "idx": idx, "name": name, "node_id": node_id, "entries": timeline, "followed": followed }),
    )
}

/// Send straight to the channel (like the phone app's chat box). The
/// message is logged as outbound traffic so it appears in the view.
async fn channel_send(
    user: RequireUser,
    _state: State<SharedState>,
    Path(idx): Path<i64>,
    RawForm(raw): RawForm,
) -> Redirect {
    let text: String = form_value(std::str::from_utf8(&raw).unwrap_or(""), "text")
        .trim()
        .to_string();
    if !text.is_empty() {
        let out = crate::broker::format_outbound(&user.0.username, &text);
        let sent = match crate::broker::broker() {
            Some(b) => b.send_chan(idx, &out).await,
            None => false,
        };
        let node_id = format!("#chan{idx}");
        let name = radio_channel_name(idx.clamp(0, 255) as u8);
        crate::broker::log_traffic_pub("meshcore", if sent { "out" } else { "out-failed" }, &node_id, &name, &out)
            .await;
    }
    Redirect::to(&format!("/platforms/meshcore/channels/{idx}"))
}

async fn channel_msg_delete(
    _user: RequireUser,
    State(state): State<SharedState>,
    Path((idx, msg_id)): Path<(i64, i64)>,
) -> Redirect {
    let node_id = format!("#chan{idx}");
    let _ = sqlx::query("DELETE FROM mesh_traffic WHERE id = ? AND node_id = ?")
        .bind(msg_id)
        .bind(&node_id)
        .execute(&state.db)
        .await;
    Redirect::to(&format!("/platforms/meshcore/channels/{idx}"))
}

pub(crate) fn meshcore_urlencode(s: &str) -> String {
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

pub(crate) fn form_value(urlencoded_body: &str, key: &str) -> String {
    form_urlencoded::parse(urlencoded_body.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}


// ------------------------------------------------------------------ settings

async fn settings_page(_user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let mut info = serde_json::json!({});
    let mut board = serde_json::json!({});
    if let Some(r) = crate::radio::radio() {
        if let Some(v) = r.self_info() {
            info = v;
        }
        if let Ok(v) = r.device_query().await {
            board = v;
        }
    }
    page(
        &state,
        "meshcore_settings.html",
        json!({
            "radio_connected": transport_connected(),
            "radio_detail": crate::radio::radio().map(|r| r.detail()).unwrap_or_default(),
            "info": info,
            "board": board,
        }),
    )
}

async fn settings_save(
    _user: RequireUser,
    _state: State<SharedState>,
    RawForm(raw): RawForm,
) -> Redirect {
    let body = std::str::from_utf8(&raw).unwrap_or("");
    if let Some(r) = crate::radio::radio() {
        let name = form_value(body, "name").trim().to_string();
        if !name.is_empty() {
            r.set_name(&name).await;
        }
        if let (Some(lat), Some(lon)) = (
            form_value(body, "lat").trim().parse::<f64>().ok(),
            form_value(body, "lon").trim().parse::<f64>().ok(),
        ) {
            r.set_coords(lat, lon).await;
        }
        if let Ok(tx) = form_value(body, "tx_power").trim().parse::<i64>() {
            r.set_tx_power(tx).await;
        }
        let share = form_value(body, "share_location") == "1";
        let _ = r.set_loc_policy(share).await;
    }
    Redirect::to("/platforms/meshcore/settings?saved=1")
}

async fn settings_advert(_user: RequireUser) -> Redirect {
    if let Some(r) = crate::radio::radio() {
        r.send_advert(true).await;
    }
    Redirect::to("/platforms/meshcore/settings?advert=1")
}

async fn settings_reboot(_user: RequireUser) -> Redirect {
    if let Some(r) = crate::radio::radio() {
        r.reboot().await;
    }
    Redirect::to("/platforms/meshcore/settings?reboot=1")
}
