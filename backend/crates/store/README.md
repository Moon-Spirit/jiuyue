# jiuyue-store

The persistence layer for the jiuyue backend: a bounded PostgreSQL connection
pool (`Store`) plus the schema's migrations, embedded into the binary.

The public API is intentionally small:

| Item                           | Purpose                                                           |
| ------------------------------ | ----------------------------------------------------------------- |
| `Store::connect(database_url)` | Build a bounded pool from a connection string                     |
| `Store::migrate(&self)`        | Apply every embedded migration that is not applied yet            |
| `Store::pool(&self)`           | Borrow the underlying `PgPool` for repositories and health checks |
| `StoreError`                   | Typed errors for URL parsing, connecting and migrating            |

```rust
use jiuyue_store::Store;

let store = Store::connect("postgres://jiuyue:jiuyue_dev_password@localhost:5432/jiuyue_dev").await?;
store.migrate().await?;
let pool = store.pool();
```

## Layout

```
backend/
├── migrations/                               # the schema, one file per change
│   └── 20260917120000_users.sql
└── crates/store/
    ├── src/                                  # Store + StoreError
    └── tests/store/                          # integration tests (real PostgreSQL)
```

Migration files live in `backend/migrations/`, **outside** the crate, because the
schema belongs to the product, not to a single crate. The crate embeds them with

```rust
static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");
```

`sqlx::migrate!` resolves its path relative to `CARGO_MANIFEST_DIR`, so from
`backend/crates/store` that is `backend/migrations`. Embedding means a deployed
server carries its own migrations and can migrate itself at boot on a machine
with no source tree.

## Migration workflow

### Add a migration

1. Create a file under `backend/migrations/` named
   `YYYYMMDDHHMMSS_<snake_case_name>.sql`, using the UTC timestamp of the change
   so files sort in application order, e.g.
   `20260917120000_users.sql`.
2. Write plain SQL. Each file runs **once**, inside its own transaction, and is
   then recorded in `_sqlx_migrations` — write the change as a forward-only step
   (`CREATE TABLE ...`, `ALTER TABLE ...`), not as a re-runnable script. Do not
   wrap the file in `BEGIN`/`COMMIT`; sqlx already does.
3. Run the tests (they migrate a fresh schema per test and will fail loudly if a
   migration does not apply cleanly).

### Run migrations

Migrations run from the application, at boot:

```rust
store.migrate().await?;
```

There is no separate `sqlx migrate` step in the deploy path: the binary owns its
schema. Running `migrate()` more than once is safe — applied migrations are
skipped — so calling it on every boot is the intended usage.

### Rules

- **Never edit or delete an applied migration.** sqlx stores a checksum and will
  refuse to run a changed file. Add a new migration instead.
- **One logical change per file**, so a failure is easy to attribute.
- **Timestamps default in the database** (`DEFAULT now()`, `timestamptz`), never
  from the application clock.

## Why runtime-checked `sqlx::query(...)`, not the compile-time macros

This crate uses the **runtime-checked** query API — `sqlx::query(...)` with
`.bind(...)` — and deliberately does **not** use `sqlx::query!` / `sqlx::query_as!`,
and keeps **no `.sqlx` offline cache** in the repository.

Reasons:

- The compile-time macros require either a live database at build time or a
  checked-in `.sqlx` cache. Both couple _building_ to schema state:
  - a live database makes `cargo build` fail on a machine that has no PostgreSQL,
    including the CI jobs and the release build that produce the server binary
    (builds happen in CI/local, never on the 2C2G production box — ADR-0010);
  - a `.sqlx` cache is a generated artifact that must be regenerated and committed
    on every query change, and it silently goes stale.
- `migrate!` is the one macro used here, and it only embeds files at compile time;
  it never contacts a database.

The cost is that query/schema mismatches surface at runtime instead of compile
time. That is covered by the integration tests, which run every query against a
real PostgreSQL — the project's rule is that the database is never mocked, because
the constraints and sequence behaviour _are_ the behaviour under test.

## Tests

The suite runs against a **real PostgreSQL** and never mocks it. Each test creates
its own uniquely named schema, points its connection at it with
`search_path`, and drops it afterwards, so tests run in parallel and can be
re-run repeatedly with no manual cleanup.

A database is configured through environment variables:

| Variable            | Meaning                                  |
| ------------------- | ---------------------------------------- |
| `TEST_DATABASE_URL` | Preferred; used by the integration tests |
| `DATABASE_URL`      | Fallback if `TEST_DATABASE_URL` is unset |

If neither is set the tests **panic with instructions** — a silently skipped
integration test would let the schema rot. Locally:

```powershell
$env:TEST_DATABASE_URL = 'postgres://jiuyue:jiuyue_dev_password@localhost:5432/jiuyue_test'
cargo test --manifest-path backend/Cargo.toml -p jiuyue-store
```

The schema is dropped by `DROP SCHEMA ... CASCADE`, so the test database also
needs `CREATE SCHEMA` privilege for the connecting role.

## Schema conventions

- **Identity is a ULID**, stored as `CHAR(26)`, generated by the application
  (time-sortable, globally unique — ADR-0003). `users.id` is the first.
- **Text is normalised before it is stored.** `username` and `email` are stored
  lowercase with a `CHECK` enforcing it, so `UNIQUE` is case-insensitive without
  `citext` or functional indexes and lookups stay plain equality.
- **Timestamps are `timestamptz` and defaulted by PostgreSQL.**
- **The database enforces what the application assumes**: format and length
  `CHECK`s live in the schema, not only in Rust.
- **Partition keys are reserved where they will be needed** — `messages` by
  `conversation_id` (ADR-0005). `users` is the global identity list and is not
  partitioned.
