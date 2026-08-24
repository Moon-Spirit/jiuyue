import { apiRequest, ApiError } from "./client";

type Translate = (key: string) => string;

export interface ConversationPeer {
  /** Server-side user UUID (a string, never a number). */
  user_id: string;
  username: string;
}

/** Conversation kind: plain direct chat or end-to-end encrypted secret chat. */
export type ConversationKind = "direct" | "secret";

/** POST /api/conversations → 201 { conversation_id, peer, created, kind } */
export interface CreateConversationResult {
  conversation_id: number;
  peer: ConversationPeer;
  /** False when the conversation already existed (idempotent open). */
  created: boolean;
  /** Echoed by the server ("secret" when the secret kind was requested). */
  kind: string;
}

/**
 * POST /api/conversations { peer_username, kind? } (Bearer access).
 * `kind` is only sent when "secret" — omitting it keeps M1/M2 request
 * bodies byte-identical for plain conversations.
 */
export function createConversation(
  accessToken: string,
  peerUsername: string,
  kind: ConversationKind = "direct",
): Promise<CreateConversationResult> {
  return apiRequest<CreateConversationResult>("/api/conversations", {
    method: "POST",
    body:
      kind === "secret"
        ? { peer_username: peerUsername, kind }
        : { peer_username: peerUsername },
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
    case "no_one_time_keys":
      return translate("errors.no_one_time_keys");
    case "bad_request":
      return translate("errors.bad_request");
    case "network_error":
      return translate("errors.network_error");
    default:
      return translate("errors.unknown");
  }
}
