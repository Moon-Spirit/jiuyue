/**
 * Client-side avatar image compression shared by the profile editor and the
 * group settings panel. Any picked image is downscaled (≤256px on its longest
 * edge) and re-encoded as a JPEG data URL the server accepts verbatim.
 */

/** Longest edge of the produced data URL, in pixels. */
export const AVATAR_MAX_EDGE = 256;
/** JPEG quality of the produced data URL. */
export const AVATAR_JPEG_QUALITY = 0.85;

/**
 * Reads `file` and resolves a downscaled JPEG data URL, or `null` when the
 * image cannot be decoded / no 2D context is available (offline, jsdom).
 */
export function fileToAvatarDataUrl(file: File): Promise<string | null> {
  return new Promise((resolve) => {
    const url = URL.createObjectURL(file);
    const image = new Image();
    image.onload = () => {
      try {
        const scale = Math.min(
          1,
          AVATAR_MAX_EDGE / Math.max(image.naturalWidth, image.naturalHeight),
        );
        const canvas = document.createElement("canvas");
        canvas.width = Math.max(1, Math.round(image.naturalWidth * scale));
        canvas.height = Math.max(1, Math.round(image.naturalHeight * scale));
        const ctx = canvas.getContext("2d");
        if (ctx === null) {
          resolve(null);
          return;
        }
        ctx.drawImage(image, 0, 0, canvas.width, canvas.height);
        resolve(canvas.toDataURL("image/jpeg", AVATAR_JPEG_QUALITY));
      } catch {
        resolve(null);
      } finally {
        URL.revokeObjectURL(url);
      }
    };
    image.onerror = () => {
      URL.revokeObjectURL(url);
      resolve(null);
    };
    image.src = url;
  });
}
