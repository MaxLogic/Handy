//! Identity of the personal "Handy MaxLogic" fork.
//!
//! The fork keeps upstream Handy's code, license and attribution but runs
//! under its own name and identifier, so its settings, models and history live
//! apart from an installed upstream Handy (`com.pais.handy`). It never updates
//! itself: an upstream release would replace the fork with upstream Handy.

/// Display name used for the window title, tray tooltip and package folder.
pub const PRODUCT_NAME: &str = "Handy MaxLogic";
/// Bundle identifier; also the name of the non-portable data directory.
pub const IDENTIFIER: &str = "eu.maxlogic.handy";
/// Identifier of upstream Handy, the source of the one-time data import.
pub const UPSTREAM_IDENTIFIER: &str = "com.pais.handy";

/// The fork build never checks for or installs updates. This forces the same
/// runtime lock that `HANDY_DISABLE_UPDATER` applies, and the updater plugin
/// is not registered at all.
pub const UPDATER_DISABLED: bool = true;

/// Tray tooltip and tray version line.
pub fn version_label(version: &str, dev: bool) -> String {
    if dev {
        format!("{PRODUCT_NAME} v{version} (Dev)")
    } else {
        format!("{PRODUCT_NAME} v{version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn json(source: &str) -> Value {
        serde_json::from_str(source).expect("valid JSON")
    }

    fn tauri_config() -> Value {
        json(include_str!("../tauri.conf.json"))
    }

    #[test]
    fn tauri_config_uses_fork_name_and_identifier() {
        let config = tauri_config();
        assert_eq!(config["productName"], PRODUCT_NAME);
        assert_eq!(config["identifier"], IDENTIFIER);
        assert_ne!(IDENTIFIER, UPSTREAM_IDENTIFIER);
    }

    #[test]
    fn tauri_config_has_no_updater_endpoints_or_artifacts() {
        let config = tauri_config();
        assert!(
            config["plugins"].get("updater").is_none(),
            "updater plugin config (endpoints/pubkey) must be absent"
        );
        assert_eq!(config["bundle"]["createUpdaterArtifacts"], false);
    }

    #[test]
    fn tauri_config_builds_portable_executable_only() {
        // No NSIS/MSI installer: `tauri build` produces the executable that
        // scripts/package-portable.ps1 assembles into a portable folder.
        assert_eq!(tauri_config()["bundle"]["active"], false);
    }

    #[test]
    fn capabilities_grant_no_updater_permissions() {
        for (name, source) in [
            ("default", include_str!("../capabilities/default.json")),
            ("desktop", include_str!("../capabilities/desktop.json")),
        ] {
            let permissions = json(source)["permissions"].clone();
            let granted = permissions.as_array().expect("permissions array");
            assert!(
                !granted
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|permission| permission.starts_with("updater:")),
                "{name} capability grants an updater permission"
            );
        }
    }

    #[test]
    fn updater_plugin_is_not_built_or_registered() {
        let lib = include_str!("lib.rs");
        assert!(
            !lib.contains(concat!("tauri_plugin_", "updater")),
            "lib.rs must not register the updater plugin"
        );
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.contains(concat!("tauri-plugin-", "updater")),
            "Cargo.toml must not depend on the updater plugin"
        );
    }

    #[test]
    fn update_checks_are_locked_off_regardless_of_stored_preference() {
        assert!(crate::settings::update_checks_forced_disabled());

        let mut settings = crate::settings::get_default_settings();
        settings.update_checks_enabled = true;
        assert!(!crate::settings::update_checks_effectively_enabled(
            &settings
        ));
    }

    #[test]
    fn version_label_names_the_fork() {
        assert_eq!(version_label("0.9.8", false), "Handy MaxLogic v0.9.8");
        assert_eq!(version_label("0.9.8", true), "Handy MaxLogic v0.9.8 (Dev)");
    }

    #[test]
    fn portable_marker_magic_is_unchanged_for_the_fork() {
        // The package script writes this exact content; portable.rs accepts it.
        let source = include_str!("portable.rs");
        assert!(source.contains("\"Handy Portable Mode\""));
        let script = include_str!("../../scripts/package-portable.ps1");
        assert!(script.contains("Handy Portable Mode"));
    }
}
