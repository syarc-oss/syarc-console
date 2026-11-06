-- MeshCore app: channels the user follows.
CREATE TABLE IF NOT EXISTS mesh_followed_channels (
    idx INTEGER PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
