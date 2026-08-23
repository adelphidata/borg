import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { BackLink } from "./BackLink";

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

describe("BackLink", () => {
  it("navigates to the destination it was given", () => {
    render(
      <MemoryRouter initialEntries={["/connections/slack"]}>
        <BackLink to="/connections" label="Connections" />
        <Routes>
          <Route path="*" element={<Where />} />
        </Routes>
      </MemoryRouter>,
    );
    expect(screen.getByTestId("path").textContent).toBe("/connections/slack");

    fireEvent.click(screen.getByText("Connections"));
    expect(screen.getByTestId("path").textContent).toBe("/connections");
  });

  it("points the chevron backwards", () => {
    // The base glyph points right ("forward"); the 180° turn is what makes this
    // read as a return link. A forward-pointing back link says the wrong thing.
    const { container } = render(
      <MemoryRouter>
        <BackLink to="/connections" label="Connections" />
      </MemoryRouter>,
    );
    expect(container.querySelector("svg")?.style.transform).toBe("rotate(180deg)");
  });
});
