//! Murmur core: the pure domain logic of the app.
//!
//! Guardrail G1: this crate never touches Tauri, files, the network, OS APIs or the speech SDK.
//! The compiler enforces it because the crate does not depend on them, and CI checks
//! `cargo tree -p murmur-core` for `tauri`.
//!
//! Reading order: `session` (the state machine) -> `pipeline` (how audio becomes text)
//! -> `text` (formatting) -> `metrics` -> `settings` -> `catalog` -> `ports`.
//! Run `rg "SOT:"` to list every source of truth in the repository.

pub mod catalog;
pub mod metrics;
pub mod pipeline;
pub mod ports;
pub mod session;
pub mod settings;
pub mod text;

use serde::{Deserialize, Serialize};
use specta::Type;

/// A spoken language that Murmur transcribes. Fixed for the length of one recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Ja,
    En,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::Ja, Language::En];

    /// BCP 47 code, as the speech engine and the database use it.
    pub fn code(self) -> &'static str {
        match self {
            Language::Ja => "ja",
            Language::En => "en",
        }
    }

    pub fn from_code(code: &str) -> Option<Language> {
        match code {
            "ja" => Some(Language::Ja),
            "en" => Some(Language::En),
            _ => None,
        }
    }
}

/// Sample rate of every PCM buffer that crosses a port. Adapters resample to this rate.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Milliseconds on a monotonic clock. The core never reads a clock; callers pass `now`.
pub type Millis = u64;
