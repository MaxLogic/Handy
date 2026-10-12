//! Verified target capture and strict replacement commit for the assistant.
//!
//! `begin_capture` reads the focused editable control's selection through UI
//! Automation (never the clipboard) and returns `Selected`, a positively
//! verified `Empty` caret, or an error, plus an opaque `TargetToken`. The
//! worker keeps the target's identity evidence and invalidates the token on
//! focus, caret or selection change. `commit` re-validates the target, then
//! publishes the replacement through the strict clipboard transaction and
//! injects one atomic Ctrl+V; it never falls back to another paste path.
//!
//! All UI Automation and WinEvent work happens on one long-lived worker thread
//! (`windows.rs`). Decisions are pure functions in `evidence.rs` and the commit
//! sequence is the generic `commit::run_commit`, so both are unit-tested
//! without a desktop. Other platforms report `CaptureError::Unsupported`.

mod commit;
mod evidence;
#[cfg(all(test, target_os = "windows"))]
mod probe;
#[cfg(test)]
mod tests;
#[cfg(target_os = "windows")]
mod windows;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

pub(crate) use crate::paste_tx::{ClipboardSettlement, LossyFormat};
pub(crate) use crate::settings::ClipboardHandling;

/// An unresponsive target expires capture after this long.
pub(crate) const CAPTURE_TIMEOUT: Duration = Duration::from_millis(1000);
/// Bound for each validation or verification round trip to the worker.
pub(crate) const VALIDATE_TIMEOUT: Duration = Duration::from_millis(1000);
/// Largest selection accepted, in Unicode scalar values (spec limit).
pub(crate) const MAX_SELECTION_SCALARS: usize = 131_072;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionId(pub u64);

/// Worker-side id of a tracked target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TargetId(pub u64);

/// Captured selection. Its `Debug` output never includes the text.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum CapturedText {
    Selected(String),
    Empty,
}

impl fmt::Debug for CapturedText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapturedText::Selected(text) => write!(
                f,
                "Selected({} scalars, {} utf16 units)",
                text.chars().count(),
                text.encode_utf16().count()
            ),
            CapturedText::Empty => write!(f, "Empty"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Captured {
    pub text: CapturedText,
    pub token: TargetToken,
}

/// Why capture failed. Nothing was mutated in any of these cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaptureError {
    // Constructed by the unsupported-platform implementation and tests.
    #[cfg_attr(all(not(test), target_os = "windows"), allow(dead_code))]
    Unsupported,
    ShuttingDown,
    Busy,
    Timeout,
    Cancelled,
    WorkerUnavailable(String),
    NoForegroundWindow,
    HandyWindowFocused,
    TargetIdentityUnavailable,
    FocusOutsideForegroundApp,
    UnsupportedTarget,
    NoTextPattern,
    PasswordField,
    Disabled,
    ReadOnly,
    EditabilityUnknown,
    NoCaret,
    MultipleSelections,
    EmptyNonDegenerate,
    EmbeddedObject,
    InvalidText,
    SelectionTooLarge,
    TargetChangedDuringCapture,
    /// HRESULT text from the provider; never content.
    Provider(String),
}

impl CaptureError {
    /// Stable key for translated, accessible messages.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::ShuttingDown => "shutting_down",
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::WorkerUnavailable(_) => "worker_unavailable",
            Self::NoForegroundWindow => "no_foreground_window",
            Self::HandyWindowFocused => "handy_window_focused",
            Self::TargetIdentityUnavailable => "target_identity_unavailable",
            Self::FocusOutsideForegroundApp => "focus_outside_foreground_app",
            Self::UnsupportedTarget => "unsupported_target",
            Self::NoTextPattern => "no_text_pattern",
            Self::PasswordField => "password_field",
            Self::Disabled => "disabled",
            Self::ReadOnly => "read_only",
            Self::EditabilityUnknown => "editability_unknown",
            Self::NoCaret => "no_caret",
            Self::MultipleSelections => "multiple_selections",
            Self::EmptyNonDegenerate => "empty_non_degenerate",
            Self::EmbeddedObject => "embedded_object",
            Self::InvalidText => "invalid_text",
            Self::SelectionTooLarge => "selection_too_large",
            Self::TargetChangedDuringCapture => "target_changed_during_capture",
            Self::Provider(_) => "provider_error",
        }
    }
}

/// Event-driven invalidation recorded on a live token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InvalidationReason {
    Foreground,
    Focus,
    Caret,
    TextSelection,
    Superseded,
    Shutdown,
}

impl InvalidationReason {
    const ALL: [InvalidationReason; 6] = [
        Self::Foreground,
        Self::Focus,
        Self::Caret,
        Self::TextSelection,
        Self::Superseded,
        Self::Shutdown,
    ];

    fn to_u8(self) -> u8 {
        Self::ALL.iter().position(|r| *r == self).unwrap_or(0) as u8 + 1
    }

    fn from_u8(value: u8) -> Option<Self> {
        value
            .checked_sub(1)
            .and_then(|i| Self::ALL.get(i as usize).copied())
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Focus => "focus",
            Self::Caret => "caret",
            Self::TextSelection => "text_selection",
            Self::Superseded => "superseded",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Records the first invalidation; later ones keep the first reason.
pub(crate) fn invalidate(flag: &AtomicU8, reason: InvalidationReason) {
    let _ = flag.compare_exchange(0, reason.to_u8(), Ordering::SeqCst, Ordering::SeqCst);
}

pub(crate) fn invalidation_of(flag: &AtomicU8) -> Option<InvalidationReason> {
    InvalidationReason::from_u8(flag.load(Ordering::SeqCst))
}

/// Why a target no longer matches its captured evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StaleReason {
    Invalidated(InvalidationReason),
    ForegroundChanged,
    ProcessChanged,
    FocusChanged,
    SelectionChanged,
    TextChanged,
    NotCollapsed,
    Unresponsive,
    Released,
    Provider(String),
}

impl StaleReason {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Invalidated(_) => "invalidated",
            Self::ForegroundChanged => "foreground_changed",
            Self::ProcessChanged => "process_changed",
            Self::FocusChanged => "focus_changed",
            Self::SelectionChanged => "selection_changed",
            Self::TextChanged => "text_changed",
            Self::NotCollapsed => "not_collapsed",
            Self::Unresponsive => "unresponsive",
            Self::Released => "released",
            Self::Provider(_) => "provider_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefuseReason {
    Shutdown,
    Cancelled,
    SessionMismatch,
    EmptyReplacement,
    TargetStale(StaleReason),
    ClipboardNotPreservable(LossyFormat),
    ClipboardBusy,
    ClipboardTimeout,
    ClipboardChanged,
    ModifiersHeld,
    #[cfg_attr(all(not(test), target_os = "windows"), allow(dead_code))]
    WorkerUnavailable,
}

impl RefuseReason {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Cancelled => "cancelled",
            Self::SessionMismatch => "session_mismatch",
            Self::EmptyReplacement => "empty_replacement",
            Self::TargetStale(_) => "target_stale",
            Self::ClipboardNotPreservable(_) => "clipboard_not_preservable",
            Self::ClipboardBusy => "clipboard_busy",
            Self::ClipboardTimeout => "clipboard_timeout",
            Self::ClipboardChanged => "clipboard_changed",
            Self::ModifiersHeld => "modifiers_held",
            Self::WorkerUnavailable => "worker_unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailReason {
    PublishFailed(String),
    InjectionNotSent(String),
}

impl FailReason {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::PublishFailed(_) => "publish_failed",
            Self::InjectionNotSent(_) => "injection_not_sent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UncertainReason {
    PartialInjection(String),
    NoReceipt,
    ClipboardChangedDuringPaste,
    NotVerified,
    SettlementUnacknowledged,
    Preempted,
}

impl UncertainReason {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::PartialInjection(_) => "partial_injection",
            Self::NoReceipt => "no_receipt",
            Self::ClipboardChangedDuringPaste => "clipboard_changed_during_paste",
            Self::NotVerified => "not_verified",
            Self::SettlementUnacknowledged => "settlement_unacknowledged",
            Self::Preempted => "preempted",
        }
    }
}

/// Result of `commit`. In `Refused` and `Failed` no paste keystroke reached
/// the target. `Uncertain` means a keystroke was injected but delivery is not
/// proven; never retry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    Delivered {
        clipboard: ClipboardSettlement,
    },
    Refused {
        reason: RefuseReason,
        clipboard: ClipboardSettlement,
    },
    Failed {
        reason: FailReason,
        clipboard: ClipboardSettlement,
    },
    Uncertain {
        reason: UncertainReason,
        clipboard: ClipboardSettlement,
    },
}

type Releaser = Arc<dyn Fn(TargetId) + Send + Sync>;

/// Opaque, single-use handle to a captured target. Dropping it releases the
/// worker-held UI Automation objects and event hooks.
pub(crate) struct TargetToken {
    id: TargetId,
    session: SessionId,
    empty: bool,
    invalidation: Arc<AtomicU8>,
    releaser: Releaser,
}

impl TargetToken {
    pub(crate) fn new(
        id: TargetId,
        session: SessionId,
        empty: bool,
        invalidation: Arc<AtomicU8>,
        releaser: Releaser,
    ) -> Self {
        Self {
            id,
            session,
            empty,
            invalidation,
            releaser,
        }
    }

    pub(crate) fn session(&self) -> SessionId {
        self.session
    }

    // The native owner retains range evidence; fixtures inspect this opaque flag.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_empty_target(&self) -> bool {
        self.empty
    }

    pub(crate) fn invalidation(&self) -> Option<InvalidationReason> {
        invalidation_of(&self.invalidation)
    }

    pub(crate) fn id(&self) -> TargetId {
        self.id
    }
}

impl fmt::Debug for TargetToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetToken")
            .field("id", &self.id)
            .field("session", &self.session)
            .field("empty", &self.empty)
            .field("invalidation", &self.invalidation())
            .finish()
    }
}

impl Drop for TargetToken {
    fn drop(&mut self) {
        (self.releaser)(self.id);
    }
}

pub(crate) struct CommitRequest<'a> {
    pub token: TargetToken,
    pub session: SessionId,
    pub replacement: &'a str,
    /// After-success clipboard policy, snapshotted at session start.
    pub clipboard_handling: ClipboardHandling,
    /// Honoured only before injection.
    pub is_cancelled: &'a (dyn Fn() -> bool + Sync),
}

// --- capture admission and expiry (platform-neutral) -----------------------

enum WaiterMsg {
    Result(Result<Captured, CaptureError>),
    Cancel,
}

/// Handle for a pending capture.
pub(crate) struct CaptureTicket {
    tx: Sender<WaiterMsg>,
}

impl CaptureTicket {
    #[cfg(test)]
    pub(crate) fn test_pending() -> Self {
        let (tx, _rx) = mpsc::channel();
        Self { tx }
    }
    /// Idempotent and non-blocking. The capture reports `Cancelled` unless its
    /// result was already delivered.
    pub(crate) fn cancel(&self) {
        let _ = self.tx.send(WaiterMsg::Cancel);
    }
}

/// Clears the in-flight admission flag when the worker finishes a request.
struct InFlightGuard(&'static AtomicBool);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The worker's side of one capture request.
pub(crate) struct CaptureReply {
    tx: Sender<WaiterMsg>,
    in_flight: InFlightGuard,
}

impl CaptureReply {
    /// Delivers the result. The admission flag clears first, so the next
    /// press is admitted as soon as the waiter reports. A result arriving
    /// after the waiter gave up is dropped here, which releases its target.
    pub(crate) fn send(self, result: Result<Captured, CaptureError>) {
        let CaptureReply { tx, in_flight } = self;
        drop(in_flight);
        let _ = tx.send(WaiterMsg::Result(result));
    }
}

/// Generic capture admission: one request in flight, `on_done` called exactly
/// once on a waiter thread within `timeout`.
pub(crate) fn begin_capture_with<F, S>(
    in_flight: &'static AtomicBool,
    timeout: Duration,
    shutting_down: bool,
    on_done: F,
    submit: S,
) -> CaptureTicket
where
    F: FnOnce(Result<Captured, CaptureError>) + Send + 'static,
    S: FnOnce(CaptureReply) -> Result<(), CaptureError> + Send + 'static,
{
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel::<WaiterMsg>();
    let ticket = CaptureTicket { tx: tx.clone() };

    let immediate = if shutting_down {
        Some(CaptureError::ShuttingDown)
    } else if in_flight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        Some(CaptureError::Busy)
    } else {
        None
    };
    let admitted = immediate.is_none();

    let on_done = Arc::new(Mutex::new(Some(on_done)));
    let call = {
        let on_done = on_done.clone();
        move |result: Result<Captured, CaptureError>| {
            let f = on_done.lock().ok().and_then(|mut slot| slot.take());
            if let Some(f) = f {
                f(result);
            }
        }
    };
    let call_in_thread = call.clone();
    let spawned = std::thread::Builder::new()
        .name("handy-selection-waiter".into())
        .spawn(move || {
            let call = call_in_thread;
            if let Some(error) = immediate {
                call(Err(error));
                return;
            }
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(WaiterMsg::Result(result)) => {
                    if Instant::now() >= deadline {
                        drop(result);
                        call(Err(CaptureError::Timeout));
                    } else {
                        call(result);
                    }
                }
                Ok(WaiterMsg::Cancel) => call(Err(CaptureError::Cancelled)),
                Err(RecvTimeoutError::Timeout) => call(Err(CaptureError::Timeout)),
                Err(RecvTimeoutError::Disconnected) => call(Err(CaptureError::WorkerUnavailable(
                    "capture request was dropped".into(),
                ))),
            }
            // Dropping `rx` here drops any late result, releasing its target.
        });
    if let Err(e) = spawned {
        if admitted {
            in_flight.store(false, Ordering::SeqCst);
        }
        call(Err(CaptureError::WorkerUnavailable(format!(
            "could not start capture waiter: {e}"
        ))));
    } else if admitted {
        let reply = CaptureReply {
            tx: tx.clone(),
            in_flight: InFlightGuard(in_flight),
        };
        // Worker startup/OnceLock initialization can take longer than the
        // capture budget. Never run submission on the shortcut/UI caller.
        if let Err(e) = std::thread::Builder::new()
            .name("handy-selection-submit".into())
            .spawn(move || {
                if let Err(error) = submit(reply) {
                    let _ = tx.send(WaiterMsg::Result(Err(error)));
                }
            })
        {
            let _ = ticket
                .tx
                .send(WaiterMsg::Result(Err(CaptureError::WorkerUnavailable(
                    format!("could not start capture submission: {e}"),
                ))));
        }
    }
    ticket
}

static CAPTURE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static LIVE_TARGET: Mutex<Option<Weak<AtomicU8>>> = Mutex::new(None);

fn signal_shutdown(stop: &AtomicBool, target: &Mutex<Option<Weak<AtomicU8>>>) {
    stop.store(true, Ordering::SeqCst);
    if let Some(flag) = target
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(Weak::upgrade)
    {
        invalidate(&flag, InvalidationReason::Shutdown);
    }
}

#[cfg(target_os = "windows")]
fn track_live_target(flag: &Arc<AtomicU8>) {
    let mut slot = LIVE_TARGET.lock().unwrap_or_else(|p| p.into_inner());
    *slot = Some(Arc::downgrade(flag));
    if is_shutting_down() {
        invalidate(flag, InvalidationReason::Shutdown);
    }
}

pub(crate) fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst)
}

/// Non-blocking. `on_done` runs exactly once on a selection-owned thread
/// within `CAPTURE_TIMEOUT`: result, Timeout, Cancelled, Busy or ShuttingDown.
pub(crate) fn begin_capture(
    session: SessionId,
    on_done: impl FnOnce(Result<Captured, CaptureError>) + Send + 'static,
) -> CaptureTicket {
    begin_capture_with(
        &CAPTURE_IN_FLIGHT,
        CAPTURE_TIMEOUT,
        is_shutting_down(),
        on_done,
        move |reply| {
            #[cfg(target_os = "windows")]
            {
                windows::submit_capture(session, reply)
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (session, reply);
                Err(CaptureError::Unsupported)
            }
        },
    )
}

/// Blocking, at most about 12 s. Call from `spawn_blocking` and await it;
/// never from the Tauri main thread or the coordinator thread. Returning is
/// the settlement acknowledgment. The token is consumed and released on
/// every path; never retry.
pub(crate) fn commit(req: CommitRequest) -> CommitOutcome {
    #[cfg(target_os = "windows")]
    {
        let mut platform = windows::WinCommitPlatform::new();
        commit::run_commit(&mut platform, req, &is_shutting_down)
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Unreachable in practice: capture never yields a token here.
        drop(req);
        CommitOutcome::Refused {
            reason: RefuseReason::WorkerUnavailable,
            clipboard: ClipboardSettlement::Untouched,
        }
    }
}

/// Stops intake, invalidates the live token, stops the worker and waits for
/// it at most 2 s.
pub(crate) fn shutdown() {
    signal_shutdown(&SHUTTING_DOWN, &LIVE_TARGET);
    #[cfg(target_os = "windows")]
    windows::shutdown();
}
