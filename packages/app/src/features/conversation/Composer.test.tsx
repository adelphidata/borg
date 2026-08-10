import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { Composer } from "./Composer";
import { draftKeyFor, moveDraft } from "./draftStore";

/** Attaching a real image needs blob URLs and a FileReader; stub the step. */
vi.mock("./attachments", () => ({
  collectImages: vi.fn(() =>
    Promise.resolve({
      attachments: [
        {
          mimeType: "image/png",
          data: new Uint8Array([1]),
          filename: "shot.png",
          previewUrl: "blob:shot",
        },
      ],
      errors: [],
    }),
  ),
}));

/** The pill talks to the daemon; this file is about the composer. */
vi.mock("./ModelPill", () => ({ ModelPill: () => null }));

const noop = () => {};

function setup(props: { draftKey: string; conversationKey: string }) {
  return render(<Composer busy={false} onSend={noop} onSteer={noop} onCancel={noop} {...props} />);
}

/** Attaches an image through the file picker, and waits for its chip. */
async function attach(container: HTMLElement) {
  const file = new File([new Uint8Array([1])], "shot.png", { type: "image/png" });
  const input = container.querySelector('input[type="file"]')!;
  fireEvent.change(input, { target: { files: [file] } });
  await waitFor(() => expect(screen.getByAltText("shot.png")).toBeTruthy());
}

beforeEach(() => {
  sessionStorage.clear();
  globalThis.URL.revokeObjectURL = vi.fn();
  // jsdom implements neither of these, and the composer reaches for both.
  window.matchMedia = vi.fn(() => ({
    matches: false,
    addEventListener: noop,
    removeEventListener: noop,
  })) as unknown as typeof window.matchMedia;
});

afterEach(cleanup);

describe("Composer", () => {
  it("keeps an attachment when a new conversation is issued its id", async () => {
    // `/c/new` types under a provisional key until the daemon answers. An image
    // attached in that window used to vanish silently when the key changed.
    const { container, rerender } = setup({
      draftKey: draftKeyFor("new"),
      conversationKey: "new",
    });
    await attach(container);

    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("sess-1")}
        conversationKey="new"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(screen.getByAltText("shot.png")).toBeTruthy();
  });

  it("drops the attachment when the user moves to another conversation", async () => {
    const { container, rerender } = setup({
      draftKey: draftKeyFor("sess-1"),
      conversationKey: "sess-1",
    });
    await attach(container);

    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("sess-2")}
        conversationKey="sess-2"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(screen.queryByAltText("shot.png")).toBeNull();
  });

  it("still shows the typed text after the draft is re-keyed under the real id", () => {
    const { rerender } = setup({ draftKey: draftKeyFor("new"), conversationKey: "new" });
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "half a thought" } });

    // What `useConversation` does once the daemon issues the id.
    moveDraft(draftKeyFor("new"), draftKeyFor("sess-1"));
    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("sess-1")}
        conversationKey="new"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(screen.getByRole("textbox")).toHaveProperty("value", "half a thought");
  });
});
