-- Agriculture suite: orchard, garden, eggs, milk, meat, foraging.
-- Entity ids keep their fastmain UUID strings (TEXT); user ids are console INTEGERs.

-- Orchard
CREATE TABLE IF NOT EXISTS orchards (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    location TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS orchard_shares (
    id TEXT PRIMARY KEY,
    orchard_id TEXT NOT NULL REFERENCES orchards(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS trees (
    id TEXT PRIMARY KEY,
    tree_type TEXT NOT NULL,
    variety TEXT NOT NULL,
    planting_date TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    notes TEXT,
    location_notes TEXT,
    orchard_id TEXT NOT NULL REFERENCES orchards(id),
    created_at TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS maintenance_records (
    id TEXT PRIMARY KEY,
    tree_id TEXT NOT NULL REFERENCES trees(id),
    maintenance_type TEXT NOT NULL,
    maintenance_date TEXT NOT NULL,
    description TEXT,
    products_used TEXT,
    cost REAL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS orchard_maintenance (
    id TEXT PRIMARY KEY,
    orchard_id TEXT NOT NULL REFERENCES orchards(id),
    maintenance_type TEXT NOT NULL,
    maintenance_date TEXT NOT NULL,
    description TEXT,
    products_used TEXT,
    cost REAL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);

-- Garden
CREATE TABLE IF NOT EXISTS gardens (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    location TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS garden_shares (
    id TEXT PRIMARY KEY,
    garden_id TEXT NOT NULL REFERENCES gardens(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS garden_crops (
    id TEXT PRIMARY KEY,
    garden_id TEXT NOT NULL REFERENCES gardens(id),
    plot_section TEXT,
    crop TEXT NOT NULL,
    variety TEXT,
    planting_date TEXT,
    planted_by TEXT,
    days_to_maturity INTEGER,
    harvest_date TEXT,
    harvested_by TEXT,
    notes TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS garden_crop_maintenance (
    id TEXT PRIMARY KEY,
    crop_id TEXT NOT NULL REFERENCES garden_crops(id),
    activity_type TEXT NOT NULL,
    activity_date TEXT NOT NULL,
    person TEXT,
    time_spent TEXT,
    materials TEXT,
    cost REAL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS garden_maintenance (
    id TEXT PRIMARY KEY,
    garden_id TEXT NOT NULL REFERENCES gardens(id),
    activity_type TEXT NOT NULL,
    activity_date TEXT NOT NULL,
    person TEXT,
    time_spent TEXT,
    materials TEXT,
    cost REAL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);

-- Eggs
CREATE TABLE IF NOT EXISTS coops (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS coop_shares (
    id TEXT PRIMARY KEY,
    coop_id TEXT NOT NULL REFERENCES coops(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS hens (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    breed TEXT,
    acquisition_date TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    notes TEXT,
    coop_id TEXT REFERENCES coops(id),
    created_at TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS egg_production (
    id TEXT PRIMARY KEY,
    hen_id TEXT NOT NULL REFERENCES hens(id),
    collection_date TEXT NOT NULL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);

-- Milk
CREATE TABLE IF NOT EXISTS milk_groups (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS milk_group_shares (
    id TEXT PRIMARY KEY,
    group_id TEXT NOT NULL REFERENCES milk_groups(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS milk_animals (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    animal_type TEXT NOT NULL,
    breed TEXT,
    acquisition_date TEXT,
    is_producing INTEGER NOT NULL DEFAULT 1,
    notes TEXT,
    group_id TEXT REFERENCES milk_groups(id),
    created_at TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS milk_production (
    id TEXT PRIMARY KEY,
    animal_id TEXT NOT NULL REFERENCES milk_animals(id),
    collection_date TEXT NOT NULL,
    amount REAL NOT NULL,
    unit TEXT NOT NULL DEFAULT 'gallons',
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS milk_medical_records (
    id TEXT PRIMARY KEY,
    animal_id TEXT NOT NULL REFERENCES milk_animals(id),
    record_date TEXT NOT NULL,
    description TEXT NOT NULL,
    cost REAL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);

-- Meat
CREATE TABLE IF NOT EXISTS meat_groups (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    description TEXT,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meat_group_shares (
    id TEXT PRIMARY KEY,
    group_id TEXT NOT NULL REFERENCES meat_groups(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meat_animals (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    animal_type TEXT NOT NULL,
    breed TEXT,
    quantity INTEGER NOT NULL DEFAULT 1,
    acquisition_date TEXT,
    status TEXT NOT NULL DEFAULT 'growing',
    notes TEXT,
    group_id TEXT REFERENCES meat_groups(id),
    created_at TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS meat_medical_records (
    id TEXT PRIMARY KEY,
    animal_id TEXT NOT NULL REFERENCES meat_animals(id),
    record_date TEXT NOT NULL,
    description TEXT NOT NULL,
    cost REAL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS meat_process_records (
    id TEXT PRIMARY KEY,
    animal_id TEXT NOT NULL REFERENCES meat_animals(id),
    process_date TEXT NOT NULL,
    number_processed INTEGER NOT NULL,
    lbs REAL,
    disposition TEXT NOT NULL,
    storage_location TEXT,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);
CREATE TABLE IF NOT EXISTS meat_eval_records (
    id TEXT PRIMARY KEY,
    animal_id TEXT NOT NULL REFERENCES meat_animals(id),
    eval_date TEXT NOT NULL,
    weight REAL,
    bcs INTEGER NOT NULL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);

-- Foraging
CREATE TABLE IF NOT EXISTS foraging_outings (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    outing_date TEXT NOT NULL,
    outing_type TEXT NOT NULL,
    duration TEXT,
    location TEXT,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'planned',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS foraging_shares (
    id TEXT PRIMARY KEY,
    outing_id TEXT NOT NULL REFERENCES foraging_outings(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS foraging_participants (
    id TEXT PRIMARY KEY,
    outing_id TEXT NOT NULL REFERENCES foraging_outings(id),
    name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS foraging_results (
    id TEXT PRIMARY KEY,
    outing_id TEXT NOT NULL REFERENCES foraging_outings(id),
    result_date TEXT NOT NULL,
    item TEXT NOT NULL,
    location TEXT,
    quantity INTEGER,
    lbs REAL,
    notes TEXT,
    created_at TEXT NOT NULL,
    recorded_by INTEGER NOT NULL REFERENCES users(id)
);
