/**
 * olm-lite — MVP end-to-end encryption for secret chats, pure TypeScript over
 * WebCrypto. NO third-party dependencies.
 *
 * Scope (MVP-grade, documented deliberately):
 * - Identity: one ECDH P-256 key pair per device. The public half is uploaded
 *   as base64 SPKI; private halves live ONLY in memory/localStorage
 *   (`jiuyue.e2ee.identity` / `jiuyue.e2ee.otks`) — device-bound by design.
 * - X3DH-style setup: SK = HKDF(DH(myID, peerID) || DH(myID, peerOTK)); the
 *   peer's one-time key doubles as its initial ratchet key (Signal semantics
 *   with the OTK in the signed-prekey role), so the Double Ratchet bootstrap
 *   is exactly the reference pseudocode.
 * - Double Ratchet: HKDF-SHA-256 root chain, symmetric HMAC-SHA-256 message
 *   chains, AES-GCM-256 per message with monotonically increasing counters,
 *   out-of-order window via a skipped-message-key map capped at 100 entries.
 * - Session state persists under `jiuyue.e2ee.session.<convId>` (device-local).
 *
 * KNOWN LIMITS: no device lists / multi-device fan-out, no prekey signature
 * verification, no forward-secret key deletion schedule beyond the skipped
 * cap, sessions do not survive across devices by design.
 * UPGRADE PATH: native builds should replace this module with the Rust
 * `vodozemac` bindings behind `crates/crypto` (same wire shape: opaque
 * ciphertext + message_type over the e2ee.msg frame); this file is the web
 * fallback until that lands.
 */

import { fetchPeerBundle, uploadE2eeKeyBundle } from "../api/e2ee";

const IDENTITY_KEY = "jiuyue.e2ee.identity";
const OTKS_KEY = "jiuyue.e2ee.otks";
export const SESSION_KEY_PREFIX = "jiuyue.e2ee.session.";
const OTK_COUNT = 10;
const MAX_SKIPPED_KEYS = 100;
const EC_ALG = { name: "ECDH", namedCurve: "P-256" } as const;

/** Thrown when a ciphertext cannot be decrypted (reason codes below). */
export class DecryptError extends Error {
  readonly reason: "no_session" | "malformed" | "tampered";
  constructor(reason: "no_session" | "malformed" | "tampered") {
    super(`olm-lite decrypt failed: ${reason}`);
    this.name = "DecryptError";
    this.reason = reason;
  }
}

// --- storage (localStorage with an in-memory fallback for non-DOM tests) ---

const memoryStorage = new Map<string, string>();
let keyPrefix = "";

function scoped(key: string): string {
  return keyPrefix.length === 0 ? key : `${keyPrefix}.${key}`;
}

/**
 * Test/isolation hook: namespaces all engine storage and drops cached key
 * material. Production code never calls this (empty scope = plain keys).
 */
export function useStorageScope(scope: string | null): void {
  keyPrefix = scope === null ? "" : `${scope}.`;
  identityCache = null;
  otksCache = null;
}

function storageGet(key: string): string | null {
  try {
    return localStorage.getItem(scoped(key));
  } catch {
    return memoryStorage.get(scoped(key)) ?? null;
  }
}

function storageSet(key: string, value: string): void {
  try {
    localStorage.setItem(scoped(key), value);
  } catch {
    memoryStorage.set(scoped(key), value);
  }
}

// --- byte helpers -----------------------------------------------------------

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Byte buffer backed by a plain ArrayBuffer (what BufferSource demands). */
type Bytes = Uint8Array<ArrayBuffer>;

function bytesToB64(bytes: Uint8Array): string {
  const chunks: string[] = [];
  for (const byte of bytes) chunks.push(String.fromCharCode(byte));
  return btoa(chunks.join(""));
}

function b64ToBytes(b64: string): Bytes {
  const binary = atob(b64);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) out[i] = binary.charCodeAt(i);
  return out;
}

function concatBytes(a: Uint8Array, b: Uint8Array): Bytes {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

function subtle(): SubtleCrypto {
  const c = globalThis.crypto;
  if (c === undefined || c.subtle === undefined) {
    throw new Error("WebCrypto unavailable in this environment");
  }
  return c.subtle;
}

async function ecdh(priv: CryptoKey, peerPubB64: string): Promise<Bytes> {
  // Bundles carry base64 SPKI; import format must match ("spki", not "raw").
  const pub = await subtle().importKey(
    "spki",
    b64ToBytes(peerPubB64),
    EC_ALG,
    true,
    [],
  );
  return new Uint8Array(
    await subtle().deriveBits({ name: "ECDH", public: pub }, priv, 256),
  );
}

async function hkdf(
  ikm: Bytes,
  salt: Bytes,
  info: string,
  lengthBytes: number,
): Promise<Bytes> {
  const key = await subtle().importKey("raw", ikm, "HKDF", false, [
    "deriveBits",
  ]);
  return new Uint8Array(
    await subtle().deriveBits(
      { name: "HKDF", hash: "SHA-256", salt, info: encoder.encode(info) },
      key,
      lengthBytes * 8,
    ),
  );
}

async function hmacSha256(keyBytes: Bytes, data: Bytes): Promise<Bytes> {
  const key = await subtle().importKey(
    "raw",
    keyBytes,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  return new Uint8Array(await subtle().sign("HMAC", key, data));
}

/** KDF_RK from the Double Ratchet spec: next root key + chain key. */
async function kdfRk(rootKey: Bytes, dhOut: Bytes): Promise<[Bytes, Bytes]> {
  const okm = await hkdf(dhOut, rootKey, "olmlite/root/v1", 64);
  return [okm.slice(0, 32), okm.slice(32)];
}

const chainStep = (ck: Bytes): Promise<Bytes> =>
  hmacSha256(ck, new Uint8Array([0x02]));
const messageKeyFrom = (ck: Bytes): Promise<Bytes> =>
  hmacSha256(ck, new Uint8Array([0x01]));

async function aesSeal(mk: Bytes, plaintext: Bytes): Promise<Bytes> {
  const raw = await hkdf(mk, new Uint8Array(32), "olmlite/msg/v1", 32);
  const key = await subtle().importKey("raw", raw, "AES-GCM", false, [
    "encrypt",
  ]);
  const iv = globalThis.crypto.getRandomValues(new Uint8Array(12));
  const ct = new Uint8Array(
    await subtle().encrypt({ name: "AES-GCM", iv }, key, plaintext),
  );
  return concatBytes(iv, ct);
}

async function aesOpen(mk: Bytes, ivAndCt: Bytes): Promise<string> {
  if (ivAndCt.length < 13) throw new DecryptError("malformed");
  const raw = await hkdf(mk, new Uint8Array(32), "olmlite/msg/v1", 32);
  const key = await subtle().importKey("raw", raw, "AES-GCM", false, [
    "decrypt",
  ]);
  const iv = ivAndCt.slice(0, 12);
  const ct = ivAndCt.slice(12);
  const plain = await subtle().decrypt({ name: "AES-GCM", iv }, key, ct);
  return decoder.decode(plain);
}

// --- identity & bundles -----------------------------------------------------

interface OtkMaterial {
  publicB64: string;
  privJwk: JsonWebKey;
  priv: CryptoKey;
}

let identityCache: { publicB64: string; priv: CryptoKey } | null = null;
let otksCache: OtkMaterial[] | null = null;

async function generateKeypair(): Promise<{
  publicB64: string;
  privJwk: JsonWebKey;
  priv: CryptoKey;
}> {
  const pair = await subtle().generateKey(EC_ALG, true, ["deriveBits"]);
  const spki = new Uint8Array(await subtle().exportKey("spki", pair.publicKey));
  return {
    publicB64: bytesToB64(spki),
    privJwk: await subtle().exportKey("jwk", pair.privateKey),
    priv: pair.privateKey,
  };
}

async function importPrivateJwk(jwk: JsonWebKey): Promise<CryptoKey> {
  return subtle().importKey("jwk", jwk, EC_ALG, true, ["deriveBits"]);
}

/** Loads or creates THIS device's identity key pair. Returns the public b64. */
export async function ensureIdentity(): Promise<string> {
  if (identityCache !== null) return identityCache.publicB64;
  const raw = storageGet(IDENTITY_KEY);
  if (raw !== null) {
    try {
      const parsed = JSON.parse(raw) as {
        publicB64: string;
        privJwk: JsonWebKey;
      };
      identityCache = {
        publicB64: parsed.publicB64,
        priv: await importPrivateJwk(parsed.privJwk),
      };
      return identityCache.publicB64;
    } catch {
      // Corrupted identity: regenerate below.
    }
  }
  const fresh = await generateKeypair();
  storageSet(
    IDENTITY_KEY,
    JSON.stringify({ publicB64: fresh.publicB64, privJwk: fresh.privJwk }),
  );
  identityCache = { publicB64: fresh.publicB64, priv: fresh.priv };
  return fresh.publicB64;
}

async function ensureOneTimeKeys(): Promise<OtkMaterial[]> {
  if (otksCache !== null) return otksCache;
  const raw = storageGet(OTKS_KEY);
  if (raw !== null) {
    try {
      const parsed = JSON.parse(raw) as Array<{
        publicB64: string;
        privJwk: JsonWebKey;
      }>;
      otksCache = [];
      for (const otk of parsed) {
        otksCache.push({
          publicB64: otk.publicB64,
          privJwk: otk.privJwk,
          priv: await importPrivateJwk(otk.privJwk),
        });
      }
      return otksCache;
    } catch {
      // Corrupted OTK store: regenerate below.
    }
  }
  const generated: OtkMaterial[] = [];
  for (let i = 0; i < OTK_COUNT; i += 1)
    generated.push(await generateKeypair());
  otksCache = generated;
  persistOtks();
  return otksCache;
}

function persistOtks(): void {
  if (otksCache === null) return;
  storageSet(
    OTKS_KEY,
    JSON.stringify(
      otksCache.map((o) => ({ publicB64: o.publicB64, privJwk: o.privJwk })),
    ),
  );
}

/** This device's bundle exactly as it would be uploaded — tests/debug only. */
export async function localBundle(): Promise<{
  identity_key: string;
  one_time_keys: string[];
}> {
  await ensureIdentity();
  const otks = await ensureOneTimeKeys();
  return {
    identity_key: identityCache?.publicB64 ?? "",
    one_time_keys: otks.map((o) => o.publicB64),
  };
}

/** POST /api/e2ee/keys/upload — idempotent upsert of this device's bundle. */
export async function publishBundle(accessToken: string): Promise<void> {
  const bundle = await localBundle();
  await uploadE2eeKeyBundle(
    accessToken,
    bundle.identity_key,
    bundle.one_time_keys,
  );
}

// --- session state ----------------------------------------------------------

interface RatchetState {
  v: 1;
  role: "initiator" | "responder";
  rootKey: string;
  dhsPrivJwk: JsonWebKey;
  dhsPub: string;
  /** Remote ratchet public key (starts at the peer's consumed OTK). */
  dhr: string | null;
  cks: string | null;
  ckr: string | null;
  ns: number;
  nr: number;
  pns: number;
  /** `[`${dhr}:${n}`, mkB64]` pairs for out-of-order delivery, capped. */
  skipped: Array<[string, string]>;
  peerIdentity: string;
}

interface MsgHeader {
  v: 1;
  id: string;
  dh: string;
  pn: number;
  n: number;
  t: 0 | 1;
}

function sessionKeyOf(conversationId: number): string {
  return `${SESSION_KEY_PREFIX}${String(conversationId)}`;
}

function loadState(conversationId: number): RatchetState | null {
  const raw = storageGet(sessionKeyOf(conversationId));
  if (raw === null) return null;
  try {
    const st = JSON.parse(raw) as RatchetState;
    if (st.v !== 1 || typeof st.rootKey !== "string") return null;
    return st;
  } catch {
    return null;
  }
}

function saveState(conversationId: number, st: RatchetState): void {
  storageSet(sessionKeyOf(conversationId), JSON.stringify(st));
}

/** True when an established session exists locally for the conversation. */
export function hasSession(conversationId: number): boolean {
  return loadState(conversationId) !== null;
}

/** Peer identity public key bound to the conversation session (or null). */
export function getPeerIdentity(conversationId: number): string | null {
  return loadState(conversationId)?.peerIdentity ?? null;
}

// --- wire payload: [u16 headerLen][headerJSON][iv||ct] ----------------------

function encodePayload(header: MsgHeader, body: Uint8Array): string {
  const headerJson = encoder.encode(JSON.stringify(header));
  const out = new Uint8Array(2 + headerJson.length + body.length);
  new DataView(out.buffer).setUint16(0, headerJson.length);
  out.set(headerJson, 2);
  out.set(body, 2 + headerJson.length);
  return bytesToB64(out);
}

function decodePayload(ciphertext: string): {
  header: MsgHeader;
  body: Bytes;
} {
  let bytes: Bytes;
  try {
    bytes = b64ToBytes(ciphertext);
  } catch {
    throw new DecryptError("malformed");
  }
  if (bytes.length < 4) throw new DecryptError("malformed");
  const headerLen = new DataView(bytes.buffer, bytes.byteOffset).getUint16(0);
  if (2 + headerLen > bytes.length) throw new DecryptError("malformed");
  let header: unknown;
  try {
    header = JSON.parse(decoder.decode(bytes.slice(2, 2 + headerLen)));
  } catch {
    throw new DecryptError("malformed");
  }
  const h = header as Partial<MsgHeader>;
  if (
    h === null ||
    typeof h !== "object" ||
    h.v !== 1 ||
    typeof h.id !== "string" ||
    typeof h.dh !== "string" ||
    typeof h.pn !== "number" ||
    typeof h.n !== "number" ||
    (h.t !== 0 && h.t !== 1)
  ) {
    throw new DecryptError("malformed");
  }
  return {
    header: { v: 1, id: h.id, dh: h.dh, pn: h.pn, n: h.n, t: h.t },
    body: bytes.slice(2 + headerLen),
  };
}

// --- ratchet internals ------------------------------------------------------

function skippedInsert(st: RatchetState, key: string, mkB64: string): void {
  const existing = st.skipped.findIndex(([k]) => k === key);
  if (existing >= 0) st.skipped.splice(existing, 1);
  st.skipped.push([key, mkB64]);
  while (st.skipped.length > MAX_SKIPPED_KEYS) st.skipped.shift();
}

async function skipMessageKeys(st: RatchetState, until: number): Promise<void> {
  if (st.ckr === null || st.dhr === null) return;
  while (st.nr < until) {
    const ck = b64ToBytes(st.ckr);
    skippedInsert(
      st,
      `${st.dhr}:${st.nr}`,
      bytesToB64(await messageKeyFrom(ck)),
    );
    st.ckr = bytesToB64(await chainStep(ck));
    st.nr += 1;
  }
}

async function dhRatchet(st: RatchetState, remotePub: string): Promise<void> {
  await skipMessageKeys(st, st.pns);
  st.dhr = remotePub;
  const [rk1, ckr] = await kdfRk(
    b64ToBytes(st.rootKey),
    await ecdh(await importPrivateJwk(st.dhsPrivJwk), remotePub),
  );
  st.rootKey = bytesToB64(rk1);
  st.ckr = bytesToB64(ckr);
  st.nr = 0;
  const fresh = await generateKeypair();
  st.dhsPrivJwk = fresh.privJwk;
  st.dhsPub = fresh.publicB64;
  const [rk2, cks] = await kdfRk(rk1, await ecdh(fresh.priv, remotePub));
  st.rootKey = bytesToB64(rk2);
  st.cks = bytesToB64(cks);
  st.pns = st.ns;
  st.ns = 0;
}

/** Signal Double Ratchet decrypt step; mutates `st`, caller persists on success. */
async function openWithState(
  st: RatchetState,
  header: MsgHeader,
  body: Bytes,
): Promise<string> {
  const skipKey = `${header.dh}:${header.n}`;
  const idx = st.skipped.findIndex(([k]) => k === skipKey);
  if (idx >= 0) {
    const mkB64 = st.skipped[idx]?.[1];
    if (mkB64 === undefined) throw new DecryptError("malformed");
    let plain: string;
    try {
      plain = await aesOpen(b64ToBytes(mkB64), body);
    } catch {
      throw new DecryptError("tampered");
    }
    st.skipped.splice(idx, 1);
    return plain;
  }
  if (header.dh !== st.dhr) await dhRatchet(st, header.dh);
  await skipMessageKeys(st, header.n);
  if (st.ckr === null || st.dhr === null) throw new DecryptError("no_session");
  const ck = b64ToBytes(st.ckr);
  let plain: string;
  try {
    plain = await aesOpen(await messageKeyFrom(ck), body);
  } catch {
    throw new DecryptError("tampered");
  }
  st.ckr = bytesToB64(await chainStep(ck));
  st.nr += 1;
  return plain;
}

// --- establishment ----------------------------------------------------------

/**
 * Initiator side: derive the shared secret from a fetched peer bundle and
 * install the session for `conversationId`. No network here — callers fetch
 * the bundle (see fetchPeerBundleAndEstablish) or pass it directly (tests).
 */
export async function establishFromBundle(
  bundle: { identity_key: string; one_time_key?: string },
  conversationId: number,
): Promise<void> {
  if (loadState(conversationId) !== null) return;
  const otk = bundle.one_time_key;
  if (typeof otk !== "string" || otk.length === 0) {
    throw new DecryptError("no_session");
  }
  await ensureIdentity();
  const myPriv = identityCache?.priv;
  if (myPriv === undefined) throw new DecryptError("no_session");
  // X3DH-style: DH1 = DH(myID, peerID), DH2 = DH(myID, peerOTK).
  const ikm = concatBytes(
    await ecdh(myPriv, bundle.identity_key),
    await ecdh(myPriv, otk),
  );
  const sk = await hkdf(ikm, new Uint8Array(32), "olmlite/x3dh/v1", 32);
  // The peer's OTK acts as its initial ratchet key (signed-prekey role).
  const fresh = await generateKeypair();
  const [rk, cks] = await kdfRk(sk, await ecdh(fresh.priv, otk));
  saveState(conversationId, {
    v: 1,
    role: "initiator",
    rootKey: bytesToB64(rk),
    dhsPrivJwk: fresh.privJwk,
    dhsPub: fresh.publicB64,
    dhr: otk,
    cks: bytesToB64(cks),
    ckr: null,
    ns: 0,
    nr: 0,
    pns: 0,
    skipped: [],
    peerIdentity: bundle.identity_key,
  });
}

/** Fetches the peer bundle over REST and establishes the initiator session. */
export async function fetchPeerBundleAndEstablish(
  username: string,
  conversationId: number,
  accessToken: string,
): Promise<void> {
  if (loadState(conversationId) !== null) return;
  const bundle = await fetchPeerBundle(accessToken, username);
  await establishFromBundle(bundle, conversationId);
}

/** Responder side: lazily establish from the first inbound session-init. */
async function establishResponder(
  conversationId: number,
  header: MsgHeader,
  body: Bytes,
): Promise<string> {
  await ensureIdentity();
  const myPriv = identityCache?.priv;
  if (myPriv === undefined) throw new DecryptError("no_session");
  const otks = await ensureOneTimeKeys();
  for (let i = 0; i < otks.length; i += 1) {
    const candidate = otks[i];
    if (candidate === undefined) break;
    // Mirror X3DH: DH1 = DH(myID, peerID), DH2 = DH(myOTK, peerID).
    const ikm = concatBytes(
      await ecdh(myPriv, header.id),
      await ecdh(candidate.priv, header.id),
    );
    const sk = await hkdf(ikm, new Uint8Array(32), "olmlite/x3dh/v1", 32);
    const st: RatchetState = {
      v: 1,
      role: "responder",
      rootKey: bytesToB64(sk),
      dhsPrivJwk: candidate.privJwk,
      dhsPub: candidate.publicB64,
      dhr: null,
      cks: null,
      ckr: null,
      ns: 0,
      nr: 0,
      pns: 0,
      skipped: [],
      peerIdentity: header.id,
    };
    try {
      // GCM auth is the proof we guessed the right OTK; on success the
      // standard path inside openWithState performs the first DH ratchet.
      const plain = await openWithState(st, header, body);
      otks.splice(i, 1);
      persistOtks();
      saveState(conversationId, st);
      return plain;
    } catch (error) {
      if (!(error instanceof DecryptError)) throw error;
      // Wrong OTK guess: wipe trial mutations and try the next key.
    }
  }
  throw new DecryptError("tampered");
}

// --- public API -------------------------------------------------------------

export async function encrypt(
  conversationId: number,
  plaintext: string,
): Promise<{ ciphertext: string; messageType: 0 | 1 }> {
  const st = loadState(conversationId);
  if (st === null || st.cks === null) throw new DecryptError("no_session");
  const identityPub = await ensureIdentity();
  // Only the INITIATOR's very first message announces the session; the
  // responder's first reply already rides an established ratchet.
  const messageType: 0 | 1 =
    st.role === "initiator" && st.ns === 0 && st.pns === 0 ? 0 : 1;
  const ck = b64ToBytes(st.cks);
  const body = await aesSeal(
    await messageKeyFrom(ck),
    encoder.encode(plaintext),
  );
  const header: MsgHeader = {
    v: 1,
    id: identityPub,
    dh: st.dhsPub,
    pn: st.pns,
    n: st.ns,
    t: messageType,
  };
  st.ns += 1;
  st.cks = bytesToB64(await chainStep(ck));
  saveState(conversationId, st);
  return { ciphertext: encodePayload(header, body), messageType };
}

/** Decrypts and returns just the plaintext (see decryptEnvelope for sender). */
export async function decrypt(
  conversationId: number,
  ciphertext: string,
): Promise<string> {
  return (await decryptEnvelope(conversationId, ciphertext)).plaintext;
}

export async function decryptEnvelope(
  conversationId: number,
  ciphertext: string,
): Promise<{ plaintext: string; senderIdentity: string }> {
  const { header, body } = decodePayload(ciphertext);
  const existing = loadState(conversationId);
  if (existing === null) {
    // No local session yet: try to establish from THIS frame regardless of
    // its t flag — relay order is not guaranteed, so the session-init may
    // arrive after later ratchet messages. Wrong-OTK trials fail GCM auth.
    return {
      plaintext: await establishResponder(conversationId, header, body),
      senderIdentity: header.id,
    };
  }
  const plaintext = await openWithState(existing, header, body);
  saveState(conversationId, existing);
  return { plaintext, senderIdentity: header.id };
}

/**
 * 12-hex-char safety number: SHA-256 over the SORTED identity keys, so both
 * sides compute the same code regardless of who calls first.
 */
export async function safetyCode(peerIdentityB64: string): Promise<string> {
  const mine = await ensureIdentity();
  const material = [mine, peerIdentityB64].sort().join("|");
  const digest = new Uint8Array(
    await subtle().digest("SHA-256", encoder.encode(material)),
  );
  return Array.from(digest.slice(0, 6))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}
