-- M7: profiles + XP / level / rank ladder.
--
-- users gains `bio` + `avatar` (display_name already exists since M1 — it is
-- the column every profile display falls back to username for when empty).
--
-- xp_accounts is the XP economy ledger, one row per user, created LAZILY on
-- the first award (register does NOT insert a row). The pure level curve
-- lives in crates/domain/src/progress.rs — the server only stores xp and the
-- recomputed level:
--
--   * xp        — lifetime total XP (daily-login bonus +20 once per UTC day,
--                 +10 per 100 message characters, secret chats excluded).
--   * level     — STORED, not derived at read time: recomputed through the
--                 domain pure fn inside the same UPDATE as every award, so
--                 it is always consistent with `xp`.
--   * last_daily_bonus_date — the UTC date the +20 bonus was last granted;
--                 NULL = never. The award UPDATE is date-guarded so the
--                 bonus fires at most once per UTC day regardless of how
--                 many connections/devices open.
--   * last_msg_xp_date / msg_xp_today — the daily messaging-XP budget
--                 (cap 200/day). A rollover reset is folded into the award:
--                 when last_msg_xp_date < today the budget restarts at 0.
--
-- `xp`/`level`/`msg_xp_today` carry CHECKs as belt-and-braces; the real
-- invariants are enforced by the Rust award path (single writer per row via
-- SELECT ... FOR UPDATE, so concurrent awards serialize and never race).

ALTER TABLE users ADD COLUMN bio text NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN avatar text NOT NULL DEFAULT '';

CREATE TABLE xp_accounts (
    user_id               uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    xp                    bigint NOT NULL DEFAULT 0 CHECK (xp >= 0),
    level                 int NOT NULL DEFAULT 1 CHECK (level >= 1),
    last_daily_bonus_date date NULL,
    last_msg_xp_date      date NULL,
    msg_xp_today          int NOT NULL DEFAULT 0 CHECK (msg_xp_today >= 0),
    updated_at            timestamptz NOT NULL DEFAULT now()
);
