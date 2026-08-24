import { apiRequest } from "./client";

export type AuthChannel = "email" | "phone";

export interface RequestCodeResult {
  expires_in_secs: number;
}

export interface TokenPair {
  access_token: string;
  refresh_token: string;
  expires_in: number;
}

export interface LoginResult extends TokenPair {
  user_id: string;
  username: string;
}

export interface RegisterResult extends TokenPair {
  user_id: string;
  username: string;
}

export interface RegisterInput {
  channel: AuthChannel;
  target: string;
  code: string;
  username: string;
  password: string;
}

export interface WsTicketResult {
  ticket: string;
}

/** POST /api/auth/request-code → { expires_in_secs } */
export function requestCode(
  channel: AuthChannel,
  target: string,
): Promise<RequestCodeResult> {
  return apiRequest<RequestCodeResult>("/api/auth/request-code", {
    method: "POST",
    body: { channel, target },
  });
}

/** POST /api/auth/register → 201 { user_id, username, access_token, refresh_token, expires_in } */
export function register(input: RegisterInput): Promise<RegisterResult> {
  return apiRequest<RegisterResult>("/api/auth/register", {
    method: "POST",
    body: input,
  });
}

/** POST /api/auth/login → 200 { access_token, refresh_token, expires_in } */
export function login(
  identifier: string,
  password: string,
): Promise<LoginResult> {
  return apiRequest<LoginResult>("/api/auth/login", {
    method: "POST",
    body: { identifier, password },
  });
}

/** POST /api/auth/refresh → 200 token pair; consumes the old refresh token. */
export function refreshSession(refreshToken: string): Promise<TokenPair> {
  return apiRequest<TokenPair>("/api/auth/refresh", {
    method: "POST",
    body: { refresh_token: refreshToken },
  });
}

/** POST /api/auth/ws-ticket (Bearer access) → 200 { ticket } */
export function requestWsTicket(accessToken: string): Promise<WsTicketResult> {
  return apiRequest<WsTicketResult>("/api/auth/ws-ticket", {
    method: "POST",
    accessToken,
  });
}
