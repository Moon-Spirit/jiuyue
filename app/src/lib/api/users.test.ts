import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { searchUsers } from "./users";

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("searchUsers — /api/users/search wrapper", () => {
  it("GETs with the Bearer token and the query encoded as q", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, [{ user_id: "u1", username: "alice", uid: 100023 }]),
    );

    const results = await searchUsers("tok-1", "alice");

    expect(results).toEqual([
      { user_id: "u1", username: "alice", uid: 100023 },
    ]);
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/users/search?q=alice");
    expect(init.method).toBe("GET");
    expect((init.headers as Headers).get("Authorization")).toBe("Bearer tok-1");
  });

  it("URL-encodes special characters in the query", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []));

    await searchUsers("tok-1", "user name&more");

    const [path] = fetchMock.mock.calls[0] as [string];
    expect(path).toBe("/api/users/search?q=user%20name%26more");
  });

  it("a digits-only query passes through unchanged (UID search)", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []));

    await searchUsers("tok-1", "100023");

    const [path] = fetchMock.mock.calls[0] as [string];
    expect(path).toBe("/api/users/search?q=100023");
  });
});
