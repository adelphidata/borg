import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { SkillSetup, needsNote } from "./SkillSetup";

const option = (name: string, over: Record<string, unknown> = {}) => ({
  name,
  install: "",
  installable: false,
  url: "",
  ...over,
});

const entry = (needs: unknown[]) =>
  ({
    name: "email",
    description: "Mail.",
    source: "builtin",
    enabled: true,
    available: false,
    mandatory: false,
    category: "core",
    defaultEnabled: true,
    needs,
  }) as never;

const click = (el: HTMLElement) => act(() => el.click());

describe("needsNote", () => {
  it("names each need, joining choices with or and needs with and", () => {
    const bin = (...names: string[]) => ({ kind: "bin", options: names.map((n) => option(n)) });
    const env = (name: string) => ({ kind: "env", options: [option(name)] });
    expect(needsNote([])).toBeUndefined();
    expect(needsNote([bin("himalaya")] as never)).toBe("Needs himalaya");
    expect(needsNote([bin("icalBuddy", "gcalcli")] as never)).toBe("Needs icalBuddy or gcalcli");
    expect(needsNote([bin("curl"), bin("jq"), env("LINEAR_API_KEY")] as never)).toBe(
      "Needs curl, jq and LINEAR_API_KEY",
    );
    // Wrong system wins: nothing to install would help.
    expect(
      needsNote([bin("x"), { kind: "os", options: [option("macos"), option("plan9")] }] as never),
    ).toBe("Runs only on macOS or plan9");
  });
});

describe("SkillSetup", () => {
  it("installs a tool Borg may install, and re-reads the skill after", async () => {
    const onChange = vi.fn();
    const daemon = fakeDaemon({ InstallSkillTool: { entry: entry([]), content: "", path: "" } });
    renderApp(
      <SkillSetup
        entry={entry([
          {
            kind: "bin",
            options: [option("himalaya", { install: "brew install himalaya", installable: true })],
          },
        ])}
        onChange={onChange}
      />,
      { daemon },
    );
    // Borg runs it; the command is not shown for the user to type.
    expect(screen.queryByText("brew install himalaya")).toBeNull();
    click(screen.getByText("Install"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "InstallSkillTool")?.request).toEqual({
        skill: "email",
        tool: "himalaya",
      }),
    );
    await waitFor(() => expect(onChange).toHaveBeenCalled());
  });

  it("shows the command or a link where Borg may not install, and asks for keys", async () => {
    const onChange = vi.fn();
    const daemon = fakeDaemon({ SetSkillSecret: { entry: entry([]), content: "", path: "" } });
    renderApp(
      <SkillSetup
        entry={entry([
          { kind: "bin", options: [option("op", { install: "brew install 1password-cli" })] },
          {
            kind: "bin",
            options: [
              option("docker", { url: "https://docs.docker.com/get-docker/" }),
              option("podman"),
            ],
          },
          { kind: "env", options: [option("LINEAR_API_KEY")] },
        ])}
        onChange={onChange}
      />,
      { daemon },
    );
    expect(screen.queryByText("Install")).toBeNull();
    expect(screen.getByText("brew install 1password-cli")).toBeTruthy();
    expect(screen.getByText("How to install").getAttribute("href")).toBe(
      "https://docs.docker.com/get-docker/",
    );
    expect(screen.getByText("Any one of these:")).toBeTruthy();
    expect(screen.getByText("Install podman, then check again.")).toBeTruthy();

    const field = screen.getByLabelText("LINEAR_API_KEY") as HTMLInputElement;
    expect(field.type).toBe("password");
    const save = screen.getByText("Save").closest("button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.change(field, { target: { value: "lin-123" } });
    click(save);
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSkillSecret")?.request).toEqual({
        skill: "email",
        name: "LINEAR_API_KEY",
        value: "lin-123",
      }),
    );
    await waitFor(() => expect(field.value).toBe(""));
    expect(onChange).toHaveBeenCalled();
  });

  it("shows why an install failed and keeps the way to check again", async () => {
    const onChange = vi.fn();
    const daemon = fakeDaemon({
      InstallSkillTool: new BorgError(
        GrpcStatus.Internal,
        "`brew install himalaya` failed:\nError: No available formula",
      ),
    });
    renderApp(
      <SkillSetup
        entry={entry([
          {
            kind: "bin",
            options: [option("himalaya", { install: "brew install himalaya", installable: true })],
          },
        ])}
        onChange={onChange}
      />,
      { daemon },
    );
    click(screen.getByText("Install"));
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toMatch(/No available formula/),
    );
    expect(onChange).not.toHaveBeenCalled();
    click(screen.getByText("Check again"));
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("says plainly when the skill cannot run on this computer", () => {
    renderApp(
      <SkillSetup
        entry={entry([{ kind: "os", options: [option("macos")] }])}
        onChange={() => {}}
      />,
    );
    expect(screen.getByText("Runs only on macOS. It cannot run on this computer.")).toBeTruthy();
  });
});
