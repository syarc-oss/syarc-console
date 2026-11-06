//! In-process WebSocket connection hub, keyed by console user id.
//! The console runs as a single process, so unlike the fastmain deployment
//! there is no cross-worker broker: this hub is authoritative.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::extract::ws::Message;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::now_str;

/// Grace period after a user's last WS drops before their RINGING direct
/// calls are cleaned up. Long enough for mobile background/foreground
/// transitions, short enough to catch real disconnects.
pub const DISCONNECT_GRACE_SECONDS: u64 = 45;

struct Hub {
    conns: Mutex<HashMap<i64, Vec<UnboundedSender<Message>>>>,
    watchdogs: Mutex<HashMap<i64, JoinHandle<()>>>,
    db: OnceLock<syarc_plugin_api::Db>,
}

static HUB: OnceLock<Hub> = OnceLock::new();

fn hub() -> &'static Hub {
    HUB.get_or_init(|| Hub {
        conns: Mutex::new(HashMap::new()),
        watchdogs: Mutex::new(HashMap::new()),
        db: OnceLock::new(),
    })
}

/// Called at plugin startup so watchdogs can reach the database.
pub fn init(db: syarc_plugin_api::Db) {
    let _ = hub().db.set(db);
}

pub fn register(user_id: i64, tx: UnboundedSender<Message>) {
    disarm_watchdog(user_id);
    hub().conns.lock().unwrap().entry(user_id).or_default().push(tx);
}

/// Remove one connection. Returns true if this was the user's last connection.
pub fn unregister(user_id: i64, tx: &UnboundedSender<Message>) -> bool {
    let mut conns = hub().conns.lock().unwrap();
    if let Some(list) = conns.get_mut(&user_id) {
        list.retain(|t| !t.same_channel(tx));
        if list.is_empty() {
            conns.remove(&user_id);
            return true;
        }
    }
    false
}

pub fn is_online(user_id: i64) -> bool {
    hub().conns.lock().unwrap().contains_key(&user_id)
}

/// Online user ids as strings (the JS clients compare against dataset strings).
pub fn online_users() -> Vec<String> {
    hub().conns
        .lock()
        .unwrap()
        .keys()
        .map(|id| id.to_string())
        .collect()
}

/// Deliver a message to every open connection of a user.
/// Returns true if at least one connection received it.
pub fn send_to_user(user_id: i64, message: &Value) -> bool {
    let text = message.to_string();
    let conns = hub().conns.lock().unwrap().get(&user_id).cloned();
    let Some(list) = conns else { return false };
    let mut delivered = false;
    for tx in list {
        if tx.send(Message::Text(text.clone().into())).is_ok() {
            delivered = true;
        }
    }
    delivered
}

pub fn broadcast_presence(user_id: i64, online: bool) {
    let message = json!({"type": "presence", "user_id": user_id.to_string(), "online": online});
    let others: Vec<i64> = hub()
        .conns
        .lock()
        .unwrap()
        .keys()
        .copied()
        .filter(|id| *id != user_id)
        .collect();
    for id in others {
        send_to_user(id, &message);
    }
}

// --- Disconnect watchdog ----------------------------------------------------
// If a user's last WS drops, wait the grace period; if they do not come back,
// end their RINGING direct calls. ACTIVE calls are never touched: their media
// flows peer-to-peer and does not need the WebSocket.

pub fn arm_watchdog(user_id: i64) {
    let mut watchdogs = hub().watchdogs.lock().unwrap();
    if let Some(existing) = watchdogs.remove(&user_id) {
        existing.abort();
    }
    watchdogs.insert(
        user_id,
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(DISCONNECT_GRACE_SECONDS)).await;
            hub().watchdogs.lock().unwrap().remove(&user_id);
            if is_online(user_id) {
                return;
            }
            let Some(db) = hub().db.get().cloned() else { return };
            let now = now_str();
            let result = sqlx::query(
                "UPDATE conferencing_calls SET status = 'ended', ended_at = ? \
                 WHERE caller_id = ? AND status = 'ringing' AND call_type = 'direct'",
            )
            .bind(&now)
            .bind(user_id)
            .execute(&db)
            .await;
            match result {
                Ok(res) if res.rows_affected() > 0 => {
                    eprintln!(
                        "conferencing watchdog: ended ringing calls of disconnected user {user_id}"
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!("conferencing watchdog error: {e}"),
            }
        }),
    );
}

pub fn disarm_watchdog(user_id: i64) {
    if let Some(existing) = hub().watchdogs.lock().unwrap().remove(&user_id) {
        existing.abort();
    }
}
