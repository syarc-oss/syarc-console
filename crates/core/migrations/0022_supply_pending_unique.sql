-- Scanner queue rows are keyed per user+barcode so add/remove operations
-- are idempotent upserts.
DELETE FROM supply_pending_barcodes
WHERE id NOT IN (SELECT MAX(id) FROM supply_pending_barcodes GROUP BY uploaded_by, barcode);
CREATE UNIQUE INDEX IF NOT EXISTS idx_supply_pending_user_barcode
    ON supply_pending_barcodes(uploaded_by, barcode);
