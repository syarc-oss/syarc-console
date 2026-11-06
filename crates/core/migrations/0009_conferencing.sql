-- Conferencing: WebRTC audio/video calling.
-- Call/invitation/recording ids keep their fastmain UUID strings (TEXT);
-- user ids are console INTEGERs; datetimes are local "%Y-%m-%d %H:%M:%S" strings.

CREATE TABLE IF NOT EXISTS conferencing_calls (
    id TEXT PRIMARY KEY,
    caller_id INTEGER NOT NULL REFERENCES users(id),
    media_type TEXT NOT NULL DEFAULT 'audio',
    status TEXT NOT NULL DEFAULT 'ringing',
    call_type TEXT NOT NULL DEFAULT 'direct',
    started_at TEXT NOT NULL,
    ended_at TEXT
);

CREATE TABLE IF NOT EXISTS conferencing_call_participants (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    call_id TEXT NOT NULL REFERENCES conferencing_calls(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    joined_at TEXT NOT NULL,
    left_at TEXT
);

CREATE TABLE IF NOT EXISTS conferencing_invitations (
    id TEXT PRIMARY KEY,
    call_id TEXT NOT NULL REFERENCES conferencing_calls(id) ON DELETE CASCADE,
    invitee_id INTEGER NOT NULL REFERENCES users(id),
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL,
    responded_at TEXT,
    expires_at TEXT NOT NULL,
    acknowledged_at TEXT
);

CREATE TABLE IF NOT EXISTS conferencing_call_dismissals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    call_id TEXT NOT NULL REFERENCES conferencing_calls(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    dismissed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conferencing_recordings (
    id TEXT PRIMARY KEY,
    call_id TEXT REFERENCES conferencing_calls(id) ON DELETE SET NULL,
    recorded_by_user_id INTEGER NOT NULL REFERENCES users(id),
    recipient_user_id INTEGER REFERENCES users(id),
    kind TEXT NOT NULL DEFAULT 'call',
    media_type TEXT NOT NULL DEFAULT 'audio',
    file_path TEXT,
    duration_seconds REAL,
    created_at TEXT NOT NULL,
    listened_at TEXT
);
