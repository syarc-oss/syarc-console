#!/usr/bin/env python3
"""Migrate supply data from fastmain Postgres into the syarc-console SQLite DB.

Usage (fastmain venv has asyncpg):
    /usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_supply.py

Run AFTER the console service has restarted with the supply migration applied.
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


def dt(v):
    return v.strftime("%Y-%m-%d %H:%M:%S") if v else None


def d(v):
    return v.strftime("%Y-%m-%d") if v else None


async def main():
    pg = await asyncpg.connect(PG_URL)

    pg_users = {str(r["id"]): r["username"] for r in await pg.fetch("SELECT id, username FROM users")}
    lite = sqlite3.connect(SQLITE)
    console_users = {r[1]: r[0] for r in lite.execute("SELECT id, username FROM users")}

    def map_user(uuid):
        if uuid is None:
            return None
        name = pg_users.get(str(uuid))
        cid = console_users.get(name)
        if cid is None:
            print(f"  SKIP: console user not found for {name} ({uuid})")
        return cid

    for t in [
        "supply_item_tracks",
        "supply_items",
        "supply_pending_barcodes",
        "supply_categories",
        "supply_store_shares",
        "supply_stores",
    ]:
        lite.execute(f"DELETE FROM {t}")

    # stores
    store_map = {}
    for r in await pg.fetch("SELECT * FROM supply_stores ORDER BY created_at"):
        uid = map_user(r["user_id"])
        if uid is None:
            print(f"  SKIP store {r['name']} (owner not in console)")
            continue
        cur = lite.execute(
            "INSERT INTO supply_stores (user_id, name, description, is_active, created_at, updated_at) VALUES (?,?,?,?,?,?)",
            (uid, r["name"], r["description"], 1 if r["is_active"] else 0, dt(r["created_at"]), dt(r["updated_at"])),
        )
        store_map[str(r["id"])] = cur.lastrowid
    print(f"stores: {len(store_map)}")

    # shares
    shares = 0
    for r in await pg.fetch("SELECT * FROM supply_store_shares ORDER BY created_at"):
        sid = store_map.get(str(r["store_id"]))
        uid = map_user(r["shared_with_user_id"])
        if sid is None or uid is None:
            continue
        lite.execute(
            "INSERT INTO supply_store_shares (store_id, shared_with_user_id, created_at) VALUES (?,?,?)",
            (sid, uid, dt(r["created_at"])),
        )
        shares += 1
    print(f"shares: {shares}")

    # categories (two-pass for parent refs)
    cat_map = {}
    cats = await pg.fetch("SELECT * FROM supply_categories ORDER BY created_at")
    for r in cats:
        cur = lite.execute(
            "INSERT INTO supply_categories (name, description, store_id, parent_id, created_at, updated_at) VALUES (?,?,?,?,?,?)",
            (r["name"], r["description"], store_map.get(str(r["store_id"])), None, dt(r["created_at"]), dt(r["updated_at"])),
        )
        cat_map[str(r["id"])] = cur.lastrowid
    for r in cats:
        if r["parent_id"] is not None and str(r["parent_id"]) in cat_map:
            lite.execute(
                "UPDATE supply_categories SET parent_id = ? WHERE id = ?",
                (cat_map[str(r["parent_id"])], cat_map[str(r["id"])]),
            )
    print(f"categories: {len(cat_map)}")

    # items
    item_map = {}
    for r in await pg.fetch("SELECT * FROM supply_items ORDER BY created_at"):
        cid = cat_map.get(str(r["category_id"]))
        uid = map_user(r["created_by"])
        if cid is None or uid is None:
            print(f"  SKIP item {r['key']}")
            continue
        cur = lite.execute(
            "INSERT INTO supply_items (key, description, barcode, quantity, minimum, location, notes, store_id, category_id, created_by, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            (
                r["key"], r["description"], r["barcode"], r["quantity"], r["minimum"],
                r["location"], r["notes"], store_map.get(str(r["store_id"])), cid, uid,
                dt(r["created_at"]), dt(r["updated_at"]),
            ),
        )
        item_map[str(r["id"])] = cur.lastrowid
    print(f"items: {len(item_map)}")

    # pending barcodes
    pending = 0
    for r in await pg.fetch("SELECT * FROM supply_pending_barcodes ORDER BY created_at"):
        uid = map_user(r["uploaded_by"])
        if uid is None:
            continue
        lite.execute(
            "INSERT INTO supply_pending_barcodes (barcode, uploaded_by, created_at) VALUES (?,?,?)",
            (r["barcode"], uid, dt(r["created_at"])),
        )
        pending += 1
    print(f"pending barcodes: {pending}")

    # tracks
    tracks = 0
    for r in await pg.fetch("SELECT * FROM supply_item_tracks ORDER BY created_at"):
        iid = item_map.get(str(r["item_id"]))
        if iid is None:
            continue
        lite.execute(
            "INSERT INTO supply_item_tracks (item_id, open_date, finished_date, notes, created_at) VALUES (?,?,?,?,?)",
            (iid, d(r["open_date"]), d(r["finished_date"]), r["notes"], dt(r["created_at"])),
        )
        tracks += 1
    print(f"tracks: {tracks}")

    lite.commit()
    lite.close()
    await pg.close()
    print("supply migration complete")


asyncio.run(main())
