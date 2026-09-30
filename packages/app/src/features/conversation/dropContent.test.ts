import { describe, expect, it } from "vitest";
import {
  carriesFiles,
  carriesText,
  droppedText,
  insertAt,
  isLinkShortcut,
  readDrop,
  type DragPayload,
} from "./dropContent";

function payload(data: Record<string, string>, files: File[] = []): DragPayload {
  return {
    types: [...Object.keys(data), ...(files.length > 0 ? ["Files"] : [])],
    files,
    getData: (format) => data[format] ?? "",
  };
}

const shortcut = new File(["[InternetShortcut]"], "Example.url", { type: "" });
const image = new File([new Uint8Array([1])], "shot.png", { type: "image/png" });

describe("what a drag carries", () => {
  it.each([
    { types: ["Files"], files: true, text: false },
    { types: ["text/plain"], files: false, text: true },
    { types: ["text/uri-list", "text/html"], files: false, text: true },
    { types: ["text/html"], files: false, text: false },
    { types: ["Files", "text/uri-list"], files: true, text: true },
  ])("$types", ({ types, files, text }) => {
    expect(carriesFiles({ types })).toBe(files);
    expect(carriesText({ types })).toBe(text);
  });
});

describe("droppedText", () => {
  it.each<{ name: string; data: Record<string, string>; want: string }>([
    {
      name: "prefers the address over the link's label",
      data: { "text/uri-list": "https://example.com/a", "text/plain": "Example" },
      want: "https://example.com/a",
    },
    {
      name: "keeps every address and skips comment lines",
      data: { "text/uri-list": "# two links\r\nhttps://a.test/\r\nhttps://b.test/\r\n" },
      want: "https://a.test/\nhttps://b.test/",
    },
    {
      name: "falls back to plain text, untouched",
      data: { "text/plain": "  a selected\nparagraph " },
      want: "  a selected\nparagraph ",
    },
    { name: "is empty when the drop has neither", data: { "text/html": "<b>x</b>" }, want: "" },
  ])("$name", ({ data, want }) => {
    expect(droppedText(payload(data))).toBe(want);
  });
});

describe("isLinkShortcut", () => {
  it.each([
    ["Example.url", true],
    ["Example.WEBLOC", true],
    ["url", false],
    ["notes.url.txt", false],
    ["shot.png", false],
  ])("%s", (name, want) => {
    expect(isLinkShortcut({ name })).toBe(want);
  });
});

describe("readDrop", () => {
  it("writes the address and ignores the shortcut file beside it", () => {
    const drop = readDrop(payload({ "text/uri-list": "https://example.com/" }, [shortcut]));
    expect(drop).toEqual({ files: [], text: "https://example.com/" });
  });

  it("attaches a picture dragged off a page rather than writing its address", () => {
    const drop = readDrop(payload({ "text/uri-list": "https://example.com/shot.png" }, [image]));
    expect(drop).toEqual({ files: [image], text: "" });
  });

  it("still reports a shortcut file dropped with no address", () => {
    expect(readDrop(payload({}, [shortcut]))).toEqual({ files: [shortcut], text: "" });
  });
});

describe("insertAt", () => {
  it.each([
    { name: "at the caret", start: 5, end: 5, want: "look LINKat this", caret: 9 },
    { name: "over a selection", start: 5, end: 8, want: "look LINKthis", caret: 9 },
    {
      name: "at the end when the caret is past it",
      start: 99,
      end: 99,
      want: "look at thisLINK",
      caret: 16,
    },
  ])("$name", ({ start, end, want, caret }) => {
    expect(insertAt("look at this", "LINK", start, end)).toEqual({ text: want, caret });
  });
});
