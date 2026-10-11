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
pub(crate) const CLIPBOARD_FREE_WAIT: Duration = Duration::from_millis(300);
pub(crate) const CLIPBOARD_FREE_POLL: Duration = Duration::from_millis(3);

/// Why the strict clipboard transaction could not start. Each carries the
/// clipboard state it left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeginClipboardError {
    Refused(RefuseReason, ClipboardSettlement),
    Failed(FailReason, ClipboardSettlement),
}

/// Platform operations of a strict commit.
pub(crate) trait CommitPlatform {
    /// Re-validates the target. `boundary = true` is the final check right
    /// before injection; on success it also disarms the event hooks so the
    /// paste's own events are not counted.
    fn validate(
        &mut self,
        id: TargetId,
        boundary: bool,
        deadline: Instant,
    ) -> Result<(), StaleReason>;
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
/// every path. Nothing is retried; cancellation after injection is ignored.
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
    if let Err(stale) = platform.validate(id, false, Instant::now() + VALIDATE_TIMEOUT) {
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

    // 4. Physically held modifiers would change the chord (Alt+Ctrl+V is
    // AltGr+V); wait briefly for release.
    let modifier_deadline = Instant::now() + MODIFIER_WAIT;
    while platform.modifiers_held() {
        if Instant::now() >= modifier_deadline {
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::ModifiersHeld, clipboard);
        }
        std::thread::sleep(MODIFIER_POLL);
    }

    // 5. Last chance to honour cancellation or shutdown.
    if is_cancelled() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::Cancelled, clipboard);
    }
    if shutting_down() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::Shutdown, clipboard);
    }

    // 5b. Another process holding the clipboard open when the target pastes
    // makes the target's own OpenClipboard fail, and the paste silently does
    // nothing (observed live with WSLg's msrdc clipboard sync). Wait, bounded,
    // until the clipboard is free; if it never frees, refuse before sending
    // anything. Waiting happens before the boundary validation, which
    // disarms the change hooks, so the target stays protected while we wait.
    let free_deadline = Instant::now() + CLIPBOARD_FREE_WAIT;
    while !platform.clipboard_free() {
        if Instant::now() >= free_deadline {
            let clipboard = platform.abort_clipboard();
            return refused(RefuseReason::ClipboardBusy, clipboard);
        }
        std::thread::sleep(CLIPBOARD_FREE_POLL);
    }
    // Cancellation and shutdown can arrive while a clipboard reader holds
    // the lock. They still precede injection and must leave text recoverable.
    if is_cancelled() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::Cancelled, clipboard);
    }
    if shutting_down() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::Shutdown, clipboard);
    }

    // 6. Final validation at the injection boundary, then clipboard
    // ownership: a user copy made after publishing must not be pasted.
    if let Err(stale) = platform.validate(id, true, Instant::now() + VALIDATE_TIMEOUT) {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::TargetStale(stale), clipboard);
    }
    if !platform.clipboard_still_owned() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ClipboardChanged, clipboard);
    }
    // Re-check without waiting: the hooks are disarmed now, so a reader that
    // grabbed the clipboard during validation means refusing, not waiting.
    if !platform.clipboard_free() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ClipboardBusy, clipboard);
    }

    // Validation may block for a full UIA timeout. These checks must be after
    // every wait/read and immediately before the chord, with guarded abort.
    if is_cancelled() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::Cancelled, clipboard);
    }
    if shutting_down() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::Shutdown, clipboard);
    }
    if platform.modifiers_held() {
        let clipboard = platform.abort_clipboard();
        return refused(RefuseReason::ModifiersHeld, clipboard);
    }
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
