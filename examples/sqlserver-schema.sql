-- The demo schema, as SQL Server.
--
-- Mirrors examples/postgres-schema.sql so the same actions work against any
-- of the three engines. Used by the live integration test
-- (crates/portcullis-db/tests/mssql_live.rs).
--
--     sqlcmd -S localhost,21433 -U sa -P '…' -C -d portcullis \
--            -i examples/sqlserver-schema.sql
--
-- Two differences worth knowing. SQL Server has no JSON type, so JSON lives
-- in nvarchar and should be declared `text`. And it has no upsert this
-- backend implements: MERGE is a different statement shape, so use
-- mode = "insert" or "update".

IF OBJECT_ID('dbo.refunds', 'U') IS NOT NULL DROP TABLE dbo.refunds;
IF OBJECT_ID('dbo.orders', 'U') IS NOT NULL DROP TABLE dbo.orders;
GO

CREATE TABLE dbo.orders (
    order_no        nvarchar(32)   NOT NULL PRIMARY KEY,
    region          nvarchar(16)   NOT NULL,
    status          nvarchar(16)   NOT NULL,
    placed_at       datetime2      NOT NULL,
    total           decimal(12, 2) NOT NULL,
    customer_email  nvarchar(254)  NOT NULL,
    card_last4      nvarchar(32)   NULL,
    -- A type Portcullis does not model: it should be left out of the schema.
    attachment      varbinary(max) NULL
);
GO

CREATE INDEX orders_region_status ON dbo.orders (region, status);
GO

CREATE TABLE dbo.refunds (
    refund_id   uniqueidentifier NOT NULL PRIMARY KEY,
    order_no    nvarchar(32)     NOT NULL,
    region      nvarchar(16)     NOT NULL,
    amount      decimal(12, 2)   NOT NULL,
    reason      nvarchar(280)    NOT NULL,
    issued_by   nvarchar(128)    NOT NULL,
    issued_at   datetime2        NOT NULL,
    -- A database-generated column: Portcullis should mark it generated.
    seq         bigint IDENTITY(1, 1) NOT NULL,
    CONSTRAINT fk_refund_order FOREIGN KEY (order_no) REFERENCES dbo.orders (order_no)
);
GO

CREATE INDEX refunds_order ON dbo.refunds (order_no);
GO

INSERT INTO dbo.orders (order_no, region, status, placed_at, total, customer_email, card_last4) VALUES
  ('8812', 'EU',   'open',    '2026-09-20T09:14:00', 1200.00, 'alice@example.com',   '4242424242424242'),
  ('8813', 'US',   'open',    '2026-09-21T16:02:00',   49.99, 'bob@example.com',     '4111111111111111'),
  ('8814', 'EU',   'shipped', '2026-09-22T11:30:00',   82.50, 'carol@example.co.uk', '5555555555554444'),
  ('8815', 'EU',   'held',    '2026-09-24T08:45:00',  310.00, 'dan@example.de',      '4242424242421234'),
  ('8816', 'APAC', 'open',    '2026-09-25T02:10:00',   75.25, 'erin@example.jp',     '4000000000000002');
GO
