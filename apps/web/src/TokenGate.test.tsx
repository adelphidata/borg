import { beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { STORAGE_KEYS } from "@borg/ui";
import { TokenGate } from "./TokenGate";

const app = () => <div>the app</div>;

const renderGate = () => render(<TokenGate>{app()}</TokenGate>);
const box = () => screen.getByPlaceholderText("Client token");

beforeEach(() => {
  localStorage.clear();
  window.location.hash = "";
});

describe("TokenGate", () => {
  it("holds the app back until a token exists", () => {
    // The browser has no unix socket to prove itself with.
    renderGate();
    expect(screen.queryByText("the app")).toBeNull();
    expect(screen.getByText("Sign in to Borg.")).toBeTruthy();
  });

  it("names the commands that get someone in", () => {
    // This is the only setup step the web surface has; a gate that does not say
    // where the token comes from is a dead end.
    renderGate();
    const command = screen.getByText("borg client-token issue web");
    // The sentence must stay readable — the commands are separate JSX
    // children, so the spaces around them are easy to lose.
    expect(command.parentElement?.textContent).toBe(
      "Run borg web to open this page signed in, or paste a token from borg client-token issue web.",
    );
  });

  it("takes the token borg web put in the hash, stores it, and strips it", () => {
    // The token must leave the address bar: a shared screenshot or a
    // bookmark would otherwise carry a live credential.
    window.location.hash = "#/?token=tok_from_cli";
    renderGate();
    expect(screen.getByText("the app")).toBeTruthy();
    expect(localStorage.getItem(STORAGE_KEYS.token)).toBe("tok_from_cli");
    expect(window.location.hash).toBe("#/");
  });

  it("keeps the rest of the route when it strips the token", () => {
    window.location.hash = "#/settings?token=tok_x&tab=advanced";
    renderGate();
    expect(window.location.hash).toBe("#/settings?tab=advanced");
    expect(localStorage.getItem(STORAGE_KEYS.token)).toBe("tok_x");
  });

  it("ignores a hash without a token, and a blank one", () => {
    window.location.hash = "#/sessions?token=";
    renderGate();
    expect(screen.queryByText("the app")).toBeNull();
    expect(localStorage.getItem(STORAGE_KEYS.token)).toBeNull();
    expect(window.location.hash).toBe("#/sessions?token=");
  });

  it("starts with an empty box", () => {
    renderGate();
    expect((box() as HTMLInputElement).value).toBe("");
  });

  it("lets a stored token straight through", () => {
    localStorage.setItem(STORAGE_KEYS.token, "tok_live");
    renderGate();
    expect(screen.getByText("the app")).toBeTruthy();
    expect(screen.queryByText("Sign in to Borg.")).toBeNull();
  });

  it("stores the pasted token and opens the app", () => {
    renderGate();
    fireEvent.change(box(), { target: { value: "tok_live" } });
    fireEvent.click(screen.getByText("Continue"));

    expect(localStorage.getItem(STORAGE_KEYS.token)).toBe("tok_live");
    expect(screen.getByText("the app")).toBeTruthy();
  });

  it("accepts the token on Enter", () => {
    renderGate();
    fireEvent.change(box(), { target: { value: "tok_live" } });
    fireEvent.keyDown(box(), { key: "Enter" });
    expect(screen.getByText("the app")).toBeTruthy();
  });

  it("only submits on Enter, not on other keys", () => {
    // Submitting mid-typing would store a half-pasted token.
    renderGate();
    fireEvent.change(box(), { target: { value: "tok_live" } });
    fireEvent.keyDown(box(), { key: "a" });
    expect(localStorage.getItem(STORAGE_KEYS.token)).toBeNull();
    expect(screen.queryByText("the app")).toBeNull();
  });

  it("trims a token pasted with surrounding whitespace", () => {
    // Copying from a terminal picks up a trailing newline, and the gateway
    // rejects the token that results.
    renderGate();
    fireEvent.change(box(), { target: { value: "  tok_live\n" } });
    fireEvent.keyDown(box(), { key: "Enter" });
    expect(localStorage.getItem(STORAGE_KEYS.token)).toBe("tok_live");
  });

  it("ignores an empty submission rather than storing a blank token", () => {
    // A stored empty string would look like a token and fail every call.
    renderGate();
    fireEvent.change(box(), { target: { value: "   " } });
    fireEvent.click(screen.getByText("Continue"));

    expect(localStorage.getItem(STORAGE_KEYS.token)).toBeNull();
    expect(screen.queryByText("the app")).toBeNull();
  });

  it("keeps the token out of the page as readable text", () => {
    renderGate();
    expect(box().getAttribute("type")).toBe("password");
  });

  it("focuses the box, since it is the only thing to do here", () => {
    renderGate();
    expect(document.activeElement).toBe(box());
  });
});
