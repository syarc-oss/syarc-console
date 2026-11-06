//! Platforms plugin: messaging + mesh bridge. See crate README for the
//! porting status of each component versus the Python original.

mod broker;
pub mod broker_server;
mod channels;
mod gazetteer;
mod hub;
#[allow(dead_code)]
mod mesh_admin;
pub mod media;
mod messaging;
mod meshcore;
pub mod radio;
mod remote_admin;
mod sage;
mod settings;

use axum::extract::State;
use axum::response::Redirect;
use axum::routing::get;
use axum::Router;
use serde_json::json;

use syarc_plugin_api::{AppManifest, AppPlugin, Db, SharedState};

pub struct PlatformsPlugin;

impl PlatformsPlugin {
    pub const MANIFEST: AppManifest = AppManifest::new("platforms", "Messaging", "/platforms/messaging")
        .icon("bi-envelope");
}

#[async_trait::async_trait]
impl AppPlugin for PlatformsPlugin {
    fn manifest(&self) -> &'static AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, _state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/mesh/ws", get(broker_server::ws_handler))
            .merge(messaging::router())
            .merge(channels::router())
            .merge(mesh_admin::router())
            .merge(meshcore::router())
            .merge(media::router())
            .merge(remote_admin::router())
            .route(
                "/knowledge",
                get(|State(state): State<SharedState>| async move {
                    let s = settings::MeshSettings::load(&state.db).await;
                    Redirect::temporary(if s.sage_kiwix_url.is_empty() {
                        "http://192.168.1.10:8098"
                    } else {
                        s.sage_kiwix_url.trim_end_matches('/')
                    })
                }),
            )
    }

    async fn on_startup(&self, db: &Db) -> Result<(), sqlx::Error> {
        // Seed MeshSettings rows so every knob is visible/editable in
        // Admin -> Settings; existing values are never overwritten.
        let sage_max = crate::sage::DEFAULT_SAGE_MAX_CHARS.to_string();
        for (k, v) in [
            ("mesh_broker_url", "http://127.0.0.1:8090"),
            ("mesh_instance_name", "local"),
            ("mesh_channel_replay_max_secs", "900"),
            ("mesh_radio_bluetooth", ""),
            ("mesh_media_dir", "/usr/local/syarc/media"),
            ("sage_cooldown_secs", "60"),
            ("sage_cooldown_exempt", ""),
            ("sage_max_chars", sage_max.as_str()),
            ("sage_workers", "2"),
            ("sage_kiwix_url", "http://192.168.1.10:8098"),
        ] {
            sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, ?)")
                .bind(k)
                .bind(v)
                .execute(db)
                .await?;
        }
        media::init(db).await;
        broker::start_broker_if_enabled(db).await;
        Ok(())
    }
}


// Silence unused warnings for modules referenced only via runtime paths.
#[allow(dead_code)]
fn _unused(_v: serde_json::Value) {
    let _ = json!({});
}
