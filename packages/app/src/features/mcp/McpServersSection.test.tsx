import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { McpServersSection } from "./McpServersSection";
import { McpServerDetailPage } from "./McpServerDetailPage";
import { emptySpec } from "./useMcpServers";

const entry = (over: Record<string, unknown> = {}, spec: Record<string, unknown> = {}) => ({
  name: "time",
  spec: { ...emptySpec("stdio"), command: "uvx", args: ["mcp-server-time"], ...spec },
  state: "connected",
  toolCount: 2,
  detail: "uvx mcp-server-time",
  ...over,
});

const listing = (servers: unknown[], over: Record<string, unknown> = {}) => ({
  ListMcpServers: {
    servers,
    mcpEnabled: true,
    presets: ["time", "filesystem", "github", "notion"],
    ...over,
  },
});

function mount(daemon: ReturnType<typeof fakeDaemon>, route = "/connections") {
  return renderApp(
    <Routes>
      <Route path="/connections" element={<McpServersSection />} />
      <Route path="/connections/mcp/:name" element={<McpServerDetailPage />} />
    </Routes>,
    { daemon, route },
  );
}

const click = (el: HTMLElement) => act(() => el.click());
const type = (el: HTMLElement, value: string) => fireEvent.change(el, { target: { value } });

describe("McpServersSection listing", () => {
  it("says what each server is, whether it answers, and how many tools it brought", async () => {
    const daemon = fakeDaemon(
      listing([
        entry(),
        entry({ name: "notion", state: "auth-failed", toolCount: 0, detail: "https://n/mcp" }),
        entry(
          { name: "off", state: "disabled", toolCount: 0, detail: "npx off" },
          { enabled: false },
        ),
      ]),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("time")).toBeTruthy());
    expect(screen.getByText("uvx mcp-server-time")).toBeTruthy();
    expect(screen.getByText("Connected · 2 tools")).toBeTruthy();
    expect(screen.getByText("Sign-in failed")).toBeTruthy();
    // A row switched off says so, whatever the registry last reported.
    expect(screen.getAllByText("Off")).toHaveLength(1);
    expect(screen.getByText("1 connected")).toBeTruthy();
  });

  it("says the list is empty, and hides a template once a server has its name", async () => {
    const daemon = fakeDaemon(listing([entry({ name: "github" })]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("github")).toBeTruthy());
    expect(screen.queryByText("No MCP servers yet.")).toBeNull();
    expect(screen.queryByRole("button", { name: /^github\s*·/ })).toBeNull();
    expect(screen.getByRole("button", { name: /^time\s*·/ })).toBeTruthy();
  });

  it("explains when MCP is off and turns it on in one click", async () => {
    const daemon = fakeDaemon({
      ...listing([entry()], { mcpEnabled: false }),
      SetSetting: {},
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText(/MCP is turned off/)).toBeTruthy());
    // The row explains too, instead of claiming a connection that cannot exist.
    expect(screen.getByText("MCP is off")).toBeTruthy();
    click(screen.getByText("Turn on MCP"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "mcp.enabled",
        value: "true",
      }),
    );
  });

  it("reports a failed listing", async () => {
    const daemon = fakeDaemon({
      ListMcpServers: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load your MCP servers.")).toBeTruthy());
  });
});

describe("McpServersSection writes", () => {
  it("adds a server from the form with the spec the daemon expects", async () => {
    const daemon = fakeDaemon({ ...listing([]), PutMcpServer: entry({ name: "gh" }) });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("No MCP servers yet.")).toBeTruthy());
    click(screen.getByText("Add an MCP server"));
    type(screen.getByLabelText("Name"), "gh");
    type(screen.getByLabelText("Command"), "npx -y @modelcontextprotocol/server-github");
    click(screen.getByText("Add server"));
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "PutMcpServer")).toBe(true));
    const put = daemon.calls.find((c) => c.method === "PutMcpServer")!.request as {
      name: string;
      overwrite: boolean;
      preset: string;
      spec: { command: string; args: string[]; enabled: boolean };
    };
    expect(put.name).toBe("gh");
    expect(put.overwrite).toBe(false);
    expect(put.preset).toBe("");
    expect(put.spec).toMatchObject({
      enabled: true,
      command: "npx",
      args: ["-y", "@modelcontextprotocol/server-github"],
    });
    // The form closes and the list is re-read.
    await waitFor(() => expect(screen.queryByLabelText("Command")).toBeNull());
    expect(daemon.calls.filter((c) => c.method === "ListMcpServers").length).toBeGreaterThan(1);
  });

  it("keeps the form, and what was typed, when the daemon refuses", async () => {
    const daemon = fakeDaemon({
      ...listing([]),
      PutMcpServer: new BorgError(INVALID_ARGUMENT as never, "server id must match"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Add an MCP server")).toBeTruthy());
    click(screen.getByText("Add an MCP server"));
    type(screen.getByLabelText("Name"), "bad name");
    type(screen.getByLabelText("Command"), "uvx t");
    click(screen.getByText("Add server"));
    await waitFor(() => expect(screen.getByText(/server id must match/)).toBeTruthy());
    expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("bad name");
  });

  it("checks the form before asking the daemon", async () => {
    const daemon = fakeDaemon(listing([]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Add an MCP server")).toBeTruthy());
    click(screen.getByText("Add an MCP server"));
    type(screen.getByLabelText("Name"), "x");
    click(screen.getByText("Add server"));
    expect(screen.getByRole("alert").textContent).toMatch(/Enter the command/);
    expect(daemon.calls.some((c) => c.method === "PutMcpServer")).toBe(false);
  });

  it("adds a template by name, as a preset rather than a spec", async () => {
    const daemon = fakeDaemon({ ...listing([]), PutMcpServer: entry() });
    mount(daemon);
    await waitFor(() => expect(screen.getByRole("button", { name: /^time\s*·/ })).toBeTruthy());
    click(screen.getByRole("button", { name: /^time\s*·/ }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "PutMcpServer")?.request).toMatchObject({
        name: "time",
        preset: "time",
        overwrite: false,
      }),
    );
  });

  it("switches a server off by rewriting its row with enabled false", async () => {
    const daemon = fakeDaemon({ ...listing([entry()]), PutMcpServer: entry() });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("time")).toBeTruthy());
    click(screen.getByRole("switch", { name: "Enable time" }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "PutMcpServer")?.request).toMatchObject({
        name: "time",
        overwrite: true,
        spec: { enabled: false, command: "uvx", args: ["mcp-server-time"] },
      }),
    );
  });

  it("asks before removing, and removes on confirmation", async () => {
    const daemon = fakeDaemon({ ...listing([entry()]), RemoveMcpServer: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("time")).toBeTruthy());
    click(screen.getByLabelText("Delete time"));
    expect(screen.getByText(/Remove time\?/)).toBeTruthy();
    click(screen.getByText("Keep"));
    expect(screen.queryByText(/Remove time\?/)).toBeNull();
    expect(daemon.calls.some((c) => c.method === "RemoveMcpServer")).toBe(false);
    click(screen.getByLabelText("Delete time"));
    click(screen.getByText("Remove"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "RemoveMcpServer")?.request).toEqual({
        name: "time",
      }),
    );
  });

  it("shows a write failure inline", async () => {
    const daemon = fakeDaemon({
      ...listing([entry()]),
      RemoveMcpServer: new BorgError(GrpcStatus.Internal, "disk is read-only"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("time")).toBeTruthy());
    click(screen.getByLabelText("Delete time"));
    click(screen.getByText("Remove"));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toMatch(/read-only/));
  });

  it("opens a row's detail page", async () => {
    const daemon = fakeDaemon(listing([entry()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("time")).toBeTruthy());
    click(screen.getByRole("button", { name: /^time/ }));
    await waitFor(() => expect(screen.getByText("Test connection")).toBeTruthy());
  });
});

describe("McpServerDetailPage", () => {
  it("shows the stored connection and saves an edit as an overwrite", async () => {
    const daemon = fakeDaemon({ ...listing([entry()]), PutMcpServer: entry() });
    mount(daemon, "/connections/mcp/time");
    await waitFor(() => expect(screen.getByText("Connected · 2 tools")).toBeTruthy());
    const command = screen.getByLabelText("Command") as HTMLInputElement;
    expect(command.value).toBe("uvx mcp-server-time");
    const save = screen.getByText("Save changes") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    type(command, "uvx mcp-server-time --local-timezone UTC");
    expect(save.disabled).toBe(false);
    click(save);
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "PutMcpServer")?.request).toMatchObject({
        name: "time",
        overwrite: true,
        spec: { enabled: true, args: ["mcp-server-time", "--local-timezone", "UTC"] },
      }),
    );
    await waitFor(() =>
      expect((screen.getByText("Save changes") as HTMLButtonElement).disabled).toBe(true),
    );
  });

  it("discards an edit without asking the daemon", async () => {
    const daemon = fakeDaemon(listing([entry()]));
    mount(daemon, "/connections/mcp/time");
    await waitFor(() => expect(screen.getByLabelText("Command")).toBeTruthy());
    type(screen.getByLabelText("Command"), "changed");
    click(screen.getByText("Discard"));
    expect((screen.getByLabelText("Command") as HTMLInputElement).value).toBe(
      "uvx mcp-server-time",
    );
    expect(daemon.calls.some((c) => c.method === "PutMcpServer")).toBe(false);
  });

  it("refuses a bad edit locally", async () => {
    const daemon = fakeDaemon(listing([entry()]));
    mount(daemon, "/connections/mcp/time");
    await waitFor(() => expect(screen.getByLabelText("Command")).toBeTruthy());
    type(screen.getByLabelText("Command"), "");
    click(screen.getByText("Save changes"));
    expect(screen.getByRole("alert").textContent).toMatch(/Enter the command/);
  });

  it("tests the connection and lists the tools it found", async () => {
    const daemon = fakeDaemon({
      ...listing([entry()]),
      TestMcpServer: {
        ok: true,
        error: "",
        serverInfo: "mcp-server-time 1.2",
        tools: ["get_time", "convert"],
      },
    });
    mount(daemon, "/connections/mcp/time");
    await waitFor(() => expect(screen.getByText("Test connection")).toBeTruthy());
    click(screen.getByText("Test connection"));
    await waitFor(() =>
      expect(screen.getByText(/Connected to mcp-server-time 1.2 · 2 tools/)).toBeTruthy(),
    );
    expect(screen.getByText("get_time")).toBeTruthy();
    expect(daemon.calls.find((c) => c.method === "TestMcpServer")?.request).toEqual({
      name: "time",
    });
  });

  it("shows a failed test as an answer, not a crash", async () => {
    const daemon = fakeDaemon({
      ...listing([entry()]),
      TestMcpServer: {
        ok: false,
        error: "handshake timed out after 60s",
        serverInfo: "",
        tools: [],
      },
    });
    mount(daemon, "/connections/mcp/time");
    await waitFor(() => expect(screen.getByText("Test connection")).toBeTruthy());
    click(screen.getByText("Test connection"));
    await waitFor(() => expect(screen.getByText(/handshake timed out/)).toBeTruthy());
  });

  it("removes the server and goes back to the list", async () => {
    const daemon = fakeDaemon({ ...listing([entry()]), RemoveMcpServer: {} });
    mount(daemon, "/connections/mcp/time");
    await waitFor(() => expect(screen.getByText("Remove this server")).toBeTruthy());
    click(screen.getByText("Remove this server"));
    click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "RemoveMcpServer")).toBe(true),
    );
    await waitFor(() => expect(screen.getByText("Add an MCP server")).toBeTruthy());
  });

  it("says when there is no such server", async () => {
    const daemon = fakeDaemon(listing([]));
    mount(daemon, "/connections/mcp/ghost");
    await waitFor(() => expect(screen.getByText(/no server called ghost/)).toBeTruthy());
  });
});
