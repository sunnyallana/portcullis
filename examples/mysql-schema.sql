-- The demo schema, as MySQL.
--
-- Mirrors examples/postgres-schema.sql so the same actions work against
-- either engine. Used by the live integration test
-- (crates/portcullis-db/tests/mysql_live.rs).
--
--     mysql -h127.0.0.1 -P33306 -uroot -p portcullis < examples/mysql-schema.sql
--
-- Two differences from the PostgreSQL version are worth knowing about:
-- MySQL has no UUID type, so identifiers are CHAR(36); and there is no array
-- type, so the deliberately-unmodelled column here is a BLOB instead.

DROP TABLE IF EXISTS refunds;
DROP TABLE IF EXISTS orders;

CREATE TABLE orders (
    order_no        VARCHAR(32) NOT NULL PRIMARY KEY,
    region          VARCHAR(16) NOT NULL,
    status          VARCHAR(16) NOT NULL,
    placed_at       DATETIME NOT NULL,
    total           DECIMAL(12, 2) NOT NULL,
    customer_email  VARCHAR(254) NOT NULL,
    card_last4      VARCHAR(32),
    -- A type Portcullis does not model: it should be left out of the schema.
    attachment      BLOB,
    INDEX (region, status)
);

CREATE TABLE refunds (
    refund_id   CHAR(36) NOT NULL PRIMARY KEY,
    order_no    VARCHAR(32) NOT NULL,
    region      VARCHAR(16) NOT NULL,
    amount      DECIMAL(12, 2) NOT NULL,
    reason      VARCHAR(280) NOT NULL,
    issued_by   VARCHAR(128) NOT NULL,
    issued_at   DATETIME NOT NULL,
    INDEX (order_no),
    CONSTRAINT fk_refund_order FOREIGN KEY (order_no) REFERENCES orders (order_no)
);

INSERT INTO orders (order_no, region, status, placed_at, total, customer_email, card_last4) VALUES
  ('8812', 'EU',   'open',    '2026-09-20 09:14:00', 1200.00, 'alice@example.com',   '4242424242424242'),
  ('8813', 'US',   'open',    '2026-09-21 16:02:00',   49.99, 'bob@example.com',     '4111111111111111'),
  ('8814', 'EU',   'shipped', '2026-09-22 11:30:00',   82.50, 'carol@example.co.uk', '5555555555554444'),
  ('8815', 'EU',   'held',    '2026-09-24 08:45:00',  310.00, 'dan@example.de',      '4242424242421234'),
  ('8816', 'APAC', 'open',    '2026-09-25 02:10:00',   75.25, 'erin@example.jp',     '4000000000000002');
