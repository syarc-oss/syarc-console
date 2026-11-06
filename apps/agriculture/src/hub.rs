//! Hub dashboards: Agriculture -> Agronomy (orchard, garden) and Husbandry
//! (eggs, milk, meat), plus Foraging directly under Agriculture.
//! The Python version gated cards on per-app user permissions, which the
//! console does not have; all sub-app cards are shown to logged-in users.

use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::json;

use syarc_plugin_api::{RequireUser, SharedState};

use crate::{page, user_json};

pub(crate) fn agriculture_router() -> Router<SharedState> {
    Router::new()
        .route("/agriculture", get(agriculture_dashboard))
        .route("/agriculture/", get(agriculture_dashboard))
}

pub(crate) fn agronomy_router() -> Router<SharedState> {
    Router::new()
        .route("/agriculture/agronomy", get(agronomy_dashboard))
        .route("/agriculture/agronomy/", get(agronomy_dashboard))
}

pub(crate) fn husbandry_router() -> Router<SharedState> {
    Router::new()
        .route("/agriculture/husbandry", get(husbandry_dashboard))
        .route("/agriculture/husbandry/", get(husbandry_dashboard))
}

async fn agriculture_dashboard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    page(
        &state,
        "agriculture/agriculture_dashboard.html",
        json!({
            "user": user_json(&user),
            "allowed_sub_apps": ["agronomy", "husbandry", "foraging"],
        }),
    )
}

async fn agronomy_dashboard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    page(
        &state,
        "agronomy/agronomy_dashboard.html",
        json!({
            "user": user_json(&user),
            "allowed_sub_apps": ["orchard", "garden"],
        }),
    )
}

async fn husbandry_dashboard(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    page(
        &state,
        "husbandry/husbandry_dashboard.html",
        json!({
            "user": user_json(&user),
            "allowed_sub_apps": ["eggs", "milk", "meat"],
        }),
    )
}
