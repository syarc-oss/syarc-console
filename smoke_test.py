#!/usr/bin/env python3
"""Library smoke test for syarc-console test instance on 127.0.0.1:8091.
All requests have timeouts; downloads are truncated (HEAD-ish reads) so nothing hangs.
Usage: python3 smoke_test.py [--admin]
"""
import http.cookiejar
import sqlite3
import sys
import urllib.request

BASE = "http://127.0.0.1:8091"
DB = "/usr/local/dev_apps/syarc-console/syarc-console.db"
ADMIN = "--admin" in sys.argv

cj = http.cookiejar.CookieJar()
op = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cj))
creds = b"username=syarc&password=changeme" if ADMIN else b"username=user1&password=changeme"


def get(path):
    with op.open(BASE + path, timeout=15) as r:
        body = r.read(500_000).decode(errors="replace")
        status = r.status
    return status, body


def head(path):
    """Fetch only first bytes (avoids hanging on large file streams)."""
    req = urllib.request.Request(BASE + path)
    with op.open(req, timeout=15) as r:
        r.read(4096)
        return r.status, r.headers.get("Content-Type", "")


def main():
    print("login:", end=" ")
    get("/login") if False else None
    op.open(BASE + "/login", creds, timeout=15).read(1000)
    print("ok")

    static_pages = [
        "/library/", "/library/music/", "/library/music/browse/albums",
        "/library/music/browse/artists", "/library/music/playlists",
        "/library/audiobook/", "/library/audiobook/browse/albums",
        "/library/audiobook/browse/authors", "/library/audiobook/browse/songs",
        "/library/audiobook/browse/topics", "/library/audiobook/browse/users",
        "/library/audiobook/playlists", "/library/audiobook/player",
        "/library/movie/", "/library/movie/browse/categories",
        "/library/movie/browse/movies", "/library/movie/browse/users",
        "/library/photo/", "/library/ebook/", "/library/ebook/browse/users",
        "/library/ebook/browse/topics", "/library/ebook/browse/authors",
        "/library/ebook/browse/books", "/library/ebook/browse/categories",
        "/library/ebook/reading-lists", "/library/ebook/scan",
        "/library/ebook/search?q=test",
    ]
    if ADMIN:
        static_pages += ["/library/ebook/admin", "/library/movie/admin"]

    db = sqlite3.connect(DB)

    def one(q):
        row = db.execute(q).fetchone()
        return row[0] if row else None

    dynamic = {
        "/library/music/album/%s": "select id from music_albums limit 1",
        "/library/music/author/%s": "select id from music_authors limit 1",
        "/library/audiobook/album/%s": "select id from audiobook_books limit 1",
        "/library/audiobook/topic/%s": "select id from audiobook_topics limit 1",
        "/library/audiobook/author/%s": "select id from audiobook_authors limit 1",
        "/library/movie/category/%s": "select id from movie_category limit 1",
        "/library/photo/album/%s": "select id from photo_albums limit 1",
        "/library/photo/photo/%s": "select id from photos limit 1",
        "/library/ebook/book/%s": "select id from ebook_books limit 1",
        "/library/ebook/read/%s": "select id from ebook_books limit 1",
        "/library/ebook/topic/%s": "select id from ebook_topics limit 1",
        "/library/ebook/author/%s": "select id from ebook_authors limit 1",
    }
    downloads = {
        "/library/ebook/download/%s": "select id from ebook_books limit 1",
    }

    failures = 0
    for p in static_pages:
        try:
            status, body = get(p)
            bad = "Template error" in body or status != 200
            failures += bad
            print(f"{'FAIL' if bad else 'ok':4} {p}" + (" TEMPLATE-ERR" if "Template error" in body else ""))
        except Exception as e:
            failures += 1
            print(f"FAIL {p} EXC {e}")

    for tpl, q in dynamic.items():
        v = one(q)
        if v is None:
            print(f"skip {tpl} (no data)")
            continue
        try:
            status, body = get(tpl % v)
            bad = "Template error" in body or status != 200
            failures += bad
            print(f"{'FAIL' if bad else 'ok':4} {tpl % v}" + (" TEMPLATE-ERR" if "Template error" in body else ""))
        except Exception as e:
            failures += 1
            print(f"FAIL {tpl % v} EXC {e}")

    for tpl, q in downloads.items():
        v = one(q)
        if v is None:
            continue
        try:
            status, ctype = head(tpl % v)
            print(f"ok   {tpl % v} -> {status} {ctype}")
        except Exception as e:
            failures += 1
            print(f"FAIL {tpl % v} EXC {e}")

    agri_pages = [
        "/agriculture/", "/agriculture/agronomy/", "/agriculture/husbandry/",
        "/agriculture/foraging/",
        "/orchard/", "/eggs/", "/eggs/dashboard", "/milk/", "/meat/",
        "/agriculture/agronomy/garden/",
    ]
    for p in agri_pages:
        try:
            status, body = get(p)
            bad = "Template error" in body or status != 200
            failures += bad
            print(f"{'FAIL' if bad else 'ok':4} {p}" + (" TEMPLATE-ERR" if "Template error" in body else ""))
        except Exception as e:
            failures += 1
            print(f"FAIL {p} EXC {e}")

    agri_dynamic = {
        "/orchard/%s": "select id from orchards limit 1",
        "/orchard/%s/trees": "select id from orchards limit 1",
        "/orchard/%s/maintenance": "select id from orchards limit 1",
        "/orchard/%s/trees/%s": "select o.id, t.id from orchards o join trees t on t.orchard_id = o.id limit 1",
        "/agriculture/agronomy/garden/%s": "select id from gardens limit 1",
        "/agriculture/agronomy/garden/%s/maintenance": "select id from gardens limit 1",
        "/agriculture/agronomy/garden/%s/crop/%s": "select g.id, c.id from gardens g join garden_crops c on c.garden_id = g.id limit 1",
        "/eggs/coops/%s": "select id from coops limit 1",
        "/eggs/coops/%s/hens": "select id from coops limit 1",
        "/eggs/coops/%s/entry": "select id from coops limit 1",
        "/eggs/coops/%s/reports": "select id from coops limit 1",
        "/milk/groups/%s": "select id from milk_groups limit 1",
        "/milk/groups/%s/members": "select id from milk_groups limit 1",
        "/milk/groups/%s/reports": "select id from milk_groups limit 1",
        "/milk/groups/%s/production": "select id from milk_groups limit 1",
        "/milk/groups/%s/animals/%s/medical": "select g.id, a.id from milk_groups g join milk_animals a on a.group_id = g.id limit 1",
        "/meat/groups/%s": "select id from meat_groups limit 1",
        "/meat/groups/%s/members": "select id from meat_groups limit 1",
        "/meat/groups/%s/animals/%s/medical": "select g.id, a.id from meat_groups g join meat_animals a on a.group_id = g.id limit 1",
        "/meat/groups/%s/animals/%s/process": "select g.id, a.id from meat_groups g join meat_animals a on a.group_id = g.id limit 1",
        "/meat/groups/%s/animals/%s/eval": "select g.id, a.id from meat_groups g join meat_animals a on a.group_id = g.id limit 1",
        "/agriculture/foraging/%s/participants": "select id from foraging_outings limit 1",
        "/agriculture/foraging/%s/results": "select id from foraging_outings limit 1",
    }
    for tpl, q in agri_dynamic.items():
        row = db.execute(q).fetchone()
        if row is None:
            print(f"skip {tpl} (no data)")
            continue
        vals = row if isinstance(row, tuple) else (row,)
        path = tpl % vals
        try:
            status, body = get(path)
            bad = "Template error" in body or status != 200
            failures += bad
            print(f"{'FAIL' if bad else 'ok':4} {path}" + (" TEMPLATE-ERR" if "Template error" in body else ""))
        except Exception as e:
            failures += 1
            print(f"FAIL {path} EXC {e}")

    agri_api = {
        "/eggs/coops/%s/hens/list": "select id from coops limit 1",
        "/eggs/coops/%s/production/list": "select id from coops limit 1",
        "/eggs/coops/%s/production/hens": "select id from coops limit 1",
        "/eggs/coops/%s/production/stats": "select id from coops limit 1",
        "/milk/groups/%s/animals/list": "select id from milk_groups limit 1",
        "/milk/groups/%s/stats": "select id from milk_groups limit 1",
        "/meat/groups/%s/animals/list": "select id from meat_groups limit 1",
        "/meat/groups/%s/stats": "select id from meat_groups limit 1",
        "/orchard/%s/trees/list": "select id from orchards limit 1",
        "/orchard/%s/stats": "select id from orchards limit 1",
    }
    for tpl, q in agri_api.items():
        v = one(q)
        if v is None:
            continue
        try:
            status, body = get(tpl % v)
            bad = "Template error" in body or status != 200
            failures += bad
            print(f"{'FAIL' if bad else 'ok':4} {tpl % v} {body[:60]}" + (" TEMPLATE-ERR" if "Template error" in body else ""))
        except Exception as e:
            failures += 1
            print(f"FAIL {tpl % v} EXC {e}")

    conf_paths = [
        "/conferencing/", "/conferencing/history",
        "/conferencing/api/online", "/conferencing/api/history",
        "/conferencing/api/ice-config", "/conferencing/api/my-active-call",
        "/conferencing/api/recordings",
    ]
    for p in conf_paths:
        try:
            status, body = get(p)
            bad = "Template error" in body or status != 200
            failures += bad
            print(f"{'FAIL' if bad else 'ok':4} {p}" + (" TEMPLATE-ERR" if "Template error" in body else ""))
        except Exception as e:
            failures += 1
            print(f"FAIL {p} EXC {e}")

    print(f"\n{failures} failure(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
