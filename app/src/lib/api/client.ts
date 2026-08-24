/**
 * Minimal JSON fetch wrapper for the JiuYue HTTP API.
 *
 * - Relative paths only: the Vite dev proxy forwards `/api` to the backend.
 * - `credentials: "omit"`: auth travels exclusively via Bearer tokens.
 * - Non-2xx responses are normalized into {@link ApiError} carrying the
 *   backend's machine-readable error code (`{"error","message"}`).
 */

/** Machine codes emitted by the backend auth/chat HTTP surface. */
export type ApiErrorCode =
  | "invalid_credentials"
  | "username_taken"
  | "identity_already_bound"
  | "validation_error"
  | "peer_not_found"
  | "bad_request";

export class ApiError extends Error {
  readonly status: number;
  /** Machine code from the server; `"network_error"`/`"unknown_error"` when absent. */
  readonly machine: string;

  constructor(status: number, machine: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.machine = machine;
  }
}

interface RequestOptions {
  method?: "GET" | "POST" | "PUT" | "DELETE";
  body?: unknown;
  accessToken?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

async function parseJsonBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (text.length === 0) return undefined;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    // Non-JSON body: surfaced as an opaque payload instead of crashing callers.
    return undefined;
  }
}

export async function apiRequest<T>(
  path: string,
  options: RequestOptions = {},
): Promise<T> {
  const { method = "GET", body, accessToken } = options;

  const headers = new Headers();
  if (body !== undefined) headers.set("Content-Type", "application/json");
  if (accessToken !== undefined)
    headers.set("Authorization", `Bearer ${accessToken}`);

  let response: Response;
  try {
    response = await fetch(path, {
      method,
      headers,
      credentials: "omit",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch (cause) {
    const detail = cause instanceof Error ? cause.message : "network failure";
    throw new ApiError(0, "network_error", detail);
  }

  const payload = await parseJsonBody(response);

  if (!response.ok) {
    let machine = "unknown_error";
    let message = response.statusText;
    if (isRecord(payload)) {
      if (typeof payload.error === "string") machine = payload.error;
      if (typeof payload.message === "string") message = payload.message;
    }
    throw new ApiError(response.status, machine, message);
  }

  return payload as T;
}
