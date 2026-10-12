use crate::assistant::{
    recovery::RecoveryStore, AssistantRecoveryState, AssistantRecoveryUpdate, RecoveryPart,
};
use std::sync::Arc;
use tauri::{AppHandle, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_specta::Event;

#[tauri::command]
#[specta::specta]
pub fn get_assistant_recovery_state(
    store: State<'_, Arc<RecoveryStore>>,
) -> AssistantRecoveryState {
    AssistantRecoveryState {
        items: store.items(),
        capacity: store.capacity() as u32,
        free_slots: store.free_slots() as u32,
    }
}
#[tauri::command]
#[specta::specta]
pub fn copy_assistant_recovery_text(
    app: AppHandle,
    store: State<'_, Arc<RecoveryStore>>,
    id: u64,
    part: RecoveryPart,
) -> Result<(), String> {
    let item = store.get(id).ok_or("not_found")?;
    let text = match part {
        RecoveryPart::Generated => item.generated,
        RecoveryPart::Instruction => item.instruction,
    };
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
pub fn dismiss_assistant_recovery_item(
    app: AppHandle,
    store: State<'_, Arc<RecoveryStore>>,
    id: u64,
) -> Result<(), String> {
    if !store.dismiss(id) {
        return Err("not_found".into());
    }
    AssistantRecoveryUpdate::Dismissed { id }
        .emit(&app)
        .map_err(|e| e.to_string())
}
