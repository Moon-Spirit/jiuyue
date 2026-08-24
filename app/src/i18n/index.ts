import { createI18n } from "vue-i18n";
import zhCN from "./locales/zh-CN";
import en from "./locales/en";

export type MessageSchema = typeof zhCN;

export type AppLocale = "zh-CN" | "en";

const LOCALE_KEY = "jiuyue.lang";

function initialLocale(): AppLocale {
  try {
    const stored = localStorage.getItem(LOCALE_KEY);
    if (stored === "zh-CN" || stored === "en") return stored;
  } catch {
    // Storage unavailable — fall back to the default locale.
  }
  return "zh-CN";
}

export function persistLocale(locale: AppLocale): void {
  try {
    localStorage.setItem(LOCALE_KEY, locale);
  } catch {
    // Persisting the choice is best-effort.
  }
}

export const i18n = createI18n<[MessageSchema], AppLocale>({
  legacy: false,
  locale: initialLocale(),
  fallbackLocale: "en",
  messages: {
    "zh-CN": zhCN,
    en,
  },
});
