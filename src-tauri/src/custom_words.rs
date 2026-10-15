//! Custom words live in `custom_words.txt` in the app data directory
//! (portable: `Data\custom_words.txt`), one word or phrase per line. Blank
//! lines and lines whose first non-space character is `#` are ignored, so the
//! user can keep notes in the file. The file is the single source of truth:
//! `settings.custom_words` mirrors it so every consumer (Whisper initial
//! prompt, fuzzy correction, LLM vocabulary) keeps reading settings unchanged.
//!
//! External edits are picked up by polling the file's size and modification
//! time. No file-watch crate is in the dependency tree, and a poll needs no
//! special handling for editors that save by writing a temp file and renaming
//! it over the original: the next poll simply sees a new signature. A change is
//! applied once the signature has been stable for one poll interval, which
//! debounces editors that write in several steps.

use log::{info, warn};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings;

pub const FILE_NAME: &str = "custom_words.txt";
const TEMP_SUFFIX: &str = ".tmp";
const POLL_INTERVAL: Duration = Duration::from_millis(500);

const HEADER: &str = "\
# Handy custom words: one word or phrase per line.
# Lines starting with # are comments; blank lines are ignored.
# Handy picks up changes to this file automatically.";

/// Error codes returned to the frontend.
pub const ERR_EMPTY: &str = "empty";
pub const ERR_COMMENT_PREFIX: &str = "comment_prefix";
pub const ERR_UNREADABLE: &str = "file_unreadable";

/// The word on a line, or `None` for blank and comment lines. Only a leading
/// `#` starts a comment, so `C#` stays a word.
fn line_word(line: &str) -> Option<&str> {
    let word = line.trim();
    (!word.is_empty() && !word.starts_with('#')).then_some(word)
}

/// Words in file order, deduplicated case-sensitively (first occurrence wins).
pub fn parse(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in text.lines().filter_map(line_word) {
        if !words.iter().any(|w| w == word) {
            words.push(word.to_string());
        }
    }
    words
}

/// File bytes as text, without a UTF-8 BOM.
pub fn decode(bytes: &[u8]) -> Result<&str, std::str::Utf8Error> {
    let text = std::str::from_utf8(bytes)?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// Whether a word can be stored on its own line and read back unchanged.
fn is_storable(word: &str) -> bool {
    line_word(word) == Some(word) && !word.contains(['\n', '\r'])
}

/// Trimmed, storable, deduplicated words. Words the file cannot represent
/// (such as a leading `#`) are dropped with a warning.
fn storable(words: &[String]) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for word in words {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        if !is_storable(word) {
            warn!("Custom word {word:?} cannot be stored in {FILE_NAME} and was dropped");
            continue;
        }
        if !result.iter().any(|w| w == word) {
            result.push(word.to_string());
        }
    }
    result
}

/// A new file: the format header, then the words.
pub fn render_new(words: &[String]) -> String {
    let mut text = String::from(HEADER);
    text.push('\n');
    for word in storable(words) {
        text.push_str(&word);
        text.push('\n');
    }
    text
}

/// `text` changed to hold exactly `words`. Comments, blank lines and the
/// remaining words keep their place and spelling; words no longer wanted (and
/// duplicates) are removed; new words are appended. The BOM and line-ending
/// style of the original are kept.
pub fn rewrite(text: &str, words: &[String]) -> String {
    let bom = text.starts_with('\u{feff}');
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let newline = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let wanted = storable(words);
    let mut kept: Vec<&str> = Vec::new();
    let mut written: Vec<&str> = Vec::new();
    for line in body.lines() {
        match line_word(line) {
            Some(word) if wanted.iter().any(|w| w == word) && !written.contains(&word) => {
                written.push(word);
                kept.push(line);
            }
            Some(_) => {}
            None => kept.push(line),
        }
    }
    kept.extend(
        wanted
            .iter()
            .map(String::as_str)
            .filter(|w| !written.contains(w)),
    );
    let mut result = String::new();
    if bom {
        result.push('\u{feff}');
    }
    for line in kept {
        result.push_str(line);
        result.push_str(newline);
    }
    result
}

fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(TEMP_SUFFIX);
    let temp = path.with_file_name(name);
    fs::write(&temp, text)?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// What a poll compares; any save changes the size or the modification time.
type Signature = (u64, Option<SystemTime>);

fn signature(path: &Path) -> io::Result<Signature> {
    let meta = fs::metadata(path)?;
    Ok((meta.len(), meta.modified().ok()))
}

#[derive(Default)]
struct WatchState {
    /// Signature of the content last applied (or written by Handy).
    applied: Option<Signature>,
    /// A new signature seen on the previous poll, applied once it is stable.
    pending: Option<Signature>,
    /// Last problem logged, so a broken file is reported once, not every poll.
    reported: Option<String>,
}

/// The custom words file and its watch state. File reads and writes happen
/// under one lock so a GUI edit and a reload never interleave.
pub struct CustomWordsFile {
    path: PathBuf,
    state: Mutex<WatchState>,
}

impl CustomWordsFile {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(WatchState::default()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn report(state: &mut WatchState, message: String) {
        if state.reported.as_ref() != Some(&message) {
            warn!("{message}");
            state.reported = Some(message);
        }
    }

    /// Startup: the words to use. Creates the file from `current` when it is
    /// missing. Returns `None` (keep `current`) when the file is unreadable.
    pub fn load_or_create(&self, current: &[String]) -> Option<Vec<String>> {
        let mut state = self.lock();
        match fs::read(&self.path) {
            Ok(bytes) => {
                state.applied = signature(&self.path).ok();
                match decode(&bytes) {
                    Ok(text) => Some(parse(text)),
                    Err(e) => {
                        Self::report(
                            &mut state,
                            format!(
                                "{} is not valid UTF-8, keeping the saved custom words: {e}",
                                self.path.display()
                            ),
                        );
                        None
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let text = render_new(current);
                if let Some(parent) = self.path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                match write_atomically(&self.path, &text) {
                    Ok(()) => {
                        info!(
                            "Created {} with {} custom words",
                            self.path.display(),
                            parse(&text).len()
                        );
                        state.applied = signature(&self.path).ok();
                    }
                    Err(e) => Self::report(
                        &mut state,
                        format!("Failed to create {}: {e}", self.path.display()),
                    ),
                }
                Some(parse(&text))
            }
            Err(e) => {
                Self::report(
                    &mut state,
                    format!(
                        "Failed to read {}, keeping the saved custom words: {e}",
                        self.path.display()
                    ),
                );
                None
            }
        }
    }

    /// Read-only load for headless runs: never creates the file.
    pub fn load_existing(&self) -> Option<Vec<String>> {
        let bytes = fs::read(&self.path).ok()?;
        decode(&bytes).ok().map(parse)
    }

    /// One watch step. Returns the new word list when the file changed and
    /// its signature has been stable since the previous poll. A missing or
    /// unreadable file keeps the last list.
    pub fn poll(&self) -> Option<Vec<String>> {
        let mut state = self.lock();
        let sig = match signature(&self.path) {
            Ok(sig) => sig,
            Err(e) => {
                state.pending = None;
                let message = if e.kind() == io::ErrorKind::NotFound {
                    format!(
                        "{} was removed; keeping the current custom words until it is saved again",
                        self.path.display()
                    )
                } else {
                    format!("Failed to check {}: {e}", self.path.display())
                };
                Self::report(&mut state, message);
                return None;
            }
        };
        if state.applied == Some(sig) {
            state.pending = None;
            return None;
        }
        if state.pending != Some(sig) {
            state.pending = Some(sig);
            return None;
        }
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) => {
                // Possibly mid-save; retry on the next poll.
                Self::report(
                    &mut state,
                    format!("Failed to read {}: {e}", self.path.display()),
                );
                return None;
            }
        };
        state.pending = None;
        state.applied = Some(sig);
        match decode(&bytes) {
            Ok(text) => {
                state.reported = None;
                Some(parse(text))
            }
            Err(e) => {
                Self::report(
                    &mut state,
                    format!(
                        "{} is not valid UTF-8, keeping the current custom words: {e}",
                        self.path.display()
                    ),
                );
                None
            }
        }
    }

    /// Edit the file: `edit` maps the file's words to the wanted words, and
    /// the file is rewritten keeping comments and order. A missing file is
    /// recreated from `current`; an unreadable one is left untouched.
    pub fn edit(
        &self,
        current: &[String],
        edit: impl FnOnce(Vec<String>) -> Vec<String>,
    ) -> Result<Vec<String>, String> {
        let mut state = self.lock();
        let text = match fs::read(&self.path) {
            Ok(bytes) => decode(&bytes)
                .map_err(|_| ERR_UNREADABLE.to_string())?
                .to_string(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => render_new(current),
            Err(e) => {
                warn!("Failed to read {}: {e}", self.path.display());
                return Err(ERR_UNREADABLE.to_string());
            }
        };
        let words = edit(parse(&text));
        let text = rewrite(&text, &words);
        write_atomically(&self.path, &text).map_err(|e| e.to_string())?;
        state.applied = signature(&self.path).ok();
        state.pending = None;
        state.reported = None;
        Ok(parse(&text))
    }

    pub fn add(&self, current: &[String], word: &str) -> Result<Vec<String>, String> {
        let word = word.trim();
        if word.is_empty() {
            return Err(ERR_EMPTY.to_string());
        }
        if !is_storable(word) {
            return Err(ERR_COMMENT_PREFIX.to_string());
        }
        self.edit(current, |mut words| {
            if !words.iter().any(|w| w == word) {
                words.push(word.to_string());
            }
            words
        })
    }

    pub fn remove(&self, current: &[String], word: &str) -> Result<Vec<String>, String> {
        self.edit(current, |words| {
            words.into_iter().filter(|w| w != word).collect()
        })
    }

    pub fn replace(&self, current: &[String], words: &[String]) -> Result<Vec<String>, String> {
        self.edit(current, |_| words.to_vec())
    }
}

/// Store the words in settings and tell the UI, unless they are unchanged.
pub fn apply(app: &AppHandle, words: Vec<String>) {
    let mut settings = settings::get_settings(app);
    if settings.custom_words == words {
        return;
    }
    settings.custom_words = words.clone();
    settings::write_settings(app, settings);
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({ "setting": "custom_words", "value": words }),
    );
}

fn file_path(app: &AppHandle) -> Result<PathBuf, String> {
    crate::portable::resolve_app_data(app, FILE_NAME).map_err(|e| e.to_string())
}

/// The managed custom words file. Fails only before `start` ran.
pub fn file(app: &AppHandle) -> Result<Arc<CustomWordsFile>, String> {
    app.try_state::<Arc<CustomWordsFile>>()
        .map(|state| state.inner().clone())
        .ok_or_else(|| "custom words file not initialized".to_string())
}

/// Load (or create) the file, mirror it into settings and watch for changes.
pub fn start(app: &AppHandle) {
    let path = match file_path(app) {
        Ok(path) => path,
        Err(e) => {
            warn!("Custom words file unavailable: {e}");
            return;
        }
    };
    let file = Arc::new(CustomWordsFile::new(path));
    if let Some(words) = file.load_or_create(&settings::get_settings(app).custom_words) {
        apply(app, words);
    }
    app.manage(file.clone());
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("custom-words-watch".into())
        .spawn(move || loop {
            std::thread::sleep(POLL_INTERVAL);
            if let Some(words) = file.poll() {
                info!(
                    "Reloaded {} custom words from {}",
                    words.len(),
                    file.path().display()
                );
                apply(&app, words);
            }
        });
    if let Err(e) = spawned {
        warn!("Failed to start the custom words watcher: {e}");
    }
}

/// Headless runs read the file once so they use the same words, without
/// creating it or watching it.
pub fn sync_headless(app: &AppHandle) {
    let Ok(path) = file_path(app) else { return };
    if let Some(words) = CustomWordsFile::new(path).load_existing() {
        let mut settings = settings::get_settings(app);
        if settings.custom_words != words {
            settings.custom_words = words;
            settings::write_settings(app, settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn parse_skips_comments_and_blanks_and_dedupes() {
        let text =
            "# header\n\n  Handy  \n   # indented comment\nC#\nHandy\nhandy\nMacBook Pro\n\t\n";
        assert_eq!(parse(text), words(&["Handy", "C#", "handy", "MacBook Pro"]));
    }

    #[test]
    fn decode_strips_bom_and_parse_tolerates_crlf() {
        let bytes = b"\xEF\xBB\xBFTFDQuery\r\n# note\r\nTADOQuery.Open\r\n";
        let text = decode(bytes).unwrap();
        assert_eq!(parse(text), words(&["TFDQuery", "TADOQuery.Open"]));
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        assert!(decode(b"ok\n\xFF\xFE bad").is_err());
    }

    #[test]
    fn render_new_has_a_header_and_drops_unstorable_words() {
        let text = render_new(&words(&[" Handy ", "#tag", "", "Handy", "C#"]));
        assert!(text.starts_with("# Handy custom words"));
        assert_eq!(parse(&text), words(&["Handy", "C#"]));
    }

    #[test]
    fn rewrite_keeps_comments_order_bom_and_crlf() {
        let text = "\u{feff}# mine\r\nAlpha\r\n\r\n# names\r\n  Beta  \r\nGamma\r\nAlpha\r\n";
        let result = rewrite(text, &words(&["Gamma", "Alpha", "Delta"]));
        assert_eq!(
            result,
            "\u{feff}# mine\r\nAlpha\r\n\r\n# names\r\nGamma\r\nDelta\r\n"
        );
    }

    #[test]
    fn rewrite_keeps_original_spelling_of_kept_lines() {
        let result = rewrite("# c\n  Beta  \n", &words(&["Beta"]));
        assert_eq!(result, "# c\n  Beta  \n");
    }

    fn fixture(text: &str) -> (tempfile::TempDir, CustomWordsFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, text).unwrap();
        let file = CustomWordsFile::new(path);
        (dir, file)
    }

    #[test]
    fn gui_add_and_remove_keep_comments() {
        let (_dir, file) = fixture("# my words\nAlpha\n# keep me\nBeta\n");
        assert_eq!(file.load_or_create(&[]), Some(words(&["Alpha", "Beta"])));
        assert_eq!(
            file.add(&[], "  Gamma ").unwrap(),
            words(&["Alpha", "Beta", "Gamma"])
        );
        assert_eq!(
            file.remove(&[], "Alpha").unwrap(),
            words(&["Beta", "Gamma"])
        );
        assert_eq!(
            fs::read_to_string(file.path()).unwrap(),
            "# my words\n# keep me\nBeta\nGamma\n"
        );
        assert_eq!(file.add(&[], "#tag"), Err(ERR_COMMENT_PREFIX.to_string()));
        assert_eq!(file.add(&[], "   "), Err(ERR_EMPTY.to_string()));
        // Handy's own write is not reported back as an external change.
        assert_eq!(file.poll(), None);
        assert_eq!(file.poll(), None);
    }

    #[test]
    fn missing_file_is_created_from_settings_with_a_header() {
        let dir = tempfile::tempdir().unwrap();
        let file = CustomWordsFile::new(dir.path().join("sub").join(FILE_NAME));
        let loaded = file.load_or_create(&words(&["Handy", "cjpais"]));
        assert_eq!(loaded, Some(words(&["Handy", "cjpais"])));
        let text = fs::read_to_string(file.path()).unwrap();
        assert!(text.starts_with("# Handy custom words"), "{text}");
        assert_eq!(parse(&text), words(&["Handy", "cjpais"]));
    }

    #[test]
    fn unreadable_file_keeps_the_last_list_and_blocks_gui_edits() {
        let (_dir, file) = fixture("Alpha\n");
        assert_eq!(file.load_or_create(&[]), Some(words(&["Alpha"])));
        fs::write(file.path(), b"Alpha\n\xFF\xFEBeta\n").unwrap();
        assert_eq!(file.poll(), None);
        assert_eq!(file.poll(), None, "invalid UTF-8 keeps the last list");
        assert_eq!(file.add(&[], "Gamma"), Err(ERR_UNREADABLE.to_string()));
        assert_eq!(
            fs::read(file.path()).unwrap(),
            b"Alpha\n\xFF\xFEBeta\n",
            "an unreadable file is never overwritten"
        );
    }

    #[test]
    fn deleted_file_keeps_the_last_list_until_a_gui_edit_recreates_it() {
        let (_dir, file) = fixture("Alpha\n");
        file.load_or_create(&[]);
        fs::remove_file(file.path()).unwrap();
        assert_eq!(file.poll(), None);
        assert_eq!(file.poll(), None);
        assert!(!file.path().exists(), "nothing is recreated on its own");
        let current = words(&["Alpha"]);
        assert_eq!(
            file.add(&current, "Beta").unwrap(),
            words(&["Alpha", "Beta"])
        );
        assert!(fs::read_to_string(file.path())
            .unwrap()
            .starts_with("# Handy"));
    }

    /// Runs the real watch loop on a thread and waits for the next reload.
    fn watch(file: Arc<CustomWordsFile>) -> std::sync::mpsc::Receiver<Vec<String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || loop {
            std::thread::sleep(POLL_INTERVAL);
            if let Some(words) = file.poll() {
                if tx.send(words).is_err() {
                    return;
                }
            }
        });
        rx
    }

    #[test]
    fn external_edits_are_applied_within_two_seconds() {
        let (dir, file) = fixture("# words\nAlpha\n");
        let file = Arc::new(file);
        file.load_or_create(&[]);
        let rx = watch(file.clone());
        let deadline = Duration::from_secs(2);

        // In-place save.
        let started = Instant::now();
        fs::write(file.path(), "# words\nAlpha\nBeta\n").unwrap();
        assert_eq!(
            rx.recv_timeout(deadline).unwrap(),
            words(&["Alpha", "Beta"])
        );
        assert!(started.elapsed() < deadline);

        // Atomic save: write a temp file, then rename it over the original.
        let temp = dir.path().join("custom_words.txt~");
        fs::write(&temp, "# words\nGamma\n").unwrap();
        let started = Instant::now();
        fs::rename(&temp, file.path()).unwrap();
        assert_eq!(rx.recv_timeout(deadline).unwrap(), words(&["Gamma"]));
        assert!(started.elapsed() < deadline);
    }
}
