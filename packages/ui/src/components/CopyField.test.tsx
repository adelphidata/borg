import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { COPIED_MS, CopyField } from "./CopyField";

const original = Object.getOwnPropertyDescriptor(navigator, "clipboard");

function setClipboard(value: unknown) {
  Object.defineProperty(navigator, "clipboard", { value, configurable: true });
}

beforeEach(() => vi.useFakeTimers());

afterEach(() => {
  vi.useRealTimers();
  if (original) Object.defineProperty(navigator, "clipboard", original);
  else setClipboard(undefined);
});

/** Click and let the clipboard promise settle. */
async function press(el: HTMLElement) {
  await act(async () => {
    fireEvent.click(el);
  });
}

describe("CopyField", () => {
  it("copies the full value and says so, then goes back to Copy", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    setClipboard({ writeText });
    render(<CopyField label="URL" value="https://borg.example/v1/events/sub_1" />);
    const button = screen.getByRole("button", { name: "Copy url" });
    await press(button);
    expect(writeText).toHaveBeenCalledWith("https://borg.example/v1/events/sub_1");
    expect(button.textContent).toBe("Copied");
    expect(screen.getByRole("status").textContent).toBe("URL copied");
    act(() => vi.advanceTimersByTime(COPIED_MS));
    expect(button.textContent).toBe("Copy");
  });

  it("hides a secret until shown, yet copies the real value either way", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    setClipboard({ writeText });
    render(<CopyField label="Key" value="bsk_live_123" secret />);
    const field = screen.getByLabelText("Key") as HTMLInputElement;
    expect(field.type).toBe("password");
    await press(screen.getByRole("button", { name: "Copy key" }));
    expect(writeText).toHaveBeenCalledWith("bsk_live_123");
    fireEvent.click(screen.getByRole("button", { name: "Show key" }));
    expect(field.type).toBe("text");
    expect(field.value).toBe("bsk_live_123");
    fireEvent.click(screen.getByRole("button", { name: "Hide key" }));
    expect(field.type).toBe("password");
  });

  it("lets the caller own Show, so one control can cover the key elsewhere too", () => {
    const onShownChange = vi.fn();
    const { rerender } = render(
      <CopyField label="Key" value="k" secret shown={false} onShownChange={onShownChange} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Show key" }));
    expect(onShownChange).toHaveBeenCalledWith(true);
    // Still hidden: the caller decides.
    expect((screen.getByLabelText("Key") as HTMLInputElement).type).toBe("password");
    rerender(<CopyField label="Key" value="k" secret shown onShownChange={onShownChange} />);
    expect((screen.getByLabelText("Key") as HTMLInputElement).type).toBe("text");
    fireEvent.click(screen.getByRole("button", { name: "Hide key" }));
    expect(onShownChange).toHaveBeenLastCalledWith(false);
  });

  it("names the value by its label and shows a plain value as it is, with no Show button", () => {
    render(<CopyField label="URL" value="http://localhost:7860/v1/events/sub_1" />);
    const field = screen.getByRole("textbox", { name: "URL" }) as HTMLInputElement;
    expect(field.value).toBe("http://localhost:7860/v1/events/sub_1");
    expect(field.readOnly).toBe(true);
    expect(screen.queryByRole("button", { name: /Show/ })).toBeNull();
    // Where the clipboard is missing, a click selects it all for copying by hand.
    fireEvent.focus(field);
    expect([field.selectionStart, field.selectionEnd]).toEqual([0, field.value.length]);
  });

  it("says it could not copy where there is no clipboard, instead of claiming it did", async () => {
    setClipboard(undefined);
    render(<CopyField label="Key" value="bsk_1" />);
    const button = screen.getByRole("button", { name: "Copy key" });
    await press(button);
    expect(button.textContent).toBe("Could not copy");
    expect(screen.getByRole("status").textContent).toContain("copy it by hand");
  });

  it("says it could not copy when the clipboard refuses", async () => {
    setClipboard({ writeText: vi.fn().mockRejectedValue(new Error("denied")) });
    render(<CopyField label="Key" value="bsk_1" />);
    const button = screen.getByRole("button", { name: "Copy key" });
    await press(button);
    expect(button.textContent).toBe("Could not copy");
  });

  it("drops the Copied receipt when it is handed a new value", async () => {
    setClipboard({ writeText: vi.fn().mockResolvedValue(undefined) });
    const { rerender } = render(<CopyField label="Key" value="bsk_old" />);
    await press(screen.getByRole("button", { name: "Copy key" }));
    expect(screen.getByRole("button", { name: "Copy key" }).textContent).toBe("Copied");
    // A replaced key: nobody has copied this one yet.
    rerender(<CopyField label="Key" value="bsk_new" />);
    expect(screen.getByRole("button", { name: "Copy key" }).textContent).toBe("Copy");
  });

  it("renders the caller's extra controls in the row", () => {
    render(
      <CopyField label="Key" value="k" actions={<button type="button">Replace key</button>} />,
    );
    expect(screen.getByRole("button", { name: "Replace key" })).toBeTruthy();
  });
});
