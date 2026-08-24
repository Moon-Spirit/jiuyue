import { afterEach, describe, expect, it } from "vitest";
import { apiBase, wsBase } from "./apiConfig";

function setTauri(value: boolean): void {
  if (value) {
    (window as Record<string, unknown>)["__TAURI_INTERNALS__"] = {};
  } else {
    delete (window as Record<string, unknown>)["__TAURI_INTERNALS__"];
  }
}

afterEach(() => {
  localStorage.removeItem("jiuyue.apiBase");
  setTauri(false);
});

describe("apiConfig", () => {
  it("web build uses relative base (vite proxy / same-origin)", () => {
    expect(apiBase()).toBe("");
    expect(wsBase()).toBe("");
  });

  it("packaged tauri app defaults to the local backend", () => {
    setTauri(true);
    expect(apiBase()).toBe("http://127.0.0.1:8080");
    expect(wsBase()).toBe("ws://127.0.0.1:8080");
  });

  it("explicit override wins over the tauri default and strips trailing slashes", () => {
    setTauri(true);
    localStorage.setItem("jiuyue.apiBase", "https://api.jiuyue.example///");
    expect(apiBase()).toBe("https://api.jiuyue.example");
    expect(wsBase()).toBe("wss://api.jiuyue.example");
  });
});
