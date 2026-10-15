//! Launch-at-login (autostart) handling.
//!
//! Linux and macOS before 13 apply the setting through
//! tauri-plugin-autostart. Windows writes its own HKCU `Run` value (see
//! `windows_run`), and macOS 13+ registers the app as a login item via
//! `SMAppService`. The plugin's launch agent plist carries no app
//! association, so the System Settings Login Items pane attributes it to the
//! code-signing certificate's developer name instead of the app (#337).
//! `SMAppService` login items are attributed to the app bundle itself and
//! appear under "Open at Login" with the app's name and icon.

use tauri::AppHandle;
#[cfg(not(windows))]
use tauri_plugin_autostart::ManagerExt;

/// Apply the user's autostart preference using the best mechanism for the
/// current platform.
///
/// Errors are logged rather than returned: the preference is re-applied on
/// every launch, so a transient failure self-heals and must not block
/// startup. This mirrors the pre-existing behavior of ignoring
/// enable()/disable() results.
pub fn apply_autostart(app: &AppHandle, enabled: bool) {
    #[cfg(target_os = "macos")]
    if macos::login_item_api_available() {
        macos::remove_plugin_launch_agent(app);
        macos::set_login_item(enabled);
        return;
    }

    #[cfg(windows)]
    let result = windows_run::apply(&app.package_info().name, enabled);

    #[cfg(not(windows))]
    let result = {
        let manager = app.autolaunch();
        if enabled {
            manager.enable()
        } else {
            manager.disable()
        }
    };

    if let Err(e) = result {
        log::warn!(
            "Failed to apply autostart setting (enabled={}): {}",
            enabled,
            e
        );
    }
}

/// The HKCU `Run` entry on Windows.
///
/// tauri-plugin-autostart 2.5.1 (through auto-launch 0.5.0) writes the
/// executable path unquoted, so for a path with spaces such as
/// `D:\Tools\Handy MaxLogic\handy-maxlogic.exe` Windows may start
/// `D:\Tools\Handy.exe` instead. This writes the same value name the plugin
/// uses (the product name), so an entry it wrote earlier is replaced or
/// removed rather than duplicated, and the plugin's `is_enabled` still reads
/// it.
#[cfg(windows)]
mod windows_run {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::path::Path;

    use winreg::enums::{RegType, HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::{RegKey, RegValue};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const STARTUP_APPROVED_KEY: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    /// Task Manager's "enabled" state, as auto-launch writes it.
    const STARTUP_APPROVED_ENABLED: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    /// Launch arguments: none, as registered with the plugin in `lib.rs`.
    const ARGS: [&str; 0] = [];

    pub fn apply(app_name: &str, enabled: bool) -> io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if enabled {
            let command = command_line(&std::env::current_exe()?, &ARGS);
            enable(&hkcu, RUN_KEY, STARTUP_APPROVED_KEY, app_name, &command)
        } else {
            disable(&hkcu, RUN_KEY, app_name)
        }
    }

    /// `"<exe>"` followed by each argument, quoted so the command line parses
    /// back to the same path and arguments (CommandLineToArgvW rules).
    pub(super) fn command_line(exe: &Path, args: &[&str]) -> OsString {
        let mut command = OsString::from("\"");
        command.push(exe.as_os_str());
        command.push("\"");
        for arg in args {
            command.push(" ");
            command.push(quote_arg(arg));
        }
        command
    }

    fn quote_arg(arg: &str) -> String {
        let mut quoted = String::from('"');
        let mut backslashes = 0;
        for c in arg.chars() {
            if c == '\\' {
                backslashes += 1;
                continue;
            }
            // Backslashes are literal unless a quote follows: then each is
            // doubled and the quote escaped.
            let escaped = if c == '"' {
                backslashes * 2 + 1
            } else {
                backslashes
            };
            quoted.extend(std::iter::repeat_n('\\', escaped));
            quoted.push(c);
            backslashes = 0;
        }
        // Doubled so the closing quote stays a quote.
        quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
        quoted.push('"');
        quoted
    }

    pub(super) fn enable(
        root: &RegKey,
        run_key: &str,
        approved_key: &str,
        name: &str,
        command: &OsStr,
    ) -> io::Result<()> {
        let (run, _) = root.create_subkey_with_flags(run_key, KEY_SET_VALUE)?;
        run.set_value(name, &command)?;
        // As auto-launch did: an entry switched off in Task Manager is
        // switched back on, since autostart was just requested. The key may
        // not exist.
        if let Ok(approved) = root.open_subkey_with_flags(approved_key, KEY_SET_VALUE) {
            approved.set_raw_value(
                name,
                &RegValue {
                    bytes: STARTUP_APPROVED_ENABLED.to_vec(),
                    vtype: RegType::REG_BINARY,
                },
            )?;
        }
        Ok(())
    }

    /// Remove the value; a missing key or value is already disabled.
    pub(super) fn disable(root: &RegKey, run_key: &str, name: &str) -> io::Result<()> {
        let run = match root.open_subkey_with_flags(run_key, KEY_SET_VALUE) {
            Ok(run) => run,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        match run.delete_value(name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use winreg::enums::KEY_READ;

        #[test]
        fn command_line_quotes_paths_with_spaces() {
            let exe = Path::new(r"C:\Program Files\Handy MaxLogic\handy-maxlogic.exe");
            assert_eq!(
                command_line(exe, &[]),
                OsString::from(r#""C:\Program Files\Handy MaxLogic\handy-maxlogic.exe""#)
            );
        }

        #[test]
        fn command_line_keeps_non_ascii_paths() {
            let exe = Path::new(r"D:\Narzędzia\Zażółć gęślą\handy-maxlogic.exe");
            assert_eq!(
                command_line(exe, &[]),
                OsString::from(r#""D:\Narzędzia\Zażółć gęślą\handy-maxlogic.exe""#)
            );
        }

        #[test]
        fn command_line_quotes_each_argument() {
            let exe = Path::new(r"C:\A B\handy-maxlogic.exe");
            let command = command_line(
                exe,
                &[
                    "--start-hidden",
                    "two words",
                    r#"say "hi""#,
                    r"dir\",
                    r"a\b",
                ],
            );
            assert_eq!(
                command,
                OsString::from(
                    r#""C:\A B\handy-maxlogic.exe" "--start-hidden" "two words" "say \"hi\"" "dir\\" "a\b""#
                )
            );
        }

        const TEST_KEY: &str = r"Software\eu.maxlogic.handy.test";

        /// Deletes the throwaway test key, also when an assertion fails.
        struct TestKey;

        impl Drop for TestKey {
            fn drop(&mut self) {
                let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(TEST_KEY);
            }
        }

        /// Writes only below HKCU\Software\eu.maxlogic.handy.test, never the
        /// real Run key.
        #[test]
        fn enable_replaces_an_unquoted_entry_and_disable_removes_it() {
            let _cleanup = TestKey;
            let run_key = format!(r"{TEST_KEY}\Run");
            let approved_key = format!(r"{TEST_KEY}\StartupApproved\Run");
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let name = "Handy MaxLogic";
            // An unquoted entry as the plugin wrote it, switched off in Task
            // Manager.
            let (run, _) = hkcu.create_subkey(&run_key).unwrap();
            run.set_value(name, &r"D:\Zażółć gęślą\handy-maxlogic.exe ")
                .unwrap();
            let (approved, _) = hkcu.create_subkey(&approved_key).unwrap();
            approved
                .set_raw_value(
                    name,
                    &RegValue {
                        bytes: vec![3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8],
                        vtype: RegType::REG_BINARY,
                    },
                )
                .unwrap();
            let exe = Path::new(r"D:\Zażółć gęślą\handy-maxlogic.exe");

            enable(
                &hkcu,
                &run_key,
                &approved_key,
                name,
                &command_line(exe, &[]),
            )
            .unwrap();

            let run = hkcu.open_subkey_with_flags(&run_key, KEY_READ).unwrap();
            let names: Vec<String> = run.enum_values().map(|v| v.unwrap().0).collect();
            assert_eq!(names, [name], "replaced, not duplicated");
            let stored: OsString = run.get_value(name).unwrap();
            assert_eq!(
                stored,
                OsString::from(r#""D:\Zażółć gęślą\handy-maxlogic.exe""#)
            );
            let approved = hkcu
                .open_subkey_with_flags(&approved_key, KEY_READ)
                .unwrap();
            assert_eq!(
                approved.get_raw_value(name).unwrap().bytes,
                STARTUP_APPROVED_ENABLED
            );

            disable(&hkcu, &run_key, name).unwrap();
            assert!(run.get_raw_value(name).is_err(), "value removed");
            disable(&hkcu, &run_key, name).expect("disabling twice is fine");
            disable(&hkcu, &format!(r"{TEST_KEY}\Missing"), name)
                .expect("a missing key is already disabled");
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::{Path, PathBuf};

    use objc2::runtime::AnyClass;
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    use tauri::{AppHandle, Manager};

    /// `SMAppService` requires macOS 13. The ServiceManagement framework is
    /// linked unconditionally (it has existed since 10.6), so looking up the
    /// class doubles as the OS version check: present exactly when the API is
    /// usable.
    pub fn login_item_api_available() -> bool {
        AnyClass::get(c"SMAppService").is_some()
    }

    /// Register or unregister the app as a login item, skipping the call when
    /// the service is already in the requested state (unregistering a
    /// never-registered service returns an error on every launch otherwise).
    pub fn set_login_item(enabled: bool) {
        let service = unsafe { SMAppService::mainAppService() };
        let status = unsafe { service.status() };

        if enabled {
            if status == SMAppServiceStatus::Enabled {
                return;
            }
            match unsafe { service.registerAndReturnError() } {
                Ok(()) => log::info!("Registered login item via SMAppService"),
                // Fails in dev (no signed app bundle) and when the user has
                // switched the item off in System Settings, which apps are
                // not allowed to override.
                Err(e) => log::warn!("Failed to register login item: {}", e),
            }
        } else {
            if status == SMAppServiceStatus::NotRegistered || status == SMAppServiceStatus::NotFound
            {
                return;
            }
            match unsafe { service.unregisterAndReturnError() } {
                Ok(()) => log::info!("Unregistered login item via SMAppService"),
                Err(e) => log::warn!("Failed to unregister login item: {}", e),
            }
        }
    }

    /// Remove the launch agent plist that tauri-plugin-autostart (via the
    /// auto-launch crate) wrote on older versions, so login doesn't start the
    /// app twice after migrating to `SMAppService`. Runs on every launch;
    /// missing file is the normal case.
    pub fn remove_plugin_launch_agent(app: &AppHandle) {
        let Ok(home) = app.path().home_dir() else {
            return;
        };
        remove_launch_agent_file(&plugin_launch_agent_path(&home, &app.package_info().name));
    }

    /// Path of the plist the auto-launch crate writes:
    /// `~/Library/LaunchAgents/{app name}.plist`.
    fn plugin_launch_agent_path(home: &Path, app_name: &str) -> PathBuf {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", app_name))
    }

    fn remove_launch_agent_file(path: &Path) {
        match std::fs::remove_file(path) {
            Ok(()) => log::info!("Removed legacy autostart launch agent {:?}", path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Failed to remove legacy launch agent {:?}: {}", path, e),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Validates the assumption `login_item_api_available` rests on: the
        /// ServiceManagement framework is linked into the binary, so the
        /// class lookup finds `SMAppService` whenever the host is macOS 13+
        /// (which anything able to build this crate is).
        #[test]
        fn sm_app_service_class_resolves() {
            assert!(login_item_api_available());
        }

        #[test]
        fn launch_agent_path_matches_auto_launch_crate() {
            let path = plugin_launch_agent_path(Path::new("/Users/someone"), "Handy");
            assert_eq!(
                path,
                Path::new("/Users/someone/Library/LaunchAgents/Handy.plist")
            );
        }

        #[test]
        fn removes_existing_launch_agent() {
            let dir = tempfile::tempdir().unwrap();
            let plist = dir.path().join("Handy.plist");
            std::fs::write(&plist, "<plist/>").unwrap();

            remove_launch_agent_file(&plist);
            assert!(!plist.exists());
        }

        #[test]
        fn missing_launch_agent_is_a_no_op() {
            let dir = tempfile::tempdir().unwrap();
            remove_launch_agent_file(&dir.path().join("Handy.plist"));
        }
    }
}
