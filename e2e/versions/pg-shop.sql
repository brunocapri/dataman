-- The shop of seeds/postgres.sql that the older suites' plans read (live_explain: `customer`,
-- `orders`), smaller: the version servers keep their data in memory. Loaded on first start.
SELECT setseed(0.42);

CREATE TABLE public.customer (
    id         bigint PRIMARY KEY,
    name       text   NOT NULL,
    email      text   NOT NULL,
    country    text   NOT NULL
);
INSERT INTO public.customer
SELECT g, 'Customer ' || g, 'customer' || g || '@example.com', (ARRAY['US','GB','DE','FR','JP','BR'])[1 + floor(random() * 6)::int]
FROM generate_series(1, 100000) g;

CREATE TABLE public.orders (
    id         bigint PRIMARY KEY,
    customer   bigint NOT NULL REFERENCES public.customer (id),
    status     text   NOT NULL,
    created_at bigint NOT NULL,
    total      bigint NOT NULL
);
INSERT INTO public.orders
SELECT g, 1 + floor(random() * 100000)::bigint, (ARRAY['delivered','paid','shipped','pending','cancelled'])[1 + floor(random() * 5)::int],
       1704067200 + floor(random() * 63072000)::bigint, 1000 + floor(random() * 249000)::bigint
FROM generate_series(1, 200000) g;
CREATE INDEX orders_customer ON public.orders (customer);
ANALYZE;
