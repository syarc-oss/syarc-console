-- Pecuniary app: checkbook registers, ledger books, precious-metals
-- portfolios, and stock portfolios with sharing. Amounts are stored as
-- plain REAL columns (the Python app's pgcrypto-encrypted amounts were
-- already unrecoverable, so the envelope-encryption layer was dropped).

CREATE TABLE IF NOT EXISTS checkbook_registers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    bank_name TEXT NOT NULL,
    account_number TEXT,
    account_type TEXT NOT NULL DEFAULT 'checking',
    description TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS checkbook_shares (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    register_id INTEGER NOT NULL REFERENCES checkbook_registers(id) ON DELETE CASCADE,
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (register_id, shared_with_user_id)
);

CREATE TABLE IF NOT EXISTS checkbook (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    register_id INTEGER REFERENCES checkbook_registers(id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users(id),
    check_number INTEGER,
    date TEXT NOT NULL,
    type TEXT NOT NULL DEFAULT 'CHECK',
    payee_description TEXT NOT NULL DEFAULT '',
    payment REAL NOT NULL DEFAULT 0,
    deposit REAL NOT NULL DEFAULT 0,
    balance REAL NOT NULL DEFAULT 0,
    returned INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS ledger_books (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS ledger_shares (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id INTEGER NOT NULL REFERENCES ledger_books(id) ON DELETE CASCADE,
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (book_id, shared_with_user_id)
);

CREATE TABLE IF NOT EXISTS ledger_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id INTEGER NOT NULL REFERENCES ledger_books(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id),
    category TEXT NOT NULL,
    entry_type TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    amount REAL NOT NULL DEFAULT 0,
    entry_date TEXT NOT NULL,
    recurrence TEXT NOT NULL DEFAULT 'one_time',
    source TEXT,
    reference_number TEXT,
    notes TEXT,
    is_reconciled INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS pms_portfolios (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS pms_shares (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    portfolio_id INTEGER NOT NULL REFERENCES pms_portfolios(id) ON DELETE CASCADE,
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (portfolio_id, shared_with_user_id)
);

CREATE TABLE IF NOT EXISTS pms_transactions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    portfolio_id INTEGER REFERENCES pms_portfolios(id) ON DELETE CASCADE,
    metal TEXT NOT NULL DEFAULT 'silver',
    date_purchased TEXT NOT NULL,
    date_received TEXT,
    vendor TEXT NOT NULL,
    units REAL NOT NULL,
    description TEXT NOT NULL,
    mint TEXT NOT NULL,
    cost REAL NOT NULL DEFAULT 0,
    ounces REAL NOT NULL DEFAULT 0,
    disposition TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    created_by INTEGER NOT NULL REFERENCES users(id)
);

CREATE TABLE IF NOT EXISTS pms_current_values (
    metal TEXT PRIMARY KEY,
    price REAL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS stock_portfolios (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    broker TEXT NOT NULL DEFAULT 'Default',
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS stock_shares (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    portfolio_id INTEGER NOT NULL REFERENCES stock_portfolios(id) ON DELETE CASCADE,
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (portfolio_id, shared_with_user_id)
);

CREATE TABLE IF NOT EXISTS stock_transactions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    portfolio_id INTEGER REFERENCES stock_portfolios(id) ON DELETE CASCADE,
    symbol TEXT NOT NULL,
    broker TEXT NOT NULL DEFAULT 'Default',
    transaction_date TEXT NOT NULL,
    type TEXT NOT NULL,
    shares REAL NOT NULL DEFAULT 0,
    price_per_share REAL NOT NULL DEFAULT 0,
    total_value REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_stock_tx_symbol ON stock_transactions(symbol);
