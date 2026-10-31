// Dev-only IPC mock so every view and pill state renders in a plain browser (T5-6).
// Loaded by main.tsx / pill.tsx only when `import.meta.env.DEV` and the app is not inside Tauri.
// It must never reach the production bundle: `pnpm build` is checked for the sentinel below.
//
// Drive it from the URL or the console:
//   index.html?view=history&empty=1&onboarding=1&lang=ja&update=available|banner|error|installfail
//   pill.html?state=recording|cancel|transcribing|delivering|pasted|copied|nospeech|failed|capture
//   window.__murmurMock.pill("copied"), .cycle(), .failDownloads(true)
import { emit } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";
import type {
  AppInfo,
  Dashboard,
  DayPoint,
  DictionaryEntry,
  HistoryPage,
  InputDevice,
  Language,
  ModelEntry,
  ModelInfo,
  Permissions,
  PillState,
  PillView,
  Settings,
  TranscriptRow,
  UpdateInfo,
} from "./bindings";

export const MOCK_SENTINEL = "murmur-dev-mock-fixture-7f3a";

const params = new URLSearchParams(window.location.search);
const isPill = window.location.pathname.endsWith("pill.html");

// ---------- Fixtures ----------

let seed = 7;
function rand(): number {
  seed = (seed * 1103515245 + 12345) & 0x7fffffff;
  return seed / 0x7fffffff;
}

const dictionary: DictionaryEntry[] = params.has("empty")
  ? []
  : [
      {
        id: "d1",
        written: "Claude Code",
        readings: ["クロードコード", "cloud code"],
        scope: "both",
      },
      { id: "d2", written: "Tauri", readings: ["タウリ"], scope: "ja" },
      { id: "d3", written: "speech.cpp", readings: ["スピーチシーピーピー"], scope: "ja" },
    ];

let settings: Settings = {
  schema_version: 1,
  hotkey: "Alt+Space",
  record_mode: "toggle",
  microphone: null,
  language: params.get("lang") === "en" ? "en" : "ja",
  models: { ja: "haas-ja-v1.0-q8_0", en: "parakeet-tdt-0.6b-v3-q8_0" },
  auto_paste: true,
  restore_clipboard: false,
  remove_fillers: true,
  add_final_punctuation: true,
  dictionary,
  typing_wpm_en: 40,
  typing_cpm_ja: 80,
  history_retention: { kind: "forever" },
  launch_at_login: false,
  auto_check_updates: true,
  ui_language: params.get("lang") === "ja" ? "ja" : params.get("lang") === "en" ? "en" : "system",
  sounds: false,
  unload_model_after_min: null,
  alternate_language_hotkey: null,
  onboarding_completed: !params.has("onboarding"),
};

const JA_TEXTS = [
  "明日の打ち合わせは午後三時からに変更になりました。資料は前日までに共有します。",
  "Claude Code で新しいブランチを切って、テストが通るまで直してからプルリクエストを出してください。",
  "牛乳、卵、食パン、コーヒー豆を買って帰る。",
  "このプロジェクトのゴールは、ローカルで動く速い音声入力を作ることです。",
  "了解です。確認して折り返します。",
];
const EN_TEXTS = [
  "Can you move the retrospective to Thursday and send the notes before then?",
  "The pill should animate its width from the native window, not from CSS.",
  "Remind me to call the dentist tomorrow at nine.",
  "Latency is measured from the stop of the recording to the clipboard write.",
  "Sounds good, let's ship it.",
];

function makeHistory(): TranscriptRow[] {
  const rows: TranscriptRow[] = [];
  const now = Date.now();
  for (let i = 0; i < 37; i++) {
    const ja = rand() > 0.4;
    const text = ja ? JA_TEXTS[i % JA_TEXTS.length] : EN_TEXTS[i % EN_TEXTS.length];
    const failed = i === 2 || i === 11;
    const recovered = i === 5;
    const minutesAgo = i * 47 + rand() * 30;
    rows.push({
      id: `h${i}`,
      created_at_ms: now - minutesAgo * 60_000,
      status: failed ? "failed" : recovered ? "recovered" : "ok",
      text: failed ? null : text,
      raw_text: failed ? null : text,
      language: ja ? "ja" : "en",
      duration_ms: 4_000 + Math.round(rand() * 12_000),
      latin_words: ja ? 0 : text.split(" ").length,
      cjk_chars: ja ? text.length : 0,
      model_id: ja ? "haas-ja-v1.0-q8_0" : "parakeet-tdt-0.6b-v3-q8_0",
      latency_ms: failed ? null : 380 + Math.round(rand() * 500),
      asr_ms: failed ? null : 300 + Math.round(rand() * 400),
      audio_path: failed ? "/tmp/spool/x.pcm" : null,
      error: failed ? (i === 2 ? "timeout" : "model_missing") : null,
    });
  }
  return rows;
}

let history: TranscriptRow[] = params.has("empty") ? [] : makeHistory();

function dayPoints(days: number): DayPoint[] {
  const out: DayPoint[] = [];
  const today = new Date();
  for (let i = days - 1; i >= 0; i--) {
    const d = new Date(today.getFullYear(), today.getMonth(), today.getDate() - i);
    const weekend = d.getDay() === 0 || d.getDay() === 6;
    const active = rand() > (weekend ? 0.7 : 0.25);
    const sessions = active ? 1 + Math.floor(rand() * 9) : 0;
    const key = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(
      d.getDate(),
    ).padStart(2, "0")}`;
    out.push({
      date: key,
      sessions,
      latin_words: Math.round(sessions * 14 * rand()),
      cjk_chars: Math.round(sessions * 40 * rand()),
    });
  }
  return out;
}

const heatmap = dayPoints(140);

function dashboard(): Dashboard {
  if (params.has("empty")) {
    return {
      total_sessions: 0,
      total_latin_words: 0,
      total_cjk_chars: 0,
      total_speech_ms: 0,
      typing_ms: 0,
      saved_ms: 0,
      speaking_wpm_en: null,
      speaking_cpm_ja: null,
      median_latency_ms: null,
      streak_days: 0,
      today_sessions: 0,
      heatmap: dayPoints(140).map((d) => ({ ...d, sessions: 0, latin_words: 0, cjk_chars: 0 })),
      last_14_days: dayPoints(14).map((d) => ({ ...d, sessions: 0, latin_words: 0, cjk_chars: 0 })),
    };
  }
  const sessions = heatmap.reduce((a, d) => a + d.sessions, 0);
  const words = heatmap.reduce((a, d) => a + d.latin_words, 0);
  const chars = heatmap.reduce((a, d) => a + d.cjk_chars, 0);
  const speech = sessions * 9_200;
  const typing = (words / settings.typing_wpm_en + chars / settings.typing_cpm_ja) * 60_000;
  return {
    total_sessions: sessions,
    total_latin_words: words,
    total_cjk_chars: chars,
    total_speech_ms: speech,
    typing_ms: Math.round(typing),
    saved_ms: Math.max(0, Math.round(typing - speech)),
    speaking_wpm_en: 148,
    speaking_cpm_ja: 262,
    median_latency_ms: 612,
    streak_days: 6,
    today_sessions: heatmap[heatmap.length - 1]?.sessions ?? 0,
    heatmap,
    last_14_days: heatmap.slice(-14),
  };
}

const catalog: ModelEntry[] = [
  {
    id: "haas-ja-v1.0-q8_0",
    display_name: "Haas-ja-v1.0 (Q8_0)",
    family: "fast_conformer",
    role: "recognition",
    tier: "standard",
    languages: ["ja"],
    repository: "ThirdIntelligence/Haas-ja-v1.0-GGUF",
    revision: "main",
    file: "haas-ja-v1.0-q8_0.gguf",
    bytes: 656_381_472,
    sha256: "",
    license: "Apache-2.0",
    attribution: "Haas-ja-v1.0 by Third Intelligence, Apache-2.0",
    capabilities: {
      custom_vocabulary: true,
      prompt_vocabulary: false,
      punctuation: true,
      language_id: false,
    },
  },
  {
    id: "haas-ja-v1.0-q5_k",
    display_name: "Haas-ja-v1.0 Light (Q5_K)",
    family: "fast_conformer",
    role: "recognition",
    tier: "light",
    languages: ["ja"],
    repository: "ThirdIntelligence/Haas-ja-v1.0-GGUF",
    revision: "main",
    file: "haas-ja-v1.0-q5_k.gguf",
    bytes: 428_021_280,
    sha256: "",
    license: "Apache-2.0",
    attribution: "Haas-ja-v1.0 by Third Intelligence, Apache-2.0",
    capabilities: {
      custom_vocabulary: true,
      prompt_vocabulary: false,
      punctuation: true,
      language_id: false,
    },
  },
  {
    id: "parakeet-tdt-0.6b-v3-q8_0",
    display_name: "Parakeet TDT 0.6B v3 (Q8_0)",
    family: "fast_conformer",
    role: "recognition",
    tier: "standard",
    languages: ["en"],
    repository: "sakasegawa/parakeet-tdt-0.6b-v3-GGUF",
    revision: "main",
    file: "parakeet-tdt-0.6b-v3-q8_0.gguf",
    bytes: 668_107_680,
    sha256: "",
    license: "CC-BY-4.0",
    attribution:
      "NVIDIA parakeet-tdt-0.6b-v3, licensed under CC-BY-4.0. Converted to GGUF by sakasegawa.",
    capabilities: {
      custom_vocabulary: false,
      prompt_vocabulary: false,
      punctuation: true,
      language_id: false,
    },
  },
  {
    id: "qwen3-asr-0.6b-q8_0",
    display_name: "Qwen3-ASR 0.6B (Q8_0)",
    family: "qwen_3_asr",
    role: "recognition",
    tier: "multilingual",
    languages: ["ja", "en"],
    repository: "Qwen/Qwen3-ASR-0.6B-GGUF",
    revision: "main",
    file: "qwen3-asr-0.6b-q8_0.gguf",
    bytes: 841_502_336,
    sha256: "",
    license: "Apache-2.0",
    attribution: "Qwen3-ASR by the Qwen team, Apache-2.0",
    capabilities: {
      custom_vocabulary: false,
      prompt_vocabulary: true,
      punctuation: true,
      language_id: true,
    },
  },
  {
    id: "silero-vad-v6.2",
    display_name: "Silero VAD v6.2",
    family: "silero_vad",
    role: "voice_detection",
    tier: "internal",
    languages: [],
    repository: "snakers4/silero-vad",
    revision: "main",
    file: "silero-vad-v6.2.gguf",
    bytes: 1_238_624,
    sha256: "",
    license: "MIT",
    attribution: "Silero VAD by Silero Team, MIT",
    capabilities: {
      custom_vocabulary: false,
      prompt_vocabulary: false,
      punctuation: false,
      language_id: false,
    },
  },
];

const installed = new Set<string>(
  params.has("empty") || params.has("onboarding")
    ? []
    : ["haas-ja-v1.0-q8_0", "parakeet-tdt-0.6b-v3-q8_0", "silero-vad-v6.2"],
);
const downloads = new Map<string, { received: number; timer: number }>();
const downloadErrors = new Map<string, string>();
let failDownloads = params.has("offline");

function modelInfos(): ModelInfo[] {
  return catalog.map((entry) => {
    const d = downloads.get(entry.id);
    const selected: Language[] = [];
    if (settings.models.ja === entry.id) {
      selected.push("ja");
    }
    if (settings.models.en === entry.id) {
      selected.push("en");
    }
    return {
      entry,
      status: {
        id: entry.id,
        installed: installed.has(entry.id),
        downloading: d
          ? { received: d.received, total: entry.bytes, bytes_per_sec: 38_000_000 }
          : null,
        error: downloadErrors.get(entry.id) ?? null,
      },
      selected_for: selected,
    };
  });
}

const permissions: Permissions = {
  microphone: params.has("onboarding") ? "NotDetermined" : "Granted",
  accessibility: !params.has("onboarding"),
};

const devices: InputDevice[] = [
  { name: "MacBook Pro Microphone", is_default: true },
  { name: "AirPods Pro", is_default: false },
  { name: "Scarlett 2i2 USB", is_default: false },
];

const appInfo: AppInfo = {
  version: "0.1.0",
  data_dir: "/Users/you/Library/Application Support/io.github.naoya5.murmur",
  speech_engine: "speech.cpp (Metal)",
};

const updateInfo: UpdateInfo = {
  version: "0.2.0",
  notes:
    "Faster Japanese model loading.\nThe pill remembers its width between sessions.\nFixes for push-to-talk on external keyboards.",
  date: "2026-11-08T09:00:00Z",
};

// ---------- Pill state ----------

let pill: PillState = { view: { kind: "hidden" }, language: settings.language };
let levelTimer = 0;

function pillViewFor(name: string): PillView {
  switch (name) {
    case "recording":
      return { kind: "recording", elapsed_ms: 12_400, mode: "toggle" };
    case "cancel":
      return { kind: "cancel_pending", remaining_ms: 3_000, from_recording: true };
    case "transcribing":
      return { kind: "transcribing" };
    case "delivering":
      return { kind: "delivering" };
    case "pasted":
      return { kind: "notice", notice: { kind: "delivered", outcome: { kind: "pasted" } } };
    case "copied":
      return {
        kind: "notice",
        notice: { kind: "delivered", outcome: { kind: "copied", reason: "no_accessibility" } },
      };
    case "nospeech":
      return { kind: "notice", notice: { kind: "no_speech" } };
    case "failed":
      return { kind: "notice", notice: { kind: "failed", failure: "timeout" } };
    case "capture":
      return { kind: "notice", notice: { kind: "capture_failed", error: "permission_denied" } };
    default:
      return { kind: "hidden" };
  }
}

function setPill(view: PillView): void {
  pill = { view, language: settings.language };
  void emit("pill-state", pill);
  window.clearInterval(levelTimer);
  if (view.kind === "recording") {
    let phase = 0;
    levelTimer = window.setInterval(() => {
      phase += 0.21;
      const level = Math.max(0, Math.min(1, 0.35 + 0.3 * Math.sin(phase) + (rand() - 0.5) * 0.5));
      void emit("audio-level", { level });
    }, 33);
  }
}

const CYCLE = [
  "recording",
  "cancel",
  "recording",
  "transcribing",
  "delivering",
  "pasted",
  "hidden",
];
function cycle(): void {
  let i = 0;
  const step = () => {
    const name = CYCLE[i % CYCLE.length];
    setPill(pillViewFor(name));
    i += 1;
    const wait =
      name === "recording" ? 3000 : name === "cancel" ? 3200 : name === "pasted" ? 1200 : 1200;
    window.setTimeout(step, wait);
  };
  step();
}

// ---------- Mutations that the real backend would broadcast ----------

function saveSettings(next: Settings): void {
  settings = next;
  void emit("settings-changed", { settings });
}

function historyChanged(): void {
  void emit("history-changed", null);
}

function startDownload(id: string): void {
  const entry = catalog.find((m) => m.id === id);
  if (!entry || installed.has(id) || downloads.has(id)) {
    return;
  }
  downloadErrors.delete(id);
  const state = { received: 0, timer: 0 };
  downloads.set(id, state);
  const stepBytes = entry.bytes / 120;
  state.timer = window.setInterval(() => {
    state.received = Math.min(entry.bytes, state.received + stepBytes);
    void emit("model-progress", {
      id,
      progress: { received: state.received, total: entry.bytes, bytes_per_sec: 38_000_000 },
    });
    if (failDownloads && state.received > entry.bytes * 0.3) {
      window.clearInterval(state.timer);
      downloads.delete(id);
      downloadErrors.set(id, "network: the connection was lost");
      void emit("models-changed", null);
      return;
    }
    if (state.received >= entry.bytes) {
      window.clearInterval(state.timer);
      downloads.delete(id);
      installed.add(id);
      void emit("models-changed", null);
    }
  }, 100);
}

/** Simulate one dictation: the pill cycles, then text is pasted into the focused field. */
function simulateDictation(): void {
  const text = settings.language === "ja" ? JA_TEXTS[1] : EN_TEXTS[1];
  window.setTimeout(() => {
    const el = document.activeElement;
    if (el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement) {
      const setter = Object.getOwnPropertyDescriptor(
        el instanceof HTMLTextAreaElement
          ? HTMLTextAreaElement.prototype
          : HTMLInputElement.prototype,
        "value",
      )?.set;
      setter?.call(el, el.value + text);
      el.dispatchEvent(new Event("input", { bubbles: true }));
    }
    history = [
      {
        id: `h${Date.now()}`,
        created_at_ms: Date.now(),
        status: "ok",
        text,
        raw_text: text,
        language: settings.language,
        duration_ms: 5_200,
        latin_words: settings.language === "en" ? text.split(" ").length : 0,
        cjk_chars: settings.language === "ja" ? text.length : 0,
        model_id: settings.models[settings.language],
        latency_ms: 540,
        asr_ms: 410,
        audio_path: null,
        error: null,
      },
      ...history,
    ];
    historyChanged();
  }, 2200);
}

// ---------- The IPC table ----------

function filteredHistory(query: string): TranscriptRow[] {
  const q = query.trim().toLowerCase();
  return q ? history.filter((r) => (r.text ?? "").toLowerCase().includes(q)) : history;
}

mockIPC(
  async (cmd, rawArgs) => {
    const args = (rawArgs ?? {}) as Record<string, unknown>;
    await new Promise((r) => window.setTimeout(r, 30));
    switch (cmd) {
      case "app_info":
        return appInfo;
      case "get_settings":
        return settings;
      case "update_settings": {
        const next = args.settings as Settings;
        if (next.hotkey === "Cmd+Space") {
          throw { kind: "hotkey", message: "Cmd+Space is taken by Spotlight" };
        }
        saveSettings(next);
        return settings;
      }
      case "list_input_devices":
        return devices;
      case "get_dashboard":
        return dashboard();
      case "list_history": {
        const rows = filteredHistory(String(args.query ?? ""));
        const offset = Number(args.offset ?? 0);
        const limit = Number(args.limit ?? 20);
        const page: HistoryPage = { items: rows.slice(offset, offset + limit), total: rows.length };
        return page;
      }
      case "delete_history_item":
        history = history.filter((r) => r.id !== args.id);
        historyChanged();
        return null;
      case "clear_history":
        history = [];
        historyChanged();
        return null;
      case "retry_history_item": {
        const row = history.find((r) => r.id === args.id);
        if (!row) {
          throw { kind: "not_found" };
        }
        if (row.error === "model_missing") {
          throw { kind: "model_missing" };
        }
        await new Promise((r) => window.setTimeout(r, 900));
        const text = row.language === "ja" ? JA_TEXTS[3] : EN_TEXTS[3];
        Object.assign(row, {
          status: "recovered",
          text,
          raw_text: text,
          error: null,
          latency_ms: 700,
          asr_ms: 650,
          audio_path: null,
        });
        historyChanged();
        return row;
      }
      case "copy_text":
        return null;
      case "list_models":
        return modelInfos();
      case "download_model":
        if (params.has("diskfull")) {
          throw { kind: "model", message: "disk full: 120 MB free, 656 MB needed" };
        }
        startDownload(String(args.id));
        return null;
      case "cancel_download": {
        const d = downloads.get(String(args.id));
        if (d) {
          window.clearInterval(d.timer);
          downloads.delete(String(args.id));
          void emit("models-changed", null);
        }
        return null;
      }
      case "delete_model":
        installed.delete(String(args.id));
        void emit("models-changed", null);
        return null;
      case "get_permissions":
        // A fresh object, as the real IPC deserializes one each call.
        return { ...permissions };
      case "request_microphone":
        permissions.microphone = "Granted";
        return permissions.microphone;
      case "request_accessibility":
        window.setTimeout(() => {
          permissions.accessibility = true;
        }, 1500);
        return null;
      case "open_privacy_pane":
      case "open_url":
        return null;
      case "suspend_hotkeys":
      case "resume_hotkeys":
      case "reset_stats":
        return null;
      case "toggle_recording":
        setPill(pillViewFor("recording"));
        window.setTimeout(() => setPill(pillViewFor("transcribing")), 1800);
        window.setTimeout(() => setPill(pillViewFor("pasted")), 2200);
        window.setTimeout(() => setPill(pillViewFor("hidden")), 3200);
        simulateDictation();
        return null;
      case "check_for_updates": {
        await new Promise((r) => window.setTimeout(r, 900));
        const mode = params.get("update") ?? "none";
        if (mode === "error") {
          throw { kind: "check", message: "dns: github.com could not be resolved" };
        }
        return mode === "none" ? null : updateInfo;
      }
      case "install_update":
        await new Promise((r) => window.setTimeout(r, 2500));
        if (params.get("update") === "installfail") {
          throw { kind: "install", message: "signature mismatch" };
        }
        return null;
      case "pill_clicked":
        setPill(pillViewFor(pill.view.kind === "recording" ? "transcribing" : "hidden"));
        return null;
      case "set_pill_width": {
        const root = document.getElementById("root");
        if (root) {
          root.style.width = `${Number(args.width)}px`;
        }
        return null;
      }
      case "get_pill_state":
        return pill;
      default:
        throw { kind: "store", message: `mock: unknown command ${cmd}` };
    }
  },
  { shouldMockEvents: true },
);

// ---------- Page dressing and console handles ----------

document.documentElement.classList.add("mock", isPill ? "mock-pill" : "mock-main");

if (!isPill && params.get("update") === "banner") {
  window.setTimeout(() => void emit("update-available", { info: updateInfo }), 1200);
}

if (!isPill && params.has("view")) {
  window.setTimeout(() => void emit("navigate", { view: params.get("view") ?? "home" }), 150);
}

if (isPill) {
  const state = params.get("state");
  window.setTimeout(() => {
    if (params.has("cycle")) {
      cycle();
    } else if (state) {
      setPill(pillViewFor(state));
    }
  }, 50);
}

declare global {
  interface Window {
    __murmurMock?: {
      pill: (name: string) => void;
      cycle: () => void;
      failDownloads: (on: boolean) => void;
      navigate: (view: string) => void;
    };
  }
}

window.__murmurMock = {
  pill: (name) => setPill(pillViewFor(name)),
  cycle,
  failDownloads: (on) => {
    failDownloads = on;
  },
  navigate: (view) => void emit("navigate", { view }),
};
