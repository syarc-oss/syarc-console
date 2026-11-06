-- Cooking app: recipes, ingredient index, and menu calendar.
-- Ported from the Python (fastmain) models; ids keep their UUID strings,
-- user ids are console INTEGERs.

CREATE TABLE IF NOT EXISTS cooking_recipes (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    steps TEXT,
    source_uid TEXT,
    source_link TEXT,
    is_public INTEGER NOT NULL DEFAULT 1,
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS cooking_ingredients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL UNIQUE
);
CREATE INDEX IF NOT EXISTS idx_cooking_ingredients_norm ON cooking_ingredients(normalized_name);

CREATE TABLE IF NOT EXISTS cooking_recipe_ingredients (
    id TEXT PRIMARY KEY,
    recipe_id TEXT NOT NULL REFERENCES cooking_recipes(id) ON DELETE CASCADE,
    ingredient_id TEXT NOT NULL REFERENCES cooking_ingredients(id) ON DELETE CASCADE,
    raw_text TEXT
);
CREATE INDEX IF NOT EXISTS idx_cri_recipe ON cooking_recipe_ingredients(recipe_id);
CREATE INDEX IF NOT EXISTS idx_cri_ingredient ON cooking_recipe_ingredients(ingredient_id);

CREATE TABLE IF NOT EXISTS cooking_menu_entries (
    id TEXT PRIMARY KEY,
    recipe_id TEXT NOT NULL REFERENCES cooking_recipes(id) ON DELETE CASCADE,
    menu_date TEXT NOT NULL,
    meal_type TEXT NOT NULL DEFAULT 'dinner',
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_cooking_menu_date ON cooking_menu_entries(menu_date);
