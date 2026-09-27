-- Tables Portcullis needs when more than one replica shares rate limits and
-- replay protection: [limits] store = "database".
--
--     mysql -h127.0.0.1 -P33306 -uroot -p portcullis < examples/shared-state-mysql.sql
--
-- Portcullis never creates these itself. Creating tables needs privileges the
-- least-privilege database role should not have, and a product that quietly
-- runs DDL against a customer's database is a product nobody approves.

CREATE TABLE IF NOT EXISTS portcullis_rate (
    caller  VARCHAR(190) NOT NULL,
    action  VARCHAR(190) NOT NULL,
    -- Unix minute. A fixed window rather than a sliding one: a sliding window
    -- costs a row per call and a range scan per check, and the failure mode
    -- here is bounded and understandable — up to twice the limit can pass
    -- across a minute boundary, and never more than that.
    bucket  BIGINT NOT NULL,
    hits    INT NOT NULL,
    PRIMARY KEY (caller, action, bucket),
    INDEX portcullis_rate_bucket (bucket)
);

CREATE TABLE IF NOT EXISTS portcullis_replay (
    `key`     VARCHAR(64) NOT NULL PRIMARY KEY,
    action    VARCHAR(190) NOT NULL,
    response  LONGTEXT NOT NULL,
    at        BIGINT NOT NULL,   -- unix seconds
    INDEX portcullis_replay_at (at)
);

-- The grants the running role needs, and no others.
--
--   GRANT SELECT, INSERT, UPDATE, DELETE ON portcullis.portcullis_rate   TO 'portcullis'@'%';
--   GRANT SELECT, INSERT,         DELETE ON portcullis.portcullis_replay TO 'portcullis'@'%';
