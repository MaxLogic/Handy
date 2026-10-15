//! Portable mode support for Handy.
//!
//! When a file named `portable` exists next to the executable, all user data
//! (settings, models, recordings, database, logs) is stored in a `Data/`
//! directory alongside the executable instead of `%APPDATA%`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tauri::Manager;

static PORTABLE_DATA_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static PREVIOUS_HF_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Detect portable mode by looking for a `portable` marker file next to the exe.
/// Must be called once at startup before Tauri initializes.
///
/// Fails when the marker is present but `Data/` cannot be created or written:
/// the app must then not start, rather than silently keep its data in
/// `%APPDATA%` while the user expects it next to the executable.
pub fn init() -> Result<(), String> {
    let previous_hf_home = std::env::var_os("HF_HOME").map(PathBuf::from);
    let _ = PREVIOUS_HF_HOME.set(previous_hf_home);

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let data_dir = match exe_dir {
        Some(exe_dir) => portable_data_dir(&exe_dir)?,
        None => None,
    };
    if let Some(data_dir) = &data_dir {
        let hf_home = hugging_face_home(data_dir);
        std::env::set_var("HF_HOME", &hf_home);
        eprintln!("[portable] data dir: {}", data_dir.display());
        eprintln!("[portable] Hugging Face home: {}", hf_home.display());
    }
    let _ = PORTABLE_DATA_DIR.set(data_dir);
    Ok(())
}

/// The portable `Data/` directory next to the executable, created when
/// missing; `None` when this is not a portable install.
fn portable_data_dir(exe_dir: &Path) -> Result<Option<PathBuf>, String> {
    let marker_path = exe_dir.join("portable");
    let data_dir = exe_dir.join("Data");

    let is_portable = if is_valid_portable_marker(&marker_path) {
        true
    } else if marker_path.exists() && data_dir.exists() {
        // Migration: v0.8.0 created an empty marker file. If we find an
        // empty/invalid marker alongside an existing Data/ dir, this is a
        // real portable install — upgrade the marker in place.
        eprintln!("[portable] upgrading legacy empty marker to magic string");
        let _ = std::fs::write(&marker_path, "Handy Portable Mode");
        true
    } else {
        false
    };
    if !is_portable {
        return Ok(None);
    }

    let unusable = |error: std::io::Error| {
        format!(
            "This is a portable install (the file '{}' exists), but its data folder '{}' cannot be used: {error}.\n\nMove the program folder to a location you can write to, or delete the 'portable' file to keep data in the user profile instead.",
            marker_path.display(),
            data_dir.display()
        )
    };
    std::fs::create_dir_all(&data_dir).map_err(unusable)?;
    let probe = data_dir.join(".write-check");
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .map_err(unusable)?;
    Ok(Some(data_dir))
}

/// Report a startup error the user must see and exit. A Windows release build
/// has no console, so the message also goes to a message box there.
pub fn exit_with_startup_error(message: &str) -> ! {
    eprintln!("[portable] {message}");
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(message),
                &HSTRING::from("Handy MaxLogic cannot start"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    std::process::exit(1)
}

/// Return the Hugging Face home configured before portable mode redirected it.
///
/// Portable releases before v0.9.6 downloaded models to this location (or to
/// hf-hub's default cache when it is `None`). Keeping it lets the model manager
/// recognize those downloads after an upgrade without copying multi-gigabyte
/// files or modifying a cache shared with other applications.
pub fn previous_hf_home() -> Option<&'static PathBuf> {
    PREVIOUS_HF_HOME.get().and_then(|path| path.as_ref())
}

/// Keep hf-hub downloads inside the portable data directory. hf-hub appends
/// its own `hub` component to `HF_HOME` for model snapshots and blobs.
fn hugging_face_home(data_dir: &Path) -> PathBuf {
    data_dir.join("huggingface")
}

/// Returns `true` if running in portable mode.
pub fn is_portable() -> bool {
    PORTABLE_DATA_DIR.get().and_then(|v| v.as_ref()).is_some()
}

/// Get the portable data dir (if active). Does not require an AppHandle.
/// Returns `None` when not in portable mode.
pub fn data_dir() -> Option<&'static PathBuf> {
    PORTABLE_DATA_DIR.get().and_then(|v| v.as_ref())
}

/// Portable-aware replacement for `app.path().app_data_dir()`.
pub fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, tauri::Error> {
    if let Some(dir) = data_dir() {
        Ok(dir.clone())
    } else {
        app.path().app_data_dir()
    }
}

/// Portable-aware replacement for `app.path().app_log_dir()`.
pub fn app_log_dir(app: &tauri::AppHandle) -> Result<PathBuf, tauri::Error> {
    if let Some(dir) = data_dir() {
        Ok(dir.join("logs"))
    } else {
        app.path().app_log_dir()
    }
}

/// Resolve a relative path against the app data directory (portable-aware).
/// Replaces `app.path().resolve(path, BaseDirectory::AppData)`.
pub fn resolve_app_data(app: &tauri::AppHandle, relative: &str) -> Result<PathBuf, tauri::Error> {
    Ok(app_data_dir(app)?.join(relative))
}

/// Get the path to use with `tauri-plugin-store`.
/// Returns an absolute path in portable mode (so the store plugin writes to
/// the portable Data dir) or the original relative path otherwise.
pub fn store_path(relative: &str) -> PathBuf {
    if let Some(dir) = data_dir() {
        dir.join(relative)
    } else {
        PathBuf::from(relative)
    }
}

/// Check if a marker file path contains the portable magic string.
/// Extracted for testability.
fn is_valid_portable_marker(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.trim().starts_with("Handy Portable Mode"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_valid_magic_string_enables_portable() {
        let dir = std::env::temp_dir().join("handy_test_valid");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("portable");
        let mut f = std::fs::File::create(&marker).unwrap();
        write!(f, "Handy Portable Mode").unwrap();
        assert!(is_valid_portable_marker(&marker));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_empty_file_does_not_enable_portable() {
        let dir = std::env::temp_dir().join("handy_test_empty");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("portable");
        std::fs::File::create(&marker).unwrap();
        assert!(!is_valid_portable_marker(&marker));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_wrong_content_does_not_enable_portable() {
        let dir = std::env::temp_dir().join("handy_test_wrong");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("portable");
        let mut f = std::fs::File::create(&marker).unwrap();
        write!(f, "some other content").unwrap();
        assert!(!is_valid_portable_marker(&marker));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_missing_file_does_not_enable_portable() {
        let path = std::path::Path::new("/nonexistent/portable");
        assert!(!is_valid_portable_marker(path));
    }

    #[test]
    fn test_legacy_empty_marker_without_data_dir_does_not_enable_portable() {
        // Empty marker alone (scoop scenario) — no Data/ dir → not portable
        let dir = std::env::temp_dir().join("handy_test_legacy_no_data");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("portable");
        std::fs::File::create(&marker).unwrap();
        assert!(!is_valid_portable_marker(&marker));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_magic_string_with_whitespace_enables_portable() {
        let dir = std::env::temp_dir().join("handy_test_ws");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("portable");
        let mut f = std::fs::File::create(&marker).unwrap();
        write!(f, "  Handy Portable Mode\n").unwrap();
        assert!(is_valid_portable_marker(&marker));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn portable_marker_creates_data_next_to_the_exe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("portable"), "Handy Portable Mode").unwrap();

        let data = portable_data_dir(dir.path()).unwrap();

        assert_eq!(data, Some(dir.path().join("Data")));
        assert!(dir.path().join("Data").is_dir());
        assert!(!dir.path().join("Data").join(".write-check").exists());
    }

    #[test]
    fn no_marker_is_not_portable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(portable_data_dir(dir.path()).unwrap(), None);
        assert!(!dir.path().join("Data").exists());
    }

    #[test]
    fn unusable_data_folder_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("portable"), "Handy Portable Mode").unwrap();
        // A file where Data/ should be: it can be neither created nor used.
        std::fs::write(dir.path().join("Data"), b"not a folder").unwrap();

        let error = portable_data_dir(dir.path()).unwrap_err();

        assert!(error.contains("cannot be used"), "{error}");
        assert!(error.contains("Data"), "{error}");
    }

    #[test]
    fn test_hugging_face_home_is_inside_portable_data() {
        let data_dir = Path::new("portable-root").join("Data");

        assert_eq!(hugging_face_home(&data_dir), data_dir.join("huggingface"));
    }
}
