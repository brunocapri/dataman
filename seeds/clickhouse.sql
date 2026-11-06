-- ClickHouse fixtures for the live tests and the e2e runner. Rerunnable: it starts over.
DROP DATABASE IF EXISTS dataman_ch;
CREATE DATABASE dataman_ch;

CREATE TABLE dataman_ch.events (
    id UInt64,
    user_id Nullable(Int32),
    kind LowCardinality(String),
    amount Decimal(18, 2),
    ratio Float64,
    happened DateTime('UTC'),
    happened_ms DateTime64(3, 'UTC'),
    day Date,
    tags Array(String),
    attrs Map(String, Int32),
    token UUID,
    ok Bool,
    note String DEFAULT 'none',
    upper_kind String MATERIALIZED upper(kind),
    INDEX by_note note TYPE bloom_filter GRANULARITY 1
) ENGINE = MergeTree ORDER BY (id, day);

INSERT INTO dataman_ch.events (id, user_id, kind, amount, ratio, happened, happened_ms, day, tags, attrs, token, ok, note) VALUES
    (1, 10, 'click', 12.50, 0.5, '2024-01-02 03:04:05', '2024-01-02 03:04:05.123', '2024-01-02', ['a', 'b'], map('x', 1), '00000000-0000-0000-0000-000000000001', true, 'first'),
    (2, NULL, 'view', 0.10, 1.5, '2024-01-03 00:00:00', '2024-01-03 00:00:00.000', '2024-01-03', [], map(), '00000000-0000-0000-0000-000000000002', false, 'tab\there, line\nbreak, back\\slash'),
    (18446744073709551615, -1, 'click', 1.00, nan, '2024-01-04 00:00:00', '2024-01-04 00:00:00.000', '2024-01-04', ['z'], map('y', 2), '00000000-0000-0000-0000-000000000003', true, 'max');

CREATE VIEW dataman_ch.clicks AS SELECT id, kind FROM dataman_ch.events WHERE kind = 'click';
CREATE TABLE dataman_ch.rollup (kind String, n UInt64) ENGINE = SummingMergeTree ORDER BY kind;
CREATE MATERIALIZED VIEW dataman_ch.rollup_mv TO dataman_ch.rollup AS SELECT kind, count() AS n FROM dataman_ch.events GROUP BY kind;
