#!/usr/bin/env python3
"""Migrate pecuniary data from fastmain Postgres into the syarc-console SQLite DB.

Amount columns are pgcrypto envelope-encrypted in Postgres (master key from
fastmain's .env protects a per-resource DEK in portfolio_keys). This script
decrypts them via pgp_sym_decrypt and migrates real values; rows whose DEK
cannot be found migrate as 0.0.

Usage (fastmain venv has asyncpg):
    /usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_pecuniary.py
"""
import asyncio
import os
import sqlite3

import asyncpg
from dotenv import dotenv_values

MK = dotenv_values("/usr/local/dev_apps/fastmain/.env")["PECUNIARY_MASTER_KEY"]

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


class Decryptor:
    def __init__(self, pg):
        self.pg = pg
        self.deks = {}

    async def dek(self, resource_id):
        rid = str(resource_id)
        if rid not in self.deks:
            enc = await self.pg.fetchval(
                "SELECT encrypted_dek FROM portfolio_keys WHERE resource_id = $1", rid
            )
            if enc is None:
                self.deks[rid] = None
            else:
                try:
                    self.deks[rid] = await self.pg.fetchval(
                        "SELECT pgp_sym_decrypt(decode($1,'hex'),$2)", enc, MK
                    )
                except Exception:
                    self.deks[rid] = None
        return self.deks[rid]

    async def val(self, resource_id, enc):
        if not enc:
            return 0.0
        dek = await self.dek(resource_id)
        if dek is None:
            return 0.0
        try:
            return float(await self.pg.fetchval(
                "SELECT pgp_sym_decrypt(decode($1,'hex'),$2)", enc, dek
            ))
        except Exception:
            return 0.0


async def main():
    pg = await asyncpg.connect(PG_URL)
    dec = Decryptor(pg)

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
        "stock_transactions", "stock_shares", "stock_portfolios",
        "pms_transactions", "pms_shares", "pms_portfolios", "pms_current_values",
        "ledger_entries", "ledger_shares", "ledger_books",
        "checkbook", "checkbook_shares", "checkbook_registers",
    ]:
        lite.execute(f"DELETE FROM {t}")

    # checkbook
    reg_map = {}
    for r in await pg.fetch("SELECT * FROM checkbook_registers ORDER BY created_at"):
        uid = map_user(r["user_id"])
        if uid is None:
            continue
        cur = lite.execute(
            "INSERT INTO checkbook_registers (user_id, name, bank_name, account_number, account_type, description, is_active, created_at) VALUES (?,?,?,?,?,?,?,?)",
            (uid, r["name"], r["bank_name"], r["account_number"], r["account_type"], r["description"], 1 if r["is_active"] else 0, dt(r["created_at"])),
        )
        reg_map[str(r["id"])] = cur.lastrowid
    print(f"registers: {len(reg_map)}")

    for r in await pg.fetch("SELECT * FROM checkbook_shares ORDER BY created_at"):
        rid = reg_map.get(str(r["register_id"]))
        uid = map_user(r["shared_with_user_id"])
        if rid is None or uid is None:
            continue
        lite.execute(
            "INSERT INTO checkbook_shares (register_id, shared_with_user_id, created_at) VALUES (?,?,?)",
            (rid, uid, dt(r["created_at"])),
        )

    n = 0
    for r in await pg.fetch("SELECT * FROM checkbook ORDER BY id"):
        rid = reg_map.get(str(r["register_id"]))
        uid = map_user(r["user_id"]) if r["user_id"] else None
        if rid is None:
            continue
        pay = await dec.val(r["register_id"], r["payment_encrypted"])
        dep = await dec.val(r["register_id"], r["deposit_encrypted"])
        bal = await dec.val(r["register_id"], r["balance_encrypted"])
        lite.execute(
            "INSERT INTO checkbook (register_id, user_id, check_number, date, type, payee_description, payment, deposit, balance, returned, created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            (rid, uid, r["check_number"], d(r["date"]), r["type"], r["payee_description"], pay, dep, bal, 1 if r["returned"] else 0, dt(r["created_at"])),
        )
        n += 1
    print(f"checkbook entries: {n}")

    # ledger
    book_map = {}
    for r in await pg.fetch("SELECT * FROM ledger_books ORDER BY created_at"):
        uid = map_user(r["user_id"])
        if uid is None:
            continue
        cur = lite.execute(
            "INSERT INTO ledger_books (user_id, name, description, is_active, created_at) VALUES (?,?,?,?,?)",
            (uid, r["name"], r["description"], 1 if r["is_active"] else 0, dt(r["created_at"])),
        )
        book_map[str(r["id"])] = cur.lastrowid
    print(f"books: {len(book_map)}")

    for r in await pg.fetch("SELECT * FROM ledger_shares ORDER BY created_at"):
        bid = book_map.get(str(r["book_id"]))
        uid = map_user(r["shared_with_user_id"])
        if bid is None or uid is None:
            continue
        lite.execute(
            "INSERT INTO ledger_shares (book_id, shared_with_user_id, created_at) VALUES (?,?,?)",
            (bid, uid, dt(r["created_at"])),
        )

    n = 0
    for r in await pg.fetch("SELECT * FROM ledger_entries ORDER BY id"):
        bid = book_map.get(str(r["book_id"]))
        uid = map_user(r["user_id"])
        if bid is None or uid is None:
            continue
        amount = await dec.val(r["book_id"], r["amount_encrypted"])
        lite.execute(
            "INSERT INTO ledger_entries (book_id, user_id, category, entry_type, description, amount, entry_date, recurrence, source, reference_number, notes, is_reconciled, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (bid, uid, r["category"], r["entry_type"], r["description"], amount, d(r["entry_date"]), r["recurrence"], r["source"], r["reference_number"], r["notes"], 1 if r["is_reconciled"] else 0, dt(r["created_at"]), dt(r["updated_at"])),
        )
        n += 1
    print(f"ledger entries: {n}")

    # pms
    pf_map = {}
    for r in await pg.fetch("SELECT * FROM pms_portfolios ORDER BY created_at"):
        uid = map_user(r["user_id"])
        if uid is None:
            continue
        cur = lite.execute(
            "INSERT INTO pms_portfolios (user_id, name, description, is_active, created_at) VALUES (?,?,?,?,?)",
            (uid, r["name"], r["description"], 1 if r["is_active"] else 0, dt(r["created_at"])),
        )
        pf_map[str(r["id"])] = cur.lastrowid
    print(f"pms portfolios: {len(pf_map)}")

    for r in await pg.fetch("SELECT * FROM pms_shares ORDER BY created_at"):
        pid = pf_map.get(str(r["portfolio_id"]))
        uid = map_user(r["shared_with_user_id"])
        if pid is None or uid is None:
            continue
        lite.execute(
            "INSERT INTO pms_shares (portfolio_id, shared_with_user_id, created_at) VALUES (?,?,?)",
            (pid, uid, dt(r["created_at"])),
        )

    n = 0
    for r in await pg.fetch("SELECT * FROM pms_transactions ORDER BY id"):
        pid = pf_map.get(str(r["portfolio_id"]))
        uid = map_user(r["created_by"])
        if pid is None or uid is None:
            continue
        cost = await dec.val(r["portfolio_id"], r["cost_encrypted"])
        ounces = await dec.val(r["portfolio_id"], r["ounces_encrypted"])
        lite.execute(
            "INSERT INTO pms_transactions (portfolio_id, metal, date_purchased, date_received, vendor, units, description, mint, cost, ounces, disposition, created_at, created_by) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (pid, r["metal"], d(r["date_purchased"]), d(r["date_received"]), r["vendor"], float(r["units"]), r["description"], r["mint"], cost, ounces, r["disposition"], dt(r["created_at"]), uid),
        )
        n += 1
    print(f"pms transactions: {n}")

    for r in await pg.fetch("SELECT * FROM pms_current_values"):
        lite.execute(
            "INSERT OR REPLACE INTO pms_current_values (metal, price, updated_at) VALUES (?,?,?)",
            (r["metal"], float(r["price"]) if r["price"] is not None else None, dt(r["updated_at"])),
        )

    # stocks
    sp_map = {}
    for r in await pg.fetch("SELECT * FROM stock_portfolios ORDER BY created_at"):
        uid = map_user(r["user_id"])
        if uid is None:
            continue
        cur = lite.execute(
            "INSERT INTO stock_portfolios (user_id, name, broker, is_active, created_at) VALUES (?,?,?,?,?)",
            (uid, r["name"], r["broker"], 1 if r["is_active"] else 0, dt(r["created_at"])),
        )
        sp_map[str(r["id"])] = cur.lastrowid
    print(f"stock portfolios: {len(sp_map)}")

    for r in await pg.fetch("SELECT * FROM stock_shares ORDER BY created_at"):
        pid = sp_map.get(str(r["portfolio_id"]))
        uid = map_user(r["shared_with_user_id"])
        if pid is None or uid is None:
            continue
        lite.execute(
            "INSERT INTO stock_shares (portfolio_id, shared_with_user_id, created_at) VALUES (?,?,?)",
            (pid, uid, dt(r["created_at"])),
        )

    n = 0
    for r in await pg.fetch("SELECT * FROM stock_transactions ORDER BY id"):
        pid = sp_map.get(str(r["portfolio_id"]))
        uid = map_user(r["user_id"])
        if pid is None or uid is None:
            continue
        shares = await dec.val(r["portfolio_id"], r["shares_encrypted"])
        pps = await dec.val(r["portfolio_id"], r["price_per_share_encrypted"])
        tot = await dec.val(r["portfolio_id"], r["total_value_encrypted"])
        lite.execute(
            "INSERT INTO stock_transactions (user_id, portfolio_id, symbol, broker, transaction_date, type, shares, price_per_share, total_value) VALUES (?,?,?,?,?,?,?,?,?)",
            (uid, pid, r["symbol"], r["broker"], d(r["transaction_date"]), r["type"], shares, pps, tot),
        )
        n += 1
    print(f"stock transactions: {n}")

    lite.commit()
    lite.close()
    await pg.close()
    print("pecuniary migration complete")


asyncio.run(main())
