-- Supply: idempotency journal for scanner "add all" chunk uploads.
-- A chunk POST that is retried after the server already committed returns
-- the stored results instead of applying the quantities a second time.

CREATE TABLE IF NOT EXISTS supply_scan_chunks (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    results TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
