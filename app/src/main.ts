import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import { router } from "./router";
import { i18n } from "./i18n";
import { installContextMenuBlock } from "./lib/contextMenuBlock";
import { useAuthStore } from "./stores/auth";
import "./styles/app.css";

// Native-feeling desktop shell: the browser/WebView context menu never opens.
installContextMenuBlock();

async function bootstrap(): Promise<void> {
  const app = createApp(App);
  const pinia = createPinia();
  app.use(pinia);

  // Restore the session (refresh flow) before the first navigation so the
  // router guards see the final auth status.
  await useAuthStore().restoreSession();

  app.use(router).use(i18n);
  app.mount("#app");
}

void bootstrap();
