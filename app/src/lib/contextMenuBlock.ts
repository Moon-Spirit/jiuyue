/**
 * App-wide right-click interception ("右键拦截").
 *
 * The desktop shell must feel native: a right-click anywhere in the window
 * suppresses the browser/WebView context menu (back / reload / inspect …)
 * instead of leaking it into the app. We only cancel the `contextmenu` event,
 * so clipboard shortcuts (Ctrl/Cmd+V) and every other keyboard path keep
 * working untouched.
 *
 * Installed once from `main.ts`; returns a disposer so tests (and any future
 * teardown path) can restore the default behavior.
 */
export function installContextMenuBlock(
  target: EventTarget = document,
): () => void {
  const handler = (event: Event): void => {
    event.preventDefault();
  };
  target.addEventListener("contextmenu", handler);
  return () => {
    target.removeEventListener("contextmenu", handler);
  };
}
