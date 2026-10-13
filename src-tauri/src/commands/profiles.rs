//! Processing profile commands: thin wrappers over `settings::profiles`.
//! Errors are stable codes the UI maps to translated messages.
use crate::settings::{
    self, AppSettings, EnglishCleanupSettings, ProcessingProfile, ProcessingProfileInput,
    ShortcutBinding,
};
use crate::shortcut;
use log::{debug, warn};
use tauri::AppHandle;

#[tauri::command]
#[specta::specta]
pub fn add_processing_profile(
    app: AppHandle,
    input: ProcessingProfileInput,
) -> Result<ProcessingProfile, String> {
    let mut settings = settings::get_settings(&app);
    let profile =
        settings::add_profile(&mut settings, input, chrono::Utc::now().timestamp_millis())?;
    settings::write_settings(&app, settings);
    Ok(profile)
}

/// Kind is immutable. A built-in writes its legacy prompt selector in the same
/// change. Registration is not touched.
#[tauri::command]
#[specta::specta]
pub fn update_processing_profile(
    app: AppHandle,
    id: String,
    input: ProcessingProfileInput,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings::update_profile(&mut settings, &id, input)?;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Custom profiles only; built-ins return `builtin_profile`.
#[tauri::command]
#[specta::specta]
pub fn delete_processing_profile(app: AppHandle, id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let binding = profile_binding_to_unregister(&settings, &id)?;
    unregister(&app, binding);
    settings::delete_profile(&mut settings, &id)?;
    settings::write_settings(&app, settings);
    crate::secure_input::reconcile_fallback(&app);
    Ok(())
}

/// Custom profiles only; built-in hotkeys can be changed or reset, not removed.
#[tauri::command]
#[specta::specta]
pub fn clear_profile_hotkey(app: AppHandle, id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let binding = profile_binding_to_unregister(&settings, &id)?;
    unregister(&app, binding);
    settings::clear_profile_hotkey(&mut settings, &id)?;
    settings::write_settings(&app, settings);
    crate::secure_input::reconcile_fallback(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_english_cleanup_settings(
    app: AppHandle,
    value: EnglishCleanupSettings,
) -> Result<(), String> {
    value.validate()?;
    let mut settings = settings::get_settings(&app);
    settings.english_cleanup = EnglishCleanupSettings {
        enabled: value.enabled,
        base_url: value.base_url.trim().to_string(),
        model: value.model.trim().to_string(),
    };
    settings::write_settings(&app, settings);
    Ok(())
}

/// The custom profile binding that may be unregistered. A binding that
/// registration skipped or never registered (a selection command off Windows)
/// is left alone: Tauri unregisters by accelerator and would remove another
/// owner.
fn profile_binding_to_unregister(
    settings: &AppSettings,
    id: &str,
) -> Result<Option<ShortcutBinding>, String> {
    let Some(binding) = settings::custom_profile_binding(settings, id)? else {
        return Ok(None);
    };
    if shortcut::registration_unused(settings, &binding.id) {
        debug!(
            "Profile binding '{}' is not registered; not unregistering",
            binding.id
        );
        return Ok(None);
    }
    Ok(Some(binding))
}

fn unregister(app: &AppHandle, binding: Option<ShortcutBinding>) {
    if let Some(binding) = binding {
        let id = binding.id.clone();
        if let Err(e) = shortcut::unregister_shortcut(app, binding) {
            warn!("Failed to unregister profile shortcut '{id}': {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ProcessingProfileInput, ProfileKind};

    fn settings_with_profile(accelerator: &str) -> (AppSettings, ProcessingProfile) {
        settings_with_profile_kind(accelerator, ProfileKind::Dictation)
    }

    fn settings_with_profile_kind(
        accelerator: &str,
        kind: ProfileKind,
    ) -> (AppSettings, ProcessingProfile) {
        let mut settings = settings::get_default_settings();
        settings.keyboard_implementation = settings::KeyboardImplementation::Tauri;
        let profile = settings::add_profile(
            &mut settings,
            ProcessingProfileInput {
                name: "Profile".into(),
                kind,
                prompt_id: Some("default_improve_transcriptions".into()),
                model: None,
                speech_language: None,
                english_cleanup: None,
            },
            1,
        )
        .unwrap();
        settings.bindings.insert(
            profile.binding_id(),
            ShortcutBinding {
                id: profile.binding_id(),
                name: "Profile".into(),
                description: String::new(),
                default_binding: String::new(),
                current_binding: accelerator.into(),
            },
        );
        (settings, profile)
    }

    #[test]
    fn handy008_selection_command_binding_is_unregistered_only_where_registered() {
        let (settings, profile) =
            settings_with_profile_kind("ctrl+f9", ProfileKind::SelectionCommand);
        let binding = profile_binding_to_unregister(&settings, &profile.id).unwrap();
        // Off Windows registration never uses a selection command hotkey.
        assert_eq!(binding.is_some(), cfg!(target_os = "windows"));
    }

    #[test]
    fn handy008_delete_never_unregisters_a_skipped_profile_binding() {
        // Ctrl+Space belongs to Transcribe; the profile binding was skipped.
        let (settings, profile) = settings_with_profile("ctrl+space");
        assert!(profile_binding_to_unregister(&settings, &profile.id)
            .unwrap()
            .is_none());
        let (settings, profile) = settings_with_profile("ctrl+f9");
        assert_eq!(
            profile_binding_to_unregister(&settings, &profile.id)
                .unwrap()
                .map(|b| b.current_binding),
            Some("ctrl+f9".to_string())
        );
        for id in [
            settings::BUILTIN_CLEANUP_PROFILE_ID,
            settings::BUILTIN_ASSISTANT_PROFILE_ID,
        ] {
            assert_eq!(
                profile_binding_to_unregister(&settings, id).unwrap_err(),
                "builtin_profile"
            );
        }
    }
}
