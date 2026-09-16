import { createApp } from "vue";
import App from "./App.vue";
import router from "./router";
import { useAuthStore } from "./stores/auth";
import { pinia } from "./stores/pinia";
import "./style.css";

const app = createApp(App).use(pinia).use(router);

// Restore a persisted session before the first navigation, so the route guard
// sees the real auth state instead of bouncing a valid session to /login.
// `restore()` reports its own failures and never rejects, so mounting is safe.
void useAuthStore(pinia)
  .restore()
  .finally(() => {
    app.mount("#app");
  });
