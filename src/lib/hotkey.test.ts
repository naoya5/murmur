import { describe, expect, it } from "vitest";
import {
  comboFromEvent,
  comboTokens,
  displayAccelerator,
  isComplete,
  keyFromCode,
  parseAccelerator,
  toAccelerator,
} from "./hotkey";

const ev = (
  code: string,
  mods: Partial<Record<"ctrlKey" | "altKey" | "shiftKey" | "metaKey", boolean>> = {},
) => ({
  code,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  metaKey: false,
  ...mods,
});

describe("keyFromCode", () => {
  it("maps letters, digits, function keys and named keys", () => {
    expect(keyFromCode("KeyD")).toBe("D");
    expect(keyFromCode("Digit1")).toBe("1");
    expect(keyFromCode("F5")).toBe("F5");
    expect(keyFromCode("F12")).toBe("F12");
    expect(keyFromCode("Space")).toBe("Space");
    expect(keyFromCode("ArrowUp")).toBe("Up");
    expect(keyFromCode("Comma")).toBe("Comma");
  });

  it("rejects keys that cannot be a hotkey", () => {
    expect(keyFromCode("ShiftLeft")).toBeNull();
    expect(keyFromCode("Escape")).toBeNull();
    expect(keyFromCode("Backspace")).toBeNull();
    expect(keyFromCode("CapsLock")).toBeNull();
  });
});

describe("comboFromEvent and toAccelerator", () => {
  it("uses the physical key, so Option+D is D and not a symbol", () => {
    const combo = comboFromEvent(ev("KeyD", { altKey: true }));
    expect(toAccelerator(combo)).toBe("Alt+D");
  });

  it("orders modifiers as Ctrl, Alt, Cmd, Shift, like the Rust tests", () => {
    const combo = comboFromEvent(
      ev("Space", { metaKey: true, shiftKey: true, ctrlKey: true, altKey: true }),
    );
    expect(toAccelerator(combo)).toBe("Ctrl+Alt+Cmd+Shift+Space");
  });

  it("treats a modifier-only event as incomplete", () => {
    const combo = comboFromEvent(ev("AltLeft", { altKey: true }));
    expect(combo.key).toBe("");
    expect(isComplete(combo)).toBe(false);
  });

  it("needs a modifier for letters and digits but not for function keys", () => {
    expect(isComplete(comboFromEvent(ev("KeyA")))).toBe(false);
    expect(isComplete(comboFromEvent(ev("KeyA", { metaKey: true })))).toBe(true);
    expect(isComplete(comboFromEvent(ev("F5")))).toBe(true);
    expect(isComplete(comboFromEvent(ev("Space")))).toBe(false);
    expect(isComplete(comboFromEvent(ev("Space", { altKey: true })))).toBe(true);
  });
});

describe("parseAccelerator", () => {
  it("round-trips the accelerators the Rust tests use", () => {
    for (const acc of [
      "Alt+Space",
      "Cmd+Shift+D",
      "F5",
      "Alt+Shift+Space",
      "Ctrl+Alt+Cmd+Shift+Space",
    ]) {
      expect(toAccelerator(parseAccelerator(acc))).toBe(acc);
    }
  });

  it("accepts aliases and any case", () => {
    expect(toAccelerator(parseAccelerator("option+space"))).toBe("Alt+Space");
    expect(toAccelerator(parseAccelerator("Command+Control+k"))).toBe("Ctrl+Cmd+K");
    expect(toAccelerator(parseAccelerator("Super+ArrowUp"))).toBe("Cmd+Up");
  });
});

describe("display", () => {
  it("shows HIG symbols in HIG order", () => {
    expect(displayAccelerator("Alt+Space")).toBe("⌥ Space");
    expect(displayAccelerator("Cmd+Shift+D")).toBe("⇧ ⌘ D");
    expect(displayAccelerator("Ctrl+Alt+Shift+Cmd+Up")).toBe("⌃ ⌥ ⇧ ⌘ ↑");
    expect(comboTokens(parseAccelerator("F5"))).toEqual(["F5"]);
  });
});
