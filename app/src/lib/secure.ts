/**
 * Refresh-token persistence seam.
 *
 * Under the Tauri desktop shell the token is delegated to the Rust side
 * (`secure_store` / `secure_load` / `secure_clear` commands), which persists it
 * DPAPI-encrypted under the OS app-data directory — never in webview
 * localStorage. In a plain browser (or jsdom tests) we degrade to localStorage
 * under the historical `jiuyue.refresh` key, keeping web semantics identical.
 */
const REFRESH_TOKEN_KEY = "jiuyue.refresh";

/** True when running inside a Tauri webview (the shell injects this global). */
function runningInTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

async function tauriInvoke<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(command, args);
}

/**
 * Persist the refresh token. Under Tauri this writes DPAPI-protected storage
 * and removes any plaintext residue an older localStorage-based build may have
 * left; if the secure path fails we fall back to localStorage so the session
 * keeps working (degraded, logged without any token material).
 */
export async function secureStoreRefresh(token: string): Promise<void> {
  if (!runningInTauri()) {
    localStorage.setItem(REFRESH_TOKEN_KEY, token);
    return;
  }
  try {
    await tauriInvoke("secure_store", { token });
    // Avoid leaving a plaintext copy behind from pre-Tauri sessions.
    localStorage.removeItem(REFRESH_TOKEN_KEY);
  } catch (error) {
    console.error("secure_store failed; degrading to localStorage", error);
    localStorage.setItem(REFRESH_TOKEN_KEY, token);
  }
}

/**
 * Read the persisted refresh token, or `null` when absent. Under Tauri the
 * secure store is authoritative; localStorage is only consulted as a legacy
 * fallback (e.g. first run after upgrading from a browser session).
 */
export async function secureLoadRefresh(): Promise<string | null> {
  if (runningInTauri()) {
    try {
      const stored = await tauriInvoke<string | null>("secure_load");
      if (stored !== null && stored !== undefined) return stored;
    } catch (error) {
      console.error("secure_load failed; falling back to localStorage", error);
    }
  }
  return localStorage.getItem(REFRESH_TOKEN_KEY);
}

/** Remove the persisted refresh token from both stores. */
export async function secureRemoveRefresh(): Promise<void> {
  if (runningInTauri()) {
    try {
      await tauriInvoke("secure_clear");
    } catch (error) {
      console.error("secure_clear failed", error);
    }
  }
  localStorage.removeItem(REFRESH_TOKEN_KEY);
}
