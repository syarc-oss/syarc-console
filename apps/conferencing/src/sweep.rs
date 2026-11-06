//! Background sweep: expires pending invitations and ends unanswered ringing
//! direct calls.

use serde_json::json;
use sqlx::Row;

use crate::{hub, now_str};

pub const SWEEP_INTERVAL_SECONDS: u64 = 10;

pub async fn sweep_once(db: &syarc_plugin_api::Db) {
    let now = now_str();
    let expired = sqlx::query_as::<_, (String, String)>(
        "SELECT id, call_id FROM conferencing_invitations \
         WHERE status = 'pending' AND expires_at <= ?",
    )
    .bind(&now)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    if expired.is_empty() {
        return;
    }

    for (inv_id, _) in &expired {
        let _ = sqlx::query(
            "UPDATE conferencing_invitations SET status = 'missed', responded_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(inv_id)
        .execute(db)
        .await;
    }

    let mut call_ids: Vec<String> = expired.iter().map(|(_, c)| c.clone()).collect();
    call_ids.dedup();
    for call_id in call_ids {
        let call = sqlx::query(
            "SELECT status, call_type, caller_id FROM conferencing_calls WHERE id = ?",
        )
        .bind(&call_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        let Some(call) = call else { continue };
        if call.get::<String, _>("status") != "ringing" {
            continue;
        }
        let live = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM conferencing_invitations \
             WHERE call_id = ? AND status IN ('pending','accepted') LIMIT 1",
        )
        .bind(&call_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
        if live.is_none() {
            let _ = sqlx::query(
                "UPDATE conferencing_calls SET status = 'ended', ended_at = ? WHERE id = ?",
            )
            .bind(&now)
            .bind(&call_id)
            .execute(db)
            .await;
            if call.get::<String, _>("call_type") == "direct" {
                hub::send_to_user(
                    call.get::<i64, _>("caller_id"),
                    &json!({"type": "call_missed", "call_id": call_id}),
                );
            }
        }
    }
}

pub async fn sweep_loop(db: syarc_plugin_api::Db) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(SWEEP_INTERVAL_SECONDS)).await;
        sweep_once(&db).await;
    }
}
