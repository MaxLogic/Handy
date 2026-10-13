//! Keyboard shortcut management module
//!
//! This module provides a unified interface for keyboard shortcuts with
//! multiple backend implementations:
//!
//! - `tauri`: Uses Tauri's built-in global-shortcut plugin
//! - `handy_keys`: Uses the handy-keys library for more control
//!
//! The active implementation is determined by the `keyboard_implementation`
//! setting and can be changed at runtime.

mod handler;
pub mod handy_keys;
pub mod tauri_impl;

use log::{debug, error, info, warn};
use serde::Serialize;
use specta::Type;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::settings::APPLE_INTELLIGENCE_DEFAULT_MODEL_ID;
use crate::settings::{
    self, get_settings, AutoSubmitKey, ChineseScript, ClipboardHandling, KeyboardImplementation,
    LLMPrompt, OverlayPosition, OverlayStyle, PasteMethod, ShortcutActivation, ShortcutBinding,
    SoundTheme, Theme, TypingTool, VadBackend, APPLE_INTELLIGENCE_PROVIDER_ID,
};
use crate::tray;

// Note: Commands are accessed via shortcut::handy_keys:: in lib.rs

/// Whether two accelerators can match the same key press. HandyKeys generic
/// modifiers overlap either side; Tauri uses accelerator identity.
pub(crate) fn bindings_overlap(a: &str, b: &str, implementation: KeyboardImplementation) -> bool {
    match implementation {
        KeyboardImplementation::Tauri => match (
            a.parse::<tauri_plugin_global_shortcut::Shortcut>(),
            b.parse::<tauri_plugin_global_shortcut::Shortcut>(),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        },
        KeyboardImplementation::HandyKeys => match (
            a.parse::<::handy_keys::Hotkey>(),
            b.parse::<::handy_keys::Hotkey>(),
        ) {
            (Ok(a), Ok(b)) => handy_keys_bindings_overlap(a, b),
            _ => false,
        },
    }
}

/// Compare backend key identities, including inactive and cancel bindings.
pub(crate) fn binding_conflict(
    bindings: &std::collections::HashMap<String, ShortcutBinding>,
    id: &str,
    candidate: &str,
    implementation: KeyboardImplementation,
) -> Option<String> {
    let mut ids: Vec<_> = bindings
        .keys()
        .filter(|other| other.as_str() != id)
        .collect();
    ids.sort();
    ids.into_iter()
        .find(|other| {
            bindings_overlap(candidate, &bindings[*other].current_binding, implementation)
        })
        .cloned()
}

/// The user-facing owner of a conflicting binding: its name, so a profile
/// conflict is announced by profile name.
fn conflict_owner_name(
    bindings: &std::collections::HashMap<String, ShortcutBinding>,
    owner: &str,
) -> String {
    bindings
        .get(owner)
        .map(|binding| binding.name.trim())
        .filter(|name| !name.is_empty())
        .unwrap_or(owner)
        .to_string()
}

fn handy_keys_bindings_overlap(a: ::handy_keys::Hotkey, b: ::handy_keys::Hotkey) -> bool {
    use ::handy_keys::Modifiers;
    // Each generic alias is the union of its left/right bits. Intersection
    // therefore means both bindings can match at least one common side.
    a.key == b.key
        && a.modifiers.contains(Modifiers::FN) == b.modifiers.contains(Modifiers::FN)
        && [
            Modifiers::CTRL,
            Modifiers::OPT,
            Modifiers::SHIFT,
            Modifiers::CMD,
        ]
        .into_iter()
        .all(|group| {
            let a = a.modifiers & group;
            let b = b.modifiers & group;
            (a.is_empty() && b.is_empty()) || a.intersects(b)
        })
}

pub(crate) fn ordered_binding_ids(
    bindings: &std::collections::HashMap<String, ShortcutBinding>,
) -> Vec<String> {
    let mut ids: Vec<_> = bindings.keys().cloned().collect();
    ids.sort_by_key(|id| {
        (
            match id.as_str() {
                "transcribe" => 0,
                "transcribe_with_post_process" => 1,
                "assistant" => 3,
                _ => 2,
            },
            id.clone(),
        )
    });
    ids
}

/// Defaults and user overrides form one conflict domain, even after migration.
fn effective_bindings(
    settings: &settings::AppSettings,
) -> std::collections::HashMap<String, ShortcutBinding> {
    let mut bindings = settings::get_default_settings().bindings;
    bindings.extend(settings.bindings.clone());
    bindings
}

/// Every binding in registration rank: the default ids (transcribe, cleanup,
/// the rest, then assistant), then the bindings of existing custom profiles in
/// profile order. Selection-command profiles are Windows-only. Earlier entries
/// win accelerator conflicts.
pub(crate) fn registration_order(settings: &settings::AppSettings) -> Vec<ShortcutBinding> {
    let bindings = effective_bindings(settings);
    let mut order: Vec<ShortcutBinding> =
        ordered_binding_ids(&settings::get_default_settings().bindings)
            .into_iter()
            .map(|id| bindings[&id].clone())
            .collect();
    for profile in settings
        .processing_profiles
        .iter()
        .filter(|p| !p.is_builtin())
    {
        if profile.kind == settings::ProfileKind::SelectionCommand && !cfg!(target_os = "windows") {
            continue;
        }
        if let Some(binding) = settings.bindings.get(&profile.binding_id()) {
            order.push(binding.clone());
        }
    }
    order
}

/// The earlier binding that keeps the accelerator when `order[index]` would
/// collide with it, including cancel and a disabled cleanup binding. Only the
/// assistant and custom profile bindings can be skipped, so the user's older
/// binding always wins.
pub(crate) fn registration_skip_owner(
    order: &[ShortcutBinding],
    index: usize,
    implementation: KeyboardImplementation,
) -> Option<String> {
    let binding = &order[index];
    if binding.id != "assistant" && settings::parse_profile_binding_id(&binding.id).is_none() {
        return None;
    }
    order[..index]
        .iter()
        .find(|earlier| {
            bindings_overlap(
                &binding.current_binding,
                &earlier.current_binding,
                implementation,
            )
        })
        .map(|earlier| earlier.id.clone())
}

/// Whether registration skips this binding, and for which owner. A skipped
/// binding must never be unregistered or restored: Tauri unregisters by
/// accelerator and would remove the other owner.
pub(crate) fn registration_skipped(settings: &settings::AppSettings, id: &str) -> Option<String> {
    let order = registration_order(settings);
    let index = order.iter().position(|binding| binding.id == id)?;
    registration_skip_owner(&order, index, settings.keyboard_implementation)
}

/// Whether registration leaves this binding unregistered: skipped for an
/// earlier owner, or not in the registration order at all (an orphan, or a
/// selection command off Windows). Such a binding must never be unregistered
/// or restored: Tauri unregisters by accelerator and would remove another
/// owner.
pub(crate) fn registration_unused(settings: &settings::AppSettings, id: &str) -> bool {
    let order = registration_order(settings);
    match order.iter().position(|binding| binding.id == id) {
        Some(index) => {
            registration_skip_owner(&order, index, settings.keyboard_implementation).is_some()
        }
        None => true,
    }
}

/// Bindings the macOS Secure Input fallback must not shadow-register: skipped
/// bindings, and profile bindings registration would not use (orphans, or
/// selection commands off Windows).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn fallback_excluded(settings: &settings::AppSettings, id: &str) -> bool {
    if registration_skipped(settings, id).is_some() {
        return true;
    }
    settings::parse_profile_binding_id(id).is_some()
        && !registration_order(settings)
            .iter()
            .any(|binding| binding.id == id)
}

pub(super) fn configured_bindings(settings: &settings::AppSettings) -> Vec<ShortcutBinding> {
    let order = registration_order(settings);
    (0..order.len())
        .filter_map(|index| {
            let binding = &order[index];
            if binding.id == "cancel"
                || (binding.id == "transcribe_with_post_process" && !settings.post_process_enabled)
            {
                return None;
            }
            if let Some(owner) =
                registration_skip_owner(&order, index, settings.keyboard_implementation)
            {
                warn!(
                    "Shortcut '{}' conflicts with '{owner}'; leaving the existing binding registered",
                    binding.id
                );
                return None;
            }
            Some(binding.clone())
        })
        .collect()
}

/// The stored binding to change. `Ok(None)` means a new hotkey for an
/// existing custom profile.
pub(crate) fn binding_to_modify(
    settings: &settings::AppSettings,
    id: &str,
) -> Result<Option<ShortcutBinding>, String> {
    if let Some(binding) = settings.bindings.get(id) {
        return Ok(Some(binding.clone()));
    }
    if settings::parse_profile_binding_id(id).is_some() {
        return match settings::custom_profile_for_binding(settings, id) {
            Some(_) => Ok(None),
            None => Err("Profile not found".into()),
        };
    }
    match settings::get_default_settings().bindings.get(id) {
        Some(default_binding) => {
            warn!(
                "Binding '{}' not found in settings, creating from defaults",
                id
            );
            Ok(Some(default_binding.clone()))
        }
        None => Err(format!("Binding with id '{}' not found in defaults", id)),
    }
}

/// A custom profile's new hotkey entry, named after the profile.
fn new_profile_binding(
    settings: &settings::AppSettings,
    id: &str,
    accelerator: &str,
) -> Option<ShortcutBinding> {
    let profile = settings::custom_profile_for_binding(settings, id)?;
    Some(ShortcutBinding {
        id: id.into(),
        name: profile.name.clone(),
        description: String::new(),
        default_binding: String::new(),
        current_binding: accelerator.into(),
    })
}

/// Initialize shortcuts using the configured implementation
pub fn init_shortcuts(app: &AppHandle) {
    let user_settings = settings::load_or_create_app_settings(app);

    // Check which implementation to use
    match user_settings.keyboard_implementation {
        KeyboardImplementation::Tauri => {
            tauri_impl::init_shortcuts(app);
        }
        KeyboardImplementation::HandyKeys => {
            if let Err(e) = handy_keys::init_shortcuts(app) {
                error!("Failed to initialize handy-keys shortcuts: {}", e);
                // Fall back to Tauri implementation and persist this fallback
                warn!("Falling back to Tauri global shortcut implementation and saving fallback to settings");

                // Update settings to persist the fallback so we don't retry HandyKeys on next launch
                let mut settings = settings::get_settings(app);
                settings.keyboard_implementation = KeyboardImplementation::Tauri;
                settings::write_settings(app, settings);

                tauri_impl::init_shortcuts(app);
            }
        }
    }
}

/// Whether the recording lifecycle currently wants the cancel shortcut.
/// Written synchronously by start/stop, so it always holds the latest request.
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Native edges of cancel registration, substituted by deterministic tests.
pub(crate) trait CancelShortcutContext {
    fn register_cancel_fallback(&self);
    fn unregister_cancel_fallback(&self);
    fn schedule_cancel_reconcile(&self);
}

impl CancelShortcutContext for AppHandle {
    fn register_cancel_fallback(&self) {
        crate::secure_input::register_cancel_fallback(self);
    }
    fn unregister_cancel_fallback(&self) {
        crate::secure_input::unregister_cancel_fallback(self);
    }
    fn schedule_cancel_reconcile(&self) {
        schedule_cancel_reconcile(self);
    }
}

/// Whether the cancel shortcut is actually registered with the backend.
/// The lock also serializes reconciliation passes.
#[cfg(not(target_os = "linux"))]
static CANCEL_REGISTERED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Register the cancel shortcut (called when recording starts)
pub(crate) fn register_cancel_shortcut(app: &impl CancelShortcutContext) {
    // Track recording lifecycle independently of the current implementation so
    // switching implementations mid-recording cannot leave stale fallback state.
    app.register_cancel_fallback();

    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
    app.schedule_cancel_reconcile();
}

/// Unregister the cancel shortcut (called when recording stops)
pub(crate) fn unregister_cancel_shortcut(app: &impl CancelShortcutContext) {
    // Explicit cancellation still has to await strict commit/abort settlement.
    // CancelArm clears assistant ownership before its final unregister.
    if crate::assistant::cancel_armed() {
        return;
    }
    app.unregister_cancel_fallback();

    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    app.schedule_cancel_reconcile();
}

#[cfg(test)]
pub(crate) fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::SeqCst)
}

/// Apply the requested cancel state off the calling thread. Start and stop run
/// on the shortcut handler thread, which must not block on registration, so the
/// work is spawned. Spawned tasks may run in any order (a very short recording
/// can see its unregister task run before its register task), so each pass
/// applies the latest request instead of the action that scheduled it.
fn schedule_cancel_reconcile(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        reconcile_cancel_shortcut(&app);
    });
}

fn reconcile_cancel_shortcut(app: &AppHandle) {
    // Cancel shortcut is disabled on Linux due to instability with dynamic shortcut registration
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut registered = CANCEL_REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
        let requested = CANCEL_REQUESTED.load(Ordering::SeqCst);
        if requested == *registered {
            return;
        }

        let Some(cancel_binding) = get_settings(app).bindings.get("cancel").cloned() else {
            return;
        };

        if requested {
            match register_shortcut(app, cancel_binding) {
                Ok(()) => *registered = true,
                Err(e) => error!("Failed to register cancel shortcut: {}", e),
            }
        } else {
            match unregister_shortcut(app, cancel_binding) {
                Ok(()) => *registered = false,
                Err(e) => error!("Failed to unregister cancel shortcut: {}", e),
            }
        }
    }
}

/// Register a shortcut using the appropriate implementation
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let settings = get_settings(app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => tauri_impl::register_shortcut(app, binding),
        KeyboardImplementation::HandyKeys => handy_keys::register_shortcut(app, binding),
    }
}

/// Unregister a shortcut using the appropriate implementation
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let settings = get_settings(app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => tauri_impl::unregister_shortcut(app, binding),
        KeyboardImplementation::HandyKeys => handy_keys::unregister_shortcut(app, binding),
    }
}

// ============================================================================
// Binding Management Commands
// ============================================================================

#[derive(Serialize, Type)]
pub struct BindingResponse {
    success: bool,
    binding: Option<ShortcutBinding>,
    error: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub fn change_binding(
    app: AppHandle,
    id: String,
    binding: String,
) -> Result<BindingResponse, String> {
    // Reject empty bindings — every shortcut should have a value
    if binding.trim().is_empty() {
        return Err("Binding cannot be empty".to_string());
    }

    let mut settings = settings::get_settings(&app);

    validate_binding_input(&id, &binding, settings.keyboard_implementation)?;

    let bindings = effective_bindings(&settings);
    if let Some(other) =
        binding_conflict(&bindings, &id, &binding, settings.keyboard_implementation)
    {
        let owner = conflict_owner_name(&bindings, &other);
        return Ok(BindingResponse {
            success: false,
            binding: None,
            error: Some(format!("Shortcut '{binding}' is already used by '{owner}'")),
        });
    }

    // The stored binding, a default one, or none yet for a profile hotkey.
    let previous = match binding_to_modify(&settings, &id) {
        Ok(previous) => previous,
        Err(error_msg) => {
            warn!("change_binding error: {}", error_msg);
            return Ok(BindingResponse {
                success: false,
                binding: None,
                error: Some(error_msg),
            });
        }
    };

    // If this is the cancel binding, just update the settings and return
    // It's managed dynamically, so we don't register/unregister here
    if id == "cancel" {
        if let Some(mut b) = settings.bindings.get(&id).cloned() {
            b.current_binding = binding;
            settings.bindings.insert(id.clone(), b.clone());
            settings::write_settings(&app, settings);
            crate::secure_input::reconcile_fallback(&app);
            return Ok(BindingResponse {
                success: true,
                binding: Some(b.clone()),
                error: None,
            });
        }
    }

    // Unregister the existing binding. A new profile hotkey has none, and its
    // empty value must never reach backend parsing.
    if let Some(previous) = &previous {
        if let Err(e) = unregister_previous_binding(&settings, previous, |binding| {
            unregister_shortcut(&app, binding)
        }) {
            let error_msg = format!("Failed to unregister shortcut: {}", e);
            error!("change_binding error: {}", error_msg);
        }
    }

    // Validate the new shortcut for the current keyboard implementation
    if let Err(e) = validate_shortcut_for_implementation(&binding, settings.keyboard_implementation)
    {
        warn!("change_binding validation error: {}", e);
        if let Some(previous) = &previous {
            restore_registration(&app, &settings, previous);
        }
        return Err(e);
    }

    // Create an updated binding
    let mut updated_binding = match &previous {
        Some(previous) => previous.clone(),
        None => new_profile_binding(&settings, &id, "").ok_or("Profile not found")?,
    };
    updated_binding.current_binding = binding;

    // Register the new binding
    if let Err(e) = register_shortcut(&app, updated_binding.clone()) {
        let error_msg = format!("Failed to register shortcut: {}", e);
        error!("change_binding error: {}", error_msg);
        if let Some(previous) = &previous {
            restore_registration(&app, &settings, previous);
        }
        return Ok(BindingResponse {
            success: false,
            binding: None,
            error: Some(error_msg),
        });
    }

    // Update the binding in the settings
    settings.bindings.insert(id, updated_binding.clone());

    // Save the settings and synchronize any active Secure Input shadows.
    settings::write_settings(&app, settings);
    crate::secure_input::reconcile_fallback(&app);

    // Return the updated binding
    Ok(BindingResponse {
        success: true,
        binding: Some(updated_binding),
        error: None,
    })
}

/// Remove only a binding that startup could have registered.
fn unregister_previous_binding(
    settings: &settings::AppSettings,
    binding: &ShortcutBinding,
    unregister: impl FnOnce(ShortcutBinding) -> Result<(), String>,
) -> Result<(), String> {
    // A conflicting assistant or profile was skipped during startup, or the
    // binding was never registered. Tauri unregisters by accelerator, so
    // removing it would remove another owner.
    if registration_unused(settings, &binding.id) {
        return Ok(());
    }
    unregister(binding.clone())
}

/// Best-effort re-register of the previous binding after a failed change,
/// so a failure leaves the user's shortcut working exactly as before.
fn restore_registration(
    app: &AppHandle,
    settings: &settings::AppSettings,
    binding: &ShortcutBinding,
) {
    if let Err(e) =
        restore_previous_binding(settings, binding, |binding| register_shortcut(app, binding))
    {
        error!(
            "Failed to restore previous binding '{}' ({}): {}",
            binding.id, binding.current_binding, e
        );
    }
}

fn restore_previous_binding(
    settings: &settings::AppSettings,
    binding: &ShortcutBinding,
    register: impl FnOnce(ShortcutBinding) -> Result<(), String>,
) -> Result<(), String> {
    if registration_unused(settings, &binding.id) {
        return Ok(());
    }
    register(binding.clone())
}

#[tauri::command]
#[specta::specta]
pub fn reset_binding(app: AppHandle, id: String) -> Result<BindingResponse, String> {
    let default_binding = reset_target(&settings::get_settings(&app), &id)?;
    change_binding(app, id, default_binding)
}

/// Built-in bindings return to their default; profile hotkeys have none.
fn reset_target(settings: &settings::AppSettings, id: &str) -> Result<String, String> {
    if settings::parse_profile_binding_id(id).is_some() {
        return Err("Profile hotkeys have no default".into());
    }
    Ok(settings::get_stored_binding(settings, id)?.default_binding)
}

/// Unregister every binding while the user is recording a new shortcut in
/// the UI, so no existing shortcut can fire — or swallow the keystrokes —
/// mid-capture. The "cancel" binding is untouched: it is managed dynamically
/// by the recording lifecycle.
pub fn suspend_all_shortcuts(app: &AppHandle) {
    for (id, binding) in settings::get_bindings(app) {
        if id == "cancel" {
            continue;
        }
        if let Err(e) = unregister_shortcut(app, binding) {
            debug!(
                "suspend_all_shortcuts: could not unregister '{}': {}",
                id, e
            );
        }
    }
}

/// Re-register every binding from settings after shortcut recording ends.
/// Registering an already-registered shortcut fails cleanly in both
/// implementations, so this is idempotent and safe on every exit path.
pub fn resume_all_shortcuts(app: &AppHandle) {
    let settings = get_settings(app);
    for binding in configured_bindings(&settings) {
        let id = binding.id.clone();
        if let Err(e) = register_shortcut(app, binding) {
            debug!("resume_all_shortcuts: could not register '{}': {}", id, e);
        }
    }
}

/// Temporarily unregister all bindings while the user is recording a
/// shortcut in the UI. This avoids firing actions while keys are recorded.
#[tauri::command]
#[specta::specta]
pub fn suspend_all_bindings(app: AppHandle) -> Result<(), String> {
    suspend_all_shortcuts(&app);
    Ok(())
}

/// Re-register all bindings after the user has finished recording.
#[tauri::command]
#[specta::specta]
pub fn resume_all_bindings(app: AppHandle) -> Result<(), String> {
    resume_all_shortcuts(&app);
    Ok(())
}

// ============================================================================
// Keyboard Implementation Switching
// ============================================================================

/// Result of changing keyboard implementation
#[derive(Serialize, Type)]
pub struct ImplementationChangeResult {
    pub success: bool,
    /// List of binding IDs that were reset to defaults due to incompatibility
    pub reset_bindings: Vec<String>,
}

/// Change the keyboard implementation with runtime switching.
/// This will unregister all shortcuts from the old implementation,
/// validate shortcuts for the new implementation (resetting invalid ones to defaults),
/// and register them with the new implementation.
#[tauri::command]
#[specta::specta]
pub fn change_keyboard_implementation_setting(
    app: AppHandle,
    implementation: String,
) -> Result<ImplementationChangeResult, String> {
    let current_settings = settings::get_settings(&app);
    let current_impl = current_settings.keyboard_implementation;
    let new_impl = parse_keyboard_implementation(&implementation);

    // If same implementation, nothing to do
    if current_impl == new_impl {
        return Ok(ImplementationChangeResult {
            success: true,
            reset_bindings: vec![],
        });
    }

    info!(
        "Switching keyboard implementation from {:?} to {:?}",
        current_impl, new_impl
    );

    // Unregister all shortcuts from the current implementation
    unregister_all_shortcuts(&app, current_impl);

    // Update the setting
    let mut settings = settings::get_settings(&app);
    settings.keyboard_implementation = new_impl;
    settings::write_settings(&app, settings);

    // Carbon fallback registrations use the Tauri plugin. Remove them before
    // registering the full Tauri implementation to avoid duplicate conflicts.
    if new_impl == KeyboardImplementation::Tauri {
        crate::secure_input::reconcile_fallback(&app);
    }

    // Initialize new implementation if needed (HandyKeys needs state)
    if new_impl == KeyboardImplementation::HandyKeys && initialize_handy_keys_with_rollback(&app)? {
        // Shortcuts already registered during init.
        crate::secure_input::reconcile_fallback(&app);
        return Ok(ImplementationChangeResult {
            success: true,
            reset_bindings: vec![],
        });
    }

    // Register all shortcuts with new implementation, resetting invalid ones
    let reset_bindings = register_all_shortcuts_for_implementation(&app, new_impl);
    crate::secure_input::reconcile_fallback(&app);

    // Emit event to notify frontend of the change
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "keyboard_implementation",
            "value": implementation,
            "reset_bindings": reset_bindings
        }),
    );

    info!("Keyboard implementation switched to {:?}", new_impl);

    Ok(ImplementationChangeResult {
        success: true,
        reset_bindings,
    })
}

/// Get the current keyboard implementation
#[tauri::command]
#[specta::specta]
pub fn get_keyboard_implementation(app: AppHandle) -> String {
    let settings = settings::get_settings(&app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => "tauri".to_string(),
        KeyboardImplementation::HandyKeys => "handy_keys".to_string(),
    }
}

// ============================================================================
// Validation Helpers
// ============================================================================

/// Navigation keys are unsafe user shortcuts. Escape remains valid for the
/// separately managed built-in cancel action.
fn validate_binding_input(
    id: &str,
    raw: &str,
    implementation: KeyboardImplementation,
) -> Result<(), String> {
    let key = raw.trim().to_ascii_lowercase();
    if matches!(key.as_str(), "enter" | "return" | "escape" | "esc" | "tab")
        && !(id == "cancel" && matches!(key.as_str(), "escape" | "esc"))
    {
        return Err("Enter, Escape and Tab cannot be used alone as a shortcut".into());
    }
    validate_shortcut_for_implementation(raw, implementation)
}

/// Validate a shortcut for a specific implementation
fn validate_shortcut_for_implementation(
    raw: &str,
    implementation: KeyboardImplementation,
) -> Result<(), String> {
    match implementation {
        KeyboardImplementation::Tauri => tauri_impl::validate_shortcut(raw),
        KeyboardImplementation::HandyKeys => handy_keys::validate_shortcut(raw),
    }
}

/// Parse a keyboard implementation string into the enum
fn parse_keyboard_implementation(s: &str) -> KeyboardImplementation {
    match s {
        "tauri" => KeyboardImplementation::Tauri,
        "handy_keys" => KeyboardImplementation::HandyKeys,
        other => {
            warn!(
                "Invalid keyboard implementation '{}', defaulting to tauri",
                other
            );
            KeyboardImplementation::Tauri
        }
    }
}

/// Unregister all shortcuts for the current implementation
fn unregister_all_shortcuts(app: &AppHandle, implementation: KeyboardImplementation) {
    let bindings = settings::get_bindings(app);

    for (id, binding) in bindings {
        // Skip cancel shortcut as it's dynamically registered
        if id == "cancel" {
            continue;
        }

        let result = match implementation {
            KeyboardImplementation::Tauri => tauri_impl::unregister_shortcut(app, binding),
            KeyboardImplementation::HandyKeys => handy_keys::unregister_shortcut(app, binding),
        };

        if let Err(e) = result {
            warn!(
                "Failed to unregister shortcut '{}' during switch: {}",
                id, e
            );
        }
    }
}

/// Register all shortcuts for a specific implementation, validating and resetting invalid ones
fn register_all_shortcuts_for_implementation(
    app: &AppHandle,
    implementation: KeyboardImplementation,
) -> Vec<String> {
    let mut current_settings = settings::get_settings(app);
    let (bindings, reset_bindings) =
        bindings_for_implementation(&mut current_settings, implementation);
    for binding in bindings {
        let id = binding.id.clone();
        let result = match implementation {
            KeyboardImplementation::Tauri => tauri_impl::register_shortcut(app, binding),
            KeyboardImplementation::HandyKeys => handy_keys::register_shortcut(app, binding),
        };
        if let Err(e) = result {
            error!(
                "Failed to register shortcut '{}' for {:?}: {}",
                id, implementation, e
            );
        }
    }
    if !reset_bindings.is_empty() {
        settings::write_settings(app, current_settings);
    }
    reset_bindings
}

fn bindings_for_implementation(
    settings: &mut settings::AppSettings,
    implementation: KeyboardImplementation,
) -> (Vec<ShortcutBinding>, Vec<String>) {
    settings.keyboard_implementation = implementation;
    let defaults = settings::get_default_settings().bindings;
    let mut reset = Vec::new();
    for id in ordered_binding_ids(&defaults) {
        if id == "cancel"
            || (id == "transcribe_with_post_process" && !settings.post_process_enabled)
        {
            continue;
        }
        let mut binding = settings.bindings.get(&id).unwrap_or(&defaults[&id]).clone();
        // Stored legacy navigation bindings remain valid; only change_binding
        // enforces the new-input navigation restriction. Backend syntax still applies.
        if let Err(e) =
            validate_shortcut_for_implementation(&binding.current_binding, implementation)
        {
            info!(
                "Shortcut '{}' ({}) is invalid for {:?}: {}. Resetting to default.",
                id, binding.current_binding, implementation, e
            );
            binding.current_binding = defaults[&id].current_binding.clone();
            settings.bindings.insert(id.clone(), binding);
            reset.push(id);
        }
    }
    // Profile hotkeys have no default: an invalid one becomes unset.
    let profile_ids: Vec<String> = settings
        .processing_profiles
        .iter()
        .filter(|p| !p.is_builtin())
        .map(|p| p.binding_id())
        .collect();
    for id in profile_ids {
        let Some(binding) = settings.bindings.get(&id) else {
            continue;
        };
        if let Err(e) =
            validate_shortcut_for_implementation(&binding.current_binding, implementation)
        {
            info!(
                "Profile shortcut '{}' ({}) is invalid for {:?}: {}. Removing it.",
                id, binding.current_binding, implementation, e
            );
            settings.bindings.remove(&id);
            reset.push(id);
        }
    }
    (configured_bindings(settings), reset)
}

/// Initialize HandyKeys if not already initialized, with rollback on failure
fn initialize_handy_keys_with_rollback(app: &AppHandle) -> Result<bool, String> {
    if app.try_state::<handy_keys::HandyKeysState>().is_some() {
        return Ok(false); // Already initialized, caller should continue
    }

    if let Err(e) = handy_keys::init_shortcuts(app) {
        error!("Failed to initialize HandyKeys: {}", e);
        // Rollback to Tauri
        let mut settings = settings::get_settings(app);
        settings.keyboard_implementation = KeyboardImplementation::Tauri;
        settings::write_settings(app, settings);
        crate::secure_input::reconcile_fallback(app);
        tauri_impl::init_shortcuts(app);
        return Err(format!(
            "Failed to initialize HandyKeys: {}. Reverted to Tauri.",
            e
        ));
    }

    // init_shortcuts already registered shortcuts
    Ok(true)
}

// ============================================================================
// General Settings Commands
// ============================================================================

#[tauri::command]
#[specta::specta]
pub fn change_shortcut_activation_setting(
    app: AppHandle,
    activation: ShortcutActivation,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.shortcut_activation = activation;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_hold_threshold_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.hold_threshold_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_audio_feedback_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.audio_feedback = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_audio_feedback_volume_setting(app: AppHandle, volume: f32) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.audio_feedback_volume = volume;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_sound_theme_setting(app: AppHandle, theme: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match theme.as_str() {
        "marimba" => SoundTheme::Marimba,
        "pop" => SoundTheme::Pop,
        "custom" => SoundTheme::Custom,
        other => {
            warn!("Invalid sound theme '{}', defaulting to marimba", other);
            SoundTheme::Marimba
        }
    };
    settings.sound_theme = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_theme_setting(app: AppHandle, theme: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match theme.as_str() {
        "system" => Theme::System,
        "light" => Theme::Light,
        "dark" => Theme::Dark,
        other => {
            warn!("Invalid theme '{}', defaulting to system", other);
            Theme::System
        }
    };
    settings.theme = parsed;
    settings::write_settings(&app, settings);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    apply_window_theme(&app, parsed);
    // Notify other webviews (the recording overlay) so they re-apply the palette
    // live — they set `data-theme` on their own document and can't see this one.
    let _ = app.emit("theme-changed", parsed);
    Ok(())
}

/// Applies the appearance setting to the native window chrome (title bar), which
/// CSS `data-theme` cannot reach. `System` clears the override so the window
/// follows the OS. Call this on startup and whenever the setting changes to keep
/// the title bar in sync with the in-app palette.
///
/// On Windows this themes the title bar only. On macOS `set_theme` sets
/// `NSApp.appearance` app-wide, which is what we want here: it darkens the title
/// bar and keeps the overlay in step. Linux is left to `data-theme` alone, since
/// its window theming is backend-dependent and unreliable.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn apply_window_theme(app: &AppHandle, theme: Theme) {
    let window_theme = match theme {
        Theme::System => None,
        Theme::Light => Some(tauri::Theme::Light),
        Theme::Dark => Some(tauri::Theme::Dark),
    };
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.set_theme(window_theme) {
            warn!("Failed to apply window theme: {}", e);
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_translate_to_english_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.translate_to_english = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_selected_language_setting(app: AppHandle, language: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.selected_language = language;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_overlay_position_setting(app: AppHandle, position: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match position.as_str() {
        // "none" is retired (visibility is overlay_style now); fold legacy callers
        // onto Bottom rather than warn.
        "none" | "bottom" => OverlayPosition::Bottom,
        "top" => OverlayPosition::Top,
        other => {
            warn!("Invalid overlay position '{}', defaulting to bottom", other);
            OverlayPosition::Bottom
        }
    };
    settings.overlay_position = parsed;
    settings::write_settings(&app, settings);

    // Whether the overlay shows at all is owned by overlay_style now; position
    // only ever toggles Top/Bottom, so the enabled cache is untouched here.
    // Update overlay position without recreating window
    crate::utils::update_overlay_position(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_overlay_style_setting(app: AppHandle, style: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match style.as_str() {
        "none" => OverlayStyle::None,
        "minimal" => OverlayStyle::Minimal,
        "live" => OverlayStyle::Live,
        other => {
            warn!("Invalid overlay style '{}', defaulting to minimal", other);
            OverlayStyle::Minimal
        }
    };
    settings.overlay_style = parsed;
    settings::write_settings(&app, settings);

    // Keep the cached overlay-enabled flag in sync so emit_levels stops (or
    // resumes) emitting on the next audio callback.
    crate::overlay::update_overlay_enabled_cache(parsed != OverlayStyle::None);

    // Reposition in case the window needs to re-center for the new style.
    crate::utils::update_overlay_position(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_debug_mode_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.debug_mode = enabled;
    settings::write_settings(&app, settings);

    // Keep webview log streaming in sync: the live log viewer only exists in
    // debug mode, so logs are forwarded to the frontend only while it is on.
    crate::WEBVIEW_LOG_STREAMING.store(enabled, std::sync::atomic::Ordering::Relaxed);

    // Emit event to notify frontend of debug mode change
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "debug_mode",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_start_hidden_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.start_hidden = enabled;
    settings::write_settings(&app, settings);

    // Notify frontend
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "start_hidden",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_autostart_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.autostart_enabled = enabled;
    settings::write_settings(&app, settings);

    // Apply the autostart setting immediately
    crate::autostart::apply_autostart(&app, enabled);

    // Notify frontend
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "autostart_enabled",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_update_checks_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    if settings::update_checks_forced_disabled() {
        return Err("Update checks are disabled in this build or by HANDY_DISABLE_UPDATER".into());
    }

    let mut settings = settings::get_settings(&app);
    settings.update_checks_enabled = enabled;
    settings::write_settings(&app, settings);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "update_checks_enabled",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_show_whats_new_on_update_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.show_whats_new_on_update = enabled;
    settings::write_settings(&app, settings);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "show_whats_new_on_update",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_whats_new_last_seen_version_setting(
    app: AppHandle,
    version: String,
) -> Result<(), String> {
    let version = version.trim().to_string();
    let mut settings = settings::get_settings(&app);
    settings.whats_new_last_seen_version = version.clone();
    settings::write_settings(&app, settings);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "whats_new_last_seen_version",
            "value": version
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn update_custom_words(app: AppHandle, words: Vec<String>) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.custom_words = words;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_word_correction_threshold_setting(
    app: AppHandle,
    threshold: f64,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.word_correction_threshold = threshold;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_extra_recording_buffer_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.extra_recording_buffer_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_delay_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.paste_delay_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_delay_after_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.paste_delay_after_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_reliable_paste_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.reliable_paste = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_method_setting(app: AppHandle, method: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match method.as_str() {
        "ctrl_v" => PasteMethod::CtrlV,
        "direct" => PasteMethod::Direct,
        "none" => PasteMethod::None,
        "shift_insert" => PasteMethod::ShiftInsert,
        "ctrl_shift_v" => PasteMethod::CtrlShiftV,
        "external_script" => PasteMethod::ExternalScript,
        other => {
            warn!("Invalid paste method '{}', defaulting to ctrl_v", other);
            PasteMethod::CtrlV
        }
    };
    settings.paste_method = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_available_typing_tools() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        crate::clipboard::get_available_typing_tools()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec!["auto".to_string()]
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_typing_tool_setting(app: AppHandle, tool: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match tool.as_str() {
        "auto" => TypingTool::Auto,
        "wtype" => TypingTool::Wtype,
        "kwtype" => TypingTool::Kwtype,
        "dotool" => TypingTool::Dotool,
        "ydotool" => TypingTool::Ydotool,
        "xdotool" => TypingTool::Xdotool,
        other => {
            warn!("Invalid typing tool '{}', defaulting to auto", other);
            TypingTool::Auto
        }
    };
    settings.typing_tool = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_external_script_path_setting(
    app: AppHandle,
    path: Option<String>,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.external_script_path = path;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_clipboard_handling_setting(app: AppHandle, handling: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match handling.as_str() {
        "dont_modify" => ClipboardHandling::DontModify,
        "copy_to_clipboard" => ClipboardHandling::CopyToClipboard,
        other => {
            warn!(
                "Invalid clipboard handling '{}', defaulting to dont_modify",
                other
            );
            ClipboardHandling::DontModify
        }
    };
    settings.clipboard_handling = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_submit_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.auto_submit = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_submit_key_setting(app: AppHandle, key: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match key.as_str() {
        "enter" => AutoSubmitKey::Enter,
        "ctrl_enter" => AutoSubmitKey::CtrlEnter,
        "cmd_enter" => AutoSubmitKey::CmdEnter,
        other => {
            warn!("Invalid auto submit key '{}', defaulting to enter", other);
            AutoSubmitKey::Enter
        }
    };
    settings.auto_submit_key = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.post_process_enabled = enabled;
    settings::write_settings(&app, settings.clone());

    // Register or unregister the post-processing shortcut
    if let Some(binding) = settings
        .bindings
        .get("transcribe_with_post_process")
        .cloned()
    {
        if enabled {
            let _ = register_shortcut(&app, binding);
        } else {
            let _ = unregister_shortcut(&app, binding);
        }
    }

    crate::secure_input::reconcile_fallback(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_experimental_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.experimental_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_base_url_setting(
    app: AppHandle,
    provider_id: String,
    base_url: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let label = settings
        .post_process_provider(&provider_id)
        .map(|provider| provider.label.clone())
        .ok_or_else(|| format!("Provider '{}' not found", provider_id))?;

    let provider = settings
        .post_process_provider_mut(&provider_id)
        .expect("Provider looked up above must exist");

    if provider.id != "custom" {
        return Err(format!(
            "Provider '{}' does not allow editing the base URL",
            label
        ));
    }

    provider.base_url = base_url;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Generic helper to validate provider exists
fn validate_provider_exists(
    settings: &settings::AppSettings,
    provider_id: &str,
) -> Result<(), String> {
    if !settings
        .post_process_providers
        .iter()
        .any(|provider| provider.id == provider_id)
    {
        return Err(format!("Provider '{}' not found", provider_id));
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_api_key_setting(
    app: AppHandle,
    provider_id: String,
    api_key: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    validate_provider_exists(&settings, &provider_id)?;
    settings.post_process_api_keys.insert(provider_id, api_key);
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_model_setting(
    app: AppHandle,
    provider_id: String,
    model: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    validate_provider_exists(&settings, &provider_id)?;
    settings.post_process_models.insert(provider_id, model);
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_post_process_provider(app: AppHandle, provider_id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    validate_provider_exists(&settings, &provider_id)?;
    settings.post_process_provider_id = provider_id;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn add_post_process_prompt(
    app: AppHandle,
    name: String,
    prompt: String,
) -> Result<LLMPrompt, String> {
    let mut settings = settings::get_settings(&app);

    // Generate unique ID using timestamp and random component
    let id = format!("prompt_{}", chrono::Utc::now().timestamp_millis());

    let new_prompt = LLMPrompt {
        id: id.clone(),
        name,
        prompt,
    };

    settings.post_process_prompts.push(new_prompt.clone());
    settings::write_settings(&app, settings);

    Ok(new_prompt)
}

#[tauri::command]
#[specta::specta]
pub fn update_post_process_prompt(
    app: AppHandle,
    id: String,
    name: String,
    prompt: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);

    if let Some(existing_prompt) = settings
        .post_process_prompts
        .iter_mut()
        .find(|p| p.id == id)
    {
        existing_prompt.name = name;
        existing_prompt.prompt = prompt;
        settings::write_settings(&app, settings);
        Ok(())
    } else {
        Err(format!("Prompt with id '{}' not found", id))
    }
}

#[tauri::command]
#[specta::specta]
pub fn delete_post_process_prompt(app: AppHandle, id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);

    settings::remove_prompt(&mut settings, &id)?;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn fetch_post_process_models(
    app: AppHandle,
    provider_id: String,
) -> Result<Vec<String>, String> {
    let settings = settings::get_settings(&app);

    // Find the provider
    let provider = settings
        .post_process_providers
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| format!("Provider '{}' not found", provider_id))?;

    if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return Ok(vec![APPLE_INTELLIGENCE_DEFAULT_MODEL_ID.to_string()]);
        }

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            return Err("Apple Intelligence is only available on Apple silicon Macs running macOS 15 or later.".to_string());
        }
    }

    // Get API key
    let api_key = settings
        .post_process_api_keys
        .get(&provider_id)
        .cloned()
        .unwrap_or_default();

    // Skip fetching if no API key for providers that typically need one
    if api_key.trim().is_empty() && provider.id != "custom" {
        return Err(format!(
            "API key is required for {}. Please add an API key to list available models.",
            provider.label
        ));
    }

    crate::llm_client::fetch_models(provider, api_key, &settings.post_process_request_options).await
}

#[tauri::command]
#[specta::specta]
pub fn set_post_process_selected_prompt(app: AppHandle, id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);

    // Verify the prompt exists
    if !settings.post_process_prompts.iter().any(|p| p.id == id) {
        return Err(format!("Prompt with id '{}' not found", id));
    }

    settings.post_process_selected_prompt_id = Some(id);
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_request_options(
    app: AppHandle,
    options: settings::LlmRequestOptions,
) -> Result<(), String> {
    options.validate()?;
    let mut settings = settings::get_settings(&app);
    settings.post_process_request_options = options;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_assistant_selected_prompt(app: AppHandle, id: Option<String>) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    if id
        .as_ref()
        .is_some_and(|id| !settings.post_process_prompts.iter().any(|p| &p.id == id))
    {
        return Err("Prompt not found".into());
    }
    settings.assistant_selected_prompt_id = id;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_mute_while_recording_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.mute_while_recording = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_append_trailing_space_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.append_trailing_space = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_lazy_stream_close_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.lazy_stream_close = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_vad_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.vad_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn change_vad_backend_setting(app: AppHandle, backend: VadBackend) -> Result<(), String> {
    if settings::get_settings(&app).vad_backend == backend {
        return Ok(());
    }

    // Construct/swap the detector and, when necessary, reopen cpal away from
    // the webview thread. Persist only after the runtime change succeeds so a
    // rejected in-progress switch or failed microphone reopen rolls back cleanly.
    let manager = app
        .state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
        .inner()
        .clone();
    tokio::task::spawn_blocking(move || manager.update_vad_backend(backend))
        .await
        .map_err(|e| format!("audio task join failed: {e}"))?
        .map_err(|e| format!("Failed to update VAD backend: {e}"))?;

    let mut current_settings = settings::get_settings(&app);
    current_settings.vad_backend = backend;
    settings::write_settings(&app, current_settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_filler_word_removal_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.filler_word_removal_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_chinese_script_setting(app: AppHandle, script: ChineseScript) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.chinese_script = script;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_app_language_setting(app: AppHandle, language: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.app_language = language.clone();
    settings::write_settings(&app, settings);

    // Refresh the tray menu with the new language
    tray::update_tray_menu(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_show_tray_icon_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.show_tray_icon = enabled;
    settings::write_settings(&app, settings);

    // Apply change immediately
    tray::set_tray_visibility(&app, enabled);

    Ok(())
}

/// Save accelerator settings and make the next model use reload with them.
/// The currently running transcription, if any, keeps its existing engine.
fn save_accelerator_and_reload_next_use(app: &AppHandle, s: settings::AppSettings) {
    settings::write_settings(app, s);

    let tm = app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>();
    tm.reload_model_on_next_use();
}

#[tauri::command]
#[specta::specta]
pub fn change_transcribe_accelerator_setting(
    app: AppHandle,
    accelerator: settings::TranscribeAcceleratorSetting,
) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.transcribe_accelerator = accelerator;
    save_accelerator_and_reload_next_use(&app, s);
    app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>()
        .retry_transcribe_gpu("the transcribe.cpp accelerator setting changed");
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_ort_accelerator_setting(
    app: AppHandle,
    accelerator: settings::OrtAcceleratorSetting,
) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.ort_accelerator = accelerator;
    save_accelerator_and_reload_next_use(&app, s);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_transcribe_gpu_device(app: AppHandle, device: Option<String>) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.transcribe_gpu_device = device;
    save_accelerator_and_reload_next_use(&app, s);
    app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>()
        .retry_transcribe_gpu("the transcribe.cpp GPU device setting changed");
    Ok(())
}

/// Return which accelerators and GPU devices are available for this build.
///
/// First-call cost is dominated by enumerating GPU devices through the
/// transcribe.cpp Metal/Vulkan backend, which loads dynamic libraries and
/// probes hardware. Run it on the blocking pool so the webview thread
/// stays responsive — see also the startup pre-warm in `lib.rs`.
#[tauri::command]
#[specta::specta]
pub async fn get_available_accelerators(
    app: AppHandle,
) -> crate::managers::transcription::AvailableAccelerators {
    tauri::async_runtime::spawn_blocking(move || {
        let tm =
            app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>();
        crate::managers::transcription::get_available_accelerators(&tm)
    })
    .await
    .expect("get_available_accelerators panicked")
}

#[cfg(test)]
mod tests {
    #[test]
    fn navigation_keys_are_rejected_for_user_bindings_but_cancel_escape_and_fkeys_work() {
        for implementation in [
            super::KeyboardImplementation::Tauri,
            super::KeyboardImplementation::HandyKeys,
        ] {
            for key in ["enter", "return", "escape", "esc", "tab", " ENTER "] {
                let error =
                    super::validate_binding_input("assistant", key, implementation).unwrap_err();
                assert!(
                    error.contains("cannot be used alone"),
                    "{implementation:?}: {key}: {error}"
                );
            }
            for key in ["esc", "escape"] {
                assert!(super::validate_binding_input("cancel", key, implementation).is_ok());
            }
            for key in ["f10", "ctrl+enter", "ctrl+alt+space"] {
                assert!(
                    super::validate_binding_input("assistant", key, implementation).is_ok(),
                    "{implementation:?}: {key}"
                );
            }
            assert!(super::validate_binding_input("cancel", "enter", implementation).is_err());
        }
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn failed_change_does_not_restore_startup_skipped_assistant() {
        let mut settings = super::settings::get_default_settings();
        settings.keyboard_implementation = super::KeyboardImplementation::Tauri;
        settings.post_process_enabled = false;
        for (owner, skipped) in [
            ("cancel", "esc"),
            ("transcribe_with_post_process", "ctrl+shift+space"),
        ] {
            settings
                .bindings
                .get_mut("assistant")
                .unwrap()
                .current_binding = skipped.into();
            assert!(!super::configured_bindings(&settings)
                .iter()
                .any(|b| b.id == "assistant"));
            // Same failed-validation rollback used by change_binding, without native registration.
            assert!(super::validate_shortcut_for_implementation(
                "fn",
                settings.keyboard_implementation
            )
            .is_err());
            super::restore_previous_binding(&settings, &settings.bindings["assistant"], |_| {
                panic!("startup-skipped assistant must not be registered over {owner}");
            })
            .unwrap();
        }
        let mut restored = false;
        super::restore_previous_binding(&settings, &settings.bindings["transcribe"], |binding| {
            assert_eq!(binding.id, "transcribe");
            restored = true;
            Ok(())
        })
        .unwrap();
        assert!(
            restored,
            "an eligible previous binding must still be restored"
        );
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn assistant_handy_keys_generic_and_sided_modifiers_overlap_in_both_directions() {
        let implementation = super::KeyboardImplementation::HandyKeys;
        for group in ["ctrl", "alt", "shift", "super"] {
            let mut settings = super::settings::get_default_settings();
            for side in ["left", "right"] {
                let generic = format!("{group}+f9");
                let sided = format!("{group}_{side}+f9");
                for (existing, candidate) in [(&generic, &sided), (&sided, &generic)] {
                    settings.bindings.get_mut("cancel").unwrap().current_binding = existing.clone();
                    assert_eq!(
                        super::binding_conflict(
                            &settings.bindings,
                            "assistant",
                            candidate,
                            implementation
                        ),
                        Some("cancel".into()),
                        "{existing} vs {candidate} must conflict"
                    );
                }
            }
            settings.bindings.get_mut("cancel").unwrap().current_binding =
                format!("{group}_left+f9");
            for candidate in [
                format!("{group}_right+f9"),
                format!("{group}_left+f10"),
                format!("fn+{group}_left+f9"),
                "f9".into(),
            ] {
                assert_eq!(
                    super::binding_conflict(
                        &settings.bindings,
                        "assistant",
                        &candidate,
                        implementation
                    ),
                    None,
                    "{group}: {candidate}"
                );
            }
        }
    }

    #[cfg(target_os = "windows")]
    use super::{
        binding_conflict, bindings_for_implementation, configured_bindings, settings,
        KeyboardImplementation,
    };

    #[cfg(target_os = "windows")]
    #[test]
    fn assistant_conflict_normalizes_aliases_and_includes_disabled_cleanup_and_cancel() {
        let mut settings = settings::get_default_settings();
        settings.post_process_enabled = false;
        assert_eq!(
            binding_conflict(
                &settings.bindings,
                "assistant",
                "CONTROL+SPACE",
                settings.keyboard_implementation
            ),
            Some("transcribe".into())
        );
        assert_eq!(
            binding_conflict(
                &settings.bindings,
                "assistant",
                "SHIFT+Control+space",
                settings.keyboard_implementation
            ),
            Some("transcribe_with_post_process".into())
        );
        assert_eq!(
            binding_conflict(
                &settings.bindings,
                "assistant",
                "ESCAPE",
                settings.keyboard_implementation
            ),
            Some("cancel".into())
        );
        assert_eq!(
            binding_conflict(
                &settings.bindings,
                "assistant",
                "ALT+Control+SPACE",
                settings.keyboard_implementation
            ),
            None
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn assistant_conflict_uses_backend_key_aliases_and_modifier_identities() {
        for implementation in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            let mut settings = settings::get_default_settings();
            settings.keyboard_implementation = implementation;
            for (existing, candidate) in [
                ("escape", "esc"),
                ("esc", "escape"),
                ("ctrl+alt+space", "ALT+CONTROL+SPACE"),
                ("ctrl+space", "control+space"),
            ] {
                settings.bindings.get_mut("cancel").unwrap().current_binding = existing.into();
                assert_eq!(
                    binding_conflict(&settings.bindings, "assistant", candidate, implementation),
                    Some("cancel".into()),
                    "{implementation:?}: {existing} vs {candidate}"
                );
            }
            settings.bindings.get_mut("cancel").unwrap().current_binding = "escape".into();
            settings
                .bindings
                .get_mut("assistant")
                .unwrap()
                .current_binding = "esc".into();
            assert!(!configured_bindings(&settings)
                .iter()
                .any(|b| b.id == "assistant"));
        }
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn assistant_handy_keys_side_distinct_modifiers_do_not_conflict() {
        let mut settings = settings::get_default_settings();
        let implementation = KeyboardImplementation::HandyKeys;
        // F9 isolates the two side-specific bindings from default Ctrl+Space,
        // which correctly overlaps either side after generic overlap checking.
        settings.bindings.get_mut("cancel").unwrap().current_binding = "ctrl_left+f9".into();
        assert_eq!(
            binding_conflict(
                &settings.bindings,
                "assistant",
                "control_left+f9",
                implementation
            ),
            Some("cancel".into())
        );
        assert_eq!(
            binding_conflict(
                &settings.bindings,
                "assistant",
                "ctrl_right+f9",
                implementation
            ),
            None
        );
        settings.bindings.get_mut("cancel").unwrap().current_binding = "option_left+f9".into();
        assert_eq!(
            binding_conflict(
                &settings.bindings,
                "assistant",
                "alt_right+f9",
                implementation
            ),
            None
        );
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn changing_startup_skipped_assistant_does_not_unregister_other_owner() {
        let mut settings = settings::get_default_settings();
        settings.keyboard_implementation = KeyboardImplementation::Tauri;
        settings
            .bindings
            .get_mut("assistant")
            .unwrap()
            .current_binding = "esc".into();
        super::unregister_previous_binding(&settings, &settings.bindings["assistant"], |_| {
            panic!("Escape belongs to cancel; the skipped assistant must not unregister it");
        })
        .unwrap();
        let mut called = false;
        super::unregister_previous_binding(
            &settings,
            &settings.bindings["transcribe"],
            |binding| {
                assert_eq!(binding.id, "transcribe");
                called = true;
                Ok(())
            },
        )
        .unwrap();
        assert!(called);
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn implementation_switch_preserves_stored_legacy_navigation_bindings() {
        for implementation in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            for key in ["enter", "escape", "tab"] {
                let mut settings = settings::get_default_settings();
                settings
                    .bindings
                    .get_mut("transcribe")
                    .unwrap()
                    .current_binding = key.into();
                let (bindings, reset) = bindings_for_implementation(&mut settings, implementation);
                assert!(
                    reset.is_empty(),
                    "stored {key} was reset for {implementation:?}"
                );
                assert_eq!(settings.bindings["transcribe"].current_binding, key);
                assert_eq!(
                    bindings
                        .iter()
                        .find(|b| b.id == "transcribe")
                        .unwrap()
                        .current_binding,
                    key
                );
                assert!(
                    super::validate_binding_input("transcribe", key, implementation).is_err(),
                    "new input must still reject {key}"
                );
            }
        }
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn assistant_implementation_switch_and_resume_keep_registration_order_and_user_bindings() {
        let mut settings = settings::get_default_settings();
        settings.post_process_enabled = true;
        settings
            .bindings
            .get_mut("transcribe")
            .unwrap()
            .current_binding = "ctrl+f8".into();
        settings
            .bindings
            .get_mut("assistant")
            .unwrap()
            .current_binding = "option_left+space".into();
        let (handy, reset) =
            bindings_for_implementation(&mut settings, KeyboardImplementation::HandyKeys);
        assert!(reset.is_empty());
        assert_eq!(
            handy.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["transcribe", "transcribe_with_post_process", "assistant"]
        );
        assert_eq!(handy.last().unwrap().current_binding, "option_left+space");
        let (tauri, reset) =
            bindings_for_implementation(&mut settings, KeyboardImplementation::Tauri);
        assert_eq!(reset, ["assistant"]);
        assert_eq!(tauri.last().unwrap().current_binding, "ctrl+alt+space");
        assert_eq!(tauri.first().unwrap().current_binding, "ctrl+f8");
        assert_eq!(
            serde_json::to_value(configured_bindings(&settings)).unwrap(),
            serde_json::to_value(&tauri).unwrap()
        );
        let (again, reset) =
            bindings_for_implementation(&mut settings, KeyboardImplementation::HandyKeys);
        assert!(reset.is_empty());
        assert_eq!(
            serde_json::to_value(again).unwrap(),
            serde_json::to_value(tauri).unwrap()
        );
        settings
            .bindings
            .get_mut("assistant")
            .unwrap()
            .current_binding = "ctrl+f8".into();
        assert_eq!(
            configured_bindings(&settings)
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            ["transcribe", "transcribe_with_post_process"]
        );
        for implementation in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            let (bindings, _) = bindings_for_implementation(&mut settings, implementation);
            assert_eq!(
                bindings.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
                ["transcribe", "transcribe_with_post_process"]
            );
        }
    }
    // -----------------------------------------------------------------------
    // HANDY-008: profile hotkeys
    // -----------------------------------------------------------------------

    use super::settings::{ProcessingProfile, ProfileKind};

    fn profile(id: &str, kind: ProfileKind) -> ProcessingProfile {
        ProcessingProfile {
            id: id.into(),
            name: format!("Profile {id}"),
            kind,
            prompt_id: Some("default_improve_transcriptions".into()),
            model: None,
            speech_language: None,
            english_cleanup: None,
        }
    }

    /// Default settings plus custom profiles with optional hotkeys.
    fn with_profiles(
        implementation: super::KeyboardImplementation,
        profiles: &[(&str, ProfileKind, Option<&str>)],
    ) -> super::settings::AppSettings {
        let mut settings = super::settings::get_default_settings();
        settings.keyboard_implementation = implementation;
        for (id, kind, accelerator) in profiles {
            let profile = profile(id, *kind);
            if let Some(accelerator) = accelerator {
                settings.bindings.insert(
                    profile.binding_id(),
                    super::ShortcutBinding {
                        id: profile.binding_id(),
                        name: profile.name.clone(),
                        description: String::new(),
                        default_binding: String::new(),
                        current_binding: (*accelerator).into(),
                    },
                );
            }
            settings.processing_profiles.push(profile);
        }
        settings
    }

    fn ids(bindings: &[super::ShortcutBinding]) -> Vec<String> {
        bindings.iter().map(|b| b.id.clone()).collect()
    }

    fn default_order() -> Vec<String> {
        super::ordered_binding_ids(&super::settings::get_default_settings().bindings)
    }

    fn accelerator(id: &str) -> String {
        super::settings::get_default_settings().bindings[id]
            .current_binding
            .clone()
    }

    #[test]
    fn handy008_registration_order_appends_custom_profiles_after_defaults() {
        let mut settings = with_profiles(
            super::KeyboardImplementation::Tauri,
            &[
                ("pd2", ProfileKind::Dictation, Some("ctrl+f9")),
                ("pc", ProfileKind::SelectionCommand, Some("ctrl+f10")),
                ("unbound", ProfileKind::Dictation, None),
                ("pd1", ProfileKind::Dictation, Some("ctrl+f11")),
            ],
        );
        // An orphan binding (un-normalized settings) is never registered.
        let mut orphan = settings.bindings["dictation_profile_pd1"].clone();
        orphan.id = "dictation_profile_gone".into();
        orphan.current_binding = "ctrl+f12".into();
        settings.bindings.insert(orphan.id.clone(), orphan);
        let mut expected = default_order();
        expected.push("dictation_profile_pd2".into());
        if cfg!(target_os = "windows") {
            expected.push("command_profile_pc".into());
        }
        expected.push("dictation_profile_pd1".into());
        assert_eq!(ids(&super::registration_order(&settings)), expected);
        // The built-in records add nothing: without custom profiles the order
        // is today's default order.
        let plain = super::settings::get_default_settings();
        assert_eq!(plain.processing_profiles.len(), 2);
        assert_eq!(ids(&super::registration_order(&plain)), default_order());
        let configured = ids(&super::configured_bindings(&settings));
        assert!(configured.contains(&"dictation_profile_pd1".to_string()));
        assert!(!configured.contains(&"dictation_profile_gone".to_string()));
        assert!(!configured.contains(&"cancel".to_string()));
    }

    #[test]
    fn handy008_overlapping_profile_is_skipped_and_the_earlier_owner_kept() {
        for implementation in [
            super::KeyboardImplementation::Tauri,
            super::KeyboardImplementation::HandyKeys,
        ] {
            let mut owners = vec![
                ("cancel", accelerator("cancel")),
                (
                    "transcribe_with_post_process",
                    accelerator("transcribe_with_post_process"),
                ),
                ("transcribe", accelerator("transcribe")),
                ("dictation_profile_first", "ctrl+f9".to_string()),
            ];
            if cfg!(target_os = "windows") {
                owners.push(("assistant", accelerator("assistant")));
            }
            for (owner, taken) in owners {
                let mut settings = with_profiles(
                    implementation,
                    &[
                        ("first", ProfileKind::Dictation, Some("ctrl+f9")),
                        ("late", ProfileKind::Dictation, Some(&taken)),
                    ],
                );
                // Cleanup off: its accelerator is still reserved (N1).
                settings.post_process_enabled = false;
                let label = format!("{implementation:?}: {owner}");
                assert_eq!(
                    super::registration_skipped(&settings, "dictation_profile_late"),
                    Some(owner.to_string()),
                    "{label}"
                );
                let configured = ids(&super::configured_bindings(&settings));
                assert!(
                    !configured.contains(&"dictation_profile_late".to_string()),
                    "{label}"
                );
                if !matches!(owner, "cancel" | "transcribe_with_post_process") {
                    assert!(configured.contains(&owner.to_string()), "{label}");
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn handy008_profile_conflicting_with_assistant_is_skipped_not_the_assistant() {
        let settings = with_profiles(
            super::KeyboardImplementation::HandyKeys,
            &[("p", ProfileKind::SelectionCommand, Some("ctrl+alt+space"))],
        );
        let configured = ids(&super::configured_bindings(&settings));
        assert!(configured.contains(&"assistant".to_string()));
        assert!(!configured.contains(&"command_profile_p".to_string()));
        assert_eq!(super::registration_skipped(&settings, "assistant"), None);
    }

    #[test]
    fn handy008_profile_conflicts_follow_each_backend_overlap_semantics() {
        let handy = with_profiles(
            super::KeyboardImplementation::HandyKeys,
            &[("p", ProfileKind::Dictation, Some("ctrl+f9"))],
        );
        // HandyKeys: a generic modifier overlaps either side.
        for candidate in ["ctrl_left+f9", "control_right+f9", "CTRL+F9"] {
            assert_eq!(
                super::binding_conflict(
                    &handy.bindings,
                    "dictation_profile_q",
                    candidate,
                    super::KeyboardImplementation::HandyKeys
                ),
                Some("dictation_profile_p".into()),
                "{candidate}"
            );
        }
        // Tauri: identity of parsed accelerators; sided names do not parse.
        assert_eq!(
            super::binding_conflict(
                &handy.bindings,
                "dictation_profile_q",
                "ctrl_left+f9",
                super::KeyboardImplementation::Tauri
            ),
            None
        );
        assert_eq!(
            super::binding_conflict(
                &handy.bindings,
                "dictation_profile_q",
                "control+F9",
                super::KeyboardImplementation::Tauri
            ),
            Some("dictation_profile_p".into())
        );
        // A profile can keep its own accelerator.
        assert_eq!(
            super::binding_conflict(
                &handy.bindings,
                "dictation_profile_p",
                "ctrl+f9",
                super::KeyboardImplementation::Tauri
            ),
            None
        );
        assert!(super::bindings_overlap(
            "ctrl+f9",
            "ctrl_right+f9",
            super::KeyboardImplementation::HandyKeys
        ));
        assert!(!super::bindings_overlap(
            "ctrl_left+f9",
            "ctrl_right+f9",
            super::KeyboardImplementation::HandyKeys
        ));
    }

    #[test]
    fn handy008_n2_never_unregisters_or_restores_a_skipped_profile_binding() {
        let settings = with_profiles(
            super::KeyboardImplementation::Tauri,
            &[
                (
                    "skipped",
                    ProfileKind::Dictation,
                    Some(&accelerator("transcribe")),
                ),
                ("fine", ProfileKind::Dictation, Some("ctrl+f9")),
            ],
        );
        let skipped = &settings.bindings["dictation_profile_skipped"];
        super::unregister_previous_binding(&settings, skipped, |_| {
            panic!("Tauri unregisters by accelerator; this would remove Transcribe")
        })
        .unwrap();
        super::restore_previous_binding(&settings, skipped, |_| {
            panic!("a skipped profile binding must not be registered over Transcribe")
        })
        .unwrap();
        let fine = &settings.bindings["dictation_profile_fine"];
        let mut calls = 0;
        super::unregister_previous_binding(&settings, fine, |binding| {
            assert_eq!(binding.id, "dictation_profile_fine");
            calls += 1;
            Ok(())
        })
        .unwrap();
        super::restore_previous_binding(&settings, fine, |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
    }

    #[test]
    fn handy008_never_unregisters_or_restores_a_binding_registration_does_not_use() {
        let mut settings = with_profiles(
            super::KeyboardImplementation::Tauri,
            &[
                ("command", ProfileKind::SelectionCommand, Some("ctrl+f9")),
                ("orphan", ProfileKind::Dictation, Some("ctrl+f10")),
            ],
        );
        // The orphan binding outlived its profile.
        settings.processing_profiles.retain(|p| p.id != "orphan");
        let mut unused = vec![settings.bindings["dictation_profile_orphan"].clone()];
        let command = settings.bindings["command_profile_command"].clone();
        if cfg!(target_os = "windows") {
            let mut calls = 0;
            super::unregister_previous_binding(&settings, &command, |_| {
                calls += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(calls, 1, "selection commands register on Windows");
        } else {
            unused.push(command);
        }
        for binding in &unused {
            super::unregister_previous_binding(&settings, binding, |_| {
                panic!("Tauri unregisters by accelerator; this binding was never registered")
            })
            .unwrap();
            super::restore_previous_binding(&settings, binding, |_| {
                panic!("a binding registration does not use must not be registered")
            })
            .unwrap();
        }
    }

    #[test]
    fn handy008_binding_to_modify_handles_new_missing_and_existing_bindings() {
        let mut settings = with_profiles(
            super::KeyboardImplementation::Tauri,
            &[
                ("bound", ProfileKind::Dictation, Some("ctrl+f9")),
                ("unbound", ProfileKind::Dictation, None),
            ],
        );
        assert_eq!(
            super::binding_to_modify(&settings, "dictation_profile_bound")
                .unwrap()
                .unwrap()
                .current_binding,
            "ctrl+f9"
        );
        assert!(
            super::binding_to_modify(&settings, "dictation_profile_unbound")
                .unwrap()
                .is_none()
        );
        let new =
            super::new_profile_binding(&settings, "dictation_profile_unbound", "ctrl+f10").unwrap();
        assert_eq!(
            (
                new.name.as_str(),
                new.default_binding.as_str(),
                new.current_binding.as_str()
            ),
            ("Profile unbound", "", "ctrl+f10")
        );
        for missing in ["dictation_profile_gone", "command_profile_unbound"] {
            assert_eq!(
                super::binding_to_modify(&settings, missing).unwrap_err(),
                "Profile not found",
                "{missing}"
            );
        }
        settings.bindings.remove("transcribe");
        assert_eq!(
            super::binding_to_modify(&settings, "transcribe")
                .unwrap()
                .unwrap()
                .current_binding,
            accelerator("transcribe")
        );
        assert!(super::binding_to_modify(&settings, "unknown").is_err());
    }

    #[test]
    fn handy008_bare_navigation_keys_are_rejected_for_profile_hotkeys() {
        for implementation in [
            super::KeyboardImplementation::Tauri,
            super::KeyboardImplementation::HandyKeys,
        ] {
            for id in ["dictation_profile_p", "command_profile_p"] {
                for key in ["enter", "escape", "esc", "tab"] {
                    assert!(
                        super::validate_binding_input(id, key, implementation).is_err(),
                        "{implementation:?} {id} {key}"
                    );
                }
                assert!(super::validate_binding_input(id, "ctrl+f9", implementation).is_ok());
            }
        }
    }

    #[test]
    fn handy008_implementation_switch_unsets_an_invalid_profile_hotkey() {
        let mut settings = with_profiles(
            super::KeyboardImplementation::HandyKeys,
            &[
                ("sided", ProfileKind::Dictation, Some("ctrl_left+f9")),
                ("plain", ProfileKind::Dictation, Some("ctrl+f10")),
            ],
        );
        let (bindings, reset) = super::bindings_for_implementation(
            &mut settings,
            super::KeyboardImplementation::HandyKeys,
        );
        assert!(reset.is_empty());
        assert!(ids(&bindings).contains(&"dictation_profile_sided".to_string()));
        let (bindings, reset) =
            super::bindings_for_implementation(&mut settings, super::KeyboardImplementation::Tauri);
        assert_eq!(reset, ["dictation_profile_sided"]);
        assert!(!settings.bindings.contains_key("dictation_profile_sided"));
        assert!(!ids(&bindings).contains(&"dictation_profile_sided".to_string()));
        assert!(ids(&bindings).contains(&"dictation_profile_plain".to_string()));
    }

    #[test]
    fn handy008_conflict_message_names_the_owner() {
        let settings = with_profiles(
            super::KeyboardImplementation::Tauri,
            &[("p", ProfileKind::Dictation, Some("ctrl+f9"))],
        );
        let bindings = super::effective_bindings(&settings);
        assert_eq!(
            super::conflict_owner_name(&bindings, "dictation_profile_p"),
            "Profile p"
        );
        assert_eq!(
            super::conflict_owner_name(&bindings, "transcribe"),
            "Transcribe"
        );
        assert_eq!(super::conflict_owner_name(&bindings, "unknown"), "unknown");
    }

    #[test]
    fn handy008_post_process_toggle_governs_only_the_cleanup_hotkey() {
        let mut settings = with_profiles(
            super::KeyboardImplementation::Tauri,
            &[("p", ProfileKind::Dictation, Some("ctrl+f9"))],
        );
        settings.post_process_enabled = false;
        let configured = ids(&super::configured_bindings(&settings));
        assert!(!configured.contains(&"transcribe_with_post_process".to_string()));
        assert!(configured.contains(&"dictation_profile_p".to_string()));
        // The disabled Cleanup accelerator stays reserved (N1).
        assert_eq!(
            super::binding_conflict(
                &super::effective_bindings(&settings),
                "dictation_profile_p",
                &accelerator("transcribe_with_post_process"),
                settings.keyboard_implementation
            ),
            Some("transcribe_with_post_process".into())
        );
        settings.post_process_enabled = true;
        assert!(ids(&super::configured_bindings(&settings))
            .contains(&"transcribe_with_post_process".to_string()));
    }

    #[test]
    fn handy008_fallback_excludes_skipped_and_orphan_profile_bindings() {
        let mut settings = with_profiles(
            super::KeyboardImplementation::HandyKeys,
            &[
                (
                    "skipped",
                    ProfileKind::Dictation,
                    Some(&accelerator("cancel")),
                ),
                ("fine", ProfileKind::Dictation, Some("ctrl+f9")),
                ("command", ProfileKind::SelectionCommand, Some("ctrl+f10")),
            ],
        );
        let mut orphan = settings.bindings["dictation_profile_fine"].clone();
        orphan.id = "dictation_profile_gone".into();
        settings.bindings.insert(orphan.id.clone(), orphan);
        assert!(super::fallback_excluded(
            &settings,
            "dictation_profile_skipped"
        ));
        assert!(super::fallback_excluded(
            &settings,
            "dictation_profile_gone"
        ));
        assert!(!super::fallback_excluded(
            &settings,
            "dictation_profile_fine"
        ));
        for builtin in ["transcribe", "transcribe_with_post_process", "cancel"] {
            assert!(!super::fallback_excluded(&settings, builtin), "{builtin}");
        }
        assert_eq!(
            super::fallback_excluded(&settings, "command_profile_command"),
            !cfg!(target_os = "windows"),
            "selection commands register only on Windows"
        );
    }

    #[test]
    fn handy008_reset_returns_builtins_to_default_and_refuses_profiles() {
        let mut settings = with_profiles(
            super::KeyboardImplementation::Tauri,
            &[("p", ProfileKind::Dictation, Some("ctrl+f9"))],
        );
        settings
            .bindings
            .get_mut("transcribe_with_post_process")
            .unwrap()
            .current_binding = "ctrl+f8".into();
        assert_eq!(
            super::reset_target(&settings, "transcribe_with_post_process").unwrap(),
            accelerator("transcribe_with_post_process")
        );
        assert_eq!(
            super::reset_target(&settings, "dictation_profile_p").unwrap_err(),
            "Profile hotkeys have no default"
        );
    }

    use handy_keys::Hotkey;
    use tauri_plugin_global_shortcut::Shortcut;

    #[test]
    fn compound_shortcut_keys_parse_on_both_backends() {
        for key in [
            "scrolllock",
            "capslock",
            "numlock",
            "pageup",
            "pagedown",
            "printscreen",
        ] {
            assert!(key.parse::<Shortcut>().is_ok(), "Tauri rejected {key}");
            assert!(key.parse::<Hotkey>().is_ok(), "HandyKeys rejected {key}");
        }
    }
}
