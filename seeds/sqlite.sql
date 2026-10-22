-- A small shop in SQLite: what the SQLite tests and the sqlite-check stage run against.
CREATE TABLE customer (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    email      TEXT UNIQUE,
    tier       TEXT NOT NULL DEFAULT 'bronze' CHECK (tier IN ('bronze', 'silver', 'gold')),
    balance    NUMERIC(12, 2) DEFAULT 0,
    active     BOOLEAN NOT NULL DEFAULT 1,
    profile    JSON,
    photo      BLOB,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    label      TEXT GENERATED ALWAYS AS (name || ' <' || coalesce(email, '?') || '>') VIRTUAL
);

CREATE TABLE orders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    customer_id INTEGER NOT NULL REFERENCES customer (id) ON DELETE CASCADE,
    status      TEXT NOT NULL DEFAULT 'new',
    total       REAL NOT NULL DEFAULT 0,
    notes       TEXT
);
CREATE INDEX orders_status_idx ON orders (status, customer_id);

CREATE TABLE item (
    order_id INTEGER NOT NULL REFERENCES orders,
    line     INTEGER NOT NULL,
    product  TEXT NOT NULL,
    qty      INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (order_id, line)
) WITHOUT ROWID;

CREATE TABLE note (body TEXT);

CREATE VIEW v_order_customer AS
    SELECT o.id, o.status, o.total, c.name FROM orders o JOIN customer c ON c.id = o.customer_id;

CREATE TRIGGER orders_touch AFTER UPDATE OF status ON orders BEGIN
    UPDATE orders SET notes = coalesce(notes, '') || '*' WHERE id = NEW.id;
END;

INSERT INTO customer (id, name, email, tier, balance, active, profile, photo) VALUES
    (1, 'Ana Lima', 'ana@example.com', 'gold', 1520.50, 1, '{"city": "Lisbon"}', X'CAFE'),
    (2, 'Bruno Costa', 'bruno@example.com', 'silver', 12.00, 1, '{"city": "Denver"}', NULL),
    (3, 'Diego O''Neil', NULL, 'bronze', 0, 0, NULL, NULL);
INSERT INTO orders (customer_id, status, total) VALUES (1, 'paid', 99.9), (1, 'new', 10), (2, 'paid', 0.1);
INSERT INTO item (order_id, line, product, qty) VALUES (1, 1, 'pen', 2), (1, 2, 'ink', 1), (3, 1, 'paper', 5);
INSERT INTO note (body) VALUES ('keyless'), ('keyless');
