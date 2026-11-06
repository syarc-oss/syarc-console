//! MediaMesh clip capture: passively reassembles Open TAK media sessions
//! (VE3/VM1 voice, VI1 photo) that ride channel data frames of type 0x5643
//! over radio2, and stores the completed bitstreams for download.
//!
//! Wire format (from Open TAK voice_wire.dart):
//!   data packet: [0x56|0x57|0x58][sid:4 BE][index:1][payload...]
//!     0x57: one trailing salt byte (dropped)
//!     0x58: trailing byte = (salt<<6) | min(left,63); left==0 => complete
//!   session announce (channel TEXT): `VE3:{sid}:{mode}:{total}:{dur}`
//!     all numbers base36, sid = 32-bit session id; caption after whitespace.
//!     VM1: = Mimi voice, VI1: = AV1 photo (dur = aspect w*100/h).
//!   codec modes: 0-7 = Codec2 (700C..3200), 32 = Mimi, 40 = AV1 image.
//!   0x72 = binary fetch request (ignored by this passive listener).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use sqlx::Row;

use syarc_plugin_api::{Db, RequireUser, SharedState};

pub const MEDIA_DATA_TYPE: u16 = 0x5643;
const DEFAULT_MEDIA_DIR: &str = "/usr/local/syarc/media";

#[derive(Debug, Clone)]
struct Session {
    kind: String, // voice | photo
    mode: u32,
    total: u32,
    dur: u32,
    caption: String,
    chan_idx: u8,
    /// Every distinct copy received per packet index. Open TAK senders may
    /// packetize the same clip twice (92B initial burst, 156B fetch answers);
    /// reassembly must pick one consistent length class (see `assemble`).
    parts: HashMap<u32, Vec<Vec<u8>>>,
    done: bool,
    /// Byte length of the buffer actually stored at the last completion.
    /// Late better-class copies reopen the session only if they would
    /// assemble a LONGER buffer (phone dual-packetization sends the good
    /// 156B-class copies after the 92B first burst).
    stored_len: usize,
    last_seen: std::time::Instant,
}

// Open TAK clips may never receive their VE3/VM1 envelope or an 0x58
// terminator; finalize a contiguous run after this much idle time.
const IDLE_COMPLETE_SECS: u64 = 4;
// Envelope-less sessions (announce missed/suppressed) can only be finalized
// by the contiguous-run fallback. Wait longer than the fetch-answer cycle
// (~10 s) so pull-mode senders' 0x72-served packets are included before we
// commit to a possibly-truncated run.
const ENVELOPE_LESS_IDLE_SECS: u64 = 15;
// Done sessions stay in memory so late, better packet copies can reopen and
// improve them; drop them after this much idle (memory bound).
const DONE_EXPIRY_SECS: u64 = 600;

/// Reassemble a stream from per-index packet copies, picking ONE consistent
/// chunk-length class (longest contiguous run from index 0, plus at most one
/// shorter tail copy from another class at the run boundary). Mixing chunk
/// sizes mid-stream garbles codec2. Returns (run, buffer) or None.
fn assemble(parts: &HashMap<u32, Vec<Vec<u8>>>) -> Option<(u32, Vec<u8>)> {
    let mut classes: HashMap<usize, HashMap<u32, Vec<u8>>> = HashMap::new();
    for (idx, copies) in parts {
        for c in copies {
            if c.is_empty() {
                continue;
            }
            classes.entry(c.len()).or_default().insert(*idx, c.clone());
        }
    }
    let mut best: Option<(u32, Vec<u8>)> = None;
    for (len, group) in &classes {
        let mut run: u32 = 0;
        while group.contains_key(&run) {
            run += 1;
        }
        if run == 0 {
            continue;
        }
        let (run, buf) = if !group.contains_key(&run) {
            let tail = parts
                .get(&run)
                .and_then(|copies| copies.iter().find(|c| c.len() < *len))
                .cloned();
            let mut b: Vec<u8> = Vec::with_capacity(run as usize * len);
            for i in 0..run {
                b.extend_from_slice(&group[&i]);
            }
            match tail {
                Some(t) => {
                    b.extend_from_slice(&t);
                    (run + 1, b)
                }
                None => (run, b),
            }
        } else {
            (
                run,
                (0..run)
                    .flat_map(|i| group[&i].iter().copied())
                    .collect::<Vec<u8>>(),
            )
        };
        let better = match &best {
            None => true,
            Some((brun, bbuf)) => run > *brun || (run == *brun && buf.len() > bbuf.len()),
        };
        if better {
            best = Some((run, buf));
        }
    }
    best
}

static SESSIONS: OnceLock<Mutex<HashMap<u32, Session>>> = OnceLock::new();
static DBH: OnceLock<Db> = OnceLock::new();
static MEDIA_DIR: OnceLock<String> = OnceLock::new();

pub async fn init(db: &Db) {
    let _ = DBH.set(db.clone());
    let dir = sqlx::query_scalar::<_, String>(
        "SELECT value FROM settings WHERE key = 'mesh_media_dir'",
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .filter(|s| !s.trim().is_empty())
    .unwrap_or_else(|| DEFAULT_MEDIA_DIR.to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("mesh media: cannot create dir {dir}: {e}");
    }
    let _ = MEDIA_DIR.set(dir);
    tokio::spawn(async {
        let mut last_prune = std::time::Instant::now() - std::time::Duration::from_secs(3600);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            sweep_idle().await;
            // Self-heal companion suppression: the completion-time sweep can
            // be dodged by task ordering/races between two clips finishing
            // concurrently; re-running it here catches any stragglers.
            if let Some(db) = DBH.get() {
                suppress_companions(db).await;
                // Channel-message retention (setting mesh_retention_days,
                // 0/absent = keep forever). Checked hourly.
                if last_prune.elapsed().as_secs() >= 3600 {
                    last_prune = std::time::Instant::now();
                    prune_traffic(db).await;
                }
            }
        }
    });
}

/// Delete mesh_traffic rows older than mesh_retention_days (0 = keep forever).
async fn prune_traffic(db: &Db) {
    let days: i64 = sqlx::query_scalar(
        "SELECT CAST(value AS INTEGER) FROM settings WHERE key = 'mesh_retention_days'",
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .unwrap_or(0);
    if days <= 0 {
        return;
    }
    if let Ok(n) = sqlx::query(
        "DELETE FROM mesh_traffic WHERE created_at < datetime('now', ?)",
    )
    .bind(format!("-{days} days"))
    .execute(db)
    .await
    {
        if n.rows_affected() > 0 {
            eprintln!("mesh retention: pruned {} rows older than {days}d", n.rows_affected());
        }
    }
}

async fn sweep_idle() {
    let mut finals: Vec<(u32, Session)> = Vec::new();
    let mut echo_cand: Vec<(u32, Session)> = Vec::new();
    {
        let mut map = sessions().lock().unwrap();
        let idle: Vec<u32> = map
            .iter()
            .filter(|(_, s)| {
                !s.done && s.last_seen.elapsed().as_secs() >= IDLE_COMPLETE_SECS
            })
            .map(|(sid, _)| *sid)
            .collect();
        let expired: Vec<u32> = map
            .iter()
            .filter(|(_, s)| {
                s.done && s.last_seen.elapsed().as_secs() > DONE_EXPIRY_SECS
            })
            .map(|(sid, _)| *sid)
            .collect();
        for sid in expired {
            map.remove(&sid);
        }
        for sid in idle {
            if let Some(s) = map.get_mut(&sid) {
                // Echo-channel heal: packets for an ALREADY-completed clip
                // arriving on another channel (possibly after a restart
                // cleared the in-memory done entry) can never finalize a
                // full session on their own — just record the second
                // channel and drop the stub. DB check happens outside the
                // lock; candidates without a complete row are reinserted.
                if s.total == 0 {
                    echo_cand.push((sid, s.clone()));
                    continue;
                }
                let run = assemble(&s.parts).map(|(n, _)| n).unwrap_or(0);
                // Dis-trust announce totals: some senders announce only the
                // first burst (total=2 of 5 real packets). Grow the total to
                // the observed run so the finalize check uses reality.
                if run > s.total {
                    s.total = run;
                }
                // Only finalize when the contiguous run actually covers the
                // announced total; completing a short run would silently
                // truncate the clip and mark it playable (corrupt output).
                if run > 0 && (s.total == 0 || run >= s.total) {
                    if s.total == 0 {
                        s.total = run;
                    }
                    s.done = true;
                    s.stored_len = assemble(&s.parts).map(|(_, b)| b.len()).unwrap_or(0);
                    finals.push((sid, s.clone()));
                } else if run > 0 && s.total > 0 && s.parts.len() >= s.total as usize {
                    eprintln!(
                        "mesh media: session {sid:08x} has {} parts (total {}) but contiguous run only {run} — missing: {:?}",
                        s.parts.len(),
                        s.total,
                        (0..s.total).filter(|i| !s.parts.contains_key(i)).collect::<Vec<_>>()
                    );
                }
            }
        }
        // Keep finalized sessions in the map (done=true): late better
        // copies may reopen and improve them (DONE_EXPIRY_SECS prunes).
    }
    if let Some(db) = DBH.get() {
        for (sid, s) in echo_cand {
            let sid_hex = format!("{sid:08x}");
            let chan = s.chan_idx as i64;
            let hit = sqlx::query(
                "UPDATE mesh_media SET chan_idx2 = ? WHERE sid = ? \
                 AND status = 'complete' AND chan_idx != ? \
                 AND (chan_idx2 IS NULL OR chan_idx2 != ?)",
            )
            .bind(chan)
            .bind(&sid_hex)
            .bind(chan)
            .bind(chan)
            .execute(db)
            .await
            .map(|r| r.rows_affected() > 0)
            .unwrap_or(false);
            if hit {
                eprintln!("mesh media: echo of {sid_hex} on chan {chan} recorded");
            } else if s.last_seen.elapsed().as_secs() >= ENVELOPE_LESS_IDLE_SECS {
                // No announce ever arrived and no completed clip exists —
                // this is a real envelope-less session, not an echo stub.
                // Finalize the contiguous run (total unknown; best effort —
                // companion suppression hides it if a fuller clip lands).
                let mut sess = s.clone();
                if let Some((run, _)) = assemble(&sess.parts) {
                    sess.total = run;
                    sess.done = true;
                    sess.stored_len = assemble(&sess.parts).map(|(_, b)| b.len()).unwrap_or(0);
                    eprintln!(
                        "mesh media: session {sid_hex} complete via envelope-less idle sweep ({} parts)",
                        sess.parts.len()
                    );
                    store_complete(&sid_hex, &sess).await;
                    // Mark done in-memory so the sweep stops re-examining;
                    // late better copies can still reopen it via handle_data.
                    if let Some(e) = sessions().lock().unwrap().get_mut(&sid) {
                        e.done = true;
                        e.total = sess.total;
                        e.stored_len = sess.stored_len;
                    }
                }
            } else {
                // No completed clip yet — keep the session for the normal
                // finalize path (envelope may still arrive).
                sessions().lock().unwrap().insert(sid, s);
            }
        }
    }
    for (sid, sess) in finals {
        let sid_hex = format!("{sid:08x}");
        eprintln!("mesh media: session {sid_hex} complete via idle sweep ({} parts)", sess.parts.len());
        store_complete(&sid_hex, &sess).await;
    }
}

fn sessions() -> &'static Mutex<HashMap<u32, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Live in-memory session table for diagnosing stuck captures.
async fn sessions_debug(_user: RequireUser) -> axum::Json<serde_json::Value> {
    let map = sessions().lock().unwrap();
    let out: Vec<serde_json::Value> = map
        .iter()
        .map(|(sid, s)| {
            let mut idx: Vec<u32> = s.parts.keys().copied().collect();
            idx.sort_unstable();
            json!({
                "sid": format!("{sid:08x}"),
                "kind": s.kind,
                "mode": s.mode,
                "total": s.total,
                "done": s.done,
                "parts": s.parts.len(),
                "missing": (0..s.total.max(1)).filter(|i| !s.parts.contains_key(i)).collect::<Vec<_>>(),
                "last_seen_secs_ago": s.last_seen.elapsed().as_secs(),
            })
        })
        .collect();
    axum::Json(json!({ "sessions": out }))
}

fn media_dir() -> &'static str {
    MEDIA_DIR.get().map(|s| s.as_str()).unwrap_or(DEFAULT_MEDIA_DIR)
}

fn mode_ext(mode: u32) -> &'static str {
    match mode {
        40 => "av1",
        32 => "mimi",
        _ => "c2",
    }
}

fn mode_name(mode: u32) -> &'static str {
    match mode {
        40 => "AV1 image",
        32 => "Mimi 24kHz",
        0 => "Codec2 700C",
        1 => "Codec2 1200",
        2 => "Codec2 2400",
        3 => "Codec2 1300",
        4 => "Codec2 1400",
        5 => "Codec2 1600",
        6 => "Codec2 3200",
        _ => "unknown",
    }
}

/// Bare codec2 mode token accepted by ffmpeg's `-mode` option.
/// Open TAK mode ids (voice_message_parser.dart VoicePacketMode enum):
/// 0=700C 1=1200 2=2400 3=1300 4=1400 5=1600 6=3200.
/// "1300" is Open TAK's label for a 1400-class stream; ffmpeg has no
/// 1300 mode token, so decode it as 1400.
fn c2_mode(mode: u32) -> &'static str {
    match mode {
        0 => "700C",
        1 => "1200",
        2 => "2400",
        3 => "1400",
        4 => "1400",
        5 => "1600",
        6 => "3200",
        _ => "700C",
    }
}

fn b36(s: &str) -> Option<u32> {
    u32::from_str_radix(s, 36).ok()
}

/// Parse a channel text message as a media session announcement.
/// Returns (sid, session fields) when the text is an envelope. Channel text
/// arrives as "<sender>: <body>" — the envelope may hide behind the prefix.
fn parse_envelope(raw: &str) -> Option<(u32, &str, u32, u32, u32, String)> {
    let is_env = |s: &str| {
        s.starts_with("VE3:") || s.starts_with("VM1:") || s.starts_with("VI1:")
    };
    // Channel text arrives as "<sender>: <body>" and den forwarders chain up
    // to 3 prefixes ("Bear Den2: Bear Den: ...: VE3:...") — peel them all.
    let mut text = raw;
    for _ in 0..3 {
        if is_env(text) {
            break;
        }
        if let Some((name, rest)) = text.split_once(": ") {
            if !name.is_empty()
                && name.len() <= 24
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == ' ' || c == '_' || c == '-')
            {
                text = rest;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    let (verb, rest) = text.split_once(':')?;
    let kind = match verb {
        "VE3" | "VM1" => "voice",
        "VI1" => "photo",
        _ => return None,
    };
    let mut fields = rest.splitn(5, char::is_whitespace);
    let spec = fields.next()?.trim();
    let caption = fields.next().unwrap_or("").trim().to_string();
    let mut it = spec.split(':');
    let sid = b36(it.next()?)?;
    let mode = b36(it.next().unwrap_or(""))?;
    let total = b36(it.next().unwrap_or(""))?;
    let dur = b36(it.next().unwrap_or(""))?;
    if it.next().is_some() {
        return None;
    }
    if sid == 0 || total == 0 || total > 600 || mode > 63 {
        return None;
    }
    Some((sid, kind, mode, total, dur, caption))
}

/// Called for every channel text message. Returns true when the text was a
/// media envelope (and should be suppressed from the chat stream).
pub fn handle_envelope(chan_idx: u8, text: &str) -> bool {
    let Some((sid, kind, mode, total, dur, caption)) = parse_envelope(text) else {
        return false;
    };
    let mut map = sessions().lock().unwrap();
    let entry = map.entry(sid).or_insert_with(|| Session {
        kind: kind.to_string(),
        mode,
        total,
        dur,
        caption: caption.clone(),
        chan_idx,
        parts: HashMap::new(),
        done: false,
        stored_len: 0,
        last_seen: std::time::Instant::now(),
    });
    let mut finalize: Option<Session> = None;
    if !entry.done {
        // A resend/refresh of the announcement may carry a longer caption.
        if caption.len() > entry.caption.len() {
            entry.caption = caption.clone();
        }
        // The announce can arrive AFTER the last data packet (bridge
        // pacing), so re-check completeness here too.
        let s = &mut *entry;
        let run = assemble(&s.parts).map(|(n, _)| n).unwrap_or(0);
        if s.total > 0 && run >= s.total && run > 0
        {
            s.done = true;
            finalize = Some(s.clone());
        }
    }
    let snapshot = entry.clone();
    drop(map);
    let sid_hex = format!("{sid:08x}");
    if let Some(sess) = finalize {
        eprintln!("mesh media: session {sid_hex} complete at announce ({} parts)", sess.parts.len());
        tokio::spawn(async move {
            store_complete(&sid_hex, &sess).await;
        });
        return true;
    }
    eprintln!(
        "mesh media: session {sid_hex} announce {kind} mode {mode} total {total} ({}/{total})",
        snapshot.parts.len()
    );
    tokio::spawn(async move {
        let Some(db) = DBH.get() else { return };
        let _ = sqlx::query(
            "INSERT INTO mesh_media (sid, kind, mode, total, received, status, caption, chan_idx, dur, created_at) \
             VALUES (?, ?, ?, ?, ?, 'partial', ?, ?, ?, datetime('now')) \
             ON CONFLICT(sid) DO UPDATE SET kind=excluded.kind, mode=excluded.mode, \
             total=excluded.total, dur=excluded.dur, \
             caption=CASE WHEN length(excluded.caption) > length(mesh_media.caption) THEN excluded.caption ELSE mesh_media.caption END",
        )
        .bind(&sid_hex)
        .bind(&snapshot.kind)
        .bind(snapshot.mode as i64)
        .bind(snapshot.total as i64)
        .bind(snapshot.parts.len() as i64)
        .bind(&snapshot.caption)
        .bind(snapshot.chan_idx as i64)
        .bind(snapshot.dur as i64)
        .execute(db)
        .await;
    });
    true
}

/// Called for every channel data frame of type 0x5643. Parses VE3-style
/// data packets and finalizes the session once all parts have arrived.
pub fn handle_data(chan_idx: u8, payload: &[u8]) {
    let Some(&b0) = payload.first() else { return };
    if payload.len() < 6 {
        return;
    }
    let sid = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let index = payload[5] as u32;
    let (part, left): (Vec<u8>, Option<u32>) = match b0 {
        0x56 => (payload[6..].to_vec(), None),
        0x57 => {
            if payload.len() < 7 {
                return;
            }
            (payload[6..payload.len() - 1].to_vec(), None)
        }
        0x58 => {
            if payload.len() < 7 {
                return;
            }
            let trailing = *payload.last().unwrap();
            (payload[6..payload.len() - 1].to_vec(), Some((trailing & 0x3f) as u32))
        }
        _ => return, // 0x72 fetch requests: not for a passive listener
    };
    if sid == 0 || index > 599 || part.len() > 200 {
        return;
    }

    let finalize = {
        let mut map = sessions().lock().unwrap();
        let entry = map.entry(sid).or_insert_with(|| Session {
            kind: "voice".into(),
            mode: 1, // Open TAK id 1 = codec2 1200; bridge sends 1200
            total: 0, // envelope not seen yet; completion decided below
            dur: 0,
            caption: String::new(),
            chan_idx,
            parts: HashMap::new(),
            done: false,
            stored_len: 0,
            last_seen: std::time::Instant::now(),
        });
        if entry.done {
            // Late copies of ALREADY-HELD indices can still improve the clip:
            // dual-packetizing senders emit the good 156B-class copies after
            // the 92B first burst, possibly after we finalized. Accept the
            // copy; reopen only when it assembles a longer buffer than what
            // was stored (store_complete never clobbers a larger clip).
            {
                let mut improved = false;
                {
                    let copies = entry.parts.entry(index).or_default();
                    if !copies.iter().any(|c| c == &part) {
                        copies.push(part.clone());
                        improved = true;
                    }
                }
                if improved {
                    let buflen = assemble(&entry.parts).map(|(_, b)| b.len()).unwrap_or(0);
                    if buflen > entry.stored_len {
                        eprintln!(
                            "mesh media: reopening done session {sid:08x} - better copy for idx {index} ({} B > {} B)",
                            buflen, entry.stored_len
                        );
                        entry.done = false;
                    } else {
                        entry.parts.get_mut(&index).map(|c| c.pop());
                    }
                }
            }
            if entry.done {
            // A packet index BEYOND everything we know (announced total or
            // received run) means the announce total was just the first
            // burst — reopen so the larger completion can replace the short
            // one (store_complete never clobbers a larger stored clip).
            let extent = entry
                .total
                .max(assemble(&entry.parts).map(|(n, _)| n).unwrap_or(0));
            if index < extent {
            // The clip already completed, but packets echoing in on a
            // DIFFERENT channel mean the clip is also present in that
            // channel's room - record it so the room timeline shows it.
            if chan_idx != entry.chan_idx {
                let sid_hex2 = format!("{sid:08x}");
                tokio::spawn(async move {
                    let Some(db) = DBH.get() else { return };
                    let _ = sqlx::query(
                        "UPDATE mesh_media SET chan_idx2 = ? WHERE sid = ? \
                         AND status = 'complete' AND chan_idx != ? \
                         AND (chan_idx2 IS NULL OR chan_idx2 != ?)",
                    )
                    .bind(chan_idx as i64)
                    .bind(&sid_hex2)
                    .bind(chan_idx as i64)
                    .bind(chan_idx as i64)
                    .execute(db)
                    .await;
                });
            }
            return;
            } // index < extent
            eprintln!(
                "mesh media: reopening done session {sid:08x} - packet {index} extends beyond extent {extent}"
            );
            entry.done = false;
            }
        }
        entry.chan_idx = chan_idx;
        {
            let copies = entry.parts.entry(index).or_default();
            if !copies.iter().any(|c| c == &part) {
                copies.push(part.clone());
            }
        }
        entry.last_seen = std::time::Instant::now();
        let s = &mut *entry;
        let run = assemble(&s.parts).map(|(n, _)| n).unwrap_or(0);
        if run > s.total {
            s.total = run;
        }
        let all = s.total > 0 && run > 0 && run >= s.total;
        let left_done = left == Some(0)
            && run > 0
            && {
                if s.total == 0 {
                    s.total = run;
                }
                run >= s.total && s.total > 0
            };
        if all || left_done {
            entry.done = true;
            entry.stored_len = assemble(&entry.parts).map(|(_, b)| b.len()).unwrap_or(0);
            Some(entry.clone())
        } else {
            None
        }
    };

    let sid_hex = format!("{sid:08x}");
    if let Some(sess) = finalize {
        eprintln!("mesh media: session {sid_hex} complete ({} parts)", sess.parts.len());
        tokio::spawn(async move {
            store_complete(&sid_hex, &sess).await;
        });
    } else {
        tokio::spawn(async move {
            let Some(db) = DBH.get() else { return };
            let _ = sqlx::query(
                "INSERT INTO mesh_media (sid, kind, status, chan_idx, received, created_at) \
                 VALUES (?, 'voice', 'partial', ?, 1, datetime('now')) \
                 ON CONFLICT(sid) DO UPDATE SET received = \
                 CASE WHEN mesh_media.total > 0 THEN MIN(mesh_media.received + 1, mesh_media.total) \
                      ELSE MIN(mesh_media.received + 1, 599) END \
                 WHERE mesh_media.status != 'deleted'",
            )
            .bind(&sid_hex)
            .bind(chan_idx as i64)
            .execute(db)
            .await;
        });
    }
}

/// Is this stored voice clip actually noise? The phone emits "companion"
/// sids alongside real clips whose payloads decode to clipped static; they
/// render in the room timeline as playable noise. Decode via ffmpeg and
/// check the flat factor (speech < 5, junk > 15 in practice). None = probe
/// unavailable/failed; keep the clip in that case.
fn voice_clip_is_noise(path: &std::path::Path, mode: u32) -> Option<bool> {
    // Two views: the clip HEAD (1.5 s — junk companion sessions are static
    // from the first samples) and the WHOLE file (some junk twins are quiet
    // up front with a static tail; measured 17 whole vs 4.4 head). A real
    // message can end with a burst of clipping noise, so the whole-file
    // threshold is looser than the head one. None = probe failed; keep.
    let run = |dur: &str| -> Option<f64> {
        let out = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel", "info", "-f", "codec2raw", "-mode", c2_mode(mode),
                "-i",
            ])
            .arg(path)
            .args(["-t", dur, "-af", "astats=metadata=1:reset=0", "-f", "null", "-"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let val = stderr
            .lines()
            .find_map(|l| l.split_once("Flat factor:").map(|(_, v)| v.trim().to_string()))?;
        val.parse().ok()
    };
    let head = run("1.5")?;
    if head > 8.0 {
        return Some(true);
    }
    let whole = run("3600")?;
    Some(whole > 12.0)
}

/// Companion suppression: Open TAK phones emit a second, much smaller
/// "companion" session alongside the real clip (same start second). The
/// stub decodes as garbled audio but can pass the flat-factor check.
/// If a same-channel voice clip that started within seconds of it
/// completed at >= 2x the size, hide the stub outright.
/// Called both at completion time and from the 2s idle loop so no
/// completion-ordering race can leave a stub playable.
async fn suppress_companions(db: &sqlx::SqlitePool) -> u64 {
    match sqlx::query(
        "UPDATE mesh_media SET status = 'error' WHERE kind = 'voice' AND status = 'complete' AND EXISTS ( \
           SELECT 1 FROM mesh_media o WHERE o.sid != mesh_media.sid \
             AND o.kind = 'voice' AND o.status = 'complete' \
             AND (o.chan_idx = mesh_media.chan_idx OR o.chan_idx = mesh_media.chan_idx2 OR o.chan_idx2 = mesh_media.chan_idx) \
             AND o.size_bytes >= 2 * mesh_media.size_bytes \
             AND o.size_bytes >= 400 \
             AND ABS(strftime('%s', o.created_at) - strftime('%s', mesh_media.created_at)) <= 120)",
    )
    .execute(db)
    .await
    {
        Ok(r) => {
            let n = r.rows_affected();
            if n > 0 {
                eprintln!("mesh media: companion suppression marked {n} clip(s) error");
            }
            n
        }
        Err(e) => {
            eprintln!("mesh media: companion suppression query failed: {e}");
            0
        }
    }
}

/// Write the assembled bitstream to disk and upsert the 'complete' row.
/// Shared by handle_data's finalize path and sweep_idle.
async fn store_complete(sid_hex: &str, sess: &Session) {
    let Some(db) = DBH.get() else { return };
    let Some((_, buf)) = assemble(&sess.parts) else {
        eprintln!("mesh media: store_complete for {sid_hex} with no usable parts");
        return;
    };
    let fname = format!("{sid_hex}_{}.{}", sess.kind, mode_ext(sess.mode));
    let path = std::path::Path::new(media_dir()).join(&fname);
    // A late duplicate session for the same sid can finalize a short stub
    // AFTER the full clip was stored — never clobber the larger stored clip.
    // A deleted clip (tombstone) stays deleted.
    if let Ok(prev) = sqlx::query(
        "SELECT size_bytes, status, COALESCE(chan_idx, -1) AS chan FROM mesh_media WHERE sid = ?",
    )
    .bind(sid_hex)
    .fetch_optional(db)
    .await
    {
        if let Some(row) = prev {
            let prev_status = row.try_get::<String, _>("status").unwrap_or_default();
            if prev_status == "deleted" {
                return;
            }
            let prev_chan = row.try_get::<i64, _>("chan").unwrap_or(-1);
            if prev_status == "complete" {
            let prev_size = row.try_get::<i64, _>("size_bytes").unwrap_or(0);
            if prev_size >= buf.len() as i64 {
                if sess.chan_idx as i64 != prev_chan {
                    // Same clip completed on another channel - make it
                    // visible in that channel's room too instead of
                    // silently dropping the duplicate.
                    let _ = sqlx::query(
                        "UPDATE mesh_media SET chan_idx2 = ? WHERE sid = ? \
                         AND status = 'complete' AND chan_idx != ? \
                         AND (chan_idx2 IS NULL OR chan_idx2 != ?)",
                    )
                    .bind(sess.chan_idx as i64)
                    .bind(sid_hex)
                    .bind(sess.chan_idx as i64)
                    .bind(sess.chan_idx as i64)
                    .execute(db)
                    .await;
                }
                eprintln!(
                    "mesh media: ignoring duplicate completion {sid_hex} ({} B; already stored {} B)",
                    buf.len(),
                    prev_size
                );
                return;
            }
            }
        }
    }
    let write = std::fs::write(&path, &buf)
        .map_err(|e| e.to_string())
        .and_then(|_| path.metadata().map(|m| m.len() as i64).map_err(|e| e.to_string()));
    let noise = if sess.kind == "voice" && sess.mode != 32 {
        voice_clip_is_noise(&path, sess.mode).unwrap_or(false)
    } else {
        false
    };
    let status = if noise { "error" } else { "complete" };
    match write {
        Ok(size) => {
            // Upsert (not plain UPDATE): the async part-counter INSERT
            // may not have committed yet when finalize wins the race.
            let _ = sqlx::query(
                "INSERT INTO mesh_media (sid, kind, mode, total, received, status, caption, chan_idx, dur, filename, size_bytes, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now')) \
                 ON CONFLICT(sid) DO UPDATE SET \
                   kind = excluded.kind, mode = excluded.mode, \
                   total = MAX(mesh_media.total, excluded.total), \
                   received = MAX(mesh_media.received, excluded.received), \
                   status = CASE WHEN mesh_media.status = 'error' AND excluded.status = 'complete' AND excluded.size_bytes <= mesh_media.size_bytes THEN 'error' ELSE excluded.status END, \
                   caption = CASE WHEN length(excluded.caption) > length(mesh_media.caption) THEN excluded.caption ELSE mesh_media.caption END, \
                   filename = excluded.filename, size_bytes = excluded.size_bytes",
            )
            .bind(sid_hex)
            .bind(&sess.kind)
            .bind(sess.mode as i64)
            .bind(sess.total as i64)
            .bind(sess.parts.len() as i64)
            .bind(status)
            .bind(&sess.caption)
            .bind(sess.chan_idx as i64)
            .bind(sess.dur as i64)
            .bind(&fname)
            .bind(size)
            .execute(db)
            .await;
            // Companion suppression: Open TAK phones emit a second, much
            // smaller "companion" session alongside the real clip (same
            // start second). The stub decodes as garbled audio but can
            // pass the flat-factor check while incomplete, briefly
            // appearing in the room timeline before more packets flip it
            // to noise. If a same-channel voice clip that started within
            // seconds of this one completed at >= 2x the size, hide this
            // one outright.
            if status == "complete" && sess.kind == "voice" {
                suppress_companions(db).await;
            }
            eprintln!("mesh media: stored {fname} ({size} bytes, {status})");
            relay_to_broker_clients(&fname, sess, size as u64).await;
        }
        Err(e) => {
            let _ = sqlx::query(
                "UPDATE mesh_media SET received = ?, status = 'error', filename = NULL WHERE sid = ?",
            )
            .bind(sess.parts.len() as i64)
            .bind(sid_hex)
            .execute(db)
            .await;
            eprintln!("mesh media: write failed for {sid_hex}: {e}");
        }
    }
}

/// Cap for relaying clip payloads over the broker link (bytes, raw).
const RELAY_MAX_BYTES: u64 = 300_000;

/// A console with its own radio shares completed clips with its broker
/// clients so radio-less consoles see them in Media Clips too.
async fn relay_to_broker_clients(fname: &str, sess: &Session, size: u64) {
    if !crate::broker_server::serving() || size > RELAY_MAX_BYTES {
        return;
    }
    let Ok(bytes) = std::fs::read(std::path::Path::new(media_dir()).join(fname)) else {
        return;
    };
    crate::broker_server::broadcast_media(
        sess.chan_idx,
        json!({
            "sid": fname.get(0..8).unwrap_or_default(),
            "kind": sess.kind,
            "mode": sess.mode,
            "total": sess.total,
            "dur": sess.dur,
            "caption": sess.caption,
            "chan_idx": sess.chan_idx,
            "filename": fname,
            "size_bytes": size,
            "b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }),
    );
}

/// Broker client side: a completed clip relayed by the radio-owning
/// console. Writes the file locally and upserts a complete mesh_media row.
pub async fn ingest_broker(media: &serde_json::Value) {
    let Some(db) = DBH.get() else { return };
    let (Some(sid), Some(kind), Some(filename), Some(b64)) = (
        media.get("sid").and_then(|v| v.as_str()),
        media.get("kind").and_then(|v| v.as_str()),
        media.get("filename").and_then(|v| v.as_str()),
        media.get("b64").and_then(|v| v.as_str()),
    ) else {
        return;
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
        return;
    };
    if sid.len() != 8 || !sid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return;
    }
    let path = std::path::Path::new(media_dir()).join(filename);
    // Tombstone: a deleted clip must stay deleted — ignore broker relays.
    if let Ok(Some(_)) = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM mesh_media WHERE sid = ? AND status = 'deleted'",
    )
    .bind(sid)
    .fetch_optional(db)
    .await
    {
        return;
    }
    // Never let a smaller re-completion clobber a larger stored clip.
    if let Ok(Some(prev_size)) = sqlx::query_scalar::<_, i64>(
        "SELECT size_bytes FROM mesh_media WHERE sid = ? AND status = 'complete'",
    )
    .bind(sid)
    .fetch_optional(db)
    .await
    {
        if prev_size >= bytes.len() as i64 {
            eprintln!(
                "mesh media: broker relay ignoring duplicate {sid} ({} B; already stored {} B)",
                bytes.len(),
                prev_size
            );
            // Echo on another channel (forwarder re-flood): tag it as the
            // clip's second channel so the room timeline shows the clip on
            // both channels even though the capture radio missed the echo.
            let inc_chan = media.get("chan_idx").and_then(|v| v.as_i64()).unwrap_or(-1);
            let inc_chan2 = media.get("chan_idx2").and_then(|v| v.as_i64()).unwrap_or(-1);
            let tag2 = if inc_chan2 >= 0 { inc_chan2 } else { inc_chan };
            if tag2 >= 0 {
                let _ = sqlx::query(
                    "UPDATE mesh_media SET chan_idx2 = ? WHERE sid = ? \
                     AND chan_idx2 IS NULL AND chan_idx != ? AND status = 'complete'",
                )
                .bind(tag2)
                .bind(sid)
                .bind(tag2)
                .execute(db)
                .await;
            }
            return;
        }
    }
    if std::fs::write(&path, &bytes).is_err() {
        eprintln!("mesh media: broker relay write failed for {sid}");
        return;
    }
    // Kill any live in-memory session for this sid so late stray packets
    // cannot re-finalize and overwrite the good file with a partial one.
    if let Ok(s) = u32::from_str_radix(sid, 16) {
        sessions().lock().unwrap().remove(&s);
    }
    let mode_v = media.get("mode").and_then(|v| v.as_i64()).unwrap_or(0);
    let noise = if kind == "voice" && mode_v != 32 {
        voice_clip_is_noise(&path, mode_v as u32).unwrap_or(false)
    } else {
        false
    };
    let status = if noise { "error" } else { "complete" };
    let _ = sqlx::query(
        "INSERT INTO mesh_media (sid, kind, mode, total, received, status, caption, chan_idx, dur, filename, size_bytes, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now')) \
         ON CONFLICT(sid) DO UPDATE SET status=excluded.status, filename=excluded.filename, \
         size_bytes=excluded.size_bytes, received=MAX(mesh_media.received, excluded.received), \
         mode=excluded.mode, chan_idx=excluded.chan_idx, total=MAX(mesh_media.total, excluded.total), \
         caption=CASE WHEN length(excluded.caption) > length(mesh_media.caption) THEN excluded.caption ELSE mesh_media.caption END",
    )
    .bind(sid)
    .bind(kind)
    .bind(mode_v)
    .bind(media.get("total").and_then(|v| v.as_i64()).unwrap_or(0))
    .bind(media.get("total").and_then(|v| v.as_i64()).unwrap_or(0))
    .bind(status)
    .bind(media.get("caption").and_then(|v| v.as_str()).unwrap_or(""))
    .bind(media.get("chan_idx").and_then(|v| v.as_i64()).unwrap_or(0))
    .bind(media.get("dur").and_then(|v| v.as_i64()).unwrap_or(0))
    .bind(filename)
    .bind(bytes.len() as i64)
    .execute(db)
    .await;
    // Second channel tag from the bridge (partial echo heard on the other
    // channel): backfill when the upserted row has none.
    if let Some(c2) = media.get("chan_idx2").and_then(|v| v.as_i64()).filter(|v| *v >= 0) {
        let _ = sqlx::query(
            "UPDATE mesh_media SET chan_idx2 = ? WHERE sid = ? \
             AND chan_idx2 IS NULL AND chan_idx != ? AND status = 'complete'",
        )
        .bind(c2)
        .bind(sid)
        .bind(c2)
        .execute(db)
        .await;
    }
    eprintln!("mesh media: broker relay stored {filename} ({} bytes)", bytes.len());
}

/// Local bridge push: the bearden-bridge completes clips (it can 0x72-fetch
/// missing parts; the console cannot) and posts the finished bitstream here
/// so console Media Clips / room timelines get the full clip. Auth = the
/// shared bearden_bridge_token (same secret the console uses to call the
/// bridge), Bearer header or ?token=.
async fn intake(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    axum::Json(mut body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let want: String = sqlx::query_scalar(
        "SELECT value FROM settings WHERE key = 'bearden_bridge_token'",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    if want.trim().is_empty() {
        return (StatusCode::FORBIDDEN, "intake disabled");
    }
    let got = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| q.get("token").cloned())
        .unwrap_or_default();
    if got != want {
        return (StatusCode::FORBIDDEN, "bad token");
    }
    let chan = body.get("chan").and_then(|v| v.as_str()).map(|s| s.to_string());
    if let Some(chan) = &chan {
        let idx = chan_idx_for_db(&state.db, chan).await;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("chan_idx".to_string(), json!(idx));
        }
    }
    if let Some(chan2) = body.get("chan2").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        let idx2 = chan_idx_for_db(&state.db, chan2).await;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("chan_idx2".to_string(), json!(idx2));
        }
    }
    ingest_broker(&body).await;
    (StatusCode::OK, "ok")
}

// ------------------------------------------------------------------ UI

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/platforms/meshcore/media", get(list))
        .route("/platforms/meshcore/media/", get(list))
        .route("/platforms/meshcore/media/{id}/file", get(file))
        .route("/platforms/meshcore/media/{id}/play", get(play))
        .route("/platforms/meshcore/media/{id}/delete", post(delete))
        .route("/platforms/meshcore/media/send", post(send_clip))
        .route("/platforms/meshcore/media/photo", post(photo_send))
        .route("/platforms/meshcore/media/debug", get(sessions_debug))
        .route("/platforms/meshcore/media/intake", post(intake))
}

fn chan_name(idx: i64) -> String {
    crate::radio::radio()
        .map(|r| r.channel_name(idx as u8))
        .unwrap_or_else(|| format!("chan{idx}"))
}

/// Resolve a wire channel name ("bears"/"bears2"/"public") to the radio
/// channel index, so locally-sent clips land on the right room timeline.
async fn chan_idx_for_db(db: &sqlx::SqlitePool, name: &str) -> i64 {
    let want = name.trim().to_lowercase();
    if let Some(r) = crate::radio::radio() {
        for (idx, n) in r.channels() {
            let n = n.trim().to_lowercase();
            if n == want || n.split_whitespace().next() == Some(want.as_str()) {
                return idx as i64;
            }
        }
    }
    if let Ok(rows) = sqlx::query_as::<_, (i64, String)>(
        "SELECT idx, name FROM mesh_followed_channels",
    )
    .fetch_all(db)
    .await
    {
        for (idx, n) in rows {
            let n = n.trim().to_lowercase();
            if n == want || n.split_whitespace().next() == Some(want.as_str()) {
                return idx;
            }
        }
    }
    0
}

/// Store a clip the console itself sent (file + complete mesh_media row) so
/// it appears immediately in the room timeline with its caption. RF capture
/// cannot supply the caption and the sender dedups its own flood.
async fn store_local_clip(
    db: &sqlx::SqlitePool,
    sid_hex: &str,
    kind: &str,
    mode: i64,
    dur: i64,
    caption: &str,
    chan_idx: i64,
    bytes: &[u8],
) {
    if sid_hex.len() != 8 || !sid_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return;
    }
    let fname = format!(
        "{sid_hex}_{}.{}",
        kind,
        mode_ext(mode.clamp(0, 255) as u32)
    );
    let path = std::path::Path::new(media_dir()).join(&fname);
    if std::fs::write(&path, bytes).is_err() {
        eprintln!("mesh media: local clip write failed for {sid_hex}");
        return;
    }
    let size = bytes.len() as i64;
    let _ = sqlx::query(
        "INSERT INTO mesh_media (sid, kind, mode, total, received, status, caption, chan_idx, dur, filename, size_bytes, created_at) \
         VALUES (?, ?, ?, 0, 0, 'complete', ?, ?, ?, ?, ?, datetime('now')) \
         ON CONFLICT(sid) DO NOTHING",
    )
    .bind(sid_hex)
    .bind(kind)
    .bind(mode)
    .bind(caption)
    .bind(chan_idx)
    .bind(dur)
    .bind(&fname)
    .bind(size)
    .execute(db)
    .await;
    eprintln!("mesh media: stored local clip {fname} ({size} bytes)");
}

async fn list(_user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let rows = sqlx::query(
        "SELECT id, sid, kind, mode, total, received, status, caption, chan_idx, dur, filename, size_bytes, created_at \
         FROM mesh_media WHERE status != 'deleted' ORDER BY id DESC LIMIT 500",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let items: Vec<_> = rows
        .into_iter()
        .map(|r| {
            let status: String = r.try_get("status").unwrap_or_default();
            json!({
                "id": r.try_get::<i64, _>("id").unwrap_or(0),
                "sid": r.try_get::<String, _>("sid").unwrap_or_default(),
                "kind": r.try_get::<String, _>("kind").unwrap_or_default(),
                "codec": mode_name(r.try_get::<i64, _>("mode").unwrap_or(999) as u32),
                "mode": r.try_get::<i64, _>("mode").unwrap_or(0),
                "total": r.try_get::<i64, _>("total").unwrap_or(0),
                "received": r.try_get::<i64, _>("received").unwrap_or(0),
                "complete": status == "complete",
                "status": status,
                "caption": r.try_get::<String, _>("caption").unwrap_or_default(),
                "channel": chan_name(r.try_get::<i64, _>("chan_idx").unwrap_or(0)),
                "dur": r.try_get::<i64, _>("dur").unwrap_or(0),
                "filename": r.try_get::<Option<String>, _>("filename").ok().flatten(),
                "size": r.try_get::<Option<i64>, _>("size_bytes").ok().flatten(),
                "created": r.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect();
    Html(state.templates.render(
        "meshcore_media.html",
        json!({ "items": items }),
    ))
}

async fn file(_user: RequireUser, Path(id): Path<i64>, State(state): State<SharedState>) -> impl IntoResponse {
    let row = sqlx::query("SELECT sid, filename FROM mesh_media WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let sid: String = row.try_get("sid").unwrap_or_default();
    let Some(fname) = row.try_get::<Option<String>, _>("filename").ok().flatten() else {
        return (StatusCode::NOT_FOUND, "not stored").into_response();
    };
    let path = std::path::Path::new(media_dir()).join(&fname);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"{fname}\""))
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            format!("file missing for session {sid}"),
        )
            .into_response(),
    }
}

/// Proxy a recorded clip to the Bear Den bridge, which chunks and paces it
/// onto radio2. JSON in/out. The console never touches the radio itself.
async fn send_clip(
    _user: RequireUser,
    State(state): State<SharedState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    async fn get(k: &str, db: &Db) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
            .bind(k)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
    }
    let url = get("bearden_bridge_url", &state.db)
        .await
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8095".into());
    let token = get("bearden_bridge_token", &state.db).await.unwrap_or_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let resp = client
        .post(format!("{url}/media/send"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status();
            let payload: serde_json::Value = r.json().await.unwrap_or(serde_json::json!({"ok": false, "error": "unparseable bridge reply"}));
            if payload["ok"].as_bool() != Some(true) {
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(payload),
                )
                    .into_response();
            }
            // Mirror the sent clip locally so it (and its caption) shows in
            // the room timeline right away.
            if let (Some(sid), Some(b64)) = (
                payload["sid"].as_str(),
                body["b64"].as_str(),
            ) {
                let kind = body["kind"].as_str().unwrap_or("voice");
                let mode = body["mode"].as_i64().or(payload["mode"].as_i64()).unwrap_or(1);
                let caption = body["caption"].as_str().unwrap_or("").trim();
                let chan_idx = chan_idx_for_db(&state.db, body["chan"].as_str().unwrap_or("bears")).await;
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                    store_local_clip(&state.db, sid, kind, mode, 0, caption, chan_idx, &bytes).await;
                }
            }
            (status, axum::Json(payload)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({"ok": false, "error": format!("bridge unreachable: {e}")})),
        )
            .into_response(),
    }
}

/// Take an arbitrary uploaded photo (JPEG/PNG/WebP, base64), downscale and
/// re-encode it as a tiny AV1/AVIF still image that fits the bridge's
/// per-session radio2 budget, then flood it via the Bear Den bridge.
/// Mirrors what Open TAK does on the phone: photos are always aggressively
/// compressed before touching the mesh.
const PHOTO_MAX_BYTES: usize = 22_950; // bridge hard cap: 255 packets x 90 bytes

async fn photo_send(
    _user: RequireUser,
    State(state): State<SharedState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    let Some(b64in) = body["image_b64"].as_str() else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"ok": false, "error": "image_b64 required"}))).into_response();
    };
    let raw = match base64::engine::general_purpose::STANDARD.decode(b64in) {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, axum::Json(json!({"ok": false, "error": format!("bad base64: {e}")}))).into_response();
        }
    };
    if raw.is_empty() || raw.len() > 30 * 1024 * 1024 {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"ok": false, "error": "empty or oversized upload"}))).into_response();
    }
    let chan = body["chan"].as_str().unwrap_or("bears").to_string();
    // Airtime budget, mirroring Open TAK's detail levels (bytes of payload).
    let budget = body["budget"]
        .as_u64()
        .map(|b| (b as usize).min(PHOTO_MAX_BYTES))
        .unwrap_or(5_500);

    let dir = std::env::temp_dir();
    let tag = std::process::id() as u128 ^ std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let in_path = dir.join(format!("mmphoto_{tag:x}.in"));
    let out_path = dir.join(format!("mmphoto_{tag:x}.obu"));
    let thumb_path = dir.join(format!("mmphoto_{tag:x}.t.obu"));
    let cleanup = |p: std::path::PathBuf| async move { let _ = tokio::fs::remove_file(p).await; };

    if let Err(e) = tokio::fs::write(&in_path, &raw).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"ok": false, "error": format!("temp write failed: {e}")}))).into_response();
    }

    // Progressive (size, crf) ladder until the bare AV1 OBU stream fits the
    // chosen budget. Open TAK's wire payload is raw OBUs (v1/v2 header),
    // NOT an AVIF container — a container file is why Open TAK reported
    // "could not be decoded".
    let mut encoded: Option<Vec<u8>> = None;
    for (w, crf) in [
        (480i32, 48i32), (480, 56), (360, 54), (360, 60),
        (256, 58), (256, 62), (192, 62), (160, 63),
    ] {
        let out = tokio::process::Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-i", in_path.to_str().unwrap_or_default(),
                "-vf", &format!("scale='min({w},iw)':-2,format=yuv420p"),
                "-frames:v", "1",
                "-c:v", "libaom-av1",
                "-still-picture", "1",
                "-crf", &crf.to_string(),
                "-cpu-used", "6",
                "-f", "obu",
                out_path.to_str().unwrap_or_default(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        match (out, tokio::fs::read(&out_path).await) {
            (Ok(st), Ok(bytes)) if st.success() && bytes.len() <= budget && !bytes.is_empty() => {
                encoded = Some(bytes);
                break;
            }
            _ => {}
        }
    }
    let Some(mut obus) = encoded else {
        let _ = cleanup(out_path).await;
        let _ = cleanup(thumb_path).await;
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({"ok": false, "error": format!("could not compress photo under {budget} bytes")})),
        )
            .into_response();
    };
    // Open TAK's encoder strips a leading temporal-delimiter OBU (0x12 0x00)
    // and its decoder re-adds it — match that exactly.
    if obus.len() > 2 && obus[0] == 0x12 && obus[1] == 0x00 {
        obus = obus[2..].to_vec();
    }

    // Tiny thumbnail for Open TAK's v2 payload (preview in first packets);
    // fall back to the v1 payload when it will not fit in 255 bytes.
    let thumb = tokio::process::Command::new("ffmpeg")
        .args([
            "-y", "-loglevel", "error",
            "-i", in_path.to_str().unwrap_or_default(),
            "-vf", "scale=48:48:force_original_aspect_ratio=decrease,format=yuv420p",
            "-frames:v", "1",
            "-c:v", "libaom-av1",
            "-still-picture", "1",
            "-crf", "63",
            "-cpu-used", "6",
            "-f", "obu",
            thumb_path.to_str().unwrap_or_default(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    let thumb_bytes = match (thumb, tokio::fs::read(&thumb_path).await) {
        (Ok(st), Ok(mut b)) if st.success() && !b.is_empty() && b.len() <= 255 => {
            if b.len() > 2 && b[0] == 0x12 && b[1] == 0x00 {
                b = b[2..].to_vec();
            }
            b
        }
        _ => Vec::new(),
    };
    let _ = cleanup(thumb_path).await;

    // Aspect ratio (width*100/height) for the VI1 announce's dur field.
    let dims = tokio::process::Command::new("ffprobe")
        .args([
            "-v", "error", "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-of", "csv=p=0",
            out_path.to_str().unwrap_or_default(),
        ])
        .output()
        .await;
    let aspect: u64 = match dims {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let mut it = s.trim().split(',');
            if let (Some(w), Some(h)) = (it.next().and_then(|v| v.parse::<u64>().ok()), it.next().and_then(|v| v.parse::<u64>().ok())) {
                if h > 0 { (w * 100 / h).clamp(20, 600) } else { 100 }
            } else { 100 }
        }
        _ => 100,
    };
    let produced = 2 + thumb_bytes.len() + obus.len();
    let _ = cleanup(out_path).await;
    let _ = cleanup(in_path).await;

    let mut wire = Vec::with_capacity(produced);
    if thumb_bytes.is_empty() {
        wire.push(1u8); // v1: [1][photo OBUs]
        wire.extend_from_slice(&obus);
    } else {
        wire.push(2u8); // v2: [2][thumb len][thumb OBUs][photo OBUs]
        wire.push(thumb_bytes.len() as u8);
        wire.extend_from_slice(&thumb_bytes);
        wire.extend_from_slice(&obus);
    }

    async fn get(k: &str, db: &Db) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
            .bind(k)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
    }
    let url = get("bearden_bridge_url", &state.db)
        .await
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8095".into());
    let token = get("bearden_bridge_token", &state.db).await.unwrap_or_default();
    let payload = json!({
        "b64": base64::engine::general_purpose::STANDARD.encode(&wire),
        "kind": "photo",
        "mode": 40,
        "chan": chan,
        "aspect": aspect,
    });
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
        .post(format!("{url}/media/send"))
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status();
            let mut reply: serde_json::Value = r.json().await.unwrap_or(json!({"ok": false, "error": "unparseable bridge reply"}));
            reply["produced_bytes"] = json!(produced);
            if reply["ok"].as_bool() != Some(true) {
                return (StatusCode::BAD_GATEWAY, axum::Json(reply)).into_response();
            }
            let caption = body["caption"].as_str().unwrap_or("").trim();
            if let Some(sid) = reply["sid"].as_str() {
                store_local_clip(
                    &state.db, sid, "photo", 40, aspect as i64, caption,
                    chan_idx_for_db(&state.db, &chan).await, &wire,
                )
                .await;
            }
            (status, axum::Json(reply)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(json!({"ok": false, "error": format!("bridge unreachable: {e}")})),
        )
            .into_response(),
    }
}

/// Decode a clip for in-browser playback/viewing: voice clips are
/// transcoded to WAV with ffmpeg (libcodec2); photo clips are served as
/// AVIF when containerized (ftyp header) or decoded from raw AV1 OBU to
/// PNG with ffmpeg (libdav1d).
async fn play(_user: RequireUser, Path(id): Path<i64>, State(state): State<SharedState>) -> axum::response::Response {
    let row = sqlx::query("SELECT kind, mode, filename FROM mesh_media WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(fname) = row.try_get::<Option<String>, _>("filename").ok().flatten() else {
        return (StatusCode::NOT_FOUND, "not stored").into_response();
    };
    let kind: String = row.try_get("kind").unwrap_or_default();
    let mode: i64 = row.try_get("mode").unwrap_or(0);
    let path = std::path::Path::new(media_dir()).join(&fname);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "file missing").into_response(),
    };

    // Photos: AVIF containers render natively; pass through with the right type.
    if kind == "photo" && bytes.len() > 12 && &bytes[4..8] == b"ftyp" {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/avif"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return (StatusCode::OK, headers, bytes).into_response();
    }

    // Open TAK wire format: [1][photo OBUs] or [2][thumb_len][thumb][photo OBUs].
    // Strip the header and re-add the temporal delimiter ffmpeg's obu demuxer
    // demands (Open TAK encoders strip it; their decoder re-adds it too).
    let mut bytes = bytes;
    if kind == "photo" && !bytes.is_empty() && bytes.len() > 12 && &bytes[4..8] != b"ftyp" {
        let skip = if bytes[0] == 1 {
            1
        } else if bytes[0] == 2 && bytes.len() > 1 && (2 + bytes[1] as usize) < bytes.len() {
            2 + bytes[1] as usize
        } else {
            0
        };
        let mut v = vec![0x12, 0x00];
        v.extend_from_slice(&bytes[skip..]);
        bytes = v;
    }

    // Everything else goes through ffmpeg.
    let args: Vec<String> = if kind == "photo" {
        vec![
            "-f".into(), "obu".into(), "-i".into(), "pipe:0".into(),
            "-frames:v".into(), "1".into(),
            "-f".into(), "image2".into(), "-c:v".into(), "png".into(), "pipe:1".into(),
        ]
    } else if (0..8).contains(&mode) {
        vec![
            "-f".into(), "codec2raw".into(), "-mode".into(), c2_mode(mode as u32).into(),
            "-i".into(), "pipe:0".into(),
            "-f".into(), "wav".into(), "pipe:1".into(),
        ]
    } else {
        // mimi (32) and unknown codecs: offer the raw bitstream.
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        return (StatusCode::OK, headers, bytes).into_response();
    };
    let out = tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match out {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("ffmpeg unavailable: {e}")).into_response()
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt as _;
        let _ = stdin.write_all(&bytes).await;
    } // drop closes stdin so ffmpeg finishes
    match child.wait_with_output().await {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
            let ctype = if kind == "photo" { "image/png" } else { "audio/wav" };
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(ctype).unwrap_or(HeaderValue::from_static("application/octet-stream")),
            );
            (StatusCode::OK, headers, o.stdout).into_response()
        }
        Ok(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("could not decode {fname} (mode {mode})"),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("ffmpeg failed: {e}")).into_response(),
    }
}

async fn delete(
    _user: RequireUser,
    Path(id): Path<i64>,
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Redirect {
    let back = params
        .get("next")
        .filter(|v| v.starts_with("/platforms/"))
        .cloned()
        .unwrap_or_else(|| "/platforms/meshcore/media".into());
    if let Ok(row) = sqlx::query("SELECT sid, filename FROM mesh_media WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
    {
        if let Some(row) = row {
            if let Some(fname) = row.try_get::<Option<String>, _>("filename").ok().flatten() {
                let path = std::path::Path::new(media_dir()).join(&fname);
                let _ = std::fs::remove_file(path);
            }
            if let Ok(sid) = row.try_get::<String, _>("sid") {
                if let Ok(v) = u32::from_str_radix(&sid, 16) {
                    sessions().lock().unwrap().remove(&v);
                }
            }
            let _ = sqlx::query(
                "UPDATE mesh_media SET status = 'deleted', filename = NULL, caption = '' WHERE id = ?",
            )
            .bind(id)
            .execute(&state.db)
            .await;
        }
    }
    Redirect::to(&back)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_multi_prefix() {
        assert!(parse_envelope("VE3:abc:1:8:4").is_some());
        assert!(parse_envelope("Stray Bear: VE3:abc:1:8:4").is_some());
        assert!(parse_envelope("Bear Den2: Bear Den: Bear Den2: VE3:32tv07:1:9:4").is_some());
        assert!(parse_envelope("Bear Den: VM1:kdqlcj:4:2:1 [VOICE]").is_some());
        assert!(parse_envelope("Bear Den: VI1:kdqlcj:4:2:150").is_some());
        assert!(parse_envelope("Stray Bear: z3sd").is_none());
        assert!(parse_envelope("Luke: @[EtherApe] hi").is_none());
    }

    #[test]
    fn assemble_dual_packetization_picks_consistent_class() {
        // Stray Bear incident (Nov 6): same sid packetized twice — 156-byte
        // chunks and 92-byte chunks of DIFFERENT byte streams, plus an 84-byte
        // tail. Must pick one class: 4x156 + 84 tail = 708 B of clean audio,
        // never a mix (mixing garbles codec2).
        let mut parts: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
        for i in 0..4u32 {
            parts.insert(i, vec![vec![0xAA; 156], vec![0xBB; 92]]);
        }
        parts.insert(4, vec![vec![0xCC; 84]]);
        let (run, buf) = assemble(&parts).expect("assembles");
        assert_eq!(run, 5);
        assert_eq!(buf.len(), 156 * 4 + 84);
        assert!(buf[..156].iter().all(|&b| b == 0xAA));
        assert!(buf[156 * 4..].iter().all(|&b| b == 0xCC));
    }
}
