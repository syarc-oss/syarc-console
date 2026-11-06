#!/usr/bin/env python3
"""Copy cooking data (recipes, ingredients, menu entries) from the fastmain
Postgres DB into the console SQLite DB, remapping user UUIDs to console ids
by username. Run with the fastmain venv's Python AFTER the console has applied
migration 0013:

    /usr/local/dev_apps/fastmain/.venv/bin/python tools/migrate_cooking.py
"""

import asyncio
import os
import sqlite3
import sys

import asyncpg

PG_URL = dict(
    host="localhost",
    user="fastapi_dev",
    password="S1lv3r",
    database="fastapi_dev",
)
SQLITE_PATH = os.path.join(os.path.dirname(__file__), "..", "syarc-console.db")


async def main():
    pg = await asyncpg.connect(**PG_URL)
    con = sqlite3.connect(SQLITE_PATH)
    con.execute("PRAGMA foreign_keys = OFF")

    username_to_id = {
        u: i for u, i in con.execute("SELECT username, id FROM users")
    }
    pg_users = await pg.fetch("SELECT id, username FROM users")
    uuid_to_id = {
        str(row["id"]): username_to_id.get(row["username"]) for row in pg_users
    }
    def map_user(uuid_str):
        if uuid_str is None:
            return None
        return uuid_to_id.get(str(uuid_str))

    recipes = await pg.fetch(
        "SELECT id, title, description, steps, source_uid, source_link, is_public, user_id, created_at FROM cooking_recipes"
    )
    ingredients = await pg.fetch(
        "SELECT id, name, normalized_name FROM cooking_ingredients"
    )
    recipe_ingredients = await pg.fetch(
        "SELECT id, recipe_id, ingredient_id, raw_text FROM cooking_recipe_ingredients"
    )
    menu = await pg.fetch(
        "SELECT id, recipe_id, menu_date, meal_type, user_id, created_at FROM cooking_menu_entries"
    )
    print(f"pg rows: {len(recipes)} recipes, {len(ingredients)} ingredients, "
          f"{len(recipe_ingredients)} recipe_ingredients, {len(menu)} menu entries")

    con.execute("DELETE FROM cooking_menu_entries")
    con.execute("DELETE FROM cooking_recipe_ingredients")
    con.execute("DELETE FROM cooking_recipes")
    con.execute("DELETE FROM cooking_ingredients")

    con.executemany(
        "INSERT OR IGNORE INTO cooking_ingredients (id, name, normalized_name) VALUES (?, ?, ?)",
        [(str(r["id"]), r["name"], r["normalized_name"]) for r in ingredients],
    )
    con.executemany(
        "INSERT INTO cooking_recipes (id, title, description, steps, source_uid, source_link, is_public, user_id, created_at) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        [
            (
                str(r["id"]), r["title"], r["description"], r["steps"],
                r["source_uid"], r["source_link"], 1 if r["is_public"] else 0,
                map_user(r["user_id"]),
                r["created_at"].strftime("%Y-%m-%d %H:%M:%S") if r["created_at"] else None,
            )
            for r in recipes
        ],
    )
    con.executemany(
        "INSERT INTO cooking_recipe_ingredients (id, recipe_id, ingredient_id, raw_text) VALUES (?, ?, ?, ?)",
        [
            (str(r["id"]), str(r["recipe_id"]), str(r["ingredient_id"]), r["raw_text"])
            for r in recipe_ingredients
        ],
    )
    con.executemany(
        "INSERT INTO cooking_menu_entries (id, recipe_id, menu_date, meal_type, user_id, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        [
            (
                str(r["id"]), str(r["recipe_id"]),
                r["menu_date"].strftime("%Y-%m-%d") if r["menu_date"] else None,
                r["meal_type"], map_user(r["user_id"]),
                r["created_at"].strftime("%Y-%m-%d %H:%M:%S") if r["created_at"] else None,
            )
            for r in menu
        ],
    )
    con.commit()

    counts = {
        t: con.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0]
        for t in ("cooking_recipes", "cooking_ingredients",
                  "cooking_recipe_ingredients", "cooking_menu_entries")
    }
    print("sqlite counts:", counts)
    con.close()
    await pg.close()


if __name__ == "__main__":
    asyncio.run(main())
