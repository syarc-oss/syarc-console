#!/usr/bin/env python3
"""Migrate surveillance data from fastmain Postgres into the syarc-console SQLite DB.

Usage (fastmain venv has asyncpg):
    /usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_surveillance.py
"""
import asyncio
import os
import sqlite3
import sys

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


async def main():
    pg = await asyncpg.connect(PG_URL)

    pg_users = await pg.fetch("SELECT id, username FROM users")
    lite = sqlite3.connect(SQLITE)
    console_users = {r[1]: r[0] for r in lite.execute("SELECT id, username FROM users")}

    def map_user(uuid):
        if uuid is None:
            return None
        name = pg_users_by_id.get(str(uuid))
        cid = console_users.get(name)
        if cid is None:
            print(f"  SKIP: console user not found for {name} ({uuid})")
        return cid

    pg_users_by_id = {str(r["id"]): r["username"] for r in pg_users}

    # wipe existing surveillance data
    for t in [
        "surveillance_ignore_windows",
        "surveillance_alarm_events",
        "surveillance_zone_cameras",
        "surveillance_zone_shares",
        "surveillance_cameras",
        "surveillance_zones",
    ]:
        lite.execute(f"DELETE FROM {t}")

    zones = await pg.fetch("SELECT * FROM surveillance_zones ORDER BY created_at")
    zone_map = {}
    for z in zones:
        uid = map_user(z["user_id"])
        if uid is None:
            continue
        cur = lite.execute(
            "INSERT INTO surveillance_zones (user_id, name, description, layout, is_active, created_at, updated_at) VALUES (?,?,?,?,?,?,?)",
            (uid, z["name"], z["description"], z["layout"] or "grid",
             1 if z["is_active"] else 0, dt(z["created_at"]), dt(z["updated_at"])),
        )
        zone_map[str(z["id"])] = cur.lastrowid
    print(f"zones: {len(zone_map)}")

    shares = await pg.fetch("SELECT * FROM surveillance_zone_shares")
    n = 0
    for s in shares:
        zid = zone_map.get(str(s["zone_id"]))
        uid = map_user(s["shared_with_user_id"])
        if zid is None or uid is None:
            continue
        lite.execute(
            "INSERT OR IGNORE INTO surveillance_zone_shares (zone_id, shared_with_user_id, can_edit, created_at) VALUES (?,?,?,?)",
            (zid, uid, 1 if s["can_edit"] else 0, dt(s["created_at"])),
        )
        n += 1
    print(f"shares: {n}")

    cams = await pg.fetch("SELECT * FROM surveillance_cameras ORDER BY created_at")
    cam_map = {}
    for c in cams:
        uid = map_user(c["user_id"])
        if uid is None:
            continue
        cur = lite.execute(
            "INSERT INTO surveillance_cameras (user_id, name, location, stream_url, stream_type, snapshot_url, username, password, is_enabled, recording_enabled, recording_retention_days, recording_mode, motion_timeout, rotation, alarm_enabled, alarm_types, alarm_start, alarm_end, trigger_camera_ids, trigger_delay, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (
                uid, c["name"], c["location"], c["stream_url"], c["stream_type"],
                c["snapshot_url"], c["username"], c["password"],
                1 if c["is_enabled"] else 0, 1 if c["recording_enabled"] else 0,
                c["recording_retention_days"], c["recording_mode"], c["motion_timeout"],
                c["rotation"], 1 if c["alarm_enabled"] else 0, c["alarm_types"],
                c["alarm_start"], c["alarm_end"], None, c["trigger_delay"],
                dt(c["created_at"]), dt(c["updated_at"]),
            ),
        )
        cam_map[str(c["id"])] = cur.lastrowid
    print(f"cameras: {len(cam_map)}")

    # rewrite trigger_camera_ids (comma-joined UUIDs) to new integer ids
    for old, new in cam_map.items():
        row = await pg.fetchrow("SELECT trigger_camera_ids FROM surveillance_cameras WHERE id = $1", __import__("uuid").UUID(old))
        tids = row["trigger_camera_ids"]
        if not tids:
            continue
        mapped = [str(cam_map[t.strip()]) for t in tids.split(",") if t.strip() and t.strip() in cam_map]
        lite.execute("UPDATE surveillance_cameras SET trigger_camera_ids = ? WHERE id = ?", (",".join(mapped), new))

    links = await pg.fetch("SELECT * FROM surveillance_zone_cameras ORDER BY display_order")
    n = 0
    for l in links:
        zid = zone_map.get(str(l["zone_id"]))
        cid = cam_map.get(str(l["camera_id"]))
        if zid is None or cid is None:
            continue
        lite.execute(
            "INSERT OR IGNORE INTO surveillance_zone_cameras (zone_id, camera_id, display_order, created_at) VALUES (?,?,?,?)",
            (zid, cid, l["display_order"], dt(l["created_at"])),
        )
        n += 1
    print(f"zone-camera links: {n}")

    events = await pg.fetch("SELECT * FROM surveillance_alarm_events ORDER BY created_at")
    n = 0
    for e in events:
        cid = cam_map.get(str(e["camera_id"]))
        if cid is None:
            continue
        lite.execute(
            "INSERT INTO surveillance_alarm_events (camera_id, camera_name, trigger_type, created_at) VALUES (?,?,?,?)",
            (cid, e["camera_name"], e["trigger_type"], dt(e["created_at"])),
        )
        n += 1
    print(f"alarm events: {n}")

    wins = await pg.fetch("SELECT * FROM surveillance_ignore_windows")
    n = 0
    for w in wins:
        cid = cam_map.get(str(w["camera_id"]))
        if cid is None:
            continue
        lite.execute(
            "INSERT INTO surveillance_ignore_windows (camera_id, name, start_time, end_time, days_of_week, is_enabled, created_at) VALUES (?,?,?,?,?,?,?)",
            (cid, w["name"], w["start_time"], w["end_time"], w["days_of_week"],
             1 if w["is_enabled"] else 0, dt(w["created_at"])),
        )
        n += 1
    print(f"ignore windows: {n}")

    lite.commit()
    lite.close()
    await pg.close()
    print("done")


if __name__ == "__main__":
    asyncio.run(main())
