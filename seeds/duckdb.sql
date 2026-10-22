-- A small shop in DuckDB: what the DuckDB tests and the duckdb-check stage run against.
CREATE SEQUENCE customer_id START 1;
CREATE TYPE tier AS ENUM ('bronze', 'silver', 'gold');
CREATE TABLE customer (
    id         INTEGER PRIMARY KEY DEFAULT nextval('customer_id'),
    name       VARCHAR NOT NULL,
    email      VARCHAR UNIQUE,
    tier       tier NOT NULL DEFAULT 'bronze',
    balance    DECIMAL(12, 2) DEFAULT 0,
    active     BOOLEAN NOT NULL DEFAULT true,
    tags       VARCHAR[],
    profile    STRUCT(city VARCHAR, zip INTEGER),
    photo      BLOB,
    created_at TIMESTAMP DEFAULT TIMESTAMP '2024-01-02 03:04:05'
);
CREATE TABLE orders (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customer (id),
    status      VARCHAR NOT NULL DEFAULT 'new',
    total       DOUBLE NOT NULL DEFAULT 0,
    notes       VARCHAR
);
CREATE INDEX orders_status_idx ON orders (status, customer_id);
CREATE TABLE note (body VARCHAR);
CREATE SCHEMA sales;
CREATE TABLE sales.region (code VARCHAR PRIMARY KEY, name VARCHAR);
CREATE VIEW v_order_customer AS SELECT o.id, o.status, o.total, c.name FROM orders o JOIN customer c ON c.id = o.customer_id;
INSERT INTO customer (name, email, tier, balance, active, tags, profile, photo) VALUES
    ('Ana Lima', 'ana@example.com', 'gold', 1520.50, true, ['vip', 'early'], {'city': 'Lisbon', 'zip': 1000}, '\xCA\xFE'::BLOB),
    ('Bruno Costa', 'bruno@example.com', 'silver', 12.00, true, [], {'city': 'Denver', 'zip': 80202}, NULL),
    ('Diego O''Neil', NULL, 'bronze', 0, false, NULL, NULL, NULL);
INSERT INTO orders VALUES (1, 1, 'paid', 99.9, NULL), (2, 1, 'new', 10, NULL), (3, 2, 'paid', 0.1, NULL);
INSERT INTO note VALUES ('keyless'), ('keyless');
INSERT INTO sales.region VALUES ('eu', 'Europe');
