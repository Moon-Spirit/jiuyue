-- M2 message experience suite: recall tombstones, reply quoting, forward
-- attribution.
--
-- * recalled_at — tombstone marker set by the sender-only, 120s-window
--   recall flow (domain crate RecallPolicy + MessageStateMachine). The row
--   and its body_enc are retained for audit but the body is NEVER served
--   again: sync/live fanout substitute an empty body + recalled flag.
-- * reply_to — optional quote target; must reference a message of the same
--   conversation (validated in the send path before insert). ON DELETE SET
--   NULL keeps history intact if the quoted row ever disappears.
-- * forwarded_from_username — reserved attribution column. DECISION
--   (documented in code too): forwarding is a pure CLIENT-side compose of a
--   new msg.send whose body is prefixed "[转发] " — no wire field exists on
--   msg.send to set this today; the column and the msg.new
--   forwarded_from_username wire field are kept null-absent for a future
--   server-side attribution path.

ALTER TABLE messages ADD COLUMN recalled_at timestamptz NULL;
ALTER TABLE messages ADD COLUMN reply_to uuid NULL REFERENCES messages(id) ON DELETE SET NULL;
ALTER TABLE messages ADD COLUMN forwarded_from_username text NULL;

CREATE INDEX idx_messages_reply_to ON messages(reply_to) WHERE reply_to IS NOT NULL;
