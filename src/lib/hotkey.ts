// Accelerators use the shape the Rust side registers: "Alt+Space", "Cmd+Shift+D", "F5".

export type Combo = {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  cmd: boolean;
  /** Accelerator key name, for example "Space", "D", "F5". Empty while only modifiers are down. */
  key: string;
};

const CODE_TO_KEY: Record<string, string> = {
  Space: "Space",
  Enter: "Enter",
  Tab: "Tab",
  ArrowUp: "Up",
  ArrowDown: "Down",
  ArrowLeft: "Left",
  ArrowRight: "Right",
  Minus: "Minus",
  Equal: "Equal",
  Comma: "Comma",
  Period: "Period",
  Slash: "Slash",
  Semicolon: "Semicolon",
  Quote: "Quote",
  BracketLeft: "BracketLeft",
  BracketRight: "BracketRight",
  Backslash: "Backslash",
  Backquote: "Backquote",
  Delete: "Delete",
  Home: "Home",
  End: "End",
  PageUp: "PageUp",
  PageDown: "PageDown",
  Insert: "Insert",
};

const KEY_SYMBOL: Record<string, string> = {
  Space: "Space",
  Enter: "↩",
  Tab: "⇥",
  Up: "↑",
  Down: "↓",
  Left: "←",
  Right: "→",
  Minus: "-",
  Equal: "=",
  Comma: ",",
  Period: ".",
  Slash: "/",
  Semicolon: ";",
  Quote: "'",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Backquote: "`",
  Delete: "⌦",
  Backspace: "⌫",
  Home: "↖",
  End: "↘",
  PageUp: "⇞",
  PageDown: "⇟",
  Escape: "⎋",
};

/** Map a DOM `KeyboardEvent.code` to an accelerator key name, or null when it is not a hotkey key. */
export function keyFromCode(code: string): string | null {
  if (/^Key[A-Z]$/.test(code)) {
    return code.slice(3);
  }
  if (/^Digit[0-9]$/.test(code)) {
    return code.slice(5);
  }
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) {
    return code;
  }
  return CODE_TO_KEY[code] ?? null;
}

export function isModifierCode(code: string): boolean {
  return (
    /^(Shift|Control|Alt|Meta)(Left|Right)?$/.test(code) || code === "CapsLock" || code === "Fn"
  );
}

export type KeyLike = {
  code: string;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  metaKey: boolean;
};

/** The combo a key event represents. Modifier-only events give an empty `key`. */
export function comboFromEvent(event: KeyLike): Combo {
  const key = isModifierCode(event.code) ? "" : (keyFromCode(event.code) ?? "");
  return {
    ctrl: event.ctrlKey,
    alt: event.altKey,
    shift: event.shiftKey,
    cmd: event.metaKey,
    key,
  };
}

export function hasModifier(combo: Combo): boolean {
  return combo.ctrl || combo.alt || combo.shift || combo.cmd;
}

/** A combo the hotkey adapter can register: a key plus, for letters and digits, at least one modifier. */
export function isComplete(combo: Combo): boolean {
  if (!combo.key) {
    return false;
  }
  const needsModifier =
    /^([A-Z]|[0-9]|Space|Enter|Tab|Up|Down|Left|Right|Minus|Equal|Comma|Period|Slash|Semicolon|Quote|Bracket(Left|Right)|Backslash|Backquote|Delete|Home|End|Page(Up|Down)|Insert)$/.test(
      combo.key,
    );
  return !needsModifier || hasModifier(combo);
}

export function toAccelerator(combo: Combo): string {
  const parts: string[] = [];
  if (combo.ctrl) {
    parts.push("Ctrl");
  }
  if (combo.alt) {
    parts.push("Alt");
  }
  if (combo.cmd) {
    parts.push("Cmd");
  }
  if (combo.shift) {
    parts.push("Shift");
  }
  parts.push(combo.key);
  return parts.join("+");
}

export function parseAccelerator(accelerator: string): Combo {
  const combo: Combo = { ctrl: false, alt: false, shift: false, cmd: false, key: "" };
  for (const raw of accelerator.split("+")) {
    const part = raw.trim();
    switch (part.toLowerCase()) {
      case "ctrl":
      case "control":
        combo.ctrl = true;
        break;
      case "alt":
      case "option":
        combo.alt = true;
        break;
      case "shift":
        combo.shift = true;
        break;
      case "cmd":
      case "command":
      case "super":
      case "meta":
      case "cmdorctrl":
      case "commandorcontrol":
        combo.cmd = true;
        break;
      case "":
        break;
      default:
        combo.key = normalizeKeyName(part);
    }
  }
  return combo;
}

function normalizeKeyName(part: string): string {
  const lower = part.toLowerCase();
  const aliases: Record<string, string> = {
    arrowup: "Up",
    arrowdown: "Down",
    arrowleft: "Left",
    arrowright: "Right",
    return: "Enter",
    esc: "Escape",
    plus: "Equal",
  };
  if (aliases[lower]) {
    return aliases[lower];
  }
  if (/^[a-z]$/.test(lower) || /^[0-9]$/.test(lower)) {
    return lower.toUpperCase();
  }
  if (/^f([1-9]|1[0-9]|2[0-4])$/.test(lower)) {
    return lower.toUpperCase();
  }
  const known = Object.keys(KEY_SYMBOL).find((k) => k.toLowerCase() === lower);
  return known ?? part;
}

/** Display tokens in HIG order: ⌃ ⌥ ⇧ ⌘ then the key. */
export function comboTokens(combo: Combo): string[] {
  const tokens: string[] = [];
  if (combo.ctrl) {
    tokens.push("⌃");
  }
  if (combo.alt) {
    tokens.push("⌥");
  }
  if (combo.shift) {
    tokens.push("⇧");
  }
  if (combo.cmd) {
    tokens.push("⌘");
  }
  if (combo.key) {
    tokens.push(KEY_SYMBOL[combo.key] ?? combo.key);
  }
  return tokens;
}

/** "Alt+Space" -> "⌥ Space". */
export function displayAccelerator(accelerator: string): string {
  return comboTokens(parseAccelerator(accelerator)).join(" ");
}
