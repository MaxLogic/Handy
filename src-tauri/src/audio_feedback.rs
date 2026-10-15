use crate::settings::SoundTheme;
use crate::settings::{self, AppSettings};
use cpal::traits::{DeviceTrait, HostTrait};
use log::{debug, error, warn};
use rodio::OutputStreamBuilder;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::thread;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundType {
    Start,
    Stop,
    /// Confirms that a cancel stopped a recording or in-flight transcription.
    Cancel,
}

/// Where a sound file lives: the user's data directory (custom theme) or the
/// bundled resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoundDir {
    AppData,
    Resource,
}

/// Built-in cancel cue shared by every bundled theme.
const BUILTIN_CANCEL_SOUND: &str = "resources/cancel.wav";

fn resolve_sound_path(
    app: &AppHandle,
    settings: &AppSettings,
    sound_type: SoundType,
) -> Option<PathBuf> {
    let candidates = sound_candidates(settings.sound_theme, sound_type);
    let last = candidates.len() - 1;
    for (index, (file, base_dir)) in candidates.into_iter().enumerate() {
        let resolved = match base_dir {
            SoundDir::AppData => crate::portable::resolve_app_data(app, &file).ok(),
            SoundDir::Resource => app
                .path()
                .resolve(&file, tauri::path::BaseDirectory::Resource)
                .ok(),
        };
        // Earlier candidates are optional overrides; the last one is used as-is.
        match resolved {
            Some(path) if index == last || path.is_file() => return Some(path),
            _ => {}
        }
    }
    None
}

/// Sound files to try in order. A custom theme without `custom_cancel.wav`
/// falls back to the built-in cancel cue.
fn sound_candidates(theme: SoundTheme, sound_type: SoundType) -> Vec<(String, SoundDir)> {
    use SoundDir::{AppData, Resource};
    match (theme, sound_type) {
        (SoundTheme::Custom, SoundType::Start) => vec![("custom_start.wav".into(), AppData)],
        (SoundTheme::Custom, SoundType::Stop) => vec![("custom_stop.wav".into(), AppData)],
        (SoundTheme::Custom, SoundType::Cancel) => vec![
            ("custom_cancel.wav".into(), AppData),
            (BUILTIN_CANCEL_SOUND.into(), Resource),
        ],
        (_, SoundType::Start) => vec![(theme.to_start_path(), Resource)],
        (_, SoundType::Stop) => vec![(theme.to_stop_path(), Resource)],
        (_, SoundType::Cancel) => vec![(BUILTIN_CANCEL_SOUND.into(), Resource)],
    }
}

pub fn play_feedback_sound(app: &AppHandle, sound_type: SoundType) {
    let settings = settings::get_settings(app);
    if !settings.audio_feedback {
        return;
    }
    if let Some(path) = resolve_sound_path(app, &settings, sound_type) {
        play_sound_async(app, path);
    }
}

/// Error-only cue for a hidden settings window. No focus or speech changes.
pub(crate) fn play_failure_feedback(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    if settings::get_settings(app).audio_feedback {
        // MessageBeep queues the configured Windows error sound asynchronously.
        // SAFETY: no pointer parameters or application/window mutation.
        if let Err(error) = unsafe {
            windows::Win32::System::Diagnostics::Debug::MessageBeep(
                windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            )
        } {
            warn!("Could not play failure cue: {error}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = app;
}

pub fn play_feedback_sound_blocking(app: &AppHandle, sound_type: SoundType) {
    let settings = settings::get_settings(app);
    if !settings.audio_feedback {
        return;
    }
    if let Some(path) = resolve_sound_path(app, &settings, sound_type) {
        play_sound_blocking(app, &path);
    }
}

pub fn play_test_sound(app: &AppHandle, sound_type: SoundType) {
    let settings = settings::get_settings(app);
    if let Some(path) = resolve_sound_path(app, &settings, sound_type) {
        play_sound_blocking(app, &path);
    }
}

fn play_sound_async(app: &AppHandle, path: PathBuf) {
    let app_handle = app.clone();
    thread::spawn(move || {
        if let Err(e) = play_sound_at_path(&app_handle, path.as_path()) {
            error!("Failed to play sound '{}': {}", path.display(), e);
        }
    });
}

fn play_sound_blocking(app: &AppHandle, path: &Path) {
    if let Err(e) = play_sound_at_path(app, path) {
        error!("Failed to play sound '{}': {}", path.display(), e);
    }
}

fn play_sound_at_path(app: &AppHandle, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let settings = settings::get_settings(app);
    let volume = settings.audio_feedback_volume;
    let selected_device = settings.selected_output_device.clone();
    play_audio_file(path, selected_device, volume)
}

fn play_audio_file(
    path: &std::path::Path,
    selected_device: Option<String>,
    volume: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let stream_builder = if let Some(device_name) = selected_device {
        if device_name == "Default" {
            debug!("Using default device");
            OutputStreamBuilder::from_default_device()?
        } else {
            let host = crate::audio_toolkit::get_cpal_host();
            let devices = host.output_devices()?;

            let mut found_device = None;
            for device in devices {
                if device.name()? == device_name {
                    found_device = Some(device);
                    break;
                }
            }

            match found_device {
                Some(device) => OutputStreamBuilder::from_device(device)?,
                None => {
                    warn!("Device '{}' not found, using default device", device_name);
                    OutputStreamBuilder::from_default_device()?
                }
            }
        }
    } else {
        debug!("Using default device");
        OutputStreamBuilder::from_default_device()?
    };

    let stream_handle = stream_builder.open_stream()?;
    let mixer = stream_handle.mixer();

    let file = File::open(path)?;
    let buf_reader = BufReader::new(file);

    let sink = rodio::play(mixer, buf_reader)?;
    sink.set_volume(volume);
    sink.sleep_until_end();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use SoundDir::{AppData, Resource};

    fn files(theme: SoundTheme, sound: SoundType) -> Vec<(String, SoundDir)> {
        sound_candidates(theme, sound)
    }

    #[test]
    fn bundled_themes_use_builtin_cancel_sound() {
        for theme in [SoundTheme::Marimba, SoundTheme::Pop] {
            assert_eq!(
                files(theme, SoundType::Cancel),
                [("resources/cancel.wav".to_string(), Resource)]
            );
        }
    }

    #[test]
    fn custom_cancel_prefers_user_file_then_falls_back_to_builtin() {
        assert_eq!(
            files(SoundTheme::Custom, SoundType::Cancel),
            [
                ("custom_cancel.wav".to_string(), AppData),
                ("resources/cancel.wav".to_string(), Resource),
            ]
        );
    }

    #[test]
    fn start_and_stop_paths_are_unchanged() {
        assert_eq!(
            files(SoundTheme::Pop, SoundType::Stop),
            [("resources/pop_stop.wav".to_string(), Resource)]
        );
        assert_eq!(
            files(SoundTheme::Marimba, SoundType::Start),
            [("resources/marimba_start.wav".to_string(), Resource)]
        );
        assert_eq!(
            files(SoundTheme::Custom, SoundType::Start),
            [("custom_start.wav".to_string(), AppData)]
        );
        assert_eq!(
            files(SoundTheme::Custom, SoundType::Stop),
            [("custom_stop.wav".to_string(), AppData)]
        );
    }

    #[test]
    fn builtin_cancel_sound_is_bundled() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(BUILTIN_CANCEL_SOUND);
        assert!(path.is_file(), "{} missing", path.display());
    }
}
