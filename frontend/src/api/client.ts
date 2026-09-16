/**
 * Minimal typed wrapper around `fetch` for same-origin backend calls.
 *
 * Every request goes through the `/api` prefix: in dev the Vite server proxies
 * `/api/*` to the backend with the prefix stripped, and production routes the
 * same way — so this module is the only place that knows the wire prefix.
 */

/** Thrown when the backend answers with a non-2xx status. */
export class ApiError extends Error {
  readonly status: number;

  constructor(status: number) {
    super(`Request failed with status ${status}`);
    this.name = "ApiError";
    this.status = status;
  }
}

export async function apiGet<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`/api${path}`, {
    headers: { Accept: "application/json" },
    ...init,
  });

  if (!response.ok) {
    throw new ApiError(response.status);
  }

  // The JSON shape is a runtime contract owned by the caller's type argument;
  // this is the single boundary where it is asserted.
  const payload: unknown = await response.json();
  return payload as T;
}
