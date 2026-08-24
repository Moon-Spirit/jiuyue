/**
 * Runtime API endpoint resolution.
 *
 * - Web dev/build: empty base — the Vite dev proxy (and same-origin deploys)
 *   forwards `/api` and `/ws` to the backend.
 * - Packaged Tauri apps: no proxy exists, so default to the local backend;
 *   `localStorage["jiuyue.apiBase"]` overrides for remote deployments.
 */

export function apiBase(): string {
  const override =
    typeof localStorage !== "undefined"
      ? localStorage.getItem("jiuyue.apiBase")
      : null;
  if (override !== null && override !== "") {
    return override.replace(/\/+$/, "");
  }
  const isTauri =
    typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  return isTauri ? "http://127.0.0.1:8080" : "";
}

/** WebSocket counterpart of {@link apiBase}: http(s)→ws(s), empty stays relative. */
export function wsBase(): string {
  const base = apiBase();
  if (base === "") return "";
  return base.replace(/^http/, "ws");
}
