CREATE TABLE products (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    sku TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    price_cents BIGINT NOT NULL CHECK (price_cents >= 0),
    -- Never negative: the order code checks, and the database enforces.
    stock INTEGER NOT NULL CHECK (stock >= 0)
);

CREATE TABLE orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    customer TEXT NOT NULL,
    total_cents BIGINT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- The price is copied at order time, so later price changes don't rewrite
-- history.
CREATE TABLE order_lines (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    order_id INTEGER NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    product_id INTEGER NOT NULL REFERENCES products (id),
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    unit_price_cents BIGINT NOT NULL
);

CREATE INDEX order_lines_order ON order_lines (order_id);
