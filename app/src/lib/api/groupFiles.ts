/**
 * Group files API client (M13b).
 *
 * - `POST /api/groups/{id}/files` with the RAW file bytes as the body (no
 *   multipart), mirroring {@link uploadMedia}: auth via `Authorization`,
 *   content type via `Content-Type`, UTF-8 name percent-encoded into
 *   `X-File-Name`. Progress comes from `XMLHttpRequest.upload.onprogress`.
 * - `GET /api/groups/{id}/files` lists the group's files + usage/quota.
 * - `GET /api/groups/files/{fileId}` streams the bytes (Bearer auth, fetch →
 *   Blob) with a `Content-Disposition` suggested name.
 * - `POST /api/groups/files/{fileId}/delete` removes a file (204).
 *
 * The backend caps a single file at {@link GROUP_FILE_MAX_BYTES} and rejects
 * oversized uploads with `413 too_large`; the limit is mirrored client-side so
 * an obviously-too-large file never leaves the device.
 */

import { apiBase } from "../apiConfig";
import { apiRequest } from "./client";

/** Per-file upload cap (matches the backend's 413 `too_large`). */
export const GROUP_FILE_MAX_BYTES = 200 * 1024 * 1024;

/** Author of a stored group file. */
export interface GroupFileUploader {
  user_id: string;
  username: string;
  /** Curated label; additive field (older servers omit it). */
  display_name?: string;
}

/** One file row from `GET /api/groups/{id}/files`. */
export interface GroupFile {
  file_id: string;
  name: string;
  mime: string;
  bytes: number;
  uploader: GroupFileUploader;
  /** RFC 3339 timestamp, passed through verbatim. */
  created_at: string;
  /** Expiry timestamp; null/absent for files that never expire. */
  expires_at?: string | null;
}

/** 201 body from `POST /api/groups/{id}/files` (no uploader yet). */
export type GroupFileUploadResult = Omit<GroupFile, "uploader">;

/** `GET /api/groups/{id}/files` → usage + quota + rows. */
export interface GroupFilesListing {
  usage_bytes: number;
  quota_bytes: number;
  files: GroupFile[];
}

/** Machine codes mirroring the backend's group-files error surface. */
export type GroupFileErrorCode =
  | "too_large"
  | "bad_request"
  | "forbidden"
  | "not_found"
  | "upload_failed"
  | "network_error";

export class GroupFileError extends Error {
  readonly code: GroupFileErrorCode;
  readonly status: number;

  constructor(code: GroupFileErrorCode, status: number, message: string) {
    super(message);
    this.name = "GroupFileError";
    this.code = code;
    this.status = status;
  }
}

function mapStatusToError(status: number, body: string): GroupFileError {
  const detail = body.length > 0 ? body : undefined;
  switch (status) {
    case 413:
      return new GroupFileError(
        "too_large",
        status,
        detail ?? "file too large",
      );
    case 400:
      return new GroupFileError("bad_request", status, detail ?? "bad request");
    case 403:
      return new GroupFileError("forbidden", status, detail ?? "forbidden");
    case 404:
      return new GroupFileError("not_found", status, detail ?? "not found");
    default:
      return new GroupFileError(
        "upload_failed",
        status,
        detail ?? `group file operation failed (${status})`,
      );
  }
}

/**
 * Uploads one file's ORIGINAL bytes to a group and resolves with the created
 * row reference. `onProgress` receives an integer 0–100 as bytes are sent.
 */
export function uploadGroupFile(
  accessToken: string,
  groupId: number,
  file: File,
  onProgress?: (percent: number) => void,
): Promise<GroupFileUploadResult> {
  return new Promise<GroupFileUploadResult>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open(
      "POST",
      `${apiBase()}/api/groups/${encodeURIComponent(String(groupId))}/files`,
    );
    xhr.setRequestHeader("Authorization", `Bearer ${accessToken}`);
    xhr.setRequestHeader(
      "Content-Type",
      file.type.length > 0 ? file.type : "application/octet-stream",
    );
    try {
      // Header values are Latin-1; raw CJK names throw in Chromium. The
      // server percent-decodes (and tolerates raw ASCII for old clients).
      xhr.setRequestHeader("X-File-Name", encodeURIComponent(file.name));
    } catch {
      // The header is optional — a hostile name must never block upload.
    }
    xhr.responseType = "text";

    if (onProgress !== undefined) {
      xhr.upload.onprogress = (event: ProgressEvent) => {
        if (event.lengthComputable && event.total > 0) {
          onProgress(Math.round((event.loaded / event.total) * 100));
        }
      };
    }

    xhr.onload = () => {
      if (xhr.status === 201) {
        let parsed: unknown;
        try {
          parsed = JSON.parse(xhr.responseText) as unknown;
        } catch {
          reject(
            new GroupFileError(
              "upload_failed",
              xhr.status,
              "group file upload response was not valid JSON",
            ),
          );
          return;
        }
        resolve(parsed as GroupFileUploadResult);
        return;
      }
      reject(mapStatusToError(xhr.status, xhr.responseText));
    };
    xhr.onerror = () =>
      reject(
        new GroupFileError(
          "network_error",
          0,
          "group file upload network failure",
        ),
      );

    xhr.send(file);
  });
}

/** GET /api/groups/{id}/files (Bearer access) — usage, quota and rows. */
export function listGroupFiles(
  accessToken: string,
  groupId: number,
): Promise<GroupFilesListing> {
  return apiRequest<GroupFilesListing>(
    `/api/groups/${encodeURIComponent(String(groupId))}/files`,
    { method: "GET", accessToken },
  );
}

/**
 * Extracts a UTF-8 file name from a `Content-Disposition` header, preferring
 * the RFC 5987 `filename*` parameter over the plain `filename`. Returns null
 * when the header carries no usable name.
 */
export function parseContentDispositionName(
  header: string | null,
): string | null {
  if (header === null || header.length === 0) return null;
  const encoded = /filename\*\s*=\s*([^\s;]+)/i.exec(header);
  if (encoded !== null && encoded[1] !== undefined) {
    let value = encoded[1].trim();
    // Strip surrounding quotes and the `UTF-8''` charset prefix when present.
    if (value.startsWith('"') && value.endsWith('"')) {
      value = value.slice(1, -1);
    }
    const prefixed = /^utf-8''/i.exec(value);
    if (prefixed !== null) value = value.slice(prefixed[0].length);
    try {
      return decodeURIComponent(value);
    } catch {
      return value;
    }
  }
  const plain = /filename\s*=\s*"([^"]*)"|filename\s*=\s*([^;]+)/i.exec(header);
  if (plain !== null) {
    const value = (plain[1] ?? plain[2] ?? "").trim();
    return value.length > 0 ? value : null;
  }
  return null;
}

/**
 * GET /api/groups/files/{fileId} (Bearer) — downloads the bytes as a Blob and
 * resolves with the server-suggested file name. Errors are normalized to
 * {@link GroupFileError}.
 */
export async function downloadGroupFile(
  accessToken: string,
  fileId: string,
): Promise<{ blob: Blob; name: string }> {
  let response: Response;
  try {
    response = await fetch(
      `${apiBase()}/api/groups/files/${encodeURIComponent(fileId)}`,
      {
        method: "GET",
        headers: { Authorization: `Bearer ${accessToken}` },
        credentials: "omit",
      },
    );
  } catch (cause) {
    const detail = cause instanceof Error ? cause.message : "network failure";
    throw new GroupFileError("network_error", 0, detail);
  }
  if (!response.ok) {
    let body = "";
    try {
      body = await response.text();
    } catch {
      body = "";
    }
    throw mapStatusToError(response.status, body);
  }
  const blob = await response.blob();
  const name = parseContentDispositionName(
    response.headers.get("Content-Disposition"),
  );
  return { blob, name: name ?? fileId };
}

/** POST /api/groups/files/{fileId}/delete → 204. */
export function deleteGroupFile(
  accessToken: string,
  fileId: string,
): Promise<void> {
  return apiRequest<void>(
    `/api/groups/files/${encodeURIComponent(fileId)}/delete`,
    { method: "POST", accessToken },
  );
}

/**
 * Triggers a browser save of a downloaded Blob via a temporary object URL +
 * synthetic anchor click, revoking the URL once the click has been dispatched.
 */
export function saveBlobAs(blob: Blob, name: string): void {
  if (typeof document === "undefined") return;
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = name;
  anchor.rel = "noopener";
  anchor.style.display = "none";
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // Revoke after the click so the navigation can still read the URL.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}
