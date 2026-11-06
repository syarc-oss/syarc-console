-- Allow recording multiple eggs per entry (per hen per date).
ALTER TABLE egg_production ADD COLUMN quantity INTEGER NOT NULL DEFAULT 1;
