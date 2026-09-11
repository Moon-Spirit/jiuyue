import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  MAX_IMAGE_BYTES,
  MAX_VIDEO_BYTES,
  MediaUploadError,
  checkMediaFile,
  formatBytes,
  mediaKindFor,
  uploadMedia,
} from "./media";

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

beforeEach(() => {
  localStorage.clear();
  MockXHR.instances = [];
  vi.stubGlobal("XMLHttpRequest", MockXHR);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("mediaKindFor", () => {
  it("recognizes the supported image and video MIME types", () => {
    expect(mediaKindFor("image/png")).toBe("image");
    expect(mediaKindFor("image/jpeg")).toBe("image");
    expect(mediaKindFor("image/webp")).toBe("image");
    expect(mediaKindFor("image/gif")).toBe("image");
    expect(mediaKindFor("video/mp4")).toBe("video");
    expect(mediaKindFor("video/quicktime")).toBe("video");
    expect(mediaKindFor("video/webm")).toBe("video");
    expect(mediaKindFor("application/pdf")).toBeNull();
    expect(mediaKindFor("")).toBeNull();
  });
});

describe("checkMediaFile", () => {
  it("accepts supported files at or under the per-kind limit", () => {
    expect(
      checkMediaFile({ type: "image/png", size: MAX_IMAGE_BYTES }),
    ).toEqual({ kind: "image", error: null });
    expect(
      checkMediaFile({ type: "video/mp4", size: MAX_VIDEO_BYTES }),
    ).toEqual({ kind: "video", error: null });
  });

  it("flags files over the limit as too_large", () => {
    expect(
      checkMediaFile({ type: "image/png", size: MAX_IMAGE_BYTES + 1 }),
    ).toEqual({ kind: "image", error: "too_large" });
    expect(
      checkMediaFile({ type: "video/mp4", size: MAX_VIDEO_BYTES + 1 }),
    ).toEqual({ kind: "video", error: "too_large" });
  });

  it("flags unsupported MIME types before any size check", () => {
    expect(checkMediaFile({ type: "application/zip", size: 1 })).toEqual({
      kind: null,
      error: "unsupported_type",
    });
  });
});

describe("formatBytes", () => {
  it("renders byte sizes with binary units", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(MAX_IMAGE_BYTES)).toBe("15 MB");
    expect(formatBytes(MAX_VIDEO_BYTES)).toBe("200 MB");
  });
});

describe("uploadMedia", () => {
  it("POSTs the raw bytes with auth/content headers and resolves the 201 body", async () => {
    const file = new File(["hello"], "photo.png", { type: "image/png" });
    const progress: number[] = [];

    const promise = uploadMedia("tok-1", file, (p) => progress.push(p));
    const xhr = MockXHR.instances.at(-1);
    expect(xhr).toBeDefined();
    if (xhr === undefined) throw new Error("no XHR instance");

    expect(xhr.method).toBe("POST");
    expect(xhr.url).toBe("/api/media");
    expect(xhr.requestHeaders.get("Authorization")).toBe("Bearer tok-1");
    expect(xhr.requestHeaders.get("Content-Type")).toBe("image/png");
    expect(xhr.requestHeaders.get("X-File-Name")).toBe(
      encodeURIComponent("photo.png"),
    );
    // Original File object travels verbatim — no re-encoding/compression.
    expect(xhr.sentBody).toBe(file);

    xhr.emitProgress(25, 100);
    xhr.emitProgress(100, 100);
    xhr.respond(
      201,
      JSON.stringify({
        media_id: "mid-1",
        kind: "image",
        mime: "image/png",
        bytes: 5,
        file_name: "photo.png",
      }),
    );

    await expect(promise).resolves.toEqual({
      media_id: "mid-1",
      kind: "image",
      mime: "image/png",
      bytes: 5,
      file_name: "photo.png",
    });
    expect(progress).toEqual([25, 100]);
  });

  it("percent-encodes CJK file names so the Latin-1 header cannot throw", async () => {
    // Regression: raw "截图.png" throws a ByteString TypeError inside
    // setRequestHeader on Chromium/WebView2, killing the upload before it
    // even leaves the client (observed in the packaged desktop app).
    const file = new File(["x"], "截图.png", { type: "image/png" });
    const promise = uploadMedia("tok", file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    expect(xhr.requestHeaders.get("X-File-Name")).toBe(
      encodeURIComponent("截图.png"),
    );
    xhr.respond(
      201,
      JSON.stringify({
        media_id: "mid-2",
        kind: "image",
        mime: "image/png",
        bytes: 1,
        file_name: "截图.png",
      }),
    );
    await expect(promise).resolves.toMatchObject({ media_id: "mid-2" });
  });

  it.each([
    [413, "too_large"],
    [415, "unsupported_type"],
    [400, "bad_request"],
    [500, "upload_failed"],
  ])("maps HTTP %i to the %s error code", async (status, code) => {
    const file = new File(["x"], "a.png", { type: "image/png" });
    const promise = uploadMedia("tok", file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    xhr.respond(status, "{}");

    await expect(promise).rejects.toMatchObject({
      name: "MediaUploadError",
      code,
      status,
    });
  });

  it("rejects a 201 with a non-JSON body as upload_failed", async () => {
    const file = new File(["x"], "a.png", { type: "image/png" });
    const promise = uploadMedia("tok", file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    xhr.respond(201, "not-json");

    const error = await promise.catch((e: unknown) => e);
    expect(error).toBeInstanceOf(MediaUploadError);
    expect((error as MediaUploadError).code).toBe("upload_failed");
  });

  it("maps a transport failure to network_error", async () => {
    const file = new File(["x"], "a.png", { type: "image/png" });
    const promise = uploadMedia("tok", file);
    const xhr = MockXHR.instances.at(-1);
    if (xhr === undefined) throw new Error("no XHR instance");
    xhr.networkFail();

    await expect(promise).rejects.toMatchObject({
      code: "network_error",
      status: 0,
    });
  });
});
