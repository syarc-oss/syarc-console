-- MediaMesh clip sessions: reassembled Open TAK media (VE3/VM1 voice,
-- VI1 photo) captured from channel data frames of type 0x5643.
CREATE TABLE IF NOT EXISTS mesh_media (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sid TEXT NOT NULL UNIQUE,          -- 8 lowercase hex chars
    kind TEXT NOT NULL DEFAULT 'voice',-- voice | photo
    mode INTEGER NOT NULL DEFAULT 0,   -- codec wire id (0-7 codec2, 32 mimi, 40 av1)
    total INTEGER NOT NULL DEFAULT 0,  -- packets announced in the envelope
    received INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'partial', -- partial | complete | error
    caption TEXT NOT NULL DEFAULT '',
    chan_idx INTEGER NOT NULL DEFAULT 0,
    dur INTEGER NOT NULL DEFAULT 0,    -- seconds (voice) or aspect*100 (photo)
    filename TEXT,
    size_bytes INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
