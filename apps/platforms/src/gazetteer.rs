//! Gazetteer: WA/OR place lookups, zip geocoding, summit and county
//! high-point facts, haversine distances. Datasets compiled in from
//! `data/*.json` (same files as the Python app's `mesh/gazetteer.py`).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::{json, Value};

#[derive(Default)]
pub struct Gazetteer {
    pub places: HashMap<String, [f64; 2]>,
    pub zips: HashMap<String, [f64; 2]>,
    pub summits: HashMap<String, Vec<Value>>,
    pub county_highpoints: HashMap<String, Value>,
    pub cascadia: HashMap<String, Value>,
}

pub fn gaz() -> &'static Gazetteer {
    static G: OnceLock<Gazetteer> = OnceLock::new();
    G.get_or_init(|| {
        let places: HashMap<String, [f64; 2]> =
            serde_json::from_str(include_str!("../data/wa_places.json")).unwrap_or_default();
        let zips: HashMap<String, [f64; 2]> =
            serde_json::from_str(include_str!("../data/wa_zips.json")).unwrap_or_default();
        let summits: HashMap<String, Vec<Value>> =
            serde_json::from_str(include_str!("../data/wa_summits.json")).unwrap_or_default();
        let county_highpoints: HashMap<String, Value> =
            serde_json::from_str(include_str!("../data/county_highpoints.json")).unwrap_or_default();
        let cascadia: HashMap<String, Value> =
            serde_json::from_str(include_str!("../data/cascadia_gazetteer.json")).unwrap_or_default();
        Gazetteer { places, zips, summits, county_highpoints, cascadia }
    })
}

pub fn geocode(place: &str) -> Option<[f64; 2]> {
    let g = gaz();
    let key = place.trim().to_lowercase();
    if let Some(c) = g.places.get(&key) {
        return Some(*c);
    }
    if let Ok(n) = key.parse::<i32>() {
        if let Some(c) = g.zips.get(&n.to_string()) {
            return Some(*c);
        }
    }
    None
}

pub fn haversine_km(a: [f64; 2], b: [f64; 2]) -> f64 {
    let (lat1, lon1, lat2, lon2) = (a[0].to_radians(), a[1].to_radians(), b[0].to_radians(), b[1].to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

/// Verified facts about a place from the Cascadia gazetteer, plus
/// summit/high-point elevation when the name matches.
pub fn place_facts(place: &str) -> Option<Value> {
    let g = gaz();
    let key = place.trim().to_lowercase();
    let coords = geocode(&key);

    // Summit elevation (highest of multiple same-named summits).
    if let Some(list) = g.summits.get(&key) {
        let mut best: Option<(f64, &Value)> = None;
        for s in list {
            // No elevation in wa_summits; fall back to coords + county.
            let _ = s;
            best = Some((0.0, s));
        }
        if let Some((_, s)) = best {
            return Some(json!({
                "place": key,
                "type": "summit",
                "lat": s.get("lat"), "lon": s.get("lon"),
                "county": s.get("county"),
                "coords": coords.map(|c| c.to_vec()),
            }));
        }
    }
    // County high point.
    for (county, hp) in &g.county_highpoints {
        if hp.get("name").and_then(|n| n.as_str()).map(|n| n.to_lowercase()) == Some(key.clone()) {
            return Some(json!({
                "place": hp.get("name"), "type": "county_highpoint",
                "county": county, "elevation_m": hp.get("elev_m"),
                "lat": hp.get("lat"), "lon": hp.get("lon"),
            }));
        }
    }
    // Cascadia gazetteer place entry.
    if let Some(places) = g.cascadia.get("places").and_then(|p| p.as_object()) {
        if let Some(entry) = places.get(&key) {
            return Some(json!({
                "place": key, "type": "place",
                "population": entry.get("population"),
                "elevation_m": entry.get("elevation_m"),
                "facts": entry.get("facts"),
                "connections": entry.get("connections"),
                "coords": coords.map(|c| c.to_vec()),
            }));
        }
    }
    if coords.is_some() {
        return Some(json!({ "place": key, "type": "located", "coords": coords.map(|c| c.to_vec()) }));
    }
    None
}

/// Straight-line distance between two places (km/miles).
pub fn distance(a: &str, b: &str) -> Value {
    let (Some(ca), Some(cb)) = (geocode(a), geocode(b)) else {
        return json!({ "error": format!("unknown place '{a}' or '{b}' in sage's coverage area") });
    };
    let km = haversine_km(ca, cb);
    json!({
        "from": a.to_lowercase(), "to": b.to_lowercase(),
        "straight_line_km": (km * 10.0).round() / 10.0,
        "straight_line_miles": (km * 0.6214 * 10.0).round() / 10.0,
        "note": "straight-line (great circle); road distance is usually 20-40% longer in this terrain",
    })
}
