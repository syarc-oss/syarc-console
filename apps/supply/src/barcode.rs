//! Barcode lookup: local SQLite product database plus concurrent online
//! providers (Open Food Facts, UPCitemDB trial), with in-memory usage stats.
//! Ported from the Python implementation's barcode_lookup.py.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

const DB_PATH: &str = "/usr/local/library/barcode_products.db";

static LOOKUPS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

async fn pool() -> Option<&'static sqlx::SqlitePool> {
    static POOL: OnceLock<Option<sqlx::SqlitePool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let opts = SqliteConnectOptions::new()
            .filename(DB_PATH)
            .read_only(true);
        Some(
            SqlitePoolOptions::new()
                .max_connections(2)
                .connect_lazy_with(opts),
        )
    })
    .as_ref()
}

pub async fn lookup_local(barcode: &str) -> Option<String> {
    if !std::path::Path::new(DB_PATH).exists() {
        return None;
    }
    let pool = pool().await?;
    let row = sqlx::query("SELECT name FROM products WHERE barcode = ?")
        .bind(barcode.trim())
        .fetch_optional(pool)
        .await
        .ok()??;
    row.try_get::<String, _>(0).ok()
}

pub async fn lookup_online(barcode: &str) -> Option<String> {
    let barcode = barcode.trim().to_string();
    if barcode.is_empty() {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .danger_accept_invalid_certs(true)
        .build()
        .ok()?;
    let (off, upc) = tokio::join!(
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fetch_openfoodfacts(&client, &barcode)
        ),
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fetch_upcitemdb(&client, &barcode)
        ),
    );
    if let Ok(Some(name)) = off {
        return Some(name);
    }
    if let Ok(Some(name)) = upc {
        return Some(name);
    }
    None
}

async fn fetch_openfoodfacts(client: &reqwest::Client, barcode: &str) -> Option<String> {
    let url = format!(
        "https://world.openfoodfacts.org/api/v2/product/{barcode}.json?fields=product_name,brands"
    );
    let resp = client.get(&url).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let product = body.get("product")?;
    let name = product.get("product_name")?.as_str()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let brands = product
        .get("brands")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .trim();
    if brands.is_empty() {
        Some(name)
    } else {
        Some(format!("{name} ({brands})"))
    }
}

async fn fetch_upcitemdb(client: &reqwest::Client, barcode: &str) -> Option<String> {
    let url = format!("https://api.upcitemdb.com/prod/trial/lookup?upc={barcode}");
    let resp = client.get(&url).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let title = body
        .get("items")?
        .get(0)?
        .get("title")?
        .as_str()?
        .trim()
        .to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

pub fn record_lookup(found: bool) {
    LOOKUPS.fetch_add(1, Ordering::Relaxed);
    if !found {
        MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn reset_stats() {
    LOOKUPS.store(0, Ordering::Relaxed);
    MISSES.store(0, Ordering::Relaxed);
}

pub async fn db_status() -> serde_json::Value {
    let lookups = LOOKUPS.load(Ordering::Relaxed);
    let misses = MISSES.load(Ordering::Relaxed);
    let mut result = serde_json::json!({
        "available": false,
        "db_count": 0,
        "db_size_mb": 0,
        "lookups": lookups,
        "misses": misses,
        "miss_rate": if lookups > 0 {
            (misses as f64 / lookups as f64 * 1000.0).round() / 10.0
        } else {
            0.0
        },
        "last_built": serde_json::Value::Null,
    });

    let metadata = match std::fs::metadata(DB_PATH) {
        Ok(m) => m,
        Err(_) => return result,
    };
    if let Some(pool) = pool().await {
        let count: Option<i64> = sqlx::query("SELECT COUNT(*) FROM products")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<i64, _>(0).ok());
        if let Some(db_count) = count {
            result["available"] = serde_json::Value::Bool(true);
            result["db_count"] = serde_json::json!(db_count);
            result["db_size_mb"] =
                serde_json::json!((metadata.len() as f64 / (1024.0 * 1024.0) * 10.0).round() / 10.0);
            if let Ok(modified) = metadata.modified() {
                let dt: chrono::DateTime<chrono::Utc> = modified.into();
                result["last_built"] = serde_json::json!(dt.to_rfc3339());
            }
        }
    }
    result
}
