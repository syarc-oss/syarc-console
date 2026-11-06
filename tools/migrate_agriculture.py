#!/usr/bin/env python3
"""Migrate agriculture suite data (orchard, garden, eggs, milk, meat, foraging)
from the fastmain Postgres DB into the syarc-console SQLite DB.

Agriculture rows are NOT encrypted in fastmain, so this is a straight copy
with user UUIDs remapped to console user ids by username.

Usage (fastmain venv has asyncpg):
    /usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_agriculture.py

Run AFTER the console service has restarted with migration 0008 applied.
"""
import asyncio
import os
import sqlite3

import asyncpg

PG_URL = os.environ.get(
    "PG_URL",
    "postgresql://fastapi_dev:S1lv3r@localhost/fastapi_dev",
)
SQLITE = os.environ.get(
    "SQLITE", "/usr/local/dev_apps/syarc-console/syarc-console.db"
)

# (pg_table, sqlite_table); insertion order respects FK dependencies.
TABLES = [
    ("orchards", "orchards"),
    ("orchard_shares", "orchard_shares"),
    ("trees", "trees"),
    ("maintenance_records", "maintenance_records"),
    ("orchard_maintenance", "orchard_maintenance"),
    ("gardens", "gardens"),
    ("garden_shares", "garden_shares"),
    ("garden_crops", "garden_crops"),
    ("garden_crop_maintenance", "garden_crop_maintenance"),
    ("garden_maintenance", "garden_maintenance"),
    ("coops", "coops"),
    ("coop_shares", "coop_shares"),
    ("hens", "hens"),
    ("egg_production", "egg_production"),
    ("milk_groups", "milk_groups"),
    ("milk_group_shares", "milk_group_shares"),
    ("milk_animals", "milk_animals"),
    ("milk_production", "milk_production"),
    ("milk_medical_records", "milk_medical_records"),
    ("meat_groups", "meat_groups"),
    ("meat_group_shares", "meat_group_shares"),
    ("meat_animals", "meat_animals"),
    ("meat_medical_records", "meat_medical_records"),
    ("meat_process_records", "meat_process_records"),
    ("meat_eval_records", "meat_eval_records"),
    ("foraging_outings", "foraging_outings"),
    ("foraging_shares", "foraging_shares"),
    ("foraging_participants", "foraging_participants"),
    ("foraging_results", "foraging_results"),
]

USER_UUID_COLUMNS = {
    "user_id",
    "shared_with_user_id",
    "created_by",
    "recorded_by",
}


def dt(v):
    return v.strftime("%Y-%m-%d %H:%M:%S") if v else None


def conv(v):
    import uuid as _uuid

    if isinstance(v, _uuid.UUID):
        return str(v)
    if hasattr(v, "strftime"):
        return dt(v)
    if isinstance(v, (bytes, bytearray)):
        return None if not v else v.decode("utf-8", "replace")
    if isinstance(v, bool):
        return int(v)
    if isinstance(v, (dict, list)):
        import json

        return json.dumps(v)
    if isinstance(v, float):
        return round(v, 6)
    return v


async def main():
    pg = await asyncpg.connect(PG_URL)

    pg_users = {str(r["id"]): r["username"] for r in await pg.fetch("SELECT id, username FROM users")}
    lite = sqlite3.connect(SQLITE)
    lite.execute("PRAGMA foreign_keys = OFF")
    console_users = {r[1]: r[0] for r in lite.execute("SELECT id, username FROM users")}

    def map_user(uuid_val):
        if uuid_val is None:
            return None
        name = pg_users.get(str(uuid_val))
        cid = console_users.get(name)
        if cid is None:
            print(f"  SKIP: console user not found for {name} ({uuid_val})")
        return cid

    for _, sqlite_table in TABLES:
        lite.execute(f"DELETE FROM {sqlite_table}")

    for pg_table, sqlite_table in TABLES:
        lite_cols = {r[1] for r in lite.execute(f"PRAGMA table_info({sqlite_table})")}
        has_created = any(
            c["column_name"] == "created_at"
            for c in await pg.fetch(
                "SELECT column_name FROM information_schema.columns WHERE table_name = $1", pg_table
            )
        )
        cols = [c["column_name"] for c in await pg.fetch(
            "SELECT column_name FROM information_schema.columns WHERE table_name = $1", pg_table
        )]
        if has_created:
            order = " ORDER BY created_at NULLS FIRST" + (", id" if "id" in cols else "")
        elif "id" in cols:
            order = " ORDER BY id"
        else:
            order = ""
        rows = await pg.fetch(f"SELECT * FROM {pg_table}{order}")
        inserted = 0
        for row in rows:
            cols, vals = [], []
            for key, value in dict(row).items():
                if key not in lite_cols:
                    continue
                if key in USER_UUID_COLUMNS:
                    value = map_user(value)
                else:
                    value = conv(value)
                cols.append(key)
                vals.append(value)
            if not cols:
                continue
            try:
                lite.execute(
                    f"INSERT OR REPLACE INTO {sqlite_table} ({', '.join(cols)}) "
                    f"VALUES ({', '.join('?' * len(cols))})",
                    vals,
                )
            except sqlite3.IntegrityError as e:
                print(f"  SKIP {pg_table} row (unmapped user or constraint): {e}")
                continue
            inserted += 1
        print(f"{pg_table}: {inserted} rows")

    lite.commit()
    lite.close()
    await pg.close()
    print("Agriculture migration complete.")


if __name__ == "__main__":
    asyncio.run(main())
