-- The MySQL test database (`dataman_test`) of the live tests (`tests/live_mysql*.rs`) and of the
-- `mysql-check` demo stage. Each run starts from scratch. Load it with the server's client:
--
--   docker exec -i dataman-mysql mysql -uroot -p"$DATAMAN_MYSQL_PASSWORD" < seeds/mysql.sql
--
-- Customers and orders with a foreign key, every type the grid decodes (ENUM, SET, JSON,
-- DECIMAL, DATETIME, TIMESTAMP, TIME, YEAR, BIT, BLOB, unsigned BIGINT past i64, zero dates),
-- a view, a trigger, a procedure and a function.
DROP DATABASE IF EXISTS dataman_test;
CREATE DATABASE dataman_test CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;
USE dataman_test;

CREATE TABLE customer (
    id         INT AUTO_INCREMENT PRIMARY KEY,
    name       VARCHAR(80) NOT NULL,
    email      VARCHAR(120) UNIQUE,
    tier       ENUM('bronze', 'silver', 'gold') NOT NULL DEFAULT 'bronze',
    balance    DECIMAL(12, 2) NOT NULL DEFAULT 0.00,
    created    DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    profile    JSON,
    photo      BLOB,
    code       BIGINT UNSIGNED,
    tags       SET('vip', 'new', 'wholesale'),
    birth_date DATE,
    at_time    TIME,
    yr         YEAR,
    flags      BIT(8),
    score      FLOAT,
    weight     DOUBLE
);

CREATE TABLE orders (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    customer_id INT NOT NULL,
    status      ENUM('open', 'paid', 'shipped', 'cancelled') NOT NULL DEFAULT 'open',
    total       DECIMAL(12, 2) NOT NULL,
    created_at  TIMESTAMP NULL,
    notes       TEXT,
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customer (id),
    INDEX orders_status_idx (status, created_at)
);

CREATE TABLE item (
    order_id INT NOT NULL,
    line     SMALLINT NOT NULL,
    product  VARCHAR(60) NOT NULL,
    qty      INT NOT NULL,
    price    DECIMAL(10, 2) NOT NULL,
    PRIMARY KEY (order_id, line),
    CONSTRAINT item_order_fk FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE
);

-- Dates off the calendar, which MySQL keeps when the SQL mode allows them.
CREATE TABLE odd_dates (
    id INT PRIMARY KEY,
    d  DATE,
    dt DATETIME
);

INSERT INTO customer (name, email, tier, balance, created, profile, photo, code, tags, birth_date, at_time, yr, flags, score, weight) VALUES
    ('Ana Smith',      'ana@example.com',    'gold',   1520.50, '2026-01-05 09:15:00.250', '{"city": "Boston", "points": 120}',   X'89504E47', 18446744073709551615, 'vip,new',   '1990-04-12', '08:30:00', 1990, b'00000101', 0.1, 72.5),
    ('Ben Lewis',      'ben@example.com',    'silver',  310.00, '2026-02-11 14:00:00',     '{"city": "Denver"}',                  NULL,       42,                   'wholesale', '1985-11-30', '-12:00:00', 1985, b'11111111', 2.5, 81.25),
    ('Carla Day',      'carla@example.com',  'bronze',    0.00, '2026-03-20 18:45:10',     NULL,                                  NULL,       NULL,                 '',          NULL,         '838:59:59', NULL, NULL, NULL, NULL),
    ('Diego O''Neil',  'diego@example.com',  'silver',   99.99, '2026-04-01 00:00:00',     '{"tags": ["a", "b"]}',                X'00FF',    9223372036854775808,  NULL,        '2000-02-29', '00:00:00.5', 2000, b'1', -3.75, 0),
    ('Eva \\ Brown',   NULL,                 'bronze',   12.00, '2026-05-15 12:00:00',     NULL,                                  NULL,       7,                    'vip',       '1999-12-31', NULL, 2026, b'10', 1e-7, 1e300);

INSERT INTO orders (customer_id, status, total, created_at, notes) VALUES
    (1, 'paid',      250.00, '2026-06-01 10:00:00', 'first order'),
    (1, 'shipped',    80.50, '2026-06-03 11:30:00', NULL),
    (2, 'open',      999.99, '2026-06-05 09:00:00', 'urgent'),
    (3, 'cancelled',  10.00, NULL,                  NULL),
    (4, 'paid',       45.00, '2026-06-07 16:20:00', 'gift; wrap it');

INSERT INTO item (order_id, line, product, qty, price) VALUES
    (1, 1, 'Mug', 2, 35.00), (1, 2, 'T-shirt', 3, 60.00), (2, 1, 'Book', 1, 80.50),
    (3, 1, 'Laptop', 1, 999.99), (5, 1, 'Keychain', 9, 5.00);

SET SESSION sql_mode = 'ALLOW_INVALID_DATES';
INSERT INTO odd_dates VALUES (1, '0000-00-00', '0000-00-00 00:00:00'), (2, '2024-02-30', '2024-02-30 10:00:00'), (3, '2024-02-29', '2024-02-29 23:59:59');
SET SESSION sql_mode = DEFAULT;

CREATE VIEW v_order_customer AS
    SELECT o.id, c.name AS customer, o.status, o.total, o.created_at
      FROM orders o JOIN customer c ON c.id = o.customer_id;

DELIMITER //
CREATE TRIGGER orders_created BEFORE INSERT ON orders
FOR EACH ROW
BEGIN
    IF NEW.created_at IS NULL THEN
        SET NEW.created_at = CURRENT_TIMESTAMP;
    END IF;
END //

CREATE PROCEDURE customer_orders(IN c INT)
BEGIN
    SELECT id, status, total FROM orders WHERE customer_id = c ORDER BY id;
END //

CREATE FUNCTION customer_total(c INT) RETURNS DECIMAL(12, 2)
READS SQL DATA
BEGIN
    DECLARE t DECIMAL(12, 2);
    SELECT COALESCE(SUM(total), 0) INTO t FROM orders WHERE customer_id = c AND status <> 'cancelled';
    RETURN t;
END //
DELIMITER ;

ANALYZE TABLE customer, orders, item;
