#!/usr/bin/env python3
"""Generate the DataBrain sample "shop" dataset (deterministic, stdlib only).

Writes, next to this script:
  customers.csv, products.csv          plain CSV (header row, RFC 4180 quoting)
  shop.db                              SQLite: same data + orders/order_items, FKs, a view
  _staging/orders.csv, order_items.csv converted to Parquet by generate.sh

The same rows go to every format, so results can be cross-checked
(e.g. revenue from SQLite == revenue from CSV + Parquet in DuckDB).
"""

import csv
import datetime as dt
import os
import random
import sqlite3

HERE = os.path.dirname(os.path.abspath(__file__))
rng = random.Random(42)

FIRST = ["Anna", "Bao", "Chloé", "David", "Emma", "Farid", "Giulia", "Hiro", "Ines", "Jonas",
         "Khanh", "Lena", "Minh", "Noah", "Olivia", "Priya", "Quang", "Rosa", "Sven", "Trang"]
LAST = ["Nguyễn", "Trần", "Smith", "O'Brien", "García", "Müller", "Rossi", "Tanaka", "Dubois", "Kowalski",
        "Lê", "Phạm", "Johnson", "Silva", "Andersson"]
PLACES = [("Vietnam", "Hà Nội"), ("Vietnam", "Hồ Chí Minh"), ("Vietnam", "Đà Nẵng"), ("USA", "New York"),
          ("USA", "San Francisco"), ("Germany", "Berlin"), ("France", "Paris"), ("Japan", "Tokyo"),
          ("UK", "London"), ("Brazil", "São Paulo")]
CATEGORIES = {
    "Laptops": ["Air 13", "Pro 14", "Pro 16", "Ultra 15"],
    "Phones": ["Mini", "Standard", "Max", "Fold"],
    "Audio": ["Earbuds", "Headphones", "Speaker, portable", "Soundbar"],
    "Accessories": ['Cable 2m', 'Charger 65W', 'Case "Slim"', 'Stand', 'Mouse', 'Keyboard'],
}
SINGULAR = {"Laptops": "Laptop", "Phones": "Phone", "Audio": "Audio", "Accessories": "Accessory"}
STATUSES = ["delivered"] * 7 + ["shipped"] * 2 + ["cancelled", "pending"]

N_CUSTOMERS = 250
N_ORDERS = 3000


def customers():
    out = []
    start = dt.date(2022, 1, 1)
    for i in range(1, N_CUSTOMERS + 1):
        first, last = rng.choice(FIRST), rng.choice(LAST)
        country, city = rng.choice(PLACES)
        name = f"{first} {last}"
        if i % 50 == 0:
            name = f"{last}, {first}"  # comma inside a quoted CSV field
        email = None if i % 17 == 0 else f"{first}.{i}@example.com".lower()  # some NULLs
        out.append({
            "customer_id": i,
            "name": name,
            "email": email,
            "country": country,
            "city": city,
            "signup_date": (start + dt.timedelta(days=rng.randrange(0, 1000))).isoformat(),
            "is_vip": rng.random() < 0.12,
        })
    return out


def products():
    out = []
    pid = 1
    for cat, names in CATEGORIES.items():
        base = {"Laptops": 1100, "Phones": 700, "Audio": 150, "Accessories": 25}[cat]
        for n in names:
            out.append({
                "product_id": pid,
                "name": f"{SINGULAR[cat]} {n}",
                "category": cat,
                "unit_price": round(base * rng.uniform(0.6, 1.8), 2),
            })
            pid += 1
    return out


def orders(custs, prods):
    ords, items = [], []
    start = dt.datetime(2023, 1, 1, 8, 0, 0)
    item_id = 1
    for oid in range(1, N_ORDERS + 1):
        c = rng.choice(custs)
        ts = start + dt.timedelta(minutes=rng.randrange(0, 3 * 365 * 24 * 60))
        status = rng.choice(STATUSES)
        total = 0.0
        for p in rng.sample(prods, rng.randint(1, 4)):
            qty = rng.randint(1, 3) if p["category"] != "Accessories" else rng.randint(1, 6)
            discount = rng.choice([0, 0, 0, 0.05, 0.1, 0.2])
            amount = round(qty * p["unit_price"] * (1 - discount), 2)
            items.append({
                "order_item_id": item_id,
                "order_id": oid,
                "product_id": p["product_id"],
                "quantity": qty,
                "unit_price": p["unit_price"],
                "discount": discount,
                "amount": amount,
            })
            item_id += 1
            total += amount
        ords.append({
            "order_id": oid,
            "customer_id": c["customer_id"],
            "ordered_at": ts.isoformat(sep=" "),
            "status": status,
            "total_amount": round(total, 2),
        })
    return ords, items


def write_csv(path, rows):
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        w.writeheader()
        for r in rows:
            w.writerow({k: ("" if v is None else ("true" if v is True else "false" if v is False else v)) for k, v in r.items()})


def write_sqlite(path, custs, prods, ords, items):
    if os.path.exists(path):
        os.remove(path)
    db = sqlite3.connect(path)
    db.executescript("""
        PRAGMA foreign_keys = ON;
        CREATE TABLE customers (
            customer_id INTEGER PRIMARY KEY,
            name        TEXT NOT NULL,
            email       TEXT,
            country     TEXT NOT NULL,
            city        TEXT NOT NULL,
            signup_date DATE NOT NULL,
            is_vip      BOOLEAN NOT NULL DEFAULT 0
        );
        CREATE TABLE products (
            product_id INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            category   TEXT NOT NULL,
            unit_price REAL NOT NULL
        );
        CREATE TABLE orders (
            order_id     INTEGER PRIMARY KEY,
            customer_id  INTEGER NOT NULL REFERENCES customers(customer_id),
            ordered_at   DATETIME NOT NULL,
            status       TEXT NOT NULL CHECK (status IN ('pending','shipped','delivered','cancelled')),
            total_amount REAL NOT NULL
        );
        CREATE TABLE order_items (
            order_item_id INTEGER PRIMARY KEY,
            order_id      INTEGER NOT NULL REFERENCES orders(order_id),
            product_id    INTEGER NOT NULL REFERENCES products(product_id),
            quantity      INTEGER NOT NULL,
            unit_price    REAL NOT NULL,
            discount      REAL NOT NULL,
            amount        REAL NOT NULL
        );
        CREATE INDEX orders_customer ON orders(customer_id);
        CREATE INDEX items_order ON order_items(order_id);
        CREATE VIEW revenue_by_country AS
            SELECT c.country, COUNT(DISTINCT o.order_id) AS orders, ROUND(SUM(o.total_amount), 2) AS revenue
            FROM orders o JOIN customers c USING (customer_id)
            WHERE o.status <> 'cancelled'
            GROUP BY c.country;
    """)
    ins = lambda t, rows: db.executemany(
        f"INSERT INTO {t} ({', '.join(rows[0])}) VALUES ({', '.join('?' * len(rows[0]))})",
        [tuple(r.values()) for r in rows])
    ins("customers", custs)
    ins("products", prods)
    ins("orders", ords)
    ins("order_items", items)
    db.commit()
    db.execute("VACUUM")
    db.close()


def main():
    custs, prods = customers(), products()
    ords, items = orders(custs, prods)
    write_csv(os.path.join(HERE, "customers.csv"), custs)
    write_csv(os.path.join(HERE, "products.csv"), prods)
    staging = os.path.join(HERE, "_staging")
    os.makedirs(staging, exist_ok=True)
    write_csv(os.path.join(staging, "orders.csv"), ords)
    write_csv(os.path.join(staging, "order_items.csv"), items)
    write_sqlite(os.path.join(HERE, "shop.db"), custs, prods, ords, items)
    print(f"customers={len(custs)} products={len(prods)} orders={len(ords)} order_items={len(items)}")


if __name__ == "__main__":
    main()
