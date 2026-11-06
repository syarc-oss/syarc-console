use axum::extract::State;
use axum::response::Html;
use serde_json::json;

use syarc_plugin_api::RequireUser;
use sqlx::Row;

use crate::SharedState;

pub async fn dashboard(user: RequireUser, State(state): State<SharedState>) -> Html<String> {
    let is_admin = user.0.role == "admin";
    let mut app_list: Vec<serde_json::Value> = Vec::new();
    let rows = if is_admin {
        sqlx::query(
            "SELECT id, name, url_prefix FROM apps \
             WHERE is_active = 1 AND show_on_dashboard = 1 ORDER BY name",
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        sqlx::query(
            "SELECT a.id, a.name, a.url_prefix FROM apps a \
             JOIN user_app_permissions p ON p.app_id = a.id AND p.user_id = ? \
             WHERE a.is_active = 1 AND a.show_on_dashboard = 1 ORDER BY a.name",
        )
        .bind(user.0.id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    };

    for row in rows {
        let id: String = row.get("id");
        let name: String = row.get("name");
        let url: String = row.get("url_prefix");
        app_list.push(json!({ "id": id, "name": name, "url": url }));
    }

    Html(state.templates.render(
        "dashboard.html",
        json!({
            "username": user.0.username,
            "is_admin": is_admin,
            "apps": app_list,
        }),
    ))
}

pub async fn about() -> Html<&'static str> {
    Html("<h1>syarc-console</h1><p>Rust control console for syarc apps.</p>")
}
