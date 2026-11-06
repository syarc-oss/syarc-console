-- Manually added mesh contacts must survive the 350-entry directory cap,
-- which prunes by last_synced_at (manual entries never re-sync).
ALTER TABLE mesh_directory ADD COLUMN is_manual INTEGER NOT NULL DEFAULT 0;
