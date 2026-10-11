//! Receipt-sequenced clipboard paste ("reliable paste", debug-gated).
//!
//! The legacy clipboard paste (`clipboard::paste_via_clipboard`) restores the
//! previous clipboard after a fixed delay. The paste keystroke is only
//! *enqueued* at that point — the target application reads the clipboard
//! whenever its event loop gets to it, so any fixed delay can lose the race
//! and the user gets their old clipboard pasted back (#502).
//!
//! This module instead publishes the transcript as a *lazy promise* and waits
//! for the operating system to tell us that a consumer actually read the
//! clipboard — a "receipt" — before restoring:
//!
//! - Windows: delayed rendering (`SetClipboardData(CF_UNICODETEXT, NULL)`),
//!   the owner window receives `WM_RENDERFORMAT` on read.
//! - macOS: `declareTypes:owner:` with an owner object, the pasteboard calls
//!   `pasteboard:provideDataForType:` on read.
//!
//! Two rules make the receipt trustworthy:
//!
//! 1. Only receipts observed *after* the paste chord was injected count. A
//!    read before that is an eager third party (clipboard manager, antivirus)
//!    reacting to the clipboard change itself.
//! 2. Restoration only happens while we still own the clipboard
//!    (sequence number / changeCount unchanged, no ownership-lost event). If
//!    the user copied something else in the meantime, their action wins.
//!
//! The restore is additionally gated on a short quiet period after the *last*
//! receipt, because some applications read the clipboard several times per
//! paste (Chromium probes, then reads). A bounded timeout caps how long the
//! transcript may occupy the clipboard; the failure mode is always "the
//! transcript stays on the clipboard a bit longer", never "stale content gets
//! pasted".

// The shared transaction state is compiled on all platforms (for the unit
// tests), but only the macOS/Windows platform modules consume all of it.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

mod strict;
#[allow(unused_imports)]
pub(crate) use strict::*;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// How long after the *last* observed read the transcript stays on the
/// clipboard before restoring. Covers applications that read the clipboard
/// several times per paste (e.g. Chromium probe-then-read).
pub(crate) const QUIET_PERIOD: Duration = Duration::from_millis(200);

/// Upper bound on how long the transcript may occupy the clipboard before we
/// restore regardless of receipts. Long enough that a realistically loaded
/// target always gets to read first; short enough that a lost keystroke does
/// not strand the transcript on the clipboard for long.
pub(crate) const RESTORE_TIMEOUT: Duration = Duration::from_secs(8);

/// When the chord could not be injected at all, no legitimate receipt can
/// arrive, so restore quickly instead of waiting out the full timeout.
pub(crate) const FAILED_INJECTION_TIMEOUT: Duration = Duration::from_millis(500);

/// Shared, cross-thread record of one paste transaction.
#[derive(Debug)]
pub(crate) struct TxState {
    /// When the transcript was published to the clipboard.
    pub published_at: Instant,
    /// When the paste chord was injected. Only receipts *after* this count as
    /// evidence the target read the transcript — earlier reads are eager third
    /// parties reacting to the clipboard change itself.
    pub injected_at: Option<Instant>,
    /// The chord could not be sent; short-circuit the wait.
    pub injection_failed: bool,
    /// Times at which a consumer requested the clipboard data.
    pub receipts: Vec<Instant>,
    /// Someone else took clipboard ownership (user copied elsewhere, ...).
    pub ownership_lost: bool,
    /// A newer paste transaction settled this one early (see flush logic in
    /// the platform modules).
    pub cancelled: bool,
    /// The post-paste Enter (auto-submit) has been sent for this transaction.
    /// (Read on Windows; the macOS path settles via `MacPending::settled`.)
    #[allow(dead_code)]
    pub auto_submit_sent: bool,
    /// First post-injection receipt has been logged.
    pub logged_receipt: bool,
    /// The strict caller abandoned the transaction before delivery; settle by
    /// restoring the snapshot regardless of the after-success policy.
    #[allow(dead_code)]
    pub aborted: bool,
    /// A newer paste transaction settled this one early.
    #[allow(dead_code)]
    pub preempted: bool,
}

impl TxState {
    pub fn new() -> Self {
        Self {
            published_at: Instant::now(),
            injected_at: None,
            injection_failed: false,
            receipts: Vec::new(),
            ownership_lost: false,
            cancelled: false,
            auto_submit_sent: false,
            logged_receipt: false,
            aborted: false,
            preempted: false,
        }
    }

    /// Records a read receipt, logging the first one that counts as evidence.
    pub fn record_receipt(&mut self, at: Instant) {
        self.receipts.push(at);
        if !self.logged_receipt {
            if let Some(injected) = self.injected_at {
                if at >= injected {
                    self.logged_receipt = true;
                    log::info!(
                        "[reliable-paste] clipboard read {}ms after chord",
                        at.duration_since(injected).as_millis()
                    );
                }
            }
        }
    }

    pub fn last_receipt_after_injection(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.receipts.iter().copied().rev().find(|t| *t >= injected)
    }

    pub fn any_receipt_after_injection(&self) -> bool {
        self.last_receipt_after_injection().is_some()
    }
}

pub(crate) enum WaitDecision {
    KeepWaiting,
    /// Stop waiting; settle the transaction (auto-submit + guarded restore).
    Finish,
}

/// Pure decision: given the current transaction state, keep waiting for the
/// target to read, or finish now. Both platform event loops call this.
pub(crate) fn evaluate(state: &TxState, now: Instant) -> WaitDecision {
    if state.ownership_lost || state.cancelled {
        return WaitDecision::Finish;
    }
    if let Some(last) = state.last_receipt_after_injection() {
        if now.duration_since(last) >= QUIET_PERIOD {
            return WaitDecision::Finish;
        }
    }
    let deadline = if state.injection_failed {
        FAILED_INJECTION_TIMEOUT
    } else {
        RESTORE_TIMEOUT
    };
    if now.duration_since(state.published_at) >= deadline {
        return WaitDecision::Finish;
    }
    WaitDecision::KeepWaiting
}

/// Modifier hold for the receipt-sequenced chord, kept at parity with the
/// legacy path (100ms) for the beta: that hold was added in #165 because real
/// users' systems dropped chords released too quickly, and the beta should
/// validate the receipt mechanism without changing a second variable.
///
/// Once receipts are proven in the field this becomes a safe tuning knob — a
/// chord the target never recognizes produces no receipt and is logged ("no
/// read within timeout") rather than failing silently, so a shorter hold
/// (measured working at 10ms on a fast machine, cutting visible latency from
/// ~110ms to ~20ms) can be tried as its own experiment later.
const CHORD_HOLD_MS: u64 = 100;

/// Sends the platform paste chord for the configured method.
pub(crate) fn send_chord(
    enigo: &mut enigo::Enigo,
    paste_method: &crate::settings::PasteMethod,
) -> Result<(), String> {
    use crate::settings::PasteMethod;
    match paste_method {
        PasteMethod::CtrlV => crate::input::send_paste_ctrl_v(enigo, CHORD_HOLD_MS),
        PasteMethod::CtrlShiftV => crate::input::send_paste_ctrl_shift_v(enigo, CHORD_HOLD_MS),
        PasteMethod::ShiftInsert => crate::input::send_paste_shift_insert(enigo, CHORD_HOLD_MS),
        other => Err(format!(
            "Invalid paste method for clipboard paste: {:?}",
            other
        )),
    }
}

/// Attempts the receipt-sequenced paste. Returns `Err` before anything has
/// been published when the platform transaction cannot start, in which case
/// the caller should fall back to the legacy paste path. On `Ok`, publishing
/// and chord injection have completed and the guarded restore (plus
/// auto-submit) finishes asynchronously.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn try_reliable_paste(
    text: &str,
    app_handle: &tauri::AppHandle,
    paste_method: &crate::settings::PasteMethod,
    enigo: &mut enigo::Enigo,
    auto_submit: bool,
    auto_submit_key: crate::settings::AutoSubmitKey,
    clipboard_handling: crate::settings::ClipboardHandling,
) -> Result<(), String> {
    platform::run(
        text,
        app_handle,
        paste_method,
        enigo,
        auto_submit,
        auto_submit_key,
        clipboard_handling,
    )
}

#[cfg(all(test, target_os = "windows"))]
pub(crate) use platform::diag;
#[cfg(all(test, target_os = "windows"))]
pub(crate) use platform::ProbeClipboardSession;
#[cfg(target_os = "windows")]
pub(crate) use platform::StrictClipboardTx;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn state_after_publish(published_ago: Duration) -> TxState {
        let mut s = TxState::new();
        s.published_at = Instant::now() - published_ago;
        s
    }

    #[test]
    fn keeps_waiting_without_receipt_within_timeout() {
        let s = state_after_publish(Duration::from_millis(100));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_after_quiet_period_once_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(250));
        s.receipts.push(Instant::now() - QUIET_PERIOD);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn waits_through_quiet_period_after_recent_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        s.receipts.push(Instant::now() - Duration::from_millis(50));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn pre_injection_receipt_does_not_count() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.receipts.push(Instant::now() - Duration::from_millis(200));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!s.any_receipt_after_injection());
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_on_timeout_without_receipt() {
        let s = state_after_publish(RESTORE_TIMEOUT);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn failed_injection_uses_short_timeout() {
        let mut s = state_after_publish(FAILED_INJECTION_TIMEOUT);
        s.injection_failed = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn ownership_loss_finishes_immediately() {
        let mut s = state_after_publish(Duration::from_millis(10));
        s.ownership_lost = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    // --- strict snapshot / publish / settle -------------------------------

    use crate::settings::ClipboardHandling;

    /// Contents of the fake system clipboard.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Content {
        User(&'static str),
        Promise,
        Text(String),
    }

    /// In-memory clipboard that records every call.
    struct FakeClipboard {
        content: Content,
        sequence: u32,
        owner_is_tx: bool,
        is_open: bool,
        open_error: Option<String>,
        /// Simulates another process copying while we wait for the lock:
        /// applied inside `open()` before it returns.
        external_copy_during_open: bool,
        lossy: Option<LossyFormat>,
        publish_error: Option<String>,
        write_error: Option<String>,
        abandoned: bool,
        calls: Vec<&'static str>,
    }

    impl FakeClipboard {
        fn new() -> Self {
            Self {
                content: Content::User("original"),
                sequence: 10,
                owner_is_tx: false,
                is_open: false,
                open_error: None,
                external_copy_during_open: false,
                lossy: None,
                publish_error: None,
                write_error: None,
                abandoned: false,
                calls: Vec::new(),
            }
        }

        fn mutate(&mut self, content: Content, ours: bool) {
            assert!(self.is_open, "clipboard written while not open");
            self.content = content;
            self.sequence += 1;
            self.owner_is_tx = ours;
        }

        fn wrote(&self) -> bool {
            self.calls
                .iter()
                .any(|c| matches!(*c, "publish" | "write_snapshot" | "write_text"))
        }
    }

    impl ClipboardOps for FakeClipboard {
        type Snapshot = Content;
        fn open(&mut self) -> Result<(), String> {
            self.calls.push("open");
            if let Some(e) = &self.open_error {
                return Err(e.clone());
            }
            if self.external_copy_during_open {
                self.external_copy_during_open = false;
                self.content = Content::User("newer user copy");
                self.sequence += 1;
                self.owner_is_tx = false;
            }
            self.is_open = true;
            Ok(())
        }
        fn close(&mut self) {
            self.calls.push("close");
            self.is_open = false;
        }
        fn sequence(&self) -> u32 {
            self.sequence
        }
        fn owned_by_tx(&self) -> bool {
            self.owner_is_tx
        }
        fn snapshot(&mut self, policy: SnapshotPolicy) -> Result<Content, LossyFormat> {
            self.calls.push("snapshot");
            assert!(self.is_open);
            match (&self.lossy, policy) {
                (Some(l), SnapshotPolicy::Strict) => Err(l.clone()),
                _ => Ok(self.content.clone()),
            }
        }
        fn publish_promise(&mut self) -> Result<(), String> {
            self.calls.push("publish");
            // Publishing empties first, so a failure still loses the content.
            self.mutate(Content::Promise, true);
            match &self.publish_error {
                Some(e) => {
                    self.content = Content::Text(String::new());
                    Err(e.clone())
                }
                None => Ok(()),
            }
        }
        fn write_snapshot(&mut self, snapshot: &mut Content) -> Result<(), String> {
            self.calls.push("write_snapshot");
            if let Some(e) = &self.write_error {
                return Err(e.clone());
            }
            self.mutate(snapshot.clone(), false);
            Ok(())
        }
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            self.calls.push("write_text");
            if let Some(e) = &self.write_error {
                return Err(e.clone());
            }
            self.mutate(Content::Text(text.to_string()), false);
            Ok(())
        }
        fn abandoned(&self) -> bool {
            self.abandoned
        }
    }

    #[test]
    fn strict_format_accepts_text_image_and_registered() {
        for format in [1, 7, CF_UNICODETEXT, 8, 17, 15, 16, 0xC000, 0xC123, 0xFFFF] {
            assert_eq!(
                format_action(SnapshotPolicy::Strict, format),
                FormatAction::ReadMemory,
                "format {format:#x}"
            );
        }
        assert_eq!(
            format_action(SnapshotPolicy::Strict, CF_BITMAP),
            FormatAction::CopyBitmap
        );
        assert_eq!(
            judge_read(SnapshotPolicy::Strict, FormatRead::Bitmap { copied: true }),
            Ok(true)
        );
        assert_eq!(
            judge_read(
                SnapshotPolicy::Strict,
                FormatRead::Memory {
                    size: MAX_FORMAT_BYTES,
                    locked: true
                }
            ),
            Ok(true)
        );
    }

    #[test]
    fn strict_format_rejects_metafiles_owner_display_palette() {
        for format in [
            CF_METAFILEPICT,
            CF_ENHMETAFILE,
            CF_DSPENHMETAFILE,
            CF_DSPMETAFILEPICT,
            CF_DSPBITMAP,
            CF_OWNERDISPLAY,
            CF_PALETTE,
        ] {
            assert_eq!(
                format_action(SnapshotPolicy::Strict, format),
                FormatAction::Reject(LossyReason::NonMemoryHandle),
                "format {format:#x}"
            );
        }
        assert_eq!(
            judge_read(SnapshotPolicy::Strict, FormatRead::Bitmap { copied: false }),
            Err(LossyReason::BitmapCopyFailed)
        );
    }

    #[test]
    fn strict_format_rejects_private_and_gdi_ranges() {
        for format in [0x0200, 0x02FF, 0x0300, 0x03FF] {
            assert_eq!(
                format_action(SnapshotPolicy::Strict, format),
                FormatAction::Reject(LossyReason::PrivateOrGdiRange),
                "format {format:#x}"
            );
        }
        assert_eq!(
            format_action(SnapshotPolicy::Strict, 0x0400),
            FormatAction::ReadMemory
        );
    }

    #[test]
    fn strict_format_rejects_oversized_zero_and_unreadable() {
        let strict = SnapshotPolicy::Strict;
        assert_eq!(
            judge_read(
                strict,
                FormatRead::Memory {
                    size: MAX_FORMAT_BYTES + 1,
                    locked: false
                }
            ),
            Err(LossyReason::TooLarge(MAX_FORMAT_BYTES + 1))
        );
        assert_eq!(
            judge_read(
                strict,
                FormatRead::Memory {
                    size: 0,
                    locked: true
                }
            ),
            Err(LossyReason::Unreadable)
        );
        assert_eq!(
            judge_read(
                strict,
                FormatRead::Memory {
                    size: 16,
                    locked: false
                }
            ),
            Err(LossyReason::Unreadable)
        );
        assert_eq!(
            judge_read(strict, FormatRead::Unreadable),
            Err(LossyReason::Unreadable)
        );
    }

    #[test]
    fn best_effort_policy_skips_instead_of_failing() {
        let best = SnapshotPolicy::BestEffort;
        for read in [
            FormatRead::Unreadable,
            FormatRead::Memory {
                size: 0,
                locked: true,
            },
            FormatRead::Memory {
                size: MAX_FORMAT_BYTES + 1,
                locked: false,
            },
            FormatRead::Memory {
                size: 8,
                locked: false,
            },
            FormatRead::Bitmap { copied: false },
        ] {
            assert_eq!(judge_read(best, read), Ok(false), "{read:?}");
        }
        // The historical skip list, including CF_DSPTEXT, is unchanged and
        // nothing is ever rejected.
        for format in [
            CF_ENHMETAFILE,
            CF_DSPENHMETAFILE,
            CF_DSPBITMAP,
            CF_DSPMETAFILEPICT,
            CF_DSPTEXT,
            CF_OWNERDISPLAY,
            CF_PALETTE,
        ] {
            assert_eq!(format_action(best, format), FormatAction::Skip);
        }
        for format in [CF_METAFILEPICT, 0x0200, 0x0300, CF_UNICODETEXT, 0xC000] {
            assert_eq!(format_action(best, format), FormatAction::ReadMemory);
        }
    }

    #[test]
    fn publish_snapshots_and_publishes_in_one_session() {
        let mut c = FakeClipboard::new();
        let (snapshot, sequence) =
            snapshot_and_publish(&mut c, SnapshotPolicy::Strict).expect("published");
        assert_eq!(snapshot, Content::User("original"));
        assert_eq!(c.content, Content::Promise);
        assert_eq!(sequence, c.sequence);
        assert_eq!(c.calls, vec!["open", "snapshot", "publish", "close"]);
    }

    #[test]
    fn publish_refuses_lossy_snapshot_without_emptying() {
        let mut c = FakeClipboard::new();
        let lossy = LossyFormat {
            format: CF_ENHMETAFILE,
            reason: LossyReason::NonMemoryHandle,
        };
        c.lossy = Some(lossy.clone());
        let result = snapshot_and_publish(&mut c, SnapshotPolicy::Strict);
        assert_eq!(result.err(), Some(PublishError::Lossy(lossy)));
        assert_eq!(c.content, Content::User("original"));
        assert_eq!(c.sequence, 10);
        assert!(!c.wrote());
        assert!(!c.is_open);
    }

    #[test]
    fn publish_abandoned_after_snapshot_leaves_clipboard_unchanged() {
        let mut c = FakeClipboard::new();
        c.abandoned = true;
        let result = snapshot_and_publish(&mut c, SnapshotPolicy::Strict);
        assert_eq!(result.err(), Some(PublishError::Abandoned));
        assert_eq!(c.content, Content::User("original"));
        assert!(!c.wrote());
        assert!(!c.is_open);
    }

    #[test]
    fn publish_busy_clipboard_changes_nothing() {
        let mut c = FakeClipboard::new();
        c.open_error = Some("held by another process".into());
        let result = snapshot_and_publish(&mut c, SnapshotPolicy::Strict);
        assert!(matches!(result, Err(PublishError::Busy(_))));
        assert_eq!(c.content, Content::User("original"));
        assert!(!c.wrote());
    }

    #[test]
    fn publish_failure_rewrites_snapshot() {
        let mut c = FakeClipboard::new();
        c.publish_error = Some("SetClipboardData failed".into());
        let result = snapshot_and_publish(&mut c, SnapshotPolicy::BestEffort);
        assert_eq!(
            result.err(),
            Some(PublishError::Failed {
                error: "SetClipboardData failed".into(),
                restore: ClipboardSettlement::Restored,
            })
        );
        assert_eq!(c.content, Content::User("original"));
        assert_eq!(
            c.calls,
            vec!["open", "snapshot", "publish", "write_snapshot", "close"]
        );
    }

    /// Publishes, returning the fake and the sequence a transaction would record.
    fn published() -> (FakeClipboard, Content, u32) {
        let mut c = FakeClipboard::new();
        let (snapshot, sequence) = snapshot_and_publish(&mut c, SnapshotPolicy::Strict).unwrap();
        c.calls.clear();
        (c, snapshot, sequence)
    }

    #[test]
    fn settle_rechecks_sequence_after_acquiring_lock() {
        assert_eq!(
            settle_with_external_copy_during_open(),
            ClipboardSettlement::ExternallyChanged
        );
    }

    /// Reused by the selection commit test so both proof filters exercise the
    /// real guarded settlement when a newer copy lands while acquiring its lock.
    pub(crate) fn settle_with_external_copy_during_open() -> ClipboardSettlement {
        let (mut c, mut snapshot, sequence) = published();
        // Before the lock, everything still looks like ours...
        assert!(c.owned_by_tx() && c.sequence() == sequence);
        // ...but a user copy lands while we wait for OpenClipboard.
        c.external_copy_during_open = true;
        let result = settle_guarded(
            &mut c,
            Some(&mut snapshot),
            sequence,
            false,
            SettleAction::Restore,
            "replacement",
        );
        assert_eq!(result, ClipboardSettlement::ExternallyChanged);
        assert_eq!(c.content, Content::User("newer user copy"));
        assert!(!c.wrote());
        assert_eq!(c.calls, vec!["open", "close"]);
        result
    }

    #[test]
    fn settle_requires_owner_under_lock() {
        let (mut c, mut snapshot, sequence) = published();
        // Same sequence number but another owner (e.g. sequence wrapped or an
        // owner change that did not bump it): never write.
        c.owner_is_tx = false;
        let result = settle_guarded(
            &mut c,
            Some(&mut snapshot),
            sequence,
            false,
            SettleAction::Restore,
            "replacement",
        );
        assert_eq!(result, ClipboardSettlement::ExternallyChanged);
        assert!(!c.wrote());
    }

    #[test]
    fn settle_skips_after_ownership_lost() {
        let (mut c, mut snapshot, sequence) = published();
        let result = settle_guarded(
            &mut c,
            Some(&mut snapshot),
            sequence,
            true,
            SettleAction::Restore,
            "replacement",
        );
        assert_eq!(result, ClipboardSettlement::ExternallyChanged);
        assert!(c.calls.is_empty(), "must not even open: {:?}", c.calls);
    }

    #[test]
    fn settle_open_failure_is_restore_failed() {
        let (mut c, mut snapshot, sequence) = published();
        c.open_error = Some("busy".into());
        let result = settle_guarded(
            &mut c,
            Some(&mut snapshot),
            sequence,
            false,
            SettleAction::Restore,
            "replacement",
        );
        assert_eq!(result, ClipboardSettlement::RestoreFailed("busy".into()));
        assert_eq!(c.content, Content::Promise);
    }

    #[test]
    fn settle_restores_or_leaves_text_per_policy() {
        let (mut c, mut snapshot, sequence) = published();
        let result = settle_guarded(
            &mut c,
            Some(&mut snapshot),
            sequence,
            false,
            SettleAction::Restore,
            "replacement",
        );
        assert_eq!(result, ClipboardSettlement::Restored);
        assert_eq!(c.content, Content::User("original"));

        let (mut c, mut snapshot, sequence) = published();
        let result = settle_guarded(
            &mut c,
            Some(&mut snapshot),
            sequence,
            false,
            SettleAction::LeaveText,
            "replacement",
        );
        assert_eq!(result, ClipboardSettlement::LeftReplacement);
        assert_eq!(c.content, Content::Text("replacement".into()));

        let (mut c, mut snapshot, sequence) = published();
        c.write_error = Some("SetClipboardData failed".into());
        let result = settle_guarded(
            &mut c,
            Some(&mut snapshot),
            sequence,
            false,
            SettleAction::Restore,
            "replacement",
        );
        assert_eq!(
            result,
            ClipboardSettlement::RestoreFailed("SetClipboardData failed".into())
        );
    }

    #[test]
    fn strict_settle_restores_after_failed_or_aborted_injection() {
        use ClipboardHandling::*;
        // Never injected, injection failed or aborted: restore, whatever the
        // after-success policy says.
        for handling in [DontModify, CopyToClipboard] {
            assert_eq!(
                strict_settle_action(false, false, false, handling),
                SettleAction::Restore
            );
            assert_eq!(
                strict_settle_action(true, true, false, handling),
                SettleAction::Restore
            );
            assert_eq!(
                strict_settle_action(true, false, true, handling),
                SettleAction::Restore
            );
        }
        assert_eq!(
            strict_settle_action(true, false, false, DontModify),
            SettleAction::Restore
        );
        assert_eq!(
            strict_settle_action(true, false, false, CopyToClipboard),
            SettleAction::LeaveText
        );
    }

    #[test]
    fn strict_read_before_injection_is_declined_and_not_counted() {
        let mut s = state_after_publish(Duration::from_millis(20));
        // Eager listener before the chord: not rendered, not a receipt.
        assert!(!on_render_request(&mut s, true, Instant::now()));
        assert!(s.receipts.is_empty());
        assert!(!s.any_receipt_after_injection());
        // The target's paste after injection: rendered and counted.
        s.injected_at = Some(Instant::now());
        assert!(on_render_request(&mut s, true, Instant::now()));
        assert!(s.any_receipt_after_injection());
    }

    #[test]
    fn dictation_renders_every_read_but_counts_only_post_injection() {
        let mut s = state_after_publish(Duration::from_millis(20));
        assert!(on_render_request(&mut s, false, Instant::now()));
        assert_eq!(s.receipts.len(), 1);
        assert!(!s.any_receipt_after_injection());
        s.injected_at = Some(Instant::now());
        assert!(on_render_request(&mut s, false, Instant::now()));
        assert!(s.any_receipt_after_injection());
    }

    #[test]
    fn render_adopts_only_own_sequence_bump() {
        assert_eq!(adopt_render_sequence(7, 7, 8), 8);
        assert_eq!(adopt_render_sequence(7, 7, 7), 7);
        // Someone else changed the clipboard before our render: keep the old
        // value so the guard refuses to restore over their content.
        assert_eq!(adopt_render_sequence(7, 9, 10), 7);
    }

    #[test]
    fn chord_classification_counts() {
        assert_eq!(classify_chord(4, 4, None), (ChordResult::Sent, false));
        let (r, release) = classify_chord(0, 4, Some("blocked".into()));
        assert!(matches!(r, ChordResult::NotSent(ref m) if m.contains("blocked")));
        assert!(!release);
        let (r, release) = classify_chord(1, 4, None);
        assert!(matches!(r, ChordResult::NotSent(_)));
        assert!(release);
        for sent in [2, 3] {
            let (r, release) = classify_chord(sent, 4, None);
            assert!(matches!(r, ChordResult::Partial(_)), "{sent}");
            assert!(release);
        }
    }

    #[test]
    fn chord_partial_prefix_releases_v_and_ctrl() {
        assert!(chord_release_keys(0, 4).is_empty());
        assert_eq!(chord_release_keys(1, 4), &[0x11]);
        assert_eq!(chord_release_keys(2, 4), &[0x56, 0x11]);
        assert_eq!(chord_release_keys(3, 4), &[0x11]);
        assert!(chord_release_keys(4, 4).is_empty());
    }

    #[test]
    fn abort_reader_request_is_serviced_during_clipboard_retry() {
        let reader_waiting = std::cell::Cell::new(true);
        let declined = std::cell::Cell::new(false);
        let state = std::cell::RefCell::new(TxState::new());
        state.borrow_mut().aborted = true;
        let result = retry_clipboard_open(
            3,
            Duration::ZERO,
            || {
                if reader_waiting.get() {
                    Err("reader awaits WM_RENDERFORMAT".into())
                } else {
                    Ok(())
                }
            },
            |_| {
                let render = on_render_request(&mut state.borrow_mut(), true, Instant::now());
                declined.set(!render);
                reader_waiting.set(false);
            },
        );
        assert_eq!(result, Ok(()));
        assert!(declined.get());
        assert!(!state.borrow().any_receipt_after_injection());
    }

    #[test]
    fn aborted_unrendered_promise_is_not_rendered_on_destroy() {
        let mut state = TxState::new();
        state.aborted = true;
        assert!(!on_render_all_request(&state, true));
        state.aborted = false;
        assert!(!on_render_all_request(&state, true));
        state.injected_at = Some(Instant::now());
        assert!(on_render_all_request(&state, true));
        assert!(on_render_all_request(&TxState::new(), false));
    }

    #[test]
    fn abort_restore_failure_retains_promise_for_later_tick() {
        let failed = ClipboardSettlement::RestoreFailed("reader still holds clipboard".into());
        assert!(retry_aborted_restore(true, true, &failed));
        assert!(!retry_aborted_restore(
            true,
            true,
            &ClipboardSettlement::Restored
        ));
        assert!(!retry_aborted_restore(
            true,
            true,
            &ClipboardSettlement::ExternallyChanged
        ));
        assert!(!retry_aborted_restore(true, false, &failed));
        assert!(!retry_aborted_restore(false, true, &failed));
    }

    #[test]
    fn preempt_for_strict_suppresses_pending_auto_submit() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(200));
        s.receipts.push(Instant::now() - Duration::from_millis(100));
        assert!(!preempt(&mut s, false));
        assert!(s.cancelled && s.preempted);
        // The old worker's own settle path checks this flag before Enter.
        assert!(s.auto_submit_sent);
    }

    #[test]
    fn preempt_for_dictation_keeps_auto_submit() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(200));
        s.receipts.push(Instant::now() - Duration::from_millis(100));
        assert!(preempt(&mut s, true));
        assert!(s.cancelled);
        assert!(!s.auto_submit_sent);

        let mut no_receipt = state_after_publish(Duration::from_millis(300));
        no_receipt.injected_at = Some(Instant::now());
        assert!(!preempt(&mut no_receipt, true));
    }
}
