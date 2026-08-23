<!-- project: jiuyue -->
<!-- extracted from librarian transcript tool_0302aa4b4001uzsUyOr4ICqzM9, 2026-08-24 -->

# Secret-Chat Crypto Foundation — Landscape Report (August 2026)

## 1. libsignal (Signal official)

**Status**: Very active — last push 2026-08-14, ~6k stars, monorepo version `0.101.0`.

**Crate structure** ([workspace Cargo.toml @ `b056faa`](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/Cargo.toml#L3-L41)): the protocol implementation is **`libsignal-protocol` at `rust/protocol`**, alongside `libsignal-core`, `signal-crypto`, `libsignal-net`, `zkgroup`, etc., plus bridge layers for JNI (Android), FFI (Swift), and Node (`rust/bridge/node`). The README confirms this layout ([README](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/README.md#L3-L10)).

**License: AGPL-3.0-only** — declared workspace-wide ([Cargo.toml L47](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/Cargo.toml#L44-L48)) and restated in the README ("Licensed under the GNU AGPLv3", copyright Signal Messenger LLC) ([README L275-L279](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/README.md#L275-L279)).

**Usability outside Signal — explicitly unsupported**:

> "This repository is used by the Signal client apps... as well as server-side. **Use outside of Signal is unsupported.** ... All APIs and implementations are subject to change without notice" ([README L19-L25](https://github.com/signalapp/libsignal/blob/b056faa6dd02961cff24064c54c089c52e1a0753/README.md#L19-L25))

Distribution channels confirm it's not packaged for third parties: **not published to crates.io** (the crates.io crate named `libsignal-protocol` is an unrelated abandoned 2019 project by Michael-F-Bryan); npm package [`@signalapp/libsignal-client`](https://www.npmjs.com/package/@signalapp/libsignal-client) (v0.101.0, 2026-08-14) ships **native Node addons** (neon), i.e. Electron-only — it cannot load in a Tauri webview, which has no Node runtime.

**AGPL implications for JiuYue**: AGPL covers _network_ use, and GPL-style linking means distributing your Tauri client binary containing libsignal code obligates you to release the **entire client source** under an AGPL-compatible license. Your axum backend never needs to link it (relay-only), so the exposure is client-side only. Using libsignal is viable **only if you're willing to open-source the whole app under AGPLv3** — a business decision, not a technical one.

## 2. Pure-Rust alternatives

| Project                                                                                                                   | License                                                                                                                                                                                                                          | Status                                                                                                | Verdict                                                                           |
| ------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| [wireapp/proteus](https://github.com/wireapp/proteus) (Wire's Axolotl/Double Ratchet, no header keys, prekey-based async) | **GPL-3.0-only** — verified in [LICENSE](https://github.com/wireapp/proteus/blob/bb759d762bfde376fa5a8a08b1d1153a345ab28a/LICENSE) and root `Cargo.toml` (`license = "GPL-3.0-only"`; note the MPL badge in its README is stale) | Maintained (v3.0.1, June 2026; used in Wire production for 1:1)                                       | Purpose-built for exactly your use case, but GPL kills closed-source distribution |
| [`double-ratchet` crate](https://crates.io/crates/double-ratchet)                                                         | MIT-ish                                                                                                                                                                                                                          | v0.1.0, March 2019, ~4k downloads                                                                     | Dead toy; unaudited                                                               |
| `x3dh` / other standalone DR/X3DH crates                                                                                  | —                                                                                                                                                                                                                                | Not found on crates.io                                                                                | Nothing credible exists                                                           |
| OMEMO ([XEP-0384](https://xmpp.org/extensions/xep-0384.html))                                                             | n/a                                                                                                                                                                                                                              | XMPP-spec-bound; key distribution via PEP pubsub nodes, historically mandates libsignal (GPL lineage) | Not reusable outside XMPP                                                         |

**Conclusion**: there is **no maintained, permissively-licensed, purpose-built pure-Rust Double Ratchet crate**. The realistic non-GPL option in Rust is Matrix's **vodozemac** (below).

## 3. MLS / OpenMLS

[OpenMLS](https://github.com/openmls/openmls) is **MIT-licensed** and very active (stable 0.8.1; 0.9.0-rc.3 on 2026-08-20). It implements [RFC 9420](https://www.rfc-editor.org/rfc/rfc9420.html) (MLS).

**Fit for Telegram-style secret chats: wrong tool.** MLS is a TreeKEM-based _group_ key agreement with epochs, key schedules, and an explicit Delivery Service + Authentication Service split. For a device-bound, 1v1-only mode you'd inherit all that machinery for a two-member tree, plus a fundamentally different server contract than X3DH prekey distribution. (MLS-for-DMs is a real direction — IETF MIMI models DMs as 2-member groups — but it pays off for multi-device/multi-member sync, which you explicitly excluded.)

## 4. JS/WASM side

- **Official TS bindings** = `@signalapp/libsignal-client`: native Node addon, **AGPL-3.0-only** → unusable in Tauri webview (no Node runtime) and license-hostile anyway.
- **[@privacyresearch/libsignal-protocol-typescript](https://www.npmjs.com/package/@privacyresearch/libsignal-protocol-typescript)**: license field reads **GPL-3.0-only** (npm registry metadata), last release `0.0.16` on **2023-05-06** → effectively abandoned _and_ copyleft. Disqualified twice over.
- Building libsignal to WASM yourself doesn't change the AGPL status.
- The only healthy JS/WASM artifact in this space is vodozemac's official wasm binding ([matrix-org/vodozemac-bindings `javascript/`](https://github.com/matrix-org/vodozemac-bindings/blob/main/javascript/src/account.rs), Apache-2.0).

## 5. Prekey / X3DH server endpoint conventions

Canonical shape from Signal-Server's [`KeysController.java`](https://github.com/signalapp/Signal-Server/blob/main/service/src/main/java/org/whispersystems/textsecuregcm/controllers/KeysController.java) (`@Path("/v2/keys")`):

```java
@PUT   /v2/keys                              // upload: SetKeysRequest { preKeys[], signedPreKey, pqPreKeys[] }
@GET   /v2/keys/{identifier}/{device_id}     // fetch bundle: identityKey + registrationId + signedPreKey + one-time prekey
@GET   /v2/keys                              // count of remaining one-time prekeys (replenish trigger)
@POST  /v2/keys/check                        // digest consistency check (client vs server)
```

Key behaviors worth copying: signed prekeys are **verified against the identity key server-side** (`checkSignedPreKeySignatures` → 422 on bad signature); fetching **consumes** a one-time prekey (`takeDevicePreKeys`); fetches are rate-limited. Signal additionally carries post-quantum Kyber prekeys since 2023 — optional for you. Comparable shapes exist elsewhere: Matrix `/keys/upload|query|claim` ([spec](https://spec.matrix.org/latest/client-server-api/#end-to-end-encryption)) and OMEMO's PEP bundle nodes ([XEP-0384](https://xmpp.org/extensions/xep-0384.html)). A minimal axum implementation is: `PUT /devices/{id}/prekeys` (signed prekey + batch of OTPs), `GET /users/{id}/prekeys/{device}` (returns identity+signed+one OTP, deletes the OTP), `GET /devices/{id}/prekeys/count`.

---

## Verdict: **Rust-core E2EE engine using vodozemac (Apache-2.0), exposed via Tauri commands**

[vodozemac](https://github.com/matrix-org/vodozemac) — Apache-2.0, v0.10.0 (April 2026), 1M+ crates.io downloads, actively maintained by Element.

Reasoning:

1. **License-safe**: Apache-2.0 permits proprietary integration with only notice-preservation obligations. Every alternative fails one constraint: libsignal/proteus = AGPL/GPL (force open-sourcing the client), OpenMLS = fine license but wrong protocol, everything else = dead or unaudited.
2. **Audited & proven**: independently audited by Least Authority (report [PDF](https://matrix.org/media/Least%20Authority%20-%20Matrix%20vodozemac%20Final%20Audit%20Report.pdf), [announcement](https://matrix.org/blog/2022/05/16/independent-public-audit-of-vodozemac-a-native-rust-reference-implementation-of-matrix-end-to-end-encryption/), 8 of 10 findings fixed during audit); it's the reference E2EE library of matrix-rust-sdk in production.
3. **Protocol fit is exact**: an Olm `Session` _is_ a Double Ratchet (`pub struct Session { sending_ratchet: DoubleRatchet, receiving_chains: ChainStore, .. }` — [source](https://github.com/matrix-org/vodozemac/blob/main/src/olm/session/mod.rs#L155-L170)), established asynchronously from identity key + one-time key (`Account::create_outbound_session(identity_key, one_time_key)` — [source](https://github.com/matrix-org/vodozemac/blob/main/src/olm/account/mod.rs#L188-L196)), with `generate_one_time_keys` / `mark_keys_as_published` / encrypted `pickle` persistence. That maps 1:1 onto Telegram-secret-chat semantics and a Signal-shaped prekey server. Ignore Megolm entirely.
4. **Minimal self-implemented crypto**: you write zero ratchet math. Your own code is session lifecycle glue, safety-number verification (fingerprint both identity keys), and the thin Tauri command layer (`init_identity`, `upload_prekeys`, `establish_session`, `encrypt`, `decrypt`) — keeping keys out of the webview JS context entirely.
5. **JS/WASM layer: don't put crypto there.** The only viable JS options are dead (privacyresearch) or AGPL native addons; the Rust core also shrinks your XSS attack surface.

**Caveats**: vodozemac's API is Matrix-flavored and docs are sparse relative to libsignal — budget time for a wrapper module; and if JiuYue ever decides to open-source the whole client under AGPL, libsignal becomes the superior technical choice (canonical protocol, PQ-X3DH included). That's the fork in the road: **closed-source commercial → vodozemac; fully-open AGPL client → libsignal.**
