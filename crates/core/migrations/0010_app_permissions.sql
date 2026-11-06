-- Per-user app access permissions. Admins bypass; regular users need a row
-- for the app they are reaching. Seeded for all existing human users by the
-- host after app seeding (mesh-* service accounts are excluded).

CREATE TABLE IF NOT EXISTS user_app_permissions (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app_id TEXT NOT NULL,
    PRIMARY KEY (user_id, app_id)
);
