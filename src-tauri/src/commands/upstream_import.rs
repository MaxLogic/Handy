use crate::fork_identity::UPSTREAM_IDENTIFIER;
use crate::settings::SETTINGS_STORE_PATH;
use crate::upstream_import::{
    self, ImportDecision, UpstreamImportChoice, UpstreamImportOffer, UpstreamImportReport,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_store::StoreExt;

static IMPORT_RUNNING: AtomicBool = AtomicBool::new(false);
static IMPORT_CANCEL: AtomicBool = AtomicBool::new(false);

/// Holds a "run in progress" flag for as long as it lives, so a second
/// import (or a stale-temp cleanup) cannot overlap a running one, and the
/// flag is released even if the run panics.
struct RunGuard(&'static AtomicBool);

impl RunGuard {
    fn acquire(flag: &'static AtomicBool) -> Option<Self> {
        (!flag.swap(true, Ordering::SeqCst)).then_some(RunGuard(flag))
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Upstream Handy's non-portable data directory (`%APPDATA%\com.pais.handy`).
fn upstream_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .data_dir()
        .map(|dir| dir.join(UPSTREAM_IDENTIFIER))
        .map_err(|e| e.to_string())
}

fn destination_dir(app: &AppHandle) -> Result<PathBuf, String> {
    crate::portable::app_data_dir(app).map_err(|e| e.to_string())
}

/// Runs off the main thread: the scan stats every file under upstream `models/`.
#[tauri::command]
#[specta::specta]
pub async fn get_upstream_import_offer(app: AppHandle) -> Option<UpstreamImportOffer> {
    tauri::async_runtime::spawn_blocking(move || {
        let source = upstream_dir(&app).ok()?;
        let destination = destination_dir(&app).ok()?;
        // Leftovers of an interrupted import never look like valid data.
        if let Some(_guard) = RunGuard::acquire(&IMPORT_RUNNING) {
            upstream_import::remove_stale_temps(&destination);
        }
        let onboarded = crate::settings::get_settings(&app).onboarding_completed;
        upstream_import::offer(&source, &destination, onboarded)
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
#[specta::specta]
pub fn decline_upstream_import(app: AppHandle) -> Result<(), String> {
    let source = upstream_dir(&app)?;
    let destination = destination_dir(&app)?;
    upstream_import::record_decision(&destination, &source, ImportDecision::Declined)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn cancel_upstream_import() {
    IMPORT_CANCEL.store(true, Ordering::SeqCst);
}

/// Copy the chosen upstream data. Progress is emitted as
/// `upstream-import-progress`. Only a run that finished without failures
/// records the `imported` decision; after a cancel or a failure the offer
/// returns on the next start. The app must restart afterwards to load imported
/// settings, models and history.
#[tauri::command]
#[specta::specta]
pub async fn run_upstream_import(
    app: AppHandle,
    choice: UpstreamImportChoice,
) -> Result<UpstreamImportReport, String> {
    let Some(guard) = RunGuard::acquire(&IMPORT_RUNNING) else {
        return Err("already_running".into());
    };
    IMPORT_CANCEL.store(false, Ordering::SeqCst);
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        let source = upstream_dir(&app)?;
        let destination = destination_dir(&app)?;
        let store = app
            .store(crate::portable::store_path(SETTINGS_STORE_PATH))
            .map_err(|e| e.to_string())?;
        let mut write_settings = |settings: &crate::settings::AppSettings| {
            // custom_words.txt is the source of truth for custom words, so
            // imported words go into the file (keeping its comments);
            // otherwise the next start would restore the file's old list.
            let mut settings = settings.clone();
            let file = crate::custom_words::file(&app)?;
            let current = crate::settings::get_settings(&app).custom_words;
            settings.custom_words = file.replace(&current, &settings.custom_words)?;
            let value = serde_json::to_value(&settings).map_err(|e| e.to_string())?;
            store.set("settings", value);
            store.save().map_err(|e| e.to_string())
        };
        let mut emit = |progress| {
            let _ = app.emit("upstream-import-progress", progress);
        };
        let report = upstream_import::run_and_record(
            &source,
            &destination,
            choice,
            &mut write_settings,
            &mut emit,
            &IMPORT_CANCEL,
        )?;
        log::info!(
            "Upstream import finished: settings={}, models copied={} skipped={}, history={}, bytes={}, failures={}, skipped links={}, cancelled={}",
            report.settings_imported,
            report.models_copied,
            report.models_skipped,
            report.history_imported,
            report.bytes_copied,
            report.failures.len(),
            report.skipped_links.len(),
            report.cancelled
        );
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|result| result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_guard_rejects_a_second_run_and_releases_on_drop() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let first = RunGuard::acquire(&FLAG).expect("first run starts");
        assert!(RunGuard::acquire(&FLAG).is_none(), "second run is refused");
        drop(first);
        assert!(RunGuard::acquire(&FLAG).is_some(), "flag released on drop");
    }

    #[test]
    fn run_guard_is_released_when_the_run_panics() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let result = std::panic::catch_unwind(|| {
            let _guard = RunGuard::acquire(&FLAG).unwrap();
            panic!("copy failed");
        });
        assert!(result.is_err());
        assert!(!FLAG.load(Ordering::SeqCst));
    }
}
