import { describe, expect, it } from "vitest";
import {
  collectAttachments,
  isSupportedDocument,
  MAX_ATTACHMENT_BYTES,
  MAX_DOCUMENT_BYTES,
  rejectionReason,
  splitAttachments,
} from "./attachments";

function file(name: string, type: string, bytes = 1): File {
  return new File([new Uint8Array(bytes)], name, { type });
}

describe("what may be attached", () => {
  it("takes a text file the browser labelled", () => {
    expect(isSupportedDocument(file("notes.md", "text/markdown"))).toBe(true);
    expect(isSupportedDocument(file("data.json", "application/json"))).toBe(true);
  });

  it("takes a source file the OS had no opinion about", () => {
    // Dropping a `.rs` reports an empty type. Refusing those would make the
    // rule depend on the OS rather than on the file.
    expect(isSupportedDocument(file("main.rs", ""))).toBe(true);
    expect(isSupportedDocument(file("Component.tsx", ""))).toBe(true);
  });

  it("does not guess from an extension when the browser was specific", () => {
    // A PDF is a PDF whatever it is called.
    expect(isSupportedDocument(file("report.pdf", "application/pdf"))).toBe(false);
    expect(isSupportedDocument(file("archive.zip", "application/zip"))).toBe(false);
  });

  it("refuses an unknown binary rather than sending bytes that cannot decode", () => {
    expect(isSupportedDocument(file("thing.bin", ""))).toBe(false);
  });

  it("refuses a credentials file the rest of Borg already blocks", () => {
    // `.env` reads as the extension `env`. Accepting it here would ship a
    // secrets file to the model through the one read path that does not go
    // past `blocked_paths`.
    expect(isSupportedDocument(file(".env", ""))).toBe(false);
    expect(isSupportedDocument(file(".env.local", ""))).toBe(false);
    expect(isSupportedDocument(file(".env.example", ""))).toBe(false);
    expect(rejectionReason(file(".env", ""))).toContain("not an image or a text file");
  });
});

describe("rejectionReason", () => {
  it("names a next action instead of only the rule", () => {
    const reason = rejectionReason(file("report.pdf", "application/pdf"));
    expect(reason).toContain("report.pdf");
    expect(reason).toContain("ask it to read the path");
  });

  it("passes an image and a text file alike", () => {
    expect(rejectionReason(file("shot.png", "image/png"))).toBeNull();
    expect(rejectionReason(file("run.log", "text/plain"))).toBeNull();
  });

  it("stops an image past the image cap", () => {
    const big = file("huge.png", "image/png", MAX_ATTACHMENT_BYTES + 1);
    expect(rejectionReason(big)).toContain("larger than 10 MB");
  });

  it("holds text to the much smaller cap the daemon enforces", () => {
    // A megabyte of log fits the wire and then fails at the provider, so the
    // refusal has to come here and name somewhere else to put it.
    const big = file("huge.log", "text/plain", MAX_DOCUMENT_BYTES + 1);
    const reason = rejectionReason(big);
    expect(reason).toContain("huge.log");
    expect(reason).toContain("ask it to read the path");
    // An image of the same size is still fine — the caps are not one number.
    expect(rejectionReason(file("shot.png", "image/png", MAX_DOCUMENT_BYTES + 1))).toBeNull();
  });

  it("says something useful about a file with no name", () => {
    expect(rejectionReason(file("", "application/pdf"))).toContain("That file");
  });
});

describe("collectAttachments", () => {
  it("sorts an image from a document, so each reaches its own wire field", async () => {
    const { attachments, errors } = await collectAttachments([
      file("shot.png", "image/png"),
      file("notes.md", "text/markdown"),
    ]);

    expect(errors).toEqual([]);
    expect(attachments.map((a) => a.kind)).toEqual(["image", "document"]);
    // Only an image renders, so only an image is worth a URL to revoke.
    expect(attachments[0]!.previewUrl).toBeTruthy();
    expect(attachments[1]!.previewUrl).toBeUndefined();
  });

  it("keeps the good files and reports the refused one", async () => {
    const { attachments, errors } = await collectAttachments([
      file("notes.md", "text/markdown"),
      file("report.pdf", "application/pdf"),
    ]);

    expect(attachments.map((a) => a.filename)).toEqual(["notes.md"]);
    expect(errors).toHaveLength(1);
    expect(errors[0]).toContain("report.pdf");
  });
});

describe("splitAttachments", () => {
  it("puts each attachment in the wire field the daemon reads it from", () => {
    const png = new Uint8Array([0x89, 0x50]);
    const md = new Uint8Array([0x23, 0x20]);
    const { images, documents } = splitAttachments([
      { kind: "image", mimeType: "image/png", data: png, filename: "shot.png" },
      { kind: "document", mimeType: "text/markdown", data: md, filename: "notes.md" },
    ]);

    // Crossed fields would be silent: a document in `images` becomes a broken
    // multimodal part, and an image in `documents` fails the UTF-8 decode.
    expect(images).toEqual([{ mimeType: "image/png", data: png, filename: "shot.png" }]);
    expect(documents).toEqual([{ filename: "notes.md", mimeType: "text/markdown", content: md }]);
  });

  it("sends no empty fields for a turn with nothing attached", () => {
    expect(splitAttachments([])).toEqual({ images: [], documents: [] });
  });
});
