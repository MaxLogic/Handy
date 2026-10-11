//! Shared strict clipboard transaction core: format policy, one-session
//! snapshot+publish, lock-guarded settlement and chord classification.
//!
//! The Windows reliable dictation path uses the publish/settle functions; the
//! strict (assistant) parts are consumed by `selection` (HANDY-005 wires them).

// Only Windows consumes this module outside tests, and the strict parts are
// reachable only from `selection`, which HANDY-005 wires up.
// TODO(HANDY-005): narrow this once the assistant workflow consumes it.
#![cfg_attr(any(not(target_os = "windows"), not(test)), allow(dead_code))]

use std::time::Instant;

use super::TxState;

// ---------------------------------------------------------------------------
// Shared snapshot / publish / settle core.
//
// The platform clipboard is reached through `ClipboardOps`, so the ordering
// rules below (snapshot and publish in one clipboard session; ownership and
// sequence rechecked *after* the clipboard lock is acquired) are proven by unit
// tests against an in-memory fake instead of being re-implemented per caller.
// ---------------------------------------------------------------------------

/// Largest single clipboard format that a snapshot copies.
pub(crate) const MAX_FORMAT_BYTES: usize = 64 * 1024 * 1024;

// Standard clipboard format ids (winuser.h), kept numeric so the policy is
// testable on every platform.
pub(crate) const CF_BITMAP: u32 = 2;
pub(crate) const CF_METAFILEPICT: u32 = 3;
pub(crate) const CF_PALETTE: u32 = 9;
pub(crate) const CF_UNICODETEXT: u32 = 13;
pub(crate) const CF_ENHMETAFILE: u32 = 14;
pub(crate) const CF_OWNERDISPLAY: u32 = 0x0080;
pub(crate) const CF_DSPTEXT: u32 = 0x0081;
pub(crate) const CF_DSPBITMAP: u32 = 0x0082;
pub(crate) const CF_DSPMETAFILEPICT: u32 = 0x0083;
pub(crate) const CF_DSPENHMETAFILE: u32 = 0x008E;

/// How a snapshot treats formats it cannot copy faithfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotPolicy {
    /// Dictation: skip what cannot be copied (the historical behaviour).
    BestEffort,
    /// Assistant replacement: every present format must be preserved, or the
    /// commit is refused before the clipboard changes.
    Strict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LossyReason {
    NonMemoryHandle,
    PrivateOrGdiRange,
    TooLarge(usize),
    Unreadable,
    BitmapCopyFailed,
    EnumerationFailed,
}

/// A clipboard format the strict snapshot cannot restore losslessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LossyFormat {
    pub format: u32,
    pub reason: LossyReason,
}

/// What the snapshot does with one enumerated format before reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FormatAction {
    ReadMemory,
    CopyBitmap,
    Skip,
    Reject(LossyReason),
}

/// Outcome of reading one format's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormatRead {
    /// `GetClipboardData` failed.
    Unreadable,
    /// Global-memory handle of `size` bytes; `locked` is whether `GlobalLock`
    /// succeeded (pass `false` when the size check already decides).
    Memory { size: usize, locked: bool },
    /// `CopyImage` of a CF_BITMAP.
    Bitmap { copied: bool },
}

/// Pure format policy, by format id.
pub(crate) fn format_action(policy: SnapshotPolicy, format: u32) -> FormatAction {
    if format == CF_BITMAP {
        return FormatAction::CopyBitmap;
    }
    match policy {
        SnapshotPolicy::Strict => {
            if matches!(
                format,
                CF_METAFILEPICT
                    | CF_ENHMETAFILE
                    | CF_DSPENHMETAFILE
                    | CF_DSPMETAFILEPICT
                    | CF_DSPBITMAP
                    | CF_OWNERDISPLAY
                    | CF_PALETTE
            ) {
                FormatAction::Reject(LossyReason::NonMemoryHandle)
            } else if (0x0200..=0x03FF).contains(&format) {
                FormatAction::Reject(LossyReason::PrivateOrGdiRange)
            } else {
                FormatAction::ReadMemory
            }
        }
        // Exactly the formats the reliable dictation path always skipped.
        SnapshotPolicy::BestEffort => {
            if matches!(
                format,
                CF_ENHMETAFILE
                    | CF_DSPENHMETAFILE
                    | CF_DSPBITMAP
                    | CF_DSPMETAFILEPICT
                    | CF_DSPTEXT
                    | CF_OWNERDISPLAY
                    | CF_PALETTE
            ) {
                FormatAction::Skip
            } else {
                FormatAction::ReadMemory
            }
        }
    }
}

/// Pure judgement of a format read: `Ok(true)` keep, `Ok(false)` skip
/// (best effort only), `Err` the strict snapshot is lossy.
pub(crate) fn judge_read(policy: SnapshotPolicy, read: FormatRead) -> Result<bool, LossyReason> {
    let verdict = match read {
        FormatRead::Unreadable => Err(LossyReason::Unreadable),
        FormatRead::Memory { size: 0, .. } => Err(LossyReason::Unreadable),
        FormatRead::Memory { size, .. } if size > MAX_FORMAT_BYTES => {
            Err(LossyReason::TooLarge(size))
        }
        FormatRead::Memory { locked: false, .. } => Err(LossyReason::Unreadable),
        FormatRead::Memory { .. } => Ok(true),
        FormatRead::Bitmap { copied: false } => Err(LossyReason::BitmapCopyFailed),
        FormatRead::Bitmap { copied: true } => Ok(true),
    };
    match (policy, verdict) {
        (SnapshotPolicy::BestEffort, Err(_)) => Ok(false),
        (_, verdict) => verdict,
    }
}

/// How the clipboard ended up after a transaction settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClipboardSettlement {
    /// Nothing was ever written.
    Untouched,
    /// The snapshot was written back.
    Restored,
    /// The replacement text was left as plain text (CopyToClipboard policy).
    LeftReplacement,
    /// Someone else owns newer clipboard content; it was left alone.
    ExternallyChanged,
    RestoreFailed(String),
    /// Settlement was not acknowledged in time.
    Unknown,
}

/// What settlement writes when the transaction still owns the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettleAction {
    Restore,
    LeaveText,
}

/// Platform clipboard primitives used by the shared transaction logic.
pub(crate) trait ClipboardOps {
    type Snapshot;
    /// Opens the clipboard, retrying briefly while another process holds it.
    fn open(&mut self) -> Result<(), String>;
    fn close(&mut self);
    fn sequence(&self) -> u32;
    /// Whether this transaction's owner window still owns the clipboard.
    fn owned_by_tx(&self) -> bool;
    fn snapshot(&mut self, policy: SnapshotPolicy) -> Result<Self::Snapshot, LossyFormat>;
    /// Empties the clipboard and publishes the lazy text promise.
    fn publish_promise(&mut self) -> Result<(), String>;
    /// Empties the clipboard and writes the snapshot back.
    fn write_snapshot(&mut self, snapshot: &mut Self::Snapshot) -> Result<(), String>;
    /// Empties the clipboard and writes `text` as plain text.
    fn write_text(&mut self, text: &str) -> Result<(), String>;
    /// The caller gave up waiting; do not publish.
    fn abandoned(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublishError {
    /// The strict snapshot found a format it cannot restore; nothing changed.
    Lossy(LossyFormat),
    /// The clipboard could not be opened; nothing changed.
    Busy(String),
    /// The caller abandoned the transaction before publishing; nothing changed.
    Abandoned,
    /// The publishing worker did not report within the caller's bound. It was
    /// abandoned; if it had already published, it restores on its own.
    Timeout,
    /// Publishing failed after the clipboard may have been emptied.
    Failed {
        error: String,
        restore: ClipboardSettlement,
    },
}

/// Snapshots the current clipboard and publishes the promise inside one
/// clipboard session, so no other copy can slip in between the two (it would
/// otherwise be overwritten and later "restored" over). Returns the snapshot
/// and the sequence number observed right after publishing.
pub(crate) fn snapshot_and_publish<C: ClipboardOps>(
    c: &mut C,
    policy: SnapshotPolicy,
) -> Result<(C::Snapshot, u32), PublishError> {
    c.open().map_err(PublishError::Busy)?;
    let mut snapshot = match c.snapshot(policy) {
        Ok(snapshot) => snapshot,
        Err(lossy) => {
            c.close();
            return Err(PublishError::Lossy(lossy));
        }
    };
    if c.abandoned() {
        c.close();
        return Err(PublishError::Abandoned);
    }
    if let Err(error) = c.publish_promise() {
        let restore = match c.write_snapshot(&mut snapshot) {
            Ok(()) => ClipboardSettlement::Restored,
            Err(e) => ClipboardSettlement::RestoreFailed(e),
        };
        c.close();
        return Err(PublishError::Failed { error, restore });
    }
    c.close();
    Ok((snapshot, c.sequence()))
}

/// Settles a transaction's clipboard. Ownership and the sequence number are
/// checked only *after* the clipboard lock is held: a check made before
/// `OpenClipboard` cannot see a copy that lands in between.
pub(crate) fn settle_guarded<C: ClipboardOps>(
    c: &mut C,
    snapshot: Option<&mut C::Snapshot>,
    expected_sequence: u32,
    ownership_lost: bool,
    action: SettleAction,
    text: &str,
) -> ClipboardSettlement {
    if ownership_lost {
        return ClipboardSettlement::ExternallyChanged;
    }
    if let Err(e) = c.open() {
        return ClipboardSettlement::RestoreFailed(e);
    }
    if !c.owned_by_tx() || c.sequence() != expected_sequence {
        c.close();
        return ClipboardSettlement::ExternallyChanged;
    }
    let settlement = match action {
        SettleAction::Restore => match snapshot {
            Some(snapshot) => match c.write_snapshot(snapshot) {
                Ok(()) => ClipboardSettlement::Restored,
                Err(e) => ClipboardSettlement::RestoreFailed(e),
            },
            None => ClipboardSettlement::RestoreFailed("no snapshot to restore".into()),
        },
        SettleAction::LeaveText => match c.write_text(text) {
            Ok(()) => ClipboardSettlement::LeftReplacement,
            Err(e) => ClipboardSettlement::RestoreFailed(e),
        },
    };
    c.close();
    settlement
}

/// Rendering the promise (WM_RENDERFORMAT) may or may not bump the clipboard
/// sequence number. Adopt the new value only when the value before rendering
/// was ours, so the guard never adopts someone else's change.
pub(crate) fn adopt_render_sequence(expected: u32, before: u32, after: u32) -> u32 {
    if before == expected {
        after
    } else {
        expected
    }
}

/// Decides a delayed-render request (`WM_RENDERFORMAT`) for the published
/// promise. Returns whether to render; records the read as a receipt only
/// when it renders.
///
/// Strict transactions decline reads that arrive before the paste chord is
/// injected. Eager clipboard listeners (observed live: WSLg's `msrdc`
/// clipboard sync, 3-75 ms after publish) otherwise consume the render, the
/// target's later paste reads the already-rendered data without a
/// `WM_RENDERFORMAT`, and the commit can only end `Uncertain(NoReceipt)`.
/// A declined format stays delayed, so the next read (the target's paste)
/// asks again. Trade-off: readers before injection get no data for this
/// transient replacement, which the published history/monitor exclusion
/// markers already ask them to ignore. Dictation keeps rendering every read
/// (unchanged); pre-injection reads never count as receipts in either mode.
pub(crate) fn on_render_request(state: &mut TxState, strict: bool, now: Instant) -> bool {
    if strict && (state.injected_at.is_none() || state.aborted || state.injection_failed) {
        return false;
    }
    state.record_receipt(now);
    true
}

/// Destruction is not a receipt; decide whether the promise may be materialized.
pub(crate) fn on_render_all_request(state: &TxState, strict: bool) -> bool {
    !strict || (state.injected_at.is_some() && !state.aborted && !state.injection_failed)
}

/// A failed pre-delivery restore must retain the owner/promise and retry on a
/// later timer tick, rather than destroy the window with the original lost.
pub(crate) fn retry_aborted_restore(
    strict: bool,
    pre_delivery: bool,
    clipboard: &ClipboardSettlement,
) -> bool {
    strict && pre_delivery && matches!(clipboard, ClipboardSettlement::RestoreFailed(_))
}

/// Retry clipboard acquisition on its owner thread without starving senders.
pub(crate) fn retry_clipboard_open(
    attempts: u32,
    delay: std::time::Duration,
    mut open: impl FnMut() -> Result<(), String>,
    mut service_sent_messages: impl FnMut(std::time::Duration),
) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..attempts {
        if attempt > 0 {
            service_sent_messages(delay);
        }
        match open() {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(format!("OpenClipboard failed: {last}"))
}

/// Key-up plan for the prefix inserted from the four-event Ctrl+V chord.
pub(crate) fn chord_release_keys(sent: u32, total: u32) -> &'static [u16] {
    match sent {
        2 if sent < total => &[0x56, 0x11],
        s if s > 0 && s < total => &[0x11],
        _ => &[],
    }
}

/// Settlement action of a strict (assistant) transaction.
pub(crate) fn strict_settle_action(
    injected: bool,
    injection_failed: bool,
    aborted: bool,
    after_success: crate::settings::ClipboardHandling,
) -> SettleAction {
    use crate::settings::ClipboardHandling;
    if aborted || !injected || injection_failed {
        return SettleAction::Restore;
    }
    match after_success {
        ClipboardHandling::DontModify => SettleAction::Restore,
        ClipboardHandling::CopyToClipboard => SettleAction::LeaveText,
    }
}

/// Marks a pending transaction as preempted by a newer one. Returns whether
/// the preempted transaction may still auto-submit now. A strict (assistant)
/// preemptor passes `allow_auto_submit = false`, which also stops the old
/// worker from sending Enter into the assistant's target later.
pub(crate) fn preempt(state: &mut TxState, allow_auto_submit: bool) -> bool {
    state.cancelled = true;
    state.preempted = true;
    if !allow_auto_submit {
        state.auto_submit_sent = true;
        return false;
    }
    state.any_receipt_after_injection() && !state.auto_submit_sent
}

/// Final state of a strict transaction, reported once by its pump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StrictSettlement {
    pub receipt_after_injection: bool,
    pub ownership_lost: bool,
    pub preempted: bool,
    pub clipboard: ClipboardSettlement,
}

/// Result of injecting the atomic paste chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChordResult {
    Sent,
    /// No paste keystroke reached the input queue.
    NotSent(String),
    /// Part of the chord was injected; the paste may or may not have happened.
    Partial(String),
}

/// Classifies a `SendInput` insertion count for the 4-event Ctrl+V chord.
/// The bool is whether a best-effort Ctrl release is needed.
pub(crate) fn classify_chord(sent: u32, total: u32, error: Option<String>) -> (ChordResult, bool) {
    let detail = || {
        format!(
            "{sent} of {total} key events injected{}",
            error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        )
    };
    match sent {
        s if total > 0 && s >= total => (ChordResult::Sent, false),
        0 => (ChordResult::NotSent(detail()), false),
        // Only Ctrl-down went in: V never did, so nothing was pasted.
        1 => (ChordResult::NotSent(detail()), true),
        _ => (ChordResult::Partial(detail()), true),
    }
}
