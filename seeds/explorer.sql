-- The explorer's demo stages (`tree-objects`, `tree-table`, `tree-filter`, `privileges`) run
-- this first, in the database of DATAMAN_BENCH_PG: one schema with every kind the explorer lists
-- (tables with rows, a partitioned table, a view, a materialized view, functions, a procedure,
-- sequences, an enum, a composite, a domain, triggers, constraints) and grants to two roles.
-- Each run starts from scratch.
DROP SCHEMA IF EXISTS dataman_shop CASCADE;
CREATE SCHEMA dataman_shop;
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dataman_reader') THEN
        CREATE ROLE dataman_reader NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dataman_app') THEN
        CREATE ROLE dataman_app LOGIN CONNECTION LIMIT 20;
    END IF;
END
$$;
CREATE TYPE dataman_shop.order_status AS ENUM ('pending', 'paid', 'shipped', 'delivered', 'refunded');
CREATE TYPE dataman_shop.address AS (street text, city text, zip text);
CREATE DOMAIN dataman_shop.amount AS numeric(12,2) CHECK (VALUE >= 0);
CREATE TABLE dataman_shop.customers (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email      text NOT NULL UNIQUE,
    name       text NOT NULL,
    ship_to    dataman_shop.address,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE dataman_shop.products (
    id    serial PRIMARY KEY,
    sku   text NOT NULL UNIQUE,
    title text NOT NULL,
    price dataman_shop.amount NOT NULL,
    stock int NOT NULL DEFAULT 0 CONSTRAINT stock_not_negative CHECK (stock >= 0)
);
CREATE TABLE dataman_shop.orders (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    customer   bigint NOT NULL REFERENCES dataman_shop.customers (id),
    status     dataman_shop.order_status NOT NULL DEFAULT 'pending',
    total      dataman_shop.amount NOT NULL,
    placed_at  timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz,
    CONSTRAINT shipped_after_paid CHECK (status <> 'shipped' OR total > 0)
);
CREATE TABLE dataman_shop.order_items (
    order_id bigint REFERENCES dataman_shop.orders (id) ON DELETE CASCADE,
    product  int REFERENCES dataman_shop.products (id),
    qty      int NOT NULL CHECK (qty > 0),
    PRIMARY KEY (order_id, product)
);
CREATE TABLE dataman_shop.events (at date NOT NULL, kind text, payload jsonb) PARTITION BY RANGE (at);
CREATE TABLE dataman_shop.events_2026_09 PARTITION OF dataman_shop.events FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE dataman_shop.events_2026_10 PARTITION OF dataman_shop.events FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
CREATE SEQUENCE dataman_shop.invoice_no START 10000;
CREATE VIEW dataman_shop.open_orders AS SELECT * FROM dataman_shop.orders WHERE status IN ('pending', 'paid');
CREATE FUNCTION dataman_shop.order_total(o bigint) RETURNS numeric LANGUAGE sql STABLE AS $$
    SELECT coalesce(sum(i.qty * p.price), 0)
      FROM dataman_shop.order_items i JOIN dataman_shop.products p ON p.id = i.product
     WHERE i.order_id = o
$$;
CREATE FUNCTION dataman_shop.top_customers(n int) RETURNS SETOF dataman_shop.customers LANGUAGE sql STABLE AS $$
    SELECT c.* FROM dataman_shop.customers c JOIN dataman_shop.orders o ON o.customer = c.id
     GROUP BY c.id ORDER BY sum(o.total) DESC LIMIT n
$$;
CREATE FUNCTION dataman_shop.touch_updated() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END
$$;
CREATE TRIGGER orders_touch BEFORE UPDATE ON dataman_shop.orders FOR EACH ROW EXECUTE FUNCTION dataman_shop.touch_updated();
CREATE TRIGGER orders_audit AFTER INSERT OR DELETE ON dataman_shop.orders FOR EACH ROW EXECUTE FUNCTION dataman_shop.touch_updated();
ALTER TABLE dataman_shop.orders DISABLE TRIGGER orders_audit;
INSERT INTO dataman_shop.customers (email, name, ship_to)
SELECT 'customer' || i || '@example.com', 'Customer ' || i, ROW(i || ' Main St', 'Lisbon', lpad((1000 + i % 8000)::text, 4, '0'))::dataman_shop.address
  FROM generate_series(1, 2400) i;
INSERT INTO dataman_shop.products (sku, title, price, stock)
SELECT 'SKU-' || lpad(i::text, 4, '0'), 'Product ' || i, (5 + (i * 37) % 200)::numeric(12,2), (i * 13) % 90
  FROM generate_series(1, 180) i;
INSERT INTO dataman_shop.orders (customer, status, total, placed_at)
SELECT 1 + (i * 7) % 2400,
       (ARRAY['pending', 'paid', 'shipped', 'delivered', 'refunded'])[1 + i % 5]::dataman_shop.order_status,
       (10 + (i * 31) % 900)::numeric(12,2),
       timestamptz '2026-09-01 00:00:00+00' + i * interval '83 seconds'
  FROM generate_series(1, 48000) i;
INSERT INTO dataman_shop.order_items (order_id, product, qty)
SELECT o, 1 + (o + k * 37) % 180, 1 + (o + k) % 4 FROM generate_series(1, 48000) o, generate_series(1, 2) k;
INSERT INTO dataman_shop.events (at, kind, payload)
SELECT date '2026-09-01' + i % 61, (ARRAY['view', 'cart', 'checkout'])[1 + i % 3], jsonb_build_object('n', i)
  FROM generate_series(1, 30000) i;
CREATE MATERIALIZED VIEW dataman_shop.daily_sales AS
SELECT placed_at::date AS day, count(*) AS orders, sum(total) AS revenue FROM dataman_shop.orders GROUP BY 1;
CREATE PROCEDURE dataman_shop.refresh_sales() LANGUAGE sql AS $$ REFRESH MATERIALIZED VIEW dataman_shop.daily_sales $$;
ANALYZE dataman_shop.customers;
ANALYZE dataman_shop.products;
ANALYZE dataman_shop.orders;
ANALYZE dataman_shop.order_items;
ANALYZE dataman_shop.events;
ANALYZE dataman_shop.daily_sales;
GRANT USAGE ON SCHEMA dataman_shop TO dataman_reader, dataman_app;
GRANT SELECT ON ALL TABLES IN SCHEMA dataman_shop TO dataman_reader;
GRANT SELECT, INSERT, UPDATE ON dataman_shop.orders TO dataman_app;
GRANT DELETE ON dataman_shop.orders TO dataman_app WITH GRANT OPTION;
GRANT SELECT ON dataman_shop.orders TO PUBLIC;
