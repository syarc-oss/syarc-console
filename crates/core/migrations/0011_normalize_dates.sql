-- Normalize date-only columns that were migrated from fastmain with time
-- components ("YYYY-MM-DD 00:00:00" or ISO timestamps) to plain "YYYY-MM-DD",
-- which is what the app writes and what the date pickers expect.

UPDATE trees SET planting_date = substr(planting_date, 1, 10) WHERE length(planting_date) > 10;
UPDATE maintenance_records SET maintenance_date = substr(maintenance_date, 1, 10);
UPDATE orchard_maintenance SET maintenance_date = substr(maintenance_date, 1, 10);
UPDATE garden_crops SET planting_date = substr(planting_date, 1, 10) WHERE length(planting_date) > 10;
UPDATE garden_crops SET harvest_date = substr(harvest_date, 1, 10) WHERE length(harvest_date) > 10;
UPDATE garden_crop_maintenance SET activity_date = substr(activity_date, 1, 10);
UPDATE garden_maintenance SET activity_date = substr(activity_date, 1, 10);
UPDATE hens SET acquisition_date = substr(acquisition_date, 1, 10) WHERE length(acquisition_date) > 10;
UPDATE egg_production SET collection_date = substr(collection_date, 1, 10) WHERE length(collection_date) > 10;
UPDATE milk_animals SET acquisition_date = substr(acquisition_date, 1, 10) WHERE length(acquisition_date) > 10;
UPDATE milk_production SET collection_date = substr(collection_date, 1, 10) WHERE length(collection_date) > 10;
UPDATE milk_medical_records SET record_date = substr(record_date, 1, 10);
UPDATE meat_animals SET acquisition_date = substr(acquisition_date, 1, 10) WHERE length(acquisition_date) > 10;
UPDATE meat_medical_records SET record_date = substr(record_date, 1, 10);
UPDATE meat_process_records SET process_date = substr(process_date, 1, 10);
UPDATE meat_eval_records SET eval_date = substr(eval_date, 1, 10);
UPDATE foraging_outings SET outing_date = substr(outing_date, 1, 10);
UPDATE foraging_results SET result_date = substr(result_date, 1, 10);
UPDATE ebook_books SET published_date = substr(published_date, 1, 10) WHERE length(published_date) > 10;
