-- Dataman's PostgreSQL test data: the database the demo mode, the check stages, the benchmarks
-- and the live tests connect to (connection `local`).
--
--   docker run -d --name dataman-pg -p 55432:5432 -e POSTGRES_PASSWORD=dataman \
--       -e POSTGRES_DB=dataman postgres:18 postgres -c shared_buffers=512MB -c work_mem=64MB
--   PGPASSWORD=dataman psql -h 127.0.0.1 -p 55432 -U postgres -d dataman -f seeds/postgres.sql
--
-- A shop: 100,000 customers, 50,000 products, 1,000,000 orders and about 3,000,000 order items
-- (times are Unix seconds, money is in cents), a reporting schema with a view and a function,
-- and a second database, `dataman_bench`, with 1,000 tables in schema `bench` (the tree
-- benchmark). The data is the same on every run (fixed random seed). Takes about a minute.

\set ON_ERROR_STOP on
SET client_min_messages = warning;

DROP SCHEMA IF EXISTS reporting CASCADE;
DROP TABLE IF EXISTS public.item, public.orders, public.product, public.customer CASCADE;

SELECT setseed(0.42) \gset

CREATE TABLE public.customer (
    id         bigint PRIMARY KEY,
    name       text   NOT NULL,
    email      text   NOT NULL,
    country    text   NOT NULL,
    city       text   NOT NULL,
    created_at bigint NOT NULL
);

INSERT INTO public.customer
SELECT g,
       'Customer ' || g,
       'customer' || g || '@example.com',
       (ARRAY['US','GB','DE','FR','ES','IT','NL','SE','NO','DK','FI','PL','PT','IE','CA','MX','AR','CL','CO','PE',
              'JP','KR','CN','IN','AU','NZ','ZA','EG','NG','KE','TR','GR','CH','AT','BE','CZ','HU','RO','UA','PA'])[1 + floor(random() * 40)::int],
       'City ' || (1 + floor(random() * 500)::int),
       1672531200 + floor(random() * 31536000)::bigint
FROM generate_series(1, 100000) g;

CREATE INDEX customer_country ON public.customer (country);

CREATE TABLE public.product (
    id       bigint  PRIMARY KEY,
    name     text    NOT NULL,
    category integer NOT NULL,
    price    bigint  NOT NULL
);

INSERT INTO public.product
SELECT g, 'Product ' || g, 1 + floor(random() * 100)::int, 500 + floor(random() * 49500)::bigint
FROM generate_series(1, 50000) g;

CREATE TABLE public.orders (
    id         bigint PRIMARY KEY,
    customer   bigint NOT NULL REFERENCES public.customer (id),
    status     text   NOT NULL,
    created_at bigint NOT NULL,
    total      bigint NOT NULL,
    address    jsonb  NOT NULL
);

INSERT INTO public.orders
SELECT g,
       1 + floor(random() * 100000)::bigint,
       CASE WHEN s < 0.35 THEN 'delivered'
            WHEN s < 0.65 THEN 'paid'
            WHEN s < 0.85 THEN 'shipped'
            WHEN s < 0.95 THEN 'pending'
            ELSE 'cancelled' END,
       1704067200 + floor(random() * 63072000)::bigint,
       1000 + floor(random() * 249000)::bigint,
       jsonb_build_object(
           'zip', lpad(floor(random() * 100000000)::bigint::text, 8, '0'),
           'street', (1 + floor(random() * 9999)::int) || ' ' || (ARRAY['Main','Oak','Pine','Maple','Cedar','Elm','Lake','Hill','Park','River'])[1 + floor(random() * 10)::int] || ' Street',
           'city', 'City ' || (1 + floor(random() * 500)::int))
FROM (SELECT g, random() AS s FROM generate_series(1, 1000000) g) o;

CREATE INDEX orders_customer ON public.orders (customer);
CREATE INDEX orders_status_created_at ON public.orders (status, created_at);

CREATE TABLE public.item (
    id       bigint  PRIMARY KEY,
    order_id bigint  NOT NULL REFERENCES public.orders (id),
    product  bigint  NOT NULL REFERENCES public.product (id),
    qty      integer NOT NULL,
    price    bigint  NOT NULL
);

INSERT INTO public.item
SELECT row_number() OVER (ORDER BY o, n), o, p, 1 + floor(random() * 5)::int, 500 + floor(random() * 49500)::bigint
FROM (SELECT o, n, 1 + floor(random() * 50000)::bigint AS p
      FROM generate_series(1, 1000000) o, generate_series(1, 5) n
      WHERE n <= 1 + (o::bigint * 7919) % 5) x;

CREATE INDEX item_order ON public.item (order_id);
CREATE INDEX item_product ON public.item (product);

CREATE SCHEMA reporting;

CREATE VIEW reporting.daily_revenue AS
SELECT to_timestamp(created_at)::date AS day, count(*) AS orders, sum(total) AS revenue
FROM public.orders
WHERE status <> 'cancelled'
GROUP BY 1;

CREATE FUNCTION reporting.customer_order_count(customer_id bigint) RETURNS bigint
LANGUAGE sql STABLE AS $$ SELECT count(*) FROM public.orders WHERE customer = customer_id $$;

VACUUM ANALYZE public.customer, public.product, public.orders, public.item;

-- The tree benchmark's database: 1,000 tables in one schema.
SELECT 'CREATE DATABASE dataman_bench' WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'dataman_bench') \gexec
\connect dataman_bench
SET client_min_messages = warning;
DROP SCHEMA IF EXISTS bench CASCADE;
CREATE SCHEMA bench;
DO $$
BEGIN
    FOR i IN 1..1000 LOOP
        EXECUTE format('CREATE TABLE bench.%I (id bigint PRIMARY KEY, name text, amount numeric, created_at timestamptz, active boolean, data jsonb, ref_id bigint, note text)',
                       'table_' || lpad(i::text, 4, '0'));
    END LOOP;
END $$;
