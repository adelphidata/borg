import type { Attachment } from "./useConversation";

/** Images the daemon's multimodal path accepts. */
const IMAGE_TYPES = new Set(["image/png", "image/jpeg", "image/gif", "image/webp"]);

/** 10 MB, matched to what a single turn can reasonably carry inline. */
export const MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024;

export function isSupportedImage(file: { type: string }): boolean {
  return IMAGE_TYPES.has(file.type);
}

/** Why a file was rejected, or null when it is fine to attach. */
export function rejectionReason(file: File): string | null {
  if (!isSupportedImage(file)) return `${file.name || "That file"} is not a PNG, JPEG, GIF or WebP`;
  if (file.size > MAX_ATTACHMENT_BYTES) {
    return `${file.name || "That image"} is larger than ${MAX_ATTACHMENT_BYTES / (1024 * 1024)} MB`;
  }
  return null;
}

/**
 * Reads a pasted or dropped file into the raw bytes `InlineImage` wants.
 *
 * The proto is explicit that `data` is raw bytes and not base64, so this hands
 * back a `Uint8Array` straight from the ArrayBuffer.
 */
export async function toAttachment(file: File): Promise<Attachment> {
  const buffer = await file.arrayBuffer();
  return {
    mimeType: file.type,
    data: new Uint8Array(buffer),
    filename: file.name || "pasted-image",
    previewUrl: URL.createObjectURL(file),
  };
}

/** Collects every supported image out of a paste or drop, reporting the rest. */
export async function collectImages(
  files: readonly File[],
): Promise<{ attachments: Attachment[]; errors: string[] }> {
  const attachments: Attachment[] = [];
  const errors: string[] = [];
  for (const file of files) {
    const reason = rejectionReason(file);
    if (reason) errors.push(reason);
    else attachments.push(await toAttachment(file));
  }
  return { attachments, errors };
}
