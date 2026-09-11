/**
 * Media upload client for the JiuYue HTTP API (M8).
 *
 * - `POST {apiBase()}/api/media` with the RAW file bytes as the request body
 *   (no multipart, no compression — original bytes travel verbatim).
 * - Auth travels via the `Authorization: Bearer` header; the content type and
 *   UTF-8 file name ride in `Content-Type` / `X-File-Name`.
 * - Progress comes from `XMLHttpRequest.upload.onprogress` (fetch cannot
 *   report upload progress), surfaced as an integer percentage.
 *
 * Size/type limits are mirrored client-side BEFORE any network call so an
 * obviously-too-large or unsupported file never leaves the device.
 */

import { apiBase } from "../apiConfig";

/** Upper bound for still images (matches the backend's 413 `too_large`). */
export const MAX_IMAGE_BYTES = 15 * 1024 * 1024;
/** Upper bound for video (matches the backend's 413 `too_large`). */
export const MAX_VIDEO_BYTES = 200 * 1024 * 1024;
/** Upper bound for voice/audio (reuses the video cap). */
export const MAX_AUDIO_BYTES = MAX_VIDEO_BYTES;

export const IMAGE_MIME_TYPES: readonly string[] = [
  "image/png",
  "image/jpeg",
  "image/webp",
  "image/gif",
];

export const VIDEO_MIME_TYPES: readonly string[] = [
  "video/mp4",
  "video/quicktime",
  "video/webm",
];

/** Container MIME types accepted for voice messages + audio files. */
export const AUDIO_MIME_TYPES: readonly string[] = [
  "audio/webm",
  "audio/ogg",
  "audio/mp4",
  "audio/mpeg",
  "audio/aac",
  "audio/wav",
];

export type MediaKind = "image" | "video" | "audio";

/** 201 response body from `POST /api/media`. */
export interface MediaUploadResult {
  media_id: string;
  kind: MediaKind;
  mime: string;
  bytes: number;
  file_name: string;
}

/** Machine codes mirroring the backend's media error surface. */
export type MediaUploadErrorCode =
  | "too_large"
  | "unsupported_type"
  | "bad_request"
  | "upload_failed"
  | "network_error";

export class MediaUploadError extends Error {
  readonly code: MediaUploadErrorCode;
  readonly status: number;

  constructor(code: MediaUploadErrorCode, status: number, message: string) {
    super(message);
    this.name = "MediaUploadError";
    this.code = code;
    this.status = status;
  }
}

/** Maps a supported MIME to its media kind, or null when unsupported. */
export function mediaKindFor(mime: string): MediaKind | null {
  // MediaRecorder reports e.g. "audio/webm;codecs=opus": compare the bare
  // container type, not the full parameterized string.
  const bare = mime.split(";")[0]?.trim().toLowerCase() ?? "";
  if (IMAGE_MIME_TYPES.includes(bare)) return "image";
  if (VIDEO_MIME_TYPES.includes(bare)) return "video";
  if (AUDIO_MIME_TYPES.includes(bare)) return "audio";
  return null;
}

/** Per-kind size cap in bytes. */
export function maxBytesForKind(kind: MediaKind): number {
  if (kind === "image") return MAX_IMAGE_BYTES;
  if (kind === "audio") return MAX_AUDIO_BYTES;
  return MAX_VIDEO_BYTES;
}

export interface MediaFileCheck {
  /** Resolved kind, or null when the MIME is unsupported. */
  kind: MediaKind | null;
  /** Machine code describing the rejection, or null when the file is valid. */
  error: MediaUploadErrorCode | null;
}

/**
 * Client-side pre-upload validation: unsupported type wins over size (an
 * unknown kind has no meaningful limit), otherwise the per-kind cap applies.
 */
export function checkMediaFile(file: {
  type: string;
  size: number;
}): MediaFileCheck {
  const kind = mediaKindFor(file.type);
  if (kind === null) return { kind: null, error: "unsupported_type" };
  if (file.size > maxBytesForKind(kind)) return { kind, error: "too_large" };
  return { kind, error: null };
}

/** Human-readable byte size, e.g. `15728640` → `"15 MB"`. */
export function formatBytes(bytes: number): string {
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded =
    unit === 0 ? String(value) : value.toFixed(value < 10 ? 1 : 0);
  return `${rounded} ${units[unit]}`;
}

function mapStatusToError(status: number, body: string): MediaUploadError {
  const detail = body.length > 0 ? body : undefined;
  switch (status) {
    case 413:
      return new MediaUploadError(
        "too_large",
        status,
        detail ?? "file too large",
      );
    case 415:
      return new MediaUploadError(
        "unsupported_type",
        status,
        detail ?? "unsupported media type",
      );
    case 400:
      return new MediaUploadError(
        "bad_request",
        status,
        detail ?? "bad media request",
      );
    default:
      return new MediaUploadError(
        "upload_failed",
        status,
        detail ?? `media upload failed (${status})`,
      );
  }
}

/**
 * Uploads one file's ORIGINAL bytes and resolves with the server's media
 * reference. `onProgress` receives an integer 0–100 as bytes are sent.
 */
export function uploadMedia(
  accessToken: string,
  file: File,
  onProgress?: (percent: number) => void,
): Promise<MediaUploadResult> {
  return new Promise<MediaUploadResult>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", `${apiBase()}/api/media`);
    xhr.setRequestHeader("Authorization", `Bearer ${accessToken}`);
    xhr.setRequestHeader("Content-Type", file.type);
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
            new MediaUploadError(
              "upload_failed",
              xhr.status,
              "media upload response was not valid JSON",
            ),
          );
          return;
        }
        resolve(parsed as MediaUploadResult);
        return;
      }
      reject(mapStatusToError(xhr.status, xhr.responseText));
    };
    xhr.onerror = () =>
      reject(
        new MediaUploadError(
          "network_error",
          0,
          "media upload network failure",
        ),
      );

    xhr.send(file);
  });
}
