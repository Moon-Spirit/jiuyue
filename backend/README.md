# jiuyue backend

Rust + Axum backend for jiuyue. This is the repository skeleton: it serves a
health probe and nothing else. Database, auth, WebSocket and media work land in
later tickets.

## Layout

```
backend/
├── Cargo.toml                 # workspace root
└── crates/server/             # jiuyue-server — library + thin binary
    ├── src/lib.rs             # public API (config / state / routes / error)
    ├── src/main.rs            # binary: bind, serve, graceful shutdown
    ├── src/config.rs          # environment-backed Config
    ├── src/state.rs           # AppState (config + process start instant)
    ├── src/routes.rs          # router, /health handler, request logging
    ├── src/error.rs           # thiserror error type
    └── tests/health.rs        # in-process integration tests
```

The crate is a **library plus a thin binary** so tests can build the router
in-process — `tower::ServiceExt::oneshot` — without binding a socket.

## Requirements

- Rust 1.85+ (toolchain 1.98.1 on the dev machine; the workspace is edition 2024)
- On Windows, `cargo` lives in `%USERPROFILE%\.cargo\bin`

## Configuration

Read once at startup from the environment (a `.env` in the working directory is
loaded first when present). Real environment variables win over `.env`.

| Variable       | Default | Notes                                               |
| -------------- | ------- | --------------------------------------------------- |
| `PORT`         | `8080`  | TCP port to bind, all interfaces                    |
| `RUST_LOG`     | `info`  | `tracing-subscriber` filter directives              |
| `DATABASE_URL` | _unset_ | Accepted for later tickets; **not required to run** |

Copy `.env.example` if you need local overrides. A missing `DATABASE_URL` is not
an error.

## Run

From `backend/`:

```powershell
cargo run -p jiuyue-server
```

Startup prints the bound address:

```
INFO jiuyue_server: jiuyue-server listening bound=0.0.0.0:8080
```

Each request logs a single line with method, path, status and latency:

```
INFO jiuyue_server::routes: request method=GET path=/health status=200 latency_ms=0
```

Stop it with `Ctrl-C`; the server drains connections before exiting.

## Hit `/health`

`GET /health` returns `200` with:

```json
{ "status": "ok", "version": "0.1.0", "uptime_s": 12 }
```

`uptime_s` is whole seconds since process start.

PowerShell:

```powershell
Invoke-RestMethod -Uri http://127.0.0.1:8080/health -Method Get |
  ConvertTo-Json -Compress
```

curl:

```bash
curl -i http://127.0.0.1:8080/health
```

## Tests and checks

From `backend/`:

```powershell
cargo test                                   # unit + integration tests
cargo fmt --all --check                      # formatting
cargo clippy --all-targets -- -D warnings    # lints, warnings are errors
cargo build                                  # binary
```

Tests need no network and no port: the integration test builds the router and
calls it in-process.

## Constraints

- **No containers.** No Dockerfile, no docker-compose, no container runtime in
  local, CI or production (see `docs/adr/0010-no-containers.md`).
- **No `unwrap()` / `expect()` / `panic!()` in library code.** Fallible paths
  return `Result` with the `thiserror` type in `src/error.rs`; `unwrap()` is fine
  in tests.
- Do not suppress lints to get clippy green — fix the code.
- Types that cross the wire are `serde`-serialisable and honest about their
  shape, because the WS/REST contract is generated from Rust types later.
