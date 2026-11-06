-- Calendar app: tasks + task crew (the invite model) and recurring chores,
-- ported from the fastmain tasks app. Ids are fresh UUID TEXT keys; user ids
-- are console INTEGERs; dates are "%Y-%m-%d" and datetimes
-- "%Y-%m-%d %H:%M:%S" local strings, per house style.

CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    owner_id INTEGER NOT NULL REFERENCES users(id),
    description TEXT NOT NULL,
    urgency INTEGER NOT NULL DEFAULT 3,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft','pending','in_progress','completed','cancelled')),
    estimated_hours REAL,
    supplies_needed TEXT,
    estimated_cost REAL,
    due_date TEXT,
    completed_at TEXT,
    location TEXT,
    notes TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_due ON tasks(due_date);
CREATE INDEX IF NOT EXISTS idx_tasks_owner ON tasks(owner_id);

CREATE TABLE IF NOT EXISTS task_crew (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','confirmed','declined')),
    assigned_at TEXT NOT NULL,
    responded_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_task_crew_user ON task_crew(user_id, status);
CREATE INDEX IF NOT EXISTS idx_task_crew_task ON task_crew(task_id);

CREATE TABLE IF NOT EXISTS chores (
    id TEXT PRIMARY KEY,
    owner_id INTEGER NOT NULL REFERENCES users(id),
    description TEXT NOT NULL,
    frequency TEXT NOT NULL DEFAULT 'weekly'
        CHECK (frequency IN ('daily','weekly','biweekly','semimonthly','monthly','quarterly','yearly')),
    day_of_week INTEGER,
    day_of_month INTEGER,
    time_estimate REAL,
    location TEXT,
    supplies_needed TEXT,
    notes TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    last_completed_at TEXT,
    next_due_date TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chores_next_due ON chores(next_due_date);

CREATE TABLE IF NOT EXISTS chore_completions (
    id TEXT PRIMARY KEY,
    chore_id TEXT NOT NULL REFERENCES chores(id) ON DELETE CASCADE,
    completed_by INTEGER NOT NULL REFERENCES users(id),
    completed_at TEXT NOT NULL,
    notes TEXT
);
CREATE INDEX IF NOT EXISTS idx_chore_completions_chore ON chore_completions(chore_id);

ALTER TABLE cooking_menu_entries ADD COLUMN cook_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL;
