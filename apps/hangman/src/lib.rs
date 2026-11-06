//! Hangman app: the default game every new registration gets access to.
//! Ported from the Python (fastmain) implementation; the 10k-word dictionary
//! is embedded at compile time and served over /hangman/api/get-word.

use std::sync::OnceLock;

use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;

use syarc_plugin_api::{RequireUser, SharedState};

const WORDS: &str = include_str!("../words.txt");

const FALLBACK_WORDS: &[&str] = &["PYTHON", "FASTAPI", "BACKEND", "ROUTER", "TEMPLATE"];

fn words() -> &'static Vec<String> {
    static WORD_LIST: OnceLock<Vec<String>> = OnceLock::new();
    WORD_LIST.get_or_init(|| {
        let list: Vec<String> = WORDS
            .lines()
            .map(|l| l.trim().to_uppercase())
            .filter(|l| !l.is_empty())
            .collect();
        if list.is_empty() {
            FALLBACK_WORDS.iter().map(|s| s.to_string()).collect()
        } else {
            list
        }
    })
}

pub struct HangmanPlugin;

impl HangmanPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("hangman", "Hangman Game", "/hangman")
            .icon("bi-controller");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for HangmanPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/hangman", get(page))
            .route("/hangman/", get(page))
            .route("/hangman/api/get-word", get(get_word))
            .with_state(state)
    }

    /// Grant hangman to every active human account (idempotent): existing
    /// installs were seeded before this app existed, so the one-time
    /// default-permission seeding missed them.
    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO user_app_permissions (user_id, app_id) \
             SELECT id, 'hangman' FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%'",
        )
        .execute(db)
        .await?;
        Ok(())
    }

    /// Hangman is the default app: every new registration gets it.
    async fn on_user_registered(
        &self,
        db: &syarc_plugin_api::Db,
        user: &syarc_plugin_api::User,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO user_app_permissions (user_id, app_id) VALUES (?, 'hangman')",
        )
        .bind(user.id)
        .execute(db)
        .await?;
        Ok(())
    }
}

async fn page(
    axum::extract::State(state): axum::extract::State<SharedState>,
    RequireUser(_user): RequireUser,
) -> Response {
    Html(state.templates.render("hangman.html", json!({}))).into_response()
}

async fn get_word(RequireUser(_user): RequireUser) -> Response {
    use rand::Rng;
    let list = words();
    let word = list[rand::thread_rng().gen_range(0..list.len())].clone();
    axum::Json(json!({ "word": word })).into_response()
}
