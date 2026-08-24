import { ref, watchEffect } from "vue";

export type ThemePreference = "light" | "dark" | "system";

const THEME_KEY = "jiuyue.theme";
const PREFERENCE_ORDER: readonly ThemePreference[] = [
  "light",
  "dark",
  "system",
];

function readStoredPreference(): ThemePreference {
  try {
    const raw = localStorage.getItem(THEME_KEY);
    if (raw === "light" || raw === "dark" || raw === "system") return raw;
  } catch {
    // Storage unavailable (private mode etc.) — fall back to system.
  }
  return "system";
}

function systemPrefersDark(): boolean {
  if (typeof window.matchMedia !== "function") return false;
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

function applyTheme(preference: ThemePreference): void {
  const dark =
    preference === "dark" || (preference === "system" && systemPrefersDark());
  document.documentElement.classList.toggle("dark", dark);
}

const preference = ref<ThemePreference>(readStoredPreference());

let initialized = false;

/**
 * Class-strategy dark mode (`html.dark`) shared app-wide.
 * The pre-paint script in index.html applies the stored choice before first
 * paint; this composable keeps it in sync afterwards.
 */
export function useTheme() {
  if (!initialized) {
    initialized = true;
    watchEffect(() => {
      applyTheme(preference.value);
      try {
        localStorage.setItem(THEME_KEY, preference.value);
      } catch {
        // Persisting the preference is best-effort.
      }
    });
    if (typeof window.matchMedia === "function") {
      window
        .matchMedia("(prefers-color-scheme: dark)")
        .addEventListener("change", () => {
          if (preference.value === "system") applyTheme("system");
        });
    }
  }

  function cycle(): void {
    const index = PREFERENCE_ORDER.indexOf(preference.value);
    preference.value = PREFERENCE_ORDER[(index + 1) % PREFERENCE_ORDER.length];
  }

  return { preference, cycle };
}
