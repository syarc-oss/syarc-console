-- Mesh link app: console users ↔ bridge internet identities (XMPP first).
CREATE TABLE IF NOT EXISTS meshlink_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,
    backend TEXT NOT NULL DEFAULT 'xmpp',
    backend_uid TEXT NOT NULL,          -- XMPP room nick for now
    display_name TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'pending',  -- pending | verified
    linked_at TEXT NOT NULL,
    verified_at TEXT,
    UNIQUE(user_id, backend, backend_uid)
);
