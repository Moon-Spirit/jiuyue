/**
 * Minimal typed wrapper around `fetch` for same-origin backend calls.
 *
 * Every request goes through the `/api` prefix: in dev the Vite server proxies
 * `/api/*` to the backend with the prefix stripped, and production routes the
 * same way — so this module is the only place that knows the wire prefix.
 *
 * Failures are turned into an {@link ApiError} that carries the backend's
 * machine-readable `ErrorBody` when one was returned. That is what lets a 409
 * "email already registered" become a field message instead of a status code the
 * caller has to interpret.
 */

import type { ErrorBody } from "../generated/ErrorBody";

/** Thrown when the backend answers with a non-2xx status. */
export class ApiError extends Error {
  readonly status: number;
  /** The parsed error contract, or `null` when the response was not one. */
  readonly body: ErrorBody | null;

  constructor(status: number, body: ErrorBody | null = null) {
    super(`Request failed with status ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }

  /**
   * Whole seconds the backend asked the caller to wait, when it asked for one.
   *
   * Throttling and lockouts carry this so the UI can render a real countdown
   * instead of parsing a human message; every other failure answers `null`. The
   * value is read defensively because a response from an older backend, or one
   * behind a proxy that rewrote the body, may simply lack the field.
   */
  get retryAfterSeconds(): number | null {
    const value: unknown = this.body?.error.retry_after_seconds;
    return typeof value === "number" ? value : null;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** Whether a decoded payload has the `{"error": {"code": ...}}` shape. */
function isErrorBody(value: unknown): value is ErrorBody {
  if (!isRecord(value)) return false;
  const detail = value["error"];
  return isRecord(detail) && typeof detail["code"] === "string";
}

/**
 * Read the contract error body, or `null` when the response was not JSON or not
 * an `ErrorBody` (a proxy's HTML 502, for instance). The status code is always
 * available to the caller, so an unparseable body loses nothing essential.
 */
async function readErrorBody(response: Response): Promise<ErrorBody | null> {
  try {
    const payload: unknown = await response.json();
    return isErrorBody(payload) ? payload : null;
  } catch (cause) {
    // Not JSON at all; nothing to recover, and the status is already known.
    void cause;
    return null;
  }
}

/** `GET /api<path>`, preserving the caller's optional `RequestInit`. */
export async function apiGet<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`/api${path}`, {
    headers: { Accept: "application/json" },
    ...init,
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }

  // The JSON shape is a runtime contract owned by the caller's type argument;
  // this is the single boundary where it is asserted.
  const payload: unknown = await response.json();
  return payload as T;
}

/** `POST /api<path>` with a JSON body and optional bearer token. */
export async function apiPost<T>(
  path: string,
  body: unknown,
  token?: string,
): Promise<T> {
  const headers: Record<string, string> = {
    Accept: "application/json",
    "Content-Type": "application/json",
  };
  if (token !== undefined) headers["Authorization"] = `Bearer ${token}`;

  const response = await fetch(`/api${path}`, {
    method: "POST",
    headers,
    body: JSON.stringify(body),
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }

  const payload: unknown = await response.json();
  return payload as T;
}

/** `GET /api<path>` with a bearer token. */
export async function apiGetAuthed<T>(path: string, token: string): Promise<T> {
  const response = await fetch(`/api${path}`, {
    headers: { Accept: "application/json", Authorization: `Bearer ${token}` },
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }

  const payload: unknown = await response.json();
  return payload as T;
}

/** `POST /api<path>` with a bearer token and no body (expects no content back). */
export async function apiPostAuthedNoContent(
  path: string,
  token: string,
): Promise<void> {
  const response = await fetch(`/api${path}`, {
    method: "POST",
    headers: { Accept: "application/json", Authorization: `Bearer ${token}` },
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }
}

/** `POST /api<path>` with a bearer token and a JSON body. */
export async function apiPostAuthed<T>(
  path: string,
  body: unknown,
  token: string,
): Promise<T> {
  const response = await fetch(`/api${path}`, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(body),
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }

  const payload: unknown = await response.json();
  return payload as T;
}

/** `PATCH /api<path>` with a bearer token and a JSON body. */
export async function apiPatchAuthed<T>(
  path: string,
  body: unknown,
  token: string,
): Promise<T> {
  const response = await fetch(`/api${path}`, {
    method: "PATCH",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(body),
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }

  const payload: unknown = await response.json();
  return payload as T;
}

/** `DELETE /api<path>` with a bearer token, expecting a JSON body back. */
export async function apiDeleteAuthed<T>(
  path: string,
  token: string,
): Promise<T> {
  const response = await fetch(`/api${path}`, {
    method: "DELETE",
    headers: { Accept: "application/json", Authorization: `Bearer ${token}` },
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }

  const payload: unknown = await response.json();
  return payload as T;
}

/** `DELETE /api<path>` with a bearer token and no content expected back. */
export async function apiDeleteAuthedNoContent(
  path: string,
  token: string,
): Promise<void> {
  const response = await fetch(`/api${path}`, {
    method: "DELETE",
    headers: { Accept: "application/json", Authorization: `Bearer ${token}` },
  });

  if (!response.ok) {
    throw new ApiError(response.status, await readErrorBody(response));
  }
}
