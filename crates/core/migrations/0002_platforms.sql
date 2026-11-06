CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS conversation_participants (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    joined_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (conversation_id, user_id)
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    sender_id INTEGER NOT NULL REFERENCES users(id),
    content TEXT NOT NULL,
    mesh_frame_id TEXT UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS message_read_statuses (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    read_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (message_id, user_id)
);

CREATE TABLE IF NOT EXISTS mesh_nodes (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL UNIQUE,
    protocol TEXT NOT NULL DEFAULT 'meshcore',
    short_name TEXT NOT NULL DEFAULT '',
    long_name TEXT NOT NULL DEFAULT '',
    bound_user_id INTEGER NOT NULL REFERENCES users(id),
    last_packet_id INTEGER,
    last_rx_time INTEGER,
    is_active INTEGER NOT NULL DEFAULT 1,
    first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS mesh_bridge_mappings (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    mesh_node_id TEXT NOT NULL REFERENCES mesh_nodes(id) ON DELETE CASCADE,
    last_forwarded_at TEXT NOT NULL DEFAULT (datetime('now')),
    enabled INTEGER NOT NULL DEFAULT 1,
    owner_user_id INTEGER REFERENCES users(id),
    to_user TEXT,
    retention_days INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (conversation_id, mesh_node_id)
);

CREATE TABLE IF NOT EXISTS mesh_ingested_frames (
    frame_id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS ix_mesh_ingested_created ON mesh_ingested_frames(created_at);

CREATE TABLE IF NOT EXISTS mesh_user_links (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mesh_node_id TEXT NOT NULL REFERENCES mesh_nodes(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (user_id, mesh_node_id)
);

CREATE TABLE IF NOT EXISTS mesh_directory (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL UNIQUE,
    protocol TEXT NOT NULL DEFAULT 'meshcore',
    node_type INTEGER NOT NULL DEFAULT 1,
    name TEXT NOT NULL DEFAULT '',
    gps_lat REAL,
    gps_lon REAL,
    is_favorite INTEGER NOT NULL DEFAULT 0,
    show_traffic INTEGER NOT NULL DEFAULT 1,
    admin_password TEXT NOT NULL DEFAULT '',
    guest_password TEXT NOT NULL DEFAULT '',
    admin_mode TEXT NOT NULL DEFAULT 'guest',
    clock_sync INTEGER NOT NULL DEFAULT 1,
    last_heard TEXT,
    last_advert INTEGER,
    first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_synced_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS mesh_traffic (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    protocol TEXT NOT NULL,
    direction TEXT NOT NULL,
    node_id TEXT NOT NULL,
    node_name TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL DEFAULT '',
    storage_path TEXT,
    instance TEXT NOT NULL DEFAULT 'local',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS ix_mesh_traffic_created ON mesh_traffic(created_at);
