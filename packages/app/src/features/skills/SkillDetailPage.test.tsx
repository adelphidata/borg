import { describe, expect, it } from "vitest";
import { Route, Routes } from "react-router-dom";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { fakeDaemon, renderApp } from "../../test/harness";
import { SkillDetailPage } from "./SkillDetailPage";

function mount(route: string, table: Record<string, unknown> = {}) {
  return renderApp(
    <Routes>
      <Route path="/connections/skills/:name" element={<SkillDetailPage />} />
    </Routes>,
    { route, daemon: fakeDaemon({ ListSkillCatalog: { skills: [] }, ...table }) },
  );
}

const nameField = () => screen.getByLabelText("Name") as HTMLInputElement;
const body = () => screen.getByLabelText("SKILL.md") as HTMLTextAreaElement;

describe("SkillDetailPage, new skill", () => {
  it("keeps the frontmatter name in step with the name field until the body is edited", async () => {
    mount("/connections/skills/new");
    await screen.findByLabelText("Name");
    fireEvent.change(nameField(), { target: { value: "release-notes" } });
    expect(body().value).toContain("name: release-notes");
    expect(body().value).toContain("# release-notes");

    fireEvent.change(body(), { target: { value: "---\nname: mine\n---\n" } });
    fireEvent.change(nameField(), { target: { value: "other" } });
    expect(body().value).toBe("---\nname: mine\n---\n");
  });

  it("takes the copied skill once it arrives, named after the name field", async () => {
    mount("/connections/skills/new?from=email", {
      GetSkill: {
        entry: { name: "email", source: "user" },
        content: "---\nname: email\ndescription: Mail.\n---\n\n# email\n",
        path: "",
      },
    });
    await waitFor(() => expect(body().value).toContain("# email"));
    expect(body().value).toContain("name: email");
    fireEvent.change(nameField(), { target: { value: "mail-2" } });
    expect(body().value).toContain("name: mail-2");
    expect(body().value).toContain("# email");
  });
});
