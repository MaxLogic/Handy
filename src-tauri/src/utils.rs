use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::shortcut;
use crate::TranscriptionCoordinator;
use log::{info, warn};
use std::sync::Arc;
use tauri::{AppHandle, Manager};

// Re-export all utility modules for easy access
// pub use crate::audio_feedback::*;
pub use crate::clipboard::*;
pub use crate::overlay::*;
pub use crate::tray::*;

/// Preserve diagnostic text in development builds, but redact it in releases.
/// Do not use for secrets such as API keys, which must always be redacted.
pub fn redact_text(text: &str) -> &str {
    if cfg!(debug_assertions) {
        text
    } else {
        "[REDACTED]"
    }
}

#[cfg(any(test, all(target_os = "windows", target_arch = "x86_64")))]
const IMAGE_FILE_MACHINE_ARM64: u16 = 0xaa64;

#[cfg(any(test, all(target_os = "windows", target_arch = "x86_64")))]
fn native_machine_is_arm64(native_machine: Option<u16>) -> bool {
    native_machine == Some(IMAGE_FILE_MACHINE_ARM64)
}

/// Whether this is the x64 Windows build running under emulation on Windows ARM64.
///
/// Only that exact process/host pairing disables the transcribe.cpp GPU path.
/// Detection is deliberately fail-open: a native x64 host, an older Windows
/// version without `IsWow64Process2`, or any API error leaves existing behavior
/// unchanged.
pub fn is_windows_x64_emulated_on_arm64() -> bool {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        use std::sync::OnceLock;

        static DETECTED: OnceLock<bool> = OnceLock::new();
        *DETECTED.get_or_init(|| native_machine_is_arm64(native_windows_machine()))
    }

    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    {
        false
    }
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn native_windows_machine() -> Option<u16> {
    use windows::core::{s, w, BOOL};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Threading::GetCurrentProcess;

    type IsWow64Process2 = unsafe extern "system" fn(HANDLE, *mut u16, *mut u16) -> BOOL;

    // Resolve IsWow64Process2 dynamically so merely starting Handy never raises
    // the minimum Windows version. Windows-on-ARM versions provide this API,
    // while a missing symbol or failed query safely preserves the x64 behavior.
    unsafe {
        let kernel32 = GetModuleHandleW(w!("kernel32.dll")).ok()?;
        let address = GetProcAddress(kernel32, s!("IsWow64Process2"))?;
        // SAFETY: GetProcAddress returned the documented IsWow64Process2 symbol;
        // function pointers have the same representation on supported Windows.
        let is_wow64_process2: IsWow64Process2 = std::mem::transmute(address);
        let mut process_machine = 0u16;
        let mut native_machine = 0u16;
        is_wow64_process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        )
        .as_bool()
        .then_some(native_machine)
    }
}

/// Native cancellation edges, substituted by deterministic lifecycle tests.
pub(crate) trait CancellationContext: shortcut::CancelShortcutContext {
    fn cancel_audio(&self) -> bool;
    fn cancel_transcription(&self);
    fn reset_after_cancel(&self);
    fn request_cancel(&self) -> bool;
}

impl CancellationContext for AppHandle {
    fn cancel_audio(&self) -> bool {
        let audio_manager = self.state::<Arc<AudioRecordingManager>>();
        let recording_was_active = audio_manager.is_recording();
        audio_manager.cancel_recording();
        recording_was_active
    }
    fn cancel_transcription(&self) {
        let tm = self.state::<Arc<TranscriptionManager>>();
        tm.cancel_stream();
        tm.cancel_transcription();
    }
    fn reset_after_cancel(&self) {
        set_tray_state(self, crate::tray::TrayIconState::Idle);
        hide_recording_overlay(self);
        self.state::<Arc<TranscriptionManager>>()
            .maybe_unload_immediately("cancellation");
    }
    fn request_cancel(&self) -> bool {
        self.try_state::<TranscriptionCoordinator>()
            .is_some_and(|coordinator| coordinator.request_cancel())
    }
}

/// Queue cancellation for the coordinator; tear down inline only without one.
pub(crate) fn cancel_current_operation(app: &impl CancellationContext) {
    if app.request_cancel() {
        info!("Cancellation requested");
        return;
    }
    warn!("No transcription coordinator; cancelling inline");
    perform_cancellation(app);
}

/// Coordinator-owned teardown, also used by the no-coordinator fallback.
pub(crate) fn perform_cancellation(app: &impl CancellationContext) -> bool {
    // Assistant ownership is retained until strict commit/abort acknowledgment.
    shortcut::unregister_cancel_shortcut(app);
    let recording_was_active = app.cancel_audio();
    app.cancel_transcription();
    // Unload before the coordinator reads its next command, so the next
    // recording loads afresh. This does not wait for cancelled work.
    app.reset_after_cancel();
    recording_was_active
}

/// Check if using the Wayland display server protocol
#[cfg(target_os = "linux")]
pub fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.to_lowercase() == "wayland")
            .unwrap_or(false)
}

/// Check if running on KDE Plasma desktop environment
#[cfg(target_os = "linux")]
pub fn is_kde_plasma() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_uppercase().contains("KDE"))
        .unwrap_or(false)
        || std::env::var("KDE_SESSION_VERSION").is_ok()
}

/// Check if running on KDE Plasma with Wayland
#[cfg(target_os = "linux")]
pub fn is_kde_wayland() -> bool {
    is_wayland() && is_kde_plasma()
}

/// Check if running on GNOME desktop environment
#[cfg(target_os = "linux")]
pub fn is_gnome() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_uppercase().contains("GNOME"))
        .unwrap_or(false)
}

/// Check if running on GNOME with Wayland
#[cfg(target_os = "linux")]
pub fn is_gnome_wayland() -> bool {
    is_wayland() && is_gnome()
}

/// Returns true when the environment variable is set to a truthy value
/// (e.g. "1", "true", "yes", "on").
/// "0", "false", "no", "off" and empty string are treated as falsy (case-insensitive).
/// Returns false when the variable is not set.
pub fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

/// Set `var` to `value` for this process (and its children) unless the user
/// already set it or opted out with a truthy `keep_flag`. Returns whether it set
/// the variable. Call only during single-threaded startup.
pub fn set_env_default_unless(var: &str, value: &str, keep_flag: &str) -> bool {
    if std::env::var_os(var).is_some() || env_flag_enabled(keep_flag) {
        return false;
    }
    std::env::set_var(var, value);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeCancellation {
        queued: bool,
        requests: std::sync::atomic::AtomicUsize,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }
    impl shortcut::CancelShortcutContext for FakeCancellation {
        fn register_cancel_fallback(&self) {}
        fn unregister_cancel_fallback(&self) {
            self.calls.lock().unwrap().push("unregister");
        }
        fn schedule_cancel_reconcile(&self) {}
    }
    impl CancellationContext for FakeCancellation {
        fn cancel_audio(&self) -> bool {
            self.calls.lock().unwrap().push("audio");
            true
        }
        fn cancel_transcription(&self) {
            self.calls.lock().unwrap().push("transcription");
        }
        fn reset_after_cancel(&self) {
            self.calls.lock().unwrap().push("reset");
        }
        fn request_cancel(&self) -> bool {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.queued
        }
    }
    #[test]
    fn cancel_request_runs_no_teardown_on_caller() {
        let _guard = crate::transcription_coordinator::tests::CANCEL_RUNTIME_TEST.blocking_lock();
        let app = FakeCancellation {
            queued: true,
            requests: std::sync::atomic::AtomicUsize::new(0),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        cancel_current_operation(&app);
        assert_eq!(app.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(app.calls.lock().unwrap().is_empty());
    }
    #[test]
    fn cancel_without_coordinator_tears_down_inline_in_order() {
        let _guard = crate::transcription_coordinator::tests::CANCEL_RUNTIME_TEST.blocking_lock();
        let app = FakeCancellation {
            queued: false,
            requests: std::sync::atomic::AtomicUsize::new(0),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        cancel_current_operation(&app);
        assert_eq!(app.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            *app.calls.lock().unwrap(),
            ["unregister", "audio", "transcription", "reset"]
        );
    }

    #[test]
    fn arm64_native_machine_is_the_only_match() {
        assert!(native_machine_is_arm64(Some(IMAGE_FILE_MACHINE_ARM64)));
        assert!(!native_machine_is_arm64(Some(0x8664))); // AMD64
        assert!(!native_machine_is_arm64(Some(0x014c))); // I386
        assert!(!native_machine_is_arm64(None)); // API unavailable or failed
    }

    #[test]
    fn env_default_sets_unset_variable() {
        let (var, keep) = (
            "HANDY_TEST_ENV_DEFAULT_UNSET",
            "HANDY_TEST_ENV_DEFAULT_UNSET_KEEP",
        );
        std::env::remove_var(var);
        assert!(set_env_default_unless(var, "1", keep));
        assert_eq!(std::env::var(var).as_deref(), Ok("1"));
        std::env::remove_var(var);
    }

    #[test]
    fn env_default_preserves_existing_value() {
        let (var, keep) = (
            "HANDY_TEST_ENV_DEFAULT_SET",
            "HANDY_TEST_ENV_DEFAULT_SET_KEEP",
        );
        std::env::set_var(var, "0");
        assert!(!set_env_default_unless(var, "1", keep));
        assert_eq!(std::env::var(var).as_deref(), Ok("0"));
        std::env::remove_var(var);
    }

    #[test]
    fn env_default_honours_opt_out_flag() {
        let (var, keep) = (
            "HANDY_TEST_ENV_DEFAULT_OPT",
            "HANDY_TEST_ENV_DEFAULT_OPT_KEEP",
        );
        std::env::remove_var(var);
        std::env::set_var(keep, "1");
        assert!(!set_env_default_unless(var, "1", keep));
        assert!(std::env::var_os(var).is_none());
        // A falsy opt-out value does not opt out.
        std::env::set_var(keep, "0");
        assert!(set_env_default_unless(var, "1", keep));
        std::env::remove_var(var);
        std::env::remove_var(keep);
    }

    #[test]
    fn env_flag_enabled_true_for_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            std::env::set_var("HANDY_TEST_FLAG_TRUTHY", value);
            assert!(env_flag_enabled("HANDY_TEST_FLAG_TRUTHY"), "{value:?}");
        }
        std::env::remove_var("HANDY_TEST_FLAG_TRUTHY");
    }

    #[test]
    fn env_flag_enabled_false_for_falsy_or_unset() {
        assert!(!env_flag_enabled("HANDY_TEST_FLAG_UNSET"));

        for value in ["0", "false", "FALSE", "no", "off", ""] {
            std::env::set_var("HANDY_TEST_FLAG_FALSY", value);
            assert!(!env_flag_enabled("HANDY_TEST_FLAG_FALSY"), "{value:?}");
        }
        std::env::remove_var("HANDY_TEST_FLAG_FALSY");
    }
}
