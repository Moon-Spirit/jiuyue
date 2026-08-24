-- M5: friend system — friend requests + symmetric friendships.
--
-- Friends are an address-book layer ONLY: they do NOT gate conversation
-- creation (direct chats stay open per product decision) and accepting a
-- request never auto-creates a conversation.
--
-- friend_requests carries the full lifecycle (pending → accepted | declined).
-- The partial unique index keeps at most one PENDING row per ordered pair;
-- declined rows remain as history and are flipped back to pending on re-send
-- (either direction counts: a declined them→me row may be reused by me→them).
--
-- friendships is the symmetric materialized edge: TWO rows per befriended
-- pair (user→friend and friend→user) sharing one lexicographically sorted
-- pair_key ("uuidA:uuidB", same convention as conversations.pair_key), so
-- unfriend removes both directions with a single pair_key predicate and the
-- unique index makes the pair un-duplicable regardless of insert order.

CREATE TABLE friend_requests (
    id           uuid PRIMARY KEY,
    from_user    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    to_user      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status       text NOT NULL DEFAULT 'pending'
                 CHECK (status IN ('pending', 'accepted', 'declined')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    responded_at timestamptz NULL
);

-- At most one pending request per ordered (from, to) pair; the application
-- layer additionally treats a pending request in EITHER direction as a
-- duplicate (409 request_already_pending).
CREATE UNIQUE INDEX friend_requests_pending_uq ON friend_requests(from_user, to_user)
    WHERE status = 'pending';

-- Incoming-request inbox scan (GET /api/friends/requests filters
-- to_user + status = 'pending').
CREATE INDEX idx_friend_requests_to_user_status ON friend_requests(to_user, status);

CREATE TABLE friendships (
    user_id   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    friend_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    since     timestamptz NOT NULL DEFAULT now(),
    -- Sorted "uuidA:uuidB" of the pair; identical on BOTH rows so one
    -- DELETE ... WHERE pair_key = $1 clears the friendship in both directions.
    pair_key  text NOT NULL,
    PRIMARY KEY (user_id, friend_id)
);

-- At most one friendship per unordered pair.
--
-- DEVIATION NOTE (deliberate, forced by SQL semantics): the ticket text
-- asked for a bare `CREATE UNIQUE INDEX friendships_pair_uq ON
-- friendships(pair_key)` WHILE ALSO requiring TWO symmetric rows per pair
-- ("unfriend removes BOTH friendship rows"). Both rows carry the SAME sorted
-- pair_key by definition, so a bare unique index would reject the mirror row
-- and silently halve the friendship. The index is therefore scoped to the
-- canonical direction (user_id < friend_id): every unordered pair contributes
-- exactly one indexed row, keeping the one-friendship-per-pair guarantee and
-- race-safe idempotent inserts, while the mirror row lives outside the index.
CREATE UNIQUE INDEX friendships_pair_uq ON friendships(pair_key)
    WHERE user_id < friend_id;
