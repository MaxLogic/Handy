//! Tauri global-shortcut implementation
//!
//! This module provides shortcut functionality using Tauri's built-in
//! global-shortcut plugin.

use log::{debug, error, warn};
use tauri::AppHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::settings::{self, ShortcutBinding};

use super::handler::handle_shortcut_event;

/// Initialize shortcuts using Tauri's global-shortcut plugin
pub(crate) trait InitContext {
    fn settings(&self) -> settings::AppSettings;
    fn register(&self, binding: ShortcutBinding) -> Result<(), String>;
}
impl InitContext for AppHandle {
    fn settings(&self) -> settings::AppSettings {
        settings::load_or_create_app_settings(self)
    }
    fn register(&self, binding: ShortcutBinding) -> Result<(), String> {
        register_shortcut(self, binding)
    }
}
pub(crate) fn init_shortcuts(app: &impl InitContext) {
    let user_settings = app.settings();
    for binding in super::configured_bindings(&user_settings) {
        let id = binding.id.clone();
        if let Err(e) = app.register(binding) {
            error!("Failed to register shortcut {} during init: {}", id, e);
        }
    }
}

/// Validate a shortcut string for the Tauri global-shortcut implementation.
/// Tauri requires at least one non-modifier key and doesn't support the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }

    let modifiers = [
        "ctrl", "control", "shift", "alt", "option", "meta", "command", "cmd", "super", "win",
        "windows",
    ];

    // Check for fn key which Tauri doesn't support
    let parts: Vec<String> = raw.split('+').map(|p| p.trim().to_lowercase()).collect();
    for part in &parts {
        if part == "fn" || part == "function" {
            return Err("The 'fn' key is not supported by Tauri global shortcuts".into());
        }
    }

    // Check for at least one non-modifier key
    let has_non_modifier = parts.iter().any(|part| !modifiers.contains(&part.as_str()));

    if !has_non_modifier {
        return Err("Tauri shortcuts must include a main key (letter, number, F-key, etc.) in addition to modifiers".into());
    }

    // The name check above passes side-specific modifiers such as
    // `option_left`, which the handy-keys recorder saves but the accelerator
    // parser rejects. Parse here so a binding carried over from handy-keys is
    // reset to the default instead of failing to register.
    raw.parse::<Shortcut>()
        .map(|_| ())
        .map_err(|e| format!("Failed to parse shortcut '{}': {}", raw, e))
}

/// Register a shortcut using Tauri's global-shortcut plugin
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    // Validate for Tauri requirements
    if let Err(e) = validate_shortcut(&binding.current_binding) {
        warn!(
            "register_tauri_shortcut validation error for binding '{}': {}",
            binding.current_binding, e
        );
        return Err(e);
    }

    // Parse shortcut and return error if it fails
    let shortcut = match binding.current_binding.parse::<Shortcut>() {
        Ok(s) => s,
        Err(e) => {
            let error_msg = format!(
                "Failed to parse shortcut '{}': {}",
                binding.current_binding, e
            );
            error!("register_tauri_shortcut parse error: {}", error_msg);
            return Err(error_msg);
        }
    };

    // Prevent duplicate registrations that would silently shadow one another
    if app.global_shortcut().is_registered(shortcut) {
        let error_msg = format!("Shortcut '{}' is already in use", binding.current_binding);
        warn!("register_tauri_shortcut duplicate error: {}", error_msg);
        return Err(error_msg);
    }

    // Clone binding.id for use in the closure
    let binding_id_for_closure = binding.id.clone();

    app.global_shortcut()
        .on_shortcut(shortcut, move |app_handle, scut, event| {
            if scut == &shortcut {
                let shortcut_string = scut.into_string();
                let is_pressed = event.state == ShortcutState::Pressed;
                // Mirrors the handy-keys event log line; the distinct prefix
                // makes it possible to tell which backend fired a shortcut
                // (e.g. when diagnosing the Secure Input fallback)
                debug!(
                    "tauri global-shortcut event: binding={}, shortcut={}, state={:?}",
                    binding_id_for_closure, shortcut_string, event.state
                );
                handle_shortcut_event(
                    app_handle,
                    &binding_id_for_closure,
                    &shortcut_string,
                    is_pressed,
                );
            }
        })
        .map_err(|e| {
            let error_msg = format!(
                "Couldn't register shortcut '{}': {}",
                binding.current_binding, e
            );
            error!("register_tauri_shortcut registration error: {}", error_msg);
            error_msg
        })?;

    Ok(())
}

/// Unregister a shortcut from Tauri's global-shortcut plugin
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let shortcut = match binding.current_binding.parse::<Shortcut>() {
        Ok(s) => s,
        Err(e) => {
            let error_msg = format!(
                "Failed to parse shortcut '{}' for unregistration: {}",
                binding.current_binding, e
            );
            error!("unregister_tauri_shortcut parse error: {}", error_msg);
            return Err(error_msg);
        }
    };

    app.global_shortcut().unregister(shortcut).map_err(|e| {
        let error_msg = format!(
            "Failed to unregister shortcut '{}': {}",
            binding.current_binding, e
        );
        error!("unregister_tauri_shortcut error: {}", error_msg);
        error_msg
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_shortcut;

    #[test]
    fn rejects_side_specific_modifiers_the_parser_cannot_register() {
        for raw in ["option_left+space", "ctrl_right+space"] {
            assert!(validate_shortcut(raw).is_err(), "{raw} should be rejected");
        }
    }

    #[test]
    fn accepts_the_default_shortcuts() {
        for raw in [
            "option+space",
            "option+shift+space",
            "ctrl+space",
            "ctrl+shift+space",
            "alt+space",
            "escape",
        ] {
            assert_eq!(validate_shortcut(raw), Ok(()), "{raw} should be accepted");
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod assistant_registration_tests {
    use crate::settings;
    struct FakeInit {
        settings: settings::AppSettings,
        registered: std::sync::Mutex<Vec<settings::ShortcutBinding>>,
    }
    fn initialized(settings: &settings::AppSettings) -> Vec<settings::ShortcutBinding> {
        let context = FakeInit {
            settings: settings.clone(),
            registered: std::sync::Mutex::new(Vec::new()),
        };
        super::init_shortcuts(&context);
        context.registered.into_inner().unwrap()
    }
    impl super::InitContext for FakeInit {
        fn settings(&self) -> settings::AppSettings {
            self.settings.clone()
        }
        fn register(&self, binding: settings::ShortcutBinding) -> Result<(), String> {
            super::validate_shortcut(&binding.current_binding)?;
            self.registered.lock().unwrap().push(binding);
            Ok(())
        }
    }

    #[test]
    fn assistant_tauri_impl_registration_order_keeps_raw_and_cleanup_first() {
        let mut settings = settings::get_default_settings();
        settings.keyboard_implementation = settings::KeyboardImplementation::Tauri;
        settings.post_process_enabled = true;
        let bindings = initialized(&settings);
        assert_eq!(
            bindings.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["transcribe", "transcribe_with_post_process", "assistant"]
        );
        for binding in bindings {
            assert!(super::validate_shortcut(&binding.current_binding).is_ok());
        }
        settings.post_process_enabled = false;
        assert_eq!(
            initialized(&settings)
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            ["transcribe", "assistant"]
        );
    }

    #[test]
    fn assistant_tauri_impl_conflict_preserves_existing_binding_and_missing_defaults() {
        let mut settings = settings::get_default_settings();
        settings.keyboard_implementation = settings::KeyboardImplementation::Tauri;
        settings.post_process_enabled = true;
        settings
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "ctrl+alt+space".into();
        let before = serde_json::to_value(&settings.bindings).unwrap();
        assert_eq!(
            initialized(&settings)
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            ["transcribe", "transcribe_with_post_process"]
        );
        assert_eq!(serde_json::to_value(&settings.bindings).unwrap(), before);
        settings.bindings.remove("assistant");
        assert_eq!(initialized(&settings).len(), 2);
        settings
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "ctrl+f8".into();
        assert_eq!(initialized(&settings).last().unwrap().id, "assistant");
        settings
            .bindings
            .get_mut("transcribe_with_post_process")
            .unwrap()
            .current_binding = "ctrl+alt+space".into();
        settings.post_process_enabled = false;
        assert_eq!(
            initialized(&settings)
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            ["transcribe"]
        );
    }
}
