//! Broker *server*: lets this console share its direct MeshCore radio
//! link with other consoles (e.g. production owns the radio, development
//! subscribes). Speaks the same WebSocket protocol as the Python
//! meshbroker, so the Rust broker client can connect to it unchanged.
//!
//! Semantics match the Python broker's subscription model:
//! - channel traffic (`#chanN`) is fanned out to every subscribed client;
//! - DMs are delivered only to clients subscribed to that node's address,
//!   which is how "source console" tracking works;
//! - every frame carries a unique id so each console's frame ledger
//!   dedups independently.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};

use axum::response::Response;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::broker;
use crate::settings::MeshSettings;

struct Client {
    tx: mpsc::UnboundedSender<Message>,
    subs: Mutex<HashSet<String>>,
    instance: Mutex<String>,
    names: Mutex<Vec<String>>,
}

struct ServeHub {
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    next_id: AtomicU64,
}

static HUB: std::sync::OnceLock<ServeHub> = std::sync::OnceLock::new();
static SERVING: AtomicBool = AtomicBool::new(false);

fn hub() -> &'static ServeHub {
    HUB.get_or_init(|| ServeHub {
        clients: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    })
}

/// Enable the broker server (call once at startup when the setting is on).
pub fn start_broker_server_if_enabled(settings: &MeshSettings) -> bool {
    let on = settings.broker_serve_enabled;
    SERVING.store(on, Ordering::Relaxed);
    if on {
        eprintln!(
            "mesh broker server enabled; other consoles may connect via ws://<this-host>:<port>/mesh/ws"
        );
    }
    on
}

pub fn serving() -> bool {
    SERVING.load(Ordering::Relaxed)
}

// ------------------------------------------------------------ fan-out entry points

/// Deliver an inbound mesh frame to subscribed clients.
pub fn broadcast_msg(frame: &Value) {
    if !serving() {
        return;
    }
    let Some(addr) = frame.get("from").and_then(|f| f.as_str()) else {
        return;
    };
    let text = frame.to_string();
    for client in hub().clients.lock().unwrap().values() {
        if client.subs.lock().unwrap().contains(addr) {
            let _ = client.tx.send(Message::Text(text.clone().into()));
        }
    }
}

/// Push the contact directory to every connected client.
pub fn broadcast_directory(contacts: &Value) {
    if !serving() {
        return;
    }
    let text = json!({ "type": "directory", "contacts": contacts }).to_string();
    for client in hub().clients.lock().unwrap().values() {
        let _ = client.tx.send(Message::Text(text.clone().into()));
    }
}

/// Relay a completed media clip to clients subscribed to its channel.
/// `media` carries sid/kind/mode/total/chan_idx/dur/caption plus base64
/// payload; radio-less consoles reassemble their own mesh_media entry.
pub fn broadcast_media(chan_idx: u8, media: Value) {
    if !serving() {
        return;
    }
    let addr = format!("#chan{chan_idx}");
    let text = json!({ "type": "media", "from": addr, "media": media }).to_string();
    for client in hub().clients.lock().unwrap().values() {
        if client.subs.lock().unwrap().contains(&addr) {
            let _ = client.tx.send(Message::Text(text.clone().into()));
        }
    }
}

/// Link status snapshot for welcome/status frames.
fn links_status() -> Value {
    let radio = crate::radio::radio().map(|r| {
        json!({
            "up": r.connected(),
            "detail": r.detail(),
            "host": r.host,
        })
    });
    let clients: Vec<Value> = hub()
        .clients
        .lock()
        .unwrap()
        .values()
        .map(|c| json!(c.instance.lock().unwrap().clone()))
        .collect();
    json!({
        "transport": "direct+serve",
        "radio": radio,
        "clients": clients,
    })
}

// ------------------------------------------------------------ websocket handler

pub async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    // Expect hello within 10s.
    let hello = match tokio::time::timeout(Duration::from_secs(10), socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).ok(),
        _ => None,
    };
    let Some(hello) = hello.filter(|h| h.get("type").and_then(|t| t.as_str()) == Some("hello")) else {
        let _ = socket.send(Message::Text(json!({"type":"error","reason":"expected hello"}).to_string().into())).await;
        return;
    };

    // Token check (shared with the broker client's token setting).
    if let Some(b) = broker::broker() {
        let token = b.settings.read().unwrap().broker_token.clone();
        if !token.is_empty() && hello.get("token").and_then(|t| t.as_str()) != Some(&token) {
            let _ = socket.send(Message::Text(json!({"type":"error","reason":"bad token"}).to_string().into())).await;
            return;
        }
    }

    let id = hub().next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let client = Arc::new(Client {
        tx,
        subs: Mutex::new(HashSet::new()),
        instance: Mutex::new(
            hello
                .get("instance")
                .and_then(|i| i.as_str())
                .unwrap_or("?")
                .to_string(),
        ),
        names: Mutex::new(Vec::new()),
    });
    hub().clients.lock().unwrap().insert(id, client.clone());

    let welcome = json!({
        "type": "welcome",
        "client_id": format!("s{id}"),
        "links": links_status(),
    });
    if socket
        .send(Message::Text(welcome.to_string().into()))
        .await
        .is_err()
    {
        hub().clients.lock().unwrap().remove(&id);
        return;
    }
    eprintln!(
        "mesh broker server: client {} connected ({})",
        id,
        hello.get("instance").and_then(|i| i.as_str()).unwrap_or("?")
    );

    // Pump: inbound client frames / outbound queued messages.
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Text(t))) => {
                        if handle_client_frame(id, &client, &t).await {
                            continue;
                        }
                        break; // close requested or parse failure on hello-level protocol
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            out = rx.recv() => {
                match out {
                    Some(msg) => { if socket.send(msg).await.is_err() { break; } }
                    None => break,
                }
            }
        }
    }
    hub().clients.lock().unwrap().remove(&id);
    eprintln!("mesh broker server: client {id} disconnected");
}

/// Returns false when the connection should be dropped.
async fn handle_client_frame(id: u64, client: &Arc<Client>, raw: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return true; // ignore junk, keep the link
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("subscribe") => {
            let mut subs = client.subs.lock().unwrap();
            subs.clear();
            if let Some(list) = v.get("subs").and_then(|s| s.as_array()) {
                for s in list {
                    if let Some(addr) = s.get("address").and_then(|a| a.as_str()) {
                        subs.insert(addr.to_string());
                    }
                }
            }
            eprintln!("mesh broker server: client {id} subscribed to {} addresses", subs.len());
            true
        }
        Some("register_names") => {
            *client.names.lock().unwrap() = v
                .get("names")
                .and_then(|n| n.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|n| n.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            true
        }
        Some("dm") => {
            let mid = v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
            let to = v.get("to").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let ok = forward_outbound(&to, &text).await;
            let _ = client.tx.send(
                Message::Text(json!({ "type": "ack", "id": mid, "ok": ok }).to_string().into()),
            );
            true
        }
        Some("chan") => {
            let mid = v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
            let chan = v.get("chan").and_then(|c| c.as_i64()).unwrap_or(-1);
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let ok = match crate::radio::radio() {
                Some(r) if r.connected() => r.send_chan(chan, &text).await,
                _ => false,
            };
            let _ = client.tx.send(
                Message::Text(json!({ "type": "ack", "id": mid, "ok": ok }).to_string().into()),
            );
            true
        }
        Some("admin_req") => {
            let rid = v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
            let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
            let node = v.get("node").and_then(|n| n.as_str()).unwrap_or("").to_string();
            let password = v.get("password").and_then(|p| p.as_str()).unwrap_or("").to_string();
            let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("").to_string();
            let resp = match run_admin_rpc(&method, &node, &password, &cmd).await {
                Ok(result) => json!({ "type": "admin_resp", "id": rid, "ok": true, "result": result }),
                Err(e) => json!({ "type": "admin_resp", "id": rid, "ok": false, "error": e }),
            };
            let _ = client.tx.send(Message::Text(resp.to_string().into()));
            true
        }
        _ => true, // unknown frame types are ignored for forward compat
    }
}

/// Execute a remote-admin request from a broker client against the local
/// radio (the broker host owns the radio link).
async fn run_admin_rpc(
    method: &str,
    node: &str,
    password: &str,
    cmd: &str,
) -> Result<Value, String> {
    let r = crate::radio::radio().ok_or("no MeshCore radio on the broker host")?;
    if !r.connected() {
        return Err(format!("MeshCore radio not connected on the broker host ({})", r.detail()));
    }
    match method {
        "login" => {
            let res = r.remote_login(node, password).await;
            if let Err(login_err) = &res {
                // The radio may not know this contact (e.g. re-flashed node
                // with a new key): push it (cmd carries the contact name for
                // this method), then retry once.
                if !cmd.is_empty() {
                    if let Err(add_err) = r.add_contact(node, cmd, 3).await {
                        return Err(format!("add contact: {add_err}; login: {login_err}"));
                    }
                    return Ok(json!(r.remote_login(node, password).await?));
                }
            }
            Ok(json!(res?))
        }
        "addcontact" => Ok(json!(r.add_contact(node, password, 3).await.is_ok())),
        "status" => r.remote_status(node).await,
        "acl" => r.remote_access_list(node).await,
        "cmd" => Ok(json!(r.remote_cmd(node, cmd).await?)),
        _ => Err(format!("unknown admin method: {method}")),
    }
}

async fn forward_outbound(to: &str, text: &str) -> bool {
    if let Some(idx) = to.strip_prefix("#chan") {
        if let (Some(r), Ok(n)) = (crate::radio::radio(), idx.parse::<i64>()) {
            return r.send_chan(n, text).await;
        }
        return false;
    }
    match crate::radio::radio() {
        Some(r) if r.connected() => r.send_dm(to, text).await,
        _ => false,
    }
}
