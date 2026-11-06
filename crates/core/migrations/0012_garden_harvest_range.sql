-- Garden crops: support harvest date ranges for indeterminate crops
-- (e.g. tomatoes). Single-harvest crops use the same date for both fields.

ALTER TABLE garden_crops RENAME COLUMN harvest_date TO first_harvest_date;
ALTER TABLE garden_crops ADD COLUMN last_harvest_date TEXT;
UPDATE garden_crops SET last_harvest_date = first_harvest_date WHERE first_harvest_date IS NOT NULL;
