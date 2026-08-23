import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { BorgTransport } from "@borg/client";
import { PlatformProvider, type Platform } from "../../lib/platform";
import { OnboardingPage } from "./OnboardingPage";

afterEach(cleanup);

/** A transport that answers every call, so the page's writes can be observed. */
function stubTransport() {
  const calls: { method: string; request: unknown }[] = [];
  const transport: BorgTransport = {
    unary: async (method, request) => {
      calls.push({ method: String(method), request });
      return {} as never;
    },
    serverStream: () => () => {},
  };
  return { calls, transport };
}

function mount(transport: BorgTransport) {
  const platform: Platform = { kind: "desktop", transport, titleBarDrag: true };
  return render(
    <PlatformProvider platform={platform}>
      <OnboardingPage onDone={vi.fn()} />
    </PlatformProvider>,
  );
}

/** Walks from the welcome screen to the provider step. */
function toProviderStep(transport: BorgTransport) {
  mount(transport);
  fireEvent.click(screen.getByText("Get started"));
}

describe("OnboardingPage", () => {
  it("says where the key comes from once a provider needing one is picked", () => {
    // Picking a provider and being asked for a credential with no hint where
    // to get it is the one dead end a first run cannot recover from.
    const { transport } = stubTransport();
    toProviderStep(transport);
    fireEvent.click(screen.getByText("Anthropic"));
    const link = screen.getByText("Get a key from Anthropic ↗") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://console.anthropic.com/settings/keys");
  });

  it("asks for nothing when the model runs on this machine", () => {
    const { transport } = stubTransport();
    toProviderStep(transport);
    fireEvent.click(screen.getByText("Ollama"));
    expect(screen.queryByPlaceholderText(/API key/)).toBeNull();
    expect(screen.queryByText(/Get a key from/)).toBeNull();
  });

  it("holds back the next step until a key has been pasted", () => {
    const { transport } = stubTransport();
    toProviderStep(transport);
    fireEvent.click(screen.getByText("OpenAI"));
    expect((screen.getByText("Next") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(screen.getByPlaceholderText("OpenAI API key"), {
      target: { value: "sk-test" },
    });
    expect((screen.getByText("Next") as HTMLButtonElement).disabled).toBe(false);
  });
});
