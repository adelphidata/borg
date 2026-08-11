import { describe, expect, it } from "vitest";
import {
  collectAttachments,
  isSupportedDocument,
  MAX_ATTACHMENT_BYTES,
  rejectionReason,
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

  it("stops a file past the size cap, whichever kind it is", () => {
    const big = file("huge.log", "text/plain", MAX_ATTACHMENT_BYTES + 1);
    expect(rejectionReason(big)).toContain("larger than 10 MB");
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
