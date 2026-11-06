use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::RequestPartsExt;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use tower_sessions::Session;

use crate::templates::Templates;
use crate::Db;

pub const USER_ID_KEY: &str = "user_id";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub role: String,
    pub is_active: i64,
}

pub struct AppState {
    pub db: Db,
    /// Separate (SQLCipher-encrypted) pool for the pecuniary app tables
    /// (checkbook/ledger/pms/stocks). Lives outside the LiteFS mount.
    pub pecuniary: Db,
    pub templates: Templates,
}

pub type SharedState = Arc<AppState>;

pub async fn current_user(
    session: &Session,
    state: &SharedState,
) -> Result<Option<User>, sqlx::Error> {
    let Ok(Some(user_id)) = session.get::<i64>(USER_ID_KEY).await else {
        return Ok(None);
    };
    sqlx::query_as::<_, User>(
        "SELECT id, username, email, password_hash, role, is_active \
         FROM users WHERE id = ? AND is_active = 1",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
}

/// Extractor that redirects unauthenticated requests to the login page.
pub struct RequireUser(pub User);

impl FromRequestParts<SharedState> for RequireUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let session = parts
            .extract::<Session>()
            .await
            .map_err(internal_error)?;
        match current_user(&session, state).await {
            Ok(Some(user)) => Ok(RequireUser(user)),
            _ => Err(Redirect::to("/login").into_response()),
        }
    }
}

/// Extractor that additionally requires the admin role.
pub struct RequireAdmin(pub User);

impl FromRequestParts<SharedState> for RequireAdmin {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let user = RequireUser::from_request_parts(parts, state)
            .await
            .map_err(|_| Redirect::to("/login").into_response())?
            .0;
        if user.role == "admin" {
            Ok(RequireAdmin(user))
        } else {
            Err((StatusCode::FORBIDDEN, Html("<h1>403 Forbidden</h1>")).into_response())
        }
    }
}

fn internal_error<E>(_: E) -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
