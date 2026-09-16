import { createPinia } from "pinia";

/**
 * The single Pinia instance for the application.
 *
 * Exported so code that runs outside a component — the router's navigation
 * guard — can resolve stores explicitly instead of relying on whichever Pinia
 * happens to be active. Tests keep creating their own instances; this is only
 * for the real app.
 */
export const pinia = createPinia();
