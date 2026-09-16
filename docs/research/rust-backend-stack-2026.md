# Rust IM Backend Stack — Decision Research (Sep 2026)

Target: single HK cloud box, 2 vCPU / 2 GB RAM, Docker Compose.
App: IM (1:1 + group), presence, typing, read receipts, E2EE secret chats, WebRTC signaling.
Frontend: Vue 3 + Vite + TS (web / PWA / Tauri).

Every version below was read live from crates.io or from the project's own manifest at the
commit SHA listed in Sources. Download figures are "last 90 days" as reported by the
crates.io API on 2026-09-16.

---

## TL;DR decision table

| # | Decision | Recommendation | Version (Sep 2026) | Strongest evidence |
|---|----------|----------------|--------------------|--------------------|
| a | HTTP framework | **axum** | 0.8.9 | 115.8M/90d vs actix-web 10.5M/90d (11x). Conduit, tuwunel, continuwuity, Revolt, RustDesk, matrix-authentication-service all on axum |
| b | WebSocket | **axum `ws` feature** (= tokio-tungstenite under the hood) | axum `ws` -> tokio-tungstenite 0.29 | axum's own manifest: `ws = ["dep:tokio-tungstenite"]`. Do NOT treat it as either/or |
| c | WS payload format | **JSON + a versioned envelope**; keep a binary codec behind a size threshold | serde_json 1.0.151 | Chat payloads are text-dominated: MessagePack saves ~0 bytes on UTF-8 text and kills DevTools, while JSON is a native parse on the client |
| d | PostgreSQL | **sqlx** (async, raw SQL, compile-time checked) | 0.9.0 | 36.9M/90d vs diesel 6.9M vs sea-orm 4.18M. MAS + RustDesk ship sqlx |
| e | JWT | **jsonwebtoken 11** with an explicit crypto backend feature | 11.1.0 | v11 requires selecting `aws_lc_rs` or `rust_crypto`; implicit features were removed |
| e | Password hash | **argon2** (Argon2id) | 0.6.0 | continuwuity pins `argon2 0.6.0`; RustCrypto, 19.1M/90d |
| + | Fan-out / presence | **in-process `tokio::sync::broadcast` + `DashMap`**; add Redis only at 2+ app nodes | - | Single-node deploy: a network hop to Redis is pure latency for no benefit |
| + | Voice / SFU | **LiveKit** (external, Go) — do not build an SFU in Rust | livekit-api | Revolt uses LiveKit for voice |
| + | E2EE at rest/transport | vodka **vodozemac** patterns / `libsignal` port; not hand-rolled | vodozemac 0.11.0 | matrix-rust-sdk uses vodozemac for Olm/Megolm |

---

## (a) Framework: axum

**Recommendation: `axum = "0.8"`.**

### Evidence
- crates.io, last 90 days: **axum 115,833,612** vs **actix-web 10,464,799** vs warp 5,239,343 vs poem 704,453. Lifetime: 471.4M vs 81.3M.
- Version currency: axum **0.8.9**, actix-web **4.15.0**, warp 0.4.3, poem 3.1.12.
- Production Rust projects on axum (all read from their manifests):
  - **Conduit** (Matrix homeserver): `axum 0.8`, `axum-extra 0.10`, `axum-server 0.7` (tls-rustls), `tower 0.5`, `tower-http 0.6`.
  - **continuwuity** (conduwuit continuation, 1,064 stars): `axum 0.8.8`, `axum-extra 0.12.0`, `axum-server 0.8.0`, `tower 0.5.2`, `tower-http 0.7.0`.
  - **tuwunel** (official successor, 2,529 stars): `tokio 1.53`, `tower-http 0.6`, `tokio-metrics 0.5`.
  - **Revolt** (`revoltchat/backend`): currently carries BOTH `rocket 0.5.1` and `axum 0.7.5` behind `rocket-impl` / `axum-impl` cargo features — i.e. a live Rocket -> axum migration.
  - **matrix-authentication-service** (Matrix's official auth service): `axum 0.7.5`, `axum-extra 0.9.3`.
  - **RustDesk server**: `axum 0.5` (old pin, but axum).
- The one large counter-example: **Lemmy** (14,597 stars) is `actix-web 4.13.0` + `tracing-actix-web`. Actix is not dead; it has a large installed base.

### Why axum for this project
- Tower middleware is shared with the rest of the Tokio ecosystem, so one `TraceLayer`, rate-limit layer and timeout layer covers HTTP and the WS upgrade path.
- `State` extractor is compile-time checked: a missing `AppState` fails the build, not the first request.
- axum's `ws` feature is the same tokio-tungstenite engine you would reach for manually, so choosing axum costs you no WebSocket performance.

### Caveats
- axum is **0.8, not 1.0**. 0.6 -> 0.7 -> 0.8 each shipped breaking changes and migration guides. Pin exactly and upgrade deliberately.
- Actix retains roughly a 10-15% raw-RPS edge and better p99 determinism (thread-per-core, no work stealing). This is a **synthetic** advantage: at chat message rates your latency is dominated by Postgres round-trips, not the router. Blog-sourced throughput numbers (10-15%) should be treated as indicative, not measured here.
- Claims that "Discord/Fly.io run axum" appear only in SEO blog posts, not in any primary source I could verify. Do not repeat them as fact.

---

## (b) WebSocket: axum `ws` (which *is* tokio-tungstenite)

**Recommendation: enable axum's `ws` feature. Do not add tokio-tungstenite directly unless you need a WS client or non-HTTP framing.**

### Evidence — this is the key correction
axum's own manifest (`tokio-rs/axum`, `axum/Cargo.toml`):

    [features]
    ws = ["dep:tokio-tungstenite"]
    ...
    tokio-tungstenite = { version = "0.29.0", optional = true }

So the premise "raw tokio-tungstenite vs axum's built-in ws extract" is a **false dichotomy**: axum's `WebSocketUpgrade` extracts from the HTTP upgrade and then hands you a tungstenite stream. axum's own `examples/chat` depends on exactly:

    axum = { features = ["ws"] }
    futures-util = { version = "0.3", features = ["sink", "std"] }

### Library landscape
| Crate | Version | 90d downloads | Spec compliance | Notes |
|-------|---------|---------------|-----------------|-------|
| tokio-tungstenite | 0.30.0 | 71,995,005 | non-strict | The default. What axum wraps. Highest maintenance + mindshare |
| fastwebsockets | 0.10.0 | 1,339,433 | non-strict | Deno's engine. Designed for Deno's **single-threaded** runtime |
| tokio-websockets | 0.13.3 | 6,505,040 | **strict** (passes full Autobahn) | tokio-util based, minimal deps, SIMD masking/UTF-8 |

### fastwebsockets — do not use it for this
- Issue **#42 "Library is not thread safe"** (CLOSED): the crate held a process-global `static mut RECV_BUF` and did `unsafe impl Send for SharedRecv {}`. Multiple sockets could alias the same mutable buffer across `.await` points — UB, and sockets could read each other's frames. Fixed only in PR #63 by moving to a per-connection `BytesMut`.
- The tokio-websockets benchmark README literally annotates it as "*unsound and not thread-safe*, non-strict spec compliance".
- Deno's own integration PR (#34314) measured the fast path: **+12.5% only at 200 connections x 16 KB payloads**, and **0.960x (slower) at 100 connections x 20-byte payloads**. Chat is the small-payload case — the wrong end of that curve.
- It *was* built for Deno's single-threaded runtime. Your axum server is multi-threaded.

### Recommendation
Use axum `ws`. Reach for raw `tokio-tungstenite` only for outbound WS clients (e.g. your WebRTC signaling peer or a bridge). Consider `tokio-websockets` only if you specifically need strict Autobahn conformance for compliance reasons.

**Also**: if your Vue front-end would rather speak Socket.IO semantics, `socketioxide` (0.18.7, 314k/90d) is the axum-native Socket.IO server. It is ~230x smaller than the axum+tungstenite path in downloads and adds a protocol you must then version — I would not take it.

### Per-connection memory (matters at 2 GB)
tokio-tungstenite's `WebSocketConfig::default()` sets `max_message_size = 64 MiB` and `max_frame_size = 16 MiB`. On a 2 GB box that is a per-connection memory/DoS ceiling you will never intend. Set both low for chat (e.g. 1 MiB message / 64 KiB frame) via axum's `WebSocketUpgrade::max_message_size` / `max_frame_size`.
Use a **bounded** `mpsc` per connection for outbound writes (capacity ~16-32) and drop/close on full. An unbounded channel turns one slow client into an OOM.

---

## (c) Serialization: JSON first

**Recommendation: `serde_json` over text frames, wrapped in a small versioned envelope. Keep a binary codec behind a size threshold only if profiling demands it.**

### Evidence
| Crate | Version | 90d downloads | Real usage |
|-------|---------|---------------|-----------|
| serde_json | 1.0.151 | 312,097,835 | universal |
| prost (protobuf) | 0.14.4 | 129,592,073 | gRPC-driven |
| rmp-serde (MessagePack) | 1.3.1 | 25,414,900 | **matrix-rust-sdk** pins it |
| minicbor (CBOR) | 2.3.0 | 5,479,448 | **continuwuity** (on-disk record keys) |

- **Osmium** (a production Rust chat platform, 2026) chose **Protobuf** — but explicitly for *actor-to-actor* serialization between nodes, "as it has good ecosystem support and offers nice backwards compatibility guarantees". That is a server-to-server link, not the browser wire.
- Matrix's client SDK carries `rmp-serde` for local/opaque caching, not for the Matrix HTTP wire (which is JSON by spec).

### Why JSON for the client wire
- A chat message is mostly UTF-8 text. MessagePack stores *the same bytes* with a shorter length prefix — near-zero saving. The 30-40% wins quoted in benchmarks come from many short keys + small integers (cursors, telemetry), not chat bodies.
- JSON is a **native** parse on the client; a JS MessagePack decoder is 2-4x slower per byte. For small messages you can lose client time while saving bytes.
- Binary frames are opaque in Chrome DevTools. Teams revert to JSON not for performance but because an incident took three hours instead of twenty minutes.

### Do this instead of switching codecs
1. Define a stable envelope from day one: `{ v, t, s, ts, d }` (protocol version, message type, per-connection sequence, timestamp, opaque payload). Retrofitting a version field after deploy breaks every client.
2. Shorten key names (helps JSON too, costs nothing).
3. Leave **permessage-deflate off**. It costs roughly 300 KB of zlib state *per socket* and saves almost nothing on a 200-byte message. At 20k sockets that is ~6 GB of RAM. On 2 GB this is not a close call.
4. If a payload class ever gets large and repetitive, add a binary frame for that class only, negotiated by WS subprotocol so old clients keep working.

### When to use protobuf
If you later shard to 2+ nodes, use protobuf (or CBOR) for the **internal** fan-out/NATS/Redis link, where schema evolution is enforced and there is no browser to debug. That is the Osmium pattern.

---

## (d) PostgreSQL: sqlx

**Recommendation: `sqlx 0.9` with `features = ["runtime-tokio-rustls", "postgres", "chrono", "uuid", "json", "migrate"]`.**

### Evidence
| Crate | Version | 90d downloads | Model |
|-------|---------|---------------|-------|
| sqlx | 0.9.0 | 36,880,405 | async, raw SQL, compile-time checked via macros |
| diesel | 2.3.13 | 6,911,290 | sync query builder DSL; async needs `diesel-async` |
| sea-orm | 2.0.3 | 4,182,822 | async ActiveRecord built on top of sqlx |
| tokio-postgres | 0.7.18 | 16,919,935 | the underlying pure-Rust driver (sqlx has its own, not this) |

sqlx leads diesel **5.3x** and sea-orm **8.8x** on recent downloads.

Real deployments read from manifests:
- **matrix-authentication-service**: `sqlx` with `postgres` + features `with-uuid`, `with-chrono`, `postgres-array`. A production auth service, axum + sqlx + Postgres.
- **RustDesk server**: `sqlx 0.6` with `runtime-tokio-rustls`, `sqlite`, `macros`, `chrono`, `json`.
- **Lemmy**: `diesel 2.3.7` + **`diesel-async 0.8.0`** + `diesel_migrations 2.3.1` + `tokio-postgres 0.7.16`. Note it does *not* use sync diesel in the request path.
- The Matrix homeservers (Conduit, tuwunel, continuwuity) use **neither** — they are `rust-rocksdb` / `rusqlite`, because Matrix needs a custom key-value record store, not a relational schema. Not a precedent for your IM domain, which is naturally relational.

### Why sqlx here
- Async-native: a `tokio` task per WS connection can `await` a query without `spawn_blocking`.
- You keep full SQL: recursive CTEs for group membership, `ON CONFLICT` upserts for read receipts, covering indexes for message pagination.
- `sqlx::query!` validates column names, types and nullability against the live schema at build time — the single biggest class of runtime bug it removes.

### Caveats
- `sqlx::query!` needs `DATABASE_URL` reachable at **compile time**. This breaks `docker build` and CI unless you commit an offline cache (`cargo sqlx prepare` -> `.sqlx/`) and build with `SQLX_OFFLINE=true`. If that is unacceptable, use the non-macro `sqlx::query(...)` API — runtime-checked but no build-time DB.
- sea-orm wraps sqlx and costs ~10-20% runtime overhead plus an abstraction you may fight on complex queries. Consider it only if the team wants ActiveRecord ergonomics.
- diesel's sync model is genuinely excellent for batch/worker code; mixing it into an axum handler needs `diesel-async`. Do not mix two DB layers in one workspace.
- **Pool sizing on 2 GB**: set `PgPool` `max_connections` to 5-10. Postgres itself holds ~5-10 MB per connection plus `max_connections=100` default; a 10-connection pool is more than enough for this workload and keeps the box alive.

---

## (e) Auth

### JWT — `jsonwebtoken 11`
**Recommendation: `jsonwebtoken = { version = "11", features = ["aws_lc_rs"] }`.**

- Version **11.1.0**; 189,620,391 lifetime / 48,175,769 in 90 days. Maintained by Keats (Vincent Prouillet), 2059 stars, last push 2026-05-29.
- The v11 release (2026-07-24) is a large breaking change. Relevant breaks:
  - **Implicit features removed** — you must now choose a crypto backend: `aws_lc_rs` or `rust_crypto` (exactly one), or supply your own `CryptoProvider`.
  - `Validation.insecure_disable_signature_validation` removed -> use `dangerous::insecure_decode`.
  - `Algorithm`, `KeyAlgorithm`, `EllipticCurve`, `ThumbprintHash` are now `non_exhaustive`.
  - `Jwk.thumbprint` now returns `Result`.
  - `EncodingKey.inner` -> `as_bytes`; `try_get_hmac_secret` removed; `DecodingKey` -> `try_get_as_bytes`.
- Pick `aws_lc_rs`: you will already have `aws-lc-rs 1.18.1` (82.9M/90d) in the tree via `rustls`, so this adds no new native dependency.
- **Design note**: a stateless JWT cannot be revoked. For an IM app use a short access-token TTL (~10-15 min) plus an opaque, hashed, DB-backed refresh token, and maintain a jti denylist for logout/device-revoke. WS connections should be authenticated on the upgrade request and re-checked on the access token's expiry, not trusted for the life of the socket.

### Password hashing — `argon2 0.6`
**Recommendation: `argon2 = { version = "0.6", default-features = false, features = ["alloc", "getrandom"] }`.**

- Version **0.6.0**; 53,203,717 lifetime / 19,128,473 in 90 days. RustCrypto.
- **continuwuity** (a production Matrix homeserver) pins exactly this: `argon2 0.6.0`, `features = ["alloc", "getrandom"]`, `default-features = false`.
- Underlying trait crate `password-hash 0.6.1` (120.8M / 32.3M) is re-exported by `argon2`; you normally do not depend on it directly.
- `bcrypt 0.19.3` (18.2M / 5.2M) still exists but Argon2id is the modern default.

**2 GB caveat**: Argon2id is memory-hard *by design*. OWASP's baseline (m=19456 KiB ≈ 19 MiB, t=2, p=1) means ~19 MB per in-flight hash. 50 simultaneous logins would try to allocate ~1 GB. On this box: keep m=19456, run hashing on `spawn_blocking`, and gate concurrency with a `Semaphore` (e.g. 4 permits). Do not raise to 64 MiB on a 2 GB host without measuring.

### Optional: sessions instead of JWT
`tower-sessions 0.15.0` (3.6M / 834k) gives cookie-backed server-side sessions if you would rather not ship JWTs at all. For a WS-heavy app, opaque session tokens with a revocation row are simpler to reason about than a JWT denylist.

---

## Supporting infrastructure

| Need | Recommendation | Why |
|------|----------------|-----|
| Presence / typing / fan-out | `tokio::sync::broadcast` + `DashMap` (7.0.0-rc2) in-process | One app instance: Redis is a network hop for zero benefit. Osmium explicitly rejected the "Redis pub/sub" approach in favour of keeping state in-process |
| Cross-node fan-out (later) | `redis 1.7.0` (25.4M/90d) or `fred 10.1.0` (1.9M/90d) pub/sub | Only when you run 2+ app containers. `redis` crate is the mindshare default |
| WebRTC / group calls | **LiveKit** (`livekit-api`, external Go SFU) | Revolt ships LiveKit for voice. Do not write an SFU |
| E2EE secret chats | `vodozemac 0.11.0` (what matrix-rust-sdk uses) or a `libsignal` port | Never hand-roll Double Ratchet / X3DH |
| Reverse proxy / TLS | Caddy or nginx in Compose; `rustls` in-app if you prefer | `axum-server 0.8.0` (10.2M/90d) has a `tls-rustls` feature if you terminate in-process |
| Rate limiting | `governor 0.10.4` (continuwuity uses it) | Login + message-send limits |

---

## 2 vCPU / 2 GB operational notes

1. **Builds will OOM before the app ever runs.** A release build of axum + sqlx + rustls + argon2 with LTO needs roughly 2-4 GB RSS. On a 2 GB box `cargo build --release` will likely be killed. Mitigations, in order of preference:
   - Build a **multi-stage Docker image on CI or your workstation** and copy the binary in; the server only runs it.
   - If you must build on the box: add 2-4 GB swap, set `CARGO_BUILD_JOBS=1`, `CARGO_PROFILE_RELEASE_LTO=thin`, `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`.
   - Note continuwuity ships `[profile.release] lto = "thin"`, not `"fat"`, for exactly this reason.
2. **Runtime footprint** is fine: Postgres ~50-100 MB, the Rust process ~60-100 MB, optional Redis ~10-30 MB. Remember Compose has its own overhead; watch `docker stats`.
3. **Do not enable permessage-deflate** (~300 KB zlib state per socket).
4. **Cap WS frame/message sizes** (tokio-tungstenite defaults are 16 MiB / 64 MiB — far too generous).
5. **Bound your outbound channels** per connection; drop or close on full.
6. **Tune the runtime**: `worker_threads = 2` on 2 vCPU. Building the runtime explicitly beats `#[tokio::main]` defaults for a long-lived-connection workload.
7. **File descriptors**: set `LimitNOFILE=65536` in the systemd unit / Compose `ulimits`. Memory is not your WS ceiling on this box; the fd limit and one network core are.
8. **Argon2 concurrency**: semaphore-bound, `spawn_blocking`.
9. **DB pool**: 5-10 connections max.
10. Realistically, this box is sized for a small user base. Connection *count* is cheap (idle WS ~7-15 KB); the constraints are build RAM, Postgres, and single-core throughput.

---

## Suggested workspace

    # Cargo.toml (workspace)
    [workspace]
    members = ["crates/api", "crates/realtime", "crates/domain", "crates/store", "crates/auth"]
    resolver = "3"
    edition  = "2024"   # matches Lemmy 1.0.0-beta.2 and continuwuity 26.8.1

    [workspace.dependencies]
    axum            = { version = "0.8", features = ["ws", "json", "http1", "http2"] }
    axum-extra      = { version = "0.12", features = ["typed-header", "cookie"] }
    tokio           = { version = "1.53", features = ["rt-multi-thread", "macros", "sync", "time", "net", "signal"] }
    tower           = { version = "0.5", features = ["util"] }
    tower-http      = { version = "0.7", features = ["trace", "cors", "timeout", "catch-panic", "limit"] }
    serde           = { version = "1", features = ["derive"] }
    serde_json      = "1"
    sqlx            = { version = "0.9", default-features = false,
                        features = ["runtime-tokio-rustls", "postgres", "chrono", "uuid", "json", "migrate"] }
    jsonwebtoken    = { version = "11", features = ["aws_lc_rs"] }
    argon2          = { version = "0.6", default-features = false, features = ["alloc", "getrandom"] }
    uuid            = { version = "1", features = ["v7", "serde"] }   # v7 = time-ordered ids
    thiserror       = "2"
    anyhow          = "1"
    tracing         = "0.1"
    tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
    dashmap         = "7.0.0-rc2"
    futures-util    = { version = "0.3", features = ["sink", "std"] }
    vodozemac       = "0.11"    # E2EE secret chats

Split `api` (HTTP) from `realtime` (WS gateway) only if you intend to scale them separately;
on one 2 GB box a single `axum` process serving both on the same listener is simpler and cheaper.

---

## Corrections to the brief's candidate list

Two premises in the request do not hold, and should not shape the design:

- **BitChat is not a Rust project.** `permissionlesstech/bitchat` (36,240 stars) is **Swift**; `bitchat-android` (7,630) is **Kotlin**. The Rust ports are tiny (`ShilohEye/bitchat-terminal` 511 stars) and are Bluetooth-mesh TUIs with no server, no database and no web framework. Zero signal for a server-side stack.
- **Anytype sync is not Rust.** `any-sync` (the Anytype sync engine) is **Go**. There is no Rust Anytype sync service.
- **Zulip is Python/Django** end to end; it has no Rust chat backend to learn from.
- Useful substitutes I substituted in: **Revolt** (Rust, axum+Rocket+LiveKit), the **Matrix** family (Conduit / tuwunel / continuwuity / matrix-authentication-service), **Osmium**, **Lemmy** (actix), **RustDesk server**.

---

## Confidence and caveats

**High confidence** (read directly from project manifests or the crates.io API):
all version numbers, all download figures, the axum `ws -> tokio-tungstenite` dependency, the
project->crate mappings, the jsonwebtoken v11 changelog, fastwebsockets issue #42, and Deno PR #34314's
benchmark table.

**Medium confidence** (official docs / maintainer statements): tokio-tungstenite's default
`max_message_size`/`max_frame_size` values; Argon2 memory behaviour.

**Low confidence / blog-sourced only** — treat as directional, not measured: Actix's "10-15% throughput"
edge, "60-90 MB idle" memory figures, and "1M concurrent WebSocket connections" claims. I found no
primary source reproducing the 1M number; the tokio-websockets benchmark repo is a more honest
comparison because it publishes its methodology. **If any of these numbers will drive a decision, measure
them on your own hardware** — none of them are close enough to matter at chat scale, but you should
confirm that yourself rather than take a blog's word.

**Explicitly rejected advice**: "choose Actix-web because it's faster" (irrelevant here),
"use fastwebsockets for speed" (unsound history + wrong payload profile + single-threaded design),
"MessagePack everywhere" (near-zero gain on chat text, worse DX), "enable permessage-deflate"
(catastrophic per-socket RAM on 2 GB), and "JWT is enough auth" (no revocation).

---

## Sources

- crates.io API (live, 2026-09-16): axum, actix-web, sqlx, diesel, sea-orm, tokio-tungstenite,
  fastwebsockets, tokio-websockets, jsonwebtoken, argon2, tower-http, tokio, serde_json, prost,
  rmp-serde, minicbor, axum-server, tokio-postgres, socketioxide, redis, fred, dashmap, aws-lc-rs,
  password-hash, bcrypt, tower-sessions  — https://crates.io/api/v1/crates/<name>
- axum `ws` feature + chat example - https://github.com/tokio-rs/axum/blob/3f46d25e9274d485a99905d3734d37828112bc31/axum/Cargo.toml
  https://github.com/tokio-rs/axum/blob/3f46d25e9274d485a99905d3734d37828112bc31/examples/chat/Cargo.toml
- Conduit - https://github.com/timokoesters/conduit/blob/08366bf28b6254867da870dc93c72720359b5189/Cargo.toml
- continuwuity - https://github.com/continuwuity/continuwuity/blob/ffcf152016c2d73af0e3218dbe34c4352c69fa33/Cargo.toml
- tuwunel - https://github.com/matrix-construct/tuwunel/blob/0c2598d41c22a06309d7da71bd16d5042de75611/Cargo.toml
- Revolt backend - https://github.com/revoltchat/backend/blob/d57cf57df0f78ca337d76934b1c6ce003e3239d0/Cargo.toml
- matrix-authentication-service - https://github.com/matrix-org/matrix-authentication-service/blob/162119dd6651370fea5a74fdd5a7b6a2d26f1b20/Cargo.toml
- matrix-rust-sdk - https://github.com/matrix-org/matrix-rust-sdk/blob/d454cb2cba7a5a91c52da1ceaffa9ef36f7a7d87/Cargo.toml
- RustDesk server - https://github.com/rustdesk/rustdesk-server/blob/a7736be5e40f85bfc141120dce587e836e5d4b80/Cargo.toml
- Lemmy (actix + diesel-async counter-example) - https://github.com/LemmyNet/lemmy/blob/main/Cargo.toml
- fastwebsockets issue #42 "Library is not thread safe" - https://github.com/denoland/fastwebsockets/issues/42
- fastwebsockets - https://github.com/denoland/fastwebsockets/blob/ed8eea7405fb695ce5c990a6448af5fa6a38c198/README.md
- Deno fastwebsockets integration PR #34314 (benchmark table) - https://github.com/denoland/deno/pull/34314
- tokio-websockets (strict conformance + competitor comparison) - https://github.com/Gelbpunkt/tokio-websockets/blob/aa19342cd86ded89fa7d902af001526d4b004dbf/README.md
  https://github.com/Gelbpunkt/tokio-websockets/blob/aa19342cd86ded89fa7d902af001526d4b004dbf/benches/README.md
- jsonwebtoken v11 changelog - https://github.com/Keats/jsonwebtoken/blob/master/CHANGELOG.md  (v11.0.0, 2026-07-24)
- Osmium (production Rust chat: actor model + Protobuf) - https://osmium.chat/blog/how-we-built-osmium-for-scale/
