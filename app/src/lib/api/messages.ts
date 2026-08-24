import { apiRequest, ApiError } from "./client";

type Translate = (key: string) => string;

export interface ConversationPeer {
  /** Server-side user UUID (a string, never a number). */
  user_id: string;
  username: string;
}

/** POST /api/conversations → 201 { conversation_id, peer, created } */
export interface CreateConversationResult {
  conversation_id: number;
  peer: ConversationPeer;
  /** False when the conversation already existed (idempotent open). */
  created: boolean;
}

/** POST /api/conversations { peer_username } (Bearer access). */
export function createConversation(
  accessToken: string,
  peerUsername: string,
): Promise<CreateConversationResult> {
  return apiRequest<CreateConversationResult>("/api/conversations", {
    method: "POST",
    body: { peer_username: peerUsername },
    accessToken,
  });
}

/** GET /api/conversations (Bearer access) — bootstraps fresh sessions. */
export function listConversations(
  accessToken: string,
): Promise<ConversationListItem[]> {
  return apiRequest<ConversationListItem[]>("/api/conversations", {
    method: "GET",
    accessToken,
  });
}

/** GET /api/conversations — 200 ConversationListItem[] (memberships, newest first). */
export interface ConversationListItem {
  conversation_id: number;
  kind: string;
  peer: ConversationPeer | null;
  last_seq: number;
  last_delivered_seq: number;
}

/**
 * Maps an API failure to a localized human message using the backend's
 * machine error code; falls back to a generic message for unknown codes.
 */
export function apiErrorMessage(error: unknown, translate: Translate): string {
  if (!(error instanceof ApiError)) return translate("errors.unknown");
  switch (error.machine) {
    case "invalid_credentials":
      return translate("errors.invalid_credentials");
    case "username_taken":
      return translate("errors.username_taken");
    case "identity_already_bound":
      return translate("errors.identity_already_bound");
    case "validation_error":
      return translate("errors.validation_error");
    case "peer_not_found":
      return translate("errors.peer_not_found");
    case "bad_request":
      return translate("errors.bad_request");
    case "network_error":
      return translate("errors.network_error");
    default:
      return translate("errors.unknown");
  }
}
