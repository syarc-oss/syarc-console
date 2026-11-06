#!/usr/bin/env python3
"""Migrate conferencing data (calls, participants, invitations, dismissals,
recordings) from the fastmain Postgres DB into the syarc-console SQLite DB.

Call/invitation/recording UUIDs are kept; user UUIDs are remapped to console
user ids by username. Recording file paths point at the same recording root
(/usr/local/recordings/conferencing), so files are referenced in place — no
copying needed.

Usage (fastmain venv has asyncpg):
    /usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_conferencing.py

Run AFTER the console service has restarted with migration 0009 applied.
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
    ("conferencing_calls", "conferencing_calls"),
    ("conferencing_call_participants", "conferencing_call_participants"),
    ("conferencing_invitations", "conferencing_invitations"),
    ("conferencing_call_dismissals", "conferencing_call_dismissals"),
    ("conferencing_recordings", "conferencing_recordings"),
]

USER_UUID_COLUMNS = {
    "caller_id",
    "user_id",
    "invitee_id",
    "recorded_by_user_id",
    "recipient_user_id",
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
            skip = False
            for key, value in dict(row).items():
                if key not in lite_cols:
                    continue
                if key in USER_UUID_COLUMNS:
                    value = map_user(value)
                    if value is None and key in ("caller_id", "user_id", "invitee_id",
                                                 "recorded_by_user_id"):
                        skip = True  # unmapped user: row is unusable
                else:
                    value = conv(value)
                cols.append(key)
                vals.append(value)
            if skip:
                continue
            lite.execute(
                f"INSERT INTO {sqlite_table} ({', '.join(cols)}) VALUES ({', '.join('?' * len(cols))})",
                vals,
            )
            inserted += 1
        lite.commit()
        print(f"{pg_table}: migrated {inserted}/{len(rows)} rows")

    # Drop orphaned children whose call row was skipped (unmapped caller).
    lite.execute(
        "DELETE FROM conferencing_call_participants WHERE call_id NOT IN (SELECT id FROM conferencing_calls)"
    )
    lite.execute(
        "DELETE FROM conferencing_invitations WHERE call_id NOT IN (SELECT id FROM conferencing_calls)"
    )
    lite.commit()
    lite.close()
    await pg.close()
    print("conferencing migration complete")


if __name__ == "__main__":
    asyncio.run(main())
