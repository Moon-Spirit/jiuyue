import { apiRequest } from "./client";

/** One hit of GET /api/users/search — a minimal public user profile. */
export interface UserSearchResult {
  user_id: string;
  username: string;
  /** Stable numeric user identifier (QQ-style); search surfaces it. */
  uid: number;
  /** Curated label; additive field (older servers omit it). */
  display_name?: string;
  /** Curated emoji avatar; additive field (older servers omit it). */
  avatar?: string | null;
}

/**
 * GET /api/users/search?q=… (Bearer access).
 *
 * Backend contract: a digits-only `q` searches by UID (exact then prefix),
 * any other `q` by username (exact then prefix). Up to 10 results, the
 * caller is always excluded, an empty `q` → 422, no hits → `[]`.
 */
export function searchUsers(
  accessToken: string,
  query: string,
): Promise<UserSearchResult[]> {
  const q = encodeURIComponent(query);
  return apiRequest<UserSearchResult[]>(`/api/users/search?q=${q}`, {
    method: "GET",
    accessToken,
  });
}
