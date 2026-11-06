//! WebSocket signaling endpoint: presence, call control, SDP/ICE relay.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tower_sessions::Session;

use syarc_plugin_api::{current_user, SharedState};

use crate::{hub, relay_webrtc, router};

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    session: Session,
    State(state): State<SharedState>,
) -> Response {
    let user = match current_user(&session, &state).await {
        Ok(Some(user)) => user,
        _ => return (StatusCode::FORBIDDEN, "Forbidden").into_response(),
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, user))
}

async fn handle_socket(socket: WebSocket, state: SharedState, user: syarc_plugin_api::User) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    hub::register(user.id, tx.clone());
    hub::broadcast_presence(user.id, true);

    let mut socket = socket;
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                        handle_client_message(&state, &user, &msg).await;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            outbound = rx.recv() => {
                match outbound {
                    Some(msg) => {
                        if socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    if hub::unregister(user.id, &tx) {
        hub::broadcast_presence(user.id, false);
        hub::arm_watchdog(user.id);
    }
}

async fn handle_client_message(state: &SharedState, user: &syarc_plugin_api::User, msg: &Value) {
    let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match msg_type {
        "call_request" => {
            let _ = router::ws_initiate(state, user, msg).await;
        }
        "call_accept" | "call_reject" | "call_end" => {
            let _ = router::ws_call_response(state, user, msg).await;
        }
        "sdp_offer" | "sdp_answer" | "ice_candidate" | "peer_ready" => {
            relay_webrtc(user.id, msg);
        }
        "ping" => {
            hub::send_to_user(user.id, &json!({"type": "pong"}));
        }
        "query_online" => {
            hub::send_to_user(
                user.id,
                &json!({"type": "online_list", "users": hub::online_users()}),
            );
        }
        _ => {}
    }
}
