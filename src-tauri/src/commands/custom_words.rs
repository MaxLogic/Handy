//! Custom words commands. Every edit goes through `custom_words.txt`, which
//! keeps the user's comments, and the result is mirrored into settings.

use crate::custom_words::{self, CustomWordsFile};
use crate::settings::get_settings;
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

fn edit_file(
    app: &AppHandle,
    edit: impl FnOnce(&CustomWordsFile, &[String]) -> Result<Vec<String>, String>,
) -> Result<(), String> {
    let file: Arc<CustomWordsFile> = custom_words::file(app)?;
    let current = get_settings(app).custom_words;
    let words = edit(&file, &current)?;
    custom_words::apply(app, words);
    Ok(())
}

/// Replace the whole custom words list (used by setting resets).
#[tauri::command]
#[specta::specta]
pub fn update_custom_words(app: AppHandle, words: Vec<String>) -> Result<(), String> {
    edit_file(&app, |file, current| file.replace(current, &words))
}

/// Add one word to `custom_words.txt`. Errors: `empty`, `comment_prefix`
/// (the word starts with `#`), `file_unreadable`, or an I/O message.
#[tauri::command]
#[specta::specta]
pub fn add_custom_word(app: AppHandle, word: String) -> Result<(), String> {
    edit_file(&app, |file, current| file.add(current, &word))
}

/// Remove one word from `custom_words.txt`, keeping comments.
#[tauri::command]
#[specta::specta]
pub fn remove_custom_word(app: AppHandle, word: String) -> Result<(), String> {
    edit_file(&app, |file, current| file.remove(current, &word))
}

/// Open `custom_words.txt` in the default text editor, recreating it from the
/// current words if it was deleted.
#[tauri::command]
#[specta::specta]
pub fn open_custom_words_file(app: AppHandle) -> Result<(), String> {
    let file = custom_words::file(&app)?;
    if !file.path().exists() {
        edit_file(&app, |file, current| file.edit(current, |words| words))?;
    }
    app.opener()
        .open_path(file.path().to_string_lossy(), None::<String>)
        .map_err(|e| format!("Failed to open {}: {e}", custom_words::FILE_NAME))
}
