//! One-time import of upstream Handy data into the Handy MaxLogic fork.
//!
//! The fork runs under its own identifier, so on first run it starts empty even
//! when upstream Handy (`com.pais.handy`) already has settings and multi-GB
//! models. This module scans the upstream data directory and, only after the
//! user chooses to, copies the selected parts into the fork's data directory
//! (portable `Data/` or `%APPDATA%\eu.maxlogic.handy`).
//!
//! Upstream data is only ever read. Files are copied to a `.importing` temp
//! name and renamed when complete, so a cancelled or failed copy never leaves a
//! truncated model that looks valid. `history.db` is copied through SQLite's
//! backup API so a concurrent upstream write cannot tear it. The user's
//! decision is recorded in a marker file so the offer is made once; a run with
//! failures records nothing, so the offer returns on the next start.

use log::warn;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::settings::{AppSettings, SETTINGS_STORE_PATH};

/// Decision marker in the fork's data directory. Its presence suppresses the offer.
pub const MARKER_FILE: &str = "upstream_import.json";
const CUSTOM_SOUNDS: [&str; 2] = ["custom_start.wav", "custom_stop.wav"];
const MODELS_DIR: &str = "models";
const HISTORY_DB: &str = "history.db";
/// SQLite side files of a database; stale ones must not meet a replaced file.
const SQLITE_SIDECARS: [&str; 3] = ["-journal", "-wal", "-shm"];
const RECORDINGS_DIR: &str = "recordings";
const TEMP_SUFFIX: &str = ".importing";
const PARTIAL_SUFFIX: &str = ".partial";
const COPY_CHUNK: usize = 4 * 1024 * 1024;
/// Pages per SQLite backup step; the cancel flag is checked between steps.
const BACKUP_PAGES: i32 = 256;
/// How long a locked upstream `history.db` is waited for before the item fails.
const DATABASE_BUSY_WAIT: Duration = Duration::from_secs(2);
const PROGRESS_STEP_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportDecision {
    Imported,
    Declined,
}

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    decision: ImportDecision,
    decided_at_unix: u64,
    source: String,
}

/// What upstream data exists and what importing it would replace.
#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct UpstreamImportOffer {
    pub source_dir: String,
    pub destination_dir: String,
    pub settings_available: bool,
    /// The fork already completed onboarding; importing settings replaces them.
    pub replaces_settings: bool,
    /// Model entries not yet present in the destination.
    pub model_count: u32,
    pub model_bytes: u64,
    pub history_available: bool,
    pub history_bytes: u64,
    /// The fork already has history entries; importing history replaces them.
    pub replaces_history: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Type)]
pub struct UpstreamImportChoice {
    pub settings: bool,
    pub models: bool,
    pub history: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Type)]
pub struct UpstreamImportFailure {
    pub item: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Type)]
pub struct UpstreamImportReport {
    pub settings_imported: bool,
    pub models_copied: u32,
    pub models_skipped: u32,
    pub history_imported: bool,
    pub bytes_copied: u64,
    pub cancelled: bool,
    pub failures: Vec<UpstreamImportFailure>,
    pub skipped_links: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportPhase {
    Models,
    History,
    Settings,
}

/// Emitted as `upstream-import-progress` while copying.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct UpstreamImportProgress {
    pub phase: ImportPhase,
    pub copied_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
struct SourceEntry {
    /// Path relative to the data directory, with `/` separators, for reports.
    label: String,
    path: PathBuf,
    bytes: u64,
}

/// Snapshot of importable upstream data.
#[derive(Debug, Clone)]
pub struct UpstreamScan {
    settings: Option<serde_json::Value>,
    /// The settings store exists but could not be read or parsed (for example
    /// while upstream Handy is writing it).
    settings_error: Option<String>,
    sounds: Vec<SourceEntry>,
    models: Vec<SourceEntry>,
    /// Model entries that are links to directories: never followed or copied.
    linked_models: Vec<String>,
    history: Vec<SourceEntry>,
    recordings: Vec<SourceEntry>,
}

impl UpstreamScan {
    fn has_settings(&self) -> bool {
        self.settings.is_some() || self.settings_error.is_some()
    }

    fn is_empty(&self) -> bool {
        !self.has_settings() && self.models.is_empty() && self.history.is_empty()
    }
}

/// Read what the upstream directory offers. Missing pieces are simply absent;
/// an unreadable or malformed settings store is kept as an error, so importing
/// settings reports it instead of skipping it.
pub fn scan(source: &Path) -> UpstreamScan {
    let (settings, settings_error) = match read_settings_value(&source.join(SETTINGS_STORE_PATH)) {
        Ok(settings) => (settings, None),
        Err(error) => {
            warn!("Upstream settings store unusable: {error}");
            (None, Some(error))
        }
    };
    let sounds = CUSTOM_SOUNDS
        .iter()
        .filter_map(|name| file_entry(source, name))
        .collect();
    let (linked, models): (Vec<SourceEntry>, Vec<SourceEntry>) = dir_entries(source, MODELS_DIR)
        .into_iter()
        .filter(|entry| !entry.label.ends_with(PARTIAL_SUFFIX))
        .filter(|entry| !entry.label.ends_with(TEMP_SUFFIX))
        .partition(|entry| is_linked_dir(&entry.path));
    let history: Vec<SourceEntry> = file_entry(source, HISTORY_DB).into_iter().collect();
    let recordings = if history.is_empty() {
        Vec::new()
    } else {
        dir_entries(source, RECORDINGS_DIR)
            .into_iter()
            .filter(|entry| entry.path.is_file())
            .collect()
    };
    UpstreamScan {
        settings,
        settings_error,
        sounds,
        models,
        linked_models: linked.into_iter().map(|entry| entry.label).collect(),
        history,
        recordings,
    }
}

fn read_settings_value(path: &Path) -> Result<Option<serde_json::Value>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not be read: {e}")),
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(mut store) => Ok(store.get_mut("settings").map(serde_json::Value::take)),
        Err(e) => Err(format!("is not valid JSON (it may be in use): {e}")),
    }
}

fn file_entry(root: &Path, name: &str) -> Option<SourceEntry> {
    let path = root.join(name);
    let metadata = fs::metadata(&path).ok()?;
    metadata.is_file().then(|| SourceEntry {
        label: name.to_string(),
        path,
        bytes: metadata.len(),
    })
}

fn dir_entries(root: &Path, dir: &str) -> Vec<SourceEntry> {
    let Ok(read_dir) = fs::read_dir(root.join(dir)) else {
        return Vec::new();
    };
    let mut entries: Vec<SourceEntry> = read_dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let path = entry.path();
            Some(SourceEntry {
                label: format!("{dir}/{name}"),
                bytes: tree_size(&path),
                path,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    entries
}

/// Whether `path` is a symlink or junction to a directory. Such links are
/// never followed, counted or copied (a link loop cannot recurse forever);
/// the import reports them as skipped.
fn is_linked_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|link| link.file_type().is_symlink())
        && fs::metadata(path).is_ok_and(|target| target.is_dir())
}

fn tree_size(path: &Path) -> u64 {
    if is_linked_dir(path) {
        return 0;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| tree_size(&entry.path()))
                .sum()
        })
        .unwrap_or(0)
}

fn destination_entry(destination: &Path, entry: &SourceEntry) -> PathBuf {
    destination.join(&entry.label)
}

fn same_directory(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Build the offer, or `None` when it must not be shown: a decision was
/// already recorded, there is no upstream data, or source and destination
/// are the same directory.
pub fn offer(
    source: &Path,
    destination: &Path,
    destination_onboarded: bool,
) -> Option<UpstreamImportOffer> {
    if read_decision(destination).is_some() || !source.is_dir() {
        return None;
    }
    if same_directory(source, destination) {
        return None;
    }
    let scan = scan(source);
    if scan.is_empty() {
        return None;
    }
    let pending_models: Vec<&SourceEntry> = scan
        .models
        .iter()
        .filter(|entry| !destination_entry(destination, entry).exists())
        .collect();
    Some(UpstreamImportOffer {
        source_dir: source.display().to_string(),
        destination_dir: destination.display().to_string(),
        settings_available: scan.has_settings(),
        replaces_settings: scan.has_settings() && destination_onboarded,
        model_count: pending_models.len() as u32,
        model_bytes: pending_models.iter().map(|entry| entry.bytes).sum(),
        history_available: !scan.history.is_empty(),
        history_bytes: scan
            .history
            .iter()
            .chain(&scan.recordings)
            .map(|entry| entry.bytes)
            .sum(),
        replaces_history: !scan.history.is_empty()
            && history_has_entries(&destination.join(HISTORY_DB)),
    })
}

/// Whether a history database exists and holds at least one entry.
pub fn history_has_entries(db: &Path) -> bool {
    if !db.is_file() {
        return false;
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    rusqlite::Connection::open_with_flags(db, flags)
        .and_then(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM transcription_history)",
                [],
                |row| row.get::<_, bool>(0),
            )
        })
        .unwrap_or(false)
}

pub fn read_decision(destination: &Path) -> Option<ImportDecision> {
    let text = fs::read_to_string(destination.join(MARKER_FILE)).ok()?;
    match serde_json::from_str::<Marker>(&text) {
        Ok(marker) => Some(marker.decision),
        // A damaged marker still means the user was asked.
        Err(_) => Some(ImportDecision::Declined),
    }
}

pub fn record_decision(
    destination: &Path,
    source: &Path,
    decision: ImportDecision,
) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    let marker = Marker {
        decision,
        decided_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        source: source.display().to_string(),
    };
    let json = serde_json::to_string_pretty(&marker).map_err(io::Error::other)?;
    write_atomically(&destination.join(MARKER_FILE), json.as_bytes())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = temp_path(path);
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn temp_path(path: &Path) -> PathBuf {
    suffixed(path, TEMP_SUFFIX)
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn is_import_temp(name: &str) -> bool {
    name.ends_with(TEMP_SUFFIX)
        || SQLITE_SIDECARS
            .iter()
            .any(|sidecar| name.ends_with(&format!("{TEMP_SUFFIX}{sidecar}")))
}

/// Remove `.importing` leftovers of an interrupted import (a crash or forced
/// quit) from the fork's data directory. Call only while no import runs.
pub fn remove_stale_temps(destination: &Path) {
    for dir in [
        destination.to_path_buf(),
        destination.join(MODELS_DIR),
        destination.join(RECORDINGS_DIR),
    ] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_name().to_str().is_some_and(is_import_temp) {
                continue;
            }
            let path = entry.path();
            match remove_any(&path) {
                Ok(()) => log::info!("Removed stale import temp {}", path.display()),
                Err(e) => warn!("Could not remove stale import temp {}: {e}", path.display()),
            }
        }
    }
}

struct Copier<'a> {
    progress: &'a mut dyn FnMut(UpstreamImportProgress),
    cancel: &'a AtomicBool,
    phase: ImportPhase,
    copied: u64,
    total: u64,
    buffer: Vec<u8>,
    /// Labels of directory links met inside copied entries.
    skipped_links: Vec<String>,
}

#[derive(Debug)]
enum CopyError {
    Cancelled,
    Io(io::Error),
}

impl From<io::Error> for CopyError {
    fn from(error: io::Error) -> Self {
        CopyError::Io(error)
    }
}

impl Copier<'_> {
    fn report(&mut self) {
        (self.progress)(UpstreamImportProgress {
            phase: self.phase,
            copied_bytes: self.copied,
            total_bytes: self.total,
        });
    }

    /// Copy a file or directory tree to `target` through a temp name; the
    /// target appears only when the whole entry was copied.
    fn copy_entry(&mut self, entry: &SourceEntry, target: &Path) -> Result<(), CopyError> {
        let temp = temp_path(target);
        remove_any(&temp)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let result = self
            .copy_tree(&entry.path, &temp, &entry.label)
            .and_then(|()| fs::rename(&temp, target).map_err(CopyError::Io));
        if result.is_err() {
            let _ = remove_any(&temp);
        }
        result
    }

    fn copy_tree(&mut self, source: &Path, target: &Path, label: &str) -> Result<(), CopyError> {
        if fs::metadata(source)?.is_dir() {
            fs::create_dir_all(target)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry.file_name();
                let label = format!("{label}/{}", name.to_string_lossy());
                if is_linked_dir(&path) {
                    warn!("Upstream import skips linked directory {}", path.display());
                    self.skipped_links.push(label);
                    continue;
                }
                self.copy_tree(&path, &target.join(name), &label)?;
            }
            return Ok(());
        }
        let mut reader = fs::File::open(source)?;
        let mut writer = fs::File::create(target)?;
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(CopyError::Cancelled);
            }
            let read = reader.read(&mut self.buffer)?;
            if read == 0 {
                break;
            }
            writer.write_all(&self.buffer[..read])?;
            self.copied += read as u64;
            self.report();
        }
        writer.sync_all()?;
        Ok(())
    }

    /// Copy a SQLite database through the backup API into a temp file, then
    /// replace `target`.
    fn copy_database(&mut self, entry: &SourceEntry, target: &Path) -> Result<(), CopyError> {
        let temp = temp_path(target);
        remove_database_files(&temp)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let result = backup_database(&entry.path, &temp, self.cancel).and_then(|()| {
            // Stale side files of the replaced database would be applied to
            // the new file, so they go first.
            for sidecar in SQLITE_SIDECARS {
                remove_any(&suffixed(target, sidecar))?;
            }
            fs::rename(&temp, target).map_err(CopyError::Io)
        });
        if result.is_err() {
            let _ = remove_database_files(&temp);
            return result;
        }
        self.copied += entry.bytes;
        self.report();
        Ok(())
    }
}

fn remove_database_files(path: &Path) -> io::Result<()> {
    remove_any(path)?;
    for sidecar in SQLITE_SIDECARS {
        remove_any(&suffixed(path, sidecar))?;
    }
    Ok(())
}

fn sqlite_error(error: rusqlite::Error) -> CopyError {
    CopyError::Io(io::Error::other(error.to_string()))
}

fn busy_error() -> CopyError {
    CopyError::Io(io::Error::other(
        "the database is in use by another program; close the original Handy and try again",
    ))
}

/// A readable message for errors reading the upstream database. A read-only
/// connection reports a hot journal or an unrecovered WAL (upstream Handy
/// stopped in, or is in, the middle of a write) as `ReadOnly`.
fn source_error(error: rusqlite::Error) -> CopyError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            busy_error()
        }
        Some(rusqlite::ErrorCode::ReadOnly) => CopyError::Io(io::Error::other(
            "it has unfinished changes from the original Handy; close the original Handy and try again",
        )),
        _ => sqlite_error(error),
    }
}

/// Snapshot `source` into a new database file at `target`. Upstream is opened
/// read-only, and a database newer than this build can migrate is refused:
/// startup would fail on it.
fn backup_database(source: &Path, target: &Path, cancel: &AtomicBool) -> Result<(), CopyError> {
    use rusqlite::backup::{Backup, StepResult};
    use rusqlite::{Connection, OpenFlags};

    let from = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(sqlite_error)?;
    from.busy_timeout(DATABASE_BUSY_WAIT)
        .map_err(sqlite_error)?;
    let version: i64 = from
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(source_error)?;
    let supported = crate::managers::history::supported_schema_version();
    if version > supported as i64 {
        return Err(CopyError::Io(io::Error::other(format!(
            "it comes from a newer Handy (history version {version}, this build supports up to {supported}), so it was not copied"
        ))));
    }

    let mut to = Connection::open(target).map_err(sqlite_error)?;
    {
        let backup = Backup::new(&from, &mut to).map_err(sqlite_error)?;
        let mut busy_since: Option<Instant> = None;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(CopyError::Cancelled);
            }
            match backup.step(BACKUP_PAGES).map_err(source_error)? {
                StepResult::Done => break,
                StepResult::More => busy_since = None,
                // Busy, Locked (the enum is non-exhaustive).
                _ => {
                    let since = *busy_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= DATABASE_BUSY_WAIT {
                        return Err(busy_error());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    // A standalone file in the fork's rollback journal mode, even when
    // upstream used WAL.
    to.pragma_update(None, "journal_mode", "DELETE")
        .map_err(sqlite_error)?;
    to.close().map_err(|(_, e)| sqlite_error(e))
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copy the chosen parts of `scan` into `destination`. Models go first (large
/// and cancellable), then history, then settings, so a cancelled run never
/// switches settings to models that were not copied. Per-item failures are
/// collected and the run continues with the next item.
pub fn run_import(
    scan: &UpstreamScan,
    choice: UpstreamImportChoice,
    destination: &Path,
    write_settings: &mut dyn FnMut(&AppSettings) -> Result<(), String>,
    progress: &mut dyn FnMut(UpstreamImportProgress),
    cancel: &AtomicBool,
) -> UpstreamImportReport {
    let mut report = UpstreamImportReport::default();
    let models: Vec<&SourceEntry> = if choice.models {
        scan.models.iter().collect()
    } else {
        Vec::new()
    };
    let (pending_models, existing_models): (Vec<&SourceEntry>, Vec<&SourceEntry>) = models
        .into_iter()
        .partition(|entry| !destination_entry(destination, entry).exists());
    report.models_skipped = existing_models.len() as u32;

    let history: Vec<&SourceEntry> = if choice.history {
        scan.history
            .iter()
            .chain(scan.recordings.iter().filter(|entry| {
                // Keep recordings the fork already has.
                !destination_entry(destination, entry).exists()
            }))
            .collect()
    } else {
        Vec::new()
    };
    let sounds: Vec<&SourceEntry> = if choice.settings && scan.settings.is_some() {
        scan.sounds.iter().collect()
    } else {
        Vec::new()
    };
    let total = pending_models
        .iter()
        .chain(&history)
        .chain(&sounds)
        .map(|entry| entry.bytes)
        .sum();

    let mut copier = Copier {
        progress,
        cancel,
        phase: ImportPhase::Models,
        copied: 0,
        total,
        buffer: vec![0; COPY_CHUNK],
        skipped_links: Vec::new(),
    };
    copier.report();

    let fail = |report: &mut UpstreamImportReport, item: &str, error: String| {
        warn!("Upstream import of {item} failed: {error}");
        report.failures.push(UpstreamImportFailure {
            item: item.to_string(),
            error,
        });
    };

    if choice.models {
        report
            .skipped_links
            .extend(scan.linked_models.iter().cloned());
    }
    for entry in pending_models {
        match copier.copy_entry(entry, &destination_entry(destination, entry)) {
            Ok(()) => report.models_copied += 1,
            Err(CopyError::Cancelled) => {
                report.cancelled = true;
                break;
            }
            Err(CopyError::Io(e)) => fail(&mut report, &entry.label, e.to_string()),
        }
    }

    if !report.cancelled && !history.is_empty() {
        copier.phase = ImportPhase::History;
        copier.report();
        let mut database_ok = true;
        for entry in history {
            let is_database = entry.label == HISTORY_DB;
            if !is_database && !database_ok {
                break;
            }
            let target = destination_entry(destination, entry);
            let copied = if is_database {
                copier.copy_database(entry, &target)
            } else {
                copier.copy_entry(entry, &target)
            };
            match copied {
                Ok(()) => {}
                Err(CopyError::Cancelled) => {
                    report.cancelled = true;
                    break;
                }
                Err(CopyError::Io(e)) => {
                    database_ok &= !is_database;
                    fail(&mut report, &entry.label, e.to_string());
                }
            }
        }
        report.history_imported = !report.cancelled && database_ok;
    }

    if let (false, true, Some(error)) = (
        report.cancelled,
        choice.settings,
        scan.settings_error.as_ref(),
    ) {
        fail(&mut report, SETTINGS_STORE_PATH, error.clone());
    }
    if !report.cancelled {
        if let (true, Some(value)) = (choice.settings, scan.settings.as_ref()) {
            copier.phase = ImportPhase::Settings;
            copier.report();
            // Same load path as a stored upstream value on startup: salvage,
            // run every migration (including the schema 5 profile migration),
            // merge new default bindings and normalize profiles.
            let (settings, _) = crate::settings::load_settings_value(value);
            match write_settings(&settings) {
                Ok(()) => report.settings_imported = true,
                Err(e) => fail(&mut report, SETTINGS_STORE_PATH, e),
            }
            if report.settings_imported {
                for entry in sounds {
                    let target = destination_entry(destination, entry);
                    match copier.copy_entry(entry, &target) {
                        Ok(()) => {}
                        Err(CopyError::Cancelled) => {
                            report.cancelled = true;
                            break;
                        }
                        Err(CopyError::Io(e)) => fail(&mut report, &entry.label, e.to_string()),
                    }
                }
            }
        }
    }

    report.bytes_copied = copier.copied;
    report.skipped_links.append(&mut copier.skipped_links);
    report
}

/// Limits `upstream-import-progress` events to phase changes, the end of the
/// copy and roughly every 16 MiB, so a multi-GB import does not flood the UI.
#[derive(Debug, Default)]
pub struct ProgressThrottle {
    last: Option<(ImportPhase, u64)>,
}

impl ProgressThrottle {
    pub fn should_emit(&mut self, progress: &UpstreamImportProgress) -> bool {
        let emit = self.last.is_none_or(|(phase, bytes)| {
            phase != progress.phase
                || progress.copied_bytes == progress.total_bytes
                || progress.copied_bytes.saturating_sub(bytes) >= PROGRESS_STEP_BYTES
        });
        if emit {
            self.last = Some((progress.phase, progress.copied_bytes));
        }
        emit
    }
}

/// A whole import as the command runs it: scan, copy with throttled progress,
/// then record the decision. The `imported` marker is written only when the
/// run finished without failures. After a cancel or any failure the offer
/// returns on the next start, so for example a locked `history.db` can be
/// retried once the original Handy is closed.
pub fn run_and_record(
    source: &Path,
    destination: &Path,
    choice: UpstreamImportChoice,
    write_settings: &mut dyn FnMut(&AppSettings) -> Result<(), String>,
    emit: &mut dyn FnMut(UpstreamImportProgress),
    cancel: &AtomicBool,
) -> Result<UpstreamImportReport, String> {
    let scan = scan(source);
    let mut throttle = ProgressThrottle::default();
    let mut progress = |progress: UpstreamImportProgress| {
        if throttle.should_emit(&progress) {
            emit(progress);
        }
    };
    let report = run_import(
        &scan,
        choice,
        destination,
        write_settings,
        &mut progress,
        cancel,
    );
    if !report.cancelled && report.failures.is_empty() {
        record_decision(destination, source, ImportDecision::Imported)
            .map_err(|e| e.to_string())?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;

    const SOURCE_SETTINGS: &str = r#"{
        "settings": {
            "bindings": {},
            "push_to_talk": true,
            "selected_model": "turbo",
            "selected_language": "pl",
            "settings_schema_version": 3,
            "post_process_enabled": true
        }
    }"#;

    struct Fixture {
        _root: tempfile::TempDir,
        source: PathBuf,
        destination: PathBuf,
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A history database at schema `version` with `rows` entries.
    fn history_db(path: &Path, version: usize, rows: usize) -> rusqlite::Connection {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE transcription_history (id INTEGER PRIMARY KEY, title TEXT);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", version as i64)
            .unwrap();
        for i in 0..rows {
            conn.execute(
                "INSERT INTO transcription_history (title) VALUES (?1)",
                [format!("entry {i}")],
            )
            .unwrap();
        }
        conn
    }

    fn history_rows(path: &Path) -> i64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM transcription_history", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn file_len(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    fn supported() -> usize {
        crate::managers::history::supported_schema_version()
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("com.pais.handy");
        let destination = root.path().join("Data");
        write(
            &source.join(SETTINGS_STORE_PATH),
            SOURCE_SETTINGS.as_bytes(),
        );
        write(&source.join("custom_start.wav"), b"start-cue");
        write(&source.join("models/ggml-large-v3-turbo.bin"), &[7u8; 9000]);
        write(&source.join("models/parakeet-v3/encoder.onnx"), &[1u8; 300]);
        write(&source.join("models/parakeet-v3/vocab.txt"), b"tokens");
        write(&source.join("models/ggml-small.bin.partial"), b"half");
        write(&source.join("models/.DS_Store"), b"x");
        drop(history_db(&source.join("history.db"), supported(), 3));
        write(&source.join("recordings/handy-1.wav"), b"wav-1");
        write(&source.join("handy.log"), b"log");
        fs::create_dir_all(&destination).unwrap();
        Fixture {
            _root: root,
            source,
            destination,
        }
    }

    /// Relative path -> (bytes, modified) for every file and directory.
    fn snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, Option<SystemTime>)> {
        fn walk(
            root: &Path,
            dir: &Path,
            out: &mut BTreeMap<String, (Vec<u8>, Option<SystemTime>)>,
        ) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let key = path.strip_prefix(root).unwrap().display().to_string();
                let metadata = fs::metadata(&path).unwrap();
                if metadata.is_dir() {
                    out.insert(key, (Vec::new(), None));
                    walk(root, &path, out);
                } else {
                    out.insert(key, (fs::read(&path).unwrap(), metadata.modified().ok()));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn all() -> UpstreamImportChoice {
        UpstreamImportChoice {
            settings: true,
            models: true,
            history: true,
        }
    }

    fn import(
        fixture: &Fixture,
        choice: UpstreamImportChoice,
        written: &mut Option<AppSettings>,
    ) -> UpstreamImportReport {
        let scan = scan(&fixture.source);
        let cancel = AtomicBool::new(false);
        run_import(
            &scan,
            choice,
            &fixture.destination,
            &mut |settings| {
                *written = Some(settings.clone());
                Ok(())
            },
            &mut |_| {},
            &cancel,
        )
    }

    #[test]
    fn offer_lists_settings_models_and_history_without_partials() {
        let f = fixture();
        let offer = offer(&f.source, &f.destination, false).expect("offer");
        assert!(offer.settings_available);
        assert!(!offer.replaces_settings);
        assert_eq!(offer.model_count, 2, "partial and hidden entries excluded");
        assert_eq!(offer.model_bytes, 9000 + 300 + 6);
        assert!(offer.history_available);
        assert_eq!(
            offer.history_bytes,
            file_len(&f.source.join("history.db")) + 5
        );
        assert!(!offer.replaces_history);
    }

    #[test]
    fn offer_warns_when_fork_settings_would_be_replaced() {
        let f = fixture();
        assert!(
            offer(&f.source, &f.destination, true)
                .unwrap()
                .replaces_settings
        );
    }

    #[test]
    fn no_offer_without_upstream_data_or_after_a_decision() {
        let f = fixture();
        assert!(offer(&f.destination.join("missing"), &f.destination, false).is_none());

        let empty = f.destination.join("empty-upstream");
        fs::create_dir_all(empty.join("models")).unwrap();
        assert!(offer(&empty, &f.destination, false).is_none());

        assert!(offer(&f.source, &f.source, false).is_none());

        record_decision(&f.destination, &f.source, ImportDecision::Declined).unwrap();
        assert_eq!(
            read_decision(&f.destination),
            Some(ImportDecision::Declined)
        );
        assert!(offer(&f.source, &f.destination, false).is_none());
    }

    #[test]
    fn damaged_marker_still_suppresses_the_offer() {
        let f = fixture();
        write(&f.destination.join(MARKER_FILE), b"{not json");
        assert!(offer(&f.source, &f.destination, false).is_none());
    }

    #[test]
    fn import_copies_selected_data_and_leaves_upstream_untouched() {
        let f = fixture();
        let before = snapshot(&f.source);
        let mut written = None;

        let report = import(&f, all(), &mut written);

        assert_eq!(snapshot(&f.source), before, "upstream data must not change");
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!report.cancelled);
        assert_eq!(report.models_copied, 2);
        assert!(report.settings_imported && report.history_imported);
        let db_bytes = file_len(&f.source.join("history.db"));
        assert_eq!(report.bytes_copied, 9000 + 300 + 6 + db_bytes + 5 + 9);
        let d = &f.destination;
        assert_eq!(
            fs::read(d.join("models/ggml-large-v3-turbo.bin"))
                .unwrap()
                .len(),
            9000
        );
        assert_eq!(
            fs::read(d.join("models/parakeet-v3/vocab.txt")).unwrap(),
            b"tokens"
        );
        assert!(!d.join("models/ggml-small.bin.partial").exists());
        assert!(!d.join("models/.DS_Store").exists());
        assert_eq!(history_rows(&d.join("history.db")), 3);
        assert_eq!(
            fs::read(d.join("recordings/handy-1.wav")).unwrap(),
            b"wav-1"
        );
        assert_eq!(fs::read(d.join("custom_start.wav")).unwrap(), b"start-cue");
        assert!(!d.join("handy.log").exists(), "logs are not imported");
        assert!(written.is_some());
    }

    #[test]
    fn imported_settings_run_the_current_migrations() {
        let f = fixture();
        let mut written = None;
        import(&f, all(), &mut written);
        let settings = written.expect("settings written");

        assert_eq!(settings.selected_model, "turbo");
        assert_eq!(settings.selected_language, "pl");
        assert!(settings.onboarding_completed, "derived from selected model");
        let current = crate::settings::get_default_settings().settings_schema_version;
        assert_eq!(settings.settings_schema_version, current);
        assert!(
            settings
                .processing_profiles
                .iter()
                .any(|profile| profile.id == "builtin_cleanup"),
            "profile schema migration adds built-in profiles"
        );
        assert!(settings.bindings.contains_key("transcribe"));
    }

    #[test]
    fn history_is_optional_and_settings_can_be_skipped() {
        let f = fixture();
        let mut written = None;
        let report = import(
            &f,
            UpstreamImportChoice {
                settings: false,
                models: true,
                history: false,
            },
            &mut written,
        );
        assert!(written.is_none());
        assert!(!report.settings_imported && !report.history_imported);
        assert!(!f.destination.join("history.db").exists());
        assert!(!f.destination.join("recordings").exists());
        assert!(!f.destination.join("custom_start.wav").exists());
        assert_eq!(report.models_copied, 2);
    }

    #[test]
    fn existing_fork_models_and_recordings_are_kept() {
        let f = fixture();
        write(
            &f.destination.join("models/ggml-large-v3-turbo.bin"),
            b"fork-own",
        );
        write(&f.destination.join("recordings/handy-1.wav"), b"fork-wav");

        assert_eq!(
            offer(&f.source, &f.destination, false).unwrap().model_count,
            1
        );
        let report = import(&f, all(), &mut None);

        assert_eq!((report.models_copied, report.models_skipped), (1, 1));
        let d = &f.destination;
        assert_eq!(
            fs::read(d.join("models/ggml-large-v3-turbo.bin")).unwrap(),
            b"fork-own"
        );
        assert_eq!(
            fs::read(d.join("recordings/handy-1.wav")).unwrap(),
            b"fork-wav"
        );
    }

    #[test]
    fn failures_are_reported_and_later_items_still_import() {
        let f = fixture();
        // A file where the models directory should be makes every model fail.
        write(&f.destination.join("models"), b"not a directory");
        let mut written = None;

        let report = import(&f, all(), &mut written);

        assert_eq!(report.models_copied, 0);
        let failed: Vec<&str> = report.failures.iter().map(|f| f.item.as_str()).collect();
        assert_eq!(
            failed,
            ["models/ggml-large-v3-turbo.bin", "models/parakeet-v3"]
        );
        assert!(report.history_imported && report.settings_imported);
    }

    #[test]
    fn settings_write_failure_is_reported_and_cues_are_not_copied() {
        let f = fixture();
        let scan = scan(&f.source);
        let report = run_import(
            &scan,
            all(),
            &f.destination,
            &mut |_| Err("store locked".into()),
            &mut |_| {},
            &AtomicBool::new(false),
        );
        assert!(!report.settings_imported);
        assert_eq!(
            report.failures,
            [UpstreamImportFailure {
                item: SETTINGS_STORE_PATH.into(),
                error: "store locked".into()
            }]
        );
        assert!(!f.destination.join("custom_start.wav").exists());
    }

    #[test]
    fn cancel_removes_the_partial_copy_and_skips_settings() {
        let f = fixture();
        let scan = scan(&f.source);
        let cancel = AtomicBool::new(false);
        let reports = AtomicUsize::new(0);
        let mut written = false;

        let report = run_import(
            &scan,
            all(),
            &f.destination,
            &mut |_| {
                written = true;
                Ok(())
            },
            &mut |progress| {
                // Cancel after the first chunk of the first model.
                if progress.copied_bytes > 0 && reports.fetch_add(1, Ordering::SeqCst) == 0 {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        );

        assert!(report.cancelled);
        assert!(!written && !report.settings_imported && !report.history_imported);
        let models = f.destination.join("models");
        let left: Vec<_> = fs::read_dir(&models)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(left.is_empty(), "no partial or temp files remain: {left:?}");
    }

    #[test]
    fn progress_reports_phases_and_reaches_the_total() {
        let f = fixture();
        let scan = scan(&f.source);
        let mut events = Vec::new();
        run_import(
            &scan,
            all(),
            &f.destination,
            &mut |_| Ok(()),
            &mut |progress| events.push(progress),
            &AtomicBool::new(false),
        );
        let last = events.last().unwrap();
        assert_eq!(last.copied_bytes, last.total_bytes);
        let phases: Vec<ImportPhase> = events.iter().map(|e| e.phase).collect();
        let first = |phase| phases.iter().position(|p| *p == phase).unwrap();
        assert!(first(ImportPhase::Models) < first(ImportPhase::History));
        assert!(first(ImportPhase::History) < first(ImportPhase::Settings));
    }

    #[test]
    fn history_entries_are_detected_read_only() {
        let f = fixture();
        let db = f.destination.join("history.db");
        assert!(!history_has_entries(&db));
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE transcription_history (id INTEGER PRIMARY KEY);")
            .unwrap();
        assert!(!history_has_entries(&db));
        conn.execute("INSERT INTO transcription_history (id) VALUES (1)", [])
            .unwrap();
        drop(conn);
        assert!(history_has_entries(&db));
        assert!(
            offer(&f.source, &f.destination, false)
                .unwrap()
                .replaces_history
        );
    }

    #[test]
    fn decision_marker_round_trips() {
        let f = fixture();
        record_decision(&f.destination, &f.source, ImportDecision::Imported).unwrap();
        assert_eq!(
            read_decision(&f.destination),
            Some(ImportDecision::Imported)
        );
        assert!(!temp_path(&f.destination.join(MARKER_FILE)).exists());
    }

    fn failed_items(report: &UpstreamImportReport) -> Vec<&str> {
        report.failures.iter().map(|f| f.item.as_str()).collect()
    }

    #[test]
    fn too_new_history_is_refused_and_the_fork_history_is_kept() {
        let f = fixture();
        fs::remove_file(f.source.join("history.db")).unwrap();
        drop(history_db(&f.source.join("history.db"), supported() + 1, 1));
        drop(history_db(
            &f.destination.join("history.db"),
            supported(),
            2,
        ));

        let report = import(&f, all(), &mut None);

        assert!(!report.history_imported);
        assert_eq!(failed_items(&report), ["history.db"]);
        assert!(
            report.failures[0].error.contains("newer Handy"),
            "{:?}",
            report.failures
        );
        assert_eq!(history_rows(&f.destination.join("history.db")), 2);
        assert!(!f.destination.join("history.db.importing").exists());
        assert!(
            !f.destination.join("recordings").exists(),
            "recordings follow the database"
        );
    }

    #[test]
    fn wal_history_is_copied_consistently_while_upstream_has_it_open() {
        let f = fixture();
        let db = f.source.join("history.db");
        fs::remove_file(&db).unwrap();
        let writer = history_db(&db, supported(), 0);
        writer
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
            .unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        for i in 0..50 {
            writer
                .execute(
                    "INSERT INTO transcription_history (title) VALUES (?1)",
                    [format!("wal entry {i}")],
                )
                .unwrap();
        }
        assert!(
            file_len(&f.source.join("history.db-wal")) > 0,
            "rows are only in the WAL, so a raw copy of history.db would miss them"
        );
        // A fork database with a stale WAL that must not meet the new file.
        write(&f.destination.join("history.db-wal"), b"stale wal");

        let report = import(&f, all(), &mut None);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(report.history_imported);
        let copied = f.destination.join("history.db");
        assert!(!f.destination.join("history.db-wal").exists());
        assert_eq!(history_rows(&copied), 50);
        let mode: String = rusqlite::Connection::open(&copied)
            .unwrap()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        drop(writer);
    }

    #[test]
    fn locked_history_is_a_reported_failure() {
        let f = fixture();
        let lock = rusqlite::Connection::open(f.source.join("history.db")).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE;").unwrap();

        let report = import(&f, all(), &mut None);

        assert_eq!(failed_items(&report), ["history.db"]);
        assert!(
            report.failures[0].error.contains("in use"),
            "{:?}",
            report.failures
        );
        assert!(!report.history_imported);
        assert!(!f.destination.join("history.db").exists());
        assert!(!f.destination.join("history.db.importing").exists());
        assert!(report.settings_imported, "later items still import");
        lock.execute_batch("ROLLBACK;").unwrap();
    }

    #[test]
    fn unreadable_settings_are_a_reported_failure() {
        let f = fixture();
        // Upstream was in the middle of writing its store.
        write(
            &f.source.join(SETTINGS_STORE_PATH),
            b"{\"settings\": {\"bind",
        );
        let offer = offer(&f.source, &f.destination, false).expect("offer");
        assert!(offer.settings_available);

        let mut written = None;
        let report = import(&f, all(), &mut written);

        assert!(written.is_none() && !report.settings_imported);
        assert_eq!(failed_items(&report), [SETTINGS_STORE_PATH]);
        assert!(!f.destination.join("custom_start.wav").exists());
        assert_eq!(report.models_copied, 2, "other items still import");
    }

    fn record(f: &Fixture, cancel: &AtomicBool) -> UpstreamImportReport {
        run_and_record(
            &f.source,
            &f.destination,
            all(),
            &mut |_| Ok(()),
            &mut |_| {},
            cancel,
        )
        .unwrap()
    }

    #[test]
    fn marker_is_written_only_after_a_clean_run() {
        let f = fixture();
        write(&f.destination.join("models"), b"not a directory");
        let report = record(&f, &AtomicBool::new(false));
        assert!(!report.failures.is_empty());
        assert_eq!(read_decision(&f.destination), None, "failures: offer again");
        assert!(offer(&f.source, &f.destination, false).is_some());

        fs::remove_file(f.destination.join("models")).unwrap();
        let report = record(&f, &AtomicBool::new(false));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(
            read_decision(&f.destination),
            Some(ImportDecision::Imported)
        );
        assert!(offer(&f.source, &f.destination, false).is_none());
    }

    #[test]
    fn cancelled_run_writes_no_marker() {
        let f = fixture();
        let report = record(&f, &AtomicBool::new(true));
        assert!(report.cancelled);
        assert_eq!(read_decision(&f.destination), None);
    }

    #[test]
    fn throttle_emits_phase_changes_steps_and_the_end() {
        let mib = 1024 * 1024;
        let event = |phase, copied_bytes| UpstreamImportProgress {
            phase,
            copied_bytes,
            total_bytes: 100 * mib,
        };
        let mut throttle = ProgressThrottle::default();
        let emitted: Vec<bool> = [
            event(ImportPhase::Models, 0),
            event(ImportPhase::Models, 4 * mib),
            event(ImportPhase::Models, 16 * mib),
            event(ImportPhase::Models, 20 * mib),
            // A phase change is announced even inside the 16 MiB step.
            event(ImportPhase::History, 21 * mib),
            event(ImportPhase::History, 25 * mib),
            event(ImportPhase::Settings, 26 * mib),
            event(ImportPhase::Settings, 100 * mib),
        ]
        .iter()
        .map(|progress| throttle.should_emit(progress))
        .collect();
        assert_eq!(emitted, [true, false, true, false, true, false, true, true]);
    }

    #[test]
    fn command_run_emits_every_phase_once_for_small_imports() {
        let f = fixture();
        let mut events = Vec::new();
        run_and_record(
            &f.source,
            &f.destination,
            all(),
            &mut |_| Ok(()),
            &mut |progress| events.push(progress),
            &AtomicBool::new(false),
        )
        .unwrap();
        let phases: Vec<ImportPhase> = events.iter().map(|e| e.phase).collect();
        assert_eq!(
            phases,
            [
                ImportPhase::Models,
                ImportPhase::History,
                ImportPhase::Settings,
                ImportPhase::Settings
            ],
            "start of each phase plus the final total"
        );
        let last = events.last().unwrap();
        assert_eq!(last.copied_bytes, last.total_bytes);
    }

    #[test]
    fn stale_import_temps_are_removed() {
        let f = fixture();
        let d = &f.destination;
        write(&d.join("models/ggml-small.bin.importing"), b"half");
        write(
            &d.join("models/parakeet-v3.importing/encoder.onnx"),
            b"half",
        );
        write(&d.join("history.db.importing"), b"half");
        write(&d.join("history.db.importing-journal"), b"half");
        write(&d.join("recordings/handy-2.wav.importing"), b"half");
        write(&d.join("models/ggml-base.bin"), b"keep");
        write(&d.join("history.db"), b"keep");

        remove_stale_temps(d);

        let mut left: Vec<String> = snapshot(d).into_keys().collect();
        left.sort();
        let expected = [
            PathBuf::from("history.db"),
            PathBuf::from("models"),
            Path::new("models").join("ggml-base.bin"),
            PathBuf::from("recordings"),
        ]
        .map(|p| p.display().to_string());
        assert_eq!(left, expected);
    }

    /// Create a directory link; `None` when this machine does not allow it.
    fn link_dir(target: &Path, link: &Path) -> Option<()> {
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_dir(target, link).is_ok() {
                return Some(());
            }
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()?;
            status.success().then_some(())
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).ok()
        }
    }

    #[test]
    fn a_model_that_is_a_directory_link_is_skipped_and_reported() {
        let f = fixture();
        let elsewhere = f.destination.join("../elsewhere");
        write(&elsewhere.join("weights.bin"), &[3u8; 700]);
        if link_dir(&elsewhere, &f.source.join("models/shared-model")).is_none() {
            eprintln!("skipped: directory links are not available here");
            return;
        }
        let offer = offer(&f.source, &f.destination, false).expect("offer");
        assert_eq!(
            (offer.model_count, offer.model_bytes),
            (2, 9000 + 300 + 6),
            "the link is neither counted nor sized"
        );

        let report = import(&f, all(), &mut None);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.models_copied, 2);
        assert_eq!(report.skipped_links, ["models/shared-model"]);
        assert!(!f.destination.join("models/shared-model").exists());
    }

    #[test]
    fn history_with_unfinished_upstream_changes_gets_a_readable_error() {
        let f = fixture();
        // A hot rollback journal: upstream Handy stopped (or is) in the
        // middle of a write. A read-only connection cannot roll it back.
        let scratch = f.destination.join("../hot");
        let writer = history_db(&scratch.join("history.db"), supported(), 3);
        // Without syncs SQLite writes the journal header at once; otherwise
        // it stays zeroed (not hot) until the first sync.
        writer.pragma_update(None, "synchronous", "OFF").unwrap();
        writer
            .execute_batch("BEGIN IMMEDIATE; UPDATE transcription_history SET title = 'x';")
            .unwrap();
        assert!(scratch.join("history.db-journal").is_file());
        fs::copy(scratch.join("history.db"), f.source.join("history.db")).unwrap();
        fs::copy(
            scratch.join("history.db-journal"),
            f.source.join("history.db-journal"),
        )
        .unwrap();
        writer.execute_batch("ROLLBACK;").unwrap();
        drop(writer);

        let report = import(&f, all(), &mut None);

        assert_eq!(failed_items(&report), ["history.db"]);
        assert_eq!(
            report.failures[0].error,
            "it has unfinished changes from the original Handy; close the original Handy and try again"
        );
        assert!(!f.destination.join("history.db").exists());
    }

    #[test]
    fn a_second_run_with_settings_unchecked_leaves_fork_settings_byte_identical() {
        let f = fixture();
        let store = f.destination.join(SETTINGS_STORE_PATH);
        let cue = f.destination.join("custom_start.wav");
        let mut write_store = |settings: &AppSettings| {
            fs::write(&store, serde_json::to_vec(settings).unwrap()).map_err(|e| e.to_string())
        };
        // First run: settings imported, but a model fails, so it is offered
        // again on the next start.
        write(&f.destination.join("models"), b"not a directory");
        let first = run_and_record(
            &f.source,
            &f.destination,
            all(),
            &mut write_store,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(first.settings_imported && !first.failures.is_empty());
        assert!(
            offer(&f.source, &f.destination, true)
                .unwrap()
                .replaces_settings
        );

        // The user then changes the fork's settings and cue.
        fs::write(&store, b"{\"settings\":{\"fork\":\"own changes\"}}").unwrap();
        fs::write(&cue, b"fork-own-cue").unwrap();
        let store_before = fs::read(&store).unwrap();
        fs::remove_file(f.destination.join("models")).unwrap();

        let second = run_and_record(
            &f.source,
            &f.destination,
            UpstreamImportChoice {
                settings: false,
                models: true,
                history: false,
            },
            &mut write_store,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(second.failures.is_empty(), "{:?}", second.failures);
        assert!(!second.settings_imported);
        assert_eq!(fs::read(&store).unwrap(), store_before);
        assert_eq!(fs::read(&cue).unwrap(), b"fork-own-cue");
        assert_eq!(second.models_copied, 2);
    }

    #[test]
    fn directory_link_loops_are_not_followed() {
        let f = fixture();
        let model = f.source.join("models/parakeet-v3");
        if link_dir(&model, &model.join("loop")).is_none() {
            eprintln!("skipped: directory links are not available here");
            return;
        }
        let offer = offer(&f.source, &f.destination, false).expect("offer");
        assert_eq!(offer.model_bytes, 9000 + 300 + 6, "the loop adds nothing");

        let report = import(&f, all(), &mut None);

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.skipped_links, ["models/parakeet-v3/loop"]);
        let copied = f.destination.join("models/parakeet-v3");
        assert!(copied.join("vocab.txt").is_file());
        assert!(!copied.join("loop").exists(), "links are not copied");
    }
}
