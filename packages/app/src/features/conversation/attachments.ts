import type { Attachment } from "./useConversation";

/** Images the daemon's multimodal path accepts. */
const IMAGE_TYPES = new Set(["image/png", "image/jpeg", "image/gif", "image/webp"]);

/**
 * Text types a browser labels confidently. Everything under `text/` counts, so
 * this only names the ones that arrive with an `application/` type instead.
 */
const DOCUMENT_TYPES = new Set([
  "application/json",
  "application/xml",
  "application/x-yaml",
  "application/yaml",
  "application/toml",
  "application/javascript",
  "application/typescript",
  "application/sql",
]);

/**
 * Extensions the OS hands over with an empty or wrong MIME type.
 *
 * Most source files land here: drop a `.rs` or a `.tsx` and the browser
 * reports `""`. Refusing those would make "attach a file" mean "attach a file
 * the OS happens to have an opinion about", which is not a rule anyone can
 * hold in their head.
 *
 * Nothing in a credential's family belongs on this list. `.env` reads as the
 * extension `env`, so naming it here would ship a secrets file to the model
 * while `.env.local` — extension `local` — was refused beside it. Borg's
 * `blocked_paths` rule keeps those files away from every other read path; an
 * attach button is not the place to make an exception.
 */
const DOCUMENT_EXTENSIONS = new Set([
  "c",
  "cc",
  "cfg",
  "conf",
  "cpp",
  "cs",
  "css",
  "csv",
  "diff",
  "go",
  "gradle",
  "h",
  "hpp",
  "htm",
  "html",
  "ini",
  "java",
  "js",
  "json",
  "jsonl",
  "jsx",
  "kt",
  "log",
  "lua",
  "md",
  "mdx",
  "mjs",
  "patch",
  "php",
  "pl",
  "properties",
  "proto",
  "py",
  "r",
  "rb",
  "rs",
  "rst",
  "scala",
  "scss",
  "sh",
  "sql",
  "svg",
  "swift",
  "toml",
  "ts",
  "tsv",
  "tsx",
  "txt",
  "vue",
  "xml",
  "yaml",
  "yml",
  "zsh",
]);

/** 10 MB, matched to what a single turn can reasonably carry inline. */
export const MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024;

/**
 * 256 KB — the daemon's `MAX_DOCUMENT_BYTES`, kept in step by hand.
 *
 * Text is capped far lower than an image because the limit that bites is the
 * context window, not the wire. Sending a megabyte of log would only buy a
 * provider error after the upload.
 */
export const MAX_DOCUMENT_BYTES = 256 * 1024;

export function isSupportedImage(file: { type: string }): boolean {
  return IMAGE_TYPES.has(file.type);
}

/** Everything after the last dot, lowercased, or "" when there is no dot. */
function extensionOf(name: string): string {
  const at = name.lastIndexOf(".");
  return at === -1 ? "" : name.slice(at + 1).toLowerCase();
}

/**
 * A file Borg can read as text.
 *
 * The daemon decodes it and refuses what is not UTF-8, so this is a courtesy
 * check that keeps an obvious refusal off the wire — not the boundary.
 */
export function isSupportedDocument(file: { type: string; name: string }): boolean {
  if (file.type.startsWith("text/") || DOCUMENT_TYPES.has(file.type)) return true;
  // A named extension only counts when the browser had no better idea. A PDF
  // reports `application/pdf`, and `notes.pdf.txt` is genuinely text.
  return file.type === "" && DOCUMENT_EXTENSIONS.has(extensionOf(file.name));
}

/** Why a file was rejected, or null when it is fine to attach. */
export function rejectionReason(file: File): string | null {
  const name = file.name || "That file";
  const isImage = isSupportedImage(file);
  if (!isImage && !isSupportedDocument(file)) {
    // Names the next action rather than the rule that was broken: a bare "not
    // supported" leaves someone holding a PDF with nowhere to go.
    return `${name} is not an image or a text file. Save it somewhere Borg can reach and ask it to read the path.`;
  }
  if (isImage) {
    if (file.size > MAX_ATTACHMENT_BYTES) {
      return `${name} is larger than ${MAX_ATTACHMENT_BYTES / (1024 * 1024)} MB`;
    }
    return null;
  }
  if (file.size > MAX_DOCUMENT_BYTES) {
    return `${name} is too long to attach to one message (over ${MAX_DOCUMENT_BYTES / 1024} KB). Save it somewhere Borg can reach and ask it to read the path.`;
  }
  return null;
}

/** Longest edge of a composer thumbnail, in CSS pixels times a retina factor. */
export const PREVIEW_MAX_EDGE = 320;

/**
 * Below this, decoding the image twice costs more than the paint it saves.
 */
export const PREVIEW_SKIP_BYTES = 256 * 1024;

/**
 * The thumbnail's dimensions: the same picture, no larger than `maxEdge`.
 *
 * Never upscales — a 40-pixel icon blown up to 320 is worse than the icon —
 * and hands back what it was given if the dimensions are nonsense, so a
 * bad decode falls through to the full-size path rather than dividing by zero.
 */
export function previewSize(
  width: number,
  height: number,
  maxEdge = PREVIEW_MAX_EDGE,
): { width: number; height: number } {
  if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) {
    return { width, height };
  }
  const longest = Math.max(width, height);
  if (longest <= maxEdge) return { width, height };
  const scale = maxEdge / longest;
  return { width: Math.round(width * scale), height: Math.round(height * scale) };
}

/**
 * An object URL for the composer chip, downscaled when that is worth doing.
 *
 * A 10 MB photo behind a 52-pixel chip is decoded and painted at full size,
 * which is what froze the composer on a drop. The bytes on the wire are
 * untouched — this is the preview only.
 *
 * Fails open at every step: no `createImageBitmap`, no 2D context (jsdom), or
 * a decode that throws all fall back to the plain object URL, which is exactly
 * what this did before. Exactly one URL is minted either way, because the
 * composer's revoke bookkeeping tracks what it is handed.
 */
async function previewUrlFor(file: File): Promise<string> {
  if (file.size <= PREVIEW_SKIP_BYTES || typeof createImageBitmap !== "function") {
    return URL.createObjectURL(file);
  }
  let bitmap: ImageBitmap | undefined;
  try {
    bitmap = await createImageBitmap(file);
    const { width, height } = previewSize(bitmap.width, bitmap.height);
    // Asked once and remembered: `canvas instanceof OffscreenCanvas` would
    // throw where the constructor does not exist at all, which is exactly the
    // browser the element fallback is here for.
    const offscreen = typeof OffscreenCanvas === "function";
    const canvas = offscreen
      ? new OffscreenCanvas(width, height)
      : Object.assign(document.createElement("canvas"), { width, height });
    const ctx = canvas.getContext("2d") as
      OffscreenCanvasRenderingContext2D | CanvasRenderingContext2D | null;
    if (!ctx) return URL.createObjectURL(file);
    ctx.drawImage(bitmap, 0, 0, width, height);
    const blob = offscreen
      ? await (canvas as OffscreenCanvas).convertToBlob({ type: "image/webp", quality: 0.82 })
      : await new Promise<Blob | null>((resolve) =>
          (canvas as HTMLCanvasElement).toBlob(resolve, "image/webp", 0.82),
        );
    return blob ? URL.createObjectURL(blob) : URL.createObjectURL(file);
  } catch {
    return URL.createObjectURL(file);
  } finally {
    // The decoded bitmap is the largest allocation here and is not collected
    // promptly on its own.
    bitmap?.close?.();
  }
}

/**
 * Reads a pasted or dropped file into the raw bytes the wire wants.
 *
 * The proto is explicit that both `data` and `content` are raw bytes and not
 * base64, so this hands back a `Uint8Array` straight from the ArrayBuffer.
 * Only an image gets an object URL — a document is shown by name, and minting
 * a URL nothing renders would leak it.
 */
export async function toAttachment(file: File): Promise<Attachment> {
  const buffer = await file.arrayBuffer();
  const isImage = isSupportedImage(file);
  return {
    kind: isImage ? "image" : "document",
    mimeType: file.type,
    data: new Uint8Array(buffer),
    filename: file.name || (isImage ? "pasted-image" : "pasted.txt"),
    previewUrl: isImage ? await previewUrlFor(file) : undefined,
  };
}

/**
 * Splits attachments into the two `SendRequest` fields they belong in.
 *
 * The wire keeps images and documents apart because the daemon does different
 * things with them: an image becomes a multimodal part, a document is decoded
 * as text behind the untrusted-content boundary. Sending one down the other's
 * field is silent corruption, so this is its own function with its own test.
 */
export function splitAttachments(attachments: readonly Attachment[]): {
  images: { mimeType: string; data: Uint8Array; filename: string }[];
  documents: { filename: string; mimeType: string; content: Uint8Array }[];
} {
  const images = [];
  const documents = [];
  for (const a of attachments) {
    if (a.kind === "image") {
      images.push({ mimeType: a.mimeType, data: a.data, filename: a.filename });
    } else {
      documents.push({ filename: a.filename, mimeType: a.mimeType, content: a.data });
    }
  }
  return { images, documents };
}

/** Collects every supported file out of a paste or drop, reporting the rest. */
export async function collectAttachments(
  files: readonly File[],
): Promise<{ attachments: Attachment[]; errors: string[] }> {
  const attachments: Attachment[] = [];
  const errors: string[] = [];
  for (const file of files) {
    const reason = rejectionReason(file);
    if (reason) {
      errors.push(reason);
      continue;
    }
    try {
      attachments.push(await toAttachment(file));
    } catch {
      // A file moved or removed between the picker and the read. Silence here
      // would be a chip that never appears and no reason why.
      errors.push(`${file.name || "That file"} could not be read. Try attaching it again.`);
    }
  }
  return { attachments, errors };
}
