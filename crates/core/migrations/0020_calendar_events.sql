-- Calendar events: appointments and meetings with invitees.
-- Ids are fresh UUID TEXT keys; user ids are console INTEGERs;
-- dates are "%Y-%m-%d", times "%H:%M", datetimes "%Y-%m-%d %H:%M:%S".

CREATE TABLE IF NOT EXISTS calendar_events (
    id TEXT PRIMARY KEY,
    owner_id INTEGER NOT NULL REFERENCES users(id),
    category TEXT NOT NULL DEFAULT 'appointment'
        CHECK (category IN ('appointment','meeting')),
    title TEXT NOT NULL,
    event_date TEXT NOT NULL,
    event_time TEXT,
    duration_mins INTEGER,
    location TEXT,
    notes TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cal_events_date ON calendar_events(event_date);

CREATE TABLE IF NOT EXISTS calendar_event_invitees (
    id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES calendar_events(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','confirmed','declined')),
    invited_at TEXT NOT NULL,
    responded_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_cal_event_invitees_user ON calendar_event_invitees(user_id, status);
