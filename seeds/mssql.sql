-- The SQL Server test database (`dataman_test`) of the live tests (`tests/live_mssql*.rs`) and of
-- the `mssql-check` demo stage. Each run starts from scratch. Load it with the container's sqlcmd:
--
--   docker exec -i dataman-mssql /opt/mssql-tools18/bin/sqlcmd -C -I -b -S localhost -U sa \
--       -P "$DATAMAN_MSSQL_PASSWORD" < seeds/mssql.sql
--
-- (`-I`: QUOTED_IDENTIFIER ON, which the persisted computed column needs; sqlcmd's default is
-- off.)
--
-- Customers, orders and items with foreign keys (one cascading), identity columns, a computed
-- column, a rowversion, decimal/money, datetime2/datetimeoffset/datetime/smalldatetime,
-- uniqueidentifier, nvarchar with non-ASCII text, varbinary, xml, a check constraint, an
-- alias type, a table type, a sequence, a view, a trigger, a procedure with two result sets, a
-- scalar and a table-valued function, a second schema with a heap (no key) and a synonym.
USE master;
GO
IF DB_ID(N'dataman_test') IS NOT NULL
BEGIN
    ALTER DATABASE dataman_test SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE dataman_test;
END
GO
CREATE DATABASE dataman_test;
GO
USE dataman_test;
GO
CREATE SCHEMA sales;
GO
CREATE TYPE dbo.national_id FROM varchar(11) NOT NULL;
CREATE TYPE dbo.order_items AS TABLE (product nvarchar(60) NOT NULL, qty int NOT NULL);
CREATE SEQUENCE dbo.seq_invoice AS bigint START WITH 1000 INCREMENT BY 1;
GO
CREATE TABLE dbo.customer (
    id           int IDENTITY(1, 1) CONSTRAINT PK_customer PRIMARY KEY,
    name         nvarchar(80) NOT NULL,
    email        varchar(120) NULL CONSTRAINT UQ_customer_email UNIQUE,
    active       bit NOT NULL CONSTRAINT DF_customer_active DEFAULT 1,
    tier         tinyint NOT NULL CONSTRAINT DF_customer_tier DEFAULT 0,
    balance      decimal(12, 2) NOT NULL CONSTRAINT DF_customer_balance DEFAULT 0,
    credit_limit money NULL,
    rate         smallmoney NULL,
    created      datetime2(3) NOT NULL CONSTRAINT DF_customer_created DEFAULT SYSUTCDATETIME(),
    seen         datetimeoffset(0) NULL,
    birth_date   date NULL,
    at_time      time(0) NULL,
    legacy_dt    datetime NULL,
    small_dt     smalldatetime NULL,
    code         uniqueidentifier NULL,
    photo        varbinary(max) NULL,
    profile      xml NULL,
    score        real NULL,
    weight       float NULL,
    points       bigint NULL,
    doc          dbo.national_id NULL,
    row_version  rowversion
);

CREATE TABLE dbo.orders (
    id          int IDENTITY(1, 1) CONSTRAINT PK_orders PRIMARY KEY,
    customer_id int NOT NULL CONSTRAINT FK_orders_customer REFERENCES dbo.customer (id),
    status      varchar(12) NOT NULL CONSTRAINT DF_orders_status DEFAULT 'open'
                CONSTRAINT CK_orders_status CHECK (status IN ('open', 'paid', 'shipped', 'cancelled')),
    total       decimal(12, 2) NOT NULL,
    created_at  datetime2(0) NULL,
    notes       nvarchar(max) NULL
);
CREATE INDEX IX_orders_status ON dbo.orders (status, created_at DESC) INCLUDE (total);

CREATE TABLE dbo.item (
    order_id int NOT NULL CONSTRAINT FK_item_order REFERENCES dbo.orders (id) ON DELETE CASCADE,
    line     smallint NOT NULL,
    product  nvarchar(60) NOT NULL,
    qty      int NOT NULL CONSTRAINT CK_item_qty CHECK (qty > 0),
    price    decimal(10, 2) NOT NULL,
    subtotal AS (qty * price) PERSISTED,
    CONSTRAINT PK_item PRIMARY KEY (order_id, line)
);

CREATE TABLE sales.goal (
    region nvarchar(40) NOT NULL CONSTRAINT PK_goal PRIMARY KEY,
    target money NOT NULL
);

-- A heap: no key, paged in no particular order.
CREATE TABLE sales.event (
    happened_at datetime2(0) NOT NULL,
    body        nvarchar(200) NULL
);

CREATE SYNONYM sales.customers FOR dbo.customer;

INSERT INTO dbo.customer (name, email, active, tier, balance, credit_limit, rate, created, seen, birth_date, at_time, legacy_dt, small_dt, code, photo, profile, score, weight, points, doc) VALUES
    (N'Ana Smith',          'ana@example.com',   1, 3, 1520.50, 5000.00, 2.5,  '2026-01-05T09:15:00.250', '2026-01-05T09:15:00-03:00', '1990-04-12', '08:30:00', '2026-01-05T09:15:00.003', '2026-01-05T09:15:00', '6F9619FF-8B86-D011-B42D-00C04FC964FF', 0x89504E47, N'<profile city="Boston"><points>120</points></profile>', 0.1, 72.5, 9223372036854775807, '12345678901'),
    (N'Ben Lewis',          'ben@example.com',   1, 2,  310.00, NULL,    NULL, '2026-02-11T14:00:00',     NULL,                         '1985-11-30', '23:59:59', NULL,                      NULL,                  NULL,                                   NULL,       NULL,                                                     2.5,  81.25, 42,                 NULL),
    (N'Carla Day',          'carla@example.com', 0, 0,    0.00, 0.0001,  NULL, '2026-03-20T18:45:10',     NULL,                         NULL,         NULL,       NULL,                      NULL,                  NULL,                                   NULL,       NULL,                                                     NULL, NULL,  NULL,               NULL),
    (N'Diego O''Neil',      'diego@example.com', 1, 2,   99.99, -12.3456, NULL, '2026-04-01T00:00:00',    '2026-04-01T12:00:00+09:00', '2000-02-29', '00:00:01', '1753-01-01T00:00:00',     NULL,                  NULL,                                   0x00FF,     NULL,                                                     -3.75, 0,    -9223372036854775808, NULL),
    (N'Élodie Zürich 東京', NULL,                1, 1,   12.00, NULL,    NULL, '2026-05-15T12:00:00',     NULL,                         '1999-12-31', NULL,       NULL,                      NULL,                  NULL,                                   NULL,       NULL,                                                     1e-7, 1e300, 7,                  NULL);

INSERT INTO dbo.orders (customer_id, status, total, created_at, notes) VALUES
    (1, 'paid',      250.00, '2026-06-01T10:00:00', N'first order'),
    (1, 'shipped',    80.50, '2026-06-03T11:30:00', NULL),
    (2, 'open',      999.99, '2026-06-05T09:00:00', N'urgent'),
    (3, 'cancelled',  10.00, NULL,                  NULL),
    (4, 'paid',       45.00, '2026-06-07T16:20:00', N'gift; wrap it');

INSERT INTO dbo.item (order_id, line, product, qty, price) VALUES
    (1, 1, N'Mug', 2, 35.00), (1, 2, N'T-shirt', 3, 60.00), (2, 1, N'Book', 1, 80.50),
    (3, 1, N'Laptop', 1, 999.99), (5, 1, N'Keychain', 9, 5.00);

INSERT INTO sales.goal (region, target) VALUES (N'Northeast', 150000), (N'Southeast', 420000.5);
INSERT INTO sales.event (happened_at, body) VALUES ('2026-06-01T10:00:00', N'opening'), ('2026-06-01T10:00:00', N'opening'), ('2026-06-02T08:00:00', NULL);
GO
CREATE VIEW dbo.v_order_customer AS
    SELECT o.id, c.name AS customer, o.status, o.total, o.created_at
      FROM dbo.orders o JOIN dbo.customer c ON c.id = o.customer_id;
GO
-- Orders without a date get the time they were created.
CREATE TRIGGER dbo.orders_created ON dbo.orders AFTER INSERT AS
BEGIN
    SET NOCOUNT ON;
    UPDATE o SET created_at = SYSUTCDATETIME()
      FROM dbo.orders o JOIN inserted i ON i.id = o.id
     WHERE o.created_at IS NULL;
END;
GO
CREATE PROCEDURE dbo.customer_orders @customer int AS
BEGIN
    SET NOCOUNT ON;
    SELECT id, status, total FROM dbo.orders WHERE customer_id = @customer ORDER BY id;
    SELECT COUNT(*) AS items FROM dbo.item i JOIN dbo.orders o ON o.id = i.order_id WHERE o.customer_id = @customer;
END;
GO
CREATE FUNCTION dbo.customer_total (@customer int) RETURNS decimal(12, 2) AS
BEGIN
    RETURN (SELECT COALESCE(SUM(total), 0) FROM dbo.orders WHERE customer_id = @customer AND status <> 'cancelled');
END;
GO
CREATE FUNCTION sales.items_of (@order int) RETURNS TABLE AS
    RETURN SELECT line, product, qty, price FROM dbo.item WHERE order_id = @order;
GO
-- JSON kept in nvarchar(max), as SQL Server stores it: nested, arrays, unicode; one plain text
-- and one broken value (no eye for those). The grid's eye opens and edits the others.
CREATE TABLE dbo.documents (
    id         int IDENTITY(1, 1) PRIMARY KEY,
    title      nvarchar(100) NOT NULL,
    body       nvarchar(max),
    updated_at datetime2(0) NOT NULL DEFAULT '2026-10-01T09:00:00'
);
GO
INSERT INTO dbo.documents (title, body, updated_at) VALUES
    (N'Order 1001', N'{"order": 1001, "customer": {"name": "Ana Lima", "city": "São Paulo", "tags": ["vip", "early"]}, "items": [{"sku": "AUD-0101", "qty": 2, "price": 199.0}, {"sku": "AUD-0105", "qty": 1, "price": 59.0}], "paid": true, "notes": null}', '2026-10-01T09:00:00'),
    (N'Settings', N'{"theme": "dark", "font": {"family": "Geist Mono", "size": 13}, "recent": [], "flags": {"beta": false, "limits": {"rows": 1000, "timeout_ms": 30000}}}', '2026-10-02T10:30:00'),
    (N'Tags', N'["数据", "données", "Daten", "🙂", "naïve café"]', '2026-10-03T11:45:00'),
    (N'Event', N'{"type": "login", "at": "2026-10-04T08:15:00Z", "geo": {"lat": -23.5505, "lng": -46.6333}, "device": {"os": "macOS", "version": "15.1"}}', '2026-10-04T08:15:00'),
    (N'Empty object', N'{}', '2026-10-05T12:00:00'),
    (N'Plain text', N'Not JSON at all: just a note.', '2026-10-06T13:00:00'),
    (N'Broken', N'{"unterminated": [1, 2', '2026-10-07T14:00:00');
GO
