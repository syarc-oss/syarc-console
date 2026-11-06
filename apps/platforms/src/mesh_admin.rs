//! Mesh Bridge admin pages (subset of routers/mesh_bridge.py):
//! status overview, node directory, traffic log, bridge mapping CRUD.

use axum::extract::{Form, Path, State};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use syarc_plugin_api::{RequireAdmin, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/platforms/messaging/mesh", get(mappings))
        .route("/platforms/messaging/mesh/", get(mappings))
        .route("/platforms/messaging/mesh/mappings", get(mappings).post(mapping_create))
        .route("/platforms/messaging/mesh/mappings/{id}/delete", post(mapping_delete))
        .route("/platforms/messaging/mesh/mappings/{id}/toggle", post(mapping_toggle))
}

fn page(state: &SharedState, template: &str, ctx: serde_json::Value) -> Html<String> {
    Html(state.templates.render(template, ctx))
}

async fn mappings(_user: RequireAdmin, State(state): State<SharedState>) -> Html<String> {
    let rows = sqlx::query(
        "SELECT m.id, m.enabled, m.owner_user_id, m.to_user, m.last_forwarded_at, m.conversation_id, \
                n.long_name, n.node_id, c.subject \
         FROM mesh_bridge_mappings m \
         JOIN mesh_nodes n ON n.id = m.mesh_node_id \
         JOIN conversations c ON c.id = m.conversation_id \
         ORDER BY m.created_at DESC",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        json!({
            "id": r.get::<String, _>("id"),
            "enabled": r.get::<i64, _>("enabled") == 1,
            "owner_user_id": r.get::<Option<i64>, _>("owner_user_id"),
            "to_user": r.get::<Option<String>, _>("to_user"),
            "last_forwarded_at": r.get::<String, _>("last_forwarded_at"),
            "conversation_id": r.get::<String, _>("conversation_id"),
            "node": { "long_name": r.get::<String, _>("long_name"), "node_id": r.get::<String, _>("node_id") },
            "conversation": { "subject": r.get::<String, _>("subject") },
        })
    })
    .collect::<Vec<_>>();
    let nodes: Vec<_> = rows.iter().filter(|m| !m["node"]["node_id"].as_str().unwrap_or("").starts_with("#chan")).cloned().collect();
    let channels: Vec<_> = rows.iter().filter(|m| m["node"]["node_id"].as_str().unwrap_or("").starts_with("#chan")).cloned().collect();
    let users: Vec<serde_json::Value> = sqlx::query(
        "SELECT id, username FROM users WHERE is_active = 1 ORDER BY username",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| json!({ "id": r.get::<i64, _>("id"), "username": r.get::<String, _>("username") }))
    .collect();
    page(&state, "mesh_mappings.html", json!({ "nodes": nodes, "channels": channels, "users": users }))
}

#[derive(Deserialize)]
struct MappingForm {
    node_id: String,
    conversation_id: String,
    owner_user_id: Option<String>,
}

async fn mapping_create(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Form(form): Form<MappingForm>,
) -> impl IntoResponse {
    let node_pk: Option<String> = sqlx::query_scalar("SELECT id FROM mesh_nodes WHERE node_id = ?")
        .bind(form.node_id.trim())
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);
    let (Some(node_pk), Ok(_)) = (node_pk, uuid::Uuid::parse_str(&form.conversation_id)) else {
        return Redirect::to("/platforms/messaging/mesh/mappings");
    };
    let owner = form
        .owner_user_id
        .as_deref()
        .and_then(|o| o.parse::<i64>().ok());
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO mesh_bridge_mappings (id, conversation_id, mesh_node_id, owner_user_id) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(form.conversation_id.trim())
    .bind(&node_pk)
    .bind(owner)
    .execute(&state.db)
    .await;
    if let Some(b) = crate::broker::broker() {
        let _ = b.sync_subscriptions().await;
    }
    Redirect::to("/platforms/messaging/mesh/mappings")
}

async fn mapping_delete(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let _ = sqlx::query("DELETE FROM mesh_bridge_mappings WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await;
    if let Some(b) = crate::broker::broker() {
        let _ = b.sync_subscriptions().await;
    }
    Redirect::to("/platforms/messaging/mesh/mappings")
}

async fn mapping_toggle(
    _admin: RequireAdmin,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let _ = sqlx::query(
        "UPDATE mesh_bridge_mappings SET enabled = 1 - enabled WHERE id = ?",
    )
    .bind(&id)
    .execute(&state.db)
    .await;
    if let Some(b) = crate::broker::broker() {
        let _ = b.sync_subscriptions().await;
    }
    Redirect::to("/platforms/messaging/mesh/mappings")
}
