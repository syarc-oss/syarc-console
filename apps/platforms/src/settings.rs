use sqlx::SqlitePool;

/// Console settings relevant to the platforms app, sourced from the
/// `settings` table with env-var fallbacks.
#[derive(Debug, Clone)]
pub struct MeshSettings {
    pub broker_enabled: bool,
    pub broker_url: String,
    pub broker_token: String,
    pub instance_name: String,
    pub channel_replay_max_secs: i64,
    pub sage_enabled: bool,
    pub sage_base_url: String,
    pub sage_model: String,
    pub sage_channels: Vec<String>,
    pub sage_cooldown_secs: i64,
    /// Senders (comma-separated long names, case-insensitive) exempt from
    /// the per-sender cooldown — handy for testing.
    pub sage_cooldown_exempt: Vec<String>,
    pub sage_max_chars: i64,
    pub sage_workers: i64,
    pub sage_kiwix_url: String,
    pub radio_enabled: bool,
    pub radio_host: String,
    /// Bluetooth SPP transport: paired device MAC (binds /dev/rfcommN),
    /// "1" to just use the pre-bound rfcomm device, or empty for wired.
    pub radio_bluetooth: String,
    pub broker_serve_enabled: bool,
}

async fn get_setting(db: &SqlitePool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl MeshSettings {
    pub async fn load(db: &SqlitePool) -> Self {
        macro_rules! s {
            ($k:expr) => { get_setting(db, $k).await };
        }
        Self {
            broker_enabled: s!("mesh_broker_enabled").as_deref() == Some("1")
                || std::env::var("MESH_BROKER_ENABLED").is_ok(),
            broker_url: s!("mesh_broker_url")
                .unwrap_or_else(|| env_or("MESH_BROKER_URL", "http://127.0.0.1:8090")),
            broker_token: s!("mesh_broker_token")
                .unwrap_or_else(|| env_or("MESH_BROKER_TOKEN", "")),
            instance_name: s!("mesh_instance_name")
                .unwrap_or_else(|| env_or("MESH_INSTANCE_NAME", "local")),
            channel_replay_max_secs: s!("mesh_channel_replay_max_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(900),
            sage_enabled: s!("sage_enabled").as_deref() == Some("1")
                || std::env::var("SAGE_ENABLED").is_ok(),
            sage_base_url: s!("sage_base_url")
                .unwrap_or_else(|| env_or("SAGE_BASE_URL", "http://127.0.0.1:8080/v1")),
            sage_model: s!("sage_model").unwrap_or_else(|| env_or("SAGE_MODEL", "llama")),
            sage_channels: s!("sage_channels")
                .unwrap_or_else(|| "Primary".into())
                .split(',')
                .map(|c| c.trim().to_lowercase())
                .filter(|c| !c.is_empty())
                .collect(),
            sage_cooldown_secs: s!("sage_cooldown_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            sage_cooldown_exempt: s!("sage_cooldown_exempt")
                .unwrap_or_default()
                .split(',')
                .map(|c| c.trim().to_lowercase())
                .filter(|c| !c.is_empty())
                .collect(),
            sage_max_chars: s!("sage_max_chars")
                .and_then(|v| v.parse().ok())
                .filter(|&v| v > 0)
                .unwrap_or(crate::sage::DEFAULT_SAGE_MAX_CHARS as i64),
            sage_workers: s!("sage_workers")
                .and_then(|v| v.parse().ok())
                .filter(|&v| (1..=8).contains(&v))
                .unwrap_or(2),
            sage_kiwix_url: s!("sage_kiwix_url")
                .unwrap_or_else(|| env_or("SAGE_KIWIX_URL", "http://192.168.1.10:8098")),
            radio_enabled: s!("mesh_radio_enabled").as_deref() == Some("1")
                || std::env::var("MESH_RADIO_ENABLED").is_ok(),
            radio_host: s!("mesh_radio_host")
                .unwrap_or_else(|| env_or("MESH_RADIO_HOST", "127.0.0.1:4000")),
            radio_bluetooth: s!("mesh_radio_bluetooth")
                .unwrap_or_else(|| env_or("MESH_RADIO_BLUETOOTH", "")),
            broker_serve_enabled: s!("mesh_broker_serve_enabled").as_deref() == Some("1")
                || std::env::var("MESH_BROKER_SERVE_ENABLED").is_ok(),
        }
    }
}
