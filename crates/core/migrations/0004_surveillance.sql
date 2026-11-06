-- Surveillance app: zones, cameras, alarm events, ignore windows.
-- Mirrors the Python (fastmain) models with integer ids referencing users.

CREATE TABLE IF NOT EXISTS surveillance_zones (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    layout TEXT NOT NULL DEFAULT 'grid',
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS surveillance_zone_shares (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_id INTEGER NOT NULL REFERENCES surveillance_zones(id) ON DELETE CASCADE,
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    can_edit INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (zone_id, shared_with_user_id)
);

CREATE TABLE IF NOT EXISTS surveillance_cameras (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    location TEXT,
    stream_url TEXT NOT NULL,
    stream_type TEXT NOT NULL DEFAULT 'mjpeg',
    snapshot_url TEXT,
    username TEXT,
    password TEXT,
    is_enabled INTEGER NOT NULL DEFAULT 1,
    recording_enabled INTEGER NOT NULL DEFAULT 0,
    recording_retention_days INTEGER NOT NULL DEFAULT 30,
    recording_mode TEXT NOT NULL DEFAULT 'continuous',
    motion_timeout INTEGER NOT NULL DEFAULT 30,
    rotation INTEGER NOT NULL DEFAULT 0,
    alarm_enabled INTEGER NOT NULL DEFAULT 0,
    alarm_types TEXT NOT NULL DEFAULT 'people',
    alarm_start TEXT NOT NULL DEFAULT '22:00',
    alarm_end TEXT NOT NULL DEFAULT '06:00',
    trigger_camera_ids TEXT,
    trigger_delay INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS surveillance_zone_cameras (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_id INTEGER NOT NULL REFERENCES surveillance_zones(id) ON DELETE CASCADE,
    camera_id INTEGER NOT NULL REFERENCES surveillance_cameras(id) ON DELETE CASCADE,
    display_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (zone_id, camera_id)
);

CREATE TABLE IF NOT EXISTS surveillance_alarm_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    camera_id INTEGER NOT NULL REFERENCES surveillance_cameras(id) ON DELETE CASCADE,
    camera_name TEXT NOT NULL,
    trigger_type TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS surveillance_ignore_windows (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    camera_id INTEGER NOT NULL REFERENCES surveillance_cameras(id) ON DELETE CASCADE,
    name TEXT,
    start_time TEXT NOT NULL,
    end_time TEXT NOT NULL,
    days_of_week TEXT NOT NULL DEFAULT '0,1,2,3,4,5,6',
    is_enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
