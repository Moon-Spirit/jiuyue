# E2EE implementation path for a dual-mode chat app (Rust relay + Vue3/TS client)

> Status: research complete — 2026-09-16
> Question: which E2EE stack to build "secret chat" (1:1, double ratchet, single device, no roaming) on top of,
> with a Rust relay that never sees plaintext and a TS client (browser + Tauri desktop).
> Scope: vodozemac vs libsignal vs alternatives, Matrix libolm->vodozemac history, and the TS-client WASM story.

## TL;DR — recommended path

**Use `vodozemac` (Apache-2.0) as the ratchet core. Write your own thin `wasm-bindgen` wrapper for the browser. Run the
same crate natively inside the Tauri Rust core (no WASM on desktop). Do NOT use libsignal (AGPL-3.0 + no browser build).**

- Rust core: `vodozemac = "0.11"` (Apache-2.0, MSRV 1.89, audited, 1.2M+ crates.io downloads).
- Browser: `wasm-bindgen` + `wasm-pack`, target `wasm32-unknown-unknown`, enable the crate's first-class `wasm_js` feature.
- Desktop (Tauri): the same `crypto-core` crate compiled natively, exposed with `#[tauri::command]` — no WASM, keys never enter the webview heap.
- npm: publish your own wrapper (no official raw-vodozemac npm package exists; the official bindings repo is unmaintained).
- Protocol: Olm sessions (`Account` / `Session` / `PreKeyMessage`) + Ed25519-signed prekey bundles + `vodozemac::sas` for safety-number verification.
- Skip Megolm (group only) unless multi-party "secret chat" is later required.

---

## (a) vodozemac — maturity, maintenance, WASM, license, production

| Field | Value |
|---|---|
| Repo | https://github.com/matrix-org/vodozemac |
| License | **Apache-2.0** (permissive, patent grant, no copyleft) |
| Stars / commits | ~397 / ~804 |
| Last push | 2026-09-15 (active) |
| Latest release | **0.11.0 (2026-09-11)**; prior 0.10.0 (2026-04-13), 0.9.0 (2025-01-31), 0.8.x (2024) |
| crates.io | `vodozemac` 0.11.0 — 1,226,940 all-time downloads; owners `poljar`, `dkasak` |
| MSRV / edition | Rust 1.89 / edition 2024 |
| Audit | Least Authority audit (May 2022), **no significant findings** — https://matrix.org/media/Least%20Authority%20-%20Matrix%20vodozemac%20Final%20Audit%20Report.pdf |
| Docs | docs.rs/vodozemac (100% documented) |

**Capabilities** (from `Cargo.toml` + docs.rs):
- **Olm** (the double ratchet, 3DH key agreement) — `olm::Account`, `olm::Session`, `olm::PreKeyMessage`, `olm::OlmMessage`.
- **Megolm** (group ratchet) — not needed for 1:1.
- **SAS** (short authentication strings, ZRTP-inspired hash-commitment) — `sas::Sas`, `sas::EstablishedSas`, `emoji_indices()`, `to_digis()`.
- libolm pickle **read** + export (`libolm-compat`), modern serde pickles (`AccountPickle`, `SessionPickle`).
- HPKE X25519/ChaCha20-Poly1305 (new in 0.11), MSC4108 ECIES, Ed25519 signing, X25519/Curve25519 keys, fallback keys.

**Feature flags** (`Cargo.toml`):
```
default = ["libolm-compat", "precomputed-tables"]
wasm_js            = ["getrandom/wasm_js"]        # <- first-class WASM support
libolm-compat      = []                            # legacy pickle import
precomputed-tables = ["curve25519-dalek/precomputed-tables"]  # +40KB tables, faster KDF/signing
insecure-pk-encryption, experimental-session-config, low-level-api
```

**Hardening posture**: `unsafe_code = "deny"`, `panic/unwrap_used/expect_used = "deny"`, mutation tests in CI, `cargo hack check --each-feature`.
Past CVEs were handled and are low-severity: CVE-2024-40640 (constant-time base64 for secrets), CVE-2024-34063 (zeroize re-enable). Both fixed in 0.6/0.7.

**Production users**: all Element variants (Web/Desktop/iOS/Android), Element X iOS/Android, Fractal, iamb, and every
matrix-rust-sdk-based client. matrix-js-sdk (browser) uses it indirectly through `matrix-sdk-crypto-wasm`.

**WASM story — the nuance that matters:**
- The **core crate compiles to WASM**: `wasm_js = ["getrandom/wasm_js"]` is a maintained feature flag (getrandom 0.4 backend).
- There is **no officially maintained npm package exposing raw Olm primitives**. `matrix-org/vodozemac-bindings` says, verbatim:
  *"Please be aware that this project is no longer actively maintained. If you require any of the bindings from this repository, you will need to extract and update them on your own."* (last push 2024-09-05).
- The only officially maintained JS crypto package is `@matrix-org/matrix-sdk-crypto-wasm`, which is **Matrix-protocol-coupled** (see section e).

---

## (b) libsignal / libsignal-client — license, crate availability, completeness

| Field | Value |
|---|---|
| Repo | https://github.com/signalapp/libsignal |
| License | **AGPL-3.0** (repo LICENSE + package license) |
| Latest | **v0.102.3 (2026-09-15)**, active, ~6,018 stars |
| README, verbatim | *"Use outside of Signal is unsupported."* |
| crates.io | **NOT published.** No official Rust crate. |

**Crate availability — you cannot `cargo add` libsignal.** What exists on crates.io is unofficial repackaging:

| Crate | Version | Updated | Notes |
|---|---|---|---|
| `libsignal-dezire` | 0.1.146 | 2026-01-28 | unofficial fork/republish |
| `libsignal-protocol-syft` / `libsignal-core-syft` | 0.85.3-beta.5 | 2025-12 | OpenMined repo, beta |
| `wacore-libsignal` | 0.7.0 | 2026-08-07 | WhatsApp-clone ecosystem, ~105k dl |
| `wa-rs-libsignal` | 0.2.0 | 2026-02-17 | ~133k dl |
| `libsignal-rust` | 0.1.0 | 2025-09 | unvetted single-author crate (1.4k dl) |
| `libsignal-protocol` / `-sys` | 0.1.0 | **2019** | dead; wraps GPLv3 libsignal-protocol-c |

⚠️ All republishes derive from AGPL-3.0 source. Depending on any of them in a distributed product triggers AGPL.

**Completeness**: libsignal is the most complete (Double Ratchet + X3DH + PQXDH + sealed sender + sender-key groups +
zkgroup + device transfer). But completeness is the wrong axis here — see the browser blocker.

**Browser blocker (decisive for your stack):**
- `@signalapp/libsignal-client` npm: **0.102.3, AGPL-3.0-only, ~16.6k weekly downloads, ~140 MB install.**
- It is a **Node native addon** (`node-gyp-build`, `build_node_bridge.py`), shipping prebuilt native libs for
  Windows/macOS/Debian-Linux only. **No WASM, no browser build.**
- Official feature request still open: https://github.com/signalapp/libsignal/issues/350 ("WASM Bridge / Build for libsignal-client").
- Third-party WASM ports exist but are incomplete and AGPL-derived: `serpek/libsignal-wasm`, `theju/libsignal-wasm`
  (vendors libsignal v0.94.0; exposes only a partial surface — PublicKey/PrivateKey/IdentityKeyPair/HPKE slice).

**Verdict: ❌ for a browser frontend, ❌ on license for a proprietary product.**

---

## (c) Other double-ratchet Rust crates — no production pedigree

crates.io sweep (2026-09-16):

| Crate | Version | Last update | Downloads | Assessment |
|---|---|---|---|---|
| `double-ratchet` | 0.1.0 | 2019 | 4,463 | abandoned |
| `double-ratchet-2` | 0.4.0-pre.2 | 2022 | 19,653 | pre-release, stalled |
| `ksi-double-ratchet` | 0.1.3 | 2024-12 | 4,338 | hobby |
| `double-ratchet-signal` | 0.1.3 | 2024-02 | 1,619 | hobby |
| `light-double-ratchet` | 0.0.2 | 2024-07 | 2,307 | toy |
| `enigma-double-ratchet` | 0.1.0 | 2025-12 | 185 | new/unproven |
| `nostr-double-ratchet` | 0.0.167 | 2026-09-07 | 3,343 | active but Nostr-specific, pre-1.0 |
| `libsignal-rust` | 0.1.0 | 2025-09 | 1,440 | unvetted |

**Only vodozemac and libsignal have real production pedigree.** Among those, vodozemac is the only **audited,
permissively licensed, actively maintained** option. There is no third serious contender.

---

## (d) Matrix libolm -> vodozemac: what happened and why (lessons for you)

**History (primary sources)**
- libolm: C/C++ from **2015**, Matrix's first Double Ratchet implementation.
- Problems, quoted from the libolm README: *"It is not written in memory-safe languages (C and C++11), resulting in
  several CVEs over the years (e.g. CVE-2021-34813 and CVE-2021-44538). It also depends on simplistic cryptography
  primitive implementations which are intended for pragmatic and education purposes rather than security — e.g. Brad
  Conte's crypto-algorithms."*
- **Dec 2021**: rewritten in Rust -> vodozemac.
- **May 2022**: Least Authority audit, no significant findings; declared the recommended successor.
- **Jul 31 2024**: officially deprecated (Matthew Hodgson, commit `6d4b5b07`) — https://matrix.org/blog/2024/08/libolm-deprecation
- The libolm repo is now **archived** (74 stars, last push 2026-02-23).

**Why they migrated**
1. Memory safety (removes a whole CVE class: buffer overflows).
2. Better primitives (RustCrypto/Dalek instead of Brad Conte).
3. Performance: ~5-6x faster than libolm.
4. Team bandwidth: cannot maintain two implementations.

**Migration mechanics worth copying**
- Wire format was kept identical (Olm v1), so migration was incremental.
- `libolm-compat` + `Account::from_libolm_pickle()` import legacy state; `to_libolm_pickle()` exports it.
- Bug found in that path and fixed in 0.11: `Account::from_libolm_pickle()` had an OTK-id off-by-one that could
  overwrite the newest imported key → prekey messages became undecryptable (PR #382). **Pin >= 0.11 if you ever import libolm state.**

**Lessons for your project**
1. Pick memory-safe, audited Rust crypto from day one; never hand-roll the ratchet.
2. Choose the implementation with an owner who is not you — you will not out-maintain a crypto library.
3. Keep the wire format stable and versioned from the first release so you can swap the core later.
4. Fork the *protocol layer* (prekey distribution, trust, safety numbers), never the *crypto core*.

---

## (e) TS client options — state of each (2025-2026)

| Option | Version | License | Last update | Weekly dl | Browser? | Verdict |
|---|---|---|---|---|---|---|
| `@matrix-org/matrix-sdk-crypto-wasm` | 18.8.0 | Apache-2.0 | 2026-09-03 | **1,598,812** | Yes (WASM) | Matrix-coupled — see below |
| `@matrix-org/olm` (libolm) | 3.2.15 | Apache-2.0 | 2026-08-21 | 26,209 | Yes (WASM) | **deprecated, do not use** |
| `@signalapp/libsignal-client` | 0.102.3 | **AGPL-3.0-only** | 2026-09-15 | 16,616 | **No — native addon** | ❌ license + no browser |
| `@privacyresearch/libsignal-protocol-typescript` | 0.0.16 | **GPL-3.0-only** | 2023-05 publish | 4,485 | Yes (pure TS) | ❌ license + abandoned |
| `libsignal` (WhiskeySockets/libsignal-node) | 6.0.0 | **GPL-3.0** | 2026-05-13 | n/a | No — Node native | ❌ license + Node only |
| `@towns-protocol/vodozemac` | 0.1.0 | Apache-2.0 | src 2025-05-01 | n/a | Yes (WASM) | ✅ good reference, pinned old |
| `@dtelecom/vodozemac-wasm` | 0.3.0 | Apache-2.0 | 2026-05-14 | 10 | Yes (WASM) | ✅ OSS reference, low adoption |
| `@commapp/vodozemac` | 0.1.1 | Apache-2.0 | 2026-01-20 | n/a | Yes | ✅ **fork with X3DH added** |
| `@kinsh/vodozemac-wasm` | 0.4.2 | Apache-2.0 | 2026-08-12 | n/a | Yes (WASM) | fork of dtelecom |
| `@cogia/vodozemac-nodejs` | 0.0.10 | Apache-2.0 | 2025-08 | n/a | Node native | niche |

### matrix-rust-sdk / crypto-wasm — why NOT for a custom protocol
`@matrix-org/matrix-sdk-crypto-wasm` is excellent and hugely used (1.6M weekly downloads, Apache-2.0, active). But it
exposes the **Matrix `OlmMachine` state machine**, not raw primitives:
- docs.rs: *"A no-network-IO encryption state machine which can be used to add Matrix E2EE support into an existing client."*
- It is driven by **Matrix `/sync` push/pull**: device lists, to-device events, room keys, cross-signing, verification requests, key backup.
- Using it means adopting Matrix's data model (user/device IDs, room events, to-device messaging) for your secret chats.
That is a protocol you would be re-implementing anyway — with a large Matrix-shaped abstraction you don't want.

### Why your own thin vodozemac WASM wrapper
- `dTelecom/vodozemac-wasm` exists precisely because *"@matrix-org/olm (libolm) has been EOL since Oct 2023... The
  Matrix-recommended successor (@matrix-org/matrix-sdk-crypto-wasm) only exposes the high-level Matrix-protocol
  OlmMachine, not raw Olm primitives. This package fills the gap."*
- Their wrapper is small and exposes exactly what 1:1 needs:
  `Account` (new / sign / pickle / fromPickle / one-time keys), `Session` (createOutboundSession / createInboundSession /
  encrypt / decrypt / pickle), `InboundResult`.
- Bundle size: **~400 KB raw WASM, ~150 KB gzipped** (vs libolm ~750 KB).
- Caveat in their README: Megolm, libolm-pickle migration and **SAS verification are not bound** — you would add SAS.
- `towns-protocol/vodozemac-bindings` shows the packaging pattern (ESM/CJS x browser/node, `towns-protocol:wasm-esm`
  export condition, Webpack `experiments.asyncWebAssembly`) and is used by a real product (Towns Protocol, MIT repo,
  Base-mainnet E2E-encrypted group chat).

**Recommendation: own the wrapper.** It is ~1 small Rust crate + wasm-bindgen. You then control the API surface, the
vodozemac version pin, and the SAS binding — instead of depending on a 10-downloads/month third-party package.

---

## Recommended architecture

### One Rust crate, three deployment targets
```
crates/crypto-core        (vodozemac 0.11, Apache-2.0 — the ONLY place crypto lives)
   ├─ native  -> Tauri desktop core  (#[tauri::command])      <- no WASM
   ├─ wasm32  -> browser  (wasm-bindgen, vodozemac/wasm_js)   <- WASM
   └─ (optional) -> relay-side validation of bundle structure only (never decrypts)
```

**Why this split is right for your design**
- Tauri desktop: running crypto natively avoids WASM entirely and keeps private keys out of the webview JS heap.
  The webview only passes ciphertext through.
- Browser: WASM because JS has no constant-time primitives and pure-TS crypto is a security anti-pattern.
- The relay never calls `decrypt()` — it stores opaque `PreKeyMessage`/`Message` bodies, publishes bundles, and
  consumes one-time keys. `matrix-sdk-crypto`'s "no-network-IO state machine" model is the right shape, but you write
  your own 200-line state machine instead of importing Matrix's.

### Concrete dependencies
```toml
# crates/crypto-core/Cargo.toml
[dependencies]
vodozemac      = "0.11"        # Apache-2.0, MSRV 1.89, audited
serde          = { version = "1", features = ["derive"] }
zeroize        = "1.9"

[target.'cfg(target_arch = "wasm32")'.dependencies]
wasm-bindgen   = "0.2"
```
```
# browser build
wasm-pack build --target web --release
# requires: vodozemac feature `wasm_js`, getrandom backend cfg for the target
```
```json
// package.json (your wrapper)
{ "name": "@you/secret-chat-crypto", "license": "Apache-2.0" }
```

### Protocol flow (Olm-based secret chat)
1. **Device identity** — `Account::new()` once per install, per device. (Secret chat = single device, no roaming —
   this matches Olm's device-scoped, non-shareable sessions exactly.)
2. **Bundle publish** — upload to relay: Curve25519 sender key (`account.curve25519_key()`), Ed25519 fingerprint key,
   N one-time keys (`account.generate_one_time_keys(n)` / `one_time_keys()`), a fallback key, and an **Ed25519
   signature over the canonical bundle** (`account.sign(...)`) so a malicious relay cannot silently swap keys.
3. **Session start** — fetch the recipient bundle, `account.create_outbound_session(SessionConfig::version_1(), their_curve25519_key, their_otk)`.
   First message is `OlmMessage::PreKey`.
4. **Session accept** — recipient: `account.create_inbound_session(config, sender_curve25519_key, &prekey_msg)` -> `InboundCreationResult { session, plaintext }`.
   Then `mark_keys_as_published()`; replenish OTKs when the pool runs low.
5. **Steady state** — `session.encrypt()` yields `OlmMessage::Normal` once a reply has been received. Out-of-order
   handling is built in (skipped message keys are stored).
6. **Mandatory hardening (from the Olm spec, "Message authentication concerns")** — include sender **and intended
   recipient** user IDs inside the plaintext of (at least) the prekey messages, otherwise unknown key-share attacks
   succeed. Spec: https://gitlab.matrix.org/matrix-org/olm/-/blob/master/docs/olm.md
7. **Safety number verification** — `vodozemac::sas`: `Sas::new()`, `diffie_hellman()`, `bytes(info)`, then
   `emoji_indices()` (Matrix-style) or `to_digis()` (Signal-style digits), with `EstablishedSas`/`Mac` for the
   confirm step. Hash-commitment flow prevents the initiator from steering the SAS. **This must be in your WASM
   wrapper — none of the third-party bindings expose it today.**
8. **Persistence** — `account.pickle()` / `session.pickle()` (serde), `zeroize` plaintext buffers, encrypt pickles at
   rest with a key from the OS keystore (Tauri) or a passphrase-derived key (browser/IndexedDB). Encrypt at rest too:
   `Account` also supports `to_libolm_pickle`/`from_libolm_pickle` if libolm interop is ever needed.
9. **Destroy** — "secret chat ended" = drop the `Account`/`Session` pickles; no server-side key material exists to
   revoke. This is a feature of your single-device, no-roaming requirement.

**First-message note**: `PreKeyMessage` carries `I_A`, `E_A`, `E_B`, chain index and `T_0` — i.e. the recipient learns
which of their OTKs was used and can complete 3DH without an extra round trip. That is what makes secret chat work
asynchronously, and why the relay only needs to store public keys + ciphertext.

### Olm vs X3DH (flag for the spec)
Olm uses **3DH**, which is very close to but not identical to Signal's **X3DH**, and Olm does **not** define a signed
prekey bundle — bundle authentication is a layer you build (step 2 above). If you want textbook X3DH, `CommE2E/vodozemac`
is a fork that *"adds X3DH support"* and publishes `@commapp/vodozemac` (Apache-2.0). Treat it as a reference
implementation, not a dependency.
Specs: X3DH https://signal.org/docs/specifications/x3dh/ | Double Ratchet https://signal.org/docs/specifications/doubleratchet/

---

## License landmines (explicit)

| Artifact | License | Risk |
|---|---|---|
| `signalapp/libsignal` (repo, `rust/`) | **AGPL-3.0** | ⛔ Network copyleft. AGPL §13: operating a network service obliges you to offer the complete corresponding source to users. Poison for a closed product. Plus "use outside of Signal is unsupported". |
| `@signalapp/libsignal-client` (npm) | **AGPL-3.0-only** | ⛔ Same. Also installs a ~140 MB native addon. |
| `serpek/libsignal-wasm`, `theju/libsignal-wasm` | AGPL-derived | ⛔ Third-party WASM ports inherit AGPL. |
| `libsignal-dezire`, `libsignal-*-syft`, `wacore-libsignal`, `wa-rs-libsignal` | AGPL-derived | ⛔ Republished AGPL. |
| `@privacyresearch/libsignal-protocol-typescript` | **GPL-3.0-only** | ⛔ Copyleft; **and** upstream repo `privacyresearchgroup/libsignal-protocol-typescript` now **404s**, last publish 2023-05. Abandoned + unaudited. |
| `libsignal` (WhiskeySockets/libsignal-node) | **GPL-3.0** | ⛔ Copyleft; Node-native only. |
| `libsignal-protocol` / `-sys` (crates.io, 2019) | GPLv3 (wrapper) | ⛔ Dead + copyleft. |
| `efsec` (npm) | GPL-3.0-or-later | ⛔ Copyleft. |
| `vodozemac` | **Apache-2.0** | ✅ Permissive + patent grant. |
| `matrix-rust-sdk`, `@matrix-org/matrix-sdk-crypto-wasm` | Apache-2.0 | ✅ |
| `@towns-protocol/vodozemac`, `@dtelecom/vodozemac-wasm`, `@commapp/vodozemac`, `@kinsh/vodozemac-wasm` | Apache-2.0 | ✅ (verify transitive deps before adopting) |
| `matrix-org/olm` (libolm) | Apache-2.0 | ✅ license, ❌ deprecated/unmaintained since Jul 2024 |

**Rule of thumb**: Apache-2.0 throughout is achievable for this entire stack without touching AGPL/GPL. Do not accept
a libsignal dependency "just for the double ratchet" — it re-licenses your product.

---

## Risks and caveats to write into the spec

1. **vodozemac is pre-1.0 (0.11).** Every recent minor has had BREAKING changes (0.10 and 0.11 both). **Pin the exact
   version** and budget an upgrade task per minor release. This is the main cost of the recommendation.
2. **You own the protocol layer.** vodozemac gives you ratchets/SAS/pickles, not prekey distribution, trust policy,
   device management, or delivery semantics. That is the actual work — and it is the part Matrix's `OlmMachine` would
   otherwise have done. Budget for it.
3. **No official raw-vodozemac npm package.** You are writing and maintaining a ~1-file wasm-bindgen wrapper. Keep it
   trivial so it stays auditable.
4. **Third-party bindings are thin-ice.** Towns' is pinned to 2025-05 and single-maintainer; dtelecom's has ~10
   weekly downloads. Use them as reference code, not as dependencies.
5. **Don't use libolm.** `matrix-org/olm` is archived, npm `@matrix-org/olm` still gets published/updated but is
   deprecated with known CVEs. It is not a fallback.
6. **getrandom on wasm32** needs its backend configured (`wasm_js` feature + target cfg). Verify your exact
   getrandom 0.4.x setup at build time — this is the most common WASM build failure.
7. **SAS is not bound** in any off-the-shelf vodozemac WASM wrapper. If safety-number verification is a requirement
   (it is, per your brief), this is custom work in your wrapper.
8. **Symmetric backup** is still "planned, not supported" in vodozemac — irrelevant for no-roaming secret chats, but
   do not design a cloud backup of secret-chat keys on top of it.

---

## Primary sources

- vodozemac repo — https://github.com/matrix-org/vodozemac
- vodozemac Cargo.toml (features, MSRV) — https://github.com/matrix-org/vodozemac/blob/main/Cargo.toml
- vodozemac CHANGELOG (0.9->0.11 breaking changes) — https://github.com/matrix-org/vodozemac/blob/main/CHANGELOG.md
- vodozemac docs (olm module, sas module) — https://docs.rs/vodozemac/latest/vodozemac/
- Olm spec (3DH, wire format, key-share attacks) — https://gitlab.matrix.org/matrix-org/olm/-/blob/master/docs/olm.md
- Megolm spec (group ratchet) — https://gitlab.matrix.org/matrix-org/olm/-/blob/master/docs/megolm.md
- Least Authority audit — https://matrix.org/media/Least%20Authority%20-%20Matrix%20vodozemac%20Final%20Audit%20Report.pdf
- libolm deprecation — https://matrix.org/blog/2024/08/libolm-deprecation
- libolm repo (archived) — https://gitlab.matrix.org/matrix-org/olm
- Matrix SAS verification spec — https://spec.matrix.org/v1.2/client-server-api/#short-authentication-string-sas-verification
- matrix-rust-sdk (Apache-2.0) — https://github.com/matrix-org/matrix-rust-sdk
- matrix-sdk-crypto docs — https://docs.rs/matrix-sdk-crypto/latest/matrix_sdk_crypto/
- matrix-sdk-crypto-wasm (npm) — https://github.com/matrix-org/matrix-sdk-crypto-wasm
- vodozemac bindings (unmaintained) — https://github.com/matrix-org/vodozemac-bindings
- libsignal (AGPL-3.0) — https://github.com/signalapp/libsignal
- libsignal WASM feature request — https://github.com/signalapp/libsignal/issues/350
- Signal Double Ratchet spec — https://signal.org/docs/specifications/doubleratchet/
- Signal X3DH spec — https://signal.org/docs/specifications/x3dh/
- Towns Protocol vodozemac bindings (Apache-2.0) — https://github.com/towns-protocol/vodozemac-bindings
- dTelecom vodozemac-wasm (Apache-2.0, non-Matrix Olm) — https://github.com/dTelecom/vodozemac-wasm
- Comm vodozemac fork with X3DH — https://github.com/CommE2E/vodozemac
- Towns Protocol (E2E encrypted product) — https://docs.towns.com/