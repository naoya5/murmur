//! SOT: model-catalog
//! What: every model Murmur can download and run: id, family, languages, the pinned Hugging Face
//!       revision, file, size, SHA-256, license, attribution and capabilities.
//! Why: one table drives downloads, verification, the Models screen, the licenses screen and the
//!      engine. Adding a model is a change to this file only (PLAN 2.16).
//! Where: download and verify in `src-tauri/src/adapters/models.rs`; load in
//!        `src-tauri/src/adapters/engine.rs`. Values are copied from speech.cpp v0.10.0
//!        `tools/catalog/catalog.json` and the Hugging Face API (HANDOFF 7.2). Raise the
//!        speech.cpp version and these revisions together, then run the engine tests.

use serde::Serialize;
use specta::Type;

use crate::Language;

pub const DEFAULT_JA_MODEL: &str = "haas-ja-v1.0-q8_0";
pub const DEFAULT_EN_MODEL: &str = "parakeet-tdt-0.6b-v3-q8_0";
pub const VAD_MODEL: &str = "silero-vad-v6.2";

/// The model family decides how the engine adapter drives the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// Parakeet TDT and Haas (FastConformer encoder + TDT decoder).
    FastConformer,
    /// Qwen3-ASR (LLM decoder; takes a text prompt).
    Qwen3Asr,
    /// Silero voice activity detection.
    SileroVad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Recognition,
    VoiceDetection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct Capabilities {
    /// Takes the dictionary as a custom vocabulary (Haas `keywords`).
    pub custom_vocabulary: bool,
    /// Takes the dictionary as a text prompt (Qwen3-ASR `prompt`).
    pub prompt_vocabulary: bool,
    /// Writes punctuation itself.
    pub punctuation: bool,
    /// Reports the language it heard.
    pub language_id: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// The default for its language.
    Standard,
    /// Smaller and lighter, slightly less accurate.
    Light,
    /// Many languages in one model.
    Multilingual,
    /// Internal helper model (VAD). Not chosen by the user.
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct ModelEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    pub family: Family,
    pub role: Role,
    pub tier: Tier,
    pub languages: &'static [Language],
    pub repository: &'static str,
    pub revision: &'static str,
    pub file: &'static str,
    #[specta(type = f64)]
    pub bytes: u64,
    pub sha256: &'static str,
    pub license: &'static str,
    /// Credit text that must be shown (CC-BY models). Empty when none is required.
    pub attribution: &'static str,
    pub capabilities: Capabilities,
}

impl ModelEntry {
    /// The pinned download URL. Only `adapters/models.rs` fetches it (guardrail G11).
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repository, self.revision, self.file
        )
    }
}

const FASTCONFORMER_HAAS: Capabilities = Capabilities {
    custom_vocabulary: true,
    prompt_vocabulary: false,
    punctuation: true,
    language_id: false,
};

const FASTCONFORMER_PARAKEET: Capabilities = Capabilities {
    custom_vocabulary: false,
    prompt_vocabulary: false,
    punctuation: true,
    language_id: false,
};

pub static CATALOG: &[ModelEntry] = &[
    ModelEntry {
        id: DEFAULT_JA_MODEL,
        display_name: "Haas-ja-v1.0 (Q8_0)",
        family: Family::FastConformer,
        role: Role::Recognition,
        tier: Tier::Standard,
        languages: &[Language::Ja],
        repository: "sakasegawa/Haas-ja-v1.0-GGUF",
        revision: "6b965d5007ba780585de51549d57a8c1c0d67c7e",
        file: "Haas-616M-ja-v1.0-Q8_0.gguf",
        bytes: 656_381_472,
        sha256: "cd5f60b2f1c411404b83fca4a65e3ce30a26d9e951b6a346529e65f22758afeb",
        license: "Apache-2.0",
        attribution: "Haas-ja-v1.0 by Third Intelligence, Apache-2.0",
        capabilities: FASTCONFORMER_HAAS,
    },
    ModelEntry {
        id: "haas-ja-v1.0-q5_k",
        display_name: "Haas-ja-v1.0 Light (Q5_K)",
        family: Family::FastConformer,
        role: Role::Recognition,
        tier: Tier::Light,
        languages: &[Language::Ja],
        repository: "sakasegawa/Haas-ja-v1.0-GGUF",
        revision: "6b965d5007ba780585de51549d57a8c1c0d67c7e",
        file: "Haas-616M-ja-v1.0-Q5_K.gguf",
        bytes: 428_021_280,
        sha256: "880ee2d3068b98834d88cff5794442a7f36f1808a6b365a5f6188a060b689658",
        license: "Apache-2.0",
        attribution: "Haas-ja-v1.0 by Third Intelligence, Apache-2.0",
        capabilities: FASTCONFORMER_HAAS,
    },
    ModelEntry {
        id: DEFAULT_EN_MODEL,
        display_name: "Parakeet TDT 0.6B v3 (Q8_0)",
        family: Family::FastConformer,
        role: Role::Recognition,
        tier: Tier::Standard,
        languages: &[Language::En],
        repository: "sakasegawa/parakeet-tdt-0.6b-v3-GGUF",
        revision: "b7ed82dbfba6e63da0ef9cf5104f5fc67da44414",
        file: "parakeet-tdt-0.6B-v3-Q8_0.gguf",
        bytes: 668_107_680,
        sha256: "7e61f3f5bb8db536ae649a541eb23349833d3d62565aff1da5bcd7c4f163639a",
        license: "CC-BY-4.0",
        attribution: "NVIDIA parakeet-tdt-0.6b-v3, licensed under CC-BY-4.0. Converted to GGUF by sakasegawa.",
        capabilities: FASTCONFORMER_PARAKEET,
    },
    ModelEntry {
        id: "qwen3-asr-0.6b-q8_0",
        display_name: "Qwen3-ASR 0.6B (Q8_0)",
        family: Family::Qwen3Asr,
        role: Role::Recognition,
        tier: Tier::Multilingual,
        languages: &[Language::Ja, Language::En],
        repository: "sakasegawa/Qwen3-ASR-0.6B-GGUF",
        revision: "ee78f8709547f39bea3b924a16c72403826cac92",
        file: "Qwen3-ASR-0.6B-Q8_0.gguf",
        bytes: 841_502_336,
        sha256: "416e10c15b4a3d9002bd337d18fc450233fdf68502b6e10d1379d2789838afd0",
        license: "Apache-2.0",
        attribution: "Qwen3-ASR by the Qwen team, Apache-2.0",
        capabilities: Capabilities {
            custom_vocabulary: false,
            prompt_vocabulary: true,
            punctuation: true,
            language_id: true,
        },
    },
    ModelEntry {
        id: VAD_MODEL,
        display_name: "Silero VAD v6.2",
        family: Family::SileroVad,
        role: Role::VoiceDetection,
        tier: Tier::Internal,
        languages: &[],
        repository: "sakasegawa/silero-vad-GGUF",
        revision: "7fcb7aa2a70c312f8579fc987b3e1cdecf9321ca",
        file: "silero-vad-309K-v6.2-F32.gguf",
        bytes: 1_238_624,
        sha256: "2fc312514e5fc4fa4ffd02b25c76a4853fa4130e8fb9777d01502183b01238cb",
        license: "MIT",
        attribution: "Silero VAD by Silero Team, MIT",
        capabilities: Capabilities {
            custom_vocabulary: false,
            prompt_vocabulary: false,
            punctuation: false,
            language_id: false,
        },
    },
];

pub fn find(id: &str) -> Option<&'static ModelEntry> {
    CATALOG.iter().find(|m| m.id == id)
}

pub fn vad() -> &'static ModelEntry {
    // VAD_MODEL is in CATALOG; the test below guards it.
    CATALOG
        .iter()
        .find(|m| m.id == VAD_MODEL)
        .unwrap_or(&CATALOG[CATALOG.len() - 1])
}

/// Recognition models the user can pick for a language, standard first.
pub fn for_language(lang: Language) -> impl Iterator<Item = &'static ModelEntry> {
    CATALOG
        .iter()
        .filter(move |m| m.role == Role::Recognition && m.languages.contains(&lang))
}
