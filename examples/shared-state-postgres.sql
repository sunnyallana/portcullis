-- Tables Portcullis needs when more than one replica shares rate limits and
-- replay protection: [limits] store = "database".
--
--     psql "$DATABASE_URL" -f examples/shared-state-postgres.sql
--
-- Portcullis never creates these itself. Creating tables needs privileges the
-- least-privilege database role should not have, and a product that quietly
-- runs DDL against a customer's database is a product nobody approves.

CREATE TABLE IF NOT EXISTS portcullis_rate (
    caller  text   NOT NULL,
    action  text   NOT NULL,
    -- Unix minute. A fixed window rather than a sliding one: a sliding window
    -- costs a row per call and a range scan per check, and the failure mode
    -- here is bounded and understandable — up to twice the limit can pass
    -- across a minute boundary, and never more than that.
    bucket  bigint NOT NULL,
    hits    integer NOT NULL,
    PRIMARY KEY (caller, action, bucket)
);

CREATE INDEX IF NOT EXISTS portcullis_rate_bucket ON portcullis_rate (bucket);

CREATE TABLE IF NOT EXISTS portcullis_replay (
    key       varchar(64) NOT NULL PRIMARY KEY,
    action    text        NOT NULL,
    response  text        NOT NULL,
    at        bigint      NOT NULL   -- unix seconds
);

CREATE INDEX IF NOT EXISTS portcullis_replay_at ON portcullis_replay (at);

-- The grants the running role needs, and no others.
--
--   GRANT SELECT, INSERT, UPDATE, DELETE ON portcullis_rate   TO portcullis;
--   GRANT SELECT, INSERT,         DELETE ON portcullis_replay TO portcullis;
