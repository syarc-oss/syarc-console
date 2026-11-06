#!/usr/bin/env python3
"""Migrate library data (music, audiobooks, ebooks, movies, photos) from the
fastmain Postgres DB into the syarc-console SQLite DB.

Library media rows are NOT encrypted in fastmain, so this is a straight copy
with user UUIDs remapped to console user ids by username.

Usage (fastmain venv has asyncpg):
    /usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_library.py

Run AFTER the console service has restarted with migration 0007 applied.
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
    ("music_authors", "music_authors"),
    ("music_albums", "music_albums"),
    ("music_album_shares", "music_album_shares"),
    ("music_tracks", "music_tracks"),
    ("music_playlists", "music_playlists"),
    ("music_playlist_items", "music_playlist_items"),
    ("audiobook_authors", "audiobook_authors"),
    ("audiobook_topics", "audiobook_topics"),
    ("audiobook_topic_shares", "audiobook_topic_shares"),
    ("audiobook_books", "audiobook_books"),
    ("audiobook_chapters", "audiobook_chapters"),
    ("audiobook_playlists", "audiobook_playlists"),
    ("audiobook_playlist_items", "audiobook_playlist_items"),
    ("ebook_authors", "ebook_authors"),
    ("ebook_categories", "ebook_categories"),
    ("ebook_topics", "ebook_topics"),
    ("ebook_topic_shares", "ebook_topic_shares"),
    ("ebook_books", "ebook_books"),
    ("ebook_book_categories", "ebook_book_categories"),
    ("ebook_reading_lists", "ebook_reading_lists"),
    ("ebook_reading_list_items", "ebook_reading_list_items"),
    ("movie_category", "movie_category"),
    ("movie_category_shares", "movie_category_shares"),
    ("movies", "movies"),
    ("photo_albums", "photo_albums"),
    ("photo_album_shares", "photo_album_shares"),
    ("photos", "photos"),
]

USER_UUID_COLUMNS = {"user_id", "shared_with_user_id", "owner_user_id"}


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
            lite.execute(
                f"INSERT OR REPLACE INTO {sqlite_table} ({', '.join(cols)}) "
                f"VALUES ({', '.join('?' * len(cols))})",
                vals,
            )
            inserted += 1
        print(f"{pg_table}: {inserted} rows")

    lite.commit()
    lite.close()
    await pg.close()
    print("Library migration complete.")


if __name__ == "__main__":
    asyncio.run(main())
