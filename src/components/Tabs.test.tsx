import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { Tabs, tabPanelId, useTabsId } from "./Tabs";

type Tab = "one" | "two" | "three";

function Harness() {
  const [tab, setTab] = useState<Tab>("one");
  const id = useTabsId();
  return (
    <>
      <Tabs<Tab>
        idBase={id}
        label="Sections"
        value={tab}
        onChange={setTab}
        options={[
          { value: "one", label: "One" },
          { value: "two", label: "Two" },
          { value: "three", label: "Three" },
        ]}
      />
      <div role="tabpanel" id={tabPanelId(id, tab)}>
        {tab}
      </div>
    </>
  );
}

afterEach(cleanup);

describe("Tabs", () => {
  it("selects a tab on click and links it to its panel", () => {
    render(<Harness />);
    fireEvent.click(screen.getByRole("tab", { name: "Two" }));
    const tab = screen.getByRole("tab", { name: "Two" });
    expect(tab.getAttribute("aria-selected")).toBe("true");
    expect(tab.getAttribute("aria-controls")).toBe(screen.getByRole("tabpanel").id);
    expect(screen.getByRole("tabpanel").textContent).toBe("two");
  });

  it("keeps only the selected tab in the tab order", () => {
    render(<Harness />);
    expect(screen.getByRole("tab", { name: "One" }).tabIndex).toBe(0);
    expect(screen.getByRole("tab", { name: "Two" }).tabIndex).toBe(-1);
  });

  it("moves with the arrow keys, Home and End, wrapping at the ends", () => {
    render(<Harness />);
    const list = screen.getByRole("tablist");
    fireEvent.keyDown(list, { key: "ArrowLeft" });
    expect(screen.getByRole("tabpanel").textContent).toBe("three");
    expect(document.activeElement).toBe(screen.getByRole("tab", { name: "Three" }));
    fireEvent.keyDown(list, { key: "ArrowRight" });
    expect(screen.getByRole("tabpanel").textContent).toBe("one");
    fireEvent.keyDown(list, { key: "End" });
    expect(screen.getByRole("tabpanel").textContent).toBe("three");
    fireEvent.keyDown(list, { key: "Home" });
    expect(screen.getByRole("tabpanel").textContent).toBe("one");
  });
});
