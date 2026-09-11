import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  GroupFileError,
  deleteGroupFile,
  downloadGroupFile,
  GROUP_FILE_MAX_BYTES,
  listGroupFiles,
  parseContentDispositionName,
  saveBlobAs,
  uploadGroupFile,
} from "./groupFiles";

interface ProgressLike {
  lengthComputable: boolean;
  loaded: number;
  total: number;
}

/** Minimal XMLHttpRequest double: the test drives loading/error explicitly. */
class MockXHR {
  static instances: MockXHR[] = [];

  method = "";
  url = "";
  requestHeaders = new Map<string, string>();
  sentBody: unknown = null;
  status = 0;
  responseText = "";
  responseType = "";
  upload: { onprogress: ((event: ProgressLike) => void) | null } = {
    onprogress: null,
  };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor() {
    MockXHR.instances.push(this);
  }

  open(method: string, url: string): void {
    this.method = method;
    this.url = url;
  }

  setRequestHeader(name: string, value: string): void {
    this.requestHeaders.set(name, value);
  }

  send(body: unknown): void {
    this.sentBody = body;
  }

  emitProgress(loaded: number, total: number): void {
    this.upload.onprogress?.({ lengthComputable: true, loaded, total });
  }

  respond(status: number, text: string): void {
    this.status = status;
    this.responseText = text;
    this.onload?.();
  }

  networkFail(): void {
    this.onerror?.();
  }
}

const uploadBody = JSON.stringify({
  file_id: "fid-1",
  name: "报告.pdf",
  mime: "application/pdf",
  bytes: 5,
  created_at: "2026-09-01T00:00:00Z",
  expires_at: null,
});

beforeEach(() => {
  localStorage.clear();
  MockXHR.instances = [];
  vi.stubGlobal("XMLHttpRequest", MockXHR);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("uploadGroupFile", () => {
  it("POSTs the raw bytes with auth/content headers and resolves the 201 body", async () => {
    const file = new File(["hello"], "报告.pdf", { type: "application/pdf" });
    const progress: number[] = [];

    const promise = uploadGroupFile("tok-1", 5, file, (p) => progress.push(p));
    const xhr = MockXHR.instances.at(-1);
    expect(xhr).toBeDefined();
    if (xhr === undefined) throw new Error("no XHR instance");

    expect(xhr.method).toBe("POST");
    expect(xhr.url).toBe("/api/groups/5/files");
    expect(xhr.requestHeaders.get("Authorization")).toBe("Bearer tok-1");
    expect(xhr.requestHeaders.get("Content-Type")).toBe("application/pdf");
    // CJK names are percent-encoded so the Latin-1 header cannot throw.
    expect(xhr.requestHeaders.get("X-File-Name")).toBe(
      encodeURIComponent("报告.pdf"),
    );
    // Original File object travels verbatim — no re-encoding.
    expect(xhr.sentBody).toBe(file);

    xhr.emitProgress(25, 100);
    xhr.emitProgress(100, 100);
    xhr.respond(201, uploadBody);

    await expect(promise).resolves.toEqual({
      file_id: "fid-1",
      name: "报告.pdf",
      mime: "application/pdf",
      bytes: 5,
      created_at: "2026-09-01T00:00:00Z",
      expires_at: null,
    });
    expect(progress).toEqual([25, 100]);
  });

  it("falls back to application/octet-stream when the file has no MIME", () => {
    const file = new File(["x"], "a", { type: "" });
    void uploadGroupFile("tok", 1, file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    expect(xhr.requestHeaders.get("Content-Type")).toBe(
      "application/octet-stream",
    );
  });

  it.each([
    [413, "too_large"],
    [400, "bad_request"],
    [403, "forbidden"],
    [500, "upload_failed"],
  ])("maps HTTP %i to the %s error code", async (status, code) => {
    const file = new File(["x"], "a.png", { type: "image/png" });
    const promise = uploadGroupFile("tok", 5, file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    xhr.respond(status, "{}");

    await expect(promise).rejects.toMatchObject({
      name: "GroupFileError",
      code,
      status,
    });
  });

  it("rejects a 201 with a non-JSON body as upload_failed", async () => {
    const file = new File(["x"], "a.png", { type: "image/png" });
    const promise = uploadGroupFile("tok", 5, file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    xhr.respond(201, "not-json");

    const error = await promise.catch((e: unknown) => e);
    expect(error).toBeInstanceOf(GroupFileError);
    expect((error as GroupFileError).code).toBe("upload_failed");
  });

  it("maps a transport failure to network_error", async () => {
    const file = new File(["x"], "a.png", { type: "image/png" });
    const promise = uploadGroupFile("tok", 5, file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    xhr.networkFail();

    await expect(promise).rejects.toMatchObject({
      code: "network_error",
      status: 0,
    });
  });

  it("exposes a 200 MiB per-file cap", () => {
    expect(GROUP_FILE_MAX_BYTES).toBe(200 * 1024 * 1024);
  });
});

describe("listGroupFiles / deleteGroupFile", () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });

  it("GETs the group listing with Bearer auth", async () => {
    const listing = {
      usage_bytes: 2048,
      quota_bytes: 1024 * 1024 * 1024,
      files: [],
    };
    fetchMock.mockResolvedValue(
      new Response(JSON.stringify(listing), { status: 200 }),
    );

    await expect(listGroupFiles("tok", 5)).resolves.toEqual(listing);

    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups/5/files");
    expect(init.method).toBe("GET");
    expect(new Headers(init.headers).get("Authorization")).toBe("Bearer tok");
  });

  it("POSTs the delete endpoint", async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }));
    await deleteGroupFile("tok", "fid-9");
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups/files/fid-9/delete");
    expect(init.method).toBe("POST");
  });
});

describe("parseContentDispositionName", () => {
  it("prefers the RFC 5987 filename* with UTF-8 decoding", () => {
    expect(
      parseContentDispositionName(
        "attachment; filename=\"report.pdf\"; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf",
      ),
    ).toBe("报告.pdf");
  });

  it("falls back to the quoted filename", () => {
    expect(
      parseContentDispositionName('attachment; filename="report.pdf"'),
    ).toBe("report.pdf");
  });

  it("returns null when the header carries no name", () => {
    expect(parseContentDispositionName(null)).toBeNull();
    expect(parseContentDispositionName("attachment")).toBeNull();
  });
});

describe("downloadGroupFile", () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });

  it("resolves a blob and the server-suggested name", async () => {
    fetchMock.mockResolvedValue(
      new Response("payload", {
        status: 200,
        headers: {
          "Content-Disposition":
            "attachment; filename*=UTF-8''%E6%96%87%E4%BB%B6.bin",
        },
      }),
    );

    const result = await downloadGroupFile("tok-2", "fid-1");
    expect(result.name).toBe("文件.bin");
    expect(result.blob.size).toBe("payload".length);

    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups/files/fid-1");
    expect(new Headers(init.headers).get("Authorization")).toBe("Bearer tok-2");
  });

  it("throws a GroupFileError on a non-2xx response", async () => {
    fetchMock.mockResolvedValue(new Response("nope", { status: 404 }));
    await expect(downloadGroupFile("tok", "fid-x")).rejects.toMatchObject({
      name: "GroupFileError",
      code: "not_found",
      status: 404,
    });
  });
});

describe("saveBlobAs", () => {
  it("creates an object URL, clicks a temp anchor, then revokes it", () => {
    vi.useFakeTimers();
    const createObjectURL = vi.fn(() => "blob:mock");
    const revokeObjectURL = vi.fn();
    vi.stubGlobal("URL", { createObjectURL, revokeObjectURL });
    const click = vi
      .spyOn(HTMLAnchorElement.prototype, "click")
      .mockImplementation(() => {});

    saveBlobAs(new Blob(["x"]), "a.txt");

    expect(createObjectURL).toHaveBeenCalledTimes(1);
    expect(click).toHaveBeenCalledTimes(1);
    const anchors = document.querySelectorAll("a[download='a.txt']");
    expect(anchors).toHaveLength(0);
    expect(revokeObjectURL).not.toHaveBeenCalled();

    vi.runAllTimers();
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:mock");
  });
});
