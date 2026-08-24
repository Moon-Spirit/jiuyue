import { createRouter, createWebHistory } from "vue-router";
import { useAuthStore } from "../stores/auth";

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: "/",
      name: "root",
      redirect: () => {
        const auth = useAuthStore();
        return auth.status === "authed" ? { name: "chat" } : { name: "login" };
      },
    },
    {
      path: "/login",
      name: "login",
      component: () => import("../views/LoginView.vue"),
      meta: { guestOnly: true },
    },
    {
      path: "/register",
      name: "register",
      component: () => import("../views/RegisterView.vue"),
      meta: { guestOnly: true },
    },
    {
      path: "/chat",
      name: "chat",
      component: () => import("../views/ChatView.vue"),
      meta: { requiresAuth: true },
    },
    {
      path: "/contacts",
      name: "contacts",
      component: () => import("../views/ContactsView.vue"),
      meta: { requiresAuth: true },
    },
    { path: "/:pathMatch(.*)*", redirect: "/" },
  ],
});

router.beforeEach((to) => {
  const auth = useAuthStore();
  if (to.meta.requiresAuth === true && auth.status !== "authed") {
    return { name: "login" };
  }
  if (to.meta.guestOnly === true && auth.status === "authed") {
    return { name: "chat" };
  }
  return true;
});
