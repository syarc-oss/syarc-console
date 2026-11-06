-- Persistent per-user scanner queue: keep the last looked-up description
-- with each pending barcode so a restored queue doesn't need fresh lookups.
ALTER TABLE supply_pending_barcodes ADD COLUMN description TEXT;
