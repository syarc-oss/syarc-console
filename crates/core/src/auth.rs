use axum::extract::{Form, State};
use axum::response::{Html, IntoResponse, Redirect};
use serde::Deserialize;
use sqlx::Row;
use tower_sessions::Session;

use syarc_plugin_api::{hash_password, verify_password, User, USER_ID_KEY};

use crate::SharedState;

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct RegisterForm {
    pub username: String,
    pub email: String,
    pub password: String,
}

pub async fn ensure_first_admin_prompt(db: &sqlx::SqlitePool) {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    if count == 0 {
        println!("No users exist. Register the first account at /register - it becomes admin.");
    }
}

pub async fn login_form(State(state): State<SharedState>) -> Html<String> {
    Html(state.templates.render("login.html", serde_json::json!({ "error": "" })))
}

pub async fn login(
    State(state): State<SharedState>,
    session: Session,
    Form(form): Form<LoginForm>,
) -> impl IntoResponse {
    let user: Option<User> = sqlx::query_as(
        "SELECT id, username, email, password_hash, role, is_active \
         FROM users WHERE username = ? AND is_active = 1",
    )
    .bind(&form.username)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    match user {
        Some(user) if verify_password(&user.password_hash, &form.password) => {
            session.insert(USER_ID_KEY, user.id).await.ok();
            sqlx::query("UPDATE users SET last_login_at = datetime('now') WHERE id = ?")
                .bind(user.id)
                .execute(&state.db)
                .await
                .ok();
            Redirect::to("/").into_response()
        }
        _ => Html(state.templates.render(
            "login.html",
            serde_json::json!({ "error": "Invalid username or password" }),
        ))
        .into_response(),
    }
}

pub async fn register_form(State(state): State<SharedState>) -> Html<String> {
    Html(state.templates.render("register.html", serde_json::json!({ "error": "" })))
}

pub async fn register(
    State(state): State<SharedState>,
    session: Session,
    Form(form): Form<RegisterForm>,
) -> impl IntoResponse {
    let existing: Option<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE username = ? OR email = ?")
            .bind(&form.username)
            .bind(&form.email)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);

    if existing.is_some() {
        return Html(state.templates.render(
            "register.html",
            serde_json::json!({ "error": "Username or email already taken" }),
        ))
        .into_response();
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    let role = if count == 0 { "admin" } else { "user" };

    let result = sqlx::query(
        "INSERT INTO users (username, email, password_hash, role) \
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(&form.username)
    .bind(&form.email)
    .bind(hash_password(&form.password))
    .bind(role)
    .fetch_one(&state.db)
    .await;

    match result {
        Ok(row) => {
            let id: i64 = row.get("id");
            let user = User {
                id,
                username: form.username.clone(),
                email: form.email.clone(),
                password_hash: String::new(),
                role: role.to_string(),
                is_active: 1,
            };
            for plugin in crate::registry::all_apps() {
                if let Err(e) = plugin.on_user_registered(&state.db, &user).await {
                    eprintln!(
                        "plugin {} on_user_registered failed: {e}",
                        plugin.manifest().name
                    );
                }
            }
            session.insert(USER_ID_KEY, id).await.ok();
            Redirect::to("/").into_response()
        }
        Err(e) => Html(state.templates.render(
            "register.html",
            serde_json::json!({ "error": format!("Registration failed: {e}") }),
        ))
        .into_response(),
    }
}

pub async fn logout(session: Session) -> impl IntoResponse {
    session.clear().await;
    Redirect::to("/login")
}
