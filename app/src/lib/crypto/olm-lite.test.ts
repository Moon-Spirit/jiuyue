// @vitest-environment node
// Node environment guarantees WebCrypto (globalThis.crypto.subtle); the
// engine falls back to in-memory storage when localStorage is absent.
import { describe, expect, it } from "vitest";
import {
  DecryptError,
  decrypt,
  encrypt,
  ensureIdentity,
  establishFromBundle,
  localBundle,
  safetyCode,
  useStorageScope,
} from "./olm-lite";

let scopeCounter = 0;

/** Fresh isolated storage scope (per-party key material in tests). */
function freshScope(): string {
  scopeCounter += 1;
  return `olm-test-${scopeCounter}`;
}

interface Party {
  scope: string;
  bundle: { identity_key: string; one_time_keys: string[] };
}

/** Creates a party: its own identity + one-time keys in an isolated scope. */
async function makeParty(): Promise<Party> {
  const scope = freshScope();
  useStorageScope(scope);
  await ensureIdentity();
  return { scope, bundle: await localBundle() };
}

function use(party: Party): void {
  useStorageScope(party.scope);
}

/** Standard initiator bootstrap of alice → bob over conversation 1. */
async function establishAliceToBob(alice: Party, bob: Party): Promise<void> {
  use(alice);
  const otk = bob.bundle.one_time_keys[0];
  if (otk === undefined) throw new Error("peer bundle has no one-time keys");
  await establishFromBundle(
    { identity_key: bob.bundle.identity_key, one_time_key: otk },
    1,
  );
}

describe("olm-lite double ratchet", () => {
  it("roundtrips messages between two parties across ratchet turns", async () => {
    const alice = await makeParty();
    const bob = await makeParty();
    await establishAliceToBob(alice, bob);

    // First message is a session-init (message_type 0).
    const first = await encrypt(1, "hello bob");
    expect(first.messageType).toBe(0);

    // Bob lazily establishes from the inbound session-init.
    use(bob);
    expect(await decrypt(1, first.ciphertext)).toBe("hello bob");

    // Bob replies → DH ratchet turn; alice consumes it.
    const reply = await encrypt(1, "hi alice");
    expect(reply.messageType).toBe(1);
    use(alice);
    expect(await decrypt(1, reply.ciphertext)).toBe("hi alice");

    // Another turn each way to prove the chains keep advancing.
    const second = await encrypt(1, "still working");
    use(bob);
    expect(await decrypt(1, second.ciphertext)).toBe("still working");
    const secondReply = await encrypt(1, "likewise");
    use(alice);
    expect(await decrypt(1, secondReply.ciphertext)).toBe("likewise");
  });

  it("decrypts out-of-order delivery once the gap fills", async () => {
    const alice = await makeParty();
    const bob = await makeParty();
    await establishAliceToBob(alice, bob);

    const one = await encrypt(1, "one");
    const two = await encrypt(1, "two");
    const three = await encrypt(1, "three");

    use(bob);
    // The newest arrives first: keys for the missing counters are cached.
    expect(await decrypt(1, three.ciphertext)).toBe("three");
    expect(await decrypt(1, one.ciphertext)).toBe("one");
    expect(await decrypt(1, two.ciphertext)).toBe("two");
  });

  it("caps the skipped-key window at 100 and evicts the oldest", async () => {
    const alice = await makeParty();
    const bob = await makeParty();
    await establishAliceToBob(alice, bob);

    const sent: Array<{ ciphertext: string }> = [];
    for (let i = 0; i < 120; i += 1) {
      sent.push(await encrypt(1, `m${i}`));
    }

    use(bob);
    // Jump straight to #119: 119 skipped keys cached, capped to the newest 100.
    expect(await decrypt(1, sent[119]?.ciphertext ?? "")).toBe("m119");
    // #0 was evicted from the window — its key is gone forever.
    await expect(decrypt(1, sent[0]?.ciphertext ?? "")).rejects.toThrow(
      DecryptError,
    );
    // #19 is the oldest survivor inside the window.
    expect(await decrypt(1, sent[19]?.ciphertext ?? "")).toBe("m19");
  });

  it("throws DecryptError on tampered ciphertext", async () => {
    const alice = await makeParty();
    const bob = await makeParty();
    await establishAliceToBob(alice, bob);

    const message = await encrypt(1, "tamper me");
    use(bob);
    expect(await decrypt(1, message.ciphertext)).toBe("tamper me");

    const next = await encrypt(1, "tamper this");
    const bytes = Uint8Array.from(atob(next.ciphertext), (c) =>
      c.charCodeAt(0),
    );
    bytes[bytes.length - 1] ^= 0xff; // flip one ciphertext byte
    const tampered = btoa(String.fromCharCode(...bytes));
    await expect(decrypt(1, tampered)).rejects.toThrow(DecryptError);
  });

  it("refuses ciphertext from a stranger (wrong key material)", async () => {
    const alice = await makeParty();
    const bob = await makeParty();
    const mallory = await makeParty();
    await establishAliceToBob(alice, bob);

    const message = await encrypt(1, "for bob only");
    use(mallory);
    // Mallory has no session; his own OTKs cannot open alice's ciphertext.
    await expect(decrypt(1, message.ciphertext)).rejects.toThrow(DecryptError);
  });

  it("throws no_session for normal messages without an established session", async () => {
    const stranger = await makeParty();
    use(stranger);
    // Hand-crafted t=1 envelope (no session exists on this device).
    const header = JSON.stringify({
      v: 1,
      id: stranger.bundle.identity_key,
      dh: stranger.bundle.identity_key,
      pn: 0,
      n: 0,
      t: 1,
    });
    const headerBytes = new TextEncoder().encode(header);
    const payload = new Uint8Array(2 + headerBytes.length + 28);
    new DataView(payload.buffer).setUint16(0, headerBytes.length);
    payload.set(headerBytes, 2);
    const envelope = btoa(String.fromCharCode(...payload));
    await expect(decrypt(1, envelope)).rejects.toThrow(DecryptError);
  });

  it("keeps sessions and identities separate per storage scope", async () => {
    const alice = await makeParty();
    const bob = await makeParty();
    await establishAliceToBob(alice, bob);
    use(bob);
    // Bob never established a session for conversation 1 as a sender.
    await expect(encrypt(1, "nope")).rejects.toThrow(DecryptError);
  });
});

describe("olm-lite safety code", () => {
  it("is deterministic and order-independent across the two parties", async () => {
    const alice = await makeParty();
    const bob = await makeParty();
    const carol = await makeParty();

    use(alice);
    const fromAlice = await safetyCode(bob.bundle.identity_key);
    const fromAliceAgain = await safetyCode(bob.bundle.identity_key);
    expect(fromAlice).toBe(fromAliceAgain);
    expect(fromAlice).toMatch(/^[0-9a-f]{12}$/);

    use(bob);
    const fromBob = await safetyCode(alice.bundle.identity_key);
    expect(fromBob).toBe(fromAlice);

    use(alice);
    const withCarol = await safetyCode(carol.bundle.identity_key);
    expect(withCarol).not.toBe(fromAlice);
  });
});
