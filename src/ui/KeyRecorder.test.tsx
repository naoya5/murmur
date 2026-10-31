import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { KeyRecorder } from "./KeyRecorder";

afterEach(cleanup);

function setup(value: string | null = "Alt+Space", clearable = false) {
  const onChange = vi.fn();
  const onListenStart = vi.fn();
  const onListenEnd = vi.fn();
  render(
    <KeyRecorder
      label="Hotkey"
      value={value}
      clearable={clearable}
      onChange={onChange}
      onListenStart={onListenStart}
      onListenEnd={onListenEnd}
    />,
  );
  const button = screen.getByRole("button", { name: "Hotkey" });
  return { button, onChange, onListenStart, onListenEnd };
}

describe("KeyRecorder", () => {
  it("shows the current combo as symbols", () => {
    setup("Cmd+Shift+D");
    expect(screen.getByText("⇧")).toBeTruthy();
    expect(screen.getByText("⌘")).toBeTruthy();
    expect(screen.getByText("D")).toBeTruthy();
  });

  it("suspends hotkeys while listening and resumes when a combo is captured", () => {
    const { button, onChange, onListenStart, onListenEnd } = setup();
    fireEvent.click(button);
    expect(onListenStart).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Press keys")).toBeTruthy();

    fireEvent.keyDown(button, { code: "KeyK", key: "˚", metaKey: true, altKey: true });
    expect(onChange).toHaveBeenCalledWith("Alt+Cmd+K");
    expect(onListenEnd).toHaveBeenCalledTimes(1);
  });

  it("ignores modifier-only presses and explains", () => {
    const { button, onChange } = setup();
    fireEvent.click(button);
    fireEvent.keyDown(button, { code: "AltLeft", altKey: true });
    fireEvent.keyUp(button, { code: "AltLeft" });
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByText("Add a letter or key to the modifiers.")).toBeTruthy();
  });

  it("cancels with Escape and resumes hotkeys", () => {
    const { button, onChange, onListenEnd } = setup();
    fireEvent.click(button);
    fireEvent.keyDown(button, { code: "Escape" });
    expect(onChange).not.toHaveBeenCalled();
    expect(onListenEnd).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Space")).toBeTruthy();
  });

  it("resumes hotkeys when focus leaves", () => {
    const { button, onListenEnd } = setup();
    fireEvent.click(button);
    fireEvent.blur(button);
    expect(onListenEnd).toHaveBeenCalledTimes(1);
  });

  it("clears with Backspace only when clearable", () => {
    const first = setup("Alt+Shift+Space", false);
    fireEvent.click(first.button);
    fireEvent.keyDown(first.button, { code: "Backspace" });
    expect(first.onChange).not.toHaveBeenCalled();
    cleanup();

    const second = setup("Alt+Shift+Space", true);
    fireEvent.click(second.button);
    fireEvent.keyDown(second.button, { code: "Backspace" });
    expect(second.onChange).toHaveBeenCalledWith(null);
  });

  it("resumes hotkeys on unmount while listening", () => {
    const onListenEnd = vi.fn();
    const { unmount } = render(
      <KeyRecorder label="Hotkey" value="F5" onChange={() => {}} onListenEnd={onListenEnd} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Hotkey" }));
    unmount();
    expect(onListenEnd).toHaveBeenCalledTimes(1);
  });
});
