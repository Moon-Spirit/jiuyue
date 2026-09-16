import { defineStore } from "pinia";
import { ref } from "vue";
import { ApiError, apiGet } from "../api/client";

/** Payload served by the backend's `GET /health`. */
export interface HealthPayload {
  status: string;
  version: string;
}

export const useHealthStore = defineStore("health", () => {
  const status = ref<string | null>(null);
  const version = ref<string | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);

  /** Calls `GET /api/health` and records the result (success or failure). */
  async function fetchHealth(): Promise<void> {
    loading.value = true;
    error.value = null;

    try {
      const payload = await apiGet<HealthPayload>("/health");
      status.value = payload.status;
      version.value = payload.version;
    } catch (cause) {
      status.value = null;
      version.value = null;
      error.value =
        cause instanceof ApiError
          ? `服务端响应异常（HTTP ${cause.status}）`
          : "无法连接后端服务";
    } finally {
      loading.value = false;
    }
  }

  return { status, version, loading, error, fetchHealth };
});
