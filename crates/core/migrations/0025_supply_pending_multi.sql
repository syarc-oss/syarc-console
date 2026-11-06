-- Each scanned item is its own queue row: scanning the same product N times
-- queues N quantity-1 rows, and Add All accumulates them into the item.
DROP INDEX IF EXISTS idx_supply_pending_user_barcode;
CREATE INDEX IF NOT EXISTS idx_supply_pending_user_barcode_multi
    ON supply_pending_barcodes(uploaded_by, barcode);
