#!/usr/bin/env python3
"""syarc-console end-to-end release test suite.

Starts a fresh console instance on PORT with a throwaway database, then
exercises every app over HTTP: auth, admin, supply, calendar, cooking,
agriculture (garden/orchard), surveillance, wordgame, hangman, meshlink,
library, pecuniary, conferencing. Covers add/edit/delete, share/unshare,
paging, permissions, and negative cases. Any 5xx (or wrong redirect/body)
is reported as FAIL.

Usage: python3 console_e2e.py [--base http://127.0.0.1:8091] [--db /tmp/x.db]
       (If --db omitted, the script starts its own server instance.)
"""

import argparse
import http.client
import json
import os
import re
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.parse
from http.cookiejar import CookieJar

RESULTS = []


def record(app, name, ok, detail=""):
    RESULTS.append((app, name, ok, detail))
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {app}: {name}" + (f"  -- {detail}" if not ok and detail else ""))


class Client:
    def __init__(self, base):
        self.base = base
        p = urllib.parse.urlparse(base)
        self.host, self.port = p.hostname, p.port
        self.jar = CookieJar()

    def _req(self, method, path, body=None, headers=None, follow=True, depth=0):
        conn = http.client.HTTPConnection(self.host, self.port, timeout=15)
        hdrs = dict(headers or {})
        if body is not None:
            if isinstance(body, dict) and "Content-Type" not in hdrs:
                body = urllib.parse.urlencode(body)
                hdrs["Content-Type"] = "application/x-www-form-urlencoded"
            hdrs["Content-Length"] = str(len(body))
        self.jar.add_cookie_header(
            type("R", (), {"url": self.base, "get_full_url": lambda s: self.base})()
        )
        cookies = "; ".join(
            f"{c.name}={c.value}" for c in self.jar
        )
        if cookies:
            hdrs["Cookie"] = cookies
        conn.request(method, path, body=body, headers=hdrs)
        resp = conn.getresponse()
        data = resp.read().decode("utf-8", "replace")
        set_cookies = resp.getheaders()
        for name, value in set_cookies:
            if name.lower() == "set-cookie":
                self.jar.extract_cookies(
                    resp,
                    type("Req", (), {"url": self.base, "headers": {}})(),
                )
                # extract_cookies needs the response-like obj; fallback manual:
        # manual cookie handling (simple id=... session cookie)
        sc = [v for k, v in resp.getheaders() if k.lower() == "set-cookie"]
        if sc:
            m = re.match(r"([^=]+)=([^;]*)", sc[0])
            if m:
                self.jar._manual = getattr(self.jar, "_manual", {})
                self.jar._manual[m.group(1)] = m.group(2)
        status = resp.status
        location = resp.getheader("Location")
        conn.close()
        if follow and status in (301, 302, 303, 307) and location and depth < 5:
            return self._req("GET", location, depth=depth + 1)
        return status, data, location

    def cookie_header(self):
        m = getattr(self.jar, "_manual", {})
        return "; ".join(f"{k}={v}" for k, v in m.items())

    def get(self, path, follow=True):
        hdrs = {}
        if self.cookie_header():
            hdrs["Cookie"] = self.cookie_header()
        return self._req_nocookiestore("GET", path, hdrs, follow)

    def _req_nocookiestore(self, method, path, hdrs, follow, body=None, depth=0):
        conn = http.client.HTTPConnection(self.host, self.port, timeout=15)
        if body is not None:
            if isinstance(body, dict):
                body = urllib.parse.urlencode(body)
                hdrs["Content-Type"] = "application/x-www-form-urlencoded"
            hdrs["Content-Length"] = str(len(body))
        conn.request(method, path, body=body, headers=hdrs)
        resp = conn.getresponse()
        data = resp.read().decode("utf-8", "replace")
        status = resp.status
        location = resp.getheader("Location")
        for k, v in resp.getheaders():
            if k.lower() == "set-cookie":
                m = re.match(r"([^=]+)=([^;]*)", v)
                if m:
                    self.jar._manual = getattr(self.jar, "_manual", {})
                    self.jar._manual[m.group(1)] = m.group(2)
                    if "Cookie" in hdrs:
                        hdrs["Cookie"] += "; "
                    hdrs.setdefault("Cookie", "")
                    if not hdrs["Cookie"].endswith("; ") and hdrs["Cookie"]:
                        hdrs["Cookie"] += "; "
                    hdrs["Cookie"] += f"{m.group(1)}={m.group(2)}"
        conn.close()
        if follow and status in (301, 302, 303, 307) and location and depth < 5:
            return self._req_nocookiestore("GET", location, dict(hdrs), True, depth=depth + 1)
        return status, data, location

    def post(self, path, form=None, follow=True, raw=None, ctype=None):
        hdrs = {}
        if self.cookie_header():
            hdrs["Cookie"] = self.cookie_header()
        body = raw if raw is not None else form
        if ctype:
            hdrs["Content-Type"] = ctype
        return self._req_nocookiestore("POST", path, hdrs, follow, body=body)

    def put(self, path, form=None, follow=False):
        hdrs = {"Cookie": self.cookie_header()} if self.cookie_header() else {}
        return self._req_nocookiestore("PUT", path, hdrs, follow, body=form)

    def patch(self, path, form=None, follow=False):
        hdrs = {"Cookie": self.cookie_header()} if self.cookie_header() else {}
        return self._req_nocookiestore("PATCH", path, hdrs, follow, body=form)

    def delete(self, path, follow=False):
        hdrs = {"Cookie": self.cookie_header()} if self.cookie_header() else {}
        return self._req_nocookiestore("DELETE", path, hdrs, follow)


def db_query(dbpath, sql, args=()):
    con = sqlite3.connect(f"file:{dbpath}?mode=ro", uri=True)
    try:
        return con.execute(sql, args).fetchall()
    finally:
        con.close()


def db_id(dbpath, table, cond="1=1", order="rowid DESC"):
    rows = db_query(dbpath, f"SELECT id FROM {table} WHERE {cond} ORDER BY {order} LIMIT 1")
    return rows[0][0] if rows else None


# --------------------------------------------------------------- test groups

def test_auth(base, db):
    c = Client(base)
    s, b, _ = c.get("/register")
    record("auth", "register page renders", s == 200 and "Register" in b, f"status={s}")
    s, b, loc = c.post("/register", {"username": "t_admin", "email": "t_admin@test", "password": "pw123456"}, follow=False)
    record("auth", "first user registers", s in (302, 303) and loc == "/", f"status={s} loc={loc}")
    s, b, _ = c.get("/")
    record("auth", "admin logged in after register", s == 200, f"status={s}")

    c2 = Client(base)
    s, b, loc = c2.post("/register", {"username": "t_user", "email": "t_user@test", "password": "pw123456"}, follow=False)
    record("auth", "second user registers", s in (302, 303), f"status={s}")
    s, b, _ = c2.get("/")
    record("auth", "second user logged in", s == 200, f"status={s}")

    c3 = Client(base)
    s, b, loc = c3.post("/register", {"username": "t_user", "email": "x@test", "password": "pw123456"}, follow=False)
    record("auth", "duplicate username rejected", s == 200 and "taken" in b, f"status={s}")

    c4 = Client(base)
    s, b, _ = c4.post("/login", {"username": "t_user", "password": "WRONG"}, follow=False)
    record("auth", "wrong password rejected", s in (200, 302) and ("Invalid" in b or s == 200), f"status={s}")
    s, b, _ = c4.get("/supply")
    record("auth", "anonymous blocked from apps", s in (302, 303) or "Log in" in b, f"status={s}")
    s, b, loc = c4.post("/login", {"username": "t_user", "password": "pw123456"}, follow=False)
    record("auth", "login works", s in (302, 303) and loc == "/", f"status={s} loc={loc}")
    s, b, loc = c4.post("/logout", follow=False)
    record("auth", "logout works", s in (302, 303), f"status={s}")
    return c, c2


def test_admin(base, db, admin, user):
    s, b, _ = admin.get("/admin")
    record("admin", "dashboard renders", s == 200, f"status={s}")
    s, b, _ = admin.get("/admin/users")
    record("admin", "users list renders", s == 200 and "t_user" in b, f"status={s}")
    uid = db_id(db, "users", "username='t_user'")
    # grant every app to t_user via permissions form
    apps = [r[0] for r in db_query(db, "SELECT id FROM apps")]
    form = {f"perm_{a}": "1" for a in apps}
    s, b, loc = admin.post(f"/admin/users/{uid}/permissions", form, follow=False)
    record("admin", "grant app permissions", s in (302, 303), f"status={s}")
    perms = db_query(db, "SELECT COUNT(*) FROM user_app_permissions WHERE user_id=?", (uid,))[0][0]
    record("admin", "permissions persisted", perms == len(apps), f"{perms}/{len(apps)}")
    s, b, _ = user.get("/supply")
    record("admin", "gated app now accessible", s == 200, f"status={s}")
    s, b, _ = admin.get("/admin/settings")
    record("admin", "settings page renders", s == 200, f"status={s}")
    s, b, loc = admin.post("/admin/settings", {"key": "bearden_bridge_url", "value": "http://127.0.0.1:8095"}, follow=False)
    record("admin", "settings save", s in (302, 303, 200), f"status={s}")
    s, b, _ = admin.get("/admin/system")
    record("admin", "system page renders", s == 200, f"status={s}")
    s, b, _ = user.get("/admin")
    record("admin", "non-admin blocked from admin", s in (302, 303, 403), f"status={s}")


def test_supply(base, db, admin, user):
    s, b, loc = user.post("/supply/store/new", {"name": "Test Pantry", "description": "main"}, follow=False)
    record("supply", "store create", s in (302, 303), f"status={s} loc={loc}")
    sid = db_id(db, "supply_stores", "name='Test Pantry'")
    record("supply", "store persisted", sid is not None)
    s, b, _ = user.get(f"/supply/stores/{sid}")
    record("supply", "store view", s == 200 and "Test Pantry" in b, f"status={s}")
    s, b, _ = user.get(f"/supply/store/{sid}/edit")
    record("supply", "store edit form", s == 200, f"status={s}")
    s, b, loc = user.post(f"/supply/store/{sid}/edit", {"name": "Test Pantry 2", "description": "d2"}, follow=False)
    record("supply", "store update", s in (302, 303), f"status={s}")
    name = db_query(db, "SELECT name FROM supply_stores WHERE id=?", (sid,))[0][0]
    record("supply", "store rename persisted", name == "Test Pantry 2", name)
    # category + item
    s, b, loc = user.post("/supply/categories/new", {"name": "Canned", "store_id": sid}, follow=False)
    record("supply", "category create", s in (302, 303), f"status={s}")
    cid = db_id(db, "supply_categories", "name='Canned'")
    s, b, _ = user.get(f"/supply/categories/{cid}")
    record("supply", "category view", s == 200, f"status={s}")
    s, b, _ = user.get(f"/supply/categories/{cid}/edit")
    record("supply", "category edit form", s == 200, f"status={s}")
    s, b, loc = user.post(f"/supply/categories/{cid}/edit", {"name": "Canned 2", "store_id": sid}, follow=False)
    record("supply", "category update", s in (302, 303), f"status={s}")
    s, b, loc = user.post("/supply/items/new", {"key": "Beans", "category_id": cid, "quantity": "4", "minimum": "2", "location": "shelf"}, follow=False)
    record("supply", "item create", s in (302, 303), f"status={s}")
    iid = db_id(db, "supply_items", "key='Beans'")
    record("supply", "item persisted", iid is not None)
    s, b, _ = user.get(f"/supply/items/{iid}/edit")
    record("supply", "item edit form", s == 200, f"status={s}")
    s, b, loc = user.post(f"/supply/items/{iid}/edit", {"key": "Beans", "category_id": cid, "quantity": "9", "minimum": "2"}, follow=False)
    record("supply", "item update", s in (302, 303), f"status={s}")
    qty = db_query(db, "SELECT quantity FROM supply_items WHERE id=?", (iid,))[0][0]
    record("supply", "item qty persisted", qty == 9, str(qty))
    # adjust/transfer APIs
    s, b, _ = user.post(f"/supply/api/items/{iid}/adjust", raw=json.dumps({"adjustment": -3}), ctype="application/json")
    record("supply", "api adjust", s == 200, f"status={s} {b[:100]}")
    qty = db_query(db, "SELECT quantity FROM supply_items WHERE id=?", (iid,))[0][0]
    record("supply", "adjust persisted", qty == 6, str(qty))
    # share/unshare
    uid2 = db_id(db, "users", "username='t_admin'")
    s, b, loc = user.post(f"/supply/store/{sid}/share", {"share_with_user_id": str(uid2)}, follow=False)
    record("supply", "store share", s in (302, 303), f"status={s}")
    shares = db_query(db, f"SELECT id FROM supply_store_shares WHERE store_id={sid}")
    shid = shares[0][0] if shares else None
    record("supply", "share persisted", shid is not None)
    s, b, _ = admin.get(f"/supply/stores/{sid}")
    record("supply", "shared store visible to sharee", s == 200 and "Test Pantry 2" in b, f"status={s}")
    s, b, loc = user.post(f"/supply/store/{sid}/revoke/{shid}", follow=False)
    record("supply", "share revoke", s in (302, 303), f"status={s}")
    n = db_query(db, f"SELECT COUNT(*) FROM supply_store_shares WHERE store_id={sid}")[0][0]
    record("supply", "revoke persisted", n == 0, str(n))
    # scanner queue
    s, b, _ = user.post("/supply/api/scanner/queue/add", raw=json.dumps({"barcode": "1234567890", "description": "ScanBean"}), ctype="application/json")
    record("supply", "scanner queue add", s == 200, f"status={s} {b[:100]}")
    s, b, _ = user.get("/supply/api/scanner/pending")
    record("supply", "scanner pending list", s == 200 and "1234567890" in b, f"status={s}")
    s, b, _ = user.post("/supply/api/scanner/batch-all", raw=json.dumps({"category_id": cid, "location": "shelf"}), ctype="application/json")
    record("supply", "scanner batch-all", s == 200, f"status={s} {b[:150]}")
    n = db_query(db, f"SELECT COUNT(*) FROM supply_items WHERE key='ScanBean'")[0][0]
    record("supply", "batch-all committed item", n == 1, str(n))
    # pages & search
    for path in ("/supply", "/supply/search?q=bean", "/supply/barcode-search?code=1234567890", "/supply/scanner", "/supply/print-form"):
        s, b, _ = user.get(path)
        record("supply", f"page {path}", s == 200, f"status={s}")
    # delete chain
    s, b, loc = user.post(f"/supply/items/{iid}/delete", follow=False)
    record("supply", "item delete", s in (302, 303), f"status={s}")
    s, b, loc = user.get(f"/supply/categories/{cid}/delete")
    record("supply", "category delete page", s == 200, f"status={s}")
    s, b, loc = user.post(f"/supply/categories/{cid}/delete", {"confirm": "1"}, follow=False)
    record("supply", "category delete confirm", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/supply/store/{sid}/delete", follow=False)
    record("supply", "store delete", s in (302, 303), f"status={s}")
    n = db_query(db, "SELECT COUNT(*) FROM supply_stores WHERE name LIKE 'Test Pantry%'")[0][0]
    record("supply", "store gone", n == 0, str(n))


def test_calendar(base, db, admin, user):
    s, b, _ = user.get("/calendar")
    record("calendar", "month view", s == 200, f"status={s}")
    s, b, _ = user.get("/calendar/day/2026-10-24")
    record("calendar", "day view", s == 200, f"status={s}")
    s, b, _ = user.get("/calendar/tasks")
    record("calendar", "tasks list", s == 200, f"status={s}")
    s, b, loc = user.post("/calendar/tasks/new", {"description": "Test task", "due_date": "2026-10-25"}, follow=False)
    record("calendar", "task create", s in (302, 303), f"status={s} loc={loc}")
    tid = db_id(db, "tasks", "description='Test task'")
    record("calendar", "task persisted", tid is not None)
    s, b, _ = user.get(f"/calendar/tasks/{tid}/edit")
    record("calendar", "task edit form", s == 200 and "Test task" in b, f"status={s}")
    s, b, loc = user.post(f"/calendar/tasks/{tid}/edit", {"description": "Test task v2", "due_date": "2026-10-26"}, follow=False)
    record("calendar", "task update", s in (302, 303), f"status={s}")
    uid2 = db_id(db, "users", "username='t_admin'")
    s, b, loc = user.post(f"/calendar/tasks/{tid}/crew/add", {"user_id": str(uid2)}, follow=False)
    record("calendar", "crew invite", s in (302, 303), f"status={s}")
    crew = db_query(db, f"SELECT id FROM task_crew WHERE task_id='{tid}'")
    crid = crew[0][0] if crew else None
    record("calendar", "crew persisted", crid is not None)
    s, b, loc = admin.post(f"/calendar/tasks/{tid}/crew/{crid}/respond", {"action": "confirm", "back": "/calendar/invites"}, follow=False)
    record("calendar", "crew respond accept", s in (302, 303), f"status={s}")
    s, b, _ = admin.get("/calendar/invites")
    record("calendar", "invites panel", s == 200, f"status={s}")
    s, b, loc = user.post(f"/calendar/tasks/{tid}/crew/{crid}/remove", follow=False)
    record("calendar", "crew remove", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/calendar/tasks/{tid}/status", {"status": "completed"}, follow=False)
    record("calendar", "task status set", s in (302, 303), f"status={s}")
    # chores
    s, b, loc = user.post("/calendar/chores/new", {"description": "Water plants", "frequency": "weekly", "start_date": "2026-10-24"}, follow=False)
    record("calendar", "chore create", s in (302, 303), f"status={s} loc={loc}")
    chid = db_id(db, "chores", "description='Water plants'")
    record("calendar", "chore persisted", chid is not None)
    s, b, loc = user.post(f"/calendar/chores/{chid}/complete", follow=False)
    record("calendar", "chore complete", s in (302, 303), f"status={s}")
    n = db_query(db, f"SELECT COUNT(*) FROM chore_completions WHERE chore_id='{chid}'")[0][0]
    record("calendar", "completion logged", n >= 1, str(n))
    s, b, loc = user.post(f"/calendar/chores/{chid}/edit", {"description": "Water plants v2", "frequency": "weekly", "start_date": "2026-10-24"}, follow=False)
    record("calendar", "chore update", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/calendar/chores/{chid}/delete", follow=False)
    record("calendar", "chore delete", s in (302, 303), f"status={s}")
    # events
    s, b, _ = user.get("/calendar/events")
    record("calendar", "events list", s == 200, f"status={s}")
    s, b, loc = user.post("/calendar/events/new", {"title": "Standup", "category": "meeting", "event_date": "2026-10-26", "event_time": "09:00", "duration_minutes": "30"}, follow=False)
    record("calendar", "event create", s in (302, 303), f"status={s} loc={loc}")
    eid = db_id(db, "calendar_events", "title='Standup'")
    record("calendar", "event persisted", eid is not None)
    s, b, _ = user.get(f"/calendar/events/{eid}/edit")
    record("calendar", "event edit form", s == 200, f"status={s}")
    s, b, loc = user.post(f"/calendar/events/{eid}/invitees/add", {"user_id": str(uid2)}, follow=False)
    record("calendar", "invitee add", s in (302, 303), f"status={s}")
    inv = db_query(db, f"SELECT id FROM calendar_event_invitees WHERE event_id='{eid}'")
    invid = inv[0][0] if inv else None
    record("calendar", "invitee persisted", invid is not None)
    s, b, loc = admin.post(f"/calendar/events/{eid}/invitees/{invid}/respond", {"action": "accept", "back": "/calendar/events"}, follow=False)
    record("calendar", "invitee respond", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/calendar/events/{eid}/invitees/{invid}/remove", follow=False)
    record("calendar", "invitee remove", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/calendar/events/{eid}/delete", follow=False)
    record("calendar", "event delete", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/calendar/tasks/{tid}/delete", follow=False)
    record("calendar", "task delete", s in (302, 303), f"status={s}")
    s, b, _ = user.get("/calendar/weather")
    record("calendar", "weather page", s == 200, f"status={s}")


def test_cooking(base, db, admin, user):
    s, b, _ = user.get("/cooking")
    record("cooking", "landing", s == 200, f"status={s}")
    s, b, loc = user.post("/cooking/recipes/new", {"title": "Test Soup", "description": "yummy", "steps": "boil\nseason"}, follow=False)
    record("cooking", "recipe create", s in (302, 303), f"status={s} loc={loc}")
    table = "cooking_recipes"
    rid = db_id(db, table, "title='Test Soup'")
    s, b, _ = user.get(f"/cooking/recipes/{rid}")
    record("cooking", "recipe detail", s == 200 and "Test Soup" in b, f"status={s}")
    s, b, _ = user.get(f"/cooking/recipes/{rid}/edit")
    record("cooking", "recipe edit form", s == 200, f"status={s}")
    s, b, loc = user.post(f"/cooking/recipes/{rid}/edit", {"title": "Test Soup v2", "description": "yummy", "steps": "boil"}, follow=False)
    record("cooking", "recipe update", s in (302, 303), f"status={s}")
    t = db_query(db, f"SELECT title FROM {table} WHERE id=?", (rid,))[0][0]
    record("cooking", "recipe rename persisted", t == "Test Soup v2", t)
    s, b, loc = user.post(f"/cooking/recipes/{rid}/add-to-menu", {"menu_date": "2026-10-26", "meal_type": "dinner"}, follow=False)
    record("cooking", "add to menu", s in (302, 303), f"status={s}")
    s, b, _ = user.get("/cooking/menu")
    record("cooking", "menu calendar", s == 200 and "Test Soup" in b, f"status={s}")
    entry = db_query(db, "SELECT id FROM cooking_menu_entries WHERE recipe_id=?", (rid,))
    eid = entry[0][0] if entry else None
    uid2 = db_id(db, "users", "username='t_admin'")
    if eid:
        s, b, loc = user.post(f"/cooking/menu/{eid}/cook", {"cook_user_id": str(uid2)}, follow=False)
        record("cooking", "assign cook", s in (302, 303), f"status={s}")
        s, b, loc = user.post(f"/cooking/menu/{eid}/delete", follow=False)
        record("cooking", "menu entry delete", s in (302, 303), f"status={s}")
    s, b, _ = user.get("/cooking/shopping-list")
    record("cooking", "shopping list", s == 200, f"status={s}")
    s, b, _ = user.get("/cooking/search?q=soup")
    record("cooking", "search page", s == 200, f"status={s}")
    s, b, _ = user.get("/cooking/api/recipes?q=soup")
    record("cooking", "api recipe search", s == 200, f"status={s}")
    s, b, loc = user.post(f"/cooking/recipes/{rid}/delete", follow=False)
    record("cooking", "recipe delete", s in (302, 303), f"status={s}")
    n = db_query(db, f"SELECT COUNT(*) FROM {table} WHERE id=?", (rid,))[0][0]
    record("cooking", "recipe gone", n == 0, str(n))


def test_agriculture(base, db, admin, user):
    # garden
    s, b, _ = user.get("/agriculture/agronomy/garden")
    record("agriculture", "garden root", s == 200, f"status={s}")
    s, b, loc = user.post("/agriculture/agronomy/garden/new", {"name": "Backyard", "location": "yard"}, follow=False)
    record("agriculture", "garden create", s in (302, 303), f"status={s} loc={loc}")
    gid = db_id(db, "gardens", "name='Backyard'")
    record("agriculture", "garden persisted", gid is not None)
    s, b, _ = user.get(f"/agriculture/agronomy/garden/{gid}")
    record("agriculture", "garden view", s == 200 and "Backyard" in b, f"status={s}")
    s, b, _ = user.get(f"/agriculture/agronomy/garden/{gid}/edit")
    record("agriculture", "garden edit form", s == 200, f"status={s}")
    s, b, loc = user.post(f"/agriculture/agronomy/garden/{gid}/edit", {"name": "Backyard 2", "location": "yard"}, follow=False)
    record("agriculture", "garden update", s in (302, 303), f"status={s}")
    # share/unshare
    uid2 = db_id(db, "users", "username='t_admin'")
    s, b, loc = user.post(f"/agriculture/agronomy/garden/{gid}/share", {"share_with_user_id": str(uid2)}, follow=False)
    record("agriculture", "garden share", s in (302, 303), f"status={s}")
    sh = db_query(db, f"SELECT id FROM garden_shares WHERE garden_id='{gid}'")
    shid = sh[0][0] if sh else None
    record("agriculture", "garden share persisted", shid is not None)
    s, b, _ = admin.get(f"/agriculture/agronomy/garden/{gid}")
    record("agriculture", "sharee can view garden", s == 200 and "Backyard" in b, f"status={s}")
    s, b, loc = user.post(f"/agriculture/agronomy/garden/{gid}/revoke/{shid}", follow=False)
    record("agriculture", "garden revoke", s in (302, 303), f"status={s}")
    # crop + maintenance
    s, b, loc = user.post(f"/agriculture/agronomy/garden/{gid}/crops/create", {"crop": "Tomatoes", "variety": "Roma", "status": "growing"}, follow=False)
    record("agriculture", "crop create", s == 200, f"status={s} {b[:100]}")
    crid = db_id(db, "garden_crops", f"garden_id='{gid}'")
    record("agriculture", "crop persisted", crid is not None)
    s, b, _ = user.get(f"/agriculture/agronomy/garden/{gid}/crop/{crid}")
    record("agriculture", "crop detail", s == 200 and "Tomatoes" in b, f"status={s}")
    s, b, loc = user.post(f"/agriculture/agronomy/garden/{gid}/crops/{crid}/maintenance/add", {"activity_date": "2026-10-24", "activity_type": "watering", "person": "t_user"}, follow=False)
    record("agriculture", "crop maintenance add", s == 200, f"status={s} {b[:120]}")
    # plot maintenance (regression for today's fixes)
    s, b, _ = user.get(f"/agriculture/agronomy/garden/{gid}/maintenance")
    record("agriculture", "plot maintenance page", s == 200 and "Watering" in b and "<option value=\"watering\"" in b, f"status={s}")
    s, b, _ = user.post(f"/agriculture/agronomy/garden/{gid}/maintenance/add", {"activity_date": "2026-10-24", "activity_type": "weeding", "person": "t_user"})
    record("agriculture", "plot maintenance add", s == 200, f"status={s} {b[:120]}")
    s, b, _ = user.get(f"/agriculture/agronomy/garden/{gid}/maintenance")
    record("agriculture", "plot maintenance records render", "Weeding" in b)
    mrec = db_id(db, "garden_maintenance", f"garden_id='{gid}'")
    s, b, _ = user.put(f"/agriculture/agronomy/garden/{gid}/maintenance/{mrec}", {"activity_date": "2026-10-24", "activity_type": "mulching", "person": "t_user"})
    record("agriculture", "plot maintenance update (PUT)", s == 200, f"status={s} {b[:120]}")
    s, b, _ = user.delete(f"/agriculture/agronomy/garden/{gid}/maintenance/{mrec}")
    record("agriculture", "plot maintenance delete", s == 200, f"status={s} {b[:120]}")
    s, b, loc = user.post(f"/agriculture/agronomy/garden/{gid}/crops/{crid}/maintenance/{db_id(db,'garden_crop_maintenance',f'crop_id=\'{crid}\'')}", follow=False) if False else (200, "", "")
    s, b, _ = user.delete(f"/agriculture/agronomy/garden/{gid}/crops/{crid}")
    record("agriculture", "crop delete", s == 200 or s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/agriculture/agronomy/garden/{gid}/delete", follow=False)
    record("agriculture", "garden delete", s in (302, 303), f"status={s}")
    # orchard
    s, b, loc = user.post("/orchard/new", {"name": "West Orchard", "location": "west"}, follow=False)
    record("agriculture", "orchard create", s in (302, 303), f"status={s} loc={loc}")
    oid = db_id(db, "orchards", "name='West Orchard'")
    record("agriculture", "orchard persisted", oid is not None)
    s, b, _ = user.get(f"/orchard/{oid}")
    record("agriculture", "orchard view", s == 200, f"status={s}")
    s, b, loc = user.post(f"/orchard/{oid}/edit", {"name": "West Orchard 2"}, follow=False)
    record("agriculture", "orchard update", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/orchard/{oid}/share", {"share_with_user_id": str(uid2)}, follow=False)
    record("agriculture", "orchard share", s in (302, 303), f"status={s}")
    sh = db_query(db, f"SELECT id FROM orchard_shares WHERE orchard_id='{oid}'")
    s, b, loc = user.post(f"/orchard/{oid}/revoke/{sh[0][0]}", follow=False)
    record("agriculture", "orchard revoke", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/orchard/{oid}/trees/create", {"tree_type": "apple", "variety": "Honeycrisp"}, follow=False)
    record("agriculture", "tree create", s in (200, 302, 303), f"status={s} {b[:100] if s==200 else loc}")
    trid = db_id(db, "trees", f"orchard_id='{oid}'")
    record("agriculture", "tree persisted", trid is not None)
    if trid:
        s, b, _ = user.get(f"/orchard/{oid}/trees/{trid}")
        record("agriculture", "tree detail", s == 200, f"status={s}")
        s, b, _ = user.patch(f"/orchard/{oid}/trees/{trid}/toggle")
        record("agriculture", "tree toggle", s in (200, 302), f"status={s} {b[:100]}")
        s, b, loc = user.post(f"/orchard/{oid}/trees/{trid}/maintenance/add", {"maintenance_date": "2026-10-24", "maintenance_type": "pruning"}, follow=False)
        record("agriculture", "tree maintenance add", s in (200, 302, 303), f"status={s}")
        s, b, _ = user.delete(f"/orchard/{oid}/trees/{trid}")
        record("agriculture", "tree delete", s == 200, f"status={s}")
    s, b, _ = user.get(f"/orchard/{oid}/maintenance")
    record("agriculture", "orchard maintenance page", s == 200, f"status={s}")
    s, b, loc = user.post(f"/orchard/{oid}/maintenance/add", {"maintenance_date": "2026-10-24", "maintenance_type": "pruning", "description": "x"}, follow=False)
    record("agriculture", "orchard maintenance add", s in (200, 302, 303), f"status={s}")
    s, b, _ = user.get(f"/orchard/{oid}/stats")
    record("agriculture", "orchard stats", s == 200, f"status={s}")
    s, b, loc = user.post(f"/orchard/{oid}/delete", follow=False)
    record("agriculture", "orchard delete", s in (302, 303), f"status={s}")


def test_surveillance(base, db, admin, user):
    s, b, _ = user.get("/surveillance")
    record("surveillance", "landing", s == 200, f"status={s}")
    s, b, loc = user.post("/surveillance/zone/new", {"name": "Yard", "description": "outside"}, follow=False)
    record("surveillance", "zone create", s in (302, 303), f"status={s} loc={loc}")
    zid = db_id(db, "surveillance_zones", "name='Yard'")
    record("surveillance", "zone persisted", zid is not None)
    s, b, _ = user.get(f"/surveillance/zone/{zid}")
    record("surveillance", "zone view", s == 200, f"status={s}")
    s, b, _ = user.get(f"/surveillance/zone/{zid}/edit")
    record("surveillance", "zone edit form", s == 200, f"status={s}")
    s, b, loc = user.post(f"/surveillance/zone/{zid}/edit", {"name": "Yard 2"}, follow=False)
    record("surveillance", "zone update", s in (302, 303), f"status={s}")
    uid2 = db_id(db, "users", "username='t_admin'")
    s, b, loc = user.post(f"/surveillance/zone/{zid}/share", {"username": "t_admin"}, follow=False)
    record("surveillance", "zone share", s in (302, 303), f"status={s}")
    sh = db_query(db, "SELECT id FROM surveillance_zone_shares WHERE zone_id=?", (zid,))
    shid = sh[0][0] if sh else None
    record("surveillance", "share persisted", shid is not None)
    if shid:
        s, b, loc = user.post(f"/surveillance/zone/{zid}/share/{shid}/delete", follow=False)
        record("surveillance", "share delete", s in (302, 303), f"status={s}")
    # camera CRUD with recording disabled (no pollers spawn)
    s, b, loc = user.post(f"/surveillance/zone/{zid}/camera/new", {"name": "Cam1", "stream_url": "http://127.0.0.1:1/x.mjpeg", "stream_type": "mjpeg", "recording_enabled": "", "alarm_enabled": ""}, follow=False)
    record("surveillance", "camera create", s in (302, 303), f"status={s} loc={loc}")
    camid = db_id(db, "surveillance_cameras", "name='Cam1'")
    record("surveillance", "camera persisted", camid is not None)
    if camid:
        s, b, _ = user.get(f"/surveillance/zone/{zid}/camera/{camid}/edit")
        record("surveillance", "camera edit form", s == 200, f"status={s}")
        s, b, loc = user.post(f"/surveillance/zone/{zid}/camera/{camid}/edit", {"name": "Cam1", "stream_url": "http://127.0.0.1:1/x.mjpeg", "recording_enabled": ""}, follow=False)
        record("surveillance", "camera update", s in (302, 303), f"status={s}")
        s, b, _ = user.get(f"/surveillance/api/camera/{camid}/recordings")
        record("surveillance", "recordings api", s == 200, f"status={s}")
        s, b, loc = user.post(f"/surveillance/zone/{zid}/camera/{camid}/delete", follow=False)
        record("surveillance", "camera delete", s in (302, 303), f"status={s}")
    s, b, loc = user.post(f"/surveillance/zone/{zid}/delete", follow=False)
    record("surveillance", "zone delete", s in (302, 303), f"status={s}")


def test_games_and_misc(base, db, admin, user):
    # hangman
    s, b, _ = user.get("/hangman")
    record("hangman", "page", s == 200, f"status={s}")
    s, b, _ = user.get("/hangman/api/get-word")
    record("hangman", "get-word api", s == 200, f"status={s} {b[:80]}")
    # wordgame
    s, b, _ = user.get("/wordgame")
    record("wordgame", "home page", s == 200, f"status={s}")
    s, b, _ = user.post("/wordgame/api/games", raw=json.dumps({"opponent_username": "t_admin"}), ctype="application/json", follow=False)
    record("wordgame", "create game", s == 200, f"status={s} {b[:120]}")
    gid = None
    try:
        gid = json.loads(b).get("game_id")
    except Exception:
        pass
    record("wordgame", "game id returned", gid is not None, b[:120])
    if gid:
        s, b, _ = admin.post(f"/wordgame/api/games/{gid}/join", follow=False)
        record("wordgame", "join game", s == 200, f"status={s} {b[:120]}")
        s, b, _ = user.get(f"/wordgame/api/games/{gid}")
        record("wordgame", "get game", s == 200, f"status={s}")
        s, b, _ = user.post(f"/wordgame/api/games/{gid}/move", raw=json.dumps({"row": 0, "col": 0}), ctype="application/json", follow=False)
        record("wordgame", "play move (accept 4xx)", s < 500, f"status={s}")
        s, b, _ = user.get(f"/wordgame/game/{gid}")
        record("wordgame", "game page", s == 200, f"status={s}")
    s, b, _ = user.get("/wordgame/api/stats")
    record("wordgame", "stats", s == 200, f"status={s}")
    # library
    s, b, _ = user.get("/library")
    record("library", "landing", s == 200, f"status={s}")
    for p in ("/library/browse?root=music", "/library/browse?root=ebook", "/library/browse?root=movie"):
        s, b, _ = user.get(p)
        record("library", f"browse {p}", s == 200, f"status={s}")
    # meshlink
    s, b, _ = user.get("/meshlink")
    record("meshlink", "page", s == 200, f"status={s}")
    # pecuniary
    s, b, _ = user.get("/pecuniary")
    record("pecuniary", "dashboard", s == 200, f"status={s}")
    # conferencing pages (WS itself not tested)
    s, b, _ = user.get("/conferencing")
    record("conferencing", "dashboard", s == 200, f"status={s}")
    s, b, _ = user.get("/conferencing/history")
    record("conferencing", "history", s == 200, f"status={s}")
    s, b, _ = user.post("/conferencing/api/create-room", {"media_type": "audio"}, follow=False)
    record("conferencing", "create room", s in (200, 409), f"status={s} {b[:120]}")
    rid = None
    try:
        rid = json.loads(b).get("room_id")
    except Exception:
        pass
    if rid:
        s, b, _ = user.get(f"/conferencing/room/{rid}")
        record("conferencing", "room page", s == 200, f"status={s}")
        s, b, _ = user.post(f"/conferencing/api/room/{rid}/invite", raw=json.dumps({"recipient_ids": [db_id(db, "users", "username='t_admin'")]}), ctype="application/json", follow=False)
        record("conferencing", "room invite", s in (200, 404, 409), f"status={s} {b[:120]}")
        s, b, _ = user.post(f"/conferencing/api/room/{rid}/leave", follow=False)
        record("conferencing", "room leave", s in (200, 404, 409), f"status={s} {b[:120]}")
    s, b, _ = user.get("/conferencing/api/recordings")
    record("conferencing", "recordings list", s == 200, f"status={s}")
    # dashboard / about
    s, b, _ = user.get("/")
    record("core", "dashboard", s == 200, f"status={s}")
    s, b, _ = user.get("/about")
    record("core", "about", s == 200, f"status={s}")
    # 404 handling
    s, b, _ = user.get("/nonexistent-page-xyz")
    record("core", "404 handled", s == 404, f"status={s}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:8091")
    ap.add_argument("--db", default="/tmp/syarc-e2e/test.db")
    ap.add_argument("--no-server", action="store_true")
    args = ap.parse_args()

    proc = None
    if not args.no_server:
        os.makedirs("/tmp/syarc-e2e", exist_ok=True)
        for f in os.listdir("/tmp/syarc-e2e"):
            p = os.path.join("/tmp/syarc-e2e", f)
            if os.path.isfile(p):
                os.remove(p)
        env = dict(os.environ)
        env.update({
            "PORT": "8091",
            "DATABASE_URL": f"sqlite://{args.db}?mode=rwc",
            "PECUNIARY_DB": "/tmp/syarc-e2e/pecuniary.db",
        })
        env.pop("PECUNIARY_DB_KEY", None)
        for kind in ("music", "audiobooks", "ebooks", "movies", "photos"):
            d = f"/tmp/syarc-e2e/lib/{kind}"
            os.makedirs(d, exist_ok=True)
            open(f"{d}/sample.dat", "w").write("x")
        for k, d in (("MUSIC", "music"), ("AUDIOBOOK", "audiobooks"), ("EBOOK", "ebooks"), ("MOVIES", "movies"), ("PHOTOS", "photos")):
            env[f"LIBRARY_{k}_ROOT"] = f"/tmp/syarc-e2e/lib/{d}"
        logf = open("/tmp/syarc-e2e/server.log", "w")
        proc = subprocess.Popen(
            ["/usr/local/dev_apps/syarc-console/target/release/syarc-console"],
            cwd="/usr/local/dev_apps/syarc-console", env=env, stdout=logf, stderr=logf,
        )
        for _ in range(50):
            time.sleep(0.2)
            try:
                c = http.client.HTTPConnection("127.0.0.1", 8091, timeout=2)
                c.request("GET", "/login")
                if c.getresponse().status == 200:
                    break
            except OSError:
                pass
        else:
            print("server failed to start; log:")
            print(open("/tmp/syarc-e2e/server.log").read()[-3000:])
            sys.exit(1)

    try:
        admin, user = test_auth(args.base, args.db)
        test_admin(args.base, args.db, admin, user)
        test_supply(args.base, args.db, admin, user)
        test_calendar(args.base, args.db, admin, user)
        test_cooking(args.base, args.db, admin, user)
        test_agriculture(args.base, args.db, admin, user)
        test_surveillance(args.base, args.db, admin, user)
        test_games_and_misc(args.base, args.db, admin, user)
    finally:
        if proc:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(5)
            except subprocess.TimeoutExpired:
                proc.kill()

    fails = [r for r in RESULTS if not r[2]]
    print(f"\n==== {len(RESULTS) - len(fails)}/{len(RESULTS)} passed, {len(fails)} failed ====")
    if fails:
        print("\nFailures:")
        for app, name, _, detail in fails:
            print(f"  {app}: {name}  -- {detail}")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
