mod auth;
mod state;
mod templates;

pub use auth::{hash_password, verify_password};
pub use state::{current_user, AppState, RequireAdmin, RequireUser, SharedState, User, USER_ID_KEY};
pub use templates::Templates;

use axum::Router;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppManifest {
    pub id: &'static str,
    pub name: &'static str,
    pub url_prefix: &'static str,
    pub icon_class: &'static str,
    pub category: Option<&'static str>,
    pub show_on_dashboard: bool,
}

impl AppManifest {
    pub const fn new(id: &'static str, name: &'static str, url_prefix: &'static str) -> Self {
        Self {
            id,
            name,
            url_prefix,
            icon_class: "bi-box",
            category: None,
            show_on_dashboard: true,
        }
    }

    pub const fn icon(mut self, icon: &'static str) -> Self {
        self.icon_class = icon;
        self
    }

    pub const fn category(mut self, category: &'static str) -> Self {
        self.category = Some(category);
        self
    }

    pub const fn hidden(mut self) -> Self {
        self.show_on_dashboard = false;
        self
    }
}

/// Shared handle to the console's database pool, handed to plugins.
pub type Db = sqlx::SqlitePool;

#[async_trait::async_trait]
pub trait AppPlugin: Send + Sync {
    fn manifest(&self) -> &'static AppManifest;

    /// Fully-built router (state not yet applied), nested by the host at
    /// `manifest().url_prefix`.
    fn router(&self, state: SharedState) -> Router<SharedState> {
        let _ = state;
        Router::new()
    }

    /// Called once at startup, after migrations, to seed app records.
    async fn on_startup(&self, _db: &Db) -> Result<(), sqlx::Error> {
        Ok(())
    }

    /// Called after a new user registers; override to grant permissions
    /// or create defaults.
    async fn on_user_registered(&self, _db: &Db, _user: &User) -> Result<(), sqlx::Error> {
        Ok(())
    }
}
