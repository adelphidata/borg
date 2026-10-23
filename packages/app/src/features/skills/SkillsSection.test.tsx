import { beforeEach, describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { SkillsSection } from "./SkillsSection";
import { SkillDetailPage } from "./SkillDetailPage";
import { frontmatterName, skillTemplate, withFrontmatterName } from "./useSkills";

const skill = (over: Record<string, unknown> = {}) => ({
  name: "git",
  description: "Work with git repositories.",
  source: "builtin",
  enabled: true,
  available: true,
  mandatory: false,
  category: "developer",
  defaultEnabled: true,
  needs: [],
  ...over,
});

const mine = (over: Record<string, unknown> = {}) =>
  skill({
    name: "release-notes",
    description: "Write release notes from merged PRs.",
    source: "user",
    category: "",
    ...over,
  });

const CONTENT =
  "---\nname: release-notes\ndescription: Write release notes from merged PRs.\n---\n\nSteps.\n";

const catalog = (skills: unknown[]) => ({ ListSkillCatalog: { skills } });
const detail = (
  entry: unknown,
  content = CONTENT,
  path = "/home/me/.borg/skills/release-notes/SKILL.md",
) => ({
  GetSkill: { entry, content, path },
});

function mount(daemon: ReturnType<typeof fakeDaemon>, route = "/connections") {
  return renderApp(
    <Routes>
      <Route path="/connections" element={<SkillsSection />} />
      <Route path="/connections/skills/:name" element={<SkillDetailPage />} />
    </Routes>,
    { daemon, route },
  );
}

const click = (el: HTMLElement) => act(() => el.click());
const type = (el: HTMLElement, value: string) => fireEvent.change(el, { target: { value } });
const fold = (name: string) => screen.getByRole("button", { name: new RegExp(`^${name}`) });

// Open folds outlive a page in sessionStorage; each test starts with none.
beforeEach(() => sessionStorage.clear());

describe("skill helpers", () => {
  it("reads the frontmatter name and only that", () => {
    expect(frontmatterName(CONTENT)).toBe("release-notes");
    expect(frontmatterName('---\nname: "quoted"\n---\nname: body')).toBe("quoted");
    expect(frontmatterName("no frontmatter\nname: x")).toBeUndefined();
    expect(frontmatterName("---\nname: open")).toBeUndefined();
  });

  it("renames only the frontmatter, never a name: line in the body", () => {
    const body = "---\nname: old\ndescription: d\n---\n\n```yaml\nname: keep\n```\n";
    expect(withFrontmatterName(body, "new")).toBe(
      "---\nname: new\ndescription: d\n---\n\n```yaml\nname: keep\n```\n",
    );
    // Frontmatter without a name line gets one; a file without frontmatter
    // is left alone.
    expect(withFrontmatterName("---\ndescription: d\n---\nx", "n")).toBe(
      "---\nname: n\ndescription: d\n---\nx",
    );
    expect(withFrontmatterName("plain", "n")).toBe("plain");
  });

  it("starts a new skill with the name already in its frontmatter", () => {
    expect(frontmatterName(skillTemplate("my-thing"))).toBe("my-thing");
    expect(frontmatterName(skillTemplate("  "))).toBe("my-skill");
  });
});

describe("SkillsSection", () => {
  it("folds the skills by category: Core first, then the listed order, then Custom", async () => {
    const daemon = fakeDaemon(
      catalog([
        skill({ name: "git" }),
        skill({ name: "stripe-link", category: "payments" }),
        // Case does not split a category.
        skill({ name: "browser", category: "Core" }),
        skill({ name: "notes", category: "utilities" }),
        skill({ name: "linear", category: "productivity" }),
        // A built-in no listed category takes lands in Custom.
        skill({ name: "zap", category: "zeta-tools" }),
        // A skill you added is custom whatever category it names.
        mine({ category: "developer" }),
        mine({ name: "from-repo", source: "project" }),
      ]),
    );
    mount(daemon);
    await waitFor(() => expect(fold("Core")).toBeTruthy());
    const order = screen
      .getAllByRole("button", { expanded: false })
      .map(
        (b) =>
          /^(Core|Utilities|Payments|Developer|Productivity|Custom)/.exec(b.textContent ?? "")?.[1],
      );
    expect(order).toEqual(["Core", "Utilities", "Payments", "Developer", "Productivity", "Custom"]);
    expect(fold("Developer").textContent).toMatch(/1$/);
    expect(fold("Custom").textContent).toMatch(/3$/);
    // Folded until asked: no switch shows before a category opens.
    expect(screen.queryByRole("switch")).toBeNull();
    click(fold("Custom"));
    expect(fold("Custom").getAttribute("aria-expanded")).toBe("true");
    for (const name of ["zap", "release-notes", "from-repo"]) {
      expect(screen.getByRole("switch", { name: `Enable ${name}` })).toBeTruthy();
    }
    expect(screen.queryByRole("switch", { name: "Enable git" })).toBeNull();
    // Only a skill that is not built in carries a tag, and it says where it came from.
    expect(screen.getByText("Yours")).toBeTruthy();
    expect(screen.getByText("From this project")).toBeTruthy();
  });

  it("counts what is on and what needs setup, then opens to the switches and notes", async () => {
    const daemon = fakeDaemon(
      catalog([
        skill({ name: "browser", category: "core" }),
        skill({ name: "scheduler", category: "core", mandatory: true }),
        skill({
          name: "aws",
          category: "core",
          available: false,
          needs: [
            { kind: "bin", options: [{ name: "aws" }] },
            { kind: "env", options: [{ name: "AWS_PROFILE" }] },
          ],
        }),
        // A skill that is off waits on nothing yet: it is not "needs setup".
        skill({
          name: "calendar",
          category: "core",
          enabled: false,
          available: false,
          needs: [{ kind: "bin", options: [{ name: "icalBuddy" }, { name: "gcalcli" }] }],
        }),
        skill({ name: "weather", category: "utilities", enabled: false, available: false }),
      ]),
    );
    mount(daemon);
    await waitFor(() => expect(fold("Core")).toBeTruthy());
    // The fold hides each row's note, so the fold itself says what needs a look.
    expect(within(fold("Core")).getByText("3 on")).toBeTruthy();
    expect(within(fold("Core")).getByText("1 needs setup")).toBeTruthy();
    expect(fold("Core").textContent).toMatch(/4$/);
    // Nothing on, nothing to flag.
    expect(fold("Utilities").textContent).not.toMatch(/ on|setup/);

    click(fold("Core"));
    const always = screen.getByRole("switch", { name: "Enable scheduler" }) as HTMLButtonElement;
    expect(always.disabled).toBe(true);
    expect(screen.getByText("Always on")).toBeTruthy();
    // The row names what is missing instead of only saying something is.
    expect(screen.getByText("Needs aws and AWS_PROFILE")).toBeTruthy();
    expect(screen.queryByText(/icalBuddy/)).toBeNull();
    expect(
      screen.getByRole("switch", { name: "Enable calendar" }).getAttribute("aria-checked"),
    ).toBe("false");

    // Only a skill of yours can be deleted from the list.
    expect(screen.queryByLabelText("Delete browser")).toBeNull();
  });

  it("turns a skill off and on through the daemon", async () => {
    const daemon = fakeDaemon({
      ...catalog([skill()]),
      SetSkillEnabled: { entry: skill({ enabled: false }), content: "", path: "" },
    });
    mount(daemon);
    await waitFor(() => expect(fold("Developer")).toBeTruthy());
    click(fold("Developer"));
    click(screen.getByRole("switch", { name: "Enable git" }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSkillEnabled")?.request).toEqual({
        name: "git",
        enabled: false,
      }),
    );
  });

  it("asks before deleting and deletes on confirmation", async () => {
    const daemon = fakeDaemon({ ...catalog([mine()]), DeleteSkill: {} });
    mount(daemon);
    await waitFor(() => expect(fold("Custom")).toBeTruthy());
    click(fold("Custom"));
    click(screen.getByLabelText("Delete release-notes"));
    expect(screen.getByText(/Delete release-notes\?/)).toBeTruthy();
    click(screen.getByText("Keep"));
    expect(daemon.calls.some((c) => c.method === "DeleteSkill")).toBe(false);
    click(screen.getByLabelText("Delete release-notes"));
    click(screen.getByText("Delete"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "DeleteSkill")?.request).toEqual({
        name: "release-notes",
      }),
    );
  });

  it("shows a refused toggle inline", async () => {
    const daemon = fakeDaemon({
      ...catalog([skill()]),
      SetSkillEnabled: new BorgError(GrpcStatus.Internal, "settings table is locked"),
    });
    mount(daemon);
    await waitFor(() => expect(fold("Developer")).toBeTruthy());
    click(fold("Developer"));
    click(screen.getByRole("switch", { name: "Enable git" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toMatch(/locked/));
  });

  it("reports a failed listing", async () => {
    const daemon = fakeDaemon({ ListSkillCatalog: new BorgError(GrpcStatus.Unavailable, "down") });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load your skills.")).toBeTruthy());
  });

  it("opens a row's detail page, comes back to the same folds, and opens the new-skill page", async () => {
    const daemon = fakeDaemon({ ...catalog([mine(), skill()]), ...detail(mine()) });
    mount(daemon);
    await waitFor(() => expect(fold("Custom")).toBeTruthy());
    click(fold("Custom"));
    click(screen.getByRole("button", { name: /^release-notes/ }));
    await waitFor(() => expect(screen.getByLabelText("SKILL.md")).toBeTruthy());
    // The back link, not the page title.
    click(screen.getAllByText("Connections").find((el) => el.tagName === "SPAN")!);
    await waitFor(() => expect(screen.getByText("New skill")).toBeTruthy());
    await waitFor(() => expect(fold("Custom").getAttribute("aria-expanded")).toBe("true"));
    expect(fold("Developer").getAttribute("aria-expanded")).toBe("false");
    click(screen.getByText("New skill"));
    await waitFor(() => expect(screen.getByText("Create skill")).toBeTruthy());
  });
});

describe("SkillDetailPage for a skill of yours", () => {
  it("edits the file in place and saves it under the same name", async () => {
    const daemon = fakeDaemon({
      ...catalog([mine()]),
      ...detail(mine()),
      PutSkill: { entry: mine(), content: CONTENT, path: "" },
    });
    mount(daemon, "/connections/skills/release-notes");
    await waitFor(() => expect(screen.getByLabelText("SKILL.md")).toBeTruthy());
    expect(screen.getByText("/home/me/.borg/skills/release-notes/SKILL.md")).toBeTruthy();
    expect(screen.getByText("Yours")).toBeTruthy();
    const save = screen.getByText("Save changes") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    type(screen.getByLabelText("SKILL.md"), CONTENT + "More.\n");
    click(save);
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "PutSkill")?.request).toEqual({
        name: "release-notes",
        content: CONTENT + "More.\n",
      }),
    );
  });

  it("refuses to save a file whose frontmatter renames the skill", async () => {
    const daemon = fakeDaemon({ ...catalog([mine()]), ...detail(mine()) });
    mount(daemon, "/connections/skills/release-notes");
    await waitFor(() => expect(screen.getByLabelText("SKILL.md")).toBeTruthy());
    type(screen.getByLabelText("SKILL.md"), CONTENT.replace("release-notes", "other"));
    click(screen.getByText("Save changes"));
    expect(screen.getByRole("alert").textContent).toMatch(/name: other/);
    expect(daemon.calls.some((c) => c.method === "PutSkill")).toBe(false);
    click(screen.getByText("Discard"));
    expect((screen.getByLabelText("SKILL.md") as HTMLTextAreaElement).value).toBe(CONTENT);
  });

  it("deletes after confirmation and returns to the list", async () => {
    const daemon = fakeDaemon({ ...catalog([mine()]), ...detail(mine()), DeleteSkill: {} });
    mount(daemon, "/connections/skills/release-notes");
    await waitFor(() => expect(screen.getByText("Delete this skill")).toBeTruthy());
    click(screen.getByText("Delete this skill"));
    click(screen.getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "DeleteSkill")).toBe(true));
    await waitFor(() => expect(fold("Custom")).toBeTruthy());
  });
});

describe("SkillDetailPage for a built-in", () => {
  it("shows the file read-only and offers a copy under a new name", async () => {
    const daemon = fakeDaemon({
      ...catalog([skill()]),
      ...detail(skill(), "---\nname: git\n---\nUse git.", ""),
    });
    mount(daemon, "/connections/skills/git");
    await waitFor(() => expect(screen.getByText("Copy to a new skill")).toBeTruthy());
    expect(screen.queryByLabelText("SKILL.md")).toBeNull();
    expect(screen.queryByText("Delete this skill")).toBeNull();
    expect(screen.getByText("Built in · developer")).toBeTruthy();
    click(screen.getByText("Copy to a new skill"));
    await waitFor(() => expect(screen.getByText("Copy of git")).toBeTruthy());
    // The copy's frontmatter follows the new name as it is typed.
    type(screen.getByLabelText("Name"), "my-git");
    await waitFor(() =>
      expect((screen.getByLabelText("SKILL.md") as HTMLTextAreaElement).value).toMatch(
        /^---\nname: my-git\n/,
      ),
    );
  });
});

describe("SkillDetailPage for a new skill", () => {
  it("creates the skill from the template and opens it", async () => {
    const daemon = fakeDaemon({
      ...catalog([]),
      PutSkill: { entry: mine({ name: "notes" }), content: skillTemplate("notes"), path: "/p" },
      ...detail(mine({ name: "notes" }), skillTemplate("notes"), "/p"),
    });
    mount(daemon, "/connections/skills/new");
    await waitFor(() => expect(screen.getByText("New skill")).toBeTruthy());
    const create = screen.getByText("Create skill") as HTMLButtonElement;
    expect(create.disabled).toBe(true);
    type(screen.getByLabelText("Name"), "notes");
    expect((screen.getByLabelText("SKILL.md") as HTMLTextAreaElement).value).toBe(
      skillTemplate("notes"),
    );
    click(create);
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "PutSkill")?.request).toEqual({
        name: "notes",
        content: skillTemplate("notes"),
      }),
    );
    await waitFor(() => expect(screen.getByText("Delete this skill")).toBeTruthy());
  });

  it("refuses a bad name or a mismatched frontmatter before the daemon is asked", async () => {
    const daemon = fakeDaemon(catalog([]));
    mount(daemon, "/connections/skills/new");
    await waitFor(() => expect(screen.getByLabelText("Name")).toBeTruthy());
    // Nothing to copy from, so nothing is fetched.
    expect(daemon.calls.some((c) => c.method === "GetSkill")).toBe(false);
    type(screen.getByLabelText("Name"), "has space");
    click(screen.getByText("Create skill"));
    expect(screen.getByRole("alert").textContent).toMatch(/letters, digits/);
    type(screen.getByLabelText("Name"), "fine");
    type(screen.getByLabelText("SKILL.md"), "---\nname: other\n---\nbody");
    // Typing in the body stops the template following the name.
    type(screen.getByLabelText("Name"), "fine2");
    expect((screen.getByLabelText("SKILL.md") as HTMLTextAreaElement).value).toMatch(/name: other/);
    click(screen.getByText("Create skill"));
    expect(screen.getByRole("alert").textContent).toMatch(/name: other/);
    expect(daemon.calls.some((c) => c.method === "PutSkill")).toBe(false);
  });

  it("shows the daemon's refusal and keeps the draft", async () => {
    const daemon = fakeDaemon({
      ...catalog([]),
      PutSkill: new BorgError(
        INVALID_ARGUMENT as never,
        "SKILL.md must start with YAML frontmatter",
      ),
    });
    mount(daemon, "/connections/skills/new");
    await waitFor(() => expect(screen.getByLabelText("Name")).toBeTruthy());
    type(screen.getByLabelText("Name"), "x");
    type(screen.getByLabelText("SKILL.md"), "just prose");
    click(screen.getByText("Create skill"));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toMatch(/YAML frontmatter/));
    expect((screen.getByLabelText("SKILL.md") as HTMLTextAreaElement).value).toBe("just prose");
  });
});
