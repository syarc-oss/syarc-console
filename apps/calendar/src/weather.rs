//! NWS forecast + alerts and Open-Meteo current-conditions fetcher for the
//! calendar app, ported from the fastmain weather app. Results are cached
//! in-process and served stale on fetch failure so pages degrade gracefully
//! offline (LAN-first philosophy).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

const POINTS_URL: &str = "https://api.weather.gov/points/{lat},{lon}";
const ALERTS_URL: &str = "https://api.weather.gov/alerts/active?point={lat},{lon}";
const OPEN_METEO_URL: &str = "https://api.open-meteo.com/v1/forecast";

const FORECAST_TTL: Duration = Duration::from_secs(30 * 60);
const FORECAST_STALE_MAX: Duration = Duration::from_secs(6 * 3600);
const CURRENT_TTL: Duration = Duration::from_secs(10 * 60);
const CURRENT_STALE_MAX: Duration = Duration::from_secs(2 * 3600);
const ALERTS_TTL: Duration = Duration::from_secs(30 * 60);

const USER_AGENT: &str = "syarc-console-weather (self-hosted)";

#[derive(Default)]
struct LocationCache {
    forecast_ts: Option<Instant>,
    forecast: Vec<Value>,
    forecast_error: Option<String>,
    alerts_ts: Option<Instant>,
    alerts: Vec<Value>,
    alerts_error: Option<String>,
    current_ts: Option<Instant>,
    current: Option<Value>,
    current_error: Option<String>,
    points_url: Option<Option<String>>,
}

static CACHES: Mutex<Option<HashMap<(i64, i64), LocationCache>>> = Mutex::new(None);

fn cache_for(lat: f64, lon: f64) -> (i64, i64) {
    ((lat * 10_000.0).round() as i64, (lon * 10_000.0).round() as i64)
}

async fn fetch_json(url: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header("Accept", "application/geo+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

fn parse_periods(forecast: &Value) -> Vec<Value> {
    let empty = Vec::new();
    let periods = forecast
        .get("properties")
        .and_then(|p| p.get("periods"))
        .and_then(|p| p.as_array())
        .unwrap_or(&empty);
    periods
        .iter()
        .take(14)
        .map(|p| {
            let wind_speed = p.get("windSpeed").and_then(|v| v.as_str()).unwrap_or("?");
            let wind_dir = p.get("windDirection").and_then(|v| v.as_str()).unwrap_or("");
            json_period(p, wind_speed, wind_dir)
        })
        .collect()
}

fn json_period(p: &Value, wind_speed: &str, wind_dir: &str) -> Value {
    serde_json::json!({
        "name": p.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "start_time": p.get("startTime").and_then(|v| v.as_str()).unwrap_or(""),
        "temp": p.get("temperature").cloned().unwrap_or(Value::Null),
        "unit": p.get("temperatureUnit").and_then(|v| v.as_str()).unwrap_or("F"),
        "wind": format!("{wind_speed} {wind_dir}").trim().to_string(),
        "short": p.get("shortForecast").and_then(|v| v.as_str()).unwrap_or(""),
        "detail": p.get("detailedForecast").and_then(|v| v.as_str()).unwrap_or(""),
        "is_day": p.get("isDaytime").and_then(|v| v.as_bool()).unwrap_or(false),
    })
}

fn parse_alerts(payload: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(features) = payload.get("features").and_then(|f| f.as_array()) {
        for f in features.iter().take(6) {
            let props = f.get("properties").cloned().unwrap_or(Value::Null);
            out.push(serde_json::json!({
                "event": props.get("event").and_then(|v| v.as_str()).unwrap_or("Alert"),
                "headline": props
                    .get("headline")
                    .and_then(|v| v.as_str())
                    .or(props.get("event").and_then(|v| v.as_str()))
                    .unwrap_or(""),
                "severity": props.get("severity").and_then(|v| v.as_str()).unwrap_or("Unknown"),
            }));
        }
    }
    out
}

const WEATHER_CODES: &[(i64, &str)] = &[
    (0, "clear"), (1, "mostly clear"), (2, "partly cloudy"), (3, "overcast"),
    (45, "fog"), (48, "freezing fog"), (51, "light drizzle"), (53, "drizzle"),
    (55, "heavy drizzle"), (56, "freezing drizzle"), (57, "heavy freezing drizzle"),
    (61, "light rain"), (63, "rain"), (65, "heavy rain"), (66, "freezing rain"),
    (67, "heavy freezing rain"), (71, "light snow"), (73, "snow"), (75, "heavy snow"),
    (77, "snow grains"), (80, "light showers"), (81, "showers"), (82, "heavy showers"),
    (85, "snow showers"), (86, "heavy snow showers"), (95, "thunderstorm"),
    (96, "thunderstorm with hail"), (99, "severe thunderstorm with hail"),
];

pub async fn get_forecast(lat: f64, lon: f64) -> (Vec<Value>, bool, Option<String>) {
    let key = cache_for(lat, lon);
    let now = Instant::now();
    {
        let mut caches = CACHES.lock().unwrap();
        let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
        if let Some(ts) = c.forecast_ts {
            if now.duration_since(ts) < FORECAST_TTL {
                return (c.forecast.clone(), false, c.forecast_error.clone());
            }
            if now.duration_since(ts) < FORECAST_STALE_MAX && !c.forecast.is_empty() {
                return (
                    c.forecast.clone(),
                    true,
                    Some("showing cached forecast (offline?)".into()),
                );
            }
        }
    }

    let points_url = {
        let mut caches = CACHES.lock().unwrap();
        let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
        if let Some(cached) = &c.points_url {
            cached.clone()
        } else {
            None
        }
    };

    let mut result: Result<Vec<Value>, String> = Err("never fetched".into());
    let url: Option<String> = match points_url {
        Some(u) => Some(u),
        None => {
            let lookup = fetch_json(&POINTS_URL.replace("{lat}", &format!("{lat}")).replace("{lon}", &format!("{lon}"))).await;
            let u = match lookup {
                Ok(data) => data
                    .get("properties")
                    .and_then(|p| p.get("forecast"))
                    .and_then(|f| f.as_str())
                    .map(String::from),
                Err(e) => {
                    result = Err(format!("NWS points lookup failed: {e}"));
                    None
                }
            };
            let mut caches = CACHES.lock().unwrap();
            let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
            c.points_url = Some(u.clone());
            u
        }
    };

    if result.is_err() {
        result = match url {
            Some(u) => match fetch_json(&u).await {
                Ok(data) => Ok(parse_periods(&data)),
                Err(e) => Err(e),
            },
            None => Err("no gridpoint for location".into()),
        };
    }

    let mut caches = CACHES.lock().unwrap();
    let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
    match result {
        Ok(periods) => {
            c.forecast = periods.clone();
            c.forecast_error = None;
            c.forecast_ts = Some(now);
            (periods, false, None)
        }
        Err(e) => {
            c.forecast_error = Some(e.clone());
            c.forecast_ts = Some(now);
            (c.forecast.clone(), false, Some(e))
        }
    }
}

pub async fn get_current(lat: f64, lon: f64) -> (Option<Value>, bool, Option<String>) {
    let key = cache_for(lat, lon);
    let now = Instant::now();
    {
        let mut caches = CACHES.lock().unwrap();
        let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
        if let Some(ts) = c.current_ts {
            if now.duration_since(ts) < CURRENT_TTL {
                return (c.current.clone(), false, c.current_error.clone());
            }
            if now.duration_since(ts) < CURRENT_STALE_MAX && c.current.is_some() {
                return (
                    c.current.clone(),
                    true,
                    Some("showing cached conditions (offline?)".into()),
                );
            }
        }
    }

    let url = format!(
        "{OPEN_METEO_URL}?latitude={lat}&longitude={lon}\
         &current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,\
         wind_speed_10m,wind_direction_10m,wind_gusts_10m\
         &temperature_unit=fahrenheit&wind_speed_unit=mph&timezone=auto"
    );
    let result = fetch_json(&url).await;

    let mut caches = CACHES.lock().unwrap();
    let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
    c.current_ts = Some(now);
    match result {
        Ok(data) => {
            let cur = data.get("current").cloned().unwrap_or(Value::Null);
            let code = cur.get("weather_code").and_then(|v| v.as_i64()).unwrap_or(0);
            let conditions = WEATHER_CODES
                .iter()
                .find(|(k, _)| *k == code)
                .map(|(_, name)| name.to_string())
                .unwrap_or_else(|| format!("code {code}"));
            let parsed = serde_json::json!({
                "time": cur.get("time").and_then(|v| v.as_str()),
                "temp_f": cur.get("temperature_2m"),
                "feels_like_f": cur.get("apparent_temperature"),
                "humidity": cur.get("relative_humidity_2m"),
                "wind_mph": cur.get("wind_speed_10m"),
                "wind_gust_mph": cur.get("wind_gusts_10m"),
                "wind_dir": cur.get("wind_direction_10m"),
                "conditions": conditions,
            });
            c.current = Some(parsed.clone());
            c.current_error = None;
            (Some(parsed), false, None)
        }
        Err(e) => {
            c.current_error = Some(e.clone());
            (c.current.clone(), false, Some(e))
        }
    }
}

pub async fn get_alerts(lat: f64, lon: f64) -> (Vec<Value>, Option<String>) {
    let key = cache_for(lat, lon);
    let now = Instant::now();
    {
        let mut caches = CACHES.lock().unwrap();
        let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
        if let Some(ts) = c.alerts_ts {
            if now.duration_since(ts) < ALERTS_TTL {
                return (c.alerts.clone(), c.alerts_error.clone());
            }
        }
    }

    let url = ALERTS_URL
        .replace("{lat}", &format!("{lat}"))
        .replace("{lon}", &format!("{lon}"));
    let result = fetch_json(&url).await;

    let mut caches = CACHES.lock().unwrap();
    let c = { let m = caches.get_or_insert_with(Default::default); m.entry(key) }.or_default();
    c.alerts_ts = Some(now);
    match result {
        Ok(data) => {
            c.alerts = parse_alerts(&data);
            c.alerts_error = None;
            (c.alerts.clone(), None)
        }
        Err(e) => {
            c.alerts_error = Some(e.clone());
            (c.alerts.clone(), Some(e))
        }
    }
}
