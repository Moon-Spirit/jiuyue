<!-- project: jiuyue -->
<!-- extracted from librarian transcript tool_0302aa381001D0yHYwNY7NfxCb, 2026-08-24 -->

# Research Report: Rust WebSocket Chat Backend Patterns (for JiuYue spec)

**Sources investigated**: official `tokio-rs/axum` examples (cloned at `3d78036`), production Rust WS servers (Mozilla **autopush-rs**, n0-computer **iroh-relay**), actix official examples, Telegram MTProto update protocol docs, RFC 9562 / PostgreSQL 18 UUIDv7 material, and 2026 engineering writeups on delivery guarantees and Redis semantics.

---

## 1. Axum WebSocket: connection registry, half-open handling, heartbeat

### 1.1 What the official axum examples actually establish

The axum repo ships two relevant examples. Neither uses DashMap — both use channels, but they solve different problems:

**Example A — `examples/websockets`**: the canonical _single-connection lifecycle_. Key moves: send an immediate `Ping` on upgrade to flush out dead sockets early, then `split()` into sender/receiver tasks joined by `tokio::select!` where whichever task finishes aborts the other:

```rust
// If any one of the tasks exit, abort the other.
tokio::select! {
    rv_a = (&mut send_task) => { recv_task.abort() },
    rv_b = (&mut recv_task) => { send_task.abort() }
}
```

**Evidence** ([websockets/src/main.rs](https://github.com/tokio-rs/axum/blob/3d78036dcac289d6c1d54934708acb6a5bd73686/examples/websockets/src/main.rs#L101-L114), [L147-L209](https://github.com/tokio-rs/axum/blob/3d78036dcac289d6c1d54934708acb6a5bd73686/examples/websockets/src/main.rs#L194-L209)). Also documented there: you never handle inbound `Ping` manually — axum's ws layer auto-replies `Pong` ([L239-L244](https://github.com/tokio-rs/axum/blob/3d78036dcac289d6c1d54934708acb6a5bd73686/examples/websockets/src/main.rs#L239-L244)).

**Example B — `examples/chat`**: the official fanout shape is a `broadcast::Sender<String>` in shared state, one subscriber per connection, and a deliberate ordering rule — **subscribe before announcing**, so a client doesn't miss its own join event ([chat/src/main.rs#L100-L107](https://github.com/tokio-rs/axum/blob/3d78036dcac289d6c1d54934708acb6a5bd73686/examples/chat/src/main.rs#L100-L107)):

```rust
struct AppState {
    user_set: Mutex<HashSet<String>>,
    tx: broadcast::Sender<String>,   // broadcast::channel(100) — bounded backlog
}
```

([L26-L32](https://github.com/tokio-rs/axum/blob/3d78036dcac289d6c1d54934708acb6a5bd73686/examples/chat/src/main.rs#L26-L32), [L46](https://github.com/tokio-rs/axum/blob/3d78036dcac289d6c1d54934708acb6a5bd73686/examples/chat/src/main.rs#L46))

⚠️ **Caveat the example glosses over**: `broadcast` drops oldest messages when a receiver lags, surfacing as `RecvError::Lagged(n)` — the example's `while let Ok(msg) = rx.recv().await` loop silently _exits_ on lag, killing the connection. For chat you must match it explicitly (skip-and-continue or force-resync). This makes raw `broadcast` wrong as your primary delivery mechanism for 1v1 DMs — losing frames is only acceptable if the DB cursor resync covers it.

### 1.2 What production Rust servers do: DashMap + bounded mpsc per connection

**Mozilla autopush-rs** (Rust WebPush server, real production fleet) — `ClientRegistry` is exactly the shape you want:

```rust
pub struct ClientRegistry {
    clients: DashMap<Uuid, RegisteredClient>,
    channel_capacity: usize,               // DEFAULT_CHANNEL_CAPACITY = 128
}
struct RegisteredClient {
    pub uaid: Uuid,
    pub uid: Uuid,
    pub tx: mpsc::Sender<ServerNotification>,   // bounded channel per client
}
```

**Evidence** ([registry.rs](https://github.com/mozilla-services/autopush-rs/blob/bcdc6fcba45185d72e1b012035f89f6f79e942f2/autoconnect/autoconnect-common/src/registry.rs#L13-L30)). Two behaviors worth copying verbatim:

- **Ghost-connection eviction on reconnect** — `connect()` inserts the new client and, if an old entry existed, pushes `ServerNotification::Disconnect` into the stale channel so the zombie task tears down ([L50-L66](https://github.com/mozilla-services/autopush-rs/blob/bcdc6fcba45185d72e1b012035f89f6f79e942f2/autoconnect/autoconnect-common/src/registry.rs#L52-L66)).
- **Backpressure via `try_send`, never `.await`** — delivery to a slow client returns `ChannelFull` instead of blocking the caller; the notification stays durable upstream and gets delivered on reconnect ([L68-L98](https://github.com/mozilla-services/autopush-rs/blob/bcdc6fcba45185d72e1b012035f89f6f79e942f2/autoconnect/autoconnect-common/src/registry.rs#L77-L97)).

Same pattern independently in **iroh-relay** (`clients: DashMap<EndpointId, ClientState>` behind an `Arc<Inner>` facade, [clients.rs](https://github.com/n0-computer/iroh/blob/01e74bc1ec78a8a96ae507b4a3ecca99f166599f/iroh-relay/src/server/clients.rs#L29-L36)) and NovaSDR (`chat_clients: DashMap<ClientId, mpsc::Sender<_>>`). **DashMap-of-senders is the de-facto production registry pattern in Rust.**

### 1.3 Heartbeat / half-open detection

Actix's official chat example is the clearest reference implementation — server pings on an interval, tracks last client activity, disconnects on timeout:

```rust
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

fn hb(&self, ctx: &mut ws::WebsocketContext<Self>) {
    ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
        if Instant::now().duration_since(act.hb) > CLIENT_TIMEOUT {
            ctx.stop();            // half-open: kill it
            return;
        }
        ctx.ping(b"");
    });
}
```

**Evidence** ([actix/examples websockets/chat/src/session.rs](https://github.com/actix/examples/blob/master/websockets/chat/src/session.rs#L9-L12), [hb fn L37-L47](https://github.com/actix/examples/blob/master/websockets/chat/src/session.rs#L37-L47)). Any inbound frame (including the automatic Pong) refreshes `hb`. Current guidance converges on ~30s ping intervals for browser/mobile clients behind NAT/LBs, with timeout at 2× interval; the 2026 WebSocket-patterns writeup additionally stresses pairing background heartbeat with a staleness check at interaction time, because mobile browsers throttle timers ([devonbleibtrey.com](https://www.devonbleibtrey.com/blog/websocket-patterns-for-chat)).

**Registry verdict for JiuYue**: `DashMap<UserId, HashMap<DeviceId, ConnHandle>>` where `ConnHandle { tx: mpsc::Sender<OutFrame>, conn_id, last_pong }`. Per-device entries (not per-user) from day one — multi-device is already in your scope. Bounded channel (128–1024) + `try_send`; on `Full`, don't block — the DB cursor resync is your safety net. Spawn one writer task per connection that owns the socket sink (the split + select! shape from Example A); never share the sink across tasks.

---

## 2. Reliable delivery: IDs, acks, sequencing, offline sync

### 2.1 Message IDs: UUIDv7 wins over snowflake for your stack

- UUIDv7 is now an IETF standard (**RFC 9562, May 2024**) — 48-bit Unix-ms timestamp prefix + 74 random bits, lexicographically sortable ([rfc-editor.org](https://www.rfc-editor.org/info/rfc9562)).
- First-class Rust support: `uuid = { version = "1", features = ["v7"] }` → `Uuid::now_v7()` ([docs.rs/uuid](https://doc.servo.org/uuid/index.html), [usage ref](https://www.ytyng.com/en/blog/ulid-vs-uuidv7-django-primary-key)).
- PostgreSQL 18 ships native `uuidv7()`; on **PG 16 you generate app-side** (which you'd do anyway — you need the ID before insert for the ack). Note: pgsql-hackers threads flag that range-partitioning _by_ UUIDv7 works but has planner/statistics caveats — partition by time column instead ([pgsql-performance thread, Oct 2025](https://www.postgresql.org/message-id/CAE_7N37Amq_G6bZDpv9tbxcZJNGVhSv8mt2MJgdZTqGO5Pdbnw@mail.gmail.com)).
- Snowflake buys you nothing here unless you need 64-bit IDs for wire-size reasons; UUIDv7 gives the same time-ordering property without running a generator service.

**Critical distinction** (echoed by every credible source): the UUIDv7/snowflake ID is for _identity and rough time order_. **Ordering and sync cursors must be a separate per-conversation monotonic integer (`seq`)** — wall-clock-derived IDs are not gap-detectable, and clock skew across nodes breaks them ([techinterview.org](https://www.techinterview.org/post/3233476407/chat-system-design-delivery-ordering-presence/), [systemforces.com](https://www.systemforces.com/blog/real-time-chat-system-design-guide)).

### 2.2 The Telegram pts model — the canonical offline-sync design

Telegram's protocol doc is the primary source for the cursor scheme you referenced ([core.telegram.org/api/updates](https://core.telegram.org/api/updates)):

- Each "message box" (conversation) has an independent auto-incremented `pts`; every update carries `pts` + `pts_count`.
- Client-side apply rules:
  - `local_pts + pts_count == pts` → apply
  - `local_pts + pts_count > pts` → duplicate/out-of-order → ignore
  - `local_pts + pts_count < pts` → **gap** → call `getDifference` (fetch delta since local state)
- On startup/offline recovery: fetch difference from stored state; `updatesTooLong` signals "too much backlog, pull manually."

This maps directly onto your offline sync: store `(conversation_id, last_seq)` per device; on reconnect send the cursor; server returns `WHERE conversation_id = $1 AND seq > $cursor ORDER BY seq`. Gap detection is O(1) and needs no diffing ([zylos.ai research](https://zylos.ai/research/2026-06-01-real-time-message-delivery-guarantees-distributed-im-systems/)).

### 2.3 Ack model & idempotency — the consensus pipeline

Every high-quality 2026 writeup converges on the same send path ([sujeet.pro staff-level design](https://sujeet.pro/articles/design-real-time-chat-messaging), [WhatsApp breakdown](https://techinterview.coach/blog/system-design-of-whatsapp/), [systemdesignschool.io](https://systemdesignschool.io/problems/chatapp/solution)):

```
client: msg{client_msg_id} ──► server: dedupe(client_msg_id) → validate
        → assign seq + INSERT (idempotent) → ACK(sender, seq, server_id)
        → route to recipient gateway → push → recipient ACKs → advance delivered-cursor
```

Non-negotiable rules extracted from these sources:

1. **Persist-then-ack.** "Acking before persisting risks silent loss" — the single-tick means _stored_, not _sent_.
2. **At-least-once transport + idempotent insert = effectively-once UX.** Client retries reuse the same `client_msg_id`; server dedupes. True exactly-once is rejected everywhere as not worth distributed-transaction cost.
3. **Idempotent insert in SQL**: `UNIQUE (conversation_id, client_msg_id)` + `INSERT ... ON CONFLICT DO NOTHING RETURNING id` — conflict ⇒ return the existing row's `(id, seq)` as a `duplicate` ack. Zylos documents WhatsApp's exact shape: inbox row survives until client ACK deletes it; the WS push is best-effort, the inbox row is the guarantee.
4. **No server-side per-device queue tables needed for MVP.** Sujeet's conclusion: "Offline sync is solved by per-device cursors plus self-echo fan-out, **not per-device server-side queues**." The messages table _is_ the offline queue; the cursor just points into it.
5. **Self-echo**: deliver the persisted message back to the sender's _other_ devices using the same path — required for multi-device coherence.

### 2.4 Assigning `seq` — three options ranked for PG16

| Option                                | How                                                                                                                      | Trade-off                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **A. DB row increment (recommended)** | `UPDATE conversations SET last_seq = last_seq + 1 WHERE id=$1 RETURNING last_seq` in the _same tx_ as the message INSERT | Atomic, durable, no extra infra; per-conversation hotspot is irrelevant at 1v1 scale ("per-conversation contention is near zero" — [whiteboardscale](https://www.whiteboardscale.com/topics/chat-system/concepts/chat-message-ordering)); matches Slack-style DB-level sequences ([zylos](https://zylos.ai/research/2026-06-01-real-time-message-delivery-guarantees-distributed-im-systems/)) |
| B. Redis `INCR seq:{conv}`            | Fast, O(1)                                                                                                               | Redis becomes the ordering authority — a crash between INCR and INSERT burns seqs (fine, gaps are legal) but a Redis failover can _reuse_ seqs (not fine unless you persist + fsync); adds a hard runtime dependency on the critical write path                                                                                                                                                |
| C. Snowflake/UUIDv7 as sort key       | No coordination                                                                                                          | Not gap-detectable; clients can't detect loss — defeats the pts model                                                                                                                                                                                                                                                                                                                          |

Gaps in `seq` must be **legal** (aborted transactions, future sharding) — clients treat gaps as "poll/resync," never as errors. Telegram tolerates this by design.

---

## 3. Redis boundaries: pub/sub vs streams vs plain keys

### 3.1 Semantics (2026 consensus)

|                                   | Pub/Sub                | Streams                                   |
| --------------------------------- | ---------------------- | ----------------------------------------- |
| Persistence                       | none — fire-and-forget | until `MAXLEN` trim                       |
| Subscriber offline during publish | message lost forever   | replayable from any ID                    |
| Delivery guarantee                | at-most-once           | at-least-once w/ consumer groups + `XACK` |
| Recovery of stuck consumer        | n/a                    | `XAUTOCLAIM`/`XCLAIM`                     |
| Cluster behavior                  | broadcast cross-shard  | shard-local per key                       |

**Evidence**: [oneuptime decision guide](https://oneuptime.com/blog/post/2026-03-31-redis-when-to-use-redis-pubsub-vs-redis-streams/view), [stackharbor](https://stackharbor.com/en/knowledge-base/redis-pubsub-vs-streams/), [datasops production guide](https://www.datasops.com/blog/redis-patterns-production) — all converge on: _"Pub/Sub for real-time broadcast where loss is acceptable; Streams whenever you cannot afford to lose messages; hybrid = stream as source of truth + pub/sub as doorbell."_

### 3.2 The boundary rule for JiuYue

> **Postgres is the only source of truth for messages. Redis holds only state that is safe to lose.**

| Concern                                                                  | Mechanism                                                                                                                | Why                                                                                                                                                                                                                                                                                                                                                                          |
| ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cross-instance live push ("wake up the instance holding device X")       | **Pub/Sub** channel `user:{id}` or `conv:{id}`                                                                           | Missed notify is harmless: recipient's cursor is behind, next sync/reconnect delivers. This is the standard router pattern — publish to target channel, owning instance writes the socket ([letsbuildsolutions](https://letsbuildsolutions.com/blog/system-design/designing-a-real-time-chat-system-message-ordering-delivery-guarantees-and-presence-management-at-scale/)) |
| Offline queue                                                            | **Neither** — it's the `messages` table + per-device cursor                                                              | See 2.3                                                                                                                                                                                                                                                                                                                                                                      |
| Retry/fan-out work queue (later: push notifications, recall propagation) | **Streams** + consumer group, `MAXLEN ~`, DLQ pattern                                                                    | At-least-once with ACK; rubel.dev documents the Pub/Sub→Streams migration after losing ~5% of notifications ([rubel.dev](https://rubel.dev/blog/redis-pub-sub-vs-streams-building-reliable-notifications))                                                                                                                                                                   |
| Presence                                                                 | Plain keys: `SET presence:{user} {conn_meta} EX 60` refreshed by heartbeat; or ZSET `last_seen` score for "last seen at" | Ephemeral by definition — "reconstructed on connect; tolerates lossy updates" ([sujeet.pro](https://sujeet.pro/articles/design-real-time-chat-messaging))                                                                                                                                                                                                                    |
| Typing indicators                                                        | Pub/Sub only                                                                                                             | Loss is meaningless; never touch disk                                                                                                                                                                                                                                                                                                                                        |

**MVP simplification**: with 1 instance, you don't even need Redis pub/sub (in-process DashMap suffices); introduce it exactly when instance #2 appears. Skip Streams entirely until you have background workers (push-notification fan-out is the usual first trigger).

---

## 4. PostgreSQL schema conventions

### 4.1 Messages table

Reference production design — MagicChat (chaitin) documents a real deployment: **range-by-year × hash(conversation_id) composite partitioning**, with a side `message_registry` table holding the global ID, conversation seq, and client idempotency key so dedupe lookups stay O(1) across all partitions, and immutable identity fields enforced by convention ([message-partitions.md](https://github.com/chaitin/MagicChat/blob/main/server/docs/message-partitions.md)):

```text
messages
└── messages_2026
    ├── messages_2026_p00 ... messages_2026_p31   -- hash(conversation_id)
```

Key rules from that doc: deploy partitioning _before_ the table is big (or shadow-table backfill); retention window defined in code; "Do not modify created_at, conversation_id, seq, sender identity, or the client message ID after insertion."

**For an MVP on PG16**: start **unpartitioned** with the right indexes — declarative partitioning pays off around hundreds of millions of rows / years of data, and converting later is a known migration (MagicChat migration `00015` does exactly this). The index layout matters from day one:

```sql
CREATE TABLE messages (
    id              uuid PRIMARY KEY,          -- UUIDv7, app-generated (uuid crate)
    conversation_id bigint      NOT NULL,
    seq             bigint      NOT NULL,      -- per-conversation monotonic (2.4-A)
    sender_id       bigint      NOT NULL,
    client_msg_id   uuid        NOT NULL,      -- idempotency key from sender device
    kind            smallint    NOT NULL DEFAULT 0,   -- text first; reply/recall later
    body            text        NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT uq_conv_client UNIQUE (conversation_id, client_msg_id)  -- dedupe
);
-- THE inbox/history query index: cursor sync + recent-history pagination
CREATE INDEX idx_messages_conv_seq ON messages (conversation_id, seq DESC);

-- conversations: seq authority lives here (single-row UPDATE ... RETURNING)
ALTER TABLE conversations ADD COLUMN last_seq bigint NOT NULL DEFAULT 0;
```

`(conversation_id, seq DESC)` serves both `sync(since_seq)` forward scans and "load last N" pagination — this is the standard layout across all schema references found ([knowledgelib schema](https://knowledgelib.io/software/system-design/chat-app-at-scale/2026), [dba.stackexchange thread](https://dba.stackexchange.com/questions/268388/chat-conversation-history-entity-relationship-diagram)). Partition later by `created_at` range (+ optional hash sub-partition), keeping the unique constraint implementable by including `conversation_id` in every uniqueness key (Postgres partitioned tables require partition keys inside unique constraints — another reason `uq_conv_client` is shaped this way).

### 4.2 Devices / sessions (multi-device)

```sql
CREATE TABLE devices (
    id           bigint PRIMARY KEY,
    user_id      bigint NOT NULL REFERENCES users,
    device_kind  smallint NOT NULL,             -- ios/android/web/desktop
    push_token   text,
    last_seen_at timestamptz NOT NULL,
    UNIQUE (user_id, device_kind)               -- or allow N per kind
);

CREATE TABLE conversation_members (
    conversation_id bigint NOT NULL,
    user_id         bigint NOT NULL,
    -- per-user read/delivered cursors (receipts ride on these later)
    last_read_seq   bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (conversation_id, user_id)
);

CREATE TABLE device_cursors (                    -- per-DEVICE sync point
    conversation_id bigint NOT NULL,
    device_id       bigint NOT NULL,
    last_seq        bigint NOT NULL DEFAULT 0,  -- Telegram-style local_pts
    PRIMARY KEY (conversation_id, device_id)
);
```

Design notes grounded in sources: Synapse scopes idempotency/transaction IDs **per device** (MSC3970 "Scope transaction IDs to devices") — so key dedupe on `(device, client_msg_id)` conceptually, though keeping the unique constraint conversation-wide is stricter and safe; per-device cursors let phone and laptop sit at different points and catch up independently ([techinterview.org](https://www.techinterview.org/post/3233476407/chat-system-design-delivery-ordering-presence/)). Live connection state (which gateway holds which device) does **not** go in Postgres — Redis presence keys only, rebuilt on connect ([systemdesignschool.io](https://systemdesignschool.io/problems/chatapp/solution): "routing and presence live in memory, off the durable path").

---

## 5. Verdict: recommended combination for JiuYue MVP

| Decision         | Pick                                                                                                                                                                                | Rationale                                                                                                              |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Registry         | `DashMap<UserId, HashMap<DeviceId, ConnHandle>>`, bounded `mpsc` (128+) per conn, `try_send` backpressure, ghost-evict on reconnect                                                 | Production-proven shape (autopush-rs, iroh); broadcast is wrong for 1v1 (Lagged drops frames silently)                 |
| Socket lifecycle | axum official shape: initial Ping → `split()` → writer task + reader task → `select!` abort-pair                                                                                    | It's the maintained upstream example ([axum#examples](https://github.com/tokio-rs/axum/tree/main/examples/websockets)) |
| Heartbeat        | Server-initiated Ping q30s, kick at 60s idle; rely on axum auto-Pong; refresh Redis presence TTL on any frame                                                                       | Actix reference impl; NAT/LB keepalive; presence for free                                                              |
| IDs              | `UUIDv7` (app-side, `uuid` crate `v7`) for message identity + `BIGINT seq` per conversation for ordering/cursor                                                                     | RFC 9562-standardized; sortable PK avoids PG16's missing `uuidv7()`; seq is what makes pts-style sync possible         |
| seq assignment   | `UPDATE conversations SET last_seq=last_seq+1 … RETURNING` in the message-insert transaction                                                                                        | One atomic roundtrip, zero new infra, survives crashes; Redis INCR demoted to optional optimization                    |
| Delivery         | Insert+seq → commit → Redis pub/sub notify → owner instance `try_send`s frame → client ACK advances `device_cursors.last_seq` → sender gets self-echo                               | Persist-then-ack; at-least-once + `ON CONFLICT DO NOTHING` dedupe = effectively-once                                   |
| Offline sync     | `sync(conv, since_seq)` cursor pull on reconnect/connect; gaps legal, trigger resync                                                                                                | Telegram pts model verbatim; no per-device queue tables                                                                |
| Redis boundary   | pub/sub = live notify + typing; plain keys TTL = presence; streams = deferred until background workers exist                                                                        | Postgres is truth; Redis only holds losable state                                                                      |
| Schema           | Unpartitioned `messages` + `(conversation_id, seq DESC)` index + `UNIQUE(conversation_id, client_msg_id)`; devices + per-device cursors; partition by year×hash when volume demands | Right-sized for MVP; migration path documented (MagicChat)                                                             |

**Multi-region note baked into these choices**: per-conversation `seq` means consistency domains are conversations, not the globe — home each conversation to a region and the pts model composes across regions without a global clock (Telegram runs exactly this way: "all boxes are completely independent, each pts sequence tied to just one box").

**Top three traps to encode in the spec explicitly**: (1) `broadcast::RecvError::Lagged` / slow-client policy — bounded channel full ⇒ skip push, rely on cursor resync, optionally kick; (2) reconnect thundering herd — exponential backoff **with jitter** client-side ([slack.engineering via knowledgelib](https://knowledgelib.io/software/system-design/chat-app-at-scale/2026)); (3) close-code discipline — never send bare `1000` from the client so reconnect logic can distinguish server/proxy closes ([devonbleibtrey](https://www.devonbleibtrey.com/blog/websocket-patterns-for-chat)).
