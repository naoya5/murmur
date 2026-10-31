//! SOT: ipc
//!
//! What: every Tauri command and event, registered in one specta builder. The builder also
//! generates `src/ipc/bindings.ts`, the only TypeScript view of these types.
//! Why: the Rust types are the truth (guardrail G4). The UI never writes an IPC type by hand,
//! and CI regenerates the file and fails on any diff.
//! Where: add a command here and in `COMMANDS`, then in `build.rs` and the window capability
//! under `capabilities/`. Run `cargo test -p murmur export_bindings` to refresh the bindings.

use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, Manager, Wry};
use tauri_specta::{Builder, collect_commands, collect_events};

/// Command names in the order of `collect_commands!`. `build.rs` and the capabilities must
/// list the same names; a test below checks `build.rs` and the capability files.
#[cfg(test)]
pub const COMMANDS: &[&str] = &["app_info"];

/// Static facts about the running app. Proves the Rust to UI round trip.
#[derive(Debug, Clone, Serialize, Type)]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
}

/// The error type every command returns. It crosses the IPC boundary as a tagged object.
#[derive(Debug, thiserror::Error, Serialize, Type)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum IpcError {
    #[error("path unavailable: {0}")]
    Path(String),
}

#[tauri::command]
#[specta::specta]
pub fn app_info(app: AppHandle) -> Result<AppInfo, IpcError> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| IpcError::Path(error.to_string()))?;
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    })
}

/// The one specta builder: all commands and events.
pub fn builder() -> Builder<Wry> {
    Builder::<Wry>::new()
        .commands(collect_commands![app_info])
        .events(collect_events![])
}

/// Where the generated bindings live, relative to `src-tauri/`.
const BINDINGS_PATH: &str = "../src/ipc/bindings.ts";

/// Write `src/ipc/bindings.ts`. Output is deterministic: no timestamps, stable ordering.
pub fn export_bindings(builder: &Builder<Wry>) -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(BINDINGS_PATH);
    // The generated file has helpers we do not use; keep strict unused checks off for it only.
    let typescript = specta_typescript::Typescript::default().header("// @ts-nocheck");
    builder.export(typescript, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI runs this test and then `git diff --exit-code src/ipc/bindings.ts`.
    #[test]
    fn export_bindings_writes_file() {
        export_bindings(&builder()).unwrap();
    }

    #[test]
    fn commands_are_in_build_manifest_and_capabilities() {
        let root = env!("CARGO_MANIFEST_DIR");
        let build = std::fs::read_to_string(format!("{root}/build.rs")).unwrap();
        let main_cap = std::fs::read_to_string(format!("{root}/capabilities/main.json")).unwrap();
        for command in COMMANDS {
            assert!(
                build.contains(&format!("\"{command}\"")),
                "{command} missing in build.rs"
            );
            let permission = format!("allow-{}", command.replace('_', "-"));
            assert!(
                main_cap.contains(&permission),
                "{permission} missing in main.json"
            );
        }
    }
}
