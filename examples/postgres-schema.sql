-- The demo schema, as PostgreSQL.
--
-- Mirrors examples/demo-data.json so the same examples/orders.toml works
-- against a real server once [backend] is switched to postgres. Also used by
-- the live integration test (crates/portcullis-db/tests/postgres_live.rs).
--
--     psql "$DATABASE_URL" -f examples/postgres-schema.sql

DROP TABLE IF EXISTS refunds;
DROP TABLE IF EXISTS orders;

CREATE TABLE orders (
    order_no        text PRIMARY KEY,
    region          text NOT NULL,
    status          text NOT NULL,
    placed_at       timestamptz NOT NULL,
    total           numeric(12, 2) NOT NULL,
    customer_email  text NOT NULL,
    card_last4      text,
    -- Deliberately a type Portcullis does not model. It should be dropped from the
    -- schema rather than guessed at, and any action naming it should fail
    -- validation at startup.
    tags            text[]
);

CREATE TABLE refunds (
    refund_id   uuid PRIMARY KEY,
    order_no    text NOT NULL REFERENCES orders (order_no),
    region      text NOT NULL,
    amount      numeric(12, 2) NOT NULL,
    reason      text NOT NULL,
    issued_by   text NOT NULL,
    issued_at   timestamptz NOT NULL,
    -- A database-generated column: Portcullis should mark it generated and warn if
    -- an action tries to set it.
    seq         bigint GENERATED ALWAYS AS IDENTITY
);

CREATE INDEX ON orders (region, status);
CREATE INDEX ON refunds (order_no);

INSERT INTO orders (order_no, region, status, placed_at, total, customer_email, card_last4, tags) VALUES
  ('8812', 'EU',   'open',    '2026-09-20T09:14:00Z', 1200.00, 'alice@example.com',    '4242424242424242', ARRAY['priority']),
  ('8813', 'US',   'open',    '2026-09-21T16:02:00Z',   49.99, 'bob@example.com',      '4111111111111111', NULL),
  ('8814', 'EU',   'shipped', '2026-09-22T11:30:00Z',   82.50, 'carol@example.co.uk',  '5555555555554444', NULL),
  ('8815', 'EU',   'held',    '2026-09-24T08:45:00Z',  310.00, 'dan@example.de',       '4242424242421234', ARRAY['fraud-review']),
  ('8816', 'APAC', 'open',    '2026-09-25T02:10:00Z',   75.25, 'erin@example.jp',      '4000000000000002', NULL);
