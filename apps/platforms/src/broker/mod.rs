//! Broker client: persistent WebSocket connection to the standalone
//! meshbroker service. Port of `mesh/broker_client.py` (simplified:
//! Link: remote commands, cross-instance client pseudo-nodes, and the
//! Meshtastic node mirror are not ported yet; see README).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use regex::Regex;
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::hub::Hub;
use crate::sage::{futures_lite_block_on, SageBot};
use crate::settings::MeshSettings;

pub const PROTOCOL_MESHCORE: &str = "meshcore";
pub const PROTOCOL_CLIENT: &str = "client";
pub const MAX_PACKET_TEXT: usize = 200;

const ULID_ENCODING: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn normalize_node_id(raw: &str) -> String {
    let s = raw.trim().to_lowercase();
    let s = s.strip_prefix("0x").unwrap_or(&s);
    s.chars().filter(|c| c.is_ascii_hexdigit()).collect()
}

#[allow(dead_code)]
pub fn normalize_meshtastic_node_id(raw: &str) -> String {
    let s = raw.trim().to_lowercase();
    let s = s.strip_prefix('!').unwrap_or(&s);
    let out: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if out.is_empty() { String::new() } else { format!("!{out}") }
}

pub fn frame_age_secs(frame_id: &str) -> f64 {
    // ULID: first 10 Crockford-base32 chars = 48-bit ms timestamp.
    let mut value: u64 = 0;
    for ch in frame_id.chars().take(10) {
        let Some(idx) = ULID_ENCODING.iter().position(|&c| {
            c == ch.to_ascii_uppercase() as u8
        }) else {
            return 0.0;
        };
        value = (value << 5) | idx as u64;
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    ((now_ms.saturating_sub(value)) as f64 / 1000.0).max(0.0)
}

pub fn format_outbound(sender_username: &str, content: &str) -> String {
    let who = sender_username.strip_prefix("mesh-").unwrap_or(sender_username);
    let body = content.trim();
    if let Some(rest) = body.strip_prefix('!') {
        return rest.chars().take(MAX_PACKET_TEXT).collect();
    }
    format!("{who}: {body}").chars().take(MAX_PACKET_TEXT).collect()
}

/// Extract a "To: username" routing prefix. Returns (target, body).
pub fn parse_to_user(text: &str) -> (Option<String>, String) {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?s)^\s*(?:(?P<via>[A-Za-z0-9_.\- ]{1,32})\s*:\s*)?[Tt][Oo]\s*:\s*(?P<target>[A-Za-z0-9_.\-]+(?:@[A-Za-z0-9_.\-]+)?)\s*(?:[-:,|]\s*)?(?P<body>.*)$"
        ).unwrap()
    });
    let Some(m) = re.captures(text) else {
        return (None, text.to_string());
    };
    let mut body = m.name("body").map(|b| b.as_str().trim().to_string()).unwrap_or_default();
    if body.is_empty() {
        body = text.to_string();
    }
    if let Some(via) = m.name("via").map(|v| v.as_str().trim()).filter(|v| !v.is_empty()) {
        body = format!("{via}: {body}");
    }
    let mut target = m.name("target").unwrap().as_str().to_string();
    if let Some((local, _)) = target.split_once('@') {
        target = local.to_string();
    }
    (Some(target), body)
}

/// Leading "username: " sender prefix (routing hint on replies).
pub fn sender_prefix(text: &str) -> Option<String> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^\s*(?P<name>[A-Za-z0-9_.\-]{1,32})\s*:\s*(?P<body>.+)$").unwrap()
    });
    re.captures(text).map(|m| m.name("name").unwrap().as_str().to_string())
}

pub struct BrokerClient {
    pub db: SqlitePool,
    pub settings: RwLock<MeshSettings>,
    pub sage: Arc<SageBot>,
    connected: AtomicBool,

    ws_tx: mpsc::UnboundedSender<Message>,
    ws_rx_holder: Mutex<Option<mpsc::UnboundedReceiver<Message>>>,
    acks: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    ack_seq: AtomicI64,
    resume_id: RwLock<String>,
    http: reqwest::Client,
}

static CLIENT: std::sync::OnceLock<Arc<BrokerClient>> = std::sync::OnceLock::new();

pub fn broker() -> Option<&'static Arc<BrokerClient>> {
    CLIENT.get()
}

pub async fn init_broker(db: SqlitePool) -> Arc<BrokerClient> {
    let settings = MeshSettings::load(&db).await;
    let (tx, rx) = mpsc::unbounded_channel();
    let client = Arc::new(BrokerClient {
        sage: SageBot::new(db.clone()),
        db,
        settings: RwLock::new(settings),
        connected: AtomicBool::new(false),

        ws_tx: tx,
        ws_rx_holder: Mutex::new(Some(rx)),
        acks: Mutex::new(HashMap::new()),
        ack_seq: AtomicI64::new(0),
        resume_id: RwLock::new(String::new()),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client"),
    });
    let _ = CLIENT.set(client.clone());
    client
}

pub async fn start_broker_if_enabled(db: &SqlitePool) {
    let client = init_broker(db.clone()).await;
    let settings = client.settings.read().unwrap().clone();
    let radio_started = crate::radio::start_radio_if_enabled(&settings);
    if radio_started {
        // Direct participation: no broker process needed. Sage and the
        // outbound catch-up queue run against the radio link.
        let me = client.clone();
        tokio::spawn(async move {
            me.sage.start().await;
        });
        eprintln!(
            "mesh radio (direct MeshCore) enabled; connecting to {}",
            settings.radio_host
        );
        // Optionally share this radio with other consoles.
        crate::broker_server::start_broker_server_if_enabled(&settings);
        return;
    }
    let enabled = settings.broker_enabled;
    if !enabled {
        eprintln!("mesh broker disabled; set mesh_broker_enabled=1 or MESH_BROKER_URL");
        return;
    }
    client.start();
}

impl BrokerClient {
    pub fn start(&self) {
        let me = broker().expect("client registered").clone();
        tokio::spawn(async move {
            me.sage.start().await;
            me.run_loop().await;
        });
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn ws_url(&self) -> String {
        let base = self.settings.read().unwrap().broker_url.trim_end_matches('/').to_string();
        let base = if let Some(r) = base.strip_prefix("https://") {
            format!("wss://{r}")
        } else if let Some(r) = base.strip_prefix("http://") {
            format!("ws://{r}")
        } else {
            base
        };
        format!("{base}/mesh/ws")
    }

    fn send_json(&self, v: &Value) -> bool {
        self.connected()
            && self
                .ws_tx
                .send(Message::Text(v.to_string()))
                .is_ok()
    }

    async fn run_loop(&self) {
        let mut backoff = 2u64;
        // Take the outbound half of the channel for this loop.
        let mut outbound = self.ws_rx_holder.lock().unwrap().take();
        loop {
            let url = self.ws_url();
            let attempt = tokio_tungstenite::connect_async(&url).await;
            let (ws, _) = match attempt {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("mesh broker link unavailable ({e}); retry {backoff}s");
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                    continue;
                }
            };
            backoff = 2;
            let (mut sink, mut stream) = ws.split();

            if self.resume_id.read().unwrap().is_empty() {
                if let Some(cursor) = self.load_resume_cursor().await {
                    *self.resume_id.write().unwrap() = cursor;
                }
            }
            let mut hello = json!({
                "type": "hello",
                "token": self.settings.read().unwrap().broker_token,
                "instance": self.settings.read().unwrap().instance_name,
                "version": "1.0",
            });
            let resume = self.resume_id.read().unwrap().clone();
            if !resume.is_empty() {
                hello["resume"] = json!(resume);
            }
            if sink.send(Message::Text(hello.to_string())).await.is_err() {
                continue;
            }
            let welcome = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await
            {
                Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).ok(),
                _ => None,
            };
            let Some(welcome) = welcome.filter(|w| w.get("type").and_then(|t| t.as_str()) == Some("welcome"))
            else {
                eprintln!("mesh broker handshake failed");
                self.connected.store(false, Ordering::Relaxed);
                continue;
            };
            self.connected.store(true, Ordering::Relaxed);
            eprintln!(
                "mesh broker connected (client={:?}, replayed={:?})",
                welcome.get("client_id"), welcome.get("resume_replayed")
            );

            if let Err(e) = self.sync_subscriptions().await {
                eprintln!("subscription sync failed: {e}");
            }
            self.register_names().await;
            let me = broker().unwrap().clone();
            tokio::spawn(async move { me.catchup_outbound().await });

            // Pump: read frames; forward outbound sends from the channel.
            loop {
                tokio::select! {
                    frame = stream.next() => {
                        match frame {
                            Some(Ok(Message::Text(t))) => {
                                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                                if let Err(e) = self.handle_frame(v).await {
                                    eprintln!("frame handling error: {e}");
                                }
                            }
                            Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        }
                    }
                    out = async {
                        match outbound.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        match out {
                            Some(msg) => { if sink.send(msg).await.is_err() { break; } }
                            None => break,
                        }
                    }
                }
            }
            self.connected.store(false, Ordering::Relaxed);
            eprintln!("mesh broker link lost; reconnecting in {backoff}s");
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    }

    async fn handle_frame(&self, frame: Value) -> Result<(), sqlx::Error> {
        match frame.get("type").and_then(|t| t.as_str()) {
            Some("msg") => {
                if let Some(id) = frame.get("id").and_then(|i| i.as_str()) {
                    if !id.is_empty() {
                        *self.resume_id.write().unwrap() = id.to_string();
                    }
                }
                self.ingest(frame).await?;
            }
            Some("media") => {
                if let Some(m) = frame.get("media") {
                    crate::media::ingest_broker(m).await;
                }
            }
            Some("status") => {
            }
            Some("directory") => {
                if let Err(e) = self.mirror_directory(frame.get("contacts")).await {
                    eprintln!("directory mirror error: {e}");
                }
            }
            Some("ack") | Some("admin_resp") => {
                let id = frame.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                if let Some(tx) = self.acks.lock().unwrap().remove(&id) {
                    let _ = tx.send(frame);
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn load_resume_cursor(&self) -> Option<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT frame_id FROM mesh_ingested_frames ORDER BY created_at DESC, frame_id DESC LIMIT 1",
        )
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()
    }

    // ------------------------------------------------------------ subscriptions

    async fn current_subscriptions(&self) -> Vec<Value> {
        let mut addresses: std::collections::BTreeSet<String> =
            (0..8).map(|i| format!("#chan{i}")).collect();
        let _ = sqlx::query(
            "SELECT n.node_id FROM mesh_nodes n \
             JOIN mesh_bridge_mappings m ON m.mesh_node_id = n.id \
             WHERE m.enabled = 1 AND n.protocol != 'client'",
        )
        .fetch_all(&self.db)
        .await
        .map(|rows| {
            for r in rows {
                if let Ok(id) = r.try_get::<String, _>("node_id") {
                    addresses.insert(id);
                }
            }
        });
        let _ = sqlx::query(
            "SELECT d.node_id FROM mesh_directory d WHERE d.show_traffic = 1",
        )
        .fetch_all(&self.db)
        .await
        .map(|rows| {
            for r in rows {
                if let Ok(id) = r.try_get::<String, _>("node_id") {
                    addresses.insert(id);
                }
            }
        });
        addresses
            .into_iter()
            .map(|a| {
                json!({
                    "address": a,
                    "kind": if a.starts_with("#chan") { "channel" } else { "node" }
                })
            })
            .collect()
    }

    pub async fn sync_subscriptions(&self) -> Result<(), sqlx::Error> {
        if !self.connected() {
            return Ok(());
        }
        let subs = self.current_subscriptions().await;
        self.send_json(&json!({ "type": "subscribe", "subs": subs }));
        Ok(())
    }

    async fn register_names(&self) {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT username FROM users WHERE is_active = 1 AND role = 'admin'",
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        if self.connected() {
            self.send_json(&json!({ "type": "register_names", "names": names }));
        }
    }

    // ------------------------------------------------------------ outbound

    async fn wait_ack(&self, mid: &str) -> Option<Value> {
        self.wait_ack_for(mid, Duration::from_secs(25)).await
    }

    async fn wait_ack_for(&self, mid: &str, timeout: Duration) -> Option<Value> {
        let (tx, rx) = oneshot::channel();
        self.acks.lock().unwrap().insert(mid.to_string(), tx);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => Some(v),
            _ => {
                self.acks.lock().unwrap().remove(mid);
                None
            }
        }
    }

    /// Request a remote-admin operation on the broker host (which owns
    /// the radio). Returns the "result" field of the response.
    pub async fn admin_rpc(
        &self,
        method: &str,
        node: &str,
        password: &str,
        cmd: &str,
    ) -> Result<Value, String> {
        if !self.connected() {
            return Err("mesh broker not connected".into());
        }
        let mid = format!("a{}", self.ack_seq.fetch_add(1, Ordering::Relaxed));
        let msg = json!({
            "type": "admin_req", "id": mid, "method": method,
            "node": node, "password": password, "cmd": cmd,
        });
        if !self.send_json(&msg) {
            return Err("mesh broker send failed".into());
        }
        // RF round-trips (login + binary request) can take a minute or more.
        let ack_timeout = if method == "acl" || method == "login" {
            Duration::from_secs(120)
        } else {
            Duration::from_secs(25)
        };
        match self.wait_ack_for(&mid, ack_timeout).await {
            Some(resp) if resp.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) => {
                Ok(resp.get("result").cloned().unwrap_or(Value::Null))
            }
            Some(resp) => Err(
                resp.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("broker admin request failed")
                    .to_string(),
            ),
            None => Err("mesh broker admin request timed out".into()),
        }
    }

    pub async fn send_dm(&self, node_id: &str, text: &str) -> bool {
        if let Some(r) = crate::radio::radio() {
            if r.connected() {
                return r.send_dm(node_id, text).await;
            }
            return false;
        }
        if self.connected() {
            let mid = format!("c{}", self.ack_seq.fetch_add(1, Ordering::Relaxed));
            let msg = json!({ "type": "dm", "id": mid, "to": node_id, "text": text });
            if self.send_json(&msg) {
                if let Some(ack) = self.wait_ack(&mid).await {
                    return ack.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
                }
                return false;
            }
        }
        self.rest_dm(node_id, text).await
    }

    pub async fn send_chan(&self, chan_idx: i64, text: &str) -> bool {
        let ok = if let Some(r) = crate::radio::radio() {
            if r.connected() {
                r.send_chan(chan_idx, text).await
            } else {
                return false;
            }
        } else if self.connected() {
            let mid = format!("c{}", self.ack_seq.fetch_add(1, Ordering::Relaxed));
            let msg = json!({ "type": "chan", "id": mid, "chan": chan_idx, "text": text });
            if self.send_json(&msg) {
                if let Some(ack) = self.wait_ack(&mid).await {
                    ack.get("ok").and_then(|o| o.as_bool()).unwrap_or(false)
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            self.rest_dm(&format!("#chan{chan_idx}"), text).await
        };
        // Locally posted channel text never comes back through ingest, so
        // run the same "? question" sage trigger on our own outbound posts.
        if ok {
            self.sage_check_outbound(chan_idx, text);
        }
        ok
    }

    /// Sage trigger for channel messages typed on the console itself.
    fn sage_check_outbound(&self, chan_idx: i64, text: &str) {
        if !self.settings.read().unwrap().sage_enabled {
            return;
        }
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(r"(?si)^(?:(?P<who>[^\n:]{1,32}):\s*)?!?\s*\?\s*(?P<q>.+)$").unwrap()
        });
        let Some(m) = re.captures(text) else { return };
        let who = m.name("who").map(|w| w.as_str().trim().to_string()).unwrap_or_default();
        if who.eq_ignore_ascii_case("sage") {
            return; // never react to our own replies
        }
        let chan_name = futures_lite_block_on(async {
            sqlx::query_scalar::<_, String>("SELECT long_name FROM mesh_nodes WHERE node_id = ?")
                .bind(format!("#chan{chan_idx}"))
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
        });
        if !self.sage.interested(&chan_name) {
            return;
        }
        let mut q = m.name("q").unwrap().as_str().trim().to_string();
        if Regex::new(r"(?i)^wx\b").unwrap().is_match(&q) {
            q = format!("weather: {q}");
        }
        self.sage.enqueue(
            chan_idx,
            &chan_name,
            &if who.is_empty() { "local".into() } else { who },
            &q,
        );
    }

    async fn rest_dm(&self, node_id: &str, text: &str) -> bool {
        let base = self.settings.read().unwrap().broker_url.trim_end_matches('/').to_string();
        let Ok(resp) = self
            .http
            .post(format!("{base}/api/dm"))
            .json(&json!({ "node_id": node_id, "text": text }))
            .send()
            .await
        else {
            return false;
        };
        resp.json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
            .unwrap_or(false)
    }

    async fn send_node_text(&self, node_id: &str, protocol: &str, text: &str) -> bool {
        if protocol == PROTOCOL_CLIENT && node_id.starts_with("client:") {
            // cross-instance client routing not ported yet
            return false;
        }
        if let Some(idx) = node_id.strip_prefix("#chan") {
            if let Ok(n) = idx.parse::<i64>() {
                return self.send_chan(n, text).await;
            }
            return false;
        }
        self.send_dm(node_id, text).await
    }

    pub async fn push_conversation_message(&self, conversation_id: &str, message_id: &str) {
        if let Err(e) = self.push_inner(conversation_id, message_id).await {
            eprintln!("push_conversation_message failed: {e}");
        }
    }

    /// Find or create the DM conversation between a console user and a mesh
    /// node (mirroring how inbound mesh DMs are threaded). The node gets a
    /// bridge mapping owned by the user, so replies from the node land in
    /// the same thread. Returns the conversation id.
    pub async fn open_dm_conversation(
        &self,
        node_id: &str,
        name: &str,
        user_id: i64,
    ) -> Result<String, sqlx::Error> {
        let node = self.upsert_mesh_node(node_id, name, PROTOCOL_MESHCORE).await?;
        let bound: i64 = node.get("bound_user_id");
        if bound != user_id {
            let (_, conv_id) = self
                .find_or_create_dm_mapping(&node, user_id, name, PROTOCOL_MESHCORE)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            return Ok(conv_id);
        }
        Err(sqlx::Error::RowNotFound)
    }

    /// Find or create the shared room conversation for a mesh node (the
    /// owner-less mapping inbound room traffic is threaded into). Returns
    /// the conversation id.
    pub async fn open_room_conversation(
        &self,
        node_id: &str,
        name: &str,
    ) -> Result<String, sqlx::Error> {
        let node = self.upsert_mesh_node(node_id, name, PROTOCOL_MESHCORE).await?;
        if let Some((_, conv_id)) = self.find_room_mapping(&node).await? {
            return Ok(conv_id);
        }
        let (_, conv_id) = self
            .create_catchall_room(&node, name)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        Ok(conv_id)
    }

    async fn push_inner(&self, conversation_id: &str, message_id: &str) -> Result<(), sqlx::Error> {
        let mappings = sqlx::query(
            "SELECT m.id AS mapping_id, m.last_forwarded_at, n.node_id, n.protocol, n.long_name \
             FROM mesh_bridge_mappings m \
             JOIN mesh_nodes n ON n.id = m.mesh_node_id \
             WHERE m.conversation_id = ? AND m.enabled = 1",
        )
        .bind(conversation_id)
        .fetch_all(&self.db)
        .await?;
        if mappings.is_empty() {
            return Ok(());
        }
        let row = sqlx::query(
            "SELECT m.content, m.created_at, u.username \
             FROM messages m JOIN users u ON u.id = m.sender_id WHERE m.id = ?",
        )
        .bind(message_id)
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else { return Ok(()) };
        let content: String = row.get("content");
        let created_at: String = row.get("created_at");
        let username: String = row.get("username");
        let text = format_outbound(&username, &content);

        for m in mappings {
            let node_id: String = m.get("node_id");
            let protocol: String = m.get("protocol");
            let mapping_id: String = m.get("mapping_id");
            let last_fw: String = m.get("last_forwarded_at");
            let long_name: String = m.get("long_name");
            let ok = self.send_node_text(&node_id, &protocol, &text).await;
            if ok {
                log_traffic(&self.db, &protocol, "out", &node_id, &long_name, &text).await;
                if created_at > last_fw {
                    let _ = sqlx::query("UPDATE mesh_bridge_mappings SET last_forwarded_at = ? WHERE id = ?")
                        .bind(&created_at)
                        .bind(&mapping_id)
                        .execute(&self.db)
                        .await;
                }
            } else {
                eprintln!("mesh out FAILED -> {node_id}: watermarked for catch-up retry");
            }
        }
        Ok(())
    }

    async fn catchup_outbound(&self) {
        // Ship messages newer than each mapping's watermark (limit to a
        // burst cap so a long outage doesn't flood the radio).
        let Ok(rows) = sqlx::query(
            "SELECT m.conversation_id, msg.id AS message_id \
             FROM mesh_bridge_mappings m \
             JOIN messages msg ON msg.conversation_id = m.conversation_id \
             WHERE m.enabled = 1 AND msg.created_at > m.last_forwarded_at \
             ORDER BY msg.created_at LIMIT 50",
        )
        .fetch_all(&self.db)
        .await
        else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        for r in rows {
            let conv: String = r.get("conversation_id");
            let mid: String = r.get("message_id");
            if seen.insert((conv.clone(), mid.clone())) {
                self.push_conversation_message(&conv, &mid).await;
            }
        }
    }

    // ------------------------------------------------------------ ingest

    async fn ingest(&self, frame: Value) -> Result<(), sqlx::Error> {
        if frame.get("kind").and_then(|k| k.as_str()) == Some("client")
            || frame.get("proto").and_then(|p| p.as_str()) == Some(PROTOCOL_CLIENT)
        {
            return Ok(()); // cross-instance client ingest not ported yet
        }
        let node_id = frame.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let text = frame.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let proto = frame.get("proto").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_MESHCORE).to_string();
        let name = frame.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let frame_id = frame.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if node_id.is_empty() || text.is_empty() {
            return Ok(());
        }
        let raw_text = text.clone();

        // Idempotent frame claim (single transaction with all writes).
        let claimed = sqlx::query("INSERT OR IGNORE INTO mesh_ingested_frames (frame_id) VALUES (?)")
            .bind(&frame_id)
            .execute(&self.db)
            .await?
            .rows_affected();
        if claimed == 0 {
            return Ok(()); // replay or duplicate
        }

        let node = self.upsert_mesh_node(&node_id, &name, &proto).await?;
        log_traffic(&self.db, &proto, "in", &node_id, &name, &text).await;

        // Stale channel replay guard.
        if node_id.starts_with("#chan")
            && !frame_id.is_empty()
            && frame_age_secs(&frame_id) > self.settings.read().unwrap().channel_replay_max_secs as f64
        {
            return Ok(());
        }

        // Sage trigger: "? question" (optionally "sender: ? ...") on a channel.
        if self.settings.read().unwrap().sage_enabled && node_id.starts_with("#chan") {
            static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
            let re = RE.get_or_init(|| {
                Regex::new(r"(?si)^(?:(?P<who>[^\n:]{1,32}):\s*)?!?\s*\?\s*(?P<q>.+)$").unwrap()
            });
            if let Some(m) = re.captures(&raw_text) {
                let chan_name = {
                    let long: String = node.get("long_name");
                    long
                };
                let chan_idx: i64 = node_id[5..].parse().unwrap_or(-1);
                if chan_idx >= 0 && self.sage.interested(&chan_name) {
                    let mut q = m.name("q").unwrap().as_str().trim().to_string();
                    if Regex::new(r"(?i)^wx\b").unwrap().is_match(&q) {
                        q = format!("weather: {q}");
                    }
                    let who = m.name("who").map(|w| w.as_str().trim().to_string()).unwrap_or_default();
                    self.sage.enqueue(chan_idx, &chan_name, &if who.is_empty() { "anon".into() } else { who }, &q);
                }
            }
        }

        // Routing: "To: user" prefix, sender-prefix reply routing, then
        // room mapping, sole-DM fallback, and finally catch-all room.
        let mut delivered_text = text.clone();
        let (routed, body) = parse_to_user(&text);
        let mut mapping: Option<(String, String)> = None; // (mapping_id, conversation_id)

        if let Some(target_name) = routed {
            if let Some(target_id) = self.resolve_mesh_user(&target_name).await? {
                delivered_text = body.clone();
                mapping = self
                    .find_or_create_dm_mapping(&node, target_id, &name, &proto)
                    .await?;
            } else {
                mapping = self.find_room_mapping(&node).await?;
            }
        }
        if mapping.is_none() {
            if let Some(sender) = sender_prefix(&text) {
                if let Some(target_id) = self.resolve_mesh_user(&sender).await? {
                    mapping = self
                        .find_or_create_dm_mapping(&node, target_id, &name, &proto)
                        .await?;
                }
            }
        }
        if mapping.is_none() {
            mapping = self.find_room_mapping(&node).await?;
        }
        if mapping.is_none() && !node_id.starts_with("#chan") {
            // Sole DM mapping fallback (gateway-to-gateway replies).
            let rows = sqlx::query(
                "SELECT id, conversation_id FROM mesh_bridge_mappings \
                 WHERE mesh_node_id = ? AND enabled = 1 AND owner_user_id IS NOT NULL \
                 ORDER BY created_at DESC",
            )
            .bind(node.get::<String, _>("id"))
            .fetch_all(&self.db)
            .await?;
            if rows.len() == 1 {
                mapping = Some((rows[0].get("id"), rows[0].get("conversation_id")));
            }
        }
        if mapping.is_none() && !node_id.starts_with("#chan") {
            // Catch-all shared room: every admin participates.
            mapping = self.create_catchall_room(&node, &name).await?;
        }
        let Some((_, conversation_id)) = mapping else {
            return Ok(()); // unmapped traffic logged only
        };

        let msg_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO messages (id, conversation_id, sender_id, content, mesh_frame_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&msg_id)
        .bind(&conversation_id)
        .bind(node.get::<i64, _>("bound_user_id"))
        .bind(delivered_text.chars().take(4000).collect::<String>())
        .bind(&frame_id)
        .bind(&now)
        .execute(&self.db)
        .await?;
        if inserted.rows_affected() == 0 {
            return Ok(()); // frame already yielded a message
        }
        let _ = sqlx::query(
            "UPDATE mesh_bridge_mappings SET last_forwarded_at = ? \
             WHERE conversation_id = ? AND ? > last_forwarded_at",
        )
        .bind(&now)
        .bind(&conversation_id)
        .bind(&now)
        .execute(&self.db)
        .await;
        let _ = sqlx::query("UPDATE conversations SET updated_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&conversation_id)
            .execute(&self.db)
            .await;

        Hub::global().notify(&conversation_id);
        eprintln!("mesh in ({}) <- {node_id}: {:?}", &proto[..proto.len().min(2)], raw_text.chars().take(60).collect::<String>());
        Ok(())
    }

    // ------------------------------------------------------------ helpers

    async fn upsert_mesh_node(&self, node_id: &str, name: &str, proto: &str) -> Result<sqlx::sqlite::SqliteRow, sqlx::Error> {
        // Nodes can be referenced by full 64-hex key or a shorter prefix
        // (the radio reports 12 hex chars). Canonicalize onto the existing
        // row so one node never gets two identities / conversations.
        let existing = sqlx::query(
            "SELECT * FROM mesh_nodes WHERE protocol = ? AND (node_id = ? OR (length(?) < length(node_id) AND node_id LIKE ? || '%') OR (length(node_id) < length(?) AND ? LIKE node_id || '%'))",
        )
        .bind(proto)
        .bind(node_id)
        .bind(node_id)
        .bind(node_id)
        .bind(node_id)
        .bind(node_id)
        .fetch_optional(&self.db)
        .await?;
        let node_id_pk = match existing {
            Some(row) => row.get::<String, _>("id"),
            None => self.auto_provision_node(node_id, name, proto).await?,
        };
        let short = name.split_whitespace().next().unwrap_or("").chars().take(16).collect::<String>();
        let long = name.chars().take(64).collect::<String>();
        sqlx::query(
            "UPDATE mesh_nodes SET short_name = ?, long_name = ?, last_seen_at = datetime('now') WHERE id = ?",
        )
        .bind(&short)
        .bind(&long)
        .bind(&node_id_pk)
        .execute(&self.db)
        .await?;
        sqlx::query("SELECT * FROM mesh_nodes WHERE id = ?")
            .bind(&node_id_pk)
            .fetch_one(&self.db)
            .await
    }

    async fn auto_provision_node(&self, node_id: &str, name: &str, proto: &str) -> Result<String, sqlx::Error> {
        let label: String = name.split_whitespace().next().unwrap_or(node_id).chars().take(32).collect();
        let mut username = format!("mesh-{label}");
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(&username)
            .fetch_optional(&self.db)
            .await?;
        if exists.is_some() {
            username = format!("mesh-{label}-{:04}", rand_suffix());
        }
        let placeholder_email = format!("{username}@mesh.local");
        let user_id: i64 = sqlx::query(
            "INSERT INTO users (username, email, password_hash, role) VALUES (?, ?, ?, 'user') RETURNING id",
        )
        .bind(&username)
        .bind(&placeholder_email)
        .bind(syarc_plugin_api::hash_password(&Uuid::new_v4().to_string()))
        .fetch_one(&self.db)
        .await?
        .get("id");
        let pk = Uuid::new_v4().to_string();
        let short: String = name.split_whitespace().next().unwrap_or("").chars().take(16).collect();
        let long: String = name.chars().take(64).collect();
        sqlx::query(
            "INSERT INTO mesh_nodes (id, node_id, protocol, short_name, long_name, bound_user_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&pk)
        .bind(node_id)
        .bind(proto)
        .bind(&short)
        .bind(&long)
        .bind(user_id)
        .execute(&self.db)
        .await?;
        eprintln!("auto-provisioned {proto} node {node_id} as user {username}");
        Ok(pk)
    }

    async fn resolve_mesh_user(&self, username: &str) -> Result<Option<i64>, sqlx::Error> {
        // Mesh senders can't see our capitalization; admins implicitly
        // hold mesh access (no per-app permission table in the console yet).
        sqlx::query_scalar::<_, i64>(
            "SELECT id FROM users WHERE lower(username) = lower(?) AND is_active = 1",
        )
        .bind(username.trim())
        .fetch_optional(&self.db)
        .await
    }

    async fn find_room_mapping(&self, node: &sqlx::sqlite::SqliteRow) -> Result<Option<(String, String)>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, conversation_id FROM mesh_bridge_mappings \
             WHERE mesh_node_id = ? AND enabled = 1 AND owner_user_id IS NULL \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(node.get::<String, _>("id"))
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|r| (r.get("id"), r.get("conversation_id"))))
    }

    #[allow(clippy::too_many_arguments)]
    async fn find_or_create_dm_mapping(
        &self,
        node: &sqlx::sqlite::SqliteRow,
        target_id: i64,
        node_display_name: &str,
        _proto: &str,
    ) -> Result<Option<(String, String)>, sqlx::Error> {
        let existing = sqlx::query(
            "SELECT id, conversation_id FROM mesh_bridge_mappings \
             WHERE mesh_node_id = ? AND enabled = 1 AND owner_user_id = ? \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(node.get::<String, _>("id"))
        .bind(target_id)
        .fetch_optional(&self.db)
        .await?;
        if let Some(r) = existing {
            return Ok(Some((r.get("id"), r.get("conversation_id"))));
        }
        let display = if node_display_name.is_empty() {
            let long: String = node.get("long_name");
            if long.is_empty() { node.get::<String, _>("node_id").chars().take(12).collect() } else { long }
        } else {
            node_display_name.to_string()
        };
        let conv_id = Uuid::new_v4().to_string();
        let mapping_id = Uuid::new_v4().to_string();
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO conversations (id, subject, created_by) VALUES (?, ?, ?)")
            .bind(&conv_id)
            .bind(format!("Mesh DM: {display}"))
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)")
            .bind(Uuid::new_v4().to_string())
            .bind(&conv_id)
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO mesh_bridge_mappings (id, conversation_id, mesh_node_id, owner_user_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&mapping_id)
        .bind(&conv_id)
        .bind(node.get::<String, _>("id"))
        .bind(target_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        eprintln!("auto-created DM mapping for {display} -> user {target_id}");
        Ok(Some((mapping_id, conv_id)))
    }

    async fn create_catchall_room(&self, node: &sqlx::sqlite::SqliteRow, name: &str) -> Result<Option<(String, String)>, sqlx::Error> {
        let display = if name.is_empty() {
            node.get::<String, _>("node_id").chars().take(12).collect::<String>()
        } else {
            name.to_string()
        };
        let admins: Vec<(i64, )> = sqlx::query("SELECT id FROM users WHERE is_active = 1 AND role = 'admin'")
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .map(|r| (r.get("id"), ))
            .collect();
        let Some((owner, )) = admins.first() else { return Ok(None) };
        let conv_id = Uuid::new_v4().to_string();
        let mapping_id = Uuid::new_v4().to_string();
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO conversations (id, subject, created_by) VALUES (?, ?, ?)")
            .bind(&conv_id)
            .bind(format!("Mesh: {display}"))
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        for (uid, ) in &admins {
            sqlx::query("INSERT INTO conversation_participants (id, conversation_id, user_id) VALUES (?, ?, ?)")
                .bind(Uuid::new_v4().to_string())
                .bind(&conv_id)
                .bind(uid)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO mesh_bridge_mappings (id, conversation_id, mesh_node_id, owner_user_id) VALUES (?, ?, ?, NULL)",
        )
        .bind(&mapping_id)
        .bind(&conv_id)
        .bind(node.get::<String, _>("id"))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let me = broker().unwrap().clone();
        tokio::spawn(async move {
            let _ = me.sync_subscriptions().await;
        });
        Ok(Some((mapping_id, conv_id)))
    }

    async fn mirror_directory(&self, contacts: Option<&Value>) -> Result<(), sqlx::Error> {
        let Some(list) = contacts.and_then(|c| c.as_array()) else { return Ok(()) };
        for c in list {
            let Some(node_id) = c.get("pubkey").and_then(|p| p.as_str()) else { continue };
            let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let node_type = c.get("adv_type").and_then(|t| t.as_i64()).unwrap_or(1);
            let lat = c.get("lat").and_then(|v| v.as_f64());
            let lon = c.get("lon").and_then(|v| v.as_f64());
            sqlx::query(
                "INSERT INTO mesh_directory (id, node_id, node_type, name, gps_lat, gps_lon) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(node_id) DO UPDATE SET \
                   name = excluded.name, node_type = excluded.node_type, \
                   gps_lat = excluded.gps_lat, gps_lon = excluded.gps_lon, \
                   last_synced_at = datetime('now')",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(normalize_node_id(node_id))
            .bind(node_type)
            .bind(name)
            .bind(lat)
            .bind(lon)
            .execute(&self.db)
            .await?;
        }
        // Cap the directory: keep the most recently synced 350 entries,
        // dropping the oldest — like the radio's own contact store.
        sqlx::query(
            "DELETE FROM mesh_directory WHERE is_manual = 0 AND node_id NOT IN \
             (SELECT node_id FROM mesh_directory WHERE is_manual = 0 ORDER BY last_synced_at DESC, rowid DESC LIMIT 350)",
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

impl BrokerClient {
    /// Entry points for the direct-radio transport (crate::radio).
    pub async fn ingest_frame(&self, frame: Value) -> Result<(), sqlx::Error> {
        self.ingest(frame).await
    }

    pub async fn mirror_directory_pub(&self, contacts: Option<&Value>) -> Result<(), sqlx::Error> {
        self.mirror_directory(contacts).await
    }

    /// Called by the radio client once its session is up, so status
    /// pages and send paths see the link as connected.
    pub fn sync_connected(&self) {
        if let Some(r) = crate::radio::radio() {
            if r.connected() {
                self.connected.store(true, Ordering::Relaxed);
                let me = broker().unwrap().clone();
                tokio::spawn(async move { me.catchup_outbound().await });
            }
        }
    }
}

/// Traffic logging for callers outside the broker client (MeshCore app).
pub async fn log_traffic_pub(protocol: &str, direction: &str, node_id: &str, node_name: &str, text: &str) {
    if let Some(b) = broker() {
        log_traffic(&b.db, protocol, direction, node_id, node_name, text).await;
    }
}

pub async fn log_traffic(db: &SqlitePool, protocol: &str, direction: &str, node_id: &str, node_name: &str, text: &str) {
    let _ = sqlx::query(
        "INSERT INTO mesh_traffic (protocol, direction, node_id, node_name, text) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(protocol)
    .bind(direction)
    .bind(node_id.chars().take(80).collect::<String>())
    .bind(node_name.chars().take(64).collect::<String>())
    .bind(text.chars().take(4000).collect::<String>())
    .execute(db)
    .await;
}

fn rand_suffix() -> u32 {
    uuid::Uuid::new_v4().as_u128() as u32 % 10000
}
