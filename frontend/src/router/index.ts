import { createRouter, createWebHistory } from "vue-router";
import { useAuthStore } from "../stores/auth";
import { pinia } from "../stores/pinia";

const router = createRouter({
  history: createWebHistory(import.meta.env.BASE_URL),
  routes: [
    {
      path: "/",
      name: "chat",
      component: () => import("../views/ChatView.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/account",
      name: "account",
      component: () => import("../views/ProtectedView.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/login",
      name: "login",
      component: () => import("../views/LoginView.vue"),
    },
    {
      path: "/register",
      name: "register",
      component: () => import("../views/RegisterView.vue"),
    },
    {
      // The three email journeys. All are reachable without a session: a link
      // from a mail client is followed in whatever browser opened it.
      path: "/verify-email",
      name: "verify-email",
      component: () => import("../views/VerifyEmailView.vue"),
    },
    {
      path: "/forgot-password",
      name: "forgot-password",
      component: () => import("../views/ForgotPasswordView.vue"),
    },
    {
      path: "/reset-password",
      name: "reset-password",
      component: () => import("../views/ResetPasswordView.vue"),
    },
    {
      path: "/health",
      name: "health",
      component: () => import("../views/HealthView.vue"),
    },
  ],
});

router.beforeEach((to) => {
  const auth = useAuthStore(pinia);

  if (to.meta.requiresAuth === true && !auth.isAuthenticated) {
    return { name: "login", query: { redirect: to.fullPath } };
  }

  // Someone already signed in has no use for the login or register form.
  if ((to.name === "login" || to.name === "register") && auth.isAuthenticated) {
    return { name: "chat" };
  }

  return true;
});

export default router;
