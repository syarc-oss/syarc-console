//! Direct MeshCore participation: speaks the companion-radio protocol
//! (the same wire protocol the MeshCore phone apps and the Python broker's
//! `meshcore` library use) straight over TCP to a companion radio node,
//! with no broker process in between.
//!
//! Framing: requests are `0x3c <len:u16le> <payload>`, responses are
//! `0x3e <len:u16le> <payload>`. Payload's first byte is a command code
//! (requests) or response code (responses). See
//! /usr/local/dev_apps/MeshCore/src/helpers/CommonCLI.cpp for the
//! firmware side of this protocol.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_serial::SerialStream;
use tokio::sync::{mpsc, oneshot};

use crate::broker::{self, BrokerClient};

// Response codes (PacketType in the companion protocol).
const R_OK: u8 = 0;
const R_ERROR: u8 = 1;
const R_CONTACT_START: u8 = 2;
const R_CONTACT: u8 = 3;
const R_CONTACT_END: u8 = 4;
const R_SELF_INFO: u8 = 5;
const R_MSG_SENT: u8 = 6;
const R_CONTACT_MSG: u8 = 7;
const R_CHANNEL_MSG: u8 = 8;
const R_NO_MORE_MSGS: u8 = 10;
const R_CHANNEL_DATA: u8 = 27;
const R_CHANNEL_INFO: u8 = 18;
const R_DEVICE_INFO: u8 = 13;
const R_ADVERTISEMENT: u8 = 0x80;
const PUSH_CODE_NEW_ADVERT: u8 = 0x8a;
const R_ACK: u8 = 0x82;
const R_MESSAGES_WAITING: u8 = 0x83;
// Remote-admin push codes (login/binary-status replies from a node).
const R_LOGIN_SUCCESS: u8 = 0x85;
const R_LOGIN_FAILED: u8 = 0x86;
const R_STATUS_RESPONSE: u8 = 0x87;
const R_BINARY_RESPONSE: u8 = 0x8C;
const CMD_SEND_LOGIN: u8 = 0x1a;
const CMD_BINARY_REQ: u8 = 0x32;
const BINARY_REQ_STATUS: u8 = 0x01;

/// response-code -> pending waiters (first wins, rest time out)
type RadioWaiters =
    Arc<Mutex<HashMap<u8, Vec<Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>>>>>;

const MAX_FRAME: usize = 300;

pub struct RadioClient {
    pub host: String,
    /// Bluetooth SPP transport: Some((mac, rfcomm_device)) when configured.
    bluetooth: Option<(String, String)>,
    connected: AtomicBool,
    detail: Mutex<String>,
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    // response-code -> pending waiters (first wins, rest time out)
    waiters: RadioWaiters,
    // channel idx -> name
    channel_names: Arc<Mutex<HashMap<u8, String>>>,
    // pubkey prefix (12 hex chars) -> advertised name
    contact_names: Arc<Mutex<HashMap<String, String>>>,
    self_name: Mutex<String>,
    self_pubkey: Mutex<String>,
    // full SELF_INFO payload from the APP_START handshake, as JSON
    self_info: Mutex<Option<serde_json::Value>>,
    // collector active while a contacts fetch is in flight
    contact_collector: Mutex<Option<mpsc::UnboundedSender<Contact>>>,
    // remote-admin CLI replies: 12-hex pubkey prefix -> waiter for reply text
    cli_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    // binary STATUS replies: 12-hex pubkey prefix -> waiter for raw frame
    status_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Vec<u8>>>>>,
    // binary (CMD_SEND_BINARY_REQ) replies: request tag -> waiter for raw frame
    binary_waiters: Arc<Mutex<HashMap<u32, oneshot::Sender<Vec<u8>>>>>,
}

#[derive(Debug, Clone)]
struct Contact {
    pubkey: String,
    name: String,
    adv_type: u8,
    lat: Option<f64>,
    lon: Option<f64>,
}

static RADIO: std::sync::OnceLock<Arc<RadioClient>> = std::sync::OnceLock::new();

pub fn radio() -> Option<&'static Arc<RadioClient>> {
    RADIO.get()
}

pub fn start_radio_if_enabled(settings: &crate::settings::MeshSettings) -> bool {
    if !settings.radio_enabled {
        return false;
    }
    let rfcomm_dev = std::env::var("MESH_RADIO_RFCOMM_DEV").unwrap_or_else(|_| "/dev/rfcomm0".into());
    let bluetooth = if settings.radio_bluetooth.trim().is_empty() {
        None
    } else if settings.radio_bluetooth.trim() == "1" || settings.radio_bluetooth.trim().eq_ignore_ascii_case("true") {
        // Use whatever rfcomm device is already bound (e.g. via a udev rule
        // or a manual `rfcomm bind`).
        Some((String::new(), rfcomm_dev))
    } else {
        Some((settings.radio_bluetooth.trim().to_string(), rfcomm_dev))
    };
    let client = Arc::new(RadioClient {
        host: settings.radio_host.clone(),
        bluetooth,
        connected: AtomicBool::new(false),
        detail: Mutex::new("not started".into()),
        cmd_tx: Mutex::new(None),
        waiters: Arc::new(Mutex::new(HashMap::new())),
        channel_names: Arc::new(Mutex::new(HashMap::new())),
        contact_names: Arc::new(Mutex::new(HashMap::new())),
        self_name: Mutex::new(String::new()),
        self_pubkey: Mutex::new(String::new()),
        self_info: Mutex::new(None),
        contact_collector: Mutex::new(None),
        cli_waiters: Arc::new(Mutex::new(HashMap::new())),
        status_waiters: Arc::new(Mutex::new(HashMap::new())),
        binary_waiters: Arc::new(Mutex::new(HashMap::new())),
    });
    let _ = RADIO.set(client.clone());
    let me = client.clone();
    tokio::spawn(async move { me.run_loop().await });
    true
}

impl RadioClient {
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn detail(&self) -> String {
        self.detail.lock().unwrap().clone()
    }

    pub fn channel_name(&self, idx: u8) -> String {
        self.channel_names
            .lock()
            .unwrap()
            .get(&idx)
            .cloned()
            .unwrap_or_else(|| format!("chan{idx}"))
    }

    /// Channels discovered from the radio, as (idx, name), sorted by idx.
    pub fn channels(&self) -> Vec<(u8, String)> {
        let mut v: Vec<(u8, String)> = self
            .channel_names
            .lock()
            .unwrap()
            .iter()
            .map(|(k, val)| (*k, val.clone()))
            .collect();
        v.sort_by_key(|(i, _)| *i);
        v
    }

    fn contact_name(&self, prefix: &str) -> String {
        self.contact_names
            .lock()
            .unwrap()
            .get(prefix)
            .cloned()
            .unwrap_or_default()
    }

    fn set_detail(&self, s: &str) {
        *self.detail.lock().unwrap() = s.to_string();
    }

    // ------------------------------------------------------------ command/response

    /// Send a command payload and wait for one of `codes` (or ERROR).
    async fn command(&self, payload: &[u8], codes: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
        let (tx, rx) = oneshot::channel();
        let waiter = Arc::new(Mutex::new(Some(tx)));
        {
            let mut w = self.waiters.lock().unwrap();
            for c in codes.iter().chain(std::iter::once(&R_ERROR)) {
                w.entry(*c).or_default().push(waiter.clone());
            }
        }
        self.prune_waiters();
        // the writer pump adds the 0x3c framing; queue the raw payload.
        let framed = payload.to_vec();
        let send = {
            let guard = self.cmd_tx.lock().unwrap();
            guard.as_ref().map(|ch| ch.send(framed).is_ok()).unwrap_or(false)
        };
        if !send {
            return Err("radio link down".into());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(data)) => {
                if !data.is_empty() && data[0] == R_ERROR {
                    return Err(format!("device error code {}", data.get(1).copied().unwrap_or(0)));
                }
                Ok(data)
            }
            _ => {
                self.prune_waiters();
                Err("command timeout".into())
            }
        }
    }

    fn prune_waiters(&self) {
        let mut w = self.waiters.lock().unwrap();
        for list in w.values_mut() {
            list.retain(|s| s.lock().unwrap().is_some());
        }
    }

    fn resolve(&self, code: u8, data: Vec<u8>) -> Option<Vec<u8>> {
        let mut w = self.waiters.lock().unwrap();
        while let Some(list) = w.get_mut(&code) {
            match list.pop() {
                Some(waiter) => {
                    let tx = waiter.lock().unwrap().take();
                    if let Some(tx) = tx {
                        if tx.send(data.clone()).is_ok() {
                            // Purge this waiter from the other code lists.
                            for l in w.values_mut() {
                                l.retain(|s| !Arc::ptr_eq(s, &waiter));
                            }
                            return Some(data);
                        }
                    }
                    // receiver dropped; try next waiter
                }
                None => {
                    w.remove(&code);
                    return None;
                }
            }
        }
        None
    }

    // ------------------------------------------------------------ outbound API

    /// Send a DM to a node identified by hex public key (a 12+ char prefix
    /// is accepted; the firmware routes on the first 6 bytes and expects
    /// ONLY those 6 bytes — anything after them is message text).
    pub async fn send_dm(&self, pubkey_hex: &str, text: &str) -> bool {
        let key = hex_decode(pubkey_hex);
        let Some(key) = key.filter(|k| k.len() >= 6) else {
            eprintln!("radio send_dm: invalid destination key");
            return false;
        };
        let ts = chrono::Utc::now().timestamp().max(0) as u32;
        let mut payload = vec![0x02, 0x00, 0x00]; // SEND_TXT_MSG, text (not cmd), attempt 0
        payload.extend_from_slice(&ts.to_le_bytes());
        payload.extend_from_slice(&key[..6]);
        payload.extend_from_slice(text.as_bytes());
        match self.command(&payload, &[R_MSG_SENT], Duration::from_secs(15)).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("radio send_dm failed: {e}");
                false
            }
        }
    }

    /// Upsert a contact on the radio with the favourite flag set, so it is
    /// protected from eviction when the radio's contact store is full and
    /// churning (the firmware only evicts non-favourite contacts).
    /// Frame layout mirrors MyMesh::updateContactFromFrame.
    pub async fn pin_contact(&self, pubkey_hex: &str, name: &str, node_type: u8) -> bool {
        let Some(key) = hex_decode(pubkey_hex).filter(|k| k.len() == 32) else {
            eprintln!("radio pin_contact: invalid key");
            return false;
        };
        let mut payload = vec![0x09]; // CMD_ADD_UPDATE_CONTACT
        payload.extend_from_slice(&key);
        payload.push(node_type);
        payload.push(0x01); // flags: favourite (eviction-protected)
        payload.push(0); // out_path_len: unknown
        payload.extend_from_slice(&[0u8; 64]); // out_path
        let mut name_bytes = [0u8; 32];
        for (d, s) in name_bytes.iter_mut().zip(name.as_bytes()) {
            *d = *s;
        }
        payload.extend_from_slice(&name_bytes);
        payload.extend_from_slice(&chrono::Utc::now().timestamp().max(0).to_le_bytes());
        match self.command(&payload, &[R_OK], Duration::from_secs(5)).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("radio pin_contact failed: {e}");
                false
            }
        }
    }

    /// Remove a contact from the radio (CMD_REMOVE_CONTACT).
    pub async fn remove_contact(&self, pubkey_hex: &str) -> bool {
        let Some(key) = hex_decode(pubkey_hex).filter(|k| k.len() == 32) else {
            return false;
        };
        let mut payload = vec![0x0f]; // CMD_REMOVE_CONTACT
        payload.extend_from_slice(&key);
        match self.command(&payload, &[R_OK], Duration::from_secs(5)).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("radio remove_contact failed: {e}");
                false
            }
        }
    }

    /// Read the auto-add config (bits: 0x01 overwrite-oldest-when-full,
    /// 0x02 chat, 0x04 repeater, 0x08 room server, 0x10 sensor).
    pub async fn get_autoadd_config(&self) -> Option<(u8, u8)> {
        let data = self
            .command(&[0x3b], &[25], Duration::from_secs(5))
            .await
            .ok()?;
        Some((*data.get(1)?, *data.get(2).unwrap_or(&0)))
    }

    /// Set the auto-add config; returns false on unsupported firmware.
    pub async fn set_autoadd_config(&self, config: u8, max_hops: u8) -> bool {
        match self
            .command(&[0x3a, config, max_hops], &[R_OK], Duration::from_secs(5))
            .await
        {
            Ok(_) => true,
            Err(e) => {
                eprintln!("radio set_autoadd_config failed: {e}");
                false
            }
        }
    }

    /// Send a channel (group) message.
    pub async fn send_chan(&self, chan_idx: i64, text: &str) -> bool {
        let ts = chrono::Utc::now().timestamp().max(0) as u32;
        let mut payload = vec![0x03, 0x00]; // SEND_CHANNEL_TXT_MSG
        payload.push(chan_idx.clamp(0, 255) as u8);
        payload.extend_from_slice(&ts.to_le_bytes());
        payload.extend_from_slice(text.as_bytes());
        match self.command(&payload, &[R_OK], Duration::from_secs(15)).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("radio send_chan failed: {e}");
                false
            }
        }
    }

    /// Send a remote-admin login to a node (repeater) over the mesh.
    /// Returns the node's permission byte (bit 0 = admin) on success.
    /// RF is lossy, so the login is retried up to 3 times with 2s gaps.
    pub async fn remote_login(&self, pubkey_hex: &str, password: &str) -> Result<u8, String> {
        let Some(key) = hex_decode(pubkey_hex).filter(|k| k.len() == 32) else {
            return Err("node key unavailable (need the full public key)".into());
        };
        let mut payload = vec![CMD_SEND_LOGIN];
        payload.extend_from_slice(&key);
        payload.extend_from_slice(password.as_bytes());
        let mut last_err = "no reply".to_string();
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            match self
                .command(&payload, &[R_LOGIN_SUCCESS, R_LOGIN_FAILED], Duration::from_secs(10))
                .await
            {
                Ok(data) => {
                    return match data.first() {
                        Some(&R_LOGIN_SUCCESS) => {
                            // Frame: [code, legacy_status, pub_key(6), tag(4), acl_perms, fw_ver]
                            // legacy_status: 1=admin, 2=read-only guest, 0=read-write.
                            // acl_perms (v7 firmware): 3=admin, 2=read-write, 0=guest.
                            let perms = if data.len() > 12 {
                                data[12] & 0x03
                            } else {
                                match data.get(1).copied().unwrap_or(2) {
                                    1 => 3, // admin
                                    0 => 2, // read-write
                                    _ => 0, // read-only guest
                                }
                            };
                            Ok(perms)
                        }
                        _ => Err("login failed (wrong password?)".into()),
                    };
                }
                Err(e) => last_err = e,
            }
        }
        Err(format!("node unreachable ({last_err})"))
    }

    /// Add (or update) a contact in the radio's contact store so it can be
    /// messaged/logged-in even if the radio never heard its advert
    /// (e.g. a re-flashed node with a new public key). Idempotent.
    pub async fn add_contact(&self, pubkey_hex: &str, name: &str, node_type: u8) -> Result<(), String> {
        let Some(key) = hex_decode(pubkey_hex).filter(|k| k.len() == 32) else {
            return Err("node key unavailable (need the full public key)".into());
        };
        // CMD_ADD_UPDATE_CONTACT frame: cmd, pub_key(32), type, flags,
        // out_path_len, out_path(64), name(32), last_advert_ts(4)
        let mut payload = vec![0x09u8];
        payload.extend_from_slice(&key);
        payload.push(node_type);
        payload.push(0); // flags
        payload.push(0); // out_path_len (will be discovered on first contact)
        payload.extend_from_slice(&[0u8; 64]); // out_path
        let mut name_bytes = [0u8; 32];
        let n = name.len().min(32);
        name_bytes[..n].copy_from_slice(&name.as_bytes()[..n]);
        payload.extend_from_slice(&name_bytes);
        payload.extend_from_slice(&0u32.to_le_bytes()); // last_advert_timestamp
        let res = self.command(&payload, &[R_OK], Duration::from_secs(10)).await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if e.contains("device error code 3") => {
                // Contact table full: enable overwrite-oldest-when-full
                // (favourite/pinned contacts stay protected) and retry once.
                if let Some((config, hops)) = self.get_autoadd_config().await {
                    if self.set_autoadd_config(config | 0x01, hops).await {
                        return match self.command(&payload, &[R_OK], Duration::from_secs(10)).await {
                            Ok(_) => Ok(()),
                            Err(e) => Err(format!("add contact failed: {e}")),
                        };
                    }
                }
                Err(format!("add contact failed: {e}"))
            }
            Err(e) => Err(format!("add contact failed: {e}")),
        }
    }

    /// Run a single meshcli command on a remote node and await its reply.
    /// The reply arrives later as a CONTACT_MSG with txt_type == 1.
    pub async fn remote_cmd(&self, pubkey_hex: &str, cmd: &str) -> Result<String, String> {
        let Some(key) = hex_decode(pubkey_hex).filter(|k| k.len() >= 6) else {
            return Err("invalid destination key".into());
        };
        let prefix: String = hex_encode(&key[..6]);
        let (tx, rx) = oneshot::channel();
        self.cli_waiters.lock().unwrap().insert(prefix.clone(), tx);
        // Safety net: drop the waiter if the caller goes away or the reply
        // never comes (a later command re-registers a fresh waiter anyway).
        let me = radio().unwrap().clone();
        let p2 = prefix.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            me.cli_waiters.lock().unwrap().remove(&p2);
        });
        let ts = chrono::Utc::now().timestamp().max(0) as u32;
        let mut payload = vec![0x02, 0x01, 0x00]; // SEND_TXT_MSG, CLI_DATA, attempt 0
        payload.extend_from_slice(&ts.to_le_bytes());
        payload.extend_from_slice(&key[..6]); // firmware consumes exactly 6 bytes; rest is text
        payload.extend_from_slice(cmd.as_bytes());
        // Wait for the radio to accept the frame, then for the node's reply.
        if let Err(e) = self.command(&payload, &[R_MSG_SENT], Duration::from_secs(15)).await {
            self.cli_waiters.lock().unwrap().remove(&prefix);
            return Err(format!("could not send command ({e})"));
        }
        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(text)) => Ok(text),
            _ => Err("no reply — node out of range, asleep, or requires admin login".into()),
        }
    }

    /// Request the binary STATUS frame from a remote node and parse it.
    pub async fn remote_status(&self, pubkey_hex: &str) -> Result<serde_json::Value, String> {
        let Some(key) = hex_decode(pubkey_hex).filter(|k| k.len() == 32) else {
            return Err("node key unavailable (need the full public key)".into());
        };
        let prefix = hex_encode(&key[..6]);
        let (tx, rx) = oneshot::channel();
        self.status_waiters.lock().unwrap().insert(prefix.clone(), tx);
        let me = radio().unwrap().clone();
        let p2 = prefix.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            me.status_waiters.lock().unwrap().remove(&p2);
        });
        let mut payload = vec![CMD_BINARY_REQ];
        payload.extend_from_slice(&key);
        payload.push(BINARY_REQ_STATUS);
        if let Err(e) = self.command(&payload, &[R_MSG_SENT, R_OK], Duration::from_secs(15)).await {
            self.status_waiters.lock().unwrap().remove(&prefix);
            return Err(format!("could not send status request ({e})"));
        }
        let data = match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(d)) => d,
            _ => {
                self.status_waiters.lock().unwrap().remove(&prefix);
                return Err("no status reply — node out of range or asleep".into());
            }
        };
        parse_status_frame(&data)
    }

    /// Fetch the room-server access-control list over the air, mirroring the
    /// official app's "access control" view: a REQ_TYPE_GET_ACCESS_LIST
    /// binary request (admin session required). The reply frame is
    /// `[0x8C, 0, tag(4), entries..]` where each entry is a 6-byte pubkey
    /// prefix plus a perms byte (3 = admin).
    pub async fn remote_access_list(&self, pubkey_hex: &str) -> Result<serde_json::Value, String> {
        let Some(key) = hex_decode(pubkey_hex).filter(|k| k.len() == 32) else {
            return Err("node key unavailable (need the full public key)".into());
        };
        let mut payload = vec![CMD_BINARY_REQ];
        payload.extend_from_slice(&key);
        payload.extend_from_slice(&[0x05, 0x00, 0x00]); // REQ_TYPE_GET_ACCESS_LIST + reserved
        // Wait for the radio to accept the frame; the reply carries a tag we
        // use to match the async 0x8C binary response.
        let data = match self.command(&payload, &[R_MSG_SENT], Duration::from_secs(15)).await {
            Ok(d) => d,
            Err(e) => return Err(format!("could not send access-list request ({e})")),
        };
        if data.len() < 6 {
            return Err("unexpected send reply from device".into());
        }
        let tag = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
        let (tx, rx) = oneshot::channel();
        self.binary_waiters.lock().unwrap().insert(tag, tx);
        let me = radio().unwrap().clone();
        let cleanup_tag = tag;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(45)).await;
            me.binary_waiters.lock().unwrap().remove(&cleanup_tag);
        });
        let frame = match tokio::time::timeout(Duration::from_secs(40), rx).await {
            Ok(Ok(f)) => f,
            _ => {
                self.binary_waiters.lock().unwrap().remove(&tag);
                return Err("no access-list reply — node out of range, asleep, or requires admin login".into());
            }
        };
        // Skip code(1), reserved(1), tag(4); remaining bytes are entries.
        let body = &frame[6.min(frame.len())..];
        let mut entries = Vec::new();
        let mut ofs = 0;
        while ofs + 7 <= body.len() {
            let prefix_bytes = &body[ofs..ofs + 6];
            if prefix_bytes.iter().all(|&b| b == 0) {
                break; // trailing zero padding of the fixed-size reply buffer
            }
            let prefix = hex_encode(prefix_bytes);
            let perms = body[ofs + 6];
            let name = self.contact_name(&prefix);
            entries.push(serde_json::json!({
                "pubkey_prefix": prefix,
                "name": name,
                "perms": perms,
                "role": if perms == 3 { "admin" } else if perms == 2 { "read/write" } else { "guest" },
            }));
            ofs += 7;
        }
        if entries.is_empty() {
            return Err("access list is empty or could not be parsed (admin login required)".into());
        }
        Ok(serde_json::json!(entries))
    }

    // ------------------------------------------------------------ connection loop

    async fn run_loop(&self) {
        let mut backoff = 2u64;
        loop {
            let detail = match &self.bluetooth {
                Some((mac, dev)) => {
                    if mac.is_empty() {
                        format!("connecting bluetooth {dev}")
                    } else {
                        format!("connecting bluetooth {mac} ({dev})")
                    }
                }
                None => format!("connecting tcp {}", self.host),
            };
            self.set_detail(&detail);
            match self.connect_session().await {
                Ok(()) => {
                    backoff = 2;
                    eprintln!("mesh radio session ended");
                }
                Err(e) => {
                    eprintln!("mesh radio ({}) error: {e}; retry in {backoff}s", self.host);
                }
            }
            self.connected.store(false, Ordering::Relaxed);
            self.set_detail("disconnected");
            self.cmd_tx.lock().unwrap().take();
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    }

    async fn connect_session(&self) -> Result<(), String> {
        // Bluetooth SPP when MESH_RADIO_BLUETOOTH is set: optionally bind
        // the paired device to an rfcomm node (requires bluez-utils; the
        // bind needs udev/permissions for non-root), then open it as a
        // serial port. Serial-wired when the host looks like a device
        // path (e.g. /dev/ttyACM0), TCP otherwise (host[:port], default 4000).
        let (read_half, write_half): (
            Box<dyn AsyncRead + Unpin + Send>,
            Box<dyn AsyncWrite + Unpin + Send>,
        ) = if let Some((mac, dev)) = &self.bluetooth {
            if !mac.is_empty() && !std::path::Path::new(dev).exists() {
                let idx = dev
                    .rsplit_once("rfcomm")
                    .and_then(|(_, n)| n.parse::<u8>().ok())
                    .unwrap_or(0);
                // Best effort; if this fails (permissions, already bound)
                // we fall through and try opening the device anyway.
                let _ = std::process::Command::new("rfcomm")
                    .args(["bind", &idx.to_string(), mac])
                    .status();
                // Give BlueZ a moment to create the node.
                for _ in 0..20 {
                    if std::path::Path::new(dev).exists() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            let builder = tokio_serial::new(dev, 115_200);
            let stream: SerialStream = SerialStream::open(&builder).map_err(|e| e.to_string())?;
            let (r, w) = tokio::io::split(stream);
            (Box::new(r) as Box<dyn AsyncRead + Unpin + Send>, Box::new(w) as Box<dyn AsyncWrite + Unpin + Send>)
        } else if self.host.starts_with('/') {
            let builder = tokio_serial::new(&self.host, 115_200);
            let stream: SerialStream = SerialStream::open(&builder).map_err(|e| e.to_string())?;
            let (r, w) = tokio::io::split(stream);
            (Box::new(r), Box::new(w))
        } else {
            let (host, port) = split_host_port(&self.host, 4000);
            let stream = TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| e.to_string())?;
            let (r, w) = stream.into_split();
            (Box::new(r), Box::new(w))
        };
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        *self.cmd_tx.lock().unwrap() = Some(cmd_tx);

        // Reader must run before the handshake (its response arrives as a
        // pushed frame), and so must the writer pump (the handshake itself
        // is sent through the command channel).
        let me = radio().unwrap().clone();
        let mut reader = tokio::spawn({
            let me2 = me.clone();
            async move { me2.read_loop(read_half).await }
        });
        let mut writer = tokio::spawn(async move {
            let mut write_half = write_half;
            let mut cmd_rx = cmd_rx;
            while let Some(payload) = cmd_rx.recv().await {
                if write_half.write_all(&frame_request(&payload)).await.is_err() {
                    return;
                }
            }
        });

        // Handshake: APP_START -> SELF_INFO.
        let appstart: &[u8] = b"\x01\x03      rust  ";
        let info = match self
            .command(appstart, &[R_SELF_INFO], Duration::from_secs(10))
            .await
        {
            Ok(info) => info,
            Err(e) => {
                reader.abort();
                writer.abort();
                return Err(format!("no companion response (app start): {e}"));
            }
        };
        self.parse_self_info(&info);

        // Clock sync (non-fatal).
        let ts = chrono::Utc::now().timestamp().max(0) as u32;
        let mut set_time = vec![0x06u8];
        set_time.extend_from_slice(&ts.to_le_bytes());
        let _ = self.command(&set_time, &[R_OK], Duration::from_secs(5)).await;

        self.connected.store(true, Ordering::Relaxed);
        self.set_detail("connected");
        let name = self.self_name.lock().unwrap().clone();
        eprintln!(
            "mesh radio connected: {name} ({}) via {}",
            self.self_pubkey.lock().unwrap().chars().take(12).collect::<String>(),
            self.host
        );

        // Refresh channel names and the contact directory, then drain any
        // messages that arrived while we were offline.
        {
            let me = me.clone();
            tokio::spawn(async move {
                me.refresh_channels().await;
                let _ = me.fetch_contacts().await;
                if let Some(b) = broker::broker() {
                    b.sync_connected();
                }
                me.drain_messages().await;
            });
        }

        // Supervise: poll for unpushed messages every 5s; exit if either
        // half of the connection dies.
        loop {
            tokio::select! {
                _ = &mut writer => {
                    reader.abort();
                    return Err("write half closed".into());
                }
                _ = &mut reader => {
                    writer.abort();
                    return Err("read half closed".into());
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // Periodic poll: pick up messages the radio did not
                    // push (some firmware versions only push while an app
                    // is attached and are conservative about it).
                    let me2 = me.clone();
                    tokio::spawn(async move { me2.drain_messages().await; });
                }
            }
        }
    }

    /// Owns the read half of the TCP connection; parses framed payloads
    /// and dispatches them. Returns when the connection dies.
    async fn read_loop(self: Arc<Self>, mut read_half: Box<dyn AsyncRead + Unpin + Send>) {
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        let mut header: Vec<u8> = Vec::with_capacity(3);
        let mut frame: Vec<u8> = Vec::new();
        let mut expected = 0usize;
        loop {
            let mut chunk = [0u8; 512];
            let n = match read_half.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            // Parse as many complete frames as available.
            loop {
                if header.len() < 3 {
                    if header.is_empty() {
                        let Some(idx) = buf.iter().position(|&b| b == 0x3e) else {
                            buf.clear();
                            break;
                        };
                        buf.drain(..idx);
                        if buf.is_empty() {
                            break;
                        }
                    }
                    let take = (3 - header.len()).min(buf.len());
                    let drained: Vec<u8> = buf.drain(..take).collect();
                    header.extend_from_slice(&drained);
                    if header.len() < 3 {
                        break;
                    }
                    expected = u16::from_le_bytes([header[1], header[2]]) as usize;
                    if expected == 0 || expected > MAX_FRAME {
                        header.clear();
                        frame.clear();
                        expected = 0;
                        continue;
                    }
                }
                let need = expected - frame.len();
                if buf.is_empty() {
                    break;
                }
                let take = need.min(buf.len());
                let drained: Vec<u8> = buf.drain(..take).collect();
                frame.extend_from_slice(&drained);
                if frame.len() == expected {
                    let payload = std::mem::take(&mut frame);
                    header.clear();
                    expected = 0;
                    self.handle_payload(self.clone(), payload).await;
                }
            }
        }
    }

    async fn handle_payload(&self, me: Arc<RadioClient>, data: Vec<u8>) {
        let Some(&code) = data.first() else { return };
        let body = &data[1..];
        match code {
            R_CONTACT_MSG | 16 => {
                let is_v3 = code == 16;
                let mut o = 0usize;
                if is_v3 {
                    o += 3; // snr + reserved
                }
                if body.len() < o + 12 {
                    return;
                }
                let prefix = hex_encode(&body[o..o + 6]);
                o += 6;
                o += 1; // path len
                if body.len() < o + 5 {
                    return;
                }
                let txt_type = body[o];
                o += 1;
                o += 4; // sender timestamp
                if txt_type == 2 {
                    o += 4; // signature
                }
                let text = String::from_utf8_lossy(&body[o..]).to_string();
                if txt_type == 1 {
                    // CLI-type reply from remote admin: hand to any pending
                    // console command, never ingest as chat traffic.
                    if let Some(tx) = self.cli_waiters.lock().unwrap().remove(&prefix) {
                        let _ = tx.send(text);
                    }
                    return;
                }
                let text = text.trim().to_string();
                if text.is_empty() {
                    return;
                }
                self.resolve(code, data.clone());
                self.ingest_dm(me, &prefix, &text).await;
            }
            R_CHANNEL_MSG | 17 => {
                let is_v3 = code == 17;
                let mut o = 0usize;
                if is_v3 {
                    o += 3;
                }
                if body.len() < o + 7 {
                    return;
                }
                let chan_idx = body[o];
                o += 1;
                o += 1; // path len
                let txt_type = body[o];
                o += 1;
                o += 4; // sender timestamp
                let text = String::from_utf8_lossy(&body[o..])
                    .trim_end_matches('\0')
                    .trim()
                    .to_string();
                if text.is_empty() {
                    return;
                }
                self.resolve(code, data.clone());
                let _ = txt_type;
                self.ingest_chan(me, chan_idx, &text).await;
            }
            R_CHANNEL_DATA => {
                // [code][snr][res1][res2][chan_idx][path_len][dtype_lo][dtype_hi][len][payload]
                if body.len() >= 9 {
                    let chan_idx = body[3];
                    let data_type = u16::from_le_bytes([body[5], body[6]]);
                    let dlen = body[7] as usize;
                    if body.len() >= 8 + dlen && data_type == crate::media::MEDIA_DATA_TYPE {
                        crate::media::handle_data(chan_idx, &body[8..8 + dlen]);
                    }
                }
                self.resolve(code, data.clone());
            }
            R_NO_MORE_MSGS => {
                self.resolve(code, data.clone());
            }
            R_ADVERTISEMENT => {
                // A new node advertised; refresh the contact directory so
                // we can attribute its name. Debounced by simply not
                // waiting on the result.
                let me2 = me.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = me2.fetch_contacts().await;
                });
            }
            PUSH_CODE_NEW_ADVERT => {
                // Flood advert from a router: the frame carries a full
                // contact record (same layout as GET_CONTACTS entries).
                // Mirror it immediately so the directory fills from
                // adverts even if a full sync never returns these nodes.
                if let Some(contact) = parse_contact(body) {
                    self.contact_names
                        .lock()
                        .unwrap()
                        .insert(contact.pubkey.chars().take(12).collect(), contact.name.clone());
                    let c = json!({
                        "pubkey": contact.pubkey,
                        "name": contact.name,
                        "adv_type": contact.adv_type as i64,
                        "lat": contact.lat,
                        "lon": contact.lon,
                    });
                    tokio::spawn(async move {
                        if let Some(b) = broker::broker() {
                            let _ = b.mirror_directory_pub(Some(&json!([c]))).await;
                        }
                        crate::broker_server::broadcast_directory(&json!([c]));
                    });
                }
            }
            R_MESSAGES_WAITING => {
                let me2 = me.clone();
                tokio::spawn(async move { me2.drain_messages().await; });
            }
            R_ACK => {
                self.resolve(code, data.clone());
            }
            R_CONTACT_START => {
                self.resolve(code, data.clone());
            }
            R_CONTACT => {
                if let Some(contact) = parse_contact(body) {
                    self.contact_names
                        .lock()
                        .unwrap()
                        .insert(contact.pubkey.chars().take(12).collect(), contact.name.clone());
                    let collector = self.contact_collector.lock().unwrap().clone();
                    if let Some(tx) = collector {
                        let _ = tx.send(contact);
                    }
                }
            }
            R_CONTACT_END => {
                self.resolve(code, data.clone());
            }
            R_STATUS_RESPONSE => {
                // Binary status frame: code, reserved, pubkey(6), 52 status bytes.
                if body.len() >= 8 {
                    let prefix = hex_encode(&body[2..8]);
                    if let Some(tx) = self.status_waiters.lock().unwrap().remove(&prefix) {
                        let _ = tx.send(data.clone());
                    }
                }
            }
            R_BINARY_RESPONSE => {
                // Binary reply frame: code, reserved, tag(4), payload.
                if body.len() >= 5 {
                    // body excludes the code byte: [reserved, tag(4), payload..]
                    let tag = u32::from_le_bytes([body[1], body[2], body[3], body[4]]);
                    if let Some(tx) = self.binary_waiters.lock().unwrap().remove(&tag) {
                        let _ = tx.send(data.clone());
                    }
                }
            }
            R_LOGIN_SUCCESS | R_LOGIN_FAILED => {
                // Solicited login replies; the generic waiter map resolves them.
                self.resolve(code, data.clone());
            }
            _ => {
                // Generic response: hand to any waiter.
                self.resolve(code, data.clone());
            }
        }
    }

    async fn ingest_dm(&self, me: Arc<RadioClient>, prefix: &str, text: &str) {
        let Some(b) = broker::broker() else { return };
        let name = self.contact_name(prefix);
        let frame = json!({
            "type": "msg",
            "proto": "meshcore",
            "from": prefix,
            "name": name,
            "text": text,
            "id": fresh_frame_id(),
        });
        crate::broker_server::broadcast_msg(&frame);
        if let Err(e) = b.ingest_frame(frame).await {
            eprintln!("radio ingest error: {e}");
        }
        drop(me);
    }

    async fn ingest_chan(&self, me: Arc<RadioClient>, chan_idx: u8, text: &str) {
        // Media session envelopes (VE3:/VM1:/VI1:) are captured by the
        // reassembler and suppressed from the chat stream.
        if crate::media::handle_envelope(chan_idx, text) {
            drop(me);
            return;
        }
        let Some(b) = broker::broker() else { return };
        let frame = json!({
            "type": "msg",
            "proto": "meshcore",
            "from": format!("#chan{chan_idx}"),
            "name": self.channel_name(chan_idx),
            "text": text,
            "id": fresh_frame_id(),
        });
        crate::broker_server::broadcast_msg(&frame);
        if let Err(e) = b.ingest_frame(frame).await {
            eprintln!("radio ingest error: {e}");
        }
        drop(me);
    }

    // ------------------------------------------------------------ sync helpers

    async fn drain_messages(&self) {
        // CMD_SYNC_NEXT_MESSAGE until the radio says there are no more.
        for _ in 0..50 {
            match self
                .command(&[0x0a], &[R_CONTACT_MSG, 16, R_CHANNEL_MSG, 17, R_CHANNEL_DATA, R_NO_MORE_MSGS], Duration::from_secs(10))
                .await
            {
                Ok(data) => {
                    if data.first() == Some(&R_NO_MORE_MSGS) {
                        break;
                    }
                    // A message frame: it was also dispatched as an event
                    // by handle_payload, keep draining.
                }
                Err(_) => break,
            }
        }
    }

    async fn refresh_channels(&self) {
        for idx in 0..8u8 {
            let payload = [0x1fu8, idx];
            if let Ok(info) = self.command(&payload, &[R_CHANNEL_INFO], Duration::from_secs(5)).await {
                if info.first() == Some(&R_CHANNEL_INFO) && info.len() >= 2 {
                    let name_bytes = info.get(2..34.min(info.len())).unwrap_or(&[]);
                    let name = String::from_utf8_lossy(name_bytes)
                        .split('\0')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !name.is_empty() {
                        self.channel_names.lock().unwrap().insert(idx, name);
                    }
                }
            }
        }
    }

    /// Create/update a channel slot on the companion radio
    /// (CMD_SET_CHANNEL 0x20: idx, name[32] null-padded, secret[16]).
    /// `secret` must be the raw 16-byte channel key. Takes 1-2s on firmware.
    pub async fn set_channel(&self, idx: u8, name: &str, secret: [u8; 16]) -> Result<(), String> {
        let mut payload = vec![0x20u8, idx];
        let mut name_buf = [0u8; 32];
        let nb = name.as_bytes();
        let n = nb.len().min(32);
        name_buf[..n].copy_from_slice(&nb[..n]);
        payload.extend_from_slice(&name_buf);
        payload.extend_from_slice(&secret);
        self.command(&payload, &[R_OK], Duration::from_secs(10)).await.map(|_| ())?;
        self.refresh_channels().await;
        Ok(())
    }

    async fn fetch_contacts(&self) -> Result<(), String> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        *self.contact_collector.lock().unwrap() = Some(tx);
        let result = self.command(&[0x04], &[R_CONTACT_END], Duration::from_secs(30)).await;
        *self.contact_collector.lock().unwrap() = None;
        result.map(|_| ())?;

        // Mirror the directory into the console DB via the broker client.
        let mut contacts = Vec::new();
        while let Ok(c) = rx.try_recv() {
            contacts.push(json!({
                "pubkey": c.pubkey,
                "name": c.name,
                "adv_type": c.adv_type as i64,
                "lat": c.lat,
                "lon": c.lon,
            }));
        }
        if let Some(b) = broker::broker() {
            let b: &BrokerClient = b;
            if let Err(e) = b.mirror_directory_pub(Some(&json!(contacts))).await {
                eprintln!("radio directory mirror error: {e}");
            }
        }
        crate::broker_server::broadcast_directory(&json!(contacts));
        Ok(())
    }

    fn parse_self_info(&self, info: &[u8]) {
        // code(1) adv_type tx_power max_tx pubkey(32) lat lon multi_acks
        // adv_loc_policy telemetry manual freq(4) bw(4) sf cr name...
        let body = &info[1..];
        if body.len() < 45 {
            return;
        }
        let pubkey = hex_encode(&body[3..35]);
        *self.self_pubkey.lock().unwrap() = pubkey.clone();
        let name = String::from_utf8_lossy(if body.len() > 57 { &body[57..] } else { &body[44..] })
            .trim_matches('\0')
            .trim()
            .to_string();
        *self.self_name.lock().unwrap() = name.clone();
        let li32 = |o: usize| i32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
        let lu32 = |o: usize| u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
        let v = serde_json::json!({
            "name": name,
            "pubkey": pubkey,
            "adv_type": body[0],
            "tx_power": body[1],
            "max_tx_power": body[2],
            "lat": li32(35) as f64 / 1e6,
            "lon": li32(39) as f64 / 1e6,
            "multi_acks": body[43],
            "adv_loc_policy": if body.len() > 44 { body[44] } else { 0 },
            "telemetry_mode": if body.len() > 45 { body[45] } else { 0 },
            "manual_add_contacts": body.len() > 46 && body[46] > 0,
            "freq_mhz": if body.len() >= 55 { lu32(47) as f64 / 1000.0 } else { 0.0 },
            "bw_khz": if body.len() >= 59 { lu32(51) as f64 / 1000.0 } else { 0.0 },
            "sf": if body.len() > 59 { body[55] } else { 0 },
            "cr": if body.len() > 60 { body[56] } else { 0 },
        });
        *self.self_info.lock().unwrap() = Some(v);
    }

    /// The SELF_INFO captured at handshake (identity + radio params).
    pub fn self_info(&self) -> Option<serde_json::Value> {
        self.self_info.lock().unwrap().clone()
    }

    /// DEVICE_QUERY: firmware/board identity (CMD 0x16).
    pub async fn device_query(&self) -> Result<serde_json::Value, String> {
        let data = self
            .command(&[0x16, 0x03], &[R_DEVICE_INFO], Duration::from_secs(5))
            .await
            .map_err(|e| e.to_string())?;
        let b = &data[1..];
        if b.is_empty() {
            return Err("short DEVICE_INFO".into());
        }
        let fw_ver = b[0];
        let mut v = serde_json::json!({ "fw_ver": fw_ver });
        if fw_ver >= 3 && b.len() >= 78 {
            v["max_contacts"] = serde_json::json!(b[1] as i64 * 2);
            v["max_channels"] = serde_json::json!(b[2]);
            let s = |o: usize, n: usize| {
                String::from_utf8_lossy(&b[o..o + n])
                    .replace('\0', "")
                    .trim()
                    .to_string()
            };
            v["fw_build"] = serde_json::json!(s(7, 12));
            v["model"] = serde_json::json!(s(19, 40));
            v["fw_version"] = serde_json::json!(s(59, 20));
        }
        Ok(v)
    }

    pub async fn set_name(&self, name: &str) -> bool {
        let mut p = vec![0x08u8];
        p.extend_from_slice(name.as_bytes());
        self.command(&p, &[R_OK], Duration::from_secs(10)).await.is_ok()
    }

    pub async fn set_coords(&self, lat: f64, lon: f64) -> bool {
        let mut p = vec![0x0eu8];
        p.extend_from_slice(&((lat * 1e6) as i32).to_le_bytes());
        p.extend_from_slice(&((lon * 1e6) as i32).to_le_bytes());
        p.extend_from_slice(&0i32.to_le_bytes());
        self.command(&p, &[R_OK], Duration::from_secs(10)).await.is_ok()
    }

    pub async fn set_tx_power(&self, dbm: i64) -> bool {
        let mut p = vec![0x0cu8];
        p.extend_from_slice(&(dbm as u32).to_le_bytes());
        self.command(&p, &[R_OK], Duration::from_secs(10)).await.is_ok()
    }

    /// Toggle location sharing in adverts (adv_loc_policy 0=off, 1=on).
    pub async fn set_loc_policy(&self, share: bool) -> bool {
        let cur = self.self_info().unwrap_or_default();
        let manual = cur
            .get("manual_add_contacts")
            .and_then(|m| m.as_bool())
            .unwrap_or(false) as u8;
        let telemetry = cur
            .get("telemetry_mode")
            .and_then(|m| m.as_u64())
            .unwrap_or(0) as u8;
        let multi_acks = cur
            .get("multi_acks")
            .and_then(|m| m.as_u64())
            .unwrap_or(0) as u8;
        let p = [0x26u8, manual, telemetry, share as u8, multi_acks];
        self.command(&p, &[R_OK], Duration::from_secs(10)).await.is_ok()
    }

    pub async fn send_advert(&self, flood: bool) -> bool {
        let p = if flood { vec![0x07u8, 0x01] } else { vec![0x07u8] };
        self.command(&p, &[R_OK], Duration::from_secs(10)).await.is_ok()
    }

    pub async fn reboot(&self) -> bool {
        let mut p = vec![0x13u8];
        p.extend_from_slice(b"reboot");
        self.command(&p, &[R_OK], Duration::from_secs(10)).await.is_ok()
    }
}

fn parse_contact(body: &[u8]) -> Option<Contact> {
    // pubkey(32) type(1) flags(1) path_len(1) path(64) name(32)
    // last_advert(4) lat(4) lon(4) lastmod(4) = 147 bytes
    if body.len() < 147 {
        return None;
    }
    let pubkey = hex_encode(&body[..32]);
    let name = String::from_utf8_lossy(&body[99..131])
        .replace('\0', "")
        .trim()
        .to_string();
    let lat = i32::from_le_bytes([body[135], body[136], body[137], body[138]]) as f64 / 1e6;
    let lon = i32::from_le_bytes([body[139], body[140], body[141], body[142]]) as f64 / 1e6;
    let mut c = Contact {
        pubkey,
        name,
        adv_type: body[32],
        lat: Some(lat),
        lon: Some(lon),
    };
    if lat.abs() < 0.0001 && lon.abs() < 0.0001 {
        c.lat = None;
        c.lon = None;
    }
    Some(c)
}

fn frame_request(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 3);
    out.push(0x3c);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn split_host_port(host: &str, default_port: u16) -> (String, u16) {
    match host.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (host.to_string(), default_port),
    }
}

/// Parse a 60-byte binary STATUS_RESPONSE frame (code, reserved, pubkey(6),
/// then 52 status bytes starting at full-data offset 8).
fn parse_status_frame(data: &[u8]) -> Result<serde_json::Value, String> {
    if data.len() < 8 + 50 {
        return Err("short status frame".into());
    }
    let b = &data[8..];
    let lu16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as i64;
    let li16 = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]) as i64;
    let lu32 = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as i64;
    let bat_mv = lu16(0);
    let uptime = lu32(14);
    let airtime = lu32(16);
    let hms = |secs: i64| {
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        format!("{h}h {m}m {s}s")
    };
    let mut v = serde_json::json!({
        "battery_mv": bat_mv,
        "battery_pct": ((bat_mv as f64 - 3300.0) / (4200.0 - 3300.0) * 100.0).clamp(0.0, 100.0) as i64,
        "tx_queue_len": lu16(2),
        "noise_floor": li16(4),
        "last_rssi": li16(6),
        "nb_recv": lu32(8),
        "nb_sent": lu32(10),
        "airtime": airtime,
        "airtime_hms": hms(airtime),
        "uptime": uptime,
        "uptime_hms": hms(uptime),
        "airtime_pct": if uptime > 0 { airtime as f64 * 100.0 / uptime as f64 } else { 0.0 },
        "sent_flood": lu32(20),
        "sent_direct": lu32(24),
        "recv_flood": lu32(28),
        "recv_direct": lu32(32),
        "full_evts": lu16(36),
        "full_evts_hex": format!("{:04x}", lu16(36)),
        "last_snr": li16(38) as f64 / 4.0,
        "direct_dups": lu16(40),
        "flood_dups": lu16(42),
        "rx_airtime": lu32(44),
        "recv_errors": if b.len() >= 56 { lu32(48) } else { -1 },
    });
    v["duplicates"] = serde_json::json!(lu16(40) + lu16(42));
    Ok(v)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// ULID-shaped frame id: 10 Crockford-base32 timestamp chars + random, so
/// broker::frame_age_secs can compute an age and the frame ledger dedups.
fn fresh_frame_id() -> String {
    const ENC: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let mut value = ms;
    let mut ts = [0u8; 10];
    for i in (0..10).rev() {
        ts[i] = ENC[(value & 31) as usize];
        value >>= 5;
    }
    let rnd = uuid::Uuid::new_v4().simple().to_string();
    format!("{}{}", String::from_utf8_lossy(&ts), &rnd[..16]).to_uppercase()
}
