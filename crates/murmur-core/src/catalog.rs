//! SOT: model-catalog
//! What: every model Murmur can download and run: id, family, languages, the pinned Hugging Face
//!       revision, file, size, SHA-256, license, attribution and capabilities.
//! Why: one table drives downloads, verification, the Models screen, the licenses screen and the
//!      engine. Adding a model is a change to this file only.
//! Where: download and verify in `src-tauri/src/adapters/models.rs`; load in
//!        `src-tauri/src/adapters/engine.rs` (speech) and `src-tauri/src/adapters/refiner.rs`
//!        (the LLMs of the refine step, run by the murmur-llm sidecar). Speech values are copied
//!        from speech.cpp v0.10.0 `tools/catalog/catalog.json` and the Hugging Face API (HANDOFF
//!        7.2). Raise the speech.cpp version and these revisions together, then run the engine
//!        tests. The LLM rows are evaluated before selection; replace the cleanup row and its
//!        default id together, then rerun the original and held-out evaluation sets.

use serde::Serialize;
use specta::Type;

use crate::Language;
use crate::refine::RefineKind;

pub const DEFAULT_JA_MODEL: &str = "haas-ja-v1.0-q8_0";
pub const DEFAULT_EN_MODEL: &str = "parakeet-tdt-0.6b-v3-q8_0";
pub const VAD_MODEL: &str = "silero-vad-v6.2";
/// The translation model (A3: a translation-only model never answered in stage 0).
pub const DEFAULT_TRANSLATE_MODEL: &str = "lfm2-350m-enjp-mt-q4_k_m";
/// The cleanup model, used before a translation when the text has a marker (E1, E3).
pub const DEFAULT_CLEAN_MODEL: &str = "qwen3.5-2b-q4_k_m";

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
    /// A chat LLM in GGUF, run by the murmur-llm sidecar (llama.cpp), never in the app process.
    Llm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Recognition,
    VoiceDetection,
    /// The refine step after the text rules (`tasks` says which).
    Refinement,
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
    /// What a `Role::Refinement` model does. Empty for every other role.
    pub tasks: &'static [RefineKind],
    /// About how much memory the model takes while it runs (measured resident size of its
    /// sidecar, the mapped file included). Shown for the refine models;
    /// `None` where it was not measured.
    #[specta(type = Option<f64>)]
    pub memory_bytes: Option<u64>,
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

/// For models that do not recognize speech.
const NO_SPEECH_CAPABILITIES: Capabilities = Capabilities {
    custom_vocabulary: false,
    prompt_vocabulary: false,
    punctuation: false,
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
        tasks: &[],
        memory_bytes: None,
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
        tasks: &[],
        memory_bytes: None,
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
        tasks: &[],
        memory_bytes: None,
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
        tasks: &[],
        memory_bytes: None,
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
        capabilities: NO_SPEECH_CAPABILITIES,
        tasks: &[],
        memory_bytes: None,
    },
    ModelEntry {
        id: DEFAULT_TRANSLATE_MODEL,
        display_name: "LFM2-350M-ENJP-MT (Q4_K_M)",
        family: Family::Llm,
        role: Role::Refinement,
        tier: Tier::Standard,
        languages: &[Language::Ja, Language::En],
        repository: "LiquidAI/LFM2-350M-ENJP-MT-GGUF",
        revision: "889c88c3d3681b4f342ae210a1d2a1369aba0e10",
        file: "LFM2-350M-ENJP-MT-Q4_K_M.gguf",
        bytes: 229_310_240,
        sha256: "574ef7980dd20d69b494bce82565db5f25124d404be02052bbca62277d33077b",
        license: "LFM Open License v1.0",
        attribution: "LFM2-350M-ENJP-MT by Liquid AI, LFM Open License v1.0",
        capabilities: NO_SPEECH_CAPABILITIES,
        tasks: &[RefineKind::Translate],
        // Measured peak resident memory: 352 to 355 MiB.
        memory_bytes: Some(355 * 1024 * 1024),
    },
    ModelEntry {
        id: DEFAULT_CLEAN_MODEL,
        display_name: "Qwen3.5-2B (Q4_K_M)",
        family: Family::Llm,
        role: Role::Refinement,
        tier: Tier::Standard,
        languages: &[Language::Ja],
        repository: "unsloth/Qwen3.5-2B-GGUF",
        revision: "f6d5376be1edb4d416d56da11e5397a961aca8ae",
        file: "Qwen3.5-2B-Q4_K_M.gguf",
        bytes: 1_280_835_840,
        sha256: "aaf42c8b7c3cab2bf3d69c355048d4a0ee9973d48f16c731c0520ee914699223",
        license: "Apache-2.0",
        attribution: "Qwen3.5-2B by the Qwen team, Apache-2.0. GGUF by Unsloth.",
        capabilities: NO_SPEECH_CAPABILITIES,
        tasks: &[RefineKind::Clean],
        // M3b: 1395 MiB after a warm request; rounded up for the Models screen.
        memory_bytes: Some(1400 * 1024 * 1024),
    },
];

pub fn find(id: &str) -> Option<&'static ModelEntry> {
    CATALOG.iter().find(|m| m.id == id)
}

pub fn vad() -> &'static ModelEntry {
    // VAD_MODEL is in CATALOG; the test below guards it.
    find(VAD_MODEL).unwrap_or(&CATALOG[0])
}

/// The model that runs one refine step. Both ids are in CATALOG; the tests below guard it.
pub fn refine_model(step: RefineKind) -> &'static ModelEntry {
    let id = match step {
        RefineKind::Clean => DEFAULT_CLEAN_MODEL,
        RefineKind::Translate => DEFAULT_TRANSLATE_MODEL,
    };
    find(id).unwrap_or(&CATALOG[0])
}

/// The history's `refine_model_id`: the model of each call in order, joined with "+" (a
/// translation that was cleaned first names both models). Ids only, never text.
pub fn refine_model_ids(steps: &[RefineKind]) -> String {
    steps
        .iter()
        .map(|step| refine_model(*step).id)
        .collect::<Vec<_>>()
        .join("+")
}

/// The refine models (the Models screen's "cleanup and translation" section).
pub fn refinement_models() -> impl Iterator<Item = &'static ModelEntry> {
    CATALOG.iter().filter(|m| m.role == Role::Refinement)
}

/// Recognition models the user can pick for a language, standard first.
pub fn for_language(lang: Language) -> impl Iterator<Item = &'static ModelEntry> {
    CATALOG
        .iter()
        .filter(move |m| m.role == Role::Recognition && m.languages.contains(&lang))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_lower_hex(s: &str, len: usize) -> bool {
        s.len() == len
            && s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    }

    #[test]
    fn ids_are_unique_and_well_formed() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate id");
        for m in CATALOG {
            assert!(!m.id.is_empty());
            assert!(
                m.id.chars().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || matches!(c, '-' | '.' | '_')),
                "{} is not a file-safe id",
                m.id
            );
            assert!(!m.display_name.is_empty(), "{}", m.id);
            assert!(!m.file.is_empty(), "{}", m.id);
            assert!(m.bytes > 0, "{}", m.id);
            assert!(!m.license.is_empty(), "{}", m.id);
        }
    }

    #[test]
    fn defaults_exist_with_the_right_role_and_language() {
        let ja = find(DEFAULT_JA_MODEL).expect("default ja model");
        assert_eq!(ja.role, Role::Recognition);
        assert_eq!(ja.tier, Tier::Standard);
        assert!(ja.languages.contains(&Language::Ja));
        assert!(ja.capabilities.custom_vocabulary, "Haas takes keywords");

        let en = find(DEFAULT_EN_MODEL).expect("default en model");
        assert_eq!(en.role, Role::Recognition);
        assert_eq!(en.tier, Tier::Standard);
        assert!(en.languages.contains(&Language::En));
        assert!(
            !en.capabilities.custom_vocabulary,
            "Parakeet needs text replacement"
        );

        let vad_entry = find(VAD_MODEL).expect("vad model");
        assert_eq!(vad_entry.role, Role::VoiceDetection);
        assert_eq!(vad_entry.family, Family::SileroVad);
        assert_eq!(vad_entry.tier, Tier::Internal);
        assert!(vad_entry.languages.is_empty());
        assert_eq!(vad(), vad_entry);
    }

    #[test]
    fn exactly_one_standard_model_per_language() {
        for lang in Language::ALL {
            let standard: Vec<&str> = for_language(lang)
                .filter(|m| m.tier == Tier::Standard)
                .map(|m| m.id)
                .collect();
            assert_eq!(standard.len(), 1, "{lang:?}: {standard:?}");
        }
    }

    #[test]
    fn for_language_lists_recognition_models_standard_first() {
        let ja: Vec<&str> = for_language(Language::Ja).map(|m| m.id).collect();
        assert_eq!(ja[0], DEFAULT_JA_MODEL);
        assert!(ja.contains(&"qwen3-asr-0.6b-q8_0"));
        assert!(!ja.contains(&VAD_MODEL));
        assert!(!ja.contains(&DEFAULT_EN_MODEL));

        let en: Vec<&str> = for_language(Language::En).map(|m| m.id).collect();
        assert_eq!(en[0], DEFAULT_EN_MODEL);
        assert!(en.contains(&"qwen3-asr-0.6b-q8_0"));
        assert!(!en.contains(&DEFAULT_JA_MODEL));
    }

    #[test]
    fn recognition_models_have_a_language_and_vad_has_none() {
        for m in CATALOG {
            match m.role {
                Role::Recognition => assert!(!m.languages.is_empty(), "{}", m.id),
                Role::VoiceDetection => assert!(m.languages.is_empty(), "{}", m.id),
                Role::Refinement => assert!(m.languages.contains(&Language::Ja), "{}", m.id),
            }
        }
    }

    #[test]
    fn only_refinement_models_have_tasks_and_they_are_llms() {
        for m in CATALOG {
            assert_eq!(m.role == Role::Refinement, !m.tasks.is_empty(), "{}", m.id);
            // The Models screen shows a memory estimate for each refine model.
            assert_eq!(
                m.role == Role::Refinement,
                m.memory_bytes.is_some_and(|b| b > m.bytes),
                "{}",
                m.id
            );
            assert_eq!(
                m.role == Role::Refinement,
                m.family == Family::Llm,
                "{}",
                m.id
            );
        }
    }

    #[test]
    fn each_refine_step_has_its_model() {
        for step in [RefineKind::Clean, RefineKind::Translate] {
            let m = refine_model(step);
            assert_eq!(m.role, Role::Refinement);
            assert!(m.tasks.contains(&step), "{}", m.id);
        }
        assert_eq!(
            refine_model(RefineKind::Translate).id,
            DEFAULT_TRANSLATE_MODEL
        );
        assert_eq!(refine_model(RefineKind::Clean).id, DEFAULT_CLEAN_MODEL);
        assert_eq!(
            refine_model_ids(&[RefineKind::Clean, RefineKind::Translate]),
            format!("{DEFAULT_CLEAN_MODEL}+{DEFAULT_TRANSLATE_MODEL}")
        );
        assert_eq!(
            refine_model_ids(&[RefineKind::Translate]),
            DEFAULT_TRANSLATE_MODEL
        );
        assert_eq!(refine_model_ids(&[]), "");
        let ids: Vec<&str> = refinement_models().map(|m| m.id).collect();
        assert_eq!(ids, vec![DEFAULT_TRANSLATE_MODEL, DEFAULT_CLEAN_MODEL]);
        // The user accepted the larger cleanup model; keep exactly one model per task.
        let total: u64 = refinement_models().map(|m| m.bytes).sum();
        assert_eq!(total, 1_510_146_080);
        let clean = refine_model(RefineKind::Clean);
        assert_eq!(clean.file, "Qwen3.5-2B-Q4_K_M.gguf");
        assert_eq!(clean.languages, &[Language::Ja]);
        assert_eq!(clean.tasks, &[RefineKind::Clean]);
        assert!(find("qwen3.5-0.8b-q4_k_m").is_none());
        // Neither is a speech model for any language.
        for lang in Language::ALL {
            assert!(for_language(lang).all(|m| m.role == Role::Recognition));
        }
    }

    #[test]
    fn hashes_and_revisions_are_pinned() {
        for m in CATALOG {
            assert!(
                is_lower_hex(m.sha256, 64),
                "{}: sha256 {:?}",
                m.id,
                m.sha256
            );
            assert!(
                is_lower_hex(m.revision, 40),
                "{}: revision {:?}",
                m.id,
                m.revision
            );
        }
    }

    #[test]
    fn urls_point_at_the_pinned_revision() {
        for m in CATALOG {
            let url = m.url();
            assert_eq!(
                url,
                format!(
                    "https://huggingface.co/{}/resolve/{}/{}",
                    m.repository, m.revision, m.file
                )
            );
            assert!(url.starts_with("https://huggingface.co/"));
            assert!(url.contains(&format!("/resolve/{}/", m.revision)));
            assert!(!url.contains("/main/"), "{}: not pinned", m.id);
            assert!(!url.contains(' '), "{}: space in url", m.id);
            assert_eq!(m.repository.split('/').count(), 2, "{}: owner/name", m.id);
        }
    }

    #[test]
    fn attribution_is_present_where_the_license_needs_it() {
        for m in CATALOG {
            if m.license.starts_with("CC-BY") {
                assert!(!m.attribution.is_empty(), "{} needs attribution", m.id);
            }
        }
    }

    #[test]
    fn find_unknown_is_none() {
        assert!(find("").is_none());
        assert!(
            find("haas-ja-v1.0-Q8_0").is_none(),
            "ids are case-sensitive"
        );
    }
}
