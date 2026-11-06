//! Sage: the mesh LLM bot. Channel questions starting with "?" are
//! answered by a local OpenAI-compatible endpoint with tool calls
//! (NWS weather/alerts, OSRM distance, gazetteer place facts). One
//! question at a time, per-sender cooldown, answers truncated to fit
//! a LoRa packet. Port of `mesh/sage.py` (simplified: the gazetteer
//! deterministic fast-path and the weather pre-cache are omitted).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::broker::log_traffic;
use crate::gazetteer;
use crate::settings::MeshSettings;

pub const DEFAULT_SAGE_MAX_CHARS: usize = 120;

const SYSTEM_PROMPT: &str = "You are sage, a concise rural-Pacific-Northwest \
assistant answering over a slow LoRa mesh. Keep answers under 120 characters. \
Questions about sage itself (\"who are you\", \"who made you\") are about you, \
the mesh assistant — answer directly and never call tools for them. \
Weather comes from get_weather / get_alerts / get_current_weather tools \
instead of guessing. Weather tools only cover Washington and Oregon. Use \
get_distance for how-far questions and get_place_facts for places, peaks \
and passes. Use search_knowledge for medical, first-aid, survival, plant, \
knot, water, food-preservation and general-knowledge questions — it searches \
offline reference libraries (WikiMed, WikEM, Appropedia, prepper guides).";

#[derive(Debug)]
pub struct Job {
    pub chan_idx: i64,
    pub sender: String,
    pub question: String,
}

pub struct SageBot {
    db: SqlitePool,
    tx: OnceLock<mpsc::UnboundedSender<Job>>,
    cooldowns: Mutex<HashMap<String, Instant>>,
    tools_ok: Mutex<bool>,
}

impl SageBot {
    pub fn new(db: SqlitePool) -> Arc<Self> {
        Arc::new(SageBot {
            db,
            tx: OnceLock::new(),
            cooldowns: Mutex::new(HashMap::new()),
            tools_ok: Mutex::new(true),
        })
    }

    fn settings(&self) -> MeshSettings {
        futures_lite_block_on(MeshSettings::load(&self.db))
    }

    pub fn interested(&self, channel_name: &str) -> bool {
        // Channel long names often carry a leading '#' (e.g. "#sage") while
        // the settings list says "sage" — compare with it stripped.
        let norm = |c: &str| c.trim().trim_start_matches('#').to_lowercase();
        let s = self.settings();
        s.sage_enabled
            && s.sage_channels
                .iter()
                .any(|c| norm(c) == norm(channel_name))
    }

    pub fn enqueue(&self, chan_idx: i64, _channel_name: &str, sender: &str, question: &str) {
        let s = self.settings();
        if !s.sage_enabled {
            return;
        }
        // Per-sender cooldown (skipped for exempt senders).
        if !s
            .sage_cooldown_exempt
            .iter()
            .any(|e| e.eq_ignore_ascii_case(sender))
        {
            let mut cds = self.cooldowns.lock().unwrap();
            let key = format!("{sender}@{chan_idx}");
            if let Some(t) = cds.get(&key) {
                if t.elapsed().as_secs() < s.sage_cooldown_secs as u64 {
                    return;
                }
            }
            cds.insert(key, Instant::now());
        }
        if let Some(tx) = self.tx.get() {
            let _ = tx.send(Job {
                chan_idx,
                sender: sender.to_string(),
                question: question.to_string(),
            });
        }
    }

    pub async fn start(&self) {
        if self.tx.get().is_some() {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel::<Job>();
        let _ = self.tx.set(tx);
        let db = self.db.clone();
        // Concurrent workers: llama.cpp batches parallel requests, so N
        // in-flight questions raise throughput without extra copies of
        // the model. `sage_workers` tunes it (default 2).
        let workers = self
            .settings()
            .sage_workers
            .clamp(1, 8) as usize;
        let rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));
        for n in 0..workers {
            let rx = rx.clone();
            let db = db.clone();
            tokio::spawn(async move {
                while let Some(job) = rx.lock().await.recv().await {
                    let sage = SageBot::new(db.clone());
                    sage.answer(job).await;
                }
                let _ = n;
            });
        }
    }

    async fn answer(&self, job: Job) {
        let s = self.settings();
        let mention = if job.sender.is_empty() || job.sender == "anon" {
            String::new()
        } else {
            format!("@{} ", job.sender)
        };

        let http = reqwest::Client::builder().timeout(Duration::from_secs(240)).build().unwrap();
        let url = format!("{}/chat/completions", s.sage_base_url.trim_end_matches('/'));

        let mut messages = vec![
            json!({ "role": "system", "content": SYSTEM_PROMPT }),
            json!({ "role": "user", "content": job.question }),
        ];

        let mut answer = String::new();
        for _round in 0..6 {
            let mut payload = json!({
                "model": s.sage_model,
                "messages": messages,
                "max_tokens": 512,
                "temperature": 0.3,
                // Qwen thinking models burn the whole budget on hidden
                // reasoning; disable it (llama.cpp chat_template_kwargs).
                "chat_template_kwargs": { "enable_thinking": false },
            });
            if *self.tools_ok.lock().unwrap() {
                payload["tools"] = tools_schema();
            }
            let resp = match http.post(&url).json(&payload).send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("sage: LLM endpoint error: {e}");
                    return;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                if status.as_u16() == 404 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue; // model loading; retry
                }
                if *self.tools_ok.lock().unwrap() && [400, 500, 501].contains(&status.as_u16()) {
                    *self.tools_ok.lock().unwrap() = false;
                    continue; // degrade to plain Q&A
                }
                eprintln!("sage: LLM endpoint returned {status}");
                return;
            }
            let data: Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("sage: bad LLM response: {e}");
                    return;
                }
            };
            let choice = data
                .pointer("/choices/0/message")
                .cloned()
                .unwrap_or(json!({}));
            let tool_calls = choice.get("tool_calls").and_then(|t| t.as_array()).cloned();
            match tool_calls {
                None => {
                    // Reasoning models (e.g. Qwen served by llama.cpp) may
                    // put the answer in reasoning_content and leave content
                    // empty when tools are offered.
                    answer = choice
                        .get("content")
                        .and_then(|c| c.as_str())
                        .filter(|c| !c.trim().is_empty())
                        .or_else(|| {
                            choice
                                .get("reasoning_content")
                                .and_then(|c| c.as_str())
                        })
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    break;
                }
                Some(calls) if calls.is_empty() => {
                    answer = choice
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    break;
                }
                Some(calls) => {
                    messages.push(choice);
                    for tc in &calls {
                        let name = tc.pointer("/function/name").and_then(|n| n.as_str()).unwrap_or("");
                        let args: Value = tc
                            .pointer("/function/arguments")
                            .and_then(|a| a.as_str())
                            .and_then(|a| serde_json::from_str(a).ok())
                            .unwrap_or(json!({}));
                        let result = run_tool(name, &args, &http, &s).await;
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tc.get("id").cloned().unwrap_or(json!("")),
                            "content": result,
                        }));
                    }
                }
            }
        }
        // Thinking models (Qwen/llama.cpp) can spend every round on tool
        // calls or reasoning; if the loop ended without an answer, force one
        // plain round without tools so the sender is never left hanging.
        if answer.is_empty() {
            messages.push(json!({ "role": "user", "content": "Answer the original question concisely now, in 150 words or fewer." }));
            let payload = json!({
                "model": s.sage_model,
                "messages": messages,
                "max_tokens": 768,
                "temperature": 0.3,
                "chat_template_kwargs": { "enable_thinking": false },
            });
            if let Ok(resp) = http.post(&url).json(&payload).send().await {
                if let Ok(data) = resp.json::<Value>().await {
                    let choice = data.pointer("/choices/0/message").cloned().unwrap_or(json!({}));
                    answer = choice
                        .get("content")
                        .and_then(|c| c.as_str())
                        .filter(|c| !c.trim().is_empty())
                        .or_else(|| choice.get("reasoning_content").and_then(|c| c.as_str()))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                }
            }
        }
        if answer.is_empty() {
            eprintln!("sage: no answer produced for question: {}", job.question);
            return;
        }
        let reply: String = format!("sage: {mention}{}", clip(&answer, s.sage_max_chars as usize))
            .chars()
            .take(crate::broker::MAX_PACKET_TEXT)
            .collect();
        eprintln!("sage reply on #chan{}: {reply}", job.chan_idx);
        log_traffic(&self.db, "meshcore", "out", &format!("#chan{}", job.chan_idx), "sage", &reply).await;
        if let Some(b) = crate::broker::broker() {
            b.send_chan(job.chan_idx, &reply).await;
        }
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max_chars {
        return t.to_string();
    }
    let cut: String = t.chars().take(max_chars).collect();
    match cut.rfind(['.', '!', '?']) {
        Some(i) if i > max_chars / 2 => cut[..=i].to_string(),
        _ => format!("{cut}..."),
    }
}

pub(crate) fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
    // Only called from sync contexts (enqueue/interested) that are
    // already inside the tokio runtime.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

fn tools_schema() -> Value {
    json!([
        { "type": "function", "function": {
            "name": "get_weather",
            "description": "Current conditions and the next few forecast periods from NWS. Place in Washington or Oregon, or lat/lon.",
            "parameters": { "type": "object", "properties": {
                "place": { "type": "string" },
                "lat": { "type": "number" }, "lon": { "type": "number" },
                "periods": { "type": "integer" } } } } },
        { "type": "function", "function": {
            "name": "get_alerts",
            "description": "Active weather alerts from NWS for a Washington or Oregon place (or lat/lon).",
            "parameters": { "type": "object", "properties": {
                "place": { "type": "string" },
                "lat": { "type": "number" }, "lon": { "type": "number" } } } } },
        { "type": "function", "function": {
            "name": "get_place_facts",
            "description": "Verified local facts about a Washington or Oregon place, peak, pass or landmark.",
            "parameters": { "type": "object", "required": ["place"],
                "properties": { "place": { "type": "string" } } } } },
        { "type": "function", "function": {
            "name": "get_distance",
            "description": "Distance between two Washington or Oregon places. ALWAYS use for how-far questions.",
            "parameters": { "type": "object", "required": ["from_place", "to_place"],
                "properties": { "from_place": { "type": "string" }, "to_place": { "type": "string" } } } } },
        { "type": "function", "function": {
            "name": "search_knowledge",
            "description": "Search offline reference libraries (medical encyclopedias, emergency medicine, survival/preparedness, appropriate tech). ALWAYS use for medical, first aid, injuries, illness, plants, knots, water purification, food preservation and general-knowledge questions.",
            "parameters": { "type": "object", "required": ["query"],
                "properties": { "query": { "type": "string", "description": "Search terms, e.g. 'splint fracture' or 'water purification'" } } } } },
    ])
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn run_tool(name: &str, args: &Value, http: &reqwest::Client, s: &MeshSettings) -> String {
    const LAT_MIN: f64 = 45.5;
    const LAT_MAX: f64 = 49.1;
    const LON_MIN: f64 = -124.8;
    const LON_MAX: f64 = -116.9;

    let place = args.get("place").and_then(|p| p.as_str()).unwrap_or("").trim();
    let mut lat = args.get("lat").and_then(|v| v.as_f64());
    let mut lon = args.get("lon").and_then(|v| v.as_f64());

    // The geography guard below only applies to geo tools; other tools
    // (get_place_facts, get_distance, search_knowledge) take their own
    // arguments and must not be blocked with "need a place or lat/lon".
    let (lat, lon) = if matches!(name, "get_weather" | "get_alerts") {
        if !place.is_empty() {
            match gazetteer::geocode(place) {
                Some([la, lo]) => {
                    lat = Some(la);
                    lon = Some(lo);
                }
                None => {
                    return json!({ "error": format!("unknown place '{place}' in sage's coverage area (Washington/Oregon)") }).to_string()
                }
            }
        }
        let (Some(lat), Some(lon)) = (lat, lon) else {
            return json!({ "error": "need a place or lat/lon" }).to_string();
        };
        if !(LAT_MIN..=LAT_MAX).contains(&lat) || !(LON_MIN..=LON_MAX).contains(&lon) {
            return json!({ "error": "outside sage's coverage area (Washington and Oregon only)" }).to_string();
        }
        (lat, lon)
    } else {
        let _ = (&mut lat, &mut lon);
        (f64::NAN, f64::NAN)
    };

    match name {
        "get_weather" => {
            match nws_forecast(http, lat, lon).await {
                Ok(v) => {
                    let periods = v.get("periods").cloned().unwrap_or(json!([]));
                    json!({ "location": if place.is_empty() { format!("{lat:.3},{lon:.3}") } else { place.to_lowercase() }, "periods": periods })
                }
                Err(e) => json!({ "error": e }),
            }
            .to_string()
        }
        "get_alerts" => match http
            .get(format!(
                "https://api.weather.gov/alerts/active?point={lat},{lon}"
            ))
            .header("User-Agent", "syarc-sage")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let count = r
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|d| {
                        d.get("features")
                            .and_then(|f| f.as_array())
                            .map(|a| a.len())
                    })
                    .unwrap_or(0);
                json!({ "active_alerts": count }).to_string()
            }
            _ => json!({ "error": "NWS alerts unavailable" }).to_string(),
        },
        "get_place_facts" => {
            if place.is_empty() {
                return json!({ "error": "need a place name" }).to_string();
            }
            gazetteer::place_facts(place)
                .unwrap_or(json!({ "error": format!("no gazetteer entry for '{place}'") }))
                .to_string()
        }
        "get_distance" => {
            let a = args.get("from_place").and_then(|p| p.as_str()).unwrap_or("");
            let b = args.get("to_place").and_then(|p| p.as_str()).unwrap_or("");
            gazetteer::distance(a, b).to_string()
        }
        "search_knowledge" => {
            let q = args.get("query").and_then(|p| p.as_str()).unwrap_or("").trim();
            if q.is_empty() {
                return json!({ "error": "need a query" }).to_string();
            }
            if s.sage_kiwix_url.is_empty() {
                return json!({ "error": "knowledge library not configured" }).to_string();
            }
            let base = s.sage_kiwix_url.trim_end_matches('/');
            let url = format!("{base}/search?pattern={}", urlencode(q));
            match http.get(&url).send().await {
                Ok(r) if r.status().is_success() => match r.text().await {
                    Ok(html) => parse_kiwix_results(&html),
                    Err(e) => json!({ "error": format!("kiwix read: {e}") }).to_string(),
                },
                Ok(r) => json!({ "error": format!("kiwix returned {}", r.status()) }).to_string(),
                Err(e) => json!({ "error": format!("kiwix unreachable: {e}") }).to_string(),
            }
        }
        _ => json!({ "error": format!("unknown tool {name}") }).to_string(),
    }
}

/// Parse kiwix-serve's HTML search results page into top-3 snippets.
fn parse_kiwix_results(html: &str) -> String {
    fn strip_tags(s: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for c in s.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let mut results = Vec::new();
    for li in html.split("<li>").skip(1) {
        if results.len() >= 3 {
            break;
        }
        let Some(a_end) = li.find("</a>") else { continue };
        let a = &li[..a_end];
        let href = a
            .find("href=\"")
            .and_then(|i| a[i + 6..].find('"').map(|j| &a[i + 6..i + 6 + j]))
            .unwrap_or("");
        let title = a.rfind('>').map(|i| strip_tags(&a[i + 1..])).unwrap_or_default();
        let snippet = li
            .find("<cite>")
            .and_then(|i| li[i + 6..].find("</cite>").map(|j| &li[i + 6..i + 6 + j]))
            .map(strip_tags)
            .unwrap_or_default();
        let snippet: String = snippet.chars().take(600).collect();
        if title.is_empty() {
            continue;
        }
        results.push(json!({
            "title": title,
            "snippet": snippet,
            "url": format!("{{kiwix}}{href}"),
        }));
    }
    if results.is_empty() {
        json!({ "results": [], "note": "no matches in offline library" })
    } else {
        json!({ "results": results })
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_kiwix_results;
    use serde_json::Value;

    #[test]
    fn parses_kiwix_html() {
        let html = r#"<div class="results"><ul><li>
            <a href="/content/wikem_en_all_maxi_2026-10/Splinting">
              Splinting
            </a>
              <cite>...<b>splint</b> firmly but not too tight. Second sentence here</cite>
              <div class="book-title">from WikEM</div>
          </li>
          <li>
            <a href="/content/appropedia_en_all_maxi_2026-02/Long_Bone_Splinting">
              Long Bone Splinting
            </a>
              <cite>...other <b>splint</b> snippet</cite>
          </li>
        </ul></div>"#;
        let v: Value = serde_json::from_str(&parse_kiwix_results(html)).unwrap();
        let r = v["results"].as_array().unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["title"].as_str().unwrap(), "Splinting");
        assert_eq!(
            r[0]["url"].as_str().unwrap(),
            "{kiwix}/content/wikem_en_all_maxi_2026-10/Splinting"
        );
        assert!(r[0]["snippet"].as_str().unwrap().starts_with("...splint firmly"));
    }
}

async fn nws_forecast(http: &reqwest::Client, lat: f64, lon: f64) -> Result<Value, String> {    let points: Value = http
        .get(format!("https://api.weather.gov/points/{lat},{lon}"))
        .header("User-Agent", "syarc-sage")
        .send()
        .await
        .map_err(|e| format!("NWS points: {e}"))?
        .json()
        .await
        .map_err(|e| format!("NWS points parse: {e}"))?;
    let forecast_url = points
        .pointer("/properties/forecast")
        .and_then(|f| f.as_str())
        .ok_or("no forecast URL from NWS")?;
    let fc: Value = http
        .get(forecast_url)
        .header("User-Agent", "syarc-sage")
        .send()
        .await
        .map_err(|e| format!("NWS forecast: {e}"))?
        .json()
        .await
        .map_err(|e| format!("NWS forecast parse: {e}"))?;
    let mut periods: Vec<Value> = Vec::new();
    if let Some(list) = fc.pointer("/properties/periods").and_then(|p| p.as_array()) {
        for p in list.iter().take(4) {
            periods.push(json!({
                "name": p.get("name"),
                "temperature": p.get("temperature"),
                "temperatureUnit": p.get("temperatureUnit"),
                "windSpeed": p.get("windSpeed"),
                "shortForecast": p.get("shortForecast"),
            }));
        }
    }
    Ok(json!({ "periods": periods }))
}
