-- The demo stages of a table's data view (`table`, `record`, `json`, `datetime`, `enum`) run
-- this before they start (the database of DATAMAN_BENCH_PG): one table with an enum, a boolean,
-- a reference, a timestamptz, a date, a jsonb and long text. Each run starts from scratch.
CREATE SCHEMA IF NOT EXISTS dataman_demo;
DROP TABLE IF EXISTS dataman_demo.tickets;
DROP TYPE IF EXISTS dataman_demo.ticket_state;
CREATE TYPE dataman_demo.ticket_state AS ENUM ('open', 'triage', 'waiting', 'closed');
CREATE TABLE dataman_demo.tickets (
    id        bigint PRIMARY KEY,
    title     text NOT NULL,
    state     dataman_demo.ticket_state NOT NULL DEFAULT 'open',
    urgent    boolean,
    customer  bigint REFERENCES public.customer (id),
    opened_at timestamptz NOT NULL,
    due       date,
    meta      jsonb,
    notes     text
);
INSERT INTO dataman_demo.tickets
SELECT i,
       (ARRAY['Refund not received', 'Address change', 'Wrong item shipped', 'Invoice copy', 'Late delivery', 'Damaged box'])[1 + i % 6] || ' #' || (4000 + i),
       (ARRAY['open', 'triage', 'waiting', 'closed'])[1 + (i * 7) % 4]::dataman_demo.ticket_state,
       CASE WHEN i % 5 = 0 THEN NULL ELSE i % 3 = 0 END,
       1 + (i * 37) % 1000,
       timestamptz '2026-09-01 09:00:00+00' + i * interval '7 hours 13 minutes',
       CASE WHEN i % 4 = 0 THEN NULL ELSE date '2026-10-01' + i % 30 END,
       jsonb_build_object(
           'channel', (ARRAY['email', 'chat', 'phone'])[1 + i % 3],
           'priority', 1 + i % 4,
           'tags', jsonb_build_array('order', CASE WHEN i % 2 = 0 THEN 'refund' ELSE 'shipping' END),
           'agent', jsonb_build_object('id', 100 + i % 9, 'name', (ARRAY['Alice', 'Ben', 'Chloe'])[1 + i % 3])),
       CASE WHEN i % 3 = 0 THEN NULL ELSE 'Customer called twice.' || E'\n' || 'Waiting on the carrier.' END
FROM generate_series(1, 240) i;
ANALYZE dataman_demo.tickets;
