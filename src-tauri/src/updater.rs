//! App updates, on top of `tauri-plugin-updater`.
//!
//! The only network call besides model download (guardrail G11): it fetches `latest.json` from
//! the endpoint in `tauri.conf.json`. Updates are checked only when the user setting allows it.
//! The artifact signature (minisign) is verified by the plugin against the public key in the
//! config before anything is installed.
//!
//! Register in `lib.rs`: `.plugin(tauri_plugin_updater::Builder::new().build())`.
//! Register in `ipc.rs`: the event `updater::UpdateAvailable` in `collect_events!`, and commands
//! that call `updater::check` and `updater::install`.
#![allow(dead_code)] // Wired by the controller / ipc; remove then.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;
use tauri_specta::Event;

/// How often the background check runs.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Wait this long after start so the check never competes with app start-up.
const START_DELAY: Duration = Duration::from_secs(10);
/// Upper bound for one check (G5). Download time is not bounded here: the user waits on it.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// A newer version found on the update endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct UpdateInfo {
    pub version: String,
    /// Release notes, if the release has any.
    pub notes: Option<String>,
    /// Publication date as an RFC 3339 string, if known.
    pub date: Option<String>,
}

/// Emitted when a background check finds a newer version.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type, Event)]
pub struct UpdateAvailable {
    pub info: UpdateInfo,
}

#[derive(Debug, thiserror::Error, Serialize, specta::Type)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum UpdateError {
    #[error("update check failed: {0}")]
    Check(String),
    #[error("update install failed: {0}")]
    Install(String),
}

async fn find_update(app: &AppHandle) -> Result<Option<tauri_plugin_updater::Update>, UpdateError> {
    let updater = app
        .updater()
        .map_err(|e| UpdateError::Check(e.to_string()))?;
    match tokio::time::timeout(CHECK_TIMEOUT, updater.check()).await {
        Ok(result) => result.map_err(|e| UpdateError::Check(e.to_string())),
        Err(_) => Err(UpdateError::Check("timed out".into())),
    }
}

/// Check once for a newer version. `None` means the app is up to date.
pub async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, UpdateError> {
    Ok(find_update(app).await?.map(|update| UpdateInfo {
        version: update.version.clone(),
        notes: update.body.clone(),
        date: update.date.map(|d| d.to_string()),
    }))
}

/// Download, verify and install the newer version, then restart the app.
/// Returns an error if there is no newer version or any step fails.
pub async fn install(app: &AppHandle) -> Result<(), UpdateError> {
    let update = find_update(app)
        .await?
        .ok_or_else(|| UpdateError::Install("no update available".into()))?;
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|e| UpdateError::Install(e.to_string()))?;
    app.restart()
}

/// Check at start and every 6 hours, but only while `enabled()` returns true. Never blocks the
/// caller; failures are ignored (the next round retries).
pub fn schedule(app: AppHandle, enabled: impl Fn() -> bool + Send + Sync + 'static) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(START_DELAY).await;
        loop {
            if enabled() {
                match check(&app).await {
                    Ok(Some(info)) => {
                        let _ = UpdateAvailable { info }.emit(&app);
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("{error}"),
                }
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}
