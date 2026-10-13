//! Selected-text assistant: bounded admission, one generation, one strict commit.
pub(crate) mod recovery;
#[cfg(test)]
pub(crate) mod tests;

use crate::actions::complete_unless_cancelled;
use crate::dictation_timing::{OperationTimer, Outcome, Stage};
use crate::llm_client::LlmError;
use crate::prompts::{self, AssistantRequest};
use crate::selection::{
    self, CapturedText, ClipboardSettlement, CommitOutcome, RefuseReason, StaleReason,
};
use crate::settings::{AppSettings, ClipboardHandling, ProfileSpec};
use recovery::{AssistantRecoveryItem, RecoveryStore, Reservation};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri_specta::Event;

pub(crate) const RECOVERY_CAPACITY: usize = 8;
pub(crate) const EXIT_COMMIT_WAIT: Duration = Duration::from_secs(16);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static ASSISTANT_CANCEL_ARMED: AtomicBool = AtomicBool::new(false);
static COMMITS: CommitTracker = CommitTracker::new();

#[derive(Clone, Copy, Debug, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Selected,
    Empty,
}
#[derive(Clone, Copy, Debug, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    Refused,
    Failed,
    Uncertain,
}
#[derive(Clone, Copy, Debug, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AssistantStage {
    Intake,
    Capture,
    Transcription,
    Generation,
}
#[derive(Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPart {
    Generated,
    Instruction,
}

#[derive(Clone, Debug, Serialize, Type, Event)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AssistantRecoveryUpdate {
    Added { item: AssistantRecoveryItem },
    Dismissed { id: u64 },
}
#[derive(Clone, Serialize, Type, Event)]
pub struct AssistantErrorEvent {
    pub stage: AssistantStage,
    pub code: String,
    pub reason: Option<String>,
    pub detail: Option<String>,
}
#[derive(Serialize, Type)]
pub struct AssistantRecoveryState {
    pub items: Vec<AssistantRecoveryItem>,
    pub capacity: u32,
    pub free_slots: u32,
}

pub(crate) enum IntakeRejection {
    ShuttingDown,
    Config(LlmError),
    RecoveryFull,
}
impl IntakeRejection {
    pub(crate) fn error(self) -> AssistantError {
        match self {
            Self::ShuttingDown => AssistantError::new(AssistantStage::Intake, "shutting_down"),
            Self::RecoveryFull => AssistantError::new(AssistantStage::Intake, "recovery_full"),
            Self::Config(error) => AssistantError::llm(AssistantStage::Intake, error),
        }
    }
}
pub(crate) struct Admission {
    pub request: AssistantRequest,
    pub reservation: Reservation,
    pub clipboard_handling: ClipboardHandling,
    pub settings: AppSettings,
    /// The selection-command profile snapshotted at admission.
    pub spec: ProfileSpec,
}
/// Order: shutdown, configuration (a missing profile is a configuration
/// error), capacity.
pub(crate) fn admit(
    shutting_down: bool,
    settings: &AppSettings,
    spec: Option<&ProfileSpec>,
    store: &Arc<RecoveryStore>,
) -> Result<Admission, IntakeRejection> {
    if shutting_down {
        return Err(IntakeRejection::ShuttingDown);
    }
    let spec = spec.ok_or(IntakeRejection::Config(LlmError::Config(
        crate::llm_client::ConfigError::ProfileNotFound,
    )))?;
    let request = AssistantRequest::from_spec(settings, spec).map_err(IntakeRejection::Config)?;
    let reservation = store.reserve().ok_or(IntakeRejection::RecoveryFull)?;
    Ok(Admission {
        request,
        reservation,
        clipboard_handling: settings.clipboard_handling,
        settings: settings.clone(),
        spec: spec.clone(),
    })
}

/// Admission for one selection-command binding, resolved on the coordinator
/// thread so the profile is snapshotted at press time.
pub(crate) fn admit_binding(
    shutting_down: bool,
    settings: &AppSettings,
    binding_id: &str,
    store: &Arc<RecoveryStore>,
) -> Result<Admission, IntakeRejection> {
    let spec = crate::settings::spec_for_binding(settings, binding_id)
        .filter(|spec| spec.kind == crate::settings::ProfileKind::SelectionCommand);
    admit(shutting_down, settings, spec.as_ref(), store)
}

pub(crate) struct AssistantSession {
    pub id: selection::SessionId,
    pub request: AssistantRequest,
    pub captured: CapturedText,
    pub token: selection::TargetToken,
    pub reservation: Reservation,
    pub clipboard_handling: ClipboardHandling,
    pub capture: Duration,
    pub cancel_arm: CancelArm,
}

pub(crate) struct CancelArm(Option<Box<dyn FnOnce() + Send + Sync>>);
impl CancelArm {
    pub(crate) fn arm<C>(app: &C) -> Self
    where
        C: crate::shortcut::CancelShortcutContext + Clone + Send + Sync + 'static,
    {
        ASSISTANT_CANCEL_ARMED.store(true, Ordering::SeqCst);
        crate::shortcut::register_cancel_shortcut(app);
        let app = app.clone();
        Self(Some(Box::new(move || {
            ASSISTANT_CANCEL_ARMED.store(false, Ordering::SeqCst);
            crate::shortcut::unregister_cancel_shortcut(&app);
        })))
    }
    #[cfg(test)]
    pub(crate) fn noop() -> Self {
        Self(None)
    }
    pub(crate) fn disarm(mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}
impl Drop for CancelArm {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}
pub(crate) fn cancel_armed() -> bool {
    ASSISTANT_CANCEL_ARMED.load(Ordering::SeqCst)
}
pub(crate) fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst)
}
pub(crate) fn begin_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
}
pub(crate) fn await_commits(timeout: Duration) {
    if !COMMITS.wait_idle(timeout) {
        log::warn!("[assistant] commit settlement wait timed out");
    }
}

pub(crate) struct CommitTracker {
    count: Mutex<usize>,
    idle: Condvar,
}
impl CommitTracker {
    pub(crate) const fn new() -> Self {
        Self {
            count: Mutex::new(0),
            idle: Condvar::new(),
        }
    }
    fn enter(&'static self) -> CommitGuard {
        *self.count.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        CommitGuard(self)
    }
    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        let count = self.count.lock().unwrap_or_else(|p| p.into_inner());
        let (count, _) = self
            .idle
            .wait_timeout_while(count, timeout, |count| *count > 0)
            .unwrap_or_else(|p| p.into_inner());
        *count == 0
    }
}
struct CommitGuard(&'static CommitTracker);
impl Drop for CommitGuard {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap_or_else(|p| p.into_inner());
        *count -= 1;
        if *count == 0 {
            self.0.idle.notify_all();
        }
    }
}

pub(crate) type CancelProbe = Arc<dyn Fn() -> bool + Send + Sync>;
pub(crate) struct SessionReport {
    pub outcome: SessionOutcome,
    pub target: TargetKind,
    pub selection_scalars: u64,
    pub llm: Option<Duration>,
    pub commit: Option<Duration>,
    pub commit_calls: u32,
}
pub(crate) enum SessionOutcome {
    Delivered,
    Recoverable(AssistantRecoveryItem),
    Discarded,
    Failed(AssistantError),
}
pub(crate) struct AssistantError {
    pub stage: AssistantStage,
    pub code: &'static str,
    pub reason: Option<&'static str>,
    pub detail: Option<String>,
}
impl AssistantError {
    pub(crate) fn new(stage: AssistantStage, code: &'static str) -> Self {
        Self {
            stage,
            code,
            reason: None,
            detail: None,
        }
    }
    fn llm(stage: AssistantStage, error: LlmError) -> Self {
        Self {
            stage,
            code: error.code(),
            reason: Some(error.reason()),
            detail: error.detail().map(str::to_owned),
        }
    }
    pub(crate) fn emit(self, app: &AppHandle) {
        crate::audio_feedback::play_failure_feedback(app);
        log::warn!(
            "[assistant] stage={:?} code={} reason={}",
            self.stage,
            self.code,
            self.reason.unwrap_or("none")
        );
        let _ = AssistantErrorEvent {
            stage: self.stage,
            code: self.code.into(),
            reason: self.reason.map(str::to_owned),
            detail: self.detail,
        }
        .emit(app);
    }
}

pub(crate) fn settlement_code(value: &ClipboardSettlement) -> &'static str {
    match value {
        ClipboardSettlement::Untouched => "untouched",
        ClipboardSettlement::Restored => "restored",
        ClipboardSettlement::LeftReplacement => "left_replacement",
        ClipboardSettlement::ExternallyChanged => "externally_changed",
        ClipboardSettlement::RestoreFailed(_) => "restore_failed",
        ClipboardSettlement::Unknown => "unknown",
    }
}

pub(crate) async fn complete_session<C>(
    session: AssistantSession,
    instruction: &str,
    is_cancelled: CancelProbe,
    commit: C,
    tracker: &'static CommitTracker,
) -> SessionReport
where
    C: for<'a> FnOnce(selection::CommitRequest<'a>) -> CommitOutcome + Send + 'static,
{
    let selected = match &session.captured {
        CapturedText::Selected(text) => text.as_str(),
        CapturedText::Empty => "",
    };
    let mut report = SessionReport {
        outcome: SessionOutcome::Discarded,
        target: if matches!(session.captured, CapturedText::Empty) {
            TargetKind::Empty
        } else {
            TargetKind::Selected
        },
        selection_scalars: selected.chars().count() as u64,
        llm: None,
        commit: None,
        commit_calls: 0,
    };
    if prompts::is_blank_transcription(instruction) {
        report.outcome = SessionOutcome::Failed(AssistantError::new(
            AssistantStage::Transcription,
            "blank_instruction",
        ));
        return report;
    }
    if is_cancelled() {
        return report;
    }
    let started = Instant::now();
    let generated = complete_unless_cancelled(
        prompts::assist(&session.request, instruction, selected),
        || is_cancelled(),
    )
    .await;
    report.llm = Some(started.elapsed());
    let generated = match generated {
        None => return report,
        Some(Err(error)) => {
            report.outcome =
                SessionOutcome::Failed(AssistantError::llm(AssistantStage::Generation, error));
            return report;
        }
        Some(Ok(text)) => text,
    };
    let AssistantSession {
        id,
        token,
        reservation,
        clipboard_handling,
        cancel_arm,
        ..
    } = session;
    let replacement = generated.clone();
    let started = Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        let _guard = tracker.enter();
        commit(selection::CommitRequest {
            token,
            session: id,
            replacement: &replacement,
            clipboard_handling,
            is_cancelled: &*is_cancelled,
        })
    })
    .await
    .unwrap_or(CommitOutcome::Uncertain {
        reason: selection::UncertainReason::SettlementUnacknowledged,
        clipboard: ClipboardSettlement::Unknown,
    });
    report.commit = Some(started.elapsed());
    report.commit_calls = 1;
    cancel_arm.disarm();
    let (kind, reason, detail, clipboard) = match outcome {
        CommitOutcome::Delivered { .. } => {
            reservation.release();
            report.outcome = SessionOutcome::Delivered;
            return report;
        }
        CommitOutcome::Refused {
            reason: RefuseReason::Cancelled | RefuseReason::Shutdown,
            ..
        } => {
            reservation.release();
            return report;
        }
        CommitOutcome::Refused { reason, clipboard } => {
            let detail = match &reason {
                RefuseReason::TargetStale(StaleReason::Invalidated(reason)) => {
                    Some(reason.code().into())
                }
                RefuseReason::TargetStale(reason) => Some(reason.code().into()),
                RefuseReason::ClipboardNotPreservable(format) => Some(format.format.to_string()),
                _ => None,
            };
            (RecoveryOutcome::Refused, reason.code(), detail, clipboard)
        }
        CommitOutcome::Failed { reason, clipboard } => {
            (RecoveryOutcome::Failed, reason.code(), None, clipboard)
        }
        CommitOutcome::Uncertain { reason, clipboard } => {
            (RecoveryOutcome::Uncertain, reason.code(), None, clipboard)
        }
    };
    report.outcome = SessionOutcome::Recoverable(reservation.fill(|id| AssistantRecoveryItem {
        id,
        created_at: chrono::Utc::now().timestamp_millis(),
        target: report.target,
        outcome: kind,
        reason: reason.into(),
        detail,
        clipboard: settlement_code(&clipboard).into(),
        instruction: instruction.into(),
        generated,
    }));
    report
}

pub(crate) async fn finish(
    app: &AppHandle,
    session: AssistantSession,
    instruction: &str,
    timer: &OperationTimer,
    probe: CancelProbe,
) {
    timer.set_capture(session.capture);
    timer.set_llm_route(session.request.route);
    let report = complete_session(session, instruction, probe, selection::commit, &COMMITS).await;
    timer.set_selection_scalars(report.selection_scalars);
    if let Some(duration) = report.llm {
        timer.add(Stage::Llm, duration);
    }
    if let Some(duration) = report.commit {
        timer.add(Stage::Commit, duration);
    }
    for _ in 0..report.commit_calls {
        timer.note_paste_call();
    }
    let outcome = match report.outcome {
        SessionOutcome::Delivered => {
            log::info!("[assistant] outcome=committed target={:?}", report.target);
            Outcome::Committed
        }
        SessionOutcome::Discarded => Outcome::Cancelled,
        SessionOutcome::Failed(error) => {
            let outcome = if error.code == "blank_instruction" {
                Outcome::EmptyOutput
            } else {
                Outcome::LlmError
            };
            error.emit(app);
            outcome
        }
        SessionOutcome::Recoverable(item) => {
            crate::audio_feedback::play_failure_feedback(app);
            let outcome = match item.outcome {
                RecoveryOutcome::Refused => Outcome::CommitRefused,
                RecoveryOutcome::Failed => Outcome::CommitFailed,
                RecoveryOutcome::Uncertain => Outcome::CommitUncertain,
            };
            log::warn!(
                "[assistant] outcome={:?} code={} detail={}",
                item.outcome,
                item.reason,
                item.detail.as_deref().unwrap_or("none")
            );
            let _ = AssistantRecoveryUpdate::Added { item }.emit(app);
            outcome
        }
    };
    timer.finish(outcome);
}
