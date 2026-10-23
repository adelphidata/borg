import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { McpServerForm, draftOf, specOf, type McpDraft } from "./McpServerForm";
import { emptySpec } from "./useMcpServers";

describe("draftOf / specOf", () => {
  it("shows a stored stdio row as one command line and reads it back", () => {
    const draft = draftOf("fs", {
      ...emptySpec("stdio"),
      command: "npx",
      args: ["-y", "@modelcontextprotocol/server-filesystem", "/My Files"],
      env: { A: "1" },
      connectTimeoutMs: 5000,
    });
    expect(draft.commandLine).toBe('npx -y @modelcontextprotocol/server-filesystem "/My Files"');
    expect(draft.env).toBe("A=1");
    expect(draft.connectTimeoutMs).toBe("5000");
    // A zero knob means "daemon default" and shows as blank, not "0".
    expect(draft.callTimeoutMs).toBe("");

    const built = specOf(draft, true);
    expect(built.error).toBeUndefined();
    expect(built.spec).toMatchObject({
      transport: "stdio",
      command: "npx",
      args: ["-y", "@modelcontextprotocol/server-filesystem", "/My Files"],
      env: { A: "1" },
      connectTimeoutMs: 5000,
      callTimeoutMs: 0,
      url: "",
      headers: {},
      bearerToken: "",
    });
  });

  it("keeps only the fields of the chosen transport", () => {
    const draft: McpDraft = {
      ...draftOf("remote", undefined),
      transport: "http",
      url: " https://mcp.example.com/mcp ",
      bearerToken: " tok ",
      headers: "X-Api-Key=k",
      // Left over from a stdio draft; must not travel with an http spec.
      commandLine: "uvx thing",
      env: "A=1",
    };
    const built = specOf(draft, false);
    expect(built.spec).toMatchObject({
      enabled: false,
      transport: "http",
      url: "https://mcp.example.com/mcp",
      bearerToken: "tok",
      headers: { "X-Api-Key": "k" },
      command: "",
      args: [],
      env: {},
    });
  });

  it("names what is wrong before the daemon is asked", () => {
    const base = draftOf("x", undefined);
    expect(specOf(base, true).error).toMatch(/Enter the command/);
    expect(specOf({ ...base, transport: "sse", url: "example.com" }, true).error).toMatch(
      /http:\/\/ or https:\/\//,
    );
    expect(specOf({ ...base, commandLine: "uvx t", env: "broken" }, true).error).toMatch(
      /"broken" is not/,
    );
    expect(
      specOf({ ...base, transport: "http", url: "https://a", headers: "nope" }, true).error,
    ).toMatch(/"nope" is not/);
    expect(specOf({ ...base, commandLine: "uvx t", rateLimitBurst: "-3" }, true).error).toMatch(
      /burst limit/,
    );
    expect(specOf({ ...base, commandLine: "uvx t", maxTools: "1.5" }, true).error).toMatch(
      /tool cap/,
    );
  });

  it("treats an unknown stored transport as stdio rather than crashing the form", () => {
    expect(draftOf("odd", { ...emptySpec(), transport: "carrier-pigeon" }).transport).toBe("stdio");
  });
});

function Harness({ initial, nameLocked = false }: { initial: McpDraft; nameLocked?: boolean }) {
  const [draft, setDraft] = useState(initial);
  return (
    <>
      <McpServerForm draft={draft} onChange={setDraft} nameLocked={nameLocked} />
      <output data-testid="draft">{JSON.stringify(draft)}</output>
    </>
  );
}

const current = (): McpDraft => JSON.parse(screen.getByTestId("draft").textContent ?? "{}");

describe("McpServerForm", () => {
  it("switches between a command and a URL, and keeps the rest under Advanced", () => {
    render(<Harness initial={draftOf("", undefined)} />);
    expect(screen.getByLabelText("Command")).toBeTruthy();
    expect(screen.queryByLabelText("URL")).toBeNull();
    expect(screen.queryByLabelText("Environment")).toBeNull();

    fireEvent.change(screen.getByLabelText("Name"), { target: { value: "gh" } });
    fireEvent.change(screen.getByLabelText("Command"), { target: { value: "npx -y x" } });
    expect(current()).toMatchObject({ name: "gh", commandLine: "npx -y x" });

    fireEvent.click(screen.getByText("Advanced…"));
    fireEvent.change(screen.getByLabelText("Environment"), { target: { value: "A=1" } });
    fireEvent.change(screen.getByLabelText("Calls per minute"), { target: { value: "90" } });
    expect(current()).toMatchObject({ env: "A=1", rateLimitPerMinute: "90" });

    fireEvent.click(screen.getByRole("button", { name: "URL (SSE)" }));
    expect(current().transport).toBe("sse");
    expect(screen.getByLabelText("URL")).toBeTruthy();
    expect(screen.getByLabelText("Bearer token")).toBeTruthy();
    expect(screen.getByLabelText("Headers")).toBeTruthy();
    expect(screen.getByLabelText("Client certificate")).toBeTruthy();
    expect(screen.queryByLabelText("Command")).toBeNull();
    fireEvent.change(screen.getByLabelText("Bearer token"), { target: { value: "t" } });
    fireEvent.change(screen.getByLabelText("Headers"), { target: { value: "H=v" } });
    fireEvent.change(screen.getByLabelText("Extra trusted CA"), { target: { value: "~/ca" } });
    expect(current()).toMatchObject({ bearerToken: "t", headers: "H=v", caCert: "~/ca" });

    fireEvent.click(screen.getByText("Hide advanced"));
    expect(screen.queryByLabelText("Headers")).toBeNull();
  });

  it("locks the name when editing an existing row", () => {
    render(<Harness initial={draftOf("fixed", emptySpec())} nameLocked />);
    const name = screen.getByLabelText("Name") as HTMLInputElement;
    expect(name.disabled).toBe(true);
    expect(screen.queryByText(/prefixes every tool/)).toBeNull();
  });
});
