# jiuyue-contract

The **single source of truth** for the jiuyue WebSocket wire contract. The Rust
types in `src/` are authoritative; the frontend imports the TypeScript generated
from them and never re-declares them.

## What lives here

| Type               | Role                                                       |
| ------------------ | ---------------------------------------------------------- |
| `PROTOCOL_VERSION` | Wire version carried on every envelope                     |
| `ServerEnvelope`   | `{ v, s, ts, e }` — sequence + server time + event         |
| `ClientEnvelope`   | `{ v, e }` — event only, no client-owned ordering          |
| `ServerEvent`      | Adjacently tagged (`{"t": ..., "d": ...}`), `Ping` for now |
| `ClientEvent`      | Adjacently tagged (`{"t": ..., "d": ...}`), `Ping` for now |
| `Ping`             | Heartbeat payload: connection sequence + originator time   |

New event variants are additive: clients ignore a `t` they do not recognise, so
adding one never breaks a deployed client and never needs a version bump.

## Why ts-rs

The contract must be generated, never hand-written. Of the available mechanisms,
`ts-rs` was chosen because:

- **It derives from the same types that (de)serialise at runtime.** The TS is a
  projection of the very `Serialize`/`Deserialize` impls the server uses, so
  there is no second schema to keep in sync.
- **`serde-compat` matches the wire exactly.** The `tag`/`content`/`rename`
  attributes that shape the JSON also shape the TypeScript, so
  `{"t": "Ping", "d": ...}` on the wire is `{ "t": "Ping", "d": Ping }` in TS.
- **No extra toolchain.** It is a normal Rust dependency — no Node build step or
  separate CLI binary — which matters on a 2 vCPU / 2 GB single-node box.
- **Deterministic output.** The optional `format` feature (dprint) is disabled;
  export is byte-for-byte reproducible, which is what makes the drift guard work.

Alternatives considered: `typeshare` (extra CLI + separate token model, so the
generated types are not the serialising types) and `schemars` → JSON Schema →
TS (an indirect, lossy hop). Both add a second source of truth; `ts-rs` does not.

## Regenerating

```powershell
powershell -File scripts/gen-contract.ps1
```

The script clears `frontend/src/generated/`, runs the `#[ts(export)]` tests in
this crate with `TS_RS_EXPORT_DIR` pointed at that directory, and leaves the
files ready to commit. The generated output **is committed** — it is the
frontend's only source for these types.

`TS_RS_EXPORT_DIR` is also set in the repository's `.cargo/config.toml`, so a
plain `cargo test` refreshes the same directory instead of dropping a stray
`bindings/` folder. The script's explicit environment always wins.

## Drift guard

CI regenerates and then runs `git diff --exit-code -- frontend/src/generated`. If
the committed types are stale the build fails. Locally the same check is:

```powershell
powershell -File scripts/gen-contract.ps1
git diff --exit-code -- frontend/src/generated
```
