use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::text::OutputLanguageEvidence;
use crate::audio_toolkit::{is_microphone_access_denied, is_no_input_device_error, VadPolicy};
use crate::dictation_timing::{self, Mode, OperationTimer, Outcome, Stage, SttPath};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::StreamWorkKind;
use crate::managers::transcription::TranscriptionManager;
use crate::prompts;
use crate::settings::{
    spec_for_binding, spec_for_profile, AppSettings, OverlayStyle, ProfileKind, ProfileRef,
    ProfileSpec, COMMAND_PROFILE_PREFIX, DICTATION_PROFILE_PREFIX,
};
use crate::shortcut;
use crate::tray::{set_tray_state, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_transcribing_overlay,
};
use crate::TranscriptionCoordinator;
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Manager;
use tauri::{AppHandle, Emitter};

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, serde::Serialize)]
struct RecordingErrorEvent {
    error_type: String,
    detail: Option<String>,
}

/// Drop guard that finishes the transcription pipeline, including immediate
/// model unloading on early exits.
struct FinishGuard(AppHandle, Arc<TranscriptionManager>);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.1.maybe_unload_immediately("transcription session");
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
        // The pipeline just freed its large transient buffers (captured PCM,
        // WAV copy, engine scratch); hand the cached pages back to the OS so
        // they don't sit in malloc arenas until they get swapped out (#1792).
        crate::memory::trim_freed_memory();
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
}

/// Internal workflow. Existing binding IDs are compatibility boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowMode {
    Raw,
    Cleanup,
    Assistant,
}

impl WorkflowMode {
    /// Custom profiles encode their immutable kind in the binding id prefix,
    /// so dispatch never reads settings.
    pub(crate) fn from_binding_id(id: &str) -> Option<Self> {
        match id {
            "transcribe" => Some(Self::Raw),
            "transcribe_with_post_process" => Some(Self::Cleanup),
            "assistant" => Some(Self::Assistant),
            id if id.starts_with(DICTATION_PROFILE_PREFIX) => Some(Self::Cleanup),
            id if id.starts_with(COMMAND_PROFILE_PREFIX) => Some(Self::Assistant),
            _ => None,
        }
    }

    pub(crate) fn binding_id(self) -> &'static str {
        match self {
            Self::Raw => "transcribe",
            Self::Cleanup => "transcribe_with_post_process",
            Self::Assistant => "assistant",
        }
    }

    fn timing_mode(self) -> Mode {
        match self {
            Self::Raw => Mode::Raw,
            Self::Cleanup => Mode::Cleanup,
            Self::Assistant => Mode::Assistant,
        }
    }
}

/// Owned from accepted recording start until stop/cancel. Invalid cleanup
/// configuration is snapshotted too, so later settings cannot switch targets.
pub(crate) struct DictationSession {
    workflow: SessionWorkflow,
    /// For timing: the profile's speech language, or the global setting.
    language: String,
    overlay_style: OverlayStyle,
    /// The profile's speech language. It overrides only this run's STT
    /// settings copy and is never written back.
    pub(crate) stt_language: Option<String>,
    /// Every profile session, built-ins included; None for raw dictation.
    pub(crate) profile: Option<ProfileRef>,
}

enum SessionWorkflow {
    Raw,
    Assistant(Box<crate::assistant::AssistantSession>),
    Cleanup(Box<Result<prompts::CleanupRequest, crate::llm_client::LlmError>>),
}

impl DictationSession {
    pub(crate) fn mode(&self) -> WorkflowMode {
        match &self.workflow {
            SessionWorkflow::Raw => WorkflowMode::Raw,
            SessionWorkflow::Cleanup(_) => WorkflowMode::Cleanup,
            SessionWorkflow::Assistant(_) => WorkflowMode::Assistant,
        }
    }

    fn raw(settings: &AppSettings) -> Self {
        Self {
            workflow: SessionWorkflow::Raw,
            language: settings.selected_language.clone(),
            overlay_style: settings.overlay_style,
            stt_language: None,
            profile: None,
        }
    }

    fn with_spec(settings: &AppSettings, spec: &ProfileSpec, workflow: SessionWorkflow) -> Self {
        Self {
            workflow,
            language: spec
                .speech_language
                .clone()
                .unwrap_or_else(|| settings.selected_language.clone()),
            overlay_style: settings.overlay_style,
            stt_language: spec.speech_language.clone(),
            profile: Some(spec.profile.clone()),
        }
    }

    fn cleanup(settings: &AppSettings, spec: &ProfileSpec) -> Self {
        let request = prompts::CleanupRequest::from_spec(settings, spec);
        Self::with_spec(settings, spec, SessionWorkflow::Cleanup(Box::new(request)))
    }

    /// Resolve what a binding records. Selection-command bindings go through
    /// capture instead and give None, as does a deleted custom profile.
    pub(crate) fn for_binding(binding_id: &str, settings: &AppSettings) -> Option<Self> {
        if binding_id == WorkflowMode::Raw.binding_id() {
            return Some(Self::raw(settings));
        }
        let spec = spec_for_binding(settings, binding_id)?;
        (spec.kind == ProfileKind::Dictation).then(|| Self::cleanup(settings, &spec))
    }

    /// The built-in workflows; kept for tests and the legacy history retry.
    pub(crate) fn snapshot(mode: WorkflowMode, settings: &AppSettings) -> Option<Self> {
        Self::for_binding(mode.binding_id(), settings)
    }

    /// Profile rows retry with their profile, never silently with built-in
    /// Cleanup; legacy rows keep the stored boolean.
    pub(crate) fn for_history_retry(
        entry: &crate::managers::history::HistoryEntry,
        settings: &AppSettings,
    ) -> Result<Self, String> {
        match &entry.profile_id {
            Some(id) => spec_for_profile(settings, id)
                .filter(|spec| spec.kind == ProfileKind::Dictation)
                .map(|spec| Self::cleanup(settings, &spec))
                .ok_or_else(|| "profile_not_found".to_string()),
            None => {
                let mode = if entry.post_process_requested {
                    WorkflowMode::Cleanup
                } else {
                    WorkflowMode::Raw
                };
                Self::snapshot(mode, settings)
                    .ok_or_else(|| "assistant sessions cannot be retried".to_string())
            }
        }
    }

    pub(crate) fn assistant(
        settings: &AppSettings,
        spec: &ProfileSpec,
        session: crate::assistant::AssistantSession,
    ) -> Self {
        Self::with_spec(
            settings,
            spec,
            SessionWorkflow::Assistant(Box::new(session)),
        )
    }

    /// Custom profiles mark their timing record; built-ins keep today's shape
    /// so built-in Cleanup stays in the baseline comparison.
    fn note_timing_profile(&self, timer: &OperationTimer) {
        if let Some(profile) = self.profile.as_ref().filter(|p| !p.builtin) {
            timer.set_profile(profile.id.clone());
        }
    }
    pub(crate) fn into_assistant(self) -> Result<crate::assistant::AssistantSession, Self> {
        if matches!(self.workflow, SessionWorkflow::Assistant(_)) {
            if let SessionWorkflow::Assistant(session) = self.workflow {
                return Ok(*session);
            }
        }
        Err(self)
    }
}

pub(crate) struct TranscribeAction;

pub(crate) async fn complete_unless_cancelled<F, C>(
    operation: F,
    is_cancelled: C,
) -> Option<F::Output>
where
    F: Future,
    C: Fn() -> bool,
{
    tokio::pin!(operation);

    loop {
        if is_cancelled() {
            return None;
        }

        if let Ok(result) =
            tokio::time::timeout(CANCELLATION_POLL_INTERVAL, operation.as_mut()).await
        {
            return Some(result);
        }
    }
}

fn should_use_streaming_overlay(style: OverlayStyle, is_streaming: bool) -> bool {
    style == OverlayStyle::Live && is_streaming
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub cleanup_error: Option<String>,
    pub cleanup_skipped: Option<prompts::CleanupSkipReason>,
    /// The route cleanup took or planned; None without a cleanup request.
    pub llm_route: Option<dictation_timing::LlmRoute>,
}

fn note_cleanup_route(timer: &OperationTimer, output: &ProcessedTranscription) {
    if let Some(route) = output.llm_route {
        timer.set_llm_route(route);
    }
}

impl ProcessedTranscription {
    fn paste_outcome(&self) -> Outcome {
        if self.cleanup_error.is_some() {
            Outcome::CleanupFallback
        } else if self.cleanup_skipped.is_some() {
            Outcome::CleanupSkipped
        } else {
            Outcome::Pasted
        }
    }
}

#[cfg(test)]
pub(crate) async fn process_transcription_output(
    session: &DictationSession,
    transcription: &str,
) -> ProcessedTranscription {
    process_transcription_output_with_language(
        session,
        transcription,
        &OutputLanguageEvidence::Unknown,
    )
    .await
}

pub(crate) async fn process_transcription_output_with_language(
    session: &DictationSession,
    transcription: &str,
    evidence: &OutputLanguageEvidence,
) -> ProcessedTranscription {
    process_transcription_output_routed(
        session,
        transcription,
        |req| prompts::plan_cleanup(req, evidence).route(req),
        |req, text| prompts::cleanup(req, text, evidence),
    )
    .await
}

#[cfg(test)]
async fn process_transcription_output_with<'a, F, Fut>(
    session: &'a DictationSession,
    transcription: &'a str,
    cleanup: F,
) -> ProcessedTranscription
where
    F: FnOnce(&'a prompts::CleanupRequest, &'a str) -> Fut,
    Fut: Future<Output = Result<prompts::CleanupOutcome, crate::llm_client::LlmError>>,
{
    process_transcription_output_routed(session, transcription, |req| req.target_route, cleanup)
        .await
}

/// `planned` names the route a failed request was sent to.
async fn process_transcription_output_routed<'a, P, F, Fut>(
    session: &'a DictationSession,
    transcription: &'a str,
    planned: P,
    cleanup: F,
) -> ProcessedTranscription
where
    P: FnOnce(&prompts::CleanupRequest) -> dictation_timing::LlmRoute,
    F: FnOnce(&'a prompts::CleanupRequest, &'a str) -> Fut,
    Fut: Future<Output = Result<prompts::CleanupOutcome, crate::llm_client::LlmError>>,
{
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;
    let mut cleanup_error = None;
    let mut cleanup_skipped = None;
    let mut llm_route = None;

    // The STT manager has already applied deterministic/custom-word cleanup.
    // Preserve that exact recoverable result whenever cleanup fails.
    if let SessionWorkflow::Cleanup(request) = &session.workflow {
        if !prompts::is_blank_transcription(transcription) {
            match request.as_ref() {
                Ok(request) => {
                    let planned = planned(request);
                    match cleanup(request, transcription).await {
                        Ok(prompts::CleanupOutcome::Cleaned {
                            text,
                            route,
                            prompt_sent,
                        }) => {
                            // Prompted dictation may echo a formatting cue even after
                            // inserting a break. Consume only source-backed echoes.
                            let text = if prompt_sent {
                                crate::dictation_formatting::consume_echoed_cues(
                                    transcription,
                                    &text,
                                )
                            } else {
                                text
                            };
                            post_processed_text = Some(text.clone());
                            final_text = text;
                            llm_route = Some(route);
                            // The S1 format ignores the prompt; record only a sent one.
                            post_process_prompt = prompt_sent.then(|| request.prompt.clone());
                        }
                        Ok(prompts::CleanupOutcome::Skipped(reason)) => {
                            cleanup_skipped = Some(reason);
                            llm_route = Some(request.target_route);
                        }
                        Err(error) => {
                            cleanup_error = Some(error.to_string());
                            llm_route = Some(planned);
                        }
                    }
                }
                Err(error) => cleanup_error = Some(error.to_string()),
            }
        }
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
        cleanup_error,
        cleanup_skipped,
        llm_route,
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
pub(crate) struct CleanupFallbackNotice {
    pub history_available: bool,
    pub raw_delivered: bool,
}

#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
pub(crate) struct PasteFailureNotice {
    pub cleanup_failed: bool,
    pub raw_history_available: bool,
}

pub(crate) trait CleanupFailureReporter {
    fn play_failure_feedback(&self);
    fn emit_notice(&self, event: &str, notice: CleanupFallbackNotice);
    fn emit_paste_failure(&self, notice: PasteFailureNotice);
}

impl CleanupFailureReporter for AppHandle {
    fn play_failure_feedback(&self) {
        crate::audio_feedback::play_failure_feedback(self);
    }
    fn emit_notice(&self, event: &str, notice: CleanupFallbackNotice) {
        let _ = self.emit(event, notice);
    }
    fn emit_paste_failure(&self, notice: PasteFailureNotice) {
        let _ = self.emit("paste-error", notice);
    }
}

pub(crate) fn report_transcription_delivery(
    app: &impl CleanupFailureReporter,
    output: &ProcessedTranscription,
    history_saved: bool,
    result: &Result<bool, String>,
) {
    match result {
        Ok(raw_delivered) => report_cleanup_failure(
            app,
            output,
            CleanupFallbackNotice {
                history_available: history_saved,
                raw_delivered: *raw_delivered,
            },
        ),
        Err(_) => {
            app.play_failure_feedback();
            app.emit_paste_failure(PasteFailureNotice {
                cleanup_failed: output.cleanup_error.is_some(),
                raw_history_available: history_saved && output.cleanup_error.is_some(),
            });
        }
    }
}

pub(crate) fn report_cleanup_failure(
    app: &impl CleanupFailureReporter,
    output: &ProcessedTranscription,
    notice: CleanupFallbackNotice,
) {
    if let Some(error) = &output.cleanup_error {
        app.play_failure_feedback();
        match output.llm_route {
            Some(route) => warn!("LLM cleanup failed: route={route:?} {error}"),
            None => warn!("LLM cleanup failed: {error}"),
        }
        app.emit_notice("cleanup-fallback", notice);
    }
}

impl TranscribeAction {
    /// `stt_language` is the session's profile speech language, if any.
    pub(crate) fn start(
        app: &AppHandle,
        binding_id: &str,
        settings: &AppSettings,
        stt_language: Option<&str>,
    ) {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        // Load model in the background
        let tm = app.state::<Arc<TranscriptionManager>>();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        // Timing only: remember whether the model was resident before the load
        // below is kicked off, so cold and warm utterances can be told apart.
        dictation_timing::note_recording_start(tm.is_model_loaded());

        // Load ASR model and VAD model in parallel
        let kickoff_started = Instant::now();
        tm.initiate_model_load();
        let rm_clone = Arc::clone(&rm);
        std::thread::spawn(move || {
            if let Err(e) = rm_clone.preload_vad() {
                debug!("VAD pre-load failed: {}", e);
            }
        });
        let kickoff_elapsed = kickoff_started.elapsed();

        // Don't open the mic if nothing can transcribe the recording; the load
        // kicked off above fails and reports why.
        if !tm.is_model_loaded() {
            let selected_model = &settings.selected_model;
            if let Err(e) = app
                .state::<Arc<ModelManager>>()
                .get_model_path(selected_model)
            {
                warn!("Not starting recording: no model can transcribe it ({})", e);
                return;
            }
        }

        let binding_id = binding_id.to_string();
        let tray_started = Instant::now();
        set_tray_state(app, TrayIconState::Recording);
        let tray_elapsed = tray_started.elapsed();

        // Get the microphone mode to determine audio feedback timing
        let plan_started = Instant::now();
        let is_always_on = settings.always_on_microphone;

        let selected_model_info = app
            .state::<Arc<ModelManager>>()
            .get_model_info(&settings.selected_model);

        // Use the app-facing model capability as the single pre-recording source
        // for live streaming decisions. Unknown support is represented as false
        // until the model registry is updated by discovery or runtime load.
        let model_supports_streaming = selected_model_info
            .as_ref()
            .map(|m| m.supports_streaming)
            .unwrap_or(false);
        let vad_policy = if !settings.vad_enabled {
            VadPolicy::Disabled
        } else if model_supports_streaming {
            VadPolicy::Streaming
        } else {
            VadPolicy::Offline
        };
        if model_supports_streaming {
            tm.start_stream_with_language(stt_language.map(str::to_string));
        }
        let plan_elapsed = plan_started.elapsed();

        // Sizing the overlay follows the same advertised capability. A model that
        // doesn't stream (or whose capability is not known yet) gets the compact
        // pill instead of an oversized transparent live window.
        let overlay_started = Instant::now();
        match settings.overlay_style {
            OverlayStyle::Live if model_supports_streaming => utils::show_streaming_overlay(app),
            OverlayStyle::Live | OverlayStyle::Minimal => show_recording_overlay(app),
            OverlayStyle::None => {} // show_overlay_state no-ops on None anyway
        }
        // Everything above runs before capture can begin, so each span here is
        // added keypress->capture latency.
        debug!(
            "start-path pre-recording steps: model_kickoff={:?} tray={:?} settings+stream_plan={:?} overlay={:?}",
            kickoff_elapsed,
            tray_elapsed,
            plan_elapsed,
            overlay_started.elapsed()
        );
        debug!("Microphone mode - always_on: {}", is_always_on);

        let mut recording_error: Option<String> = None;
        let recording_start_time = Instant::now();
        match rm.try_start_recording(&binding_id, vad_policy) {
            Ok(readiness) => {
                debug!(
                    "Recording request accepted in {:?}; waiting for first microphone samples",
                    recording_start_time.elapsed()
                );
                let generation = readiness.generation();
                let app_clone = app.clone();
                let rm_clone = Arc::clone(&rm);
                std::thread::spawn(move || {
                    if !readiness.wait() {
                        debug!("Microphone readiness wait ended without receiving samples");
                        return;
                    }

                    // Development-only preview hook for evaluating the brief
                    // arming animation on hardware that normally starts too fast
                    // to make it visible.
                    #[cfg(debug_assertions)]
                    if let Ok(delay_ms) = std::env::var("HANDY_DEBUG_MIC_READY_DELAY_MS")
                        .unwrap_or_default()
                        .parse::<u64>()
                    {
                        let delay_ms = delay_ms.min(10_000);
                        if delay_ms > 0 {
                            debug!("Delaying microphone-ready cue by {delay_ms}ms for UI preview");
                            std::thread::sleep(Duration::from_millis(delay_ms));
                        }
                    }

                    if !rm_clone.is_recording_readiness_current(generation) {
                        debug!("Microphone became ready for an inactive recording");
                        return;
                    }

                    debug!("Microphone is receiving samples; recording is ready");
                    utils::emit_recording_ready(&app_clone);

                    // The start chime is a readiness cue, so it must follow the
                    // first real input callback rather than Stream::play() or a
                    // fixed delay. The helper returns immediately when feedback
                    // is disabled; mute still follows the same readiness point.
                    if rm_clone.is_recording_readiness_current(generation) {
                        play_feedback_sound_blocking(&app_clone, SoundType::Start);
                    }
                    if rm_clone.is_recording_readiness_current(generation) {
                        rm_clone.apply_mute();
                    }
                });
            }
            Err(e) => {
                debug!("Failed to start recording: {}", e);
                recording_error = Some(e);
            }
        }

        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            tm.cancel_stream();
            utils::hide_recording_overlay(app);
            set_tray_state(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                let error_type = if is_microphone_access_denied(&err) {
                    "microphone_permission_denied"
                } else if is_no_input_device_error(&err) {
                    "no_input_device"
                } else {
                    "unknown"
                };
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: error_type.to_string(),
                        detail: Some(err),
                    },
                );
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    pub(crate) fn stop(app: &AppHandle, binding_id: &str, session: DictationSession) {
        // The timing clock starts at the accepted stop event, with the mode
        // and language from recording start.
        let timer =
            OperationTimer::begin_stop(session.mode().timing_mode(), session.language.clone());
        session.note_timing_profile(&timer);
        let stop_entry = Instant::now();

        // Prevent a slow microphone from emitting a ready event or start chime
        // after the user has already requested stop.
        app.state::<Arc<AudioRecordingManager>>()
            .invalidate_recording_readiness();

        // Unregister the cancel shortcut when transcription stops
        if session.mode() != WorkflowMode::Assistant {
            shortcut::unregister_cancel_shortcut(app);
        }

        let stop_time = Instant::now();
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        set_tray_state(app, TrayIconState::Transcribing);
        // Stop should give immediate visual feedback. Live streaming can keep
        // the larger panel, but it still switches from listening to a working
        // spinner while the stream finalizes. Non-streaming paths use the
        // compact transcribing pill (None no-ops in show_*).
        let style = session.overlay_style;
        // Capture this before finalizing the stream so every later working state
        // targets the same overlay that was shown for this transcription.
        let use_streaming_overlay = should_use_streaming_overlay(style, tm.is_streaming());
        if use_streaming_overlay {
            tm.emit_stream_working(StreamWorkKind::Transcribing);
        } else {
            show_transcribing_overlay(app);
        }

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        let cancel_generation = rm.cancel_generation();

        timer.add_since(Stage::StopSync, stop_entry);
        let spawned_at = Instant::now();
        tauri::async_runtime::spawn(async move {
            timer.add_since(Stage::TaskSched, spawned_at);
            let _guard = FinishGuard(ah.clone(), Arc::clone(&tm));
            // Drop assistant cancel ownership before notifying the coordinator.
            let session = session;
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            let stop_recording_time = Instant::now();
            let stopped = rm.stop_recording(&binding_id, cancel_generation);
            timer.add_since(Stage::AudioFinalize, stop_recording_time);
            if let Some(samples) = stopped {
                timer.set_audio_samples(samples.len());
                debug!(
                    "Recording stopped and samples retrieved in {:?}, sample count: {}",
                    stop_recording_time.elapsed(),
                    samples.len()
                );

                if rm.was_cancelled_since(cancel_generation) {
                    debug!("Transcription operation cancelled after recording stop");
                    timer.finish(Outcome::Cancelled);
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    return;
                }

                if samples.is_empty() {
                    debug!("Recording produced no audio samples; skipping persistence");
                    timer.finish(Outcome::EmptyAudio);
                    // Tear down any streaming worker so its channel doesn't leak
                    // and block the next start_stream.
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                } else {
                    // Save WAV concurrently with transcription
                    let sample_count = samples.len();
                    let file_name = format!("handy-{}.wav", chrono::Utc::now().timestamp());
                    let wav_path = hm.recordings_dir().join(&file_name);
                    let wav_path_for_verify = wav_path.clone();
                    let wav_handle = if session.mode() == WorkflowMode::Assistant {
                        None
                    } else {
                        let samples_for_wav = samples.clone();
                        Some(tauri::async_runtime::spawn_blocking(move || {
                            crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav)
                        }))
                    };

                    // Transcribe concurrently with WAV save. If a live stream was
                    // running, finalize it and use its text (all audio was already
                    // fed to the stream); otherwise batch-transcribe the samples.
                    timer.note_engine_pending();
                    dictation_timing::reset_text_cleanup();
                    let transcription_time = Instant::now();
                    let transcription_result = match tm.finalize_stream() {
                        // A finalized stream with usable text wins. An empty result
                        // (no active stream, produced nothing, or the stream failed
                        // or its worker crashed) falls back to a full batch
                        // transcription of the same audio. A cancelled finalize is
                        // surfaced instead, so a cancel never starts a batch run.
                        Ok(Some((text, language))) if !text.trim().is_empty() => {
                            timer.set_stt_path(SttPath::StreamFinalize);
                            Ok((text, language))
                        }
                        Ok(_) => {
                            timer.set_stt_path(SttPath::Batch);
                            tm.transcribe_with_language_for(
                                samples,
                                session.stt_language.as_deref(),
                            )
                        }
                        Err(err) => Err(err),
                    };
                    timer.add_stt(transcription_time.elapsed());

                    // Await WAV save and verify
                    let wav_wait_started = Instant::now();
                    let wav_saved = if let Some(wav_handle) = wav_handle {
                        match wav_handle.await {
                            Ok(Ok(())) => {
                                match crate::audio_toolkit::verify_wav_file(
                                    &wav_path_for_verify,
                                    sample_count,
                                ) {
                                    Ok(()) => true,
                                    Err(e) => {
                                        error!("WAV verification failed: {}", e);
                                        false
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                error!("Failed to save WAV file: {}", e);
                                false
                            }
                            Err(e) => {
                                error!("WAV save task panicked: {}", e);
                                false
                            }
                        }
                    } else {
                        false
                    };
                    timer.add_since(Stage::WavWait, wav_wait_started);

                    if rm.was_cancelled_since(cancel_generation) {
                        debug!("Transcription operation cancelled before output handling");
                        timer.finish(Outcome::Cancelled);
                        utils::hide_recording_overlay(&ah);
                        set_tray_state(&ah, TrayIconState::Idle);
                        return;
                    }

                    match transcription_result {
                        Ok((transcription, output_language)) => {
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                utils::redact_text(&transcription)
                            );

                            let session = match session.into_assistant() {
                                Ok(session) => {
                                    if use_streaming_overlay {
                                        tm.emit_stream_working(StreamWorkKind::Polishing);
                                    } else {
                                        show_processing_overlay(&ah);
                                    }
                                    let probe: crate::assistant::CancelProbe =
                                        Arc::new(move || rm.was_cancelled_since(cancel_generation));
                                    crate::assistant::finish(
                                        &ah,
                                        session,
                                        &transcription,
                                        &timer,
                                        probe,
                                    )
                                    .await;
                                    utils::hide_recording_overlay(&ah);
                                    set_tray_state(&ah, TrayIconState::Idle);
                                    return;
                                }
                                Err(session) => session,
                            };
                            if session.mode() == WorkflowMode::Cleanup {
                                if use_streaming_overlay {
                                    tm.emit_stream_working(StreamWorkKind::Polishing);
                                } else {
                                    show_processing_overlay(&ah);
                                }
                            }
                            let cleanup_started = Instant::now();
                            let processed = complete_unless_cancelled(
                                process_transcription_output_with_language(
                                    &session,
                                    &transcription,
                                    &output_language,
                                ),
                                || rm.was_cancelled_since(cancel_generation),
                            )
                            .await;
                            if session.mode() == WorkflowMode::Cleanup
                                && !prompts::is_blank_transcription(&transcription)
                            {
                                timer.add_since(Stage::Llm, cleanup_started);
                            }
                            if let Some(processed) = &processed {
                                note_cleanup_route(&timer, processed);
                            }
                            let Some(mut processed) = processed else {
                                debug!("Transcription operation cancelled during output handling");
                                timer.finish(Outcome::Cancelled);
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            };

                            if rm.was_cancelled_since(cancel_generation) {
                                debug!("Transcription operation cancelled before paste");
                                timer.finish(Outcome::Cancelled);
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            // Save to history if WAV was saved
                            let mut history_saved = false;
                            if wav_saved {
                                let history_started = Instant::now();
                                match hm.save_entry(
                                    file_name,
                                    transcription,
                                    session.mode() == WorkflowMode::Cleanup,
                                    processed.post_processed_text.clone(),
                                    processed.post_process_prompt.clone(),
                                    session.profile.as_ref(),
                                ) {
                                    Ok(_) => history_saved = true,
                                    Err(err) => error!("Failed to save history entry: {}", err),
                                }
                                timer.add_since(Stage::History, history_started);
                            }

                            if processed.final_text.is_empty() {
                                timer.finish(Outcome::EmptyOutput);
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                            } else {
                                let ah_clone = ah.clone();
                                let paste_time = Instant::now();
                                let paste_outcome = processed.paste_outcome();
                                let final_text = std::mem::take(&mut processed.final_text);
                                let rm_for_paste = Arc::clone(&rm);
                                let timer_for_paste = timer.clone();
                                let main_sched_started = Instant::now();
                                ah.run_on_main_thread(move || {
                                    timer_for_paste.add_since(Stage::MainSched, main_sched_started);
                                    if rm_for_paste.was_cancelled_since(cancel_generation) {
                                        debug!("Transcription operation cancelled before paste");
                                        timer_for_paste.finish(Outcome::Cancelled);
                                        utils::hide_recording_overlay(&ah_clone);
                                        set_tray_state(&ah_clone, TrayIconState::Idle);
                                        return;
                                    }

                                    timer_for_paste.note_paste_call();
                                    let paste_started = Instant::now();
                                    let paste_result = utils::paste(final_text, ah_clone.clone());
                                    timer_for_paste.add_since(Stage::Paste, paste_started);
                                    report_transcription_delivery(
                                        &ah_clone,
                                        &processed,
                                        history_saved,
                                        &paste_result,
                                    );
                                    match paste_result {
                                        Ok(_) => {
                                            timer_for_paste.finish(paste_outcome);
                                            debug!(
                                                "Text pasted successfully in {:?}",
                                                paste_time.elapsed()
                                            )
                                        }
                                        Err(e) => {
                                            timer_for_paste.finish(Outcome::PasteError);
                                            error!("Failed to paste transcription: {}", e);
                                        }
                                    }
                                    utils::hide_recording_overlay(&ah_clone);
                                    set_tray_state(&ah_clone, TrayIconState::Idle);
                                })
                                .unwrap_or_else(|e| {
                                    timer.finish(Outcome::MainThreadError);
                                    error!("Failed to run paste on main thread: {:?}", e);
                                    utils::hide_recording_overlay(&ah);
                                    set_tray_state(&ah, TrayIconState::Idle);
                                });
                            }
                        }
                        Err(err) => {
                            if rm.was_cancelled_since(cancel_generation) {
                                debug!(
                                    "Transcription operation cancelled after transcription error"
                                );
                                timer.finish(Outcome::Cancelled);
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            timer.finish(Outcome::TranscriptionError);
                            error!("Transcription failed: {}", err);
                            // Surface the failure to the UI (toast). The full
                            // message is also in handy.log via the line above.
                            let _ = ah.emit("transcription-error", err.to_string());
                            // Save entry with empty text so user can retry
                            if wav_saved {
                                if let Err(save_err) = hm.save_entry(
                                    file_name,
                                    String::new(),
                                    session.mode() == WorkflowMode::Cleanup,
                                    None,
                                    None,
                                    session.profile.as_ref(),
                                ) {
                                    error!("Failed to save failed history entry: {}", save_err);
                                }
                            }
                            utils::hide_recording_overlay(&ah);
                            set_tray_state(&ah, TrayIconState::Idle);
                        }
                    }
                }
            } else {
                debug!("No samples retrieved from recording stop");
                timer.finish(Outcome::NoSamples);
                // Tear down any streaming worker so its channel doesn't leak.
                tm.cancel_stream();
                utils::hide_recording_overlay(&ah);
                set_tray_state(&ah, TrayIconState::Idle);
            }
        });

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});

#[cfg(test)]
mod tests {
    #[derive(Default)]
    struct FakeCleanupReporter {
        events: std::cell::RefCell<Vec<(String, super::CleanupFallbackNotice)>>,
        cues: std::cell::Cell<usize>,
        paste_events: std::cell::RefCell<Vec<super::PasteFailureNotice>>,
    }
    impl super::CleanupFailureReporter for FakeCleanupReporter {
        fn play_failure_feedback(&self) {
            self.cues.set(self.cues.get() + 1);
        }
        fn emit_notice(&self, event: &str, notice: super::CleanupFallbackNotice) {
            self.events.borrow_mut().push((event.into(), notice));
        }
        fn emit_paste_failure(&self, notice: super::PasteFailureNotice) {
            self.paste_events.borrow_mut().push(notice);
        }
    }
    #[test]
    fn cleanup_and_paste_failure_reports_one_notice_with_saved_raw_history() {
        for history_saved in [true, false] {
            for cleanup_failed in [true, false] {
                let reporter = FakeCleanupReporter::default();
                let output = super::ProcessedTranscription {
                    final_text: "raw transcription".into(),
                    post_processed_text: None,
                    post_process_prompt: None,
                    cleanup_error: cleanup_failed.then(|| "llm_timeout".into()),
                    cleanup_skipped: None,
                    llm_route: None,
                };
                super::report_transcription_delivery(
                    &reporter,
                    &output,
                    history_saved,
                    &Err("paste refused".into()),
                );
                assert!(reporter.events.borrow().is_empty());
                assert_eq!(
                    &*reporter.paste_events.borrow(),
                    &[super::PasteFailureNotice {
                        cleanup_failed,
                        raw_history_available: history_saved && cleanup_failed
                    }]
                );
                assert_eq!(reporter.cues.get(), 1);
                assert_eq!(
                    serde_json::to_value(reporter.paste_events.borrow()[0]).unwrap()
                        ["cleanup_failed"],
                    serde_json::json!(cleanup_failed)
                );
            }
        }
    }
    #[test]
    fn cleanup_with_no_paste_reports_no_delivery_and_actual_history_availability() {
        for history_saved in [true, false] {
            let reporter = FakeCleanupReporter::default();
            let output = super::ProcessedTranscription {
                final_text: "raw transcription".into(),
                post_processed_text: None,
                post_process_prompt: None,
                cleanup_error: Some("llm_timeout".into()),
                cleanup_skipped: None,
                llm_route: None,
            };
            super::report_transcription_delivery(&reporter, &output, history_saved, &Ok(false));
            assert_eq!(
                &*reporter.events.borrow(),
                &[(
                    "cleanup-fallback".into(),
                    super::CleanupFallbackNotice {
                        history_available: history_saved,
                        raw_delivered: false
                    }
                )]
            );
            assert!(reporter.paste_events.borrow().is_empty());
            assert_eq!(reporter.cues.get(), 1);
        }
    }
    #[test]
    fn cleanup_fallback_notice_is_not_transcription_failure() {
        let output = super::ProcessedTranscription {
            final_text: "raw transcription".into(),
            post_processed_text: None,
            post_process_prompt: None,
            cleanup_error: Some("llm_timeout".into()),
            cleanup_skipped: None,
            llm_route: None,
        };
        let reporter = FakeCleanupReporter::default();
        let notice = super::CleanupFallbackNotice {
            history_available: false,
            raw_delivered: true,
        };
        super::report_cleanup_failure(&reporter, &output, notice);
        assert_eq!(
            &*reporter.events.borrow(),
            &[("cleanup-fallback".into(), notice)]
        );
        assert_eq!(reporter.cues.get(), 1);
        assert_eq!(output.final_text, "raw transcription");
        let success = super::ProcessedTranscription {
            cleanup_error: None,
            ..output
        };
        super::report_cleanup_failure(&reporter, &success, notice);
        assert_eq!(reporter.events.borrow().len(), 1);
        assert_eq!(reporter.cues.get(), 1);
    }
    #[test]
    fn cleanup_fallback_notice_reports_saved_history_without_claiming_paste() {
        let reporter = FakeCleanupReporter::default();
        let output = super::ProcessedTranscription {
            final_text: "raw transcription".into(),
            post_processed_text: None,
            post_process_prompt: None,
            cleanup_error: Some("llm_timeout".into()),
            cleanup_skipped: None,
            llm_route: None,
        };
        let notice = super::CleanupFallbackNotice {
            history_available: true,
            raw_delivered: false,
        };
        super::report_cleanup_failure(&reporter, &output, notice);
        assert_eq!(
            &*reporter.events.borrow(),
            &[("cleanup-fallback".into(), notice)]
        );
    }
    use super::*;
    use crate::llm_client::test_http::{completion, FakeServer, Step};
    use crate::llm_client::{ConfigError, LlmError, OutputError};
    use crate::settings::OverlayStyle;
    use std::future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[tokio::test]
    async fn handy009_r2_skipped_cleanup_delivers_exact_raw_without_failure_events() {
        for (language, evidence, reason) in [
            (
                "auto",
                OutputLanguageEvidence::Unknown,
                prompts::CleanupSkipReason::UnknownOutputLanguage,
            ),
            (
                "auto",
                OutputLanguageEvidence::ModelDetected("pl".into()),
                prompts::CleanupSkipReason::NonEnglishOutput,
            ),
            (
                "pl",
                OutputLanguageEvidence::ModelDetected("pl".into()),
                prompts::CleanupSkipReason::NonEnglishOutput,
            ),
        ] {
            let server = FakeServer::new(vec![]).await;
            let mut settings = configured_settings(&server.url);
            settings.selected_language = language.into();
            settings
                .post_process_models
                .insert("custom".into(), "S1-mini".into());
            let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
            let raw = "  Call Paweł, then TADOQuery.Open.\n";
            let output = process_transcription_output_with_language(&session, raw, &evidence).await;
            assert_eq!(output.final_text, raw);
            assert_eq!(output.cleanup_skipped, Some(reason));
            assert!(output.cleanup_error.is_none());
            assert!(output.post_processed_text.is_none());
            assert!(output.post_process_prompt.is_none());
            assert_eq!(
                serde_json::to_value(output.paste_outcome()).unwrap(),
                "cleanup_skipped"
            );
            assert!(server.requests.lock().unwrap().is_empty());
            let reporter = FakeCleanupReporter::default();
            for delivered in [true, false] {
                report_transcription_delivery(&reporter, &output, true, &Ok(delivered));
            }
            assert_eq!(reporter.cues.get(), 0);
            assert!(reporter.events.borrow().is_empty());
            assert!(reporter.paste_events.borrow().is_empty());
        }
    }

    #[tokio::test]
    async fn handy009_r3_stale_translation_setting_skips_s1_without_failure_events() {
        let server = FakeServer::new(vec![]).await;
        let mut settings = configured_settings(&server.url);
        settings.translate_to_english = true;
        settings.selected_model = "small".into();
        // Switching to Turbo leaves the hidden translation setting enabled.
        settings.selected_model = "turbo".into();
        settings.selected_language = "pl".into();
        settings
            .post_process_models
            .insert("custom".into(), "S1-mini".into());
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let raw = "Proszę wysłać raport jutro.";
        let output = process_transcription_output_with_language(
            &session,
            raw,
            &OutputLanguageEvidence::UserSelected("pl".into()),
        )
        .await;
        assert_eq!(output.final_text, raw);
        assert_eq!(
            output.cleanup_skipped,
            Some(prompts::CleanupSkipReason::NonEnglishOutput)
        );
        assert!(output.cleanup_error.is_none());
        assert!(output.post_processed_text.is_none());
        assert!(output.post_process_prompt.is_none());
        assert!(server.requests.lock().unwrap().is_empty());
        let reporter = FakeCleanupReporter::default();
        for delivered in [true, false] {
            report_transcription_delivery(&reporter, &output, true, &Ok(delivered));
        }
        assert_eq!(reporter.cues.get(), 0);
        assert!(reporter.events.borrow().is_empty());
        assert!(reporter.paste_events.borrow().is_empty());
    }

    #[tokio::test]
    async fn handy009_r3_translated_cleanup_does_not_record_unsent_custom_prompt() {
        let server = FakeServer::new(vec![Step::Respond(
            200,
            completion(Some("Please send the report tomorrow."), "stop"),
        )])
        .await;
        let mut settings = configured_settings(&server.url);
        settings.selected_language = "pl".into();
        settings.translate_to_english = true;
        settings
            .post_process_models
            .insert("custom".into(), "S1-mini".into());
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let output = process_transcription_output_with_language(
            &session,
            "please send the report tomorrow",
            &OutputLanguageEvidence::TranslatedToEnglish,
        )
        .await;
        assert_eq!(output.final_text, "Please send the report tomorrow.");
        assert_eq!(
            output.post_processed_text.as_deref(),
            Some("Please send the report tomorrow.")
        );
        assert!(output.cleanup_error.is_none());
        assert!(output.cleanup_skipped.is_none());
        assert!(output.post_process_prompt.is_none());
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn handy009_r2_auto_language_evidence_reaches_cleanup_pipeline() {
        let server = FakeServer::new(vec![Step::Respond(
            200,
            completion(Some("Sounds good."), "stop"),
        )])
        .await;
        let mut settings = configured_settings(&server.url);
        settings.selected_language = "auto".into();
        settings
            .post_process_models
            .insert("custom".into(), "S1-mini".into());
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let output = process_transcription_output_with_language(
            &session,
            "sounds good",
            &OutputLanguageEvidence::ModelDetected("en".into()),
        )
        .await;
        assert_eq!(output.final_text, "Sounds good.");
        assert!(output.cleanup_error.is_none());
        assert!(output.cleanup_skipped.is_none());
        assert_eq!(output.paste_outcome(), Outcome::Pasted);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    fn configured_settings(url: &str) -> AppSettings {
        let mut settings = crate::settings::get_default_settings();
        settings.post_process_provider_id = "custom".into();
        settings.post_process_providers = vec![crate::llm_client::test_http::provider(url)];
        settings
            .post_process_models
            .insert("custom".into(), "chosen-model".into());
        settings
    }

    #[test]
    fn workflow_bindings_and_timing_keep_compatibility() {
        for (mode, id, timing) in [
            (WorkflowMode::Raw, "transcribe", Mode::Raw),
            (
                WorkflowMode::Cleanup,
                "transcribe_with_post_process",
                Mode::Cleanup,
            ),
        ] {
            assert_eq!(WorkflowMode::from_binding_id(id), Some(mode));
            assert_eq!(mode.binding_id(), id);
            assert_eq!(mode.timing_mode(), timing);
        }
        assert_eq!(WorkflowMode::from_binding_id("cancel"), None);
    }

    #[tokio::test]
    async fn cleanup_fallback_has_distinct_paste_timing_outcome() {
        let session = DictationSession::snapshot(
            WorkflowMode::Cleanup,
            &configured_settings("http://unused.invalid"),
        )
        .unwrap();
        let output = process_transcription_output_with(&session, "recoverable raw", |_, _| {
            future::ready(Err(LlmError::Timeout))
        })
        .await;
        assert_eq!(output.final_text, "recoverable raw");
        assert_eq!(
            serde_json::to_value(output.paste_outcome()).unwrap(),
            "cleanup_fallback"
        );
    }

    #[tokio::test]
    async fn invalid_cleanup_config_has_fallback_paste_timing_outcome() {
        let mut settings = configured_settings("http://unused.invalid");
        settings.post_process_models.clear();
        assert!(prompts::CleanupRequest::from_settings(&settings).is_err());
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();

        let output = process_transcription_output(&session, "recoverable raw").await;

        assert_eq!(output.final_text, "recoverable raw");
        assert_eq!(output.cleanup_error.as_deref(), Some("llm_config"));
        assert_eq!(output.paste_outcome(), Outcome::CleanupFallback);
    }

    #[tokio::test]
    async fn successful_cleanup_keeps_pasted_timing_outcome() {
        let session = DictationSession::snapshot(
            WorkflowMode::Cleanup,
            &configured_settings("http://unused.invalid"),
        )
        .unwrap();
        let output = process_transcription_output_with(&session, "raw", |_, _| {
            future::ready(Ok(prompts::CleanupOutcome::Cleaned {
                text: "Cleaned.".into(),
                route: dictation_timing::LlmRoute::Default,
                prompt_sent: true,
            }))
        })
        .await;
        assert_eq!(output.final_text, "Cleaned.");
        assert_eq!(output.paste_outcome(), Outcome::Pasted);
    }

    #[tokio::test]
    async fn echoed_formatting_is_repaired_for_delivery_and_history_only_in_prompted_cleanup() {
        let session = DictationSession::snapshot(
            WorkflowMode::Cleanup,
            &configured_settings("http://unused.invalid"),
        )
        .unwrap();
        for prompt_sent in [true, false] {
            let output = process_transcription_output_with(
                &session,
                "First. New paragraph, second.",
                |_, _| {
                    future::ready(Ok(prompts::CleanupOutcome::Cleaned {
                        text: "First.\nNew paragraph, second.".into(),
                        route: dictation_timing::LlmRoute::Default,
                        prompt_sent,
                    }))
                },
            )
            .await;
            let expected = if prompt_sent {
                "First.\n\nsecond."
            } else {
                "First.\nNew paragraph, second."
            };
            assert_eq!(output.final_text, expected);
            assert_eq!(output.post_processed_text.as_deref(), Some(expected));
            assert!(output.cleanup_error.is_none());
        }
    }

    #[tokio::test]
    async fn raw_mode_never_calls_client_even_when_post_process_enabled() {
        let mut settings = configured_settings("http://unused.invalid");
        settings.post_process_enabled = true;
        settings.custom_words = vec!["TFDQuery".into(), "TADOQuery.Open".into()];
        let session = DictationSession::snapshot(WorkflowMode::Raw, &settings).unwrap();
        let raw = "Call TADOQuery.Open on TFDQuery.";
        let output = process_transcription_output_with(&session, raw, |_, _| {
            panic!("raw mode must never invoke an LLM client");
            #[allow(unreachable_code)]
            future::ready(Ok(prompts::CleanupOutcome::Cleaned {
                text: String::new(),
                route: dictation_timing::LlmRoute::Default,
                prompt_sent: true,
            }))
        })
        .await;
        assert_eq!(output.final_text, raw);
        assert!(output.post_processed_text.is_none());
        assert!(output.post_process_prompt.is_none());
        assert!(output.cleanup_error.is_none());
        assert_eq!(output.paste_outcome(), Outcome::Pasted);
    }

    #[tokio::test]
    async fn cleanup_uses_start_snapshot_of_selected_provider_model_prompt_and_options() {
        let selected = FakeServer::new(vec![Step::Respond(
            200,
            completion(Some(r#"{"transcription":"Call TADOQuery.Open."}"#), "stop"),
        )])
        .await;
        let other = FakeServer::new(vec![]).await;
        let mut settings = configured_settings(&selected.url);
        let mut other_provider = crate::llm_client::test_http::provider(&other.url);
        other_provider.id = "other".into();
        settings.post_process_providers.push(other_provider);
        settings
            .post_process_models
            .insert("other".into(), "other-model".into());
        settings.custom_words = vec!["TADOQuery.Open".into()];
        settings.post_process_request_options.temperature = Some(0.25);
        settings.post_process_request_options.max_output_tokens = Some(123);
        let prompt = settings
            .post_process_prompts
            .iter()
            .find(|p| Some(&p.id) == settings.post_process_selected_prompt_id.as_ref())
            .unwrap()
            .prompt
            .clone();
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();

        // All edits affect the next utterance, including the enabled flag.
        settings.post_process_provider_id = "other".into();
        settings.post_process_enabled = false;
        settings
            .post_process_models
            .insert("custom".into(), "edited-model".into());
        settings.post_process_request_options.temperature = Some(1.0);
        settings.post_process_request_options.max_output_tokens = Some(999);
        settings.post_process_prompts.clear();
        settings.custom_words.clear();
        let output = process_transcription_output(&session, "call TADOQuery.Open").await;
        assert_eq!(output.final_text, "Call TADOQuery.Open.");
        assert_eq!(
            output.post_processed_text.as_deref(),
            Some("Call TADOQuery.Open.")
        );
        assert_eq!(output.post_process_prompt.as_deref(), Some(prompt.as_str()));
        assert!(output.cleanup_error.is_none());
        assert!(other.requests.lock().unwrap().is_empty());
        let requests = selected.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["model"], "chosen-model");
        assert_eq!(requests[0]["temperature"], 0.25);
        assert_eq!(requests[0]["max_tokens"], 123);
        assert!(requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains(&prompt));
        assert!(requests[0]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("<vocabulary>\nTADOQuery.Open\n</vocabulary>"));
    }

    #[tokio::test]
    async fn cleanup_provider_and_invalid_output_failures_retain_raw_for_delivery_and_history() {
        for (status, body, code) in [
            (503, "provider unavailable".into(), "llm_provider"),
            (200, completion(Some(""), "stop"), "llm_invalid_output"),
            (
                200,
                completion(Some("<think>private</think>"), "stop"),
                "llm_invalid_output",
            ),
            (
                200,
                completion(Some("partial"), "length"),
                "llm_invalid_output",
            ),
        ] {
            let server = FakeServer::new(vec![Step::Respond(status, body)]).await;
            let session = DictationSession::snapshot(
                WorkflowMode::Cleanup,
                &configured_settings(&server.url),
            )
            .unwrap();
            let raw = "TFDQuery and TADOQuery.Open.";
            let output = process_transcription_output(&session, raw).await;
            assert_eq!(output.final_text, raw);
            assert!(output.post_processed_text.is_none());
            assert!(output.post_process_prompt.is_none());
            assert!(output.cleanup_error.as_deref().unwrap().starts_with(code));
            assert_eq!(
                server.requests.lock().unwrap().len(),
                1,
                "never retry failed generation"
            );
        }
    }

    #[tokio::test]
    async fn cleanup_timeout_retains_raw_and_reports_failure() {
        let server = FakeServer::new(vec![Step::StallBeforeHeaders]).await;
        let mut settings = configured_settings(&server.url);
        settings.post_process_request_options.timeout_secs = 1;
        settings.post_process_request_options.connect_timeout_secs = 1;
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let output = process_transcription_output(&session, "recoverable raw").await;
        assert_eq!(output.final_text, "recoverable raw");
        assert_eq!(output.cleanup_error.as_deref(), Some("llm_timeout"));
        assert!(output.post_processed_text.is_none());
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_config_is_snapshotted_and_blank_stt_never_calls_client() {
        let mut settings = configured_settings("http://unused.invalid");
        settings.post_process_models.clear();
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        settings
            .post_process_models
            .insert("custom".into(), "fixed-later".into());
        let output = process_transcription_output_with(&session, "raw", |_, _| {
            panic!("a later configuration fix must not change this session");
            #[allow(unreachable_code)]
            future::ready(Ok(prompts::CleanupOutcome::Cleaned {
                text: String::new(),
                route: dictation_timing::LlmRoute::Default,
                prompt_sent: true,
            }))
        })
        .await;
        assert_eq!(output.final_text, "raw");
        assert_eq!(output.cleanup_error.as_deref(), Some("llm_config"));

        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let output = process_transcription_output_with(&session, " \r\n", |_, _| {
            panic!("blank STT must not call the client");
            #[allow(unreachable_code)]
            future::ready(Err(LlmError::Config(ConfigError::NoModel)))
        })
        .await;
        assert_eq!(output.final_text, " \r\n");
        assert!(output.cleanup_error.is_none());
    }

    #[tokio::test]
    async fn cancellation_during_cleanup_drops_the_request_and_produces_no_output() {
        let session = DictationSession::snapshot(
            WorkflowMode::Cleanup,
            &configured_settings("http://unused.invalid"),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let dropped = AtomicBool::new(false);
        struct RequestGuard<'a>(&'a AtomicBool);
        impl Drop for RequestGuard<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let operation = process_transcription_output_with(&session, "spoken raw", |_, _| async {
            let _guard = RequestGuard(&dropped);
            // Cancel only after the real processing path has entered the request.
            cancelled.store(true, Ordering::Release);
            future::pending::<Result<prompts::CleanupOutcome, LlmError>>().await
        });
        let output =
            complete_unless_cancelled(operation, || cancelled.load(Ordering::Acquire)).await;
        assert!(
            output.is_none(),
            "no result may reach history/paste after cancellation"
        );
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn every_typed_cleanup_error_keeps_exact_deterministic_text() {
        let session = DictationSession::snapshot(
            WorkflowMode::Cleanup,
            &configured_settings("http://unused.invalid"),
        )
        .unwrap();
        for error in [
            LlmError::Config(ConfigError::NoModel),
            LlmError::Timeout,
            LlmError::Transport("offline".into()),
            LlmError::ResponseTooLarge,
            LlmError::InvalidOutput(OutputError::MalformedJson),
        ] {
            let output = process_transcription_output_with(&session, "TFDQuery.\n", |_, _| {
                future::ready(Err(error))
            })
            .await;
            assert_eq!(output.final_text, "TFDQuery.\n");
            assert!(output.cleanup_error.is_some());
            assert!(output.post_processed_text.is_none());
        }
    }

    #[test]
    fn completed_operation_returns_its_output() {
        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::ready("done"),
            || false,
        ));

        assert_eq!(result, Some("done"));
    }

    #[test]
    fn pending_operation_stops_after_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_thread = Arc::clone(&cancelled);
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            cancelled_for_thread.store(true, Ordering::Release);
        });

        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::pending::<()>(),
            || cancelled.load(Ordering::Acquire),
        ));

        cancel_thread.join().unwrap();
        assert_eq!(result, None);
    }

    // -----------------------------------------------------------------------
    // HANDY-008: profile sessions
    // -----------------------------------------------------------------------

    fn custom_profile(
        id: &str,
        kind: crate::settings::ProfileKind,
        model: Option<crate::settings::LlmEndpoint>,
        speech_language: Option<&str>,
    ) -> crate::settings::ProcessingProfile {
        crate::settings::ProcessingProfile {
            id: id.into(),
            name: format!("Profile {id}"),
            kind,
            prompt_id: Some("default_improve_transcriptions".into()),
            model,
            speech_language: speech_language.map(Into::into),
            english_cleanup: None,
        }
    }

    fn builtin_cleanup(settings: &mut AppSettings) -> &mut crate::settings::ProcessingProfile {
        settings
            .processing_profiles
            .iter_mut()
            .find(|p| p.id == crate::settings::BUILTIN_CLEANUP_PROFILE_ID)
            .unwrap()
    }

    fn history_entry(
        profile_id: Option<&str>,
        requested: bool,
    ) -> crate::managers::history::HistoryEntry {
        crate::managers::history::HistoryEntry {
            id: 1,
            file_name: "handy-1.wav".into(),
            timestamp: 1,
            saved: false,
            title: "Recording".into(),
            transcription_text: String::new(),
            post_processed_text: None,
            post_process_prompt: None,
            post_process_requested: requested,
            profile_id: profile_id.map(Into::into),
            profile_name: None,
        }
    }

    #[test]
    fn handy008_for_binding_resolves_each_binding_kind() {
        use crate::settings::ProfileKind;
        let mut settings = configured_settings("http://unused.invalid");
        settings
            .processing_profiles
            .push(custom_profile("pd", ProfileKind::Dictation, None, None));
        settings.processing_profiles.push(custom_profile(
            "pc",
            ProfileKind::SelectionCommand,
            None,
            None,
        ));
        let raw = DictationSession::for_binding("transcribe", &settings).unwrap();
        assert_eq!(raw.mode(), WorkflowMode::Raw);
        assert_eq!(
            (raw.profile.clone(), raw.stt_language.clone()),
            (None, None)
        );
        let cleanup =
            DictationSession::for_binding("transcribe_with_post_process", &settings).unwrap();
        assert_eq!(cleanup.mode(), WorkflowMode::Cleanup);
        assert_eq!(
            cleanup.profile,
            Some(crate::settings::ProfileRef {
                id: crate::settings::BUILTIN_CLEANUP_PROFILE_ID.into(),
                name: String::new(),
                builtin: true
            })
        );
        let custom = DictationSession::for_binding("dictation_profile_pd", &settings).unwrap();
        assert_eq!(custom.mode(), WorkflowMode::Cleanup);
        assert_eq!(custom.profile.as_ref().unwrap().id, "pd");
        assert!(!custom.profile.as_ref().unwrap().builtin);
        for capture_or_missing in [
            "assistant",
            "command_profile_pc",
            "dictation_profile_missing",
            "dictation_profile_pc",
            "cancel",
        ] {
            assert!(
                DictationSession::for_binding(capture_or_missing, &settings).is_none(),
                "{capture_or_missing}"
            );
        }
        assert_eq!(
            WorkflowMode::from_binding_id("dictation_profile_pd"),
            Some(WorkflowMode::Cleanup)
        );
        assert_eq!(
            WorkflowMode::from_binding_id("command_profile_pc"),
            Some(WorkflowMode::Assistant)
        );
    }

    #[test]
    fn handy008_stt_language_is_none_for_builtins_without_a_hint() {
        use crate::settings::ProfileKind;
        let mut settings = configured_settings("http://unused.invalid");
        settings.selected_language = "auto".into();
        let seeded = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        assert_eq!(seeded.stt_language, None);
        assert_eq!(seeded.language, "auto");
        builtin_cleanup(&mut settings).speech_language = Some("pl".into());
        let hinted = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        assert_eq!(hinted.stt_language.as_deref(), Some("pl"));
        assert_eq!(hinted.language, "pl");
        settings.processing_profiles.push(custom_profile(
            "pd",
            ProfileKind::Dictation,
            None,
            Some("de"),
        ));
        let custom = DictationSession::for_binding("dictation_profile_pd", &settings).unwrap();
        assert_eq!(custom.stt_language.as_deref(), Some("de"));
        let raw = DictationSession::snapshot(WorkflowMode::Raw, &settings).unwrap();
        assert_eq!(raw.stt_language, None);
        assert_eq!(raw.language, "auto");
        // The override is session-only: global settings are unchanged.
        assert_eq!(settings.selected_language, "auto");
    }

    #[tokio::test]
    async fn handy008_session_snapshot_survives_later_profile_edit() {
        use crate::settings::{LlmEndpoint, ProfileKind};
        let selected = FakeServer::new(vec![Step::Respond(
            200,
            completion(Some(r#"{"transcription":"Translated."}"#), "stop"),
        )])
        .await;
        let other = FakeServer::new(vec![]).await;
        let mut settings = configured_settings(&other.url);
        settings
            .post_process_prompts
            .push(crate::settings::LLMPrompt {
                id: "prompt_translate".into(),
                name: "Translate".into(),
                prompt: "Translate into German.".into(),
            });
        let mut profile = custom_profile(
            "pd",
            ProfileKind::Dictation,
            Some(LlmEndpoint {
                provider_id: "custom".into(),
                base_url: Some(selected.url.clone()),
                model: "start-model".into(),
            }),
            None,
        );
        profile.prompt_id = Some("prompt_translate".into());
        settings.processing_profiles.push(profile);
        let session = DictationSession::for_binding("dictation_profile_pd", &settings).unwrap();
        // Later edits, renames and deletes never affect a running session.
        settings.processing_profiles.retain(|p| p.id != "pd");
        settings
            .post_process_prompts
            .retain(|p| p.id != "prompt_translate");
        let output = process_transcription_output(&session, "Guten Tag").await;
        assert_eq!(output.final_text, "Translated.");
        assert_eq!(
            output.post_process_prompt.as_deref(),
            Some("Translate into German.")
        );
        assert_eq!(
            output.llm_route,
            Some(dictation_timing::LlmRoute::ProfileModel)
        );
        let requests = selected.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["model"], "start-model");
        assert!(other.requests.lock().unwrap().is_empty());
    }

    fn english_routing(settings: &mut AppSettings, english_url: &str) {
        settings.english_cleanup = crate::settings::EnglishCleanupSettings {
            enabled: true,
            base_url: english_url.into(),
            model: "S1-mini".into(),
        };
    }

    #[tokio::test]
    async fn handy008_english_route_success_records_no_unsent_prompt() {
        let default = FakeServer::new(vec![]).await;
        let english = FakeServer::new(vec![Step::Respond(
            200,
            completion(Some("Sounds good."), "stop"),
        )])
        .await;
        let mut settings = configured_settings(&default.url);
        english_routing(&mut settings, &english.url);
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let output = process_transcription_output_with_language(
            &session,
            "sounds good",
            &OutputLanguageEvidence::UserSelected("en".into()),
        )
        .await;
        assert_eq!(output.final_text, "Sounds good.");
        assert_eq!(output.post_processed_text.as_deref(), Some("Sounds good."));
        assert!(output.post_process_prompt.is_none());
        assert_eq!(
            output.llm_route,
            Some(dictation_timing::LlmRoute::EnglishModel)
        );
        assert_eq!(english.requests.lock().unwrap().len(), 1);
        assert!(default.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn handy008_english_route_failure_reports_one_cue_and_notice_without_fallback() {
        let default = FakeServer::new(vec![]).await;
        let closed = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let mut settings = configured_settings(&default.url);
        settings.post_process_request_options.connect_timeout_secs = 1;
        settings.post_process_request_options.timeout_secs = 2;
        english_routing(&mut settings, &closed);
        let session = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let raw = "sounds good";
        let output = process_transcription_output_with_language(
            &session,
            raw,
            &OutputLanguageEvidence::ModelDetected("en".into()),
        )
        .await;
        assert_eq!(output.final_text, raw);
        assert!(output.cleanup_error.is_some());
        assert!(output.cleanup_skipped.is_none());
        assert_eq!(
            output.llm_route,
            Some(dictation_timing::LlmRoute::EnglishModel)
        );
        assert_eq!(output.paste_outcome(), Outcome::CleanupFallback);
        assert_eq!(default.accepts.load(Ordering::SeqCst), 0, "no fallback");
        let reporter = FakeCleanupReporter::default();
        report_transcription_delivery(&reporter, &output, true, &Ok(true));
        assert_eq!(reporter.cues.get(), 1);
        assert_eq!(
            &*reporter.events.borrow(),
            &[(
                "cleanup-fallback".into(),
                CleanupFallbackNotice {
                    history_available: true,
                    raw_delivered: true
                }
            )]
        );
    }

    #[test]
    fn handy008_history_retry_uses_the_entry_profile() {
        use crate::settings::ProfileKind;
        let mut settings = configured_settings("http://unused.invalid");
        settings.processing_profiles.push(custom_profile(
            "pd",
            ProfileKind::Dictation,
            None,
            Some("pl"),
        ));
        let custom =
            DictationSession::for_history_retry(&history_entry(Some("pd"), true), &settings)
                .unwrap();
        assert_eq!(custom.mode(), WorkflowMode::Cleanup);
        assert_eq!(custom.profile.as_ref().unwrap().id, "pd");
        assert_eq!(custom.stt_language.as_deref(), Some("pl"));
        assert_eq!(
            DictationSession::for_history_retry(&history_entry(Some("gone"), true), &settings)
                .err()
                .as_deref(),
            Some("profile_not_found"),
            "never silently retry with built-in Cleanup"
        );
        let builtin = DictationSession::for_history_retry(
            &history_entry(Some(crate::settings::BUILTIN_CLEANUP_PROFILE_ID), true),
            &settings,
        )
        .unwrap();
        assert!(builtin.profile.as_ref().unwrap().builtin);
        let legacy =
            DictationSession::for_history_retry(&history_entry(None, true), &settings).unwrap();
        assert_eq!(legacy.mode(), WorkflowMode::Cleanup);
        assert_eq!(
            legacy.profile.as_ref().unwrap().id,
            crate::settings::BUILTIN_CLEANUP_PROFILE_ID
        );
        let legacy_raw =
            DictationSession::for_history_retry(&history_entry(None, false), &settings).unwrap();
        assert_eq!(legacy_raw.mode(), WorkflowMode::Raw);
    }

    #[tokio::test]
    async fn handy008_timing_marks_only_custom_profiles_and_records_the_route() {
        use crate::settings::{LlmEndpoint, ProfileKind};
        let server = FakeServer::new(vec![
            Step::Respond(200, completion(Some(r#"{"transcription":"A."}"#), "stop")),
            Step::Respond(200, completion(Some(r#"{"transcription":"B."}"#), "stop")),
            Step::Respond(200, completion(Some(r#"{"transcription":"C."}"#), "stop")),
        ])
        .await;
        let mut settings = configured_settings(&server.url);
        settings
            .processing_profiles
            .push(custom_profile("pd", ProfileKind::Dictation, None, None));
        let seeded = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        builtin_cleanup(&mut settings).model = Some(LlmEndpoint {
            provider_id: "custom".into(),
            base_url: None,
            model: "override".into(),
        });
        let overridden = DictationSession::snapshot(WorkflowMode::Cleanup, &settings).unwrap();
        let custom = DictationSession::for_binding("dictation_profile_pd", &settings).unwrap();
        for (session, profile, route) in [
            (&seeded, None, dictation_timing::LlmRoute::Default),
            (&overridden, None, dictation_timing::LlmRoute::ProfileModel),
            (
                &custom,
                Some("pd".to_string()),
                dictation_timing::LlmRoute::Default,
            ),
        ] {
            let timer = OperationTimer::begin_stop(Mode::Cleanup, session.language.clone());
            session.note_timing_profile(&timer);
            let output = process_transcription_output(session, "raw").await;
            note_cleanup_route(&timer, &output);
            let record = timer.record_for_test();
            assert_eq!(record.profile, profile);
            assert_eq!(record.llm_route, Some(route));
            timer.finish(Outcome::Pasted);
        }
        let raw = DictationSession::snapshot(WorkflowMode::Raw, &settings).unwrap();
        let timer = OperationTimer::begin_stop(Mode::Raw, raw.language.clone());
        raw.note_timing_profile(&timer);
        let output = process_transcription_output(&raw, "raw").await;
        note_cleanup_route(&timer, &output);
        let record = timer.record_for_test();
        assert_eq!((record.profile, record.llm_route), (None, None));
        timer.finish(Outcome::Pasted);
    }

    #[test]
    fn live_overlay_uses_streaming_states_only_for_streaming_models() {
        assert!(should_use_streaming_overlay(OverlayStyle::Live, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::Live, false));
        assert!(!should_use_streaming_overlay(OverlayStyle::Minimal, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::None, true));
    }
}
