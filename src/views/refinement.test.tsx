import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SettingsProvider } from "../app/settings";
import { I18nContext, type Locale, strings } from "../i18n";
import {
  commands,
  type ModelInfo,
  type RefineStatus,
  type Settings as SettingsType,
  type TranscriptRow,
} from "../ipc";
import { noticeContent } from "../pill/Pill";
import { History } from "./History";
import { Models } from "./Models";
import { Settings } from "./Settings";

const navigation = vi.hoisted(() => ({ navigate: vi.fn() }));
vi.mock("../app/navigation", () => ({
  useNavigation: () => ({
    navigate: navigation.navigate,
    target: { view: "settings", anchor: "recording" },
  }),
}));
vi.mock("../ipc", () => ({
  commands: {
    getSettings: vi.fn(),
    updateSettings: vi.fn(),
    listInputDevices: vi.fn(),
    getPermissions: vi.fn(),
    appInfo: vi.fn(),
    listModels: vi.fn(),
    getRefinerStatus: vi.fn(),
    listHistory: vi.fn(),
    copyText: vi.fn(),
  },
  useEvent: () => {},
}));

function settings(patch: Partial<SettingsType> = {}): SettingsType {
  return {
    schema_version: 1,
    hotkey: "Alt+Space",
    record_mode: "toggle",
    microphone: null,
    language: "ja",
    models: { ja: "haas", en: "parakeet" },
    auto_paste: true,
    restore_clipboard: false,
    remove_fillers: true,
    add_final_punctuation: true,
    dictionary: [],
    typing_wpm_en: 40,
    typing_cpm_ja: 80,
    history_retention: { kind: "forever" },
    launch_at_login: false,
    auto_check_updates: false,
    ui_language: "en",
    sounds: false,
    unload_model_after_min: null,
    alternate_language_hotkey: null,
    onboarding_completed: true,
    annotations_enabled: false,
    annotations_hotkey: "Alt+Shift+KeyS",
    annotations_retention_days: 7,
    translate_enabled: false,
    translate_hotkey: "Alt+Shift+KeyE",
    refine_enabled: false,
    refine_lists: true,
    ...patch,
  };
}

function model(task: "clean" | "translate", installed: boolean): ModelInfo {
  const id = task === "clean" ? "qwen3.5-2b-q4_k_m" : "lfm2-350m-enjp-mt-q4_k_m";
  return {
    entry: {
      id,
      display_name: task === "clean" ? "Qwen3.5-2B (Q4_K_M)" : "LFM2-350M-ENJP-MT",
      family: "llm",
      role: "refinement",
      tier: "standard",
      languages: ["ja"],
      repository: "fixture",
      revision: "fixture",
      file: `${id}.gguf`,
      bytes: task === "clean" ? 1_280_835_840 : 229_310_240,
      sha256: "",
      license: "Apache-2.0",
      attribution: "Fixture",
      tasks: [task],
      memory_bytes: null,
      capabilities: {
        custom_vocabulary: false,
        prompt_vocabulary: false,
        punctuation: false,
        language_id: false,
      },
    },
    status: { id, installed, downloading: null, error: null },
    selected_for: [],
  };
}

function historyRow(status: RefineStatus, changed = status === "ok"): TranscriptRow {
  const original = "あの、資料を共有しました。";
  return {
    id: "cleanup-row",
    created_at_ms: Date.now(),
    status: "ok",
    text: changed ? "資料を共有しました。" : original,
    raw_text: original,
    language: "ja",
    duration_ms: 4_000,
    latin_words: 0,
    cjk_chars: 10,
    model_id: "haas",
    latency_ms: 700,
    asr_ms: 120,
    audio_path: null,
    error: null,
    refine_kind: "clean",
    refine_status: status,
    pre_refine_text: original,
    refine_model_id: status === "skipped" ? null : "qwen",
    refine_ms: status === "skipped" ? null : 580,
  };
}

function show(children: ReactNode, locale: Locale = "en") {
  return render(
    <I18nContext.Provider value={strings[locale]}>
      <SettingsProvider>{children}</SettingsProvider>
    </I18nContext.Provider>,
  );
}

beforeEach(() => {
  vi.resetAllMocks();
  HTMLElement.prototype.scrollIntoView = vi.fn();
  vi.mocked(commands.getSettings).mockResolvedValue(settings());
  vi.mocked(commands.updateSettings).mockImplementation(async (next) => ({
    status: "ok",
    data: next,
  }));
  vi.mocked(commands.listInputDevices).mockResolvedValue([]);
  vi.mocked(commands.getPermissions).mockResolvedValue({
    microphone: "Granted",
    accessibility: true,
    screen_recording: false,
  });
  vi.mocked(commands.appInfo).mockResolvedValue({
    version: "0.1.0",
    data_dir: "/fixture",
    speech_engine: "fixture",
  });
  vi.mocked(commands.listModels).mockResolvedValue([
    model("clean", false),
    model("translate", false),
  ]);
  vi.mocked(commands.getRefinerStatus).mockResolvedValue({
    clean_error: null,
    translate_error: null,
  });
  vi.mocked(commands.listHistory).mockResolvedValue({
    status: "ok",
    data: { items: [], total: 0 },
  });
  vi.mocked(commands.copyText).mockResolvedValue({ status: "ok", data: null });
});
afterEach(cleanup);

describe("cleanup settings", () => {
  it.each(["en", "ja"] as const)(
    "saves enable and lists independently and links the missing model in %s",
    async (locale) => {
      show(<Settings />, locale);
      const t = strings[locale];
      const enabled = await screen.findByRole("switch", { name: t.settings.experimental.refine });
      expect(
        screen.queryByRole("switch", { name: t.settings.experimental.refineLists }),
      ).toBeNull();
      fireEvent.click(enabled);
      const lists = await screen.findByRole("switch", {
        name: t.settings.experimental.refineLists,
      });
      expect(lists.getAttribute("aria-checked")).toBe("true");
      expect(vi.mocked(commands.updateSettings).mock.lastCall?.[0]).toMatchObject({
        refine_enabled: true,
        refine_lists: true,
        translate_enabled: false,
      });
      fireEvent.click(screen.getByRole("button", { name: t.settings.experimental.openModels }));
      expect(navigation.navigate).toHaveBeenCalledWith("models", "refine");
      fireEvent.click(lists);
      await waitFor(() => expect(lists.getAttribute("aria-checked")).toBe("false"));
      expect(vi.mocked(commands.updateSettings).mock.lastCall?.[0]).toMatchObject({
        refine_enabled: true,
        refine_lists: false,
        translate_enabled: false,
      });
      fireEvent.click(enabled);
      await waitFor(() =>
        expect(
          screen.queryByRole("switch", { name: t.settings.experimental.refineLists }),
        ).toBeNull(),
      );
    },
  );

  it("needs only the cleanup model, and reports its own startup failure", async () => {
    vi.mocked(commands.getSettings).mockResolvedValue(settings({ refine_enabled: true }));
    vi.mocked(commands.listModels).mockResolvedValue([
      model("clean", true),
      model("translate", false),
    ]);
    vi.mocked(commands.getRefinerStatus).mockResolvedValue({
      clean_error: "model_load_failed",
      translate_error: null,
    });
    show(<Settings />);
    expect(
      await screen.findByText(
        strings.en.settings.experimental.refineModelReady("Qwen3.5-2B (Q4_K_M)"),
      ),
    ).toBeTruthy();
    expect(
      screen.queryByRole("button", { name: strings.en.settings.experimental.openModels }),
    ).toBeNull();
    expect(screen.getByRole("alert").textContent).toContain("rule-formatted text");
    expect(screen.getByRole("alert").textContent).not.toContain("translated");
  });

  it("reports a model-status read failure and retries without changing settings", async () => {
    vi.mocked(commands.getSettings).mockResolvedValue(settings({ refine_enabled: true }));
    vi.mocked(commands.listModels).mockRejectedValueOnce(new Error("unavailable"));
    show(<Settings />);
    const retry = await screen.findByRole("button", { name: strings.en.common.retry });
    expect(screen.getByRole("alert").textContent).toBe(strings.en.models.loadError);
    fireEvent.click(retry);
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(
      await screen.findByRole("button", { name: strings.en.settings.experimental.openModels }),
    ).toBeTruthy();
    expect(commands.updateSettings).not.toHaveBeenCalled();
  });

  it("keeps the toggle off when saving fails and shows the local error", async () => {
    vi.mocked(commands.updateSettings).mockResolvedValue({
      status: "error",
      error: { kind: "save", message: "disk full" },
    });
    show(<Settings />);
    const enabled = await screen.findByRole("switch", {
      name: strings.en.settings.experimental.refine,
    });
    fireEvent.click(enabled);
    expect((await screen.findByRole("alert")).textContent).toContain("disk full");
    expect(enabled.getAttribute("aria-checked")).toBe("false");
  });

  it.each([false, true])(
    "does not carry a rejected cleanup enable into another save (pending=%s)",
    async (pending) => {
      let rejectSave: (() => void) | undefined;
      vi.mocked(commands.updateSettings).mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            rejectSave = () =>
              resolve({ status: "error", error: { kind: "save", message: "disk full" } });
          }),
      );
      show(<Settings />);
      fireEvent.click(
        await screen.findByRole("switch", { name: strings.en.settings.experimental.refine }),
      );
      await waitFor(() => expect(commands.updateSettings).toHaveBeenCalledTimes(1));
      if (!pending) {
        rejectSave?.();
        await screen.findByRole("alert");
      }
      fireEvent.click(
        screen.getByRole("switch", { name: strings.en.settings.experimental.translate }),
      );
      if (pending) rejectSave?.();
      await waitFor(() => expect(commands.updateSettings).toHaveBeenCalledTimes(2));
      expect(vi.mocked(commands.updateSettings).mock.lastCall?.[0]).toMatchObject({
        refine_enabled: false,
        translate_enabled: true,
      });
    },
  );

  it("preserves both accepted patches when controls are clicked before either response", async () => {
    show(<Settings />);
    const enabled = await screen.findByRole("switch", {
      name: strings.en.settings.experimental.refine,
    });
    fireEvent.click(enabled);
    fireEvent.click(
      screen.getByRole("switch", { name: strings.en.settings.experimental.translate }),
    );
    await waitFor(() => expect(commands.updateSettings).toHaveBeenCalledTimes(2));
    expect(vi.mocked(commands.updateSettings).mock.lastCall?.[0]).toMatchObject({
      refine_enabled: true,
      translate_enabled: true,
    });
  });

  it("shows downloadable refinement models with only cleanup enabled", async () => {
    vi.mocked(commands.getSettings).mockResolvedValue(settings({ refine_enabled: true }));
    show(<Models />);
    expect(await screen.findByRole("heading", { name: strings.en.models.refine })).toBeTruthy();
    expect(screen.getByText("Qwen3.5-2B (Q4_K_M)")).toBeTruthy();
  });
});

describe("cleanup history", () => {
  const statuses = ["ok", "skipped", "failed", "timeout"] as const;
  it.each(statuses.flatMap((status) => [true, false].map((changed) => ({ status, changed }))))(
    "offers the original only when changed ($status, changed=$changed)",
    async ({ status, changed }) => {
      const row = historyRow(status, changed);
      vi.mocked(commands.listHistory).mockResolvedValue({
        status: "ok",
        data: { items: [row], total: 1 },
      });
      show(<History />);
      const badge =
        status === "ok"
          ? strings.en.history.refine.clean
          : status === "skipped"
            ? strings.en.history.refineSkipped
            : strings.en.history.refineFailed.clean;
      expect(await screen.findByText(badge)).toBeTruthy();
      if (!changed) {
        expect(
          screen.queryByRole("button", { name: strings.en.history.showBefore.clean }),
        ).toBeNull();
        expect(
          screen.queryByRole("button", { name: strings.en.history.copyBefore.clean }),
        ).toBeNull();
        return;
      }
      fireEvent.click(screen.getByRole("button", { name: strings.en.history.showBefore.clean }));
      const copy = screen.getByRole("button", { name: strings.en.history.copyBefore.clean });
      fireEvent.click(copy);
      await waitFor(() => expect(commands.copyText).toHaveBeenCalledWith(row.pre_refine_text));
      expect(
        screen
          .getByRole("button", { name: strings.en.history.hideBefore.clean })
          .getAttribute("aria-expanded"),
      ).toBe("true");
    },
  );
});

describe("translation history", () => {
  it.each(["ok", "skipped", "failed", "timeout"] as const)(
    "keeps original access limited to successful translations (%s)",
    async (status) => {
      const row = { ...historyRow(status, false), refine_kind: "translate" as const };
      vi.mocked(commands.listHistory).mockResolvedValue({
        status: "ok",
        data: { items: [row], total: 1 },
      });
      show(<History />);
      await screen.findByText(row.text as string);
      const before = screen.queryByRole("button", {
        name: strings.en.history.showBefore.translate,
      });
      if (status === "ok") {
        // Translation still offers its Japanese even when it matches the delivered text.
        expect(before).toBeTruthy();
      } else {
        expect(before).toBeNull();
      }
    },
  );
});

describe("cleanup fallback notice", () => {
  it.each(["en", "ja"] as const)(
    "localizes the skip and preserves the delivery outcome in %s",
    (locale) => {
      const t = strings[locale];
      expect(
        noticeContent({ kind: "cleanup_skipped", outcome: { kind: "pasted" } }, t),
      ).toMatchObject({ text: t.pill.cleanupSkipped, sub: t.pill.pasted });
      expect(
        noticeContent(
          { kind: "cleanup_skipped", outcome: { kind: "copied", reason: "auto_paste_off" } },
          t,
        ),
      ).toMatchObject({ text: t.pill.cleanupSkipped, sub: t.pill.copiedReason.auto_paste_off });
      expect(
        noticeContent({ kind: "cleanup_skipped", outcome: { kind: "unconfirmed" } }, t).sub,
      ).toBe(t.pill.unconfirmed);
    },
  );
});
