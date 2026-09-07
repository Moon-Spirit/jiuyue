import { apiRequest, ApiError } from "./client";
import { apiErrorMessage } from "./messages";

type Translate = (key: string) => string;

/** Shared `{ user_id, username }` shape returned across the friends API. */
export interface FriendUser {
  user_id: string;
  username: string;
  /** Numeric user identifier (QQ-style). Present on newer server responses. */
  uid?: number;
  /** Curated label; additive field (older servers omit it). */
  display_name?: string;
  /** Curated emoji avatar; additive field (older servers omit it). */
  avatar?: string | null;
}

/** POST /api/friends/requests → 201 { request_id, to } */
export interface SendFriendRequestResult {
  request_id: string;
  to: FriendUser;
}

/** GET /api/friends/requests → incoming entry (someone asked ME). */
export interface IncomingFriendRequest {
  request_id: string;
  from: FriendUser;
  /** RFC 3339 timestamp, passed through verbatim. */
  created_at: string;
}

/** GET /api/friends/requests → outgoing entry (I asked someone). */
export interface OutgoingFriendRequest {
  request_id: string;
  to: FriendUser;
  created_at: string;
}

/** GET /api/friends/requests response body. */
export interface FriendRequestLists {
  incoming: IncomingFriendRequest[];
  outgoing: OutgoingFriendRequest[];
}

/** POST /api/friends/requests/{id}/accept → { friend } */
export interface AcceptFriendRequestResult {
  friend: FriendUser;
}

/** GET /api/friends → established friendships. */
export interface Friend {
  user_id: string;
  username: string;
  /** Numeric user identifier; the add-friend flow relies on it. */
  uid?: number;
  /** RFC 3339 timestamp of when the friendship was established. */
  since: string;
  /** Curated label; additive field (older servers omit it). */
  display_name?: string;
  /** Curated emoji avatar; additive field (older servers omit it). */
  avatar?: string | null;
}

/**
 * POST /api/friends/requests { username } (Bearer access).
 * Errors: 404 peer_not_found · 400 self_request ·
 * 409 already_friends | request_already_pending.
 */
export function sendFriendRequest(
  accessToken: string,
  username: string,
): Promise<SendFriendRequestResult> {
  return apiRequest<SendFriendRequestResult>("/api/friends/requests", {
    method: "POST",
    body: { username },
    accessToken,
  });
}

/** GET /api/friends/requests (Bearer access) — incoming + outgoing lists. */
export function listFriendRequests(
  accessToken: string,
): Promise<FriendRequestLists> {
  return apiRequest<FriendRequestLists>("/api/friends/requests", {
    method: "GET",
    accessToken,
  });
}

/** POST /api/friends/requests/{id}/accept (Bearer access) → the new friend. */
export function acceptFriendRequest(
  accessToken: string,
  requestId: string,
): Promise<AcceptFriendRequestResult> {
  return apiRequest<AcceptFriendRequestResult>(
    `/api/friends/requests/${encodeURIComponent(requestId)}/accept`,
    { method: "POST", accessToken },
  );
}

/** POST /api/friends/requests/{id}/decline (Bearer access) → 204. */
export function declineFriendRequest(
  accessToken: string,
  requestId: string,
): Promise<void> {
  return apiRequest<void>(
    `/api/friends/requests/${encodeURIComponent(requestId)}/decline`,
    { method: "POST", accessToken },
  );
}

/** DELETE /api/friends/requests/{id} (Bearer access) — cancel outgoing → 204. */
export function cancelFriendRequest(
  accessToken: string,
  requestId: string,
): Promise<void> {
  return apiRequest<void>(
    `/api/friends/requests/${encodeURIComponent(requestId)}`,
    { method: "DELETE", accessToken },
  );
}

/** GET /api/friends (Bearer access) — established friend list. */
export function listFriends(accessToken: string): Promise<Friend[]> {
  return apiRequest<Friend[]>("/api/friends", {
    method: "GET",
    accessToken,
  });
}

/** DELETE /api/friends/{userId} (Bearer access) — remove a friend → 204. */
export function unfriend(accessToken: string, userId: string): Promise<void> {
  return apiRequest<void>(`/api/friends/${encodeURIComponent(userId)}`, {
    method: "DELETE",
    accessToken,
  });
}

/**
 * Maps an add-friend failure to a localized human message using the
 * backend's machine error code; falls back to the shared mapper.
 */
export function friendApiErrorMessage(
  error: unknown,
  translate: Translate,
): string {
  if (!(error instanceof ApiError)) return translate("errors.unknown");
  switch (error.machine) {
    case "self_request":
      return translate("errors.self_request");
    case "already_friends":
      return translate("errors.already_friends");
    case "request_already_pending":
      return translate("errors.request_already_pending");
    default:
      return apiErrorMessage(error, translate);
  }
}
