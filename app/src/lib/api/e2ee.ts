import { apiRequest, ApiError } from "./client";

/**
 * REST surface for the E2EE key directory (M3 secret chats).
 *
 * - POST /api/e2ee/keys/upload (Bearer) — upserts the caller's bundle.
 * - GET  /api/e2ee/keys/{username} (Bearer) — atomically POPS one of the
 *   peer's one-time keys; 409 `no_one_time_keys` when the pool is drained.
 */

/** Wire shape of a published key bundle. Keys are base64 SPKI public keys. */
export interface E2eeKeyBundle {
  identity_key: string;
  /** Present on fetch (the popped one-time key); absent when exhausted. */
  one_time_key?: string;
}

/** Thrown when the peer has no one-time keys left (HTTP 409). */
export class NoOneTimeKeysError extends Error {
  readonly username: string;
  constructor(username: string) {
    super(`no one-time keys available for ${username}`);
    this.name = "NoOneTimeKeysError";
    this.username = username;
  }
}

/** POST /api/e2ee/keys/upload { identity_key, one_time_keys } (Bearer access). */
export function uploadE2eeKeyBundle(
  accessToken: string,
  identityKey: string,
  oneTimeKeys: string[],
): Promise<void> {
  return apiRequest<void>("/api/e2ee/keys/upload", {
    method: "POST",
    accessToken,
    body: { identity_key: identityKey, one_time_keys: oneTimeKeys },
  });
}

/** GET /api/e2ee/keys/{username} → 200 bundle | 409 no_one_time_keys. */
export async function fetchPeerBundle(
  accessToken: string,
  username: string,
): Promise<E2eeKeyBundle> {
  try {
    return await apiRequest<E2eeKeyBundle>(
      `/api/e2ee/keys/${encodeURIComponent(username)}`,
      { method: "GET", accessToken },
    );
  } catch (error) {
    if (error instanceof ApiError && error.status === 409) {
      throw new NoOneTimeKeysError(username);
    }
    throw error;
  }
}
