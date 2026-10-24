//! Strict commit sequence, generic over the platform so its ordering rules
//! are unit-tested with a fake.

use std::time::{Duration, Instant};

use super::{
    ClipboardHandling, ClipboardSettlement, CommitOutcome, CommitRequest, FailReason, RefuseReason,
    StaleReason, TargetId, UncertainReason, VALIDATE_TIMEOUT,
};
use crate::paste_tx::{ChordResult, StrictSettlement};

/// How long physically held modifiers may delay injection.
pub(crate) const MODIFIER_WAIT: Duration = Duration::from_millis(300);
pub(crate) const MODIFIER_POLL: Duration = Duration::from_millis(10);
/// How long injection may wait for another process to close the clipboard.
pub(crate) const CLIPBOARD_FREE_WAIT: Duration = Duration::from_millis(1500);
pub(crate) const CLIPBOARD_FREE_POLL: Duration = Duration::from_millis(5);
const CLIPBOARD_FREE_MAX_POLL: Duration = Duration::from_millis(50);

/// Why the strict clipboard transaction could not start. Each carries the
/// clipboard state it left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeginClipboardError {
    Refused(RefuseReason, ClipboardSettlement),
    Failed(FailReason, ClipboardSettlement),
}

/// Platform operations of a strict commit.
pub(crate) trait CommitPlatform {
    /// Re-validates the original target while retaining all invalidation hooks.
    fn validate(&mut self, id: TargetId, deadline: Instant) -> Result<(), StaleReason>;
    /// Drains pending invalidations and disarms hooks only on success.
    /// No waits or retries are permitted after this injection preparation.
    fn prepare_injection(&mut self, id: TargetId, deadline: Instant) -> Result<(), StaleReason>;
    /// Strict snapshot and publish of `text`, verbatim.
    fn begin_clipboard(
        &mut self,
        text: &str,
        after_success: ClipboardHandling,
    ) -> Result<(), BeginClipboardError>;
    fn clipboard_still_owned(&self) -> bool;
    /// No process (including this one) has the clipboard open right now.
    fn clipboard_free(&self) -> bool;
    fn modifiers_held(&self) -> bool;
    fn inject_paste(&mut self) -> ChordResult;
    /// Pre-delivery abort: restore if still owned, wait for the result.
    fn abort_clipboard(&mut self) -> ClipboardSettlement;
    /// Waits (bounded) for the post-injection settlement report.
    fn await_settlement(&mut self) -> Option<StrictSettlement>;
    fn verify_inserted(&mut self, id: TargetId, text: &str, deadline: Instant) -> bool;
}

fn refused(reason: RefuseReason, clipboard: ClipboardSettlement) -> CommitOutcome {
    CommitOutcome::Refused { reason, clipboard }
}

/// Runs the strict commit. The token is consumed and dropped (released) on
/// every path. Only pre-injection clipboard readiness is retried; publication
/// and injection each happen at most once. Cancellation after injection is ignored.
pub(crate) fn run_commit<P: CommitPlatform>(
    platform: &mut P,
    req: CommitRequest<'_>,
    shutting_down: &dyn Fn() -> bool,
) -> CommitOutcome {
    let CommitRequest {
        token,
        session,
        replacement,
        clipboard_handling,
        is_cancelled,
    } = req;
    let id = token.id();
    let untouched = || ClipboardSettlement::Untouched;

    // 1. Cheap refusals; nothing touched.
    if shutting_down() {
        return refused(RefuseReason::Shutdown, untouched());
    }
    if session != token.session() {
        return refused(RefuseReason::SessionMismatch, untouched());
    }
    if replacement.is_empty() {
        return refused(RefuseReason::EmptyReplacement, untouched());
    }
    if is_cancelled() {
        return refused(RefuseReason::Cancelled, untouched());
    }
    if let Some(reason) = token.invalidation() {
        return refused(
            RefuseReason::TargetStale(StaleReason::Invalidated(reason)),
            untouched(),
        );
    }

    // 2. Validate before any clipboard work: no churn for a stale target.
    if let Err(stale) = platform.validate(id, Instant::now() + VALIDATE_TIMEOUT) {
        return refused(RefuseReason::TargetStale(stale), untouched());
    }

    // 3. Strict snapshot + publish in one clipboard session.
    match platform.begin_clipboard(replacement, clipboard_handling) {
        Ok(()) => {}
        Err(BeginClipboardError::Refused(reason, clipboard)) => return refused(reason, clipboard),
        Err(BeginClipboardError::Failed(reason, clipboard)) => {
            return CommitOutcome::Failed { reason, clipboard }
        }
    }

    // Hooks retain the capture-time authority throughout every wait and every
    // endpoint read, including change-and-change-back events.
    let authority_refusal = || {
        if is_cancelled() {
            Some(RefuseReason::Cancelled)
        } else if shutting_down() {
            Some(RefuseReason::Shutdown)
        } else {
            token
                .invalidation()
                .map(|reason| RefuseReason::TargetStale(StaleReason::Invalidated(reason)))
        }
    };
    let modifier_deadline = Instant::now() + MODIFIER_WAIT;
    while platform.modifiers_held() {
        let reason = authority_refusal().or_else(|| {
            (!platform.clipboard_still_owned()).then_some(RefuseReason::ClipboardChanged)
        });
        if let Some(reason) = reason {
            let clipboard = platform.abort_clipboard();
            return refused(reason, clipboard);
        }
        if Instant::now() >= modifier_deadline {
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::ModifiersHeld, clipboard);
        }
        std::thread::sleep(MODIFIER_POLL);
    }

    // Clipboard readers (notably WSLg synchronization) can hold the clipboard
    // briefly, including during target validation. Retry readiness only; never
    // republish the replacement or reacquire capture authority.
    let started = Instant::now();
    let free_deadline = started + CLIPBOARD_FREE_WAIT;
    let mut backoff = CLIPBOARD_FREE_POLL;
    let mut retries = 0;
    loop {
        if let Some(reason) = authority_refusal() {
            let clipboard = platform.abort_clipboard();
            return refused(reason, clipboard);
        }
        if !platform.clipboard_still_owned() {
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::ClipboardChanged, clipboard);
        }
        if Instant::now() >= free_deadline {
            log::debug!(
                "[selection] clipboard busy: phase=readiness retries={} elapsed_ms={}",
                retries,
                started.elapsed().as_millis()
            );
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::ClipboardBusy, clipboard);
        }
        let free_before_validation = platform.clipboard_free();
        if let Err(stale) =
            platform.validate(id, (Instant::now() + VALIDATE_TIMEOUT).min(free_deadline))
        {
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::TargetStale(stale), clipboard);
        }
        if let Some(reason) = authority_refusal() {
            let clipboard = platform.abort_clipboard();
            return refused(reason, clipboard);
        }
        if !platform.clipboard_still_owned() {
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::ClipboardChanged, clipboard);
        }
        if Instant::now() >= free_deadline {
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::ClipboardBusy, clipboard);
        }
        if free_before_validation && platform.clipboard_free() {
            break;
        }
        let remaining = free_deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            std::thread::sleep(backoff.min(remaining));
        }
        retries += 1;
        backoff = (backoff * 2).min(CLIPBOARD_FREE_MAX_POLL);
    }
    if platform.modifiers_held() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ModifiersHeld, clipboard);
    }
    // Commit the hook lifecycle only after readiness succeeds. This operation
    // drains the worker queue and checks the original invalidation flag before
    // unhooking, preserving ABA protection until the last immediate boundary.
    if let Err(stale) = platform.prepare_injection(id, free_deadline) {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::TargetStale(stale), clipboard);
    }
    if let Some(reason) = authority_refusal() {
        let clipboard = platform.abort_clipboard();
        return refused(reason, clipboard);
    }
    if !platform.clipboard_still_owned() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ClipboardChanged, clipboard);
    }
    if Instant::now() >= free_deadline {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ClipboardBusy, clipboard);
    }
    // No waiting after hooks are disarmed: a newly arriving reader refuses.
    if !platform.clipboard_free() {
        log::debug!(
            "[selection] clipboard busy: phase=prepared retries={} elapsed_ms={}",
            retries,
            started.elapsed().as_millis()
        );
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ClipboardBusy, clipboard);
    }
    if platform.modifiers_held() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ModifiersHeld, clipboard);
    }
    if let Some(reason) = authority_refusal() {
        let clipboard = platform.abort_clipboard();
        return refused(reason, clipboard);
    }
    if Instant::now() >= free_deadline {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ClipboardBusy, clipboard);
    }
    log::debug!(
        "[selection] clipboard ready: retries={} elapsed_ms={}",
        retries,
        started.elapsed().as_millis()
    );
    // 7. Inject immediately.
    let chord = platform.inject_paste();
    if let ChordResult::NotSent(message) = chord {
        let clipboard = platform.abort_clipboard();
        return CommitOutcome::Failed {
            reason: FailReason::InjectionNotSent(message),
            clipboard,
        };
    }

    // 8. Wait for the settlement acknowledgment.
    let Some(settled) = platform.await_settlement() else {
        return CommitOutcome::Uncertain {
            reason: UncertainReason::SettlementUnacknowledged,
            clipboard: ClipboardSettlement::Unknown,
        };
    };

    // 9. Classify. A receipt alone is not proof: verify the inserted text.
    let clipboard = settled.clipboard.clone();
    let uncertain = |reason| CommitOutcome::Uncertain {
        reason,
        clipboard: clipboard.clone(),
    };
    if let ChordResult::Partial(message) = chord {
        return uncertain(UncertainReason::PartialInjection(message));
    }
    if !settled.receipt_after_injection {
        return uncertain(if settled.ownership_lost {
            UncertainReason::ClipboardChangedDuringPaste
        } else if settled.preempted {
            UncertainReason::Preempted
        } else {
            UncertainReason::NoReceipt
        });
    }
    if platform.verify_inserted(id, replacement, Instant::now() + VALIDATE_TIMEOUT) {
        CommitOutcome::Delivered { clipboard }
    } else {
        uncertain(UncertainReason::NotVerified)
    }
    // 10. `token` drops here (and on every early return), releasing the target.
}
