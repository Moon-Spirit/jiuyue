-- UID: stable numeric user id (QQ-style).
--
-- Every user gets a permanent, unique `uid` bigint. Allocation is APP-SIDE:
-- `users_uid_seq` carries NO column DEFAULT — the register handler runs
-- `SELECT nextval('users_uid_seq')` inside its insert transaction and stores
-- the explicit value. This keeps allocation in one visible place (the code),
-- avoids implicit insert behavior, and remains safe under concurrency because
-- `nextval` is atomic.

ALTER TABLE users ADD COLUMN uid bigint;

CREATE SEQUENCE IF NOT EXISTS users_uid_seq START WITH 100000;

-- Backfill pre-existing users deterministically in (created_at, id) order.
-- A row_number window (not `UPDATE ... SET uid = nextval(...)`) is used
-- because nextval's evaluation order under a hash/merge join is not
-- guaranteed, while row_number IS. uid_n = 100000 - 1 + n, so the first
-- legacy user gets exactly 100000.
WITH ranked AS (
    SELECT id,
           (100000 - 1 + row_number() OVER (ORDER BY created_at, id))::bigint AS next_uid
    FROM users
    WHERE uid IS NULL
)
UPDATE users u
SET uid = ranked.next_uid
FROM ranked
WHERE u.id = ranked.id;

-- Advance the sequence past every backfilled value so future app-side
-- allocations (nextval in the register transaction) never collide.
SELECT setval(
    'users_uid_seq',
    GREATEST((SELECT COALESCE(MAX(uid), 0) FROM users), 100000 - 1),
    true
);

ALTER TABLE users ALTER COLUMN uid SET NOT NULL;

CREATE UNIQUE INDEX users_uid_uq ON users(uid);

-- Owned so dropping `users.uid` drops the sequence cleanly. Deliberately NOT
-- a DEFAULT: allocation stays app-side (nextval inside the register tx).
ALTER SEQUENCE users_uid_seq OWNED BY users.uid;
