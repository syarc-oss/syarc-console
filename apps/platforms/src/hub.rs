use std::sync::OnceLock;
use tokio::sync::broadcast;

/// SSE fan-out hub. One process-wide broadcast channel carrying
/// conversation ids; conversation streams subscribe and filter by id, then
/// re-poll the DB (authoritative). Replaces the Python per-conversation
/// asyncio queues plus the cross-worker DB safety poll (single process here).
#[derive(Clone, Debug)]
pub struct SseEvent {
    pub conversation_id: String,
}

pub struct Hub {
    tx: broadcast::Sender<SseEvent>,
}

impl Hub {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    pub fn global() -> &'static Hub {
        static HUB: OnceLock<Hub> = OnceLock::new();
        HUB.get_or_init(Hub::new)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SseEvent> {
        self.tx.subscribe()
    }

    pub fn notify(&self, conversation_id: &str) {
        let _ = self.tx.send(SseEvent {
            conversation_id: conversation_id.to_string(),
        });
    }
}
