-- Supply app: stores, categories, items, usage tracking, pending barcodes.
-- Mirrors the Python (fastmain) models with integer ids referencing users.

CREATE TABLE IF NOT EXISTS supply_stores (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS supply_store_shares (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    store_id INTEGER NOT NULL REFERENCES supply_stores(id) ON DELETE CASCADE,
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (store_id, shared_with_user_id)
);

CREATE TABLE IF NOT EXISTS supply_categories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    description TEXT,
    store_id INTEGER REFERENCES supply_stores(id) ON DELETE CASCADE,
    parent_id INTEGER REFERENCES supply_categories(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS supply_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    key TEXT NOT NULL,
    description TEXT,
    barcode TEXT,
    quantity INTEGER NOT NULL DEFAULT 0,
    minimum INTEGER,
    location TEXT,
    notes TEXT,
    store_id INTEGER REFERENCES supply_stores(id) ON DELETE CASCADE,
    category_id INTEGER NOT NULL REFERENCES supply_categories(id),
    created_by INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_supply_items_barcode ON supply_items(barcode);

CREATE TABLE IF NOT EXISTS supply_pending_barcodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    barcode TEXT NOT NULL,
    uploaded_by INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS supply_item_tracks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id INTEGER NOT NULL REFERENCES supply_items(id) ON DELETE CASCADE,
    open_date TEXT NOT NULL,
    finished_date TEXT,
    notes TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
