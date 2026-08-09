import { describe, expect, it } from "vitest";
import { MAX_ATTACHMENT_BYTES, collectImages, rejectionReason } from "./attachments";

function file(name: string, type: string, size = 10): File {
  const blob = new Blob([new Uint8Array(size)], { type });
  return new File([blob], name, { type });
}

describe("rejectionReason", () => {
  it("accepts every image type the daemon can carry inline", () => {
    for (const type of ["image/png", "image/jpeg", "image/gif", "image/webp"]) {
      expect(rejectionReason(file("a", type))).toBeNull();
    }
  });

  it("rejects a non-image and names it", () => {
    expect(rejectionReason(file("notes.pdf", "application/pdf"))).toContain("notes.pdf");
  });

  it("rejects an image past the size cap", () => {
    const huge = file("big.png", "image/png", MAX_ATTACHMENT_BYTES + 1);
    expect(rejectionReason(huge)).toContain("larger than");
  });

  it("accepts an image exactly at the cap", () => {
    expect(rejectionReason(file("edge.png", "image/png", MAX_ATTACHMENT_BYTES))).toBeNull();
  });
});

describe("collectImages", () => {
  it("reads raw bytes, not base64 — the proto is explicit about that", async () => {
    const bytes = new Uint8Array([137, 80, 78, 71]);
    const png = new File([bytes], "shot.png", { type: "image/png" });

    const { attachments } = await collectImages([png]);

    expect(attachments).toHaveLength(1);
    expect(attachments[0]!.data).toEqual(bytes);
    expect(attachments[0]!.mimeType).toBe("image/png");
    expect(attachments[0]!.filename).toBe("shot.png");
  });

  it("keeps the good files and reports the bad ones", async () => {
    const { attachments, errors } = await collectImages([
      file("ok.png", "image/png"),
      file("bad.txt", "text/plain"),
    ]);

    expect(attachments).toHaveLength(1);
    expect(errors).toHaveLength(1);
  });

  it("names a pasted file that arrives without one", async () => {
    const pasted = new File([new Uint8Array([1])], "", { type: "image/png" });
    const { attachments } = await collectImages([pasted]);
    expect(attachments[0]!.filename).toBe("pasted-image");
  });
});
